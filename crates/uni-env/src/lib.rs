//! uni-env — the reactive trait ENVIRONMENT that makes one UI universal.
//!
//! Every responsive / adaptive / cowork decision in the engine reads from
//! [`Env`]: the window size, density, text scale, input mode, build variant,
//! and surface kind. This crate is `std`-only and dependency-free.
//!
//! Reactivity wiring (signals / observers) comes later; today this is a plain
//! value struct with derived query methods.

#![forbid(unsafe_code)]

/// Width buckets, mirroring the Material/adaptive "window size class" model.
///
/// Boundaries are in **logical pixels**: `< 600` is [`Compact`], `< 840` is
/// [`Medium`], and `>= 840` is [`Expanded`].
///
/// [`Compact`]: WidthClass::Compact
/// [`Medium`]: WidthClass::Medium
/// [`Expanded`]: WidthClass::Expanded
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WidthClass {
    /// Phones in portrait, narrow side panes: `< 600` logical px.
    Compact,
    /// Small tablets, large phones in landscape: `[600, 840)` logical px.
    Medium,
    /// Tablets, desktops, large shells: `>= 840` logical px.
    Expanded,
}

/// Height buckets. `< 480` logical px is [`Short`], otherwise [`Tall`].
///
/// [`Short`]: HeightClass::Short
/// [`Tall`]: HeightClass::Tall
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeightClass {
    /// Landscape phones, squat embedded panels: `< 480` logical px.
    Short,
    /// Everything with vertical room to breathe: `>= 480` logical px.
    Tall,
}

/// How the user is pointing at the UI. Drives hit-target sizing and hover
/// affordances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputMode {
    /// Direct touch — large hit targets, no hover.
    Touch,
    /// Mouse / trackpad / stylus — precise, hover-capable.
    Pointer,
    /// TV / car / 10-foot remote — focus-driven, directional.
    Remote,
}

/// Which build of the product this is. Selects the accent color (design law)
/// and gates internal-only affordances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuildVariant {
    /// Internal / dev build — violet accent.
    Internal,
    /// Public / shipping build — lime accent.
    Public,
}

/// The physical (or logical) surface the UI is presented on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SurfaceKind {
    /// Native desktop window.
    Desktop,
    /// Handset.
    Phone,
    /// Tablet.
    Tablet,
    /// Embedded / kiosk / appliance panel.
    Embedded,
    /// A shell surface (launcher, system UI).
    Shell,
    /// A dashboard / control surface.
    Dashboard,
    /// Running in a browser.
    Web,
}

/// Logical-pixel breakpoint between [`WidthClass::Compact`] and
/// [`WidthClass::Medium`].
pub const BREAKPOINT_COMPACT: f32 = 600.0;
/// Logical-pixel breakpoint between [`WidthClass::Medium`] and
/// [`WidthClass::Expanded`].
pub const BREAKPOINT_MEDIUM: f32 = 840.0;
/// Logical-pixel breakpoint marking a "large" expanded surface (desktop-wide).
pub const BREAKPOINT_LARGE: f32 = 1200.0;
/// Logical-pixel breakpoint between [`HeightClass::Short`] and
/// [`HeightClass::Tall`].
pub const BREAKPOINT_TALL: f32 = 480.0;

/// Build-variant accent for the Internal build: violet `0x7D39EBFF`.
pub const ACCENT_INTERNAL: u32 = 0x7D39_EBFF;
/// Build-variant accent for the Public build: lime `0xC6FF33FF`.
pub const ACCENT_PUBLIC: u32 = 0xC6FF_33FF;

/// The universal UI environment.
///
/// A snapshot of everything an adaptive layout decision needs. Sizes are in
/// **logical pixels**.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Env {
    /// Window width, logical px.
    pub win_w: f32,
    /// Window height, logical px.
    pub win_h: f32,
    /// VisualDensity-like compaction factor (1.0 == comfortable default).
    pub density: f32,
    /// OS Dynamic-Type analogue — text size multiplier (1.0 == default).
    pub text_scale: f32,
    /// Active input mode.
    pub input_mode: InputMode,
    /// Build variant (selects accent + internal affordances).
    pub build_variant: BuildVariant,
    /// Surface this UI is presented on.
    pub surface_kind: SurfaceKind,
}

impl Env {
    /// Construct an `Env` for a window of the given logical size, filling the
    /// rest with sane defaults: density `1.0`, text scale `1.0`,
    /// [`InputMode::Pointer`], [`BuildVariant::Public`], and a
    /// [`SurfaceKind`] inferred from the width class.
    pub fn for_window(w: f32, h: f32) -> Self {
        let surface_kind = match width_class_of(w) {
            WidthClass::Compact => SurfaceKind::Phone,
            WidthClass::Medium => SurfaceKind::Tablet,
            WidthClass::Expanded => SurfaceKind::Desktop,
        };
        Env {
            win_w: w,
            win_h: h,
            density: 1.0,
            text_scale: 1.0,
            input_mode: InputMode::Pointer,
            build_variant: BuildVariant::Public,
            surface_kind,
        }
    }

    /// The width class for the current window width.
    pub fn width_class(&self) -> WidthClass {
        width_class_of(self.win_w)
    }

    /// The height class for the current window height.
    pub fn height_class(&self) -> HeightClass {
        if self.win_h < BREAKPOINT_TALL {
            HeightClass::Short
        } else {
            HeightClass::Tall
        }
    }

    /// `true` when the active input mode is direct touch.
    pub fn is_touch(&self) -> bool {
        matches!(self.input_mode, InputMode::Touch)
    }

    /// The build-variant accent color as `0xRRGGBBAA`.
    ///
    /// Internal builds get violet, public builds get lime — per design law.
    /// (No emerald.)
    pub fn accent(&self) -> u32 {
        match self.build_variant {
            BuildVariant::Internal => ACCENT_INTERNAL,
            BuildVariant::Public => ACCENT_PUBLIC,
        }
    }

    /// Viewport-width unit: `f` percent of the window width, in logical px.
    ///
    /// CSS `vw`/`vh` don't exist here, so these are the engine's analogue.
    /// `vw(100.0)` == full width; `vw(50.0)` == half width.
    pub fn vw(&self, f: f32) -> f32 {
        self.win_w * f / 100.0
    }

    /// Viewport-height unit: `f` percent of the window height, in logical px.
    pub fn vh(&self, f: f32) -> f32 {
        self.win_h * f / 100.0
    }
}

/// Free helper: the [`WidthClass`] for a given logical width. Shared by
/// [`Env::width_class`] and [`Env::for_window`].
fn width_class_of(w: f32) -> WidthClass {
    if w < BREAKPOINT_COMPACT {
        WidthClass::Compact
    } else if w < BREAKPOINT_MEDIUM {
        WidthClass::Medium
    } else {
        WidthClass::Expanded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_class_thresholds() {
        assert_eq!(
            Env::for_window(599.0, 800.0).width_class(),
            WidthClass::Compact
        );
        assert_eq!(
            Env::for_window(600.0, 800.0).width_class(),
            WidthClass::Medium
        );
        assert_eq!(
            Env::for_window(839.0, 800.0).width_class(),
            WidthClass::Medium
        );
        assert_eq!(
            Env::for_window(840.0, 800.0).width_class(),
            WidthClass::Expanded
        );
        // edge: zero width is still Compact.
        assert_eq!(
            Env::for_window(0.0, 800.0).width_class(),
            WidthClass::Compact
        );
    }

    #[test]
    fn height_class_thresholds() {
        assert_eq!(
            Env::for_window(800.0, 479.0).height_class(),
            HeightClass::Short
        );
        assert_eq!(
            Env::for_window(800.0, 480.0).height_class(),
            HeightClass::Tall
        );
        assert_eq!(
            Env::for_window(800.0, 1000.0).height_class(),
            HeightClass::Tall
        );
    }

    #[test]
    fn accent_selects_violet_for_internal() {
        let mut env = Env::for_window(1024.0, 768.0);
        env.build_variant = BuildVariant::Internal;
        assert_eq!(env.accent(), 0x7D39_EBFF);
        assert_eq!(env.accent(), ACCENT_INTERNAL);
    }

    #[test]
    fn accent_selects_lime_for_public() {
        let mut env = Env::for_window(1024.0, 768.0);
        env.build_variant = BuildVariant::Public;
        assert_eq!(env.accent(), 0xC6FF_33FF);
        assert_eq!(env.accent(), ACCENT_PUBLIC);
        // No emerald: the green accent must be lime, not 0x10B981.. or similar.
        assert_ne!(env.accent(), 0x10B9_81FF);
    }

    #[test]
    fn vw_vh_math() {
        let env = Env::for_window(1000.0, 500.0);
        assert_eq!(env.vw(100.0), 1000.0);
        assert_eq!(env.vw(50.0), 500.0);
        assert_eq!(env.vw(0.0), 0.0);
        assert_eq!(env.vh(100.0), 500.0);
        assert_eq!(env.vh(10.0), 50.0);
        assert_eq!(env.vh(0.0), 0.0);
    }

    #[test]
    fn is_touch_reflects_input_mode() {
        let mut env = Env::for_window(400.0, 800.0);
        env.input_mode = InputMode::Touch;
        assert!(env.is_touch());
        env.input_mode = InputMode::Pointer;
        assert!(!env.is_touch());
        env.input_mode = InputMode::Remote;
        assert!(!env.is_touch());
    }

    #[test]
    fn for_window_defaults() {
        let env = Env::for_window(1280.0, 720.0);
        assert_eq!(env.density, 1.0);
        assert_eq!(env.text_scale, 1.0);
        assert_eq!(env.input_mode, InputMode::Pointer);
        assert_eq!(env.build_variant, BuildVariant::Public);
        assert_eq!(env.surface_kind, SurfaceKind::Desktop);
    }
}
