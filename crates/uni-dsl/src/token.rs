//! The `.uni` lexer, built on `logos`.
//!
//! We tokenize the *surface* syntax only: structural punctuation, the few
//! literal shapes (string / number / bool / color / length) and bare
//! identifiers. Meaning (is this identifier an element kind or a property
//! name? is this number an int or a float?) is resolved by the parser, not
//! here — the lexer stays dumb and fast on purpose.

use logos::Logos;

/// A single lexical token. `Color` and `Length` are recognized lexically
/// because their leading `#` / trailing `px` make them unambiguous, but we
/// keep the *string slice* and parse the payload during AST construction so
/// the lexer never allocates or fails on a number it can't fit.
#[derive(Logos, Debug, Clone, PartialEq, Eq, Hash)]
#[logos(skip r"[ \t\r\n\f]+")] // whitespace
#[logos(skip r"//[^\n]*")] // line comments
pub enum Token {
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token(":")]
    Colon,
    #[token(";")]
    Semicolon,

    #[token("true", |_| true)]
    #[token("false", |_| false)]
    Bool(bool),

    // Color: `#RRGGBB` or `#RRGGBBAA`.
    #[regex(r"#[0-9a-fA-F]{8}", |lex| lex.slice().to_owned())]
    #[regex(r"#[0-9a-fA-F]{6}", |lex| lex.slice().to_owned())]
    Color(String),

    // Length: a number immediately followed by `px`, e.g. `16px`, `1.5px`.
    #[regex(r"[0-9]+(\.[0-9]+)?px", |lex| lex.slice().to_owned())]
    Length(String),

    // Bare number: int or decimal. (`px`-suffixed numbers are caught above.)
    #[regex(r"[0-9]+(\.[0-9]+)?", |lex| lex.slice().to_owned())]
    Number(String),

    // String literal with simple `\"` and `\\` escapes.
    #[regex(r#""([^"\\]|\\.)*""#, |lex| unescape(lex.slice()))]
    Str(String),

    // Identifier: element kind or property name.
    #[regex(r"[A-Za-z_][A-Za-z0-9_-]*", |lex| lex.slice().to_owned())]
    Ident(String),
}

/// Strip the surrounding quotes and resolve `\"`, `\\`, `\n`, `\t` escapes.
fn unescape(raw: &str) -> String {
    let inner = &raw[1..raw.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(other) => out.push(other), // covers \" and \\
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}
