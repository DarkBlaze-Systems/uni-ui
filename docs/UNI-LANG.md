<!--
  UNI-LANG.md — the `.uni` declarative-UI language. Derived from
  crates/uni-dsl (token.rs, ast.rs, parser.rs, lib.rs); not aspirational.
  Where this prose and the source disagree, the source wins.
-->

# The `.uni` Language (v0)

`.uni` is our native declarative-UI source language. The `uni-dsl` crate lexes
and parses it and **lowers** it into a `uni_ir::Document`. It never edits the IR
directly: it emits the IR's own `Mutation`s through `Document::apply_from` with
`Origin::System`, so a parsed document is just a System-authored prefix of the
same cowork mutation stream a human or the AI companion would later extend.

This document is the grammar plus a worked example, tracking
`crates/uni-dsl/src/{token.rs, ast.rs, parser.rs, lib.rs}`.

---

## 1. Example

```uni
// A small panel. `//` line comments are ignored.
Stack {
    padding: 16px;
    background: #0a0a0a;

    Text {
        content: "Uni-UI";
        size: 28px;
        color: #ffffff;
    }

    Rect {
        width: 200px;
        height: 80px;
        color: #7d39eb;
    }
}
```

This lowers to a `Document` rooted at the `Stack`, with two children (`Text`,
`Rect`) in source order, every prop applied — `padding` → `Px(16.0)`,
`background` → `Color(0x0a0a0aff)` (the `#RRGGBB` form expands to full alpha),
`color: #7d39eb` → `Color(0x7d39ebff)` — all via `Origin::System` mutations
recorded in the document's audit log.

A richer example exercising the additive constructs (bindings, callbacks,
control flow):

```uni
Stack {
    width: $w;                 // bound prop  -> SetBinding { expr: "w" }
    color: $theme.accent;      // dotted-path binding
    padding: 16px;             // literal still -> SetProp

    on click: submit("form");  // callback -> SetCallback
    on hover: toggle();        // zero-arg callback

    if ($visible) {            // -> CreateNode{kind:"If"} + SetBinding{cond}
        Text { content: "shown"; }
    }

    for ($items) {             // -> CreateNode{kind:"For"} + SetBinding{items}
        Rect { width: 10px; }  //    children are the per-item template
    }
}
```

---

## 2. Lexical grammar

The lexer (built on `logos`) tokenizes surface syntax only. Whitespace
(`[ \t\r\n\f]+`) and `//` line comments are skipped. Meaning — is an identifier a
kind or a prop name? is a number an int or a float? — is resolved by the parser.

| Token | Pattern | Notes |
|-------|---------|-------|
| `LBrace` `RBrace` | `{` `}` | Block delimiters. |
| `LParen` `RParen` | `(` `)` | Control-flow condition / callback args. |
| `Colon` | `:` | Separates a key/event from its value. |
| `Semicolon` | `;` | Terminates a prop or callback entry. |
| `Comma` | `,` | Separates callback arguments. |
| `If` `For` `On` | `if` `for` `on` | Reserved keywords, matched before `Ident`. |
| `Bool` | `true` / `false` | → `Literal::Bool`. |
| `Binding` | `\$[A-Za-z_][A-Za-z0-9_-]*(\.[A-Za-z_][A-Za-z0-9_-]*)*` | A `$`-prefixed dotted path. The leading `$` is stripped; the dotted path is kept verbatim as the binding expression. |
| `Color` | `#[0-9a-fA-F]{8}` or `#[0-9a-fA-F]{6}` | Hex color; 8-digit form first. |
| `Length` | `[0-9]+(\.[0-9]+)?px` | A number immediately followed by `px`. |
| `Number` | `[0-9]+(\.[0-9]+)?` | A bare int or decimal (px-suffixed numbers are caught above). |
| `Str` | `"([^"\\]|\\.)*"` | String with `\"`, `\\`, `\n`, `\t`, `\r` escapes. |
| `Ident` | `[A-Za-z_][A-Za-z0-9_-]*` | Element kind or property name. |

Because `if` / `for` / `on` are keywords, they cannot be used as element kinds or
prop names. Identifiers may contain `-` (kebab) after the first char.

The lexer is fail-fast: the first byte it cannot tokenize (e.g. `@`) yields a
`ParseError::Lex { span }`.

---

## 3. Syntactic grammar

The parser (built on `chumsky` 0.9) consumes the token stream and yields exactly
**one** root `Element`, then requires end-of-input. In EBNF over the tokens
above:

```ebnf
document   = element , EOF ;

element    = Ident , "{" , { entry } , "}" ;

entry      = prop
           | callback
           | if_block
           | for_block
           | element ;            (* a nested child *)

prop       = Ident , ":" , value , ";" ;

value      = literal
           | Binding ;            (* $dotted.path, $ already stripped *)

callback   = "on" , Ident , ":" , Ident , args , ";" ;

args       = "(" , [ literal , { "," , literal } , [ "," ] ] , ")" ;

if_block   = "if" , "(" , Binding , ")" , "{" , { element } , "}" ;

for_block  = "for" , "(" , Binding , ")" , "{" , { element } , "}" ;

literal    = Str | Number | Bool | Color | Length ;
```

Notes faithful to the parser:

- **Entry ordering.** A body holds props, callbacks, `if`/`for` blocks, and
  nested children **freely interleaved** in any order. The parser tries
  alternatives keyword-first (`prop` → `callback` → `if_block` → `for_block` →
  `child`), so the keyword-led constructs are matched before the generic element.
- **Exactly one root.** The top level is a single element followed by `end()`;
  trailing content is a parse error.
- **Callback args** are a possibly-empty, comma-separated list of *literals*,
  with an optional trailing comma. Args are literals only — not bindings.
- **`if` / `for` conditions** are a single parenthesized `Binding` (a `$`-path),
  not an arbitrary expression in v0.
- **Control-flow bodies** contain only nested elements (no props/callbacks
  directly in the block — those belong on the children).

A malformed token stream yields `ParseError::Parse { message }` (e.g. a missing
`}` or a `on click;` with no handler); the message wraps chumsky's diagnostics.

---

## 4. Values and how they lower

The parser resolves each literal from its lexeme into an AST `Literal`, then
`lower` maps it to a `uni_ir::Value`:

| Surface form | AST `Literal` | IR `Value` |
|--------------|---------------|------------|
| `"text"` | `Str(String)` | `Value::Text` |
| `42` | `Int(i64)` | `Value::Int` |
| `1.5` | `Float(f64)` | `Value::Float` |
| `true` / `false` | `Bool(bool)` | `Value::Bool` |
| `#RRGGBB` / `#RRGGBBAA` | `Color(u32)` | `Value::Color` (packed `0xRRGGBBAA`) |
| `Npx` | `Px(f32)` | `Value::Px` |

Resolution rules:

- **Number** containing a `.` → `Float`, otherwise `Int`.
- **Color** `#RRGGBB` expands to alpha `0xFF` (`(value << 8) | 0xFF`); `#RRGGBBAA`
  is used verbatim. Result is packed `0xRRGGBBAA`.
- **Length** drops the `px` suffix and parses the remainder as `f32`.

---

## 5. Lowering to the IR

`parse(src) -> Result<Document, ParseError>` runs lex → parse → lower. Lowering
walks the AST root and emits IR mutations under `Origin::System`:

| Surface construct | Emitted mutation(s) |
|-------------------|---------------------|
| an element `Kind { .. }` | `CreateNode { id, kind }` (id from `fresh_id`) |
| `key: <literal>;` | `SetProp { id, key, value }` |
| `key: $path;` | `SetBinding { id, key, binding: { expr: path } }` |
| `on <event>: <name>(<args>);` | `SetCallback { id, event, action: { name, args } }` (args are literal `Value`s) |
| a nested child element | recurse, then `AppendChild { parent, child }` |
| the root element | `SetRoot { id }` (emitted last, after the subtree) |
| `if ($cond) { .. }` | synthetic `CreateNode { kind: "If" }` + `SetBinding { key: "cond", expr }`, children appended |
| `for ($items) { .. }` | synthetic `CreateNode { kind: "For" }` + `SetBinding { key: "items", expr }`, children appended |

Key semantics:

- A **bound prop** (`$`) lowers to `SetBinding`, not `SetProp`. A literal on a
  *different* key still lowers to `SetProp`; both kinds coexist on one node (the
  IR keeps `props` and `bindings` side by side). No literal prop is created for a
  bound key.
- **`if` / `for` lower structurally only.** They produce a synthetic node of kind
  `"If"` / `"For"` carrying the parenthesized expression as a `cond` / `items`
  element-binding, with the block's children appended as the template/body. This
  crate does **no evaluation** — the reactive layer expands them later.
- The whole lowered document is a `System`-authored prefix in the audit log; a
  human or the AI extends the same stream afterward.

### Errors

```rust
pub enum ParseError {
    Lex   { span: Range<usize> },  // unrecognized input byte span
    Parse { message: String },     // parser rejected the token stream
    Lower { message: String },     // IR rejected a lowered mutation (should be unreachable on valid input)
}
```

`ParseError` implements `Display` and `std::error::Error`. `Lower` is defensive:
on a valid parse the IR should accept every mutation we build, so it is surfaced
rather than panicked on.

---

## 6. Source of truth

This grammar is prose over `crates/uni-dsl`. The token patterns in `token.rs`,
the AST in `ast.rs`, the combinators in `parser.rs`, and the lowering in `lib.rs`
are authoritative; this document is a reading aid. The IR vocabulary these
mutations target is specified in `docs/UNI-IR-SPEC.md`.
