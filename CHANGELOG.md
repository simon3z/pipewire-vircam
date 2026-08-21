# Changelog

All notable changes to this crate, per release.

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
