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
//! | `Grid`                    | CSS grid (`columns` prop → N equal `1fr` cols)|
//! | `Form` / `Section`        | flex column, inset grouped-card background    |
//! | `List` / `LazyVStack`     | windowed column (only visible rows are built) |
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

fn bool_of(node: &Node, key: &str) -> Option<bool> {
    match node.props.get(key) {
        Some(Value::Bool(b)) => Some(*b),
        _ => None,
    }
}

/// Read a 0..1 float (`Float`/`Int`/`Px` accepted) and clamp it into `[0, 1]`.
fn unit_of(node: &Node, key: &str) -> Option<f32> {
    match node.props.get(key) {
        Some(Value::Float(v)) => Some(*v as f32),
        Some(Value::Int(v)) => Some(*v as f32),
        Some(Value::Px(v)) => Some(*v),
        _ => None,
    }
    .map(|v| v.clamp(0.0, 1.0))
}

/// Scale the alpha byte of a packed `0xRRGGBBAA` color by `factor` (0..1),
/// leaving RGB untouched. Used by the `opacity` modifier.
fn scale_alpha(color: u32, factor: f32) -> u32 {
    let factor = factor.clamp(0.0, 1.0);
    let a = (color & 0xff) as f32;
    let scaled = (a * factor).round().clamp(0.0, 255.0) as u32;
    (color & 0xffffff00) | scaled
}

/// Is this a flex/grid container kind (as opposed to a drawing leaf)?
///
/// `Form` / `Section` are vertical grouped containers (SwiftUI grouped-list
/// look); `List` / `LazyVStack` are vertical scrolling containers that window
/// their children to the visible range. `Overlay` / `Underlay` are layered view
/// wrappers, and `Sheet` / `Alert` / `Popover` / `Menu` are presentation cards —
/// all carry children, so they build a taffy subtree like the flex containers.
fn is_container(kind: &str) -> bool {
    matches!(
        kind,
        "Stack"
            | "Column"
            | "Row"
            | "Grid"
            | "Form"
            | "Section"
            | "List"
            | "LazyVStack"
            | "Overlay"
            | "Underlay"
            | "Sheet"
            | "Alert"
            | "Popover"
            | "Menu"
    )
}

/// A vertically-scrolling, windowed list container (real virtualization).
fn is_list(kind: &str) -> bool {
    matches!(kind, "List" | "LazyVStack")
}

// ---------------------------------------------------------------------------
// Text measurement
// ---------------------------------------------------------------------------

/// How layout learns the intrinsic size of a `Text` leaf.
///
/// Layout doesn't shape glyphs itself — it asks a `TextMeasurer` for the
/// `(width, height)` a run of `text` wants at a given font `size`. The default
/// ([`HeuristicMeasurer`]) is a cheap monospace-ish guess; the optional
/// `real-text` feature provides a `cosmic-text`-backed implementation with real
/// shaping metrics. Inject your own to test, mock, or match a specific font.
pub trait TextMeasurer {
    /// The intrinsic `(width, height)` of `text` rendered at `size` logical px.
    fn measure(&self, text: &str, size: f32) -> (f32, f32);
}

/// The default, dependency-free measurer: a crude monospace-ish metric so text
/// boxes occupy space in layout (`size*0.6` per char wide, `size*1.4` tall).
///
/// Good enough for v0 and for builds that don't want a font stack. This is the
/// exact heuristic `uni-core` shipped before the trait existed — kept byte-for-
/// byte so layouts don't shift when you don't opt into `real-text`.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeuristicMeasurer;

impl TextMeasurer for HeuristicMeasurer {
    fn measure(&self, text: &str, size: f32) -> (f32, f32) {
        let chars = text.chars().count().max(1) as f32;
        (size * 0.6 * chars, size * 1.4)
    }
}

/// A `cosmic-text`-backed measurer: real shaping, real metrics. Behind the
/// non-default `real-text` feature so the common build stays light.
///
/// Holds its own [`cosmic_text::FontSystem`]; construct once and reuse, as
/// loading system fonts is not free. `measure` shapes a single line (no wrap)
/// and reports the widest run and the buffer's full height.
#[cfg(feature = "real-text")]
pub struct CosmicTextMeasurer {
    font_system: std::cell::RefCell<cosmic_text::FontSystem>,
}

#[cfg(feature = "real-text")]
impl CosmicTextMeasurer {
    /// Build a measurer with a fresh font system (loads installed fonts).
    pub fn new() -> Self {
        Self {
            font_system: std::cell::RefCell::new(cosmic_text::FontSystem::new()),
        }
    }
}

#[cfg(feature = "real-text")]
impl Default for CosmicTextMeasurer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "real-text")]
impl TextMeasurer for CosmicTextMeasurer {
    fn measure(&self, text: &str, size: f32) -> (f32, f32) {
        use cosmic_text::{Attrs, Buffer, Family, Metrics, Shaping};

        let mut fs = self.font_system.borrow_mut();
        // Line height tracks the renderer's `size * 1.2`.
        let mut buffer = Buffer::new(&mut fs, Metrics::new(size, size * 1.2));
        buffer.set_text(
            &mut fs,
            text,
            Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
        );
        buffer.shape_until_scroll(&mut fs, false);

        let mut width = 0.0_f32;
        let mut height = 0.0_f32;
        for run in buffer.layout_runs() {
            width = width.max(run.line_w);
            height += run.line_height;
        }
        if height == 0.0 {
            height = size * 1.2;
        }
        (width, height)
    }
}

/// Intrinsic size of a `Text` leaf, via `measurer`. Non-`Text` leaves are zero.
fn text_intrinsic_size(node: &Node, measurer: &dyn TextMeasurer) -> Size<f32> {
    let content = text_of(node, "content").unwrap_or_default();
    let size = px_of(node, "size").unwrap_or(16.0);
    let (w, h) = measurer.measure(&content, size);
    Size {
        width: w,
        height: h,
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

    // SwiftUI `Spacer`: a flex child that expands to fill the available
    // main-axis space. With no explicit size and no explicit `grow`, default
    // its `flex_grow` to 1 so it pushes its siblings apart (and stays auto-
    // sized so it claims none of its own intrinsic space). A leaf, paints
    // nothing — handled in `paint`.
    if node.kind == "Spacer"
        && style.flex_grow == 0.0
        && px_of(node, "width").is_none()
        && px_of(node, "height").is_none()
    {
        style.flex_grow = 1.0;
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
            // Real CSS grid: `columns` (Int) → N equal `1fr` tracks, so children
            // flow into a fixed number of equal-width columns and auto-generated
            // rows. `gap`/`spacing` (handled below for all containers) become the
            // grid row/column gap. With no/invalid `columns` we fall back to a
            // single 1fr column (graceful default — still a valid grid).
            style.display = Display::Grid;
            let cols = int_of(node, "columns").filter(|&c| c > 0).unwrap_or(1) as usize;
            style.grid_template_columns =
                std::iter::repeat_with(|| fr(1.0)).take(cols).collect();
        }
        "Form" | "Section" => {
            // Grouped container: a vertical flex stack. `paint` draws the inset
            // grouped-list background; here we just stack rows with section
            // spacing. Default a comfortable inter-row gap if none is given.
            style.display = Display::Flex;
            style.flex_direction = FlexDirection::Column;
        }
        "List" | "LazyVStack" => {
            // Windowed scrolling list. The container is a positioning context;
            // its visible children are placed absolutely at their scrolled y in
            // `build_subtree` / `build_cached`. Default to filling its parent's
            // cross axis so rows have a width to stretch into.
            style.display = Display::Flex;
            style.flex_direction = FlexDirection::Column;
        }
        "Overlay" | "Underlay" | "Sheet" | "Alert" | "Popover" | "Menu" => {
            // Layered view wrappers and presentation cards stack their content
            // vertically. Flow layout is unchanged by the layer role — only the
            // paint z-order differs (handled in `paint_order`).
            style.display = Display::Flex;
            style.flex_direction = FlexDirection::Column;
        }
        _ => {
            // Leaf — no display override (stays Flex default, with no children).
        }
    }

    // Grouped containers default to a section-spacing gap when unset.
    if matches!(node.kind.as_str(), "Form" | "Section")
        && px_of(node, "gap").is_none()
        && px_of(node, "spacing").is_none()
    {
        style.gap = Size::length(8.0);
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

/// Compute the **visible window** of a `List` / `LazyVStack`: which child
/// indices actually need to exist this frame, and the absolute y each occupies
/// inside the list (top-left origin, before the list's own position is added).
///
/// Real virtualization: given a uniform `item_height` (Px, required for
/// windowing — without it we degrade to showing every child), the `gap` between
/// rows, the `scroll_offset` (Px, how far the content has scrolled up), and the
/// list's viewport height, we keep only the rows intersecting
/// `[scroll_offset, scroll_offset + viewport_h]` plus a small overscan. Every
/// other child is *never built into the taffy tree* — it costs nothing in
/// layout or paint. Returns `(item_height, stride, visible)` where `visible`
/// is `(child_index, content_y)` pairs in order, or `None` to mean "not
/// windowed, build all children normally".
/// `(window_top_y, total_content_height, [(row_index, content_y), …])`.
type ListWindow = (f32, f32, Vec<(usize, f32)>);

fn list_window(node: &Node, viewport_h: f32) -> Option<ListWindow> {
    let item_h = px_of(node, "item_height").or_else(|| px_of(node, "row_height"))?;
    if item_h <= 0.0 {
        return None;
    }
    let gap = px_of(node, "gap").or_else(|| px_of(node, "spacing")).unwrap_or(0.0);
    let stride = item_h + gap;
    let scroll = px_of(node, "scroll_offset").unwrap_or(0.0).max(0.0);
    // Overscan: render a little above/below the viewport so a fast scroll never
    // flashes blank rows. One row, capped, is plenty.
    let overscan = px_of(node, "overscan").unwrap_or(item_h.min(64.0)).max(0.0);

    let count = node.children.len();
    if count == 0 || stride <= 0.0 {
        return Some((item_h, stride, Vec::new()));
    }

    let top = (scroll - overscan).max(0.0);
    let bottom = scroll + viewport_h + overscan;
    let first = (top / stride).floor() as usize;
    // Last index whose row top is above `bottom` (exclusive upper bound).
    let last = ((bottom / stride).ceil() as usize).min(count);

    let visible = (first..last)
        .map(|i| (i, i as f32 * stride))
        .collect();
    Some((item_h, stride, visible))
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

    let taffy_id = if is_list(&node.kind) {
        if let Some((item_h, _stride, visible)) = list_window(node, viewport.1) {
            // Windowed: build ONLY the visible children, each absolutely placed
            // at its scrolled y. Off-screen children never enter the tree.
            let scroll = px_of(node, "scroll_offset").unwrap_or(0.0).max(0.0);
            let children: Vec<taffy::NodeId> = visible
                .iter()
                .filter_map(|&(ci, content_y)| {
                    let child = *node.children.get(ci)?;
                    let t = build_subtree(doc, child, viewport, false, tree, map)?;
                    // Pin the row at its scrolled position, with the uniform height.
                    if let Ok(mut s) = tree.style(t).cloned() {
                        s.position = Position::Absolute;
                        s.inset.left = length(0.0);
                        s.inset.top = length(content_y - scroll);
                        if s.size.height == Dimension::auto() {
                            s.size.height = length(item_h);
                        }
                        let _ = tree.set_style(t, s);
                    }
                    Some(t)
                })
                .collect();
            tree.new_with_children(style, &children).ok()?
        } else {
            let children: Vec<taffy::NodeId> = node
                .children
                .iter()
                .filter_map(|&c| build_subtree(doc, c, viewport, false, tree, map))
                .collect();
            tree.new_with_children(style, &children).ok()?
        }
    } else if is_container(&node.kind) {
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
/// `Text` leaves are sized via [`HeuristicMeasurer`] — the cheap default. For
/// real shaping metrics, use [`layout_with_measure`] with another
/// [`TextMeasurer`] (e.g. the `real-text`-gated `CosmicTextMeasurer`).
///
/// An empty document (no root) yields an empty [`Layout`].
pub fn layout(doc: &Document, viewport: (f32, f32)) -> Layout {
    layout_with_measure(doc, viewport, &HeuristicMeasurer)
}

/// Like [`layout`], but `Text` intrinsic sizing is routed through `measurer`.
///
/// This is the seam between layout and text shaping: layout never touches
/// glyphs directly, it only asks `measurer` how big each `Text` run wants to be.
pub fn layout_with_measure(
    doc: &Document,
    viewport: (f32, f32),
    measurer: &dyn TextMeasurer,
) -> Layout {
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
                return Size {
                    width: w,
                    height: h,
                };
            }
            let intrinsic = context
                .and_then(|&mut ir_id| {
                    doc.get(ir_id)
                        .filter(|n| n.kind == "Text")
                        .map(|n| text_intrinsic_size(n, measurer))
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
// D3 — incremental layout
// ---------------------------------------------------------------------------

/// Every node reachable from the document root (its live node set).
fn reachable_set(doc: &Document) -> std::collections::HashSet<NodeId> {
    let mut set = std::collections::HashSet::new();
    if let Some(r) = doc.root() {
        let mut stack = vec![r];
        while let Some(id) = stack.pop() {
            if set.insert(id) {
                if let Some(n) = doc.get(id) {
                    stack.extend(n.children.iter().copied());
                }
            }
        }
    }
    set
}

/// A persistent layout context that **skips clean subtrees** between frames.
///
/// [`layout`] rebuilds the whole taffy tree on every call. `LayoutCache` keeps
/// the tree alive: [`compute`](LayoutCache::compute) re-styles *only* the nodes
/// named in the `dirty` set, and taffy reuses its cached result for every clean
/// subtree. taffy's measure function is invoked **only** for nodes it actually
/// recomputes, so a clean `Text` leaf is never re-measured — the observable
/// proof that the clean subtree was skipped.
///
/// The tree is rebuilt from scratch only when the document's *structure* changes
/// (root swapped, nodes added/removed, or a child list reordered); pure property
/// edits take the cheap incremental path. Results are identical to [`layout`].
pub struct LayoutCache {
    tree: TaffyTree<NodeId>,
    fwd: HashMap<NodeId, taffy::NodeId>,
    map: HashMap<taffy::NodeId, NodeId>,
    root_taffy: Option<taffy::NodeId>,
    root_id: Option<NodeId>,
    viewport: (f32, f32),
    children_sig: HashMap<NodeId, Vec<NodeId>>,
}

impl Default for LayoutCache {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutCache {
    pub fn new() -> Self {
        LayoutCache {
            tree: TaffyTree::new(),
            fwd: HashMap::new(),
            map: HashMap::new(),
            root_taffy: None,
            root_id: None,
            viewport: (0.0, 0.0),
            children_sig: HashMap::new(),
        }
    }

    /// Incrementally compute layout, re-styling only `dirty` nodes and letting
    /// taffy reuse cached results for clean subtrees. Uses [`HeuristicMeasurer`].
    pub fn compute(
        &mut self,
        doc: &Document,
        viewport: (f32, f32),
        dirty: &std::collections::BTreeSet<NodeId>,
    ) -> Layout {
        self.compute_with_measure(doc, viewport, dirty, &HeuristicMeasurer)
    }

    /// Like [`compute`](LayoutCache::compute) but routes `Text` sizing through
    /// `measurer`.
    pub fn compute_with_measure(
        &mut self,
        doc: &Document,
        viewport: (f32, f32),
        dirty: &std::collections::BTreeSet<NodeId>,
        measurer: &dyn TextMeasurer,
    ) -> Layout {
        if self.needs_rebuild(doc) {
            self.rebuild(doc, viewport);
        } else {
            // A viewport change re-flows from the root down.
            if viewport != self.viewport {
                if let (Some(rid), Some(&rt)) =
                    (self.root_id, self.root_id.and_then(|r| self.fwd.get(&r)))
                {
                    if let Some(node) = doc.get(rid) {
                        let _ = self.tree.set_style(rt, style_for(node, viewport, true));
                    }
                }
            }
            // Re-style only the dirty nodes that still exist. taffy's `set_style`
            // marks the node (and its ancestors) dirty, so clean subtrees keep
            // their cached layout and are never re-measured.
            for &id in dirty {
                if let (Some(&tid), Some(node)) = (self.fwd.get(&id), doc.get(id)) {
                    let is_root = Some(id) == self.root_id;
                    let _ = self
                        .tree
                        .set_style(tid, style_for(node, viewport, is_root));
                }
            }
        }
        self.viewport = viewport;
        self.run(doc, viewport, measurer)
    }

    /// A structural change (not a mere property edit) forces a full rebuild.
    fn needs_rebuild(&self, doc: &Document) -> bool {
        if self.root_taffy.is_none() || doc.root() != self.root_id {
            return true;
        }
        let reachable = reachable_set(doc);
        // Windowed lists only build their *visible* children into the tree, so
        // the cache's node set is intentionally a subset of the reachable set
        // and a scroll must re-window. Rebuild whenever a list is present.
        if reachable.iter().any(|id| doc.get(*id).is_some_and(|n| is_list(&n.kind))) {
            return true;
        }
        if reachable.len() != self.fwd.len() {
            return true;
        }
        for id in &reachable {
            if !self.fwd.contains_key(id) {
                return true;
            }
            let cur = doc.get(*id).map(|n| n.children.clone()).unwrap_or_default();
            if self.children_sig.get(id) != Some(&cur) {
                return true;
            }
        }
        false
    }

    fn rebuild(&mut self, doc: &Document, viewport: (f32, f32)) {
        self.tree = TaffyTree::new();
        self.fwd.clear();
        self.map.clear();
        self.children_sig.clear();
        self.root_id = doc.root();
        self.root_taffy = match doc.root() {
            Some(r) => self.build_cached(doc, r, viewport, true),
            None => None,
        };
    }

    fn build_cached(
        &mut self,
        doc: &Document,
        id: NodeId,
        viewport: (f32, f32),
        is_root: bool,
    ) -> Option<taffy::NodeId> {
        let node = doc.get(id)?;
        let style = style_for(node, viewport, is_root);
        let taffy_id = if is_list(&node.kind) {
            if let Some((item_h, _stride, visible)) = list_window(node, viewport.1) {
                let scroll = px_of(node, "scroll_offset").unwrap_or(0.0).max(0.0);
                let children: Vec<taffy::NodeId> = visible
                    .iter()
                    .filter_map(|&(ci, content_y)| {
                        let child = *node.children.get(ci)?;
                        let t = self.build_cached(doc, child, viewport, false)?;
                        if let Ok(mut s) = self.tree.style(t).cloned() {
                            s.position = Position::Absolute;
                            s.inset.left = length(0.0);
                            s.inset.top = length(content_y - scroll);
                            if s.size.height == Dimension::auto() {
                                s.size.height = length(item_h);
                            }
                            let _ = self.tree.set_style(t, s);
                        }
                        Some(t)
                    })
                    .collect();
                self.tree.new_with_children(style, &children).ok()?
            } else {
                let children: Vec<taffy::NodeId> = node
                    .children
                    .iter()
                    .filter_map(|&c| self.build_cached(doc, c, viewport, false))
                    .collect();
                self.tree.new_with_children(style, &children).ok()?
            }
        } else if is_container(&node.kind) {
            let children: Vec<taffy::NodeId> = node
                .children
                .iter()
                .filter_map(|&c| self.build_cached(doc, c, viewport, false))
                .collect();
            self.tree.new_with_children(style, &children).ok()?
        } else {
            self.tree.new_leaf_with_context(style, id).ok()?
        };
        self.fwd.insert(id, taffy_id);
        self.map.insert(taffy_id, id);
        self.children_sig.insert(id, node.children.clone());
        Some(taffy_id)
    }

    fn run(&mut self, doc: &Document, viewport: (f32, f32), measurer: &dyn TextMeasurer) -> Layout {
        let mut out = Layout {
            viewport,
            ..Layout::default()
        };
        let Some(root_taffy) = self.root_taffy else {
            return out;
        };
        let available = Size {
            width: AvailableSpace::Definite(viewport.0),
            height: AvailableSpace::Definite(viewport.1),
        };
        let res = self.tree.compute_layout_with_measure(
            root_taffy,
            available,
            |known, _avail, _node, context, _style| {
                if let (Some(w), Some(h)) = (known.width, known.height) {
                    return Size { width: w, height: h };
                }
                let intrinsic = context
                    .and_then(|&mut ir_id| {
                        doc.get(ir_id)
                            .filter(|n| n.kind == "Text")
                            .map(|n| text_intrinsic_size(n, measurer))
                    })
                    .unwrap_or(Size::ZERO);
                Size {
                    width: known.width.unwrap_or(intrinsic.width),
                    height: known.height.unwrap_or(intrinsic.height),
                }
            },
        );
        if res.is_err() {
            return out;
        }
        collect_rects(&self.tree, root_taffy, &self.map, (0.0, 0.0), &mut out);
        out
    }
}

// ---------------------------------------------------------------------------
// Presentation / overlay classification
// ---------------------------------------------------------------------------

/// The layering role a node plays *relative to its siblings*.
///
/// SwiftUI's `.overlay(view)` / `.background(view)` (and `ZStack`-style
/// presentation) attach an extra view that paints *over* or *under* its host.
/// We model that with either a dedicated node `kind` (`"Overlay"` / `"Underlay"`)
/// or a `role` prop (`"overlay"` / `"underlay"`) on any node, so an authored
/// child can be promoted above or demoted below its plain siblings without
/// changing flow layout — only paint z-order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayerRole {
    /// Paints under its plain siblings (a `.background(view)` layer).
    Underlay,
    /// Paints in declared order among plain siblings.
    Normal,
    /// Paints over its plain siblings (an `.overlay(view)` layer).
    Overlay,
}

/// Classify a node's layering role from its `kind` or explicit `role` prop.
fn layer_role(node: &Node) -> LayerRole {
    match node.kind.as_str() {
        "Overlay" => return LayerRole::Overlay,
        "Underlay" => return LayerRole::Underlay,
        _ => {}
    }
    match str_of(node, "role") {
        Some("overlay") => LayerRole::Overlay,
        Some("underlay") => LayerRole::Underlay,
        _ => LayerRole::Normal,
    }
}

/// A top-of-everything presentation surface: a `Sheet`/`Alert`/`Popover`/`Menu`
/// painted over the whole scene, gated by a `presented` (Bool) prop.
fn is_presentation(kind: &str) -> bool {
    matches!(kind, "Sheet" | "Alert" | "Popover" | "Menu")
}

/// Is a presentation surface currently shown? Absent `presented` ⇒ not shown,
/// so an authored-but-dormant Sheet/Alert paints nothing until gated on.
fn is_presented(node: &Node) -> bool {
    bool_of(node, "presented") == Some(true)
}

/// Build the **paint order** over the nodes the layout actually produced.
///
/// Starts from `layout.order()` (taffy painter's order, parent-before-child,
/// honoring windowed lists) and re-sorts it so that, *within each parent*:
/// `underlay` children paint first, plain children in declared order, then
/// `overlay` children last — while every presentation surface subtree
/// (`Sheet`/`Alert`/`Popover`/`Menu`) is hoisted to the very end so it paints on
/// top of everything. Subtrees stay contiguous (parent-before-child preserved),
/// so descendants of an overlay/underlay/presentation node travel with it.
fn paint_order(doc: &Document, layout: &Layout) -> Vec<NodeId> {
    use std::collections::HashSet;

    // Only nodes the layout produced a rect for participate (windowed lists drop
    // off-screen rows). Group each laid-out node's *present* children, sorted by
    // role, so we can re-emit subtrees in the right z-order.
    let present: HashSet<NodeId> = layout.order().iter().copied().collect();

    // Per node: its present children sorted [underlay.., normal.., overlay..],
    // each bucket keeping declared order.
    let sorted_children = |id: NodeId| -> Vec<NodeId> {
        let Some(node) = doc.get(id) else {
            return Vec::new();
        };
        let mut under = Vec::new();
        let mut normal = Vec::new();
        let mut over = Vec::new();
        for &c in &node.children {
            if !present.contains(&c) {
                continue;
            }
            let Some(cn) = doc.get(c) else { continue };
            match layer_role(cn) {
                LayerRole::Underlay => under.push(c),
                LayerRole::Normal => normal.push(c),
                LayerRole::Overlay => over.push(c),
            }
        }
        under.into_iter().chain(normal).chain(over).collect()
    };

    let mut base = Vec::new();
    let mut deferred = Vec::new(); // presentation-surface subtree roots
    let mut visited = HashSet::new();

    // Iterative pre-order DFS using an explicit stack of (node, is_subtree_root).
    fn push_subtree(
        doc: &Document,
        id: NodeId,
        sorted: &dyn Fn(NodeId) -> Vec<NodeId>,
        visited: &mut HashSet<NodeId>,
        out: &mut Vec<NodeId>,
        deferred: &mut Vec<NodeId>,
        top_level: bool,
    ) {
        if visited.contains(&id) {
            return;
        }
        // Hoist a presentation surface (and its subtree) to the deferred pass,
        // unless we're already painting inside a deferred presentation subtree.
        // Note: we DON'T mark it visited here, so the deferred pass can paint it.
        if top_level {
            if let Some(n) = doc.get(id) {
                if is_presentation(&n.kind) {
                    deferred.push(id);
                    return;
                }
            }
        }
        visited.insert(id);
        out.push(id);
        for c in sorted(id) {
            push_subtree(doc, c, sorted, visited, out, deferred, top_level);
        }
    }

    if let Some(root) = doc.root() {
        if present.contains(&root) {
            push_subtree(doc, root, &sorted_children, &mut visited, &mut base, &mut deferred, true);
        }
    }

    // Now append each deferred presentation subtree, painted top-most, in the
    // order they were discovered. Inside a presentation subtree, role ordering
    // still applies but nested presentation surfaces are NOT re-hoisted (they
    // belong to their parent surface).
    for root in deferred {
        push_subtree(doc, root, &sorted_children, &mut visited, &mut base, &mut Vec::new(), false);
    }

    base
}

/// Paint the chrome of a *presented* presentation surface into `scene`.
///
/// `Sheet` / `Alert` lay a full-viewport dimmed **scrim** behind a centered (or
/// laid-out) **card**, modal-style. `Popover` / `Menu` are anchored surfaces —
/// no scrim, just the card at the node's laid-out rect (its anchor). The card's
/// own children paint on top afterwards (the caller falls through to them).
///
/// Props honored: `scrim` (Color, overrides the default dim), `background`
/// (Color, card fill), `corner_radius`/`radius` (card rounding).
fn paint_presentation_surface(node: &Node, rect: ComputedRect, vw: f32, vh: f32, scene: &mut Scene) {
    let kind = node.kind.as_str();
    let modal = matches!(kind, "Sheet" | "Alert");

    // 1) Scrim: a dim full-viewport veil under modal surfaces only.
    if modal {
        let scrim = color_of(node, "scrim").unwrap_or(0x00000099);
        scene.push(DrawCmd::FilledRect {
            x: 0.0,
            y: 0.0,
            w: vw,
            h: vh,
            color: scrim,
            corner_radius: 0.0,
        });
    }

    // 2) Card: the surface panel at its laid-out rect.
    let default_fill = match kind {
        // Menus/popovers read as a slightly lighter floating card.
        "Menu" | "Popover" => 0x2a2a2aff,
        _ => 0x1c1c1eff,
    };
    let fill = color_of(node, "background")
        .or_else(|| color_of(node, "color"))
        .unwrap_or(default_fill);
    let default_radius = if modal { 14.0 } else { 10.0 };
    let corner_radius = px_of(node, "corner_radius")
        .or_else(|| px_of(node, "radius"))
        .unwrap_or(default_radius);
    scene.push(DrawCmd::FilledRect {
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
        color: fill,
        corner_radius,
    });
}

// ---------------------------------------------------------------------------
// Paint pass
// ---------------------------------------------------------------------------

/// Emit a painter-ordered [`Scene`] from a computed [`Layout`].
///
/// Walks nodes in painter's order so a parent's `background` and a `Frost`
/// panel both blur/cover exactly what was drawn before them.
///
/// Three paint-order refinements layer on top of taffy's parent-before-child
/// order: `overlay`/`underlay` children are re-sorted within their parent (an
/// overlay paints last/on-top, an underlay first/behind); presentation surfaces
/// (`Sheet`/`Alert`/`Popover`/`Menu`) are hoisted to the very top of the scene
/// and only paint when their `presented` prop is `true`.
pub fn paint(doc: &Document, layout: &Layout) -> Scene {
    let mut scene: Scene = Vec::new();
    let (vw, vh) = layout.viewport;
    let root = doc.root();

    // Nodes suppressed by a `hidden` ancestor (the whole subtree is skipped).
    let mut hidden_subtree: std::collections::HashSet<NodeId> =
        std::collections::HashSet::new();

    // Subtree of a non-presented presentation surface: paints nothing at all.
    let mut dormant_subtree: std::collections::HashSet<NodeId> =
        std::collections::HashSet::new();

    let order = paint_order(doc, layout);
    for (idx, &id) in order.iter().enumerate() {
        let Some(node) = doc.get(id) else {
            continue;
        };
        let Some(rect) = layout.rect(id) else {
            continue;
        };
        let is_root = root == Some(id);

        // `hidden` modifier (Bool true): skip this node AND its whole subtree.
        // Order is parent-before-child, so marking the children here suppresses
        // them when we reach them later in the walk.
        if hidden_subtree.contains(&id) || bool_of(node, "hidden") == Some(true) {
            let mut stack = vec![id];
            while let Some(n) = stack.pop() {
                hidden_subtree.insert(n);
                if let Some(node) = doc.get(n) {
                    stack.extend(node.children.iter().copied());
                }
            }
            continue;
        }

        // Presentation surface (`Sheet`/`Alert`/`Popover`/`Menu`): only paints
        // when gated on by `presented == true`. A dormant (not-presented)
        // surface — and its whole subtree — paints nothing.
        if dormant_subtree.contains(&id) {
            continue;
        }
        if is_presentation(&node.kind) {
            if !is_presented(node) {
                // Mark the subtree dormant so its descendants paint nothing.
                let mut stack = vec![id];
                while let Some(n) = stack.pop() {
                    dormant_subtree.insert(n);
                    if let Some(node) = doc.get(n) {
                        stack.extend(node.children.iter().copied());
                    }
                }
                continue;
            }
            // Presented: paint the surface chrome (scrim for Sheet/Alert, then a
            // card) *before* its subtree, so the subtree paints on top of it.
            paint_presentation_surface(node, rect, vw, vh, &mut scene);
            // Fall through so the surface's own children paint over the chrome.
            continue;
        }

        // `opacity` modifier (Float 0..1): scales the alpha of this node's
        // painted fill/text. (Subtree-wide opacity would need a layer; v0
        // scales the node's own primitives, which covers leaves and a
        // container's own background.)
        let opacity = unit_of(node, "opacity").unwrap_or(1.0);

        // `shadow` modifier: a soft offset dark rounded rect painted *behind*
        // the node. `shadow` (Px) or `shadow_radius` give the blur radius;
        // `shadow_color` overrides the default translucent black.
        let shadow_radius = px_of(node, "shadow").or_else(|| px_of(node, "shadow_radius"));
        if let Some(sr) = shadow_radius {
            if sr > 0.0 {
                let shadow_color = color_of(node, "shadow_color").unwrap_or(0x00000066);
                let corner_radius = px_of(node, "corner_radius")
                    .or_else(|| px_of(node, "radius"))
                    .unwrap_or(0.0);
                // Offset down-right by a fraction of the radius, and reuse the
                // Frost backend's blur to soften the edge.
                let off = (sr * 0.5).min(8.0);
                scene.push(DrawCmd::FrostedRect {
                    x: rect.x + off,
                    y: rect.y + off,
                    w: rect.w,
                    h: rect.h,
                    corner_radius,
                    tint: scale_alpha(shadow_color, opacity),
                    blur_radius: sr,
                });
            }
        }

        match node.kind.as_str() {
            "Spacer" => {
                // A layout-only flex spacer: claims main-axis space (handled in
                // `style_for`) but paints nothing.
            }
            "Divider" => {
                // A thin line spanning the node's cross axis. `thickness` (Px)
                // sets the line weight (default 1px); `color`/`background` give
                // a subtle ink color (default translucent white).
                let thickness = px_of(node, "thickness").unwrap_or(1.0).max(0.0);
                let color = color_of(node, "color")
                    .or_else(|| color_of(node, "background"))
                    .unwrap_or(0xffffff26);
                let color = scale_alpha(color, opacity);
                // Draw the line along the longer axis of the laid-out rect,
                // centered on the short axis, with the given thickness.
                if rect.w >= rect.h {
                    let y = rect.y + (rect.h - thickness) * 0.5;
                    scene.push(DrawCmd::FilledRect {
                        x: rect.x,
                        y,
                        w: rect.w,
                        h: thickness,
                        color,
                        corner_radius: 0.0,
                    });
                } else {
                    let x = rect.x + (rect.w - thickness) * 0.5;
                    scene.push(DrawCmd::FilledRect {
                        x,
                        y: rect.y,
                        w: thickness,
                        h: rect.h,
                        color,
                        corner_radius: 0.0,
                    });
                }
            }
            "Image" => {
                // Placeholder box honoring width/height/cornerRadius/background.
                // Real asset decode (`src`/`content`) lands later; for now we
                // always render the filled rounded placeholder rect.
                let color = color_of(node, "background")
                    .or_else(|| color_of(node, "color"))
                    .unwrap_or(0xffffff14);
                let corner_radius = px_of(node, "cornerRadius")
                    .or_else(|| px_of(node, "corner_radius"))
                    .or_else(|| px_of(node, "radius"))
                    .unwrap_or(0.0);
                scene.push(DrawCmd::FilledRect {
                    x: rect.x,
                    y: rect.y,
                    w: rect.w,
                    h: rect.h,
                    color: scale_alpha(color, opacity),
                    corner_radius,
                });
            }
            "Form" | "Section" => {
                // Grouped-list look: an inset rounded card painted behind the
                // section's rows. `background` overrides the default subtle fill;
                // `corner_radius`/`radius` the rounding (default 10px, SwiftUI's
                // inset grouped style). Always painted (the group is the visual).
                let color = color_of(node, "background").unwrap_or(0xffffff0d);
                let corner_radius = px_of(node, "corner_radius")
                    .or_else(|| px_of(node, "radius"))
                    .unwrap_or(10.0);
                scene.push(DrawCmd::FilledRect {
                    x: rect.x,
                    y: rect.y,
                    w: rect.w,
                    h: rect.h,
                    color: scale_alpha(color, opacity),
                    corner_radius,
                });
            }
            "Stack" | "Column" | "Row" | "Grid" | "List" | "LazyVStack" => {
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
                            color: scale_alpha(color, opacity),
                            corner_radius: 0.0,
                        });
                    } else {
                        scene.push(DrawCmd::FilledRect {
                            x: rect.x,
                            y: rect.y,
                            w: rect.w,
                            h: rect.h,
                            color: scale_alpha(color, opacity),
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
                    color: scale_alpha(color, opacity),
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
                    tint: scale_alpha(tint, opacity),
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
                    color: scale_alpha(color, opacity),
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

    /// D3: incremental relayout re-measures ONLY the dirty node — clean
    /// subtrees are skipped — and the result equals a full layout.
    #[test]
    fn incremental_layout_skips_clean_subtrees() {
        use std::cell::RefCell;
        use std::collections::BTreeSet;

        // Records which text strings taffy actually asked us to measure.
        struct Recorder {
            seen: RefCell<Vec<String>>,
        }
        impl TextMeasurer for Recorder {
            fn measure(&self, text: &str, size: f32) -> (f32, f32) {
                self.seen.borrow_mut().push(text.to_string());
                HeuristicMeasurer.measure(text, size)
            }
        }

        // A Column of three fixed-width Text leaves.
        let mut doc = Document::new();
        let col = doc.fresh_id();
        doc.apply_from(Origin::System, Mutation::CreateNode { id: col, kind: "Column".into() })
            .unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id: col }).unwrap();
        let mut texts = Vec::new();
        for s in ["alpha", "beta", "gamma"] {
            let t = doc.fresh_id();
            doc.apply_from(Origin::System, Mutation::CreateNode { id: t, kind: "Text".into() })
                .unwrap();
            doc.apply_from(Origin::System, Mutation::SetProp { id: t, key: "content".into(), value: Value::Text(s.into()) })
                .unwrap();
            doc.apply_from(Origin::System, Mutation::SetProp { id: t, key: "width".into(), value: Value::Px(100.0) })
                .unwrap();
            doc.apply_from(Origin::System, Mutation::AppendChild { parent: col, child: t }).unwrap();
            texts.push(t);
        }

        let vp = (200.0, 400.0);
        let mut cache = LayoutCache::new();
        let rec = Recorder { seen: RefCell::new(Vec::new()) };

        // First compute: full build → every text leaf is measured.
        let _ = cache.compute_with_measure(&doc, vp, &BTreeSet::new(), &rec);
        assert!(
            rec.seen.borrow().iter().any(|t| t == "beta"),
            "first pass measures all texts"
        );

        // Change ONLY the first text; mark only it dirty.
        doc.apply_from(Origin::Ai, Mutation::SetProp { id: texts[0], key: "content".into(), value: Value::Text("alpha-CHANGED".into()) })
            .unwrap();
        rec.seen.borrow_mut().clear();
        let mut dirty = BTreeSet::new();
        dirty.insert(texts[0]);
        let inc = cache.compute_with_measure(&doc, vp, &dirty, &rec);

        let seen = rec.seen.borrow().clone();
        assert!(seen.iter().any(|t| t == "alpha-CHANGED"), "dirty node IS re-measured");
        assert!(!seen.iter().any(|t| t == "beta"), "clean 'beta' skipped, got {seen:?}");
        assert!(!seen.iter().any(|t| t == "gamma"), "clean 'gamma' skipped, got {seen:?}");

        // Correctness: incremental layout matches a fresh full layout exactly.
        let fresh = layout_with_measure(&doc, vp, &HeuristicMeasurer);
        for &t in &texts {
            assert_eq!(inc.rect(t), fresh.rect(t), "incremental rect matches full layout");
        }
        assert_eq!(inc.rect(col), fresh.rect(col));
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
        doc.apply_from(Origin::System, Mutation::SetRoot { id })
            .unwrap();
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
        prop(
            &mut doc,
            overlay,
            "position",
            Value::Text("absolute".into()),
        );
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

    #[test]
    fn heuristic_measurer_matches_legacy_metric() {
        // The default measurer must reproduce the pre-trait numbers exactly,
        // so opting out of `real-text` never shifts a layout.
        let m = HeuristicMeasurer;
        // 6 chars at size 20 → 6 * 20 * 0.6 wide, 20 * 1.4 tall.
        assert_eq!(m.measure("Uni-UI", 20.0), (72.0, 28.0));
        // Empty content still claims one char's width (max(1)).
        assert_eq!(m.measure("", 10.0), (6.0, 14.0));
    }

    #[test]
    fn default_layout_uses_heuristic_text_size() {
        // A `Row` lets the text keep its intrinsic main-axis width (a Stack's
        // single child would stretch to fill the cross axis instead).
        let mut doc = Document::new();
        let root = node(&mut doc, "Row");
        set_root(&mut doc, root);
        let t = node(&mut doc, "Text");
        prop(&mut doc, t, "content", Value::Text("ABCD".into()));
        prop(&mut doc, t, "size", Value::Px(20.0));
        child(&mut doc, root, t);

        let l = layout(&doc, (400.0, 400.0));
        let r = l.rect(t).unwrap();
        // 4 chars * 20 * 0.6 = 48 main-axis width, straight from the heuristic.
        // (Cross-axis height flex-stretches to the row, so we don't pin it.)
        assert_eq!(r.w, 48.0);
    }

    #[test]
    fn layout_with_measure_routes_through_custom_measurer() {
        // A stub measurer with fixed, distinctive metrics: layout must report
        // exactly what we hand back, proving the seam is real.
        struct Fixed;
        impl TextMeasurer for Fixed {
            fn measure(&self, text: &str, _size: f32) -> (f32, f32) {
                (text.chars().count() as f32 * 100.0, 33.0)
            }
        }

        let mut doc = Document::new();
        let root = node(&mut doc, "Row");
        set_root(&mut doc, root);
        let t = node(&mut doc, "Text");
        prop(&mut doc, t, "content", Value::Text("xy".into()));
        prop(&mut doc, t, "size", Value::Px(20.0));
        child(&mut doc, root, t);

        // In a Row the text keeps its measured main-axis WIDTH (200) — proof
        // the measurer's width feeds layout.
        let row = layout_with_measure(&doc, (1000.0, 1000.0), &Fixed);
        assert_eq!(row.rect(t).unwrap().w, 200.0);

        // Re-parent under a Column so the text keeps its measured main-axis
        // HEIGHT (33) — proof the measurer's height feeds layout too.
        let col_root = node(&mut doc, "Column");
        let t2 = node(&mut doc, "Text");
        prop(&mut doc, t2, "content", Value::Text("xy".into()));
        prop(&mut doc, t2, "size", Value::Px(20.0));
        child(&mut doc, col_root, t2);
        set_root(&mut doc, col_root);
        let col = layout_with_measure(&doc, (1000.0, 1000.0), &Fixed);
        assert_eq!(col.rect(t2).unwrap().h, 33.0);
    }

    // -----------------------------------------------------------------------
    // SwiftUI-equivalent views + modifiers
    // -----------------------------------------------------------------------

    #[test]
    fn spacer_grows_to_fill_main_axis() {
        // A Row: [fixed 100px Rect][Spacer][fixed 100px Rect] in a 400px row.
        // The Spacer should expand to the 200px gap, pushing the second rect
        // to the far end — SwiftUI `Spacer()` behavior.
        let mut doc = Document::new();
        let root = node(&mut doc, "Row");
        set_root(&mut doc, root);

        let a = node(&mut doc, "Rect");
        prop(&mut doc, a, "width", Value::Px(100.0));
        prop(&mut doc, a, "height", Value::Px(40.0));
        child(&mut doc, root, a);

        let spacer = node(&mut doc, "Spacer");
        child(&mut doc, root, spacer);

        let b = node(&mut doc, "Rect");
        prop(&mut doc, b, "width", Value::Px(100.0));
        prop(&mut doc, b, "height", Value::Px(40.0));
        child(&mut doc, root, b);

        let l = layout(&doc, (400.0, 100.0));
        let sr = l.rect(spacer).unwrap();
        let br = l.rect(b).unwrap();
        // Spacer fills the 400 - 100 - 100 = 200px gap.
        assert!((sr.w - 200.0).abs() < 0.5, "spacer width {}", sr.w);
        // Second rect pushed to the far end.
        assert!((br.x - 300.0).abs() < 0.5, "b x {}", br.x);
        // Spacer paints nothing.
        let scene = paint(&doc, &l);
        // Two rects, no command at the spacer's position.
        assert_eq!(
            scene
                .iter()
                .filter(|c| matches!(c, DrawCmd::FilledRect { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn divider_paints_a_thin_line() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Column");
        set_root(&mut doc, root);

        let d = node(&mut doc, "Divider");
        prop(&mut doc, d, "width", Value::Px(200.0));
        prop(&mut doc, d, "thickness", Value::Px(2.0));
        prop(&mut doc, d, "color", Value::Color(0x808080ff));
        child(&mut doc, root, d);

        let scene = lower(&doc, (400.0, 400.0));
        let line = scene
            .iter()
            .find_map(|c| match c {
                DrawCmd::FilledRect {
                    w,
                    h,
                    color,
                    corner_radius,
                    ..
                } => Some((*w, *h, *color, *corner_radius)),
                _ => None,
            })
            .expect("divider emits a filled rect line");
        // Thin line: thickness 2px tall, spanning the 200px width.
        assert_eq!(line.1, 2.0, "thickness");
        assert_eq!(line.0, 200.0, "spans cross axis width");
        assert_eq!(line.2, 0x808080ff, "ink color");
        assert_eq!(line.3, 0.0, "line has no rounding");
    }

    #[test]
    fn image_renders_rounded_placeholder() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Stack");
        set_root(&mut doc, root);

        let img = node(&mut doc, "Image");
        prop(&mut doc, img, "width", Value::Px(64.0));
        prop(&mut doc, img, "height", Value::Px(64.0));
        prop(&mut doc, img, "cornerRadius", Value::Px(12.0));
        prop(&mut doc, img, "background", Value::Color(0x223344ff));
        // A src present — should still just paint the placeholder box.
        prop(&mut doc, img, "src", Value::Text("logo.png".into()));
        child(&mut doc, root, img);

        let scene = lower(&doc, (400.0, 400.0));
        let placeholder = scene
            .iter()
            .find_map(|c| match c {
                DrawCmd::FilledRect {
                    w,
                    h,
                    color,
                    corner_radius,
                    ..
                } if *w == 64.0 => Some((*h, *color, *corner_radius)),
                _ => None,
            })
            .expect("image emits a placeholder rect");
        assert_eq!(placeholder.0, 64.0, "height");
        assert_eq!(placeholder.1, 0x223344ff, "background");
        assert_eq!(placeholder.2, 12.0, "cornerRadius honored");
    }

    #[test]
    fn opacity_modifier_reduces_alpha() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Stack");
        set_root(&mut doc, root);

        let r = node(&mut doc, "Rect");
        prop(&mut doc, r, "width", Value::Px(50.0));
        prop(&mut doc, r, "height", Value::Px(50.0));
        // Fully-opaque red, at 50% opacity → alpha halved.
        prop(&mut doc, r, "color", Value::Color(0xff0000ff));
        prop(&mut doc, r, "opacity", Value::Float(0.5));
        child(&mut doc, root, r);

        let scene = lower(&doc, (400.0, 400.0));
        let rect = scene
            .iter()
            .find_map(|c| match c {
                DrawCmd::FilledRect { w, color, .. } if *w == 50.0 => Some(*color),
                _ => None,
            })
            .expect("rect present");
        // RGB preserved; alpha 0xff scaled by 0.5 → ~0x80 (127.5 rounds to 128).
        assert_eq!(rect & 0xffffff00, 0xff000000, "rgb preserved");
        let alpha = rect & 0xff;
        assert!(alpha < 0xff, "alpha reduced, got {alpha:#x}");
        assert_eq!(alpha, 128, "0xff * 0.5 → 128");
    }

    #[test]
    fn hidden_modifier_omits_node_and_subtree() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Column");
        set_root(&mut doc, root);

        // A visible rect.
        let visible = node(&mut doc, "Rect");
        prop(&mut doc, visible, "width", Value::Px(30.0));
        prop(&mut doc, visible, "height", Value::Px(30.0));
        prop(&mut doc, visible, "color", Value::Color(0x00ff00ff));
        child(&mut doc, root, visible);

        // A hidden subtree: a Stack with a child rect, both must be omitted.
        let hidden_box = node(&mut doc, "Stack");
        prop(&mut doc, hidden_box, "hidden", Value::Bool(true));
        prop(&mut doc, hidden_box, "background", Value::Color(0xff0000ff));
        child(&mut doc, root, hidden_box);
        let inner = node(&mut doc, "Rect");
        prop(&mut doc, inner, "width", Value::Px(40.0));
        prop(&mut doc, inner, "height", Value::Px(40.0));
        prop(&mut doc, inner, "color", Value::Color(0x0000ffff));
        child(&mut doc, hidden_box, inner);

        let scene = lower(&doc, (400.0, 400.0));
        // Only the visible green rect survives — neither the hidden Stack's
        // background nor its inner blue rect is painted.
        assert!(
            scene
                .iter()
                .any(|c| matches!(c, DrawCmd::FilledRect { color: 0x00ff00ff, .. })),
            "visible rect painted"
        );
        assert!(
            !scene
                .iter()
                .any(|c| matches!(c, DrawCmd::FilledRect { color: 0xff0000ff, .. })),
            "hidden node's background omitted"
        );
        assert!(
            !scene
                .iter()
                .any(|c| matches!(c, DrawCmd::FilledRect { color: 0x0000ffff, .. })),
            "hidden node's subtree omitted"
        );
    }

    #[test]
    fn shadow_modifier_paints_behind_node() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Stack");
        set_root(&mut doc, root);

        let r = node(&mut doc, "Rect");
        prop(&mut doc, r, "width", Value::Px(60.0));
        prop(&mut doc, r, "height", Value::Px(60.0));
        prop(&mut doc, r, "color", Value::Color(0xffffffff));
        prop(&mut doc, r, "shadow", Value::Px(10.0));
        child(&mut doc, root, r);

        let scene = lower(&doc, (400.0, 400.0));
        // The shadow is a soft (frosted) rect painted BEFORE the rect's fill.
        let shadow_idx = scene
            .iter()
            .position(|c| matches!(c, DrawCmd::FrostedRect { .. }))
            .expect("shadow frosted rect present");
        let rect_idx = scene
            .iter()
            .position(|c| matches!(c, DrawCmd::FilledRect { color: 0xffffffff, w: 60.0, .. }))
            .expect("rect fill present");
        assert!(shadow_idx < rect_idx, "shadow paints behind the node");
        // Shadow carries the requested blur radius.
        match &scene[shadow_idx] {
            DrawCmd::FrostedRect { blur_radius, .. } => assert_eq!(*blur_radius, 10.0),
            _ => unreachable!(),
        }
    }

    #[cfg(feature = "real-text")]
    #[test]
    fn cosmic_measurer_gives_nonzero_size() {
        // With the `real-text` feature, the cosmic-text measurer should shape
        // real glyphs and report positive metrics (exact values are font-
        // dependent, so we only assert they're sane).
        let m = CosmicTextMeasurer::new();
        let (w, h) = m.measure("Hello", 24.0);
        assert!(w > 0.0, "width should be positive, got {w}");
        assert!(h > 0.0, "height should be positive, got {h}");
    }

    // -----------------------------------------------------------------------
    // SwiftUI container parity: Grid, List windowing, Form/Section
    // -----------------------------------------------------------------------

    /// A `Grid` with `columns: 3` lays children into 3 equal columns: the row's
    /// width is split into thirds (real CSS-grid track sizing, not flex-wrap),
    /// and the 4th child wraps to a second row directly below the 1st.
    #[test]
    fn grid_columns_make_equal_tracks_and_wrap() {
        let mut doc = Document::new();
        let g = node(&mut doc, "Grid");
        set_root(&mut doc, g);
        prop(&mut doc, g, "columns", Value::Int(3));

        // Four auto-sized cells (no explicit width → each fills its track).
        let mut cells = Vec::new();
        for _ in 0..4 {
            let c = node(&mut doc, "Rect");
            prop(&mut doc, c, "height", Value::Px(20.0));
            child(&mut doc, g, c);
            cells.push(c);
        }

        // 300px-wide viewport → three 100px columns.
        let l = layout(&doc, (300.0, 300.0));
        let r0 = l.rect(cells[0]).unwrap();
        let r1 = l.rect(cells[1]).unwrap();
        let r2 = l.rect(cells[2]).unwrap();
        let r3 = l.rect(cells[3]).unwrap();

        // Equal column widths.
        assert!((r0.w - 100.0).abs() < 0.5, "col0 width {}", r0.w);
        assert!((r1.w - 100.0).abs() < 0.5, "col1 width {}", r1.w);
        assert!((r2.w - 100.0).abs() < 0.5, "col2 width {}", r2.w);
        // Columns advance across the row.
        assert!((r0.x - 0.0).abs() < 0.5, "col0 x {}", r0.x);
        assert!((r1.x - 100.0).abs() < 0.5, "col1 x {}", r1.x);
        assert!((r2.x - 200.0).abs() < 0.5, "col2 x {}", r2.x);
        // First three share a row; the 4th wraps under the 1st (new row).
        assert!((r0.y - r1.y).abs() < 0.5, "first three on one row");
        assert!((r0.y - r2.y).abs() < 0.5, "first three on one row");
        assert!(r3.y > r0.y + 0.5, "4th cell wrapped to row 2 (y {})", r3.y);
        assert!((r3.x - 0.0).abs() < 0.5, "4th cell back in column 0");
    }

    /// `Grid` honors `gap` between tracks: with one 10px gap between two
    /// columns in a 210px viewport, each column is 100px and the second starts
    /// at 110px.
    #[test]
    fn grid_honors_gap_between_columns() {
        let mut doc = Document::new();
        let g = node(&mut doc, "Grid");
        set_root(&mut doc, g);
        prop(&mut doc, g, "columns", Value::Int(2));
        prop(&mut doc, g, "gap", Value::Px(10.0));

        let a = node(&mut doc, "Rect");
        prop(&mut doc, a, "height", Value::Px(20.0));
        child(&mut doc, g, a);
        let b = node(&mut doc, "Rect");
        prop(&mut doc, b, "height", Value::Px(20.0));
        child(&mut doc, g, b);

        let l = layout(&doc, (210.0, 200.0));
        let ra = l.rect(a).unwrap();
        let rb = l.rect(b).unwrap();
        // (210 - 10 gap) / 2 = 100 each.
        assert!((ra.w - 100.0).abs() < 0.5, "col0 width {}", ra.w);
        assert!((rb.w - 100.0).abs() < 0.5, "col1 width {}", rb.w);
        // Second column offset past first + gap.
        assert!((rb.x - 110.0).abs() < 0.5, "col1 x {}", rb.x);
    }

    /// Real virtualization: a `List` of 1000 fixed-height rows, scrolled deep,
    /// builds and paints ONLY the rows in (and just around) the viewport — every
    /// off-screen row is absent from the Layout AND the Scene.
    #[test]
    fn list_windows_to_visible_rows_only() {
        let mut doc = Document::new();
        let list = node(&mut doc, "List");
        set_root(&mut doc, list);
        prop(&mut doc, list, "item_height", Value::Px(50.0));
        // No gap → row i occupies y = i*50.
        prop(&mut doc, list, "overscan", Value::Px(0.0));
        // Scroll so the first visible row is #20 (offset 1000 / 50).
        prop(&mut doc, list, "scroll_offset", Value::Px(1000.0));

        let mut rows = Vec::new();
        for i in 0..1000 {
            let r = node(&mut doc, "Rect");
            prop(&mut doc, r, "height", Value::Px(50.0));
            // Tag each row's color with its index so we can find it in the scene.
            prop(&mut doc, r, "color", Value::Color(0x01000000 | i as u32));
            child(&mut doc, list, r);
            rows.push(r);
        }

        // Viewport 50 wide, 200 tall → 4 rows fit (200/50): indices 20..24.
        let vp = (50.0, 200.0);
        let l = layout(&doc, vp);

        // On-screen rows present in the Layout.
        for (k, &row) in rows[20..24].iter().enumerate() {
            let i = 20 + k;
            assert!(l.rect(row).is_some(), "visible row {i} laid out");
        }
        // Off-screen rows absent from the Layout (never built).
        assert!(l.rect(rows[0]).is_none(), "row 0 (far above) skipped");
        assert!(l.rect(rows[10]).is_none(), "row 10 (above) skipped");
        assert!(l.rect(rows[500]).is_none(), "row 500 (far below) skipped");
        assert!(l.rect(rows[999]).is_none(), "last row skipped");

        // Only a small window is laid out, not all 1000 rows (+1 for the list).
        assert!(l.len() < 20, "only a windowed subset laid out, got {}", l.len());

        // Visible rows land at their scrolled screen position: row 20 at y=0.
        let r20 = l.rect(rows[20]).unwrap();
        assert!((r20.y - 0.0).abs() < 0.5, "row 20 at viewport top, y {}", r20.y);
        let r21 = l.rect(rows[21]).unwrap();
        assert!((r21.y - 50.0).abs() < 0.5, "row 21 below row 20, y {}", r21.y);

        // Scene parity: off-screen rows emit no draw command, on-screen do.
        let scene = paint(&doc, &l);
        let painted = |idx: usize| {
            let want = 0x01000000 | idx as u32;
            scene
                .iter()
                .any(|c| matches!(c, DrawCmd::FilledRect { color, .. } if *color == want))
        };
        assert!(painted(20), "row 20 painted");
        assert!(painted(23), "row 23 painted");
        assert!(!painted(0), "row 0 not painted");
        assert!(!painted(500), "row 500 not painted");
        assert!(!painted(999), "row 999 not painted");
    }

    /// Scrolling the list shifts the visible window: different rows become live.
    #[test]
    fn list_window_follows_scroll_offset() {
        let mut doc = Document::new();
        let list = node(&mut doc, "List");
        set_root(&mut doc, list);
        prop(&mut doc, list, "item_height", Value::Px(40.0));
        prop(&mut doc, list, "overscan", Value::Px(0.0));
        prop(&mut doc, list, "scroll_offset", Value::Px(0.0));

        let mut rows = Vec::new();
        for _ in 0..200 {
            let r = node(&mut doc, "Rect");
            prop(&mut doc, r, "height", Value::Px(40.0));
            child(&mut doc, list, r);
            rows.push(r);
        }

        let vp = (50.0, 160.0); // 4 rows tall.
        // At offset 0: rows 0..4 visible, row 50 not.
        let l0 = layout(&doc, vp);
        assert!(l0.rect(rows[0]).is_some(), "row 0 visible at top");
        assert!(l0.rect(rows[50]).is_none(), "row 50 not visible at top");

        // Scroll down to row 50.
        prop(&mut doc, list, "scroll_offset", Value::Px(2000.0)); // 50 * 40
        let l1 = layout(&doc, vp);
        assert!(l1.rect(rows[50]).is_some(), "row 50 visible after scroll");
        assert!(l1.rect(rows[0]).is_none(), "row 0 no longer visible");
    }

    /// `Form` and `Section` are styled grouped containers: each paints an inset
    /// rounded card behind its rows, and stacks its rows vertically with a
    /// default section spacing.
    #[test]
    fn form_and_section_paint_grouped_card() {
        let mut doc = Document::new();
        let form = node(&mut doc, "Form");
        set_root(&mut doc, form);

        let section = node(&mut doc, "Section");
        prop(&mut doc, section, "background", Value::Color(0x222222ff));
        child(&mut doc, form, section);

        let a = node(&mut doc, "Rect");
        prop(&mut doc, a, "width", Value::Px(120.0));
        prop(&mut doc, a, "height", Value::Px(30.0));
        child(&mut doc, section, a);
        let b = node(&mut doc, "Rect");
        prop(&mut doc, b, "width", Value::Px(120.0));
        prop(&mut doc, b, "height", Value::Px(30.0));
        child(&mut doc, section, b);

        let l = layout(&doc, (300.0, 400.0));
        let ra = l.rect(a).unwrap();
        let rb = l.rect(b).unwrap();
        // Rows stack vertically, separated by the default 8px section gap.
        assert!(rb.y >= ra.y + ra.h, "rows do not overlap");
        assert!((rb.y - (ra.y + ra.h + 8.0)).abs() < 0.5, "default section gap, got {}", rb.y - (ra.y + ra.h));

        let scene = paint(&doc, &l);
        // Section's explicit grouped-card background is painted.
        assert!(
            scene.iter().any(|c| matches!(
                c,
                DrawCmd::FilledRect { color: 0x222222ff, corner_radius, .. } if *corner_radius > 0.0
            )),
            "section paints a rounded grouped card"
        );
        // Form (no explicit background) still paints its default inset card.
        let form_rect = l.rect(form).unwrap();
        assert!(
            scene.iter().any(|c| matches!(
                c,
                DrawCmd::FilledRect { w, h, corner_radius, .. }
                    if (*w - form_rect.w).abs() < 0.5 && (*h - form_rect.h).abs() < 0.5 && *corner_radius > 0.0
            )),
            "form paints a default grouped card"
        );
    }

    /// The windowed `List` also works through the incremental `LayoutCache`:
    /// a scroll re-windows to the new visible rows.
    #[test]
    fn list_windowing_through_layout_cache() {
        use std::collections::BTreeSet;

        let mut doc = Document::new();
        let list = node(&mut doc, "List");
        set_root(&mut doc, list);
        prop(&mut doc, list, "item_height", Value::Px(50.0));
        prop(&mut doc, list, "overscan", Value::Px(0.0));
        prop(&mut doc, list, "scroll_offset", Value::Px(0.0));

        let mut rows = Vec::new();
        for _ in 0..300 {
            let r = node(&mut doc, "Rect");
            prop(&mut doc, r, "height", Value::Px(50.0));
            child(&mut doc, list, r);
            rows.push(r);
        }

        let vp = (50.0, 200.0); // 4 rows.
        let mut cache = LayoutCache::new();
        let l0 = cache.compute(&doc, vp, &BTreeSet::new());
        assert!(l0.rect(rows[0]).is_some(), "row 0 visible at top (cache)");
        assert!(l0.rect(rows[100]).is_none(), "row 100 windowed out (cache)");

        // Scroll to row 100 (5000 / 50) and recompute.
        prop(&mut doc, list, "scroll_offset", Value::Px(5000.0));
        let mut dirty = BTreeSet::new();
        dirty.insert(list);
        let l1 = cache.compute(&doc, vp, &dirty);
        assert!(l1.rect(rows[100]).is_some(), "row 100 visible after scroll (cache)");
        assert!(l1.rect(rows[0]).is_none(), "row 0 windowed out after scroll (cache)");
    }

    // -----------------------------------------------------------------------
    // Presentation / overlay rendering
    // -----------------------------------------------------------------------

    /// Index in the scene of the first `FilledRect` whose fill matches `color`.
    fn rect_idx(scene: &Scene, color: u32) -> Option<usize> {
        scene
            .iter()
            .position(|c| matches!(c, DrawCmd::FilledRect { color: c2, .. } if *c2 == color))
    }

    /// An `overlay`-role child paints AFTER its plain sibling (on top), even
    /// though it is declared first; an `underlay`-role child paints BEFORE.
    #[test]
    fn overlay_paints_after_base_underlay_before() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Stack");
        set_root(&mut doc, root);

        // Declared order: overlay, base, underlay — deliberately scrambled so
        // only the role (not declaration order) can produce the right z-order.
        let over = node(&mut doc, "Rect");
        prop(&mut doc, over, "role", Value::Text("overlay".into()));
        prop(&mut doc, over, "width", Value::Px(40.0));
        prop(&mut doc, over, "height", Value::Px(40.0));
        prop(&mut doc, over, "color", Value::Color(0x00ff00ff));
        child(&mut doc, root, over);

        let base = node(&mut doc, "Rect");
        prop(&mut doc, base, "width", Value::Px(40.0));
        prop(&mut doc, base, "height", Value::Px(40.0));
        prop(&mut doc, base, "color", Value::Color(0xff0000ff));
        child(&mut doc, root, base);

        let under = node(&mut doc, "Underlay"); // kind-based underlay wrapper
        let under_rect = node(&mut doc, "Rect");
        prop(&mut doc, under_rect, "width", Value::Px(40.0));
        prop(&mut doc, under_rect, "height", Value::Px(40.0));
        prop(&mut doc, under_rect, "color", Value::Color(0x0000ffff));
        child(&mut doc, under, under_rect);
        child(&mut doc, root, under);

        let scene = lower(&doc, (400.0, 400.0));
        let i_under = rect_idx(&scene, 0x0000ffff).expect("underlay painted");
        let i_base = rect_idx(&scene, 0xff0000ff).expect("base painted");
        let i_over = rect_idx(&scene, 0x00ff00ff).expect("overlay painted");

        // Underlay (behind) < base < overlay (on top), regardless of declared order.
        assert!(i_under < i_base, "underlay before base: {i_under} < {i_base}");
        assert!(i_base < i_over, "overlay after base: {i_base} < {i_over}");
    }

    /// A kind `"Overlay"` wrapper paints its subtree on top of plain siblings.
    #[test]
    fn overlay_kind_layers_above_siblings() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Stack");
        set_root(&mut doc, root);

        // Overlay declared FIRST but must paint last.
        let ov = node(&mut doc, "Overlay");
        let ov_child = node(&mut doc, "Rect");
        prop(&mut doc, ov_child, "width", Value::Px(20.0));
        prop(&mut doc, ov_child, "height", Value::Px(20.0));
        prop(&mut doc, ov_child, "color", Value::Color(0xabcdefff));
        child(&mut doc, ov, ov_child);
        child(&mut doc, root, ov);

        let base = node(&mut doc, "Rect");
        prop(&mut doc, base, "width", Value::Px(20.0));
        prop(&mut doc, base, "height", Value::Px(20.0));
        prop(&mut doc, base, "color", Value::Color(0x123456ff));
        child(&mut doc, root, base);

        let scene = lower(&doc, (200.0, 200.0));
        let i_base = rect_idx(&scene, 0x123456ff).unwrap();
        let i_ov = rect_idx(&scene, 0xabcdefff).unwrap();
        assert!(i_ov > i_base, "overlay subtree paints above base: {i_ov} > {i_base}");
    }

    /// A *presented* `Sheet` emits a full-viewport dimmed scrim plus a card;
    /// a *non-presented* one emits nothing at all (neither chrome nor subtree).
    #[test]
    fn presented_sheet_emits_scrim_and_card_dormant_emits_nothing() {
        // --- Presented sheet ---
        let mut doc = Document::new();
        let root = node(&mut doc, "Stack");
        set_root(&mut doc, root);
        prop(&mut doc, root, "background", Value::Color(0x101010ff));

        let sheet = node(&mut doc, "Sheet");
        prop(&mut doc, sheet, "presented", Value::Bool(true));
        prop(&mut doc, sheet, "width", Value::Px(200.0));
        prop(&mut doc, sheet, "height", Value::Px(120.0));
        prop(&mut doc, sheet, "background", Value::Color(0x1c1c1eff));
        child(&mut doc, root, sheet);
        // The sheet's content paints on top of the card.
        let content = node(&mut doc, "Rect");
        prop(&mut doc, content, "width", Value::Px(50.0));
        prop(&mut doc, content, "height", Value::Px(50.0));
        prop(&mut doc, content, "color", Value::Color(0x778899ff));
        child(&mut doc, sheet, content);

        let (vw, vh) = (400.0, 400.0);
        let scene = lower(&doc, (vw, vh));

        // Scrim: a full-viewport FilledRect that is NOT the root clear color.
        let scrim_idx = scene.iter().position(|c| matches!(
            c,
            DrawCmd::FilledRect { x: 0.0, y: 0.0, w, h, color, .. }
                if (*w - vw).abs() < 0.5 && (*h - vh).abs() < 0.5 && *color != 0x101010ff
        )).expect("scrim painted full-viewport over the scene");

        // Card: the sheet's 200x120 panel.
        let card_idx = scene.iter().position(|c| matches!(
            c,
            DrawCmd::FilledRect { w, h, color: 0x1c1c1eff, .. }
                if (*w - 200.0).abs() < 0.5 && (*h - 120.0).abs() < 0.5
        )).expect("sheet card painted");

        // Content sits on top of the card.
        let content_idx = rect_idx(&scene, 0x778899ff).expect("sheet content painted");

        // The whole sheet (scrim, then card, then content) paints AFTER the
        // root background — it is hoisted on top of everything.
        let root_idx = rect_idx(&scene, 0x101010ff).expect("root background painted");
        assert!(scrim_idx > root_idx, "scrim above base scene");
        assert!(scrim_idx < card_idx, "scrim behind card");
        assert!(card_idx < content_idx, "card behind its content");

        // --- Non-presented sheet: nothing from the sheet subtree ---
        let mut doc2 = Document::new();
        let root2 = node(&mut doc2, "Stack");
        set_root(&mut doc2, root2);
        prop(&mut doc2, root2, "background", Value::Color(0x101010ff));

        let sheet2 = node(&mut doc2, "Sheet");
        // `presented` absent ⇒ dormant.
        prop(&mut doc2, sheet2, "width", Value::Px(200.0));
        prop(&mut doc2, sheet2, "height", Value::Px(120.0));
        prop(&mut doc2, sheet2, "background", Value::Color(0x1c1c1eff));
        child(&mut doc2, root2, sheet2);
        let content2 = node(&mut doc2, "Rect");
        prop(&mut doc2, content2, "width", Value::Px(50.0));
        prop(&mut doc2, content2, "height", Value::Px(50.0));
        prop(&mut doc2, content2, "color", Value::Color(0x778899ff));
        child(&mut doc2, sheet2, content2);

        let scene2 = lower(&doc2, (vw, vh));
        // Only the root background — no scrim, no card, no content.
        assert!(rect_idx(&scene2, 0x1c1c1eff).is_none(), "dormant sheet card not painted");
        assert!(rect_idx(&scene2, 0x778899ff).is_none(), "dormant sheet content not painted");
        // No full-viewport rect other than the root clear color.
        assert!(
            !scene2.iter().any(|c| matches!(
                c,
                DrawCmd::FilledRect { w, h, color, .. }
                    if (*w - vw).abs() < 0.5 && (*h - vh).abs() < 0.5 && *color != 0x101010ff
            )),
            "dormant sheet paints no scrim"
        );
    }

    /// A presented `Popover` is an *anchored* surface: it paints a card at its
    /// laid-out rect with NO dimming scrim (only modal Sheet/Alert dim).
    #[test]
    fn presented_popover_paints_card_without_scrim() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Stack");
        set_root(&mut doc, root);

        let pop = node(&mut doc, "Popover");
        prop(&mut doc, pop, "presented", Value::Bool(true));
        prop(&mut doc, pop, "width", Value::Px(160.0));
        prop(&mut doc, pop, "height", Value::Px(90.0));
        prop(&mut doc, pop, "background", Value::Color(0x2a2a2aff));
        child(&mut doc, root, pop);

        let (vw, vh) = (400.0, 400.0);
        let scene = lower(&doc, (vw, vh));
        // Card present.
        assert!(
            scene.iter().any(|c| matches!(
                c,
                DrawCmd::FilledRect { w, h, color: 0x2a2a2aff, .. }
                    if (*w - 160.0).abs() < 0.5 && (*h - 90.0).abs() < 0.5
            )),
            "popover card painted"
        );
        // No full-viewport scrim for an anchored popover.
        assert!(
            !scene.iter().any(|c| matches!(
                c,
                DrawCmd::FilledRect { x: 0.0, y: 0.0, w, h, .. }
                    if (*w - vw).abs() < 0.5 && (*h - vh).abs() < 0.5
            )),
            "popover paints no scrim"
        );
    }

    /// A `Menu` is an anchored popover-style surface: a card holding a list of
    /// items, painted on top of everything, gated by `presented`.
    #[test]
    fn presented_menu_paints_anchored_list() {
        let mut doc = Document::new();
        let root = node(&mut doc, "Stack");
        set_root(&mut doc, root);
        prop(&mut doc, root, "background", Value::Color(0x101010ff));

        let menu = node(&mut doc, "Menu");
        prop(&mut doc, menu, "presented", Value::Bool(true));
        prop(&mut doc, menu, "width", Value::Px(180.0));
        prop(&mut doc, menu, "background", Value::Color(0x2a2a2aff));
        child(&mut doc, root, menu);

        // Three menu items as rows in the card.
        let mut items = Vec::new();
        for i in 0..3 {
            let it = node(&mut doc, "Rect");
            prop(&mut doc, it, "height", Value::Px(24.0));
            prop(&mut doc, it, "color", Value::Color(0x010000ff | ((i as u32) << 8)));
            child(&mut doc, menu, it);
            items.push(it);
        }

        let scene = lower(&doc, (400.0, 400.0));
        let card_idx = rect_idx(&scene, 0x2a2a2aff).expect("menu card painted");
        // No scrim (anchored, not modal).
        assert!(
            !scene.iter().any(|c| matches!(
                c,
                DrawCmd::FilledRect { x: 0.0, y: 0.0, w, h, .. }
                    if (*w - 400.0).abs() < 0.5 && (*h - 400.0).abs() < 0.5 && !matches!(c, DrawCmd::FilledRect { color: 0x101010ff, .. })
            )),
            "menu paints no scrim"
        );
        // Items stack vertically inside the card, non-overlapping.
        let l = layout(&doc, (400.0, 400.0));
        let r0 = l.rect(items[0]).unwrap();
        let r1 = l.rect(items[1]).unwrap();
        assert!(r1.y >= r0.y + r0.h, "menu items stack vertically");
        // Items paint after (on top of) the menu card.
        let i0 = rect_idx(&scene, 0x010000ff).expect("item 0 painted");
        assert!(i0 > card_idx, "menu items paint over the card: {i0} > {card_idx}");
    }
}
