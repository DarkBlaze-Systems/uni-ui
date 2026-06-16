//! # flutter-import — clean-room Flutter/Dart widget importer
//!
//! Lowers a subset of the Flutter/Dart widget-tree notation into a
//! `uni_ir::Document`. This is a **clean-room** implementation built solely
//! from Flutter's published public documentation; no Flutter or Dart SDK
//! source is read, copied, or derived from.
//!
//! ## Supported syntax
//! - `WidgetName(prop: value, prop: value)` — widget call with named props
//! - `WidgetName(child: Widget(...))` — single child via `child:`
//! - `WidgetName(children: [W1(...), W2(...)])` — multiple children via `children:`
//! - `'string'` or `"string"` → `Value::Text`
//! - `Color(0xAARRGGBB)` → `Value::Color` in RRGGBBAA layout
//! - `N.0` or `N` (bare number) → `Value::Float` or `Value::Int`
//! - `true` / `false` → `Value::Bool`
//!
//! ## Widget kind mapping
//! | Flutter                           | uni-ir   |
//! |-----------------------------------|----------|
//! | `Text`                            | `Text`   |
//! | `Column`                          | `Column` |
//! | `Row`                             | `Row`    |
//! | `Stack`                           | `Stack`  |
//! | `Container`                       | `Stack`  |
//! | `ElevatedButton`, `TextButton`, `FilledButton` | `Button` |
//! | `SizedBox`                        | `Rect`   |
//! | `Scaffold`                        | `Stack`  |
//! | `Padding`                         | `Stack`  |
//! | everything else                   | kept as-is |

use uni_ir::{Action, Document, Mutation, NodeId, Origin, Value};

// ──────────────────────────────────────────────────────────────────── errors ──

/// Parse or lowering error from the Flutter importer.
#[derive(Debug)]
pub struct FlutterImportError {
    pub message: String,
    pub line: usize,
}

impl std::fmt::Display for FlutterImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "flutter-import error at line {}: {}",
            self.line, self.message
        )
    }
}

impl std::error::Error for FlutterImportError {}

fn err(msg: impl Into<String>, line: usize) -> FlutterImportError {
    FlutterImportError {
        message: msg.into(),
        line,
    }
}

// ─────────────────────────────────────────────────────────── unsupported ──────

/// A widget prop the importer recognized but deliberately *dropped* rather than
/// lower into the IR.
///
/// Flutter carries a lot of structured argument values (`EdgeInsets.all(8)`,
/// `MainAxisAlignment.center`, decorations, …) that have no literal home in our
/// vocabulary yet. Instead of swallowing them, each drop is recorded here so the
/// caller — and the AI companion driving a port — can see what was lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    /// 1-based source line where the dropped construct began.
    pub line: usize,
    /// A short description of what was dropped (e.g. `"prop 'padding' = EdgeInsets"`).
    pub text: String,
}

/// The full result of [`parse_with_report`]: the lowered document plus the list
/// of props that were dropped on the floor.
#[derive(Debug)]
pub struct ImportReport {
    pub document: Document,
    pub unsupported: Vec<Unsupported>,
}

// ────────────────────────────────────────────────────────────────── raw AST ──

/// A single parsed Flutter value (before mapping to uni-ir `Value`).
#[derive(Debug, Clone)]
enum FVal {
    Str(String),
    Color(u32), // already converted to RRGGBBAA
    Float(f64),
    Int(i64),
    Bool(bool),
    Widget(FWidget),
    List(Vec<FVal>),
    Ident(String),
}

/// A parsed Flutter widget call: `WidgetName(prop: value, ...)`.
#[derive(Debug, Clone)]
struct FWidget {
    kind: String,
    /// Named arguments / props.  `child` and `children` are stored here
    /// initially and extracted during lowering.
    args: Vec<(String, FVal)>,
    /// Positional string arg (e.g. `Text("hello")`).
    positional: Option<String>,
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

    fn skip_ws(&mut self) {
        while self
            .peek()
            .map(|c| c.is_ascii_whitespace())
            .unwrap_or(false)
        {
            self.advance();
        }
    }

    fn read_ident(&mut self) -> String {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'_' {
                s.push(c as char);
                self.advance();
            } else {
                break;
            }
        }
        s
    }

    fn read_string(&mut self, quote: u8) -> Result<String, FlutterImportError> {
        self.advance(); // consume opening quote
        let mut s = String::new();
        loop {
            match self.advance() {
                None => return Err(err("unterminated string literal", self.line)),
                Some(b) if b == quote => break,
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

    fn read_number(&mut self) -> (f64, bool) {
        // Returns (value, is_float)
        let mut s = String::new();
        let mut is_float = false;
        // Optional leading minus
        if self.peek() == Some(b'-') {
            s.push('-');
            self.advance();
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                s.push(c as char);
                self.advance();
            } else if c == b'.' && !is_float {
                // Look ahead: must be digit after dot to be float
                if self
                    .src
                    .get(self.pos + 1)
                    .map(|d| d.is_ascii_digit())
                    .unwrap_or(false)
                {
                    is_float = true;
                    s.push(c as char);
                    self.advance();
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        (s.parse().unwrap_or(0.0), is_float)
    }

    fn expect_byte(&mut self, expected: u8) -> Result<(), FlutterImportError> {
        self.skip_ws();
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

// ─────────────────────────────────────── parser (recursive descent) ──────────

/// Parse one value: string, Color(...), number, bool, widget, list.
fn parse_value(lex: &mut Lexer<'_>) -> Result<FVal, FlutterImportError> {
    lex.skip_ws();
    match lex.peek() {
        Some(b'"') => Ok(FVal::Str(lex.read_string(b'"')?)),
        Some(b'\'') => Ok(FVal::Str(lex.read_string(b'\'')?)),
        Some(b'[') => {
            lex.advance(); // consume '['
            let mut items = Vec::new();
            loop {
                lex.skip_ws();
                if lex.peek() == Some(b']') {
                    lex.advance();
                    break;
                }
                if lex.peek().is_none() {
                    return Err(err("unexpected EOF in list", lex.line));
                }
                items.push(parse_value(lex)?);
                lex.skip_ws();
                if lex.peek() == Some(b',') {
                    lex.advance();
                }
            }
            Ok(FVal::List(items))
        }
        Some(c) if c.is_ascii_digit() || c == b'-' => {
            let (n, is_float) = lex.read_number();
            if is_float {
                Ok(FVal::Float(n))
            } else {
                Ok(FVal::Int(n as i64))
            }
        }
        Some(_) => {
            let ident = lex.read_ident();
            match ident.as_str() {
                "true" => return Ok(FVal::Bool(true)),
                "false" => return Ok(FVal::Bool(false)),
                "" => return Err(err("expected a value", lex.line)),
                _ => {}
            }
            lex.skip_ws();
            if lex.peek() == Some(b'(') {
                // Either Color(0x...) or Widget(...)
                lex.advance(); // consume '('
                if ident == "Color" {
                    // Color(0xAARRGGBB)
                    lex.skip_ws();
                    let color = parse_color_literal(lex)?;
                    lex.skip_ws();
                    // tolerate trailing comma before ')'
                    if lex.peek() == Some(b',') {
                        lex.advance();
                    }
                    lex.skip_ws();
                    lex.expect_byte(b')')?;
                    Ok(FVal::Color(color))
                } else if ident == "TextStyle" {
                    // TextStyle(fontSize: 18.0, color: Color(0xFF...))
                    let args = parse_arg_list(lex)?;
                    Ok(FVal::Widget(FWidget {
                        kind: "TextStyle".into(),
                        args,
                        positional: None,
                    }))
                } else {
                    // Widget call
                    let widget = parse_widget_body(lex, ident)?;
                    Ok(FVal::Widget(widget))
                }
            } else {
                Ok(FVal::Ident(ident))
            }
        }
        None => Err(err("unexpected EOF while parsing value", lex.line)),
    }
}

/// Parse `0xAARRGGBB` hex literal and return as RRGGBBAA.
fn parse_color_literal(lex: &mut Lexer<'_>) -> Result<u32, FlutterImportError> {
    lex.skip_ws();
    // expect '0x' or '0X'
    if lex.peek() == Some(b'0') {
        lex.advance();
        match lex.peek() {
            Some(b'x') | Some(b'X') => {
                lex.advance();
            }
            _ => return Err(err("expected '0x' for Color literal", lex.line)),
        }
    } else {
        return Err(err("expected '0x' for Color literal", lex.line));
    }
    let mut hex = String::new();
    while let Some(c) = lex.peek() {
        if c.is_ascii_hexdigit() {
            hex.push(c as char);
            lex.advance();
        } else {
            break;
        }
    }
    let argb =
        u32::from_str_radix(&hex, 16).map_err(|_| err("invalid hex in Color literal", lex.line))?;
    // Flutter: AARRGGBB → uni-ir: RRGGBBAA
    let rrggbbaa = (argb & 0x00FF_FFFF) << 8 | (argb >> 24);
    Ok(rrggbbaa)
}

/// Parse the argument list inside `(...)` that was already opened.
/// Handles: positional string first, then `name: value` pairs, comma-separated.
fn parse_arg_list(lex: &mut Lexer<'_>) -> Result<Vec<(String, FVal)>, FlutterImportError> {
    let mut args = Vec::new();
    loop {
        lex.skip_ws();
        if lex.peek() == Some(b')') {
            lex.advance();
            break;
        }
        if lex.peek().is_none() {
            return Err(err("unexpected EOF in argument list", lex.line));
        }
        // Peek ahead: if it's `ident:` then named arg, otherwise positional value
        let saved_pos = lex.pos;
        let saved_line = lex.line;
        let candidate = lex.read_ident();
        lex.skip_ws();
        if !candidate.is_empty() && lex.peek() == Some(b':') {
            lex.advance(); // consume ':'
            let val = parse_value(lex)?;
            args.push((candidate, val));
        } else {
            // Not a named arg — restore and parse as positional value
            lex.pos = saved_pos;
            lex.line = saved_line;
            let val = parse_value(lex)?;
            args.push(("__positional__".into(), val));
        }
        lex.skip_ws();
        if lex.peek() == Some(b',') {
            lex.advance();
        }
    }
    Ok(args)
}

/// Parse the body of a widget call after the `(` has been consumed.
fn parse_widget_body(lex: &mut Lexer<'_>, kind: String) -> Result<FWidget, FlutterImportError> {
    let args = parse_arg_list(lex)?;
    // Separate positional string arg from named args
    let mut named = Vec::new();
    let mut positional = None;
    for (k, v) in args {
        if k == "__positional__" {
            if let FVal::Str(s) = v {
                positional = Some(s);
            }
            // non-string positionals are dropped
        } else {
            named.push((k, v));
        }
    }
    Ok(FWidget {
        kind,
        args: named,
        positional,
    })
}

/// Parse a top-level widget: `WidgetName(...)`.
fn parse_widget(lex: &mut Lexer<'_>) -> Result<Option<FWidget>, FlutterImportError> {
    lex.skip_ws();
    if lex.peek().is_none() {
        return Ok(None);
    }
    let kind = lex.read_ident();
    if kind.is_empty() {
        return Ok(None);
    }
    lex.skip_ws();
    lex.expect_byte(b'(')?;
    let widget = parse_widget_body(lex, kind)?;
    Ok(Some(widget))
}

// ──────────────────────────────────────────────────────────── kind mapping ───

fn map_kind(flutter_kind: &str) -> &str {
    match flutter_kind {
        "Text" => "Text",
        "Column" => "Column",
        "Row" => "Row",
        "Stack" => "Stack",
        "Container" | "Scaffold" | "Padding" => "Stack",
        "ElevatedButton" | "TextButton" | "FilledButton" => "Button",
        "SizedBox" => "Rect",
        other => other,
    }
}

// ──────────────────────────────────────────────────────────── prop mapping ───

/// Map a Flutter prop name to a uni-ir prop name (or return None if it should
/// be handled as a structural element: child / children).
fn map_prop(flutter_prop: &str) -> Option<&str> {
    match flutter_prop {
        "child" | "children" => None, // structural — handled separately
        "text" | "data" => Some("content"),
        "fontSize" | "font-size" => Some("size"),
        "color" | "foregroundColor" => Some("color"),
        "backgroundColor" => Some("background"),
        "borderRadius" => Some("corner_radius"),
        "width" => Some("width"),
        "height" => Some("height"),
        "onPressed" | "onTap" => None, // callback — handled separately
        "style" => None,               // inline expansion — handled separately
        "padding" => Some("padding"),
        _ => Some(flutter_prop),
    }
}

fn fval_to_ir(v: FVal) -> Option<Value> {
    match v {
        FVal::Str(s) => Some(Value::Text(s)),
        FVal::Color(c) => Some(Value::Color(c)),
        FVal::Float(f) => Some(Value::Float(f)),
        FVal::Int(i) => Some(Value::Int(i)),
        FVal::Bool(b) => Some(Value::Bool(b)),
        FVal::Ident(_) | FVal::Widget(_) | FVal::List(_) => None,
    }
}

// ──────────────────────────────────────────────────────────── lowering ────────

/// Describe an `FVal` for an unsupported-drop report (no value content, just shape).
fn fval_shape(v: &FVal) -> &'static str {
    match v {
        FVal::Str(_) => "string",
        FVal::Color(_) => "color",
        FVal::Float(_) => "float",
        FVal::Int(_) => "int",
        FVal::Bool(_) => "bool",
        FVal::Widget(_) => "widget-expr",
        FVal::List(_) => "list",
        FVal::Ident(_) => "ident-expr",
    }
}

fn lower_widget(
    widget: &FWidget,
    doc: &mut Document,
    unsupported: &mut Vec<Unsupported>,
) -> Result<NodeId, FlutterImportError> {
    let kind = map_kind(&widget.kind).to_string();
    let id = doc.fresh_id();
    doc.apply_from(Origin::System, Mutation::CreateNode { id, kind })
        .map_err(|e| err(format!("{e:?}"), 0))?;

    // Handle positional string (e.g. Text("hello"))
    if let Some(ref s) = widget.positional {
        doc.apply_from(
            Origin::System,
            Mutation::SetProp {
                id,
                key: "content".into(),
                value: Value::Text(s.clone()),
            },
        )
        .ok();
    }

    // Process named args
    for (key, val) in &widget.args {
        match key.as_str() {
            "child" => {
                if let FVal::Widget(child_w) = val {
                    let child_id = lower_widget(child_w, doc, unsupported)?;
                    doc.apply_from(
                        Origin::System,
                        Mutation::AppendChild {
                            parent: id,
                            child: child_id,
                        },
                    )
                    .map_err(|e| err(format!("{e:?}"), 0))?;
                }
            }
            "children" => {
                if let FVal::List(items) = val {
                    for item in items {
                        if let FVal::Widget(child_w) = item {
                            let child_id = lower_widget(child_w, doc, unsupported)?;
                            doc.apply_from(
                                Origin::System,
                                Mutation::AppendChild {
                                    parent: id,
                                    child: child_id,
                                },
                            )
                            .map_err(|e| err(format!("{e:?}"), 0))?;
                        }
                    }
                }
            }
            "onPressed" | "onTap" => {
                let action_name = match val {
                    FVal::Str(s) => s.clone(),
                    FVal::Ident(s) => s.clone(),
                    _ => key.clone(),
                };
                doc.apply_from(
                    Origin::System,
                    Mutation::SetCallback {
                        id,
                        event: "click".into(),
                        action: Action {
                            name: action_name,
                            args: vec![],
                        },
                    },
                )
                .ok();
            }
            "style" => {
                // TextStyle(...) or similar — extract inner props
                if let FVal::Widget(style_w) = val {
                    for (sk, sv) in &style_w.args {
                        if let Some(ir_key) = map_prop(sk) {
                            if let Some(ir_val) = fval_to_ir(sv.clone()) {
                                doc.apply_from(
                                    Origin::System,
                                    Mutation::SetProp {
                                        id,
                                        key: ir_key.into(),
                                        value: ir_val,
                                    },
                                )
                                .ok();
                            }
                        }
                    }
                }
            }
            other => {
                if let Some(ir_key) = map_prop(other) {
                    // Context-sensitive: `color` on a Container/Scaffold/Padding is a
                    // surface FILL (→ background), whereas `color` on Text is the
                    // foreground ink. uni-core kinds for those containers are layout
                    // kinds, so anything that is NOT a Text gets color→background.
                    let ir_key = if other == "color" && widget.kind != "Text" {
                        "background"
                    } else {
                        ir_key
                    };
                    if let Some(ir_val) = fval_to_ir(val.clone()) {
                        doc.apply_from(
                            Origin::System,
                            Mutation::SetProp {
                                id,
                                key: ir_key.into(),
                                value: ir_val,
                            },
                        )
                        .ok();
                    } else {
                        // A mapped prop whose value has no IR literal — e.g.
                        // `padding: EdgeInsets.all(8)` or an enum like
                        // `mainAxisAlignment: MainAxisAlignment.center`.
                        unsupported.push(Unsupported {
                            line: 0,
                            text: format!("prop '{other}' = {}", fval_shape(val)),
                        });
                    }
                }
            }
        }
    }

    Ok(id)
}

// ────────────────────────────────────────────────────────── public API ────────

/// Parse a Flutter/Dart widget-tree source string and return a
/// `uni_ir::Document`.
///
/// The outermost widget becomes the document root. The parser is hand-rolled
/// recursive descent; no external parser dependencies are used.
///
/// This is the lossy-by-design front door: dropped props are discarded. Use
/// [`parse_with_report`] when you need to know what was lost.
pub fn parse(src: &str) -> Result<Document, FlutterImportError> {
    Ok(parse_with_report(src)?.document)
}

/// Parse a Flutter/Dart widget-tree source string into a [`Document`] *and* a
/// list of the props that were recognized but dropped rather than lowered.
///
/// Additive sibling to [`parse`]: same lowering, but the [`ImportReport`] also
/// surfaces every [`Unsupported`] drop (structured argument values with no IR
/// literal, enum constants, decorations).
pub fn parse_with_report(src: &str) -> Result<ImportReport, FlutterImportError> {
    let mut lex = Lexer::new(src);
    let mut doc = Document::new();
    let mut unsupported = Vec::new();

    if let Some(widget) = parse_widget(&mut lex)? {
        let root_id = lower_widget(&widget, &mut doc, &mut unsupported)?;
        doc.apply_from(Origin::System, Mutation::SetRoot { id: root_id })
            .map_err(|e| err(format!("{e:?}"), 0))?;
    }

    Ok(ImportReport {
        document: doc,
        unsupported,
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

    fn root_kind(src: &str) -> String {
        let doc = parse(src).expect("parse ok");
        let root = doc.root().expect("root");
        doc.get(root).expect("node").kind.clone()
    }

    // 1. Text widget round-trips its string via `content`
    #[test]
    fn parse_text_widget() {
        let v = root_prop(r#"Text("Hello world")"#, "content");
        assert_eq!(v, Some(Value::Text("Hello world".into())));
    }

    // 2. Column with multiple children via `children:`
    #[test]
    fn parse_column_with_children() {
        let src = r#"Column(children: [Text("a"), Text("b")])"#;
        let doc = parse(src).expect("parse ok");
        let root = doc.root().unwrap();
        assert_eq!(doc.get(root).unwrap().kind, "Column");
        assert_eq!(doc.get(root).unwrap().children.len(), 2);
    }

    // 3. Color(0xAARRGGBB) converted to RRGGBBAA
    #[test]
    fn parse_color_argb_to_rrggbba() {
        // 0xFF_E040_FB  →  AA=FF RR=E0 GG=40 BB=FB
        // RRGGBBAA = E040_FBFF
        let src = r#"Container(color: Color(0xFFE040FB))"#;
        let v = root_prop(src, "background");
        assert_eq!(v, Some(Value::Color(0xE040_FBFF)));
    }

    // 4. Container maps to Stack
    #[test]
    fn parse_container_maps_to_stack() {
        assert_eq!(root_kind("Container()"), "Stack");
    }

    // 5. ElevatedButton maps to Button
    #[test]
    fn parse_elevated_button_maps_to_button() {
        assert_eq!(root_kind("ElevatedButton(onPressed: submit)"), "Button");
    }

    // 6. Nested child via `child:`
    #[test]
    fn parse_nested_child() {
        let src = r#"Container(child: Text("inside"))"#;
        let doc = parse(src).expect("parse ok");
        let root = doc.root().unwrap();
        let children = &doc.get(root).unwrap().children;
        assert_eq!(children.len(), 1);
        let child = doc.get(children[0]).unwrap();
        assert_eq!(child.kind, "Text");
        assert_eq!(
            child.props.get("content"),
            Some(&Value::Text("inside".into()))
        );
    }

    // 7. Both single-quote and double-quote strings parse to Text
    #[test]
    fn parse_string_single_and_double_quotes() {
        let v1 = root_prop(r#"Text("double")"#, "content");
        let v2 = root_prop("Text('single')", "content");
        assert_eq!(v1, Some(Value::Text("double".into())));
        assert_eq!(v2, Some(Value::Text("single".into())));
    }

    // 8. Bool value parsed
    #[test]
    fn parse_bool_value() {
        let src = "SizedBox(enabled: true)";
        let v = root_prop(src, "enabled");
        assert_eq!(v, Some(Value::Bool(true)));
    }

    // 9. onPressed becomes a callback named "click"
    #[test]
    fn parse_on_pressed_becomes_callback() {
        let src = r#"ElevatedButton(onPressed: 'handleSubmit')"#;
        let doc = parse(src).expect("parse ok");
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        let cb = node.callbacks.get("click");
        assert!(cb.is_some(), "expected 'click' callback");
        assert_eq!(cb.unwrap().name, "handleSubmit");
    }

    // 10. TextStyle style: extracts fontSize → size
    #[test]
    fn parse_text_style_extracts_font_size() {
        let src = r#"Text("Hi", style: TextStyle(fontSize: 24.0))"#;
        let v = root_prop(src, "size");
        assert_eq!(v, Some(Value::Float(24.0)));
    }

    // Bonus: SizedBox maps to Rect
    #[test]
    fn parse_sized_box_maps_to_rect() {
        assert_eq!(root_kind("SizedBox(width: 100.0, height: 50.0)"), "Rect");
    }

    // Bonus: width and height props preserved
    #[test]
    fn parse_width_height_props() {
        let src = "SizedBox(width: 200.0, height: 100.0)";
        assert_eq!(root_prop(src, "width"), Some(Value::Float(200.0)));
        assert_eq!(root_prop(src, "height"), Some(Value::Float(100.0)));
    }

    // Bonus: Row maps to Row
    #[test]
    fn parse_row_widget() {
        assert_eq!(root_kind("Row(children: [])"), "Row");
    }

    // Bonus: Scaffold maps to Stack
    #[test]
    fn parse_scaffold_maps_to_stack() {
        assert_eq!(root_kind("Scaffold()"), "Stack");
    }

    // Bonus: integer value parsed as Int
    #[test]
    fn parse_integer_value() {
        let v = root_prop("SizedBox(borderRadius: 8)", "corner_radius");
        assert_eq!(v, Some(Value::Int(8)));
    }

    // ── E1: unsupported-construct reporting ───────────────────────────────────

    #[test]
    fn clean_input_reports_no_unsupported() {
        let report = parse_with_report(r#"Text("Hi")"#).unwrap();
        assert!(report.unsupported.is_empty());
        assert!(report.document.root().is_some());
    }

    #[test]
    fn enum_valued_prop_is_recorded() {
        // `mainAxisAlignment: center` — a bare ident with no IR literal.
        let src = r#"Column(mainAxisAlignment: center, children: [])"#;
        let report = parse_with_report(src).unwrap();
        assert_eq!(report.unsupported.len(), 1);
        assert!(report.unsupported[0].text.contains("mainAxisAlignment"));
        assert!(report.unsupported[0].text.contains("ident-expr"));
        // The Column itself still lowered.
        assert_eq!(
            report
                .document
                .get(report.document.root().unwrap())
                .unwrap()
                .kind,
            "Column"
        );
    }

    #[test]
    fn nested_drop_is_recorded_from_child() {
        // The drop happens on a child, proving the collector threads recursively.
        let src = r#"Container(child: Text("hi", maxLines: someVar))"#;
        let report = parse_with_report(src).unwrap();
        assert!(report
            .unsupported
            .iter()
            .any(|u| u.text.contains("maxLines")));
    }

    #[test]
    fn parse_still_returns_bare_document() {
        // Back-compat: `parse` signature unchanged, drops are silent.
        let doc = parse(r#"Column(mainAxisAlignment: center, children: [])"#).unwrap();
        assert!(doc.root().is_some());
    }
}
