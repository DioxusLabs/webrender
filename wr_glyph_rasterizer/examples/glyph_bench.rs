/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Glyph rasterization micro-benchmark.
//!
//! Drives the platform `FontContext` (FreeType by default on Linux, or the
//! Fontations backend when built with `--features fontations`) directly with
//! a realistic set of workloads, and prints per-glyph timings in a
//! machine-parsable format:
//!
//! ```text
//! RESULT <backend> <scenario> <glyphs_per_iter> <iters> <ns_per_glyph_mean> <ns_per_glyph_min_iter>
//! ```
//!
//! Build/run (from the repo root):
//!   FreeType:   cargo run --release -p wr_glyph_rasterizer --example glyph_bench
//!   Fontations: cargo run --release -p wr_glyph_rasterizer --example glyph_bench --features fontations

use std::sync::Arc;
use std::time::Instant;

use api::{
    FontInstanceFlags, FontInstanceKey, FontInstanceOptions, FontKey, FontRenderMode,
    IdNamespace, SyntheticItalics, units::DevicePoint,
};
use wr_glyph_rasterizer::platform::font::FontContext;
use wr_glyph_rasterizer::{BaseFontInstance, FontInstance, FontTransform, GlyphKey};

#[cfg(feature = "fontations")]
const BACKEND: &str = "fontations";
#[cfg(not(feature = "fontations"))]
const BACKEND: &str = "freetype";

const NAMESPACE: IdNamespace = IdNamespace(1);

struct FontSpec {
    key: FontKey,
    bytes: Arc<Vec<u8>>,
}

fn load_font(path: &str, id: u32) -> FontSpec {
    let bytes = std::fs::read(path).expect(path);
    FontSpec {
        key: FontKey::new(NAMESPACE, id),
        bytes: Arc::new(bytes),
    }
}

struct Scenario {
    name: &'static str,
    font_id: u32,
    size: f32,
    flags: FontInstanceFlags,
    synthetic_italics: SyntheticItalics,
    render_mode: FontRenderMode,
    transform: FontTransform,
    subpx_offsets: &'static [f32],
    /// Use the CJK workload charset instead of ASCII+Latin-1.
    cjk: bool,
    /// Measure get_glyph_dimensions instead of rasterize_glyph.
    dimensions_only: bool,
    /// Recreate the FontContext (and re-add fonts) before every iteration,
    /// measuring the cache-miss / cold cost.
    fresh_context: bool,
}

impl Scenario {
    fn base(name: &'static str, size: f32) -> Self {
        Scenario {
            name,
            font_id: 0,
            size,
            flags: FontInstanceFlags::empty(),
            synthetic_italics: SyntheticItalics::disabled(),
            render_mode: FontRenderMode::Alpha,
            transform: FontTransform::identity(),
            subpx_offsets: &[0.0],
            cjk: false,
            dimensions_only: false,
            fresh_context: false,
        }
    }
}

fn make_instance(scenario: &Scenario, instance_id: u32) -> FontInstance {
    let font_key = FontKey::new(NAMESPACE, scenario.font_id);
    let options = FontInstanceOptions {
        render_mode: scenario.render_mode,
        flags: scenario.flags,
        synthetic_italics: scenario.synthetic_italics,
        _padding: 0,
    };
    let base = BaseFontInstance::new(
        FontInstanceKey::new(NAMESPACE, instance_id),
        font_key,
        scenario.size,
        Some(options),
        None,
        Vec::new(),
    );
    let mut instance = FontInstance::from_base(Arc::new(base));
    instance.transform = scenario.transform;
    FontContext::prepare_font(&mut instance);
    instance
}

/// The glyph indices of the printable ASCII + Latin-1 characters.
fn workload_glyph_indices(context: &mut FontContext, font_key: FontKey) -> Vec<u32> {
    let mut indices = Vec::new();
    for ch in (0x20u32..0x7F).chain(0xA0..0x100) {
        let ch = char::from_u32(ch).unwrap();
        if let Some(index) = context.get_glyph_index(font_key, ch) {
            if index != 0 {
                indices.push(index);
            }
        }
    }
    indices
}

/// A CJK workload: ~160 ideographs sampled across the URO block plus a set
/// of especially stroke-dense characters.
fn cjk_glyph_indices(context: &mut FontContext, font_key: FontKey) -> Vec<u32> {
    let mut chars: Vec<char> = (0x4E00u32..0x9FA5).step_by(347)
        .filter_map(char::from_u32)
        .collect();
    // Stroke-dense / complex ideographs.
    chars.extend("龘龖靉鬱鮤鹾囔灩籲蠻鑬鮯鱲鸯讞讚釁雞鐹鑾翮餞餎餮囊蠇讓讐麩黤黹黽齏爨瑩琺".chars());
    let mut indices = Vec::new();
    for ch in chars {
        if let Some(index) = context.get_glyph_index(font_key, ch) {
            if index != 0 {
                indices.push(index);
            }
        }
    }
    indices
}

fn make_keys(scenario: &Scenario, indices: &[u32], instance: &FontInstance) -> Vec<GlyphKey> {
    let subpx_dir = instance.get_subpx_dir();
    let mut keys = Vec::new();
    for &offset in scenario.subpx_offsets {
        for &index in indices {
            keys.push(GlyphKey::new(
                index,
                DevicePoint::new(offset, 0.0),
                subpx_dir,
            ));
        }
    }
    keys
}

fn add_fonts(context: &mut FontContext, fonts: &[FontSpec]) {
    for font in fonts {
        context.add_raw_font(&font.key, font.bytes.clone(), 0);
    }
}

fn run_scenario(scenario: &Scenario, fonts: &[FontSpec], instance_id: u32) {
    let mut context = FontContext::new();
    add_fonts(&mut context, fonts);

    let instance = make_instance(scenario, instance_id);
    let font_key = FontKey::new(NAMESPACE, scenario.font_id);
    let indices = if scenario.cjk {
        cjk_glyph_indices(&mut context, font_key)
    } else {
        workload_glyph_indices(&mut context, font_key)
    };
    let keys = make_keys(scenario, &indices, &instance);
    assert!(!keys.is_empty());

    // Choose iteration counts so each scenario runs for roughly a fixed
    // amount of time. First estimate the per-iteration cost with warmup.
    let warmup_iters = if scenario.fresh_context { 3 } else { 10 };
    let mut rasterized = 0usize;
    let mut failed = 0usize;
    let warmup_start = Instant::now();
    for _ in 0..warmup_iters {
        if scenario.fresh_context {
            context = FontContext::new();
            add_fonts(&mut context, fonts);
        }
        FontContext::begin_rasterize(&instance);
        for key in &keys {
            if scenario.dimensions_only {
                if context.get_glyph_dimensions(&instance, key).is_some() {
                    rasterized += 1;
                } else {
                    failed += 1;
                }
            } else {
                match context.rasterize_glyph(&instance, key) {
                    Ok(glyph) => {
                        std::hint::black_box(&glyph.bytes);
                        rasterized += 1;
                    }
                    Err(_) => failed += 1,
                }
            }
        }
        FontContext::end_rasterize(&instance);
    }
    let per_iter = warmup_start.elapsed() / warmup_iters as u32;

    // Aim for ~2s of measurement, between 5 and 200 iterations.
    let target = std::time::Duration::from_secs(2);
    let iters = (target.as_secs_f64() / per_iter.as_secs_f64().max(1e-9))
        .clamp(5.0, 200.0) as usize;

    let mut iter_times = Vec::with_capacity(iters);
    for _ in 0..iters {
        if scenario.fresh_context {
            context = FontContext::new();
            add_fonts(&mut context, fonts);
        }
        let start = Instant::now();
        FontContext::begin_rasterize(&instance);
        for key in &keys {
            if scenario.dimensions_only {
                std::hint::black_box(context.get_glyph_dimensions(&instance, key));
            } else {
                match context.rasterize_glyph(&instance, key) {
                    Ok(glyph) => {
                        std::hint::black_box(&glyph.bytes);
                    }
                    Err(_) => {}
                }
            }
        }
        FontContext::end_rasterize(&instance);
        iter_times.push(start.elapsed());
    }

    let glyphs_per_iter = keys.len();
    let mean_ns = iter_times.iter().map(|t| t.as_nanos()).sum::<u128>() as f64
        / (iters as f64 * glyphs_per_iter as f64);
    let min_ns = iter_times.iter().map(|t| t.as_nanos()).min().unwrap() as f64
        / glyphs_per_iter as f64;

    println!(
        "RESULT {} {} glyphs={} iters={} ok={} fail={} mean_ns_per_glyph={:.0} min_iter_ns_per_glyph={:.0}",
        BACKEND, scenario.name, glyphs_per_iter, iters,
        rasterized / warmup_iters, failed / warmup_iters,
        mean_ns, min_ns,
    );
}

fn main() {
    let root = std::env::var("WR_ROOT").unwrap_or_else(|_| ".".to_string());
    let font_path = |name: &str| format!("{}/wrench/reftests/text/{}", root, name);

    let mut fonts = vec![
        load_font(&font_path("FreeSans.ttf"), 0),
        load_font(&font_path("Proggy.ttf"), 1),
        load_font(&font_path("VeraBd.ttf"), 2),
    ];
    // Optional CJK font (e.g. Noto Sans SC); enables the cjk_* scenarios.
    let cjk_font = std::env::var("WR_CJK_FONT").ok();
    if let Some(path) = &cjk_font {
        fonts.push(load_font(path, 3));
    }

    let rotate30 = {
        let (s, c) = (30f32.to_radians().sin(), 30f32.to_radians().cos());
        FontTransform::new(c, -s, s, c)
    };
    let skew = FontTransform::new(1.0, 0.25, 0.0, 1.0);

    let mut scenarios = vec![
        Scenario::base("alpha_10px", 10.0),
        Scenario::base("alpha_16px", 16.0),
        Scenario::base("alpha_24px", 24.0),
        Scenario::base("alpha_48px", 48.0),
        Scenario {
            subpx_offsets: &[0.0, 0.25, 0.5, 0.75],
            flags: FontInstanceFlags::SUBPIXEL_POSITION,
            ..Scenario::base("subpixel_positions_16px", 16.0)
        },
        Scenario {
            flags: FontInstanceFlags::SYNTHETIC_BOLD,
            ..Scenario::base("synthetic_bold_16px", 16.0)
        },
        Scenario {
            synthetic_italics: SyntheticItalics::enabled(),
            ..Scenario::base("synthetic_italics_16px", 16.0)
        },
        Scenario {
            transform: rotate30,
            ..Scenario::base("rotated30_16px", 16.0)
        },
        Scenario {
            transform: skew,
            ..Scenario::base("skewed_16px", 16.0)
        },
        Scenario {
            render_mode: FontRenderMode::Mono,
            ..Scenario::base("mono_16px", 16.0)
        },
        Scenario {
            render_mode: FontRenderMode::Subpixel,
            ..Scenario::base("subpixel_aa_16px", 16.0)
        },
        Scenario {
            font_id: 1,
            flags: FontInstanceFlags::EMBEDDED_BITMAPS,
            ..Scenario::base("embedded_bitmap_proggy_8px", 8.25)
        },
        Scenario {
            dimensions_only: true,
            ..Scenario::base("dimensions_16px", 16.0)
        },
        Scenario {
            fresh_context: true,
            ..Scenario::base("fresh_context_16px", 16.0)
        },
    ];

    if cjk_font.is_some() {
        for &size in &[16.0f32, 24.0, 48.0] {
            let name: &'static str = Box::leak(
                format!("cjk_alpha_{}px", size as u32).into_boxed_str(),
            );
            scenarios.push(Scenario {
                font_id: 3,
                cjk: true,
                ..Scenario::base(name, size)
            });
        }
    }

    if let Ok(filter) = std::env::var("BENCH_FILTER") {
        scenarios.retain(|s| s.name.contains(&filter));
    }

    println!("backend: {}", BACKEND);
    for (i, scenario) in scenarios.iter().enumerate() {
        run_scenario(scenario, &fonts, 100 + i as u32);
    }
}
