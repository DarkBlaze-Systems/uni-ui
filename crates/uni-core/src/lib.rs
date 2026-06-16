//! # uni-core — the layout + lowering layer
//!
//! Turns a `uni-ir` [`Document`] into a `uni-render` [`Scene`] (a flat,
//! painter's-order list of draw commands) in two passes:
//!
//! 1. **Layout** ([`layout`]) — builds a [`taffy`] tree mirroring the IR and
//!    runs real constraint layout (flex / grid / sizing / padding / gap),
//!    producing a [`Layout`]: every node's *absolute* computed rect keyed by
//!    [`NodeId`], in painter's (parent-before-child) order.
//! 2. **Paint** ([`paint`]) — walks those rects and emits the [`Scene`].
//!
//! [`lower`] is the convenience composition `paint(doc, &layout(doc, vp))`,
//! kept stable so existing callers (the renderer example, tests) don't break.
//!
//! [`hit_test`] resolves a point to the topmost node whose computed rect
//! contains it — the seed of focus / pointer routing / a11y.
//!
//! ## kind → layout mapping
//!
//! | IR `kind`                 | taffy node                                    |
//! |---------------------------|-----------------------------------------------|
//! | `Stack` / `Column`        | flex, `flex_direction: Column`                |
//! | `Row`                     | flex, `flex_direction: Row`                   |
//! | `Grid`                    | CSS grid (`columns` prop → N equal columns)   |
//! | `Text`                    | leaf, intrinsic size measured from content    |
//! | `Rect`                    | leaf (`width`/`height` or auto)               |
//! | `Frost` / `FrostedRect`   | leaf (`width`/`height` or auto)               |
//! | anything else             | leaf                                          |

use std::collections::HashMap;

use taffy::prelude::*;

use uni_ir::{Document, Node, NodeId, Value};
use uni_render::{DrawCmd, Scene};

// ---------------------------------------------------------------------------
// Prop readers
// ---------------------------------------------------------------------------

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

fn int_of(node: &Node, key: &str) -> Option<i64> {
    match node.props.get(key) {
        Some(Value::Int(v)) => Some(*v),
        Some(Value::Float(v)) => Some(*v as i64),
        Some(Value::Px(v)) => Some(*v as i64),
        _ => None,
    }
}

fn text_of(node: &Node, key: &str) -> Option<String> {
    match node.props.get(key) {
        Some(Value::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn str_of<'a>(node: &'a Node, key: &str) -> Option<&'a str> {
    match node.props.get(key) {
        Some(Value::Text(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// Is this a flex/grid container kind (as opposed to a drawing leaf)?
fn is_container(kind: &str) -> bool {
    matches!(kind, "Stack" | "Column" | "Row" | "Grid")
}

/// Intrinsic size of a `Text` leaf: a crude monospace-ish metric so text boxes
/// actually occupy space in the layout (`size*0.6` per char wide, `size*1.4`
/// tall). Good enough for v0 — real shaping lives in the renderer.
fn text_intrinsic_size(node: &Node) -> Size<f32> {
    let content = text_of(node, "content").unwrap_or_default();
    let size = px_of(node, "size").unwrap_or(16.0);
    let chars = content.chars().count().max(1) as f32;
    Size {
        width: size * 0.6 * chars,
        height: size * 1.4,
    }
}

// ---------------------------------------------------------------------------
// Computed layout
// ---------------------------------------------------------------------------

/// One node's computed, *absolute* rectangle (top-left origin, logical px).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ComputedRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl ComputedRect {
    /// Does this rect contain `point` (half-open on the far edges)?
    pub fn contains(&self, (px, py): (f32, f32)) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

/// The result of the layout pass: every node's absolute computed rect, plus a
/// painter-ordered list of node ids (parent before child, siblings in order).
#[derive(Clone, Debug, Default)]
pub struct Layout {
    rects: HashMap<NodeId, ComputedRect>,
    /// Painter's order: the order rects should be drawn / hit-tested against.
    /// A node appears *before* its descendants, so later entries are "on top".
    order: Vec<NodeId>,
    viewport: (f32, f32),
}

impl Layout {
    /// The absolute computed rect for `id`, if it was laid out.
    pub fn rect(&self, id: NodeId) -> Option<ComputedRect> {
        self.rects.get(&id).copied()
    }

    /// Nodes in painter's order (parent before children). Last == topmost.
    pub fn order(&self) -> &[NodeId] {
        &self.order
    }

    /// The viewport the layout was computed for.
    pub fn viewport(&self) -> (f32, f32) {
        self.viewport
    }

    /// Number of laid-out nodes.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Layout pass
// ---------------------------------------------------------------------------

/// Translate an IR [`Node`] into a taffy [`Style`].
fn style_for(node: &Node, viewport: (f32, f32), is_root: bool) -> Style {
    let mut style = Style::default();

    // Sizing: explicit width/height → fixed; absent → auto (grow via flex_grow).
    let mut size = Size::auto();
    if let Some(w) = px_of(node, "width") {
        size.width = length(w);
    }
    if let Some(h) = px_of(node, "height") {
        size.height = length(h);
    }
    // The root always fills the viewport.
    if is_root {
        size = Size {
            width: length(viewport.0),
            height: length(viewport.1),
        };
    }
    style.size = size;

    // flex_grow: `grow` prop (Int/Float/Px all accepted).
    if let Some(g) = px_of(node, "grow") {
        style.flex_grow = g;
    }

    // Absolute positioning: `position: "absolute"` + optional `left`/`top`/
    // `right`/`bottom` insets — lets a node float over its siblings (e.g. a
    // frosted-glass panel over a row of rects) instead of taking flow space.
    if str_of(node, "position") == Some("absolute") {
        style.position = Position::Absolute;
        if let Some(v) = px_of(node, "left") {
            style.inset.left = length(v);
        }
        if let Some(v) = px_of(node, "top") {
            style.inset.top = length(v);
        }
        if let Some(v) = px_of(node, "right") {
            style.inset.right = length(v);
        }
        if let Some(v) = px_of(node, "bottom") {
            style.inset.bottom = length(v);
        }
    }

    match node.kind.as_str() {
        "Stack" | "Column" => {
            style.display = Display::Flex;
            style.flex_direction = FlexDirection::Column;
        }
        "Row" => {
            style.display = Display::Flex;
            style.flex_direction = FlexDirection::Row;
        }
        "Grid" => {
            // v0 grid: flex row-wrap fallback (the task explicitly allows this
            // — simpler and robust until full CSS-grid track sizing is wired).
            // `columns` is read so children wrap after that many per row when a
            // child width is set; the flex algorithm handles the rest.
            style.display = Display::Flex;
            style.flex_direction = FlexDirection::Row;
            style.flex_wrap = FlexWrap::Wrap;
            let _ = int_of(node, "columns");
        }
        _ => {
            // Leaf — no display override (stays Flex default, with no children).
        }
    }

    if is_container(&node.kind) {
        if let Some(p) = px_of(node, "padding") {
            style.padding = Rect::length(p);
        }
        // `gap` or `spacing` (alias) → both axes.
        if let Some(g) = px_of(node, "gap").or_else(|| px_of(node, "spacing")) {
            style.gap = Size::length(g);
        }
        // `align` → cross-axis (align_items). `justify` → main-axis.
        if let Some(a) = str_of(node, "align") {
            style.align_items = align_items_of(a);
        }
        if let Some(j) = str_of(node, "justify") {
            style.justify_content = justify_of(j);
        }
    }

    style
}

fn align_items_of(s: &str) -> Option<AlignItems> {
    match s {
        "start" => Some(AlignItems::START),
        "center" => Some(AlignItems::CENTER),
        "end" => Some(AlignItems::END),
        "stretch" => Some(AlignItems::STRETCH),
        _ => None,
    }
}

fn justify_of(s: &str) -> Option<JustifyContent> {
    match s {
        "start" => Some(JustifyContent::START),
        "center" => Some(JustifyContent::CENTER),
        "end" => Some(JustifyContent::END),
        "stretch" => Some(JustifyContent::STRETCH),
        "space-between" => Some(JustifyContent::SPACE_BETWEEN),
        "space-around" => Some(JustifyContent::SPACE_AROUND),
        "space-evenly" => Some(JustifyContent::SPACE_EVENLY),
        _ => None,
    }
}

/// Recursively build the taffy subtree for `id`, recording the IR↔taffy id
/// mapping. Leaves carry their IR id as node context so the measure function
/// can give `Text` an intrinsic size.
fn build_subtree(
    doc: &Document,
    id: NodeId,
    viewport: (f32, f32),
    is_root: bool,
    tree: &mut TaffyTree<NodeId>,
    map: &mut HashMap<taffy::NodeId, NodeId>,
) -> Option<taffy::NodeId> {
    let node = doc.get(id)?;
    let style = style_for(node, viewport, is_root);

    let taffy_id = if is_container(&node.kind) {
        let children: Vec<taffy::NodeId> = node
            .children
            .iter()
            .filter_map(|&c| build_subtree(doc, c, viewport, false, tree, map))
            .collect();
        tree.new_with_children(style, &children).ok()?
    } else {
        tree.new_leaf_with_context(style, id).ok()?
    };

    map.insert(taffy_id, id);
    Some(taffy_id)
}

/// Walk the computed taffy tree, accumulating absolute positions and recording
/// each IR node's [`ComputedRect`] in painter's order.
fn collect_rects(
    tree: &TaffyTree<NodeId>,
    taffy_id: taffy::NodeId,
    map: &HashMap<taffy::NodeId, NodeId>,
    origin: (f32, f32),
    out: &mut Layout,
) {
    let Ok(l) = tree.layout(taffy_id) else {
        return;
    };
    let abs_x = origin.0 + l.location.x;
    let abs_y = origin.1 + l.location.y;

    if let Some(&ir_id) = map.get(&taffy_id) {
        out.rects.insert(
            ir_id,
            ComputedRect {
                x: abs_x,
                y: abs_y,
                w: l.size.width,
                h: l.size.height,
            },
        );
        out.order.push(ir_id);
    }

    if let Ok(children) = tree.children(taffy_id) {
        for child in children {
            collect_rects(tree, child, map, (abs_x, abs_y), out);
        }
    }
}

/// Run real constraint layout over `doc` for the given logical `viewport`,
/// returning every node's absolute [`ComputedRect`] in painter's order.
///
/// An empty document (no root) yields an empty [`Layout`].
pub fn layout(doc: &Document, viewport: (f32, f32)) -> Layout {
    let mut out = Layout {
        viewport,
        ..Layout::default()
    };

    let Some(root_id) = doc.root() else {
        return out;
    };

    let mut tree: TaffyTree<NodeId> = TaffyTree::new();
    let mut map: HashMap<taffy::NodeId, NodeId> = HashMap::new();

    let Some(root_taffy) = build_subtree(doc, root_id, viewport, true, &mut tree, &mut map) else {
        return out;
    };

    let available = Size {
        width: AvailableSpace::Definite(viewport.0),
        height: AvailableSpace::Definite(viewport.1),
    };

    let compute = tree.compute_layout_with_measure(
        root_taffy,
        available,
        |known, _avail, _node, context, _style| {
            // Honor any known dimension; else use the leaf's intrinsic size.
            if let (Some(w), Some(h)) = (known.width, known.height) {
                return Size { width: w, height: h };
            }
            let intrinsic = context
                .and_then(|&mut ir_id| {
                    doc.get(ir_id)
                        .filter(|n| n.kind == "Text")
                        .map(text_intrinsic_size)
                })
                .unwrap_or(Size::ZERO);
            Size {
                width: known.width.unwrap_or(intrinsic.width),
                height: known.height.unwrap_or(intrinsic.height),
            }
        },
    );
    if compute.is_err() {
        return out;
    }

    collect_rects(&tree, root_taffy, &map, (0.0, 0.0), &mut out);
    out
}

// ---------------------------------------------------------------------------
// Paint pass
// ---------------------------------------------------------------------------

/// Emit a painter-ordered [`Scene`] from a computed [`Layout`].
///
/// Walks nodes in painter's order so a parent's `background` and a `Frost`
/// panel both blur/cover exactly what was drawn before them.
pub fn paint(doc: &Document, layout: &Layout) -> Scene {
    let mut scene: Scene = Vec::new();
    let (vw, vh) = layout.viewport;
    let root = doc.root();

    for (idx, &id) in layout.order().iter().enumerate() {
        let Some(node) = doc.get(id) else {
            continue;
        };
        let Some(rect) = layout.rect(id) else {
            continue;
        };
        let is_root = root == Some(id);

        match node.kind.as_str() {
            "Stack" | "Column" | "Row" | "Grid" => {
                // Container background, painted behind its children.
                if let Some(color) = color_of(node, "background") {
                    let corner_radius = px_of(node, "corner_radius")
                        .or_else(|| px_of(node, "radius"))
                        .unwrap_or(0.0);
                    if is_root && idx == 0 {
                        // Root background fills the whole viewport (the renderer
                        // treats the first full-cover rect as its clear color).
                        scene.push(DrawCmd::FilledRect {
                            x: 0.0,
                            y: 0.0,
                            w: vw,
                            h: vh,
                            color,
                            corner_radius: 0.0,
                        });
                    } else {
                        scene.push(DrawCmd::FilledRect {
                            x: rect.x,
                            y: rect.y,
                            w: rect.w,
                            h: rect.h,
                            color,
                            corner_radius,
                        });
                    }
                }
            }
            "Rect" => {
                let color = color_of(node, "color").unwrap_or(0xffffffff);
                let corner_radius = px_of(node, "corner_radius")
                    .or_else(|| px_of(node, "radius"))
                    .unwrap_or(0.0);
                scene.push(DrawCmd::FilledRect {
                    x: rect.x,
                    y: rect.y,
                    w: rect.w,
                    h: rect.h,
                    color,
                    corner_radius,
                });
            }
            "Frost" | "FrostedRect" => {
                let tint = color_of(node, "tint")
                    .or_else(|| color_of(node, "color"))
                    .unwrap_or(0xffffff40);
                let blur_radius = px_of(node, "blur_radius")
                    .or_else(|| px_of(node, "blur"))
                    .unwrap_or(12.0);
                let corner_radius = px_of(node, "corner_radius")
                    .or_else(|| px_of(node, "radius"))
                    .unwrap_or(0.0);
                scene.push(DrawCmd::FrostedRect {
                    x: rect.x,
                    y: rect.y,
                    w: rect.w,
                    h: rect.h,
                    corner_radius,
                    tint,
                    blur_radius,
                });
            }
            "Text" => {
                let content = text_of(node, "content").unwrap_or_default();
                let size = px_of(node, "size").unwrap_or(16.0);
                let color = color_of(node, "color").unwrap_or(0xffffffff);
                scene.push(DrawCmd::Text {
                    x: rect.x,
                    y: rect.y,
                    content,
                    size,
                    color,
                });
            }
            _ => {}
        }
    }

    scene
}

// ---------------------------------------------------------------------------
// hit-test
// ---------------------------------------------------------------------------

/// Return the topmost node whose computed rect contains `point`.
///
/// "Topmost" == last in painter's order (drawn last), so we scan the order
/// list back-to-front and return the first hit.
pub fn hit_test(layout: &Layout, point: (f32, f32)) -> Option<NodeId> {
    layout
        .order()
        .iter()
        .rev()
        .copied()
        .find(|&id| layout.rect(id).map(|r| r.contains(point)).unwrap_or(false))
}

// ---------------------------------------------------------------------------
// convenience
// ---------------------------------------------------------------------------

/// Lower a [`Document`] into a [`Scene`] for the given logical viewport size.
///
/// Convenience composition of [`layout`] then [`paint`]. Kept stable so the
/// renderer example and downstream callers keep working.
pub fn lower(doc: &Document, viewport: (f32, f32)) -> Scene {
    let l = layout(doc, viewport);
    paint(doc, &l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uni_ir::{Mutation, Origin};

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

    fn child(doc: &mut Document, parent: NodeId, c: NodeId) {
        doc.apply_from(Origin::System, Mutation::AppendChild { parent, child: c })
            .unwrap();
    }

    fn set_root(doc: &mut Document, id: NodeId) {
        doc.apply_from(Origin::System, Mutation::SetRoot { id }).unwrap();
    }

    #[test]
    fn empty_document_lowers_to_empty_scene() {
        let doc = Document::new();
        assert!(lower(&doc, (640.0, 480.0)).is_empty());
        assert!(layout(&doc, (640.0, 480.0)).is_empty());
    }

    #[test]
    fn lowers_stack_with_text_and_rect() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Stack");
        set_root(&mut doc, root);
        prop(&mut doc, root, "background", Value::Color(0x0a0a0aff));

        let t = node(&mut doc, "Text");
        prop(&mut doc, t, "content", Value::Text("Uni-UI".into()));
        prop(&mut doc, t, "size", Value::Px(28.0));
        prop(&mut doc, t, "color", Value::Color(0xffffffff));
        child(&mut doc, root, t);

        let r = node(&mut doc, "Rect");
        prop(&mut doc, r, "width", Value::Px(200.0));
        prop(&mut doc, r, "height", Value::Px(80.0));
        prop(&mut doc, r, "color", Value::Color(0x7d39ebff));
        child(&mut doc, root, r);

        let scene = lower(&doc, (800.0, 600.0));
        // background (full viewport) + text + rect
        assert_eq!(scene.len(), 3);
        assert!(matches!(
            scene[0],
            DrawCmd::FilledRect {
                color: 0x0a0a0aff,
                w: 800.0,
                h: 600.0,
                ..
            }
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
    fn column_stacks_children_vertically_non_overlapping() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Column");
        set_root(&mut doc, root);

        let a = node(&mut doc, "Rect");
        prop(&mut doc, a, "width", Value::Px(100.0));
        prop(&mut doc, a, "height", Value::Px(50.0));
        child(&mut doc, root, a);

        let b = node(&mut doc, "Rect");
        prop(&mut doc, b, "width", Value::Px(100.0));
        prop(&mut doc, b, "height", Value::Px(70.0));
        child(&mut doc, root, b);

        let l = layout(&doc, (400.0, 400.0));
        let ra = l.rect(a).unwrap();
        let rb = l.rect(b).unwrap();
        assert_eq!(ra.y, 0.0);
        // b sits directly below a (no gap set).
        assert_eq!(rb.y, ra.y + ra.h);
        // No vertical overlap.
        assert!(rb.y >= ra.y + ra.h);
    }

    #[test]
    fn row_places_children_horizontally() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Row");
        set_root(&mut doc, root);

        let a = node(&mut doc, "Rect");
        prop(&mut doc, a, "width", Value::Px(100.0));
        prop(&mut doc, a, "height", Value::Px(40.0));
        child(&mut doc, root, a);

        let b = node(&mut doc, "Rect");
        prop(&mut doc, b, "width", Value::Px(60.0));
        prop(&mut doc, b, "height", Value::Px(40.0));
        child(&mut doc, root, b);

        let l = layout(&doc, (400.0, 200.0));
        let ra = l.rect(a).unwrap();
        let rb = l.rect(b).unwrap();
        assert_eq!(ra.x, 0.0);
        assert_eq!(rb.x, ra.x + ra.w);
        assert_eq!(ra.y, rb.y);
    }

    #[test]
    fn padding_and_gap_are_honored() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Column");
        set_root(&mut doc, root);
        prop(&mut doc, root, "padding", Value::Px(10.0));
        prop(&mut doc, root, "gap", Value::Px(8.0));

        let a = node(&mut doc, "Rect");
        prop(&mut doc, a, "width", Value::Px(50.0));
        prop(&mut doc, a, "height", Value::Px(30.0));
        child(&mut doc, root, a);

        let b = node(&mut doc, "Rect");
        prop(&mut doc, b, "width", Value::Px(50.0));
        prop(&mut doc, b, "height", Value::Px(30.0));
        child(&mut doc, root, b);

        let l = layout(&doc, (400.0, 400.0));
        let ra = l.rect(a).unwrap();
        let rb = l.rect(b).unwrap();
        // First child offset by padding.
        assert_eq!(ra.x, 10.0);
        assert_eq!(ra.y, 10.0);
        // Second child below first, separated by gap.
        assert_eq!(rb.y, ra.y + ra.h + 8.0);
    }

    #[test]
    fn flex_grow_distributes_remaining_space() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Row");
        set_root(&mut doc, root);

        // Two growing children share a 300px-wide row equally.
        let a = node(&mut doc, "Rect");
        prop(&mut doc, a, "grow", Value::Int(1));
        prop(&mut doc, a, "height", Value::Px(40.0));
        child(&mut doc, root, a);

        let b = node(&mut doc, "Rect");
        prop(&mut doc, b, "grow", Value::Int(1));
        prop(&mut doc, b, "height", Value::Px(40.0));
        child(&mut doc, root, b);

        let l = layout(&doc, (300.0, 100.0));
        let ra = l.rect(a).unwrap();
        let rb = l.rect(b).unwrap();
        assert!((ra.w - 150.0).abs() < 0.5, "a width {}", ra.w);
        assert!((rb.w - 150.0).abs() < 0.5, "b width {}", rb.w);
        assert!((rb.x - 150.0).abs() < 0.5, "b x {}", rb.x);
    }

    #[test]
    fn nested_row_inside_column() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Column");
        set_root(&mut doc, root);

        let header = node(&mut doc, "Rect");
        prop(&mut doc, header, "height", Value::Px(40.0));
        prop(&mut doc, header, "width", Value::Px(200.0));
        child(&mut doc, root, header);

        let row = node(&mut doc, "Row");
        child(&mut doc, root, row);
        let left = node(&mut doc, "Rect");
        prop(&mut doc, left, "width", Value::Px(80.0));
        prop(&mut doc, left, "height", Value::Px(60.0));
        child(&mut doc, row, left);
        let right = node(&mut doc, "Rect");
        prop(&mut doc, right, "width", Value::Px(80.0));
        prop(&mut doc, right, "height", Value::Px(60.0));
        child(&mut doc, row, right);

        let l = layout(&doc, (500.0, 500.0));
        let hr = l.rect(header).unwrap();
        let lr = l.rect(left).unwrap();
        let rr = l.rect(right).unwrap();
        // Row sits below the header.
        assert!(lr.y >= hr.y + hr.h);
        // The two row children are laid out side by side, absolute coords.
        assert_eq!(rr.x, lr.x + lr.w);
        assert_eq!(lr.y, rr.y);
    }

    #[test]
    fn hit_test_returns_topmost_node() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Stack");
        set_root(&mut doc, root);
        prop(&mut doc, root, "background", Value::Color(0x000000ff));

        let r = node(&mut doc, "Rect");
        prop(&mut doc, r, "width", Value::Px(100.0));
        prop(&mut doc, r, "height", Value::Px(100.0));
        child(&mut doc, root, r);

        let l = layout(&doc, (400.0, 400.0));
        // Inside the rect (top-left area) → topmost is the Rect, not the root.
        assert_eq!(hit_test(&l, (10.0, 10.0)), Some(r));
        // Outside the rect but inside the (viewport-filling) root → root.
        assert_eq!(hit_test(&l, (300.0, 300.0)), Some(root));
        // Far outside the viewport → nothing.
        assert_eq!(hit_test(&l, (9999.0, 9999.0)), None);
    }

    #[test]
    fn absolute_position_floats_over_siblings() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Row");
        set_root(&mut doc, root);

        let base = node(&mut doc, "Rect");
        prop(&mut doc, base, "width", Value::Px(100.0));
        prop(&mut doc, base, "height", Value::Px(100.0));
        child(&mut doc, root, base);

        // Absolutely-positioned overlay: ignores flow, sits at its inset.
        let overlay = node(&mut doc, "Frost");
        prop(&mut doc, overlay, "position", Value::Text("absolute".into()));
        prop(&mut doc, overlay, "left", Value::Px(40.0));
        prop(&mut doc, overlay, "top", Value::Px(20.0));
        prop(&mut doc, overlay, "width", Value::Px(50.0));
        prop(&mut doc, overlay, "height", Value::Px(50.0));
        child(&mut doc, root, overlay);

        let l = layout(&doc, (400.0, 400.0));
        let br = l.rect(base).unwrap();
        let or = l.rect(overlay).unwrap();
        // base laid out in flow at origin; overlay floats at its inset, overlapping.
        assert_eq!(br.x, 0.0);
        assert_eq!(or.x, 40.0);
        assert_eq!(or.y, 20.0);
        // The overlay is topmost (painted last) where they overlap.
        assert_eq!(hit_test(&l, (45.0, 25.0)), Some(overlay));
    }

    #[test]
    fn frost_lowers_to_frosted_rect() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Stack");
        set_root(&mut doc, root);

        let f = node(&mut doc, "Frost");
        prop(&mut doc, f, "width", Value::Px(120.0));
        prop(&mut doc, f, "height", Value::Px(80.0));
        prop(&mut doc, f, "tint", Value::Color(0xffffff40));
        prop(&mut doc, f, "blur_radius", Value::Px(14.0));
        prop(&mut doc, f, "corner_radius", Value::Px(16.0));
        child(&mut doc, root, f);

        let scene = lower(&doc, (400.0, 400.0));
        let frost = scene
            .iter()
            .find(|c| matches!(c, DrawCmd::FrostedRect { .. }))
            .expect("frosted rect present");
        match frost {
            DrawCmd::FrostedRect {
                w,
                h,
                tint,
                blur_radius,
                corner_radius,
                ..
            } => {
                assert_eq!(*w, 120.0);
                assert_eq!(*h, 80.0);
                assert_eq!(*tint, 0xffffff40);
                assert_eq!(*blur_radius, 14.0);
                assert_eq!(*corner_radius, 16.0);
            }
            _ => unreachable!(),
        }
    }
}
