//! # uni-ir — the DarkBlaze Uni-UI intermediate representation
//!
//! This is the keystone of the engine. Every declarative-UI frontend
//! (our native DSL, and the Slint / Flutter / SwiftUI importers) *lowers*
//! into this IR; every renderer *consumes* it. It is the one canonical,
//! opinionated description of a user interface.
//!
//! Three doctrine invariants are encoded directly in the types here:
//!
//! 1. **AI-malleable.** A [`Document`] is never edited in place by hidden
//!    code paths. It changes *only* by applying a [`Mutation`]. That makes
//!    the live UI a stream of discrete, replayable edits the AI companion
//!    can author, inspect, and reverse.
//!
//! 2. **Cowork dual-control.** Every applied edit carries an [`Origin`]
//!    (`Human` / `Ai` / `System`) and lands in an append-only audit log.
//!    Human and AI drive the *same* mutation surface — neither has a
//!    privileged back door. This is the cowork contract as a data type.
//!
//! 3. **Opinionated + normalizing.** Frontends do not mimic their source
//!    framework; they re-express it in this vocabulary. The IR is the
//!    canon, not a passthrough.
//!
//! v0 models the *core dialect* (node tree + properties + mutation stream).
//! Reactive bindings, layout-constraint nodes, and the MLIR-style frontend
//! dialects + lowering passes layer on top as later milestones.

use std::collections::{BTreeMap, HashMap};

/// Stable identity for a node within a [`Document`].
///
/// Identity is explicit (not positional) so the diffing/reconciliation a
/// reactive layer performs survives reordering, and so the AI can target a
/// specific node across edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct NodeId(pub u64);

/// A property value. Deliberately small in v0 — widened as frontends need it.
///
/// `Color` is packed `0xRRGGBBAA`. `Px` is logical pixels (device-independent);
/// physical-pixel resolution happens at render time, never in the IR.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Color(u32),
    Px(f32),
    /// A heterogeneous list of values — used by `For` binding to carry per-item
    /// data from the `Store` into the repeated subtree.
    List(Vec<Value>),
}

/// A named action invoked when an event fires on a node (rung 3: interaction).
///
/// An `Action` is *intent*, not execution: it names a handler and carries its
/// literal arguments. A later interaction/runtime layer maps `name` to actual
/// behavior. Keeping it declarative means a fired callback is just another
/// auditable record on the cowork surface — see [`Document::fire`].
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Action {
    pub name: String,
    pub args: Vec<Value>,
}

/// A dynamic property binding (rung 4: bindings).
///
/// `expr` is a state-key or expression string that a later reactive layer
/// resolves to a [`Value`]. Bindings live *alongside* literal [`Node::props`],
/// never replacing them: a node may carry both `props["width"]` (a literal)
/// and `bindings["width"]` (a dynamic source). Resolution order is the
/// reactive layer's concern, not the IR's.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Binding {
    pub expr: String,
}

/// A single element in the UI tree.
///
/// `kind` is a normalized element name in *our* vocabulary (e.g. `"Stack"`,
/// `"Text"`, `"Rect"`) — a Flutter `Column` or a SwiftUI `VStack` both lower
/// to the same `kind`, which is the point.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Node {
    pub kind: String,
    pub props: BTreeMap<String, Value>,
    pub children: Vec<NodeId>,
    pub parent: Option<NodeId>,
    /// Event name -> action to invoke. Empty by default, so existing
    /// `Node { kind, ..default() }` construction is unaffected.
    pub callbacks: BTreeMap<String, Action>,
    /// Property key -> dynamic binding. Empty by default.
    pub bindings: BTreeMap<String, Binding>,
}

/// Who authored an edit. The cowork-contract provenance tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Origin {
    Human,
    Ai,
    System,
}

/// A discrete change to a [`Document`]. The renderer-facing mutation stream
/// and the AI-malleability surface are one and the same enum.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Mutation {
    /// Create a detached node of the given `kind`.
    CreateNode { id: NodeId, kind: String },
    /// Make `id` the document root.
    SetRoot { id: NodeId },
    /// Set (or overwrite) a property on a node.
    SetProp {
        id: NodeId,
        key: String,
        value: Value,
    },
    /// Remove a property from a node.
    RemoveProp { id: NodeId, key: String },
    /// Append `child` to `parent`'s children (and set `child.parent`).
    AppendChild { parent: NodeId, child: NodeId },
    /// Detach `child` from `parent` (does not delete the node).
    RemoveChild { parent: NodeId, child: NodeId },
    /// Delete a node and detach it from its parent. Children are orphaned,
    /// not recursively deleted (callers compose deletes explicitly).
    RemoveNode { id: NodeId },
    /// Register (or overwrite) the [`Action`] fired for `event` on a node.
    SetCallback {
        id: NodeId,
        event: String,
        action: Action,
    },
    /// Bind (or overwrite) a dynamic [`Binding`] for `key` on a node.
    SetBinding {
        id: NodeId,
        key: String,
        binding: Binding,
    },
    /// Audited record that a callback was *fired* (not registered). Emitted by
    /// [`Document::fire`] so human- and AI-fired invocations are both
    /// attributable in the log. Applying it does not mutate the tree.
    Invoke { id: NodeId, event: String },
    /// Remove a registered callback for `event` from a node. The undo-inverse
    /// of a [`SetCallback`](Mutation::SetCallback) that added a fresh handler.
    RemoveCallback { id: NodeId, event: String },
    /// Remove a dynamic binding for `key` from a node. The undo-inverse of a
    /// [`SetBinding`](Mutation::SetBinding) that added a fresh binding.
    RemoveBinding { id: NodeId, key: String },
    /// Re-materialize a previously deleted node *whole* (kind, props, children,
    /// parent, callbacks, bindings) and, if it had been the root, restore that.
    /// This is the undo-inverse of [`RemoveNode`](Mutation::RemoveNode); it is
    /// not a primitive a frontend authors directly.
    Reconstruct {
        id: NodeId,
        node: Box<Node>,
        was_root: bool,
    },
}

/// An audited edit: a mutation plus who authored it.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Edit {
    pub origin: Origin,
    pub mutation: Mutation,
}

/// Errors from applying a malformed [`Mutation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrError {
    NoSuchNode(NodeId),
    DuplicateNode(NodeId),
    NotAChild { parent: NodeId, child: NodeId },
}

/// A complete UI tree plus its append-only edit history.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Document {
    nodes: HashMap<NodeId, Node>,
    root: Option<NodeId>,
    next_id: u64,
    log: Vec<Edit>,
}

impl Document {
    pub fn new() -> Self {
        Document::default()
    }

    /// Allocate a fresh, never-reused [`NodeId`].
    pub fn fresh_id(&mut self) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Apply one edit, recording it (and its [`Origin`]) to the audit log.
    ///
    /// The log is appended *only on success*, so a rejected edit never
    /// pollutes the history — the audit trail reflects what actually
    /// happened to the tree.
    pub fn apply(&mut self, edit: Edit) -> Result<(), IrError> {
        self.dispatch(&edit.mutation)?;
        self.log.push(edit);
        Ok(())
    }

    /// Convenience: apply a mutation attributed to a given origin.
    pub fn apply_from(&mut self, origin: Origin, mutation: Mutation) -> Result<(), IrError> {
        self.apply(Edit { origin, mutation })
    }

    fn dispatch(&mut self, m: &Mutation) -> Result<(), IrError> {
        match m {
            Mutation::CreateNode { id, kind } => {
                if self.nodes.contains_key(id) {
                    return Err(IrError::DuplicateNode(*id));
                }
                self.nodes.insert(
                    *id,
                    Node {
                        kind: kind.clone(),
                        ..Node::default()
                    },
                );
            }
            Mutation::SetRoot { id } => {
                self.expect(*id)?;
                self.root = Some(*id);
            }
            Mutation::SetProp { id, key, value } => {
                let node = self.nodes.get_mut(id).ok_or(IrError::NoSuchNode(*id))?;
                node.props.insert(key.clone(), value.clone());
            }
            Mutation::RemoveProp { id, key } => {
                let node = self.nodes.get_mut(id).ok_or(IrError::NoSuchNode(*id))?;
                node.props.remove(key);
            }
            Mutation::AppendChild { parent, child } => {
                self.expect(*parent)?;
                self.expect(*child)?;
                let p = self.nodes.get_mut(parent).unwrap();
                if !p.children.contains(child) {
                    p.children.push(*child);
                }
                self.nodes.get_mut(child).unwrap().parent = Some(*parent);
            }
            Mutation::RemoveChild { parent, child } => {
                self.expect(*parent)?;
                let p = self.nodes.get_mut(parent).unwrap();
                match p.children.iter().position(|c| c == child) {
                    Some(i) => {
                        p.children.remove(i);
                    }
                    None => {
                        return Err(IrError::NotAChild {
                            parent: *parent,
                            child: *child,
                        })
                    }
                }
                if let Some(c) = self.nodes.get_mut(child) {
                    c.parent = None;
                }
            }
            Mutation::RemoveNode { id } => {
                let removed = self.nodes.remove(id).ok_or(IrError::NoSuchNode(*id))?;
                if let Some(parent) = removed.parent {
                    if let Some(p) = self.nodes.get_mut(&parent) {
                        p.children.retain(|c| c != id);
                    }
                }
                if self.root == Some(*id) {
                    self.root = None;
                }
            }
            Mutation::SetCallback { id, event, action } => {
                let node = self.nodes.get_mut(id).ok_or(IrError::NoSuchNode(*id))?;
                node.callbacks.insert(event.clone(), action.clone());
            }
            Mutation::SetBinding { id, key, binding } => {
                let node = self.nodes.get_mut(id).ok_or(IrError::NoSuchNode(*id))?;
                node.bindings.insert(key.clone(), binding.clone());
            }
            Mutation::Invoke { id, event: _ } => {
                // Pure audit record: the node must exist, but the tree is
                // unchanged. The Edit's Origin (carried by apply) is what makes
                // a fired callback attributable.
                self.expect(*id)?;
            }
            Mutation::RemoveCallback { id, event } => {
                let node = self.nodes.get_mut(id).ok_or(IrError::NoSuchNode(*id))?;
                node.callbacks.remove(event);
            }
            Mutation::RemoveBinding { id, key } => {
                let node = self.nodes.get_mut(id).ok_or(IrError::NoSuchNode(*id))?;
                node.bindings.remove(key);
            }
            Mutation::Reconstruct { id, node, was_root } => {
                if self.nodes.contains_key(id) {
                    return Err(IrError::DuplicateNode(*id));
                }
                let parent = node.parent;
                self.nodes.insert(*id, (**node).clone());
                // Restore the back-reference on the parent's child list (which
                // RemoveNode had pruned), preserving append-order semantics.
                if let Some(p) = parent {
                    if let Some(pn) = self.nodes.get_mut(&p) {
                        if !pn.children.contains(id) {
                            pn.children.push(*id);
                        }
                    }
                }
                if *was_root {
                    self.root = Some(*id);
                }
            }
        }
        Ok(())
    }

    fn expect(&self, id: NodeId) -> Result<(), IrError> {
        if self.nodes.contains_key(&id) {
            Ok(())
        } else {
            Err(IrError::NoSuchNode(id))
        }
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }

    pub fn root(&self) -> Option<NodeId> {
        self.root
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The append-only edit history — the cowork audit trail.
    pub fn audit_log(&self) -> &[Edit] {
        &self.log
    }

    /// Fire the callback registered for `event` on node `id`.
    ///
    /// This is the rung-3 interaction surface, and it honors the cowork
    /// contract: a human-fired and an AI-fired invocation travel the *same*
    /// path. Firing records an audited [`Edit`] carrying the given [`Origin`]
    /// and a [`Mutation::Invoke`], so every invocation is attributable in the
    /// log — neither party has a back door.
    ///
    /// Returns a clone of the [`Action`] to run, or `None` if the node has no
    /// callback for `event` (in which case nothing is logged).
    pub fn fire(&mut self, id: NodeId, event: &str, origin: Origin) -> Option<Action> {
        let action = self.nodes.get(&id)?.callbacks.get(event)?.clone();
        // Same audited path for everyone; the Invoke is a pure record.
        self.apply_from(
            origin,
            Mutation::Invoke {
                id,
                event: event.to_string(),
            },
        )
        .ok()?;
        Some(action)
    }
}

// ---------------------------------------------------------------------------
// A4 — diff.
//
// Reconcile two documents into the mutation stream that turns `old` into `new`.
// Identity is by `NodeId` (the IR's whole point: identity is explicit, not
// positional), so a node that merely moved or restyled is matched, not
// recreated. The emitted stream, applied to a clone of `old`, reproduces `new`.
// ---------------------------------------------------------------------------

/// Compute the mutations that transform `old` into `new`.
///
/// Ordering is chosen so the stream applies cleanly start-to-finish:
/// new nodes are created first (so later `AppendChild`/`SetRoot` can reference
/// them), then per-node prop/child/callback/binding deltas, then the root, then
/// dropped nodes are removed last (after they've been detached).
pub fn diff(old: &Document, new: &Document) -> Vec<Mutation> {
    let mut out = Vec::new();

    // 1. Create nodes that are new (present in `new`, absent in `old`).
    //    Sorted for deterministic output.
    let mut new_ids: Vec<NodeId> = new.nodes.keys().copied().collect();
    new_ids.sort_by_key(|id| id.0);
    for id in &new_ids {
        if !old.nodes.contains_key(id) {
            out.push(Mutation::CreateNode {
                id: *id,
                kind: new.nodes[id].kind.clone(),
            });
        }
    }

    // 2. Per-surviving-and-new node, emit content deltas.
    for id in &new_ids {
        let nn = &new.nodes[id];
        let empty = Node::default();
        // For brand-new nodes the baseline is an empty node (CreateNode made a
        // default), so every populated field shows up as a delta.
        let on = old.nodes.get(id).unwrap_or(&empty);

        // Props: set changed/added, remove dropped.
        for (k, v) in &nn.props {
            if on.props.get(k) != Some(v) {
                out.push(Mutation::SetProp {
                    id: *id,
                    key: k.clone(),
                    value: v.clone(),
                });
            }
        }
        for k in on.props.keys() {
            if !nn.props.contains_key(k) {
                out.push(Mutation::RemoveProp {
                    id: *id,
                    key: k.clone(),
                });
            }
        }

        // Callbacks.
        for (e, a) in &nn.callbacks {
            if on.callbacks.get(e) != Some(a) {
                out.push(Mutation::SetCallback {
                    id: *id,
                    event: e.clone(),
                    action: a.clone(),
                });
            }
        }
        for e in on.callbacks.keys() {
            if !nn.callbacks.contains_key(e) {
                out.push(Mutation::RemoveCallback {
                    id: *id,
                    event: e.clone(),
                });
            }
        }

        // Bindings.
        for (k, b) in &nn.bindings {
            if on.bindings.get(k) != Some(b) {
                out.push(Mutation::SetBinding {
                    id: *id,
                    key: k.clone(),
                    binding: b.clone(),
                });
            }
        }
        for k in on.bindings.keys() {
            if !nn.bindings.contains_key(k) {
                out.push(Mutation::RemoveBinding {
                    id: *id,
                    key: k.clone(),
                });
            }
        }

        // Children: detach those dropped, append those added. Append-order in
        // `new` is preserved; we don't attempt minimal reorder in v0.
        for c in &on.children {
            if !nn.children.contains(c) {
                out.push(Mutation::RemoveChild {
                    parent: *id,
                    child: *c,
                });
            }
        }
        for c in &nn.children {
            if !on.children.contains(c) {
                out.push(Mutation::AppendChild {
                    parent: *id,
                    child: *c,
                });
            }
        }
    }

    // 3. Root change.
    if old.root != new.root {
        if let Some(r) = new.root {
            out.push(Mutation::SetRoot { id: r });
        }
    }

    // 4. Remove nodes dropped in `new` (now detached by the child deltas).
    let mut old_ids: Vec<NodeId> = old.nodes.keys().copied().collect();
    old_ids.sort_by_key(|id| id.0);
    for id in &old_ids {
        if !new.nodes.contains_key(id) {
            out.push(Mutation::RemoveNode { id: *id });
        }
    }

    out
}

/// A structural invariant violation found by [`Document::verify`].
///
/// These describe *internal incoherence* — a tree that the mutation API should
/// never produce, but which a hand-built or deserialized document might. Verify
/// is the IR's self-check: trust the wire, but verify the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrDefect {
    /// A node names a `parent` that does not exist in the document.
    DanglingParent { node: NodeId, parent: NodeId },
    /// A node lists a `child` id that does not exist in the document.
    ChildMissing { parent: NodeId, child: NodeId },
    /// A node lists `child` but the child's `parent` back-reference disagrees
    /// (or vice versa) — the parent/child link is one-directional.
    BackrefMismatch { parent: NodeId, child: NodeId },
    /// Following `parent` pointers (or the child DAG) loops back on itself.
    Cycle { node: NodeId },
    /// `root` names a node that does not exist.
    RootMissing { root: NodeId },
}

impl Document {
    /// Check every structural invariant and report *all* defects found (not
    /// just the first). `Ok(())` means the graph is internally coherent:
    ///
    /// - every listed child exists;
    /// - every `parent` pointer exists and is mirrored by the parent's child
    ///   list, and every child list entry is mirrored by the child's parent;
    /// - no node is its own ancestor (no parent-chain cycle);
    /// - the root, if set, exists.
    pub fn verify(&self) -> Result<(), Vec<IrDefect>> {
        let mut defects = Vec::new();

        // Root must exist.
        if let Some(r) = self.root {
            if !self.nodes.contains_key(&r) {
                defects.push(IrDefect::RootMissing { root: r });
            }
        }

        for (id, node) in &self.nodes {
            // Parent must exist, and must list us as one of its children.
            if let Some(p) = node.parent {
                match self.nodes.get(&p) {
                    None => defects.push(IrDefect::DanglingParent {
                        node: *id,
                        parent: p,
                    }),
                    Some(pn) if !pn.children.contains(id) => {
                        defects.push(IrDefect::BackrefMismatch {
                            parent: p,
                            child: *id,
                        })
                    }
                    _ => {}
                }
            }

            // Each child must exist, and must point back at us.
            for child in &node.children {
                match self.nodes.get(child) {
                    None => defects.push(IrDefect::ChildMissing {
                        parent: *id,
                        child: *child,
                    }),
                    Some(cn) if cn.parent != Some(*id) => defects.push(IrDefect::BackrefMismatch {
                        parent: *id,
                        child: *child,
                    }),
                    _ => {}
                }
            }
        }

        // Cycle detection via the parent chain: walk up from each node; if we
        // revisit a node we entered, there is a loop.
        for start in self.nodes.keys() {
            let mut seen = std::collections::HashSet::new();
            let mut cur = Some(*start);
            while let Some(c) = cur {
                if !seen.insert(c) {
                    defects.push(IrDefect::Cycle { node: c });
                    break;
                }
                cur = self.nodes.get(&c).and_then(|n| n.parent);
            }
        }

        // Cycle reports can duplicate across start nodes that share a loop;
        // collapse to a stable, de-duplicated set.
        defects.sort_by(defect_order);
        defects.dedup();

        if defects.is_empty() {
            Ok(())
        } else {
            Err(defects)
        }
    }
}

/// A total order over defects so [`Document::verify`] can de-duplicate the
/// (otherwise unordered, `HashMap`-driven) findings deterministically.
fn defect_order(a: &IrDefect, b: &IrDefect) -> std::cmp::Ordering {
    fn key(d: &IrDefect) -> (u8, u64, u64) {
        match d {
            IrDefect::DanglingParent { node, parent } => (0, node.0, parent.0),
            IrDefect::ChildMissing { parent, child } => (1, parent.0, child.0),
            IrDefect::BackrefMismatch { parent, child } => (2, parent.0, child.0),
            IrDefect::Cycle { node } => (3, node.0, 0),
            IrDefect::RootMissing { root } => (4, root.0, 0),
        }
    }
    key(a).cmp(&key(b))
}

// ---------------------------------------------------------------------------
// A2 — invertible mutations + undo.
//
// Every structural mutation has an inverse *relative to a concrete document
// state* (you cannot invert `RemoveProp` without knowing the value it removed).
// So `inverse` reads the live document, and `undo_last` applies the computed
// inverse as a *new audited edit* — undo is itself part of the cowork trail,
// never a hidden rollback.
// ---------------------------------------------------------------------------

impl Mutation {
    /// Compute the mutation that reverses `self` against the *current* `doc`
    /// state. Returns `None` when there is nothing to undo (e.g. [`Invoke`],
    /// which never touched the tree).
    ///
    /// [`Invoke`]: Mutation::Invoke
    pub fn inverse(&self, doc: &Document) -> Option<Mutation> {
        match self {
            // Undoing a creation is a deletion.
            Mutation::CreateNode { id, .. } => Some(Mutation::RemoveNode { id: *id }),

            // Root: restore whatever root was set before. None-safe — if there
            // was no prior root there is nothing meaningful to set back to, so
            // we report no inverse rather than inventing a SetRoot(None).
            Mutation::SetRoot { .. } => doc.root.map(|prev| Mutation::SetRoot { id: prev }),

            // Prop: restore the prior value, or remove it if it was absent.
            Mutation::SetProp { id, key, .. } => {
                let node = doc.nodes.get(id)?;
                Some(match node.props.get(key) {
                    Some(prev) => Mutation::SetProp {
                        id: *id,
                        key: key.clone(),
                        value: prev.clone(),
                    },
                    None => Mutation::RemoveProp {
                        id: *id,
                        key: key.clone(),
                    },
                })
            }
            Mutation::RemoveProp { id, key } => {
                let node = doc.nodes.get(id)?;
                // Only invertible if the prop is currently present.
                node.props.get(key).map(|prev| Mutation::SetProp {
                    id: *id,
                    key: key.clone(),
                    value: prev.clone(),
                })
            }

            // Children are mirror operations.
            Mutation::AppendChild { parent, child } => Some(Mutation::RemoveChild {
                parent: *parent,
                child: *child,
            }),
            Mutation::RemoveChild { parent, child } => Some(Mutation::AppendChild {
                parent: *parent,
                child: *child,
            }),

            // Deleting a node: rebuild it (kind, props, callbacks, bindings) and
            // re-attach it to its parent. Children of the removed node are
            // orphaned by `RemoveNode`, so there is nothing to restore there.
            Mutation::RemoveNode { id } => {
                let node = doc.nodes.get(id)?;
                Some(Mutation::Reconstruct {
                    id: *id,
                    node: Box::new(node.clone()),
                    was_root: doc.root == Some(*id),
                })
            }

            // Callback / binding: restore prior or remove.
            Mutation::SetCallback { id, event, .. } => {
                let node = doc.nodes.get(id)?;
                Some(match node.callbacks.get(event) {
                    Some(prev) => Mutation::SetCallback {
                        id: *id,
                        event: event.clone(),
                        action: prev.clone(),
                    },
                    None => Mutation::RemoveCallback {
                        id: *id,
                        event: event.clone(),
                    },
                })
            }
            Mutation::SetBinding { id, key, .. } => {
                let node = doc.nodes.get(id)?;
                Some(match node.bindings.get(key) {
                    Some(prev) => Mutation::SetBinding {
                        id: *id,
                        key: key.clone(),
                        binding: prev.clone(),
                    },
                    None => Mutation::RemoveBinding {
                        id: *id,
                        key: key.clone(),
                    },
                })
            }

            // These three exist only as undo-inverses; inverting them again
            // would be undo-of-undo, which `undo_last` handles via the normal
            // log, so we do not synthesize a deeper inverse here.
            Mutation::Reconstruct { .. }
            | Mutation::RemoveCallback { .. }
            | Mutation::RemoveBinding { .. } => None,

            // A fired callback never mutated the tree: nothing to undo.
            Mutation::Invoke { .. } => None,
        }
    }
}

impl Document {
    /// Undo the most recent *structural* edit (the last non-[`Invoke`] entry in
    /// the audit log), by computing its inverse against the current state and
    /// applying that inverse as a brand-new audited edit attributed to
    /// `origin`. Undo is therefore visible in the log like any other cowork
    /// action — there is no silent rewind.
    ///
    /// Returns `Ok(())` after a successful undo. If there is no undoable edit,
    /// or its inverse cannot be applied, the document is left untouched.
    ///
    /// [`Invoke`]: Mutation::Invoke
    pub fn undo_last(&mut self, origin: Origin) -> Result<(), IrError> {
        // Index of the last edit that actually changed the tree.
        let idx = match self
            .log
            .iter()
            .rposition(|e| !matches!(e.mutation, Mutation::Invoke { .. }))
        {
            Some(i) => i,
            None => return Ok(()), // nothing structural to undo
        };

        // The inverse of an edit is only well-defined against the state the
        // edit *acted on* — i.e. the document just before it. A `SetProp` that
        // overwrote a value cannot reveal the old value once it has landed, so
        // we rebuild that pre-edit state by replaying the prefix of the log and
        // compute the inverse there. (Replaying the audit log is the canonical
        // way to reconstruct any historical state — the IR is its own ledger.)
        let mut pre = Document::new();
        for e in &self.log[..idx] {
            // The prefix is a sequence of already-accepted edits, so dispatch
            // cannot fail; if it somehow did we simply skip undo.
            if pre.dispatch(&e.mutation).is_err() {
                return Ok(());
            }
        }

        let target = self.log[idx].mutation.clone();
        match target.inverse(&pre) {
            Some(inv) => self.apply_from(origin, inv),
            None => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// A1 — JSON wire format.
//
// A `Document` keys its nodes by `NodeId` in a `HashMap`. JSON object keys must
// be strings, and serde refuses a `HashMap<NodeId, _>` map without a string-key
// shim — so rather than fight the representation, we serialize through an
// explicit `Vec<(NodeId, Node)>` wire shape. This also pins a stable, ordered,
// human-diffable on-disk form independent of `HashMap` iteration order.
// ---------------------------------------------------------------------------

/// On-the-wire shadow of [`Document`]. Private: the only sanctioned crossing of
/// the process boundary is via [`Document::to_json`] / [`Document::from_json`].
#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
struct DocumentWire {
    nodes: Vec<(NodeId, Node)>,
    root: Option<NodeId>,
    next_id: u64,
    log: Vec<Edit>,
}

#[cfg(feature = "serde")]
impl Document {
    /// Serialize the whole document — tree, root, id counter, and the full
    /// audit log — to a JSON string. The log travels with the tree because the
    /// cowork provenance trail *is* part of the document's identity.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let mut nodes: Vec<(NodeId, Node)> =
            self.nodes.iter().map(|(id, n)| (*id, n.clone())).collect();
        // Stable ordering so the wire form is deterministic and diff-friendly.
        nodes.sort_by_key(|(id, _)| id.0);
        let wire = DocumentWire {
            nodes,
            root: self.root,
            next_id: self.next_id,
            log: self.log.clone(),
        };
        serde_json::to_string(&wire)
    }

    /// Reconstruct a document from its JSON wire form. The round trip is exact:
    /// `Document::from_json(&doc.to_json()?)? == doc`.
    pub fn from_json(s: &str) -> Result<Document, serde_json::Error> {
        let wire: DocumentWire = serde_json::from_str(s)?;
        Ok(Document {
            nodes: wire.nodes.into_iter().collect(),
            root: wire.root,
            next_id: wire.next_id,
            log: wire.log,
        })
    }
}

// ---------------------------------------------------------------------------
// B1 — AuditSink.
//
// The audit log lives *inside* the document (the cowork trail is part of the
// document's identity — see A1). A sink is the outward-facing mirror of that
// trail: a place edits are streamed to as they happen, for persistence,
// observability, or replication. Critically, a sink is NEVER stored on
// `Document` (that would add a non-derivable trait-object field and break A1's
// serde/PartialEq derives). It is wired in either by replaying the in-document
// log to it, or by editing through the thin `AuditedDocument` wrapper.
// ---------------------------------------------------------------------------

/// A destination for audited edits. Implementors persist, forward, or observe
/// each [`Edit`] as it is committed.
pub trait AuditSink {
    /// Record one committed edit. Called in commit order.
    fn record(&self, edit: &Edit);
}

/// A sink that drops everything. Useful as a default and in tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSink;

impl AuditSink for NoopSink {
    fn record(&self, _edit: &Edit) {}
}

impl Document {
    /// Stream the entire in-document audit log to `sink`, in order. The cheap,
    /// no-stored-state way to attach a sink after the fact: build/mutate the
    /// document normally, then mirror its trail outward.
    pub fn replay_to(&self, sink: &dyn AuditSink) {
        for edit in &self.log {
            sink.record(edit);
        }
    }
}

/// A thin live wrapper: edits applied through it land in the inner [`Document`]
/// *and* are mirrored to the borrowed sink as they commit. No trait object is
/// stored on the document itself — the sink is borrowed for the wrapper's life.
pub struct AuditedDocument<'a> {
    doc: Document,
    sink: &'a dyn AuditSink,
}

impl<'a> AuditedDocument<'a> {
    /// Wrap a (possibly pre-populated) document with a live sink.
    pub fn new(doc: Document, sink: &'a dyn AuditSink) -> Self {
        AuditedDocument { doc, sink }
    }

    /// Apply a mutation: commit it to the inner document, and on success mirror
    /// the resulting [`Edit`] to the sink (commit order, correct [`Origin`]).
    pub fn apply_from(&mut self, origin: Origin, mutation: Mutation) -> Result<(), IrError> {
        let edit = Edit { origin, mutation };
        self.doc.apply(edit.clone())?;
        self.sink.record(&edit);
        Ok(())
    }

    /// Allocate a fresh id from the inner document.
    pub fn fresh_id(&mut self) -> NodeId {
        self.doc.fresh_id()
    }

    /// Borrow the inner document (read-only).
    pub fn document(&self) -> &Document {
        &self.doc
    }

    /// Unwrap, returning the inner document and dropping the sink borrow.
    pub fn into_inner(self) -> Document {
        self.doc
    }
}

/// A sink that serializes each edit to one JSON line (JSONL) into any
/// [`std::io::Write`]. The canonical persisted form of the cowork trail.
#[cfg(feature = "serde")]
pub struct JsonlSink<W: std::io::Write> {
    out: std::cell::RefCell<W>,
}

#[cfg(feature = "serde")]
impl<W: std::io::Write> JsonlSink<W> {
    /// Wrap a writer (a file, a socket, a `Vec<u8>`/`String` buffer, …).
    pub fn new(out: W) -> Self {
        JsonlSink {
            out: std::cell::RefCell::new(out),
        }
    }

    /// Reclaim the underlying writer.
    pub fn into_inner(self) -> W {
        self.out.into_inner()
    }
}

#[cfg(feature = "serde")]
impl<W: std::io::Write> AuditSink for JsonlSink<W> {
    fn record(&self, edit: &Edit) {
        // Best-effort: a sink must not panic the edit path. Serialization of an
        // `Edit` cannot fail (all fields are plain data); write errors are the
        // caller's concern and surface on flush of the underlying writer.
        if let Ok(line) = serde_json::to_string(edit) {
            let mut w = self.out.borrow_mut();
            let _ = writeln!(w, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a small tree from a mix of human and AI edits, then assert both
    /// the resulting structure and that the audit log preserved provenance.
    #[test]
    fn human_and_ai_edit_the_same_tree() {
        let mut doc = Document::new();

        // Human creates the root stack.
        let root = doc.fresh_id();
        doc.apply_from(
            Origin::Human,
            Mutation::CreateNode {
                id: root,
                kind: "Stack".into(),
            },
        )
        .unwrap();
        doc.apply_from(Origin::Human, Mutation::SetRoot { id: root })
            .unwrap();

        // The AI companion adds a Text child and styles it (its own volition).
        let label = doc.fresh_id();
        doc.apply_from(
            Origin::Ai,
            Mutation::CreateNode {
                id: label,
                kind: "Text".into(),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::Ai,
            Mutation::SetProp {
                id: label,
                key: "content".into(),
                value: Value::Text("Hello".into()),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::Ai,
            Mutation::AppendChild {
                parent: root,
                child: label,
            },
        )
        .unwrap();

        // Structure.
        assert_eq!(doc.root(), Some(root));
        assert_eq!(doc.len(), 2);
        assert_eq!(doc.get(root).unwrap().children, vec![label]);
        assert_eq!(doc.get(label).unwrap().parent, Some(root));
        assert_eq!(
            doc.get(label).unwrap().props.get("content"),
            Some(&Value::Text("Hello".into()))
        );

        // Cowork audit trail: 5 edits, authored by the right parties.
        let log = doc.audit_log();
        assert_eq!(log.len(), 5);
        assert_eq!(log[0].origin, Origin::Human);
        assert_eq!(log[2].origin, Origin::Ai);
        assert!(log.iter().filter(|e| e.origin == Origin::Ai).count() == 3);
    }

    #[test]
    fn rejected_edits_do_not_pollute_the_log() {
        let mut doc = Document::new();
        let ghost = NodeId(999);
        let err = doc
            .apply_from(
                Origin::System,
                Mutation::SetProp {
                    id: ghost,
                    key: "x".into(),
                    value: Value::Px(1.0),
                },
            )
            .unwrap_err();
        assert_eq!(err, IrError::NoSuchNode(ghost));
        assert!(doc.audit_log().is_empty());
    }

    #[test]
    fn remove_child_detaches_without_deleting() {
        let mut doc = Document::new();
        let a = doc.fresh_id();
        let b = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id: a,
                kind: "Stack".into(),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id: b,
                kind: "Rect".into(),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::AppendChild {
                parent: a,
                child: b,
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::RemoveChild {
                parent: a,
                child: b,
            },
        )
        .unwrap();

        assert!(doc.get(a).unwrap().children.is_empty());
        assert_eq!(doc.get(b).unwrap().parent, None);
        assert!(doc.get(b).is_some(), "detached node still exists");
    }

    /// Register a callback, then fire it as both a human and the AI. Each fire
    /// returns the Action AND appends an audited Edit with the correct Origin —
    /// the cowork contract: same path, both attributable, no back door.
    #[test]
    fn fire_returns_action_and_audits_the_invocation() {
        let mut doc = Document::new();
        let btn = doc.fresh_id();
        doc.apply_from(
            Origin::Human,
            Mutation::CreateNode {
                id: btn,
                kind: "Button".into(),
            },
        )
        .unwrap();

        let action = Action {
            name: "submit".into(),
            args: vec![Value::Int(7)],
        };
        doc.apply_from(
            Origin::Human,
            Mutation::SetCallback {
                id: btn,
                event: "tap".into(),
                action: action.clone(),
            },
        )
        .unwrap();

        // It's stored on the node.
        assert_eq!(doc.get(btn).unwrap().callbacks.get("tap"), Some(&action));

        // Human fires it: gets the Action back.
        let fired = doc.fire(btn, "tap", Origin::Human);
        assert_eq!(fired, Some(action.clone()));

        // AI fires the very same callback: same path, also gets the Action.
        let fired_ai = doc.fire(btn, "tap", Origin::Ai);
        assert_eq!(fired_ai, Some(action.clone()));

        // Firing a non-existent event logs nothing and returns None.
        assert_eq!(doc.fire(btn, "nope", Origin::Ai), None);

        // The two invocations are both in the audit log with correct origins.
        let invokes: Vec<&Edit> = doc
            .audit_log()
            .iter()
            .filter(|e| matches!(e.mutation, Mutation::Invoke { .. }))
            .collect();
        assert_eq!(invokes.len(), 2);
        assert_eq!(invokes[0].origin, Origin::Human);
        assert_eq!(invokes[1].origin, Origin::Ai);
        assert_eq!(
            invokes[0].mutation,
            Mutation::Invoke {
                id: btn,
                event: "tap".into()
            }
        );
    }

    /// SetBinding stores a dynamic binding that reads back, and lives alongside
    /// (not replacing) any literal prop on the same key.
    #[test]
    fn set_binding_stores_and_reads_alongside_literals() {
        let mut doc = Document::new();
        let n = doc.fresh_id();
        doc.apply_from(
            Origin::Ai,
            Mutation::CreateNode {
                id: n,
                kind: "Rect".into(),
            },
        )
        .unwrap();

        // A literal width...
        doc.apply_from(
            Origin::Ai,
            Mutation::SetProp {
                id: n,
                key: "width".into(),
                value: Value::Px(100.0),
            },
        )
        .unwrap();
        // ...and a binding for the same key.
        let binding = Binding {
            expr: "state.width".into(),
        };
        doc.apply_from(
            Origin::Ai,
            Mutation::SetBinding {
                id: n,
                key: "width".into(),
                binding: binding.clone(),
            },
        )
        .unwrap();

        // Both coexist.
        assert_eq!(
            doc.get(n).unwrap().props.get("width"),
            Some(&Value::Px(100.0))
        );
        assert_eq!(doc.get(n).unwrap().bindings.get("width"), Some(&binding));
    }

    /// A2 — do then undo restores the prior state exactly, and the undo is
    /// itself recorded in the audit log (no silent rewind).
    #[test]
    fn undo_restores_state_and_is_logged() {
        let mut doc = Document::new();
        let n = doc.fresh_id();
        doc.apply_from(
            Origin::Human,
            Mutation::CreateNode {
                id: n,
                kind: "Rect".into(),
            },
        )
        .unwrap();
        doc.apply_from(Origin::Human, Mutation::SetRoot { id: n })
            .unwrap();

        // Snapshot before the edit we'll undo.
        let before = doc.clone();
        let log_len_before = doc.audit_log().len();

        // A new prop, then undo it -> the prop is gone again.
        doc.apply_from(
            Origin::Ai,
            Mutation::SetProp {
                id: n,
                key: "color".into(),
                value: Value::Color(0xFF0000FF),
            },
        )
        .unwrap();
        assert!(doc.get(n).unwrap().props.contains_key("color"));

        doc.undo_last(Origin::Human).unwrap();
        assert!(!doc.get(n).unwrap().props.contains_key("color"));

        // Tree matches the pre-edit snapshot...
        assert_eq!(doc.get(n).unwrap().props, before.get(n).unwrap().props);
        // ...but the log GREW by two (the SetProp and its undoing inverse):
        // undo is auditable, not a rollback.
        assert_eq!(doc.audit_log().len(), log_len_before + 2);
        assert_eq!(doc.audit_log().last().unwrap().origin, Origin::Human);
    }

    /// A2 — overwriting a prop then undoing restores the *prior value*, not
    /// absence; and undoing a child append detaches it again.
    #[test]
    fn undo_restores_prior_value_and_inverts_structure() {
        let mut doc = Document::new();
        let parent = doc.fresh_id();
        let child = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id: parent,
                kind: "Stack".into(),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id: child,
                kind: "Text".into(),
            },
        )
        .unwrap();

        // Establish an initial value, then overwrite it.
        doc.apply_from(
            Origin::Human,
            Mutation::SetProp {
                id: child,
                key: "size".into(),
                value: Value::Px(12.0),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::Ai,
            Mutation::SetProp {
                id: child,
                key: "size".into(),
                value: Value::Px(24.0),
            },
        )
        .unwrap();

        // Undo the overwrite -> prior value restored, not removed.
        doc.undo_last(Origin::Human).unwrap();
        assert_eq!(
            doc.get(child).unwrap().props.get("size"),
            Some(&Value::Px(12.0))
        );

        // Append then undo -> detached again.
        doc.apply_from(Origin::Ai, Mutation::AppendChild { parent, child })
            .unwrap();
        assert_eq!(doc.get(parent).unwrap().children, vec![child]);
        doc.undo_last(Origin::Ai).unwrap();
        assert!(doc.get(parent).unwrap().children.is_empty());
        assert_eq!(doc.get(child).unwrap().parent, None);
    }

    /// A2 — deleting a node and undoing reconstructs it whole and re-roots it.
    #[test]
    fn undo_reconstructs_a_removed_node() {
        let mut doc = Document::new();
        let n = doc.fresh_id();
        doc.apply_from(
            Origin::Human,
            Mutation::CreateNode {
                id: n,
                kind: "Rect".into(),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::Human,
            Mutation::SetProp {
                id: n,
                key: "w".into(),
                value: Value::Px(5.0),
            },
        )
        .unwrap();
        doc.apply_from(Origin::Human, Mutation::SetRoot { id: n })
            .unwrap();

        doc.apply_from(Origin::System, Mutation::RemoveNode { id: n })
            .unwrap();
        assert!(doc.get(n).is_none());
        assert_eq!(doc.root(), None);

        doc.undo_last(Origin::System).unwrap();
        let node = doc.get(n).expect("node reconstructed");
        assert_eq!(node.kind, "Rect");
        assert_eq!(node.props.get("w"), Some(&Value::Px(5.0)));
        assert_eq!(doc.root(), Some(n), "root restored on reconstruct");
    }

    /// A3 — a well-formed document verifies clean.
    #[test]
    fn verify_accepts_a_well_formed_document() {
        let mut doc = Document::new();
        let root = doc.fresh_id();
        let child = doc.fresh_id();
        doc.apply_from(
            Origin::Human,
            Mutation::CreateNode {
                id: root,
                kind: "Stack".into(),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::Human,
            Mutation::CreateNode {
                id: child,
                kind: "Text".into(),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::Human,
            Mutation::AppendChild {
                parent: root,
                child,
            },
        )
        .unwrap();
        doc.apply_from(Origin::Human, Mutation::SetRoot { id: root })
            .unwrap();

        assert_eq!(doc.verify(), Ok(()));
    }

    /// A3 — a hand-corrupted graph reports the precise defect. We reach past the
    /// mutation API (same-crate test) to forge incoherence the API forbids.
    #[test]
    fn verify_catches_hand_corruption() {
        // 1. A child id that doesn't exist.
        let mut doc = Document::new();
        let a = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id: a,
                kind: "Stack".into(),
            },
        )
        .unwrap();
        doc.nodes.get_mut(&a).unwrap().children.push(NodeId(404));
        assert_eq!(
            doc.verify(),
            Err(vec![IrDefect::ChildMissing {
                parent: a,
                child: NodeId(404)
            }])
        );

        // 2. A parent back-reference that the parent does not mirror.
        let mut doc = Document::new();
        let p = doc.fresh_id();
        let c = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id: p,
                kind: "Stack".into(),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id: c,
                kind: "Rect".into(),
            },
        )
        .unwrap();
        doc.nodes.get_mut(&c).unwrap().parent = Some(p); // p does NOT list c
        assert_eq!(
            doc.verify(),
            Err(vec![IrDefect::BackrefMismatch {
                parent: p,
                child: c
            }])
        );

        // 3. A dangling parent pointer.
        let mut doc = Document::new();
        let n = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id: n,
                kind: "Rect".into(),
            },
        )
        .unwrap();
        doc.nodes.get_mut(&n).unwrap().parent = Some(NodeId(777));
        assert_eq!(
            doc.verify(),
            Err(vec![IrDefect::DanglingParent {
                node: n,
                parent: NodeId(777)
            }])
        );

        // 4. A missing root.
        let mut doc = Document::new();
        doc.root = Some(NodeId(13));
        assert_eq!(
            doc.verify(),
            Err(vec![IrDefect::RootMissing { root: NodeId(13) }])
        );

        // 5. A parent-chain cycle: x is its own parent.
        let mut doc = Document::new();
        let x = doc.fresh_id();
        doc.apply_from(
            Origin::System,
            Mutation::CreateNode {
                id: x,
                kind: "Stack".into(),
            },
        )
        .unwrap();
        doc.nodes.get_mut(&x).unwrap().parent = Some(x);
        doc.nodes.get_mut(&x).unwrap().children.push(x);
        let defects = doc.verify().unwrap_err();
        assert!(
            defects.contains(&IrDefect::Cycle { node: x }),
            "cycle reported: {defects:?}"
        );
    }

    /// B1 — edits applied through `AuditedDocument` land in the sink in commit
    /// order with the correct `Origin`; and `replay_to` mirrors an existing
    /// document's whole trail.
    #[test]
    fn audit_sink_receives_edits_in_order_with_origin() {
        use std::cell::RefCell;

        // A test sink that records (origin, mutation-tag) for each edit.
        struct Collect {
            seen: RefCell<Vec<(Origin, String)>>,
        }
        impl AuditSink for Collect {
            fn record(&self, edit: &Edit) {
                let tag = match &edit.mutation {
                    Mutation::CreateNode { .. } => "CreateNode",
                    Mutation::SetRoot { .. } => "SetRoot",
                    Mutation::SetProp { .. } => "SetProp",
                    Mutation::AppendChild { .. } => "AppendChild",
                    _ => "other",
                };
                self.seen.borrow_mut().push((edit.origin, tag.to_string()));
            }
        }
        let sink = Collect {
            seen: RefCell::new(Vec::new()),
        };

        // Live path: edit THROUGH the wrapper.
        let mut adoc = AuditedDocument::new(Document::new(), &sink);
        let root = adoc.fresh_id();
        adoc.apply_from(
            Origin::Human,
            Mutation::CreateNode {
                id: root,
                kind: "Stack".into(),
            },
        )
        .unwrap();
        adoc.apply_from(Origin::Human, Mutation::SetRoot { id: root })
            .unwrap();
        let label = adoc.fresh_id();
        adoc.apply_from(
            Origin::Ai,
            Mutation::CreateNode {
                id: label,
                kind: "Text".into(),
            },
        )
        .unwrap();
        adoc.apply_from(
            Origin::Ai,
            Mutation::AppendChild {
                parent: root,
                child: label,
            },
        )
        .unwrap();

        // A rejected edit must NOT reach the sink (apply fails before record).
        let err = adoc.apply_from(
            Origin::System,
            Mutation::SetProp {
                id: NodeId(9999),
                key: "x".into(),
                value: Value::Int(0),
            },
        );
        assert!(err.is_err());

        assert_eq!(
            *sink.seen.borrow(),
            vec![
                (Origin::Human, "CreateNode".to_string()),
                (Origin::Human, "SetRoot".to_string()),
                (Origin::Ai, "CreateNode".to_string()),
                (Origin::Ai, "AppendChild".to_string()),
            ]
        );

        // The inner document is real and consistent.
        let doc = adoc.into_inner();
        assert_eq!(doc.len(), 2);
        assert_eq!(doc.verify(), Ok(()));

        // replay_to mirrors the SAME four edits (the rejected one never logged).
        let replay = Collect {
            seen: RefCell::new(Vec::new()),
        };
        doc.replay_to(&replay);
        assert_eq!(replay.seen.borrow().len(), 4);

        // NoopSink is inert.
        doc.replay_to(&NoopSink);
    }

    /// B1 — `JsonlSink` writes one JSON line per edit, decodable back to `Edit`.
    #[cfg(feature = "serde")]
    #[test]
    fn jsonl_sink_writes_one_json_line_per_edit() {
        let mut doc = Document::new();
        let a = doc.fresh_id();
        doc.apply_from(
            Origin::Human,
            Mutation::CreateNode {
                id: a,
                kind: "Stack".into(),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::Ai,
            Mutation::SetProp {
                id: a,
                key: "w".into(),
                value: Value::Px(10.0),
            },
        )
        .unwrap();

        let sink = JsonlSink::new(Vec::<u8>::new());
        doc.replay_to(&sink);
        let bytes = sink.into_inner();
        let text = String::from_utf8(bytes).unwrap();

        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per edit");
        // Each line round-trips back to the original Edit, in order.
        let e0: Edit = serde_json::from_str(lines[0]).unwrap();
        let e1: Edit = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(e0, doc.audit_log()[0]);
        assert_eq!(e1, doc.audit_log()[1]);
        assert_eq!(e0.origin, Origin::Human);
        assert_eq!(e1.origin, Origin::Ai);
    }

    /// A4 — `diff(old, new)` applied to a clone of `old` reproduces `new`
    /// exactly, across creates, removes, prop/child/callback/binding deltas,
    /// and a root change.
    #[test]
    fn diff_then_apply_reproduces_new() {
        // --- OLD document ---
        let mut old = Document::new();
        let root = old.fresh_id();
        let keep = old.fresh_id();
        let drop = old.fresh_id();
        old.apply_from(
            Origin::Human,
            Mutation::CreateNode {
                id: root,
                kind: "Stack".into(),
            },
        )
        .unwrap();
        old.apply_from(
            Origin::Human,
            Mutation::CreateNode {
                id: keep,
                kind: "Text".into(),
            },
        )
        .unwrap();
        old.apply_from(
            Origin::Human,
            Mutation::CreateNode {
                id: drop,
                kind: "Rect".into(),
            },
        )
        .unwrap();
        old.apply_from(
            Origin::Human,
            Mutation::SetProp {
                id: keep,
                key: "size".into(),
                value: Value::Px(12.0),
            },
        )
        .unwrap();
        old.apply_from(
            Origin::Human,
            Mutation::SetCallback {
                id: keep,
                event: "tap".into(),
                action: Action {
                    name: "old".into(),
                    args: vec![],
                },
            },
        )
        .unwrap();
        old.apply_from(
            Origin::Human,
            Mutation::AppendChild {
                parent: root,
                child: keep,
            },
        )
        .unwrap();
        old.apply_from(
            Origin::Human,
            Mutation::AppendChild {
                parent: root,
                child: drop,
            },
        )
        .unwrap();
        old.apply_from(Origin::Human, Mutation::SetRoot { id: root })
            .unwrap();

        // --- NEW document: derived from old, then diverged ---
        let mut new = old.clone();
        // drop the `drop` node (detach then remove)
        new.apply_from(
            Origin::Ai,
            Mutation::RemoveChild {
                parent: root,
                child: drop,
            },
        )
        .unwrap();
        new.apply_from(Origin::Ai, Mutation::RemoveNode { id: drop })
            .unwrap();
        // change a prop on keep, drop its callback, add a binding
        new.apply_from(
            Origin::Ai,
            Mutation::SetProp {
                id: keep,
                key: "size".into(),
                value: Value::Px(24.0),
            },
        )
        .unwrap();
        new.apply_from(
            Origin::Ai,
            Mutation::RemoveCallback {
                id: keep,
                event: "tap".into(),
            },
        )
        .unwrap();
        new.apply_from(
            Origin::Ai,
            Mutation::SetBinding {
                id: keep,
                key: "size".into(),
                binding: Binding {
                    expr: "state.s".into(),
                },
            },
        )
        .unwrap();
        // add a brand-new node and re-root onto it
        let fresh = new.fresh_id();
        new.apply_from(
            Origin::Ai,
            Mutation::CreateNode {
                id: fresh,
                kind: "Panel".into(),
            },
        )
        .unwrap();
        new.apply_from(
            Origin::Ai,
            Mutation::SetProp {
                id: fresh,
                key: "title".into(),
                value: Value::Text("hi".into()),
            },
        )
        .unwrap();
        new.apply_from(
            Origin::Ai,
            Mutation::AppendChild {
                parent: fresh,
                child: root,
            },
        )
        .unwrap();
        new.apply_from(Origin::Ai, Mutation::SetRoot { id: fresh })
            .unwrap();

        // --- The contract: diff applied onto a clone of old == new (trees). ---
        let muts = diff(&old, &new);
        let mut rebuilt = old.clone();
        for m in muts {
            rebuilt
                .apply_from(Origin::System, m)
                .expect("diff mutation applies cleanly");
        }

        // Compare the structural state (nodes + root); the audit logs differ by
        // construction, which is expected — diff reproduces the *tree*, not the
        // history that produced it.
        assert_eq!(rebuilt.root(), new.root());
        assert_eq!(rebuilt.len(), new.len());
        for id in [root, keep, fresh] {
            assert_eq!(rebuilt.get(id), new.get(id), "node {id:?} mismatch");
        }
        assert!(rebuilt.get(drop).is_none(), "dropped node gone");
        assert_eq!(rebuilt.verify(), Ok(()));
    }

    /// A1 — a human+AI document survives a JSON round trip byte-for-meaning:
    /// the reconstructed document equals the original, audit log included.
    #[cfg(feature = "serde")]
    #[test]
    fn json_round_trip_preserves_document_and_log() {
        let mut doc = Document::new();

        let root = doc.fresh_id();
        doc.apply_from(
            Origin::Human,
            Mutation::CreateNode {
                id: root,
                kind: "Stack".into(),
            },
        )
        .unwrap();
        doc.apply_from(Origin::Human, Mutation::SetRoot { id: root })
            .unwrap();

        let label = doc.fresh_id();
        doc.apply_from(
            Origin::Ai,
            Mutation::CreateNode {
                id: label,
                kind: "Text".into(),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::Ai,
            Mutation::SetProp {
                id: label,
                key: "content".into(),
                value: Value::Text("Hello".into()),
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::Ai,
            Mutation::SetBinding {
                id: label,
                key: "content".into(),
                binding: Binding {
                    expr: "state.greeting".into(),
                },
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::Ai,
            Mutation::SetCallback {
                id: label,
                event: "tap".into(),
                action: Action {
                    name: "noop".into(),
                    args: vec![Value::Int(1), Value::Bool(true)],
                },
            },
        )
        .unwrap();
        doc.apply_from(
            Origin::System,
            Mutation::AppendChild {
                parent: root,
                child: label,
            },
        )
        .unwrap();

        let json = doc.to_json().unwrap();
        let back = Document::from_json(&json).unwrap();

        assert_eq!(back, doc, "round-tripped document must equal the original");
        assert_eq!(back.audit_log().len(), doc.audit_log().len());
        assert_eq!(back.audit_log().len(), 7);
        assert_eq!(back.root(), Some(root));
    }
}
