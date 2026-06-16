//! # uni-dsl — the `.uni` declarative-UI language frontend
//!
//! Lexes and parses our native `.uni` source and **lowers** it into a
//! [`uni_ir::Document`]. It does not edit the IR directly: it emits the IR's
//! own [`Mutation`]s (`CreateNode` / `SetProp` / `AppendChild` / `SetRoot`)
//! through [`Document::apply_from`] with [`Origin::System`], so a parsed
//! document is just a System-authored prefix of the same cowork mutation
//! stream a human or the AI companion would later extend.
//!
//! ## Grammar (v0)
//!
//! ```text
//! Stack {
//!     padding: 16px;
//!     background: #0a0a0a;
//!     Text { content: "Uni-UI"; size: 28px; color: #ffffff; }
//!     Rect { width: 200px; height: 80px; color: #7d39eb; }
//! }
//! ```
//!
//! - An **element** is `Kind { ... }`; `Kind` is an identifier and becomes the
//!   node's `kind`.
//! - A **body** holds `prop: value;` entries, `on <event>: ..;` handlers,
//!   `if`/`for` blocks, and nested child elements — freely interleaved.
//! - **Values**: string `"..."` → [`Value::Text`]; integer → [`Value::Int`];
//!   decimal → [`Value::Float`]; `true`/`false` → [`Value::Bool`]; color
//!   `#RRGGBB` / `#RRGGBBAA` → [`Value::Color`] packed `0xRRGGBBAA` (`#RRGGBB`
//!   expands to alpha `0xFF`); length `Npx` → [`Value::Px`].
//! - There is exactly **one root** element.
//! - `//` line comments are ignored.
//!
//! ## Additive constructs (rungs 3–5)
//!
//! ```text
//! Stack {
//!     width: $w;                       // bound prop → SetBinding
//!     color: $theme.accent;            // dotted-path binding
//!     padding: 16px;                   // literal still → SetProp
//!     on click: submit("form");        // callback → SetCallback
//!     on hover: toggle();              // zero-arg callback
//!     if ($visible) {                  // → CreateNode{kind:"If"} + SetBinding{cond}
//!         Text { content: "shown"; }
//!     }
//!     for ($items) {                   // → CreateNode{kind:"For"} + SetBinding{items}
//!         Rect { width: 10px; }        //   children are the template
//!     }
//! }
//! ```
//!
//! - A **bound prop** `key: $path;` strips the `$` and lowers to
//!   [`Mutation::SetBinding`] with `Binding { expr: "path" }`. Literal props on
//!   *other* keys still lower to [`Mutation::SetProp`]; both can sit on one node.
//! - A **callback** `on <event>: <name>(<args>);` lowers to
//!   [`Mutation::SetCallback`] with an [`Action`] whose `args` are literal
//!   [`Value`]s.
//! - **`if`/`for`** lower *structurally only*: a synthetic node of kind `"If"`
//!   / `"For"` carrying the parenthesized expression as a `cond` / `items`
//!   binding, with the block's children appended. The reactive layer expands
//!   them later — this crate does no evaluation.
//! - `if`, `for`, and `on` are reserved keywords (not usable as element kinds
//!   or prop names).

mod ast;
mod parser;
mod token;

use logos::Logos;

use ast::{Element, Literal, PropValue};
use token::Token;
use uni_ir::{Action, Binding, Document, Mutation, Origin, Value};

/// A failure to turn `.uni` source into a [`Document`].
///
/// `Lex` and `Parse` are surface-syntax problems; `Lower` would only fire if
/// the IR rejected a mutation we built (it shouldn't, given a valid parse —
/// it's surfaced rather than panicked on for defensiveness).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The lexer hit input it could not tokenize, at the given byte span.
    Lex { span: std::ops::Range<usize> },
    /// The parser rejected the token stream. `message` is human-readable.
    Parse { message: String },
    /// The IR rejected a lowered mutation (should be unreachable on valid input).
    Lower { message: String },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Lex { span } => {
                write!(f, "lex error: unrecognized input at bytes {span:?}")
            }
            ParseError::Parse { message } => write!(f, "parse error: {message}"),
            ParseError::Lower { message } => write!(f, "lowering error: {message}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse `.uni` source and lower it to a [`uni_ir::Document`].
///
/// The returned document has its root set, every element materialized as a
/// node, every property applied, and the parent/child tree wired up — all via
/// [`Origin::System`] mutations recorded in the document's audit log.
pub fn parse(src: &str) -> Result<Document, ParseError> {
    let tokens = lex(src)?;

    use chumsky::Parser;
    let root = parser::parser()
        .parse(tokens)
        .map_err(|errs| ParseError::Parse {
            message: errs
                .iter()
                .map(|e| format!("{e:?}"))
                .collect::<Vec<_>>()
                .join("; "),
        })?;

    lower(&root)
}

/// Run the lexer to completion, failing on the first unrecognized byte.
fn lex(src: &str) -> Result<Vec<Token>, ParseError> {
    let mut lexer = Token::lexer(src);
    let mut tokens = Vec::new();
    while let Some(result) = lexer.next() {
        match result {
            Ok(tok) => tokens.push(tok),
            Err(()) => return Err(ParseError::Lex { span: lexer.span() }),
        }
    }
    Ok(tokens)
}

/// Lower the AST root into a fresh [`Document`], emitting IR mutations.
fn lower(root: &Element) -> Result<Document, ParseError> {
    let mut doc = Document::new();
    let root_id = lower_element(&mut doc, root)?;
    doc.apply_from(Origin::System, Mutation::SetRoot { id: root_id })
        .map_err(lower_err)?;
    Ok(doc)
}

/// Recursively materialize one element (and its subtree) into the document,
/// returning its allocated [`uni_ir::NodeId`].
fn lower_element(doc: &mut Document, el: &Element) -> Result<uni_ir::NodeId, ParseError> {
    let id = doc.fresh_id();
    doc.apply_from(
        Origin::System,
        Mutation::CreateNode {
            id,
            kind: el.kind.clone(),
        },
    )
    .map_err(lower_err)?;

    // Element-level synthetic bindings (e.g. an `If`'s `cond`, a `For`'s
    // `items`) attach directly to this node.
    for (key, expr) in &el.element_bindings {
        doc.apply_from(
            Origin::System,
            Mutation::SetBinding {
                id,
                key: key.clone(),
                binding: Binding { expr: expr.clone() },
            },
        )
        .map_err(lower_err)?;
    }

    // A prop is either a literal (→ SetProp) or a dynamic binding (→
    // SetBinding). Literals and bindings on different keys coexist on a node.
    for prop in &el.props {
        let mutation = match &prop.value {
            PropValue::Literal(lit) => Mutation::SetProp {
                id,
                key: prop.key.clone(),
                value: lower_value(lit),
            },
            PropValue::Binding(expr) => Mutation::SetBinding {
                id,
                key: prop.key.clone(),
                binding: Binding { expr: expr.clone() },
            },
        };
        doc.apply_from(Origin::System, mutation)
            .map_err(lower_err)?;
    }

    // Event handlers → SetCallback with a literal-arg Action.
    for cb in &el.callbacks {
        doc.apply_from(
            Origin::System,
            Mutation::SetCallback {
                id,
                event: cb.event.clone(),
                action: Action {
                    name: cb.action.clone(),
                    args: cb.args.iter().map(lower_value).collect(),
                },
            },
        )
        .map_err(lower_err)?;
    }

    for child in &el.children {
        let child_id = lower_element(doc, child)?;
        doc.apply_from(
            Origin::System,
            Mutation::AppendChild {
                parent: id,
                child: child_id,
            },
        )
        .map_err(lower_err)?;
    }

    Ok(id)
}

/// Map a surface [`Literal`] to an IR [`Value`].
fn lower_value(lit: &Literal) -> Value {
    match lit {
        Literal::Str(s) => Value::Text(s.clone()),
        Literal::Int(i) => Value::Int(*i),
        Literal::Float(f) => Value::Float(*f),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Color(c) => Value::Color(*c),
        Literal::Px(p) => Value::Px(*p),
    }
}

fn lower_err(e: uni_ir::IrError) -> ParseError {
    ParseError::Lower {
        message: format!("{e:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        Stack {
            padding: 16px;
            background: #0a0a0a;
            Text { content: "Uni-UI"; size: 28px; color: #ffffff; }
            Rect { width: 200px; height: 80px; color: #7d39eb; }
        }
    "#;

    #[test]
    fn parses_the_sample_document() {
        let doc = parse(SAMPLE).expect("sample should parse");

        // Root is the Stack.
        let root_id = doc.root().expect("a root must be set");
        let root = doc.get(root_id).unwrap();
        assert_eq!(root.kind, "Stack");

        // padding == Px(16.0), background expanded to full alpha.
        assert_eq!(root.props.get("padding"), Some(&Value::Px(16.0)));
        assert_eq!(
            root.props.get("background"),
            Some(&Value::Color(0x0a0a0aff))
        );

        // Two children, in order: Text then Rect.
        assert_eq!(root.children.len(), 2);
        let text = doc.get(root.children[0]).unwrap();
        let rect = doc.get(root.children[1]).unwrap();
        assert_eq!(text.kind, "Text");
        assert_eq!(rect.kind, "Rect");

        // Text.content is the string, Text.size is a Px length.
        assert_eq!(
            text.props.get("content"),
            Some(&Value::Text("Uni-UI".to_string()))
        );
        assert_eq!(text.props.get("size"), Some(&Value::Px(28.0)));
        assert_eq!(text.props.get("color"), Some(&Value::Color(0xffffffff)));

        // Rect width/height are `px` lengths here.
        assert_eq!(rect.props.get("width"), Some(&Value::Px(200.0)));
        assert_eq!(rect.props.get("height"), Some(&Value::Px(80.0)));
        // Rect.color is the packed brand purple with full alpha.
        assert_eq!(rect.props.get("color"), Some(&Value::Color(0x7d39ebff)));
    }

    #[test]
    fn parented_children_carry_back_pointers() {
        let doc = parse(SAMPLE).unwrap();
        let root_id = doc.root().unwrap();
        for child_id in &doc.get(root_id).unwrap().children {
            assert_eq!(doc.get(*child_id).unwrap().parent, Some(root_id));
        }
    }

    #[test]
    fn int_float_and_bool_values_lower_distinctly() {
        let doc = parse(r#"Box { flex: 2; ratio: 1.5; visible: true; }"#).unwrap();
        let root = doc.get(doc.root().unwrap()).unwrap();
        assert_eq!(root.props.get("flex"), Some(&Value::Int(2)));
        assert_eq!(root.props.get("ratio"), Some(&Value::Float(1.5)));
        assert_eq!(root.props.get("visible"), Some(&Value::Bool(true)));
    }

    #[test]
    fn malformed_input_is_a_parse_error() {
        // Missing the closing brace + missing semicolon on a prop.
        let err = parse(r#"Stack { padding: 16px "#).unwrap_err();
        assert!(
            matches!(err, ParseError::Parse { .. }),
            "expected a Parse error, got {err:?}"
        );
    }

    #[test]
    fn garbage_bytes_are_a_lex_error() {
        // `@` is not a legal `.uni` character.
        let err = parse(r#"Stack { @ }"#).unwrap_err();
        assert!(
            matches!(err, ParseError::Lex { .. }),
            "expected a Lex error, got {err:?}"
        );
    }

    /// `$`-prefixed values lower to bindings; literal values on other keys
    /// still lower to props. A node can carry both.
    #[test]
    fn bound_props_lower_to_bindings_alongside_literals() {
        let doc = parse(r#"Rect { width: $w; color: $theme.accent; height: 80px; }"#)
            .expect("bindings should parse");
        let root = doc.get(doc.root().unwrap()).unwrap();

        // Bindings: `$` stripped, dotted path kept.
        assert_eq!(
            root.bindings.get("width"),
            Some(&uni_ir::Binding { expr: "w".into() })
        );
        assert_eq!(
            root.bindings.get("color"),
            Some(&uni_ir::Binding {
                expr: "theme.accent".into()
            })
        );
        // The literal sibling still lowered to a prop.
        assert_eq!(root.props.get("height"), Some(&Value::Px(80.0)));
        // No literal prop was created for a bound key.
        assert_eq!(root.props.get("width"), None);
    }

    /// `on <event>: <name>(<args>);` lowers to a SetCallback with a literal-arg
    /// Action; a zero-arg handler is also valid.
    #[test]
    fn callbacks_lower_to_actions() {
        let doc = parse(r#"Button { on click: submit("form"); on hover: toggle(); }"#)
            .expect("callbacks should parse");
        let root = doc.get(doc.root().unwrap()).unwrap();

        assert_eq!(
            root.callbacks.get("click"),
            Some(&uni_ir::Action {
                name: "submit".into(),
                args: vec![Value::Text("form".into())],
            })
        );
        assert_eq!(
            root.callbacks.get("hover"),
            Some(&uni_ir::Action {
                name: "toggle".into(),
                args: vec![],
            })
        );
    }

    /// Callback args carry mixed literal kinds faithfully.
    #[test]
    fn callback_args_preserve_literal_kinds() {
        let doc = parse(r#"Box { on tap: act("s", 3, 1.5, true, #ff0000, 8px); }"#).unwrap();
        let root = doc.get(doc.root().unwrap()).unwrap();
        assert_eq!(
            root.callbacks.get("tap").unwrap().args,
            vec![
                Value::Text("s".into()),
                Value::Int(3),
                Value::Float(1.5),
                Value::Bool(true),
                Value::Color(0xff0000ff),
                Value::Px(8.0),
            ]
        );
    }

    /// `if`/`for` lower to structural `If`/`For` nodes carrying their
    /// expression as a binding, with the block's children appended.
    #[test]
    fn if_and_for_lower_to_structural_nodes() {
        let src = r#"
            Stack {
                if ($visible) {
                    Text { content: "shown"; }
                }
                for ($items) {
                    Rect { width: 10px; }
                }
            }
        "#;
        let doc = parse(src).expect("if/for should parse");
        let root = doc.get(doc.root().unwrap()).unwrap();
        assert_eq!(root.children.len(), 2);

        // First child: an `If` node with a `cond` binding and one Text child.
        let if_node = doc.get(root.children[0]).unwrap();
        assert_eq!(if_node.kind, "If");
        assert_eq!(
            if_node.bindings.get("cond"),
            Some(&uni_ir::Binding {
                expr: "visible".into()
            })
        );
        assert_eq!(if_node.children.len(), 1);
        assert_eq!(doc.get(if_node.children[0]).unwrap().kind, "Text");

        // Second child: a `For` node with an `items` binding and a template.
        let for_node = doc.get(root.children[1]).unwrap();
        assert_eq!(for_node.kind, "For");
        assert_eq!(
            for_node.bindings.get("items"),
            Some(&uni_ir::Binding {
                expr: "items".into()
            })
        );
        assert_eq!(for_node.children.len(), 1);
        assert_eq!(doc.get(for_node.children[0]).unwrap().kind, "Rect");
    }

    /// A malformed binding/callback is a parse error, not a panic.
    #[test]
    fn malformed_callback_is_a_parse_error() {
        // `on click` with no `: name(...)`.
        let err = parse(r#"Button { on click; }"#).unwrap_err();
        assert!(
            matches!(err, ParseError::Parse { .. }),
            "expected a Parse error, got {err:?}"
        );
    }
}
