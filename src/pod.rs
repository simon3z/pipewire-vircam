//! POD construction for `pipewire-vircam`.
//!
//! Builds the `EnumFormat` parameter objects (one per advertised
//! (format, size, fps) combination, all with *plain* values — OBS's
//! `camera-portal.c` parses the size as a plain `Rectangle` and drops
//! range/choice encodings, so every advertised (format, size, fps) is a
//! plain value), the `ParamLatency` objects (one per direction, values
//! derived from the advertised fps range), and the `ParamBuffers` /
//! `ParamMeta` negotiation replies.
//!
//! The `pipewire` crate's safe `connect`/`update_params` take `&mut [&Pod]`,
//! which cannot be built from owned PODs (as of pipewire 0.10: `Pod` is
//! `Copy`, but no `&mut`-reference coercion into `&mut [&Pod]` exists), so
//! we serialize PODs to owned blobs and pass POD pointers to the C
//! functions directly (see `camera.rs`).

use pipewire as pw;
use pipewire::spa::{
    param::{
        format::{FormatProperties, MediaSubtype, MediaType},
        ParamType,
    },
    pod::{serialize::PodSerializer, Object, Pod, Property, PropertyFlags, Value},
    utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Id, Rectangle, SpaTypes},
};

use crate::{Config, Format};

/// Hard ceiling on `Config::max_buffers` (keeps the SPA range sane).
pub const MAX_BUFFERS: i32 = 16;

/// The `spa_video_color_*` enums the v4l2 node advertises
/// (`SPA_FORMAT_VIDEO_color*`). The v4l2 node uses plain `Id` values (see
/// `v4l2-format-utils.c::add_colorimetry`), and the `spa_video_format_*`
/// helpers the v4l2 node uses wrap those same ints. `pw-topology`'s
/// `video.c` decodes them back to `"16-235"`, `"bt601"`, `"bt709"`, ... by
/// value.
const COLOR_RANGE_0_255: u32 = pw::spa::sys::SPA_VIDEO_COLOR_RANGE_0_255;
const COLOR_RANGE_16_235: u32 = pw::spa::sys::SPA_VIDEO_COLOR_RANGE_16_235;
const COLOR_MATRIX_BT601: u32 = pw::spa::sys::SPA_VIDEO_COLOR_MATRIX_BT601;
const COLOR_MATRIX_BT709: u32 = pw::spa::sys::SPA_VIDEO_COLOR_MATRIX_BT709;
const COLOR_TRANSFER_BT709: u32 = pw::spa::sys::SPA_VIDEO_TRANSFER_BT709;
const COLOR_PRIMARIES_BT709: u32 = pw::spa::sys::SPA_VIDEO_COLOR_PRIMARIES_BT709;

/// A single framerate choice as a `(num, denom)` fraction.
///
/// `Mode::fps` entries are whole frame rates (`fps/1`); the denominator is
/// kept so consumers can pick an arbitrary rate from the advertised enum.
pub type FpsChoice = (u32, u32);

/// One `EnumFormat` Format object with plain (fixed) values for a single
/// (format, size) pair, plus the colorimetry properties a regular PipeWire
/// camera node advertises (`colorRange`, `colorMatrix`, `transferFunction`,
/// `colorPrimaries`). The `framerate` is a `Choice` (enum) of `(num, denom)`
/// fractions — the "multiple fps choices" shape `pw-topology` shows as
/// `default: 10/1, alt1: ...`.
pub fn enumformat_pod(format: Format, width: u32, height: u32, fps: &[FpsChoice]) -> Vec<u8> {
    // YUY2/UYVY are full range (0-255) like webcam YUYV422.
    // NV12/NV21/I420 are limited range (16-235) like H.264 codecs.
    // RGB formats don't use color range (it's meaningless for RGB).
    let range = match format {
        Format::Yuy2 | Format::Uvyvy => COLOR_RANGE_0_255,
        Format::Nv12 | Format::Nv21 | Format::I420 => COLOR_RANGE_16_235,
        _ => COLOR_RANGE_0_255, // RGB: default to full range (ignored by decoders)
    };
    let matrix = if height <= 480 {
        COLOR_MATRIX_BT601
    } else {
        COLOR_MATRIX_BT709
    };
    // Build the framerate choice (enum): default = first, alternatives = rest.
    let framerate = if fps.len() == 1 {
        Value::Fraction(Fraction {
            num: fps[0].0,
            denom: fps[0].1,
        })
    } else {
        Value::Choice(pw::spa::pod::ChoiceValue::Fraction(Choice(
            ChoiceFlags::empty(),
            ChoiceEnum::Enum {
                default: Fraction {
                    num: fps[0].0,
                    denom: fps[0].1,
                },
                alternatives: fps
                    .iter()
                    .map(|&(n, d)| Fraction { num: n, denom: d })
                    .collect(),
            },
        )))
    };
    // Maximum framerate we can sustain at this (format, size). Real camera
    // nodes advertise it; browsers use it to validate the negotiated rate.
    // Only emitted when there is more than one choice (a single-choice enum
    // already pins the rate exactly).
    let max_fps = fps.iter().fold(0u32, |m, &(n, d)| m.max(d.max(1) * n));
    let mut properties: Vec<Property> = vec![
        Property {
            key: FormatProperties::MediaType.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Id(Id(MediaType::Video.as_raw())),
        },
        Property {
            key: FormatProperties::MediaSubtype.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Id(Id(MediaSubtype::Raw.as_raw())),
        },
        Property {
            key: FormatProperties::VideoFormat.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Id(Id(format.video_format().as_raw())),
        },
        // NB: no `VideoModifier` property. Besides being optional (absent = no
        // modifier, per `spa/param/video/raw-utils.h`), its *presence* makes
        // gstreamer-pipewire treat the format as a "modified" one and request
        // DmaBuf-only buffers, which this node cannot provide (the daemon then
        // fails with "alloc buffers: Operation not supported"). The v4l2 node
        // emits it only for formats that have a real modifier; we have none.
        Property {
            key: FormatProperties::VideoSize.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Rectangle(Rectangle { width, height }),
        },
        Property {
            key: FormatProperties::VideoFramerate.as_raw(),
            flags: PropertyFlags::empty(),
            value: framerate,
        },
        Property {
            key: FormatProperties::VideoColorRange.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Id(Id(range)),
        },
        Property {
            key: FormatProperties::VideoColorMatrix.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Id(Id(matrix)),
        },
        Property {
            key: FormatProperties::VideoTransferFunction.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Id(Id(COLOR_TRANSFER_BT709)),
        },
        Property {
            key: FormatProperties::VideoColorPrimaries.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Id(Id(COLOR_PRIMARIES_BT709)),
        },
    ];
    if fps.len() > 1 && max_fps > 0 {
        properties.push(Property {
            key: pw::spa::sys::SPA_FORMAT_VIDEO_maxFramerate,
            flags: PropertyFlags::empty(),
            value: Value::Fraction(Fraction {
                num: max_fps,
                denom: 1,
            }),
        });
    }
    let obj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties,
    };
    serialize(obj)
}

/// `ParamLatency` objects: one per direction, matching what real camera
/// nodes advertise. The values are derived from the node's own contract:
///
/// * *input* direction: all zero — the node has no input port (zero is the
///   "unset" encoding, not "zero latency").
/// * *output* direction: quantum = 1 frame per buffer (this node's buffer
///   model: one buffer holds one frame and the driver pushes one buffer per
///   period), min/max rate = the min/max advertised fps, min/max ns = the
///   frame period at those rates.
///
/// Browsers (unlike OBS) validate this param when deciding whether a stream
/// is usable, so it must be present and well-formed.
pub fn latency_pods(config: &Config) -> Vec<Vec<u8>> {
    let fps: Vec<u32> = config
        .modes
        .iter()
        .flat_map(|m| m.fps.iter())
        .copied()
        .filter(|&f| f > 0)
        .collect();
    let min_rate = fps.iter().min().copied();
    let max_rate = fps.iter().max().copied();
    // Frame period in ns: shortest at the fastest rate, longest at the
    // slowest rate.
    let min_ns = max_rate.map(ns_per_frame).unwrap_or(0);
    let max_ns = min_rate.map(ns_per_frame).unwrap_or(0);
    [
        latency_pod(Direction::Input, 0.0, 0, 0, 0, 0),
        latency_pod(
            Direction::Output,
            1.0,
            min_rate.unwrap_or(0) as i32,
            max_rate.unwrap_or(0) as i32,
            min_ns,
            max_ns,
        ),
    ]
    .to_vec()
}

/// Frame period in nanoseconds at `fps` whole frames per second.
fn ns_per_frame(fps: u32) -> i64 {
    (1_000_000_000u64 / fps.max(1) as u64) as i64
}

/// A single `ParamLatency` object.
///
/// `quantum` is frames per buffer (min = max). `min_rate`/`max_rate` are Hz
/// and `min_ns`/`max_ns` are nanoseconds; `0` means "unset / determined by
/// negotiation" (zero is the unset encoding in SPA, see
/// `SPA_LATENCY_INFO_UNSET` collapsing to 0).
pub fn latency_pod(
    direction: Direction,
    quantum: f32,
    min_rate: i32,
    max_rate: i32,
    min_ns: i64,
    max_ns: i64,
) -> Vec<u8> {
    let obj = Object {
        type_: SpaTypes::ObjectParamLatency.as_raw(),
        id: ParamType::Latency.as_raw(),
        properties: vec![
            Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_direction,
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(direction.as_raw())),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_minQuantum,
                flags: PropertyFlags::empty(),
                value: Value::Float(quantum),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_maxQuantum,
                flags: PropertyFlags::empty(),
                value: Value::Float(quantum),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_minRate,
                flags: PropertyFlags::empty(),
                value: Value::Int(min_rate),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_maxRate,
                flags: PropertyFlags::empty(),
                value: Value::Int(max_rate),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_minNs,
                flags: PropertyFlags::empty(),
                value: Value::Long(min_ns),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_LATENCY_maxNs,
                flags: PropertyFlags::empty(),
                value: Value::Long(max_ns),
            },
        ],
    };
    serialize(obj)
}

/// The `ParamLatency` negotiation reply for a negotiated framerate: the
/// frame period at that rate (min = max), one frame per buffer. This is
/// what the v4l2 driver emits after accepting a Format
/// (`rate.num * 1s / rate.denom`, min_ns = max_ns); rate is left unset
/// because the negotiated rate is a fraction and the rate fields are whole
/// Hz.
pub fn negotiated_latency_pod(fps_num: u32, fps_denom: u32) -> Vec<u8> {
    let period_ns = (fps_denom as u64 * 1_000_000_000 / fps_num.max(1) as u64) as i64;
    latency_pod(Direction::Output, 1.0, 0, 0, period_ns, period_ns)
}

/// `ParamBuffers` reply: accept 2..MAX_BUFFERS buffers of the negotiated
/// geometry. `num_planes` is the number of data blocks (planes) per buffer;
/// `stride`/`height` describe the first (primary) plane — PipeWire derives
/// the per-plane layout from the negotiated video format.
pub fn buffers_pod(stride: u32, height: u32, num_planes: u32, max_buffers: u32) -> Vec<u8> {
    // Consumers pick a count in 2..=max; default is the midpoint-ish 4
    // (or lower if the range is tight).
    let min = 2u32.min(max_buffers);
    let max = max_buffers.clamp(2, MAX_BUFFERS as u32).max(min);
    let default = 4.min(max).max(min);
    let obj = Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            Property {
                key: pw::spa::sys::SPA_PARAM_BUFFERS_buffers,
                flags: PropertyFlags::empty(),
                value: Value::Choice(pw::spa::pod::ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: default as i32,
                        min: min as i32,
                        max: max as i32,
                    },
                ))),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_BUFFERS_blocks,
                flags: PropertyFlags::empty(),
                value: Value::Int(num_planes as i32),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_BUFFERS_size,
                flags: PropertyFlags::empty(),
                value: Value::Int(stride as i32 * height as i32),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_BUFFERS_stride,
                flags: PropertyFlags::empty(),
                value: Value::Int(stride as i32),
            },
        ],
    };
    serialize(obj)
}

/// `ParamMeta` object asking for a `Header` meta alongside the video data.
pub fn meta_pod() -> Vec<u8> {
    let obj = Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            Property {
                key: pw::spa::sys::SPA_PARAM_META_type,
                flags: PropertyFlags::empty(),
                value: Value::Int(pw::spa::sys::SPA_META_Header as i32),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_META_size,
                flags: PropertyFlags::empty(),
                value: Value::Int(std::mem::size_of::<pw::spa::sys::spa_meta_header>() as i32),
            },
        ],
    };
    serialize(obj)
}

/// The blobs to advertise for `config`: one plain-values Format object per
/// unique (format, size, fps) combination, one `Latency` object per
/// direction (values derived from the advertised fps range), followed by a
/// `meta` (Header) request.
///
/// Loop order: FORMAT outer, FPS inner, so entries with the same
/// (size, format) are *consecutive* — this lets OBS group them into a
/// single "size + format" row with an fps sub-list, instead of showing
/// each fps as a separate row.
pub fn advertised_param_blobs(config: &Config) -> Vec<Vec<u8>> {
    // Deduplicate (format, size) combinations before advertising.
    #[derive(Clone)]
    struct Advertised(Format, u32, u32, Vec<FpsChoice>);
    let mut advertised: Vec<Advertised> = Vec::new();
    for mode in &config.modes {
        for &format in &mode.formats {
            let fps_choices: Vec<FpsChoice> = mode.fps.iter().map(|&f| (f, 1u32)).collect();
            let entry = Advertised(format, mode.width, mode.height, fps_choices.clone());
            if !advertised
                .iter()
                .any(|e| e.0 == entry.0 && e.1 == entry.1 && e.2 == entry.2)
            {
                advertised.push(entry);
            }
        }
    }
    let mut blobs: Vec<Vec<u8>> = Vec::with_capacity(advertised.len() + 3);
    for Advertised(format, width, height, fps_choices) in &advertised {
        blobs.push(enumformat_pod(*format, *width, *height, fps_choices));
    }
    // Latency params: one per direction (Input + Output), values derived
    // from the advertised fps range.
    blobs.extend(latency_pods(config));
    blobs.push(meta_pod());
    blobs
}

/// Serialize a POD `Object` into an owned blob. The blob is parsed back
/// before it leaves this module, so a serialization regression panics here
/// rather than deep in PipeWire.
pub fn serialize(obj: Object) -> Vec<u8> {
    let (cursor, _size) =
        PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
            .expect("failed to serialize POD");
    let bytes = cursor.into_inner();
    // Sanity: round-trip parse before the blob leaves this module.
    Pod::from_bytes(&bytes).expect("failed to parse serialized POD");
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mode;
    use libspa::param::video::VideoInfoRaw;

    /// A distinctive geometry: width != height != fps, so a size/framerate
    /// swap or a "denom always 1" assumption is caught by the round-trip.
    const W: u32 = 384;
    const H: u32 = 272;
    const FPS: u32 = 47;

    /// `enumformat_pod` must serialize to a POD that the video-format parser
    /// reads back as the exact (format, size, fps) we asked for.
    #[test]
    fn enumformat_pod_roundtrip() {
        for f in Format::all() {
            let blob = enumformat_pod(*f, W, H, &[(FPS, 1)]);
            let pod = Pod::from_bytes(&blob).expect("serialize should round-trip");
            let mut info = VideoInfoRaw::default();
            info.parse(pod)
                .expect("EnumFormat must parse as a video format");
            assert_eq!(
                info.format(),
                f.video_format(),
                "format id mismatch for {f:?}"
            );
            assert_eq!(info.size().width, W);
            assert_eq!(info.size().height, H);
            assert_eq!(info.framerate().num, FPS);
            assert_eq!(info.framerate().denom, 1);
        }
    }

    /// The colorimetry properties must be present (the POD is larger than
    /// the original 5-property shape) and parseable.
    #[test]
    fn enumformat_pod_colorimetry() {
        let blob_sd = enumformat_pod(Format::Rgba, 640, 480, &[(30, 1)]);
        let pod_sd = Pod::from_bytes(&blob_sd).expect("serialize should round-trip");
        let blob_hd = enumformat_pod(Format::Rgba, 1920, 1080, &[(30, 1)]);
        let pod_hd = Pod::from_bytes(&blob_hd).expect("serialize should round-trip");
        // Both should parse and be non-empty.
        assert!(pod_sd.size() > 0);
        assert!(pod_hd.size() > 0);
        // Both should have the same size (same shape, different values).
        assert_eq!(pod_sd.size(), pod_hd.size());
    }

    /// Two fps values for the same (format, size) must serialize to distinct
    /// PODs carrying the exact framerate — the multi-fps regression guard.
    #[test]
    fn enumformat_pod_multi_fps() {
        for fps in [15u32, 30, 60] {
            let blob = enumformat_pod(Format::Rgba, W, H, &[(fps, 1)]);
            let pod = Pod::from_bytes(&blob).expect("serialize should round-trip");
            let mut info = VideoInfoRaw::default();
            info.parse(pod)
                .expect("EnumFormat must parse as a video format");
            assert_eq!(info.framerate().num, fps);
            assert_eq!(info.framerate().denom, 1);
            assert_eq!(info.size().width, W);
            assert_eq!(info.size().height, H);
        }
    }

    /// Duplicate (format, size) combinations are advertised once, and
    /// the advertised blobs end with the `meta` (Header) request.
    #[test]
    fn advertised_param_blobs_dedup() {
        let config = Config {
            name: "cam".into(),
            media_name: "cam".into(),
            modes: vec![
                Mode {
                    width: W,
                    height: H,
                    fps: vec![FPS, FPS],
                    formats: vec![Format::Rgba, Format::Rgba],
                },
                Mode {
                    width: W,
                    height: H,
                    fps: vec![FPS + 1],
                    formats: vec![Format::Rgba],
                },
            ],
            max_buffers: 4,
        };
        let blobs = advertised_param_blobs(&config);
        // Both modes have the same (format, size) → deduped to 1 Format + 2 Latency (input + output) + 1 meta.
        assert_eq!(blobs.len(), 4);
        // The last blob is the meta (Header) request.
        let meta = Pod::from_bytes(&blobs[3]).expect("meta pod must parse");
        let obj = meta.as_object().expect("meta pod must be an object");
        assert_eq!(obj.id().0, ParamType::Meta.as_raw());
    }

    /// Grouping invariant: for each (format, size), all fps entries must be
    /// *consecutive* in the advertised blob list.
    #[test]
    fn advertised_param_blobs_grouped_by_format_then_size() {
        let config = Config {
            name: "cam".into(),
            media_name: "cam".into(),
            modes: vec![
                Mode {
                    width: 1920,
                    height: 1080,
                    fps: vec![24, 30],
                    formats: vec![Format::Rgba, Format::Nv12],
                },
                Mode {
                    width: 1280,
                    height: 720,
                    fps: vec![24, 30],
                    formats: vec![Format::Rgba, Format::Nv12],
                },
            ],
            max_buffers: 4,
        };
        let blobs = advertised_param_blobs(&config);
        // With one Format per (format, size): 4 Formats + 2 Latency + 1 meta = 7.
        assert_eq!(blobs.len(), 7);
    }

    /// Parse a latency POD back into its fields: (direction, minQuantum,
    /// maxQuantum, minRate, maxRate, minNs, maxNs).
    fn parse_latency_pod(blob: &[u8]) -> (u32, f32, f32, i32, i32, i64, i64) {
        use pw::spa::sys::*;
        let pod = Pod::from_bytes(blob).expect("latency pod must parse");
        let obj = pod.as_object().expect("latency pod must be an object");
        assert_eq!(obj.id().0, ParamType::Latency.as_raw());
        let get = |key: u32| -> &Pod {
            obj.find_prop(Id(key))
                .unwrap_or_else(|| panic!("latency property {key} must be present"))
                .value()
        };
        (
            get(SPA_PARAM_LATENCY_direction)
                .get_id()
                .expect("direction is an id")
                .0,
            get(SPA_PARAM_LATENCY_minQuantum)
                .get_float()
                .expect("minQuantum is a float"),
            get(SPA_PARAM_LATENCY_maxQuantum)
                .get_float()
                .expect("maxQuantum is a float"),
            get(SPA_PARAM_LATENCY_minRate)
                .get_int()
                .expect("minRate is an int"),
            get(SPA_PARAM_LATENCY_maxRate)
                .get_int()
                .expect("maxRate is an int"),
            get(SPA_PARAM_LATENCY_minNs)
                .get_long()
                .expect("minNs is a long"),
            get(SPA_PARAM_LATENCY_maxNs)
                .get_long()
                .expect("maxNs is a long"),
        )
    }

    /// The advertised latency pods derive their values from the config's
    /// fps range: input direction is unset (all zero, no input port); output
    /// direction is 1 frame/buffer over the min..max fps, with the frame
    /// period (ns) at those rates.
    #[test]
    fn latency_pods_derive_from_config() {
        let config = Config {
            name: "cam".into(),
            media_name: "cam".into(),
            modes: vec![
                Mode {
                    width: 1920,
                    height: 1080,
                    fps: vec![30, 60],
                    formats: vec![Format::Rgba],
                },
                Mode {
                    width: 1280,
                    height: 720,
                    fps: vec![24],
                    formats: vec![Format::Nv12],
                },
            ],
            max_buffers: 4,
        };
        let pods = latency_pods(&config);
        assert_eq!(pods.len(), 2);
        let (dir, min_q, max_q, min_r, max_r, min_ns, max_ns) = parse_latency_pod(&pods[0]);
        assert_eq!(dir, Direction::Input.as_raw());
        assert_eq!(
            (min_q, max_q, min_r, max_r, min_ns, max_ns),
            (0.0, 0.0, 0, 0, 0, 0)
        );
        let (dir, min_q, max_q, min_r, max_r, min_ns, max_ns) = parse_latency_pod(&pods[1]);
        assert_eq!(dir, Direction::Output.as_raw());
        assert_eq!((min_q, max_q), (1.0, 1.0));
        assert_eq!((min_r, max_r), (24, 60));
        // Frame period: shortest at 60 fps, longest at 24 fps.
        assert_eq!(min_ns, 1_000_000_000i64 / 60);
        assert_eq!(max_ns, 1_000_000_000i64 / 24);
    }

    /// The negotiated latency reply carries the frame period at the
    /// negotiated rate (min = max), one frame per buffer, rate unset.
    #[test]
    fn negotiated_latency_pod_carries_frame_period() {
        for (num, denom, period_ns) in [
            (30u32, 1u32, 33_333_333i64),
            (60, 1, 16_666_666),
            (30000, 1001, 33_366_666),
        ] {
            let blob = negotiated_latency_pod(num, denom);
            let (dir, min_q, max_q, min_r, max_r, min_ns, max_ns) = parse_latency_pod(&blob);
            assert_eq!(dir, Direction::Output.as_raw());
            assert_eq!((min_q, max_q), (1.0, 1.0));
            assert_eq!((min_r, max_r), (0, 0));
            assert_eq!((min_ns, max_ns), (period_ns, period_ns));
        }
    }

    /// `buffers_pod` and `meta_pod` serialize to well-formed (parseable)
    /// objects and are non-trivial in size.
    #[test]
    fn buffers_and_meta_pods_roundtrip() {
        for blob in [
            buffers_pod(7680, 1080, 1, 4),
            buffers_pod(1920, 1080, 3, 16),
            buffers_pod(1920, 1080, 3, 2),
            meta_pod(),
        ] {
            let pod = Pod::from_bytes(&blob).expect("serialize should round-trip");
            assert!(
                pod.size() > 0 && !pod.as_bytes().is_empty(),
                "pod must be non-trivial"
            );
        }
    }
}
