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

use uni_a11y::build_tree;
use uni_core::{hit_test, layout, paint, Layout};
use uni_env::Env;
use uni_ir::{Action, Document, Mutation, NodeId, Origin};
use uni_reactor::Store;
use uni_render::{
    translate_window_event, InputEvent, PointerButton, RenderError, Renderer, Scene, WgpuRenderer,
};
use uni_spring::{Spring, SpringState};
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
    /// Currently focused node (Tab/arrow-key navigation target).
    focused: Option<NodeId>,
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
            focused: None,
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
                Mutation::SetProp { id, key: prop, value: uni_ir::Value::Px(value) },
            );
        }
        self.relayout();
        !self.animations.is_empty()
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
        self.env = Env::for_window(viewport.0, viewport.1);
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
            InputEvent::KeyDown { key } if key == "Tab" => {
                self.move_focus(true)
            }
            InputEvent::KeyDown { key } if key == "Enter" || key == " " => {
                self.activate_focused()
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
        let event_loop = winit::event_loop::EventLoop::<accesskit_winit::Event>::with_user_event()
            .build()?;
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
    /// Returns `true` if a handler ran (same semantics as [`Runtime::dispatch`]).
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
}

/// The winit `ApplicationHandler` wrapper that drives a [`Runtime`] against a
/// live window + GPU renderer.
struct WindowedApp {
    rt: Runtime,
    a11y: Option<accesskit_winit::Adapter>,
    proxy: winit::event_loop::EventLoopProxy<accesskit_winit::Event>,
    last_frame: Option<std::time::Instant>,
}

impl winit::application::ApplicationHandler<accesskit_winit::Event> for WindowedApp {
    fn user_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        event: accesskit_winit::Event,
    ) {
        match event.window_event {
            accesskit_winit::WindowEvent::InitialTreeRequested => {
                // The a11y platform needs the initial tree now. Push the full tree.
                if let Some(a11y) = &mut self.a11y {
                    let doc = &self.rt.doc;
                    let layout = &self.rt.layout;
                    let focused = self.rt.focused;
                    a11y.update_if_active(|| build_tree(doc, layout, focused));
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
        let adapter = accesskit_winit::Adapter::with_event_loop_proxy(
            event_loop,
            window.as_ref(),
            self.proxy.clone(),
        );
        self.a11y = Some(adapter);
        // Now show the window.
        window.set_visible(true);
        self.rt.window = Some(window);
        self.last_frame = Some(std::time::Instant::now());
        // Push the initial a11y tree now that activation will have been triggered.
        if let Some(a11y) = &mut self.a11y {
            let doc = &self.rt.doc;
            let layout = &self.rt.layout;
            let focused = self.rt.focused;
            a11y.update_if_active(|| build_tree(doc, layout, focused));
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
                                let doc = &self.rt.doc;
                                let layout = &self.rt.layout;
                                let focused = self.rt.focused;
                                a11y.update_if_active(|| build_tree(doc, layout, focused));
                            }
                            window.request_redraw();
                        }
                    }
                }
                _ => {
                    if self.rt.on_input(&input) {
                        if let Some(a11y) = &mut self.a11y {
                            let doc = &self.rt.doc;
                            let layout = &self.rt.layout;
                            let focused = self.rt.focused;
                            a11y.update_if_active(|| build_tree(doc, layout, focused));
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
                    let doc = &self.rt.doc;
                    let layout = &self.rt.layout;
                    let focused = self.rt.focused;
                    a11y.update_if_active(|| build_tree(doc, layout, focused));
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
            let mut host = Self { rt, renderer, width, height };
            host.render();
            host
        }

        /// Parse `.uni` source and build a host at the given size.
        pub fn from_uni(
            src: &str,
            width: u32,
            height: u32,
        ) -> Result<Self, uni_dsl::ParseError> {
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
        doc.apply_from(Origin::System, Mutation::CreateNode { id, kind: "Rect".into() }).unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id }).unwrap();

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

    #[test]
    fn a11y_tree_built_headless() {
        let rt = Runtime::from_uni(
            r#"Stack { Text { content: "Hello"; } }"#,
            (800.0, 600.0),
        )
        .unwrap();
        let update = uni_a11y::build_tree(rt.doc(), rt.layout(), None);
        // Should have at least 3 entries: window root + Stack + Text.
        assert!(update.nodes.len() >= 3, "expected window+Stack+Text in a11y tree");
    }

    // -----------------------------------------------------------------------
    // Keyboard navigation tests (Task B / Task E)
    // -----------------------------------------------------------------------

    /// Helper: build a Document with a root Row containing `count` Button nodes,
    /// each carrying a "click" callback that fires the action "hit_{i}".
    fn focusable_doc(count: usize) -> (Document, Vec<NodeId>) {
        let mut doc = Document::new();
        let row = doc.fresh_id();
        doc.apply_from(Origin::System, Mutation::CreateNode { id: row, kind: "Row".into() })
            .unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id: row }).unwrap();

        let mut ids = Vec::new();
        for i in 0..count {
            let btn = doc.fresh_id();
            doc.apply_from(
                Origin::System,
                Mutation::CreateNode { id: btn, kind: "Stack".into() },
            )
            .unwrap();
            doc.apply_from(
                Origin::System,
                Mutation::SetProp { id: btn, key: "width".into(), value: Value::Px(60.0) },
            )
            .unwrap();
            doc.apply_from(
                Origin::System,
                Mutation::SetProp { id: btn, key: "height".into(), value: Value::Px(40.0) },
            )
            .unwrap();
            doc.apply_from(
                Origin::System,
                Mutation::SetCallback {
                    id: btn,
                    event: "click".into(),
                    action: uni_ir::Action { name: format!("hit_{i}"), args: vec![] },
                },
            )
            .unwrap();
            doc.apply_from(
                Origin::System,
                Mutation::AppendChild { parent: row, child: btn },
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
        assert_eq!(rt.focused(), Some(btns[1]), "second Tab focuses second node");

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
            rt.register("hit_0", Box::new(move |_s: &mut Store, _o: Origin| { *h0.borrow_mut() += 1; }));
            let h1 = hit1.clone();
            rt.register("hit_1", Box::new(move |_s: &mut Store, _o: Origin| { *h1.borrow_mut() += 1; }));
        }

        // Tab to the first button, then activate with Enter.
        rt.on_input(&InputEvent::KeyDown { key: "Tab".into() });
        assert_eq!(rt.focused(), Some(btns[0]));

        let activated = rt.on_input(&InputEvent::KeyDown { key: "Enter".into() });
        assert!(activated, "Enter on a focused node should return true");
        assert_eq!(*hit0.borrow(), 1, "hit_0 handler should have fired once");
        assert_eq!(*hit1.borrow(), 0, "hit_1 should not have fired");
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
        assert_eq!(rt.focused(), Some(btns[0]), "Tab past last should wrap to first");

        // Backward from first should wrap to last.
        rt.move_focus(false);
        assert_eq!(rt.focused(), Some(btns[2]), "backward from first should wrap to last");
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
