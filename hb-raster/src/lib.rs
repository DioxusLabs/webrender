/*
 * Copyright © 2026  Behdad Esfahbod
 *
 *  This is part of HarfBuzz, a text shaping library.
 *
 * Permission is hereby granted, without written agreement and without
 * license or royalty fees, to use, copy, modify, and distribute this
 * software and its documentation for any purpose, provided that the
 * above copyright notice and the following two paragraphs appear in
 * all copies of this software.
 *
 * IN NO EVENT SHALL THE COPYRIGHT HOLDER BE LIABLE TO ANY PARTY FOR
 * DIRECT, INDIRECT, SPECIAL, INCIDENTAL, OR CONSEQUENTIAL DAMAGES
 * ARISING OUT OF THE USE OF THIS SOFTWARE AND ITS DOCUMENTATION, EVEN
 * IF THE COPYRIGHT HOLDER HAS BEEN ADVISED OF THE POSSIBILITY OF SUCH
 * DAMAGE.
 *
 * THE COPYRIGHT HOLDER SPECIFICALLY DISCLAIMS ANY WARRANTIES, INCLUDING,
 * BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND
 * FITNESS FOR A PARTICULAR PURPOSE.  THE SOFTWARE PROVIDED HEREUNDER IS
 * ON AN "AS IS" BASIS, AND THE COPYRIGHT HOLDER HAS NO OBLIGATION TO
 * PROVIDE MAINTENANCE, SUPPORT, UPDATES, ENHANCEMENTS, OR MODIFICATIONS.
 *
 * Author(s): Behdad Esfahbod
 */

//! Rust port of HarfBuzz's `hb-raster-draw.cc` (author: Behdad Esfahbod):
//! an analytic coverage rasterizer that turns filled outlines into 8-bit
//! alpha coverage.
//!
//! Differences from the C++ original: there is no transform or scale
//! factor (coordinates are taken in device space), no reference counting
//! or image objects (the caller supplies the output buffer and stride),
//! and only the non-zero winding `hb_raster_draw_t` rasterizer is ported.

/// Fixed-point precision for sub-pixel coordinates.
/// 8 bits = 24.8: 256 sub-pixel units per pixel.
const PIXEL_BITS: i32 = 8;
const ONE_PIXEL: i32 = 1 << PIXEL_BITS;
const PIXEL_MASK: i32 = ONE_PIXEL - 1;
/// Full-coverage alpha = 2 * ONE_PIXEL^2.
const FULL_COVERAGE: i32 = 2 * ONE_PIXEL * ONE_PIXEL;

/// Maximum Bézier flattening subdivision depth.
const MAX_DEPTH: i32 = 16;

/// Output extents, in whole pixels.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Extents {
    /// X origin of the top-left pixel of the output.
    pub x0: i32,
    /// Y origin of the top-left pixel of the output.
    pub y0: i32,
    pub width: u32,
    pub height: u32,
}

/// Normalized edge: `yh > yl` always.
#[derive(Copy, Clone, Debug)]
struct Edge {
    /// Lower endpoint (fixed point).
    xl: i32,
    yl: i32,
    /// Upper endpoint (fixed point).
    xh: i32,
    yh: i32,
    /// dx/dy in 16.16 fixed point.
    slope: i64,
    /// +1 or -1.
    wind: i32,
}

/// An analytic coverage rasterizer.
///
/// Feed it a path with [`Raster::move_to`] and friends (in device-space
/// pixel coordinates), then call [`Raster::render`] to produce A8
/// coverage. Rendering clears the accumulated geometry, so a single
/// `Raster` can be reused for many paths without reallocating.
pub struct Raster {
    edges: Vec<Edge>,
    row_area: Vec<i32>,
    row_cover: Vec<i16>,
    edge_buckets: Vec<Vec<u32>>,
    active_edges: Vec<u32>,
    /// Current point of the path being accumulated.
    current: (f32, f32),
    /// First point of the current contour.
    start: (f32, f32),
}

impl Default for Raster {
    fn default() -> Self {
        Self::new()
    }
}

impl Raster {
    pub fn new() -> Self {
        Raster {
            edges: Vec::new(),
            row_area: Vec::new(),
            row_cover: Vec::new(),
            edge_buckets: Vec::new(),
            active_edges: Vec::new(),
            current: (0.0, 0.0),
            start: (0.0, 0.0),
        }
    }

    /// Discard all accumulated geometry.
    pub fn clear(&mut self) {
        self.edges.clear();
        self.active_edges.clear();
        self.current = (0.0, 0.0);
        self.start = (0.0, 0.0);
    }

    #[inline]
    pub fn move_to(&mut self, x: f32, y: f32) {
        // Implicitly close the previous contour: filling is defined on
        // closed contours only.
        self.close_path();
        self.current = (x, y);
        self.start = (x, y);
    }

    #[inline]
    pub fn line_to(&mut self, x: f32, y: f32) {
        let (x0, y0) = self.current;
        self.emit_segment(x0, y0, x, y);
        self.current = (x, y);
    }

    #[inline]
    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let (x0, y0) = self.current;
        self.flatten_quadratic(x0, y0, cx, cy, x, y);
        self.current = (x, y);
    }

    #[inline]
    pub fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let (x0, y0) = self.current;
        self.flatten_cubic(x0, y0, cx0, cy0, cx1, cy1, x, y);
        self.current = (x, y);
    }

    /// Close the current contour with a straight line back to its start.
    /// This is a no-op if the contour is already closed.
    #[inline]
    pub fn close_path(&mut self) {
        let (x0, y0) = self.current;
        let (sx, sy) = self.start;
        if x0 != sx || y0 != sy {
            self.emit_segment(x0, y0, sx, sy);
            self.current = self.start;
        }
    }

    fn emit_segment(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        let bx0 = (x0 * ONE_PIXEL as f32).round() as i32;
        let by0 = (y0 * ONE_PIXEL as f32).round() as i32;
        let bx1 = (x1 * ONE_PIXEL as f32).round() as i32;
        let by1 = (y1 * ONE_PIXEL as f32).round() as i32;

        if by0 == by1 {
            // Horizontal — skip.
            return;
        }

        let (xl, yl, xh, yh, wind) = if by0 < by1 {
            (bx0, by0, bx1, by1, 1)
        } else {
            (bx1, by1, bx0, by0, -1)
        };
        let slope = ((xh as i64 - xl as i64) * 65536) / (yh as i64 - yl as i64);

        self.edges.push(Edge {
            xl,
            yl,
            xh,
            yh,
            slope,
            wind,
        });
    }

    /// Quadratic Bézier flattener — iterative de Casteljau at t=0.5, with
    /// FreeType's control-point deviation flatness test.
    fn flatten_quadratic(
        &mut self,
        mut x0: f32,
        mut y0: f32,
        mut x1: f32,
        mut y1: f32,
        mut x2: f32,
        mut y2: f32,
    ) {
        // Depth is capped at MAX_DEPTH, so this capacity is sufficient.
        let mut stack = [(0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32, 0i32); MAX_DEPTH as usize];
        let mut top = 0usize;
        let mut depth = 0i32;

        loop {
            const FLAT_THRESH: f32 = 0.25;
            let dx = (x0 + x2 - 2.0 * x1).abs();
            let dy = (y0 + y2 - 2.0 * y1).abs();
            let is_flat = dx <= FLAT_THRESH && dy <= FLAT_THRESH;

            if depth >= MAX_DEPTH || is_flat {
                self.emit_segment(x0, y0, x2, y2);
                if top == 0 {
                    return;
                }
                top -= 1;
                let n = stack[top];
                x0 = n.0;
                y0 = n.1;
                x1 = n.2;
                y1 = n.3;
                x2 = n.4;
                y2 = n.5;
                depth = n.6;
                continue;
            }

            let (x01, y01) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
            let (x12, y12) = ((x1 + x2) * 0.5, (y1 + y2) * 0.5);
            let (xm, ym) = ((x01 + x12) * 0.5, (y01 + y12) * 0.5);

            stack[top] = (xm, ym, x12, y12, x2, y2, depth + 1);
            top += 1;
            x2 = xm;
            y2 = ym;
            x1 = x01;
            y1 = y01;
            depth += 1;
        }
    }

    /// Cubic Bézier flattener — iterative de Casteljau at t=0.5, with
    /// FreeType's chord-trisection distance flatness test.
    fn flatten_cubic(
        &mut self,
        mut x0: f32,
        mut y0: f32,
        mut x1: f32,
        mut y1: f32,
        mut x2: f32,
        mut y2: f32,
        mut x3: f32,
        mut y3: f32,
    ) {
        #[allow(clippy::type_complexity)]
        let mut stack = [(
            0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32, 0i32,
        ); MAX_DEPTH as usize];
        let mut top = 0usize;
        let mut depth = 0i32;

        loop {
            const FLAT_THRESH: f32 = 0.5;
            let d10x = (2.0 * x0 - 3.0 * x1 + x3).abs();
            let d10y = (2.0 * y0 - 3.0 * y1 + y3).abs();
            let d20x = (x0 - 3.0 * x2 + 2.0 * x3).abs();
            let d20y = (y0 - 3.0 * y2 + 2.0 * y3).abs();
            let is_flat = d10x <= FLAT_THRESH &&
                d10y <= FLAT_THRESH &&
                d20x <= FLAT_THRESH &&
                d20y <= FLAT_THRESH;

            if depth >= MAX_DEPTH || is_flat {
                self.emit_segment(x0, y0, x3, y3);
                if top == 0 {
                    return;
                }
                top -= 1;
                let n = stack[top];
                x0 = n.0;
                y0 = n.1;
                x1 = n.2;
                y1 = n.3;
                x2 = n.4;
                y2 = n.5;
                x3 = n.6;
                y3 = n.7;
                depth = n.8;
                continue;
            }

            let (x01, y01) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
            let (x12, y12) = ((x1 + x2) * 0.5, (y1 + y2) * 0.5);
            let (x23, y23) = ((x2 + x3) * 0.5, (y2 + y3) * 0.5);
            let (x012, y012) = ((x01 + x12) * 0.5, (y01 + y12) * 0.5);
            let (x123, y123) = ((x12 + x23) * 0.5, (y12 + y23) * 0.5);
            let (xm, ym) = ((x012 + x123) * 0.5, (y012 + y123) * 0.5);

            stack[top] = (xm, ym, x123, y123, x23, y23, x3, y3, depth + 1);
            top += 1;
            x3 = xm;
            y3 = ym;
            x2 = x012;
            y2 = y012;
            x1 = x01;
            y1 = y01;
            depth += 1;
        }
    }

    /// The pixel extents of the accumulated geometry, or an empty extents
    /// if no geometry has been accumulated.
    pub fn extents(&self) -> Extents {
        if self.edges.is_empty() {
            return Extents::default();
        }
        let mut xmin = self.edges[0].xl;
        let mut xmax = self.edges[0].xl;
        let mut ymin = self.edges[0].yl;
        let mut ymax = self.edges[0].yh;
        for e in &self.edges {
            xmin = xmin.min(e.xl.min(e.xh));
            xmax = xmax.max(e.xl.max(e.xh));
            ymin = ymin.min(e.yl);
            ymax = ymax.max(e.yh);
        }

        // Convert fixed-point → pixels (floor for min, ceil for max). Edge
        // coordinates are saturated to i32 range in emit_segment, so the
        // ceil step is widened to avoid signed overflow.
        let x0 = xmin >> PIXEL_BITS;
        let y0 = ymin >> PIXEL_BITS;
        let x1 = ((xmax as i64 + PIXEL_MASK as i64) >> PIXEL_BITS) as i32;
        let y1 = ((ymax as i64 + PIXEL_MASK as i64) >> PIXEL_BITS) as i32;

        Extents {
            x0,
            y0,
            width: (x1 - x0).max(0) as u32,
            height: (y1 - y0).max(0) as u32,
        }
    }

    /// Rasterize the accumulated geometry into `out` as 8-bit alpha
    /// coverage with the given row `stride`, then clear the geometry so
    /// this rasterizer can be reused.
    ///
    /// `out` must hold at least `stride * extents.height` bytes and is
    /// fully overwritten (including any padding implied by `stride`).
    pub fn render(&mut self, extents: Extents, stride: usize, out: &mut [u8]) {
        // Any unclosed contour is implicitly closed for filling.
        self.close_path();

        let width = extents.width as usize;
        let height = extents.height as usize;
        assert!(stride >= width);
        let out = &mut out[.. stride * height];
        for b in out.iter_mut() {
            *b = 0;
        }

        if self.edges.is_empty() || width == 0 || height == 0 {
            self.clear();
            return;
        }

        self.row_area.clear();
        self.row_area.resize(width, 0);
        self.row_cover.clear();
        self.row_cover.resize(width, 0);

        // Bucket edges by their starting pixel row. Only grow the outer
        // vector; clear inner vectors without freeing their storage.
        let old_buckets = self.edge_buckets.len();
        if height > old_buckets {
            self.edge_buckets.resize_with(height, Vec::new);
        }
        for bucket in self.edge_buckets[.. old_buckets.min(height)].iter_mut() {
            bucket.clear();
        }

        for (i, edge) in self.edges.iter().enumerate() {
            let mut row = (edge.yl >> PIXEL_BITS) - extents.y0;
            if row < 0 {
                row = 0;
            }
            if row as usize >= height {
                continue;
            }
            self.edge_buckets[row as usize].push(i as u32);
        }

        // Scanline loop with active edge list.
        self.active_edges.clear();

        for row in 0 .. height {
            let y_top = clamp_i64_to_i32((extents.y0 as i64 + row as i64) * ONE_PIXEL as i64);

            // Add new edges from this row's bucket.
            self.active_edges.extend_from_slice(&self.edge_buckets[row]);

            // Process active edges and compact live ones in one linear pass.
            let mut x_min = width;
            let mut x_max = 0usize;
            let mut write = 0usize;
            for j in 0 .. self.active_edges.len() {
                let edge_idx = self.active_edges[j];
                let e = self.edges[edge_idx as usize];
                if e.yh <= y_top {
                    continue;
                }

                edge_sweep_row(
                    &mut self.row_area,
                    &mut self.row_cover,
                    extents.x0,
                    y_top,
                    &e,
                    &mut x_min,
                    &mut x_max,
                );
                self.active_edges[write] = edge_idx;
                write += 1;
            }
            self.active_edges.truncate(write);

            if x_min <= x_max {
                let row_buf = &mut out[row * stride .. row * stride + width];
                let cover_accum = sweep_row_to_alpha(
                    row_buf,
                    &mut self.row_area,
                    &mut self.row_cover,
                    x_min,
                    x_max,
                );

                // If cover doesn't cancel, fill the constant-alpha tail.
                if cover_accum != 0 {
                    let mut alpha = (cover_accum * (2 * ONE_PIXEL)).abs();
                    if alpha > FULL_COVERAGE {
                        alpha = FULL_COVERAGE;
                    }
                    let byte = alpha_to_byte(alpha);
                    for b in row_buf[x_max + 1 ..].iter_mut() {
                        *b = byte;
                    }
                }
            }
        }

        self.clear();
    }
}

#[inline]
fn clamp_i64_to_i32(v: i64) -> i32 {
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

#[inline]
fn alpha_to_byte(alpha: i32) -> u8 {
    ((alpha as u32 * 255 + FULL_COVERAGE as u32 / 2) >> (2 * PIXEL_BITS + 1)) as u8
}

/// Add one edge piece's area/cover into a single cell.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn cell_add(
    area: &mut [i32],
    cover: &mut [i16],
    col: i32,
    fx0: i32,
    fy0: i32,
    fx1: i32,
    fy1: i32,
    wind: i32,
    x_min: &mut usize,
    x_max: &mut usize,
) {
    let width = area.len();
    if col < 0 || col as usize >= width {
        if col < 0 {
            // Edge is to the left of the surface. The winding contribution
            // still carries into the visible region, so add the cover delta
            // to column 0. Area is not added since the edge doesn't cross
            // column 0's cell.
            let dy = fy1 - fy0;
            cover[0] = cover[0].wrapping_add((dy * wind) as i16);
            *x_min = 0;
        }
        return;
    }
    let col = col as usize;
    let dy = fy1 - fy0;
    area[col] += (fx0 + fx1) * dy * wind;
    cover[col] = cover[col].wrapping_add((dy * wind) as i16);
    *x_min = (*x_min).min(col);
    *x_max = (*x_max).max(col);
}

/// Walk one edge through the pixel cells of a single pixel row,
/// accumulating area/cover. `y_top` is the row's top edge in fixed point.
fn edge_sweep_row(
    area: &mut [i32],
    cover: &mut [i16],
    x_org: i32,
    y_top: i32,
    edge: &Edge,
    x_min: &mut usize,
    x_max: &mut usize,
) {
    let y_bot = y_top + ONE_PIXEL;

    let ey0 = edge.yl.max(y_top);
    let ey1 = edge.yh.min(y_bot);
    if ey0 >= ey1 {
        return;
    }

    // X at clipped endpoints (fixed-point). Keep the interpolation in
    // 64-bit so extreme y values do not overflow before the slope multiply.
    let x0_64 = edge.xl as i64 + (((ey0 as i64 - edge.yl as i64) * edge.slope) >> 16);
    let x1_64 = edge.xl as i64 + (((ey1 as i64 - edge.yl as i64) * edge.slope) >> 16);
    let x0 = clamp_i64_to_i32(x0_64);
    let x1 = clamp_i64_to_i32(x1_64);

    // Fractional y within this pixel row, in [0, ONE_PIXEL].
    let fy0 = ey0 - y_top;
    let fy1 = ey1 - y_top;

    let cx0 = x0 >> PIXEL_BITS;
    let fx0 = x0 & PIXEL_MASK;
    let cx1 = x1 >> PIXEL_BITS;
    let fx1 = x1 & PIXEL_MASK;
    let wind = edge.wind;

    // Fast path: both endpoints in the same pixel column.
    if cx0 == cx1 {
        cell_add(
            area,
            cover,
            cx0 - x_org,
            fx0,
            fy0,
            fx1,
            fy1,
            wind,
            x_min,
            x_max,
        );
        return;
    }

    let total_dx = x1 as i64 - x0 as i64;
    let total_dy = fy1 as i64 - fy0 as i64;

    // fy increment per pixel column (constant since x_b advances by ONE_PIXEL).
    let delta_fy = (ONE_PIXEL as i64 * total_dy / total_dx) as i32;

    if total_dx > 0 {
        // Left-to-right edge.
        let x_b = clamp_i64_to_i32((cx0 as i64 + 1) * ONE_PIXEL as i64);
        let mut fy_b = fy0 + (((x_b as i64 - x0 as i64) * total_dy) / total_dx) as i32;
        cell_add(
            area, cover, cx0 - x_org, fx0, fy0, ONE_PIXEL, fy_b, wind, x_min, x_max,
        );

        let mut fy_prev = fy_b;
        let mut cx = cx0 + 1;
        while cx < cx1 {
            fy_b = fy_prev + delta_fy;
            cell_add(
                area, cover, cx - x_org, 0, fy_prev, ONE_PIXEL, fy_b, wind, x_min, x_max,
            );
            fy_prev = fy_b;
            cx += 1;
        }

        cell_add(
            area, cover, cx1 - x_org, 0, fy_prev, fx1, fy1, wind, x_min, x_max,
        );
    } else {
        // Right-to-left edge.
        let x_b = clamp_i64_to_i32(cx0 as i64 * ONE_PIXEL as i64);
        let mut fy_b = fy0 + (((x_b as i64 - x0 as i64) * total_dy) / total_dx) as i32;
        cell_add(
            area, cover, cx0 - x_org, fx0, fy0, 0, fy_b, wind, x_min, x_max,
        );

        let mut fy_prev = fy_b;
        let mut cx = cx0 - 1;
        while cx > cx1 {
            fy_b = fy_prev - delta_fy;
            cell_add(
                area, cover, cx - x_org, ONE_PIXEL, fy_prev, 0, fy_b, wind, x_min, x_max,
            );
            fy_prev = fy_b;
            cx -= 1;
        }

        cell_add(
            area, cover, cx1 - x_org, ONE_PIXEL, fy_prev, fx1, fy1, wind, x_min, x_max,
        );
    }
}

/// Convert cover-delta + area to alpha bytes, then clear them.
/// Returns the final cover accumulator over `[x_min, x_max]`.
fn sweep_row_to_alpha(
    row_buf: &mut [u8],
    area: &mut [i32],
    cover: &mut [i16],
    x_min: usize,
    x_max: usize,
) -> i32 {
    const COVER_SCALE: i32 = 2 * ONE_PIXEL;
    let mut cover_accum = 0i32;

    for x in x_min ..= x_max {
        cover_accum += cover[x] as i32;
        let val = cover_accum * COVER_SCALE - area[x];
        let mut alpha = val.abs();
        if alpha > FULL_COVERAGE {
            alpha = FULL_COVERAGE;
        }
        row_buf[x] = alpha_to_byte(alpha);
        area[x] = 0;
        cover[x] = 0;
    }

    cover_accum
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_rect(x0: f32, y0: f32, x1: f32, y1: f32, extents: Extents) -> Vec<u8> {
        let mut r = Raster::new();
        r.move_to(x0, y0);
        r.line_to(x1, y0);
        r.line_to(x1, y1);
        r.line_to(x0, y1);
        r.close_path();
        let stride = extents.width as usize;
        let mut out = vec![0u8; stride * extents.height as usize];
        r.render(extents, stride, &mut out);
        out
    }

    #[test]
    fn pixel_aligned_rect_is_solid() {
        let ext = Extents { x0: 0, y0: 0, width: 4, height: 4 };
        let out = render_rect(1.0, 1.0, 3.0, 3.0, ext);
        let expected: Vec<u8> = vec![
            0, 0, 0, 0, //
            0, 255, 255, 0, //
            0, 255, 255, 0, //
            0, 0, 0, 0,
        ];
        assert_eq!(out, expected);
    }

    #[test]
    fn fractional_rect_has_proportional_edges() {
        // A rect from (0.5, 0.5) to (2.5, 2.5): corners get 25% coverage,
        // edges 50%, the single interior pixel full coverage.
        let ext = Extents { x0: 0, y0: 0, width: 3, height: 3 };
        let out = render_rect(0.5, 0.5, 2.5, 2.5, ext);
        let q = 64; // 0.25 * 255 rounded
        let h = 128; // 0.5 * 255 rounded
        let expected: Vec<u8> = vec![q, h, q, h, 255, h, q, h, q];
        assert_eq!(out, expected);
    }

    #[test]
    fn empty_path_is_all_zero() {
        let mut r = Raster::new();
        let ext = Extents { x0: 0, y0: 0, width: 4, height: 4 };
        let mut out = vec![0xAAu8; 16];
        r.render(ext, 4, &mut out);
        assert!(out.iter().all(|&b| b == 0));
    }

    #[test]
    fn horizontal_and_degenerate_paths_do_not_panic() {
        let mut r = Raster::new();
        r.move_to(0.0, 0.0);
        r.line_to(4.0, 0.0);
        r.line_to(0.0, 0.0);
        r.close_path();
        // A single point.
        r.move_to(2.0, 2.0);
        r.close_path();
        let ext = Extents { x0: 0, y0: 0, width: 4, height: 4 };
        let mut out = vec![0u8; 16];
        r.render(ext, 4, &mut out);
        assert!(out.iter().all(|&b| b == 0));
    }

    #[test]
    fn geometry_outside_extents_is_clipped() {
        // Rect extends left of and above the surface; the left-of-surface
        // winding must still fill the visible part.
        let ext = Extents { x0: 0, y0: 0, width: 4, height: 4 };
        let out = render_rect(-10.0, -10.0, 2.0, 2.0, ext);
        let expected: Vec<u8> = vec![
            255, 255, 0, 0, //
            255, 255, 0, 0, //
            0, 0, 0, 0, //
            0, 0, 0, 0,
        ];
        assert_eq!(out, expected);
    }

    #[test]
    fn nonzero_winding_overlap_stays_opaque() {
        // Two overlapping same-direction squares: the overlap must not
        // become double-covered (clamped) nor cancel out.
        let mut r = Raster::new();
        for off in [0.0f32, 1.0] {
            r.move_to(off, off);
            r.line_to(off + 3.0, off);
            r.line_to(off + 3.0, off + 3.0);
            r.line_to(off, off + 3.0);
            r.close_path();
        }
        let ext = Extents { x0: 0, y0: 0, width: 4, height: 4 };
        let mut out = vec![0u8; 16];
        r.render(ext, 4, &mut out);
        let expected: Vec<u8> = vec![
            255, 255, 255, 0, //
            255, 255, 255, 255, //
            255, 255, 255, 255, //
            0, 255, 255, 255,
        ];
        assert_eq!(out, expected);
    }

    #[test]
    fn opposite_winding_hole() {
        // Outer square clockwise, inner square counter-clockwise: a hole.
        let mut r = Raster::new();
        r.move_to(0.0, 0.0);
        r.line_to(4.0, 0.0);
        r.line_to(4.0, 4.0);
        r.line_to(0.0, 4.0);
        r.close_path();
        r.move_to(1.0, 1.0);
        r.line_to(1.0, 3.0);
        r.line_to(3.0, 3.0);
        r.line_to(3.0, 1.0);
        r.close_path();
        let ext = Extents { x0: 0, y0: 0, width: 4, height: 4 };
        let mut out = vec![0u8; 16];
        r.render(ext, 4, &mut out);
        let expected: Vec<u8> = vec![
            255, 255, 255, 255, //
            255, 0, 0, 255, //
            255, 0, 0, 255, //
            255, 255, 255, 255,
        ];
        assert_eq!(out, expected);
    }

    #[test]
    fn triangle_matches_analytic_coverage() {
        // Right triangle with vertices (0,0), (8,0), (0,8): the diagonal
        // cells have ~50% coverage and total coverage is half the square.
        let mut r = Raster::new();
        r.move_to(0.0, 0.0);
        r.line_to(8.0, 0.0);
        r.line_to(0.0, 8.0);
        r.close_path();
        let ext = Extents { x0: 0, y0: 0, width: 8, height: 8 };
        let mut out = vec![0u8; 64];
        r.render(ext, 8, &mut out);

        let total: u32 = out.iter().map(|&b| b as u32).sum();
        let expected = 0.5 * 8.0 * 8.0 * 255.0;
        assert!(
            (total as f32 - expected).abs() < 255.0 * 1.0,
            "total coverage {} vs expected {}",
            total,
            expected
        );
        // Diagonal cells are half covered.
        for i in 0 .. 8usize {
            let v = out[i * 8 + (7 - i)] as i32;
            assert!((v - 128).abs() <= 2, "diagonal pixel {} = {}", i, v);
        }
    }

    #[test]
    fn circle_matches_analytic_area() {
        // Approximate a circle of radius 8 centred at (10, 10) with four
        // cubic segments and compare total coverage against pi*r^2.
        const K: f32 = 0.552_284_75;
        let (cx, cy, r0) = (10.0f32, 10.0f32, 8.0f32);
        let k = K * r0;
        let mut r = Raster::new();
        r.move_to(cx, cy - r0);
        r.curve_to(cx + k, cy - r0, cx + r0, cy - k, cx + r0, cy);
        r.curve_to(cx + r0, cy + k, cx + k, cy + r0, cx, cy + r0);
        r.curve_to(cx - k, cy + r0, cx - r0, cy + k, cx - r0, cy);
        r.curve_to(cx - r0, cy - k, cx - k, cy - r0, cx, cy - r0);
        r.close_path();

        let ext = Extents { x0: 0, y0: 0, width: 20, height: 20 };
        let mut out = vec![0u8; 400];
        r.render(ext, 20, &mut out);

        let total: f32 = out.iter().map(|&b| b as f32).sum::<f32>() / 255.0;
        let expected = std::f32::consts::PI * r0 * r0;
        assert!(
            // The tolerance accommodates the flattening error of the
            // FreeType-style 0.5px chord test.
            (total - expected).abs() / expected < 0.02,
            "circle area {} vs expected {}",
            total,
            expected
        );
        // Centre is fully covered, corners are empty.
        assert_eq!(out[10 * 20 + 10], 255);
        assert_eq!(out[0], 0);
    }

    #[test]
    fn auto_extents_bound_the_geometry() {
        let mut r = Raster::new();
        r.move_to(1.25, 2.5);
        r.line_to(5.5, 2.5);
        r.line_to(5.5, 7.75);
        r.close_path();
        assert_eq!(
            r.extents(),
            Extents { x0: 1, y0: 2, width: 5, height: 6 }
        );
    }

    #[test]
    fn offset_extents_translate_the_output() {
        // Rendering with a non-zero extents origin shifts the geometry.
        let ext = Extents { x0: 2, y0: 3, width: 4, height: 4 };
        let out = render_rect(3.0, 4.0, 5.0, 6.0, ext);
        let expected: Vec<u8> = vec![
            0, 0, 0, 0, //
            0, 255, 255, 0, //
            0, 255, 255, 0, //
            0, 0, 0, 0,
        ];
        assert_eq!(out, expected);
    }

    #[test]
    fn stride_larger_than_width_is_respected() {
        let ext = Extents { x0: 0, y0: 0, width: 2, height: 2 };
        let mut r = Raster::new();
        r.move_to(0.0, 0.0);
        r.line_to(2.0, 0.0);
        r.line_to(2.0, 2.0);
        r.line_to(0.0, 2.0);
        r.close_path();
        let mut out = vec![0xAAu8; 8];
        r.render(ext, 4, &mut out);
        assert_eq!(out, vec![255, 255, 0, 0, 255, 255, 0, 0]);
    }

    #[test]
    fn raster_is_reusable() {
        let ext = Extents { x0: 0, y0: 0, width: 4, height: 4 };
        let mut r = Raster::new();
        let mut out = vec![0u8; 16];
        for _ in 0 .. 3 {
            r.move_to(1.0, 1.0);
            r.line_to(3.0, 1.0);
            r.line_to(3.0, 3.0);
            r.line_to(1.0, 3.0);
            r.close_path();
            r.render(ext, 4, &mut out);
            assert_eq!(out[1 * 4 + 1], 255);
            assert_eq!(out[0], 0);
        }
    }
}
