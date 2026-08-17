//! `pipewire-vircam` — create a PipeWire virtual camera from Rust and fill
//! from your own code.
//!
//! # The shape of the API
//!
//! ```rust,no_run
//! use pipewire_vircam::{Camera, Config, Format, Mode, State};
//! // Note: `negotiated` is the per-frame negotiation snapshot (format,
//! // size, fps, stride) — see [`Negotiated`].
//!
//! // 1080p@30/60 and 720p@30.
//! let config = Config {
//!     name: "mycam".into(),
//!     media_name: "My Virtual Camera".into(),
//!     modes: vec![
//!         Mode { width: 1920, height: 1080, fps: vec![30, 60], formats: vec![Format::Rgba, Format::Bgra] },
//!         Mode { width: 1280, height: 720, fps: vec![30], formats: vec![Format::Rgba] },
//!     ],
//!     max_buffers: 4,
//! };
//!
//! let cam = Camera::new(config).expect("create camera");
//! cam.on_state(|state| match state {
//!     State::Disconnected { error: Some(msg) } => eprintln!("consumer left: {msg}"),
//!     State::Disconnected { error: None } => println!("consumer left"),
//!     State::Paused { .. } => println!("consumer connected"),
//!     State::Streaming { .. } => println!("streaming"),
//! })
//! .run(|frame, negotiated| {
//!     // Called from the camera thread at the negotiated fps, only while a
//!     // consumer is connected and streaming. `frame` is self-describing:
//!     // width/height/format, and one fillable `Plane` per buffer plane.
//!     // Fill each plane's `data` (already sized stride x height).
//!     for p in &mut frame.planes {
//!         p.data.fill(0); /* e.g. a black frame */
//!     }
//! });
//! ```
//!
//! The camera drives the graph: a timer triggers one graph cycle per frame
//! period, the crate dequeues a buffer, hands you a [`Frame`] plus the
//! [`Negotiated`] snapshot for that frame, and returns the buffer when your
//! handler returns (RAII: you don't manage buffers).
//!
//! ## Error handling
//!
//! Construction failures (bad config, PipeWire connect failure) return
//! [`Error`]. A runtime stream error (e.g. the link dies) is reported as
//! [`State::Disconnected { error: Some(msg) }`] and logged to stdout as
//! `stream state: "error" <msg>`; the camera stays up and a new consumer can
//! reconnect.

mod camera;
mod error;
mod pod;

pub use camera::{Camera, Frame, Plane, State};
pub use error::Error;

/// A raw (uncompressed) video format the camera can produce.
///
/// All are single- or multi-plane *raw* formats (no compression). MJPG is
/// deliberately excluded (it needs an encoder).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Format {
    /// RGBA, 4 bytes/pixel.
    Rgba,
    /// BGRA, 4 bytes/pixel.
    Bgra,
    /// BGRx, 4 bytes/pixel (X unused).
    Bgrx,
    /// RGBx, 4 bytes/pixel (X unused).
    Rgbx,
    /// BGR, 3 bytes/pixel.
    Bgr,
    /// RGB, 3 bytes/pixel.
    Rgb,
    /// I420: Y plane + planar U and V (4:2:0).
    I420,
    /// NV12: Y plane + interleaved UV plane (4:2:0).
    Nv12,
    /// NV21: Y plane + interleaved VU plane (4:2:0).
    Nv21,
    /// YUY2: packed Y U Y V (4:2:2).
    Yuy2,
    /// UYVY: packed U Y V Y (4:2:2).
    Uvyvy,
    /// 8-bit grayscale, 1 byte/pixel.
    Grey,
}

/// The set of formats `pipewire-vircam` supports, in a stable order. Single source
/// truth for both the `Format` → SPA id mapping and its inverse, so the two
/// can never drift apart.
const ALL_FORMATS: &[Format] = &[
    Format::Rgba,
    Format::Bgra,
    Format::Bgrx,
    Format::Rgbx,
    Format::Bgr,
    Format::Rgb,
    Format::I420,
    Format::Nv12,
    Format::Nv21,
    Format::Yuy2,
    Format::Uvyvy,
    Format::Grey,
];

impl Format {
    /// The libspa `VideoFormat` this maps to (single source of truth for the
    /// SPA id).
    fn video_format(self) -> libspa::param::video::VideoFormat {
        use libspa::param::video::VideoFormat as V;
        match self {
            Format::Rgba => V::RGBA,
            Format::Bgra => V::BGRA,
            Format::Bgrx => V::BGRx,
            Format::Rgbx => V::RGBx,
            Format::Bgr => V::BGR,
            Format::Rgb => V::RGB,
            Format::I420 => V::I420,
            Format::Nv12 => V::NV12,
            Format::Nv21 => V::NV21,
            Format::Yuy2 => V::YUY2,
            Format::Uvyvy => V::UYVY,
            Format::Grey => V::GRAY8,
        }
    }

    /// The plane layout for this format at `width x height`: one
    /// `(stride_bytes, height)` per plane, in plane order.
    pub fn planes(&self, width: u32, height: u32) -> Vec<(u32, u32)> {
        match self {
            Format::Rgba | Format::Bgra | Format::Bgrx | Format::Rgbx => {
                vec![(width * 4, height)]
            }
            Format::Bgr | Format::Rgb => vec![(width * 3, height)],
            Format::I420 => {
                vec![
                    (width, height),
                    (width / 2, height / 2),
                    (width / 2, height / 2),
                ]
            }
            Format::Nv12 | Format::Nv21 => {
                vec![(width, height), (width, height / 2)]
            }
            Format::Yuy2 | Format::Uvyvy => vec![(width * 2, height)],
            Format::Grey => vec![(width, height)],
        }
    }

    /// All formats `pipewire-vircam` supports, in a stable order.
    pub fn all() -> &'static [Format] {
        ALL_FORMATS
    }

    /// The SPA format id this maps to (`SPA_VIDEO_FORMAT_*`). Derived from the
    /// libspa constant, not hardcoded.
    pub fn spa_id(self) -> u32 {
        self.video_format().as_raw()
    }

    /// Inverse of [`Format::spa_id`]: the `Format` for a SPA id, or `None` if
    /// the id is not one we support.
    fn from_spa_id(id: u32) -> Option<Format> {
        ALL_FORMATS.iter().copied().find(|f| f.spa_id() == id)
    }

    /// The format's lowercase name (`"rgba"`, `"nv12"`, ...). Inverse of
    /// [`Format::from_str`].
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Rgba => "rgba",
            Format::Bgra => "bgra",
            Format::Bgrx => "bgrx",
            Format::Rgbx => "rgbx",
            Format::Bgr => "bgr",
            Format::Rgb => "rgb",
            Format::I420 => "i420",
            Format::Nv12 => "nv12",
            Format::Nv21 => "nv21",
            Format::Yuy2 => "yuy2",
            Format::Uvyvy => "uyvy",
            Format::Grey => "grey",
        }
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for Format {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ALL_FORMATS
            .iter()
            .copied()
            .find(|f| f.as_str() == s)
            .ok_or_else(|| {
                format!(
                    "unknown format \"{s}\" (try one of: {})",
                    ALL_FORMATS
                        .iter()
                        .map(|f| f.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }
}

/// One advertised (resolution, framerates, formats) combination.
#[derive(Clone, Debug)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    /// Framerates in whole frames per second (>= 1). Each is advertised
    /// separately as `fps/1`; the negotiated rate
    /// (`Negotiated::fps_num/fps_denom`) may differ, and consumers pick it
    /// within what you advertise.
    pub fps: Vec<u32>,
    /// Formats offered for this size/framerate (>= 1).
    pub formats: Vec<Format>,
}

/// Camera configuration.
///
/// Validated in [`Camera::new`]: at least one mode, each with a sane
/// size/fps and at least one format. Duplicate (format, size, fps)
/// combinations are deduplicated before being advertised.
///
/// NOTE: you are responsible for ensuring your fill handler can actually
/// produce frames at the advertised rate. If processing takes longer than
/// `1/fps` seconds, the driver skips missed periods rather than bursting
/// catch-up frames.
#[derive(Clone, Debug)]
pub struct Config {
    /// Node name (`node.name`). This is what consumers target, e.g.
    /// `pipewiresrc target-object=<name>` or in OBS.
    pub name: String,
    /// Human readable name (`media.name`, nick, description).
    pub media_name: String,
    pub modes: Vec<Mode>,
    /// Upper bound on the buffer ring size advertised to consumers
    /// (they pick a count in `2..=max_buffers`). Default is 4.
    /// Each 1080p RGBA buffer is ~8 MB, so this caps ring memory.
    pub max_buffers: u32,
}

/// What the camera negotiated with the connected consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Negotiated {
    pub format: Format,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_denom: u32,
    /// Row stride in bytes (>= width * bytes-per-pixel).
    pub stride: u32,
    /// PipeWire node id of the connected consumer (0 until known).
    pub node_id: u32,
}

impl Negotiated {
    /// The negotiated framerate as a float (e.g. 29.97 for 30000/1001).
    pub fn fps(&self) -> f64 {
        self.fps_num as f64 / self.fps_denom.max(1) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::{Format, ALL_FORMATS};

    /// Every supported `Format` maps back to itself via its SPA id.
    #[test]
    fn format_spa_id_roundtrip() {
        for f in ALL_FORMATS {
            let back = Format::from_spa_id(f.spa_id());
            assert_eq!(back, Some(*f), "from_spa_id({}) != {f:?}", f.spa_id());
        }
    }

    /// No two supported formats share an SPA id (a copy-paste id collision
    /// would silently mis-negotiate).
    #[test]
    fn format_spa_ids_are_unique() {
        for (i, a) in ALL_FORMATS.iter().enumerate() {
            for b in ALL_FORMATS.iter().skip(i + 1) {
                assert_ne!(
                    a.spa_id(),
                    b.spa_id(),
                    "{} and {} share spa_id {}",
                    a.spa_id(),
                    b.spa_id(),
                    a.spa_id()
                );
            }
        }
    }

    /// Every format parses back from its name (`FromStr` round-trip), and
    /// unknown names are rejected.
    #[test]
    fn format_from_str_roundtrip() {
        for f in ALL_FORMATS {
            let parsed: Format = f.as_str().parse().unwrap_or_else(|e| panic!("{f:?}: {e}"));
            assert_eq!(parsed, *f);
        }
        assert!("mjpg".parse::<Format>().is_err());
        assert!("".parse::<Format>().is_err());
        // Names are case-sensitive (matches the wire names we document).
        assert!("RGBA".parse::<Format>().is_err());
    }

    /// `from_spa_id` rejects ids we don't support (e.g. MJPG, or garbage).
    #[test]
    fn from_spa_id_rejects_unknown() {
        assert_eq!(Format::from_spa_id(0), None); // SPA_VIDEO_FORMAT_UNKNOWN
        assert_eq!(Format::from_spa_id(3), None); // YV12 (not supported)
        assert_eq!(Format::from_spa_id(9999), None);
        assert_eq!(Format::from_spa_id(u32::MAX), None);
    }

    /// The per-plane layout (stride, height) for each format at a known
    /// size. This is the core stride/plane math — the thing most likely to
    /// regress.
    #[test]
    fn format_planes_layout() {
        let (w, h) = (1920u32, 1080u32);
        assert_eq!(Format::Rgba.planes(w, h), vec![(w * 4, h)]);
        assert_eq!(Format::Bgra.planes(w, h), vec![(w * 4, h)]);
        assert_eq!(Format::Bgrx.planes(w, h), vec![(w * 4, h)]);
        assert_eq!(Format::Rgbx.planes(w, h), vec![(w * 4, h)]);
        assert_eq!(Format::Bgr.planes(w, h), vec![(w * 3, h)]);
        assert_eq!(Format::Rgb.planes(w, h), vec![(w * 3, h)]);
        // I420: Y + planar U,V (half resolution each).
        assert_eq!(
            Format::I420.planes(w, h),
            vec![(w, h), (w / 2, h / 2), (w / 2, h / 2)]
        );
        // NV12/NV21: Y + interleaved UV/VU (half resolution).
        assert_eq!(Format::Nv12.planes(w, h), vec![(w, h), (w, h / 2)]);
        assert_eq!(Format::Nv21.planes(w, h), vec![(w, h), (w, h / 2)]);
        // Packed 4:2:2: 2 bytes/pixel, one plane.
        assert_eq!(Format::Yuy2.planes(w, h), vec![(w * 2, h)]);
        assert_eq!(Format::Uvyvy.planes(w, h), vec![(w * 2, h)]);
        // Grayscale: 1 byte/pixel, one plane.
        assert_eq!(Format::Grey.planes(w, h), vec![(w, h)]);
    }

    /// Every format has at least one plane whose stride is a multiple of 4
    /// (PipeWire row-stride alignment) and whose height divides the frame
    /// height evenly (chroma subsampling).
    #[test]
    fn planes_are_sane_for_all_formats() {
        let (w, h) = (1920u32, 1080u32);
        for f in ALL_FORMATS {
            let planes = f.planes(w, h);
            assert!(!planes.is_empty(), "{f:?} has no planes");
            for (stride, ph) in planes {
                assert_eq!(stride % 4, 0, "{f:?} stride {stride} not 4-aligned");
                assert_eq!(h % ph, 0, "{f:?} height {h} not divisible by plane {ph}");
            }
        }
    }
}
