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
of the fork. Both servers then handle trickle ICE the same way:

- They hand each candidate to the m-line named by the `a=mid` of its media
  section. Candidates before the first `m=` line go to the first media
  section.
- They send an `ETag` with the SDP answer and require it in the `If-Match`
  header of each PATCH, as RFC 9725 asks: `428` without `If-Match`, `412`
  with another entity-tag.
- They answer `422` to ICE restarts, which they don't support, `404` for
  unknown sessions, `409` before the session has the client's SDP, and
  `400` for fragments that don't match the session.

## The repro (command line)

A Rust program with both sides in one process:

- **`src/server.rs`**: `whipserversrc` or `whepserversink` on 127.0.0.1.
  It logs each `handle-ice` the signaller emits and counts the candidates
  it hands to webrtcbin, per m-line.
- **`src/client.rs`**: a plain `webrtcbin` that behaves like a browser.
  - It POSTs its offer before it has gathered any candidate and applies
    the answer right away.
  - For 4 s, it PATCHes each candidate in its own fragment (`a=ice-ufrag`,
    `a=ice-pwd`, `m=`, `a=mid`, `a=candidate`), plus `a=end-of-candidates`,
    with the answer's `ETag` in `If-Match` if there is one.
  - Then it sends the edge cases one at a time, listed in the table below.
- **`src/main.rs`**: starts both and prints the summary.

The WHIP client sends one video stream (max-bundle). The WHEP client
receives video and audio with `bundle-policy=none`, so that each m-line has
its own ICE transport and its own ICE credentials.

The features `upstream` and `fork` choose the gst-plugin-webrtc to test:
`main` of gst-plugins-rs, or the fork branch. Both follow their branch, and
`Cargo.lock` isn't committed, so a fresh clone builds the latest of each.
Run `cargo update` to pick up new commits in an existing checkout.

```sh
cargo run --features upstream -- whip   # or whep
cargo run --features fork -- whip

./compare.sh   # all four, summaries only
```

Requires GStreamer ≥ 1.28 development files with webrtcbin, libnice, VP8 and
Opus (tested with 1.28.7 on Linux), and Rust ≥ 1.92.

## Results

The number of candidates depends on the host's network interfaces; the
test host gathers 15 per m-line. The same on every run:

| | WHIP `main` | WHIP fork | WHEP `main` | WHEP fork |
|---|---|---|---|---|
| trickled candidates | `501` (0/15) | `204` (15/15) | `204` (30/30) | `204` (30/30) |
| handed to webrtcbin, per m-line | none | `{0: 15}` | **`{0: 30}`** | `{0: 15, 1: 15}` |
| fragment with only the last mid | | | `204`, **m-line 0** | `204`, m-line 1 |
| candidate before any `m=` line | `501` | `204`, m-line 0 | `204`, **dropped** | `204`, m-line 0 |
| `If-Match: *` | `501` | `204` | `204` | `204` |
| no `If-Match` | `501` | `428` | **`204`** | `428` |
| wrong `If-Match` | `501` | `412` | **`204`** | `412` |
| ICE restart (other `ice-ufrag`) | `501` | `422` | **`204`**, handed to webrtcbin | `422` |
| unknown session | `501` | `404` | **`204`**, handed to webrtcbin | `404` |

The WHIP client has only one m-line, so it has no "only the last mid" case.

ICE connects in all four on localhost: without the trickled candidates,
the server still learns the client's address from the client's
connectivity checks (peer-reflexive candidates). Which pair ICE selects,
and whether the server reports its remote candidate as `host` or `prflx`,
depends on timing, so the repro doesn't report it.

`./compare.sh` prints, for WHEP with the fork:

```
=== WHEP with fork webrtc-whip-whep-trickle-ice
trickled candidates accepted: 30/30
candidates handed to the server's webrtcbin, per m-line: {0: 15, 1: 15}
fragment with only mid audio1 (m-line 1): 204 No Content, handed to m-line(s) [1]
candidate before the first m= line, no m= line: 204 No Content, handed to m-line(s) [0]
If-Match: *: 204 No Content, handed to m-line(s) [0]
no If-Match: 428 Precondition Required, handed to m-line(s) []
wrong If-Match: 412 Precondition Failed, handed to m-line(s) []
ICE restart (new ice-ufrag): 422 Unprocessable Entity, handed to m-line(s) []
unknown session: 404 Not Found, handed to m-line(s) []
client ice-connection-state: Completed
server ice-connection-state: Completed
```

and with `main`:

```
=== WHEP with upstream main
trickled candidates accepted: 30/30
candidates handed to the server's webrtcbin, per m-line: {0: 30}
fragment with only mid audio1 (m-line 1): 204 No Content, handed to m-line(s) [0]
candidate before the first m= line, no m= line: 204 No Content, handed to m-line(s) []
If-Match: *: 204 No Content, handed to m-line(s) [0]
no If-Match: 204 No Content, handed to m-line(s) [0]
wrong If-Match: 204 No Content, handed to m-line(s) [0]
ICE restart (new ice-ufrag): 204 No Content, handed to m-line(s) [0]
unknown session: 204 No Content, handed to m-line(s) [0]
client ice-connection-state: Completed
server ice-connection-state: Completed
```

## Browser repro

The same servers, with real browsers as clients:

```sh
cargo run --features upstream --bin web   # or --features fork
# open http://127.0.0.1:8189/
```

`src/bin/web.rs` runs `whipserversrc` and `whepserversink` and serves the
test page (`web/index.html`) on port 8189. It proxies `/whip/*` and
`/whep/*` to the signallers (they send no CORS headers), and the page asks
`/repro/session/{id}` which candidates the server handed to webrtcbin.

On the page:

- Pick **WHIP** to publish a canvas video and a tone (no camera
  permission needed), or **WHEP** to play the server's test streams.
- **Trickle ICE** sends the offer right away and PATCHes each candidate,
  the way browser WHIP/WHEP clients do. Unchecked, the page waits for ICE
  gathering and puts all candidates in the offer.
- **No BUNDLE** removes `a=group:BUNDLE` from the offer, so that each
  m-line has its own ICE transport. With BUNDLE, browsers only gather and
  trickle for the first m-line, which hides the WHEP bug.
- **Start**, then wait about 5 s. The page shows each PATCH status and
  each candidate the server handed to webrtcbin, with the m-line it should
  have gone to. Then it sends the same edge cases as the command-line
  client and shows each status next to what the fork answers. Rows in red
  are wrong. **Stop** sends the DELETE; restart the server if a session
  gets stuck.

Browsers hide local addresses behind mDNS names (`….local`) unless the page
has camera or microphone permission, so the candidates look like
`candidate:0 1 UDP 2122252543 6e05….local 46581 typ host`.

### Automated, with Playwright

```sh
cd web-test
npm install
npx playwright install chromium firefox
node run.mjs   # server from above must be running
```

`run.mjs` runs WHIP and WHEP, each with and without BUNDLE, in Chromium and
Firefox, and prints `ok` or `FAIL` for each edge case. Results on Linux
(headless Chromium gathers one candidate per m-line, Firefox four):

| | WHIP `main` | WHIP fork | WHEP `main` | WHEP fork |
|---|---|---|---|---|
| Chromium, no BUNDLE: PATCH statuses | `501` ×2 | `204` ×2 | `204` ×2 | `204` ×2 |
| Chromium, no BUNDLE: handed to webrtcbin | none | `{0: 1, 1: 1}` | **`{0: 2}`**, 1 wrong | `{0: 1, 1: 1}` |
| Firefox, no BUNDLE: PATCH statuses | `501` ×8 | `204` ×8 | `204` ×8 | `204` ×8 |
| Firefox, no BUNDLE: handed to webrtcbin | none | `{0: 4, 1: 4}` | **`{0: 8}`**, 4 wrong | `{0: 4, 1: 4}` |
| with BUNDLE (both browsers) | `501` | `204`, all on m-line 0 | `204`, all on m-line 0 | `204`, all on m-line 0 |
| edge cases, all 8 runs | 52 of 56 fail | 56 ok | | |

ICE connected in every run.

Browsers use the same ICE credentials for every m-line, even without
BUNDLE. The command-line WHEP client has different ones per m-line, which
only it tests: a fragment for m-line 1 starts with m-line 1's credentials,
which aren't an ICE restart.

### On macOS

Tested on macOS 26 (Apple silicon) with the official GStreamer 1.28.7
installer, by hand in the browser:

```sh
F=/Library/Frameworks/GStreamer.framework/Versions/1.0
$F/bin/gst-inspect-1.0 webrtcbin nicesrc vp8enc opusenc   # all four must exist
PKG_CONFIG_PATH=$F/lib/pkgconfig cargo build --features upstream --bin web
install_name_tool -add_rpath $F/lib target/debug/web   # again after each build
target/debug/web
```

Then open http://127.0.0.1:8189/. With Homebrew's GStreamer
(`brew install gstreamer libnice-gstreamer pkgconf`), `cargo run` should
work without the rpath; that is untested. `run.mjs` also accepts `webkit`
(`node run.mjs http://127.0.0.1:8189 chromium,firefox,webkit`), also
untested. macOS may ask whether `web` may accept incoming connections;
allow it.

## Side finding: webrtcsink deadlocks without BUNDLE

With congestion control enabled (the default) and one ICE transport per
m-line, `whepserversink` deadlocks once the session starts, on both `main`
and the fork. The repro disables congestion control to get around it.

`start_session()` connects to webrtcbin's `request-aux-sender`, which is
emitted once per transport. The handler (`net/webrtc/src/webrtcsink/imp.rs`)
locks `settings`, then sets
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
