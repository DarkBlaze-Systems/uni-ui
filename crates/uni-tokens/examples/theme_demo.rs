//! theme_demo — the DarkBlaze design law, printed.
//!
//! Run it:
//!     CARGO_TARGET_DIR=./target cargo run -p uni-tokens --example theme_demo
//!
//! For every build [`Variant`] this walks the palette twice — once in
//! [`ThemeMode::Dark`] (the engine's default) and once in [`ThemeMode::Light`]
//! — and prints each color field side by side as `#RRGGBBAA`. It shows two
//! truths of the design law at a glance:
//!
//!   1. **Dark-default / light pairing.** Dark mode is a near-black substrate
//!      with white ink; Light mode inverts that. The chrome stays monochrome
//!      in both — depth is glow + shadow, never color.
//!   2. **The build-variant accent.** The *only* chromatic note is `accent`:
//!      violet for Internal builds, lime for Public. It is the one field that
//!      does NOT change between Dark and Light.

use uni_tokens::{Palette, ThemeMode, Variant};

/// Format a packed `0xRRGGBBAA` color as a `#RRGGBBAA` hex string.
fn hex(c: u32) -> String {
    format!("#{c:08X}")
}

/// Print one palette as labelled `field: #RRGGBBAA` rows, side by side.
fn print_pair(dark: &Palette, light: &Palette) {
    // field name, dark value, light value
    let rows: [(&str, u32, u32); 7] = [
        ("substrate", dark.substrate, light.substrate),
        ("ink", dark.ink, light.ink),
        ("ink_soft", dark.ink_soft, light.ink_soft),
        ("ink_faint", dark.ink_faint, light.ink_faint),
        ("glow", dark.glow, light.glow),
        ("shadow", dark.shadow, light.shadow),
        ("accent", dark.accent, light.accent),
    ];

    println!("    {:<12} {:<12} {:<12}", "field", "Dark", "Light");
    println!("    {:<12} {:<12} {:<12}", "-----", "----", "-----");
    for (name, d, l) in rows {
        let same = if d == l { "  (shared)" } else { "" };
        println!("    {:<12} {:<12} {:<12}{}", name, hex(d), hex(l), same,);
    }
}

fn main() {
    println!("== DarkBlaze Uni-UI — theme_demo ==");
    println!("Dark is the default; Light inverts substrate/ink. Accent is");
    println!("the only chromatic note and is shared across both modes.\n");

    for variant in [Variant::Internal, Variant::Public] {
        let accent_name = match variant {
            Variant::Internal => "violet",
            Variant::Public => "lime",
        };
        println!("Variant::{:?}  (accent = {})", variant, accent_name,);

        let dark = Palette::for_mode(ThemeMode::Dark, variant);
        let light = Palette::for_mode(ThemeMode::Light, variant);
        print_pair(&dark, &light);

        // The pairing, asserted in plain sight.
        assert_ne!(
            dark.substrate, light.substrate,
            "dark and light substrates must differ",
        );
        assert_eq!(dark.accent, light.accent, "accent is mode-independent",);
        println!();
    }

    // The accent is the one field that distinguishes the two builds.
    let internal = Palette::for_mode(ThemeMode::Dark, Variant::Internal);
    let public = Palette::for_mode(ThemeMode::Dark, Variant::Public);
    println!("Build-variant accent contrast:");
    println!("    Internal -> {}  (violet)", hex(internal.accent));
    println!("    Public   -> {}  (lime)", hex(public.accent));
    assert_ne!(internal.accent, public.accent);

    println!("\nDesign law holds: monochrome chrome, dark-default, one accent.");
}
