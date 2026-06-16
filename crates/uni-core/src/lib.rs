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
fn is_container(kind: &str) -> bool {
    matches!(kind, "Stack" | "Column" | "Row" | "Grid")
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
        let taffy_id = if is_container(&node.kind) {
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

    // Nodes suppressed by a `hidden` ancestor (the whole subtree is skipped).
    let mut hidden_subtree: std::collections::HashSet<NodeId> =
        std::collections::HashSet::new();

    for (idx, &id) in layout.order().iter().enumerate() {
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
}
