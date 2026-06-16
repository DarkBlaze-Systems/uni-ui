//! Color helper. Colors in [`crate::DrawCmd`] are packed `u32` as `0xRRGGBBAA`
//! (the same convention CSS hex-with-alpha uses). This module unpacks them into
//! linear/sRGB float components for the GPU.

/// An unpacked, straight-alpha color with sRGB-encoded RGB channels in `0.0..=1.0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    /// Unpack a `0xRRGGBBAA` value into sRGB float components.
    pub const fn from_u32(packed: u32) -> Self {
        let r = ((packed >> 24) & 0xff) as f32 / 255.0;
        let g = ((packed >> 16) & 0xff) as f32 / 255.0;
        let b = ((packed >> 8) & 0xff) as f32 / 255.0;
        let a = (packed & 0xff) as f32 / 255.0;
        Self { r, g, b, a }
    }

    /// As a `wgpu::Color`-style `[f64; 4]` for clear values (sRGB components).
    pub fn to_f64_array(self) -> [f64; 4] {
        [self.r as f64, self.g as f64, self.b as f64, self.a as f64]
    }

    /// Convert an sRGB-encoded channel to linear light.
    pub(crate) fn srgb_to_linear(c: f32) -> f32 {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    /// Linear-light RGBA for writing into vertex buffers that feed an sRGB
    /// surface format (the GPU expects linear values; the surface re-encodes).
    pub fn to_linear_array(self) -> [f32; 4] {
        [
            Self::srgb_to_linear(self.r),
            Self::srgb_to_linear(self.g),
            Self::srgb_to_linear(self.b),
            self.a,
        ]
    }
}
