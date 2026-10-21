# whipserversrc / whepserversink: trickle ICE

Trickle ICE in WHIP and WHEP (RFC 9725 section 4.3.2): the client sends its
offer right away, then PATCHes its ICE candidates to the session URL as an
`application/trickle-ice-sdpfrag` (RFC 8840) as it gathers them. Browsers
work this way, and it saves the time until gathering completes, which can be
seconds (VPN or virtual interfaces, unreachable STUN/TURN servers). Waiting
for gathering before sending the offer delays every session by that much.

With gst-plugins-rs `main`:

- **WHIP (`whipserversrc`)** answers every PATCH with `501 Not Implemented`.
  The trickled candidates are lost. On a LAN, ICE still connects, because
  the server learns the client's address from its connectivity checks
  (peer-reflexive candidate). It cannot check any client candidate itself.
- **WHEP (`whepserversink`)** accepts trickled candidates, but takes the
  m-line index from the position of the media section **in the PATCH body**
  and ignores its `a=mid`. A browser sends one candidate per PATCH, so the
  candidates of every media section end up on m-line 0. Without BUNDLE, the
  audio transport gets none of its candidates (and the video transport gets
  the audio's). It also answers `204` to ICE restarts and to PATCHes for
  sessions that don't exist, and still hands those candidates to webrtcbin.

The fix is in branch
[`webrtc-whip-whep-trickle-ice`](https://gitlab.freedesktop.org/me5/gst-plugins-rs/-/commits/webrtc-whip-whep-trickle-ice)
(two commits on top of `main` 9cb42da34).

## The repro

A Rust program with both sides in one process. The server is
`whipserversrc` or `whepserversink` on 127.0.0.1; the client is a plain
`webrtcbin`:

- It POSTs its offer before it has gathered any candidate.
- It PATCHes each candidate in its own fragment
  (`a=ice-ufrag`, `a=ice-pwd`, `m=`, `a=mid`, `a=candidate`), then
  `a=end-of-candidates`.
- It sends three more PATCHes: a fragment with only the candidates of the
  last media section, an ICE restart (another `ice-ufrag`), and a PATCH for
  an unknown session.

It logs each PATCH status and each `handle-ice` the server's signaller
emits, and at the end lists the client candidates the server's webrtcbin
knows (from `get-stats`): `typ host` if it learned them from signalling,
`typ prflx` if only from connectivity checks.

The WHIP client sends one video stream (max-bundle). The WHEP client
receives video and audio with `bundle-policy=none`, so that each m-line has
its own ICE transport.

The features `upstream` and `fork` choose the gst-plugin-webrtc to test,
both pinned by commit:

```sh
cargo run --features upstream -- whip   # or whep
cargo run --features fork -- whip

./compare.sh   # all four, summaries only
```

Requires GStreamer ≥ 1.28 development files with webrtcbin, libnice, VP8 and
Opus (tested with 1.28.7 on Linux), and Rust ≥ 1.92.

## Results

Addresses below are replaced by documentation addresses.

| | WHIP `main` | WHIP fork | WHEP `main` | WHEP fork |
|---|---|---|---|---|
| trickled candidates | `501` (0/7) | `204` (7/7) | `204` (14/14) | `204` (14/14) |
| `audio1` candidates go to | | | **m-line 0** | m-line 1 |
| ICE restart | `501` | `422` | **`204`** | `422` |
| unknown session | `501` | `404` | **`204`** | `404` |
| server knows the client as | **prflx** | host | video host, **audio prflx** | host, host |

### WHIP, `main`

```
 0.030s [client] POST http://127.0.0.1:8190/whip/endpoint: offer with 0 candidate(s)
 0.038s [client] 201 Created, session http://127.0.0.1:8190/whip/resource/bb53a4dd-…
 0.039s [client] PATCH m-line 0 (mid video0) candidate:1 1 UDP 2015363327 192.0.2.10 57451 typ host -> 501 Not Implemented
 …
 0.043s [client] PATCH m-line 0 (mid video0) end-of-candidates -> 501 Not Implemented

=== WHIP with upstream main (9cb42da34)
trickled candidates accepted: 0/7
ICE restart (new ice-ufrag): 501 Not Implemented
unknown session: 501 Not Implemented
client ice-connection-state: Completed
server ice-connection-state: Completed
client candidates known to the server's webrtcbin:
  transport-stats_webrtcdtlstransport1: fe80::a:57751 typ prflx
```

### WHIP, fork

```
 0.043s [server] handle-ice m-line 0 mid Some("video0"): candidate:1 1 UDP 2015363327 192.0.2.10 38657 typ host
 …
=== WHIP with fork webrtc-whip-whep-trickle-ice (f9c7317b5)
trickled candidates accepted: 7/7
ICE restart (new ice-ufrag): 422 Unprocessable Entity
unknown session: 404 Not Found
client ice-connection-state: Completed
server ice-connection-state: Completed
client candidates known to the server's webrtcbin:
  transport-stats_webrtcdtlstransport1: fe80::a:50897 typ host
```

### WHEP, `main`

The audio candidates (mid `audio1`) are handed to webrtcbin as m-line 0:

```
 0.211s [server] handle-ice m-line 0 mid None: a=candidate:7 1 UDP 2015364863 fe80::a 44666 typ host
 0.212s [client] PATCH m-line 0 (mid video0) candidate:7 1 UDP 2015364863 fe80::a 44666 typ host -> 204 No Content
 0.214s [server] handle-ice m-line 0 mid None: a=candidate:1 1 UDP 2015363327 192.0.2.10 53284 typ host
 0.214s [client] PATCH m-line 1 (mid audio1) candidate:1 1 UDP 2015363327 192.0.2.10 53284 typ host -> 204 No Content
 …
 4.210s [server] handle-ice m-line 0 mid None: a=candidate:9999 1 UDP 2122260223 192.0.2.1 50000 typ host
 4.211s [server] handle-ice m-line 0 mid None: a=candidate:9999 1 UDP 2122260223 192.0.2.1 50000 typ host
 4.211s [server] handle-ice m-line 0 mid None: a=candidate:9999 1 UDP 2122260223 192.0.2.1 50000 typ host

=== WHEP with upstream main (9cb42da34)
trickled candidates accepted: 14/14
fragment with only mid audio1 (m-line 1): 204 No Content
ICE restart (new ice-ufrag): 204 No Content
unknown session: 204 No Content
client ice-connection-state: Completed
server ice-connection-state: Completed
client candidates known to the server's webrtcbin:
  transport-stats_webrtcdtlstransport2: fe80::a:54244 typ host
  transport-stats_webrtcdtlstransport3: fe80::a:41459 typ prflx
```

The three `192.0.2.1` lines are the last three PATCHes: the `audio1`-only
fragment, the ICE restart, and the unknown session. All three reach
webrtcbin as m-line 0.

### WHEP, fork

```
 0.213s [server] handle-ice m-line 0 mid Some("video0"): candidate:7 1 UDP 2015364863 fe80::a 58398 typ host
 0.214s [server] handle-ice m-line 1 mid Some("audio1"): candidate:1 1 UDP 2015363327 192.0.2.10 37724 typ host
 …
 4.211s [server] handle-ice m-line 1 mid Some("audio1"): candidate:9999 1 UDP 2122260223 192.0.2.1 50000 typ host

=== WHEP with fork webrtc-whip-whep-trickle-ice (f9c7317b5)
trickled candidates accepted: 14/14
fragment with only mid audio1 (m-line 1): 204 No Content
ICE restart (new ice-ufrag): 422 Unprocessable Entity
unknown session: 404 Not Found
client ice-connection-state: Completed
server ice-connection-state: Completed
client candidates known to the server's webrtcbin:
  transport-stats_webrtcdtlstransport2: fe80::a:58398 typ host
  transport-stats_webrtcdtlstransport3: fe80::a:57191 typ host
```

## Side finding: webrtcsink deadlocks without BUNDLE

With congestion control enabled (the default) and one ICE transport per
m-line, `whepserversink` deadlocks once the session starts, on both `main`
and the fork. The repro disables congestion control to get around it.

`start_session()` connects to webrtcbin's `request-aux-sender`, which is
emitted once per transport. The handler (`net/webrtc/src/webrtcsink/imp.rs`,
around line 3544, `main` 9cb42da34) locks `settings`, then sets
`estimated-bitrate` on the shared congestion controller. That emits
`notify::estimated-bitrate` synchronously, and the handler connected on the
previous call runs `set_bitrate()`, which locks `settings` again:

```
#4  Mutex<webrtcsink::imp::Settings>::lock
#5  webrtcsink::imp::BaseWebRTCSink::set_bitrate at src/webrtcsink/imp.rs:4284
#6  start_session::{closure#2}::{closure#2}::{closure#0} at src/webrtcsink/imp.rs:3561
    … g_object_notify on drop of the freeze guard …
#18 glib::object::ObjectExt::set_properties
#19 start_session::{closure#2}::{closure#2} at src/webrtcsink/imp.rs:3547
```

Every other webrtcsink thread then blocks on `settings` too, including
`get-stats` and the state change to `NULL`. To see it, remove
`congestion-control=disabled` in `whep_server()` and run `whep`.
