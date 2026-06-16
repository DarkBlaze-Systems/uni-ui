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

use uni_env::{Env, WidthClass};
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
// adaptive_scaffold
// ---------------------------------------------------------------------------

/// Top-level scaffold that switches layout by [`WidthClass`].
///
/// - **Compact** (`< 600px`): `Column` — nav appended at bottom, body fills above
/// - **Medium** (`[600, 840)px`): `Row` — side rail (nav, 72px wide) + body fills rest
/// - **Expanded** (`>= 840px`): `Row` — side nav (nav, 256px wide) + body fills rest
///
/// The scaffold sets `width` and `height` to 100% of the viewport (via
/// [`Env::vw`] / [`Env::vh`]). The caller builds `nav` and `body` nodes
/// separately and hands them in; this function positions them correctly and
/// returns the root container id.
pub fn adaptive_scaffold(
    doc: &mut Document,
    tokens: &Tokens,
    env: &Env,
    nav: NodeId,
    body: NodeId,
) -> NodeId {
    match env.width_class() {
        WidthClass::Compact => {
            // Column: body fills remaining height above, nav pinned to bottom.
            let root = create(doc, "Column");
            prop(doc, root, "width", Value::Px(env.vw(100.0)));
            prop(doc, root, "height", Value::Px(env.vh(100.0)));
            prop(doc, root, "background", Value::Color(tokens.palette.substrate));

            // Body grows to fill; nav sits at the bottom at natural size.
            prop(doc, body, "grow", Value::Float(1.0));
            append(doc, root, body);
            append(doc, root, nav);
            root
        }
        WidthClass::Medium => {
            // Row: 72px rail + body fills rest.
            let root = create(doc, "Row");
            prop(doc, root, "width", Value::Px(env.vw(100.0)));
            prop(doc, root, "height", Value::Px(env.vh(100.0)));
            prop(doc, root, "background", Value::Color(tokens.palette.substrate));

            prop(doc, nav, "width", Value::Px(72.0));
            prop(doc, nav, "height", Value::Px(env.vh(100.0)));
            prop(doc, body, "grow", Value::Float(1.0));
            append(doc, root, nav);
            append(doc, root, body);
            root
        }
        WidthClass::Expanded => {
            // Row: 256px side nav + body fills rest.
            let root = create(doc, "Row");
            prop(doc, root, "width", Value::Px(env.vw(100.0)));
            prop(doc, root, "height", Value::Px(env.vh(100.0)));
            prop(doc, root, "background", Value::Color(tokens.palette.substrate));

            prop(doc, nav, "width", Value::Px(256.0));
            prop(doc, nav, "height", Value::Px(env.vh(100.0)));
            prop(doc, body, "grow", Value::Float(1.0));
            append(doc, root, nav);
            append(doc, root, body);
            root
        }
    }
}

// ---------------------------------------------------------------------------
// adaptive_nav
// ---------------------------------------------------------------------------

/// Navigation bar / rail / sidebar that morphs by [`WidthClass`].
///
/// Each item in `items` is `(icon_char, label_text)`.
///
/// - **Compact**: horizontal `Row` at bottom — icon + label beneath, each ~72px wide.
/// - **Medium**: vertical `Column` — icon-only 48×48 rects, width = 72px.
/// - **Expanded**: vertical `Column` with icon + label side-by-side, width = 256px.
///
/// Every item carries a `"select"` callback named `nav_select_{i}` (0-indexed).
pub fn adaptive_nav(
    doc: &mut Document,
    tokens: &Tokens,
    env: &Env,
    items: &[(&str, &str)],
) -> NodeId {
    match env.width_class() {
        WidthClass::Compact => {
            // Horizontal bottom bar.
            let root = create(doc, "Row");
            prop(doc, root, "background", Value::Color(tokens.palette.substrate));
            prop(doc, root, "shadow", Value::Px(tokens.space.snug));
            prop(doc, root, "corner_radius", Value::Px(0.0));

            for (i, (icon, lbl)) in items.iter().enumerate() {
                let item = create(doc, "Column");
                prop(doc, item, "width", Value::Px(72.0));
                prop(doc, item, "align", Value::Text("center".into()));
                prop(doc, item, "justify", Value::Text("center".into()));

                // Icon rect.
                let icon_rect = create(doc, "Rect");
                prop(doc, icon_rect, "width", Value::Px(32.0));
                prop(doc, icon_rect, "height", Value::Px(32.0));
                prop(doc, icon_rect, "color", Value::Color(tokens.palette.accent));
                let icon_label = create(doc, "Text");
                prop(doc, icon_label, "content", Value::Text((*icon).into()));
                prop(doc, icon_label, "color", Value::Color(tokens.palette.accent));
                append(doc, item, icon_rect);
                append(doc, item, icon_label);

                // Label text below the icon.
                let text = create(doc, "Text");
                prop(doc, text, "content", Value::Text((*lbl).into()));
                prop(
                    doc,
                    text,
                    "size",
                    Value::Px(tokens.r#type.scaled(tokens.r#type.caption.base.size)),
                );
                prop(doc, text, "color", Value::Color(tokens.palette.ink));
                append(doc, item, text);

                callback(
                    doc,
                    item,
                    "select",
                    Action {
                        name: format!("nav_select_{i}"),
                        args: vec![],
                    },
                );
                append(doc, root, item);
            }
            root
        }
        WidthClass::Medium => {
            // Vertical rail, icon only.
            let root = create(doc, "Column");
            prop(doc, root, "background", Value::Color(tokens.palette.substrate));
            prop(doc, root, "width", Value::Px(72.0));
            prop(doc, root, "align", Value::Text("center".into()));

            for (i, (icon, _lbl)) in items.iter().enumerate() {
                let item = create(doc, "Stack");
                prop(doc, item, "width", Value::Px(48.0));
                prop(doc, item, "height", Value::Px(48.0));
                prop(doc, item, "align", Value::Text("center".into()));
                prop(doc, item, "justify", Value::Text("center".into()));

                let icon_rect = create(doc, "Rect");
                prop(doc, icon_rect, "width", Value::Px(32.0));
                prop(doc, icon_rect, "height", Value::Px(32.0));
                prop(doc, icon_rect, "color", Value::Color(tokens.palette.accent));
                let icon_label = create(doc, "Text");
                prop(doc, icon_label, "content", Value::Text((*icon).into()));
                prop(doc, icon_label, "color", Value::Color(tokens.palette.accent));
                append(doc, item, icon_rect);
                append(doc, item, icon_label);

                callback(
                    doc,
                    item,
                    "select",
                    Action {
                        name: format!("nav_select_{i}"),
                        args: vec![],
                    },
                );
                append(doc, root, item);
            }
            root
        }
        WidthClass::Expanded => {
            // Vertical sidebar, icon + label side-by-side.
            let root = create(doc, "Column");
            prop(doc, root, "background", Value::Color(tokens.palette.substrate));
            prop(doc, root, "width", Value::Px(256.0));
            prop(doc, root, "gap", Value::Px(tokens.space.snug));
            // Header spacer.
            prop(doc, root, "padding", Value::Px(tokens.space.comfy));

            for (i, (icon, lbl)) in items.iter().enumerate() {
                let item = create(doc, "Row");
                prop(doc, item, "gap", Value::Px(tokens.space.snug));
                prop(doc, item, "align", Value::Text("center".into()));

                let icon_rect = create(doc, "Rect");
                prop(doc, icon_rect, "width", Value::Px(32.0));
                prop(doc, icon_rect, "height", Value::Px(32.0));
                prop(doc, icon_rect, "color", Value::Color(tokens.palette.accent));
                let icon_label = create(doc, "Text");
                prop(doc, icon_label, "content", Value::Text((*icon).into()));
                prop(doc, icon_label, "color", Value::Color(tokens.palette.accent));
                append(doc, item, icon_rect);
                append(doc, item, icon_label);

                let text = create(doc, "Text");
                prop(doc, text, "content", Value::Text((*lbl).into()));
                prop(
                    doc,
                    text,
                    "size",
                    Value::Px(tokens.r#type.scaled(tokens.r#type.body.base.size)),
                );
                prop(doc, text, "color", Value::Color(tokens.palette.ink));
                append(doc, item, text);

                callback(
                    doc,
                    item,
                    "select",
                    Action {
                        name: format!("nav_select_{i}"),
                        args: vec![],
                    },
                );
                append(doc, root, item);
            }
            root
        }
    }
}

// ---------------------------------------------------------------------------
// list_detail_pane
// ---------------------------------------------------------------------------

/// Two-column list+detail layout that adapts by [`WidthClass`].
///
/// - **Compact**: `Column` — `list_node` only; `detail_node` is hidden
///   (`width: 0px, height: 0px`) so the caller can swap visibility.
/// - **Medium** / **Expanded**: `Row` — list pane (fixed 320px) + detail pane
///   (`grow: 1.0`).
pub fn list_detail_pane(
    doc: &mut Document,
    tokens: &Tokens,
    env: &Env,
    list_node: NodeId,
    detail_node: NodeId,
) -> NodeId {
    match env.width_class() {
        WidthClass::Compact => {
            let root = create(doc, "Column");
            prop(doc, root, "background", Value::Color(tokens.palette.substrate));
            prop(doc, root, "width", Value::Px(env.vw(100.0)));
            prop(doc, root, "height", Value::Px(env.vh(100.0)));

            // List visible; detail collapsed.
            prop(doc, list_node, "grow", Value::Float(1.0));
            append(doc, root, list_node);

            prop(doc, detail_node, "width", Value::Px(0.0));
            prop(doc, detail_node, "height", Value::Px(0.0));
            append(doc, root, detail_node);

            root
        }
        WidthClass::Medium | WidthClass::Expanded => {
            let root = create(doc, "Row");
            prop(doc, root, "background", Value::Color(tokens.palette.substrate));
            prop(doc, root, "width", Value::Px(env.vw(100.0)));
            prop(doc, root, "height", Value::Px(env.vh(100.0)));

            // List pane: fixed 320px.
            prop(doc, list_node, "width", Value::Px(320.0));
            append(doc, root, list_node);

            // Detail pane: fills the rest.
            prop(doc, detail_node, "grow", Value::Float(1.0));
            append(doc, root, detail_node);

            root
        }
    }
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

    // -----------------------------------------------------------------------
    // adaptive_scaffold tests
    // -----------------------------------------------------------------------

    #[test]
    fn adaptive_scaffold_compact_uses_column_layout() {
        let mut doc = Document::new();
        let t = toks();
        let env = uni_env::Env::for_window(400.0, 800.0); // Compact

        let nav = create(&mut doc, "Stack");
        let body = create(&mut doc, "Stack");
        let scaffold = adaptive_scaffold(&mut doc, &t, &env, nav, body);

        let node = doc.get(scaffold).unwrap();
        assert_eq!(node.kind, "Column", "Compact scaffold must be a Column");
        // Both nav and body are children.
        assert!(node.children.contains(&nav));
        assert!(node.children.contains(&body));
        // In Compact layout body comes first (grows), nav at the bottom.
        assert_eq!(node.children[0], body);
        assert_eq!(node.children[1], nav);
        // Scaffold carries width/height props.
        assert!(node.props.get("width").is_some());
        assert!(node.props.get("height").is_some());
    }

    #[test]
    fn adaptive_scaffold_expanded_uses_row_layout() {
        let mut doc = Document::new();
        let t = toks();
        let env = uni_env::Env::for_window(1024.0, 768.0); // Expanded

        let nav = create(&mut doc, "Stack");
        let body = create(&mut doc, "Stack");
        let scaffold = adaptive_scaffold(&mut doc, &t, &env, nav, body);

        let node = doc.get(scaffold).unwrap();
        assert_eq!(node.kind, "Row", "Expanded scaffold must be a Row");
        // nav at left (256px wide), body fills rest.
        assert_eq!(node.children[0], nav);
        assert_eq!(node.children[1], body);
        assert_eq!(
            doc.get(nav).unwrap().props.get("width"),
            Some(&Value::Px(256.0))
        );
        assert_eq!(
            doc.get(body).unwrap().props.get("grow"),
            Some(&Value::Float(1.0))
        );
    }

    // -----------------------------------------------------------------------
    // adaptive_nav tests
    // -----------------------------------------------------------------------

    #[test]
    fn adaptive_nav_compact_is_row() {
        let mut doc = Document::new();
        let t = toks();
        let env = uni_env::Env::for_window(375.0, 812.0); // Compact

        let items = [("⌂", "Home"), ("⚙", "Settings"), ("👤", "Profile")];
        let nav = adaptive_nav(&mut doc, &t, &env, &items);

        let node = doc.get(nav).unwrap();
        assert_eq!(node.kind, "Row", "Compact nav must be a Row (bottom bar)");
        assert_eq!(node.children.len(), items.len());

        // Each child is a Column item with a "select" callback.
        for (i, &child_id) in node.children.iter().enumerate() {
            let child = doc.get(child_id).unwrap();
            assert_eq!(child.kind, "Column");
            let sel = child.callbacks.get("select").expect("select callback present");
            assert_eq!(sel.name, format!("nav_select_{i}"));
        }
    }

    #[test]
    fn adaptive_nav_expanded_is_column() {
        let mut doc = Document::new();
        let t = toks();
        let env = uni_env::Env::for_window(1280.0, 800.0); // Expanded

        let items = [("⌂", "Home"), ("⚙", "Settings")];
        let nav = adaptive_nav(&mut doc, &t, &env, &items);

        let node = doc.get(nav).unwrap();
        assert_eq!(node.kind, "Column", "Expanded nav must be a Column (sidebar)");
        assert_eq!(
            node.props.get("width"),
            Some(&Value::Px(256.0)),
            "Expanded nav must be 256px wide"
        );
        assert_eq!(node.children.len(), items.len());

        // Each child is a Row with icon + label and a select callback.
        for (i, &child_id) in node.children.iter().enumerate() {
            let child = doc.get(child_id).unwrap();
            assert_eq!(child.kind, "Row");
            let sel = child.callbacks.get("select").expect("select callback");
            assert_eq!(sel.name, format!("nav_select_{i}"));
        }
    }

    // -----------------------------------------------------------------------
    // list_detail_pane tests
    // -----------------------------------------------------------------------

    #[test]
    fn list_detail_pane_compact_hides_detail() {
        let mut doc = Document::new();
        let t = toks();
        let env = uni_env::Env::for_window(390.0, 844.0); // Compact

        let list_node = create(&mut doc, "Column");
        let detail_node = create(&mut doc, "Stack");
        let pane = list_detail_pane(&mut doc, &t, &env, list_node, detail_node);

        let root = doc.get(pane).unwrap();
        assert_eq!(root.kind, "Column", "Compact list-detail must be a Column");
        assert!(root.children.contains(&list_node));
        assert!(root.children.contains(&detail_node));

        // Detail node is hidden (zero dimensions).
        let detail = doc.get(detail_node).unwrap();
        assert_eq!(detail.props.get("width"), Some(&Value::Px(0.0)));
        assert_eq!(detail.props.get("height"), Some(&Value::Px(0.0)));
    }

    #[test]
    fn list_detail_pane_expanded_shows_both() {
        let mut doc = Document::new();
        let t = toks();
        let env = uni_env::Env::for_window(1440.0, 900.0); // Expanded

        let list_node = create(&mut doc, "Column");
        let detail_node = create(&mut doc, "Stack");
        let pane = list_detail_pane(&mut doc, &t, &env, list_node, detail_node);

        let root = doc.get(pane).unwrap();
        assert_eq!(root.kind, "Row", "Expanded list-detail must be a Row");
        assert_eq!(root.children[0], list_node);
        assert_eq!(root.children[1], detail_node);

        // List pane: fixed 320px.
        assert_eq!(
            doc.get(list_node).unwrap().props.get("width"),
            Some(&Value::Px(320.0))
        );
        // Detail pane: grows to fill.
        assert_eq!(
            doc.get(detail_node).unwrap().props.get("grow"),
            Some(&Value::Float(1.0))
        );
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

// ---------------------------------------------------------------------------
// Scroll view
// ---------------------------------------------------------------------------

/// A scrollable container that clips `content_node` to a fixed viewport size.
///
/// The scroll offset is tracked in the `Store` under `scroll_key` (a `Float`).
/// Two callbacks — `"scroll_up"` and `"scroll_down"` — are wired on the root
/// so a runtime handler can bump the offset and re-layout. Actual pixel
/// clipping is the renderer's responsibility; the widget establishes the IR
/// structure and callbacks needed for full scroll support.
pub fn scroll_view(
    doc: &mut Document,
    tokens: &Tokens,
    content_node: NodeId,
    scroll_key: &str,
    width: f32,
    height: f32,
) -> NodeId {
    let root = create(doc, "Stack");
    prop(doc, root, "width", Value::Px(width));
    prop(doc, root, "height", Value::Px(height));
    prop(doc, root, "background", Value::Color(tokens.palette.substrate));
    prop(doc, root, "corner_radius", Value::Px(tokens.shape.medium));
    prop(doc, root, "overflow", Value::Text("clip".into()));
    binding(doc, root, "scroll_offset", scroll_key);
    callback(doc, root, "scroll_up", Action { name: "scroll_up".into(), args: vec![] });
    callback(doc, root, "scroll_down", Action { name: "scroll_down".into(), args: vec![] });
    append(doc, root, content_node);
    root
}

// ---------------------------------------------------------------------------
// Text input
// ---------------------------------------------------------------------------

/// An editable single-line text field.
///
/// The current value is bound to `value_key` in the Store. The field fires a
/// `"submit"` callback (action: `"submit"`) when Enter is pressed, and a
/// `"focus"` callback (action: `"focus"`) when the field is clicked/tapped.
/// Actual character-level input editing is handled by the runtime's key
/// dispatch; this widget establishes the IR structure, bindings, and callbacks.
pub fn text_input(
    doc: &mut Document,
    tokens: &Tokens,
    placeholder: &str,
    value_key: &str,
) -> NodeId {
    let root = create(doc, "Row");
    prop(doc, root, "width", Value::Px(280.0));
    prop(doc, root, "height", Value::Px(40.0));
    prop(doc, root, "background", Value::Color(tokens.palette.substrate));
    prop(doc, root, "corner_radius", Value::Px(tokens.shape.small));
    prop(doc, root, "padding", Value::Px(tokens.space.snug));
    prop(doc, root, "placeholder", Value::Text(placeholder.into()));
    callback(doc, root, "submit", Action { name: "submit".into(), args: vec![] });
    callback(doc, root, "focus",  Action { name: "focus".into(),  args: vec![] });

    let text = create(doc, "Text");
    binding(doc, text, "content", value_key);
    prop(doc, text, "color", Value::Color(tokens.palette.ink));
    prop(doc, text, "size",  Value::Px(tokens.r#type.body.base.size));
    append(doc, root, text);

    root
}

// ---------------------------------------------------------------------------
// Tooltip
// ---------------------------------------------------------------------------

/// A floating tooltip label, absolutely positioned near its anchor.
///
/// Callers append the returned node to a container that overlays the UI
/// (e.g. a `ZStack`-style `Stack`) and set `visible: Bool(true/false)` in
/// the Store via `visible_key` to show/hide it.
pub fn tooltip(
    doc: &mut Document,
    tokens: &Tokens,
    text: &str,
    visible_key: &str,
) -> NodeId {
    let root = create(doc, "Stack");
    prop(doc, root, "background", Value::Color(tokens.palette.ink));
    prop(doc, root, "corner_radius", Value::Px(tokens.shape.small));
    prop(doc, root, "padding", Value::Px(tokens.space.tight));
    prop(doc, root, "position", Value::Text("absolute".into()));
    binding(doc, root, "visible", visible_key);

    let label = create(doc, "Text");
    prop(doc, label, "content", Value::Text(text.into()));
    prop(doc, label, "color", Value::Color(tokens.palette.substrate));
    prop(doc, label, "size",  Value::Px(tokens.r#type.caption.base.size));
    append(doc, root, label);

    root
}

#[cfg(test)]
mod scroll_input_tests {
    use super::*;
    use uni_tokens::Tokens;

    fn tokens() -> Tokens { Tokens::for_variant(uni_tokens::Variant::Internal) }

    #[test]
    fn scroll_view_has_scroll_callbacks() {
        let mut doc = Document::new();
        let content = {
            let id = doc.fresh_id();
            doc.apply_from(Origin::System, uni_ir::Mutation::CreateNode {
                id, kind: "Stack".into(),
            }).unwrap();
            id
        };
        let t = tokens();
        let sv = scroll_view(&mut doc, &t, content, "offset", 400.0, 300.0);
        let n = doc.get(sv).unwrap();
        assert!(n.callbacks.contains_key("scroll_up"), "missing scroll_up");
        assert!(n.callbacks.contains_key("scroll_down"), "missing scroll_down");
        assert_eq!(n.bindings.get("scroll_offset").map(|b| b.expr.as_str()), Some("offset"));
    }

    #[test]
    fn scroll_view_clips_to_size() {
        let mut doc = Document::new();
        let content = {
            let id = doc.fresh_id();
            doc.apply_from(Origin::System, uni_ir::Mutation::CreateNode {
                id, kind: "Stack".into(),
            }).unwrap();
            id
        };
        let t = tokens();
        let sv = scroll_view(&mut doc, &t, content, "offset", 320.0, 240.0);
        let n = doc.get(sv).unwrap();
        assert_eq!(n.props.get("width"), Some(&Value::Px(320.0)));
        assert_eq!(n.props.get("height"), Some(&Value::Px(240.0)));
    }

    #[test]
    fn text_input_has_submit_callback_and_binding() {
        let mut doc = Document::new();
        let t = tokens();
        let ti = text_input(&mut doc, &t, "Search…", "query");
        let n = doc.get(ti).unwrap();
        assert!(n.callbacks.contains_key("submit"), "missing submit callback");
        assert!(n.callbacks.contains_key("focus"), "missing focus callback");
        // The Text child carries the binding.
        let text_child = n.children[0];
        assert_eq!(
            doc.get(text_child).unwrap().bindings.get("content").map(|b| b.expr.as_str()),
            Some("query")
        );
    }

    #[test]
    fn tooltip_visible_binding_wired() {
        let mut doc = Document::new();
        let t = tokens();
        let tip = tooltip(&mut doc, &t, "Save", "show_tip");
        let n = doc.get(tip).unwrap();
        assert_eq!(n.bindings.get("visible").map(|b| b.expr.as_str()), Some("show_tip"));
        assert_eq!(n.children.len(), 1);
    }
}
