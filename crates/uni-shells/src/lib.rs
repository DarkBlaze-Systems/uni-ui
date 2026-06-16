//! # uni-shells — DarkBlaze Uni-UI adaptive shell surfaces
//!
//! This crate builds the OS-chrome layer: shell widgets that morph between
//! distinct stages driven by `Runtime::animate()`. The canonical example is
//! the **Smart-Topbar** — a 4-stage spring-animated pill that transitions
//! between Silence / Notify / Morph / Chat.
//!
//! ## Architecture
//!
//! Every shell here follows the same shape as `uni-widgets`:
//!
//! - It owns a subtree of [`uni_ir::Document`] nodes allocated with
//!   [`Document::fresh_id`] and emitted via [`Origin::System`] mutations.
//! - It exposes the relevant [`NodeId`]s so the caller can hand them to
//!   `Runtime::animate()` for spring-driven transitions.
//! - `transition()` returns the `(width, height)` targets so the caller can
//!   feed them straight into `Runtime::animate()` without re-computing them.
//!
//! ## Smart-Topbar stages
//!
//! ```text
//! Silence  36px  ●   minimal ambient pill
//! Notify   80px  Notification   brief expansion
//! Morph   160px  AI is presenting...   interactive card
//! Chat    320px  Chat with AI   full AI surface
//! ```

use uni_ir::{Document, Mutation, NodeId, Origin, Value};
use uni_tokens::Tokens;

// ---------------------------------------------------------------------------
// Internal emit helpers (identical contract to uni-widgets)
// ---------------------------------------------------------------------------

fn create(doc: &mut Document, kind: &str) -> NodeId {
    let id = doc.fresh_id();
    doc.apply_from(
        Origin::System,
        Mutation::CreateNode {
            id,
            kind: kind.into(),
        },
    )
    .expect("fresh id is always unique");
    id
}

fn prop(doc: &mut Document, id: NodeId, key: &str, value: Value) {
    doc.apply_from(
        Origin::System,
        Mutation::SetProp {
            id,
            key: key.into(),
            value,
        },
    )
    .expect("node exists");
}

fn append(doc: &mut Document, parent: NodeId, child: NodeId) {
    doc.apply_from(Origin::System, Mutation::AppendChild { parent, child })
        .expect("both nodes exist");
}

// ---------------------------------------------------------------------------
// TopbarStage
// ---------------------------------------------------------------------------

/// The four morphological stages of the Smart-Topbar.
///
/// Each stage maps to a target `(width, height)` pair. The caller hands these
/// targets to `Runtime::animate()` so a spring drives the transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopbarStage {
    /// Minimal pill/notch — ambient only.
    Silence,
    /// Brief notification expansion.
    Notify,
    /// Action card — AI presenting something interactive.
    Morph,
    /// Full AI chat surface.
    Chat,
}

impl TopbarStage {
    /// The target height in logical pixels for this stage.
    pub fn target_height(&self) -> f32 {
        match self {
            TopbarStage::Silence => 36.0,
            TopbarStage::Notify => 80.0,
            TopbarStage::Morph => 160.0,
            TopbarStage::Chat => 320.0,
        }
    }

    /// The target width in logical pixels for this stage, given viewport width.
    ///
    /// Returns an absolute pixel value (not a fraction), clamped to a minimum
    /// so the pill never shrinks below a readable size.
    pub fn width_fraction(&self, viewport_w: f32) -> f32 {
        match self {
            TopbarStage::Silence => (viewport_w * 0.35).max(120.0),
            TopbarStage::Notify => (viewport_w * 0.65).max(200.0),
            TopbarStage::Morph => (viewport_w * 0.85).max(300.0),
            TopbarStage::Chat => viewport_w - 32.0,
        }
    }

    /// The status-text content to display while at this stage.
    fn status_text(&self) -> &'static str {
        match self {
            TopbarStage::Silence => "●",
            TopbarStage::Notify => "Notification",
            TopbarStage::Morph => "AI is presenting...",
            TopbarStage::Chat => "Chat with AI",
        }
    }
}

// ---------------------------------------------------------------------------
// SmartTopbar
// ---------------------------------------------------------------------------

/// Smart-Topbar shell: builds the IR subtree and exposes node ids for animation.
///
/// Call [`SmartTopbar::new`] (or the free [`build_topbar`] wrapper) to emit the
/// initial `Silence`-stage subtree. Call [`SmartTopbar::transition`] to advance
/// the stage and obtain the spring-animation targets.
///
/// The subtree structure:
///
/// ```text
/// root  Stack  (pill container, position=absolute, centered at top)
/// └─ content  Stack  (grow=1.0, flex fill)
///    └─ status_text  Text  (ambient label: "●" / "Notification" / …)
/// ```
pub struct SmartTopbar {
    /// Root container node (the pill).
    pub root: NodeId,
    /// Inner content stack.
    pub content: NodeId,
    /// Status text node (ambient label in Silence, notification in Notify, etc).
    pub status_text: NodeId,
    /// Current stage.
    pub stage: TopbarStage,
}

impl SmartTopbar {
    /// Build the topbar IR subtree in `doc`, initially at [`TopbarStage::Silence`].
    ///
    /// All mutations are attributed to [`Origin::System`] — this is library-
    /// authored chrome, not a human or AI edit.
    pub fn new(doc: &mut Document, _tokens: &Tokens, viewport_w: f32) -> Self {
        let stage = TopbarStage::Silence;
        let width = stage.width_fraction(viewport_w);
        let height = stage.target_height();

        // Root pill container.
        let root = create(doc, "Stack");
        prop(doc, root, "width", Value::Px(width));
        prop(doc, root, "height", Value::Px(height));
        prop(doc, root, "background", Value::Color(0x1A1A1AFF));
        prop(doc, root, "corner_radius", Value::Px(18.0));
        prop(doc, root, "position", Value::Text("absolute".into()));
        prop(doc, root, "top", Value::Px(8.0));
        // Center horizontally: left = (viewport_w - width) / 2
        prop(doc, root, "left", Value::Px((viewport_w - width) / 2.0));
        prop(doc, root, "padding", Value::Px(8.0));

        // Inner content stack (flex-grow fills the pill).
        let content = create(doc, "Stack");
        prop(doc, content, "grow", Value::Float(1.0));
        append(doc, root, content);

        // Status text (ambient dot at rest).
        let status_text = create(doc, "Text");
        prop(
            doc,
            status_text,
            "content",
            Value::Text(stage.status_text().into()),
        );
        prop(doc, status_text, "color", Value::Color(0xFFFFFFFF));
        prop(doc, status_text, "size", Value::Px(14.0));
        append(doc, content, status_text);

        SmartTopbar {
            root,
            content,
            status_text,
            stage,
        }
    }

    /// Transition to a new stage.
    ///
    /// - Updates `status_text.content` via a [`Origin::System`] [`Mutation::SetProp`]
    ///   so the audit trail records the change.
    /// - Updates `self.stage`.
    /// - Returns `(width_target, height_target)` — feed these directly to
    ///   `Runtime::animate()` on `self.root`.
    pub fn transition(
        &mut self,
        doc: &mut Document,
        stage: TopbarStage,
        viewport_w: f32,
    ) -> (f32, f32) {
        // Update the visible status text.
        prop(
            doc,
            self.status_text,
            "content",
            Value::Text(stage.status_text().into()),
        );

        self.stage = stage;

        let width = stage.width_fraction(viewport_w);
        let height = stage.target_height();
        (width, height)
    }
}

// ---------------------------------------------------------------------------
// build_topbar — free-function convenience wrapper
// ---------------------------------------------------------------------------

/// Build a [`SmartTopbar`] in `doc` and return it.
///
/// This is a thin wrapper around [`SmartTopbar::new`]; prefer calling it when
/// you don't need to hold a `&mut Document` for long.
pub fn build_topbar(doc: &mut Document, tokens: &Tokens, viewport_w: f32) -> SmartTopbar {
    SmartTopbar::new(doc, tokens, viewport_w)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use uni_tokens::{Tokens, Variant};

    fn toks() -> Tokens {
        Tokens::for_variant(Variant::Internal)
    }

    // ---- Stage geometry ----

    #[test]
    fn topbar_silence_height_is_36() {
        assert_eq!(TopbarStage::Silence.target_height(), 36.0);
    }

    #[test]
    fn topbar_stage_heights_increase() {
        assert!(TopbarStage::Notify.target_height() > TopbarStage::Silence.target_height());
        assert!(TopbarStage::Morph.target_height() > TopbarStage::Notify.target_height());
        assert!(TopbarStage::Chat.target_height() > TopbarStage::Morph.target_height());
    }

    #[test]
    fn stage_widths_increase_with_viewport() {
        let vw = 1280.0_f32;
        let silence = TopbarStage::Silence.width_fraction(vw);
        let notify = TopbarStage::Notify.width_fraction(vw);
        let morph = TopbarStage::Morph.width_fraction(vw);
        let chat = TopbarStage::Chat.width_fraction(vw);
        assert!(notify > silence);
        assert!(morph > notify);
        assert!(chat > morph);
    }

    // ---- IR construction ----

    #[test]
    fn build_topbar_creates_root_in_doc() {
        let mut doc = Document::new();
        let t = toks();
        let tb = build_topbar(&mut doc, &t, 1280.0);

        // Root node must exist in the document.
        assert!(doc.get(tb.root).is_some(), "root node not in doc");
        // Content node must exist and be a child of root.
        assert!(doc.get(tb.content).is_some(), "content node not in doc");
        assert!(
            doc.get(tb.root).unwrap().children.contains(&tb.content),
            "content is not a child of root"
        );
        // status_text must exist and be a child of content.
        assert!(doc.get(tb.status_text).is_some(), "status_text not in doc");
        assert!(
            doc.get(tb.content).unwrap().children.contains(&tb.status_text),
            "status_text is not a child of content"
        );
    }

    #[test]
    fn topbar_root_has_pill_corner_radius() {
        let mut doc = Document::new();
        let t = toks();
        let tb = build_topbar(&mut doc, &t, 1280.0);

        let root = doc.get(tb.root).unwrap();
        assert_eq!(
            root.props.get("corner_radius"),
            Some(&Value::Px(18.0)),
            "pill corner_radius must be 18px"
        );
    }

    #[test]
    fn topbar_root_is_absolute_positioned_near_top() {
        let mut doc = Document::new();
        let t = toks();
        let tb = build_topbar(&mut doc, &t, 1280.0);

        let root = doc.get(tb.root).unwrap();
        assert_eq!(
            root.props.get("position"),
            Some(&Value::Text("absolute".into())),
            "root must be absolute-positioned"
        );
        assert_eq!(
            root.props.get("top"),
            Some(&Value::Px(8.0)),
            "top offset must be 8px"
        );
    }

    #[test]
    fn topbar_initial_status_text_is_ambient_dot() {
        let mut doc = Document::new();
        let t = toks();
        let tb = build_topbar(&mut doc, &t, 1280.0);

        let st = doc.get(tb.status_text).unwrap();
        assert_eq!(
            st.props.get("content"),
            Some(&Value::Text("●".into())),
            "initial status text must be the ambient dot ●"
        );
    }

    #[test]
    fn transition_to_notify_returns_larger_height() {
        let mut doc = Document::new();
        let t = toks();
        let mut tb = build_topbar(&mut doc, &t, 1280.0);

        let silence_h = TopbarStage::Silence.target_height();
        let (_, notify_h) = tb.transition(&mut doc, TopbarStage::Notify, 1280.0);
        assert!(
            notify_h > silence_h,
            "Notify height ({notify_h}) must exceed Silence height ({silence_h})"
        );
    }

    #[test]
    fn transition_updates_status_text_content() {
        let mut doc = Document::new();
        let t = toks();
        let mut tb = build_topbar(&mut doc, &t, 1280.0);

        // Before transition: ambient dot.
        assert_eq!(
            doc.get(tb.status_text).unwrap().props.get("content"),
            Some(&Value::Text("●".into()))
        );

        // Transition to Chat.
        tb.transition(&mut doc, TopbarStage::Chat, 1280.0);
        assert_eq!(
            doc.get(tb.status_text).unwrap().props.get("content"),
            Some(&Value::Text("Chat with AI".into())),
            "after Chat transition status_text must read 'Chat with AI'"
        );

        // Transition to Morph.
        tb.transition(&mut doc, TopbarStage::Morph, 1280.0);
        assert_eq!(
            doc.get(tb.status_text).unwrap().props.get("content"),
            Some(&Value::Text("AI is presenting...".into()))
        );
    }

    #[test]
    fn transition_updates_stage_field() {
        let mut doc = Document::new();
        let t = toks();
        let mut tb = build_topbar(&mut doc, &t, 1280.0);

        assert_eq!(tb.stage, TopbarStage::Silence);
        tb.transition(&mut doc, TopbarStage::Notify, 1280.0);
        assert_eq!(tb.stage, TopbarStage::Notify);
        tb.transition(&mut doc, TopbarStage::Chat, 1280.0);
        assert_eq!(tb.stage, TopbarStage::Chat);
    }

    #[test]
    fn transition_returns_correct_geometry_for_all_stages() {
        let mut doc = Document::new();
        let t = toks();
        let mut tb = build_topbar(&mut doc, &t, 1280.0);
        let vw = 1280.0_f32;

        for stage in [
            TopbarStage::Silence,
            TopbarStage::Notify,
            TopbarStage::Morph,
            TopbarStage::Chat,
        ] {
            let (w, h) = tb.transition(&mut doc, stage, vw);
            assert_eq!(h, stage.target_height(), "height mismatch for {stage:?}");
            assert_eq!(
                w,
                stage.width_fraction(vw),
                "width mismatch for {stage:?}"
            );
        }
    }

    #[test]
    fn topbar_background_is_near_black_pill() {
        let mut doc = Document::new();
        let t = toks();
        let tb = build_topbar(&mut doc, &t, 1280.0);

        let root = doc.get(tb.root).unwrap();
        assert_eq!(
            root.props.get("background"),
            Some(&Value::Color(0x1A1A1AFF)),
            "topbar background must be near-black pill 0x1A1A1AFF"
        );
    }

    #[test]
    fn content_width_minimum_clamp_at_small_viewport() {
        // With a tiny viewport the minimum clamps should engage.
        let vw = 50.0_f32; // smaller than any minimum
        assert!(
            TopbarStage::Silence.width_fraction(vw) >= 120.0,
            "Silence must be at least 120px"
        );
        assert!(
            TopbarStage::Notify.width_fraction(vw) >= 200.0,
            "Notify must be at least 200px"
        );
        assert!(
            TopbarStage::Morph.width_fraction(vw) >= 300.0,
            "Morph must be at least 300px"
        );
    }
}
