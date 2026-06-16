//! # uni-runtime — the app / event-loop layer (rung 3, *interactive*)
//!
//! This crate **closes the accountability circle into the live UI**. The other
//! crates established the pieces:
//!
//! - `uni-ir` owns the [`Document`] and the audited [`Document::fire`] surface
//!   (every invocation lands in the append-only audit log carrying its
//!   [`Origin`]).
//! - `uni-dsl` parses `.uni` into a `Document` (callbacks → `SetCallback`).
//! - `uni-core` does [`layout`], [`paint`], and [`hit_test`].
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

use std::collections::HashMap;
use std::sync::Arc;

use uni_core::{hit_test, layout, paint, Layout};
use uni_ir::{Action, Document, Mutation, NodeId, Origin};
use uni_reactor::Store;
use uni_render::{
    translate_window_event, InputEvent, PointerButton, RenderError, Renderer, Scene, WgpuRenderer,
};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

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
}

impl Runtime {
    /// Build a runtime around an existing [`Document`] for the given logical
    /// `viewport`. No window is created — this is the headless/testable form.
    pub fn new(doc: Document, viewport: (f32, f32)) -> Self {
        let mut rt = Runtime {
            doc,
            store: Store::new(),
            layout: Layout::default(),
            viewport,
            registry: HashMap::new(),
            cursor: (0.0, 0.0),
            window: None,
            renderer: None,
        };
        // Push any state already present into the bound props, then lay out, so
        // the very first frame reflects the store (not just literal defaults).
        rt.sync_bindings();
        rt.relayout();
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
    /// call [`Runtime::sync_bindings`] (or let [`Runtime::dispatch`] do it) to
    /// push the values into the bound props.
    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    /// **Push live state into the literal props of bound nodes — id-stable.**
    ///
    /// Walk the live [`Document`]; for every node carrying `bindings`, look up
    /// each bound key in the [`Store`] and, if present, apply an
    /// `Origin::System` [`Mutation::SetProp`] on that *same* node id. This makes
    /// the DSL's `$bindings` live without ever changing node ids or tree
    /// structure (so hit-test and [`Document::fire`] keep targeting the same
    /// nodes). Called before every [`relayout`](Runtime::relayout).
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

    /// Recompute the layout for the current document + viewport. Called after
    /// every action and on resize.
    fn relayout(&mut self) {
        self.layout = layout(&self.doc, self.viewport);
    }

    /// Set the viewport and recompute the layout.
    pub fn set_viewport(&mut self, viewport: (f32, f32)) {
        self.viewport = viewport;
        self.relayout();
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
    ///
    /// Returns `true` if a click was handled (so a caller without a window can
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
                match self.bubble_to_handler((*x, *y), "click") {
                    Some(target) => self.dispatch(target, "click", Origin::Human),
                    None => false,
                }
            }
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

    // -- windowed entry point -------------------------------------------------

    /// Run the interactive event loop on a real window (blocking). The window's
    /// initial logical size becomes the viewport. Press a window's close button
    /// to exit; the audit log is printed on exit.
    pub fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        let event_loop = EventLoop::new()?;
        event_loop.set_control_flow(ControlFlow::Wait);
        let mut app = WindowedApp { rt: self };
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
}

/// The winit `ApplicationHandler` wrapper that drives a [`Runtime`] against a
/// live window + GPU renderer.
struct WindowedApp {
    rt: Runtime,
}

impl ApplicationHandler for WindowedApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.rt.window.is_some() {
            return;
        }
        let (vw, vh) = self.rt.viewport;
        let attrs = Window::default_attributes()
            .with_title("Uni-UI — interactive runtime")
            .with_inner_size(winit::dpi::LogicalSize::new(vw as f64, vh as f64));
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
                self.rt.window = Some(window);
                eprintln!(
                    "uni-runtime: click the button (Human fire) or press 'A' (AI fire). \
                     Close the window to print the audit log."
                );
            }
            Err(e) => {
                eprintln!("renderer init failed: {e}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(window) = self.rt.window.clone() else {
            return;
        };

        // Translate the window event into our renderer-agnostic InputEvent and
        // feed it through the audited input path. A handled click requests a
        // repaint. We also special-case the 'A' key to demonstrate ai_fire on
        // the same surface.
        if let Some(input) =
            translate_window_event(&event, window.scale_factor(), &mut self.rt.cursor)
        {
            match &input {
                InputEvent::KeyDown { key } if key.eq_ignore_ascii_case("a") => {
                    // Cowork proof, live: the AI fires "click" on whatever node
                    // would handle a click under the current cursor (falling
                    // back to any node that has a click handler).
                    if let Some(target) = self.rt.ai_click_target() {
                        if self.rt.ai_fire(target, "click") {
                            window.request_redraw();
                        }
                    }
                }
                _ => {
                    if self.rt.on_input(&input) {
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
                window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let size = window.inner_size();
                if let Some(r) = self.rt.renderer.as_mut() {
                    r.resize(size.width, size.height, scale_factor);
                }
            }
            WindowEvent::RedrawRequested => {
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
        self.layout
            .order()
            .iter()
            .copied()
            .find(|&id| {
                self.doc
                    .get(id)
                    .map(|n| n.callbacks.contains_key("click"))
                    .unwrap_or(false)
            })
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
            doc.get(button).unwrap().callbacks.get("click").unwrap().name,
            "increment"
        );
        // The label carries a binding for `content` → the `label` state key.
        assert_eq!(
            doc.get(label).unwrap().bindings.get("content"),
            Some(&Binding { expr: "label".into() })
        );
    }

    /// `sync_bindings` pushes a store value into the bound node's literal prop,
    /// **on the same node id** (no structural change).
    #[test]
    fn sync_bindings_pushes_store_value_into_bound_prop() {
        let mut rt = Runtime::from_uni(COUNTER_UNI, (800.0, 600.0)).unwrap();
        let label = label_id(rt.doc());

        // Set state for the bound key, then sync.
        rt.store_mut().set("label", Value::Text("from state".into()));
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
        doc.apply_from(Origin::System, Mutation::CreateNode { id: row, kind: "Row".into() })
            .unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id: row }).unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::SetCallback {
                id: row,
                event: "click".into(),
                action: uni_ir::Action { name: "ping".into(), args: vec![] },
            },
        )
        .unwrap();

        let child = doc.fresh_id();
        doc.apply_from(Origin::System, Mutation::CreateNode { id: child, kind: "Rect".into() })
            .unwrap();
        doc.apply_from(Origin::System, Mutation::SetProp { id: child, key: "width".into(), value: Value::Px(100.0) }).unwrap();
        doc.apply_from(Origin::System, Mutation::SetProp { id: child, key: "height".into(), value: Value::Px(100.0) }).unwrap();
        doc.apply_from(Origin::System, Mutation::AppendChild { parent: row, child }).unwrap();

        let mut rt = Runtime::new(doc, (400.0, 400.0));
        let pinged = Rc::new(RefCell::new(0i64));
        let p = pinged.clone();
        rt.register("ping", Box::new(move |_store: &mut Store, _origin: Origin| { *p.borrow_mut() += 1; }));

        // The child Rect has no callback of its own.
        assert!(rt.doc().get(child).unwrap().callbacks.get("click").is_none());

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
}
