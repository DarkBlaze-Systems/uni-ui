//! # uni-runtime — the app / event-loop layer (rung 3, *interactive*)
//!
//! This crate **closes the accountability circle into the live UI**. The other
//! crates established the pieces:
//!
//! - `uni-ir` owns the [`Document`] and the audited [`Document::fire`] surface
//!   (every invocation lands in the append-only audit log carrying its
//!   [`Origin`]).
//! - `uni-dsl` parses `.uni` into a `Document` (callbacks → `SetCallback`).
//! - `uni-core` does [`uni_core::layout`], [`paint`], and [`hit_test`].
//! - `uni-render` rasterizes a `Scene` to a window and translates winit events
//!   into renderer-agnostic [`InputEvent`]s.
//!
//! [`Runtime`] threads them into one interactive loop:
//!
//! ```text
//!   winit event ─▶ translate_window_event ─▶ InputEvent
//!                                               │ PointerDown
//!                                               ▼
//!                       hit_test(layout, cursor) ─▶ NodeId
//!                                               │  bubble up parents
//!                                               ▼  until a node has callbacks["click"]
//!                       doc.fire(node, "click", Origin::Human) ─▶ Option<Action>
//!                                               │  look up Action.name in the registry
//!                                               ▼
//!                       handler(&mut store, origin)  (mutates STATE, not the doc)
//!                                               │
//!                                               ▼
//!                       sync_bindings()  (push store values into bound props,
//!                                               │  id-stable: same node ids)
//!                                               ▼
//!                       re-layout + re-paint ─▶ request_redraw
//! ```
//!
//! ## Rung 4: the DSL goes *live*
//!
//! `uni-reactor` supplies the reactive [`uni_reactor::Store`] and `uni-widgets`
//! supplies styled subtrees built from the same IR. The runtime holds a `Store`
//! and makes the DSL's `$bindings` **live** in the interactive loop: handlers
//! mutate *state* (`store.set(...)`), and [`Runtime::sync_bindings`] then pushes
//! the new values into the bound nodes' literal props **without changing node
//! ids or structure** — so hit-test and [`Document::fire`] keep working. The
//! bound UI therefore updates *via state*, never by a direct prop write in the
//! handler. (`If`/`For` structural expansion in the live loop is a known v0
//! limitation — it changes ids; `uni-reactor`'s `resolve` covers it for the
//! static path. Bindings-live is the goal here.)
//!
//! ## The cowork proof
//!
//! [`Runtime::ai_fire`] does the **same** thing via [`Origin::Ai`]: identical
//! hit-of-target → `fire` → registry dispatch → re-render path. The AI drives
//! the very same audited surface as the human — there is no privileged back
//! door. Both invocations are attributable in [`Document::audit_log`].
//!
//! The window/renderer plumbing is optional (feature-free, but only built when
//! a real window is created), so the core fire→handler→mutation cycle is fully
//! unit-testable headless — see the tests at the bottom of this file.
//!
//! ## D3 — incremental layout
//!
//! Every applied [`Mutation`] names the node(s) it touched. The runtime folds
//! those ids out of the audit log into a **dirty-node set**
//! ([`Runtime::dirty_nodes`]); a *clean* (empty) set lets `relayout` skip the
//! work entirely. When the set is non-empty, the dirty ids are handed to a
//! persistent [`uni_core::LayoutCache`], which re-styles **only** those nodes
//! and lets taffy reuse its cached layout for every clean subtree — clean
//! leaves are never even re-measured. The result is identical to a full
//! [`uni_core::layout`], at a fraction of the work on a localized edit.

mod gesture;
pub use gesture::{GestureEvent, GestureKind, Recognizer};

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use uni_core::{hit_test, paint, Layout, LayoutCache};
use uni_env::Env;
use uni_ir::{Action, Document, Mutation, NodeId, Origin};
use uni_reactor::Store;
use uni_render::{
    translate_window_event, InputEvent, PointerButton, RenderError, Renderer, Scene, WgpuRenderer,
};
use uni_spring::{Animation, Curve, Spring, SpringState};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::window::Window;

/// A handler bound to an [`Action`] name (rung 4: state-driven).
///
/// A handler receives the runtime's reactive [`Store`] and the [`Origin`] that
/// fired it, and its job is to **mutate state** — `store.set(key, value)` —
/// rather than touch the document tree directly. The audited anchor for the
/// invocation is the `doc.fire(target, event, origin)` [`Mutation::Invoke`] the
/// runtime already recorded *before* calling the handler; the handler's
/// `store.set` is the effect.
///
/// On the next [`Runtime::sync_bindings`] the new store values are pushed into
/// the bound nodes' literal props (id-stable), so the live UI reflects state on
/// the very next repaint. Handlers are `FnMut` so they can carry mutable state
/// (e.g. a counter), though the canonical place for app state is the [`Store`].
pub type Handler = Box<dyn FnMut(&mut Store, Origin)>;

/// A single active spring animation: drives one named prop on one node per-frame.
struct AnimationEntry {
    id: NodeId,
    prop: String,
    spring: Spring,
    state: SpringState,
}

// ============================================================================
// Implicit animation — descriptor-driven, time-curve interpolation
// ============================================================================
//
// The spring path above ([`Runtime::animate`] / [`Runtime::tick_animations`]) is
// the *explicit* surface: a caller hands a from/target and a `Spring`. The
// surface below is the *implicit* one: a node carries an `animation` descriptor
// (a `Value::Text` prop, e.g. `"300ms ease-in-out"`), and whenever a *watched*
// numeric prop on that node changes, the runtime automatically enqueues an
// animation that interpolates the prop from its old value to its new value
// across the descriptor's curve and duration. Transitions ride the same
// machinery: a node gaining/losing `presented` (or being inserted) animates its
// `opacity` 0<->1. All writes go back into the doc as audited `Origin::System`
// `SetProp`s — exactly the accountability discipline the rest of the runtime
// keeps.

// The animation *descriptor* type the implicit path samples is
// [`uni_spring::Animation`] — the SwiftUI-style `Curve` + `duration` pairing,
// whose [`uni_spring::Animation::sample`] maps elapsed time onto a `0.0..=1.0`
// progress fraction (and whose `Spring` curve reuses the same physics
// integrator the explicit path does — the "use uni-spring" hook). The runtime
// only adds the *parser* that turns a node's `animation` prop string into one of
// those descriptors, and the orchestration that composes the sampled fraction
// with an old→new value pair.

const MIN_DURATION: f32 = 1e-3;

/// The default implicit animation: `300ms`, ease-in-out.
fn default_animation() -> Animation {
    Animation::ease_in_out(0.3)
}

/// Parse an `animation` descriptor string into a [`uni_spring::Animation`].
///
/// Examples: `"300ms ease-in-out"`, `"0.5s spring"`, `"200ms linear"`,
/// `"150ms ease-out"`. Tokens are order-independent and case-insensitive;
/// anything unrecognized falls back to the default curve/duration. Duration
/// accepts `<n>ms`, `<n>s`, or a bare number (seconds).
fn parse_animation(desc: &str) -> Animation {
    let mut curve = Curve::EaseInOut;
    let mut duration = 0.3f32;
    for tok in desc.split_whitespace() {
        let lower = tok.to_ascii_lowercase();
        match lower.as_str() {
            "linear" => curve = Curve::Linear,
            "ease-in" | "easein" => curve = Curve::EaseIn,
            "ease-out" | "easeout" => curve = Curve::EaseOut,
            "ease-in-out" | "ease" | "easeinout" => curve = Curve::EaseInOut,
            // A spring curve, parameterised the SwiftUI way (response/damping).
            "spring" => {
                return {
                    // `spring_with` carries its own settle `duration`; keep any
                    // explicit duration token already seen, else use a snappy
                    // default period.
                    let resp = 0.3;
                    Animation::spring_with(resp, 0.825, duration.max(MIN_DURATION))
                };
            }
            _ => {
                if let Some(d) = parse_duration(&lower) {
                    duration = if d > MIN_DURATION { d } else { MIN_DURATION };
                }
            }
        }
    }
    Animation::new(curve, duration.max(MIN_DURATION))
}

/// A single active *implicit* animation: interpolates one numeric prop on one
/// node from `from` to `to` along an [`uni_spring::Animation`] curve over
/// wall-clock time.
struct ImplicitEntry {
    id: NodeId,
    prop: String,
    from: f32,
    to: f32,
    anim: Animation,
    elapsed: f32,
}

impl ImplicitEntry {
    /// The current interpolated value at this entry's elapsed time.
    ///
    /// Composes [`uni_spring::Animation::sample`]'s `0..=1` progress with the
    /// `from`→`to` span. Once finished the value snaps exactly onto `to`.
    fn value(&self) -> f32 {
        if self.is_done() {
            return self.to;
        }
        let f = self.anim.sample(self.elapsed);
        self.from + (self.to - self.from) * f
    }

    /// Whether the animation has run out its descriptor's duration. (uni-spring's
    /// `sample` normalizes every curve — spring included — to reach `1.0` at
    /// `duration`, so a single time comparison settles them all.)
    fn is_done(&self) -> bool {
        self.elapsed >= self.anim.duration
    }
}

/// Parse a duration token: `<n>ms`, `<n>s`, or a bare number (seconds).
fn parse_duration(tok: &str) -> Option<f32> {
    if let Some(ms) = tok.strip_suffix("ms") {
        ms.parse::<f32>().ok().map(|v| v / 1000.0)
    } else if let Some(s) = tok.strip_suffix('s') {
        s.parse::<f32>().ok()
    } else {
        tok.parse::<f32>().ok()
    }
}

/// Pull a numeric value out of a [`uni_ir::Value`] for animation purposes.
/// `Px`, `Float`, `Int` and `Bool` all map to an `f32`; anything else is `None`.
fn value_as_f32(v: &uni_ir::Value) -> Option<f32> {
    match v {
        uni_ir::Value::Px(x) => Some(*x),
        uni_ir::Value::Float(x) => Some(*x as f32),
        uni_ir::Value::Int(x) => Some(*x as f32),
        uni_ir::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// The numeric props the implicit-animation watcher tracks for change. A change
/// on any of these (on a node carrying an `animation` descriptor) enqueues an
/// interpolation from the old value to the new one.
const WATCHED_PROPS: &[&str] = &["width", "height", "opacity", "x", "y", "corner_radius"];

/// The interactive runtime: the live document, its layout/viewport, the handler
/// registry, and (optionally) a window + GPU renderer.
///
/// Construct headless with [`Runtime::new`] (tests, the AI driving the surface
/// with no window) or run it on a real window with [`Runtime::run`].
pub struct Runtime {
    /// The live UI tree + its audit log.
    doc: Document,
    /// The reactive state store. Handlers mutate *this* (not the doc directly);
    /// [`Runtime::sync_bindings`] then pushes its values into the bound nodes'
    /// literal props before each re-layout, keeping node ids stable.
    store: Store,
    /// The most recently computed layout (re-computed after every action and
    /// on resize). Hit-testing and painting read from this.
    layout: Layout,
    /// The logical-pixel viewport the layout is computed for.
    viewport: (f32, f32),
    /// Action name → handler. Human- and AI-fired callbacks dispatch through
    /// this *same* map (the cowork contract).
    registry: HashMap<String, Handler>,
    /// Last known cursor position in logical px (threaded through the winit
    /// translator so button presses carry coordinates).
    cursor: (f32, f32),
    /// The window + renderer. `None` when running headless.
    window: Option<Arc<Window>>,
    renderer: Option<WgpuRenderer>,
    /// The current environment derived from the viewport.
    env: Env,
    /// Active spring animations: each entry drives one prop on one node per-frame.
    animations: Vec<AnimationEntry>,
    /// Active *implicit* (descriptor-driven, time-curve) animations. Populated by
    /// [`Runtime::tick`] when a watched numeric prop changes on a node carrying an
    /// `animation` descriptor, or when a `presented`/insertion transition fires.
    implicit: Vec<ImplicitEntry>,
    /// Last-seen value of every watched numeric prop on every animated node,
    /// keyed by `(node, prop)`. [`Runtime::tick`] diffs the live doc against this
    /// to detect the changes that start an implicit animation. The *displayed*
    /// value (mid-flight interpolation) is what's stored, so a change redirects
    /// from where the prop currently is, not from the stale target.
    prop_snapshot: HashMap<(NodeId, String), f32>,
    /// The set of node ids that existed (and were `presented`) on the previous
    /// tick, so insertions and presented-gain/loss transitions can be detected.
    presence_snapshot: BTreeSet<NodeId>,
    presented_snapshot: BTreeSet<NodeId>,
    /// Currently focused node (Tab/arrow-key navigation target).
    focused: Option<NodeId>,
    /// **D3 — incremental-layout foundation.** The set of node ids touched by
    /// applied mutations since the last [`relayout`](Runtime::relayout). Populated
    /// from the audit log (every prop/child/binding edit names a node), and
    /// drained on relayout. A *clean* set (empty) means nothing structural moved,
    /// so [`relayout`](Runtime::relayout) can short-circuit. See
    /// [`Runtime::dirty_nodes`].
    dirty: BTreeSet<NodeId>,
    /// How far into the audit log we've already folded into `dirty`. Lets us
    /// scan only the *new* edits since the last sweep.
    audit_cursor: usize,
    /// **D3.** Persistent incremental-layout cache: re-styles only the `dirty`
    /// nodes each relayout and lets taffy skip clean subtrees. See
    /// [`uni_core::LayoutCache`].
    layout_cache: LayoutCache,
    /// **Navigation stack.** An ordered list of route keys; the last element is
    /// the *current* route. [`Runtime::navigate`] pushes, [`Runtime::back`]
    /// pops. The bottom route is never popped, so there is always somewhere to
    /// be. Drives screen/destination selection in a router-style UI; both edits
    /// ride the same audited action path (see [`Runtime::navigate`]).
    nav_stack: Vec<String>,
    /// **S5 — gesture recognizers.** SwiftUI-style tap/long-press/drag/magnify/
    /// rotation recognizers attached to nodes. Each consumes the same
    /// [`InputEvent`]s the pointer path does (plus time, via [`Runtime::tick`])
    /// and dispatches its recognized event through the *same* audited
    /// `dispatch` path a click takes. See [`gesture`].
    gestures: Vec<Recognizer>,
}

impl Runtime {
    /// Build a runtime around an existing [`Document`] for the given logical
    /// `viewport`. No window is created — this is the headless/testable form.
    pub fn new(doc: Document, viewport: (f32, f32)) -> Self {
        let env = Env::for_window(viewport.0, viewport.1);
        let mut rt = Runtime {
            doc,
            store: Store::new(),
            layout: Layout::default(),
            viewport,
            registry: HashMap::new(),
            cursor: (0.0, 0.0),
            window: None,
            renderer: None,
            env,
            animations: Vec::new(),
            implicit: Vec::new(),
            prop_snapshot: HashMap::new(),
            presence_snapshot: BTreeSet::new(),
            presented_snapshot: BTreeSet::new(),
            focused: None,
            dirty: BTreeSet::new(),
            audit_cursor: 0,
            layout_cache: LayoutCache::new(),
            nav_stack: Vec::new(),
            gestures: Vec::new(),
        };
        // Push any state already present into the bound props, then lay out, so
        // the very first frame reflects the store (not just literal defaults).
        rt.sync_bindings();
        rt.relayout();
        // Seed the implicit-animation watcher so the *first* real change (not the
        // initial prop values) is what triggers an animation.
        rt.seed_animation_snapshots();
        rt
    }

    /// Parse `.uni` source into a [`Document`] and wrap it in a runtime.
    pub fn from_uni(src: &str, viewport: (f32, f32)) -> Result<Self, uni_dsl::ParseError> {
        Ok(Runtime::new(uni_dsl::parse(src)?, viewport))
    }

    /// Register (or replace) the handler run when an [`Action`] of this `name`
    /// fires. The handler mutates the document; the runtime re-lays-out and
    /// repaints afterward.
    pub fn register(&mut self, name: impl Into<String>, handler: Handler) {
        self.registry.insert(name.into(), handler);
    }

    /// Immutable access to the live document (and thus its [`Document::audit_log`]).
    pub fn doc(&self) -> &Document {
        &self.doc
    }

    /// Immutable access to the reactive [`Store`] — the canonical home of app
    /// state that bindings read from.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Mutable access to the [`Store`]. Seed initial state here before running;
    /// call [`Runtime::sync_bindings`] (or let `dispatch` do it) to
    /// push the values into the bound props.
    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    /// The current [`Env`] (viewport-derived: size class, accent, input mode).
    pub fn env(&self) -> Env {
        self.env
    }

    /// Mutable access — override input_mode, build_variant, etc. before running.
    pub fn env_mut(&mut self) -> &mut Env {
        &mut self.env
    }

    /// Start (or replace) a spring animation driving `prop` on `id` from
    /// `from` toward `target`. The animation runs until settled.
    pub fn animate(&mut self, id: NodeId, prop: &str, from: f32, target: f32, spring: Spring) {
        self.animations.retain(|a| !(a.id == id && a.prop == prop));
        self.animations.push(AnimationEntry {
            id,
            prop: prop.to_string(),
            spring,
            state: SpringState::new(from, target),
        });
    }

    /// True when all animations have settled.
    pub fn animations_settled(&self) -> bool {
        self.animations.is_empty()
    }

    /// Step all active spring animations by `dt` seconds, apply the resulting
    /// `Px` prop values to the document (Origin::System), remove settled ones,
    /// and re-layout if anything moved. Returns `true` when animations remain.
    pub fn tick_animations(&mut self, dt: f32) -> bool {
        if self.animations.is_empty() {
            return false;
        }
        let running = self.step_springs(dt);
        self.relayout();
        running
    }

    /// Step every active spring animation by `dt`, write the resulting `Px` prop
    /// values into the doc (audited `Origin::System`), and remove settled ones —
    /// **without** relaying out. The relayout is the caller's, so several
    /// animation paths can share a single recompute per frame (see
    /// [`Runtime::tick`]). Returns `true` while springs remain.
    fn step_springs(&mut self, dt: f32) -> bool {
        const EPS: f32 = 0.5; // pixel-level settle threshold
        if self.animations.is_empty() {
            return false;
        }
        let mut updates: Vec<(NodeId, String, f32)> = Vec::new();
        for entry in &mut self.animations {
            entry.state.step(&entry.spring, dt);
            updates.push((entry.id, entry.prop.clone(), entry.state.value));
        }
        self.animations.retain(|a| !a.state.is_settled(EPS));
        for (id, prop, value) in updates {
            let _ = self.doc.apply_from(
                Origin::System,
                Mutation::SetProp {
                    id,
                    key: prop,
                    value: uni_ir::Value::Px(value),
                },
            );
        }
        !self.animations.is_empty()
    }

    // -- implicit, descriptor-driven animation --------------------------------

    /// Read a node's `animation` descriptor prop, if it carries one.
    ///
    /// The descriptor is a `Value::Text` like `"300ms ease-in-out"`. A node with
    /// no `animation` prop is *not* implicitly animated — its prop changes apply
    /// instantly, as before.
    fn node_animation(&self, id: NodeId) -> Option<Animation> {
        let node = self.doc.get(id)?;
        match node.props.get("animation") {
            Some(uni_ir::Value::Text(desc)) => Some(parse_animation(desc)),
            // A bare `animation: true` (or any present non-text value) opts in
            // with the default curve/duration.
            Some(_) => Some(default_animation()),
            None => None,
        }
    }

    /// Snapshot the current watched-prop values + presence/presented sets without
    /// enqueueing anything. Used at construction so only *subsequent* changes
    /// animate.
    fn seed_animation_snapshots(&mut self) {
        let mut snap: HashMap<(NodeId, String), f32> = HashMap::new();
        let mut present: BTreeSet<NodeId> = BTreeSet::new();
        let mut presented: BTreeSet<NodeId> = BTreeSet::new();
        if let Some(root) = self.doc.root() {
            let mut stack = vec![root];
            while let Some(id) = stack.pop() {
                let Some(node) = self.doc.get(id) else {
                    continue;
                };
                present.insert(id);
                if Self::node_is_presented(node) {
                    presented.insert(id);
                }
                for key in WATCHED_PROPS {
                    if let Some(v) = node.props.get(*key).and_then(value_as_f32) {
                        snap.insert((id, (*key).to_string()), v);
                    }
                }
                stack.extend(node.children.iter().copied());
            }
        }
        self.prop_snapshot = snap;
        self.presence_snapshot = present;
        self.presented_snapshot = presented;
    }

    /// Whether a node is currently "presented" — its `presented` prop is a truthy
    /// bool/number. The presentation flag drives opacity transitions.
    fn node_is_presented(node: &uni_ir::Node) -> bool {
        match node.props.get("presented") {
            Some(uni_ir::Value::Bool(b)) => *b,
            Some(v) => value_as_f32(v).map(|x| x != 0.0).unwrap_or(false),
            None => false,
        }
    }

    /// **Detect changes and enqueue implicit animations.**
    ///
    /// Walk the live doc and, for every node carrying an `animation` descriptor,
    /// compare each watched numeric prop against the last-seen snapshot. A change
    /// enqueues (or redirects) an [`ImplicitEntry`] interpolating from the
    /// currently-*displayed* value to the new value over the descriptor's curve.
    ///
    /// Transitions: a node that newly *gains* `presented` (or is freshly
    /// inserted while presented) animates `opacity` 0→1; a node that *loses*
    /// `presented` animates `opacity` 1→0. These ride the same `ImplicitEntry`
    /// queue, using the node's descriptor (or the default animation).
    fn detect_implicit_animations(&mut self) {
        let mut to_enqueue: Vec<ImplicitEntry> = Vec::new();
        let mut next_present: BTreeSet<NodeId> = BTreeSet::new();
        let mut next_presented: BTreeSet<NodeId> = BTreeSet::new();

        if let Some(root) = self.doc.root() {
            let mut stack = vec![root];
            while let Some(id) = stack.pop() {
                let Some(node) = self.doc.get(id) else {
                    continue;
                };
                next_present.insert(id);
                let presented_now = Self::node_is_presented(node);
                if presented_now {
                    next_presented.insert(id);
                }
                let anim = self.node_animation(id);

                // --- watched numeric prop changes -------------------------
                if let Some(anim) = anim {
                    for key in WATCHED_PROPS {
                        let Some(new_v) = node.props.get(*key).and_then(value_as_f32) else {
                            continue;
                        };
                        let skey = (id, (*key).to_string());
                        match self.prop_snapshot.get(&skey).copied() {
                            Some(old_v) if (old_v - new_v).abs() > f32::EPSILON => {
                                // Redirect from where the prop is *currently
                                // displayed* (mid-flight), not the stale old value.
                                let from = self
                                    .implicit
                                    .iter()
                                    .find(|e| e.id == id && e.prop == *key)
                                    .map(ImplicitEntry::value)
                                    .unwrap_or(old_v);
                                to_enqueue.push(ImplicitEntry {
                                    id,
                                    prop: (*key).to_string(),
                                    from,
                                    to: new_v,
                                    anim,
                                    elapsed: 0.0,
                                });
                            }
                            _ => {}
                        }
                    }
                }

                // --- presented / insertion opacity transitions ------------
                let was_present = self.presence_snapshot.contains(&id);
                let was_presented = self.presented_snapshot.contains(&id);
                let trans = anim.unwrap_or_else(default_animation);
                if presented_now && (!was_presented || !was_present) {
                    // Gained presented (or inserted while presented): fade in.
                    to_enqueue.push(ImplicitEntry {
                        id,
                        prop: "opacity".to_string(),
                        from: 0.0,
                        to: 1.0,
                        anim: trans,
                        elapsed: 0.0,
                    });
                } else if !presented_now && was_presented {
                    // Lost presented: fade out.
                    to_enqueue.push(ImplicitEntry {
                        id,
                        prop: "opacity".to_string(),
                        from: 1.0,
                        to: 0.0,
                        anim: trans,
                        elapsed: 0.0,
                    });
                }

                stack.extend(node.children.iter().copied());
            }
        }

        // Apply: a new entry for the same (id, prop) replaces any in-flight one.
        for entry in to_enqueue {
            // Record the new *target* in the snapshot so we don't re-detect it.
            self.prop_snapshot
                .insert((entry.id, entry.prop.clone()), entry.to);
            self.implicit
                .retain(|e| !(e.id == entry.id && e.prop == entry.prop));
            self.implicit.push(entry);
        }
        self.presence_snapshot = next_present;
        self.presented_snapshot = next_presented;
    }

    /// **Advance implicit animations by `dt`, write interpolated props back.**
    ///
    /// For each active [`ImplicitEntry`]: advance its clock, compute the
    /// interpolated value, and apply it to the doc as an audited `Origin::System`
    /// `SetProp` (a `Value::Px`). Settled entries (clock past the descriptor's
    /// duration) snap to the target and are removed. Returns `true` while any
    /// implicit animation is still running.
    ///
    /// The displayed value is also written back into the snapshot so a *further*
    /// change mid-flight redirects from the current on-screen value.
    fn advance_implicit(&mut self, dt: f32) -> bool {
        if self.implicit.is_empty() {
            return false;
        }
        let mut updates: Vec<(NodeId, String, f32)> = Vec::new();
        for entry in &mut self.implicit {
            entry.elapsed += dt;
            let v = entry.value();
            updates.push((entry.id, entry.prop.clone(), v));
        }
        // Drop the finished ones (they've reached `to`).
        self.implicit.retain(|e| !e.is_done());

        for (id, prop, value) in updates {
            // Keep the snapshot in step with what's displayed so an interrupting
            // change redirects from here, and a settle records the final target.
            self.prop_snapshot.insert((id, prop.clone()), value);
            let _ = self.doc.apply_from(
                Origin::System,
                Mutation::SetProp {
                    id,
                    key: prop,
                    value: uni_ir::Value::Px(value),
                },
            );
        }
        !self.implicit.is_empty()
    }

    /// **The implicit-animation frame.** Detect prop/presentation changes and
    /// enqueue animations for them, advance every active animation (spring +
    /// implicit) by `dt` seconds, write the interpolated props back into the doc
    /// (audited `Origin::System`), and re-layout. Returns `true` while any
    /// animation is still running.
    ///
    /// This is the single entry a frame loop calls. A prop change on an
    /// `animation`-bearing node does **not** snap instantly: it interpolates
    /// across successive `tick` calls and reaches the target when the curve
    /// completes, at which point the entry settles and is removed.
    pub fn tick(&mut self, dt: f32) -> bool {
        self.detect_implicit_animations();
        // Advance the explicit spring path (no-op + no relayout if empty), then
        // the implicit path. Both write `Origin::System` SetProps into the doc.
        let spring_running = if self.animations.is_empty() {
            false
        } else {
            // Step springs but defer the relayout to the single one below.
            self.step_springs(dt)
        };
        let implicit_running = self.advance_implicit(dt);
        // One relayout folds in every prop the two paths just wrote.
        self.relayout();
        spring_running || implicit_running
    }

    /// True when no implicit animation is in flight.
    pub fn implicit_settled(&self) -> bool {
        self.implicit.is_empty()
    }

    /// **Push live state into the literal props of bound nodes — id-stable.**
    ///
    /// Walk the live [`Document`]; for every node carrying `bindings`, look up
    /// each bound key in the [`Store`] and, if present, apply an
    /// `Origin::System` [`Mutation::SetProp`] on that *same* node id. This makes
    /// the DSL's `$bindings` live without ever changing node ids or tree
    /// structure (so hit-test and [`Document::fire`] keep targeting the same
    /// nodes). Called before every `relayout`.
    ///
    /// Structural binding nodes (`If` / `For`) are deliberately *not* expanded
    /// here: live structural reconciliation would mint fresh ids and is a later
    /// rung. v0 makes *bindings* live; `uni-reactor`'s `resolve` unit-tests
    /// cover `If`/`For` expansion for the static-resolve path.
    pub fn sync_bindings(&mut self) {
        // Snapshot (id, key, resolved value) first so we don't borrow the doc
        // while applying mutations. Only keys present in the store are applied;
        // an unset binding leaves any existing literal prop untouched. We walk
        // the live tree from the root (ids are preserved across the walk).
        let mut updates: Vec<(NodeId, String, uni_ir::Value)> = Vec::new();
        if let Some(root) = self.doc.root() {
            let mut stack = vec![root];
            while let Some(id) = stack.pop() {
                let Some(node) = self.doc.get(id) else {
                    continue;
                };
                for (key, binding) in &node.bindings {
                    if let Some(value) = self.store.get(binding.expr.as_str()) {
                        updates.push((id, key.clone(), value));
                    }
                }
                stack.extend(node.children.iter().copied());
            }
        }
        for (id, key, value) in updates {
            // Origin::System: this is the runtime applying state, not a Human
            // or AI edit. The Human/AI provenance lives on the fire() Invoke.
            self.doc
                .apply_from(Origin::System, Mutation::SetProp { id, key, value })
                .expect("bound node exists (walked from the live doc)");
        }
    }

    /// The current computed layout.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The logical viewport the layout is computed for.
    pub fn viewport(&self) -> (f32, f32) {
        self.viewport
    }

    /// **D3 — fold new audit-log edits into the dirty-node set.**
    ///
    /// Every structural [`Mutation`] names the node(s) it touched; we scan the
    /// edits appended since the last sweep (tracked by `audit_cursor`) and add
    /// their ids to `self.dirty`. This is the populate-from-applied-mutations
    /// half of incremental layout — the foundation a later rung uses to relayout
    /// only dirty subtrees.
    fn mark_dirty_from_log(&mut self) {
        // Collect the touched ids first so the immutable audit-log borrow ends
        // before we mutate `self.dirty`.
        let log = self.doc.audit_log();
        let mut touched: Vec<NodeId> = Vec::new();
        for edit in &log[self.audit_cursor..] {
            match &edit.mutation {
                Mutation::CreateNode { id, .. }
                | Mutation::SetRoot { id }
                | Mutation::SetProp { id, .. }
                | Mutation::RemoveProp { id, .. }
                | Mutation::RemoveNode { id }
                | Mutation::SetCallback { id, .. }
                | Mutation::RemoveCallback { id, .. }
                | Mutation::SetBinding { id, .. }
                | Mutation::RemoveBinding { id, .. }
                | Mutation::Reconstruct { id, .. } => {
                    touched.push(*id);
                }
                // Parent/child edits dirty *both* ends (the subtree moved).
                Mutation::AppendChild { parent, child }
                | Mutation::RemoveChild { parent, child } => {
                    touched.push(*parent);
                    touched.push(*child);
                }
                // Invoke is a pure audit record — it changes no tree geometry.
                Mutation::Invoke { .. } => {}
            }
        }
        let new_cursor = log.len();
        self.dirty.extend(touched);
        self.audit_cursor = new_cursor;
    }

    /// The set of node ids touched by applied mutations since the last layout.
    ///
    /// **D3 foundation.** A *clean* (empty) set means nothing structural changed,
    /// so layout can be skipped. The set is folded from the audit log on each
    /// `relayout` and drained there. Exposed so a caller (or
    /// a future incremental-layout pass) can see exactly which subtrees are dirty.
    pub fn dirty_nodes(&self) -> &BTreeSet<NodeId> {
        &self.dirty
    }

    /// Recompute the layout for the current document + viewport. Called after
    /// every action and on resize.
    ///
    /// **D3 (first approximation).** Before recomputing, fold any new audit-log
    /// edits into the dirty-node set. If that set is *empty* — nothing was
    /// touched since the last layout — short-circuit and keep the existing
    /// layout (the clean-subtree skip). Otherwise recompute the whole layout and
    /// drain the dirty set. (Recomputing the *whole* tree on any dirty node is
    /// the conservative first cut; relaying out only the dirty subtrees is the
    /// next rung — see the crate-level D3 note.)
    fn relayout(&mut self) {
        self.mark_dirty_from_log();
        // First-frame / explicit-relayout guard: an empty layout must always be
        // computed once even if no mutation was logged (e.g. viewport-only init).
        if self.dirty.is_empty() && !self.layout.order().is_empty() {
            return;
        }
        // Incremental: the cache re-styles only the dirty nodes and lets taffy
        // skip every clean subtree (clean leaves are never re-measured).
        self.layout = self
            .layout_cache
            .compute(&self.doc, self.viewport, &self.dirty);
        self.dirty.clear();
    }

    /// Force a full layout recompute regardless of the dirty set (used when the
    /// viewport itself changes, which dirties geometry without any tree edit).
    fn relayout_force(&mut self) {
        self.mark_dirty_from_log();
        // A viewport change re-flows from the root; the cache detects the new
        // viewport and recomputes accordingly (still reusing the taffy tree).
        self.layout = self
            .layout_cache
            .compute(&self.doc, self.viewport, &self.dirty);
        self.dirty.clear();
    }

    /// Set the viewport and recompute the layout.
    ///
    /// A viewport change dirties *geometry* without touching the tree, so this
    /// forces a full relayout (the dirty-node short-circuit only covers
    /// tree-edit-driven changes).
    pub fn set_viewport(&mut self, viewport: (f32, f32)) {
        self.viewport = viewport;
        self.env = Env::for_window(viewport.0, viewport.1);
        self.relayout_force();
    }

    /// Paint the current layout into a [`Scene`].
    pub fn scene(&self) -> Scene {
        paint(&self.doc, &self.layout)
    }

    // -- the audited dispatch core (shared by human input and AI fire) --------

    /// Fire `event` on `target` with the given [`Origin`], then run the named
    /// handler (if the fire produced an [`Action`] we have a handler for) and
    /// re-layout. Returns `true` if a handler ran (i.e. the document may have
    /// changed).
    ///
    /// This is the single path **both** human input ([`Runtime::on_input`]) and
    /// the AI ([`Runtime::ai_fire`]) funnel through — no back door. `fire`
    /// itself records the audited [`uni_ir::Mutation::Invoke`] carrying `origin`.
    fn dispatch(&mut self, target: NodeId, event: &str, origin: Origin) -> bool {
        // 1. Fire on the IR — records an Origin-audited Invoke in the log (the
        //    required accountability anchor) and hands back the Action to run
        //    (or None if no such callback).
        let Some(Action { name, .. }) = self.doc.fire(target, event, origin) else {
            return false;
        };
        // 2. Look the action up in the registry and run it. We temporarily take
        //    the handler out of the map so the closure can borrow `&mut store`
        //    without aliasing `self.registry` (handlers may re-register, etc.).
        let Some(mut handler) = self.registry.remove(&name) else {
            // Fired and audited, but no behavior bound — still a valid record.
            return true;
        };
        // The handler mutates STATE, not the doc directly. It receives the
        // store and the firing Origin (so it can attribute its own changes).
        handler(&mut self.store, origin);
        // Put it back (unless the handler registered a replacement under the
        // same name while running).
        self.registry.entry(name).or_insert(handler);
        // 3. State changed → push it into the bound props (id-stable), then
        //    recompute layout so the next paint reflects the new state.
        self.sync_bindings();
        self.relayout();
        true
    }

    /// Find the node that should handle `event` for a pointer at `point`:
    /// hit-test to the topmost node, then **bubble up parents** until one has a
    /// callback registered for `event`. Returns `None` if nothing in the chain
    /// handles it.
    fn bubble_to_handler(&self, point: (f32, f32), event: &str) -> Option<NodeId> {
        let mut current = hit_test(&self.layout, point)?;
        loop {
            let node = self.doc.get(current)?;
            if node.callbacks.contains_key(event) {
                return Some(current);
            }
            match node.parent {
                Some(parent) => current = parent,
                None => return None,
            }
        }
    }

    /// Feed one renderer-agnostic [`InputEvent`] in. On a left `PointerDown`,
    /// hit-test + bubble to a `"click"` handler and fire it as [`Origin::Human`].
    /// `Tab` moves keyboard focus forward; `Enter`/`Space` activate the focused node.
    ///
    /// Returns `true` if the event was handled (so a caller without a window can
    /// tell something happened; the windowed loop uses it to request a redraw).
    pub fn on_input(&mut self, input: &InputEvent) -> bool {
        match input {
            InputEvent::PointerMoved { x, y } => {
                self.cursor = (*x, *y);
                false
            }
            InputEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
            } => {
                self.cursor = (*x, *y);
                // If the hit node is focusable, move keyboard focus to it.
                if let Some(hit) = self.bubble_to_handler((*x, *y), "click") {
                    self.focused = Some(hit);
                    self.dispatch(hit, "click", Origin::Human)
                } else {
                    false
                }
            }
            InputEvent::KeyDown { key } if key == "Tab" => self.move_focus(true),
            InputEvent::KeyDown { key } if key == "Enter" || key == " " => self.activate_focused(),
            _ => false,
        }
    }

    /// **Cowork proof.** Fire `event` on `target` as the **AI** — the exact same
    /// `fire(Origin::Ai)` → registry-dispatch → re-layout path the human input
    /// takes, just with [`Origin::Ai`] provenance. Demonstrates the AI drives
    /// the same audited surface as the human, with no privileged back door.
    ///
    /// Returns `true` if a handler ran.
    pub fn ai_fire(&mut self, target: NodeId, event: &str) -> bool {
        self.dispatch(target, event, Origin::Ai)
    }

    /// **Enumerate the registered callbacks** as `(node, event, action_name)`
    /// triples — every place an event is bound to a named action in the live
    /// document, in layout order.
    ///
    /// This is the cowork *index*: the menu of things that can be invoked on
    /// this UI. A human reads it off the screen; the AI reads it off this list.
    /// Both then drive the very same [`invoke`](Runtime::invoke) path — the
    /// list is the proof that the AI's surface is exactly the human's, no more
    /// and no less.
    pub fn actions(&self) -> Vec<(NodeId, String, String)> {
        let mut out = Vec::new();
        for id in self.layout.order() {
            if let Some(node) = self.doc.get(*id) {
                for (event, action) in &node.callbacks {
                    out.push((*id, event.clone(), action.name.clone()));
                }
            }
        }
        out
    }

    /// **Invoke `event` on `node` with an explicit [`Origin`].**
    ///
    /// The single, origin-parameterized entry the cowork contract turns on: it
    /// routes through the **identical** handler path as a human pointer event —
    /// [`Document::fire`] records the audited [`Origin`]-tagged `Invoke`, the
    /// registry dispatches the named handler, state syncs, and the layout
    /// recomputes. Pass [`Origin::Ai`] and the AI drives precisely what a human
    /// click drives; pass [`Origin::Human`] and it is the human's. There is no
    /// second code path — `ai_fire` and human input both funnel here via
    /// `dispatch`.
    ///
    /// Returns `true` if a handler ran.
    pub fn invoke(&mut self, node: NodeId, event: &str, origin: Origin) -> bool {
        self.dispatch(node, event, origin)
    }

    // -- navigation + presentation state (over Store + the audited path) ------

    /// The current route — the top of the navigation stack — or `None` when the
    /// stack is empty (nothing has been navigated to yet).
    ///
    /// The route stack is plain presentation state: a `Vec<String>` of route
    /// keys threaded through the same audited action path as everything else
    /// (see [`Runtime::navigate`]). A router-style UI reads this to pick which
    /// screen/destination to show.
    pub fn route(&self) -> Option<&str> {
        self.nav_stack.last().map(String::as_str)
    }

    /// The whole navigation stack, bottom → top (the top is the current route).
    pub fn nav_stack(&self) -> &[String] {
        &self.nav_stack
    }

    /// **Push a route onto the navigation stack with an explicit [`Origin`].**
    ///
    /// This is the navigation primitive both a human tap and an AI drive funnel
    /// through. It records an audited [`Mutation::Invoke`] on the document root —
    /// tagged with `origin`, exactly like a fired callback — *before* mutating
    /// the route stack, so every navigation is attributable in
    /// [`Document::audit_log`]. The audited event encodes the destination as
    /// `"navigate:<route>"`. It also mirrors the current route into the [`Store`]
    /// under the `"route"` key, so a binding to `$route` reflects the destination
    /// on the next [`sync_bindings`](Runtime::sync_bindings).
    ///
    /// Returns `true` (a navigation always takes effect).
    pub fn navigate(&mut self, route: impl Into<String>, origin: Origin) -> bool {
        let route = route.into();
        // Audited anchor: the same Invoke surface human input rides, so a
        // navigation is attributable to Human or Ai in the log. The event string
        // carries the destination.
        self.record_nav_invoke(format!("navigate:{route}"), origin);
        self.nav_stack.push(route.clone());
        self.store.set("route", uni_ir::Value::Text(route));
        self.sync_bindings();
        self.relayout();
        true
    }

    /// **Pop the current route off the navigation stack with an explicit
    /// [`Origin`].**
    ///
    /// The inverse of [`navigate`](Runtime::navigate): records an audited
    /// `"back"` [`Mutation::Invoke`] on the root (tagged with `origin`), then
    /// pops the top route and re-mirrors the now-current route into the
    /// [`Store`]'s `"route"` key. Returns `true` if a route was popped, `false`
    /// when the stack was already empty (nothing to go back to).
    pub fn back(&mut self, origin: Origin) -> bool {
        if self.nav_stack.is_empty() {
            return false;
        }
        self.record_nav_invoke("back".to_string(), origin);
        self.nav_stack.pop();
        match self.nav_stack.last() {
            Some(route) => self.store.set("route", uni_ir::Value::Text(route.clone())),
            None => self.store.set("route", uni_ir::Value::Text(String::new())),
        }
        self.sync_bindings();
        self.relayout();
        true
    }

    /// **Present a Sheet/Alert/Popover bound to `key`.**
    ///
    /// Sets the [`Store`] key to `Value::Bool(true)` through the same audited
    /// action path (recording a `"present"` [`Mutation::Invoke`] on the root,
    /// tagged `origin`), then syncs bindings + relays out so a node whose
    /// `presented` (or any `$key`-bound bool prop) reflects the new state on the
    /// next paint. Returns `true`.
    pub fn present(&mut self, key: impl Into<String>, origin: Origin) -> bool {
        self.set_presented(key.into(), true, "present", origin)
    }

    /// **Dismiss the Sheet/Alert/Popover bound to `key`.**
    ///
    /// The inverse of [`present`](Runtime::present): clears the [`Store`] key to
    /// `Value::Bool(false)` through the audited path (a `"dismiss"`
    /// [`Mutation::Invoke`] on the root, tagged `origin`). Returns `true`.
    pub fn dismiss(&mut self, key: impl Into<String>, origin: Origin) -> bool {
        self.set_presented(key.into(), false, "dismiss", origin)
    }

    /// True when the presentation key is currently set to `Bool(true)` in the
    /// store (i.e. the bound Sheet/Alert/Popover is showing).
    pub fn is_presented(&self, key: &str) -> bool {
        matches!(self.store.get(key), Some(uni_ir::Value::Bool(true)))
    }

    /// Shared core for [`present`](Runtime::present) / [`dismiss`](Runtime::dismiss):
    /// record an audited Invoke on the root naming the action + key, flip the
    /// bound store bool, then sync + relayout so a bound node reflects it.
    fn set_presented(&mut self, key: String, value: bool, action: &str, origin: Origin) -> bool {
        self.record_nav_invoke(format!("{action}:{key}"), origin);
        self.store.set(key, uni_ir::Value::Bool(value));
        self.sync_bindings();
        self.relayout();
        true
    }

    /// Record an audited [`Mutation::Invoke`] on the document root carrying
    /// `event` and tagged with `origin` — the shared accountability anchor for
    /// navigation and presentation state changes. Mirrors the same audited
    /// fire() surface human input rides; no-op only when the doc has no root.
    fn record_nav_invoke(&mut self, event: String, origin: Origin) {
        if let Some(root) = self.doc.root() {
            let _ = self
                .doc
                .apply_from(origin, Mutation::Invoke { id: root, event });
        }
    }

    // -- windowed entry point -------------------------------------------------

    /// Run the interactive event loop on a real window (blocking). The window's
    /// initial logical size becomes the viewport. Press a window's close button
    /// to exit; the audit log is printed on exit.
    pub fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        let event_loop =
            winit::event_loop::EventLoop::<accesskit_winit::Event>::with_user_event().build()?;
        event_loop.set_control_flow(ControlFlow::Wait);
        let proxy = event_loop.create_proxy();
        let mut app = WindowedApp {
            rt: self,
            a11y: None,
            proxy,
            last_frame: None,
        };
        event_loop.run_app(&mut app)?;
        Ok(())
    }

    /// Print the audit log to stderr, one line per invoke/edit, making the
    /// Human-vs-AI accountability trail visible.
    pub fn print_audit_log(&self) {
        eprintln!("--- audit log ({} edits) ---", self.doc.audit_log().len());
        for (i, edit) in self.doc.audit_log().iter().enumerate() {
            eprintln!("  [{i:>3}] {:?}  {:?}", edit.origin, edit.mutation);
        }
        let (human, ai) = self.invoke_counts();
        eprintln!("--- invokes: {human} Human, {ai} Ai (same audited fire path) ---");
    }

    /// Count `Invoke` records in the audit log by origin: `(human, ai)`.
    pub fn invoke_counts(&self) -> (usize, usize) {
        let mut human = 0;
        let mut ai = 0;
        for edit in self.doc.audit_log() {
            if matches!(edit.mutation, uni_ir::Mutation::Invoke { .. }) {
                match edit.origin {
                    Origin::Human => human += 1,
                    Origin::Ai => ai += 1,
                    Origin::System => {}
                }
            }
        }
        (human, ai)
    }

    /// Return the currently focused node id, if any.
    pub fn focused(&self) -> Option<NodeId> {
        self.focused
    }

    /// Move focus to the next/previous focusable node in tree order.
    ///
    /// Focusable nodes are those with a `"click"` callback. Returns `true` if
    /// there is at least one focusable node (focus moved); `false` otherwise.
    pub fn move_focus(&mut self, forward: bool) -> bool {
        let focusable = self.focusable_nodes();
        if focusable.is_empty() {
            return false;
        }
        let next = match self.focused {
            None => {
                if forward {
                    0
                } else {
                    focusable.len() - 1
                }
            }
            Some(cur) => {
                let pos = focusable.iter().position(|&id| id == cur).unwrap_or(0);
                if forward {
                    (pos + 1) % focusable.len()
                } else {
                    (pos + focusable.len() - 1) % focusable.len()
                }
            }
        };
        self.focused = Some(focusable[next]);
        true
    }

    /// All nodes that have a `"click"` callback, in layout order.
    fn focusable_nodes(&self) -> Vec<NodeId> {
        self.layout
            .order()
            .iter()
            .copied()
            .filter(|&id| {
                self.doc
                    .get(id)
                    .map(|n| n.callbacks.contains_key("click"))
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Activate (fire `"click"`) on the currently focused node.
    ///
    /// Returns `true` if a handler ran (same semantics as `dispatch`).
    pub fn activate_focused(&mut self) -> bool {
        match self.focused {
            Some(id) => self.dispatch(id, "click", Origin::Human),
            None => false,
        }
    }

    /// Get the current a11y tree update reflecting the current focus state.
    pub fn a11y_update(&self) -> uni_a11y::TreeUpdate {
        uni_a11y::build_tree(&self.doc, &self.layout, self.focused)
    }

    // -- S5: SwiftUI-style gesture recognizers --------------------------------

    /// **Attach a gesture recognizer to a node** (SwiftUI's `.gesture(...)` /
    /// `.onTapGesture(...)`). `action` is the base event name the recognizer
    /// fires through the audited dispatch path: the bare name for a discrete
    /// recognition (tap, long-press), `"<action>_changed"`/`"<action>_ended"` for
    /// a continuous one (drag/magnify/rotation). Register a [`Handler`] under the
    /// same name(s) with [`Runtime::register`] to give it behavior.
    ///
    /// Returns the index of the recognizer in the runtime's gesture set, so a
    /// caller can read its live state ([`Runtime::gesture`]) — e.g. a drag's
    /// translation or a magnify's scale.
    pub fn add_gesture(
        &mut self,
        node: NodeId,
        action: impl Into<String>,
        kind: GestureKind,
    ) -> usize {
        self.gestures.push(Recognizer::new(node, action, kind));
        self.gestures.len() - 1
    }

    /// Immutable access to a registered recognizer by index (its live
    /// translation/scale/angle + phase). See [`Runtime::add_gesture`].
    pub fn gesture(&self, idx: usize) -> Option<&Recognizer> {
        self.gestures.get(idx)
    }

    /// Number of registered gesture recognizers.
    pub fn gesture_count(&self) -> usize {
        self.gestures.len()
    }

    /// **Feed one [`InputEvent`] to the gesture recognizers**, firing every
    /// recognized gesture through the *same* audited `dispatch` path a
    /// click takes — tagged with `origin` (a human's real input is
    /// [`Origin::Human`]; an AI feeding synthetic input passes [`Origin::Ai`]).
    /// Returns `true` if any gesture fired a handler.
    ///
    /// Each pointer event is hit-tested once (bubbling to the recognizer's node),
    /// so a press only arms recognizers whose node it actually lands on. Pinch /
    /// rotate events are not positional — they drive every magnify/rotation
    /// recognizer (the caller targets by registering only the intended ones).
    ///
    /// **Combined-gesture precedence.** After feeding, if a drag on a node has
    /// become *active*, any pending tap/long-press recognizer on that **same
    /// node** is cancelled — a drag past its threshold wins over a not-yet-fired
    /// tap/long-press, mirroring SwiftUI's `exclusively(before:)`/simultaneous
    /// resolution for the common case.
    pub fn feed_gesture(&mut self, input: &InputEvent, origin: Origin) -> bool {
        // Which node (if any) this pointer event bubbles to, by recognizer node.
        // We resolve hits up-front so the borrow of `self` for hit-testing ends
        // before we mutate the recognizers.
        let point = match input {
            InputEvent::PointerDown { x, y, .. }
            | InputEvent::PointerUp { x, y, .. }
            | InputEvent::PointerMoved { x, y } => Some((*x, *y)),
            _ => None,
        };
        // For a press, a recognizer's node is "hit" if the pointer hit-tests into
        // it or a descendant (bubbling). Movement/release reuse whichever
        // recognizers are already tracking, so we mark them hit too.
        let mut hits: Vec<bool> = Vec::with_capacity(self.gestures.len());
        for rec in &self.gestures {
            let hit = match point {
                Some(p) => self.point_hits_node(p, rec.node) || rec.is_tracking(),
                None => false,
            };
            hits.push(hit);
        }

        // Drive each recognizer, collecting (idx, node, action, event) to fire.
        let mut fired: Vec<(usize, NodeId, String, GestureEvent)> = Vec::new();
        for (i, rec) in self.gestures.iter_mut().enumerate() {
            for ev in rec.feed(input, hits[i]) {
                fired.push((i, rec.node, rec.action.clone(), ev));
            }
        }

        // Combined precedence: an active drag cancels pending tap/long-press on
        // the same node.
        self.apply_drag_precedence();

        self.fire_gesture_events(fired, origin)
    }

    /// **Advance gesture time by `dt` seconds**, firing any long-press that has
    /// now been held past its `minimumDuration` through the audited dispatch path
    /// (tagged `origin`). Returns `true` if a long-press fired a handler. Call
    /// this from the frame loop alongside [`Runtime::tick`].
    pub fn tick_gestures(&mut self, dt: f32, origin: Origin) -> bool {
        let mut fired: Vec<(usize, NodeId, String, GestureEvent)> = Vec::new();
        for (i, rec) in self.gestures.iter_mut().enumerate() {
            for ev in rec.tick(dt) {
                fired.push((i, rec.node, rec.action.clone(), ev));
            }
        }
        // A long-press that fires should also cancel a pending tap on the same
        // node, so a release after it does not double-register.
        let firing_nodes: Vec<NodeId> = fired.iter().map(|(_, n, _, _)| *n).collect();
        for rec in &mut self.gestures {
            if matches!(rec.kind, GestureKind::Tap { .. }) && firing_nodes.contains(&rec.node) {
                rec.cancel();
            }
        }
        self.fire_gesture_events(fired, origin)
    }

    /// **Feed a pinch (magnify) delta programmatically** to every
    /// [`GestureKind::Magnify`] recognizer (SwiftUI's `MagnifyGesture`). The
    /// headless/desktop input path has no multitouch, so magnification is driven
    /// by feeding deltas here (a trackpad backend, a test, or an AI). Each step
    /// fires `"<action>_changed"` through the audited path; the live factor is on
    /// [`Recognizer::scale`]. Returns `true` if a handler fired.
    pub fn pinch(&mut self, delta: f32, origin: Origin) -> bool {
        self.feed_gesture(&InputEvent::Pinch { delta }, origin)
    }

    /// **Feed a rotation delta programmatically** to every
    /// [`GestureKind::Rotation`] recognizer (SwiftUI's `RotationGesture`); see
    /// [`Runtime::pinch`]. Each step fires `"<action>_changed"`; the live angle
    /// (radians) is on [`Recognizer::rotation`]. Returns `true` if a handler
    /// fired.
    pub fn rotate(&mut self, delta: f32, origin: Origin) -> bool {
        self.feed_gesture(&InputEvent::Rotate { delta }, origin)
    }

    /// **Conclude every active continuous (magnify/rotation) gesture**, firing
    /// their `"<action>_ended"` event through the audited path. Drags end on a
    /// `PointerUp`; pinch/rotate have no natural "up" in the headless vocabulary,
    /// so this is how a caller signals the gesture is over. Returns `true` if a
    /// handler fired.
    pub fn end_continuous_gestures(&mut self, origin: Origin) -> bool {
        let mut fired: Vec<(usize, NodeId, String, GestureEvent)> = Vec::new();
        for (i, rec) in self.gestures.iter_mut().enumerate() {
            for ev in rec.end_continuous() {
                fired.push((i, rec.node, rec.action.clone(), ev));
            }
        }
        self.fire_gesture_events(fired, origin)
    }

    /// Combined precedence: for any drag recognizer that is now active, cancel
    /// every *pending* tap/long-press recognizer on the **same node** so a drag
    /// past its threshold wins over a not-yet-fired tap/long-press.
    fn apply_drag_precedence(&mut self) {
        let dragging: Vec<NodeId> = self
            .gestures
            .iter()
            .filter(|r| matches!(r.kind, GestureKind::Drag { .. }) && r.is_active())
            .map(|r| r.node)
            .collect();
        if dragging.is_empty() {
            return;
        }
        for rec in &mut self.gestures {
            let cancelable = matches!(
                rec.kind,
                GestureKind::Tap { .. } | GestureKind::LongPress { .. }
            );
            if cancelable && rec.is_tracking() && dragging.contains(&rec.node) {
                rec.cancel();
            }
        }
    }

    /// Fire a batch of recognized gesture events through the audited dispatch
    /// path, composing each concrete event name. Returns `true` if any handler
    /// ran. (`idx` is kept in the tuple for symmetry with the recognizer set; the
    /// node + action are what dispatch needs.)
    fn fire_gesture_events(
        &mut self,
        fired: Vec<(usize, NodeId, String, GestureEvent)>,
        origin: Origin,
    ) -> bool {
        let mut any = false;
        for (_idx, node, action, ev) in fired {
            let event = gesture::event_name(&action, &ev);
            if self.dispatch(node, &event, origin) {
                any = true;
            }
        }
        any
    }

    /// Whether `point` (logical px) hit-tests into `node` or one of its
    /// descendants — i.e. a pointer there would bubble to `node`. Used to decide
    /// which recognizers a press arms.
    fn point_hits_node(&self, point: (f32, f32), node: NodeId) -> bool {
        let Some(mut current) = hit_test(&self.layout, point) else {
            return false;
        };
        loop {
            if current == node {
                return true;
            }
            match self.doc.get(current).and_then(|n| n.parent) {
                Some(parent) => current = parent,
                None => return false,
            }
        }
    }
}

/// The winit `ApplicationHandler` wrapper that drives a [`Runtime`] against a
/// live window + GPU renderer.
struct WindowedApp {
    rt: Runtime,
    a11y: Option<a11y::A11yAdapter>,
    proxy: winit::event_loop::EventLoopProxy<accesskit_winit::Event>,
    last_frame: Option<std::time::Instant>,
}

impl winit::application::ApplicationHandler<accesskit_winit::Event> for WindowedApp {
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: accesskit_winit::Event) {
        match event.window_event {
            accesskit_winit::WindowEvent::InitialTreeRequested => {
                // The a11y platform needs the initial tree now. Push the full tree.
                if let Some(a11y) = &mut self.a11y {
                    a11y.commit(&self.rt.doc, &self.rt.layout, self.rt.focused);
                }
            }
            accesskit_winit::WindowEvent::ActionRequested(req) => {
                use accesskit::Action;
                if req.action == Action::Click {
                    let ir_id = NodeId(req.target_node.0.wrapping_sub(1));
                    if self.rt.dispatch(ir_id, "click", Origin::Human) {
                        if let Some(w) = self.rt.window.clone() {
                            w.request_redraw();
                        }
                    }
                }
            }
            accesskit_winit::WindowEvent::AccessibilityDeactivated => {}
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.rt.window.is_some() {
            return;
        }
        let (vw, vh) = self.rt.viewport;
        let attrs = Window::default_attributes()
            .with_title("Uni-UI — interactive runtime")
            .with_inner_size(winit::dpi::LogicalSize::new(vw as f64, vh as f64))
            .with_visible(false); // Must be invisible before creating a11y adapter
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("window creation failed: {e}");
                event_loop.exit();
                return;
            }
        };
        match WgpuRenderer::new(window.clone()) {
            Ok(r) => {
                self.rt.renderer = Some(r);
            }
            Err(e) => {
                eprintln!("renderer init failed: {e}");
                event_loop.exit();
                return;
            }
        }
        // Create accessibility adapter BEFORE making the window visible.
        let adapter = a11y::A11yAdapter::new(event_loop, window.as_ref(), self.proxy.clone());
        self.a11y = Some(adapter);
        // Now show the window.
        window.set_visible(true);
        self.rt.window = Some(window);
        self.last_frame = Some(std::time::Instant::now());
        // Push the initial a11y tree now that activation will have been triggered.
        if let Some(a11y) = &mut self.a11y {
            a11y.commit(&self.rt.doc, &self.rt.layout, self.rt.focused);
        }
        eprintln!(
            "uni-runtime: click the button (Human fire) or press 'A' (AI fire). \
             Close the window to print the audit log."
        );
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.rt.window.clone() else {
            return;
        };

        if let Some(a11y) = &mut self.a11y {
            a11y.process_event(window.as_ref(), &event);
        }

        if let Some(input) =
            translate_window_event(&event, window.scale_factor(), &mut self.rt.cursor)
        {
            match &input {
                InputEvent::KeyDown { key } if key.eq_ignore_ascii_case("a") => {
                    if let Some(target) = self.rt.ai_click_target() {
                        if self.rt.ai_fire(target, "click") {
                            if let Some(a11y) = &mut self.a11y {
                                a11y.commit(&self.rt.doc, &self.rt.layout, self.rt.focused);
                            }
                            window.request_redraw();
                        }
                    }
                }
                _ => {
                    if self.rt.on_input(&input) {
                        if let Some(a11y) = &mut self.a11y {
                            a11y.commit(&self.rt.doc, &self.rt.layout, self.rt.focused);
                        }
                        window.request_redraw();
                    }
                }
            }
        }

        match event {
            WindowEvent::CloseRequested => {
                self.rt.print_audit_log();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = self.rt.renderer.as_mut() {
                    r.resize(size.width, size.height, window.scale_factor());
                }
                let scale = window.scale_factor() as f32;
                self.rt
                    .set_viewport((size.width as f32 / scale, size.height as f32 / scale));
                if let Some(a11y) = &mut self.a11y {
                    a11y.commit(&self.rt.doc, &self.rt.layout, self.rt.focused);
                }
                window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let size = window.inner_size();
                if let Some(r) = self.rt.renderer.as_mut() {
                    r.resize(size.width, size.height, scale_factor);
                }
            }
            WindowEvent::RedrawRequested => {
                let dt = self
                    .last_frame
                    .map(|t| {
                        let now = std::time::Instant::now();
                        now.duration_since(t).as_secs_f32()
                    })
                    .unwrap_or(1.0 / 60.0);
                self.last_frame = Some(std::time::Instant::now());

                let animating = self.rt.tick_animations(dt);
                if animating {
                    event_loop.set_control_flow(ControlFlow::Poll);
                    window.request_redraw();
                } else {
                    event_loop.set_control_flow(ControlFlow::Wait);
                }

                let scene = self.rt.scene();
                if let Some(r) = self.rt.renderer.as_mut() {
                    match r.render(&scene) {
                        Ok(()) => {}
                        Err(RenderError::SurfaceLost) => {
                            let s = window.inner_size();
                            r.resize(s.width, s.height, window.scale_factor());
                        }
                        Err(e) => eprintln!("render error: {e}"),
                    }
                }
            }
            _ => {}
        }
    }
}

impl Runtime {
    /// Pick a target for an AI-driven click: the node under the current cursor
    /// that handles `"click"` (bubbling), or — if the cursor isn't over one —
    /// any node in the tree that has a `"click"` callback (so pressing 'A'
    /// always demonstrates the cowork path even without aiming the mouse).
    fn ai_click_target(&self) -> Option<NodeId> {
        if let Some(t) = self.bubble_to_handler(self.cursor, "click") {
            return Some(t);
        }
        self.layout.order().iter().copied().find(|&id| {
            self.doc
                .get(id)
                .map(|n| n.callbacks.contains_key("click"))
                .unwrap_or(false)
        })
    }
}

// ============================================================================
// Web / wasm canvas-host leaf (THE FLOW: a swappable Platform+Renderer leaf)
// ============================================================================

/// The browser/wasm canvas host.
///
/// On the web there is no winit window or GPU surface to own up-front, so this
/// leaf pairs a [`Runtime`] (the audited fire→handler→state→sync→spring core,
/// unchanged) with a software [`uni_render::CanvasRenderer`] that paints into an
/// in-memory RGBA buffer. A browser copies that buffer into a `<canvas>` (or it
/// is asserted in a headless test). The same `Document`, `Store`, `Origin`
/// audit path, springs, and adaptive `Env` drive it — only the platform seam
/// (windowing + rasterizer) is swapped, exactly as THE FLOW intends.
///
/// The wasm-bindgen JS entry points are additive and target-gated
/// (`#[cfg(target_arch = "wasm32")]`); this core is pure and native-testable.
#[cfg(feature = "web")]
pub mod web {
    use super::Runtime;
    use uni_ir::{Document, NodeId};
    use uni_render::{CanvasRenderer, InputEvent, PointerButton, Renderer};

    /// A canvas-backed UI host for the wasm/WebGPU target.
    pub struct CanvasHost {
        rt: Runtime,
        renderer: CanvasRenderer,
        width: u32,
        height: u32,
    }

    impl CanvasHost {
        /// Build a host around an existing [`Document`] at the given pixel size.
        pub fn new(doc: Document, width: u32, height: u32) -> Self {
            let rt = Runtime::new(doc, (width as f32, height as f32));
            let renderer = CanvasRenderer::new(width, height);
            let mut host = Self {
                rt,
                renderer,
                width,
                height,
            };
            host.render();
            host
        }

        /// Parse `.uni` source and build a host at the given size.
        pub fn from_uni(src: &str, width: u32, height: u32) -> Result<Self, uni_dsl::ParseError> {
            Ok(Self::new(uni_dsl::parse(src)?, width, height))
        }

        /// Borrow the inner runtime (audit log, store, doc).
        pub fn runtime(&self) -> &Runtime {
            &self.rt
        }

        /// Mutable runtime access (register handlers, seed state, `ai_fire`).
        pub fn runtime_mut(&mut self) -> &mut Runtime {
            &mut self.rt
        }

        /// Advance spring animations by `dt` seconds and repaint. Returns `true`
        /// while animations are still running (the browser should keep its
        /// `requestAnimationFrame` loop alive).
        pub fn tick(&mut self, dt: f32) -> bool {
            let animating = self.rt.tick_animations(dt);
            self.render();
            animating
        }

        /// Feed a left pointer-down at `(x, y)` logical px (the audited Human
        /// path). Repaints and returns `true` if a click was handled.
        pub fn pointer_down(&mut self, x: f32, y: f32) -> bool {
            let handled = self.rt.on_input(&InputEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
            });
            if handled {
                self.render();
            }
            handled
        }

        /// Fire `event` on `target` as the AI — the same audited surface, proving
        /// cowork on the web target too.
        pub fn ai_fire(&mut self, target: NodeId, event: &str) -> bool {
            let fired = self.rt.ai_fire(target, event);
            if fired {
                self.render();
            }
            fired
        }

        /// Resize the canvas + viewport and repaint.
        pub fn resize(&mut self, width: u32, height: u32) {
            self.width = width;
            self.height = height;
            self.renderer.resize(width, height, 1.0);
            self.rt.set_viewport((width as f32, height as f32));
            self.render();
        }

        /// The current RGBA pixel buffer (row-major, top-down) for blitting into
        /// a browser `<canvas>` via `putImageData`.
        pub fn pixels(&self) -> &[u8] {
            &self.renderer.pixels
        }

        /// Paint the current scene into the canvas buffer.
        pub fn render(&mut self) {
            let scene = self.rt.scene();
            let _ = self.renderer.render(&scene);
        }
    }
}

// ============================================================================
// F1 — the accesskit_winit adapter seam
// ============================================================================

/// **The screen-reader bridge for a real window.**
///
/// `uni-a11y` builds the platform-agnostic [`uni_a11y::TreeUpdate`] (a pure
/// function of [`Document`] + [`Layout`] + focus). This module owns the *other*
/// half: pushing that tree into the live OS accessibility platform through
/// [`accesskit_winit`], on **every commit** — every frame the document, layout,
/// or focus may have changed.
///
/// The `WindowedApp` event loop already wires a raw `accesskit_winit::Adapter`
/// inline; `A11yAdapter` factors that out into one named seam with a single
/// `commit` call, so the "push the current tree" gesture
/// lives in one place and reads the same way everywhere. The constructor is
/// native (it needs a real winit event loop + window to register with the OS);
/// the build-tree side is fully headless-tested in `uni-a11y` and via
/// [`Runtime::a11y_update`].
///
/// **Screen-reader verification is manual.** A unit test can prove the tree is
/// built and the adapter constructs/compiles, but confirming an actual screen
/// reader (Orca / NVDA / VoiceOver) speaks the controls requires a human with
/// assistive tech running against a real window — see the runnable example
/// `uni-runtime` ships and the `run` entry point.
pub mod a11y {
    use accesskit_winit::Adapter;
    use uni_core::Layout;
    use uni_ir::{Document, NodeId};
    use winit::event::WindowEvent;
    use winit::event_loop::ActiveEventLoop;
    use winit::window::Window;

    /// Owns the platform accessibility adapter and pushes the current tree on
    /// each commit.
    pub struct A11yAdapter {
        adapter: Adapter,
    }

    impl A11yAdapter {
        /// **Native constructor.** Register an accessibility adapter for `window`
        /// against the running `event_loop`, routing platform a11y events back
        /// through `proxy`. Call this *before* the window is made visible, exactly
        /// as the platform expects.
        pub fn new(
            event_loop: &ActiveEventLoop,
            window: &Window,
            proxy: winit::event_loop::EventLoopProxy<accesskit_winit::Event>,
        ) -> Self {
            let adapter = Adapter::with_event_loop_proxy(event_loop, window, proxy);
            A11yAdapter { adapter }
        }

        /// Forward a raw winit window event to the platform adapter (so it can
        /// track activation/focus on its side). Call from `window_event`.
        pub fn process_event(&mut self, window: &Window, event: &WindowEvent) {
            self.adapter.process_event(window, event);
        }

        /// **Push the current tree — call on every commit.**
        ///
        /// Builds `uni_a11y::build_tree(doc, layout, focused)` and hands it to the
        /// platform (only when the a11y platform is active, so it's cheap when no
        /// screen reader is listening). This is the single line a frame runs to
        /// keep assistive tech in lock-step with the live UI.
        pub fn commit(&mut self, doc: &Document, layout: &Layout, focused: Option<NodeId>) {
            self.adapter
                .update_if_active(|| uni_a11y::build_tree(doc, layout, focused));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use uni_ir::{Binding, Mutation, NodeId, Value};

    /// The counter UI, with the label's `content` **bound** to the `"label"`
    /// state key (rung 4). The literal `content` is the initial frame; once the
    /// store has a `"label"` value, `sync_bindings` overrides it.
    const COUNTER_UNI: &str = r#"
        Stack { padding: 24px; gap: 16px; background: #0a0a0a;
          Text { content: $label; size: 32px; color: #ffffff; }
          Button { width: 200px; height: 64px; color: #7d39eb; corner_radius: 16px;
                   on click: increment();
                   Text { content: "Click me"; size: 20px; color: #ffffff; } }
        }
    "#;

    /// Locate the label Text (the first child of the root Stack) so a test can
    /// inspect its `content`.
    fn label_id(doc: &Document) -> NodeId {
        let root = doc.root().unwrap();
        doc.get(root).unwrap().children[0]
    }

    /// Locate the Button node (the second child of the root Stack).
    fn button_id(doc: &Document) -> NodeId {
        let root = doc.root().unwrap();
        doc.get(root).unwrap().children[1]
    }

    /// Build a runtime from the counter `.uni`, register an `"increment"`
    /// handler that bumps a counter held *in the store* and writes the rendered
    /// label into the bound `"label"` key. The handler mutates **state only**;
    /// the bound Text updates via `sync_bindings`, not a direct prop write.
    fn counter_runtime() -> Runtime {
        let mut rt = Runtime::from_uni(COUNTER_UNI, (800.0, 600.0)).expect("counter .uni parses");
        rt.register(
            "increment",
            Box::new(move |store: &mut Store, _origin: Origin| {
                let n = match store.get("count") {
                    Some(Value::Int(n)) => n + 1,
                    _ => 1,
                };
                store.set("count", Value::Int(n));
                store.set("label", Value::Text(format!("Clicks: {n}")));
            }),
        );
        rt
    }

    #[test]
    fn counter_uni_parses_to_expected_tree() {
        let rt = Runtime::from_uni(COUNTER_UNI, (800.0, 600.0)).unwrap();
        let doc = rt.doc();
        let root = doc.root().unwrap();
        assert_eq!(doc.get(root).unwrap().kind, "Stack");
        let label = label_id(doc);
        let button = button_id(doc);
        assert_eq!(doc.get(label).unwrap().kind, "Text");
        assert_eq!(doc.get(button).unwrap().kind, "Button");
        // The button carries the parsed click callback → increment.
        assert_eq!(
            doc.get(button)
                .unwrap()
                .callbacks
                .get("click")
                .unwrap()
                .name,
            "increment"
        );
        // The label carries a binding for `content` → the `label` state key.
        assert_eq!(
            doc.get(label).unwrap().bindings.get("content"),
            Some(&Binding {
                expr: "label".into()
            })
        );
    }

    /// `sync_bindings` pushes a store value into the bound node's literal prop,
    /// **on the same node id** (no structural change).
    #[test]
    fn sync_bindings_pushes_store_value_into_bound_prop() {
        let mut rt = Runtime::from_uni(COUNTER_UNI, (800.0, 600.0)).unwrap();
        let label = label_id(rt.doc());

        // Set state for the bound key, then sync.
        rt.store_mut()
            .set("label", Value::Text("from state".into()));
        rt.sync_bindings();

        // The SAME node's `content` prop now holds the store value.
        assert_eq!(
            rt.doc().get(label).unwrap().props.get("content"),
            Some(&Value::Text("from state".into()))
        );
        // Id-stable: the label is still the first child of the root.
        assert_eq!(label_id(rt.doc()), label);
        // The push is attributed to System (the runtime applying state).
        let last_setprop = rt
            .doc()
            .audit_log()
            .iter()
            .rev()
            .find(|e| matches!(e.mutation, Mutation::SetProp { .. }))
            .unwrap();
        assert_eq!(last_setprop.origin, Origin::System);
    }

    /// The fire→handler→state→sync cycle, headless: a human fire bumps the
    /// counter (in the store) and the bound label updates *via state*. The
    /// audit log records a Human Invoke.
    #[test]
    fn human_fire_runs_handler_and_updates_bound_label_via_state() {
        let mut rt = counter_runtime();
        let button = button_id(rt.doc());

        let handled = rt.dispatch(button, "click", Origin::Human);
        assert!(handled);
        assert_eq!(rt.store().get("count"), Some(Value::Int(1)));

        let label = label_id(rt.doc());
        // The bound Text reflects the counter — pushed from the store.
        assert_eq!(
            rt.doc().get(label).unwrap().props.get("content"),
            Some(&Value::Text("Clicks: 1".into()))
        );

        let (human, ai) = rt.invoke_counts();
        assert_eq!((human, ai), (1, 0));
    }

    /// `ai_fire` travels the SAME path as a human fire: same handler, same
    /// state mutation, just `Origin::Ai`. After a human click then an AI fire
    /// the counter is 2 and the log has one Human + one Ai Invoke — and the
    /// bound label reflects the counter on both paths.
    #[test]
    fn ai_fire_takes_the_same_audited_path_as_human() {
        let mut rt = counter_runtime();
        let button = button_id(rt.doc());

        // Human clicks once.
        assert!(rt.dispatch(button, "click", Origin::Human));
        let label = label_id(rt.doc());
        assert_eq!(
            rt.doc().get(label).unwrap().props.get("content"),
            Some(&Value::Text("Clicks: 1".into())),
            "bound label reflects state after the Human path"
        );

        // The AI fires the very same callback on the very same node.
        assert!(rt.ai_fire(button, "click"));

        assert_eq!(rt.store().get("count"), Some(Value::Int(2)));
        assert_eq!(
            rt.doc().get(label).unwrap().props.get("content"),
            Some(&Value::Text("Clicks: 2".into())),
            "bound label reflects state after the Ai path"
        );

        // Accountability circle visible: one Human Invoke, one Ai Invoke.
        let (human, ai) = rt.invoke_counts();
        assert_eq!((human, ai), (1, 1));
    }

    /// **B2 — the cowork index + symmetric invoke.** [`Runtime::actions`]
    /// enumerates the button's `click → increment` callback, and the AI can
    /// [`invoke`](Runtime::invoke) that exact `(node, event)` with `Origin::Ai`,
    /// reaching the *same* handler a human pointer event would — proving the AI
    /// has neither more nor less than the human surface.
    #[test]
    fn ai_can_invoke_the_same_action_a_human_can() {
        let mut rt = counter_runtime();
        let button = button_id(rt.doc());

        // The cowork index surfaces exactly the button's increment callback.
        let actions = rt.actions();
        assert_eq!(
            actions,
            vec![(button, "click".to_string(), "increment".to_string())],
            "actions() lists the (node, event, action) the UI exposes"
        );

        // The AI invokes that very triple as Origin::Ai — same handler path.
        let (node, event, _name) = &actions[0];
        let ran = rt.invoke(*node, event, Origin::Ai);
        assert!(ran, "the AI-invoked action ran its registered handler");
        assert_eq!(rt.store().get("count"), Some(Value::Int(1)));

        // A human can invoke the identical triple — same path, same effect.
        assert!(rt.invoke(*node, event, Origin::Human));
        assert_eq!(rt.store().get("count"), Some(Value::Int(2)));

        // The audit log proves both rode the one fire() surface: 1 Ai, 1 Human.
        let (human, ai) = rt.invoke_counts();
        assert_eq!((human, ai), (1, 1));
    }

    /// `on_input` with a left PointerDown over the button hit-tests, bubbles to
    /// the click handler, fires it as Human, mutates state and updates the bound
    /// label — proving the full input → hit-test → bubble → fire → handler →
    /// sync chain.
    #[test]
    fn pointer_down_over_button_bubbles_and_fires() {
        let mut rt = counter_runtime();
        let button = button_id(rt.doc());
        // Center of the button's computed rect.
        let r = rt.layout().rect(button).expect("button laid out");
        let point = (r.x + r.w / 2.0, r.y + r.h / 2.0);

        let handled = rt.on_input(&InputEvent::PointerDown {
            x: point.0,
            y: point.1,
            button: PointerButton::Left,
        });
        assert!(handled, "click over the button should be handled");
        assert_eq!(rt.store().get("count"), Some(Value::Int(1)));
        let (human, _ai) = rt.invoke_counts();
        assert_eq!(human, 1);
    }

    /// Bubbling: a click landing on a child node that has *no* click callback
    /// must bubble up to its parent and fire there. We build a small tree where
    /// a container (`Row`, laid out by uni-core) carries the click handler and a
    /// child `Rect` (laid out, no callback) receives the pointer.
    #[test]
    fn click_on_child_bubbles_to_parent_handler() {
        let mut doc = Document::new();
        let row = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id: row,
                kind: "Row".into(),
            },
        )
        .unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id: row })
            .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::SetCallback {
                id: row,
                event: "click".into(),
                action: uni_ir::Action {
                    name: "ping".into(),
                    args: vec![],
                },
            },
        )
        .unwrap();

        let child = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id: child,
                kind: "Rect".into(),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::SetProp {
                id: child,
                key: "width".into(),
                value: Value::Px(100.0),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::SetProp {
                id: child,
                key: "height".into(),
                value: Value::Px(100.0),
            },
        )
        .unwrap();
        doc.apply_from(Origin::System, Mutation::AppendChild { parent: row, child })
            .unwrap();

        let mut rt = Runtime::new(doc, (400.0, 400.0));
        let pinged = Rc::new(RefCell::new(0i64));
        let p = pinged.clone();
        rt.register(
            "ping",
            Box::new(move |_store: &mut Store, _origin: Origin| {
                *p.borrow_mut() += 1;
            }),
        );

        // The child Rect has no callback of its own.
        assert!(!rt.doc().get(child).unwrap().callbacks.contains_key("click"));

        let r = rt.layout().rect(child).expect("child laid out");
        let point = (r.x + r.w / 2.0, r.y + r.h / 2.0);
        // Topmost hit is the child Rect, not the Row.
        assert_eq!(hit_test(rt.layout(), point), Some(child));

        assert!(rt.on_input(&InputEvent::PointerDown {
            x: point.0,
            y: point.1,
            button: PointerButton::Left,
        }));
        assert_eq!(*pinged.borrow(), 1, "click bubbled up to the Row handler");
    }

    /// A click in empty space (no handler in the bubble chain) does nothing.
    #[test]
    fn click_on_empty_space_does_nothing() {
        let mut rt = counter_runtime();
        let handled = rt.on_input(&InputEvent::PointerDown {
            x: 799.0,
            y: 599.0,
            button: PointerButton::Left,
        });
        assert!(!handled);
        assert_eq!(rt.store().get("count"), None);
        let (human, ai) = rt.invoke_counts();
        assert_eq!((human, ai), (0, 0));
    }

    #[test]
    fn env_tracks_viewport_size_class() {
        use uni_env::WidthClass;
        // Compact (< 600)
        let rt = Runtime::new(Document::new(), (400.0, 800.0));
        assert_eq!(rt.env().width_class(), WidthClass::Compact);

        // Expanded (>= 840) after resize
        let mut rt2 = Runtime::new(Document::new(), (400.0, 800.0));
        rt2.set_viewport((1024.0, 768.0));
        assert_eq!(rt2.env().width_class(), WidthClass::Expanded);
    }

    #[test]
    fn animate_drives_prop_toward_target() {
        let mut doc = Document::new();
        let id = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id,
                kind: "Rect".into(),
            },
        )
        .unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id })
            .unwrap();

        let mut rt = Runtime::new(doc, (800.0, 600.0));
        rt.animate(id, "width", 0.0, 200.0, uni_spring::Spring::spatial());

        assert!(!rt.animations_settled(), "animation should be running");

        // Tick enough frames to settle (spatial spring, target=200).
        let mut settled = false;
        for _ in 0..10_000 {
            if !rt.tick_animations(1.0 / 60.0) {
                settled = true;
                break;
            }
        }
        assert!(settled, "spring should settle in finite steps");
        // The prop should now be close to 200.
        let val = rt.doc().get(id).unwrap().props.get("width").cloned();
        match val {
            Some(uni_ir::Value::Px(v)) => assert!((v - 200.0).abs() < 1.0, "width={v}"),
            other => panic!("expected Px, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Implicit, descriptor-driven animation (the `tick` surface)
    // -----------------------------------------------------------------------

    /// Build a single `Rect` root carrying an `animation` descriptor and an
    /// initial `width`. Returns the runtime + the node id.
    fn animated_rect(desc: &str, width: f32) -> (Runtime, NodeId) {
        let mut doc = Document::new();
        let id = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id,
                kind: "Rect".into(),
            },
        )
        .unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id })
            .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::SetProp {
                id,
                key: "animation".into(),
                value: Value::Text(desc.into()),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::SetProp {
                id,
                key: "width".into(),
                value: Value::Px(width),
            },
        )
        .unwrap();
        let rt = Runtime::new(doc, (800.0, 600.0));
        (rt, id)
    }

    fn width_of(rt: &Runtime, id: NodeId) -> f32 {
        match rt.doc().get(id).unwrap().props.get("width") {
            Some(Value::Px(v)) => *v,
            other => panic!("expected Px width, got {other:?}"),
        }
    }

    /// **The core requirement.** A watched prop change on an `animation`-bearing
    /// node interpolates across ticks (it is *not* applied instantly) and reaches
    /// the target; the animation then settles and is removed.
    #[test]
    fn implicit_prop_change_interpolates_across_ticks_then_settles() {
        let (mut rt, id) = animated_rect("300ms linear", 0.0);

        // A change to a watched prop. This sets the *target*; it must NOT take
        // geometric effect instantly.
        rt.doc
            .apply_from(
                Origin::System,
                Mutation::SetProp {
                    id,
                    key: "width".into(),
                    value: Value::Px(300.0),
                },
            )
            .unwrap();

        // First tick: detect the change, advance one small step. The displayed
        // width must be partway, not the full 300 (interpolation, not snap).
        let dt = 1.0 / 60.0;
        let running = rt.tick(dt);
        assert!(running, "an implicit animation should be in flight");
        assert!(!rt.implicit_settled());
        let after_one = width_of(&rt, id);
        assert!(
            after_one > 0.0 && after_one < 300.0,
            "width should be mid-flight after one tick, got {after_one}"
        );

        // Keep ticking until the curve completes.
        let mut prev = after_one;
        let mut steps = 1;
        while rt.tick(dt) {
            let now = width_of(&rt, id);
            assert!(
                now + 1e-3 >= prev,
                "linear interpolation should be monotonic up: {prev} -> {now}"
            );
            prev = now;
            steps += 1;
            assert!(steps < 1000, "should settle in finite ticks");
        }

        // Settled: removed from the queue and snapped exactly onto the target.
        assert!(rt.implicit_settled(), "animation removed when done");
        let final_w = width_of(&rt, id);
        assert!(
            (final_w - 300.0).abs() < 1e-3,
            "reaches the target exactly, got {final_w}"
        );
        // It genuinely took multiple frames (~0.3s / (1/60s) ~= 18 ticks).
        assert!(steps > 5, "should span many ticks, took {steps}");
    }

    /// A node *without* an `animation` descriptor applies prop changes instantly
    /// (no interpolation enqueued) — the opt-in is the descriptor.
    #[test]
    fn no_descriptor_means_no_implicit_animation() {
        let mut doc = Document::new();
        let id = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id,
                kind: "Rect".into(),
            },
        )
        .unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id })
            .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::SetProp {
                id,
                key: "width".into(),
                value: Value::Px(10.0),
            },
        )
        .unwrap();
        let mut rt = Runtime::new(doc, (800.0, 600.0));

        rt.doc
            .apply_from(
                Origin::System,
                Mutation::SetProp {
                    id,
                    key: "width".into(),
                    value: Value::Px(500.0),
                },
            )
            .unwrap();
        let running = rt.tick(1.0 / 60.0);
        assert!(!running, "no descriptor → nothing animates");
        assert!(rt.implicit_settled());
        // The prop is exactly the value that was set — unchanged by tick.
        assert_eq!(width_of(&rt, id), 500.0);
    }

    /// **Transitions.** A node that gains `presented` animates its `opacity`
    /// from 0 toward 1 across ticks (the insertion/presentation fade-in); losing
    /// `presented` fades it back toward 0.
    #[test]
    fn presented_gain_and_loss_animates_opacity() {
        let (mut rt, id) = animated_rect("200ms linear", 50.0);

        // Gain `presented`.
        rt.doc
            .apply_from(
                Origin::System,
                Mutation::SetProp {
                    id,
                    key: "presented".into(),
                    value: Value::Bool(true),
                },
            )
            .unwrap();
        let dt = 1.0 / 60.0;
        assert!(rt.tick(dt), "fade-in should be running");
        let op_in = match rt.doc().get(id).unwrap().props.get("opacity") {
            Some(Value::Px(v)) => *v,
            other => panic!("expected opacity Px, got {other:?}"),
        };
        assert!(
            op_in > 0.0 && op_in < 1.0,
            "opacity mid fade-in, got {op_in}"
        );
        // Run the fade-in to completion → opacity ~= 1.
        while rt.tick(dt) {}
        let op_done = match rt.doc().get(id).unwrap().props.get("opacity") {
            Some(Value::Px(v)) => *v,
            other => panic!("{other:?}"),
        };
        assert!((op_done - 1.0).abs() < 1e-3, "fades in to 1, got {op_done}");

        // Now lose `presented` → fade-out toward 0.
        rt.doc
            .apply_from(
                Origin::System,
                Mutation::SetProp {
                    id,
                    key: "presented".into(),
                    value: Value::Bool(false),
                },
            )
            .unwrap();
        assert!(rt.tick(dt), "fade-out should be running");
        while rt.tick(dt) {}
        let op_out = match rt.doc().get(id).unwrap().props.get("opacity") {
            Some(Value::Px(v)) => *v,
            other => panic!("{other:?}"),
        };
        assert!(op_out.abs() < 1e-3, "fades out to 0, got {op_out}");
    }

    /// The `Spring` curve is sourced from `uni-spring`: it reaches (and, being
    /// under-damped/spatial, may briefly pass) the target, and settles onto it.
    #[test]
    fn spring_curve_uses_uni_spring_and_reaches_target() {
        // A spatial (under-damped) spring curve, short duration.
        let anim = parse_animation("100ms spring");
        assert!(matches!(anim.curve, Curve::Spring { .. }));
        // At t=0 progress is 0. uni-spring's `sample` integrates the physics
        // toward 1 and normalizes the spring curve to land on 1.0 at the
        // descriptor's duration, so the prop reaches its target there.
        assert!(anim.sample(0.0).abs() < 1e-3);
        let at_end = anim.sample(anim.duration);
        assert!(
            (at_end - 1.0).abs() < 1e-3,
            "spring sample reaches 1 at duration, got {at_end}"
        );

        // Drive a real prop with it end-to-end.
        let (mut rt, id) = animated_rect("100ms spring", 0.0);
        rt.doc
            .apply_from(
                Origin::System,
                Mutation::SetProp {
                    id,
                    key: "width".into(),
                    value: Value::Px(120.0),
                },
            )
            .unwrap();
        let dt = 1.0 / 120.0;
        let mut ticks = 0;
        while rt.tick(dt) {
            ticks += 1;
            assert!(ticks < 1000);
        }
        assert!(ticks > 1, "spring animation spans multiple ticks");
        // Settles exactly on the target (we snap at the duration budget).
        assert!((width_of(&rt, id) - 120.0).abs() < 1e-3);
    }

    /// `Animation::sample` maps elapsed time onto the right progress fraction for
    /// each curve, clamped to the unit interval at the ends.
    #[test]
    fn animation_sample_curves_behave() {
        let lin = Animation::new(Curve::Linear, 1.0);
        assert!((lin.sample(0.0) - 0.0).abs() < 1e-6);
        assert!((lin.sample(0.5) - 0.5).abs() < 1e-6);
        assert!((lin.sample(1.0) - 1.0).abs() < 1e-6);
        assert!((lin.sample(2.0) - 1.0).abs() < 1e-6, "clamps past the end");

        let ease = Animation::new(Curve::EaseInOut, 1.0);
        assert!(ease.sample(0.0).abs() < 1e-6);
        assert!((ease.sample(0.5) - 0.5).abs() < 1e-6, "symmetric at the mid");
        assert!((ease.sample(1.0) - 1.0).abs() < 1e-6);
        // Eased mid-rise is slower at the start than linear.
        assert!(ease.sample(0.2) < lin.sample(0.2));
    }

    /// A change *mid-flight* redirects the animation from the currently-displayed
    /// value (not the stale original) and still reaches the new target.
    #[test]
    fn midflight_change_redirects_and_reaches_new_target() {
        let (mut rt, id) = animated_rect("300ms linear", 0.0);
        rt.doc
            .apply_from(
                Origin::System,
                Mutation::SetProp {
                    id,
                    key: "width".into(),
                    value: Value::Px(300.0),
                },
            )
            .unwrap();
        let dt = 1.0 / 60.0;
        // A few ticks in.
        for _ in 0..5 {
            rt.tick(dt);
        }
        let mid = width_of(&rt, id);
        assert!(mid > 0.0 && mid < 300.0);

        // Redirect to a new target.
        rt.doc
            .apply_from(
                Origin::System,
                Mutation::SetProp {
                    id,
                    key: "width".into(),
                    value: Value::Px(100.0),
                },
            )
            .unwrap();
        while rt.tick(dt) {}
        assert!(
            (width_of(&rt, id) - 100.0).abs() < 1e-3,
            "redirected animation reaches the new target"
        );
    }

    #[test]
    fn a11y_tree_built_headless() {
        let rt =
            Runtime::from_uni(r#"Stack { Text { content: "Hello"; } }"#, (800.0, 600.0)).unwrap();
        let update = uni_a11y::build_tree(rt.doc(), rt.layout(), None);
        // Should have at least 3 entries: window root + Stack + Text.
        assert!(
            update.nodes.len() >= 3,
            "expected window+Stack+Text in a11y tree"
        );
    }

    /// **F1 — the commit payload the adapter pushes is the right tree.** The
    /// native [`a11y::A11yAdapter`] needs a real winit window to register with
    /// the OS (so a full screen-reader assertion is *manual*), but the data it
    /// commits each frame is exactly `Runtime::a11y_update` —
    /// `build_tree(doc, layout, focused)`. We assert that payload tracks focus,
    /// which is what `A11yAdapter::commit` would hand the platform on each frame.
    #[test]
    fn a11y_commit_payload_tracks_focus() {
        let (doc, btns) = focusable_doc(2);
        let mut rt = Runtime::new(doc, (800.0, 600.0));

        // No focus → the payload focuses the synthetic window root, not a node.
        let before = rt.a11y_update();
        assert!(
            before.focus != uni_a11y::build_tree(rt.doc(), rt.layout(), Some(btns[0])).focus,
            "an unfocused tree differs from one focused on a real node"
        );

        // Move focus → the very payload A11yAdapter::commit would push now points
        // the platform at the focused button.
        rt.move_focus(true);
        assert_eq!(rt.focused(), Some(btns[0]));
        let after = rt.a11y_update();
        let want = uni_a11y::build_tree(rt.doc(), rt.layout(), Some(btns[0]));
        assert_eq!(
            after.focus, want.focus,
            "commit payload focuses the focused node"
        );
    }

    // -----------------------------------------------------------------------
    // D3 — dirty-node tracking (incremental-layout foundation)
    // -----------------------------------------------------------------------

    /// A firing handler that mutates state dirties the bound node(s): the dirty
    /// set is populated from the applied mutations, then drained by relayout.
    #[test]
    fn dispatch_populates_then_drains_dirty_set() {
        let mut rt = counter_runtime();
        let button = button_id(rt.doc());
        let label = label_id(rt.doc());

        // After construction the set is clean (the constructor's first layout
        // drained it).
        assert!(
            rt.dirty_nodes().is_empty(),
            "starts clean after initial layout"
        );

        // A click mutates state → sync_bindings writes the bound label prop →
        // that SetProp is logged → the label id lands in the dirty set, which is
        // then drained by the relayout inside dispatch.
        assert!(rt.dispatch(button, "click", Origin::Human));
        assert!(
            rt.dirty_nodes().is_empty(),
            "relayout drains the dirty set after recomputing"
        );

        // Prove the *foundation* records the touched node before relayout drains
        // it: apply a raw SetProp on the label and fold the log without a full
        // relayout.
        rt.doc
            .apply_from(
                Origin::System,
                Mutation::SetProp {
                    id: label,
                    key: "size".into(),
                    value: Value::Px(40.0),
                },
            )
            .unwrap();
        rt.mark_dirty_from_log();
        assert!(
            rt.dirty_nodes().contains(&label),
            "the touched label id is recorded in the dirty set"
        );
    }

    /// The clean-subtree short-circuit: a relayout with nothing dirty does not
    /// recompute (the existing layout is preserved untouched).
    #[test]
    fn clean_relayout_short_circuits() {
        let (doc, _btns) = focusable_doc(2);
        let mut rt = Runtime::new(doc, (800.0, 600.0));

        // Snapshot the layout order; a clean relayout must leave it identical.
        let order_before = rt.layout().order().to_vec();
        assert!(rt.dirty_nodes().is_empty());
        rt.relayout(); // nothing dirty → short-circuit
        assert_eq!(
            rt.layout().order(),
            order_before.as_slice(),
            "clean relayout preserves the existing layout"
        );
    }

    // -----------------------------------------------------------------------
    // Keyboard navigation tests (Task B / Task E)
    // -----------------------------------------------------------------------

    /// Helper: build a Document with a root Row containing `count` Button nodes,
    /// each carrying a "click" callback that fires the action "hit_{i}".
    fn focusable_doc(count: usize) -> (Document, Vec<NodeId>) {
        let mut doc = Document::new();
        let row = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id: row,
                kind: "Row".into(),
            },
        )
        .unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id: row })
            .unwrap();

        let mut ids = Vec::new();
        for i in 0..count {
            let btn = doc.fresh_id();
            doc.apply_from(
                Origin::System,
                Mutation::CreateNode {
                    id: btn,
                    kind: "Stack".into(),
                },
            )
            .unwrap();
            doc.apply_from(
                Origin::System,
                Mutation::SetProp {
                    id: btn,
                    key: "width".into(),
                    value: Value::Px(60.0),
                },
            )
            .unwrap();
            doc.apply_from(
                Origin::System,
                Mutation::SetProp {
                    id: btn,
                    key: "height".into(),
                    value: Value::Px(40.0),
                },
            )
            .unwrap();
            doc.apply_from(
                Origin::System,
                Mutation::SetCallback {
                    id: btn,
                    event: "click".into(),
                    action: uni_ir::Action {
                        name: format!("hit_{i}"),
                        args: vec![],
                    },
                },
            )
            .unwrap();
            doc.apply_from(
                Origin::System,
                Mutation::AppendChild {
                    parent: row,
                    child: btn,
                },
            )
            .unwrap();
            ids.push(btn);
        }
        (doc, ids)
    }

    #[test]
    fn tab_moves_focus_to_focusable_node() {
        let (doc, btns) = focusable_doc(3);
        let mut rt = Runtime::new(doc, (800.0, 600.0));

        // Initially no focus.
        assert_eq!(rt.focused(), None);

        // First Tab: should focus the first focusable node.
        let moved = rt.on_input(&InputEvent::KeyDown { key: "Tab".into() });
        assert!(moved, "Tab should return true when focusable nodes exist");
        assert_eq!(rt.focused(), Some(btns[0]), "first Tab focuses first node");

        // Second Tab: moves to the next.
        rt.on_input(&InputEvent::KeyDown { key: "Tab".into() });
        assert_eq!(
            rt.focused(),
            Some(btns[1]),
            "second Tab focuses second node"
        );

        // Third Tab: moves to the third.
        rt.on_input(&InputEvent::KeyDown { key: "Tab".into() });
        assert_eq!(rt.focused(), Some(btns[2]), "third Tab focuses third node");
    }

    #[test]
    fn enter_activates_focused_node() {
        let (doc, btns) = focusable_doc(2);
        let mut rt = Runtime::new(doc, (800.0, 600.0));

        // Register handlers for both buttons.
        let hit0 = Rc::new(RefCell::new(0i64));
        let hit1 = Rc::new(RefCell::new(0i64));
        {
            let h0 = hit0.clone();
            rt.register(
                "hit_0",
                Box::new(move |_s: &mut Store, _o: Origin| {
                    *h0.borrow_mut() += 1;
                }),
            );
            let h1 = hit1.clone();
            rt.register(
                "hit_1",
                Box::new(move |_s: &mut Store, _o: Origin| {
                    *h1.borrow_mut() += 1;
                }),
            );
        }

        // Tab to the first button, then activate with Enter.
        rt.on_input(&InputEvent::KeyDown { key: "Tab".into() });
        assert_eq!(rt.focused(), Some(btns[0]));

        let activated = rt.on_input(&InputEvent::KeyDown {
            key: "Enter".into(),
        });
        assert!(activated, "Enter on a focused node should return true");
        assert_eq!(*hit0.borrow(), 1, "hit_0 handler should have fired once");
        assert_eq!(*hit1.borrow(), 0, "hit_1 should not have fired");
    }

    // -----------------------------------------------------------------------
    // Navigation + presentation state (over Store + the audited action path)
    // -----------------------------------------------------------------------

    /// `navigate` pushes a route (observable via `route()`/`nav_stack()`) and
    /// `back` pops it — the route stack behaves like a navigation stack, and
    /// each edit lands an audited Invoke in the log.
    #[test]
    fn navigate_pushes_and_back_pops() {
        let mut rt = counter_runtime();

        // Empty to start.
        assert_eq!(rt.route(), None);
        assert!(rt.nav_stack().is_empty());

        // Push two routes (Human, then Ai — both ride the audited path).
        assert!(rt.navigate("home", Origin::Human));
        assert_eq!(rt.route(), Some("home"));
        assert!(rt.navigate("details", Origin::Ai));
        assert_eq!(rt.route(), Some("details"));
        assert_eq!(rt.nav_stack(), &["home".to_string(), "details".to_string()]);

        // The current route is mirrored into the store for $route bindings.
        assert_eq!(rt.store().get("route"), Some(Value::Text("details".into())));

        // back() pops the top → the previous route is current again.
        assert!(rt.back(Origin::Human));
        assert_eq!(rt.route(), Some("home"));
        assert_eq!(rt.store().get("route"), Some(Value::Text("home".into())));

        // Pop the last one → empty stack, back() now returns false.
        assert!(rt.back(Origin::Human));
        assert_eq!(rt.route(), None);
        assert!(!rt.back(Origin::Human), "nothing left to pop");

        // The navigation edits are attributable in the audit log: one Human +
        // one Ai navigate Invoke recorded on push (back records its own too).
        let (human, ai) = rt.invoke_counts();
        assert!(human >= 1 && ai >= 1, "both origins recorded: {human} H, {ai} A");
    }

    /// `present(key)` sets the bound bool true, and after `sync_bindings` a
    /// relayout reflects it on the bound node's prop; `dismiss(key)` clears it.
    #[test]
    fn present_sets_bound_key_and_dismiss_clears_it() {
        // A Stack whose `visible` prop is bound to the `"sheet"` state key — the
        // Sheet/Alert/Popover show-flag.
        const UI: &str = r#"
            Stack {
              Sheet { visible: $sheet; width: 100px; height: 100px; }
            }
        "#;
        let mut rt = Runtime::from_uni(UI, (400.0, 400.0)).expect("ui parses");
        let root = rt.doc().root().unwrap();
        let sheet = rt.doc().get(root).unwrap().children[0];

        // Not presented initially.
        assert!(!rt.is_presented("sheet"));

        // present() flips the store bool true, syncs it into the bound prop, and
        // relays out — the bound node's `visible` prop now reads Bool(true).
        assert!(rt.present("sheet", Origin::Human));
        assert!(rt.is_presented("sheet"));
        assert_eq!(
            rt.doc().get(sheet).unwrap().props.get("visible"),
            Some(&Value::Bool(true)),
            "the bound key drove the prop true after a relayout"
        );

        // dismiss() clears it back to false on the same bound node.
        assert!(rt.dismiss("sheet", Origin::Ai));
        assert!(!rt.is_presented("sheet"));
        assert_eq!(
            rt.doc().get(sheet).unwrap().props.get("visible"),
            Some(&Value::Bool(false)),
            "dismiss cleared the bound prop to false"
        );

        // Both present + dismiss are audited (one Human, one Ai Invoke at least).
        let (human, ai) = rt.invoke_counts();
        assert!(human >= 1 && ai >= 1, "present/dismiss audited: {human} H, {ai} A");
    }

    #[test]
    fn focus_wraps_around() {
        let (doc, btns) = focusable_doc(3);
        let mut rt = Runtime::new(doc, (800.0, 600.0));

        // Tab three times to reach the last button.
        rt.move_focus(true);
        rt.move_focus(true);
        rt.move_focus(true);
        assert_eq!(rt.focused(), Some(btns[2]), "should be at last node");

        // One more Tab should wrap to the first.
        rt.move_focus(true);
        assert_eq!(
            rt.focused(),
            Some(btns[0]),
            "Tab past last should wrap to first"
        );

        // Backward from first should wrap to last.
        rt.move_focus(false);
        assert_eq!(
            rt.focused(),
            Some(btns[2]),
            "backward from first should wrap to last"
        );
    }

    // -----------------------------------------------------------------------
    // S5 — SwiftUI-style gesture recognizers
    // -----------------------------------------------------------------------

    /// Build a runtime with a single big node (a `Stack` that fills the viewport)
    /// carrying a `click` callback so it's hit-testable, plus a shared counter
    /// every gesture handler bumps. Returns `(runtime, node, hits)`.
    fn gesture_runtime() -> (Runtime, NodeId, Rc<RefCell<Vec<String>>>) {
        let mut doc = Document::new();
        let node = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id: node,
                kind: "Stack".into(),
            },
        )
        .unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id: node })
            .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::SetProp {
                id: node,
                key: "width".into(),
                value: Value::Px(400.0),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::SetProp {
                id: node,
                key: "height".into(),
                value: Value::Px(400.0),
            },
        )
        .unwrap();
        let rt = Runtime::new(doc, (400.0, 400.0));
        let hits = Rc::new(RefCell::new(Vec::<String>::new()));
        (rt, node, hits)
    }

    /// Register a gesture callback on `node` for `event`, whose handler records
    /// `event` in the shared log. Returns nothing; the action name == event.
    fn bind_gesture(
        rt: &mut Runtime,
        node: NodeId,
        event: &str,
        log: Rc<RefCell<Vec<String>>>,
    ) {
        rt.doc
            .apply_from(
                Origin::System,
                Mutation::SetCallback {
                    id: node,
                    event: event.into(),
                    action: uni_ir::Action {
                        name: event.into(),
                        args: vec![],
                    },
                },
            )
            .unwrap();
        let ev = event.to_string();
        rt.register(
            event,
            Box::new(move |_store: &mut Store, _origin: Origin| {
                log.borrow_mut().push(ev.clone());
            }),
        );
    }

    fn down(x: f32, y: f32) -> InputEvent {
        InputEvent::PointerDown {
            x,
            y,
            button: PointerButton::Left,
        }
    }
    fn up(x: f32, y: f32) -> InputEvent {
        InputEvent::PointerUp {
            x,
            y,
            button: PointerButton::Left,
        }
    }
    fn moved(x: f32, y: f32) -> InputEvent {
        InputEvent::PointerMoved { x, y }
    }

    /// `TapGesture(count: 1)` — a press→release within slop fires the bare action
    /// once, through the audited path as `Origin::Human`.
    #[test]
    fn tap_gesture_single_fires_on_press_release() {
        let (mut rt, node, log) = gesture_runtime();
        bind_gesture(&mut rt, node, "tap", log.clone());
        rt.add_gesture(node, "tap", GestureKind::Tap { count: 1 });

        assert!(!rt.feed_gesture(&down(100.0, 100.0), Origin::Human));
        let fired = rt.feed_gesture(&up(102.0, 101.0), Origin::Human);
        assert!(fired, "release within slop recognizes the tap");
        assert_eq!(*log.borrow(), vec!["tap".to_string()]);
        // Audited as a Human invoke on the gesture's node.
        let (human, ai) = rt.invoke_counts();
        assert_eq!((human, ai), (1, 0));
    }

    /// `onTapGesture(count: 2)` — only the second press→release recognizes;
    /// a single tap does not fire.
    #[test]
    fn tap_gesture_double_requires_two() {
        let (mut rt, node, log) = gesture_runtime();
        bind_gesture(&mut rt, node, "dbl", log.clone());
        rt.add_gesture(node, "dbl", GestureKind::Tap { count: 2 });

        rt.feed_gesture(&down(100.0, 100.0), Origin::Human);
        assert!(!rt.feed_gesture(&up(100.0, 100.0), Origin::Human));
        assert!(log.borrow().is_empty(), "one tap is not a double tap");

        rt.feed_gesture(&down(101.0, 101.0), Origin::Human);
        let fired = rt.feed_gesture(&up(101.0, 101.0), Origin::Human);
        assert!(fired, "second tap completes the double");
        assert_eq!(*log.borrow(), vec!["dbl".to_string()]);
    }

    /// A press that moves out of slop before release does not register a tap.
    #[test]
    fn tap_gesture_cancelled_by_movement() {
        let (mut rt, node, log) = gesture_runtime();
        bind_gesture(&mut rt, node, "tap", log.clone());
        rt.add_gesture(node, "tap", GestureKind::Tap { count: 1 });

        rt.feed_gesture(&down(100.0, 100.0), Origin::Human);
        rt.feed_gesture(&moved(180.0, 100.0), Origin::Human); // far past slop
        let fired = rt.feed_gesture(&up(180.0, 100.0), Origin::Human);
        assert!(!fired);
        assert!(log.borrow().is_empty(), "moved-away press is not a tap");
    }

    /// `LongPressGesture(minimumDuration:)` — a held press fires once `tick`
    /// advances past the duration; a short hold never fires.
    #[test]
    fn long_press_fires_after_min_duration() {
        let (mut rt, node, log) = gesture_runtime();
        bind_gesture(&mut rt, node, "long", log.clone());
        rt.add_gesture(node, "long", GestureKind::LongPress { min_duration: 0.5 });

        rt.feed_gesture(&down(100.0, 100.0), Origin::Human);
        // Not yet held long enough.
        assert!(!rt.tick_gestures(0.2, Origin::Human));
        assert!(log.borrow().is_empty());
        // Cross the threshold.
        let fired = rt.tick_gestures(0.4, Origin::Human);
        assert!(fired, "held past 0.5s recognizes the long-press");
        assert_eq!(*log.borrow(), vec!["long".to_string()]);
        // Idempotent: it fires exactly once.
        assert!(!rt.tick_gestures(1.0, Origin::Human));
        assert_eq!(log.borrow().len(), 1);
    }

    /// A long-press released before its duration never fires.
    #[test]
    fn long_press_cancelled_by_early_release() {
        let (mut rt, node, log) = gesture_runtime();
        bind_gesture(&mut rt, node, "long", log.clone());
        rt.add_gesture(node, "long", GestureKind::LongPress { min_duration: 0.5 });

        rt.feed_gesture(&down(100.0, 100.0), Origin::Human);
        rt.tick_gestures(0.2, Origin::Human);
        rt.feed_gesture(&up(100.0, 100.0), Origin::Human);
        assert!(!rt.tick_gestures(1.0, Origin::Human), "released early");
        assert!(log.borrow().is_empty());
    }

    /// `DragGesture` — once movement passes the minimum distance the drag is
    /// active, each move fires `_changed` (with the live translation) and the
    /// release fires `_ended`. The recognizer exposes the live translation.
    #[test]
    fn drag_gesture_changed_and_ended_with_translation() {
        let (mut rt, node, log) = gesture_runtime();
        bind_gesture(&mut rt, node, "drag_changed", log.clone());
        bind_gesture(&mut rt, node, "drag_ended", log.clone());
        let g = rt.add_gesture(node, "drag", GestureKind::Drag { min_distance: 10.0 });

        rt.feed_gesture(&down(100.0, 100.0), Origin::Human);
        // A tiny move inside the threshold: no _changed yet.
        assert!(!rt.feed_gesture(&moved(103.0, 100.0), Origin::Human));
        assert!(log.borrow().is_empty());

        // Move past the threshold: drag becomes active, fires _changed.
        assert!(rt.feed_gesture(&moved(140.0, 130.0), Origin::Human));
        assert_eq!(rt.gesture(g).unwrap().translation(), (40.0, 30.0));
        assert!(rt.gesture(g).unwrap().is_active());

        // A further move fires another _changed with the new translation.
        assert!(rt.feed_gesture(&moved(150.0, 100.0), Origin::Human));
        assert_eq!(rt.gesture(g).unwrap().translation(), (50.0, 0.0));

        // Release fires _ended.
        assert!(rt.feed_gesture(&up(150.0, 100.0), Origin::Human));
        assert!(!rt.gesture(g).unwrap().is_active());

        assert_eq!(
            *log.borrow(),
            vec![
                "drag_changed".to_string(),
                "drag_changed".to_string(),
                "drag_ended".to_string()
            ]
        );
    }

    /// `MagnifyGesture` — driven programmatically (no headless multitouch): each
    /// `pinch` delta fires `_changed` and composes the live scale multiplicatively;
    /// concluding fires `_ended`.
    #[test]
    fn magnify_gesture_accumulates_scale_programmatically() {
        let (mut rt, node, log) = gesture_runtime();
        bind_gesture(&mut rt, node, "zoom_changed", log.clone());
        bind_gesture(&mut rt, node, "zoom_ended", log.clone());
        let g = rt.add_gesture(node, "zoom", GestureKind::Magnify);

        assert!(rt.pinch(0.5, Origin::Human)); // scale 1.5
        assert!((rt.gesture(g).unwrap().scale() - 1.5).abs() < 1e-6);
        assert!(rt.pinch(0.2, Origin::Human)); // scale 1.5 * 1.2 = 1.8
        assert!((rt.gesture(g).unwrap().scale() - 1.8).abs() < 1e-6);

        assert!(rt.end_continuous_gestures(Origin::Human));
        assert_eq!(
            *log.borrow(),
            vec![
                "zoom_changed".to_string(),
                "zoom_changed".to_string(),
                "zoom_ended".to_string()
            ]
        );
    }

    /// `RotationGesture` — driven programmatically: each `rotate` delta fires
    /// `_changed` and sums the live angle; concluding fires `_ended`. The AI can
    /// drive it via `Origin::Ai` on the same audited path.
    #[test]
    fn rotation_gesture_accumulates_angle_programmatically() {
        let (mut rt, node, log) = gesture_runtime();
        bind_gesture(&mut rt, node, "spin_changed", log.clone());
        bind_gesture(&mut rt, node, "spin_ended", log.clone());
        let g = rt.add_gesture(node, "spin", GestureKind::Rotation);

        assert!(rt.rotate(0.5, Origin::Ai));
        assert!(rt.rotate(0.25, Origin::Ai));
        assert!((rt.gesture(g).unwrap().rotation() - 0.75).abs() < 1e-6);
        assert!(rt.end_continuous_gestures(Origin::Ai));

        assert_eq!(
            *log.borrow(),
            vec![
                "spin_changed".to_string(),
                "spin_changed".to_string(),
                "spin_ended".to_string()
            ]
        );
        // All on the audited path as AI.
        let (human, ai) = rt.invoke_counts();
        assert_eq!((human, ai), (0, 3));
    }

    /// **Combined / simultaneous.** A tap and a magnify on the same node coexist:
    /// a pinch drives the magnify (firing `_changed`) while a separate
    /// press→release still recognizes the tap — neither cancels the other.
    #[test]
    fn simultaneous_tap_and_magnify_coexist() {
        let (mut rt, node, log) = gesture_runtime();
        bind_gesture(&mut rt, node, "tap", log.clone());
        bind_gesture(&mut rt, node, "zoom_changed", log.clone());
        rt.add_gesture(node, "tap", GestureKind::Tap { count: 1 });
        rt.add_gesture(node, "zoom", GestureKind::Magnify);

        rt.pinch(0.3, Origin::Human);
        rt.feed_gesture(&down(100.0, 100.0), Origin::Human);
        rt.feed_gesture(&up(100.0, 100.0), Origin::Human);

        let fired = log.borrow().clone();
        assert!(fired.contains(&"zoom_changed".to_string()));
        assert!(fired.contains(&"tap".to_string()));
    }

    /// **Combined / sequenced precedence.** A drag and a tap on the same node:
    /// once the drag crosses its threshold it wins, cancelling the pending tap so
    /// the release does NOT also fire a tap. Only the drag's events fire.
    #[test]
    fn drag_beyond_threshold_cancels_pending_tap() {
        let (mut rt, node, log) = gesture_runtime();
        bind_gesture(&mut rt, node, "tap", log.clone());
        bind_gesture(&mut rt, node, "drag_changed", log.clone());
        bind_gesture(&mut rt, node, "drag_ended", log.clone());
        rt.add_gesture(node, "tap", GestureKind::Tap { count: 1 });
        rt.add_gesture(node, "drag", GestureKind::Drag { min_distance: 10.0 });

        rt.feed_gesture(&down(100.0, 100.0), Origin::Human);
        // Cross the drag threshold: drag activates, pending tap is cancelled.
        rt.feed_gesture(&moved(140.0, 100.0), Origin::Human);
        rt.feed_gesture(&up(140.0, 100.0), Origin::Human);

        let fired = log.borrow().clone();
        assert!(
            fired.contains(&"drag_changed".to_string()) && fired.contains(&"drag_ended".to_string()),
            "drag fired its events"
        );
        assert!(
            !fired.contains(&"tap".to_string()),
            "a winning drag cancels the pending tap"
        );
    }

    /// A short press→release below the drag threshold (with both a tap and a drag
    /// bound) still recognizes the tap — the drag never activated, so it does not
    /// suppress the tap.
    #[test]
    fn tap_survives_when_drag_does_not_activate() {
        let (mut rt, node, log) = gesture_runtime();
        bind_gesture(&mut rt, node, "tap", log.clone());
        bind_gesture(&mut rt, node, "drag_changed", log.clone());
        rt.add_gesture(node, "tap", GestureKind::Tap { count: 1 });
        rt.add_gesture(node, "drag", GestureKind::Drag { min_distance: 20.0 });

        rt.feed_gesture(&down(100.0, 100.0), Origin::Human);
        rt.feed_gesture(&moved(105.0, 100.0), Origin::Human); // within both slops
        rt.feed_gesture(&up(105.0, 100.0), Origin::Human);

        assert_eq!(*log.borrow(), vec!["tap".to_string()]);
    }

    /// A press that misses the recognizer's node does not arm it (hit-testing
    /// bubbles, but a point outside the node is not a hit).
    #[test]
    fn gesture_only_arms_on_a_hit() {
        // A small 60x40 node placed inside a larger viewport, so there is empty
        // space to press in.
        let (doc, btns) = focusable_doc(1);
        let target = btns[0];
        let mut rt = Runtime::new(doc, (800.0, 600.0));
        let log = Rc::new(RefCell::new(Vec::<String>::new()));
        bind_gesture(&mut rt, target, "tap", log.clone());
        rt.add_gesture(target, "tap", GestureKind::Tap { count: 1 });

        // Press far away from the node, then release: no tap.
        rt.feed_gesture(&down(790.0, 590.0), Origin::Human);
        let fired = rt.feed_gesture(&up(790.0, 590.0), Origin::Human);
        assert!(!fired);
        assert!(log.borrow().is_empty(), "a miss does not arm the tap");
    }
}

#[cfg(all(test, feature = "web"))]
mod web_tests {
    use super::web::CanvasHost;

    const UI: &str = r#"
        Stack { padding: 12px; background: #101010;
          Button { width: 120px; height: 40px; color: #7d39eb; corner_radius: 8px;
                   on click: noop(); }
        }
    "#;

    #[test]
    fn canvas_host_renders_nonempty_pixels() {
        let host = CanvasHost::from_uni(UI, 320, 200).expect("ui parses");
        let px = host.pixels();
        assert_eq!(px.len(), 320 * 200 * 4);
        // The dark background fill makes at least some pixels non-zero.
        assert!(px.iter().any(|&b| b != 0), "expected painted pixels");
    }

    #[test]
    fn canvas_host_resize_changes_buffer_len() {
        let mut host = CanvasHost::from_uni(UI, 100, 100).unwrap();
        assert_eq!(host.pixels().len(), 100 * 100 * 4);
        host.resize(200, 150);
        assert_eq!(host.pixels().len(), 200 * 150 * 4);
    }

    #[test]
    fn canvas_host_tick_settles() {
        let mut host = CanvasHost::from_uni(UI, 64, 64).unwrap();
        // With no active animation, tick reports not-animating immediately.
        assert!(!host.tick(1.0 / 60.0));
    }
}
