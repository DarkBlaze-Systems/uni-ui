//! The renderer-agnostic scene description.
//!
//! A [`Scene`] is just an ordered list of [`DrawCmd`]s. Commands paint in
//! order (painter's algorithm): later commands draw on top of earlier ones.
//! This is the lowering target for `uni-core`: a `uni-ir` `Document` becomes a
//! `Vec<DrawCmd>` and that's all a [`crate::Renderer`] ever sees.

/// A single drawing primitive. All coordinates/sizes are **logical pixels**,
/// origin top-left, y increasing downward. Colors are packed `0xRRGGBBAA`.
#[derive(Clone, Debug, PartialEq)]
pub enum DrawCmd {
    /// A solid-filled rectangle with optional uniform corner rounding.
    FilledRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        /// Fill color, packed `0xRRGGBBAA`.
        color: u32,
        /// Corner radius in logical pixels. `0.0` == sharp corners. Clamped to
        /// half the shorter side by the backend.
        corner_radius: f32,
    },
    /// A **frosted-glass panel**: a rounded rectangle whose interior shows the
    /// scene drawn *before* it, blurred (a real backdrop blur), with a
    /// translucent `tint` laid over the blur and a subtle 1px light inner edge.
    ///
    /// Painter's order holds: a `FrostedRect` blurs only what was drawn before
    /// it in the [`Scene`]; commands after it paint on top of the panel as
    /// usual. The backend implements this with an offscreen render-graph
    /// (render-to-texture + separable Gaussian blur + composite), so the effect
    /// is a genuine blur of the backdrop rather than a flat translucent fill.
    FrostedRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        /// Corner radius in logical pixels. `0.0` == sharp corners. Clamped to
        /// half the shorter side by the backend.
        corner_radius: f32,
        /// Tint laid over the blurred backdrop, packed `0xRRGGBBAA`. Usually a
        /// translucent white (light glass) or near-black (dark glass).
        tint: u32,
        /// Gaussian blur radius in logical pixels. Larger == frostier. `0.0`
        /// disables the blur (tint-only panel).
        blur_radius: f32,
    },
    /// A run of text. Layout/shaping is the backend's job; the position is the
    /// top-left of the text box's content origin.
    Text {
        x: f32,
        y: f32,
        content: String,
        /// Font size in logical pixels.
        size: f32,
        /// Text color, packed `0xRRGGBBAA`.
        color: u32,
    },
}

/// An ordered list of draw commands. Painted front-to-back in `Vec` order.
pub type Scene = Vec<DrawCmd>;
