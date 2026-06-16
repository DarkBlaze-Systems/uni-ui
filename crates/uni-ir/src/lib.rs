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
pub struct NodeId(pub u64);

/// A property value. Deliberately small in v0 — widened as frontends need it.
///
/// `Color` is packed `0xRRGGBBAA`. `Px` is logical pixels (device-independent);
/// physical-pixel resolution happens at render time, never in the IR.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Color(u32),
    Px(f32),
}

/// A named action invoked when an event fires on a node (rung 3: interaction).
///
/// An `Action` is *intent*, not execution: it names a handler and carries its
/// literal arguments. A later interaction/runtime layer maps `name` to actual
/// behavior. Keeping it declarative means a fired callback is just another
/// auditable record on the cowork surface — see [`Document::fire`].
#[derive(Debug, Clone, PartialEq)]
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
pub struct Binding {
    pub expr: String,
}

/// A single element in the UI tree.
///
/// `kind` is a normalized element name in *our* vocabulary (e.g. `"Stack"`,
/// `"Text"`, `"Rect"`) — a Flutter `Column` or a SwiftUI `VStack` both lower
/// to the same `kind`, which is the point.
#[derive(Debug, Clone, Default)]
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
pub enum Origin {
    Human,
    Ai,
    System,
}

/// A discrete change to a [`Document`]. The renderer-facing mutation stream
/// and the AI-malleability surface are one and the same enum.
#[derive(Debug, Clone, PartialEq)]
pub enum Mutation {
    /// Create a detached node of the given `kind`.
    CreateNode { id: NodeId, kind: String },
    /// Make `id` the document root.
    SetRoot { id: NodeId },
    /// Set (or overwrite) a property on a node.
    SetProp { id: NodeId, key: String, value: Value },
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
    SetCallback { id: NodeId, event: String, action: Action },
    /// Bind (or overwrite) a dynamic [`Binding`] for `key` on a node.
    SetBinding { id: NodeId, key: String, binding: Binding },
    /// Audited record that a callback was *fired* (not registered). Emitted by
    /// [`Document::fire`] so human- and AI-fired invocations are both
    /// attributable in the log. Applying it does not mutate the tree.
    Invoke { id: NodeId, event: String },
}

/// An audited edit: a mutation plus who authored it.
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Default)]
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
        doc.apply_from(Origin::Human, Mutation::CreateNode { id: root, kind: "Stack".into() }).unwrap();
        doc.apply_from(Origin::Human, Mutation::SetRoot { id: root }).unwrap();

        // The AI companion adds a Text child and styles it (its own volition).
        let label = doc.fresh_id();
        doc.apply_from(Origin::Ai, Mutation::CreateNode { id: label, kind: "Text".into() }).unwrap();
        doc.apply_from(Origin::Ai, Mutation::SetProp {
            id: label,
            key: "content".into(),
            value: Value::Text("Hello".into()),
        }).unwrap();
        doc.apply_from(Origin::Ai, Mutation::AppendChild { parent: root, child: label }).unwrap();

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
            .apply_from(Origin::System, Mutation::SetProp {
                id: ghost,
                key: "x".into(),
                value: Value::Px(1.0),
            })
            .unwrap_err();
        assert_eq!(err, IrError::NoSuchNode(ghost));
        assert!(doc.audit_log().is_empty());
    }

    #[test]
    fn remove_child_detaches_without_deleting() {
        let mut doc = Document::new();
        let a = doc.fresh_id();
        let b = doc.fresh_id();
        doc.apply_from(Origin::System, Mutation::CreateNode { id: a, kind: "Stack".into() }).unwrap();
        doc.apply_from(Origin::System, Mutation::CreateNode { id: b, kind: "Rect".into() }).unwrap();
        doc.apply_from(Origin::System, Mutation::AppendChild { parent: a, child: b }).unwrap();
        doc.apply_from(Origin::System, Mutation::RemoveChild { parent: a, child: b }).unwrap();

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
        doc.apply_from(Origin::Human, Mutation::CreateNode { id: btn, kind: "Button".into() })
            .unwrap();

        let action = Action { name: "submit".into(), args: vec![Value::Int(7)] };
        doc.apply_from(Origin::Human, Mutation::SetCallback {
            id: btn,
            event: "tap".into(),
            action: action.clone(),
        })
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
            Mutation::Invoke { id: btn, event: "tap".into() }
        );
    }

    /// SetBinding stores a dynamic binding that reads back, and lives alongside
    /// (not replacing) any literal prop on the same key.
    #[test]
    fn set_binding_stores_and_reads_alongside_literals() {
        let mut doc = Document::new();
        let n = doc.fresh_id();
        doc.apply_from(Origin::Ai, Mutation::CreateNode { id: n, kind: "Rect".into() })
            .unwrap();

        // A literal width...
        doc.apply_from(Origin::Ai, Mutation::SetProp {
            id: n,
            key: "width".into(),
            value: Value::Px(100.0),
        })
        .unwrap();
        // ...and a binding for the same key.
        let binding = Binding { expr: "state.width".into() };
        doc.apply_from(Origin::Ai, Mutation::SetBinding {
            id: n,
            key: "width".into(),
            binding: binding.clone(),
        })
        .unwrap();

        // Both coexist.
        assert_eq!(doc.get(n).unwrap().props.get("width"), Some(&Value::Px(100.0)));
        assert_eq!(doc.get(n).unwrap().bindings.get("width"), Some(&binding));
    }
}
