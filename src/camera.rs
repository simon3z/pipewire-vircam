//! `Camera`: the user-facing virtual camera.
//!
//! Owns the PipeWire main loop, context, stream, and (during `run`) the
//! stream listener and driver timer. The user only sees [`Camera`],
//! [`State`], [`Negotiated`], and [`Frame`].

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use libspa::buffer::Data;
use pipewire as pw;
use pipewire::spa::{
    param::ParamType,
    utils::{Direction, SpaTypes},
};

use crate::{error::Error, pod, Config, Format, Negotiated};

/// One plane of a raw video frame.
///
/// `data` has length >= `stride * height` (it is the buffer's max size for
/// this plane). The meaningful pixels occupy the first `stride * height`
/// bytes, laid out as `height` rows of `stride` bytes each.
pub struct Plane<'f> {
    /// Row stride in bytes (bytes per row).
    pub stride: u32,
    /// Number of rows in this plane.
    pub height: u32,
    /// The plane's pixel data, mutable.
    pub data: &'f mut [u8],
}

/// One frame of the camera output, handed to your fill handler.
///
/// Self-describing: the negotiated geometry is on the view itself, so your
/// code can adapt to whatever the consumer picked (format, size, planes).
///
/// `planes` is ordered by plane index (0 = luma/primary). For packed
/// formats there is exactly one plane. For I420 there are three (Y, U, V);
/// for NV12/NV21 two (Y, then interleaved UV/VU).
pub struct Frame<'f> {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    /// Framerate numerator (from the negotiation for this buffer).
    pub fps_num: u32,
    /// Framerate denominator (1 for whole-number rates).
    pub fps_denom: u32,
    /// Planes, in plane order. Fill every plane before returning.
    pub planes: Vec<Plane<'f>>,
}

impl<'f> Frame<'f> {
    /// Fill all planes with a neutral "black" value appropriate for the
    /// format: packed RGB fills zero; planar/interleaved YUV sets luma to 0
    /// and chroma to 128 (neutral). Use this while your pipeline is not
    /// ready instead of `p.data.fill(0)`, which leaves undefined chroma for
    /// YUV formats.
    pub fn fill_black(&mut self) {
        let planes = std::mem::take(&mut self.planes);
        // SAFETY: the planes are disjoint spa_data elements taken from one
        // live buffer; `fill` on each is independent. We move them out and
        // back so the per-plane borrows don't overlap in the type system.
        for (i, p) in planes.into_iter().enumerate() {
            let Plane {
                stride,
                height,
                data,
            } = p;
            let mut plane = Plane {
                stride,
                height,
                data,
            };
            match (self.format, i) {
                (Format::I420, 0) | (Format::Nv12, 0) | (Format::Nv21, 0) => plane.fill(0),
                (Format::I420, 1) | (Format::I420, 2) => plane.fill(128),
                (Format::Nv12, 1) | (Format::Nv21, 1) => plane.fill_interleaved(128, 128),
                _ => plane.fill(0),
            }
        }
    }
}

impl Plane<'_> {
    fn fill(&mut self, value: u8) {
        let n = (self.stride * self.height) as usize;
        self.data[..n].fill(value);
    }

    /// Fill an interleaved two-byte plane (UV/VU) with alternating values.
    fn fill_interleaved(&mut self, a: u8, b: u8) {
        let n = (self.stride * self.height) as usize;
        let mut i = 0;
        while i + 1 < n {
            self.data[i] = a;
            self.data[i + 1] = b;
            i += 2;
        }
    }
}

/// Connection state of the camera, reported to [`Camera::on_state`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// No consumer is linked. `error` carries the message when the
    /// transition was caused by a stream error (e.g. the link died).
    ///
    /// After `Disconnected`, your fill handler is not called again until a
    /// new consumer connects and negotiates; you may safely release any
    /// per-consumer resources here.
    Disconnected { error: Option<String> },
    /// A consumer is connected (node linked, format negotiated) but not
    /// actively pulling frames yet.
    Paused { node_id: u32 },
    /// Frames are flowing; your fill handler is being called at the
    /// negotiated framerate.
    Streaming { node_id: u32 },
}

/// Shared between the stream callbacks and the driver timer.
struct Pacing {
    /// True while we should push frames (streaming && driving && !lazy).
    streaming: AtomicBool,
    /// Frame period in nanoseconds (from the negotiated framerate).
    period_ns: AtomicU64,
    /// Deadline of the next frame (monotonic nanoseconds since `START`).
    next_due: AtomicU64,
}

static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn now_ns() -> u64 {
    // Cheap enough: the timer callback runs at most 1 kHz.
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_nanos() as u64
}

/// Listener user data: what the stream callbacks share.
struct Inner {
    pacing: Arc<Pacing>,
    /// Latest negotiation result (set in `param_changed`, consumed in
    /// `process`; callback access is sequential, no lock needed).
    negotiated: Option<Negotiated>,
    max_buffers: u32,
    state_cb: Box<dyn FnMut(State)>,
    neg_cb: Box<dyn FnMut(&Negotiated)>,
    #[allow(clippy::type_complexity)]
    fill: Box<dyn FnMut(&mut Frame, &Negotiated)>,
}

/// A PipeWire virtual camera: create it, register callbacks, run it.
pub struct Camera {
    mainloop: pw::main_loop::MainLoopRc,
    stream: pw::stream::StreamRc,
    pacing: Arc<Pacing>,
    max_buffers: u32,
    state_cb: Box<dyn FnMut(State)>,
    neg_cb: Box<dyn FnMut(&Negotiated)>,
}

/// A handle to signal the main loop to quit, usable from any callback
/// (state, negotiated, fill). Capture it via [`Camera::quit_handle`] and move
/// it into your closure before [`Camera::run`] (which consumes the camera).
#[derive(Clone)]
pub struct QuitHandle {
    mainloop: pw::main_loop::MainLoopRc,
}

impl QuitHandle {
    /// Signal the main loop to quit, causing [`Camera::run`] to return. The
    /// camera node is torn down when `run` returns. Safe to call from any
    /// callback (state, negotiated, fill).
    pub fn quit(&self) {
        self.mainloop.quit();
    }
}

fn validate(config: &Config) -> Result<(), Error> {
    if config.name.is_empty() {
        return Err(Error::InvalidConfig("name must not be empty".into()));
    }
    if config.modes.is_empty() {
        return Err(Error::InvalidConfig("at least one mode is required".into()));
    }
    for mode in &config.modes {
        if mode.width == 0 || mode.height == 0 {
            return Err(Error::InvalidConfig("mode size must be nonzero".into()));
        }
        if mode.fps.is_empty() {
            return Err(Error::InvalidConfig("mode needs at least one fps".into()));
        }
        if mode.fps.contains(&0) {
            return Err(Error::InvalidConfig(
                "mode fps values must be nonzero".into(),
            ));
        }
        if mode.formats.is_empty() {
            return Err(Error::InvalidConfig(
                "each mode needs at least one format".into(),
            ));
        }
    }
    Ok(())
}

/// Reinterpret owned POD blobs as `&mut [Pod]` for the safe wrappers.
/// SAFETY: each blob is a well-formed serialized POD (built and round-trip
/// parsed in [`pod::serialize`]) that outlives the call, so the reinterpreted
/// `Pod`s are valid for the duration of the FFI call. The blobs are disjoint,
/// so the mutable references do not alias.
/// `pw_stream_update_params` with owned POD blobs (the safe wrapper's
/// `&mut [&Pod]` argument cannot be built from owned PODs in pipewire 0.10).
/// SAFETY: the blobs are well-formed serialized PODs that outlive the call.
fn pw_stream_update_params_ptr(stream: *mut pw::sys::pw_stream, blobs: &[Vec<u8>]) -> i32 {
    let mut ptrs: Vec<*const pw::spa::sys::spa_pod> =
        blobs.iter().map(|b| b.as_ptr() as *const _).collect();
    unsafe { pw::sys::pw_stream_update_params(stream, ptrs.as_mut_ptr().cast(), ptrs.len() as u32) }
}

impl Camera {
    /// Create the camera node (does not block; use [`run`](Self::run) to
    /// start the main loop).
    pub fn new(config: Config) -> Result<Self, Error> {
        validate(&config)?;

        let mainloop =
            pw::main_loop::MainLoopRc::new(None).map_err(|e| Error::Connect(e.to_string()))?;

        let context = pw::context::ContextRc::new(&mainloop, None)
            .map_err(|e| Error::Connect(format!("{e:?}")))?;
        let core = context
            .connect_rc(None)
            .map_err(|e| Error::Connect(format!("{e:?}")))?;

        let properties = pw::properties::properties! {
            *pw::keys::NODE_NAME => config.name.as_str(),
            *pw::keys::NODE_DESCRIPTION => config.media_name.as_str(),
            *pw::keys::NODE_NICK => config.media_name.as_str(),
            *pw::keys::MEDIA_NAME => config.media_name.as_str(),
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Camera",
        };

        let stream = pw::stream::StreamRc::new(core, &config.name, properties)
            .map_err(|e| Error::Connect(e.to_string()))?;

        // EnumFormat params: one plain-value Format object per
        // (format, size, fps) combination (OBS requires plain values),
        // plus a `ParamMeta` (Header) request.
        let param_blobs = pod::advertised_param_blobs(&config);

        // The safe `connect`'s `&mut [&Pod]` argument cannot be built from
        // owned PODs in pipewire 0.10, so call the C function with a
        // borrowed pointer array (the blobs outlive the call).
        let mut pod_ptrs: Vec<*const pw::spa::sys::spa_pod> =
            param_blobs.iter().map(|b| b.as_ptr() as *const _).collect();
        let r = unsafe {
            pw::sys::pw_stream_connect(
                stream.as_raw_ptr(),
                Direction::Output.as_raw(),
                pw::constants::ID_ANY,
                pw::stream::StreamFlags::DRIVER.bits(),
                pod_ptrs.as_mut_ptr(),
                pod_ptrs.len() as u32,
            )
        };
        if r < 0 {
            return Err(Error::Connect(format!("pw_stream_connect: {r}")));
        }

        Ok(Camera {
            max_buffers: config.max_buffers,
            mainloop,
            stream,
            pacing: Arc::new(Pacing {
                streaming: AtomicBool::new(false),
                period_ns: AtomicU64::new(0),
                next_due: AtomicU64::new(0),
            }),
            state_cb: Box::new(|_| {}),
            neg_cb: Box::new(|_| {}),
        })
    }

    /// Register a connect/disconnect callback. Called on the main loop
    /// thread for every state transition. Default: no-op.
    pub fn on_state(mut self, cb: impl FnMut(State) + 'static) -> Self {
        self.state_cb = Box::new(cb);
        self
    }

    /// Signal the main loop to quit, causing [`Camera::run`](Self::run) to
    /// return. Safe to call from any callback (state, negotiated, fill);
    /// the camera node is torn down when `run` returns.
    pub fn quit(&self) {
        self.mainloop.quit();
    }

    /// A [`QuitHandle`] for quitting the camera from a callback (state,
    /// negotiated, fill). Capture it *before* [`Camera::run`] (which consumes
    /// `self`) and move it into your closure.
    pub fn quit_handle(&self) -> QuitHandle {
        QuitHandle {
            mainloop: self.mainloop.clone(),
        }
    }

    /// Register a "negotiated" callback. Called whenever a connected
    /// consumer picks a (format, size, fps) combination. It is guaranteed
    /// to be called at least once before the first `fill` call for a given
    /// consumer session. Default: no-op.
    pub fn on_negotiated(mut self, cb: impl FnMut(&Negotiated) + 'static) -> Self {
        self.neg_cb = Box::new(cb);
        self
    }

    /// Start the camera: install the stream listener and driver timer,
    /// then block on the main loop until it is quit (SIGINT/SIGTERM).
    ///
    /// `fill` is called on the main loop thread, at the negotiated fps,
    /// only while a consumer is connected and streaming. It must fill
    /// `frame.planes` (see [`Frame`]). `negotiated` is the per-frame
    /// snapshot of what the consumer picked for *this* buffer (format,
    /// size, fps, stride) — it cannot change between the two arguments.
    ///
    /// If your fill handler takes longer than one frame period, subsequent
    /// periods are skipped (no catch-up burst): at 30 fps the fill must
    /// complete in ~33 ms or frames are silently dropped. Consider logging
    /// a warning if you exceed the budget.
    ///
    /// All callbacks (`on_state`, `on_negotiated`) and `fill` run on the
    /// PipeWire main-loop thread; keep them fast and make any captured
    /// state `Send`-safe (the listener user data crosses into that thread).
    pub fn run(self, fill: impl FnMut(&mut Frame, &Negotiated) + 'static) -> Result<(), Error> {
        let inner = Inner {
            pacing: self.pacing.clone(),
            negotiated: None,
            max_buffers: self.max_buffers,
            state_cb: self.state_cb,
            neg_cb: self.neg_cb,
            fill: Box::new(fill),
        };

        // The driver timer and the SIGINT/SIGTERM sources must stay alive for
        // the loop's duration: dropping a source destroys it (timer) or
        // unregisters the handler (signal).
        let _timer = driver_timer(&self.mainloop, &self.stream, self.pacing.clone());
        let _quit_signals = quit_signals(&self.mainloop);

        // Keep the main loop alive until after the stream is dropped.
        let _mainloop = self.mainloop.clone();
        let _listener = self
            .stream
            .add_local_listener_with_user_data(inner)
            .state_changed(on_state_changed)
            .param_changed(on_param_changed)
            .process(on_process)
            .register();

        self.mainloop.run();
        Ok(())
    }
}

/// Create the driver timer and arm it for a 1 ms period. The callback
/// produces at most one frame per negotiated period (software pacing, so fps
/// can change with renegotiation without re-arming). The caller must keep
/// the returned source alive for the main loop's duration (drop destroys it).
fn driver_timer<'l>(
    mainloop: &'l pw::main_loop::MainLoopRc,
    stream: &pw::stream::StreamRc,
    pacing: Arc<Pacing>,
) -> pw::loop_::TimerSource<'l> {
    let stream = stream.clone();
    let timer = mainloop.loop_().add_timer(move |_exps| {
        if !pacing.streaming.load(Ordering::SeqCst) {
            return;
        }
        let period = pacing.period_ns.load(Ordering::SeqCst);
        if period == 0 {
            return;
        }
        let now = now_ns();
        let next = pacing.next_due.load(Ordering::SeqCst);
        if now < next {
            return;
        }
        // Advance past any missed periods (no catch-up bursts).
        let missed = (now - next) / period;
        pacing
            .next_due
            .store(next + (missed + 1) * period, Ordering::SeqCst);
        let _ = stream.trigger_process();
    });
    let _ = timer.update_timer(
        Some(Duration::from_millis(1)),
        Some(Duration::from_millis(1)),
    );
    timer
}

/// Register SIGINT/SIGTERM as main-loop quit triggers (like the C reference).
/// The returned sources must stay alive for the main loop's duration (drop
/// unregisters the handler).
fn quit_signals(
    mainloop: &pw::main_loop::MainLoopRc,
) -> (pw::loop_::SignalSource<'_>, pw::loop_::SignalSource<'_>) {
    let loop_ = mainloop.loop_();
    let ml = mainloop.clone();
    let sig_int = loop_.add_signal_local(pw::loop_::Signal::INT, move || ml.quit());
    let ml = mainloop.clone();
    let sig_term = loop_.add_signal_local(pw::loop_::Signal::TERM, move || ml.quit());
    (sig_int, sig_term)
}

/// `state_changed`: update the pacing flag and report [`State`] to the user.
fn on_state_changed(
    stream: &pw::stream::Stream,
    inner: &mut Inner,
    _old: pw::stream::StreamState,
    new: pw::stream::StreamState,
) {
    let node_id = stream.node_id();
    match new {
        // A driver stream has a single link; the states reflect the stream,
        // not individual consumers.
        pw::stream::StreamState::Unconnected | pw::stream::StreamState::Connecting => {
            inner.pacing.streaming.store(false, Ordering::SeqCst);
            (inner.state_cb)(State::Disconnected { error: None });
        }
        pw::stream::StreamState::Paused => {
            inner.pacing.streaming.store(false, Ordering::SeqCst);
            (inner.state_cb)(State::Paused { node_id });
        }
        pw::stream::StreamState::Streaming => {
            let driving = stream.is_driving();
            let lazy = unsafe { pw::sys::pw_stream_is_lazy(stream.as_raw_ptr()) };
            inner.pacing.next_due.store(now_ns(), Ordering::SeqCst);
            inner
                .pacing
                .streaming
                .store(driving && !lazy, Ordering::SeqCst);
            (inner.state_cb)(State::Streaming { node_id });
        }
        pw::stream::StreamState::Error(msg) => {
            inner.pacing.streaming.store(false, Ordering::SeqCst);
            println!("stream state: \"error\" {msg}");
            (inner.state_cb)(State::Disconnected { error: Some(msg) });
        }
    }
}

/// `param_changed`: parse the negotiated Format, snapshot it, update the
/// driver period, and reply with `ParamBuffers` + a `meta` (Header) request.
fn on_param_changed(
    stream: &pw::stream::Stream,
    inner: &mut Inner,
    id: u32,
    param: Option<&pw::spa::pod::Pod>,
) {
    if id != ParamType::Format.as_raw() {
        return;
    }
    let Some(param) = param else { return };
    if param.type_().as_raw() != SpaTypes::Object.as_raw() {
        return;
    }
    let mut info = libspa::param::video::VideoInfoRaw::default();
    if info.parse(param).is_err() {
        eprintln!("pipewire-vircam: failed to parse negotiated Format");
        return;
    }
    let Some(format) = Format::from_spa_id(info.format().0) else {
        eprintln!(
            "pipewire-vircam: negotiated format {} is not supported",
            info.format().0
        );
        return;
    };
    let (stride, _h) = format.planes(info.size().width, info.size().height)[0];
    let neg = Negotiated {
        format,
        width: info.size().width,
        height: info.size().height,
        fps_num: info.framerate().num,
        fps_denom: info.framerate().denom,
        stride,
        node_id: stream.node_id(),
    };
    inner.pacing.period_ns.store(
        neg.fps_denom as u64 * 1_000_000_000 / neg.fps_num.max(1) as u64,
        Ordering::SeqCst,
    );
    inner.negotiated = Some(neg);
    (inner.neg_cb)(&neg);

    // Reply with ParamBuffers (and re-request the Header meta).
    let num_planes = neg.format.planes(neg.width, neg.height).len() as u32;
    let blobs: Vec<Vec<u8>> = vec![
        pod::meta_pod(),
        pod::buffers_pod(neg.stride, neg.height, num_planes, inner.max_buffers),
    ];
    let _ = pw_stream_update_params_ptr(stream.as_raw_ptr(), &blobs);
}

/// `process`: snapshot the negotiation for this buffer, hand a [`Frame`] to
/// the fill handler, and return the buffer (dropping it re-queues it).
fn on_process(stream: &pw::stream::Stream, inner: &mut Inner) {
    let Some(mut buf) = stream.dequeue_buffer() else {
        return;
    };
    // Snapshot the negotiation for this buffer. `negotiated`
    // may be replaced by a later `param_changed` (renegotiation),
    // so every field used below comes from this local copy.
    let Some(neg) = inner.negotiated else {
        return; // not negotiated yet; buffer queued as-is
    };
    let layout = neg.format.planes(neg.width, neg.height);
    let Some(planes) = build_planes(buf.datas_mut(), &layout) else {
        return;
    };
    let mut frame = Frame {
        width: neg.width,
        height: neg.height,
        format: neg.format,
        fps_num: neg.fps_num,
        fps_denom: neg.fps_denom,
        planes,
    };
    (inner.fill)(&mut frame, &neg);
    // Dropping `buf` queues the buffer back to the stream.
}

/// Build the [`Frame`] plane views from the buffer's `datas` and the
/// negotiated per-plane layout, and set each plane's chunk (offset 0, full
/// size). Returns `None` if a plane has no data (the buffer is not usable).
///
/// SAFETY: `datas` is `&mut [Data]` for the whole buffer; each `datas[i]` is
/// a distinct `spa_data` whose `data` pointer points into its own (disjoint)
/// buffer allocated by PipeWire for this plane. The buffer is alive for the
/// duration of the call and is not shared, so holding `&mut` slices into
/// several planes at once is sound. We use raw pointers to get around the
/// borrow checker's (correct, but overly conservative) rule that two
/// `datas[i].method()` calls in one expression both borrow the whole slice.
fn build_planes<'b>(datas: &'b mut [Data], layout: &[(u32, u32)]) -> Option<Vec<Plane<'b>>> {
    let n = datas.len().min(layout.len());
    let datas_ptr: *mut Data = datas.as_mut_ptr();
    let mut planes: Vec<Plane> = Vec::with_capacity(n);
    for (i, &(stride, height)) in layout.iter().take(n).enumerate() {
        // SAFETY: `datas_ptr.add(i)` is in-bounds (i < n <= len) and each
        // element is a disjoint `spa_data`.
        let d: &mut Data = unsafe { &mut *datas_ptr.add(i) };
        // Set the chunk for this plane (offset 0, full size). The
        // chunk borrow must end before we take the data borrow (both are
        // `&mut Data`), hence the inner scope.
        {
            let c = d.chunk_mut();
            *c.offset_mut() = 0;
            *c.size_mut() = stride * height;
            *c.stride_mut() = stride as i32;
        }
        // SAFETY: `d.data()`'s `&mut` is into this plane's buffer
        // (maxsize >= stride*height); the buffer outlives the caller.
        let data = d.data()?;
        planes.push(Plane {
            stride,
            height,
            data,
        });
    }
    Some(planes)
}
