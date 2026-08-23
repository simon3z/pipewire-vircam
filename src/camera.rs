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
/// `planes` is ordered by plane index (0 = luma/primary). All supported
/// formats are packed, so there is always exactly one plane.
pub struct Frame<'f> {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    /// Framerate numerator (from the negotiation for this buffer).
    pub fps_num: u32,
    /// Framerate denominator (1 for whole-number rates).
    pub fps_denom: u32,
    /// Frame sequence number: increments by one per produced frame
    /// (per consumer session, starting at 0).
    pub seq: u64,
    /// Presentation timestamp in nanoseconds, monotonic — nanoseconds since
    /// this camera started ([`Camera::run`](Camera::run)). The same value is
    /// written to the buffer's `Header` meta (`pts`) when that meta is
    /// negotiated, so GStreamer-based consumers (`pipewiresrc`) see it as
    /// the frame PTS. Use it e.g. to capture your screen/backend at the
    /// right moment; consumers that need deltas only ever use differences.
    pub pts: u64,
    /// Planes, in plane order. Fill every plane before returning.
    pub planes: Vec<Plane<'f>>,
}

impl<'f> Frame<'f> {
    /// Fill the (single) plane with a neutral "black" value: packed formats
    /// fill zero. Use this while your pipeline is not ready instead of
    /// `p.data.fill(0)`, which leaves undefined chroma for YUV formats.
    pub fn fill_black(&mut self) {
        // Fill every plane with a neutral "black" value.
        for plane in &mut self.planes {
            plane.fill(0);
        }
    }
}

impl Plane<'_> {
    fn fill(&mut self, value: u8) {
        let n = (self.stride * self.height) as usize;
        self.data[..n].fill(value);
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

/// "Accept" callback: called *before* we reply with `ParamBuffers`.
/// The user is expected to set up their backend here (synchronously), so the
/// first frame is ready before the consumer starts pulling.
/// Returning `Err` rejects the geometry (no reply sent).
type NegotiateAcceptCb = Box<dyn FnMut(&Negotiated) -> Result<(), String>>;
/// "Negotiated" callback: called *after* we have accepted the geometry
/// (i.e. after the `ParamBuffers` reply is queued). Purely informational.
type NegotiatedCb = Box<dyn FnMut(&Negotiated)>;
/// Frame-fill callback: called per frame to fill the buffer.
type FillCb = Box<dyn FnMut(&mut Frame, &Negotiated)>;

/// Listener user data: what the stream callbacks share.
struct Inner {
    pacing: Arc<Pacing>,
    /// Latest negotiation result (set in `param_changed`, consumed in
    /// `process`; callback access is sequential, no lock needed).
    negotiated: Option<Negotiated>,
    /// Frame sequence counter (incremented per produced frame). All
    /// callbacks run on the main-loop thread, so a plain field is fine.
    seq: u64,
    max_buffers: u32,
    state_cb: Box<dyn FnMut(State)>,
    neg_cb_accept: NegotiateAcceptCb,
    neg_cb: NegotiatedCb,
    fill: FillCb,
}

/// A PipeWire virtual camera: create it, register callbacks, run it.
pub struct Camera {
    mainloop: pw::main_loop::MainLoopRc,
    stream: pw::stream::StreamRc,
    pacing: Arc<Pacing>,
    max_buffers: u32,
    state_cb: Box<dyn FnMut(State)>,
    neg_cb_accept: NegotiateAcceptCb,
    neg_cb: NegotiatedCb,
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

/// Connect the stream in output direction and advertise `param_blobs`.
///
/// SAFETY: each blob is a well-formed serialized POD (built and round-trip
/// parsed in [`pod::serialize`]) and outlives the call, so the borrowed
/// pointer array is valid for the duration of the FFI call.
fn connect_output_stream(
    stream: &pw::stream::StreamRc,
    param_blobs: Vec<Vec<u8>>,
) -> Result<(), Error> {
    // The safe `connect`'s `&mut [&Pod]` argument cannot be built from owned
    // PODs in pipewire 0.10, so call the C function with a borrowed pointer
    // array (the blobs outlive the call).
    let mut pod_ptrs: Vec<*const pw::spa::sys::spa_pod> =
        param_blobs.iter().map(|b| b.as_ptr() as *const _).collect();
    let r = unsafe {
        pw::sys::pw_stream_connect(
            stream.as_raw_ptr(),
            Direction::Output.as_raw(),
            pw::constants::ID_ANY,
            // DRIVER: we own the clock. MAP_BUFFERS: Chrome requires this to
            // map buffers into its address space (matches the C reference).
            (pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS).bits(),
            pod_ptrs.as_mut_ptr(),
            pod_ptrs.len() as u32,
        )
    };
    if r < 0 {
        return Err(Error::Connect(format!("pw_stream_connect: {r}")));
    }
    Ok(())
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

/// Call `pw_stream_update_params` with owned POD blobs. The safe wrapper's
/// `&mut [&Pod]` argument cannot be built from owned PODs in pipewire 0.10, so
/// we pass a pointer array to the C function instead.
///
/// SAFETY: each blob is a well-formed serialized POD (built and round-trip
/// parsed in [`pod::serialize`]) and outlives the call, so the borrowed
/// pointer array is valid for the duration of the FFI call.
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
        connect_output_stream(&stream, pod::advertised_param_blobs(&config))?;

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
            neg_cb_accept: Box::new(|_| Ok(())),
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
    ///
    /// If you need to *reject* a geometry (e.g. you cannot set up your
    /// backend at that size), use [`on_negotiate_accept`](Self::on_negotiate_accept)
    /// and return `Err` from there — the camera will then not reply with
    /// `ParamBuffers`, and the consumer will pick a different (format, size,
    /// fps) or give up.
    pub fn on_negotiated(mut self, cb: impl FnMut(&Negotiated) + 'static) -> Self {
        self.neg_cb = Box::new(cb);
        self
    }

    /// Register an *accept* callback: called *before* the camera replies with
    /// `ParamBuffers` to a consumer's Format request. The user is expected to
    /// set up their backend here (synchronously) so that the first frame is
    /// ready by the time the consumer starts pulling. Returning `Err` rejects
    /// the geometry (no reply is sent), and the consumer will pick a different
    /// (format, size, fps) or give up. Default: always accept.
    pub fn on_negotiate_accept(
        mut self,
        cb: impl FnMut(&Negotiated) -> Result<(), String> + 'static,
    ) -> Self {
        self.neg_cb_accept = Box::new(cb);
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
            seq: 0,
            max_buffers: self.max_buffers,
            state_cb: self.state_cb,
            neg_cb_accept: self.neg_cb_accept,
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
            (inner.state_cb)(State::Disconnected { error: Some(msg) });
        }
    }
}

/// `param_changed`: parse the negotiated Format, snapshot it, update the
/// driver period, and reply with `ParamBuffers` + a `meta` (Header) request.
///
/// **The reply is the acknowledgement.** PipeWire will not enter `Streaming`
/// until the source has replied with params that are compatible with what
/// the consumer asked for. If the user's `on_negotiate_accept` callback
/// returns an `Err`, we reply with *nothing* (no `ParamBuffers`), which tells the
/// consumer "I cannot honour this request" and lets it pick a different
/// (format, size, fps) or give up. This is what lets the user set up their
/// backend *before* the camera commits to a geometry, so the first frame is
/// ready by the time the consumer starts pulling.
fn on_param_changed(
    stream: &pw::stream::Stream,
    inner: &mut Inner,
    id: u32,
    param: Option<&pw::spa::pod::Pod>,
) {
    if id != ParamType::Format.as_raw() {
        return;
    }
    let Some(param) = param else {
        return;
    };
    let Some(neg) = parse_negotiated(param, stream.node_id()) else {
        return;
    };
    inner.pacing.period_ns.store(
        neg.fps_denom as u64 * 1_000_000_000 / neg.fps_num.max(1) as u64,
        Ordering::SeqCst,
    );
    inner.negotiated = Some(neg);

    // Ask the user whether we can honour this request. The user is
    // responsible for setting up their backend here (synchronously) so
    // that the first frame is ready by the time the consumer starts
    // pulling. If the user returns `Err`, we do NOT reply with
    // `ParamBuffers`, so the consumer knows we rejected the geometry and
    // will either pick a different one or give up.
    if (inner.neg_cb_accept)(&neg).is_err() {
        return;
    }
    (inner.neg_cb)(&neg);
    reply_buffers(stream, neg, inner.max_buffers);
}

/// Reply with `ParamBuffers` (and re-request the Header meta) — the
/// acknowledgement that we accepted the consumer's (format, size, fps).
///
/// `blocks` is the number of data blocks (planes) per buffer; `size` is the
/// size of the *first* block (`stride × height`), and the stride of the first
/// block. PipeWire derives the per-plane layout from the negotiated video
/// format, so we only need to describe the primary block here (matching
/// upstream `video-src.c` and the `redcam-test` oracle).
///
/// Parse the consumer's Format param into a [`Negotiated`], or `None` if the
/// param is not a parseable video format we support.
fn parse_negotiated(param: &pw::spa::pod::Pod, node_id: u32) -> Option<Negotiated> {
    if param.type_().as_raw() != SpaTypes::Object.as_raw() {
        return None;
    }
    let mut info = libspa::param::video::VideoInfoRaw::default();
    if info.parse(param).is_err() {
        return None;
    }
    let format = Format::from_spa_id(info.format().0)?;
    let (stride, _h) = format.planes(info.size().width, info.size().height)[0];
    Some(Negotiated {
        format,
        width: info.size().width,
        height: info.size().height,
        fps_num: info.framerate().num,
        fps_denom: info.framerate().denom,
        stride,
        node_id,
    })
}

fn reply_buffers(stream: &pw::stream::Stream, neg: Negotiated, max_buffers: u32) {
    let num_planes = neg.format.planes(neg.width, neg.height).len() as u32;
    // Match the C reference: reply with ParamBuffers first, then ParamMeta.
    let blobs: Vec<Vec<u8>> = vec![
        pod::buffers_pod(neg.stride, neg.height, num_planes, max_buffers),
        pod::meta_pod(),
    ];
    let _ = pw_stream_update_params_ptr(stream.as_raw_ptr(), &blobs);
}

/// `process`: dequeue a buffer, stamp its timing, hand a [`Frame`] to the
/// fill handler, and queue the buffer back on every exit path.
///
/// The safe [`pw::stream::Stream::dequeue_buffer`] API does not expose the
/// buffer's meta regions (and [`pw::buffer::Buffer`] has no writable-meta
/// API), so we go through the raw stream FFI: `pw_stream_dequeue_buffer`
/// / `pw_stream_queue_buffer` are thin wrappers, the stream owns the
/// buffer, and this callback runs on the stream's main-loop thread.
fn on_process(stream: &pw::stream::Stream, inner: &mut Inner) {
    // SAFETY: `stream` outlives the listener and this callback runs on the
    // stream's main-loop thread. `pw_stream_dequeue_buffer` returns a
    // stream-owned `pw_buffer`, or NULL when no buffer is available.
    let buf: *mut pw::sys::pw_buffer =
        unsafe { pw::sys::pw_stream_dequeue_buffer(stream.as_raw_ptr()) };
    if buf.is_null() {
        return;
    }
    // SAFETY: `buf` is non-null and owned by the stream; everything between
    // dequeue and queue below runs on the main-loop thread and holds no
    // reference into the buffer when `queue` is called (see `process_buffer`
    // — all locals borrow only within that function). Queueing recycles the
    // buffer; it is not dropped here.
    unsafe {
        process_buffer(stream, inner, buf);
        pw::sys::pw_stream_queue_buffer(stream.as_raw_ptr(), buf);
    }
}

/// Stamp the buffer's timing (seq/pts), build the [`Frame`] views and call
/// the user fill handler. The buffer is queued back by the caller.
fn process_buffer(stream: &pw::stream::Stream, inner: &mut Inner, buf: *mut pw::sys::pw_buffer) {
    // Snapshot the negotiation for this buffer. `negotiated` may be replaced
    // by a later `param_changed` (renegotiation), so every field used below
    // comes from this local copy.
    let Some(neg) = inner.negotiated else { return };
    // SAFETY: `buf` is non-null (checked by the caller) and stream-owned.
    let sbuf: *mut pw::spa::sys::spa_buffer = unsafe { (*buf).buffer };

    let layout = neg.format.planes(neg.width, neg.height);
    // `empty` is a local fallback for buffers with no `datas` (e.g. a plain
    // audio buffer). A zero-length `&mut` can be returned into the caller,
    // so `build_planes` stays total and `on_process`'s queue call is
    // reachable.
    let mut empty: [Data; 0] = [];
    let datas = datas_of_buffer(sbuf, &mut empty);
    let Some(planes) = build_planes(datas, &layout) else {
        return;
    };

    // Stamp the Header meta *before* the fill callback, so `pts` is the
    // frame's presentation time and the user can render for it. `seq` is the
    // per-camera frame counter, `pts` from the PipeWire clock (matches
    // the C reference).
    let seq = inner.seq;
    inner.seq += 1;
    // SAFETY: `stream` is alive and we're on the main-loop thread.
    let pts = unsafe { pw::sys::pw_stream_get_nsec(stream.as_raw_ptr()) } as u64;
    write_header_meta(sbuf, seq, pts);

    let mut frame = Frame {
        width: neg.width,
        height: neg.height,
        format: neg.format,
        fps_num: neg.fps_num,
        fps_denom: neg.fps_denom,
        seq,
        pts,
        planes,
    };
    (inner.fill)(&mut frame, &neg);
}

/// The buffer's `datas` slice, exactly like the safe `Buffer::datas_mut`
/// rebuilds it (same null/length guards, same cast — `Data` is
/// `#[repr(transparent)]` over `spa_data`). `empty` is the fallback for a
/// buffer with no `datas` regions.
fn datas_of_buffer(sbuf: *mut pw::spa::sys::spa_buffer, empty: &mut [Data; 0]) -> &mut [Data] {
    unsafe {
        if !sbuf.is_null() && (*sbuf).n_datas > 0 && !(*sbuf).datas.is_null() {
            std::slice::from_raw_parts_mut((*sbuf).datas as *mut Data, (*sbuf).n_datas as usize)
        } else {
            empty
        }
    }
}

/// Write the `SPA_META_Header` meta (`flags`, `offset`, `pts`, `dts_offset`,
/// `seq`) if the buffer carries a *full* one (32 bytes). A smaller meta
/// region is left untouched — writing only what the negotiated size
/// guarantees (no partial writes).
///
/// SAFETY: `sbuf` must be non-null and point at a buffer we hold (dequeued,
/// not shared). `spa_buffer_find_meta` is a read-only lookup over
/// `sbuf->metas`; the only write is to the meta's own payload, guarded by
/// `size >= sizeof(spa_meta_header)`.
fn write_header_meta(sbuf: *mut pw::spa::sys::spa_buffer, seq: u64, pts: u64) {
    // SAFETY: see above.
    let m: *mut pw::spa::sys::spa_meta =
        unsafe { pw::spa::sys::spa_buffer_find_meta(sbuf, pw::spa::sys::SPA_META_Header) };
    if m.is_null() {
        return;
    }
    // SAFETY: `m` is a live `spa_meta` element inside the buffer we hold.
    let m = unsafe { &mut *m };
    if m.size < std::mem::size_of::<pw::spa::sys::spa_meta_header>() as u32 {
        return;
    }
    // SAFETY: the size check guarantees a full `spa_meta_header` at `m.data`.
    let h = unsafe { &mut *(m.data as *mut pw::spa::sys::spa_meta_header) };
    h.flags = 0;
    h.offset = 0;
    h.pts = pts as i64;
    h.dts_offset = 0;
    h.seq = seq;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a stack `spa_buffer` that carries one `Header` meta whose
    /// payload is `payload` (of `size` bytes), and return the buffer pointer.
    fn buffer_with_meta(
        meta: &mut pw::spa::sys::spa_meta,
        payload: &mut pw::spa::sys::spa_buffer,
    ) -> *mut pw::spa::sys::spa_buffer {
        payload.n_metas = 1;
        payload.metas = meta;
        payload.n_datas = 0;
        payload.datas = std::ptr::null_mut();
        payload as *mut pw::spa::sys::spa_buffer
    }

    #[test]
    fn writes_full_header_meta() {
        let mut payload: pw::spa::sys::spa_meta_header = unsafe { std::mem::zeroed() };
        payload.seq = 0xffff_ffff_ffff_ffff; // sentinel
        let mut meta = pw::spa::sys::spa_meta {
            type_: pw::spa::sys::SPA_META_Header,
            size: std::mem::size_of::<pw::spa::sys::spa_meta_header>() as u32,
            data: &mut payload as *mut _ as *mut _,
        };
        let mut buffer: pw::spa::sys::spa_buffer = unsafe { std::mem::zeroed() };
        let buf = buffer_with_meta(&mut meta, &mut buffer);

        write_header_meta(buf, 42, 1_000_000_000);

        assert_eq!(payload.flags, 0);
        assert_eq!(payload.offset, 0);
        assert_eq!(payload.pts, 1_000_000_000);
        assert_eq!(payload.dts_offset, 0);
        assert_eq!(payload.seq, 42);
    }

    /// A meta region smaller than the full 32-byte header must be left
    /// untouched (no partial writes, no OOB).
    #[test]
    fn leaves_small_meta_untouched() {
        let mut payload: u64 = 0;
        let mut meta = pw::spa::sys::spa_meta {
            type_: pw::spa::sys::SPA_META_Header,
            size: 8,
            data: &mut payload as *mut _ as *mut _,
        };
        let mut buffer: pw::spa::sys::spa_buffer = unsafe { std::mem::zeroed() };
        let buf = buffer_with_meta(&mut meta, &mut buffer);

        write_header_meta(buf, 42, 1_000_000_000);

        assert_eq!(payload, 0);
    }

    /// A buffer without any Header meta (e.g. only the auto `Busy` meta) is
    /// a no-op.
    #[test]
    fn no_meta_is_a_noop() {
        let mut payload: u64 = 0;
        let mut meta = pw::spa::sys::spa_meta {
            type_: pw::spa::sys::SPA_META_Busy,
            size: 8,
            data: &mut payload as *mut _ as *mut _,
        };
        let mut buffer: pw::spa::sys::spa_buffer = unsafe { std::mem::zeroed() };
        let buf = buffer_with_meta(&mut meta, &mut buffer);

        write_header_meta(buf, 42, 1_000_000_000);

        assert_eq!(payload, 0);
    }
}
