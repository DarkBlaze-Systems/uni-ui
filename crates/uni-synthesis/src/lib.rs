//! # uni-synthesis — per-user AI synthesis
//!
//! The "we serve them" layer. A [`Synthesizer`] reads the current [`Env`] and
//! [`UserProfile`] and produces a list of [`uni_ir::Mutation`]s that adapt
//! the UI specifically for this user. Mutations are applied via
//! `Origin::Ai` so they land in the audit log and are attributable.
//!
//! ## Design
//! The `Synthesizer` trait is the stable interface. [`BasicSynthesizer`] is
//! the rule-based implementation. Future AI-driven synthesizers (LLM, on-device
//! model) implement the same trait and are swappable without changing the engine.

use uni_env::{Env, InputMode, WidthClass};
use uni_ir::{Document, Mutation, NodeId, Origin, Value};

/// A user's declared preferences that inform synthesis.
#[derive(Debug, Clone)]
pub struct UserProfile {
    /// Preferred text scale multiplier (1.0 == default, 1.5 == large text).
    pub text_scale: f32,
    /// Preferred motion level: 0.0 = no motion, 1.0 = full expressive.
    pub motion: f32,
    /// Whether the user prefers high-contrast mode.
    pub high_contrast: bool,
    /// Preferred density: 0.8 = compact, 1.0 = default, 1.2 = spacious.
    pub density: f32,
    /// Preferred theme: true = dark, false = light.
    pub dark_mode: bool,
}

impl Default for UserProfile {
    fn default() -> Self {
        Self {
            text_scale: 1.0,
            motion: 1.0,
            high_contrast: false,
            density: 1.0,
            dark_mode: true,
        }
    }
}

/// The synthesis result: a list of mutations to apply via `Origin::Ai`.
pub struct SynthesisResult {
    pub mutations: Vec<(NodeId, Mutation)>,
}

/// A synthesizer adapts the UI document for a specific user+env combination.
///
/// Implement this trait for rule-based, LLM-driven, or on-device model synthesis.
pub trait Synthesizer {
    /// Given the current document, env, and user profile, produce mutations
    /// that should be applied via `Origin::Ai` to adapt the UI.
    fn synthesize(&self, doc: &Document, env: &Env, profile: &UserProfile) -> SynthesisResult;
}

/// Apply a [`SynthesisResult`] to a document via `Origin::Ai`.
/// Returns the number of mutations applied.
pub fn apply(doc: &mut Document, result: SynthesisResult) -> usize {
    let mut count = 0;
    for (_id, mutation) in result.mutations {
        if doc.apply_from(Origin::Ai, mutation).is_ok() {
            count += 1;
        }
    }
    count
}

/// Rule-based synthesizer: adapts text size, hit targets, spacing, and
/// contrast from the `UserProfile` and `Env` without any external model.
///
/// This is the baseline that ships. An LLM synthesizer can wrap or replace it.
pub struct BasicSynthesizer;

impl Synthesizer for BasicSynthesizer {
    fn synthesize(&self, doc: &Document, env: &Env, profile: &UserProfile) -> SynthesisResult {
        let mut mutations = Vec::new();

        // Walk every node in the document.
        if let Some(root) = doc.root() {
            walk(doc, root, env, profile, &mut mutations);
        }

        SynthesisResult { mutations }
    }
}

fn walk(
    doc: &Document,
    id: NodeId,
    env: &Env,
    profile: &UserProfile,
    out: &mut Vec<(NodeId, Mutation)>,
) {
    let Some(node) = doc.get(id) else { return };

    // 1. Text nodes: scale font size by user text_scale.
    if node.kind == "Text" {
        if let Some(Value::Px(base_size)) = node.props.get("size").cloned() {
            let scaled = (base_size * profile.text_scale).round();
            if (scaled - base_size).abs() > 0.5 {
                out.push((id, Mutation::SetProp {
                    id,
                    key: "size".into(),
                    value: Value::Px(scaled),
                }));
            }
        }
        // High contrast: override text color to pure white.
        if profile.high_contrast {
            out.push((id, Mutation::SetProp {
                id,
                key: "color".into(),
                value: Value::Color(0xFFFF_FFFF),
            }));
        }
    }

    // 2. Interactive nodes (have click callback): enlarge hit target for touch.
    if node.callbacks.contains_key("click") && env.input_mode == InputMode::Touch {
        if let Some(Value::Px(h)) = node.props.get("height").cloned() {
            if h < 48.0 {
                out.push((id, Mutation::SetProp {
                    id,
                    key: "height".into(),
                    value: Value::Px(48.0),
                }));
            }
        }
        if let Some(Value::Px(w)) = node.props.get("width").cloned() {
            if w < 48.0 {
                out.push((id, Mutation::SetProp {
                    id,
                    key: "width".into(),
                    value: Value::Px(48.0),
                }));
            }
        }
    }

    // 3. Layout containers: adjust gap/padding for density.
    if matches!(node.kind.as_str(), "Stack" | "Row" | "Column") {
        if let Some(Value::Px(gap)) = node.props.get("gap").cloned() {
            let adjusted = (gap * profile.density).round();
            if (adjusted - gap).abs() > 0.5 {
                out.push((id, Mutation::SetProp {
                    id,
                    key: "gap".into(),
                    value: Value::Px(adjusted),
                }));
            }
        }
    }

    // 4. Compact width class: collapse wide fixed-width elements.
    if env.width_class() == WidthClass::Compact {
        if let Some(Value::Px(w)) = node.props.get("width").cloned() {
            if w > env.win_w {
                out.push((id, Mutation::SetProp {
                    id,
                    key: "width".into(),
                    value: Value::Px(env.win_w - 32.0),
                }));
            }
        }
    }

    // Recurse.
    let children: Vec<NodeId> = node.children.clone();
    for child in children {
        walk(doc, child, env, profile, out);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use uni_env::{BuildVariant, InputMode, SurfaceKind};
    use uni_ir::{Action, Mutation, Origin, Value};

    fn make_env(win_w: f32, win_h: f32, input_mode: InputMode) -> Env {
        Env {
            win_w,
            win_h,
            density: 1.0,
            text_scale: 1.0,
            input_mode,
            build_variant: BuildVariant::Public,
            surface_kind: SurfaceKind::Desktop,
        }
    }

    fn make_doc_with_text(size: f32) -> (Document, NodeId) {
        let mut doc = Document::new();
        let root = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode { id: root, kind: "Stack".into() },
        )
        .unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id: root }).unwrap();

        let text = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode { id: text, kind: "Text".into() },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::SetProp { id: text, key: "size".into(), value: Value::Px(size) },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::AppendChild { parent: root, child: text },
        )
        .unwrap();
        (doc, text)
    }

    #[test]
    fn synthesize_scales_text_size_by_profile() {
        let (doc, text_id) = make_doc_with_text(16.0);
        let env = make_env(1280.0, 800.0, InputMode::Pointer);
        let profile = UserProfile { text_scale: 1.5, ..Default::default() };

        let result = BasicSynthesizer.synthesize(&doc, &env, &profile);

        // Should have one mutation: SetProp size -> 24.0 (16 * 1.5)
        let size_muts: Vec<_> = result
            .mutations
            .iter()
            .filter(|(id, m)| {
                *id == text_id
                    && matches!(m, Mutation::SetProp { key, value: Value::Px(v), .. }
                        if key == "size" && (*v - 24.0).abs() < 0.01)
            })
            .collect();
        assert_eq!(size_muts.len(), 1, "expected one text-size scale mutation");
    }

    #[test]
    fn synthesize_no_change_when_scale_is_1() {
        let (doc, _) = make_doc_with_text(16.0);
        let env = make_env(1280.0, 800.0, InputMode::Pointer);
        let profile = UserProfile { text_scale: 1.0, ..Default::default() };

        let result = BasicSynthesizer.synthesize(&doc, &env, &profile);

        // No size mutations when scale is exactly 1.0.
        let size_muts: Vec<_> = result
            .mutations
            .iter()
            .filter(|(_, m)| matches!(m, Mutation::SetProp { key, .. } if key == "size"))
            .collect();
        assert!(size_muts.is_empty(), "scale=1.0 should produce no size mutations");
    }

    #[test]
    fn synthesize_high_contrast_sets_white_text() {
        let (doc, text_id) = make_doc_with_text(16.0);
        let env = make_env(1280.0, 800.0, InputMode::Pointer);
        let profile = UserProfile { high_contrast: true, ..Default::default() };

        let result = BasicSynthesizer.synthesize(&doc, &env, &profile);

        let color_muts: Vec<_> = result
            .mutations
            .iter()
            .filter(|(id, m)| {
                *id == text_id
                    && matches!(m, Mutation::SetProp { key, value: Value::Color(0xFFFF_FFFF), .. }
                        if key == "color")
            })
            .collect();
        assert_eq!(color_muts.len(), 1, "high contrast should set color to pure white");
    }

    #[test]
    fn synthesize_touch_enlarges_small_buttons() {
        let mut doc = Document::new();
        let root = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode { id: root, kind: "Stack".into() },
        )
        .unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id: root }).unwrap();

        let btn = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode { id: btn, kind: "Button".into() },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::SetProp { id: btn, key: "width".into(), value: Value::Px(32.0) },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::SetProp { id: btn, key: "height".into(), value: Value::Px(32.0) },
        )
        .unwrap();
        // Register a click callback so the synthesizer sees it as interactive.
        doc.apply_from(
            Origin::System,
            Mutation::SetCallback {
                id: btn,
                event: "click".into(),
                action: Action { name: "noop".into(), args: vec![] },
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::AppendChild { parent: root, child: btn },
        )
        .unwrap();

        let env = make_env(400.0, 800.0, InputMode::Touch);
        let profile = UserProfile::default();

        let result = BasicSynthesizer.synthesize(&doc, &env, &profile);

        let w_muts: Vec<_> = result
            .mutations
            .iter()
            .filter(|(id, m)| {
                *id == btn
                    && matches!(m, Mutation::SetProp { key, value: Value::Px(v), .. }
                        if key == "width" && (*v - 48.0).abs() < 0.01)
            })
            .collect();
        let h_muts: Vec<_> = result
            .mutations
            .iter()
            .filter(|(id, m)| {
                *id == btn
                    && matches!(m, Mutation::SetProp { key, value: Value::Px(v), .. }
                        if key == "height" && (*v - 48.0).abs() < 0.01)
            })
            .collect();
        assert_eq!(w_muts.len(), 1, "touch should enlarge small button width to 48");
        assert_eq!(h_muts.len(), 1, "touch should enlarge small button height to 48");
    }

    #[test]
    fn synthesize_touch_does_not_shrink_large_buttons() {
        let mut doc = Document::new();
        let root = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode { id: root, kind: "Stack".into() },
        )
        .unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id: root }).unwrap();

        let btn = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode { id: btn, kind: "Button".into() },
        )
        .unwrap();
        // Already large enough.
        doc.apply_from(
            Origin::System,
            Mutation::SetProp { id: btn, key: "width".into(), value: Value::Px(80.0) },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::SetProp { id: btn, key: "height".into(), value: Value::Px(60.0) },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::SetCallback {
                id: btn,
                event: "click".into(),
                action: Action { name: "noop".into(), args: vec![] },
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::AppendChild { parent: root, child: btn },
        )
        .unwrap();

        let env = make_env(400.0, 800.0, InputMode::Touch);
        let profile = UserProfile::default();

        let result = BasicSynthesizer.synthesize(&doc, &env, &profile);

        // No width/height mutations for an already-large button.
        let dim_muts: Vec<_> = result
            .mutations
            .iter()
            .filter(|(id, m)| {
                *id == btn
                    && matches!(m, Mutation::SetProp { key, .. } if key == "width" || key == "height")
            })
            .collect();
        assert!(dim_muts.is_empty(), "large button must not be resized");
    }

    #[test]
    fn synthesize_density_adjusts_gap() {
        let mut doc = Document::new();
        let root = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode { id: root, kind: "Stack".into() },
        )
        .unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id: root }).unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::SetProp { id: root, key: "gap".into(), value: Value::Px(10.0) },
        )
        .unwrap();

        let env = make_env(1280.0, 800.0, InputMode::Pointer);
        let profile = UserProfile { density: 1.5, ..Default::default() };

        let result = BasicSynthesizer.synthesize(&doc, &env, &profile);

        let gap_muts: Vec<_> = result
            .mutations
            .iter()
            .filter(|(id, m)| {
                *id == root
                    && matches!(m, Mutation::SetProp { key, value: Value::Px(v), .. }
                        if key == "gap" && (*v - 15.0).abs() < 0.01)
            })
            .collect();
        assert_eq!(gap_muts.len(), 1, "density=1.5 should scale gap from 10 to 15");
    }

    #[test]
    fn synthesize_compact_clips_wide_elements() {
        let mut doc = Document::new();
        let root = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode { id: root, kind: "Stack".into() },
        )
        .unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id: root }).unwrap();

        let panel = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode { id: panel, kind: "Rect".into() },
        )
        .unwrap();
        // Panel wider than compact window (390px).
        doc.apply_from(
            Origin::System,
            Mutation::SetProp { id: panel, key: "width".into(), value: Value::Px(800.0) },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::AppendChild { parent: root, child: panel },
        )
        .unwrap();

        // Compact window: 390px wide.
        let env = make_env(390.0, 844.0, InputMode::Touch);
        let profile = UserProfile::default();

        let result = BasicSynthesizer.synthesize(&doc, &env, &profile);

        let clip_muts: Vec<_> = result
            .mutations
            .iter()
            .filter(|(id, m)| {
                *id == panel
                    && matches!(m, Mutation::SetProp { key, value: Value::Px(v), .. }
                        if key == "width" && (*v - (390.0 - 32.0)).abs() < 0.01)
            })
            .collect();
        assert_eq!(clip_muts.len(), 1, "compact env should clip element wider than window");
    }

    #[test]
    fn apply_mutations_via_origin_ai() {
        let (mut doc, text_id) = make_doc_with_text(16.0);
        let env = make_env(1280.0, 800.0, InputMode::Pointer);
        let profile = UserProfile { text_scale: 2.0, high_contrast: true, ..Default::default() };

        let result = BasicSynthesizer.synthesize(&doc, &env, &profile);
        let log_before = doc.audit_log().len();

        let applied = apply(&mut doc, result);

        // At least 2 mutations: size scale + high-contrast color.
        assert!(applied >= 2, "expected at least 2 mutations applied, got {applied}");

        // All new edits are attributed to Ai.
        let new_edits = &doc.audit_log()[log_before..];
        assert!(
            new_edits.iter().all(|e| e.origin == Origin::Ai),
            "all synthesis mutations must carry Origin::Ai"
        );

        // The text node now has the scaled size.
        let text_node = doc.get(text_id).unwrap();
        assert_eq!(
            text_node.props.get("size"),
            Some(&Value::Px(32.0)),
            "size should be 16*2=32 after apply"
        );
        // And pure-white color for high contrast.
        assert_eq!(
            text_node.props.get("color"),
            Some(&Value::Color(0xFFFF_FFFF)),
            "color should be pure white after high-contrast apply"
        );
    }
}
