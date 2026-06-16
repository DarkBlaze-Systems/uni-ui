//! Canvas2D software rasterizer — used by the wasm target and for pixel-level tests.
//!
//! Implements [`Renderer`] by painting [`DrawCmd`]s into an in-memory RGBA buffer.
//! Good enough for CI screenshot tests and as the foundation for the wasm canvas backend.

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
        }])
        .unwrap();
        r.render(&vec![]).unwrap(); // empty scene
        assert!(r.pixels.iter().all(|&p| p == 0), "pixels should be cleared");
    }
}
