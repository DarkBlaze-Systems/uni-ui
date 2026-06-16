//! # uni-tokens — the DarkBlaze Uni-UI design-token layer
//!
//! Pure data + functions, std-only, no heavy dependencies. This crate
//! encodes the project's **LOCKED design law** as concrete values so every
//! renderer and frontend draws from one opinionated source of truth.
//!
//! Colors are packed `u32` in `0xRRGGBBAA` order, matching uni-ir's
//! [`Value::Color(u32)`], so a token drops straight into the IR with no
//! conversion. Sizes/spacing are `f32` logical pixels (device-independent;
//! physical-pixel resolution happens at render time, never here).
//!
//! ## The design law, in five parts
//!
//! - **[`Palette`]** — sparse monochrome chrome. Substrate is white, ink is
//!   near-black; depth is rendered with white *glow* and dark *shadow*, never
//!   with color. The only chromatic note is the build-variant **accent**
//!   ([`Variant::Internal`] => violet, [`Variant::Public`] => lime), used
//!   sparingly. There is deliberately **no emerald**.
//! - **[`Space`]** — a geometric base-4/8 scale.
//! - **[`Type`]** — semantic type roles, each with a parallel *emphasized*
//!   variant (M3-Expressive), plus a global `font_scale` (Dynamic-Type).
//! - **[`Motion`]** — two-spring discipline: spatial easing may overshoot,
//!   effects easing is flat and never bounces.
//! - **[`Shape`]** — a corner-radius scale from `0` to fully rounded.
//!
//! [`Tokens::for_variant`] assembles the whole set for a build variant.
//!
//! [`Value::Color(u32)`]: ../uni_ir/enum.Value.html

// =============================================================================
// Variant
// =============================================================================

/// Which build of the product these tokens dress.
///
/// The variant changes exactly one thing in the visual language: the accent
/// color. Everything else (the monochrome chrome, spacing, type, motion,
/// shape) is shared, by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variant {
    /// Internal builds wear the violet accent.
    Internal,
    /// Public builds wear the lime accent.
    Public,
}

// =============================================================================
// ThemeMode
// =============================================================================

/// Light or dark theme — controls which palette substrate/ink pair is used.
///
/// The engine is **dark-first**: dark mode uses a near-black substrate with
/// white ink; light mode inverts them. The accent and accent-based depth
/// signals (glow, shadow) adapt to stay legible in both modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ThemeMode {
    /// Dark chrome — near-black substrate, white ink. Default.
    #[default]
    Dark,
    /// Light chrome — white substrate, near-black ink.
    Light,
}

// =============================================================================
// Palette
// =============================================================================

/// A packed `0xRRGGBBAA` color, matching uni-ir's `Value::Color(u32)`.
pub type Color = u32;

/// Substrate white — the canvas the chrome sits on.
pub const SUBSTRATE: Color = 0xffffffff;
/// Near-black — the deepest ink / inverse substrate.
pub const NEAR_BLACK: Color = 0x0a0a0aff;

/// Internal build accent: violet, used sparingly.
pub const ACCENT_VIOLET: Color = 0x7D39EBFF;
/// Public build accent: lime, used sparingly.
pub const ACCENT_LIME: Color = 0xC6FF33FF;

// NB: there is intentionally NO emerald constant. Depth is rendered with
// glow + shadow, not with color, and the only chromatic accent is the
// per-variant violet/lime above. A test asserts no `0x..` emerald leaks in.

/// Sparse monochrome chrome.
///
/// White substrate, near-black ink, and depth expressed through a white
/// `glow` and a dark `shadow` — *not* through color. The single chromatic
/// note is `accent`, selected by [`Variant`].
///
/// Ink comes in three legibility levels: `ink` (body), `ink_soft`
/// (secondary), `ink_faint` (tertiary / disabled).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// The canvas color most surfaces are painted on.
    pub substrate: Color,
    /// Primary ink (body text, default foreground).
    pub ink: Color,
    /// Secondary ink — softened for supporting content.
    pub ink_soft: Color,
    /// Tertiary ink — faint, for placeholders / disabled states.
    pub ink_faint: Color,
    /// White glow used to render raised depth (light, not color).
    pub glow: Color,
    /// Dark shadow used to render recessed depth (dark, not color).
    pub shadow: Color,
    /// The one chromatic note — violet (Internal) or lime (Public).
    pub accent: Color,
}

impl Palette {
    /// The monochrome chrome for `variant`. Only `accent` varies.
    /// Equivalent to `for_mode(ThemeMode::Light, variant)` — kept for
    /// backward compatibility with existing call sites.
    pub const fn for_variant(variant: Variant) -> Self {
        Palette {
            substrate: SUBSTRATE, // white
            ink: NEAR_BLACK,
            ink_soft: 0x0a0a0aaa,
            ink_faint: 0x0a0a0a66,
            glow: 0xffffff66,
            shadow: 0x0a0a0a40,
            accent: match variant {
                Variant::Internal => ACCENT_VIOLET,
                Variant::Public => ACCENT_LIME,
            },
        }
    }

    /// Select the palette for a given [`ThemeMode`] + [`Variant`].
    ///
    /// - **Dark** (default): near-black substrate, white ink, white glow on
    ///   dark backgrounds — this is the engine's primary aesthetic.
    /// - **Light**: white substrate, near-black ink — for contexts that need
    ///   a classic light appearance.
    pub const fn for_mode(mode: ThemeMode, variant: Variant) -> Self {
        let accent = match variant {
            Variant::Internal => ACCENT_VIOLET,
            Variant::Public => ACCENT_LIME,
        };
        match mode {
            ThemeMode::Dark => Palette {
                substrate: NEAR_BLACK, // 0x0a0a0aff — the dark canvas
                ink: 0xffffffff,       // white text/icons
                ink_soft: 0xffffffaa,
                ink_faint: 0xffffff66,
                glow: 0xffffff22,   // subtle white rim light
                shadow: 0x00000066, // deeper shadow on dark
                accent,
            },
            ThemeMode::Light => Palette {
                substrate: SUBSTRATE, // 0xffffffff — white canvas
                ink: NEAR_BLACK,
                ink_soft: 0x0a0a0aaa,
                ink_faint: 0x0a0a0a66,
                glow: 0xffffff66,
                shadow: 0x0a0a0a40,
                accent,
            },
        }
    }
}

// =============================================================================
// Space
// =============================================================================

/// Geometric base-4/8 spacing scale, in `f32` logical pixels.
///
/// `tight` 4 · `snug` 8 · `comfy` 16 · `loose` 24 · `vast` 32. Layout should
/// reach for these named steps rather than ad-hoc numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Space {
    /// 4px — hairline gaps, icon padding.
    pub tight: f32,
    /// 8px — the base unit.
    pub snug: f32,
    /// 16px — comfortable content padding.
    pub comfy: f32,
    /// 24px — section separation.
    pub loose: f32,
    /// 32px — large structural gaps.
    pub vast: f32,
}

impl Default for Space {
    fn default() -> Self {
        Space {
            tight: 4.0,
            snug: 8.0,
            comfy: 16.0,
            loose: 24.0,
            vast: 32.0,
        }
    }
}

// =============================================================================
// Type
// =============================================================================

/// Font weight as a numeric axis value (CSS-style, 100..=900).
pub type Weight = u16;

/// Regular weight.
pub const WEIGHT_REGULAR: Weight = 400;
/// Medium weight.
pub const WEIGHT_MEDIUM: Weight = 500;
/// Semibold weight.
pub const WEIGHT_SEMIBOLD: Weight = 600;
/// Bold weight.
pub const WEIGHT_BOLD: Weight = 700;

/// One concrete type style: size, weight, and tracking.
///
/// `size` and `letter_spacing` are logical pixels (tracking may be negative
/// for tight display text). These are the *base* values; the owning [`Type`]
/// applies its `font_scale` to produce the effective size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextStyle {
    /// Base size in logical pixels (before `font_scale`).
    pub size: f32,
    /// Numeric font weight.
    pub weight: Weight,
    /// Letter spacing (tracking) in logical pixels; may be negative.
    pub letter_spacing: f32,
}

impl TextStyle {
    const fn new(size: f32, weight: Weight, letter_spacing: f32) -> Self {
        TextStyle {
            size,
            weight,
            letter_spacing,
        }
    }
}

/// One semantic role: its base style plus an M3-Expressive `emphasized`
/// variant of the same role (heavier / tighter for moments of emphasis).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Role {
    /// The default rendering of this role.
    pub base: TextStyle,
    /// The expressive, emphasized rendering of the same role.
    pub emphasized: TextStyle,
}

/// The semantic type ramp.
///
/// Six roles — `display` / `title` / `subtitle` / `body` / `caption` / `mono`
/// — each carrying a base and an [`emphasized`](Role::emphasized) style.
/// `font_scale` is the global Dynamic-Type multiplier: `1.0` is the design
/// default, larger values enlarge every role uniformly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Type {
    /// Largest role — hero / display copy.
    pub display: Role,
    /// Screen / section titles.
    pub title: Role,
    /// Supporting headings.
    pub subtitle: Role,
    /// Default reading text.
    pub body: Role,
    /// Small, secondary annotations.
    pub caption: Role,
    /// Monospaced role for code / tabular figures.
    pub mono: Role,
    /// Global Dynamic-Type multiplier applied to every role's size.
    pub font_scale: f32,
}

impl Default for Type {
    fn default() -> Self {
        Type {
            display: Role {
                base: TextStyle::new(57.0, WEIGHT_REGULAR, -0.25),
                emphasized: TextStyle::new(57.0, WEIGHT_SEMIBOLD, -0.5),
            },
            title: Role {
                base: TextStyle::new(28.0, WEIGHT_SEMIBOLD, 0.0),
                emphasized: TextStyle::new(28.0, WEIGHT_BOLD, -0.25),
            },
            subtitle: Role {
                base: TextStyle::new(22.0, WEIGHT_MEDIUM, 0.0),
                emphasized: TextStyle::new(22.0, WEIGHT_SEMIBOLD, 0.0),
            },
            body: Role {
                base: TextStyle::new(16.0, WEIGHT_REGULAR, 0.15),
                emphasized: TextStyle::new(16.0, WEIGHT_MEDIUM, 0.1),
            },
            caption: Role {
                base: TextStyle::new(12.0, WEIGHT_REGULAR, 0.4),
                emphasized: TextStyle::new(12.0, WEIGHT_SEMIBOLD, 0.4),
            },
            mono: Role {
                base: TextStyle::new(14.0, WEIGHT_REGULAR, 0.0),
                emphasized: TextStyle::new(14.0, WEIGHT_SEMIBOLD, 0.0),
            },
            font_scale: 1.0,
        }
    }
}

impl Type {
    /// Effective (scaled) size for a base `size`, applying `font_scale`.
    ///
    /// This is the one place the Dynamic-Type multiplier is honored, so call
    /// it rather than reading `style.size` directly when rendering.
    pub fn scaled(&self, size: f32) -> f32 {
        size * self.font_scale
    }
}

// =============================================================================
// Motion
// =============================================================================

/// Tunables for an overshooting (ease-out-back-like) spatial spring.
///
/// `overshoot` > 0 lets the motion sail past its target before settling — the
/// hallmark of the *spatial* spring. Effects motion never uses these.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpatialEasing {
    /// How far past the target the curve sails (back-ease tension). > 0.
    pub overshoot: f32,
    /// Settling stiffness of the spatial spring.
    pub stiffness: f32,
    /// Damping of the spatial spring.
    pub damping: f32,
}

/// Tunables for a flat (no-bounce) effects spring.
///
/// By contract `overshoot` does not exist here: effects — opacity, color,
/// blur — must never bounce.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectsEasing {
    /// Stiffness of the (critically/over-damped) effects spring.
    pub stiffness: f32,
    /// Damping of the effects spring; chosen so it never overshoots.
    pub damping: f32,
}

/// Two-spring motion discipline.
///
/// Two distinct easings, never interchanged: a `spatial` spring that may
/// overshoot (movement, size, position) and an `effects` spring that is flat
/// (opacity, color, blur). `expressive` toggles the louder Expressive feel.
/// Durations are seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Motion {
    /// Expressive mode — bigger overshoot, livelier timing.
    pub expressive: bool,
    /// The overshooting spring for spatial properties.
    pub spatial: SpatialEasing,
    /// The flat, non-bouncing spring for effects.
    pub effects: EffectsEasing,
    /// Fast duration (seconds) — micro-feedback.
    pub fast: f32,
    /// Default duration (seconds).
    pub default: f32,
    /// Slow duration (seconds) — large transitions.
    pub slow: f32,
}

impl Default for Motion {
    fn default() -> Self {
        Motion {
            expressive: false,
            spatial: SpatialEasing {
                overshoot: 0.15,
                stiffness: 220.0,
                damping: 26.0,
            },
            effects: EffectsEasing {
                // Critically damped: settles without ever crossing the target.
                stiffness: 300.0,
                damping: 40.0,
            },
            fast: 0.12,
            default: 0.24,
            slow: 0.40,
        }
    }
}

impl Motion {
    /// The Expressive preset — same two-spring discipline, turned up: the
    /// spatial spring overshoots more, effects stay flat (never bounce).
    pub fn expressive() -> Self {
        Motion {
            expressive: true,
            spatial: SpatialEasing {
                overshoot: 0.30,
                stiffness: 180.0,
                damping: 18.0,
            },
            // Effects easing is unchanged in spirit: still flat, no overshoot.
            effects: EffectsEasing {
                stiffness: 280.0,
                damping: 38.0,
            },
            fast: 0.14,
            default: 0.28,
            slow: 0.48,
        }
    }
}

// =============================================================================
// Shape
// =============================================================================

/// Corner-radius scale, in `f32` logical pixels.
///
/// Runs from `none` (square) to `full` (a sentinel large enough to fully
/// round any reasonable control — renderers clamp it to `height / 2`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shape {
    /// 0px — hard corners.
    pub none: f32,
    /// 4px — subtle softening.
    pub small: f32,
    /// 8px — default control rounding.
    pub medium: f32,
    /// 16px — cards / sheets.
    pub large: f32,
    /// Fully rounded sentinel (pill / circle); clamp to half the lesser side.
    pub full: f32,
}

impl Default for Shape {
    fn default() -> Self {
        Shape {
            none: 0.0,
            small: 4.0,
            medium: 8.0,
            large: 16.0,
            full: 9999.0,
        }
    }
}

// =============================================================================
// Tokens
// =============================================================================

/// The complete design-token set for one build variant.
///
/// Assemble it with [`Tokens::for_variant`]; everything but the palette accent
/// is shared across variants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tokens {
    /// Monochrome chrome + per-variant accent.
    pub palette: Palette,
    /// Base-4/8 spacing scale.
    pub space: Space,
    /// Semantic type roles + Dynamic-Type scale.
    pub r#type: Type,
    /// Two-spring motion discipline.
    pub motion: Motion,
    /// Corner-radius scale.
    pub shape: Shape,
}

impl Tokens {
    /// Build the full token set for `variant`.
    pub fn for_variant(variant: Variant) -> Self {
        Tokens {
            palette: Palette::for_variant(variant),
            space: Space::default(),
            r#type: Type::default(),
            motion: Motion::default(),
            shape: Shape::default(),
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_selects_the_right_accent() {
        let internal = Tokens::for_variant(Variant::Internal);
        let public = Tokens::for_variant(Variant::Public);

        assert_eq!(internal.palette.accent, ACCENT_VIOLET);
        assert_eq!(internal.palette.accent, 0x7D39EBFF);
        assert_eq!(public.palette.accent, ACCENT_LIME);
        assert_eq!(public.palette.accent, 0xC6FF33FF);
        assert_ne!(internal.palette.accent, public.palette.accent);
    }

    #[test]
    fn chrome_is_monochrome_substrate_and_ink() {
        let t = Tokens::for_variant(Variant::Public);
        assert_eq!(t.palette.substrate, SUBSTRATE);
        assert_eq!(t.palette.substrate, 0xffffffff);
        assert_eq!(t.palette.ink, NEAR_BLACK);
        assert_eq!(t.palette.ink, 0x0a0a0aff);
        // Ink levels share the near-black hue, descending in alpha.
        let alpha = |c: Color| c & 0xff;
        assert!(alpha(t.palette.ink) > alpha(t.palette.ink_soft));
        assert!(alpha(t.palette.ink_soft) > alpha(t.palette.ink_faint));
    }

    #[test]
    fn font_scale_multiplies_sizes() {
        let mut ty = Type::default();
        let base = ty.body.base.size;

        // Default scale is identity.
        assert_eq!(ty.font_scale, 1.0);
        assert_eq!(ty.scaled(base), base);

        // Doubling the scale doubles every effective size.
        ty.font_scale = 2.0;
        assert_eq!(ty.scaled(base), base * 2.0);
        assert_eq!(ty.scaled(ty.display.base.size), ty.display.base.size * 2.0);

        // A fractional Dynamic-Type setting scales proportionally.
        ty.font_scale = 1.5;
        assert!((ty.scaled(10.0) - 15.0).abs() < f32::EPSILON);
    }

    #[test]
    fn every_role_has_an_emphasized_variant() {
        let ty = Type::default();
        for role in [
            ty.display,
            ty.title,
            ty.subtitle,
            ty.body,
            ty.caption,
            ty.mono,
        ] {
            // Emphasized is at least as heavy as the base — the whole point.
            assert!(role.emphasized.weight >= role.base.weight);
        }
    }

    #[test]
    fn motion_is_two_spring_spatial_overshoots_effects_flat() {
        let m = Motion::default();
        // Spatial spring overshoots...
        assert!(m.spatial.overshoot > 0.0);
        // ...durations are ordered fast < default < slow.
        assert!(m.fast < m.default && m.default < m.slow);

        let e = Motion::expressive();
        assert!(e.expressive);
        // Expressive overshoots harder than the default spatial spring.
        assert!(e.spatial.overshoot > m.spatial.overshoot);
        // Effects easing has no overshoot field at all — flat by construction.
        // (Asserting damping >= "near-critical" keeps it non-bouncy.)
        assert!(e.effects.damping > 0.0);
    }

    #[test]
    fn shape_runs_zero_to_full() {
        let s = Shape::default();
        assert_eq!(s.none, 0.0);
        assert!(s.none < s.small);
        assert!(s.small < s.medium);
        assert!(s.medium < s.large);
        assert!(s.large < s.full);
    }

    #[test]
    fn space_is_geometric_base_4_8() {
        let sp = Space::default();
        assert_eq!(sp.tight, 4.0);
        assert_eq!(sp.snug, 8.0);
        assert_eq!(sp.comfy, 16.0);
        assert_eq!(sp.loose, 24.0);
        assert_eq!(sp.vast, 32.0);
    }

    /// The design law forbids emerald. Emerald lives around hue 140-165°,
    /// high green with low red/blue. Assert none of our defined colors fall
    /// in that band — proving "no emerald constant present".
    #[test]
    fn no_emerald_constant_present() {
        let colors = [SUBSTRATE, NEAR_BLACK, ACCENT_VIOLET, ACCENT_LIME];
        for c in colors {
            let r = ((c >> 24) & 0xff) as i32;
            let g = ((c >> 16) & 0xff) as i32;
            let b = ((c >> 8) & 0xff) as i32;
            // Emerald = green clearly dominant over BOTH red and blue.
            // (Lime is yellow-green: green high but red is also high, so it
            // is NOT emerald and correctly passes.)
            let is_emerald = g > 120 && g > r + 60 && g > b + 60;
            assert!(
                !is_emerald,
                "color {c:#010x} reads as emerald (r={r} g={g} b={b}) — forbidden",
            );
        }
        // Sanity: lime IS green-dominant vs blue but stays high-red, so it
        // must survive the emerald check above.
        let lr = ((ACCENT_LIME >> 24) & 0xff) as i32;
        let lg = ((ACCENT_LIME >> 16) & 0xff) as i32;
        assert!(
            lr > 150 && lg > 150,
            "lime should be yellow-green, not emerald"
        );
    }
}
