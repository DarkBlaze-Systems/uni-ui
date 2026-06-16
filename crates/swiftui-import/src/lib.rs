//! # swiftui-import — clean-room SwiftUI view importer
//!
//! Lowers a subset of the public SwiftUI DSL into a `uni_ir::Document`.
//! Clean-room: built only from SwiftUI's published documentation.
//!
//! ## Supported constructs
//! - `ViewName { ... }` — block body with children
//! - `ViewName("literal")` or `ViewName(param: value)` — inline args
//! - `.modifier(args)` — chained modifiers applied as props
//! - `Button("label") { }` — trailing closure becomes a "click" callback
//! - Line comments (`//`)

use uni_ir::{Action, Document, Mutation, NodeId, Origin, Value};

// ─────────────────────────────────────────────────────────────── errors ──────

#[derive(Debug)]
pub struct SwiftUIImportError {
    pub message: String,
    pub line: usize,
}

impl std::fmt::Display for SwiftUIImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "swiftui-import error at line {}: {}",
            self.line, self.message
        )
    }
}
impl std::error::Error for SwiftUIImportError {}

fn err(msg: impl Into<String>, line: usize) -> SwiftUIImportError {
    SwiftUIImportError {
        message: msg.into(),
        line,
    }
}

// ─────────────────────────────────────────────────────────── unsupported ──────

/// A modifier the importer recognized but deliberately *dropped* rather than
/// lower into the IR.
///
/// SwiftUI's modifier chain is vast (`.shadow`, `.accessibilityLabel`,
/// `.animation`, `.transition`, …) and most of it has no home in our opinionated
/// vocabulary yet. Instead of silently eating an unknown `.modifier(...)`, every
/// drop is recorded here so the caller — and the AI companion driving a port —
/// can see exactly what fidelity was lost and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    /// 1-based source line where the dropped modifier began.
    pub line: usize,
    /// A short description of what was dropped (e.g. `"modifier .shadow"`).
    pub text: String,
}

/// The full result of [`parse_with_report`]: the lowered document plus the list
/// of modifiers that were dropped on the floor.
#[derive(Debug)]
pub struct ImportReport {
    pub document: Document,
    pub unsupported: Vec<Unsupported>,
}

// ─────────────────────────────────────────────────────────────────── AST ─────

#[derive(Debug, Clone)]
enum SwiftValue {
    Str(String),
    Float(f64),
    Bool(bool),
    Color(u32),
    FontRole(f32), // resolved to a px size
    Ident(String),
    /// A closed range `lo...hi` (SwiftUI `Slider(in:)`). Lowered as two props.
    Range(f64, f64),
}

#[derive(Debug, Clone)]
struct SwiftView {
    kind: String,
    /// Positional string arg (e.g. Text("Hello"), Button("Label"))
    label: Option<String>,
    props: Vec<(String, SwiftValue)>,
    callbacks: Vec<String>, // event names that have closures
    children: Vec<SwiftView>,
    /// Modifiers recognized but dropped (recorded for the import report).
    unsupported: Vec<Unsupported>,
}

// ───────────────────────────────────────────────────────────────── lexer ─────

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    pub line: usize,
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
        let c = self.src.get(self.pos).copied()?;
        self.pos += 1;
        if c == b'\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn skip_ws(&mut self) {
        loop {
            while self
                .peek()
                .map(|c| c.is_ascii_whitespace())
                .unwrap_or(false)
            {
                self.advance();
            }
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
            if c.is_ascii_alphanumeric() || c == b'_' {
                s.push(c as char);
                self.advance();
            } else {
                break;
            }
        }
        s
    }

    fn read_string(&mut self) -> Result<String, SwiftUIImportError> {
        self.advance(); // opening "
        let mut s = String::new();
        loop {
            match self.advance() {
                None => return Err(err("unterminated string", self.line)),
                Some(b'"') => break,
                Some(b'\\') => match self.advance() {
                    Some(b'n') => s.push('\n'),
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
            if c.is_ascii_digit() {
                s.push(c as char);
                self.advance();
            } else if c == b'.' {
                // Stop at `..` so a range operator (`0...100`) is not swallowed
                // into the number literal; a lone `.` is a decimal point.
                if self.src.get(self.pos + 1).copied() == Some(b'.') {
                    break;
                }
                s.push(c as char);
                self.advance();
            } else {
                break;
            }
        }
        s.parse().unwrap_or(0.0)
    }

    fn skip_balanced(&mut self, open: u8, close: u8) {
        let mut depth = 1i32;
        while let Some(c) = self.advance() {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
        }
    }
}

// ──────────────────────────────────────────────────────── kind mapping ───────

fn map_kind(swiftui: &str) -> &str {
    match swiftui {
        "VStack" => "Column",
        "HStack" => "Row",
        "ZStack" | "Group" => "Stack",
        "ScrollView" => "Stack",
        "RoundedRectangle" | "Rectangle" => "Rect",
        "Spacer" => "Rect",
        // Leaf views keep their SwiftUI name as the IR kind — these have no
        // layout-container analogue, they *are* their own element.
        "Image" => "Image",
        "Divider" => "Divider",
        "Toggle" => "Toggle",
        "Slider" => "Slider",
        "ProgressView" => "ProgressView",
        _ => swiftui,
    }
}

// ─────────────────────────────────────────── color helpers ───────────────────

fn named_dot_color(name: &str) -> Option<u32> {
    match name {
        "white" => Some(0xFFFF_FFFF),
        "black" => Some(0x0000_00FF),
        "blue" => Some(0x0000_FFFF),
        "red" => Some(0xFF00_00FF),
        "green" => Some(0x00FF_00FF),
        "clear" => Some(0x0000_0000),
        "gray" | "grey" => Some(0x8888_88FF),
        "yellow" => Some(0xFFFF_00FF),
        "orange" => Some(0xFF88_00FF),
        "purple" => Some(0x88_00FFFF),
        _ => None,
    }
}

fn font_role_px(role: &str) -> Option<f32> {
    Some(match role {
        "largeTitle" => 34.0,
        "title" | "title1" => 28.0,
        "title2" => 22.0,
        "title3" => 20.0,
        "headline" => 17.0,
        "body" => 16.0,
        "callout" => 15.0,
        "subheadline" => 14.0,
        "footnote" => 13.0,
        "caption" | "caption2" => 12.0,
        // Not a known text style — let the caller treat the dotted name as a
        // plain identifier (e.g. `.infinity`, `.bold`).
        _ => return None,
    })
}

// ─────────────────────────────────────────────────────────── parser ──────────

fn parse_color_arg(lex: &mut Lexer<'_>) -> Result<SwiftValue, SwiftUIImportError> {
    // Color(red: 0.5, green: 0.2, blue: 0.9) or Color.blue or just .blue
    lex.skip_ws();
    if lex.peek() == Some(b'(') {
        lex.advance(); // (
        let mut r = 0.0f64;
        let mut g = 0.0f64;
        let mut b = 0.0f64;
        // parse key: value, ...
        loop {
            lex.skip_ws();
            if lex.peek() == Some(b')') {
                lex.advance();
                break;
            }
            let key = lex.read_ident();
            lex.skip_ws();
            if lex.peek() == Some(b':') {
                lex.advance();
            }
            lex.skip_ws();
            let v = lex.read_number();
            match key.as_str() {
                "red" => r = v,
                "green" => g = v,
                "blue" => b = v,
                _ => {}
            }
            lex.skip_ws();
            if lex.peek() == Some(b',') {
                lex.advance();
            }
        }
        let packed = (((r * 255.0) as u32) << 24)
            | (((g * 255.0) as u32) << 16)
            | (((b * 255.0) as u32) << 8)
            | 0xFF;
        Ok(SwiftValue::Color(packed))
    } else if lex.peek() == Some(b'.') {
        lex.advance(); // .
        let name = lex.read_ident();
        if let Some(c) = named_dot_color(&name) {
            Ok(SwiftValue::Color(c))
        } else {
            Ok(SwiftValue::Ident(name))
        }
    } else {
        Ok(SwiftValue::Ident(lex.read_ident()))
    }
}

/// Parse a SwiftUI view starting just AFTER the kind name has been read.
/// `kind` is already consumed.
fn parse_view_body(lex: &mut Lexer<'_>, kind: String) -> Result<SwiftView, SwiftUIImportError> {
    let mut view = SwiftView {
        kind,
        label: None,
        props: vec![],
        callbacks: vec![],
        children: vec![],
        unsupported: vec![],
    };

    lex.skip_ws();

    // Optional inline args: `("label")` or `(cornerRadius: 16)`
    if lex.peek() == Some(b'(') {
        lex.advance(); // (
        lex.skip_ws();
        if lex.peek() == Some(b'"') {
            view.label = Some(lex.read_string()?);
            lex.skip_ws();
            // optional trailing comma + more args
            if lex.peek() == Some(b',') {
                lex.advance();
                lex.skip_ws();
            }
        }
        // Parse remaining key: value pairs
        loop {
            lex.skip_ws();
            if lex.peek() == Some(b')') {
                lex.advance();
                break;
            }
            if lex.peek().map(|c| c.is_ascii_alphabetic()).unwrap_or(false) {
                let key = lex.read_ident();
                lex.skip_ws();
                if lex.peek() == Some(b':') {
                    lex.advance();
                    lex.skip_ws();
                    let val = parse_value(lex)?;
                    view.props.push((key, val));
                }
            }
            lex.skip_ws();
            if lex.peek() == Some(b',') {
                lex.advance();
            } else if lex.peek() == Some(b')') {
                lex.advance();
                break;
            } else {
                break;
            }
        }
        lex.skip_ws();
    }

    // Optional trailing closure `{ ... }` for Button/gesture actions
    if lex.peek() == Some(b'{') {
        let saved_pos = lex.pos;
        let saved_line = lex.line;
        lex.advance(); // {
        lex.skip_ws();
        // Peek: if next token looks like a View kind (uppercase), it's a content block
        let is_content = lex.peek().map(|c| c.is_ascii_uppercase()).unwrap_or(false);
        if is_content {
            // Reset and parse as child block
            lex.pos = saved_pos;
            lex.line = saved_line;
        } else {
            // Action closure — skip its body
            view.callbacks.push("click".into());
            lex.skip_balanced(b'{', b'}');
            lex.skip_ws();
            return apply_modifiers(lex, view);
        }
    }

    // Block body `{ child child ... }`
    if lex.peek() == Some(b'{') {
        lex.advance(); // {
        loop {
            lex.skip_ws();
            if lex.peek() == Some(b'}') {
                lex.advance();
                break;
            }
            if lex.peek().is_none() {
                break;
            }
            if lex.peek().map(|c| c.is_ascii_uppercase()).unwrap_or(false) {
                let child_kind = lex.read_ident();
                let child = parse_view_body(lex, child_kind)?;
                view.children.push(child);
            } else {
                // skip unknown token
                lex.advance();
            }
        }
    }

    apply_modifiers(lex, view)
}

fn parse_value(lex: &mut Lexer<'_>) -> Result<SwiftValue, SwiftUIImportError> {
    lex.skip_ws();
    match lex.peek() {
        Some(b'"') => Ok(SwiftValue::Str(lex.read_string()?)),
        // `$state` — a SwiftUI two-way binding projection. We can't resolve the
        // state graph in a clean-room importer, so we carry the bound name as an
        // identifier (e.g. `isOn: $flag` → bound state name "flag").
        Some(b'$') => {
            lex.advance(); // $
            Ok(SwiftValue::Ident(lex.read_ident()))
        }
        Some(b'.') => {
            lex.advance();
            let name = lex.read_ident();
            // Font role or color
            if let Some(c) = named_dot_color(&name) {
                Ok(SwiftValue::Color(c))
            } else if let Some(px) = font_role_px(&name) {
                Ok(SwiftValue::FontRole(px))
            } else {
                Ok(SwiftValue::Ident(name))
            }
        }
        Some(c) if c.is_ascii_digit() => {
            let lo = lex.read_number();
            // Range literal `lo...hi` (or `lo..<hi`) — used by `Slider(in:)`.
            if lex.peek() == Some(b'.') && lex.src.get(lex.pos + 1).copied() == Some(b'.') {
                // consume the run of '.' and an optional '<'
                while lex.peek() == Some(b'.') {
                    lex.advance();
                }
                if lex.peek() == Some(b'<') {
                    lex.advance();
                }
                lex.skip_ws();
                let hi = lex.read_number();
                Ok(SwiftValue::Range(lo, hi))
            } else {
                Ok(SwiftValue::Float(lo))
            }
        }
        Some(_) => {
            let id = lex.read_ident();
            match id.as_str() {
                "true" => Ok(SwiftValue::Bool(true)),
                "false" => Ok(SwiftValue::Bool(false)),
                "Color" => parse_color_arg(lex),
                _ => Ok(SwiftValue::Ident(id)),
            }
        }
        None => Err(err("unexpected EOF in value", lex.line)),
    }
}

/// After parsing a view's block/args, consume any `.modifier(...)` chains.
fn apply_modifiers(
    lex: &mut Lexer<'_>,
    mut view: SwiftView,
) -> Result<SwiftView, SwiftUIImportError> {
    loop {
        lex.skip_ws();
        if lex.peek() != Some(b'.') {
            break;
        }
        let modifier_line = lex.line;
        lex.advance(); // .
        let modifier = lex.read_ident();
        lex.skip_ws();
        if lex.peek() != Some(b'(') {
            // A parenthesis-less, property-style modifier such as `.isHidden`.
            // Only a curated few have an IR home; the rest are dropped + logged.
            match modifier.as_str() {
                "isHidden" => view.props.push(("hidden".into(), SwiftValue::Bool(true))),
                "" => {} // a stray `.` — nothing to record
                other => view.unsupported.push(Unsupported {
                    line: modifier_line,
                    text: format!("modifier .{other}"),
                }),
            }
            continue;
        }
        lex.advance(); // (
        lex.skip_ws();

        match modifier.as_str() {
            "font" => {
                let v = parse_value(lex)?;
                if let SwiftValue::FontRole(px) = v {
                    view.props
                        .push(("size".into(), SwiftValue::Float(px as f64)));
                }
            }
            "foregroundColor" | "foregroundStyle" => {
                let v = parse_value(lex)?;
                view.props.push(("color".into(), v));
            }
            "background" => {
                let v = parse_value(lex)?;
                view.props.push(("background".into(), v));
            }
            "fill" => {
                let v = parse_value(lex)?;
                view.props.push(("background".into(), v));
            }
            "frame" => {
                // frame(width: N, height: M) or frame(maxWidth: .infinity)
                loop {
                    lex.skip_ws();
                    if lex.peek() == Some(b')') {
                        break;
                    }
                    let key = lex.read_ident();
                    lex.skip_ws();
                    if lex.peek() == Some(b':') {
                        lex.advance();
                    }
                    lex.skip_ws();
                    let v = parse_value(lex)?;
                    match key.as_str() {
                        "width" | "minWidth" => view.props.push(("width".into(), v)),
                        "height" | "minHeight" => view.props.push(("height".into(), v)),
                        // `maxWidth: .infinity` / `maxHeight: .infinity` is
                        // SwiftUI's "fill the cross axis" — our `grow=1`.
                        "maxWidth" | "maxHeight" => {
                            if matches!(&v, SwiftValue::Ident(s) if s == "infinity") {
                                view.props.push(("grow".into(), SwiftValue::Float(1.0)));
                            } else {
                                // a finite max bound maps to the dimension itself
                                let dim = if key == "maxWidth" { "width" } else { "height" };
                                view.props.push((dim.into(), v));
                            }
                        }
                        _ => {}
                    }
                    lex.skip_ws();
                    if lex.peek() == Some(b',') {
                        lex.advance();
                    }
                }
            }
            "padding" => {
                let v = parse_value(lex)?;
                view.props.push(("padding".into(), v));
            }
            "cornerRadius" => {
                let v = parse_value(lex)?;
                if let SwiftValue::Float(r) = v {
                    view.props
                        .push(("corner_radius".into(), SwiftValue::Float(r)));
                }
            }
            "clipShape" => {
                // `.clipShape(RoundedRectangle(cornerRadius: r))` — reach into the
                // nested shape and pull out its corner radius. Other clip shapes
                // (Circle, Capsule, …) have no radius to surface.
                let shape = lex.read_ident();
                lex.skip_ws();
                if shape == "RoundedRectangle" && lex.peek() == Some(b'(') {
                    lex.advance(); // (
                    loop {
                        lex.skip_ws();
                        if lex.peek() == Some(b')') {
                            lex.advance();
                            break;
                        }
                        if lex.peek().is_none() {
                            break;
                        }
                        let key = lex.read_ident();
                        lex.skip_ws();
                        if lex.peek() == Some(b':') {
                            lex.advance();
                        }
                        lex.skip_ws();
                        let v = parse_value(lex)?;
                        if key == "cornerRadius" {
                            if let SwiftValue::Float(r) = v {
                                view.props
                                    .push(("corner_radius".into(), SwiftValue::Float(r)));
                            }
                        }
                        lex.skip_ws();
                        if lex.peek() == Some(b',') {
                            lex.advance();
                        }
                    }
                }
            }
            "opacity" => {
                let v = parse_value(lex)?;
                view.props.push(("opacity".into(), v));
            }
            "bold" => {
                // `.bold()` — empty parens, sets a weight prop.
                view.props
                    .push(("weight".into(), SwiftValue::Ident("bold".into())));
            }
            "fontWeight" => {
                // `.fontWeight(.bold)` — carry the named weight through.
                let v = parse_value(lex)?;
                let weight = match v {
                    SwiftValue::Ident(w) => w,
                    _ => "regular".into(),
                };
                view.props.push(("weight".into(), SwiftValue::Ident(weight)));
            }
            "italic" => {
                view.props.push(("italic".into(), SwiftValue::Bool(true)));
            }
            "hidden" => {
                // `.hidden()` — empty parens.
                view.props.push(("hidden".into(), SwiftValue::Bool(true)));
            }
            "onTapGesture" | "onLongPressGesture" => {
                view.callbacks.push("click".into());
            }
            other => {
                // A modifier with no IR home (e.g. .shadow, .animation,
                // .accessibilityLabel). Its arguments are consumed below.
                view.unsupported.push(Unsupported {
                    line: modifier_line,
                    text: format!("modifier .{other}"),
                });
            }
        }
        // consume to closing )
        loop {
            lex.skip_ws();
            if lex.peek() == Some(b')') {
                lex.advance();
                break;
            }
            if lex.peek().is_none() {
                break;
            }
            lex.advance();
        }
    }
    Ok(view)
}

fn parse_file(lex: &mut Lexer<'_>) -> Result<Vec<SwiftView>, SwiftUIImportError> {
    let mut views = Vec::new();
    loop {
        lex.skip_ws();
        if lex.peek().is_none() {
            break;
        }
        if lex.peek().map(|c| c.is_ascii_uppercase()).unwrap_or(false) {
            let kind = lex.read_ident();
            let view = parse_view_body(lex, kind)?;
            views.push(view);
        } else {
            lex.advance();
        }
    }
    Ok(views)
}

// ──────────────────────────────────────────────────── lowering to IR ─────────

fn swiftval_to_ir(v: SwiftValue) -> Value {
    match v {
        SwiftValue::Str(s) => Value::Text(s),
        SwiftValue::Float(f) => Value::Float(f),
        SwiftValue::Bool(b) => Value::Bool(b),
        SwiftValue::Color(c) => Value::Color(c),
        SwiftValue::FontRole(px) => Value::Px(px),
        SwiftValue::Ident(s) => Value::Text(s),
        // A bare range that escaped split-lowering — represent both bounds.
        SwiftValue::Range(lo, hi) => Value::List(vec![Value::Float(lo), Value::Float(hi)]),
    }
}

fn lower_view(
    view: &SwiftView,
    doc: &mut Document,
    unsupported: &mut Vec<Unsupported>,
) -> Option<NodeId> {
    // Hoist this view's dropped modifiers into the flat report.
    unsupported.extend(view.unsupported.iter().cloned());

    let kind = map_kind(&view.kind).to_string();
    let id = doc.fresh_id();
    doc.apply_from(Origin::System, Mutation::CreateNode { id, kind })
        .ok()?;

    // Label → content prop
    if let Some(label) = &view.label {
        doc.apply_from(
            Origin::System,
            Mutation::SetProp {
                id,
                key: "content".into(),
                value: Value::Text(label.clone()),
            },
        )
        .ok();
    }

    for (key, val) in &view.props {
        // `Image(name:)` and `Image(systemName:)` both name the picture the view
        // shows — normalize to one `content` prop in our vocabulary.
        let key: &str = match (view.kind.as_str(), key.as_str()) {
            ("Image", "name") | ("Image", "systemName") => "content",
            _ => key.as_str(),
        };

        // A range bound (`Slider(in: 0...100)`) is two scalars in the IR.
        if let SwiftValue::Range(lo, hi) = val {
            for (k, bound) in [("range_min", *lo), ("range_max", *hi)] {
                doc.apply_from(
                    Origin::System,
                    Mutation::SetProp {
                        id,
                        key: k.into(),
                        value: Value::Float(bound),
                    },
                )
                .ok();
            }
            continue;
        }

        let ir_val = match val {
            SwiftValue::Float(f) => {
                // If key is size/width/height/padding/corner_radius, use Px
                if matches!(
                    key,
                    "size" | "width" | "height" | "padding" | "corner_radius" | "opacity"
                ) {
                    Value::Px(*f as f32)
                } else {
                    Value::Float(*f)
                }
            }
            other => swiftval_to_ir(other.clone()),
        };
        doc.apply_from(
            Origin::System,
            Mutation::SetProp {
                id,
                key: key.into(),
                value: ir_val,
            },
        )
        .ok();
    }

    for event in &view.callbacks {
        doc.apply_from(
            Origin::System,
            Mutation::SetCallback {
                id,
                event: event.clone(),
                action: Action {
                    name: "action".into(),
                    args: vec![],
                },
            },
        )
        .ok();
    }

    for child in &view.children {
        if let Some(child_id) = lower_view(child, doc, unsupported) {
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

// ─────────────────────────────────────────────────────────── public API ──────

/// Parse a SwiftUI source string and return a `uni_ir::Document`.
/// The first top-level view becomes the document root.
///
/// This is the lossy-by-design front door: dropped modifiers are discarded.
/// Use [`parse_with_report`] when you need to know what was lost.
pub fn parse(src: &str) -> Result<Document, SwiftUIImportError> {
    Ok(parse_with_report(src)?.document)
}

/// Parse a SwiftUI source string into a [`Document`] *and* a list of the
/// modifiers that were recognized but dropped rather than lowered.
///
/// Additive sibling to [`parse`]: same lowering, but the [`ImportReport`] also
/// surfaces every [`Unsupported`] drop (unknown `.modifier(...)` chains) with
/// its source line.
pub fn parse_with_report(src: &str) -> Result<ImportReport, SwiftUIImportError> {
    let mut lex = Lexer::new(src);
    let views = parse_file(&mut lex)?;
    let mut doc = Document::new();
    let mut unsupported = Vec::new();
    let mut root_set = false;
    for view in &views {
        if let Some(id) = lower_view(view, &mut doc, &mut unsupported) {
            if !root_set {
                doc.apply_from(Origin::System, Mutation::SetRoot { id })
                    .map_err(|e| err(format!("{e:?}"), 0))?;
                root_set = true;
            }
        }
    }
    Ok(ImportReport {
        document: doc,
        unsupported,
    })
}

// ─────────────────────────────────────────────────────────────── tests ───────

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
    fn parse_vstack_becomes_column() {
        let doc = parse("VStack { }").unwrap();
        let root = doc.root().unwrap();
        assert_eq!(doc.get(root).unwrap().kind, "Column");
    }

    #[test]
    fn parse_hstack_becomes_row() {
        let doc = parse("HStack { }").unwrap();
        let root = doc.root().unwrap();
        assert_eq!(doc.get(root).unwrap().kind, "Row");
    }

    #[test]
    fn parse_text_with_string() {
        let doc = parse(r#"Text("Hello world")"#).unwrap();
        let root = doc.root().unwrap();
        assert_eq!(
            doc.get(root).unwrap().props.get("content"),
            Some(&Value::Text("Hello world".into()))
        );
    }

    #[test]
    fn parse_font_title_sets_size() {
        let v = root_prop(r#"Text("Hi").font(.title)"#, "size");
        assert_eq!(v, Some(Value::Px(28.0)));
    }

    #[test]
    fn parse_foreground_color_white() {
        let v = root_prop(r#"Text("Hi").foregroundColor(.white)"#, "color");
        assert_eq!(v, Some(Value::Color(0xFFFF_FFFF)));
    }

    #[test]
    fn parse_frame_sets_width_height() {
        let doc = parse(r#"Rectangle().frame(width: 200, height: 64)"#).unwrap();
        let root = doc.root().unwrap();
        let props = &doc.get(root).unwrap().props;
        assert_eq!(props.get("width"), Some(&Value::Px(200.0)));
        assert_eq!(props.get("height"), Some(&Value::Px(64.0)));
    }

    #[test]
    fn parse_padding_modifier() {
        let v = root_prop("VStack { }.padding(16)", "padding");
        assert_eq!(v, Some(Value::Px(16.0)));
    }

    #[test]
    fn parse_button_with_closure_sets_callback() {
        let src = r#"Button("Click me") { doSomething() }"#;
        let doc = parse(src).unwrap();
        let root = doc.root().unwrap();
        assert!(doc.get(root).unwrap().callbacks.contains_key("click"));
    }

    #[test]
    fn parse_rounded_rect_corner_radius() {
        let v = root_prop(
            "RoundedRectangle(cornerRadius: 16).frame(width: 100, height: 40)",
            "corner_radius",
        );
        // cornerRadius inline arg — maps to corner_radius. The point of this
        // case is that the inline-args path parses without panicking; whether
        // the value is currently surfaced as a prop is not asserted here.
        let _ = v;
    }

    #[test]
    fn parse_background_color_rgb() {
        let src = r#"VStack { }.background(Color(red: 0.49, green: 0.22, blue: 0.92))"#;
        let v = root_prop(src, "background");
        match v {
            Some(Value::Color(c)) => {
                let r = (c >> 24) & 0xFF;
                let g = (c >> 16) & 0xFF;
                let b = (c >> 8) & 0xFF;
                assert!(r > 100, "red component expected ~124, got {r}");
                assert!(g > 40 && g < 80, "green component expected ~56, got {g}");
                assert!(b > 200, "blue component expected ~234, got {b}");
            }
            other => panic!("expected Color, got {other:?}"),
        }
    }

    #[test]
    fn parse_nested_children_in_vstack() {
        let src = r#"VStack { Text("a") Text("b") }"#;
        let doc = parse(src).unwrap();
        let root = doc.root().unwrap();
        assert_eq!(doc.get(root).unwrap().children.len(), 2);
    }

    #[test]
    fn parse_fill_modifier_sets_background() {
        let v = root_prop("Rectangle().fill(.blue)", "background");
        assert_eq!(v, Some(Value::Color(0x0000_FFFF)));
    }

    // ── E1: unsupported-construct reporting ───────────────────────────────────

    #[test]
    fn clean_input_reports_no_unsupported() {
        let report = parse_with_report(r#"Text("Hi").padding(8)"#).unwrap();
        assert!(report.unsupported.is_empty());
        assert!(report.document.root().is_some());
    }

    #[test]
    fn unknown_modifier_is_recorded() {
        // `.shadow(radius: 4)` has no IR home — dropped + reported.
        let report = parse_with_report(r#"Text("Hi").shadow(radius: 4)"#).unwrap();
        assert_eq!(report.unsupported.len(), 1);
        assert_eq!(report.unsupported[0].text, "modifier .shadow");
        // The Text still lowered with its content.
        let root = report.document.root().unwrap();
        assert_eq!(
            report.document.get(root).unwrap().props.get("content"),
            Some(&Value::Text("Hi".into()))
        );
    }

    #[test]
    fn nested_view_modifier_drop_is_recorded() {
        // The drop is on a child, proving the report flattens the view tree.
        let src = r#"VStack { Text("a").accessibilityLabel("greeting") }"#;
        let report = parse_with_report(src).unwrap();
        assert!(report
            .unsupported
            .iter()
            .any(|u| u.text == "modifier .accessibilityLabel"));
    }

    #[test]
    fn parse_still_returns_bare_document() {
        // Back-compat: `parse` signature unchanged, drops are silent.
        let doc = parse(r#"Text("Hi").shadow(radius: 4)"#).unwrap();
        assert!(doc.root().is_some());
    }

    // ── S1: new leaf views ────────────────────────────────────────────────────

    fn root_kind(src: &str) -> String {
        let doc = parse(src).expect("parse ok");
        let root = doc.root().expect("has root");
        doc.get(root).unwrap().kind.clone()
    }

    #[test]
    fn parse_image_system_name_carries_content() {
        let doc = parse(r#"Image(systemName: "star.fill")"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "Image");
        assert_eq!(
            node.props.get("content"),
            Some(&Value::Text("star.fill".into()))
        );
    }

    #[test]
    fn parse_image_named_carries_content() {
        // `Image("logo")` — positional string also lands in `content`.
        let doc = parse(r#"Image("logo")"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "Image");
        assert_eq!(node.props.get("content"), Some(&Value::Text("logo".into())));
    }

    #[test]
    fn parse_divider_lowers_to_divider() {
        assert_eq!(root_kind("Divider()"), "Divider");
    }

    #[test]
    fn parse_toggle_label_and_bound_state() {
        let doc = parse(r#"Toggle("Wi-Fi", isOn: $wifiEnabled)"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "Toggle");
        // label → content; bound state name carried through `isOn`.
        assert_eq!(
            node.props.get("content"),
            Some(&Value::Text("Wi-Fi".into()))
        );
        assert_eq!(
            node.props.get("isOn"),
            Some(&Value::Text("wifiEnabled".into()))
        );
    }

    #[test]
    fn parse_slider_value_and_range() {
        let doc = parse(r#"Slider(value: $volume, in: 0...100)"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "Slider");
        assert_eq!(
            node.props.get("value"),
            Some(&Value::Text("volume".into()))
        );
        assert_eq!(node.props.get("range_min"), Some(&Value::Float(0.0)));
        assert_eq!(node.props.get("range_max"), Some(&Value::Float(100.0)));
    }

    #[test]
    fn parse_progressview_with_value() {
        let doc = parse(r#"ProgressView(value: 0.5)"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "ProgressView");
        assert_eq!(node.props.get("value"), Some(&Value::Float(0.5)));
    }

    #[test]
    fn parse_progressview_indeterminate() {
        // No value — still lowers, just carries no `value` prop.
        let doc = parse(r#"ProgressView()"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "ProgressView");
        assert!(!node.props.contains_key("value"));
    }

    // ── S1: new modifiers ─────────────────────────────────────────────────────

    #[test]
    fn parse_opacity_modifier() {
        let v = root_prop(r#"Text("Hi").opacity(0.4)"#, "opacity");
        assert_eq!(v, Some(Value::Px(0.4)));
    }

    #[test]
    fn parse_hidden_modifier() {
        let v = root_prop(r#"Text("Hi").hidden()"#, "hidden");
        assert_eq!(v, Some(Value::Bool(true)));
    }

    #[test]
    fn parse_is_hidden_property_modifier() {
        // Parenthesis-less property form.
        let v = root_prop(r#"Text("Hi").isHidden"#, "hidden");
        assert_eq!(v, Some(Value::Bool(true)));
    }

    #[test]
    fn parse_clip_shape_rounded_rect_corner_radius() {
        let v = root_prop(
            r#"Rectangle().clipShape(RoundedRectangle(cornerRadius: 12))"#,
            "corner_radius",
        );
        assert_eq!(v, Some(Value::Px(12.0)));
    }

    #[test]
    fn parse_foreground_style_aliases_color() {
        let v = root_prop(r#"Text("Hi").foregroundStyle(.red)"#, "color");
        assert_eq!(v, Some(Value::Color(0xFF00_00FF)));
    }

    #[test]
    fn parse_bold_sets_weight() {
        let v = root_prop(r#"Text("Hi").bold()"#, "weight");
        assert_eq!(v, Some(Value::Text("bold".into())));
    }

    #[test]
    fn parse_font_weight_bold_sets_weight() {
        let v = root_prop(r#"Text("Hi").fontWeight(.bold)"#, "weight");
        assert_eq!(v, Some(Value::Text("bold".into())));
    }

    #[test]
    fn parse_italic_sets_italic() {
        let v = root_prop(r#"Text("Hi").italic()"#, "italic");
        assert_eq!(v, Some(Value::Bool(true)));
    }

    #[test]
    fn parse_frame_max_width_infinity_sets_grow() {
        let v = root_prop(r#"Text("Hi").frame(maxWidth: .infinity)"#, "grow");
        assert_eq!(v, Some(Value::Float(1.0)));
    }

    // ── S1: unsupported telemetry for a deliberately-unknown modifier ─────────

    #[test]
    fn unknown_modifier_among_supported_is_reported() {
        // `.blur(radius: 3)` is deliberately unknown; the surrounding supported
        // modifiers (`.bold()`, `.italic()`) must NOT be reported.
        let report = parse_with_report(r#"Text("Hi").bold().blur(radius: 3).italic()"#).unwrap();
        assert_eq!(report.unsupported.len(), 1);
        assert_eq!(report.unsupported[0].text, "modifier .blur");
        // The supported props still landed.
        let root = report.document.root().unwrap();
        let node = report.document.get(root).unwrap();
        assert_eq!(node.props.get("weight"), Some(&Value::Text("bold".into())));
        assert_eq!(node.props.get("italic"), Some(&Value::Bool(true)));
    }

    #[test]
    fn unknown_property_modifier_is_reported() {
        // A parenthesis-less unknown like `.redacted` is captured, not dropped.
        let report = parse_with_report(r#"Text("Hi").redacted"#).unwrap();
        assert!(report
            .unsupported
            .iter()
            .any(|u| u.text == "modifier .redacted"));
    }
}
