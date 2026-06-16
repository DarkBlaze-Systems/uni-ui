//! The `.uni` parser, built on `chumsky` 0.9.
//!
//! Consumes the [`Token`] stream produced by the lexer and yields a single
//! root [`Element`]. Property values are resolved from their lexeme here
//! (e.g. `#7d39eb` → packed `0x7d39ebff`, `16px` → `Px(16.0)`).

use chumsky::prelude::*;

use crate::ast::{Element, Literal, Prop};
use crate::token::Token;

/// A body entry: either a property or a nested child element.
enum Entry {
    Prop(Prop),
    Child(Element),
}

/// Build the parser. The top level is exactly one element.
pub fn parser() -> impl Parser<Token, Element, Error = Simple<Token>> {
    // An identifier token → its String.
    let ident = select! { Token::Ident(name) => name };

    // A literal value token → an AST `Literal`.
    let literal = select! {
        Token::Str(s) => Literal::Str(s),
        Token::Bool(b) => Literal::Bool(b),
        Token::Color(c) => Literal::Color(parse_color(&c)),
        Token::Length(l) => Literal::Px(parse_length(&l)),
        Token::Number(n) => parse_number(&n),
    };

    // Recursively define an element so bodies can nest.
    recursive(|element| {
        // `prop: value;`
        let prop = ident
            .then_ignore(just(Token::Colon))
            .then(literal)
            .then_ignore(just(Token::Semicolon))
            .map(|(key, value)| Entry::Prop(Prop { key, value }));

        // A nested child element.
        let child = element.map(Entry::Child);

        // Body = any number of props/children, in any order.
        let entries = prop.or(child).repeated();

        // `Kind { entries }`
        ident
            .then(entries.delimited_by(just(Token::LBrace), just(Token::RBrace)))
            .map(|(kind, entries)| {
                let mut props = Vec::new();
                let mut children = Vec::new();
                for entry in entries {
                    match entry {
                        Entry::Prop(p) => props.push(p),
                        Entry::Child(c) => children.push(c),
                    }
                }
                Element {
                    kind,
                    props,
                    children,
                }
            })
    })
    .then_ignore(end())
}

/// `#RRGGBB` (expand to alpha `0xFF`) or `#RRGGBBAA` → packed `0xRRGGBBAA`.
fn parse_color(raw: &str) -> u32 {
    let hex = &raw[1..]; // drop leading '#'
    let value = u32::from_str_radix(hex, 16).unwrap_or(0);
    if hex.len() == 6 {
        (value << 8) | 0xFF
    } else {
        value
    }
}

/// `Npx` → `f32` (drop the `px` suffix).
fn parse_length(raw: &str) -> f32 {
    raw[..raw.len() - 2].parse().unwrap_or(0.0)
}

/// A bare number: integer → `Int`, anything with a `.` → `Float`.
fn parse_number(raw: &str) -> Literal {
    if raw.contains('.') {
        Literal::Float(raw.parse().unwrap_or(0.0))
    } else {
        Literal::Int(raw.parse().unwrap_or(0))
    }
}
