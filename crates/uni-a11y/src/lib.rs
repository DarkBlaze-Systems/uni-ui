//! # uni-a11y — the accessibility seam (THE FLOW, rung 5)
//!
//! Accessibility is first-class from day one. This crate is the bridge between
//! our two canonical descriptions of a UI — the [`uni_ir::Document`] (the
//! *what*: node tree + properties + callbacks) and the [`uni_core::Layout`]
//! (the *where*: every node's absolute computed rect) — and the platform
//! accessibility world, modeled with [`accesskit`] (MIT/Apache-2.0, permissive).
//!
//! The single entry point is [`build_tree`]: it walks the laid-out document and
//! emits an [`accesskit::TreeUpdate`] whose nodes mirror the IR tree one-to-one,
//! each carrying a screen-reader role, a bounding box, a name, and — for
//! interactive nodes — an actionable [`accesskit::Action`].
//!
//! Wiring this update to the OS (via `accesskit_winit` and friends) is a later
//! integration step; this crate only *builds the tree*. That separation is the
//! point: the a11y tree is a pure function of IR + layout, so it can be tested
//! headlessly and reused by any platform adapter.
//!
//! ## IR `kind` → accesskit [`Role`] mapping
//!
//! | IR `kind`                          | accesskit `Role`   |
//! |------------------------------------|--------------------|
//! | `Text`                             | `Label`            |
//! | `Button`                           | `Button`           |
//! | `Stack` / `Row` / `Column` / `Grid`| `Group`            |
//! | `Rect` / `Frost` / `FrostedRect`   | `GenericContainer` (decorative) |
//! | anything else                      | `GenericContainer` |

/// Re-export so downstream crates can name the return type without a direct accesskit dep.
pub use accesskit::TreeUpdate;
use accesskit::{Action, Node as A11yNode, NodeId as A11yNodeId, Rect, Role, Tree, TreeId};

use uni_core::{ComputedRect, Layout};
use uni_ir::{Document, NodeId, Value};

/// Map an IR node `kind` to the accessibility [`Role`] a screen reader should
/// announce.
///
/// Containers collapse to `Group`; pure drawing primitives (`Rect`, `Frost`)
/// are decorative and map to `GenericContainer` so they don't get announced as
/// meaningful structure. Unknown kinds default to `GenericContainer`.
pub fn role_for_kind(kind: &str) -> Role {
    match kind {
        "Text" => Role::Label,
        "Button" => Role::Button,
        "Stack" | "Row" | "Column" | "Grid" => Role::Group,
        // Decorative drawing leaves — present for hit-testing/structure but not
        // semantically meaningful, so a generic (effectively transparent) role.
        "Rect" | "Frost" | "FrostedRect" => Role::GenericContainer,
        _ => Role::GenericContainer,
    }
}

/// The synthetic accesskit id of the tree root window. IR ids are offset by one
/// (`ir.0 + 1`) so this id can never collide with a real node — see [`a11y_id`].
const ROOT_ID: A11yNodeId = A11yNodeId(0);

/// Convert an IR [`NodeId`] into the accesskit [`NodeId`] used in the tree.
///
/// Offset by 1 so it never collides with the synthetic [`ROOT_ID`].
fn a11y_id(id: NodeId) -> A11yNodeId {
    A11yNodeId(id.0 + 1)
}

/// Read a human-readable name for a node, preferring an explicit `label`/`name`
/// prop, then a `Text` node's `content`.
fn name_of(doc: &Document, id: NodeId) -> Option<String> {
    let node = doc.get(id)?;
    let text = |key: &str| match node.props.get(key) {
        Some(Value::Text(s)) => Some(s.clone()),
        _ => None,
    };
    text("label")
        .or_else(|| text("name"))
        .or_else(|| text("content"))
}

/// Translate a [`ComputedRect`] (top-left origin, w/h) into an accesskit
/// [`Rect`] (min/max corners, y-down).
fn bounds_of(r: ComputedRect) -> Rect {
    Rect {
        x0: r.x as f64,
        y0: r.y as f64,
        x1: (r.x + r.w) as f64,
        y1: (r.y + r.h) as f64,
    }
}

/// Build the accesskit [`Node`] for a single IR node, *without* recursing into
/// children (the caller wires children + walks the tree).
///
/// Returns `None` if the node has no computed rect (was not laid out) or is
/// missing from the document.
fn build_node(doc: &Document, layout: &Layout, id: NodeId) -> Option<A11yNode> {
    let ir = doc.get(id)?;
    let rect = layout.rect(id)?;

    let mut node = A11yNode::new(role_for_kind(&ir.kind));
    node.set_bounds(bounds_of(rect));

    if let Some(name) = name_of(doc, id) {
        node.set_label(name);
    }

    // Interactive nodes (anything carrying a `click` callback) are made
    // focusable + actionable: a screen reader can activate them, and the
    // platform Click/Default action is supported.
    if ir.callbacks.contains_key("click") {
        node.add_action(Action::Focus);
        node.add_action(Action::Click);
    }

    Some(node)
}

/// Recursively emit `id` and its laid-out descendants into `out`, returning the
/// accesskit id of `id` if it was laid out (so the parent can list it as a
/// child). Children that were not laid out are simply skipped.
fn walk(
    doc: &Document,
    layout: &Layout,
    id: NodeId,
    out: &mut Vec<(A11yNodeId, A11yNode)>,
) -> Option<A11yNodeId> {
    let mut node = build_node(doc, layout, id)?;

    if let Some(ir) = doc.get(id) {
        let children: Vec<A11yNodeId> = ir
            .children
            .iter()
            .filter_map(|&c| walk(doc, layout, c, out))
            .collect();
        if !children.is_empty() {
            node.set_children(children);
        }
    }

    let aid = a11y_id(id);
    out.push((aid, node));
    Some(aid)
}

/// Build an accessibility [`TreeUpdate`] from a [`uni_ir::Document`] and its
/// computed [`uni_core::Layout`].
///
/// The update contains one accesskit node per laid-out IR node, mirroring the
/// IR tree, parented under a synthetic `Window` root (so the document tree hangs
/// off a single, stable root the platform can anchor to). Each node carries its
/// role, bounds, name, and any interactive actions.
///
/// When `focused` is `Some(id)`, the tree update's `focus` field is set to the
/// accesskit id of that IR node (so the platform highlights it). When `None`,
/// focus defaults to the synthetic root (no node focused).
///
/// Feed the returned update to an `accesskit` platform adapter
/// (e.g. `accesskit_winit`) to expose the UI to assistive technology.
pub fn build_tree(doc: &Document, layout: &Layout, focused: Option<NodeId>) -> TreeUpdate {
    let mut nodes: Vec<(A11yNodeId, A11yNode)> = Vec::new();

    // Synthetic window root: the document's root (if any, and if laid out) hangs
    // beneath it. Using a dedicated root keeps the platform anchor stable even
    // when the document root changes or the document is empty.
    let mut root = A11yNode::new(Role::Window);

    if let Some(doc_root) = doc.root() {
        if let Some(child) = walk(doc, layout, doc_root, &mut nodes) {
            root.set_children(vec![child]);
        }
    }

    nodes.push((ROOT_ID, root));

    let focus = match focused {
        Some(id) => a11y_id(id),
        None => ROOT_ID,
    };

    TreeUpdate {
        nodes,
        tree: Some(Tree::new(ROOT_ID)),
        tree_id: TreeId::ROOT,
        focus,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uni_ir::{Action as IrAction, Mutation, Origin};

    fn node(doc: &mut Document, kind: &str) -> NodeId {
        let id = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id,
                kind: kind.into(),
            },
        )
        .unwrap();
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
        .unwrap();
    }

    fn child(doc: &mut Document, parent: NodeId, c: NodeId) {
        doc.apply_from(Origin::System, Mutation::AppendChild { parent, child: c })
            .unwrap();
    }

    fn set_root(doc: &mut Document, id: NodeId) {
        doc.apply_from(Origin::System, Mutation::SetRoot { id })
            .unwrap();
    }

    /// Find the built node for an IR id in a TreeUpdate's node list.
    fn find(update: &TreeUpdate, ir: NodeId) -> &A11yNode {
        let want = a11y_id(ir);
        &update
            .nodes
            .iter()
            .find(|(id, _)| *id == want)
            .expect("node present in tree update")
            .1
    }

    #[test]
    fn role_mapping_covers_the_doctrine_table() {
        assert_eq!(role_for_kind("Text"), Role::Label);
        assert_eq!(role_for_kind("Button"), Role::Button);
        assert_eq!(role_for_kind("Stack"), Role::Group);
        assert_eq!(role_for_kind("Row"), Role::Group);
        assert_eq!(role_for_kind("Column"), Role::Group);
        assert_eq!(role_for_kind("Grid"), Role::Group);
        assert_eq!(role_for_kind("Rect"), Role::GenericContainer);
        assert_eq!(role_for_kind("Frost"), Role::GenericContainer);
        assert_eq!(role_for_kind("FrostedRect"), Role::GenericContainer);
        assert_eq!(role_for_kind("Whatever"), Role::GenericContainer);
    }

    /// The canonical seam test: Stack > [Text "Hi", Button(click)].
    /// Lay it out, build the a11y tree, and assert role/name/bounds/actions and
    /// correct nesting.
    #[test]
    fn builds_tree_from_ir_and_layout() {
        let mut doc = Document::new();

        let root = node(&mut doc, "Stack");
        set_root(&mut doc, root);

        let text = node(&mut doc, "Text");
        prop(&mut doc, text, "content", Value::Text("Hi".into()));
        child(&mut doc, root, text);

        let btn = node(&mut doc, "Button");
        // Give the button a label and a click callback so it's actionable.
        prop(&mut doc, btn, "label", Value::Text("Go".into()));
        doc.apply_from(
            Origin::System,
            Mutation::SetCallback {
                id: btn,
                event: "click".into(),
                action: IrAction {
                    name: "submit".into(),
                    args: vec![],
                },
            },
        )
        .unwrap();
        child(&mut doc, root, btn);

        let layout = uni_core::layout(&doc, (800.0, 600.0));
        let update = build_tree(&doc, &layout, None);

        // ---- Text node: Label role, name "Hi", bounds matching the layout. ----
        let a_text = find(&update, text);
        assert_eq!(a_text.role(), Role::Label);
        assert_eq!(a_text.label(), Some("Hi"));

        let lr = layout.rect(text).expect("text laid out");
        let b = a_text.bounds().expect("text has bounds");
        assert_eq!(b.x0, lr.x as f64);
        assert_eq!(b.y0, lr.y as f64);
        assert_eq!(b.x1, (lr.x + lr.w) as f64);
        assert_eq!(b.y1, (lr.y + lr.h) as f64);

        // ---- Button: Button role, actionable (Click + Focus). ----
        let a_btn = find(&update, btn);
        assert_eq!(a_btn.role(), Role::Button);
        assert_eq!(a_btn.label(), Some("Go"));
        assert!(
            a_btn.supports_action(Action::Click),
            "button should support Click"
        );
        assert!(
            a_btn.supports_action(Action::Focus),
            "button should be focusable"
        );

        // The Text node, with no callback, is NOT actionable.
        assert!(
            !a_text.supports_action(Action::Click),
            "text should not support Click"
        );

        // ---- Nesting: window root -> Stack -> [Text, Button]. ----
        let window = &update
            .nodes
            .iter()
            .find(|(id, _)| *id == ROOT_ID)
            .unwrap()
            .1;
        assert_eq!(window.children(), &[a11y_id(root)]);

        let a_root = find(&update, root);
        assert_eq!(a_root.role(), Role::Group);
        assert_eq!(a_root.children(), &[a11y_id(text), a11y_id(btn)]);

        // The update is rooted at our synthetic window and focuses it.
        assert_eq!(update.tree.as_ref().unwrap().root, ROOT_ID);
        assert_eq!(update.focus, ROOT_ID);
    }

    /// **F2 — every Button is announceable and actionable.** For *every* IR
    /// node that maps to [`Role::Button`], the emitted accesskit node must carry
    /// a non-empty name (so a screen reader can speak it) and at least one
    /// actionable [`Action`] (so it can be activated). A button a screen-reader
    /// can neither name nor press is a dead control; this guards against it.
    #[test]
    fn every_button_has_a_name_and_an_action() {
        let mut doc = Document::new();

        let root = node(&mut doc, "Stack");
        set_root(&mut doc, root);

        // A button named via `label`, with a click callback.
        let save = node(&mut doc, "Button");
        prop(&mut doc, save, "label", Value::Text("Save".into()));
        doc.apply_from(
            Origin::System,
            Mutation::SetCallback {
                id: save,
                event: "click".into(),
                action: IrAction {
                    name: "save".into(),
                    args: vec![],
                },
            },
        )
        .unwrap();
        child(&mut doc, root, save);

        // A second button named via the `name` prop, also clickable.
        let cancel = node(&mut doc, "Button");
        prop(&mut doc, cancel, "name", Value::Text("Cancel".into()));
        doc.apply_from(
            Origin::System,
            Mutation::SetCallback {
                id: cancel,
                event: "click".into(),
                action: IrAction {
                    name: "cancel".into(),
                    args: vec![],
                },
            },
        )
        .unwrap();
        child(&mut doc, root, cancel);

        let layout = uni_core::layout(&doc, (800.0, 600.0));
        let update = build_tree(&doc, &layout, None);

        // Walk every emitted node; for each one whose role is Button, assert it
        // has a non-empty name and supports an actionable Click.
        let mut buttons_seen = 0;
        for (aid, anode) in &update.nodes {
            if anode.role() == Role::Button {
                buttons_seen += 1;
                assert!(
                    anode.label().map(|s| !s.is_empty()).unwrap_or(false),
                    "Button {aid:?} must have a non-empty name"
                );
                assert!(
                    anode.supports_action(Action::Click),
                    "Button {aid:?} must support an actionable Click"
                );
            }
        }
        assert_eq!(buttons_seen, 2, "both Button nodes should be in the tree");
    }

    #[test]
    fn empty_document_yields_just_a_window_root() {
        let doc = Document::new();
        let layout = uni_core::layout(&doc, (640.0, 480.0));
        let update = build_tree(&doc, &layout, None);

        assert_eq!(update.nodes.len(), 1);
        let (id, win) = &update.nodes[0];
        assert_eq!(*id, ROOT_ID);
        assert_eq!(win.role(), Role::Window);
        assert!(win.children().is_empty());
    }
}
