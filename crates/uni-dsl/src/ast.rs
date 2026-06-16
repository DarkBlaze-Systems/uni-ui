//! The `.uni` abstract syntax tree.
//!
//! This is the parser's output — a faithful tree of the source, *before*
//! lowering to `uni-ir`. Keeping it separate means the grammar can grow
//! (bindings, expressions, …) without entangling the IR-lowering step.

/// A literal property value, still in surface form. Lowered to
/// [`uni_ir::Value`] in `lower`.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    /// Packed `0xRRGGBBAA`.
    Color(u32),
    /// Logical pixels.
    Px(f32),
}

/// The value side of a `key: ...;` entry: either a literal or a `$`-prefixed
/// dynamic binding expression.
///
/// A literal lowers to a `SetProp`; a binding lowers to a `SetBinding`. Both
/// can coexist on a node for *different* keys — the IR keeps `props` and
/// `bindings` side by side.
#[derive(Debug, Clone, PartialEq)]
pub enum PropValue {
    /// `key: <literal>;`
    Literal(Literal),
    /// `key: $dotted.path;` — the stored string is the path with the `$`
    /// already stripped (e.g. `theme.accent`).
    Binding(String),
}

/// One `key: value;` entry inside an element body. `value` is a literal or a
/// dynamic binding.
#[derive(Debug, Clone, PartialEq)]
pub struct Prop {
    pub key: String,
    pub value: PropValue,
}

/// One `on <event>: <name>(<args>);` event handler inside an element body.
#[derive(Debug, Clone, PartialEq)]
pub struct Callback {
    pub event: String,
    pub action: String,
    /// Literal arguments to the action, in source order.
    pub args: Vec<Literal>,
}

/// A `Kind { ... }` element: a node kind plus an ordered body of properties,
/// event handlers, and child elements.
///
/// Control-flow constructs (`if`/`for`) reuse this same node shape: they lower
/// to synthetic `kind`s (`"If"` / `"For"`) and carry their condition/collection
/// expression in `bindings` (key `"cond"` / `"items"`).
#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    pub kind: String,
    pub props: Vec<Prop>,
    pub callbacks: Vec<Callback>,
    pub children: Vec<Element>,
    /// Synthetic bindings attached to the element itself, keyed by name. Used
    /// by `if`/`for` to attach `cond`/`items` expressions; empty for ordinary
    /// elements (their dynamic props live in `props` as [`PropValue::Binding`]).
    pub element_bindings: Vec<(String, String)>,
}

impl Element {
    /// Construct an ordinary element with no element-level bindings.
    pub fn new(kind: String, props: Vec<Prop>, callbacks: Vec<Callback>, children: Vec<Element>) -> Self {
        Element {
            kind,
            props,
            callbacks,
            children,
            element_bindings: Vec::new(),
        }
    }
}
