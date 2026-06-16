//! uni-react — a fine-grained reactive signal layer for the Uni-UI engine.
//!
//! This is **not** a virtual-DOM diffing layer. It is a clean-room, std-only
//! implementation of a push/pull reactive graph in the lineage of
//! SolidJS / leptos `reactive_graph` / preact-signals, reconstructed from
//! public descriptions of those models (no source ported).
//!
//! # Model
//!
//! Three primitive node kinds live inside a single [`Runtime`] arena:
//!
//! * [`Signal<T>`] — a mutable root of reactivity. [`Signal::get`] records the
//!   *current observer* as a dependent; [`Signal::set`] marks dependents dirty
//!   and schedules any effects.
//! * [`Memo<T>`] — a derived, cached value. Recomputed lazily (pull) only when
//!   one of its dependencies actually changed, and only when read.
//! * [`Effect`] — a side-effecting closure that re-runs (push) whenever a
//!   signal or memo it read changes.
//!
//! # Dependency tracking
//!
//! Tracking is automatic and dynamic. A thread-local **observer stack** holds
//! the node currently executing. When a reactive source is *read* it looks at
//! the top of that stack and, if present, wires a two-way edge:
//! `source.subscribers += observer` and `observer.sources += source`.
//! Because the source set is rebuilt from scratch on every (re)run, stale
//! dependencies are dropped automatically — an effect that stops reading a
//! signal stops depending on it.
//!
//! Propagation is split into the classic two phases:
//!
//! * **Mark** (push): `set` walks the subscriber graph marking every reachable
//!   memo/effect `Dirty`, and pushes dirty effects onto a run queue.
//! * **Update** (pull): effects in the queue re-run; a memo recomputes only
//!   when its value is actually requested *and* it is currently dirty, then
//!   caches the result. This is what makes memos memoize: an unchanged
//!   dependency graph yields zero recomputes.
//!
//! # Status / future milestones
//!
//! v0 is **single-threaded**: the arena lives behind an `Rc<RefCell<..>>`
//! handle and none of the public types are `Send`/`Sync`. A future milestone
//! is to make the [`Runtime`] thread-safe (interior `Mutex`/sharded locks,
//! atomic generations) so signals can be `Send + Sync` and driven from a
//! render thread distinct from the logic thread. Also future: explicit
//! disposal/ownership scopes, batched updates, and untracked-read scoping.

use std::cell::RefCell;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// Arena keys
// ---------------------------------------------------------------------------

/// A generational index into the runtime arena (slotmap-style).
///
/// `gen` lets us detect use of a stale key after a slot is reused. v0 never
/// frees nodes, so the generation is always 0, but the field is kept so the
/// representation is forward-compatible with disposal.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct NodeKey {
    index: usize,
    gen: u32,
}

// ---------------------------------------------------------------------------
// Node state
// ---------------------------------------------------------------------------

/// Whether a derived node's cached value can be trusted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Cache is valid (or, for signals, always valid).
    Clean,
    /// A dependency changed; the node must recompute before its value is read.
    Dirty,
}

/// What a node *is*. The boxed value/closure is type-erased; the typed
/// handles ([`Signal`], [`Memo`]) re-impose the type on access.
enum NodeKind {
    /// A root mutable cell.
    Signal,
    /// A derived value computed by `compute`, caching into the node's `value`.
    Memo {
        compute: Box<dyn FnMut(&Runtime) -> Box<dyn AnyValue>>,
    },
    /// A side effect; `run` re-executes when a dependency changes.
    Effect {
        run: Box<dyn FnMut(&Runtime)>,
    },
}

/// One reactive node in the arena.
struct Node {
    kind: NodeKind,
    state: State,
    /// Cached value (signals always have one; memos after first compute).
    value: Option<Box<dyn AnyValue>>,
    /// Sources this node read during its last run (its dependencies).
    sources: HashSet<NodeKey>,
    /// Nodes that depend on this one.
    subscribers: HashSet<NodeKey>,
}

/// Minimal `Any`-like trait so we can store heterogeneous values in the arena
/// and downcast on typed access.
trait AnyValue {
    fn as_any(&self) -> &dyn std::any::Any;
}

impl<T: 'static> AnyValue for T {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

/// The reactive arena: owns all nodes, the observer stack, and the effect
/// run queue. Cheaply clonable (`Rc` handle) so typed handles can hold one.
#[derive(Clone)]
pub struct Runtime {
    inner: Rc<RefCell<Inner>>,
}

struct Inner {
    nodes: Vec<Option<Node>>,
    /// Stack of currently-executing observers; top is the active one.
    observer_stack: Vec<NodeKey>,
    /// Effects scheduled to run (drained by `flush`).
    pending_effects: Vec<NodeKey>,
    /// Re-entrancy guard so nested `set`s don't double-flush.
    flushing: bool,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    /// Create a fresh, empty runtime.
    pub fn new() -> Self {
        Runtime {
            inner: Rc::new(RefCell::new(Inner {
                nodes: Vec::new(),
                observer_stack: Vec::new(),
                pending_effects: Vec::new(),
                flushing: false,
            })),
        }
    }

    /// Create a new signal seeded with `value`.
    pub fn signal<T: 'static>(&self, value: T) -> Signal<T> {
        let key = {
            let mut inner = self.inner.borrow_mut();
            inner.alloc(Node {
                kind: NodeKind::Signal,
                state: State::Clean,
                value: Some(Box::new(value)),
                sources: HashSet::new(),
                subscribers: HashSet::new(),
            })
        };
        Signal {
            rt: self.clone(),
            key,
            _marker: PhantomData,
        }
    }

    /// Create a memo deriving its value from `compute`. The closure may read
    /// signals/memos; those reads become the memo's tracked dependencies.
    pub fn memo<T, F>(&self, mut compute: F) -> Memo<T>
    where
        T: 'static,
        F: FnMut(&Runtime) -> T + 'static,
    {
        let key = {
            let mut inner = self.inner.borrow_mut();
            inner.alloc(Node {
                kind: NodeKind::Memo {
                    compute: Box::new(move |rt| Box::new(compute(rt))),
                },
                state: State::Dirty, // force first compute on first read
                value: None,
                sources: HashSet::new(),
                subscribers: HashSet::new(),
            })
        };
        Memo {
            rt: self.clone(),
            key,
            _marker: PhantomData,
        }
    }

    /// Create an effect. The closure runs immediately once (to capture its
    /// initial dependencies) and re-runs whenever any of them change.
    pub fn effect<F>(&self, run: F) -> Effect
    where
        F: FnMut(&Runtime) + 'static,
    {
        let key = {
            let mut inner = self.inner.borrow_mut();
            inner.alloc(Node {
                kind: NodeKind::Effect { run: Box::new(run) },
                state: State::Dirty,
                value: None,
                sources: HashSet::new(),
                subscribers: HashSet::new(),
            })
        };
        // Initial run wires up dependencies.
        self.run_node(key);
        Effect {
            rt: self.clone(),
            key,
        }
    }

    // -- internal mechanics --------------------------------------------------

    /// Record `source` as a dependency of the current observer (if any).
    fn track(&self, source: NodeKey) {
        let mut inner = self.inner.borrow_mut();
        if let Some(&observer) = inner.observer_stack.last() {
            if observer == source {
                return; // never self-subscribe
            }
            if let Some(n) = inner.node_mut(source) {
                n.subscribers.insert(observer);
            }
            if let Some(n) = inner.node_mut(observer) {
                n.sources.insert(source);
            }
        }
    }

    /// Mark everything transitively downstream of `key` as dirty, queueing any
    /// effects encountered. Signals themselves are never "dirty"; we start
    /// from their subscribers.
    fn mark_subscribers_dirty(&self, key: NodeKey) {
        let mut inner = self.inner.borrow_mut();
        let mut stack: Vec<NodeKey> = match inner.node(key) {
            Some(n) => n.subscribers.iter().copied().collect(),
            None => return,
        };
        while let Some(k) = stack.pop() {
            let (is_effect, already_dirty, more_subs) = match inner.node(k) {
                Some(n) => {
                    let is_effect = matches!(n.kind, NodeKind::Effect { .. });
                    let dirty = n.state == State::Dirty;
                    let subs: Vec<NodeKey> = n.subscribers.iter().copied().collect();
                    (is_effect, dirty, subs)
                }
                None => continue,
            };
            // A memo that's already dirty has already propagated to its
            // subscribers, so we can stop. Effects are always (re)queued.
            if already_dirty && !is_effect {
                continue;
            }
            if let Some(n) = inner.node_mut(k) {
                n.state = State::Dirty;
            }
            if is_effect {
                if !inner.pending_effects.contains(&k) {
                    inner.pending_effects.push(k);
                }
            } else {
                // memo: propagate dirtiness to ITS subscribers
                stack.extend(more_subs);
            }
        }
    }

    /// Run all queued effects until the queue drains.
    fn flush(&self) {
        {
            let mut inner = self.inner.borrow_mut();
            if inner.flushing {
                return;
            }
            inner.flushing = true;
        }
        loop {
            let next = {
                let mut inner = self.inner.borrow_mut();
                inner.pending_effects.pop()
            };
            match next {
                Some(key) => {
                    // Only run if still dirty (could have been cleaned).
                    let dirty = self
                        .inner
                        .borrow()
                        .node(key)
                        .map(|n| n.state == State::Dirty)
                        .unwrap_or(false);
                    if dirty {
                        self.run_node(key);
                    }
                }
                None => break,
            }
        }
        self.inner.borrow_mut().flushing = false;
    }

    /// (Re)run a memo or effect node: clear its old dependency edges, push it
    /// as the current observer, execute, then store the result and mark clean.
    fn run_node(&self, key: NodeKey) {
        // Clear stale incoming edges so dependencies are rebuilt fresh.
        {
            let mut inner = self.inner.borrow_mut();
            let old_sources: Vec<NodeKey> = match inner.node(key) {
                Some(n) => n.sources.iter().copied().collect(),
                None => return,
            };
            for s in old_sources {
                if let Some(n) = inner.node_mut(s) {
                    n.subscribers.remove(&key);
                }
            }
            if let Some(n) = inner.node_mut(key) {
                n.sources.clear();
            }
            inner.observer_stack.push(key);
        }

        // Take the closure out so we can call it without holding the borrow.
        let taken = {
            let mut inner = self.inner.borrow_mut();
            inner
                .node_mut(key)
                .map(|n| std::mem::replace(&mut n.kind, NodeKind::Signal))
        };

        let result = match taken {
            Some(NodeKind::Memo { mut compute }) => {
                let v = compute(self);
                Some((NodeKind::Memo { compute }, Some(v)))
            }
            Some(NodeKind::Effect { mut run }) => {
                run(self);
                Some((NodeKind::Effect { run }, None))
            }
            Some(other) => Some((other, None)), // not expected
            None => None,
        };

        // Restore kind + value, mark clean, pop observer.
        let mut inner = self.inner.borrow_mut();
        inner.observer_stack.pop();
        if let Some((kind, new_value)) = result {
            if let Some(n) = inner.node_mut(key) {
                n.kind = kind;
                if let Some(v) = new_value {
                    n.value = Some(v);
                }
                n.state = State::Clean;
            }
        }
    }

    /// Ensure a memo's cache is current, recomputing if dirty.
    fn ensure_memo_current(&self, key: NodeKey) {
        let dirty = self
            .inner
            .borrow()
            .node(key)
            .map(|n| n.state == State::Dirty)
            .unwrap_or(false);
        if dirty {
            self.run_node(key);
        }
    }
}

impl Inner {
    fn alloc(&mut self, node: Node) -> NodeKey {
        let index = self.nodes.len();
        self.nodes.push(Some(node));
        NodeKey { index, gen: 0 }
    }

    fn node(&self, key: NodeKey) -> Option<&Node> {
        self.nodes.get(key.index).and_then(|s| s.as_ref())
    }

    fn node_mut(&mut self, key: NodeKey) -> Option<&mut Node> {
        self.nodes.get_mut(key.index).and_then(|s| s.as_mut())
    }
}

// ---------------------------------------------------------------------------
// Signal
// ---------------------------------------------------------------------------

/// A mutable, trackable root value.
pub struct Signal<T> {
    rt: Runtime,
    key: NodeKey,
    _marker: PhantomData<T>,
}

impl<T> Clone for Signal<T> {
    fn clone(&self) -> Self {
        Signal {
            rt: self.rt.clone(),
            key: self.key,
            _marker: PhantomData,
        }
    }
}

impl<T: Clone + 'static> Signal<T> {
    /// Read the value, recording the current observer as a dependent.
    pub fn get(&self) -> T {
        self.rt.track(self.key);
        self.get_untracked()
    }

    /// Replace the value, mark dependents dirty, and run scheduled effects.
    pub fn set(&self, value: T) {
        {
            let mut inner = self.rt.inner.borrow_mut();
            if let Some(n) = inner.node_mut(self.key) {
                n.value = Some(Box::new(value));
            }
        }
        self.rt.mark_subscribers_dirty(self.key);
        self.rt.flush();
    }

    /// Apply `f` to the current value and store the result (read-modify-write).
    pub fn update<F: FnOnce(&mut T)>(&self, f: F) {
        let mut v = self.get_untracked();
        f(&mut v);
        self.set(v);
    }

    /// Read without registering a dependency.
    pub fn get_untracked(&self) -> T {
        let inner = self.rt.inner.borrow();
        let n = inner.node(self.key).expect("signal node missing");
        let boxed = n.value.as_ref().expect("signal always has a value");
        // `boxed.as_ref()` yields `&dyn AnyValue` for the INNER value; calling
        // `as_any` directly on the `Box` would resolve to the box's own impl.
        AnyValue::as_any(boxed.as_ref())
            .downcast_ref::<T>()
            .expect("signal type mismatch")
            .clone()
    }
}

// ---------------------------------------------------------------------------
// Memo
// ---------------------------------------------------------------------------

/// A derived, cached value. Recomputes lazily when a dependency changes.
pub struct Memo<T> {
    rt: Runtime,
    key: NodeKey,
    _marker: PhantomData<T>,
}

impl<T> Clone for Memo<T> {
    fn clone(&self) -> Self {
        Memo {
            rt: self.rt.clone(),
            key: self.key,
            _marker: PhantomData,
        }
    }
}

impl<T: Clone + 'static> Memo<T> {
    /// Read the memo's value, recomputing it first if a dependency changed,
    /// then recording the current observer as a dependent of this memo.
    pub fn get(&self) -> T {
        // Recompute (if dirty) BEFORE tracking, so the memo's own dependency
        // reads aren't mis-attributed to our caller.
        self.rt.ensure_memo_current(self.key);
        self.rt.track(self.key);
        let inner = self.rt.inner.borrow();
        let n = inner.node(self.key).expect("memo node missing");
        let boxed = n.value.as_ref().expect("memo computed before read");
        AnyValue::as_any(boxed.as_ref())
            .downcast_ref::<T>()
            .expect("memo type mismatch")
            .clone()
    }
}

// ---------------------------------------------------------------------------
// Effect
// ---------------------------------------------------------------------------

/// A handle to a running effect. Keeping the handle alive documents the
/// effect's lifetime; v0 has no explicit disposal (a future milestone), so
/// dropping the handle does not yet remove the node from the graph.
pub struct Effect {
    #[allow(dead_code)]
    rt: Runtime,
    #[allow(dead_code)]
    key: NodeKey,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    /// (1) signal -> memo -> effect propagation fires on change.
    #[test]
    fn signal_memo_effect_propagation() {
        let rt = Runtime::new();
        let count = rt.signal(1i32);

        let c2 = count.clone();
        let doubled = rt.memo(move |_| c2.get() * 2);

        let observed = Rc::new(Cell::new(0i32));
        let obs = observed.clone();
        let d2 = doubled.clone();
        let _eff = rt.effect(move |_| {
            obs.set(d2.get());
        });

        // Initial run captured doubled = 2.
        assert_eq!(observed.get(), 2);

        // Change the signal: memo recomputes, effect re-fires.
        count.set(5);
        assert_eq!(observed.get(), 10);
    }

    /// (2) an effect does NOT re-run when an unrelated signal changes.
    #[test]
    fn effect_skips_unrelated_signal() {
        let rt = Runtime::new();
        let watched = rt.signal(0i32);
        let unrelated = rt.signal(0i32);

        let runs = Rc::new(Cell::new(0u32));
        let r = runs.clone();
        let w = watched.clone();
        let _eff = rt.effect(move |_| {
            let _ = w.get(); // only depends on `watched`
            r.set(r.get() + 1);
        });

        assert_eq!(runs.get(), 1); // initial run

        // Mutating an unrelated signal must NOT re-run the effect.
        unrelated.set(42);
        assert_eq!(runs.get(), 1);

        // Mutating the watched signal must re-run it.
        watched.set(1);
        assert_eq!(runs.get(), 2);
    }

    /// (3) memo caches: recompute count only increments when a dep changes.
    #[test]
    fn memo_caches_until_dependency_changes() {
        let rt = Runtime::new();
        let a = rt.signal(2i32);
        let b = rt.signal(100i32); // unrelated to the memo

        let computes = Rc::new(Cell::new(0u32));
        let c = computes.clone();
        let a2 = a.clone();
        let squared = rt.memo(move |_| {
            c.set(c.get() + 1);
            let v = a2.get();
            v * v
        });

        // Lazy: no compute until first read.
        assert_eq!(computes.get(), 0);

        assert_eq!(squared.get(), 4);
        assert_eq!(computes.get(), 1);

        // Repeated reads hit the cache.
        assert_eq!(squared.get(), 4);
        assert_eq!(squared.get(), 4);
        assert_eq!(computes.get(), 1);

        // Changing an unrelated signal does not invalidate the cache.
        b.set(101);
        assert_eq!(computes.get(), 1);
        assert_eq!(squared.get(), 4);
        assert_eq!(computes.get(), 1);

        // Changing the real dependency invalidates; recompute on next read.
        a.set(3);
        assert_eq!(computes.get(), 1); // still lazy: not yet recomputed
        assert_eq!(squared.get(), 9);
        assert_eq!(computes.get(), 2);
    }

    /// Bonus: dynamic dependency tracking — an effect only depends on what it
    /// read on its LAST run.
    #[test]
    fn dynamic_dependencies_are_rebuilt() {
        let rt = Runtime::new();
        let cond = rt.signal(true);
        let x = rt.signal(1i32);
        let y = rt.signal(1i32);

        let runs = Rc::new(Cell::new(0u32));
        let r = runs.clone();
        let (cc, xx, yy) = (cond.clone(), x.clone(), y.clone());
        let _eff = rt.effect(move |_| {
            r.set(r.get() + 1);
            if cc.get() {
                let _ = xx.get();
            } else {
                let _ = yy.get();
            }
        });
        assert_eq!(runs.get(), 1);

        // Currently reading x; changing y should do nothing.
        y.set(2);
        assert_eq!(runs.get(), 1);
        x.set(2);
        assert_eq!(runs.get(), 2);

        // Flip condition -> now depends on y, not x.
        cond.set(false);
        assert_eq!(runs.get(), 3);
        x.set(3); // x no longer a dependency
        assert_eq!(runs.get(), 3);
        y.set(3);
        assert_eq!(runs.get(), 4);
    }
}
