# Changelog

All notable changes to this crate, per release.

## 0.7.0

- **Breaking:** removed the planar formats `Format::I420`, `Format::Nv12`,
  and `Format::Nv21`. The crate now advertises and fills only packed
  formats (RGBA, BGRA, BGRx, RGBx, BGR, RGB, YUY2, UYVY, GREY). Chrome
  negotiates a planar format (e.g. I420) when it is advertised and then
  churns between Paused and Streaming, while packed-only works reliably.
  Planar support will be reintroduced once the Chrome-side rejection of the
  planar buffer path is fixed.
- POD: YUY2/UYVY now advertise limited range (16-235), BT.601 matrix, and
  BT.709 transfer + primaries (matching a real webcam; Chrome requires this).
  RGB keeps full range. Each buffer requests a `Header` meta (carries PTS),
  like a real camera. The initial connect no longer advertises `ParamLatency`
  (matching the C reference).
- The reference implementation (`reference/redcam.c`) now advertises YUY2 and
  UYVY, fills the `Header` meta (PTS/seq), sets `MAP_BUFFERS`, and uses
  `media.role = "Camera"` so browsers list the node as a camera source.
- `Frame::fill_black` and the `redcam` binary fill only packed formats.
- Internal refactor (no API change): the colorimetry constants in `pod.rs`
  that the sys bindings don't expose (`SPA_VIDEO_COLOR_TRANSFER_BT709`,
  `SPA_VIDEO_COLOR_PRIMARIES_BT709`) are now inlined at the property site
  instead of being module constants, and `pod.rs`/`camera.rs` were split
  into small private helpers (e.g. `format_properties`, `connect_output_
  stream`, `parse_negotiated`).

## 0.6.0

- `Frame` now exposes timing: `seq` (a per-camera frame counter, starting at
  0 and advancing by one per produced frame) and `pts` (presentation
  timestamp in nanoseconds, monotonic, ns since the camera started). The
  value is stamped *before* your fill callback runs, so it is the frame's
  presentation time; use it to capture your screen/backend at the right
  moment. The same `pts` is written to the buffer's `Header` meta when that
  meta is negotiated into the shared buffer (best-effort: the core only
  allocates meta regions both endpoints agreed on, and some setups — e.g.
  this project's e2e environment — don't, so the meta write is a no-op
  there and consumers fall back to their own timing, as the v4l2 node
  always does).
- The camera writes the full `SPA_META_Header` (`flags`, `offset`, `pts`,
  `dts_offset`, `seq`) on each dequeued buffer when the meta is present and
  fully sized; smaller (partial) meta regions are left untouched, so no
  out-of-bounds writes are possible. GStreamer-based consumers
  (`pipewiresrc`) read this meta and map `pts` to the buffer's PTS.
- The C reference (`reference/redcam.c`) now writes the same real `pts`
  (CLOCK_MONOTONIC, the domain the v4l2 node stamps on captures) instead of
  `pts = -1`, and no longer performs an 8-byte partial meta write (which was
  a latent out-of-bounds write of the 32-byte `seq` field). The oracle
  (`reference/redcam-test.c`) now also asserts, best-effort, that a
  producer's `pts` values advance strictly when the Header meta is present.

## 0.5.0

- The advertised params now include `ParamLatency` objects, one per
  direction: the input direction is unset (the node has no input port); the
  output direction declares one frame per buffer over the min..max advertised
  fps, with the frame period (ns) at those rates. The negotiation reply
  (together with `ParamBuffers`) now carries a `ParamLatency` with the
  negotiated frame period (min = max) — the same value the driver paces by.
- The advertised `EnumFormat` entries carry `maxFramerate` when the entry has
  more than one fps choice.
- Fix: the advertised `EnumFormat` entries no longer carry a `VideoModifier`
  property (which was always `0`). Its *presence* makes gstreamer-pipewire
  treat the format as a "modified" one and request DmaBuf-only buffers, which
  this node cannot provide — capturing with `pipewiresrc` failed with
  "alloc buffers: Operation not supported". The property is optional
  (absent = no modifier), and the v4l2 node emits it only for formats with a
  real modifier.

## 0.4.0

- The advertised `EnumFormat` entries now carry the colorimetry properties a
  regular PipeWire camera node advertises (`colorRange`, `colorMatrix`,
  `transferFunction`, `colorPrimaries`): BT.709 matrix/transfer/primaries,
  with BT.601 matrix for heights ≤ 480 (matching the v4l2 node's
  `V4L2_COLORSPACE_REC709` convention), and per-format color range —
  full range (0-255) for YUY2/UYVY like webcam YUYV422, limited range
  (16-235) for NV12/NV21/I420 like H.264 codecs.
- The advertised `EnumFormat` entries are now one per unique (format, size)
  combination, with the mode's framerates as a framerate `Choice` (enum) when
  several fps are advertised — matching what upstream v4l2 nodes advertise
  and how `pw-topology` renders `default: N/1, alt1: ...`. Single-fps entries
  keep a plain `Fraction`.
- Internal: the library no longer prints debug lines to stdout (error state
  is still propagated via `State` callbacks); color constants now come from
  the `libspa` sys bindings instead of hardcoded values (this also fixed
  `SPA_VIDEO_TRANSFER_BT709`, which is 5, not 6).
- The demo's `redcam` binary no longer prints per-frame negotiation lines;
  it still prints node id and stream state.

## 0.3.0

- Added `Camera::on_negotiate_accept`: an accept callback invoked before the
  camera replies with `ParamBuffers`; returning `Err` rejects the negotiated
  geometry (no reply is sent), so the user can set up their backend before the
  consumer starts pulling.
- **Breaking:** `Error` no longer carries the `Stream`, `UnsupportedFormat`,
  and `PipeWire` variants (they were never produced); runtime stream errors
  are reported via `State::Disconnected { error: Some(..) }`, and an
  unsupported negotiated format is handled by not replying with `ParamBuffers`.
  `Error` is now just `InvalidConfig` / `Connect`.
- The advertised `EnumFormat` entries are now ordered (format, size) outer / fps
  inner, so all framerates for a (size, format) are consecutive; consumers like
  OBS can group them into one row with an fps sub-list.

## 0.2.0

- **Breaking:** `Mode::fps` is now `Vec<u32>` (was `u32`), so a single size can
  advertise several framerates in one `Mode`.
- Added `Camera::quit_handle()` returning a `QuitHandle`: capture it before
  `Camera::run` and move it into a callback to signal a clean teardown from
  inside fill/state/negotiated handlers.
- Internal refactor: `Camera::run` decomposed into named handlers; advertised
  POD construction moved into `pod.rs` (`advertised_param_blobs`, unit-tested);
  `Format::video_format` no longer returns an impossible `Option`; the demo's
  red fill no longer allocates per frame.

## 0.1.0

- First release.
- `Camera`/`Config`/`Mode`/`Format`/`Frame`/`Negotiated`/`State`/`Plane` API.
- 12 video formats (packed RGB family, I420/NV12/NV21/YUY2/UYVY/GREY8), all
  uncompressed; software pacing via a 1 ms driver timer (fps survives
  renegotiation without re-arming).
- `redcam` demo binary + `examples/mycam.rs`.
