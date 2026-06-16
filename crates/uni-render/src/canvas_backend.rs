//! Canvas2D software rasterizer — used by the wasm target and for pixel-level tests.
//!
//! Implements [`Renderer`] by painting [`DrawCmd`]s into an in-memory RGBA buffer.
//! Good enough for CI screenshot tests and as the foundation for the wasm canvas backend.

use crate::scene::{Fill, PathOp, Shape};
use crate::{DrawCmd, RenderError, Renderer, Scene};

pub struct CanvasRenderer {
    width: u32,
    height: u32,
    /// RGBA pixels, row-major, top-down.
    pub pixels: Vec<u8>,
}

impl CanvasRenderer {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0u8; (width * height * 4) as usize],
        }
    }

    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: u32) {
        let a = (color & 0xFF) as u8;
        // Fully transparent source contributes nothing.
        if a == 0 {
            return;
        }
        let alpha = a as f32 / 255.0;

        // Convert the source color to LINEAR light once (uni-simd runtime
        // dispatch: AVX-512 → AVX2 → SSE2 → NEON → scalar, same binary). Linear
        // compositing is the correct space to blend in — sRGB blending darkens
        // edges. The source is one pixel; dest is converted per row below.
        let mut src_lin = [0.0f32; 4];
        uni_simd::srgb_to_linear_u32(&[color], &mut src_lin);

        let x0 = x.max(0.0) as i32;
        let y0 = y.max(0.0) as i32;
        let x1 = ((x + w) as i32).min(self.width as i32);
        let y1 = ((y + h) as i32).min(self.height as i32);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let row_len = (x1 - x0) as usize;

        // Scratch buffers reused across rows.
        let mut row_u32 = vec![0u32; row_len];
        let mut dst_lin = vec![0.0f32; row_len * 4];

        for py in y0..y1 {
            let base = ((py as u32 * self.width + x0 as u32) * 4) as usize;
            // Gather this row's destination pixels as 0xRRGGBBAA.
            for (i, slot) in row_u32.iter_mut().enumerate() {
                let p = base + i * 4;
                *slot = ((self.pixels[p] as u32) << 24)
                    | ((self.pixels[p + 1] as u32) << 16)
                    | ((self.pixels[p + 2] as u32) << 8)
                    | (self.pixels[p + 3] as u32);
            }
            // Destination → linear (SIMD batch over the whole row).
            uni_simd::srgb_to_linear_u32(&row_u32, &mut dst_lin);

            // Source-over composite in linear light, then convert back.
            for chunk in dst_lin.chunks_exact_mut(4) {
                chunk[0] = src_lin[0] * alpha + chunk[0] * (1.0 - alpha);
                chunk[1] = src_lin[1] * alpha + chunk[1] * (1.0 - alpha);
                chunk[2] = src_lin[2] * alpha + chunk[2] * (1.0 - alpha);
                chunk[3] = alpha + chunk[3] * (1.0 - alpha); // alpha is linear
            }
            uni_simd::linear_to_srgb_u32(&dst_lin, &mut row_u32);

            // Scatter back to the RGBA byte buffer.
            for (i, packed) in row_u32.iter().enumerate() {
                let p = base + i * 4;
                self.pixels[p] = ((*packed >> 24) & 0xFF) as u8;
                self.pixels[p + 1] = ((*packed >> 16) & 0xFF) as u8;
                self.pixels[p + 2] = ((*packed >> 8) & 0xFF) as u8;
                self.pixels[p + 3] = (*packed & 0xFF) as u8;
            }
        }
    }

    /// Composite a single source-over pixel at `(px, py)` with packed color.
    fn blend_pixel(&mut self, px: i32, py: i32, color: u32) {
        if px < 0 || py < 0 || px >= self.width as i32 || py >= self.height as i32 {
            return;
        }
        let a = (color & 0xFF) as f32 / 255.0;
        if a <= 0.0 {
            return;
        }
        let mut src = [0.0f32; 4];
        uni_simd::srgb_to_linear_u32(&[color], &mut src);
        let base = ((py as u32 * self.width + px as u32) * 4) as usize;
        let dpacked = ((self.pixels[base] as u32) << 24)
            | ((self.pixels[base + 1] as u32) << 16)
            | ((self.pixels[base + 2] as u32) << 8)
            | (self.pixels[base + 3] as u32);
        let mut dst = [0.0f32; 4];
        uni_simd::srgb_to_linear_u32(&[dpacked], &mut dst);
        for k in 0..3 {
            dst[k] = src[k] * a + dst[k] * (1.0 - a);
        }
        dst[3] = a + dst[3] * (1.0 - a);
        let mut out = [0u32; 1];
        uni_simd::linear_to_srgb_u32(&dst, &mut out);
        let packed = out[0];
        self.pixels[base] = ((packed >> 24) & 0xFF) as u8;
        self.pixels[base + 1] = ((packed >> 16) & 0xFF) as u8;
        self.pixels[base + 2] = ((packed >> 8) & 0xFF) as u8;
        self.pixels[base + 3] = (packed & 0xFF) as u8;
    }

    /// Rasterize a filled [`Shape`] over its frame, sampling `fill` per pixel
    /// (so gradients ramp; solids are constant). Coverage is a hard inside/
    /// outside test — no AA, which is fine for the software/test backend.
    fn fill_shape(&mut self, x: f32, y: f32, w: f32, h: f32, shape: &Shape, fill: &Fill) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let x0 = x.floor().max(0.0) as i32;
        let y0 = y.floor().max(0.0) as i32;
        let x1 = ((x + w).ceil() as i32).min(self.width as i32);
        let y1 = ((y + h).ceil() as i32).min(self.height as i32);
        let cx = x + w * 0.5;
        let cy = y + h * 0.5;
        let rx = w * 0.5;
        let ry = h * 0.5;
        let radius = match shape {
            Shape::RoundedRect { radius } => radius.max(0.0).min(w.min(h) / 2.0),
            Shape::Capsule => w.min(h) / 2.0,
            _ => 0.0,
        };
        for py in y0..y1 {
            for px in x0..x1 {
                let fx = px as f32 + 0.5;
                let fy = py as f32 + 0.5;
                let inside = match shape {
                    Shape::Rect => true,
                    Shape::Circle => {
                        let r = rx.min(ry);
                        let dx = fx - cx;
                        let dy = fy - cy;
                        dx * dx + dy * dy <= r * r
                    }
                    Shape::Ellipse => {
                        let dx = (fx - cx) / rx;
                        let dy = (fy - cy) / ry;
                        dx * dx + dy * dy <= 1.0
                    }
                    Shape::RoundedRect { .. } | Shape::Capsule => {
                        rounded_rect_inside(fx, fy, x, y, w, h, radius)
                    }
                };
                if !inside {
                    continue;
                }
                let u = ((fx - x) / w).clamp(0.0, 1.0);
                let v = ((fy - y) / h).clamp(0.0, 1.0);
                let color = fill.sample(u, v);
                self.blend_pixel(px, py, color);
            }
        }
    }

    /// Fill a path (even-odd) and/or stroke its segments. Curves are flattened.
    fn draw_path(
        &mut self,
        ops: &[PathOp],
        fill: &Option<Fill>,
        stroke: &Option<(u32, f32)>,
    ) {
        let polylines = flatten_path(ops);
        if let Some(fill) = fill {
            // Scanline even-odd fill over the union of subpaths.
            let mut min_y = f32::INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            let mut min_x = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            for poly in &polylines {
                for &(px, py) in poly {
                    min_y = min_y.min(py);
                    max_y = max_y.max(py);
                    min_x = min_x.min(px);
                    max_x = max_x.max(px);
                }
            }
            if max_y > min_y && max_x > min_x {
                let span_x = (max_x - min_x).max(f32::EPSILON);
                let span_y = (max_y - min_y).max(f32::EPSILON);
                let ys = min_y.floor().max(0.0) as i32;
                let ye = (max_y.ceil() as i32).min(self.height as i32);
                for py in ys..ye {
                    let sy = py as f32 + 0.5;
                    let mut xs: Vec<f32> = Vec::new();
                    for poly in &polylines {
                        let n = poly.len();
                        if n < 2 {
                            continue;
                        }
                        for i in 0..n {
                            let (ax, ay) = poly[i];
                            let (bx, by) = poly[(i + 1) % n];
                            if (ay <= sy && by > sy) || (by <= sy && ay > sy) {
                                let t = (sy - ay) / (by - ay);
                                xs.push(ax + t * (bx - ax));
                            }
                        }
                    }
                    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    let mut i = 0;
                    while i + 1 < xs.len() {
                        let xa = xs[i].max(0.0);
                        let xb = xs[i + 1].min(self.width as f32);
                        let pxa = xa.floor() as i32;
                        let pxb = xb.ceil() as i32;
                        for px in pxa..pxb {
                            let sx = px as f32 + 0.5;
                            if sx >= xs[i] && sx < xs[i + 1] {
                                let u = ((sx - min_x) / span_x).clamp(0.0, 1.0);
                                let v = ((sy - min_y) / span_y).clamp(0.0, 1.0);
                                let color = fill.sample(u, v);
                                self.blend_pixel(px, py, color);
                            }
                        }
                        i += 2;
                    }
                }
            }
        }
        if let Some((color, width)) = stroke {
            let hw = (width * 0.5).max(0.5);
            for poly in &polylines {
                for w in poly.windows(2) {
                    self.stroke_segment(w[0], w[1], *color, hw);
                }
            }
        }
    }

    /// Draw a thick line segment by stamping squares along it (cheap, no AA).
    fn stroke_segment(&mut self, a: (f32, f32), b: (f32, f32), color: u32, hw: f32) {
        let dx = b.0 - a.0;
        let dy = b.1 - a.1;
        let len = (dx * dx + dy * dy).sqrt().max(1.0);
        let steps = len.ceil() as i32;
        for s in 0..=steps {
            let t = s as f32 / steps as f32;
            let px = a.0 + dx * t;
            let py = a.1 + dy * t;
            let r = hw.ceil() as i32;
            for oy in -r..=r {
                for ox in -r..=r {
                    self.blend_pixel((px + ox as f32) as i32, (py + oy as f32) as i32, color);
                }
            }
        }
    }
}

/// Inside-test for a rounded rect frame at sample `(fx, fy)`.
fn rounded_rect_inside(fx: f32, fy: f32, x: f32, y: f32, w: f32, h: f32, radius: f32) -> bool {
    if radius <= 0.0 {
        return fx >= x && fx <= x + w && fy >= y && fy <= y + h;
    }
    // Distance to the inner box (inset by radius); inside if within radius.
    let inner_min_x = x + radius;
    let inner_max_x = x + w - radius;
    let inner_min_y = y + radius;
    let inner_max_y = y + h - radius;
    let qx = (inner_min_x - fx).max(fx - inner_max_x).max(0.0);
    let qy = (inner_min_y - fy).max(fy - inner_max_y).max(0.0);
    if fx < x || fx > x + w || fy < y || fy > y + h {
        return false;
    }
    qx * qx + qy * qy <= radius * radius
}

/// Flatten a path's ops into one polyline per subpath (quads -> line segments).
fn flatten_path(ops: &[PathOp]) -> Vec<Vec<(f32, f32)>> {
    let mut polys: Vec<Vec<(f32, f32)>> = Vec::new();
    let mut cur: Vec<(f32, f32)> = Vec::new();
    let mut pt = (0.0f32, 0.0f32);
    for op in ops {
        match *op {
            PathOp::MoveTo { x, y } => {
                if cur.len() >= 2 {
                    polys.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
                pt = (x, y);
                cur.push(pt);
            }
            PathOp::LineTo { x, y } => {
                pt = (x, y);
                cur.push(pt);
            }
            PathOp::QuadTo { cx, cy, x, y } => {
                // Flatten the quadratic into fixed segments.
                let steps = 16;
                let (x0, y0) = pt;
                for s in 1..=steps {
                    let t = s as f32 / steps as f32;
                    let mt = 1.0 - t;
                    let qx = mt * mt * x0 + 2.0 * mt * t * cx + t * t * x;
                    let qy = mt * mt * y0 + 2.0 * mt * t * cy + t * t * y;
                    cur.push((qx, qy));
                }
                pt = (x, y);
            }
            PathOp::Close => {
                if cur.len() >= 2 {
                    polys.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
            }
        }
    }
    if cur.len() >= 2 {
        polys.push(cur);
    }
    polys
}

impl Renderer for CanvasRenderer {
    fn resize(&mut self, width: u32, height: u32, _scale: f64) {
        self.width = width;
        self.height = height;
        self.pixels = vec![0u8; (width * height * 4) as usize];
    }

    fn render(&mut self, scene: &Scene) -> Result<(), RenderError> {
        // Clear to transparent black.
        self.pixels.iter_mut().for_each(|p| *p = 0);
        for cmd in scene {
            match cmd {
                DrawCmd::FilledRect {
                    x, y, w, h, color, ..
                } => {
                    self.fill_rect(*x, *y, *w, *h, *color);
                }
                DrawCmd::Text {
                    x,
                    y,
                    content,
                    color,
                    size,
                } => {
                    // In the software backend, draw text as a colored bar
                    // (real text rendering needs cosmic-text; this is the stub).
                    let w = (content.len() as f32 * size * 0.6).max(4.0);
                    let h = *size + 2.0;
                    self.fill_rect(*x, *y, w, h, *color);
                }
                DrawCmd::FrostedRect {
                    x, y, w, h, tint, ..
                } => {
                    // Frosted glass: just fill with semi-transparent tint.
                    let tinted = (*tint & 0xFFFFFF00) | 0xCC;
                    self.fill_rect(*x, *y, *w, *h, tinted);
                }
                DrawCmd::FilledShape {
                    x,
                    y,
                    w,
                    h,
                    shape,
                    fill,
                    ..
                } => {
                    // Rotation is ignored in the software backend (axis-aligned).
                    self.fill_shape(*x, *y, *w, *h, shape, fill);
                }
                DrawCmd::Path { ops, fill, stroke } => {
                    self.draw_path(ops, fill, stroke);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod canvas_tests {
    use super::*;

    #[test]
    fn canvas_renders_filled_rect_into_pixels() {
        let mut r = CanvasRenderer::new(10, 10);
        r.render(&vec![DrawCmd::FilledRect {
            x: 2.0,
            y: 2.0,
            w: 4.0,
            h: 4.0,
            color: 0xFF0000FF, // red, fully opaque
            corner_radius: 0.0,
            rotation: 0.0,
        }])
        .unwrap();
        // Pixel at (3,3) should be red.
        let idx = (3 * 10 + 3) * 4;
        assert_eq!(r.pixels[idx], 255, "R channel");
        assert_eq!(r.pixels[idx + 1], 0, "G channel");
        assert_eq!(r.pixels[idx + 2], 0, "B channel");
    }

    #[test]
    fn canvas_clears_between_renders() {
        let mut r = CanvasRenderer::new(4, 4);
        r.render(&vec![DrawCmd::FilledRect {
            x: 0.0,
            y: 0.0,
            w: 4.0,
            h: 4.0,
            color: 0x00FF00FF,
            corner_radius: 0.0,
            rotation: 0.0,
        }])
        .unwrap();
        r.render(&vec![]).unwrap(); // empty scene
        assert!(r.pixels.iter().all(|&p| p == 0), "pixels should be cleared");
    }

    fn px(r: &CanvasRenderer, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let i = ((y * r.width + x) * 4) as usize;
        (r.pixels[i], r.pixels[i + 1], r.pixels[i + 2], r.pixels[i + 3])
    }

    #[test]
    fn canvas_circle_fills_center_not_corner() {
        let mut r = CanvasRenderer::new(20, 20);
        r.render(&vec![DrawCmd::filled_shape(
            0.0,
            0.0,
            20.0,
            20.0,
            Shape::Circle,
            Fill::Solid(0x00FF00FF),
        )])
        .unwrap();
        // Center is inside the circle -> green.
        assert_eq!(px(&r, 10, 10), (0, 255, 0, 255), "circle center filled");
        // Top-left corner is outside the inscribed circle -> untouched.
        assert_eq!(px(&r, 0, 0), (0, 0, 0, 0), "circle corner empty");
    }

    #[test]
    fn canvas_ellipse_fills_frame_extents() {
        let mut r = CanvasRenderer::new(40, 20);
        r.render(&vec![DrawCmd::filled_shape(
            0.0,
            0.0,
            40.0,
            20.0,
            Shape::Ellipse,
            Fill::Solid(0xFF0000FF),
        )])
        .unwrap();
        // Mid-left edge inside the ellipse; corner outside.
        assert_eq!(px(&r, 1, 10).0, 255, "ellipse mid-left filled red");
        assert_eq!(px(&r, 0, 0), (0, 0, 0, 0), "ellipse corner empty");
    }

    #[test]
    fn canvas_capsule_rounds_short_ends() {
        let mut r = CanvasRenderer::new(40, 20);
        r.render(&vec![DrawCmd::filled_shape(
            0.0,
            0.0,
            40.0,
            20.0,
            Shape::Capsule,
            Fill::Solid(0xFFFFFFFF),
        )])
        .unwrap();
        // Center filled; the extreme top-left corner is rounded away.
        assert_eq!(px(&r, 20, 10).3, 255, "capsule center filled");
        assert_eq!(px(&r, 0, 0), (0, 0, 0, 0), "capsule corner rounded away");
    }

    #[test]
    fn canvas_linear_gradient_ramps_across() {
        let mut r = CanvasRenderer::new(100, 4);
        r.render(&vec![DrawCmd::filled_shape(
            0.0,
            0.0,
            100.0,
            4.0,
            Shape::Rect,
            Fill::Linear {
                start: (0.0, 0.5),
                end: (1.0, 0.5),
                stops: vec![(0.0, 0xFF0000FF), (1.0, 0x0000FFFF)],
            },
        )])
        .unwrap();
        let left_r = px(&r, 1, 2).0;
        let right_r = px(&r, 98, 2).0;
        let left_b = px(&r, 1, 2).2;
        let right_b = px(&r, 98, 2).2;
        // Left end is red-ish, right end is blue-ish.
        assert!(left_r > right_r, "red falls off left->right ({left_r} > {right_r})");
        assert!(right_b > left_b, "blue rises left->right ({right_b} > {left_b})");
    }

    #[test]
    fn canvas_radial_gradient_center_vs_edge() {
        let mut r = CanvasRenderer::new(40, 40);
        r.render(&vec![DrawCmd::filled_shape(
            0.0,
            0.0,
            40.0,
            40.0,
            Shape::Rect,
            Fill::Radial {
                center: (0.5, 0.5),
                start_radius: 0.0,
                end_radius: 0.5,
                stops: vec![(0.0, 0xFFFFFFFF), (1.0, 0x000000FF)],
            },
        )])
        .unwrap();
        // Center is the first stop (white), the frame edge nears the last (black).
        assert!(px(&r, 20, 20).0 > 200, "radial center bright");
        assert!(px(&r, 20, 0).0 < 80, "radial edge dark");
    }

    #[test]
    fn canvas_path_fills_triangle() {
        let mut r = CanvasRenderer::new(20, 20);
        r.render(&vec![DrawCmd::Path {
            ops: vec![
                PathOp::MoveTo { x: 10.0, y: 2.0 },
                PathOp::LineTo { x: 18.0, y: 18.0 },
                PathOp::LineTo { x: 2.0, y: 18.0 },
                PathOp::Close,
            ],
            fill: Some(Fill::Solid(0x00FF00FF)),
            stroke: None,
        }])
        .unwrap();
        // Inside the triangle (lower middle) is filled; outside (top corner) is not.
        assert_eq!(px(&r, 10, 15).1, 255, "triangle interior filled");
        assert_eq!(px(&r, 1, 1), (0, 0, 0, 0), "outside triangle empty");
    }

    #[test]
    fn canvas_path_stroke_paints_outline() {
        let mut r = CanvasRenderer::new(20, 20);
        r.render(&vec![DrawCmd::Path {
            ops: vec![
                PathOp::MoveTo { x: 2.0, y: 10.0 },
                PathOp::LineTo { x: 18.0, y: 10.0 },
            ],
            fill: None,
            stroke: Some((0xFFFFFFFF, 2.0)),
        }])
        .unwrap();
        // A pixel on the stroked line is painted.
        assert!(px(&r, 10, 10).3 > 0, "stroke painted on the line");
    }
}
