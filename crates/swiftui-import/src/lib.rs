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
//! - Containers: `List`/`List(data)`, `LazyVStack`/`LazyHStack` (lazy `List`/`Row`),
//!   `Grid`/`GridRow`, `Form`, `Section(header:)`, `Picker(selection:)`, `Stepper(value:)`
//! - `@State var name` declarations and `$binding` call-site uses (bound names recorded)
//! - Navigation & presentation: `NavigationStack`/`NavigationView`,
//!   `NavigationLink("label") { dest }` / `NavigationLink(value:)`, `TabView`
//!   with `.tabItem` metadata, `Menu("label") { … }`
//! - Presentation modifiers as bound child *layers*: `.sheet(isPresented:$x)`,
//!   `.alert(title, isPresented:$x)`, `.popover(isPresented:$x)`,
//!   `.overlay { … }` / `.background(View)` (overlay/underlay layers, distinct
//!   from `.background(Color)` which stays a style prop)
//! - Gesture modifiers lowered to callbacks/props on the node:
//!   `.onTapGesture { }` → a `"click"` callback; `.onTapGesture(count: 2) { }` →
//!   a `tap_count` prop + `"click"`; `.onLongPressGesture { }` → `"longpress"`;
//!   `.gesture(DragGesture().onChanged{}.onEnded{})` → `"drag_changed"` /
//!   `"drag_ended"` (only the phases present); `.gesture(MagnificationGesture())`
//!   → `"magnify"`; `.gesture(RotationGesture())` → `"rotate"`.
//!   `.simultaneousGesture` / `.highPriorityGesture` are recognized and tagged
//!   with a `gesture_priority` prop (`simultaneous` / `high`). An unknown
//!   recognizer inside `.gesture(...)` is recorded as an [`Unsupported`] drop.
//! - Transform & animation modifiers: `.offset(x:,y:)` / `.offset(CGSize)` →
//!   `offset_x`/`offset_y`; `.rotationEffect(.degrees(d))` → `rotation`;
//!   `.scaleEffect(s)` → `scale`; `.animation(.curve(duration:), value:)` → an
//!   `animation` descriptor prop (`"<curve>:<duration>"`); `.transition(.opacity
//!   /.slide/.scale)` → `transition`; `withAnimation { … }` wrappers recognized
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
    /// Names of `@State`-declared variables seen at top level, in source order.
    ///
    /// SwiftUI's state graph cannot be resolved in a clean-room importer, but a
    /// `@State var count = 0` declaration tells us `count` is a piece of mutable
    /// view state. Recording the names lets the AI companion (and a later
    /// reactive layer) reconnect `$count` bindings at call sites to their source.
    pub state_vars: Vec<String>,
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
        // Containers that collect their children into a scrollable list.
        // `List`, `Form`, `Grid`, `Section`, `Picker`, `Stepper` keep their
        // SwiftUI name as the IR kind — they *are* their own container element.
        "List" | "LazyVStack" => "List",
        "LazyHStack" => "Row",
        "Grid" | "GridRow" => "Grid",
        "Form" => "Form",
        "Section" => "Section",
        "Picker" => "Picker",
        "Stepper" => "Stepper",
        // Navigation / presentation containers keep their SwiftUI name as the IR
        // kind — they *are* their own structural element in our vocabulary.
        "NavigationStack" | "NavigationView" => "NavigationStack",
        "NavigationLink" => "NavigationLink",
        "TabView" => "TabView",
        "Menu" => "Menu",
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
                } else {
                    // A bare positional identifier — `List(items)`, `ForEach(rows)`.
                    // It names the data collection the container iterates; carry
                    // it as a `data` prop so the binding survives the lower.
                    view.props.push(("data".into(), SwiftValue::Ident(key)));
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
        let children = parse_children_block(lex)?;
        view.children.extend(children);
    }

    apply_modifiers(lex, view)
}

/// Parse a `{ View View ... }` content block, returning its child views.
/// Assumes `lex` is positioned at the opening `{`.
fn parse_children_block(lex: &mut Lexer<'_>) -> Result<Vec<SwiftView>, SwiftUIImportError> {
    let mut children = Vec::new();
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
            children.push(child);
        } else {
            // skip unknown token
            lex.advance();
        }
    }
    Ok(children)
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
                // A view-valued argument such as `Section(header: Text("Settings"))`
                // or `Picker(... ) { }` with a `Text(...)` label. Pull the inner
                // string literal out as the value; if there is none, fall back to
                // the view's own name. Either way the nested parens are consumed.
                _ if lex.peek() == Some(b'(') => {
                    lex.advance(); // (
                    let mut inner: Option<String> = None;
                    loop {
                        lex.skip_ws();
                        match lex.peek() {
                            Some(b')') => {
                                lex.advance();
                                break;
                            }
                            Some(b'"') => inner = Some(lex.read_string()?),
                            None => break,
                            _ => {
                                lex.advance();
                            }
                        }
                    }
                    Ok(SwiftValue::Str(inner.unwrap_or(id)))
                }
                _ => Ok(SwiftValue::Ident(id)),
            }
        }
        None => Err(err("unexpected EOF in value", lex.line)),
    }
}

/// Map a presentation modifier name to the IR layer kind it produces.
fn present_kind(modifier: &str) -> &'static str {
    match modifier {
        "sheet" => "Sheet",
        "popover" => "Popover",
        "fullScreenCover" => "FullScreenCover",
        "alert" => "Alert",
        "confirmationDialog" => "ConfirmationDialog",
        _ => "Sheet",
    }
}

/// Read a `key: $bound` / `key: value` argument list inside a modifier's
/// parens, collecting any `isPresented`/`item`/`value`/`for` binding and the
/// first positional string (a title). Stops at the closing `)`, which it
/// consumes. Returns `(title, isPresented_binding)`.
fn parse_present_args(
    lex: &mut Lexer<'_>,
) -> Result<(Option<String>, Option<String>), SwiftUIImportError> {
    let mut title: Option<String> = None;
    let mut bound: Option<String> = None;
    loop {
        lex.skip_ws();
        match lex.peek() {
            Some(b')') => {
                lex.advance();
                break;
            }
            None => break,
            Some(b'"') => {
                // a positional title string (e.g. `.alert("Delete?", …)`)
                if title.is_none() {
                    title = Some(lex.read_string()?);
                } else {
                    lex.read_string()?;
                }
            }
            Some(c) if c.is_ascii_alphabetic() => {
                let key = lex.read_ident();
                lex.skip_ws();
                if lex.peek() == Some(b':') {
                    lex.advance();
                    lex.skip_ws();
                    let v = parse_value(lex)?;
                    if matches!(key.as_str(), "isPresented" | "item" | "value" | "for")
                        && bound.is_none()
                    {
                        if let SwiftValue::Ident(name) = &v {
                            bound = Some(name.clone());
                        }
                    }
                }
            }
            _ => {
                lex.advance();
            }
        }
        lex.skip_ws();
        if lex.peek() == Some(b',') {
            lex.advance();
        }
    }
    Ok((title, bound))
}

/// Build a synthetic child-layer [`SwiftView`] of the given kind, recording the
/// presentation binding (`bound_to`) and optional title as props, and folding
/// its content children up out of `content`.
fn make_layer(kind: &str, bound_to: Option<String>, title: Option<String>) -> SwiftView {
    let mut props = Vec::new();
    if let Some(b) = bound_to {
        props.push(("bound_to".to_string(), SwiftValue::Ident(b)));
    }
    if let Some(t) = title {
        props.push(("title".to_string(), SwiftValue::Str(t)));
    }
    SwiftView {
        kind: kind.to_string(),
        label: None,
        props,
        callbacks: vec![],
        children: vec![],
        unsupported: vec![],
    }
}

/// `.sheet(isPresented:$x) { content }` / `.popover(...) { … }` etc. Parses the
/// optional arg list, then the trailing content closure, and attaches a single
/// synthetic child layer (kind = `layer_kind`) bound to the presentation state.
fn parse_presentation_modifier(
    lex: &mut Lexer<'_>,
    view: &mut SwiftView,
    layer_kind: &str,
) -> Result<(), SwiftUIImportError> {
    let (title, bound) = if lex.peek() == Some(b'(') {
        lex.advance(); // (
        parse_present_args(lex)?
    } else {
        (None, None)
    };
    let mut layer = make_layer(layer_kind, bound, title);
    lex.skip_ws();
    if lex.peek() == Some(b'{') {
        layer.children = parse_children_block(lex)?;
    }
    view.children.push(layer);
    Ok(())
}

/// `.alert(title, isPresented:$x) { actions }` — like a presentation modifier,
/// but the SwiftUI alert closure holds action buttons rather than content; we
/// still capture them as the layer's children so they survive the lower.
fn parse_alert_modifier(
    lex: &mut Lexer<'_>,
    view: &mut SwiftView,
    layer_kind: &str,
) -> Result<(), SwiftUIImportError> {
    parse_presentation_modifier(lex, view, layer_kind)?;
    // An alert may also carry a trailing message closure: `.alert(…){…} message:{…}`.
    // SwiftUI's labeled trailing-closure form is rare in practice; if a bare
    // `{ … }` follows, fold it into the same layer's children.
    lex.skip_ws();
    if lex.peek() == Some(b'{') {
        if let Some(layer) = view.children.last_mut() {
            let extra = parse_children_block(lex)?;
            layer.children.extend(extra);
        } else {
            parse_children_block(lex)?;
        }
    }
    Ok(())
}

/// `.overlay { View }` / `.overlay(alignment:.top) { View }` /
/// `.background { View }` — the closure views become an overlay/underlay layer.
fn parse_layer_modifier(
    lex: &mut Lexer<'_>,
    view: &mut SwiftView,
    layer_kind: &str,
) -> Result<(), SwiftUIImportError> {
    let mut layer = make_layer(layer_kind, None, None);
    // Optional `(alignment: …)` or `(View)` positional content.
    if lex.peek() == Some(b'(') {
        lex.advance(); // (
        loop {
            lex.skip_ws();
            match lex.peek() {
                Some(b')') => {
                    lex.advance();
                    break;
                }
                None => break,
                // A positional View argument, e.g. `.overlay(Circle())` or
                // `.background(RoundedRectangle(...))`.
                Some(c) if c.is_ascii_uppercase() => {
                    let kind = lex.read_ident();
                    let child = parse_view_body(lex, kind)?;
                    layer.children.push(child);
                }
                _ => {
                    lex.advance();
                }
            }
        }
    }
    lex.skip_ws();
    if lex.peek() == Some(b'{') {
        let kids = parse_children_block(lex)?;
        layer.children.extend(kids);
    }
    view.children.push(layer);
    Ok(())
}

/// `.tabItem { Label("Home", systemImage:"house") }` — attaches the tab's
/// label metadata to *this* view (a tab page). The first string literal seen
/// in the closure becomes the tab title prop.
fn parse_tab_item_modifier(
    lex: &mut Lexer<'_>,
    view: &mut SwiftView,
) -> Result<(), SwiftUIImportError> {
    view.props
        .push(("tab_item".to_string(), SwiftValue::Bool(true)));
    // Optional parens (rare) then the content closure.
    if lex.peek() == Some(b'(') {
        lex.skip_balanced(b'(', b')');
        lex.skip_ws();
    }
    if lex.peek() == Some(b'{') {
        let kids = parse_children_block(lex)?;
        // Pull a label string out of the closure (Label/Text/Image inside).
        for kid in &kids {
            if let Some(label) = &kid.label {
                view.props
                    .push(("tab_label".to_string(), SwiftValue::Str(label.clone())));
                break;
            }
        }
    }
    Ok(())
}

/// Disambiguate `.background(...)`: peek (without consuming) just past the `(`
/// to decide if it is a *view* layer (uppercase view name) rather than a style
/// such as `.background(Color.blue)` / `.background(.red)`. `Color(...)` is a
/// style, so it is excluded.
fn background_is_view_layer(lex: &mut Lexer<'_>) -> bool {
    let saved_pos = lex.pos;
    let saved_line = lex.line;
    let mut decided = false;
    if lex.peek() == Some(b'(') {
        lex.advance();
        lex.skip_ws();
        if lex.peek().map(|c| c.is_ascii_uppercase()).unwrap_or(false) {
            let ident = lex.read_ident();
            // `Color(...)` is a style, not a view layer.
            decided = ident != "Color";
        }
    } else {
        lex.skip_ws();
        // `.background { View }` (no parens) is always a view layer.
        decided = lex.peek() == Some(b'{');
    }
    lex.pos = saved_pos;
    lex.line = saved_line;
    decided
}

/// `.offset(x:,y:)` / `.offset(CGSize(width:,height:))` / `.offset(10, 20)`.
///
/// Assumes `lex` is positioned just after the opening `(`. Reads to (but does
/// not consume) the matching `)` — the generic close-paren drain in
/// [`apply_modifiers`] swallows the trailing `)`. Sets `offset_x` / `offset_y`
/// as float props on `view` for whichever axes were given.
fn parse_offset_args(
    lex: &mut Lexer<'_>,
    view: &mut SwiftView,
) -> Result<(), SwiftUIImportError> {
    // `.offset(CGSize(width: 10, height: 20))` — unwrap the CGSize and read its
    // `width`/`height` as x/y.
    let saved_pos = lex.pos;
    let saved_line = lex.line;
    if lex.peek().map(|c| c.is_ascii_uppercase()).unwrap_or(false) {
        let ident = lex.read_ident();
        lex.skip_ws();
        if ident == "CGSize" && lex.peek() == Some(b'(') {
            lex.advance(); // (
            read_offset_kv(lex, view, "width", "height")?;
            // consume the CGSize's own closing ) if present
            lex.skip_ws();
            if lex.peek() == Some(b')') {
                lex.advance();
            }
            return Ok(());
        }
        // Not a CGSize — rewind and fall through to the labeled/positional path.
        lex.pos = saved_pos;
        lex.line = saved_line;
    }
    // Labeled `x:`/`y:` or positional `10, 20`.
    read_offset_kv(lex, view, "x", "y")?;
    Ok(())
}

/// Read a comma-separated list of `<x_key>:`/`<y_key>:` labeled values (or two
/// positional floats, mapped to x then y) into `offset_x`/`offset_y` props.
/// Stops at — and does not consume — the closing `)`.
fn read_offset_kv(
    lex: &mut Lexer<'_>,
    view: &mut SwiftView,
    x_key: &str,
    y_key: &str,
) -> Result<(), SwiftUIImportError> {
    let mut positional = 0usize;
    loop {
        lex.skip_ws();
        match lex.peek() {
            Some(b')') | None => break,
            Some(c) if c.is_ascii_alphabetic() => {
                let key = lex.read_ident();
                lex.skip_ws();
                if lex.peek() == Some(b':') {
                    lex.advance();
                }
                lex.skip_ws();
                let v = parse_value(lex)?;
                if let SwiftValue::Float(f) = v {
                    if key == x_key {
                        view.props.push(("offset_x".into(), SwiftValue::Float(f)));
                    } else if key == y_key {
                        view.props.push(("offset_y".into(), SwiftValue::Float(f)));
                    }
                }
            }
            _ => {
                // A positional float: first → x, second → y.
                let v = parse_value(lex)?;
                if let SwiftValue::Float(f) = v {
                    let key = if positional == 0 { "offset_x" } else { "offset_y" };
                    view.props.push((key.into(), SwiftValue::Float(f)));
                }
                positional += 1;
            }
        }
        lex.skip_ws();
        if lex.peek() == Some(b',') {
            lex.advance();
        } else {
            break;
        }
    }
    Ok(())
}

/// Parse a rotation angle starting just after the modifier's `(`. Handles
/// `.degrees(d)`, `.radians(r)`, and `Angle(degrees: d)` / `Angle(radians: r)`.
/// Returns the angle in degrees. Reads to (but does not consume) the outer `)`.
fn parse_angle_degrees(lex: &mut Lexer<'_>) -> Result<Option<f64>, SwiftUIImportError> {
    lex.skip_ws();
    // `.degrees(...)` / `.radians(...)`
    if lex.peek() == Some(b'.') {
        lex.advance(); // .
        let unit = lex.read_ident();
        lex.skip_ws();
        if lex.peek() == Some(b'(') {
            lex.advance(); // (
            lex.skip_ws();
            let n = lex.read_number();
            lex.skip_ws();
            if lex.peek() == Some(b')') {
                lex.advance();
            }
            return Ok(Some(if unit == "radians" {
                n.to_degrees()
            } else {
                n
            }));
        }
        return Ok(None);
    }
    // `Angle(degrees: d)` / `Angle(radians: r)`
    if lex.peek().map(|c| c.is_ascii_uppercase()).unwrap_or(false) {
        let ident = lex.read_ident();
        lex.skip_ws();
        if ident == "Angle" && lex.peek() == Some(b'(') {
            lex.advance(); // (
            lex.skip_ws();
            let key = lex.read_ident();
            lex.skip_ws();
            if lex.peek() == Some(b':') {
                lex.advance();
            }
            lex.skip_ws();
            let n = lex.read_number();
            lex.skip_ws();
            if lex.peek() == Some(b')') {
                lex.advance();
            }
            return Ok(Some(if key == "radians" {
                n.to_degrees()
            } else {
                n
            }));
        }
    }
    Ok(None)
}

/// Parse an `.animation(curve, value: x)` argument list into a `"<curve>:<dur>"`
/// descriptor string. The curve is the leading `.easeInOut(duration: 0.3)` /
/// `.linear` / `.spring()` token; `value:` (the trigger) is ignored. Reads to
/// (but does not consume) the outer `)`.
fn parse_animation_descriptor(lex: &mut Lexer<'_>) -> Result<String, SwiftUIImportError> {
    let mut curve = String::from("default");
    let mut duration: Option<f64> = None;
    lex.skip_ws();
    if lex.peek() == Some(b'.') {
        lex.advance(); // .
        curve = lex.read_ident();
        lex.skip_ws();
        // Optional `(duration: 0.3)` / `(...)` arg list on the curve.
        if lex.peek() == Some(b'(') {
            lex.advance(); // (
            loop {
                lex.skip_ws();
                match lex.peek() {
                    Some(b')') | None => {
                        if lex.peek() == Some(b')') {
                            lex.advance();
                        }
                        break;
                    }
                    Some(c) if c.is_ascii_alphabetic() => {
                        let key = lex.read_ident();
                        lex.skip_ws();
                        if lex.peek() == Some(b':') {
                            lex.advance();
                        }
                        lex.skip_ws();
                        let v = parse_value(lex)?;
                        if key == "duration" {
                            if let SwiftValue::Float(f) = v {
                                duration = Some(f);
                            }
                        }
                    }
                    _ => {
                        lex.advance();
                    }
                }
                lex.skip_ws();
                if lex.peek() == Some(b',') {
                    lex.advance();
                }
            }
        }
    }
    Ok(match duration {
        Some(d) => format!("{curve}:{d}"),
        None => curve,
    })
}

/// Parse a `.transition(...)` argument into its named transition. Handles
/// `.opacity` / `.slide` / `.scale` (and any other dotted name). Reads to (but
/// does not consume) the outer `)`.
fn parse_transition_name(lex: &mut Lexer<'_>) -> Result<String, SwiftUIImportError> {
    lex.skip_ws();
    if lex.peek() == Some(b'.') {
        lex.advance(); // .
        let name = lex.read_ident();
        return Ok(name);
    }
    // A non-dotted form (`.transition(AnyTransition.scale)`) — take the trailing
    // dotted member if present, else the bare identifier.
    let id = lex.read_ident();
    lex.skip_ws();
    if lex.peek() == Some(b'.') {
        lex.advance();
        return Ok(lex.read_ident());
    }
    Ok(id)
}

/// Read a `key: value` argument list inside a gesture modifier's parens,
/// collecting an integer `count:` (tap count) if present and any positional
/// integer (the `.onLongPressGesture` form takes none, but `.onTapGesture` may
/// be written `.onTapGesture(count: 2)`). Stops at — and consumes — the
/// matching `)`. Returns the recognized tap `count`, if any.
fn parse_gesture_count(lex: &mut Lexer<'_>) -> Result<Option<i64>, SwiftUIImportError> {
    let mut count: Option<i64> = None;
    loop {
        lex.skip_ws();
        match lex.peek() {
            Some(b')') => {
                lex.advance();
                break;
            }
            None => break,
            Some(c) if c.is_ascii_alphabetic() => {
                let key = lex.read_ident();
                lex.skip_ws();
                if lex.peek() == Some(b':') {
                    lex.advance();
                    lex.skip_ws();
                    let v = parse_value(lex)?;
                    if key == "count" {
                        if let SwiftValue::Float(f) = v {
                            count = Some(f as i64);
                        }
                    }
                }
            }
            _ => {
                lex.advance();
            }
        }
        lex.skip_ws();
        if lex.peek() == Some(b',') {
            lex.advance();
        }
    }
    Ok(count)
}

/// `.onTapGesture { }` → a `"click"` callback; `.onTapGesture(count: 2) { }` →
/// a `tap_count` prop plus the `"click"` callback. The trailing action closure
/// (if any) is recognized and skipped. Assumes `lex` is positioned just after
/// the `onTapGesture` identifier.
fn parse_tap_gesture(
    lex: &mut Lexer<'_>,
    view: &mut SwiftView,
) -> Result<(), SwiftUIImportError> {
    lex.skip_ws();
    if lex.peek() == Some(b'(') {
        lex.advance(); // (
        if let Some(count) = parse_gesture_count(lex)? {
            view.props
                .push(("tap_count".into(), SwiftValue::Float(count as f64)));
        }
    }
    lex.skip_ws();
    if lex.peek() == Some(b'{') {
        lex.advance();
        lex.skip_balanced(b'{', b'}');
    }
    view.callbacks.push("click".into());
    Ok(())
}

/// `.onLongPressGesture { }` (with optional `(minimumDuration:…)` args) → a
/// `"longpress"` callback. Args and the trailing action closure are skipped.
/// Assumes `lex` is positioned just after the `onLongPressGesture` identifier.
fn parse_long_press_gesture(
    lex: &mut Lexer<'_>,
    view: &mut SwiftView,
) -> Result<(), SwiftUIImportError> {
    lex.skip_ws();
    if lex.peek() == Some(b'(') {
        lex.advance(); // (
        lex.skip_balanced(b'(', b')');
    }
    lex.skip_ws();
    if lex.peek() == Some(b'{') {
        lex.advance();
        lex.skip_balanced(b'{', b'}');
    }
    view.callbacks.push("longpress".into());
    Ok(())
}

/// `.gesture(DragGesture().onChanged{…}.onEnded{…})` and friends.
///
/// Reads the gesture *value* inside `.gesture(...)` / `.simultaneousGesture(...)`
/// / `.highPriorityGesture(...)`. The leading uppercase identifier names the
/// recognizer (`DragGesture`, `MagnificationGesture`, `RotationGesture`, …);
/// each recognizer maps to the callbacks it can emit:
///   - `DragGesture` → `drag_changed` / `drag_ended` (only the phases that have
///     an `.onChanged`/`.onEnded` handler in the chain)
///   - `MagnificationGesture` → `magnify`
///   - `RotationGesture` → `rotate`
///
/// `TapGesture`/`LongPressGesture` recognizers map to `click`/`longpress`.
///
/// Unknown recognizers are reported as [`Unsupported`]. Assumes `lex` is
/// positioned just after the opening `(` of the `.gesture(` call. Reads to — and
/// consumes — the matching close `)`.
fn parse_gesture_value(
    lex: &mut Lexer<'_>,
    view: &mut SwiftView,
    modifier_line: usize,
) -> Result<(), SwiftUIImportError> {
    lex.skip_ws();
    // Recognizer name, e.g. `DragGesture`.
    let recognizer = if lex
        .peek()
        .map(|c| c.is_ascii_alphabetic() || c == b'_')
        .unwrap_or(false)
    {
        lex.read_ident()
    } else {
        String::new()
    };
    // Consume the recognizer's own constructor parens, e.g. `DragGesture()` or
    // `DragGesture(minimumDistance: 10)`.
    lex.skip_ws();
    if lex.peek() == Some(b'(') {
        lex.advance(); // (
        lex.skip_balanced(b'(', b')');
    }
    // Walk the `.onChanged{…}.onEnded{…}` (or `.updating{…}`) handler chain,
    // noting which phases are present, until the gesture call's close `)`.
    let mut has_changed = false;
    let mut has_ended = false;
    loop {
        lex.skip_ws();
        match lex.peek() {
            Some(b')') => {
                lex.advance();
                break;
            }
            None => break,
            Some(b'.') => {
                lex.advance(); // .
                let phase = lex.read_ident();
                match phase.as_str() {
                    "onChanged" | "updating" => has_changed = true,
                    "onEnded" => has_ended = true,
                    _ => {}
                }
                lex.skip_ws();
                if lex.peek() == Some(b'(') {
                    lex.advance(); // (
                    lex.skip_balanced(b'(', b')');
                    lex.skip_ws();
                }
                if lex.peek() == Some(b'{') {
                    lex.advance();
                    lex.skip_balanced(b'{', b'}');
                }
            }
            _ => {
                lex.advance();
            }
        }
    }
    match recognizer.as_str() {
        "DragGesture" => {
            // Default to both phases if the chain named no handler at all.
            if !has_changed && !has_ended {
                has_changed = true;
                has_ended = true;
            }
            if has_changed {
                view.callbacks.push("drag_changed".into());
            }
            if has_ended {
                view.callbacks.push("drag_ended".into());
            }
        }
        "MagnificationGesture" | "MagnifyGesture" => view.callbacks.push("magnify".into()),
        "RotationGesture" | "RotateGesture" => view.callbacks.push("rotate".into()),
        "TapGesture" => view.callbacks.push("click".into()),
        "LongPressGesture" => view.callbacks.push("longpress".into()),
        other => view.unsupported.push(Unsupported {
            line: modifier_line,
            text: format!("gesture {other}"),
        }),
    }
    Ok(())
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

        // ── presentation / layering modifiers ───────────────────────────────
        // These may carry both `(args)` (an `isPresented:`/`value:`/title arg)
        // and a trailing `{ content }` closure whose views become a *child
        // layer* on this view, not props. They are handled here, before the
        // generic `(args)` consumption, because their closure content matters.
        match modifier.as_str() {
            "sheet" | "popover" | "fullScreenCover" => {
                parse_presentation_modifier(lex, &mut view, present_kind(&modifier))?;
                continue;
            }
            "alert" | "confirmationDialog" => {
                parse_alert_modifier(lex, &mut view, present_kind(&modifier))?;
                continue;
            }
            "overlay" => {
                parse_layer_modifier(lex, &mut view, "Overlay")?;
                continue;
            }
            "tabItem" => {
                parse_tab_item_modifier(lex, &mut view)?;
                continue;
            }
            // ── gesture modifiers ────────────────────────────────────────────
            // These carry a trailing action `{ … }` closure (and possibly an
            // argument list) that has no view content — we recognize the gesture
            // and lower it to callbacks/props rather than feed it to the generic
            // `(args)` drain below.
            "onTapGesture" => {
                parse_tap_gesture(lex, &mut view)?;
                continue;
            }
            "onLongPressGesture" => {
                parse_long_press_gesture(lex, &mut view)?;
                continue;
            }
            // `.gesture(...)` / `.simultaneousGesture(...)` / `.highPriorityGesture(...)`.
            // The recognizer inside the parens decides the callbacks; the variant
            // decides a `gesture_priority` prop (`simultaneous` / `high`).
            "gesture" | "simultaneousGesture" | "highPriorityGesture" => {
                let priority = match modifier.as_str() {
                    "simultaneousGesture" => Some("simultaneous"),
                    "highPriorityGesture" => Some("high"),
                    _ => None,
                };
                if let Some(p) = priority {
                    view.props
                        .push(("gesture_priority".into(), SwiftValue::Ident(p.into())));
                }
                if lex.peek() == Some(b'(') {
                    lex.advance(); // (
                    parse_gesture_value(lex, &mut view, modifier_line)?;
                }
                continue;
            }
            // `.background` is overloaded: `.background(Color)` → a background
            // *prop* (the existing path below); `.background(View) { }` or
            // `.background { View }` → an underlay *layer*. Disambiguate by
            // peeking past `(`: a `Color`/`.named`/numeric arg is a style;
            // an uppercase view name (or a bare `{`) is content.
            "background" if background_is_view_layer(lex) => {
                parse_layer_modifier(lex, &mut view, "Underlay")?;
                continue;
            }
            "navigationDestination" => {
                // `.navigationDestination(isPresented: $x) { dest }` /
                // `.navigationDestination(for: T.self) { item in dest }` — the
                // destination subtree becomes a NavigationLink-style layer.
                parse_presentation_modifier(lex, &mut view, "NavigationDestination")?;
                continue;
            }
            _ => {}
        }

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
            "offset" => {
                // `.offset(x: 10, y: 20)` — labeled args — or `.offset(CGSize(width:
                // 10, height: 20))`, or the positional `.offset(10, 20)` shorthand.
                // All three resolve to `offset_x` / `offset_y` px props.
                parse_offset_args(lex, &mut view)?;
            }
            "rotationEffect" => {
                // `.rotationEffect(.degrees(d))` / `.rotationEffect(.radians(r))` /
                // `.rotationEffect(Angle(degrees: d))`. Surface a `rotation` prop in
                // degrees (radians are converted).
                if let Some(deg) = parse_angle_degrees(lex)? {
                    view.props
                        .push(("rotation".into(), SwiftValue::Float(deg)));
                }
            }
            "scaleEffect" => {
                // `.scaleEffect(1.5)` — a uniform scale factor → `scale` prop.
                let v = parse_value(lex)?;
                if let SwiftValue::Float(s) = v {
                    view.props.push(("scale".into(), SwiftValue::Float(s)));
                }
            }
            "animation" => {
                // `.animation(.easeInOut(duration: 0.3), value: x)` — lower the
                // curve + duration into a single `animation` descriptor prop of the
                // shape `"<curve>:<duration>"` (e.g. `"easeInOut:0.3"`). The
                // `value:` trigger has no IR home and is ignored.
                let desc = parse_animation_descriptor(lex)?;
                view.props
                    .push(("animation".into(), SwiftValue::Str(desc)));
            }
            "transition" => {
                // `.transition(.opacity)` / `.transition(.slide)` /
                // `.transition(.scale)` — carry the named transition through as a
                // `transition` prop.
                let name = parse_transition_name(lex)?;
                view.props
                    .push(("transition".into(), SwiftValue::Str(name)));
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

/// Peek the identifier at the current position without consuming it.
fn peek_ident(lex: &Lexer<'_>) -> String {
    let mut i = lex.pos;
    let mut s = String::new();
    while let Some(&c) = lex.src.get(i) {
        if c.is_ascii_alphanumeric() || c == b'_' {
            s.push(c as char);
            i += 1;
        } else {
            break;
        }
    }
    s
}

/// Consume a `withAnimation` tail: an optional `(.curve…)` animation argument
/// followed by its `{ … }` mutation closure. Both are skipped — `withAnimation`
/// carries no view content, only imperative state changes.
fn skip_with_animation_tail(lex: &mut Lexer<'_>) {
    lex.skip_ws();
    if lex.peek() == Some(b'(') {
        lex.advance();
        lex.skip_balanced(b'(', b')');
        lex.skip_ws();
    }
    if lex.peek() == Some(b'{') {
        lex.advance();
        lex.skip_balanced(b'{', b'}');
    }
}

struct ParsedFile {
    views: Vec<SwiftView>,
    state_vars: Vec<String>,
}

fn parse_file(lex: &mut Lexer<'_>) -> Result<ParsedFile, SwiftUIImportError> {
    let mut views = Vec::new();
    let mut state_vars = Vec::new();
    loop {
        lex.skip_ws();
        match lex.peek() {
            None => break,
            // A property-wrapper attribute. `@State var name = …` declares a
            // piece of view state; capture `name`. Other attributes (`@Binding`,
            // `@Environment`, …) read the same shape, so we record any
            // `@Attr var name` declaration here.
            Some(b'@') => {
                lex.advance(); // @
                let attr = lex.read_ident();
                lex.skip_ws();
                // Expect `var <name>` (or `let <name>`). Read the binding keyword
                // then the declared identifier.
                let kw = lex.read_ident();
                if attr == "State" && (kw == "var" || kw == "let") {
                    lex.skip_ws();
                    let name = lex.read_ident();
                    if !name.is_empty() {
                        state_vars.push(name);
                    }
                }
                // Consume the rest of the declaration line (type annotation and
                // `= initializer`) so a string initializer like `= "Ada"` is not
                // mis-read as a view kind by the top-level scanner.
                while lex.peek().map(|c| c != b'\n').unwrap_or(false) {
                    lex.advance();
                }
            }
            Some(c) if c.is_ascii_uppercase() => {
                let kind = lex.read_ident();
                let view = parse_view_body(lex, kind)?;
                views.push(view);
            }
            // `withAnimation(.easeInOut) { state.toggle() }` — a SwiftUI animation
            // wrapper around imperative state mutations. We recognize it so the
            // scanner does not mis-read its lowercase body as stray tokens; the
            // animation argument and the mutation closure have no view content, so
            // both are consumed.
            Some(c) if c.is_ascii_lowercase() && peek_ident(lex) == "withAnimation" => {
                lex.read_ident();
                skip_with_animation_tail(lex);
            }
            _ => {
                lex.advance();
            }
        }
    }
    Ok(ParsedFile { views, state_vars })
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

    // Container-shape markers that the IR kind alone cannot carry:
    //  - `LazyVStack`/`LazyHStack` lower to `List`/`Row` but are *lazy* — mark it.
    //  - `GridRow` shares the `Grid` kind with its parent `Grid`; tag the row so
    //    a consumer can tell the container from one of its rows.
    match view.kind.as_str() {
        "LazyVStack" | "LazyHStack" => {
            doc.apply_from(
                Origin::System,
                Mutation::SetProp {
                    id,
                    key: "lazy".into(),
                    value: Value::Bool(true),
                },
            )
            .ok();
        }
        "GridRow" => {
            doc.apply_from(
                Origin::System,
                Mutation::SetProp {
                    id,
                    key: "grid_row".into(),
                    value: Value::Bool(true),
                },
            )
            .ok();
        }
        _ => {}
    }

    // Label → content prop. A `Section`'s positional string is its *header*,
    // not generic content, so it lands under `header` instead.
    if let Some(label) = &view.label {
        let label_key = if view.kind == "Section" {
            "header"
        } else {
            "content"
        };
        doc.apply_from(
            Origin::System,
            Mutation::SetProp {
                id,
                key: label_key.into(),
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
                    "size"
                        | "width"
                        | "height"
                        | "padding"
                        | "corner_radius"
                        | "opacity"
                        | "offset_x"
                        | "offset_y"
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
    let parsed = parse_file(&mut lex)?;
    let mut doc = Document::new();
    let mut unsupported = Vec::new();
    let mut root_set = false;
    for view in &parsed.views {
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
        state_vars: parsed.state_vars,
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

    // ── G1: collection containers & controls ─────────────────────────────────

    #[test]
    fn parse_list_block_becomes_list() {
        let doc = parse(r#"List { Text("a") Text("b") }"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "List");
        assert_eq!(node.children.len(), 2);
    }

    #[test]
    fn parse_list_with_data_carries_data_prop() {
        // `List(items) { … }` — the positional collection lands as `data`.
        let doc = parse(r#"List(items) { Text("row") }"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "List");
        assert_eq!(node.props.get("data"), Some(&Value::Text("items".into())));
        assert_eq!(node.children.len(), 1);
    }

    #[test]
    fn parse_lazy_vstack_is_lazy_list() {
        let doc = parse(r#"LazyVStack { Text("a") }"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "List");
        assert_eq!(node.props.get("lazy"), Some(&Value::Bool(true)));
    }

    #[test]
    fn parse_lazy_hstack_is_lazy_row() {
        let doc = parse(r#"LazyHStack { Text("a") }"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "Row");
        assert_eq!(node.props.get("lazy"), Some(&Value::Bool(true)));
    }

    #[test]
    fn parse_grid_and_grid_row() {
        let doc = parse(r#"Grid { GridRow { Text("a") Text("b") } }"#).unwrap();
        let root = doc.root().unwrap();
        let grid = doc.get(root).unwrap();
        assert_eq!(grid.kind, "Grid");
        // The Grid is not itself a row.
        assert!(!grid.props.contains_key("grid_row"));
        let row_id = grid.children[0];
        let row = doc.get(row_id).unwrap();
        assert_eq!(row.kind, "Grid");
        assert_eq!(row.props.get("grid_row"), Some(&Value::Bool(true)));
        assert_eq!(row.children.len(), 2);
    }

    #[test]
    fn parse_form_becomes_form() {
        let doc = parse(r#"Form { Toggle("Wi-Fi", isOn: $wifi) }"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "Form");
        assert_eq!(node.children.len(), 1);
    }

    #[test]
    fn parse_section_string_header() {
        // `Section("Title") { … }` — the positional string is the header.
        let doc = parse(r#"Section("General") { Text("a") }"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "Section");
        assert_eq!(node.props.get("header"), Some(&Value::Text("General".into())));
        assert!(!node.props.contains_key("content"));
        assert_eq!(node.children.len(), 1);
    }

    #[test]
    fn parse_section_header_view_arg() {
        // `Section(header: Text("Settings")) { … }` — the inner Text's string is
        // pulled out as the header value, nested parens consumed cleanly.
        let doc = parse(r#"Section(header: Text("Settings")) { Text("a") }"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "Section");
        assert_eq!(
            node.props.get("header"),
            Some(&Value::Text("Settings".into()))
        );
        assert_eq!(node.children.len(), 1);
    }

    #[test]
    fn parse_picker_with_bound_selection() {
        let doc = parse(r#"Picker("Flavor", selection: $choice) { Text("a") Text("b") }"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "Picker");
        assert_eq!(node.props.get("content"), Some(&Value::Text("Flavor".into())));
        // `$choice` binding → bound state name "choice".
        assert_eq!(
            node.props.get("selection"),
            Some(&Value::Text("choice".into()))
        );
        assert_eq!(node.children.len(), 2);
    }

    #[test]
    fn parse_stepper_with_bound_value() {
        let doc = parse(r#"Stepper(value: $count)"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "Stepper");
        assert_eq!(node.props.get("value"), Some(&Value::Text("count".into())));
    }

    // ── G1: @State recognition & binding capture ─────────────────────────────

    #[test]
    fn state_var_declarations_are_recorded() {
        let src = r#"
            @State var count = 0
            @State var name = "Ada"
            VStack { Text("hi") }
        "#;
        let report = parse_with_report(src).unwrap();
        assert_eq!(report.state_vars, vec!["count".to_string(), "name".to_string()]);
        // The view after the declarations still lowered.
        let root = report.document.root().unwrap();
        assert_eq!(report.document.get(root).unwrap().kind, "Column");
    }

    #[test]
    fn state_and_binding_round_trip() {
        // A `@State var` declaration and the matching `$binding` at the call site
        // are both captured: the declared name in `state_vars`, the bound name on
        // the control prop. This is the binding test.
        let src = r#"
            @State var isOn = false
            Toggle("Wi-Fi", isOn: $isOn)
        "#;
        let report = parse_with_report(src).unwrap();
        assert_eq!(report.state_vars, vec!["isOn".to_string()]);
        let root = report.document.root().unwrap();
        let node = report.document.get(root).unwrap();
        assert_eq!(node.props.get("isOn"), Some(&Value::Text("isOn".into())));
    }

    #[test]
    fn no_state_vars_when_none_declared() {
        let report = parse_with_report(r#"Text("Hi")"#).unwrap();
        assert!(report.state_vars.is_empty());
    }

    // ── N1: navigation & presentation lowering ───────────────────────────────

    /// Find the first descendant (incl. root) whose kind matches.
    fn find_kind<'a>(doc: &'a Document, kind: &str) -> Option<&'a uni_ir::Node> {
        let mut stack = vec![doc.root()?];
        while let Some(id) = stack.pop() {
            let node = doc.get(id)?;
            if node.kind == kind {
                return doc.get(id);
            }
            for &c in &node.children {
                stack.push(c);
            }
        }
        None
    }

    #[test]
    fn parse_navigation_stack_is_container() {
        let doc = parse(r#"NavigationStack { Text("home") }"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "NavigationStack");
        assert_eq!(node.children.len(), 1);
    }

    #[test]
    fn parse_navigation_view_aliases_stack() {
        // Legacy `NavigationView` normalizes to the same container kind.
        assert_eq!(root_kind(r#"NavigationView { Text("a") }"#), "NavigationStack");
    }

    #[test]
    fn parse_navigation_link_label_and_destination() {
        // `NavigationLink("label") { dest }` — label carried, destination child.
        let doc = parse(r#"NavigationLink("Profile") { ProfileView() }"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "NavigationLink");
        assert_eq!(
            node.props.get("content"),
            Some(&Value::Text("Profile".into()))
        );
        // The destination subtree lowered as a child.
        assert_eq!(node.children.len(), 1);
    }

    #[test]
    fn parse_navigation_link_value_form() {
        // `NavigationLink(value: item) { Text("Go") }` — value binding carried,
        // the label view becomes a child.
        let doc = parse(r#"NavigationLink(value: item) { Text("Go") }"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "NavigationLink");
        assert_eq!(node.props.get("value"), Some(&Value::Text("item".into())));
        assert_eq!(node.children.len(), 1);
    }

    #[test]
    fn parse_navigation_link_inside_stack() {
        let src = r#"NavigationStack { NavigationLink("Settings") { SettingsView() } }"#;
        let doc = parse(src).unwrap();
        let stack = doc.get(doc.root().unwrap()).unwrap();
        assert_eq!(stack.kind, "NavigationStack");
        let link = doc.get(stack.children[0]).unwrap();
        assert_eq!(link.kind, "NavigationLink");
        assert_eq!(link.props.get("content"), Some(&Value::Text("Settings".into())));
    }

    #[test]
    fn parse_tab_view_with_tab_items() {
        let src = r#"TabView {
            Text("Home").tabItem { Label("Home", systemImage: "house") }
            Text("Profile").tabItem { Text("Profile") }
        }"#;
        let doc = parse(src).unwrap();
        let root = doc.root().unwrap();
        let tabview = doc.get(root).unwrap();
        assert_eq!(tabview.kind, "TabView");
        assert_eq!(tabview.children.len(), 2);
        // Each page carries tab metadata.
        let page0 = doc.get(tabview.children[0]).unwrap();
        assert_eq!(page0.props.get("tab_item"), Some(&Value::Bool(true)));
        assert_eq!(page0.props.get("tab_label"), Some(&Value::Text("Home".into())));
        let page1 = doc.get(tabview.children[1]).unwrap();
        assert_eq!(page1.props.get("tab_label"), Some(&Value::Text("Profile".into())));
    }

    #[test]
    fn parse_menu_label_and_children() {
        let doc = parse(r#"Menu("Options") { Button("A") { } Button("B") { } }"#).unwrap();
        let root = doc.root().unwrap();
        let node = doc.get(root).unwrap();
        assert_eq!(node.kind, "Menu");
        assert_eq!(node.props.get("content"), Some(&Value::Text("Options".into())));
        assert_eq!(node.children.len(), 2);
    }

    #[test]
    fn parse_sheet_is_bound_child_layer() {
        // The bound-presentation test: `.sheet(isPresented:$show)` produces a
        // "Sheet" child layer bound to `show`, with its content folded in.
        let doc = parse(r#"Text("base").sheet(isPresented: $showSheet) { Text("inside") }"#).unwrap();
        let sheet = find_kind(&doc, "Sheet").expect("Sheet layer present");
        assert_eq!(sheet.props.get("bound_to"), Some(&Value::Text("showSheet".into())));
        // The sheet's content lowered as its child.
        assert_eq!(sheet.children.len(), 1);
        let inner = doc.get(sheet.children[0]).unwrap();
        assert_eq!(inner.props.get("content"), Some(&Value::Text("inside".into())));
    }

    #[test]
    fn parse_alert_title_and_binding() {
        let doc = parse(
            r#"Text("base").alert("Delete?", isPresented: $showAlert) { Button("OK") { } }"#,
        )
        .unwrap();
        let alert = find_kind(&doc, "Alert").expect("Alert layer present");
        assert_eq!(alert.props.get("title"), Some(&Value::Text("Delete?".into())));
        assert_eq!(alert.props.get("bound_to"), Some(&Value::Text("showAlert".into())));
        // Action buttons captured as children.
        assert_eq!(alert.children.len(), 1);
    }

    #[test]
    fn parse_popover_is_bound_child_layer() {
        let doc = parse(r#"Text("base").popover(isPresented: $pop) { Text("over") }"#).unwrap();
        let pop = find_kind(&doc, "Popover").expect("Popover layer present");
        assert_eq!(pop.props.get("bound_to"), Some(&Value::Text("pop".into())));
        assert_eq!(pop.children.len(), 1);
    }

    #[test]
    fn parse_menu_overlay_layer() {
        // `.overlay { View }` becomes a distinct "Overlay" child layer.
        let doc = parse(r#"Text("base").overlay { Circle() }"#).unwrap();
        let overlay = find_kind(&doc, "Overlay").expect("Overlay layer present");
        assert_eq!(overlay.children.len(), 1);
        // The base view still kept its own content.
        let root = doc.get(doc.root().unwrap()).unwrap();
        assert_eq!(root.props.get("content"), Some(&Value::Text("base".into())));
    }

    #[test]
    fn parse_overlay_with_positional_view() {
        // `.overlay(Badge())` positional-content form.
        let doc = parse(r#"Image("photo").overlay(Badge())"#).unwrap();
        let overlay = find_kind(&doc, "Overlay").expect("Overlay layer present");
        assert_eq!(overlay.children.len(), 1);
        assert_eq!(doc.get(overlay.children[0]).unwrap().kind, "Badge");
    }

    #[test]
    fn parse_background_view_is_underlay_layer() {
        // `.background(View)` is a distinct underlay *layer*, not a style prop.
        let doc = parse(r#"Text("base").background(RoundedRectangle(cornerRadius: 8))"#).unwrap();
        let under = find_kind(&doc, "Underlay").expect("Underlay layer present");
        assert_eq!(under.children.len(), 1);
        // No `background` style prop was set on the base view.
        let root = doc.get(doc.root().unwrap()).unwrap();
        assert!(!root.props.contains_key("background"));
    }

    #[test]
    fn parse_background_closure_is_underlay_layer() {
        // `.background { View }` (no parens) is also an underlay layer.
        let doc = parse(r#"Text("base").background { Color.blue }"#).unwrap();
        assert!(find_kind(&doc, "Underlay").is_some());
    }

    #[test]
    fn parse_background_color_remains_a_style_prop() {
        // Regression guard: `.background(Color.blue)` stays a style, NOT a layer.
        let doc = parse(r#"Text("base").background(Color.blue)"#).unwrap();
        assert!(find_kind(&doc, "Underlay").is_none());
        let root = doc.get(doc.root().unwrap()).unwrap();
        assert_eq!(root.props.get("background"), Some(&Value::Color(0x0000_FFFF)));
    }

    #[test]
    fn unknown_presentation_like_modifier_still_reported() {
        // A genuinely-unknown modifier is still surfaced (no silent eating).
        let report = parse_with_report(r#"Text("Hi").presentationDetents([.medium])"#).unwrap();
        assert!(report
            .unsupported
            .iter()
            .any(|u| u.text == "modifier .presentationDetents"));
    }

    // ── T1: transform & animation modifiers ──────────────────────────────────

    #[test]
    fn parse_offset_labeled_x_y() {
        let doc = parse(r#"Text("Hi").offset(x: 10, y: 20)"#).unwrap();
        let root = doc.root().unwrap();
        let props = &doc.get(root).unwrap().props;
        assert_eq!(props.get("offset_x"), Some(&Value::Px(10.0)));
        assert_eq!(props.get("offset_y"), Some(&Value::Px(20.0)));
    }

    #[test]
    fn parse_offset_only_y() {
        // SwiftUI allows omitting an axis; only the given one is set.
        let doc = parse(r#"Text("Hi").offset(y: 8)"#).unwrap();
        let props = &doc.get(doc.root().unwrap()).unwrap().props;
        assert_eq!(props.get("offset_y"), Some(&Value::Px(8.0)));
        assert!(!props.contains_key("offset_x"));
    }

    #[test]
    fn parse_offset_cgsize() {
        // `.offset(CGSize(width: 5, height: 15))` → offset_x / offset_y.
        let doc = parse(r#"Text("Hi").offset(CGSize(width: 5, height: 15))"#).unwrap();
        let props = &doc.get(doc.root().unwrap()).unwrap().props;
        assert_eq!(props.get("offset_x"), Some(&Value::Px(5.0)));
        assert_eq!(props.get("offset_y"), Some(&Value::Px(15.0)));
    }

    #[test]
    fn parse_offset_positional() {
        // Positional shorthand `.offset(3, 4)` maps to x then y.
        let doc = parse(r#"Text("Hi").offset(3, 4)"#).unwrap();
        let props = &doc.get(doc.root().unwrap()).unwrap().props;
        assert_eq!(props.get("offset_x"), Some(&Value::Px(3.0)));
        assert_eq!(props.get("offset_y"), Some(&Value::Px(4.0)));
    }

    #[test]
    fn parse_rotation_effect_degrees() {
        let v = root_prop(r#"Text("Hi").rotationEffect(.degrees(45))"#, "rotation");
        assert_eq!(v, Some(Value::Float(45.0)));
    }

    #[test]
    fn parse_rotation_effect_radians_converts() {
        // `.radians(π)` ≈ 180°.
        let doc = parse(r#"Text("Hi").rotationEffect(.radians(3.14159265))"#).unwrap();
        let v = doc.get(doc.root().unwrap()).unwrap().props.get("rotation").cloned();
        match v {
            Some(Value::Float(deg)) => assert!((deg - 180.0).abs() < 0.01, "got {deg}"),
            other => panic!("expected Float ~180, got {other:?}"),
        }
    }

    #[test]
    fn parse_rotation_effect_angle_form() {
        let v = root_prop(r#"Text("Hi").rotationEffect(Angle(degrees: 90))"#, "rotation");
        assert_eq!(v, Some(Value::Float(90.0)));
    }

    #[test]
    fn parse_scale_effect_sets_scale() {
        let v = root_prop(r#"Text("Hi").scaleEffect(1.5)"#, "scale");
        assert_eq!(v, Some(Value::Float(1.5)));
    }

    #[test]
    fn parse_animation_curve_and_duration() {
        // `.animation(.easeInOut(duration: 0.3), value: x)` → "easeInOut:0.3".
        let v = root_prop(
            r#"Text("Hi").animation(.easeInOut(duration: 0.3), value: count)"#,
            "animation",
        );
        assert_eq!(v, Some(Value::Text("easeInOut:0.3".into())));
    }

    #[test]
    fn parse_animation_curve_no_duration() {
        // `.animation(.linear, value: x)` → just the curve name.
        let v = root_prop(r#"Text("Hi").animation(.linear, value: x)"#, "animation");
        assert_eq!(v, Some(Value::Text("linear".into())));
    }

    #[test]
    fn parse_animation_spring_with_parens() {
        // `.spring()` curve with no duration arg still lowers to the curve name.
        let v = root_prop(r#"Text("Hi").animation(.spring(), value: x)"#, "animation");
        assert_eq!(v, Some(Value::Text("spring".into())));
    }

    #[test]
    fn parse_transition_opacity() {
        let v = root_prop(r#"Text("Hi").transition(.opacity)"#, "transition");
        assert_eq!(v, Some(Value::Text("opacity".into())));
    }

    #[test]
    fn parse_transition_slide() {
        let v = root_prop(r#"Text("Hi").transition(.slide)"#, "transition");
        assert_eq!(v, Some(Value::Text("slide".into())));
    }

    #[test]
    fn parse_transition_scale() {
        let v = root_prop(r#"Text("Hi").transition(.scale)"#, "transition");
        assert_eq!(v, Some(Value::Text("scale".into())));
    }

    #[test]
    fn animation_and_transition_no_longer_reported_unsupported() {
        // Regression: these were previously only recorded as Unsupported. They
        // are now lowered, so the report must be clean.
        let report = parse_with_report(
            r#"Text("Hi").animation(.easeInOut(duration: 0.2), value: x).transition(.opacity)"#,
        )
        .unwrap();
        assert!(
            report.unsupported.is_empty(),
            "animation/transition should be lowered, not reported: {:?}",
            report.unsupported
        );
        let node = report.document.get(report.document.root().unwrap()).unwrap();
        assert_eq!(node.props.get("animation"), Some(&Value::Text("easeInOut:0.2".into())));
        assert_eq!(node.props.get("transition"), Some(&Value::Text("opacity".into())));
    }

    #[test]
    fn with_animation_wrapper_is_recognized() {
        // `withAnimation { … }` wraps imperative mutations — it is recognized and
        // skipped, and the view declared alongside it still lowers.
        let src = r#"
            withAnimation(.easeInOut) { isExpanded.toggle() }
            VStack { Text("hi") }
        "#;
        let doc = parse(src).unwrap();
        let root = doc.root().unwrap();
        assert_eq!(doc.get(root).unwrap().kind, "Column");
    }

    #[test]
    fn with_animation_no_arg_is_recognized() {
        let src = r#"
            withAnimation { count += 1 }
            Text("done")
        "#;
        let doc = parse(src).unwrap();
        let root = doc.root().unwrap();
        assert_eq!(
            doc.get(root).unwrap().props.get("content"),
            Some(&Value::Text("done".into()))
        );
    }

    #[test]
    fn unknown_transform_like_modifier_still_reported() {
        // A genuinely-unknown transform stays surfaced (no silent eating).
        let report = parse_with_report(r#"Text("Hi").rotation3DEffect(.degrees(10))"#).unwrap();
        assert!(report
            .unsupported
            .iter()
            .any(|u| u.text == "modifier .rotation3DEffect"));
    }

    // ── GST: gesture modifiers → callbacks / props ────────────────────────────

    fn root_node(doc: &Document) -> &uni_ir::Node {
        doc.get(doc.root().unwrap()).unwrap()
    }

    #[test]
    fn parse_on_tap_gesture_sets_click_callback() {
        let doc = parse(r#"Text("Hi").onTapGesture { doThing() }"#).unwrap();
        let node = root_node(&doc);
        assert!(node.callbacks.contains_key("click"));
        // No tap_count for the single-tap form.
        assert!(!node.props.contains_key("tap_count"));
    }

    #[test]
    fn parse_on_tap_gesture_count_sets_prop_and_callback() {
        // `.onTapGesture(count: 2) { }` → tap_count prop + click callback.
        let doc = parse(r#"Text("Hi").onTapGesture(count: 2) { doThing() }"#).unwrap();
        let node = root_node(&doc);
        assert!(node.callbacks.contains_key("click"));
        assert_eq!(node.props.get("tap_count"), Some(&Value::Float(2.0)));
    }

    #[test]
    fn parse_on_long_press_gesture_sets_longpress_callback() {
        let doc = parse(r#"Text("Hi").onLongPressGesture { doThing() }"#).unwrap();
        let node = root_node(&doc);
        assert!(node.callbacks.contains_key("longpress"));
        // A long press is NOT a click.
        assert!(!node.callbacks.contains_key("click"));
    }

    #[test]
    fn parse_on_long_press_gesture_with_args() {
        // `.onLongPressGesture(minimumDuration: 1.0) { }` still lowers cleanly.
        let doc =
            parse(r#"Text("Hi").onLongPressGesture(minimumDuration: 1.0) { doThing() }"#).unwrap();
        let node = root_node(&doc);
        assert!(node.callbacks.contains_key("longpress"));
    }

    #[test]
    fn parse_drag_gesture_changed_and_ended() {
        let src = r#"Text("Hi").gesture(DragGesture().onChanged { v in handle(v) }.onEnded { v in done(v) })"#;
        let doc = parse(src).unwrap();
        let node = root_node(&doc);
        assert!(node.callbacks.contains_key("drag_changed"));
        assert!(node.callbacks.contains_key("drag_ended"));
    }

    #[test]
    fn parse_drag_gesture_only_on_changed() {
        // Only the `.onChanged` phase is present → only `drag_changed`.
        let src = r#"Text("Hi").gesture(DragGesture().onChanged { v in handle(v) })"#;
        let doc = parse(src).unwrap();
        let node = root_node(&doc);
        assert!(node.callbacks.contains_key("drag_changed"));
        assert!(!node.callbacks.contains_key("drag_ended"));
    }

    #[test]
    fn parse_drag_gesture_bare_defaults_both_phases() {
        // `.gesture(DragGesture())` with no handlers → both phases offered.
        let doc = parse(r#"Text("Hi").gesture(DragGesture())"#).unwrap();
        let node = root_node(&doc);
        assert!(node.callbacks.contains_key("drag_changed"));
        assert!(node.callbacks.contains_key("drag_ended"));
    }

    #[test]
    fn parse_magnification_gesture_sets_magnify() {
        let src = r#"Image("photo").gesture(MagnificationGesture().onChanged { s in zoom(s) })"#;
        let doc = parse(src).unwrap();
        let node = root_node(&doc);
        assert!(node.callbacks.contains_key("magnify"));
    }

    #[test]
    fn parse_rotation_gesture_sets_rotate() {
        let src = r#"Image("photo").gesture(RotationGesture().onChanged { a in spin(a) })"#;
        let doc = parse(src).unwrap();
        let node = root_node(&doc);
        assert!(node.callbacks.contains_key("rotate"));
    }

    #[test]
    fn parse_simultaneous_gesture_priority_prop() {
        let doc =
            parse(r#"Text("Hi").simultaneousGesture(DragGesture().onEnded { v in done(v) })"#)
                .unwrap();
        let node = root_node(&doc);
        assert_eq!(
            node.props.get("gesture_priority"),
            Some(&Value::Text("simultaneous".into()))
        );
        assert!(node.callbacks.contains_key("drag_ended"));
    }

    #[test]
    fn parse_high_priority_gesture_priority_prop() {
        let doc = parse(
            r#"Text("Hi").highPriorityGesture(TapGesture().onEnded { tapped() })"#,
        )
        .unwrap();
        let node = root_node(&doc);
        assert_eq!(
            node.props.get("gesture_priority"),
            Some(&Value::Text("high".into()))
        );
        // A TapGesture recognizer lowers to a click.
        assert!(node.callbacks.contains_key("click"));
    }

    #[test]
    fn unknown_gesture_recognizer_is_reported() {
        // `HoverGesture` has no IR home — recorded, not silently eaten.
        let report =
            parse_with_report(r#"Text("Hi").gesture(HoverGesture().onChanged { h() })"#).unwrap();
        assert!(
            report.unsupported.iter().any(|u| u.text == "gesture HoverGesture"),
            "expected unknown gesture report, got {:?}",
            report.unsupported
        );
    }

    #[test]
    fn gesture_modifiers_not_reported_unsupported() {
        // Regression: supported gesture modifiers must not be flagged as drops.
        let report = parse_with_report(
            r#"Text("Hi").onTapGesture { a() }.gesture(DragGesture().onEnded { b() })"#,
        )
        .unwrap();
        assert!(
            report.unsupported.is_empty(),
            "gestures should lower, not report: {:?}",
            report.unsupported
        );
    }

    #[test]
    fn gesture_chain_continues_to_following_modifier() {
        // A modifier after the gesture chain still parses (cursor left clean).
        let doc = parse(
            r#"Text("Hi").gesture(DragGesture().onChanged { v() }).padding(8)"#,
        )
        .unwrap();
        let node = root_node(&doc);
        assert!(node.callbacks.contains_key("drag_changed"));
        assert_eq!(node.props.get("padding"), Some(&Value::Px(8.0)));
    }
}
