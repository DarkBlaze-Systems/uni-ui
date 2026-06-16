<!--
  UNI-IR-SPEC.md — the canonical description of the Uni-UI intermediate
  representation. Derived from crates/uni-ir/src/lib.rs; not aspirational.
  Where this prose and the source disagree, the source wins.
-->

# Uni-IR Specification (v0)

The intermediate representation is the keystone of the engine. Every
declarative-UI frontend — our native `.uni` DSL and the Slint / Flutter /
SwiftUI importers — *lowers* into this IR; every renderer *consumes* it. It is
the one canonical, opinionated description of a user interface.

This document is the IR vocabulary: the node model, property `Value` types, the
`Mutation` enum and its semantics, the `Origin` / audit model, and the JSON wire
format. It tracks `crates/uni-ir/src/lib.rs` exactly — read that file when in
doubt.

## Doctrine encoded in the types

Three invariants are not conventions; they are spelled into the types.

1. **AI-malleable.** A `Document` is never edited in place by hidden code paths.
   It changes *only* by applying a `Mutation`. The live UI is therefore a stream
   of discrete, replayable edits the AI companion can author, inspect, and
   reverse.
2. **Cowork dual-control.** Every applied edit carries an `Origin`
   (`Human` / `Ai` / `System`) and lands in an append-only audit log. Human and
   AI drive the *same* mutation surface — neither has a privileged back door.
3. **Opinionated + normalizing.** Frontends do not mimic their source framework;
   they re-express it in this vocabulary. A Flutter `Column` and a SwiftUI
   `VStack` both lower to the same `kind`. The IR is the canon, not a passthrough.

v0 models the core dialect: node tree + properties + mutation stream, plus
callbacks (rung 3) and bindings (rung 4). Reactive evaluation, layout-constraint
nodes, and the MLIR-style frontend dialects layer on top later.

---

## 1. Node model

### `NodeId`

```rust
pub struct NodeId(pub u64);
```

Stable, explicit identity for a node within a `Document`. Identity is *not*
positional, so reconciliation survives reordering and the AI can target a
specific node across edits. Ids are allocated by `Document::fresh_id`, which
hands out monotonically increasing, never-reused values.

### `Node`

```rust
pub struct Node {
    pub kind: String,
    pub props: BTreeMap<String, Value>,
    pub children: Vec<NodeId>,
    pub parent: Option<NodeId>,
    pub callbacks: BTreeMap<String, Action>,
    pub bindings: BTreeMap<String, Binding>,
}
```

A single element in the UI tree.

- **`kind`** — a normalized element name in *our* vocabulary (e.g. `"Stack"`,
  `"Text"`, `"Rect"`, `"Button"`, `"Panel"`, and the synthetic control-flow
  kinds `"If"` / `"For"`). The IR does not enumerate a closed kind set; `kind`
  is a free string that frontends agree on.
- **`props`** — literal property values, keyed by name. A `BTreeMap`, so
  iteration (and the wire form) is deterministically ordered by key.
- **`children`** — ordered list of child ids. Append-order is meaningful and
  preserved.
- **`parent`** — back-reference to the owning node, or `None` for a detached /
  root node. Kept in sync with the parent's `children` by the mutation dispatch.
- **`callbacks`** — event name → `Action` to invoke (rung 3). Empty by default.
- **`bindings`** — property key → dynamic `Binding` (rung 4). Empty by default.

`Node` derives `Default`, so `Node { kind, ..Default::default() }` is the
canonical way to build a fresh node — every collection field starts empty.

### Property keys

Property names are free strings agreed by frontend and renderer; the IR does not
validate them. Conventionally seen in the source and tests: `padding`,
`background`, `content`, `size`, `color`, `width`, `height`, `flex`, `ratio`,
`visible`, `title`. A node may carry a literal `props["width"]` **and** a dynamic
`bindings["width"]` for the same key simultaneously; resolution order is the
reactive layer's concern, not the IR's.

---

## 2. `Value` — property value types

```rust
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Color(u32),
    Px(f32),
    List(Vec<Value>),
}
```

Deliberately small in v0; widened as frontends need it.

| Variant     | Rust payload   | Meaning |
|-------------|----------------|---------|
| `Bool`      | `bool`         | A boolean flag. |
| `Int`       | `i64`          | A signed integer. |
| `Float`     | `f64`          | A floating-point scalar. |
| `Text`      | `String`       | A UTF-8 string (labels, content). |
| `Color`     | `u32`          | Packed `0xRRGGBBAA`. `#RRGGBB` source expands to alpha `0xFF`. |
| `Px`        | `f32`          | Logical (device-independent) pixels. Physical-pixel resolution happens at render time, never in the IR. |
| `List`      | `Vec<Value>`   | A heterogeneous list; used by a `For` binding to carry per-item data from the store into the repeated subtree. |

`Value` is `Clone + PartialEq`. Because `f64`/`f32` are involved it is **not**
`Eq` or `Hash`.

---

## 3. `Action` and `Binding`

### `Action` (rung 3 — interaction)

```rust
pub struct Action {
    pub name: String,
    pub args: Vec<Value>,
}
```

Named intent, not execution: it identifies a handler by `name` and carries its
literal `args`. A later interaction/runtime layer maps `name` to actual behavior.
Keeping it declarative means a fired callback is just another auditable record on
the cowork surface (see `Document::fire`).

### `Binding` (rung 4 — dynamic props)

```rust
pub struct Binding {
    pub expr: String,
}
```

A dynamic property binding. `expr` is a state-key or expression string (e.g.
`"state.width"`, `"theme.accent"`) that a later reactive layer resolves to a
`Value`. Bindings live *alongside* literal props, never replacing them.

---

## 4. `Mutation` — the edit vocabulary

```rust
pub enum Mutation { /* ... */ }
```

The renderer-facing mutation stream and the AI-malleability surface are one and
the same enum. Every change to a `Document` is one of these.

| Variant | Fields | Semantics |
|---------|--------|-----------|
| `CreateNode` | `{ id, kind }` | Create a detached node of `kind`. Errors `DuplicateNode` if `id` already exists. |
| `SetRoot` | `{ id }` | Make `id` the document root. `id` must already exist (`NoSuchNode` otherwise). |
| `SetProp` | `{ id, key, value }` | Set or overwrite a literal property. |
| `RemoveProp` | `{ id, key }` | Remove a property (no-op if absent). |
| `AppendChild` | `{ parent, child }` | Append `child` to `parent.children` (idempotent — won't duplicate) and set `child.parent = Some(parent)`. Both must exist. |
| `RemoveChild` | `{ parent, child }` | Detach `child` from `parent` and clear `child.parent`. Does **not** delete the node. Errors `NotAChild` if `child` isn't in `parent.children`. |
| `RemoveNode` | `{ id }` | Delete a node and prune it from its parent's child list. Children are **orphaned, not recursively deleted** — callers compose deletes explicitly. Clears `root` if it was the root. |
| `SetCallback` | `{ id, event, action }` | Register or overwrite the `Action` fired for `event`. |
| `SetBinding` | `{ id, key, binding }` | Bind or overwrite a dynamic `Binding` for `key`. |
| `Invoke` | `{ id, event }` | **Audit-only.** Records that a callback was *fired* (not registered). `id` must exist, but the tree is unchanged. Emitted by `Document::fire`. |
| `RemoveCallback` | `{ id, event }` | Remove a registered callback. The undo-inverse of a `SetCallback` that added a fresh handler. |
| `RemoveBinding` | `{ id, key }` | Remove a dynamic binding. The undo-inverse of a `SetBinding` that added a fresh binding. |
| `Reconstruct` | `{ id, node, was_root }` | Re-materialize a previously deleted node *whole* (kind, props, children, parent, callbacks, bindings) and restore root if `was_root`. The undo-inverse of `RemoveNode`; **not a primitive a frontend authors directly.** Errors `DuplicateNode` if `id` exists. |

### Application errors

```rust
pub enum IrError {
    NoSuchNode(NodeId),
    DuplicateNode(NodeId),
    NotAChild { parent: NodeId, child: NodeId },
}
```

`Document::apply` dispatches the mutation first and **appends to the log only on
success**, so a rejected edit never pollutes the history — the audit trail
reflects what actually happened to the tree.

### Invertibility (undo)

`Mutation::inverse(&self, doc) -> Option<Mutation>` computes the reverse of an
edit *against a concrete document state* (you cannot invert `RemoveProp` without
knowing the value removed). Highlights:

- `CreateNode` ⇄ `RemoveNode`; `AppendChild` ⇄ `RemoveChild`.
- `SetProp` inverts to restoring the prior value, or `RemoveProp` if the key was
  absent. Same shape for `SetCallback` / `SetBinding`.
- `RemoveNode` inverts to `Reconstruct` (rebuild whole + re-attach + re-root).
- `SetRoot` inverts to `SetRoot` of the previous root, or `None` if there was no
  prior root.
- `Invoke` has no inverse (it never touched the tree); `Reconstruct`,
  `RemoveCallback`, and `RemoveBinding` return `None` (they exist only *as*
  inverses).

`Document::undo_last(origin)` reconstructs the pre-edit state by replaying the
log prefix, computes the inverse there, and applies it **as a brand-new audited
edit** attributed to `origin`. Undo is visible in the log like any other cowork
action — there is no silent rewind.

---

## 5. `Origin` and the audit model

```rust
pub enum Origin { Human, Ai, System }

pub struct Edit {
    pub origin: Origin,
    pub mutation: Mutation,
}
```

Every committed change is an `Edit`: a `Mutation` plus the `Origin` that
authored it. `System` is the provenance the DSL/importers stamp on the
machine-lowered prefix; `Human` and `Ai` are the two cowork drivers.

### `Document`

```rust
pub struct Document { /* nodes, root, next_id, log — all private */ }
```

A complete UI tree plus its append-only edit history. Fields are private; the
sanctioned surface is:

- `new()`, `fresh_id()` — construct; allocate a never-reused `NodeId`.
- `apply(Edit)` / `apply_from(Origin, Mutation)` — commit one edit, logging on
  success.
- `get(id)`, `root()`, `len()`, `is_empty()` — read the tree.
- `audit_log() -> &[Edit]` — the append-only cowork trail.
- `fire(id, event, origin) -> Option<Action>` — the rung-3 interaction surface.
  Human- and AI-fired invocations travel the *same* path: each fire records an
  audited `Edit` carrying the `Origin` and a `Mutation::Invoke`, then returns a
  clone of the `Action` to run (or `None`, logging nothing, if no callback is
  registered).
- `verify()` — structural self-check (below).
- `to_json()` / `from_json()` — wire crossing (§7, requires the `serde` feature).

### Structural verification

```rust
pub enum IrDefect {
    DanglingParent { node, parent },
    ChildMissing { parent, child },
    BackrefMismatch { parent, child },
    Cycle { node },
    RootMissing { root },
}
```

`Document::verify() -> Result<(), Vec<IrDefect>>` reports *all* defects found
(not just the first), de-duplicated in a stable order. `Ok(())` means the graph
is internally coherent: every listed child exists; every `parent` pointer exists
and is mirrored by the parent's child list and vice versa; no node is its own
ancestor (no parent-chain cycle); and the root, if set, exists. These describe
incoherence the mutation API never produces, but a hand-built or deserialized
document might — *trust the wire, but verify the graph.*

### Diffing / reconciliation

`diff(old, new) -> Vec<Mutation>` computes the mutation stream that turns `old`
into `new`. Matching is by `NodeId`, so a node that merely moved or restyled is
matched, not recreated. The emitted stream, applied to a clone of `old`,
reproduces `new`'s *tree* (not its history). Emission order is fixed so the
stream applies cleanly start-to-finish: create new nodes, then per-node
prop/callback/binding/child deltas, then a root change, then remove dropped
nodes last (after they've been detached).

### Audit sinks (outward mirror of the trail)

The audit log lives *inside* the document — it is part of the document's
identity. A sink is the outward-facing mirror, never a stored field on
`Document` (that would break the serde / `PartialEq` derives).

```rust
pub trait AuditSink { fn record(&self, edit: &Edit); }
pub struct NoopSink;                    // drops everything
pub struct AuditedDocument<'a> { /* doc + borrowed &dyn AuditSink */ }
pub struct JsonlSink<W: Write> { /* one JSON line per edit (serde) */ }
```

- `Document::replay_to(sink)` streams the whole in-document log to a sink in
  order — the cheap, no-stored-state way to attach a sink after the fact.
- `AuditedDocument::new(doc, sink)` edits *through* a wrapper: each committed
  edit lands in the inner document **and** is mirrored to the borrowed sink in
  commit order with the correct `Origin`. A rejected edit reaches neither.
- `JsonlSink` (feature `serde`) serializes each `Edit` to one JSON line — the
  canonical persisted form of the cowork trail.

---

## 6. Origin/audit lifecycle (worked sketch)

A human creates the root stack; the AI companion adds and styles a `Text` child.
Each call is `apply_from(origin, mutation)`, so the resulting `audit_log()` is an
ordered, attributable record:

```text
[ Human  CreateNode { id: 0, kind: "Stack" }
  Human  SetRoot    { id: 0 }
  Ai     CreateNode { id: 1, kind: "Text" }
  Ai     SetProp    { id: 1, key: "content", value: Text("Hello") }
  Ai     AppendChild{ parent: 0, child: 1 } ]
```

Five edits, three authored by `Ai`. Firing a callback later appends an
`Invoke` edit with the firing `Origin`; undoing appends the computed inverse with
the undoer's `Origin`. Nothing rewinds silently.

---

## 7. JSON wire format (feature `serde`)

`Document` keys its nodes by `NodeId` in a `HashMap`. JSON object keys must be
strings, so rather than fight a string-key shim the document serializes through
an explicit ordered `Vec<(NodeId, Node)>` wire shape, sorted by id. This pins a
stable, human-diffable on-disk form independent of `HashMap` iteration order.

```rust
// Private — the only sanctioned crossing is to_json / from_json.
struct DocumentWire {
    nodes: Vec<(NodeId, Node)>,
    root: Option<NodeId>,
    next_id: u64,
    log: Vec<Edit>,
}
```

- `Document::to_json() -> Result<String, serde_json::Error>` — serializes the
  whole document: tree, root, the `next_id` counter, and the **full audit log**.
  The log travels with the tree because the cowork provenance trail *is* part of
  the document's identity.
- `Document::from_json(&str) -> Result<Document, serde_json::Error>` — the round
  trip is exact: `Document::from_json(&doc.to_json()?)? == doc`.

### Variant encoding

Types tagged `#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]`
use serde's default representations:

- `NodeId(pub u64)` — a newtype tuple struct → a bare JSON number, e.g. `0`.
- `Value`, `Mutation`, `Origin`, `IrDefect` — externally tagged enums. A unit
  variant is its name as a string (`"Human"`); a struct/newtype variant is a
  single-key object whose key is the variant name.
- `Node`, `Action`, `Binding`, `Edit` — plain objects keyed by field name. The
  `BTreeMap` fields (`props`, `callbacks`, `bindings`) serialize as JSON objects
  with key-sorted, deterministic ordering.

### Illustrative shape

A `Value::Text("Hello")` encodes as:

```json
{ "Text": "Hello" }
```

An `Edit` (one log entry) encodes as:

```json
{
  "origin": "Ai",
  "mutation": {
    "SetProp": { "id": 1, "key": "content", "value": { "Text": "Hello" } }
  }
}
```

A whole document (the `DocumentWire` shape) is:

```json
{
  "nodes": [
    [0, { "kind": "Stack", "props": {}, "children": [1], "parent": null,
          "callbacks": {}, "bindings": {} }],
    [1, { "kind": "Text", "props": { "content": { "Text": "Hello" } },
          "children": [], "parent": 0, "callbacks": {}, "bindings": {} }]
  ],
  "root": 0,
  "next_id": 2,
  "log": [ /* the ordered Vec<Edit> shown above */ ]
}
```

`JsonlSink` reuses the per-`Edit` encoding above, one compact line per edit.

---

## 8. Source of truth

This spec is prose over `crates/uni-ir/src/lib.rs`. The types, mutation
semantics, error/defect enums, and wire shape there are authoritative; this
document is a reading aid, not a second definition. See `docs/UNI-LANG.md` for
the `.uni` surface language that lowers into this IR.
