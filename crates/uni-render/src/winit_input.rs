//! winit -> [`crate::InputEvent`] translation.
//!
//! This is the windowing-aware half of the input plumbing. It depends on
//! `winit` (which is fine: this module lives alongside the wgpu/winit backend,
//! not in the GPU-free [`crate::input`] module) and turns a concrete
//! `winit::event::WindowEvent` into an optional renderer-agnostic
//! [`InputEvent`] in **logical pixels**.

use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::Key;

use crate::input::{InputEvent, PointerButton};

/// Line height (logical px) used to convert line-based scroll wheels into
/// pixel deltas, so consumers see one consistent unit regardless of platform.
const SCROLL_LINE_HEIGHT: f32 = 16.0;

/// Translate a winit [`WindowEvent`] into an [`InputEvent`] in **logical
/// pixels** (top-left origin, y-down), or `None` if the event carries no input
/// we surface.
///
/// `scale_factor` is the window's HiDPI ratio: winit reports cursor positions
/// in *physical* pixels, so they are divided by it to land on the same logical
/// grid that [`crate::DrawCmd`]s use. `cursor` is the caller-owned "last known
/// cursor position" (logical px): `CursorMoved` updates it, and button presses
/// read it so press/release events carry coordinates (winit's `MouseInput`
/// itself has no position).
///
/// ```no_run
/// # use uni_render::translate_window_event;
/// let mut cursor = (0.0_f32, 0.0_f32);
/// # let scale_factor = 1.0_f64;
/// # fn handle(_e: uni_render::InputEvent) {}
/// # let event: winit::event::WindowEvent = unimplemented!();
/// if let Some(input) = translate_window_event(&event, scale_factor, &mut cursor) {
///     handle(input);
/// }
/// ```
pub fn translate_window_event(
    event: &WindowEvent,
    scale_factor: f64,
    cursor: &mut (f32, f32),
) -> Option<InputEvent> {
    let scale = scale_factor as f32;
    match event {
        WindowEvent::CursorMoved { position, .. } => {
            let x = position.x as f32 / scale;
            let y = position.y as f32 / scale;
            *cursor = (x, y);
            Some(InputEvent::PointerMoved { x, y })
        }
        WindowEvent::MouseInput { state, button, .. } => {
            let (x, y) = *cursor;
            let button = map_button(*button);
            match state {
                ElementState::Pressed => Some(InputEvent::PointerDown { x, y, button }),
                ElementState::Released => Some(InputEvent::PointerUp { x, y, button }),
            }
        }
        WindowEvent::MouseWheel { delta, .. } => {
            let (dx, dy) = match delta {
                // Line-based wheels: scale by a fixed logical line height.
                MouseScrollDelta::LineDelta(lx, ly) => {
                    (lx * SCROLL_LINE_HEIGHT, ly * SCROLL_LINE_HEIGHT)
                }
                // Pixel deltas come in physical px; convert to logical px.
                MouseScrollDelta::PixelDelta(p) => (p.x as f32 / scale, p.y as f32 / scale),
            };
            Some(InputEvent::Scroll { dx, dy })
        }
        WindowEvent::KeyboardInput { event, .. } => {
            let key = key_name(&event.logical_key);
            match event.state {
                ElementState::Pressed => Some(InputEvent::KeyDown { key }),
                ElementState::Released => Some(InputEvent::KeyUp { key }),
            }
        }
        _ => None,
    }
}

/// Map a winit [`MouseButton`] onto a [`PointerButton`].
///
/// winit's `Back`/`Forward` thumb buttons have no dedicated `PointerButton`
/// variant, so they are preserved via `Other(_)` with the codes winit itself
/// uses for them on the `Other` path.
fn map_button(button: MouseButton) -> PointerButton {
    match button {
        MouseButton::Left => PointerButton::Left,
        MouseButton::Right => PointerButton::Right,
        MouseButton::Middle => PointerButton::Middle,
        MouseButton::Back => PointerButton::Other(4),
        MouseButton::Forward => PointerButton::Other(5),
        MouseButton::Other(code) => PointerButton::Other(code),
    }
}

/// A simple, human-readable key name for v0.
///
/// `Character` keys yield the typed text (e.g. `"a"`, `"A"`, `"1"`); named keys
/// (e.g. Enter, ArrowLeft, Space) use winit's `NamedKey` `Debug` name; anything
/// else falls back to `"Unidentified"`.
fn key_name(key: &Key) -> String {
    match key {
        Key::Character(s) => s.to_string(),
        Key::Named(named) => format!("{named:?}"),
        Key::Dead(Some(c)) => c.to_string(),
        Key::Dead(None) => "Dead".to_string(),
        Key::Unidentified(_) => "Unidentified".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_mapping() {
        assert_eq!(map_button(MouseButton::Left), PointerButton::Left);
        assert_eq!(map_button(MouseButton::Right), PointerButton::Right);
        assert_eq!(map_button(MouseButton::Middle), PointerButton::Middle);
        assert_eq!(map_button(MouseButton::Back), PointerButton::Other(4));
        assert_eq!(map_button(MouseButton::Forward), PointerButton::Other(5));
        assert_eq!(map_button(MouseButton::Other(9)), PointerButton::Other(9));
    }

    #[test]
    fn character_key_name() {
        let key = Key::Character("a".into());
        assert_eq!(key_name(&key), "a");
    }

    #[test]
    fn named_key_name() {
        use winit::keyboard::NamedKey;
        let key = Key::Named(NamedKey::Enter);
        assert_eq!(key_name(&key), "Enter");
    }

    #[test]
    fn cursor_move_updates_tracker_and_is_logical() {
        let mut cursor = (0.0, 0.0);
        let event = WindowEvent::CursorMoved {
            device_id: winit::event::DeviceId::dummy(),
            position: winit::dpi::PhysicalPosition::new(200.0, 100.0),
        };
        // scale_factor 2.0 -> logical halves the physical coords.
        let out = translate_window_event(&event, 2.0, &mut cursor);
        assert_eq!(out, Some(InputEvent::PointerMoved { x: 100.0, y: 50.0 }));
        assert_eq!(cursor, (100.0, 50.0));
    }

    #[test]
    fn mouse_press_uses_tracked_cursor() {
        let mut cursor = (12.0, 34.0);
        let event = WindowEvent::MouseInput {
            device_id: winit::event::DeviceId::dummy(),
            state: ElementState::Pressed,
            button: MouseButton::Left,
        };
        let out = translate_window_event(&event, 1.0, &mut cursor);
        assert_eq!(
            out,
            Some(InputEvent::PointerDown {
                x: 12.0,
                y: 34.0,
                button: PointerButton::Left
            })
        );
    }

    #[test]
    fn line_scroll_converts_to_pixels() {
        let mut cursor = (0.0, 0.0);
        let event = WindowEvent::MouseWheel {
            device_id: winit::event::DeviceId::dummy(),
            delta: MouseScrollDelta::LineDelta(0.0, -2.0),
            phase: winit::event::TouchPhase::Moved,
        };
        let out = translate_window_event(&event, 1.0, &mut cursor);
        assert_eq!(
            out,
            Some(InputEvent::Scroll {
                dx: 0.0,
                dy: -2.0 * SCROLL_LINE_HEIGHT
            })
        );
    }
}
