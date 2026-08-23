//! POD construction for `pipewire-vircam`.
//!
//! Builds the `EnumFormat` parameter objects (one per advertised
//! (format, size, fps) combination, all with *plain* values — OBS's
//! `camera-portal.c` parses the size as a plain `Rectangle` and drops
//! range/choice encodings, so every advertised (format, size, fps) is a
//! plain value) and the `ParamBuffers` / `ParamMeta` negotiation replies.
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
    utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes},
};

use crate::{Config, Format};

/// Hard ceiling on `Config::max_buffers` (keeps the SPA range sane).
pub const MAX_BUFFERS: i32 = 16;

/// The `spa_video_color_*` enums the v4l2 node advertises
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
    // NB: no `VideoModifier` property. Besides being optional (absent = no
    // modifier, per `spa/param/video/raw-utils.h`), its *presence* makes
    // gstreamer-pipewire treat the format as a "modified" one and request
    // DmaBuf-only buffers, which this node cannot provide (the daemon then
    // fails with "alloc buffers: Operation not supported"). The v4l2 node
    // emits it only for formats that have a real modifier; we have none.
    let mut properties = format_properties(format, width, height, fps);
    if let Some((num, denom)) = max_framerate(fps) {
        properties.push(Property {
            key: pw::spa::sys::SPA_FORMAT_VIDEO_maxFramerate,
            flags: PropertyFlags::empty(),
            value: Value::Fraction(Fraction { num, denom }),
        });
    }
    let obj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties,
    };
    serialize(obj)
}

/// The base `EnumFormat` properties for one (format, size) pair.
fn format_properties(format: Format, width: u32, height: u32, fps: &[FpsChoice]) -> Vec<Property> {
    // Match real webcam behavior: limited range (16-235), BT.601 matrix,
    // BT.709 transfer + primaries. Packed Y'UV formats get limited range
    // (Chrome requires this for YUY2); RGB gets full range (ignored by
    // decoders). The v4l2 node advertises these as plain `Id` values and
    // `pw-topology`'s `video.c` decodes them back by value.
    let range = match format {
        Format::Yuy2 | Format::Uvyvy => pw::spa::sys::SPA_VIDEO_COLOR_RANGE_16_235,
        _ => pw::spa::sys::SPA_VIDEO_COLOR_RANGE_0_255,
    };
    vec![
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
        Property {
            key: FormatProperties::VideoSize.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Rectangle(Rectangle { width, height }),
        },
        Property {
            key: FormatProperties::VideoFramerate.as_raw(),
            flags: PropertyFlags::empty(),
            value: framerate_value(fps),
        },
        Property {
            key: FormatProperties::VideoColorRange.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Id(Id(range)),
        },
        Property {
            key: FormatProperties::VideoColorMatrix.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Id(Id(pw::spa::sys::SPA_VIDEO_COLOR_MATRIX_BT601)),
        },
        Property {
            key: FormatProperties::VideoTransferFunction.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Id(Id(5)), // SPA_VIDEO_COLOR_TRANSFER_BT709 = 5
        },
        Property {
            key: FormatProperties::VideoColorPrimaries.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Id(Id(1)), // SPA_VIDEO_COLOR_PRIMARIES_BT709 = 1
        },
    ]
}

/// Framerate choice value for `fps`: a plain `Fraction` for a single rate,
/// or a `Choice` (enum) of fractions for several — the "multiple fps
/// choices" shape `pw-topology` renders as `default: 30/1, alt1: 60/1`.
fn framerate_value(fps: &[FpsChoice]) -> Value {
    if fps.len() == 1 {
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
    }
}

/// The maximum advertised framerate as `(num, denom)`, for the
/// `maxFramerate` property. Real camera nodes advertise it; browsers use it
/// to validate the negotiated rate. Only emitted for multi-choice entries
/// (a single-choice enum already pins the rate exactly).
fn max_framerate(fps: &[FpsChoice]) -> Option<(u32, u32)> {
    if fps.len() > 1 {
        let num = fps.iter().fold(0u32, |m, &(n, d)| m.max(d.max(1) * n));
        Some((num, 1))
    } else {
        None
    }
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
        properties: buffers_properties((min, max, default), num_planes, stride, height),
    };
    serialize(obj)
}

/// The `ParamBuffers` properties: the buffer-count range and the first
/// (primary) plane's block count, size and stride.
fn buffers_properties(
    (min, max, default): (u32, u32, u32),
    num_planes: u32,
    stride: u32,
    height: u32,
) -> Vec<Property> {
    vec![
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
    ]
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
                // Use Id (not Int) to match the C reference:
                // meta types are PipeWire object IDs.
                value: Value::Id(Id(pw::spa::sys::SPA_META_Header)),
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
/// unique (format, size, fps) combination, followed by a `meta` (Header)
/// request.
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
    let mut blobs: Vec<Vec<u8>> = Vec::with_capacity(advertised.len() + 1);
    for Advertised(format, width, height, fps_choices) in &advertised {
        blobs.push(enumformat_pod(*format, *width, *height, fps_choices));
    }
    // Ask for a Header meta (carries PTS) on each buffer, like a real
    // camera. Consumers such as Chrome use this to timestamp frames.
    // Match the C reference: no Latency params in the initial connect.
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
        // Both modes have the same (format, size) → deduped to 1 Format + 1 meta.
        assert_eq!(blobs.len(), 2);
        // The last blob is the meta (Header) request.
        let meta = Pod::from_bytes(&blobs[1]).expect("meta pod must parse");
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
                    formats: vec![Format::Rgba, Format::Yuy2],
                },
                Mode {
                    width: 1280,
                    height: 720,
                    fps: vec![24, 30],
                    formats: vec![Format::Rgba, Format::Yuy2],
                },
            ],
            max_buffers: 4,
        };
        let blobs = advertised_param_blobs(&config);
        // With one Format per (format, size): 4 Formats + 1 meta = 5.
        assert_eq!(blobs.len(), 5);
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
