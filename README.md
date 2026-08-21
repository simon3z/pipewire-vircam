# PipeWire Virtual Camera for Rust

A small, focused Rust library (**`pipewire-vircam`**) for building **PipeWire virtual
cameras** from Rust: register a camera node, and fill its frames from your own
code. No v4l2loopback, no ffmpeg, no Python — just the official `pipewire`
Rust crate and a callback that hands you a fillable frame view whenever a
consumer connects.

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust edition: 2021](https://img.shields.io/badge/rust-2021-orange.svg)](https://doc.rust-lang.org/edition-guide)
[![Tests: 47 checks](https://img.shields.io/badge/tests-47%20checks-brightgreen.svg)](e2e.sh)

<!--
  Once hosted on GitHub with a CI workflow, replace the static test badge with:
    [![CI](https://github.com/<owner>/pipewire-vircam/actions/workflows/ci.yml/badge.svg)]
  and add:
    [![crates.io](https://img.shields.io/crates/v/pipewire-vircam.svg)](https://crates.io/crates/pipewire-vircam)
    [![docs.rs](https://img.shields.io/badge/docs.rs-pipewire--vircam-green.svg)](https://docs.rs/pipewire-vircam)
  The test count badge is maintained by hand (or by a CI step that updates
  the number) because it reflects the E2E harness, not just cargo test.
-->

This repo is a **library + a working demo** that proves it end to end:

1. **`pipewire-vircam`** (the crate) — the library. Configure a camera name and
   any set of (size, fps, format) modes; it registers the PipeWire node and
   calls *your* callback with a fillable, self-describing frame view each
   frame while a consumer is connected. The app's own code produces the
   pixels.
2. **`redcam`** (a demo binary) — built on `pipewire-vircam`, fills every frame with
   **solid red** at **1920×1080 @ 30 fps** by default. It is the runnable
   example *and* the target of the self-evaluation harness below, so "it
   works" is a machine-checked fact, not a claim.

Supports the uncompressed raw formats (RGBA/BGRA/BGRx/RGBx/BGR/RGB and
I420/NV12/NV21/YUY2/UYVY/GREY) and multiple modes per camera.

No v4l2loopback, no ffmpeg, no Python — just the official `pipewire` Rust
crate and a small harness that proves, with no human eyeballing, that a real
PipeWire consumer actually receives red 1080p frames at the right rate.

```
src/            the `pipewire-vircam` crate (the primary deliverable)
  lib.rs          public API (Camera, Config, Mode, Format, Frame, …)
  camera.rs       node + stream + driver timer + callbacks
  pod.rs          SPA POD construction (EnumFormat / buffers / meta)
  error.rs        the error type
  bin/redcam.rs   the solid-red demo binary (harness target)
reference/    standalone C (not part of the crate) — built to repo root
  redcam.c        the same red camera in C (reference producer, `redcam-c`)
  redcam-test.c   the independent capture consumer + oracle (`redcam-test`)
ci.sh           CI quality gate (fmt, clippy, package, test, arborist)
e2e.sh          E2E harness (needs live PipeWire session)
Makefile        `make`, `make test`, `make e2e`, `make clean`
```

### The red-cam trio

Three files share the `redcam` stem and form one connected concept — *the
same solid-red camera, demonstrated and verified in three ways*:

- **`src/bin/redcam.rs`** — the **demo**: the primary, built from `pipewire-vircam`.
  This is the harness target.
- **`reference/redcam.c`** — the **reference implementation**: the same red
  camera written standalone in C, no crate. It is a second, independent
  implementation of the concept (and a fallback you can point the harness at
  with `RED_BIN=redcam-c`). It is *not* kept in lockstep with the crate — it
  offers only the packed-raw formats.
- **`reference/redcam-test.c`** — the **oracle**: an independent C consumer
  that captures from *any* of the above and asserts the pixels, size, fps, and
  (optionally) sequence. It shares no code with any producer, so it is the
  honest check that frames really arrive red.

The crate (`src/`) is the deliverable; the two `reference/` files are
standalone C that deliberately stays outside it. All three are named after
the thing they each are a form of: `redcam`.

## Requirements

- A running **PipeWire** + **WirePlumber** session (this is a session-managed
  node, not a raw core export). `wpctl status` should work.
- **Rust** (stable ≥ 1.80) + `cargo`, and `pkg-config libpipewire-0.3`
  (PipeWire ≥ 1.x, SPA ≥ 0.2) for the C oracle.
- A C compiler (gcc/clang) for the `redcam-test` oracle and `redcam-c`
  reference.
- For the integration check only: GStreamer with the `pipewire` plugin
  (`gst-plugins-bad` + `gstreamer-plugins-pipewire`), and `ImageMagick`
  (`identify`/`convert`) to verify the captured PNG.

## Build

```sh
make
```

Builds the **Rust** `redcam` (via `cargo build --release`) and the **C**
`redcam-test` oracle. `make redcam-c` also builds the C reference producer.

## Run the camera

```sh
./target/release/redcam
# or: make redcam && ./target/release/redcam
```

On startup it prints the node id and, as a consumer connects, the negotiated
format, e.g.

```
node id: 90
stream state: "paused"
negotiated: format=11 1920x1080@30/1 stride=7680
stream state: "streaming"
```

It registers a node visible in `wpctl status` (under **Video**) and `pw-cli ls`
as:

```
node.name = "redcam"
MediaClass "Video/Source"
MediaName "Red Virtual Camera"
```

Leave it running and any PipeWire-aware consumer can select it.

### Options

```
redcam [--name NAME] [--mode WxH@FPS]...
```

`--mode` is repeatable. The default is a single `1920x1080@30` mode offering
every supported format. (Formats are per-mode in `Config`; parsed `--mode`
values get the full supported set.)

## Using the `pipewire-vircam` crate

The minimal example lives in [`examples/mycam.rs`](examples/mycam.rs) (built
by `cargo build --examples`). In brief:

```rust
use pipewire_vircam::{Camera, Config, Format, Mode, Negotiated};

fn fill(frame: &mut pipewire_vircam::Frame, _negotiated: &Negotiated) {
    // `frame` is self-describing: the negotiated format/size plus one plane
    // per buffer. Packed formats have one plane; planar YUV has several
    // (Y, then U/V or interleaved UV). Fill every plane.
    for plane in &mut frame.planes {
        let len = (plane.stride * plane.height) as usize;
        // ... write your pixels into plane.data[..len] ...
        plane.data[..len].fill(0);
    }
}

// `Camera::new` creates the node and does NOT block. `.run(fill)` installs
// the driver timer and blocks until SIGINT/SIGTERM.
let cam = Camera::new(Config {
    name: "mycam".into(),
    media_name: "My Camera".into(),
    modes: vec![Mode { width: 1920, height: 1080, fps: vec![30], formats: vec![Format::Rgba] }],
    max_buffers: 4,
})?;
cam.run(fill)?;
```

- `Camera::new` creates the node and does **not** block.
- `.on_state(...)` / `.on_negotiated(...)` are optional builders; `.run(fill)`
  blocks until SIGINT/SIGTERM.
- The `fill` closure takes `(&mut Frame, &Negotiated)` and is called on the
  main-loop thread, at the negotiated fps, only while a consumer is connected
  and streaming. `Frame` is self-describing, so your code adapts to whatever
  the consumer negotiated.
- `State` reports `Disconnected { error }` / `Paused { node_id }` /
  `Streaming { node_id }`; `Negotiated` carries `format`, `width`, `height`,
  `fps_num/denom` (plus `fps()`), `stride`, `node_id`.
- `Frame` also carries timing: `seq` (per-camera frame counter, from 0)
  and `pts` (presentation timestamp, nanoseconds since the camera started —
  monotonic). The same `pts` is written to the buffer's `Header` meta when
  that meta is negotiated (best-effort; GStreamer `pipewiresrc` maps it to
  the frame PTS). See the changelog for the 0.6.0 note on meta negotiation.

## How to consume `redcam`

Pick the node and open an input stream on it. Three concrete ways:

1. **This repo's oracle** (the self-evaluation):
   ```sh
   NODE_ID=$(./target/release/redcam | awk '/node id:/{print $3}')
   ./redcam-test "$NODE_ID" 30
   ```
   Captures 30 frames through a real PipeWire input stream and asserts every
   pixel is red, size is 1920×1080, and rate ≈ 30 fps.

2. **GStreamer** (a real third-party app — no ffmpeg):
   ```sh
   gst-launch-1.0 pipewiresrc target-object=redcam \
       ! videoconvert ! pngenc snapshot=true ! filesink location=/tmp/red.png
   ```
   `target-object=redcam` selects the node by name. You can also capture a
   few frames to a file: `... num-buffers=10 ! ... ! filesink location=/tmp/red.png`.

3. **Any PipeWire-aware app** (e.g. OBS, a camera selector in your media
   stack) will see "Red Virtual Camera" as a capture device.

## Self-evaluation (`make e2e`)

`make e2e` runs `e2e.sh`, which builds the **Rust** camera and the C oracle,
then for **three sequences** (two identical full 1080p sequences, plus a
multi-size/multi-fps sequence) starts the camera and checks:

1. **Registration** — the node is in `pw-cli` with `MediaClass "Video/Source"`
   and `MediaName "Red Virtual Camera"`.
2. **Session manager** — the node is visible in `wpctl status` (Video tree)
   (full sequences).
3. **Core assertion** — `redcam-test` (the independent C oracle) captures 30
   frames per check and proves every pixel is red, the size is exact, and fps
   is within [24, 40] of the requested rate — for each of the 12 formats
   (full sequences) and for six size/fps/format combinations
   (1920×1080@30, 1280×720@60, 640×480@15).
4. **Real-app integration** — a GStreamer `pipewiresrc → videoconvert →
   pngenc` pipeline captures a frame from the node; the PNG is verified to be
   1920×1080 and red (via ImageMagick) (full sequences).
5. **Clean teardown** — killing redcam removes the node; no error lines in the
   redcam log.

Exit code is 0 only if *all* 47 checks pass on *all* sequences. Every
long-running process (redcam, consumers, gst pipelines) is backgrounded and
always killed. It needs a live PipeWire/WirePlumber session.

`make test` runs `ci.sh`, the lean quality gate: `cargo fmt`, `clippy`,
`package`, `test` (unit tests + timing benchmarks) and an arborist complexity
check. No live session needed.

To test the C reference producer instead of the Rust one:
`RED_BIN=redcam-c make e2e` (after `make redcam-c`).

### What the oracle asserts

| Check | Proves |
|-------|--------|
| `size_ok` | negotiated size is exactly the requested size (default 1920×1080) |
| `red_ok` | **every** pixel of **every** frame is solid red for the negotiated format |
| `fps` | frames arrive at ≈the requested fps (timer is driving, not a one-shot) |
| `frames` | N distinct frames were received |
| `seq_ok` | (best-effort) per-frame sequence advanced — only when the Header meta is negotiated |
| `pts_ok` | (best-effort) per-frame `pts` advanced strictly — only when the Header meta is present with valid (≥ 0) pts |

`red_ok` is a full-pixel `memcmp` against a precomputed red row for the exact
negotiated format (`RGB`→`FF0000`, `BGR`→`0000FF`, `RGBA`→`FF0000FF`,
`BGRA`→`0000FFFF`), so it is exact, not a sampled/averaged check.

### Why no ffmpeg / v4l2loopback?

The point is a *PipeWire-native* virtual camera: it registers as a normal
`Video/Source` node in the PipeWire/WirePlumber graph, so any
PipeWire-aware consumer (GStreamer `pipewiresrc`, your media stack, etc.) can
select it. `v4l2loopback` would fake a `/dev/video` device; here the camera
lives in the session, which is the more general and PipeWire-idiomatic
approach.

## Design notes

- **Official `pipewire` 0.10 crate**, safe API throughout except three narrow
  `sys` calls (`pw_stream_connect`, `pw_stream_update_params`, whose `&mut
  [&Pod]` arguments can't be built from owned PODs in this crate version, and
  `pw_stream_is_lazy`, which has no safe wrapper).
- **`pw_stream`** API (as in upstream `video-src.c` / `video-play.c`), not a
  raw SPA node export — it handles buffer allocation and negotiation plumbing.
- **Fixed spec, all uncompressed formats.** The demo advertises 1920×1080@30
  with every format the crate can fill byte-exactly (the packed RGB family and
  I420/NV12/NV21/YUY2/UYVY/GREY), so we never negotiate a format we can't
  produce exactly. For YUV, "red" is filled as the BT.709 limited-range
  equivalent (Y=63, Cb=104, Cr=240); MJPG and 10/16-bit formats are excluded
  (they need an encoder or aren't raw).
- **`EnumFormat` size is *plain*; framerate may be a `Choice`.** The size is a
  plain `Rectangle` (not `CHOICE_RANGE`) — apps like OBS parse the size with a
  plain-rectangle parse and silently drop entries whose size is a choice
  value, which was the cause of OBS showing empty format/resolution dropdowns.
  Framerate is a plain `Fraction` for one fps per entry, or an enum `Choice`
  (default + alternatives) when a mode advertises several fps for the same
  (format, size); `pw-topology` renders that as `default: 30/1, alt1: 60/1`,
  and OBS groups consecutive entries into one row with an fps sub-list.
  A `maxFramerate` property is added when an entry has more than one fps
  choice. Deliberately absent: `VideoModifier` — its *presence* (even with
  the value `0`) makes gstreamer-pipewire request DmaBuf-only buffers, and
  the daemon then fails with "alloc buffers: Operation not supported"; the
  v4l2 node emits it only for formats with a real modifier.
- **`ParamLatency` is advertised (static) and replied with (negotiated).**
  Two `Latency` objects are advertised (input: unset — no input port;
  output: 1 frame/buffer, min/max rate = the advertised fps range, min/max
  ns = the frame period at those rates), and the `ParamBuffers` negotiation
  reply carries the negotiated frame period (min = max) — what the v4l2
  driver emits after accepting a Format. Browsers (unlike OBS) validate this
  param when deciding whether a stream is usable.
- **Source is the DRIVER**; a 1 ms timer produces at most one frame per
  negotiated period (software pacing, so fps survives renegotiation without
  re-arming). The consumer is passive.
- **Consumer needs `PW_STREAM_FLAG_AUTOCONNECT` + `INACTIVE` + a follow-up
  `pw_stream_set_active(true)`,** and must reply with `ParamBuffers` in
  `param_changed` — this is what makes WirePlumber link the consumer to the
  producer (WirePlumber's session manager links the graph off
  `node.autoconnect`).
- **`SignalSource` must be kept alive.** The crate's signal/timer sources
  unregister on drop; `Camera` holds the SIGINT/SIGTERM sources for the
  loop's duration so the process exits (and the node is removed) on signal.

## Development

```sh
make test          # lean CI gate (fmt, clippy, package, test, arborist)
make e2e           # the E2E harness (Rust redcam + C oracle; live PipeWire)
make redcam-c      # C reference producer
make clean && make # rebuild
./ci.sh            # same as make test
./e2e.sh           # same as make e2e
```

See the "Key decisions" / design sections above for how it works; the
source files are self-documenting, and the E2E harness (`e2e.sh`) is the
source of truth for what "works."

## License

Copyright 2026 Federico Simoncelli. Licensed under the Apache License,
Version 2.0 (see the `LICENSE` file).
