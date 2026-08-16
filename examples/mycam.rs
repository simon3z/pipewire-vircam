//! Minimal `pipewire-vircam` usage: create a camera and fill its frames from
//! code. Run with:  cargo run --example mycam
//!
//! This is the smallest realistic example of the public API. The `redcam`
//! binary (src/bin/redcam.rs) is the full-featured version (CLI, all formats).

use pipewire_vircam::{Camera, Config, Format, Mode, State};

/// Fill one frame. `frame` is self-describing: it carries the negotiated
/// format/size and a plane per buffer. For a packed format there is exactly
/// one plane; for planar YUV there are several (fill each).
fn fill(frame: &mut pipewire_vircam::Frame, _negotiated: &pipewire_vircam::Negotiated) {
    // Example: clear every byte of every plane. Real apps write their own
    // pixels here, using frame.format to know the layout and
    // (plane.stride, plane.height) for the geometry.
    for plane in &mut frame.planes {
        plane.data[..(plane.stride * plane.height) as usize].fill(0);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    Camera::new(Config {
        name: "mycam".into(),
        media_name: "My Camera".into(),
        modes: vec![Mode {
            width: 1920,
            height: 1080,
            fps: vec![30],
            formats: vec![Format::Rgba],
        }],
        max_buffers: 4,
    })?
    .on_state(|st: State| println!("{st:?}"))
    // `run` blocks until SIGINT/SIGTERM.
    .run(fill)?;

    Ok(())
}
