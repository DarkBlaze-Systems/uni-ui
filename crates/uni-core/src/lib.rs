//! # uni-core — the lowering layer
//!
//! Walks a `uni-ir` [`Document`] and lowers it into a `uni-render` [`Scene`]
//! (a flat, painter's-order list of draw commands). This is the bridge between
//! *what the UI is* (the IR) and *what gets drawn* (the Scene the renderer sees).
//!
//! v0 is a deliberately naive **top-down vertical-stack** layout: a `Stack`
//! root with `Text` / `Rect` children, honoring `padding`, `spacing`,
//! `background`, and per-child sizing/color props. Real constraint layout
//! (taffy), nested containers, the retained tree (focus/hit-test/a11y), and
//! reactive re-lowering driven by `uni-react` are the next milestones — but
//! this is enough to render a real `.uni` file end-to-end.

use uni_ir::{Document, Node, Value};
use uni_render::{DrawCmd, Scene};

fn color_of(node: &Node, key: &str) -> Option<u32> {
    match node.props.get(key) {
        Some(Value::Color(c)) => Some(*c),
        _ => None,
    }
}

fn px_of(node: &Node, key: &str) -> Option<f32> {
    match node.props.get(key) {
        Some(Value::Px(v)) => Some(*v),
        Some(Value::Int(v)) => Some(*v as f32),
        Some(Value::Float(v)) => Some(*v as f32),
        _ => None,
    }
}

fn text_of(node: &Node, key: &str) -> Option<String> {
    match node.props.get(key) {
        Some(Value::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Lower a [`Document`] into a [`Scene`] for the given logical viewport size.
///
/// The root's `background` becomes a full-viewport fill (the renderer uses the
/// first full-cover rect as its clear color). Children lay out vertically from
/// the root's `padding`, separated by `spacing`.
pub fn lower(doc: &Document, viewport: (f32, f32)) -> Scene {
    let (vw, vh) = viewport;
    let mut scene: Scene = Vec::new();

    let Some(root_id) = doc.root() else {
        return scene;
    };
    let Some(root) = doc.get(root_id) else {
        return scene;
    };

    // Stack background fills the viewport.
    let bg = color_of(root, "background").unwrap_or(0x0a0a0aff);
    scene.push(DrawCmd::FilledRect {
        x: 0.0,
        y: 0.0,
        w: vw,
        h: vh,
        color: bg,
        corner_radius: 0.0,
    });

    let pad = px_of(root, "padding").unwrap_or(0.0);
    let spacing = px_of(root, "spacing").unwrap_or(8.0);
    let cx = pad;
    let mut cy = pad;

    for &child_id in &root.children {
        let Some(child) = doc.get(child_id) else {
            continue;
        };
        match child.kind.as_str() {
            "Text" => {
                let content = text_of(child, "content").unwrap_or_default();
                let size = px_of(child, "size").unwrap_or(16.0);
                let color = color_of(child, "color").unwrap_or(0xffffffff);
                scene.push(DrawCmd::Text {
                    x: cx,
                    y: cy,
                    content,
                    size,
                    color,
                });
                cy += size * 1.4 + spacing;
            }
            "Rect" => {
                let w = px_of(child, "width").unwrap_or(100.0);
                let h = px_of(child, "height").unwrap_or(40.0);
                let color = color_of(child, "color").unwrap_or(0xffffffff);
                let corner_radius = px_of(child, "corner_radius")
                    .or_else(|| px_of(child, "radius"))
                    .unwrap_or(0.0);
                scene.push(DrawCmd::FilledRect {
                    x: cx,
                    y: cy,
                    w,
                    h,
                    color,
                    corner_radius,
                });
                cy += h + spacing;
            }
            _ => {}
        }
    }

    scene
}

#[cfg(test)]
mod tests {
    use super::*;
    use uni_ir::{Mutation, Origin};

    fn prop(doc: &mut Document, id: uni_ir::NodeId, key: &str, value: Value) {
        doc.apply_from(
            Origin::System,
            Mutation::SetProp {
                id,
                key: key.into(),
                value,
            },
        )
        .unwrap();
    }

    #[test]
    fn lowers_stack_with_text_and_rect() {
        let mut doc = Document::new();
        let root = doc.fresh_id();
        doc.apply_from(Origin::System, Mutation::CreateNode { id: root, kind: "Stack".into() })
            .unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id: root }).unwrap();
        prop(&mut doc, root, "padding", Value::Px(16.0));
        prop(&mut doc, root, "background", Value::Color(0x0a0a0aff));

        let t = doc.fresh_id();
        doc.apply_from(Origin::System, Mutation::CreateNode { id: t, kind: "Text".into() })
            .unwrap();
        prop(&mut doc, t, "content", Value::Text("Uni-UI".into()));
        prop(&mut doc, t, "size", Value::Px(28.0));
        prop(&mut doc, t, "color", Value::Color(0xffffffff));
        doc.apply_from(Origin::System, Mutation::AppendChild { parent: root, child: t }).unwrap();

        let r = doc.fresh_id();
        doc.apply_from(Origin::System, Mutation::CreateNode { id: r, kind: "Rect".into() })
            .unwrap();
        prop(&mut doc, r, "width", Value::Px(200.0));
        prop(&mut doc, r, "height", Value::Px(80.0));
        prop(&mut doc, r, "color", Value::Color(0x7d39ebff));
        doc.apply_from(Origin::System, Mutation::AppendChild { parent: root, child: r }).unwrap();

        let scene = lower(&doc, (800.0, 600.0));
        assert_eq!(scene.len(), 3, "background + text + rect");

        assert!(matches!(
            scene[0],
            DrawCmd::FilledRect { color: 0x0a0a0aff, w: 800.0, h: 600.0, .. }
        ));
        match &scene[1] {
            DrawCmd::Text { content, color, .. } => {
                assert_eq!(content, "Uni-UI");
                assert_eq!(*color, 0xffffffff);
            }
            other => panic!("expected text, got {other:?}"),
        }
        match &scene[2] {
            DrawCmd::FilledRect { color, w, h, .. } => {
                assert_eq!(*color, 0x7d39ebff);
                assert_eq!(*w, 200.0);
                assert_eq!(*h, 80.0);
            }
            other => panic!("expected rect, got {other:?}"),
        }
    }

    #[test]
    fn empty_document_lowers_to_empty_scene() {
        let doc = Document::new();
        assert!(lower(&doc, (640.0, 480.0)).is_empty());
    }
}
