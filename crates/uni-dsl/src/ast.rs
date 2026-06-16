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

/// One `prop: value;` entry inside an element body.
#[derive(Debug, Clone, PartialEq)]
pub struct Prop {
    pub key: String,
    pub value: Literal,
}

/// A `Kind { ... }` element: a node kind plus an ordered body of properties
/// and child elements.
#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    pub kind: String,
    pub props: Vec<Prop>,
    pub children: Vec<Element>,
}
