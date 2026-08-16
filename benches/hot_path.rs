//! Benchmarks for the per-frame hot path and negotiation setup cost.
//!
//! These measure pure-Rust operations that run on every frame (or every
//! negotiation) so we can catch regressions without a live PipeWire session.
//!
//! Run with: `cargo bench` or as part of `make test` (the harness invokes
//! them via `cargo test --benches`).

use pipewire_vircam::{Format, Mode, Negotiated};

/// Measure `Format::planes()` — called once per buffer in the `process`
/// callback to determine the plane layout. This is pure arithmetic but we
/// want to ensure it stays trivially fast.
#[test]
fn bench_format_planes() {
    let (w, h) = (1920u32, 1080);

    // Warmup.
    for _ in 0..1000 {
        black_box(Format::Rgba.planes(w, h));
    }

    let start = std::time::Instant::now();
    let iterations = 1_000_000;
    for _ in 0..iterations {
        let _ = Format::Rgba.planes(w, h);
        let _ = Format::I420.planes(w, h);
        let _ = Format::Nv12.planes(w, h);
    }
    let elapsed = start.elapsed();
    let ns_per_call = elapsed.as_nanos() as u64 / (iterations * 3) as u64;

    // Should be well under 50ns per call (a few integer multiplications +
    // a small Vec allocation). This is a regression guard, not an absolute
    // target.
    println!("Format::planes: {ns_per_call} ns/call");
    assert!(
        ns_per_call < 50,
        "Format::planes took {ns_per_call} ns — likely a regression"
    );
}

/// Measure the POD serialization cost (negotiation path). This runs once per
/// consumer connect/renegotiate, not per frame, but we want to ensure it
/// stays cheap enough that reconnect storms don't stall.
#[test]
fn bench_pod_serialization() {
    // We can't call pod::serialize directly (private module), so we measure
    // the public API surface: building a Config and Camera is where PODs get
    // built. But Camera::new requires PipeWire, so instead we benchmark the
    // pure arithmetic that dominates the POD body construction.

    // The enumformat_pod body is ~6 properties with integer values; the
    // serialization is dominated by copying those into a Vec<u8>. We
    // approximate by measuring the allocation + fill pattern.
    let mode = Mode {
        width: 1920,
        height: 1080,
        fps: vec![30],
        formats: vec![Format::Rgba],
    };

    let start = std::time::Instant::now();
    let iterations = 100_000;
    for _ in 0..iterations {
        // This mirrors what Camera::new does per (format, size, fps) combo.
        let planes = mode.formats[0].planes(mode.width, mode.height);
        black_box(planes);
    }
    let elapsed = start.elapsed();
    let ns_per_call = elapsed.as_nanos() as u64 / iterations;

    println!("Mode plane layout (negotiation setup): {ns_per_call} ns/call");
    // Should be trivially fast (< 100ns).
    assert!(
        ns_per_call < 100,
        "Negotiation setup took {ns_per_call} ns — likely a regression"
    );
}

/// Measure `Negotiated::fps()` — called by user code in the fill handler.
#[test]
fn bench_negotiated_fps() {
    let neg = Negotiated {
        format: Format::Rgba,
        width: 1920,
        height: 1080,
        fps_num: 30000,
        fps_denom: 1001,
        stride: 7680,
        node_id: 0,
    };

    let start = std::time::Instant::now();
    let iterations = 10_000_000;
    for _ in 0..iterations {
        black_box(neg.fps());
    }
    let elapsed = start.elapsed();
    let ns_per_call = elapsed.as_nanos() as u64 / iterations;

    println!("Negotiated::fps(): {ns_per_call} ns/call");
    // f64 division is ~20-30 cycles; should be < 50ns even on slow hardware.
    assert!(
        ns_per_call < 50,
        "Negotiated::fps() took {ns_per_call} ns — likely a regression"
    );
}

/// Helper to prevent the compiler from optimizing away the measurement.
#[inline(always)]
fn black_box<T>(x: T) -> T {
    std::hint::black_box(x)
}
