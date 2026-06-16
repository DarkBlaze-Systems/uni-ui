//! # uni-widgets — the DarkBlaze Uni-UI widget library
//!
//! Composite, styled, interactive widgets, each authored as a [`uni_ir`]
//! subtree built from design [`uni_tokens`]. Every widget here is a **builder
//! function** with the same shape:
//!
//! ```ignore
//! fn widget(doc: &mut Document, tokens: &Tokens, ...) -> NodeId
//! ```
//!
//! A builder allocates fresh node ids, emits the subtree via
//! [`Mutation`]s attributed to [`Origin::System`] (widgets are library-
//! authored chrome, not human or AI edits), and returns the **root**
//! [`NodeId`] of the subtree. The caller then appends that root wherever it
//! wants in its own tree — the builder never sets a document root or attaches
//! itself anywhere.
//!
//! ## Why container kinds
//!
//! In `uni-core`, only `Stack` / `Column` / `Row` / `Grid` are flex/grid
//! **containers** — their children get laid out and receive computed rects.
//! Leaf kinds (`Button`, `Rect`, `Text`, `Frost`) do *not* lay out children.
//! So a composite widget that needs its parts positioned (a button with a
//! centered label, a checkbox with a box beside text) is built on a **container
//! kind** (`Stack` / `Row` / `Column`), never on a leaf. A "button" is a styled
//! `Stack`, not a `Button` leaf — that is the only way its `Text` child gets a
//! rect.

use uni_ir::{Action, Binding, Document, Mutation, NodeId, Origin, Value};
use uni_tokens::Tokens;

// ---------------------------------------------------------------------------
// internal emit helpers — all edits are Origin::System (library-authored chrome)
// ---------------------------------------------------------------------------

/// Create a fresh node of `kind`, returning its id. Panics only on an IR
/// invariant violation (a fresh id can never collide), which is a bug here.
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

/// Set a property on a node.
fn prop(doc: &mut Document, id: NodeId, key: &str, value: Value) {
    doc.apply_from(
        Origin::System,
        Mutation::SetProp {
            id,
            key: key.into(),
            value,
        },
    )
    .expect("node exists (just created)");
}

/// Append `child` under `parent`.
fn append(doc: &mut Document, parent: NodeId, child: NodeId) {
    doc.apply_from(Origin::System, Mutation::AppendChild { parent, child })
        .expect("both nodes exist");
}

/// Register a fired-event callback on a node.
fn callback(doc: &mut Document, id: NodeId, event: &str, action: Action) {
    doc.apply_from(
        Origin::System,
        Mutation::SetCallback {
            id,
            event: event.into(),
            action,
        },
    )
    .expect("node exists");
}

/// Bind a dynamic property source on a node.
fn binding(doc: &mut Document, id: NodeId, key: &str, expr: &str) {
    doc.apply_from(
        Origin::System,
        Mutation::SetBinding {
            id,
            key: key.into(),
            binding: Binding { expr: expr.into() },
        },
    )
    .expect("node exists");
}

// ---------------------------------------------------------------------------
// label
// ---------------------------------------------------------------------------

/// A piece of static text styled from the token type ramp.
///
/// Builds a single `Text` leaf whose `size` comes from a semantic role —
/// `title` when `emphasized`, `body` otherwise — run through the type scale's
/// `font_scale`, and whose `color` is the ink palette. Returns the `Text` id.
pub fn label(doc: &mut Document, tokens: &Tokens, text: &str, emphasized: bool) -> NodeId {
    let role = if emphasized {
        tokens.r#type.title
    } else {
        tokens.r#type.body
    };
    let style = if emphasized {
        role.emphasized
    } else {
        role.base
    };
    let size = tokens.r#type.scaled(style.size);

    let t = create(doc, "Text");
    prop(doc, t, "content", Value::Text(text.into()));
    prop(doc, t, "size", Value::Px(size));
    prop(doc, t, "weight", Value::Int(style.weight as i64));
    prop(doc, t, "color", Value::Color(tokens.palette.ink));
    t
}

// ---------------------------------------------------------------------------
// button
// ---------------------------------------------------------------------------

/// An accent-filled, rounded, padded button.
///
/// Built as a `Stack` **container** (so its label child is laid out and
/// centered) painted in `tokens.palette.accent`, with `padding` from the
/// spacing scale and `corner_radius` from the shape scale. A centered `Text`
/// child carries `label`, inked against the accent in substrate. A `"click"`
/// callback fires the named `on_click` action (no args).
pub fn button(doc: &mut Document, tokens: &Tokens, label: &str, on_click: &str) -> NodeId {
    let root = create(doc, "Stack");
    prop(doc, root, "background", Value::Color(tokens.palette.accent));
    prop(doc, root, "padding", Value::Px(tokens.space.comfy));
    prop(doc, root, "corner_radius", Value::Px(tokens.shape.medium));
    // Center the label within the button on both axes.
    prop(doc, root, "align", Value::Text("center".into()));
    prop(doc, root, "justify", Value::Text("center".into()));

    callback(
        doc,
        root,
        "click",
        Action {
            name: on_click.into(),
            args: vec![],
        },
    );

    // Centered label. On the accent fill, read against substrate (white).
    let text = create(doc, "Text");
    prop(doc, text, "content", Value::Text(label.into()));
    prop(
        doc,
        text,
        "size",
        Value::Px(tokens.r#type.scaled(tokens.r#type.body.emphasized.size)),
    );
    prop(
        doc,
        text,
        "weight",
        Value::Int(tokens.r#type.body.emphasized.weight as i64),
    );
    prop(doc, text, "color", Value::Color(tokens.palette.substrate));
    append(doc, root, text);

    root
}

// ---------------------------------------------------------------------------
// checkbox
// ---------------------------------------------------------------------------

/// A labelled checkbox: a small box beside its caption.
///
/// Built as a `Row` **container** (box and caption laid out side by side) with
/// a `gap` from the spacing scale. The box is a `Rect` with a small corner
/// radius whose `filled`/`checked` state is bound to `state_key` (so the
/// reactor drives its fill). A `"click"` callback fires `toggle(state_key)`,
/// the caption is a token-styled `label`. Returns the `Row` id.
pub fn checkbox(doc: &mut Document, tokens: &Tokens, label_text: &str, state_key: &str) -> NodeId {
    let row = create(doc, "Row");
    prop(doc, row, "gap", Value::Px(tokens.space.snug));
    prop(doc, row, "align", Value::Text("center".into()));
    callback(
        doc,
        row,
        "click",
        Action {
            name: "toggle".into(),
            args: vec![Value::Text(state_key.into())],
        },
    );

    // The check box itself: a small square Rect with a soft corner.
    let r#box = create(doc, "Rect");
    let side = tokens.space.comfy;
    prop(doc, r#box, "width", Value::Px(side));
    prop(doc, r#box, "height", Value::Px(side));
    prop(doc, r#box, "corner_radius", Value::Px(tokens.shape.small));
    prop(doc, r#box, "color", Value::Color(tokens.palette.accent));
    // Bind its filled/checked state to the supplied state key.
    binding(doc, r#box, "checked", state_key);
    append(doc, row, r#box);

    // The caption.
    let caption = label(doc, tokens, label_text, false);
    append(doc, row, caption);

    row
}

// ---------------------------------------------------------------------------
// card
// ---------------------------------------------------------------------------

/// An elevated / frosted surface ready to hold content.
///
/// Built as a `Stack` **container** with `padding` and a large `corner_radius`
/// from the token scales, painted in substrate. A `Frost` child sits behind as
/// the backdrop blur (absolute-positioned to fill, so it covers the card's
/// extent without taking flow space). The caller appends content children to
/// the returned `Stack` id; those children lay out in the column flow above the
/// frost backdrop.
pub fn card(doc: &mut Document, tokens: &Tokens) -> NodeId {
    let root = create(doc, "Stack");
    prop(doc, root, "background", Value::Color(tokens.palette.substrate));
    prop(doc, root, "padding", Value::Px(tokens.space.comfy));
    prop(doc, root, "corner_radius", Value::Px(tokens.shape.large));
    prop(doc, root, "gap", Value::Px(tokens.space.snug));

    // Frosted backdrop: absolute so it fills the card without consuming flow.
    let frost = create(doc, "Frost");
    prop(doc, frost, "position", Value::Text("absolute".into()));
    prop(doc, frost, "left", Value::Px(0.0));
    prop(doc, frost, "top", Value::Px(0.0));
    prop(doc, frost, "right", Value::Px(0.0));
    prop(doc, frost, "bottom", Value::Px(0.0));
    prop(doc, frost, "corner_radius", Value::Px(tokens.shape.large));
    prop(doc, frost, "tint", Value::Color(tokens.palette.glow));
    append(doc, root, frost);

    root
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// A reactive list bound to a state collection.
///
/// Built as a `Column` **container** holding a single `For` node whose `items`
/// are bound to `items_key`; the reactor expands the `For` once per item,
/// cloning the template child (a token-styled `label`). The `Column` carries a
/// `gap` so expanded rows breathe. Returns the `Column` id.
pub fn list(doc: &mut Document, tokens: &Tokens, items_key: &str) -> NodeId {
    let col = create(doc, "Column");
    prop(doc, col, "gap", Value::Px(tokens.space.snug));

    // The repeater: bound to the items collection, expanded by the reactor.
    let r#for = create(doc, "For");
    binding(doc, r#for, "items", items_key);
    append(doc, col, r#for);

    // Template child: one styled label cloned per item.
    let template = label(doc, tokens, "", false);
    append(doc, r#for, template);

    col
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use uni_tokens::{Tokens, Variant};

    fn toks() -> Tokens {
        Tokens::for_variant(Variant::Internal)
    }

    /// A widget is "container-kind" iff uni-core treats it as a flex/grid
    /// container — i.e. its children get laid out.
    fn is_container_kind(kind: &str) -> bool {
        matches!(kind, "Stack" | "Column" | "Row" | "Grid")
    }

    #[test]
    fn button_is_a_container_with_click_and_a_text_label() {
        let mut doc = Document::new();
        let t = toks();
        let b = button(&mut doc, &t, "Save", "save_doc");

        let node = doc.get(b).unwrap();
        // Root is a container kind so its child lays out.
        assert!(is_container_kind(&node.kind), "kind was {}", node.kind);

        // Accent fill, padded, rounded.
        assert_eq!(
            node.props.get("background"),
            Some(&Value::Color(t.palette.accent))
        );
        assert!(node.props.get("padding").is_some());
        assert!(node.props.get("corner_radius").is_some());

        // A "click" callback firing the supplied action name.
        let click = node.callbacks.get("click").expect("has click callback");
        assert_eq!(click.name, "save_doc");
        assert!(click.args.is_empty());

        // Exactly one child, a Text whose content == the label.
        assert_eq!(node.children.len(), 1);
        let child = doc.get(node.children[0]).unwrap();
        assert_eq!(child.kind, "Text");
        assert_eq!(
            child.props.get("content"),
            Some(&Value::Text("Save".into()))
        );
    }

    #[test]
    fn label_produces_text_with_token_derived_size() {
        let mut doc = Document::new();
        let t = toks();

        let l = label(&mut doc, &t, "Body copy", false);
        let node = doc.get(l).unwrap();
        assert_eq!(node.kind, "Text");
        assert_eq!(
            node.props.get("content"),
            Some(&Value::Text("Body copy".into()))
        );
        // Size is the body role's scaled size.
        let expected = t.r#type.scaled(t.r#type.body.base.size);
        assert_eq!(node.props.get("size"), Some(&Value::Px(expected)));
        assert_eq!(node.props.get("color"), Some(&Value::Color(t.palette.ink)));

        // Emphasized pulls the heavier title role → a larger size.
        let e = label(&mut doc, &t, "Title", true);
        let enode = doc.get(e).unwrap();
        let title_size = t.r#type.scaled(t.r#type.title.emphasized.size);
        assert_eq!(enode.props.get("size"), Some(&Value::Px(title_size)));
        assert!(title_size > expected, "title should outsize body");
    }

    #[test]
    fn checkbox_has_row_box_binding_and_callback() {
        let mut doc = Document::new();
        let t = toks();
        let c = checkbox(&mut doc, &t, "Enabled", "settings.enabled");

        let row = doc.get(c).unwrap();
        assert_eq!(row.kind, "Row");
        assert!(is_container_kind(&row.kind));

        // toggle(state_key) on click.
        let click = row.callbacks.get("click").expect("click callback");
        assert_eq!(click.name, "toggle");
        assert_eq!(click.args, vec![Value::Text("settings.enabled".into())]);

        // Row has the box (Rect) then the caption (Text).
        assert_eq!(row.children.len(), 2);
        let r#box = doc.get(row.children[0]).unwrap();
        assert_eq!(r#box.kind, "Rect");
        // The box's checked state is bound to the state key.
        assert_eq!(
            r#box.bindings.get("checked"),
            Some(&Binding {
                expr: "settings.enabled".into()
            })
        );

        let caption = doc.get(row.children[1]).unwrap();
        assert_eq!(caption.kind, "Text");
        assert_eq!(
            caption.props.get("content"),
            Some(&Value::Text("Enabled".into()))
        );
    }

    #[test]
    fn card_is_a_container_ready_for_children() {
        let mut doc = Document::new();
        let t = toks();
        let card_id = card(&mut doc, &t);

        let node = doc.get(card_id).unwrap();
        assert!(is_container_kind(&node.kind), "kind was {}", node.kind);
        assert!(node.props.get("padding").is_some());
        assert!(node.props.get("corner_radius").is_some());

        // A frost backdrop child is present.
        assert!(node
            .children
            .iter()
            .any(|&c| doc.get(c).unwrap().kind == "Frost"));

        // Caller can append content; it nests under the container.
        let body = label(&mut doc, &t, "Card body", false);
        append(&mut doc, card_id, body);
        assert!(doc.get(card_id).unwrap().children.contains(&body));
        assert_eq!(doc.get(body).unwrap().parent, Some(card_id));
    }

    #[test]
    fn list_is_a_column_with_a_bound_for_and_template() {
        let mut doc = Document::new();
        let t = toks();
        let l = list(&mut doc, &t, "todos");

        let col = doc.get(l).unwrap();
        assert_eq!(col.kind, "Column");
        assert!(is_container_kind(&col.kind));

        // It contains a For node bound to the items key.
        assert_eq!(col.children.len(), 1);
        let r#for = doc.get(col.children[0]).unwrap();
        assert_eq!(r#for.kind, "For");
        assert_eq!(
            r#for.bindings.get("items"),
            Some(&Binding {
                expr: "todos".into()
            })
        );

        // The For has a template child (a label Text) the reactor clones.
        assert_eq!(r#for.children.len(), 1);
        assert_eq!(doc.get(r#for.children[0]).unwrap().kind, "Text");
    }

    /// Widgets compose: a card holding a button + a checkbox nests cleanly,
    /// and every interactive part keeps its callbacks/bindings after nesting.
    #[test]
    fn widgets_nest_correctly() {
        let mut doc = Document::new();
        let t = toks();

        let card_id = card(&mut doc, &t);
        let title = label(&mut doc, &t, "Settings", true);
        let chk = checkbox(&mut doc, &t, "Dark mode", "ui.dark");
        let save = button(&mut doc, &t, "Save", "persist");

        append(&mut doc, card_id, title);
        append(&mut doc, card_id, chk);
        append(&mut doc, card_id, save);

        let card_node = doc.get(card_id).unwrap();
        // frost backdrop + the three appended widgets.
        assert!(card_node.children.contains(&title));
        assert!(card_node.children.contains(&chk));
        assert!(card_node.children.contains(&save));

        // Nesting preserved structure: button still has its click + label.
        assert!(doc.get(save).unwrap().callbacks.contains_key("click"));
        assert_eq!(doc.get(save).unwrap().children.len(), 1);

        // Checkbox still has its box binding.
        let chk_box = doc.get(chk).unwrap().children[0];
        assert!(doc.get(chk_box).unwrap().bindings.contains_key("checked"));
    }
}
