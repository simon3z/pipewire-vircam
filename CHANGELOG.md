# Changelog

All notable changes to this crate, per release.

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
