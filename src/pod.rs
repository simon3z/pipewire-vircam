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
        video::VideoFormat,
        ParamType,
    },
    pod::{serialize::PodSerializer, Object, Pod, Property, PropertyFlags, Value},
    utils::{Choice, ChoiceEnum, Fraction, Rectangle, SpaTypes},
};

use crate::{Format, Mode};

/// Hard ceiling on `Config::max_buffers` (keeps the SPA range sane).
pub const MAX_BUFFERS: i32 = 16;

fn video_format(f: Format) -> VideoFormat {
    f.video_format()
        .expect("Format always maps to a VideoFormat")
}

/// One `EnumFormat` Format object with plain (fixed) values.
pub fn enumformat_pod(format: Format, mode: &Mode) -> Vec<u8> {
    let obj = pw::spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(FormatProperties::VideoFormat, Id, video_format(format)),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle {
                width: mode.width,
                height: mode.height
            }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction {
                num: mode.fps,
                denom: 1
            }
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
    use crate::{Format, Mode};
    use libspa::param::video::VideoInfoRaw;

    /// A distinctive mode: width != height != fps, so a size/framerate swap
    /// or a "denom always 1" assumption is caught by the round-trip.
    fn mode() -> Mode {
        Mode {
            width: 384,
            height: 272,
            fps: 47,
            formats: Vec::new(),
        }
    }

    /// `enumformat_pod` must serialize to a POD that the video-format parser
    /// reads back as the exact (format, size, fps) we asked for — the OBS
    /// "plain (fixed) values" regression guard.
    #[test]
    fn enumformat_pod_roundtrip() {
        for f in Format::all() {
            let m = mode();
            let blob = enumformat_pod(*f, &m);
            let pod = Pod::from_bytes(&blob).expect("serialize should round-trip");
            let mut info = VideoInfoRaw::default();
            info.parse(pod)
                .expect("EnumFormat must parse as a video format");
            assert_eq!(
                info.format(),
                video_format(*f),
                "format id mismatch for {f:?}"
            );
            assert_eq!(info.size().width, m.width);
            assert_eq!(info.size().height, m.height);
            assert_eq!(info.framerate().num, m.fps);
            assert_eq!(info.framerate().denom, 1);
        }
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
