//! Renderer-agnostic **input events**.
//!
//! This is rung-3 plumbing: it gives the rest of the engine (`uni-core`
//! hit-testing, event routing, …) a small, stable vocabulary of pointer /
//! scroll / keyboard events to consume — *without* any of them having to know
//! about winit, wgpu, or any windowing backend.
//!
//! Like [`crate::scene`], this module is deliberately **GPU- and
//! windowing-free**: it mentions neither `wgpu` nor `winit`. The job of turning
//! a concrete `winit::event::WindowEvent` into one of these is done by
//! [`crate::translate_window_event`], which lives in the winit-using part of
//! the crate.
//!
//! All coordinates are **logical pixels**, origin top-left, y increasing
//! downward — exactly the space [`crate::DrawCmd`]s are placed in, so a pointer
//! position can be compared directly against a scene's rects for hit-testing.

/// Which pointer (mouse) button an event refers to.
///
/// `Other(u16)` carries the raw backend button code for buttons beyond the
/// usual three (e.g. mouse "back"/"forward" thumb buttons), so no information
/// is lost crossing the boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PointerButton {
    /// The primary button (left, for right-handed mice).
    Left,
    /// The secondary button (right).
    Right,
    /// The middle button (often the scroll-wheel click).
    Middle,
    /// Any other button, identified by its raw backend code.
    Other(u16),
}

/// A single, backend-agnostic input event in **logical pixels**
/// (top-left origin, y-down).
///
/// These are produced from windowing events by
/// [`crate::translate_window_event`] and consumed by the rest of the engine.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    /// The pointer moved to `(x, y)` (logical px).
    PointerMoved { x: f32, y: f32 },
    /// A pointer `button` was pressed at `(x, y)` (logical px).
    PointerDown {
        x: f32,
        y: f32,
        button: PointerButton,
    },
    /// A pointer `button` was released at `(x, y)` (logical px).
    PointerUp {
        x: f32,
        y: f32,
        button: PointerButton,
    },
    /// A scroll/wheel gesture. `dx`/`dy` are logical-pixel deltas (positive
    /// `dy` scrolls content down). Line-based wheels are converted to pixels by
    /// the translator using a fixed line height.
    Scroll { dx: f32, dy: f32 },
    /// A key was pressed. `key` is a simple human-readable name for v0
    /// (e.g. `"a"`, `"Enter"`, `"ArrowLeft"`, `"Space"`).
    KeyDown { key: String },
    /// A key was released. See [`InputEvent::KeyDown`] for the `key` naming.
    KeyUp { key: String },
}
