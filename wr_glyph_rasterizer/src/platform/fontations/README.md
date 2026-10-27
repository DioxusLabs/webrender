# Glyphrast backend

The `fontations` feature uses glyphrast at revision
`08e742ba11d6a63971483eaf732dd714fe4a3c90`. Wrench enables this feature on
its WebRender dependency; build it with `cargo build --release --features software`
from `wrench/`.

## Caching

Each worker's `FontContext` owns one persistent glyphrast `RasterContext`
(including its bounded scaler cache and rasterization scratch storage), plus
one reusable `Image`. Registered fonts own a glyphrast/read-fonts `Font`, which
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

Dimension queries currently render into the reusable image to obtain exactly
the same placement as rasterization, including color and bitmap glyphs.
This costs more than the old backend's outline-only metrics path.

## Validation (Linux, software rendering)

`cargo check -p wr_glyph_rasterizer --features fontations` succeeded; the
library tests passed (8 passed, 0 failed).

For `wrench --headless --software reftest reftests/text`:

| Backend | Passing | Failing |
| --- | ---: | ---: |
| glyphrast | 47 | 1 |
| FreeType | 48 | 0 |
| original vello_cpu fontations | 46 | 2 |

Glyphrast's failure is `snap-clip.yaml == snap-clip-ref.yaml`: one pixel,
maximum channel difference 3. Original fontations also fails this comparison
(one pixel, maximum difference 2), and additionally fails
`allow-subpixel.yaml != allow-subpixel-ref.yaml`. No tolerance or rasterization
workaround was added. The clipping difference's underlying cause has not been
isolated to glyphrast.

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
