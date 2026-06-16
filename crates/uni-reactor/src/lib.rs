//! # uni-reactor — reactive evaluation (rung 4: the DSL goes *live*)
//!
//! `uni-ir` carries dynamic [`Binding`]s (`bindings["content"] = "title"`) and
//! structural nodes (`If`, `For`) alongside literal props, but it deliberately
//! does *not* resolve them — that's this crate's job. The reactor takes a
//! *source* [`Document`] (the authored tree, bindings and all) plus a [`Store`]
//! of state and produces a *resolved* render [`Document`]: bindings become
//! literal props, `If` nodes appear or vanish by their condition, and `For`
//! nodes expand into repeated instances. The resolved document is plain,
//! literal IR — exactly what [`uni_core::layout`] consumes.
//!
//! ## State store
//!
//! [`Store`] is a `String -> Value` map backed by `uni-react` signals, so a
//! later runtime can observe changes. v0 exposes a `dirty` flag (set on every
//! [`Store::set`]) that a [`Reactor`] uses to know when to re-resolve.
//!
//! ## Binding expressions (v0)
//!
//! A [`Binding::expr`] is treated as a plain **state key** (a bare key, or a
//! dotted path such as `"user.name"` — looked up verbatim in the [`Store`]).
//! There is no expression grammar yet; richer evaluation is a later rung.
//!
//! ## `For` expansion
//!
//! `For` supports two binding shapes. If `bindings["items"]` resolves to
//! `Value::Int(n)`, the template children are repeated `n` times (count path).
//! If it resolves to `Value::List(items)`, the template children are repeated
//! once per item; each cloned root child receives an `item` prop (the item
//! value) and an `index` prop (`Value::Int(i)`).

use std::collections::BTreeMap;

use uni_ir::{Binding, Document, Mutation, NodeId, Origin, Value};
use uni_react::{Runtime, Signal};

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// A reactive state store: `String -> Value`, backed by `uni-react` signals.
///
/// Each key owns its own [`Signal<Option<Value>>`], so a future runtime can
/// wire effects to individual keys. v0 additionally tracks a coarse `dirty`
/// signal (flipped on every [`set`](Store::set)) that the [`Reactor`] reads to
/// decide when to re-resolve.
#[derive(Clone)]
pub struct Store {
    rt: Runtime,
    /// Per-key value cells. We keep the map of signals so reads/writes go
    /// through the reactive graph (and stay forward-compatible with effects).
    cells: BTreeMap<String, Signal<Option<Value>>>,
    /// Coarse change tick: bumped on every `set`. The reactor flips a dirty
    /// flag from an effect on this signal.
    tick: Signal<u64>,
}

impl Store {
    /// Create an empty store with a fresh reactive runtime.
    pub fn new() -> Self {
        let rt = Runtime::new();
        let tick = rt.signal(0u64);
        Store {
            rt,
            cells: BTreeMap::new(),
            tick,
        }
    }

    /// The reactive runtime backing this store (shared with any [`Reactor`]
    /// built from it, so effects observe the same graph).
    pub fn runtime(&self) -> &Runtime {
        &self.rt
    }

    /// A signal that ticks (increments) on every [`set`](Store::set). Read it
    /// inside a `uni-react` effect to be notified of *any* store change.
    pub fn change_tick(&self) -> Signal<u64> {
        self.tick.clone()
    }

    /// Set `key` to `value`, notifying observers.
    ///
    /// Creates the key's signal on first write. Also bumps the coarse change
    /// tick so a reactor knows to re-resolve.
    pub fn set(&mut self, key: impl Into<String>, value: Value) {
        let key = key.into();
        match self.cells.get(&key) {
            Some(sig) => sig.set(Some(value)),
            None => {
                let sig = self.rt.signal(Some(value));
                self.cells.insert(key, sig);
            }
        }
        // Bump the change tick (read-modify-write through the signal).
        self.tick.update(|t| *t += 1);
    }

    /// Read `key`, recording a reactive dependency on it (and on the change
    /// tick, so brand-new keys still notify). Returns `None` if unset.
    pub fn get(&self, key: &str) -> Option<Value> {
        // Track the coarse tick so a read performed inside an effect re-fires
        // even when the key did not exist at read time (its signal is created
        // lazily on first `set`).
        let _ = self.tick.get();
        self.cells.get(key).and_then(|sig| sig.get())
    }
}

impl Default for Store {
    fn default() -> Self {
        Store::new()
    }
}

impl Store {
    /// Snapshot the current store to a simple key=value text format.
    /// Format: one "key\ttype\tencoded_value\n" line per entry.
    /// Only entries with a set value are included.
    pub fn snapshot(&self) -> String {
        let mut out = String::new();
        for (k, sig) in &self.cells {
            if let Some(v) = sig.get() {
                let (ty, val) = encode_value(&v);
                out.push_str(&format!("{k}\t{ty}\t{val}\n"));
            }
        }
        out
    }

    /// Restore from a snapshot produced by [`Store::snapshot`].
    /// Unknown keys are silently skipped. Returns the number of keys restored.
    pub fn restore(&mut self, snapshot: &str) -> usize {
        let mut count = 0;
        for line in snapshot.lines() {
            let parts: Vec<&str> = line.splitn(3, '\t').collect();
            if parts.len() != 3 {
                continue;
            }
            if let Some(v) = decode_value(parts[1], parts[2]) {
                self.set(parts[0], v);
                count += 1;
            }
        }
        count
    }
}

fn encode_value(v: &Value) -> (&'static str, String) {
    match v {
        Value::Bool(b) => ("bool", b.to_string()),
        Value::Int(n) => ("int", n.to_string()),
        Value::Float(f) => ("float", f.to_string()),
        Value::Text(s) => ("text", s.replace('\n', "\\n").replace('\t', "\\t")),
        Value::Color(c) => ("color", format!("{c:08X}")),
        Value::Px(f) => ("px", f.to_string()),
        Value::List(items) => ("list", items.len().to_string()), // shallow: count only
    }
}

fn decode_value(ty: &str, val: &str) -> Option<Value> {
    match ty {
        "bool" => Some(Value::Bool(val == "true")),
        "int" => val.parse().ok().map(Value::Int),
        "float" => val.parse().ok().map(Value::Float),
        "text" => Some(Value::Text(val.replace("\\n", "\n").replace("\\t", "\t"))),
        "color" => u32::from_str_radix(val, 16).ok().map(Value::Color),
        "px" => val.parse().ok().map(Value::Px),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// resolve
// ---------------------------------------------------------------------------

/// Evaluate one [`Binding`] against the [`Store`].
///
/// v0: `expr` is a plain state key (bare or dotted), looked up verbatim.
fn eval_binding(b: &Binding, store: &Store) -> Option<Value> {
    store.get(b.expr.as_str())
}

/// Produce a *resolved* render [`Document`] from a *source* document + state.
///
/// - **Bindings:** every node's `bindings[k]` is evaluated as a state key; the
///   result (if any) is written to the resolved node's `props[k]`, setting or
///   overriding a literal of the same key. Literal props with no binding pass
///   through unchanged.
/// - **`If`:** a node of kind `"If"` evaluates `bindings["cond"]`. If it is
///   `Value::Bool(true)`, the node's *children* are spliced into the parent in
///   place (the `If` wrapper itself is dropped); otherwise the `If` and its
///   whole subtree are omitted.
/// - **`For`:** a node of kind `"For"` evaluates `bindings["items"]`. v0 — if
///   it resolves to `Value::Int(n)`, the template children are emitted `n`
///   times in place, each instance getting fresh [`NodeId`]s. The `For`
///   wrapper itself is dropped.
/// - Every other node is copied with its children resolved recursively, in
///   order.
///
/// The result is a fresh [`Document`] (new ids) of plain literal IR, suitable
/// for [`uni_core::layout`]. A source with no root yields an empty document.
pub fn resolve(src: &Document, store: &Store) -> Document {
    let mut out = Document::new();
    let Some(src_root) = src.root() else {
        return out;
    };

    // The source root is resolved as a list of output roots. `If`/`For` could
    // in principle expand the root into 0..n nodes; we keep the *first* as the
    // document root (a render document has a single root), which is the natural
    // case for a real tree (the root is a concrete container, not a control
    // node).
    let roots = resolve_into(src, src_root, store, &mut out);
    if let Some(&root) = roots.first() {
        out.apply_from(Origin::System, Mutation::SetRoot { id: root })
            .expect("freshly created root exists");
        // Any stray extra roots are appended under the chosen root so they are
        // not orphaned (keeps the output a single connected tree).
        for &extra in &roots[1..] {
            out.apply_from(
                Origin::System,
                Mutation::AppendChild {
                    parent: root,
                    child: extra,
                },
            )
            .expect("freshly created nodes exist");
        }
    }
    out
}

/// Resolve `src_id` from `src`, emitting node(s) into `out`. Returns the list
/// of *output* node ids this source node expanded to (0 for a false `If`, `n`
/// template copies for a `For`, exactly 1 for an ordinary node).
fn resolve_into(
    src: &Document,
    src_id: NodeId,
    store: &Store,
    out: &mut Document,
) -> Vec<NodeId> {
    let Some(node) = src.get(src_id) else {
        return Vec::new();
    };

    match node.kind.as_str() {
        "If" => {
            let cond = node
                .bindings
                .get("cond")
                .and_then(|b| eval_binding(b, store));
            if matches!(cond, Some(Value::Bool(true))) {
                // Splice the children in place; drop the `If` wrapper.
                let mut emitted = Vec::new();
                for &child in &node.children {
                    emitted.extend(resolve_into(src, child, store, out));
                }
                emitted
            } else {
                // False (or non-bool / unset): omit node + subtree entirely.
                Vec::new()
            }
        }
        "For" => {
            let items = node
                .bindings
                .get("items")
                .and_then(|b| eval_binding(b, store));
            let mut emitted = Vec::new();
            match items {
                Some(Value::List(list)) => {
                    // Per-item expansion: one pass per item, injecting `item`
                    // and `index` props onto each cloned root child.
                    for (i, item_val) in list.iter().enumerate() {
                        for &child in &node.children {
                            let child_ids = resolve_into(src, child, store, out);
                            for new_child in child_ids {
                                // Inject `item` and `index` onto the cloned root child.
                                out.apply_from(
                                    Origin::System,
                                    Mutation::SetProp {
                                        id: new_child,
                                        key: "item".into(),
                                        value: item_val.clone(),
                                    },
                                )
                                .expect("node just created");
                                out.apply_from(
                                    Origin::System,
                                    Mutation::SetProp {
                                        id: new_child,
                                        key: "index".into(),
                                        value: Value::Int(i as i64),
                                    },
                                )
                                .expect("node just created");
                                emitted.push(new_child);
                            }
                        }
                    }
                }
                Some(Value::Int(n)) if n > 0 => {
                    // Count-based expansion (v0 path): repeat template `n` times.
                    for _ in 0..n as usize {
                        for &child in &node.children {
                            emitted.extend(resolve_into(src, child, store, out));
                        }
                    }
                }
                _ => {} // 0 or unset: emit nothing
            }
            emitted
        }
        _ => {
            // Ordinary node: copy it, resolve bindings into literal props,
            // recurse into children.
            let new_id = emit_node(src, src_id, node, store, out);
            vec![new_id]
        }
    }
}

/// Emit a single ordinary node into `out`: create it, copy literal props,
/// overlay resolved bindings, then resolve+append its children. Returns the
/// new node's id.
fn emit_node(
    src: &Document,
    src_id: NodeId,
    node: &uni_ir::Node,
    store: &Store,
    out: &mut Document,
) -> NodeId {
    let new_id = out.fresh_id();
    out.apply_from(
        Origin::System,
        Mutation::CreateNode {
            id: new_id,
            kind: node.kind.clone(),
        },
    )
    .expect("fresh id is unique");

    // Literal props pass through unchanged.
    for (k, v) in &node.props {
        out.apply_from(
            Origin::System,
            Mutation::SetProp {
                id: new_id,
                key: k.clone(),
                value: v.clone(),
            },
        )
        .expect("node just created");
    }

    // Bindings overlay/override their prop with the resolved literal value.
    for (k, b) in &node.bindings {
        if let Some(value) = eval_binding(b, store) {
            out.apply_from(
                Origin::System,
                Mutation::SetProp {
                    id: new_id,
                    key: k.clone(),
                    value,
                },
            )
            .expect("node just created");
        }
    }

    // Callbacks pass through unchanged (interaction survives resolution).
    for (event, action) in &node.callbacks {
        out.apply_from(
            Origin::System,
            Mutation::SetCallback {
                id: new_id,
                event: event.clone(),
                action: action.clone(),
            },
        )
        .expect("node just created");
    }

    // Resolve children and append in order.
    for &child in &node.children {
        for new_child in resolve_into(src, child, store, out) {
            out.apply_from(
                Origin::System,
                Mutation::AppendChild {
                    parent: new_id,
                    child: new_child,
                },
            )
            .expect("parent and child just created");
        }
    }

    let _ = src_id; // kept for symmetry / future per-node provenance
    new_id
}

// ---------------------------------------------------------------------------
// Reactor
// ---------------------------------------------------------------------------

/// Holds a source [`Document`] + a [`Store`] and produces resolved render
/// documents on demand, re-resolving only when the store has changed.
///
/// Intended to be driven by `uni-runtime` later: keep a `Reactor`, mutate its
/// store, and pull [`resolved`](Reactor::resolved) to get a fresh render tree.
pub struct Reactor {
    src: Document,
    store: Store,
    /// Set whenever the store changes (via the change-tick effect); cleared
    /// when [`resolved`](Reactor::resolved) recomputes.
    dirty: std::rc::Rc<std::cell::Cell<bool>>,
    /// Keep the change-watching effect alive for the reactor's lifetime.
    _watch: uni_react::Effect,
}

impl Reactor {
    /// Build a reactor over `src` + `store`. Installs a `uni-react` effect on
    /// the store's change tick so any [`Store::set`] flips the dirty flag.
    pub fn new(src: Document, store: Store) -> Self {
        let dirty = std::rc::Rc::new(std::cell::Cell::new(true));
        let d = dirty.clone();
        let tick = store.change_tick();
        // The effect reads the tick (subscribing to it); every later `set`
        // bumps the tick and re-fires this, marking us dirty. The initial run
        // also sets dirty=true, which is fine — first `resolved()` must compute.
        let watch = store.runtime().effect(move |_| {
            let _ = tick.get();
            d.set(true);
        });
        Reactor {
            src,
            store,
            dirty,
            _watch: watch,
        }
    }

    /// Immutable access to the backing store.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Mutable access to the backing store. Mutating it (via [`Store::set`])
    /// marks the reactor dirty through the change-tick effect.
    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    /// Replace the source document, forcing a re-resolve on next pull.
    pub fn set_source(&mut self, src: Document) {
        self.src = src;
        self.dirty.set(true);
    }

    /// Has the store changed since the last [`resolved`](Reactor::resolved)?
    pub fn is_dirty(&self) -> bool {
        self.dirty.get()
    }

    /// Produce the resolved render document. Always reflects the current store;
    /// clears the dirty flag. (v0 recomputes on every call — the dirty flag is
    /// exposed so a caller/runtime can *skip* the call when nothing changed.)
    pub fn resolved(&self) -> Document {
        self.dirty.set(false);
        resolve(&self.src, &self.store)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use uni_ir::{Binding, Mutation, Origin, Value};

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

    fn set_root(doc: &mut Document, id: NodeId) {
        doc.apply_from(Origin::System, Mutation::SetRoot { id }).unwrap();
    }

    fn child(doc: &mut Document, parent: NodeId, c: NodeId) {
        doc.apply_from(Origin::System, Mutation::AppendChild { parent, child: c })
            .unwrap();
    }

    fn bind(doc: &mut Document, id: NodeId, key: &str, expr: &str) {
        doc.apply_from(
            Origin::System,
            Mutation::SetBinding {
                id,
                key: key.into(),
                binding: Binding { expr: expr.into() },
            },
        )
        .unwrap();
    }

    /// Find the single node of `kind` in a resolved doc by walking from root.
    fn find_kind(doc: &Document, kind: &str) -> Vec<NodeId> {
        let mut found = Vec::new();
        if let Some(root) = doc.root() {
            collect(doc, root, kind, &mut found);
        }
        found
    }

    fn collect(doc: &Document, id: NodeId, kind: &str, out: &mut Vec<NodeId>) {
        if let Some(n) = doc.get(id) {
            if n.kind == kind {
                out.push(id);
            }
            for &c in &n.children {
                collect(doc, c, kind, out);
            }
        }
    }

    #[test]
    fn binding_resolves_to_literal_prop() {
        let mut src = Document::new();
        let root = node(&mut src, "Stack");
        set_root(&mut src, root);
        let label = node(&mut src, "Text");
        bind(&mut src, label, "content", "title");
        child(&mut src, root, label);

        let mut store = Store::new();
        store.set("title", Value::Text("Hi".into()));

        let resolved = resolve(&src, &store);
        let texts = find_kind(&resolved, "Text");
        assert_eq!(texts.len(), 1);
        assert_eq!(
            resolved.get(texts[0]).unwrap().props.get("content"),
            Some(&Value::Text("Hi".into()))
        );
    }

    #[test]
    fn literal_props_pass_through_and_binding_overrides() {
        let mut src = Document::new();
        let root = node(&mut src, "Stack");
        set_root(&mut src, root);
        let label = node(&mut src, "Text");
        // A literal content...
        src.apply_from(
            Origin::System,
            Mutation::SetProp {
                id: label,
                key: "content".into(),
                value: Value::Text("default".into()),
            },
        )
        .unwrap();
        // ...plus a literal size with NO binding (must pass through).
        src.apply_from(
            Origin::System,
            Mutation::SetProp {
                id: label,
                key: "size".into(),
                value: Value::Px(20.0),
            },
        )
        .unwrap();
        // A binding on content overrides the literal.
        bind(&mut src, label, "content", "title");
        child(&mut src, root, label);

        let mut store = Store::new();
        store.set("title", Value::Text("Bound".into()));

        let resolved = resolve(&src, &store);
        let t = find_kind(&resolved, "Text")[0];
        let props = &resolved.get(t).unwrap().props;
        assert_eq!(props.get("content"), Some(&Value::Text("Bound".into())));
        assert_eq!(props.get("size"), Some(&Value::Px(20.0)));
    }

    #[test]
    fn if_false_omits_children_true_keeps_them() {
        let build = |cond: bool| {
            let mut src = Document::new();
            let root = node(&mut src, "Stack");
            set_root(&mut src, root);
            let iff = node(&mut src, "If");
            bind(&mut src, iff, "cond", "show");
            child(&mut src, root, iff);
            let inner = node(&mut src, "Rect");
            child(&mut src, iff, inner);

            let mut store = Store::new();
            store.set("show", Value::Bool(cond));
            resolve(&src, &store)
        };

        // cond=false → the Rect (If's child) is absent, and no If wrapper.
        let r_false = build(false);
        assert!(find_kind(&r_false, "Rect").is_empty());
        assert!(find_kind(&r_false, "If").is_empty());

        // cond=true → the Rect is spliced into the parent; still no If wrapper.
        let r_true = build(true);
        assert_eq!(find_kind(&r_true, "Rect").len(), 1);
        assert!(find_kind(&r_true, "If").is_empty());
    }

    #[test]
    fn for_with_int_repeats_template() {
        let mut src = Document::new();
        let root = node(&mut src, "Stack");
        set_root(&mut src, root);
        let forn = node(&mut src, "For");
        bind(&mut src, forn, "items", "count");
        child(&mut src, root, forn);
        let tmpl = node(&mut src, "Rect");
        child(&mut src, forn, tmpl);

        let mut store = Store::new();
        store.set("count", Value::Int(3));

        let resolved = resolve(&src, &store);
        // 3 template instances, no For wrapper.
        assert_eq!(find_kind(&resolved, "Rect").len(), 3);
        assert!(find_kind(&resolved, "For").is_empty());
        // The instances are distinct nodes (fresh ids).
        let rects = find_kind(&resolved, "Rect");
        assert_ne!(rects[0], rects[1]);
        assert_ne!(rects[1], rects[2]);
    }

    #[test]
    fn reactor_re_resolves_after_store_change() {
        let mut src = Document::new();
        let root = node(&mut src, "Stack");
        set_root(&mut src, root);
        let label = node(&mut src, "Text");
        bind(&mut src, label, "content", "title");
        child(&mut src, root, label);

        let mut store = Store::new();
        store.set("title", Value::Text("first".into()));

        let mut reactor = Reactor::new(src, store);
        assert!(reactor.is_dirty());

        let r1 = reactor.resolved();
        let t1 = find_kind(&r1, "Text")[0];
        assert_eq!(
            r1.get(t1).unwrap().props.get("content"),
            Some(&Value::Text("first".into()))
        );
        // After pulling, the reactor is clean...
        assert!(!reactor.is_dirty());

        // ...until the store changes, which re-marks it dirty (via the effect).
        reactor
            .store_mut()
            .set("title", Value::Text("second".into()));
        assert!(reactor.is_dirty());

        let r2 = reactor.resolved();
        let t2 = find_kind(&r2, "Text")[0];
        assert_eq!(
            r2.get(t2).unwrap().props.get("content"),
            Some(&Value::Text("second".into()))
        );
    }

    #[test]
    fn for_expands_by_list_count() {
        // A For node bound to a List of 3 items expands into 3 children,
        // each carrying `item` and `index` props.
        use uni_ir::{Binding, Document, Mutation, NodeId, Origin, Value};
        use crate::Store;

        let mut doc = Document::new();
        // root Stack
        let root = doc.fresh_id();
        doc.apply_from(Origin::System, Mutation::CreateNode { id: root, kind: "Stack".into() }).unwrap();
        doc.apply_from(Origin::System, Mutation::SetRoot { id: root }).unwrap();
        // For node bound to "items"
        let for_node = doc.fresh_id();
        doc.apply_from(Origin::System, Mutation::CreateNode { id: for_node, kind: "For".into() }).unwrap();
        doc.apply_from(Origin::System, Mutation::SetBinding {
            id: for_node, key: "items".into(),
            binding: Binding { expr: "items".into() },
        }).unwrap();
        doc.apply_from(Origin::System, Mutation::AppendChild { parent: root, child: for_node }).unwrap();
        // Template child inside For: a Text node
        let template = doc.fresh_id();
        doc.apply_from(Origin::System, Mutation::CreateNode { id: template, kind: "Text".into() }).unwrap();
        doc.apply_from(Origin::System, Mutation::AppendChild { parent: for_node, child: template }).unwrap();

        let mut store = Store::new();
        store.set("items", Value::List(vec![
            Value::Text("apple".into()),
            Value::Text("banana".into()),
            Value::Text("cherry".into()),
        ]));

        let resolved = crate::resolve(&doc, &store);

        // The For node should have been replaced by 3 children in the root.
        let _root_node = resolved.get(resolved.root().unwrap()).unwrap();
        // The For placeholder itself may or may not remain; what matters is
        // that 3 expanded Text nodes exist in the tree.
        let mut text_count = 0;
        fn count_text(doc: &Document, id: NodeId, n: &mut usize) {
            if let Some(node) = doc.get(id) {
                if node.kind == "Text" { *n += 1; }
                for &c in &node.children { count_text(doc, c, n); }
            }
        }
        count_text(&resolved, resolved.root().unwrap(), &mut text_count);
        assert_eq!(text_count, 3, "expected 3 Text nodes from list expansion");

        // Also verify `item` and `index` props are set on each Text node.
        let texts = find_kind(&resolved, "Text");
        let expected_items = ["apple", "banana", "cherry"];
        for (i, &tid) in texts.iter().enumerate() {
            let node = resolved.get(tid).unwrap();
            assert_eq!(
                node.props.get("item"),
                Some(&Value::Text(expected_items[i].into())),
                "Text[{i}] should have item prop"
            );
            assert_eq!(
                node.props.get("index"),
                Some(&Value::Int(i as i64)),
                "Text[{i}] should have index prop"
            );
        }
    }

    #[test]
    fn snapshot_and_restore_roundtrip() {
        let mut store = Store::new();
        store.set("flag", Value::Bool(true));
        store.set("count", Value::Int(42));
        store.set("ratio", Value::Float(3.14));
        store.set("label", Value::Text("hello\nworld".into()));
        store.set("accent", Value::Color(0x7D39_EBFF));
        store.set("size", Value::Px(16.5));

        let snap = store.snapshot();

        let mut store2 = Store::new();
        let restored = store2.restore(&snap);

        assert_eq!(restored, 6, "all 6 keys should restore");
        assert_eq!(store2.get("flag"), Some(Value::Bool(true)));
        assert_eq!(store2.get("count"), Some(Value::Int(42)));
        assert_eq!(store2.get("label"), Some(Value::Text("hello\nworld".into())));
        assert_eq!(store2.get("accent"), Some(Value::Color(0x7D39_EBFF)));
        assert_eq!(store2.get("size"), Some(Value::Px(16.5)));
        // Float roundtrip — check approximate equality.
        if let Some(Value::Float(f)) = store2.get("ratio") {
            assert!((f - 3.14).abs() < 1e-9);
        } else {
            panic!("ratio should be a Float");
        }
    }

    #[test]
    fn restore_partial_snapshot() {
        // A snapshot that only has some keys; other keys in the store are untouched.
        let mut store = Store::new();
        store.set("existing", Value::Int(99));

        let partial = "newkey\tint\t7\n";
        let restored = store.restore(partial);

        assert_eq!(restored, 1);
        assert_eq!(store.get("newkey"), Some(Value::Int(7)));
        // Pre-existing key is untouched.
        assert_eq!(store.get("existing"), Some(Value::Int(99)));
    }

    #[test]
    fn snapshot_empty_store_is_empty_string() {
        let store = Store::new();
        let snap = store.snapshot();
        assert!(snap.is_empty(), "empty store snapshots to empty string");
    }

    /// The resolved document is plain literal IR that `uni_core::layout` accepts.
    #[test]
    fn resolved_doc_is_layout_ready() {
        let mut src = Document::new();
        let root = node(&mut src, "Column");
        set_root(&mut src, root);
        let forn = node(&mut src, "For");
        bind(&mut src, forn, "items", "n");
        child(&mut src, root, forn);
        let tmpl = node(&mut src, "Rect");
        src.apply_from(
            Origin::System,
            Mutation::SetProp {
                id: tmpl,
                key: "width".into(),
                value: Value::Px(50.0),
            },
        )
        .unwrap();
        src.apply_from(
            Origin::System,
            Mutation::SetProp {
                id: tmpl,
                key: "height".into(),
                value: Value::Px(30.0),
            },
        )
        .unwrap();
        child(&mut src, forn, tmpl);

        let mut store = Store::new();
        store.set("n", Value::Int(2));

        let resolved = resolve(&src, &store);
        let l = uni_core::layout(&resolved, (400.0, 400.0));
        // root + 2 rects laid out.
        assert_eq!(l.len(), 3);
    }
}
