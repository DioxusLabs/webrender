//! Informal throughput benchmark for the coverage sweep.
//!
//! Run the SIMD sweep with `cargo run --release --example bench` and the
//! scalar sweep with `cargo run --release --example bench --features
//! scalar_sweep`.

use std::time::Instant;

use hb_raster::{Extents, Raster};

/// Draw a wavy ring, which touches many cells per row.
fn draw_ring(r: &mut Raster, size: f32, phase: f32) {
    let (cx, cy) = (size * 0.5, size * 0.5);
    let steps = 256;
    for (i, radius) in [size * 0.45, size * 0.3].iter().enumerate() {
        // The inner contour runs backwards so that it punches a hole.
        let dir = if i == 1 { -1.0 } else { 1.0 };
        for step in 0 .. steps {
            let t = dir * step as f32 / steps as f32 * std::f32::consts::TAU;
            let rad = radius * (1.0 + 0.05 * (t * 7.0 + phase).sin());
            let (x, y) = (cx + rad * t.cos(), cy + rad * t.sin());
            if step == 0 {
                r.move_to(x, y);
            } else {
                r.line_to(x, y);
            }
        }
        r.close_path();
    }
}

fn main() {
    let sweep = if cfg!(feature = "scalar_sweep") {
        "scalar"
    } else if cfg!(target_arch = "x86_64") {
        "sse2"
    } else if cfg!(target_arch = "aarch64") {
        "neon"
    } else {
        "scalar"
    };

    for &size in &[16u32, 64, 256] {
        let extents = Extents { x0: 0, y0: 0, width: size, height: size };
        let stride = size as usize;
        let mut out = vec![0u8; stride * size as usize];
        let mut raster = Raster::new();

        // Warm up.
        for i in 0 .. 10 {
            draw_ring(&mut raster, size as f32, i as f32);
            raster.render(extents, stride, &mut out);
        }

        let iters = 2_000_000 / (size as u64 * size as u64).max(1);
        let start = Instant::now();
        let mut checksum: u64 = 0;
        for i in 0 .. iters {
            draw_ring(&mut raster, size as f32, i as f32);
            raster.render(extents, stride, &mut out);
            checksum += out[stride * (size as usize / 2) + size as usize / 2] as u64;
        }
        let elapsed = start.elapsed();

        println!(
            "{:>6} {:>4}x{:<4} {:>7} iters  {:>9.3} ms  {:>9.3} us/render (checksum {})",
            sweep,
            size,
            size,
            iters,
            elapsed.as_secs_f64() * 1e3,
            elapsed.as_secs_f64() * 1e6 / iters as f64,
            checksum,
        );
    }
}
