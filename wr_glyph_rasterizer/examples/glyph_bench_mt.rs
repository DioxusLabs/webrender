/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! End-to-end multithreaded glyph rasterization benchmark.
//!
//! Drives WebRender's real `GlyphRasterizer` (request_glyphs / resolve_glyphs
//! with its rayon worker pool and batching heuristics) with a large number of
//! glyph requests, exactly as the render backend does. vello_cpu internal
//! threading stays at the production setting (num_threads: 0); parallelism
//! comes purely from WR's worker pool.
//!
//! Environment variables:
//!   WR_ROOT    repo root (default ".")
//!   WR_WORKERS worker thread count for the rayon pool (default: rayon's
//!              default, i.e. num logical CPUs)
//!   WR_REPEATS number of timed repeats (default 5)
//!
//! Output:
//!   MTRESULT <backend> workers=<n> glyphs=<per_repeat> repeats=<n> \
//!            mean_ms=<..> min_ms=<..> glyphs_per_sec=<..>
//!
//! Build/run (from the repo root):
//!   FreeType:   cargo run --release -p wr_glyph_rasterizer --example glyph_bench_mt
//!   Fontations: cargo run --release -p wr_glyph_rasterizer --example glyph_bench_mt --features fontations

use std::sync::Arc;
use std::time::Instant;

use api::{
    ColorF, FontInstanceFlags, FontInstanceKey, FontInstanceOptions, FontKey,
    FontRenderMode, FontTemplate, IdNamespace, units::DevicePoint,
};
use rayon::ThreadPoolBuilder;
use wr_glyph_rasterizer::{
    BaseFontInstance, FontInstance, GlyphKey, GlyphRasterizer, SharedFontResources,
    profiler::GlyphRasterizeProfiler,
};

#[cfg(feature = "fontations")]
const BACKEND: &str = "fontations";
#[cfg(not(feature = "fontations"))]
const BACKEND: &str = "freetype";

struct Profiler;
impl GlyphRasterizeProfiler for Profiler {
    fn start_time(&mut self) {}
    fn end_time(&mut self) -> f64 {
        0.
    }
    fn set(&mut self, _value: f64) {}
}

fn main() {
    let root = std::env::var("WR_ROOT").unwrap_or_else(|_| ".".to_string());
    let repeats: usize = std::env::var("WR_REPEATS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let workers_requested: Option<usize> = std::env::var("WR_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok());

    let namespace = IdNamespace(0);
    let mut fonts = SharedFontResources::new(namespace);

    let font_key = FontKey::new(namespace, 0);
    let raw_font_data =
        std::fs::read(format!("{}/wrench/reftests/text/FreeSans.ttf", root)).unwrap();
    let font_template = FontTemplate::Raw(Arc::new(raw_font_data), 0);
    let shared_font_key = fonts
        .font_keys
        .add_key(&font_key, &font_template)
        .expect("Failed to add font key");
    fonts.templates.add_font(shared_font_key, font_template);

    let mut builder = ThreadPoolBuilder::new().thread_name(|idx| format!("WRWorker#{}", idx));
    if let Some(n) = workers_requested {
        builder = builder.num_threads(n);
    }
    let workers = Arc::new(builder.build().unwrap());
    let num_workers = workers.current_num_threads();

    let mut glyph_rasterizer = GlyphRasterizer::new(workers, None, false);
    glyph_rasterizer.add_font(
        shared_font_key,
        fonts.templates.get_font(&shared_font_key).unwrap(),
    );

    // A font instance per size, as WR would create for styled text runs.
    let sizes: &[f32] = &[10.0, 12.0, 14.0, 16.0, 18.0, 20.0, 24.0, 28.0, 32.0, 48.0];
    let mut instances = Vec::new();
    for (i, &size) in sizes.iter().enumerate() {
        let instance_key = FontInstanceKey::new(namespace, 1 + i as u32);
        let base = BaseFontInstance::new(
            instance_key,
            shared_font_key,
            size,
            Some(FontInstanceOptions {
                render_mode: FontRenderMode::Alpha,
                ..Default::default()
            }),
            None,
            Vec::new(),
        );
        let shared_instance = fonts
            .instance_keys
            .add_key(base)
            .expect("Failed to add font instance key");
        fonts.instances.add_font_instance(shared_instance);

        let mut font = FontInstance::new(
            fonts.instances.get_font_instance(instance_key).unwrap(),
            ColorF::BLACK.into(),
            FontRenderMode::Alpha,
            FontInstanceFlags::SUBPIXEL_POSITION,
        );
        glyph_rasterizer.prepare_font(&mut font);
        instances.push(font);
    }

    // Glyph indices for printable ASCII + Latin-1.
    let mut indices = Vec::new();
    for ch in (0x20u32..0x7F).chain(0xA0..0x100) {
        let ch = char::from_u32(ch).unwrap();
        if let Some(index) = glyph_rasterizer.get_glyph_index(shared_font_key, ch) {
            if index != 0 {
                indices.push(index);
            }
        }
    }

    // 4 subpixel offsets per glyph per size: 191 * 4 * 10 = 7640 unique
    // requests per repeat.
    let offsets = [0.0f32, 0.25, 0.5, 0.75];
    let mut per_instance_keys: Vec<Vec<GlyphKey>> = Vec::new();
    for font in &instances {
        let subpx_dir = font.get_subpx_dir();
        let mut keys = Vec::new();
        for &offset in &offsets {
            for &index in &indices {
                keys.push(GlyphKey::new(
                    index,
                    DevicePoint::new(offset, 0.0),
                    subpx_dir,
                ));
            }
        }
        per_instance_keys.push(keys);
    }
    let glyphs_per_repeat: usize = per_instance_keys.iter().map(|k| k.len()).sum();

    // Requests are submitted in chunks like WR text runs; the rasterizer's
    // own GLYPH_BATCH_SIZE batching/flushing then kicks in.
    const CHUNK: usize = 32;

    let mut run = |glyph_rasterizer: &mut GlyphRasterizer| -> (std::time::Duration, usize) {
        let start = Instant::now();
        for (font, keys) in instances.iter().zip(&per_instance_keys) {
            for chunk in keys.chunks(CHUNK) {
                glyph_rasterizer.request_glyphs(font.clone(), chunk, |_| true);
            }
        }
        let mut ok = 0usize;
        glyph_rasterizer.resolve_glyphs(
            |job, _| {
                if let Ok(glyph) = job.result {
                    std::hint::black_box(&glyph.bytes);
                    ok += 1;
                }
            },
            &mut Profiler,
        );
        (start.elapsed(), ok)
    };

    // Warmup.
    let (_, ok) = run(&mut glyph_rasterizer);

    let mut times = Vec::new();
    for _ in 0..repeats {
        let (t, _) = run(&mut glyph_rasterizer);
        times.push(t);
    }

    let mean_ms = times.iter().map(|t| t.as_secs_f64()).sum::<f64>() / repeats as f64 * 1e3;
    let min_ms = times
        .iter()
        .map(|t| t.as_secs_f64())
        .fold(f64::INFINITY, f64::min)
        * 1e3;
    println!(
        "MTRESULT {} workers={} glyphs={} ok={} repeats={} mean_ms={:.1} min_ms={:.1} glyphs_per_sec={:.0}",
        BACKEND,
        num_workers,
        glyphs_per_repeat,
        ok,
        repeats,
        mean_ms,
        min_ms,
        glyphs_per_repeat as f64 / (mean_ms / 1e3),
    );
}
