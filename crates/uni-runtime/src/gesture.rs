//! SwiftUI-style **gesture recognizers**, driven from renderer-agnostic
//! [`uni_render::InputEvent`]s and dispatched through the runtime's **one
//! audited action path** (the runtime's private `dispatch`, the one a click
//! travels).
//!
//! SwiftUI builds interaction out of *gestures* (`TapGesture`, `LongPressGesture`,
//! `DragGesture`, `MagnifyGesture`, `RotationGesture`) attached to a view with
//! `.gesture(...)` / `.onTapGesture(...)`. This module mirrors that shape on top
//! of the pointer/scroll/pinch/rotate vocabulary `uni-render` already produces:
//!
//! - A [`GestureKind`] is a SwiftUI recognizer *descriptor* (the parameters:
//!   tap count, long-press minimum duration, drag threshold).
//! - A [`Recognizer`] pairs a [`GestureKind`] with the **node** it is attached to
//!   and the **action name** to fire when it recognizes — plus its live state
//!   machine. It consumes [`uni_render::InputEvent`]s (and time, via
//!   [`Recognizer::tick`]) and emits zero or more [`GestureEvent`]s.
//! - The runtime owns a set of recognizers (a tiny *arena*) and, on every input,
//!   feeds each one and routes the emitted [`GestureEvent`]s to
//!   `Runtime::dispatch` with the originating [`uni_ir::Origin`]. So a
//!   `drag_changed` fires exactly like a `click`: `doc.fire(node, event, origin)`
//!   records the audited `Invoke`, then the registered handler runs. There is **no
//!   second code path** — gestures are just more events on the same surface, and
//!   an AI driving synthetic input is as accountable as a human.
//!
//! ## Why a recognizer carries *live translation/scale/angle*
//!
//! SwiftUI's gesture closures receive a *value* (`DragGesture.Value.translation`,
//! `MagnifyGesture.Value.magnification`, …). We expose the same live quantity on
//! the recognizer ([`Recognizer::translation`], [`Recognizer::scale`],
//! [`Recognizer::rotation`]) so a handler — or a test — can read where the
//! gesture currently is, not just that it fired.
//!
//! ## Multitouch on a headless / desktop path
//!
//! Desktop winit has no first-class pinch/rotate in the renderer-agnostic input
//! path, so [`uni_render::InputEvent::Pinch`] / [`uni_render::InputEvent::Rotate`]
//! are additive, default-safe variants that the winit translator never emits in
//! v0. The magnify/rotation recognizers are therefore driven **programmatically**
//! — feed a pinch/rotate delta (a touch/trackpad backend, a test, or an AI does
//! this) and they recognize exactly as a tap/drag does from pointer events.

use uni_render::{InputEvent, PointerButton};

/// The kind + parameters of a SwiftUI-style gesture recognizer.
///
/// This is the descriptor half (mirroring `TapGesture(count:)`,
/// `LongPressGesture(minimumDuration:)`, `DragGesture(minimumDistance:)`,
/// `MagnifyGesture`, `RotationGesture`); the live state lives on the owning
/// [`Recognizer`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GestureKind {
    /// `TapGesture(count:)` — recognizes after `count` press→release cycles land
    /// (without exceeding the movement slop). `count == 1` is a single tap,
    /// `count == 2` a double tap. Fires the action's event on recognition.
    Tap {
        /// Number of taps required (`onTapGesture(count:)`).
        count: u32,
    },
    /// `LongPressGesture(minimumDuration:)` — recognizes once the pointer has
    /// been held down (without moving past the slop) for at least
    /// `min_duration` seconds. Driven by [`Recognizer::tick`].
    LongPress {
        /// Minimum hold time in seconds before recognition.
        min_duration: f32,
    },
    /// `DragGesture(minimumDistance:)` — once the pointer moves past
    /// `min_distance` logical px while pressed, the drag is *active* and emits a
    /// `..._changed` event on every subsequent move (carrying the live
    /// translation) and a `..._ended` on release.
    Drag {
        /// Distance (logical px) the pointer must travel before the drag begins.
        min_distance: f32,
    },
    /// `MagnifyGesture` — accumulates [`InputEvent::Pinch`] deltas into a running
    /// scale (starting at `1.0`); emits a `..._changed` per step and an
    /// `..._ended` when the gesture is concluded ([`Recognizer::end_continuous`]).
    Magnify,
    /// `RotationGesture` — accumulates [`InputEvent::Rotate`] deltas into a
    /// running angle (radians, starting at `0.0`); emits a `..._changed` per step
    /// and an `..._ended` on conclusion.
    Rotation,
}

/// What a recognizer emits when it advances — the event *name suffix* plus the
/// action to fire. The runtime composes the concrete event string
/// (`"<action>"`, `"<action>_changed"`, `"<action>_ended"`) and fires it on the
/// recognizer's node through the audited dispatch path.
#[derive(Clone, Debug, PartialEq)]
pub enum GestureEvent {
    /// A discrete gesture recognized (tap, long-press): fire the bare action
    /// event once.
    Recognized,
    /// A continuous gesture updated (drag/magnify/rotation in progress): fire the
    /// `_changed` event. The live value is read off the recognizer.
    Changed,
    /// A continuous gesture concluded: fire the `_ended` event.
    Ended,
}

/// The phase a continuous (drag/magnify/rotation) recognizer is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Not tracking anything.
    Idle,
    /// Pointer is down but the activation threshold has not been crossed yet
    /// (a pending tap/long-press, or a drag still inside its slop).
    Pending,
    /// The gesture is active and emitting `_changed`/`_ended`.
    Active,
}

/// A live SwiftUI-style gesture recognizer: a [`GestureKind`] bound to a node +
/// action name, plus the state machine that turns input into [`GestureEvent`]s.
#[derive(Clone, Debug)]
pub struct Recognizer {
    /// The node this gesture is attached to (events fire on it).
    pub node: uni_ir::NodeId,
    /// The base action/event name. The bare name fires for discrete gestures and
    /// the active start of continuous ones; `_changed`/`_ended` are appended for
    /// the continuous phases.
    pub action: String,
    /// The descriptor (kind + parameters).
    pub kind: GestureKind,

    // -- live state ----------------------------------------------------------
    phase: Phase,
    /// Press origin (logical px) for drag/tap slop measurement.
    press_origin: (f32, f32),
    /// Current pointer position (logical px).
    cursor: (f32, f32),
    /// Seconds the pointer has been held in the current press (long-press clock).
    held: f32,
    /// Completed press→release cycles for a multi-tap.
    tap_count: u32,
    /// Live drag translation since the press origin.
    translation: (f32, f32),
    /// Live magnify scale (1.0 == no magnification).
    scale: f32,
    /// Live rotation angle in radians.
    rotation: f32,
    /// Set once a long-press has fired, so a subsequent release does not also
    /// register a tap.
    long_fired: bool,
}

/// Movement slop (logical px): a press that stays within this radius still
/// counts as a tap / can become a long-press; moving past it cancels them.
const TAP_SLOP: f32 = 10.0;

impl Recognizer {
    /// Build a recognizer of `kind` attached to `node`, firing `action`.
    pub fn new(node: uni_ir::NodeId, action: impl Into<String>, kind: GestureKind) -> Self {
        Recognizer {
            node,
            action: action.into(),
            kind,
            phase: Phase::Idle,
            press_origin: (0.0, 0.0),
            cursor: (0.0, 0.0),
            held: 0.0,
            tap_count: 0,
            translation: (0.0, 0.0),
            scale: 1.0,
            rotation: 0.0,
            long_fired: false,
        }
    }

    /// The live drag translation `(dx, dy)` since the press began (SwiftUI's
    /// `DragGesture.Value.translation`). Zero unless this is a drag mid-gesture.
    pub fn translation(&self) -> (f32, f32) {
        self.translation
    }

    /// The live magnification factor (SwiftUI's `MagnifyGesture` magnification);
    /// `1.0` means unchanged.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// The live rotation angle in radians (SwiftUI's `RotationGesture` angle).
    pub fn rotation(&self) -> f32 {
        self.rotation
    }

    /// True while this recognizer is actively emitting `_changed` events.
    pub fn is_active(&self) -> bool {
        self.phase == Phase::Active
    }

    /// Whether `point` is within tap/long-press slop of the press origin.
    fn within_slop(&self, point: (f32, f32)) -> bool {
        let dx = point.0 - self.press_origin.0;
        let dy = point.1 - self.press_origin.1;
        (dx * dx + dy * dy).sqrt() <= TAP_SLOP
    }

    /// Feed one [`InputEvent`] in. Returns the [`GestureEvent`]s this recognizer
    /// emits in response (most events emit none; a recognition emits one, a drag
    /// move emits one `Changed`, a release may emit `Ended`).
    ///
    /// `hit` reports whether the pointer event landed on (or bubbled to) this
    /// recognizer's node — pointer recognizers only arm on a press that hits
    /// them. Pinch/rotate steps ignore `hit` (they are fed programmatically and
    /// targeted by the caller).
    pub fn feed(&mut self, input: &InputEvent, hit: bool) -> Vec<GestureEvent> {
        match self.kind {
            GestureKind::Tap { count } => self.feed_tap(input, hit, count),
            GestureKind::LongPress { .. } => self.feed_longpress(input, hit),
            GestureKind::Drag { min_distance } => self.feed_drag(input, hit, min_distance),
            GestureKind::Magnify => self.feed_magnify(input),
            GestureKind::Rotation => self.feed_rotation(input),
        }
    }

    fn feed_tap(&mut self, input: &InputEvent, hit: bool, count: u32) -> Vec<GestureEvent> {
        match input {
            InputEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
            } if hit => {
                self.press_origin = (*x, *y);
                self.cursor = (*x, *y);
                self.phase = Phase::Pending;
                Vec::new()
            }
            InputEvent::PointerMoved { x, y } => {
                self.cursor = (*x, *y);
                // Moving out of slop cancels a pending tap sequence.
                if self.phase == Phase::Pending && !self.within_slop((*x, *y)) {
                    self.phase = Phase::Idle;
                    self.tap_count = 0;
                }
                Vec::new()
            }
            InputEvent::PointerUp {
                x,
                y,
                button: PointerButton::Left,
            } => {
                if self.phase == Phase::Pending && self.within_slop((*x, *y)) {
                    self.tap_count += 1;
                    self.phase = Phase::Idle;
                    if self.tap_count >= count {
                        self.tap_count = 0;
                        return vec![GestureEvent::Recognized];
                    }
                } else {
                    self.phase = Phase::Idle;
                    self.tap_count = 0;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn feed_longpress(&mut self, input: &InputEvent, hit: bool) -> Vec<GestureEvent> {
        match input {
            InputEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
            } if hit => {
                self.press_origin = (*x, *y);
                self.cursor = (*x, *y);
                self.held = 0.0;
                self.long_fired = false;
                self.phase = Phase::Pending;
                Vec::new()
            }
            InputEvent::PointerMoved { x, y } => {
                self.cursor = (*x, *y);
                if self.phase == Phase::Pending && !self.within_slop((*x, *y)) {
                    // Moved too far before the hold completed: cancel.
                    self.phase = Phase::Idle;
                }
                Vec::new()
            }
            InputEvent::PointerUp { .. } => {
                self.phase = Phase::Idle;
                self.held = 0.0;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn feed_drag(&mut self, input: &InputEvent, hit: bool, min_distance: f32) -> Vec<GestureEvent> {
        match input {
            InputEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
            } if hit => {
                self.press_origin = (*x, *y);
                self.cursor = (*x, *y);
                self.translation = (0.0, 0.0);
                self.phase = Phase::Pending;
                Vec::new()
            }
            InputEvent::PointerMoved { x, y } => {
                if self.phase == Phase::Idle {
                    return Vec::new();
                }
                self.cursor = (*x, *y);
                let dx = *x - self.press_origin.0;
                let dy = *y - self.press_origin.1;
                self.translation = (dx, dy);
                let dist = (dx * dx + dy * dy).sqrt();
                if self.phase == Phase::Pending {
                    if dist >= min_distance {
                        self.phase = Phase::Active;
                        return vec![GestureEvent::Changed];
                    }
                    Vec::new()
                } else {
                    // Already active: every move is a `_changed`.
                    vec![GestureEvent::Changed]
                }
            }
            InputEvent::PointerUp { x, y, .. } => {
                if self.phase == Phase::Active {
                    self.translation = (*x - self.press_origin.0, *y - self.press_origin.1);
                    self.phase = Phase::Idle;
                    return vec![GestureEvent::Ended];
                }
                self.phase = Phase::Idle;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn feed_magnify(&mut self, input: &InputEvent) -> Vec<GestureEvent> {
        match input {
            InputEvent::Pinch { delta } => {
                if self.phase != Phase::Active {
                    self.phase = Phase::Active;
                    self.scale = 1.0;
                }
                // Magnification composes multiplicatively (a +0.1 delta grows 10%).
                self.scale *= 1.0 + delta;
                vec![GestureEvent::Changed]
            }
            _ => Vec::new(),
        }
    }

    fn feed_rotation(&mut self, input: &InputEvent) -> Vec<GestureEvent> {
        match input {
            InputEvent::Rotate { delta } => {
                if self.phase != Phase::Active {
                    self.phase = Phase::Active;
                    self.rotation = 0.0;
                }
                self.rotation += delta;
                vec![GestureEvent::Changed]
            }
            _ => Vec::new(),
        }
    }

    /// Advance time by `dt` seconds. Only the long-press recognizer cares: once
    /// the pointer has been held (within slop) past its `min_duration`, it
    /// recognizes once and emits [`GestureEvent::Recognized`]. Returns the
    /// emitted events (empty for non-long-press kinds or before the threshold).
    pub fn tick(&mut self, dt: f32) -> Vec<GestureEvent> {
        if let GestureKind::LongPress { min_duration } = self.kind {
            if self.phase == Phase::Pending && !self.long_fired {
                self.held += dt;
                if self.held >= min_duration {
                    self.long_fired = true;
                    self.phase = Phase::Idle;
                    return vec![GestureEvent::Recognized];
                }
            }
        }
        Vec::new()
    }

    /// Conclude a continuous (magnify/rotation) gesture programmatically: if it
    /// was active, emit [`GestureEvent::Ended`] and reset. Pointer-driven drags
    /// end on `PointerUp`, so this is for the pinch/rotate path that has no
    /// natural "up" in the headless input vocabulary.
    pub fn end_continuous(&mut self) -> Vec<GestureEvent> {
        if self.phase == Phase::Active
            && matches!(self.kind, GestureKind::Magnify | GestureKind::Rotation)
        {
            self.phase = Phase::Idle;
            return vec![GestureEvent::Ended];
        }
        Vec::new()
    }

    /// Force this recognizer back to idle, dropping any pending/active state
    /// **without** emitting events. Used by combined-gesture precedence: when a
    /// drag wins, a sibling tap/long-press on the same node is cancelled so it
    /// cannot also fire.
    pub fn cancel(&mut self) {
        self.phase = Phase::Idle;
        self.tap_count = 0;
        self.held = 0.0;
        self.long_fired = false;
        self.translation = (0.0, 0.0);
    }

    /// Whether this recognizer currently has a press in flight (pending or
    /// active) — used to decide combined-gesture precedence.
    pub fn is_tracking(&self) -> bool {
        self.phase != Phase::Idle
    }
}

/// Compose the concrete event string a [`GestureEvent`] fires, given a base
/// `action` name: `Recognized` → the bare name, `Changed` → `"<action>_changed"`,
/// `Ended` → `"<action>_ended"`.
pub(crate) fn event_name(action: &str, ev: &GestureEvent) -> String {
    match ev {
        GestureEvent::Recognized => action.to_string(),
        GestureEvent::Changed => format!("{action}_changed"),
        GestureEvent::Ended => format!("{action}_ended"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uni_ir::NodeId;

    fn down(x: f32, y: f32) -> InputEvent {
        InputEvent::PointerDown {
            x,
            y,
            button: PointerButton::Left,
        }
    }
    fn up(x: f32, y: f32) -> InputEvent {
        InputEvent::PointerUp {
            x,
            y,
            button: PointerButton::Left,
        }
    }

    /// Event-name composition: the bare action for a recognition, `_changed` /
    /// `_ended` for the continuous phases.
    #[test]
    fn event_names_compose() {
        assert_eq!(event_name("drag", &GestureEvent::Recognized), "drag");
        assert_eq!(event_name("drag", &GestureEvent::Changed), "drag_changed");
        assert_eq!(event_name("drag", &GestureEvent::Ended), "drag_ended");
    }

    /// A tap recognizer in isolation: a hit press→release within slop recognizes;
    /// a press that misses (hit == false) never arms.
    #[test]
    fn recognizer_tap_needs_a_hit() {
        let mut r = Recognizer::new(NodeId(0), "tap", GestureKind::Tap { count: 1 });
        // Missed press: no state, no recognition.
        assert!(r.feed(&down(0.0, 0.0), false).is_empty());
        assert!(r.feed(&up(0.0, 0.0), false).is_empty());
        assert!(!r.is_tracking());

        // Hit press → release recognizes.
        assert!(r.feed(&down(5.0, 5.0), true).is_empty());
        assert_eq!(r.feed(&up(6.0, 5.0), true), vec![GestureEvent::Recognized]);
    }

    /// A magnify recognizer composes scale multiplicatively and ends only when
    /// concluded.
    #[test]
    fn recognizer_magnify_scale_and_end() {
        let mut r = Recognizer::new(NodeId(0), "zoom", GestureKind::Magnify);
        assert_eq!(
            r.feed(&InputEvent::Pinch { delta: 1.0 }, false),
            vec![GestureEvent::Changed]
        );
        assert_eq!(r.scale(), 2.0);
        assert!(r.is_active());
        assert_eq!(r.end_continuous(), vec![GestureEvent::Ended]);
        assert!(!r.is_active());
        // A second end is a no-op.
        assert!(r.end_continuous().is_empty());
    }

    /// `cancel` drops state without emitting and leaves the recognizer idle.
    #[test]
    fn recognizer_cancel_is_silent() {
        let mut r = Recognizer::new(NodeId(0), "tap", GestureKind::Tap { count: 2 });
        r.feed(&down(0.0, 0.0), true);
        r.feed(&up(0.0, 0.0), true); // one tap banked
        r.cancel();
        assert!(!r.is_tracking());
        // After cancel the count is reset, so a fresh single press→release does
        // not complete the (count: 2) gesture.
        r.feed(&down(0.0, 0.0), true);
        assert!(r.feed(&up(0.0, 0.0), true).is_empty());
    }
}
