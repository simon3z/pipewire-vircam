# Changelog

All notable changes to this crate, per release.

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
