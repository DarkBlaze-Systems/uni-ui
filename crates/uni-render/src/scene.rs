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
