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
  the server learns the client's address from the client's connectivity
  checks (peer-reflexive candidates). It cannot check any client candidate
  itself.
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

A Rust program with both sides in one process:

- **`src/server.rs`**: `whipserversrc` or `whepserversink` on 127.0.0.1.
  It logs each `handle-ice` the signaller emits and counts the candidates
  it hands to webrtcbin, per m-line.
- **`src/client.rs`**: a plain `webrtcbin` that behaves like a browser.
  - It POSTs its offer before it has gathered any candidate and applies
    the answer right away.
  - For 4 s, it PATCHes each candidate in its own fragment (`a=ice-ufrag`,
    `a=ice-pwd`, `m=`, `a=mid`, `a=candidate`), plus `a=end-of-candidates`.
  - Then it sends three more PATCHes: a fragment with only the candidates
    of the last media section, an ICE restart (another `ice-ufrag`), and a
    PATCH for an unknown session.
- **`src/main.rs`**: starts both and prints the summary.

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

The number of candidates depends on the host's network interfaces; this
host gathers 17 per m-line. Addresses below are replaced by documentation
addresses.

| | WHIP `main` | WHIP fork | WHEP `main` | WHEP fork |
|---|---|---|---|---|
| trickled candidates | `501` (0/17) | `204` (17/17) | `204` (34/34) | `204` (34/34) |
| handed to webrtcbin, per m-line | none | `{0: 17}` | **`{0: 34}`** | `{0: 17, 1: 17}` |
| ICE restart | `501` | `422` | **`204`** | `422` |
| unknown session | `501` | `404` | **`204`** | `404` |

These are the same on every run. ICE connects in all four on localhost:
without the trickled candidates, the server still learns the client's
address from the client's connectivity checks (peer-reflexive candidates).
Which pair ICE selects, and whether the server reports its remote candidate
as `host` or `prflx`, depends on timing, so the repro doesn't report it.

### WHIP, `main`

```
 0.041s [client] POST http://127.0.0.1:8190/whip/endpoint: offer with 0 candidate(s)
 0.073s [client] 201 Created, session http://127.0.0.1:8190/whip/resource/5de5e440-e031-40ad-93e4-6da5920dbe63
 0.073s [client] PATCH m-line 0 (mid video0) candidate:1 1 UDP 2015363327 192.0.2.10 55952 typ host -> 501 Not Implemented
 0.074s [client] PATCH m-line 0 (mid video0) candidate:2 1 UDP 2015363583 192.0.2.11 60840 typ host -> 501 Not Implemented
 …
 0.086s [client] PATCH m-line 0 (mid video0) end-of-candidates -> 501 Not Implemented

=== WHIP with upstream main (9cb42da34)
trickled candidates accepted: 0/17
candidates handed to the server's webrtcbin, per m-line: {}
ICE restart (new ice-ufrag): 501 Not Implemented
unknown session: 501 Not Implemented
client ice-connection-state: Connected
server ice-connection-state: Completed
```

### WHIP, fork

```
 0.068s [client] POST http://127.0.0.1:8190/whip/endpoint: offer with 0 candidate(s)
 0.115s [client] 201 Created, session http://127.0.0.1:8190/whip/resource/d26e8936-c5cf-4f28-9c12-d73321b7f004
 0.116s [server] handle-ice m-line 0 mid Some("video0"): candidate:1 1 UDP 2015363327 192.0.2.10 42771 typ host
 0.119s [client] PATCH m-line 0 (mid video0) candidate:1 1 UDP 2015363327 192.0.2.10 42771 typ host -> 204 No Content
 …
 0.211s [client] PATCH m-line 0 (mid video0) end-of-candidates -> 204 No Content

=== WHIP with fork webrtc-whip-whep-trickle-ice (f9c7317b5)
trickled candidates accepted: 17/17
candidates handed to the server's webrtcbin, per m-line: {0: 17}
ICE restart (new ice-ufrag): 422 Unprocessable Entity
unknown session: 404 Not Found
client ice-connection-state: Connected
server ice-connection-state: Completed
```

### WHEP, `main`

The audio candidates (mid `audio1`) are handed to webrtcbin as m-line 0:

```
 0.085s [client] POST http://127.0.0.1:8191/whep/endpoint: offer with 0 candidate(s)
 0.429s [client] 201 Created, session http://127.0.0.1:8191/whep/resource/e3edeb2e-1ff8-4bbf-9a25-02e112870083
 0.432s [server] handle-ice m-line 0 mid None: a=candidate:1 1 UDP 2015363327 192.0.2.10 55263 typ host
 0.432s [client] PATCH m-line 0 (mid video0) candidate:1 1 UDP 2015363327 192.0.2.10 55263 typ host -> 204 No Content
 …
 0.450s [client] PATCH m-line 0 (mid video0) candidate:17 1 UDP 2015367423 2001:db8::1a 36277 typ host -> 204 No Content
 0.451s [client] PATCH m-line 0 (mid video0) end-of-candidates -> 204 No Content
 0.452s [server] handle-ice m-line 0 mid None: a=candidate:1 1 UDP 2015363327 192.0.2.10 43704 typ host
 0.453s [client] PATCH m-line 1 (mid audio1) candidate:1 1 UDP 2015363327 192.0.2.10 43704 typ host -> 204 No Content
 …
 4.431s [server] handle-ice m-line 0 mid None: a=candidate:9999 1 UDP 2122260223 192.0.2.1 50000 typ host
 4.434s [server] handle-ice m-line 0 mid None: a=candidate:9999 1 UDP 2122260223 192.0.2.1 50000 typ host
 4.437s [server] handle-ice m-line 0 mid None: a=candidate:9999 1 UDP 2122260223 192.0.2.1 50000 typ host

=== WHEP with upstream main (9cb42da34)
trickled candidates accepted: 34/34
candidates handed to the server's webrtcbin, per m-line: {0: 34}
fragment with only mid audio1 (m-line 1): 204 No Content
ICE restart (new ice-ufrag): 204 No Content
unknown session: 204 No Content
client ice-connection-state: Connected
server ice-connection-state: Completed
```

The three `192.0.2.1` lines are the last three PATCHes: the `audio1`-only
fragment, the ICE restart, and the unknown session. All three reach
webrtcbin as m-line 0.

### WHEP, fork

```
 0.086s [client] POST http://127.0.0.1:8191/whep/endpoint: offer with 0 candidate(s)
 0.406s [client] 201 Created, session http://127.0.0.1:8191/whep/resource/b3121184-3f33-406f-ba96-494f4afb7e7a
 0.408s [server] handle-ice m-line 0 mid Some("video0"): candidate:1 1 UDP 2015363327 192.0.2.10 57671 typ host
 0.410s [client] PATCH m-line 0 (mid video0) candidate:1 1 UDP 2015363327 192.0.2.10 57671 typ host -> 204 No Content
 …
 0.499s [client] PATCH m-line 0 (mid video0) candidate:17 1 UDP 2015367423 2001:db8::1a 50607 typ host -> 204 No Content
 0.506s [client] PATCH m-line 0 (mid video0) end-of-candidates -> 204 No Content
 0.510s [server] handle-ice m-line 1 mid Some("audio1"): candidate:1 1 UDP 2015363327 192.0.2.10 49237 typ host
 0.513s [client] PATCH m-line 1 (mid audio1) candidate:1 1 UDP 2015363327 192.0.2.10 49237 typ host -> 204 No Content
 …
 4.409s [server] handle-ice m-line 1 mid Some("audio1"): candidate:9999 1 UDP 2122260223 192.0.2.1 50000 typ host

=== WHEP with fork webrtc-whip-whep-trickle-ice (f9c7317b5)
trickled candidates accepted: 34/34
candidates handed to the server's webrtcbin, per m-line: {0: 17, 1: 17}
fragment with only mid audio1 (m-line 1): 204 No Content
ICE restart (new ice-ufrag): 422 Unprocessable Entity
unknown session: 404 Not Found
client ice-connection-state: Completed
server ice-connection-state: Connected
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
`congestion-control=disabled` in `server::whep()` and run `whep`.
