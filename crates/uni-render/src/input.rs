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
    /// A **pinch/magnify** gesture step: `delta` is the incremental change in
    /// magnification factor since the previous step (so successive deltas
    /// *multiply* into a running scale; `0.0` is no change, `+0.1` grows by 10%).
    ///
    /// Desktop winit has no first-class multitouch pinch in the renderer-agnostic
    /// path, so this variant is **additive and default-safe**: the winit
    /// translator never emits it today. It exists so a trackpad/touch backend —
    /// or a test / an AI driving the surface programmatically — can feed a pinch
    /// to the gesture recognizers without inventing a side channel.
    Pinch { delta: f32 },
    /// A **rotation** gesture step: `delta` is the incremental change in angle
    /// (radians, counter-clockwise positive) since the previous step; successive
    /// deltas *sum* into a running rotation.
    ///
    /// Like [`InputEvent::Pinch`], this is additive and default-safe — emitted by
    /// a touch/trackpad backend or fed programmatically, never by the desktop
    /// winit translator in v0.
    Rotate { delta: f32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The additive pinch/rotate variants carry their delta and compare by value
    /// (so a recognizer can pattern-match and a test can assert on them).
    #[test]
    fn pinch_and_rotate_carry_delta() {
        assert_eq!(
            InputEvent::Pinch { delta: 0.25 },
            InputEvent::Pinch { delta: 0.25 }
        );
        assert_ne!(
            InputEvent::Rotate { delta: 0.1 },
            InputEvent::Rotate { delta: 0.2 }
        );
        match (InputEvent::Pinch { delta: -0.3 }) {
            InputEvent::Pinch { delta } => assert_eq!(delta, -0.3),
            _ => unreachable!("constructed a Pinch"),
        }
    }
}
