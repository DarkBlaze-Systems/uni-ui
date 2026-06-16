//! uni-webframe — the **Flow seam for foreign web engines**.
//!
//! A `WebFrame` is a rectangular region of the UI whose contents are drawn by a
//! *web engine* (an HTML/CSS/JS renderer) rather than by Uni-UI's own
//! [`uni_render`] pipeline. Per the Flow doctrine (see `engine/FLOW.md`), such a
//! foreign engine is isolated as a **swappable backend leaf behind the Flow,
//! never above it**: the engine core owns the spine and composites the web
//! surface as an *isolated leaf*, so the web engine can never contaminate the
//! core or its license.
//!
//! Concretely, a `WebFrame` surface is composited **behind** the Flow: it paints
//! into its rect first, and ordinary Uni-UI draw commands (chrome, overlays,
//! frosted panels) paint *on top* of it in the same [`Scene`]. The web engine
//! draws inside its box; it is never allowed to draw over Uni-UI's own UI.
//!
//! # The contract
//!
//! [`WebBackend`] is the trait a real web engine implements. The engine core
//! depends **only** on this trait — never on a concrete web engine type. That is
//! the whole point of the seam: swap the leaf, keep the core.
//!
//! # v0: contract + a stub, no browser
//!
//! This crate deliberately pulls in **no** browser dependency yet (no `wry`, no
//! `servo`). It defines the [`WebBackend`] contract and ships [`StubWebBackend`],
//! which `paint`s a visible placeholder (a rounded panel + a `WebFrame: <url>`
//! label) so a `WebFrame` composites *visibly today*, proving the seam
//! end-to-end before any heavy engine exists. The only dependency is
//! [`uni_render`], for the renderer-agnostic [`Scene`]/[`DrawCmd`] vocabulary.
//!
//! # The two future real leaves
//!
//! When the seam is filled for real, `StubWebBackend` is replaced (not the
//! trait) by one of:
//!
//! - **OS-webview** (via [`wry`](https://crates.io/crates/wry)) — the lightest
//!   path to *full, live* web: it hands the page to the operating system's own
//!   webview (WebView2 / WKWebView / WebKitGTK). Smallest binary, full modern
//!   web, but not sovereign (you ship the OS's engine, not yours).
//! - **Servo** (`libservo`) — the **sovereign** path: a clean-room,
//!   permissively-licensed, Rust web engine that composites its output to a
//!   **texture** Uni-UI then samples into the `WebFrame` rect. Heavier and
//!   younger than the OS webview, but fully owned and embeddable.
//!
//! Either way, **3D and video come for free** through whichever engine fills
//! this seam: WebGL/WebGPU canvases and `<video>` are just web content the
//! engine already renders into its surface — Uni-UI gets them by compositing the
//! leaf, with no 3D/video code of its own.
//!
//! In every case `paint` is the composite step. The stub literally emits
//! [`DrawCmd`]s; a real backend would upload its rendered surface as a texture
//! and emit a textured-quad command (a future [`DrawCmd`] variant) covering the
//! same rect — but the core's view of the seam, this trait, does not change.

use uni_render::{DrawCmd, Scene};

/// The rectangle a [`WebBackend`] paints into, in **logical pixels**, origin
/// top-left, y increasing downward — the same coordinate space as
/// [`uni_render::DrawCmd`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WebRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl WebRect {
    /// Construct a rect.
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
}

/// The Flow seam for a foreign web engine.
///
/// This is the only surface the engine core knows about. A real engine
/// (OS-webview via `wry`, or Servo via `libservo`) implements it; the core never
/// names the concrete type. See the [module docs](crate) for the full rationale.
pub trait WebBackend {
    /// Navigate the web surface to `url`.
    fn load(&mut self, url: &str);

    /// Resize the web surface to `w` × `h` logical pixels.
    fn resize(&mut self, w: f32, h: f32);

    /// Paint the web surface into the given rect, **appending** to the scene.
    ///
    /// Real backends composite their rendered surface as a texture (a textured
    /// quad over `rect`); the [stub](StubWebBackend) draws a visible
    /// placeholder. Either way the commands are *appended*, so they land behind
    /// any Uni-UI chrome drawn after this call — the leaf stays behind the Flow.
    fn paint(&mut self, rect: WebRect, scene: &mut Scene);

    /// The URL currently loaded.
    fn url(&self) -> &str;
}

/// A no-engine placeholder [`WebBackend`].
///
/// It holds no real web engine. [`paint`](WebBackend::paint) emits a rounded
/// panel plus a `WebFrame: <url>` label, so a `WebFrame` composites visibly
/// today — enough to prove the seam end-to-end. Swap this for `wry`/`libservo`
/// later without touching the [`WebBackend`] trait or the core.
pub struct StubWebBackend {
    url: String,
    w: f32,
    h: f32,
}

impl StubWebBackend {
    /// Create a stub backend initially loaded at `url`. Size defaults to `0`
    /// until [`resize`](WebBackend::resize) is called.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            w: 0.0,
            h: 0.0,
        }
    }

    /// Current logical width.
    pub fn width(&self) -> f32 {
        self.w
    }

    /// Current logical height.
    pub fn height(&self) -> f32 {
        self.h
    }
}

impl WebBackend for StubWebBackend {
    fn load(&mut self, url: &str) {
        self.url.clear();
        self.url.push_str(url);
    }

    fn resize(&mut self, w: f32, h: f32) {
        self.w = w;
        self.h = h;
    }

    fn paint(&mut self, rect: WebRect, scene: &mut Scene) {
        // The placeholder panel: a filled rounded rect filling the web rect.
        // A near-black glassy fill so it reads clearly as a distinct surface.
        scene.push(DrawCmd::FilledRect {
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
            color: 0x10_14_18_ff,
            corner_radius: 8.0,
        });

        // The label, inset from the panel's top-left, proving which URL this
        // leaf would render.
        scene.push(DrawCmd::Text {
            x: rect.x + 12.0,
            y: rect.y + 12.0,
            content: format!("WebFrame: {}", self.url),
            size: 16.0,
            color: 0xe6_ed_f3_ff,
        });
    }

    fn url(&self) -> &str {
        &self.url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sets_url_and_zero_size() {
        let b = StubWebBackend::new("https://example.com");
        assert_eq!(b.url(), "https://example.com");
        assert_eq!(b.width(), 0.0);
        assert_eq!(b.height(), 0.0);
    }

    #[test]
    fn load_changes_url() {
        let mut b = StubWebBackend::new("https://example.com");
        b.load("https://servo.org");
        assert_eq!(b.url(), "https://servo.org");
    }

    #[test]
    fn resize_updates_dimensions() {
        let mut b = StubWebBackend::new("https://example.com");
        b.resize(640.0, 480.0);
        assert_eq!(b.width(), 640.0);
        assert_eq!(b.height(), 480.0);
    }

    #[test]
    fn paint_produces_nonempty_scene_with_url_text() {
        let mut b = StubWebBackend::new("https://example.com");
        let mut scene: Scene = Vec::new();
        b.paint(WebRect::new(10.0, 20.0, 300.0, 200.0), &mut scene);

        assert!(!scene.is_empty(), "paint must append draw commands");

        // There must be a filled panel covering the rect.
        let has_panel = scene.iter().any(|c| {
            matches!(
                c,
                DrawCmd::FilledRect { x, y, w, h, .. }
                    if *x == 10.0 && *y == 20.0 && *w == 300.0 && *h == 200.0
            )
        });
        assert!(
            has_panel,
            "paint must draw the placeholder panel into the rect"
        );

        // There must be a Text command containing the loaded url.
        let url_in_text = scene.iter().any(|c| match c {
            DrawCmd::Text { content, .. } => content.contains("https://example.com"),
            _ => false,
        });
        assert!(url_in_text, "a Text command must contain the loaded url");
    }

    #[test]
    fn paint_appends_behind_later_chrome() {
        // The web leaf paints first; chrome appended after lands on top.
        let mut b = StubWebBackend::new("https://example.com");
        let mut scene: Scene = Vec::new();
        b.paint(WebRect::new(0.0, 0.0, 100.0, 100.0), &mut scene);
        let after_web = scene.len();
        scene.push(DrawCmd::FilledRect {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
            color: 0xffffffff,
            corner_radius: 0.0,
        });
        // Web commands occupy the earlier (behind) slots; chrome is last (on top).
        assert!(after_web >= 2);
        assert_eq!(scene.len(), after_web + 1);
    }

    #[test]
    fn paint_reflects_loaded_url_after_navigation() {
        let mut b = StubWebBackend::new("https://example.com");
        b.load("https://servo.org");
        let mut scene: Scene = Vec::new();
        b.paint(WebRect::new(0.0, 0.0, 100.0, 100.0), &mut scene);
        let url_in_text = scene.iter().any(|c| match c {
            DrawCmd::Text { content, .. } => content.contains("https://servo.org"),
            _ => false,
        });
        assert!(url_in_text);
    }

    /// `StubWebBackend` is usable purely through the `WebBackend` trait object,
    /// proving the core can hold the seam without naming the concrete type.
    #[test]
    fn usable_as_trait_object() {
        let mut b: Box<dyn WebBackend> = Box::new(StubWebBackend::new("https://example.com"));
        b.resize(320.0, 240.0);
        b.load("https://example.org");
        let mut scene: Scene = Vec::new();
        b.paint(WebRect::new(0.0, 0.0, 320.0, 240.0), &mut scene);
        assert!(!scene.is_empty());
        assert_eq!(b.url(), "https://example.org");
    }
}
