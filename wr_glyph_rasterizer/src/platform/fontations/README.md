# Glyphrast backend

The `fontations` feature uses glyphrast at revision
`08e742ba11d6a63971483eaf732dd714fe4a3c90`. Wrench enables this feature on
its WebRender dependency; build it with `cargo build --release --features software`
from `wrench/`.

## Caching

Each worker's `FontContext` owns one persistent glyphrast `RasterContext`
(including its bounded scaler cache and rasterization scratch storage), plus
one reusable `Image` and an outline buffer for dimension queries. Registered fonts own a glyphrast/read-fonts `Font`, which
shares its byte storage and lazily cached parsed tables; WebRender no longer
needs its own unsafe yoke adapter. Native fonts keep memory-mapped storage.

Variation instances are constructed once per registered-font generation and
`FontInstanceKey`. The scaler cache keys include the generation, normalized
coordinates, size and hinting options. Re-registering a font assigns a fresh
generation; deleting it removes the registered font and its variation instances.
Deleting an instance removes its variation entry. Glyphrast has no public
per-font scaler eviction API, so its bounded internal cache can retain deleted
fonts until eviction, but cannot reuse them for a new generation.

WebRender's existing higher-level glyph cache and worker distribution are
unchanged. Final atlas buffers remain owned by WebRender; glyphrast image
storage is reused between calls.

## Known limitations at the pinned revision

These are intentionally not compensated for in WebRender:

* **Scaled embedded bitmaps can disappear.** Glyphrast's
  `Scaler::scale_bitmap_impl` / `Bitmap::scaled_size` truncate scaled width and
  height to integers. At 8.25px, Proggy punctuation such as `!`, `'`, `-`,
  `.`, `:`, `_` and `|` gets a zero width or height. The benchmark renders
  177/191 glyphs versus FreeType's 188/191 (spaces account for three legitimate
  empty glyphs). A direct glyphrast-only probe reproduces the zero placements.
  Preserving small glyphs is necessary to match FreeType.
* **Monochrome rendering/hinting:** `zeno::Format` has alpha and subpixel
  formats, but no monochrome format, and glyphrast's `RasterTarget` has no
  mono target. Mono requests disable subpixel positioning but currently use
  grayscale coverage with normal hinting, not FreeType's monochrome result.
* **Vertical LCD:** glyphrast exposes vertical LCD hinting, but its Render API
  uses zeno's horizontal subpixel offsets, with no vertical raster format.
  `LCD_VERTICAL` therefore cannot obtain FreeType-equivalent vertical coverage.
* **Bitmap transforms:** glyphrast's `Render::render_into` bitmap branches
  delegate directly to bitmap scaling. They do not apply Render's transform,
  subpixel offset or embolden settings. Ordinary resizing is handled inside
  glyphrast, but rotated/skewed/italic/bold bitmap instances cannot match
  transformed outlines.
* **Color emboldening:** `Render::embolden` is applied only in the plain
  outline branch, not COLRv0/v1. Synthetic bold color outlines are unchanged.
* **Color alpha contract:** `Content::Color` has no straight/premultiplied
  distinction. Its documentation describes straight alpha for PNG/COLRv0,
  while the COLRv1 painter and COLRv0 blitter produce premultiplied pixels;
  CBDT bytes are passed through. The adapter follows the straight-RGBA
  contract when producing WebRender's premultiplied BGRA, so already
  premultiplied sources with translucent pixels can be darkened. A consistent
  upstream alpha contract is needed; this is source-inspected, not covered
  by the text reftests' non-color font fixtures.

* **Bitmap/color bounds require rendering.** `Scaler::scale_outline_into` and
  zeno's public bounds API provide exact plain-outline placement without coverage
  generation. There is no corresponding glyphrast bitmap strike/placement query:
  `strike::Bitmap`, `Strikes` and `Scaler::scale_bitmap_impl` are private;
  the public `scale_bitmap_into` decodes/resizes pixels. Color rendering also has
  no placement-only API covering COLRv0/v1 and their source fallback rules.
  Queries for fonts with these sources therefore retain rendering rather than
  duplicating glyphrast's strike selection, color painting or failure semantics.

Plain-outline dimension queries now scale into a reusable outline, apply the
same embolden, transform and signed subpixel offset as Render, and use zeno's
path bounds with floor(min)/ceil(max), exactly as `Mask::placement` does.
No coverage buffer is generated. Tests compare placements against rendering
for Latin-1, empty glyphs, bold, italics, rotation, skew, reflection, horizontal
and vertical quarter-pixel positions, and alpha/LCD; invalid IDs return None.
Bitmap queries test the retained rendering fallback.

## Validation (Linux, software rendering)

`cargo check -p wr_glyph_rasterizer --features fontations` succeeded; the
library tests passed (8 passed, 0 failed).

For `wrench --headless --software reftest reftests/text`:

| Backend | Passing | Failing |
| --- | ---: | ---: |
| glyphrast | 47 | 1 |
| FreeType | 48 | 0 |
| original vello_cpu fontations | 46 | 2 |

The initial glyphrast failure was `snap-clip.yaml == snap-clip-ref.yaml`: one pixel,
maximum channel difference 3. Original fontations also fails this comparison
(one pixel, maximum difference 2), and additionally fails
`allow-subpixel.yaml != allow-subpixel-ref.yaml`. No tolerance or rasterization
workaround was added. The cause is now isolated below; follow-up counts include
one additional failure observed in the same round as the LCD correction.
The direct comparison below rules out LCD order as its cause.

### Benchmarks

Mean ns per glyph from `glyph_bench`, 200 iterations per scenario, run
sequentially after builds completed:

| Scenario | glyphrast | FreeType | vello_cpu |
| --- | ---: | ---: | ---: |
| alpha 10px | 2152 | 2825 | 2675 |
| alpha 16px | 2733 | 3282 | 3177 |
| alpha 24px | 3702 | 3916 | 3715 |
| alpha 48px | 6183 | 5595 | 5497 |
| subpixel positions 16px | 2870 | 4461 | 3233 |
| synthetic bold 16px | 3348 | 5549 | 4881 |
| synthetic italics 16px | 2973 | 2558 | 3134 |
| rotated 30 degrees 16px | 3246 | 3023 | 3267 |
| skewed 16px | 3249 | 2562 | 3080 |
| mono 16px | 2979 | 4892 | 2981 |
| subpixel AA 16px | 6672 | 13194 | 2974 |
| embedded bitmap Proggy 8.25px | 706 | 331 | 593 |
| dimensions 16px | 2725 | 3029 | 769 |
| fresh context 16px | 3197 | 3731 | 3059 |

The old vello_cpu backend disables subpixel AA, so that row measures grayscale
there. Mono output is also not equivalent. Bitmap success counts differ:
glyphrast 177/191, FreeType 188/191, vello_cpu 0/191; bitmap timings are not an
equal-work comparison. Other raster scenarios return 189/191 glyphs (two empty
spaces), and four-offset scenarios return 756/764. Dimension queries succeed
for all 191 glyphs. No optional CJK font was supplied.

`glyph_bench_mt`, four workers, five repeats, 764 requested glyphs / 756
nonempty results per size; mean milliseconds:

| Size | glyphrast | FreeType | vello_cpu |
| --- | ---: | ---: | ---: |
| 10px | 2.26 | 14.89 | 2.82 |
| 16px | 2.70 | 17.37 | 3.30 |
| 24px | 3.74 | 18.75 | 3.88 |
| 48px | 7.31 | 27.79 | 6.45 |

These are single-machine measurements, not statistically controlled claims
about renderer performance. The benchmark's FreeType path has different
threading behavior from fontations.

## Follow-up: bounds, profiling and LCD correctness

### Changes

* Dimension queries avoid rendering for plain outlines; rasterization no longer
  computes the unused horizontal advance.
* Atlas conversion dispatches on content once per row, rather than per pixel.
  Alpha expansion uses bounded source/destination slices and chunked writes;
  LCD rows are copied once and their alpha is filled separately. The owned
  atlas allocation and glyphrast's reusable image are still necessary.
* Corrected the LCD format mapping: default uses `Format::Subpixel`;
  `SUBPIXEL_BGR` uses `Format::subpixel_bgra()`. Zeno's offsets translate
  the *path*, rather than specifying sample locations. Despite the helper name,
  the former produces the FreeType-compatible BGRA coverage order for ordinary
  RGB panels. A FreeSans glyph-50 probe at 26.6667px gave right-edge BGRA
  `[0,3,86,86]` in FreeType, `[78,3,0,78]` before this correction, and
  `[0,3,78,78]` afterward. Coverage magnitudes need not match the different
  LCD filters. Tests check right-edge channel orientation and exact BGR reversal.
* Subpixel signs were already correct: both backends add positive x and
  *negative* y to a y-up outline (`unix/font.rs::rasterize_glyph_outline`).
  Quarter-pixel GlyphKey offsets are exactly representable in FreeType's 26.6
  conversion; glyphrast receives the same offsets without a sign change.

### snap-clip: missing mono, not placement rounding

The manifest uses `options(disable-aa)`; manifest parsing maps this
to `FontRenderMode::Mono`. YAML's size 20 is points
(`YamlHelper::as_pt_to_f32`), i.e. 26.6667px. The backend disables subpixel
positioning for Mono but glyphrast has neither a mono format nor a mono hint
target, so `Render::render_into` uses `Format::Alpha` with normal hinting.

Decoded failure images differ only at (30,20): clipped white (255) versus
reference gray (252). A direct glyphrast-only reproduction with FreeSans
glyph 50 has outline max-x 20.03125, placement left=1,width=20, and coverage
3 at local (20,10). The glyph's origin is (10,30), placing this sample at
(30,20), outside the snapped right clip edge 30. The unclipped reference
therefore retains it. The adapter returns exactly [3,3,3,3] for this pixel.
FreeType Mono instead has left=1,width=19, excluding that pixel.
**Disproof of a placement-only cause:** requesting Alpha from FreeType also
produces [3,3,3,3] there, with the same left=1,width=20 placement.
This is the existing mono capability gap (`zeno::Format`,
`glyphrast::RasterTarget`, `Render::render_into`), not a rounding fix to
make in WebRender. No thresholding or clipping workaround was added.

### Profiles

Hardware `perf` is unavailable here (`perf_event_paranoid=4`); Callgrind
was used instead. These are **retired instruction counts, not sampled CPU
time**. A copy of glyph_bench capped at 20 timed iterations (plus its 10
warmups), otherwise unchanged, used BENCH_FILTER for each scenario. Font
parsing/startup is included. Symbols below are demangled for readability.
Reproduce the profile shape with, for example:

```sh
BENCH_FILTER=alpha_48px valgrind --tool=callgrind target/release/examples/glyph_bench
callgrind_annotate --auto=no callgrind.out.*
```

| Scenario | Before total instructions | After | Before adapter rasterize self | After adapter rasterize self |
| --- | ---: | ---: | ---: | ---: |
| Alpha 48px | 400519832 | 311899009 | 114057150 | 26080131 |
| Italics 16px | 160379917 | 148493838 | 18335880 | 7076481 |
| Proggy 8.25px | 59430933 | 57775368 | 3738180 | 2726460 |

This isolates a real WebRender-side conversion cost even when wall-clock
measurements are noisy. No added per-glyph font parse or cache rebuild was
found; scaler construction still looks up the persistent per-worker cache.

Remaining performance gaps / upstream hotspots:

* **Outline coverage cost:** `Render::render_into` dispatches the outline
  branch to zeno. After the adapter optimization, Alpha 48px spends 45.43%
  of instructions in `zeno::Rasterizer::line_to`, 16.87% in
  `Rasterizer::rasterize` and 4.39% in `quad_to`.
  Italics spends 42.05%, 11.30% and 5.99%, respectively.
  Before conversion optimization, rotation and skew already spent
  41.19%/38.65% in line_to, 10.54%/10.49% in rasterize and
  5.23%/5.60% in quad_to. Both backends disable hinting for these transforms,
  so this is not an accidental hinted-vs-unhinted comparison.
  FreeType's corresponding profiles instead lead with `gray_conic_to`
  (18–19% for the transforms); its complete instruction totals were
  145777624 (italics), 155524177 (rotation), 144915216 (skew), versus
  glyphrast's original 160379917/175597842/158917797.
  These are library coverage costs, not evidence of a specific algorithmic bug;
  instruction fractions do not establish their wall-clock contribution.
* **Bitmap resampling:** `Scaler::scale_bitmap_impl` reaches
  `bitmap::resize(Filter::Mitchell)` whenever requested size differs from
  strike ppem. Proggy 8.25px takes that branch. After optimization,
  `glyphrast::bitmap::resize` accounts for 34988970 instructions (60.56%),
  and `strike::Bitmap::decode` for 4461930 (7.72%).
  FreeType's bitmap profile totals only 20174440 instructions, with
  `tt_sbit_decoder_load_bit_aligned` and `tt_sbit_decoder_load_image`
  its leading library functions and no Mitchell resize. Glyphrast's public
  bitmap API offers no metadata-only or decoded-strike reuse hook to eliminate
  that work in the adapter. No nearest-neighbor/strike-selection workaround
  was substituted. Success counts still differ because of the existing
  scaled-dimension truncation gap.

### Follow-up verification and timings

Linux software text suite: **glyphrast 46 passing, 2 failing; FreeType 48
passing, 0 failing**. Library tests: **9 passed, 0 failed**. Fontations
cargo check and both software Wrench builds completed.

Failures:

* `snap-clip`: unchanged, max difference 3 at one pixel, explained above.
* **Additional failure observed alongside the LCD-order correction:** `split-batch`, max
  difference 2 across 1514 pixels; the existing software tolerance allows
  1510. An isolated run also reproduces 1514. Its two YAML files are
  byte-identical; the discrepancy is between successive scene renders,
  not a direct FreeType image comparison. Whether atlas coordinates or other
  renderer state explains the difference remains unverified.
  The specific renderer/coverage interaction is **not isolated**, and is not
  being claimed as a confirmed glyphrast bug. The correct LCD mapping is kept;
  no fuzzy tolerance, shader or raster workaround was added.

### Direct FreeType LCD comparison and split-batch investigation

An external probe used `platform::font::FontContext::{add_raw_font,
prepare_font,rasterize_glyph}` in separate FreeType and fontations builds.
It used identical font bytes, instance options, sizes and `GlyphKey`s:
FreeSans glyphs 36/50 and VeraBd glyphs 55/45, at 16, 26.6667 and 666.6667
device pixels; zero offset, horizontal quarter-pixel and vertical half-pixel
offsets; `FontRenderMode::Subpixel`; and both values of `SUBPIXEL_BGR`.
That is 72 renders per backend, 144 total. Buffers were aligned by their
returned left/top placement before comparing BGRA channels.

Both backends reverse R/B exactly when `SUBPIXEL_BGR` changes. All 72
cross-backend comparisons favor the current mapping over its opposite:
aggregate absolute RGB byte error is 605,848 versus 11,675,964. For example,
FreeSans glyph 50 at 26.6667px, zero offset, world pixel (20,10) in y-up
coordinates returns:

| Backend | BGR off, BGRA bytes | BGR on, BGRA bytes |
| --- | --- | --- |
| FreeType | `[0,3,86,86]` | `[86,3,0,86]` |
| glyphrast | `[0,3,78,78]` | `[78,3,0,78]` |

The correct mapping remains default -> `Format::Subpixel`, BGR ->
`Format::subpixel_bgra()`. Different coverage magnitudes are not channel-order
errors. Probe source, logs and raw buffers are retained in
`/home/ubuntu/glyphrast-results/lcd.rs`, `lcd-{freetype,glyphrast}.log` and
`lcd-comparison/` on the investigation machine.

`split-batch` does **not** exercise this mapping. Runtime rasterizer traces
show VeraBd glyphs 55/45 at 320px, `Alpha`, `TEXTURE_PADDING`, zero offset,
and `GlyphFormat::Alpha`. `TextRunPrimitive::update_font_instance` limits the
oversized 500pt text to `FONT_SIZE_LIMIT` and disables subpixel AA. The
`render_mode == Subpixel` branch selecting the LCD format is not reached.

Both current and previous LCD mappings can pass the isolated test. Crucially,
the previous mapping also fails with exactly the original 1,514 differing
pixels (maximum difference 2) when font cleanup is flushed before the reference
scene. Thus restoring the previous mapping is not a fix.

The differing runs track an asynchronous font-lifetime distinction in the
harness: `render_yaml` calls `YamlFrameReader::deinit`, which sends font
deletions without waiting. `ApiResources::update` deduplicates new fonts,
whereas `ResourceCache::update_resources` processes deletions after scene swap.
In passing traced runs the reference reused the shared font/glyph cache and
the images were identical. In failing runs it registered a new shared font;
the glyph atlas rectangles moved:

| Glyph | Test atlas rect | Reference atlas rect |
| --- | --- | --- |
| 45 | `(0,0)-(110,299)` | `(332,0)-(442,299)` |
| 55 | `(112,0)-(330,235)` | `(444,0)-(662,235)` |

The test/reference rasterized buffers were byte-identical for each glyph:
SHA-256 `86b99218b0d8fe079fca03ed4481aa3418d23aaec516ab2d3ee0cc2dd8c2c522`
for 45 and `f5231ddd2b488a0b5c41e7728cab9f1fabed5f065fa86c27fe8262ce86bf0830`
for 55. The failing rendered images also matched the original failure
byte-for-byte. Evidence is in `split-current-image-trace.log`,
`split-localuv.log` and `split-old-trace/` in the results directory.

This localizes the discrepancy downstream of glyph rasterization, with atlas
relocation/render-cache lifetime implicated, not a glyphrast coverage or LCD
bug. The exact renderer sampling operation responsible remains unresolved.
SWGL quantizes linear sampling to 1/128 texel (`LINEAR_QUANTIZE_UV`), but that
alone does not establish the complete causal chain. An origin-relative UV
interpolation experiment worsened the difference and was removed. All trace
hooks, cleanup barriers and shader experiments were removed; no tolerance
change or workaround is retained. A renderer fix is still outstanding, and
this is not added to the glyphrast gap list.

The final clean, uninstrumented software text run reported **47 passing,
1 failing** (`snap-clip`, one pixel, maximum difference 3). `split-batch`
passed in that run; this is not evidence of a fix, given the cache-lifetime
sensitivity above. Build and reftest logs are `lcd-final-wrench-build.log`
and `lcd-final-text-reftests.log` in the results directory.

### Follow-up benchmark numbers

Mean ns/glyph, 200 iterations per scenario, sequential runs after builds and
profiling completed. Before columns are the initial recorded runs; current
FreeType was remeasured. Scheduling noise is substantial (notably current skew:
4785 mean versus 2645 best iteration). These are observations, not controlled
speedup claims; instruction profiles provide the stronger conversion evidence.

| Scenario | glyphrast before | glyphrast after | FreeType now |
| --- | ---: | ---: | ---: |
| Alpha 10px | 2152 | 2521 | 3844 |
| Alpha 16px | 2733 | 3234 | 3353 |
| Alpha 24px | 3702 | 3957 | 4001 |
| Alpha 48px | 6183 | 5704 | 5941 |
| Subpixel positions 16px | 2870 | 3100 | 3801 |
| Bold | 3348 | 3348 | 5937 |
| Italics | 2973 | 2891 | 3019 |
| Rotation | 3246 | 3414 | 2826 |
| Skew | 3249 | 4785 | 2494 |
| Mono | 2979 | 2924 | 5011 |
| Subpixel AA | 6672 | 6550 | 6612 |
| Proggy bitmap | 706 | 571 | 171 |
| Dimensions 16px | 2725 | 563 | 1523 |
| Fresh context | 3197 | 3847 | 3531 |

The previous vello_cpu dimension measurement was 769 ns. Bitmap and Mono
remain unequal-work comparisons, as described above; no optional CJK or color
fixtures were added. Vello_cpu was not rebuilt for this follow-up.

Four-worker glyph_bench_mt, five repeats, mean milliseconds:

| Size | glyphrast before | glyphrast after | FreeType now |
| --- | ---: | ---: | ---: |
| 10px | 2.26 | 3.06 | 18.09 |
| 16px | 2.70 | 4.00 | 19.24 |
| 24px | 3.74 | 4.27 | 24.18 |
| 48px | 7.31 | 9.22 | 34.51 |

These whole-pipeline runs include worker scheduling and are noisier than the
single-thread microbenchmarks. Raw follow-up logs, probes and Callgrind
profiles from this machine are retained in /home/ubuntu/glyphrast-results/.
Cross-platform builds and color-font visual checks remain unverified.
