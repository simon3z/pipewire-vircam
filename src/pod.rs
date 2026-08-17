//! POD construction for `pipewire-vircam`.
//!
//! Builds the EnumFormat parameter objects (one per advertised
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
    utils::{Choice, ChoiceEnum, Fraction, Rectangle, SpaTypes},
};

use crate::{Config, Format};

/// Hard ceiling on `Config::max_buffers` (keeps the SPA range sane).
pub const MAX_BUFFERS: i32 = 16;

/// One `EnumFormat` Format object with plain (fixed) values for a single
/// (format, size, fps) combination.
pub fn enumformat_pod(format: Format, width: u32, height: u32, fps: u32) -> Vec<u8> {
    let obj = pw::spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(FormatProperties::VideoFormat, Id, format.video_format()),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle { width, height }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction { num: fps, denom: 1 }
        ),
    );
    serialize(obj)
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
                    pw::spa::utils::ChoiceFlags::empty(),
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

/// The `EnumFormat` blobs to advertise for `config`: one plain-values Format
/// object per unique (format, size, fps) combination, followed by a `meta`
/// (Header) request.
pub fn advertised_param_blobs(config: &Config) -> Vec<Vec<u8>> {
    // Deduplicate (format, size, fps) combinations before advertising.
    let mut advertised: Vec<(Format, u32, u32, u32)> = Vec::new();
    for mode in &config.modes {
        for &fps in &mode.fps {
            for &format in &mode.formats {
                let entry = (format, mode.width, mode.height, fps);
                if !advertised.contains(&entry) {
                    advertised.push(entry);
                }
            }
        }
    }
    let mut blobs: Vec<Vec<u8>> = Vec::with_capacity(advertised.len() + 1);
    for &(format, width, height, fps) in &advertised {
        blobs.push(enumformat_pod(format, width, height, fps));
    }
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
    /// reads back as the exact (format, size, fps) we asked for — the OBS
    /// "plain (fixed) values" regression guard.
    #[test]
    fn enumformat_pod_roundtrip() {
        for f in Format::all() {
            let blob = enumformat_pod(*f, W, H, FPS);
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

    /// Two fps values for the same (format, size) must serialize to distinct
    /// PODs carrying the exact framerate — the multi-fps regression guard.
    #[test]
    fn enumformat_pod_multi_fps() {
        for fps in [15u32, 30, 60] {
            let blob = enumformat_pod(Format::Rgba, W, H, fps);
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

    /// Duplicate (format, size, fps) combinations are advertised once, and
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
        // (Rgba, WxH, FPS) twice -> 1, (Rgba, WxH, FPS + 1) -> 1, plus meta.
        assert_eq!(blobs.len(), 3);
        let meta = Pod::from_bytes(&blobs[2]).expect("meta pod must parse");
        let obj = meta.as_object().expect("meta pod must be an object");
        assert_eq!(obj.id().0, ParamType::Meta.as_raw());
    }

    /// `buffers_pod` and `meta_pod` serialize to well-formed (parseable)
    /// objects and are non-trivial in size.
    #[test]
    fn buffers_and_meta_pods_roundtrip() {
        for blob in [
            buffers_pod(7680, 1080, 1, 4),
            buffers_pod(1920, 1080, 3, 16), // e.g. I420
            buffers_pod(1920, 1080, 3, 2),  // tight range: min == max == 2
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
