//! `redcam` — solid red 1080p30 virtual camera built on the `pipewire-vircam` crate.
//!
//! Usage:
//! ```text
//! redcam [--name NAME] [--mode WxH@FPS]...
//! ```
//!
//! Default: one mode, 1920x1080@30, all packed raw formats
//! (RGBA/BGRA/BGRx/RGBx/BGR/RGB/YUY2/UYVY/GREY).

use std::sync::atomic::{AtomicBool, Ordering};

use pipewire_vircam::{Camera, Config, Format, Mode, Negotiated, Plane, State};

static NODE_PRINTED: AtomicBool = AtomicBool::new(false);

/// Fill `plane` with a solid byte value.
fn fill_solid(p: &mut Plane, value: u8) {
    let n = p.stride as usize * p.height as usize;
    p.data[..n].fill(value);
}

/// Fill a packed format plane with the "red" byte pattern repeated across
/// the row.
fn fill_packed(p: &mut Plane, pattern: &[u8]) {
    let n = p.stride as usize * p.height as usize;
    let mut i = 0;
    while i < n {
        for (k, &v) in pattern.iter().enumerate() {
            if i + k < n {
                p.data[i + k] = v;
            }
        }
        i += pattern.len();
    }
}

/// The "red" byte pattern for a single-plane format (one entry per
/// pixel/group). YUV values are the BT.709 limited-range red equivalent.
fn red_pattern(format: Format) -> Option<&'static [u8]> {
    match format {
        Format::Rgba => Some(&[255, 0, 0, 255]),
        Format::Bgra => Some(&[0, 0, 255, 255]),
        Format::Bgrx => Some(&[0, 0, 255, 0]),
        Format::Rgbx => Some(&[255, 0, 0, 0]),
        Format::Bgr => Some(&[0, 0, 255]),
        Format::Rgb => Some(&[255, 0, 0]),
        Format::Yuy2 => Some(&[63, 104, 63, 240]),
        Format::Uvyvy => Some(&[104, 63, 240, 63]),
        _ => None,
    }
}

/// Fill every plane of the frame with the solid-red representation for the
/// negotiated format.
fn fill_red(frame: &mut pipewire_vircam::Frame, _negotiated: &pipewire_vircam::Negotiated) {
    match frame.format {
        // Packed single-plane formats: repeat the byte pattern.
        Format::Rgba
        | Format::Bgra
        | Format::Bgrx
        | Format::Rgbx
        | Format::Bgr
        | Format::Rgb
        | Format::Yuy2
        | Format::Uvyvy => {
            let pattern =
                red_pattern(frame.format).expect("single-plane formats all have a red pattern");
            fill_packed(&mut frame.planes[0], pattern);
        }
        // GREY: solid luma.
        Format::Grey => {
            fill_solid(&mut frame.planes[0], 63);
        }
        // Nothing else to fill (all packed formats handled above).
        _ => {}
    }
}

fn parse_mode(s: &str) -> Result<Mode, String> {
    // "WxH@FPS"
    let (wh, fps) = s
        .split_once('@')
        .ok_or_else(|| format!("mode needs WxH@FPS: {s}"))?;
    let (w, h) = wh
        .split_once('x')
        .ok_or_else(|| format!("mode needs WxH@FPS: {s}"))?;
    let w: u32 = w.parse().map_err(|_| format!("bad width: {s}"))?;
    let h: u32 = h.parse().map_err(|_| format!("bad height: {s}"))?;
    let fps: u32 = fps.parse().map_err(|_| format!("bad fps: {s}"))?;
    Ok(Mode {
        width: w,
        height: h,
        fps: vec![fps],
        formats: Format::all().to_vec(),
    })
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut name = "redcam".to_string();
    let mut modes: Vec<Mode> = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--name" => {
                name = args.next().unwrap_or_else(|| {
                    eprintln!("redcam: --name needs a value");
                    std::process::exit(2);
                });
            }
            "--mode" => {
                let v = args.next().unwrap_or_else(|| {
                    eprintln!("redcam: --mode needs a value");
                    std::process::exit(2);
                });
                match parse_mode(&v) {
                    Ok(m) => modes.push(m),
                    Err(e) => {
                        eprintln!("redcam: {e}");
                        std::process::exit(2);
                    }
                }
            }
            other => {
                eprintln!("redcam: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    if modes.is_empty() {
        modes.push(Mode {
            width: 1920,
            height: 1080,
            fps: vec![30],
            formats: Format::all().to_vec(),
        });
    }

    let config = Config {
        name,
        media_name: "Red Virtual Camera".into(),
        modes,
        max_buffers: 4,
    };

    let cam = Camera::new(config).unwrap_or_else(|e| {
        eprintln!("redcam: {e}");
        std::process::exit(1);
    });

    if let Err(e) = cam
        .on_state(|st: State| match st {
            State::Disconnected { error: Some(_) } => {}
            State::Disconnected { error: None } => println!("stream state: \"unconnected\""),
            State::Paused { node_id } => {
                if !NODE_PRINTED.swap(true, Ordering::SeqCst) {
                    println!("node id: {node_id}");
                }
                println!("stream state: \"paused\"");
            }
            State::Streaming { node_id: _ } => println!("stream state: \"streaming\""),
        })
        .on_negotiated(|n: &Negotiated| {
            println!(
                "negotiated: format={} {}x{}@{}/{} stride={}",
                n.format.spa_id(),
                n.width,
                n.height,
                n.fps_num,
                n.fps_denom,
                n.stride
            )
        })
        .run(fill_red)
    {
        eprintln!("redcam: {e:?}");
        std::process::exit(1);
    }
}
