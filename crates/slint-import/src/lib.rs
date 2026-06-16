//! # slint-import — clean-room Slint DSL importer
//!
//! Lowers a subset of the public Slint DSL grammar into a `uni_ir::Document`.
//! This is a **clean-room** implementation built solely from Slint's published
//! documentation; no Slint or copyleft source is read or derived from.
//!
//! ## Supported constructs
//! - Element instantiation: `KindName { ... }`
//! - Property assignments: `prop-name: value;`
//! - Nested children
//! - `component Ident { ... }` declarations — inlined on use
//! - Line comments (`//`)
//! - Values: `"string"`, `Npx`, `#RRGGBB[AA]`, named colors, `true`/`false`, numbers
//!
//! ## Prop name mapping (Slint → uni-ir)
//! | Slint           | uni-ir          |
//! |-----------------|-----------------|
//! | `text`          | `content`        |
//! | `font-size`     | `size`           |
//! | `border-radius` | `corner_radius`  |
//! | everything else | dashes stripped  |

use std::collections::HashMap;
use uni_ir::{Document, Mutation, NodeId, Origin, Value};

// ─────────────────────────────────────────────────────────────────── errors ──

/// Parse or lower error from the Slint importer.
#[derive(Debug)]
pub struct SlintImportError {
    pub message: String,
    pub line: usize,
}

impl std::fmt::Display for SlintImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "slint-import error at line {}: {}",
            self.line, self.message
        )
    }
}

impl std::error::Error for SlintImportError {}

fn err(msg: impl Into<String>, line: usize) -> SlintImportError {
    SlintImportError {
        message: msg.into(),
        line,
    }
}

// ─────────────────────────────────────────────────────────── unsupported ──────

/// A source construct the importer recognized but deliberately *dropped*
/// rather than lower into the IR.
///
/// The IR is opinionated, not a passthrough — some Slint surface has no home in
/// our vocabulary yet. Instead of swallowing it silently, every drop is recorded
/// here so the caller (and the AI companion driving a port) can see exactly what
/// fidelity was lost and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    /// 1-based source line where the dropped construct began.
    pub line: usize,
    /// A short description of what was dropped (e.g. `"inherits Base"`).
    pub text: String,
}

/// The full result of [`parse_with_report`]: the lowered document plus the
/// list of constructs that were dropped on the floor.
#[derive(Debug)]
pub struct ImportReport {
    pub document: Document,
    pub unsupported: Vec<Unsupported>,
}

// ─────────────────────────────────────────────────────────────────── AST ─────

#[derive(Debug, Clone)]
enum SlintValue {
    Px(f32),
    Color(u32),
    Str(String),
    Bool(bool),
    Float(f64),
    Ident(String),
}

#[derive(Debug, Clone)]
struct SlintElement {
    kind: String,
    props: Vec<(String, SlintValue)>,
    children: Vec<SlintElement>,
}

#[derive(Debug)]
struct SlintFile {
    components: HashMap<String, SlintElement>,
    elements: Vec<SlintElement>,
    unsupported: Vec<Unsupported>,
}

// ─────────────────────────────────────────────────────────────────── lexer ───

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            line: 1,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let ch = self.src.get(self.pos).copied()?;
        self.pos += 1;
        if ch == b'\n' {
            self.line += 1;
        }
        Some(ch)
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            // Whitespace
            while self
                .peek()
                .map(|c| c.is_ascii_whitespace())
                .unwrap_or(false)
            {
                self.advance();
            }
            // Line comment
            if self.pos + 1 < self.src.len()
                && self.src[self.pos] == b'/'
                && self.src[self.pos + 1] == b'/'
            {
                while self.peek().map(|c| c != b'\n').unwrap_or(false) {
                    self.advance();
                }
            } else {
                break;
            }
        }
    }

    fn read_ident(&mut self) -> String {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' {
                s.push(c as char);
                self.advance();
            } else {
                break;
            }
        }
        s
    }

    fn read_string(&mut self) -> Result<String, SlintImportError> {
        // consume opening "
        self.advance();
        let mut s = String::new();
        loop {
            match self.advance() {
                None => return Err(err("unterminated string literal", self.line)),
                Some(b'"') => break,
                Some(b'\\') => match self.advance() {
                    Some(b'n') => s.push('\n'),
                    Some(b't') => s.push('\t'),
                    Some(c) => s.push(c as char),
                    None => return Err(err("unterminated escape", self.line)),
                },
                Some(c) => s.push(c as char),
            }
        }
        Ok(s)
    }

    fn read_number(&mut self) -> f64 {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'.' {
                s.push(c as char);
                self.advance();
            } else {
                break;
            }
        }
        s.parse().unwrap_or(0.0)
    }

    fn read_color_hex(&mut self) -> Result<u32, SlintImportError> {
        // consume '#'
        self.advance();
        let mut hex = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_hexdigit() {
                hex.push(c as char);
                self.advance();
            } else {
                break;
            }
        }
        let packed = match hex.len() {
            6 => {
                let v =
                    u32::from_str_radix(&hex, 16).map_err(|_| err("bad hex color", self.line))?;
                (v << 8) | 0xFF
            }
            8 => u32::from_str_radix(&hex, 16).map_err(|_| err("bad hex color", self.line))?,
            _ => {
                return Err(err(
                    format!("hex color must be 6 or 8 digits, got {}", hex.len()),
                    self.line,
                ))
            }
        };
        Ok(packed)
    }

    fn read_value(&mut self) -> Result<SlintValue, SlintImportError> {
        self.skip_ws_and_comments();
        match self.peek() {
            Some(b'"') => Ok(SlintValue::Str(self.read_string()?)),
            Some(b'#') => Ok(SlintValue::Color(self.read_color_hex()?)),
            Some(c) if c.is_ascii_digit() => {
                let n = self.read_number();
                self.skip_ws_and_comments();
                if self.peek() == Some(b'p') && self.src.get(self.pos + 1) == Some(&b'x') {
                    self.advance();
                    self.advance();
                    Ok(SlintValue::Px(n as f32))
                } else {
                    Ok(SlintValue::Float(n))
                }
            }
            Some(_) => {
                let ident = self.read_ident();
                match ident.as_str() {
                    "true" => Ok(SlintValue::Bool(true)),
                    "false" => Ok(SlintValue::Bool(false)),
                    _ => Ok(SlintValue::Ident(ident)),
                }
            }
            None => Err(err(
                "unexpected end of input while reading value",
                self.line,
            )),
        }
    }

    fn expect_byte(&mut self, expected: u8) -> Result<(), SlintImportError> {
        self.skip_ws_and_comments();
        match self.peek() {
            Some(c) if c == expected => {
                self.advance();
                Ok(())
            }
            Some(c) => Err(err(
                format!("expected '{}', got '{}'", expected as char, c as char),
                self.line,
            )),
            None => Err(err(
                format!("expected '{}', got EOF", expected as char),
                self.line,
            )),
        }
    }
}

// ─────────────────────────────────────────── parser (recursive descent) ─────

fn parse_element_with_kind(
    lex: &mut Lexer<'_>,
    kind: String,
    unsupported: &mut Vec<Unsupported>,
) -> Result<SlintElement, SlintImportError> {
    lex.expect_byte(b'{')?;

    let mut props = Vec::new();
    let mut children = Vec::new();

    loop {
        lex.skip_ws_and_comments();
        if lex.peek() == Some(b'}') {
            lex.advance();
            break;
        }
        if lex.peek().is_none() {
            return Err(err("unexpected EOF inside element body", lex.line));
        }

        let key = lex.read_ident();
        if key.is_empty() {
            return Err(err("expected property name or child element", lex.line));
        }

        lex.skip_ws_and_comments();

        if lex.peek() == Some(b':') {
            lex.advance();
            // A two-way binding `prop: <=> other;` has no static value we can
            // lower — record it and skip to the terminating `;`.
            lex.skip_ws_and_comments();
            if lex.peek() == Some(b'<') {
                let line = lex.line;
                skip_to_semicolon(lex);
                unsupported.push(Unsupported {
                    line,
                    text: format!("two-way binding on '{key}'"),
                });
                continue;
            }
            let value = lex.read_value()?;
            lex.skip_ws_and_comments();
            lex.expect_byte(b';')?;
            props.push((key, value));
        } else if lex.peek() == Some(b'{') {
            let child = parse_element_with_kind(lex, key, unsupported)?;
            children.push(child);
        } else if lex.peek() == Some(b'=') {
            // Callback handler `clicked => { ... }` — intent we don't yet lower.
            let line = lex.line;
            // consume '=' '>' then a balanced `{ ... }` if present, else to ';'
            lex.advance();
            lex.skip_ws_and_comments();
            if lex.peek() == Some(b'>') {
                lex.advance();
            }
            lex.skip_ws_and_comments();
            if lex.peek() == Some(b'{') {
                lex.advance();
                skip_balanced_braces(lex);
            } else {
                skip_to_semicolon(lex);
            }
            unsupported.push(Unsupported {
                line,
                text: format!("callback handler '{key}'"),
            });
        } else {
            return Err(err(format!("expected ':' or '{{' after '{key}'"), lex.line));
        }
    }

    Ok(SlintElement {
        kind,
        props,
        children,
    })
}

/// Consume bytes up to and including the next `;` (or EOF).
fn skip_to_semicolon(lex: &mut Lexer<'_>) {
    while let Some(c) = lex.peek() {
        lex.advance();
        if c == b';' {
            break;
        }
    }
}

/// Consume a balanced `{ ... }` body, assuming the opening `{` was already eaten.
fn skip_balanced_braces(lex: &mut Lexer<'_>) {
    let mut depth = 1i32;
    while let Some(c) = lex.advance() {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
}

fn parse_file(lex: &mut Lexer<'_>) -> Result<SlintFile, SlintImportError> {
    let mut components: HashMap<String, SlintElement> = HashMap::new();
    let mut elements: Vec<SlintElement> = Vec::new();
    let mut unsupported: Vec<Unsupported> = Vec::new();

    loop {
        lex.skip_ws_and_comments();
        if lex.peek().is_none() {
            break;
        }

        let kw = lex.read_ident();
        if kw.is_empty() {
            break;
        }

        if kw == "component" {
            // component Name { ... }
            lex.skip_ws_and_comments();
            let name = lex.read_ident();
            if name.is_empty() {
                return Err(err("expected component name", lex.line));
            }
            // Optional 'inherits SomeBase' clause — we inline components as a
            // generic Stack, so the base relationship is dropped. Record it.
            lex.skip_ws_and_comments();
            if lex.peek() != Some(b'{') {
                let line = lex.line;
                let _clause = lex.read_ident(); // 'inherits'
                lex.skip_ws_and_comments();
                let base = lex.read_ident(); // base name
                lex.skip_ws_and_comments();
                if !base.is_empty() {
                    unsupported.push(Unsupported {
                        line,
                        text: format!("inherits {base}"),
                    });
                }
            }
            let body = parse_element_with_kind(lex, "__component__".into(), &mut unsupported)?;
            components.insert(name, body);
        } else {
            // Top-level element instance
            lex.skip_ws_and_comments();
            let elem = parse_element_with_kind(lex, kw, &mut unsupported)?;
            elements.push(elem);
        }
    }

    Ok(SlintFile {
        components,
        elements,
        unsupported,
    })
}

// ──────────────────────────────────────────────────────── lowering to IR ─────

fn map_prop_name(slint_name: &str) -> String {
    match slint_name {
        "text" => "content".into(),
        "font-size" => "size".into(),
        "border-radius" => "corner_radius".into(),
        other => other.replace('-', "_"),
    }
}

fn named_color(name: &str) -> Option<u32> {
    match name {
        "white" => Some(0xFFFF_FFFF),
        "black" => Some(0x0000_00FF),
        "red" => Some(0xFF00_00FF),
        "green" => Some(0x00FF_00FF),
        "blue" => Some(0x0000_FFFF),
        "transparent" => Some(0x0000_0000),
        _ => None,
    }
}

fn slint_value_to_ir(v: SlintValue) -> Value {
    match v {
        SlintValue::Px(f) => Value::Px(f),
        SlintValue::Color(c) => Value::Color(c),
        SlintValue::Str(s) => Value::Text(s),
        SlintValue::Bool(b) => Value::Bool(b),
        SlintValue::Float(f) => Value::Float(f),
        SlintValue::Ident(name) => {
            if let Some(c) = named_color(&name) {
                Value::Color(c)
            } else {
                Value::Text(name)
            }
        }
    }
}

fn lower_element(
    elem: &SlintElement,
    doc: &mut Document,
    components: &HashMap<String, SlintElement>,
) -> Option<NodeId> {
    // Resolve component inline: if the element's kind matches a declared component,
    // merge the component body's props (use-site overrides) and prepend its children.
    let (effective_kind, extra_props, extra_children): (String, Vec<_>, Vec<_>) =
        if let Some(comp) = components.get(&elem.kind) {
            (
                // Components render as a Stack by default (generic container)
                "Stack".into(),
                comp.props.clone(),
                comp.children.clone(),
            )
        } else {
            (elem.kind.clone(), vec![], vec![])
        };

    let id = doc.fresh_id();
    doc.apply_from(
        Origin::System,
        Mutation::CreateNode {
            id,
            kind: effective_kind,
        },
    )
    .ok()?;

    // Component base props first (use-site overrides them below).
    for (k, v) in extra_props {
        let key = map_prop_name(&k);
        doc.apply_from(
            Origin::System,
            Mutation::SetProp {
                id,
                key,
                value: slint_value_to_ir(v),
            },
        )
        .ok();
    }
    // Use-site props.
    for (k, v) in &elem.props {
        let key = map_prop_name(k);
        doc.apply_from(
            Origin::System,
            Mutation::SetProp {
                id,
                key,
                value: slint_value_to_ir(v.clone()),
            },
        )
        .ok();
    }

    // Component base children first.
    for child_elem in &extra_children {
        if let Some(child_id) = lower_element(child_elem, doc, components) {
            doc.apply_from(
                Origin::System,
                Mutation::AppendChild {
                    parent: id,
                    child: child_id,
                },
            )
            .ok();
        }
    }
    // Use-site children.
    for child_elem in &elem.children {
        if let Some(child_id) = lower_element(child_elem, doc, components) {
            doc.apply_from(
                Origin::System,
                Mutation::AppendChild {
                    parent: id,
                    child: child_id,
                },
            )
            .ok();
        }
    }

    Some(id)
}

// ────────────────────────────────────────────────────────────── public API ───

/// Parse a Slint DSL source string and return a `uni_ir::Document`.
///
/// The first top-level element (non-component) becomes the document root.
/// Component declarations are inlined at their use sites.
///
/// This is the lossy-by-design front door: dropped constructs are discarded.
/// Use [`parse_with_report`] when you need to know what was lost.
pub fn parse(src: &str) -> Result<Document, SlintImportError> {
    Ok(parse_with_report(src)?.document)
}

/// Parse a Slint DSL source string into a [`Document`] *and* a list of the
/// constructs that were recognized but dropped rather than lowered.
///
/// Additive sibling to [`parse`]: same lowering, but the [`ImportReport`] also
/// surfaces every [`Unsupported`] drop (two-way bindings, callback handlers,
/// `inherits` base relationships) with its source line.
pub fn parse_with_report(src: &str) -> Result<ImportReport, SlintImportError> {
    let mut lex = Lexer::new(src);
    let file = parse_file(&mut lex)?;
    let mut doc = Document::new();

    let mut root_set = false;
    for elem in &file.elements {
        if let Some(id) = lower_element(elem, &mut doc, &file.components) {
            if !root_set {
                doc.apply_from(Origin::System, Mutation::SetRoot { id })
                    .map_err(|e| err(format!("{e:?}"), 0))?;
                root_set = true;
            }
        }
    }

    Ok(ImportReport {
        document: doc,
        unsupported: file.unsupported,
    })
}

// ─────────────────────────────────────────────────────────────────── tests ───

#[cfg(test)]
mod tests {
    use super::*;
    use uni_ir::Value;

    fn root_prop(src: &str, key: &str) -> Option<Value> {
        let doc = parse(src).expect("parse ok");
        let root = doc.root()?;
        doc.get(root)?.props.get(key).cloned()
    }

    #[test]
    fn parse_empty_string_returns_empty_doc() {
        let doc = parse("").unwrap();
        assert!(doc.root().is_none());
    }

    #[test]
    fn parse_single_element_becomes_root() {
        let doc = parse("Rectangle {}").unwrap();
        let root = doc.root().expect("root set");
        assert_eq!(doc.get(root).unwrap().kind, "Rectangle");
    }

    #[test]
    fn prop_text_maps_to_content() {
        let v = root_prop(r#"Text { text: "Hello"; }"#, "content");
        assert_eq!(v, Some(Value::Text("Hello".into())));
    }

    #[test]
    fn prop_font_size_maps_to_size() {
        let v = root_prop("Text { font-size: 18px; }", "size");
        assert_eq!(v, Some(Value::Px(18.0)));
    }

    #[test]
    fn prop_border_radius_maps_to_corner_radius() {
        let v = root_prop("Rectangle { border-radius: 8px; }", "corner_radius");
        assert_eq!(v, Some(Value::Px(8.0)));
    }

    #[test]
    fn color_hex_6_digit_gets_ff_alpha() {
        let v = root_prop("Rectangle { background: #7D39EB; }", "background");
        assert_eq!(v, Some(Value::Color(0x7D39_EBFF)));
    }

    #[test]
    fn color_hex_8_digit_kept_as_is() {
        let v = root_prop("Rectangle { background: #7D39EBCC; }", "background");
        assert_eq!(v, Some(Value::Color(0x7D39_EBCC)));
    }

    #[test]
    fn color_name_white_is_0xffffffff() {
        let v = root_prop("Rectangle { background: white; }", "background");
        assert_eq!(v, Some(Value::Color(0xFFFF_FFFF)));
    }

    #[test]
    fn nested_element_becomes_child() {
        let doc = parse(r#"Stack { Text { text: "Hi"; } }"#).unwrap();
        let root = doc.root().unwrap();
        let children = &doc.get(root).unwrap().children;
        assert_eq!(children.len(), 1);
        let child = doc.get(children[0]).unwrap();
        assert_eq!(child.kind, "Text");
        assert_eq!(child.props.get("content"), Some(&Value::Text("Hi".into())));
    }

    #[test]
    fn component_decl_is_inlined_on_use() {
        let src = r#"
            component MyBox {
                border-radius: 12px;
                Text { text: "inside"; }
            }
            MyBox {}
        "#;
        let doc = parse(src).unwrap();
        let root = doc.root().unwrap();
        // Component body props should be on the inlined node.
        assert_eq!(
            doc.get(root).unwrap().props.get("corner_radius"),
            Some(&Value::Px(12.0))
        );
        // Component body children should be inlined.
        assert_eq!(doc.get(root).unwrap().children.len(), 1);
    }

    #[test]
    fn px_value_parsed_correctly() {
        let v = root_prop("Stack { width: 200px; }", "width");
        assert_eq!(v, Some(Value::Px(200.0)));
    }

    #[test]
    fn bool_value_parsed() {
        let v = root_prop("Input { enabled: true; }", "enabled");
        assert_eq!(v, Some(Value::Bool(true)));
    }

    #[test]
    fn line_comments_are_ignored() {
        let src = r#"
            // This is a comment
            Stack {
                // Another comment
                width: 100px; // trailing
            }
        "#;
        let v = root_prop(src, "width");
        assert_eq!(v, Some(Value::Px(100.0)));
    }

    #[test]
    fn prop_name_dash_stripped() {
        // A prop with dashes not in the special mapping gets dashes→underscores.
        let v = root_prop("Stack { min-width: 50px; }", "min_width");
        assert_eq!(v, Some(Value::Px(50.0)));
    }

    #[test]
    fn multiple_children_in_order() {
        let src = r#"Stack { Text { text: "a"; } Text { text: "b"; } }"#;
        let doc = parse(src).unwrap();
        let root = doc.root().unwrap();
        let children = &doc.get(root).unwrap().children;
        assert_eq!(children.len(), 2);
        assert_eq!(
            doc.get(children[0]).unwrap().props.get("content"),
            Some(&Value::Text("a".into()))
        );
        assert_eq!(
            doc.get(children[1]).unwrap().props.get("content"),
            Some(&Value::Text("b".into()))
        );
    }

    // ── E1: unsupported-construct reporting ───────────────────────────────────

    #[test]
    fn clean_input_reports_no_unsupported() {
        let report = parse_with_report(r#"Text { text: "Hi"; }"#).unwrap();
        assert!(report.unsupported.is_empty());
        assert!(report.document.root().is_some());
    }

    #[test]
    fn callback_handler_is_recorded_not_errored() {
        // The old parser would have errored on `=>`; now it's dropped + reported.
        let src = r#"Button { text: "Go"; clicked => { do_thing(); } }"#;
        let report = parse_with_report(src).unwrap();
        assert_eq!(report.unsupported.len(), 1);
        assert!(report.unsupported[0].text.contains("clicked"));
        // The supported prop still lowered.
        let root = report.document.root().unwrap();
        assert_eq!(
            report.document.get(root).unwrap().props.get("content"),
            Some(&Value::Text("Go".into()))
        );
    }

    #[test]
    fn two_way_binding_is_recorded() {
        let src = r#"Input { value: <=> model.value; }"#;
        let report = parse_with_report(src).unwrap();
        assert_eq!(report.unsupported.len(), 1);
        assert!(report.unsupported[0].text.contains("two-way"));
    }

    #[test]
    fn inherits_clause_is_recorded() {
        let src = r#"
            component MyButton inherits Rectangle {
                border-radius: 4px;
            }
            MyButton {}
        "#;
        let report = parse_with_report(src).unwrap();
        assert!(report
            .unsupported
            .iter()
            .any(|u| u.text == "inherits Rectangle"));
    }

    #[test]
    fn parse_still_returns_bare_document() {
        // Back-compat: `parse` signature unchanged, drops are silent.
        let doc = parse(r#"Button { clicked => {} }"#).unwrap();
        assert!(doc.root().is_some());
    }
}
