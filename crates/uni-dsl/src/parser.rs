//! The `.uni` parser, built on `chumsky` 0.9.
//!
//! Consumes the [`Token`] stream produced by the lexer and yields a single
//! root [`Element`]. Property values are resolved from their lexeme here
//! (e.g. `#7d39eb` → packed `0x7d39ebff`, `16px` → `Px(16.0)`).
//!
//! Beyond literal props, the grammar carries three additive constructs:
//!
//! - **Bound props** — `key: $dotted.path;` produces a [`PropValue::Binding`].
//! - **Callbacks** — `on <event>: <name>(<args>);` produces a [`Callback`].
//! - **Control flow** — `if ($cond) { .. }` / `for ($items) { .. }` produce
//!   synthetic `If` / `For` elements carrying the expression in
//!   `element_bindings`.

use chumsky::prelude::*;

use crate::ast::{Callback, Element, Literal, Prop, PropValue};
use crate::token::Token;

/// A body entry: a property, an event handler, or a nested child element.
enum Entry {
    Prop(Prop),
    Callback(Callback),
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

    // A `$dotted.path` binding token → its stripped expression string.
    let binding_expr = select! { Token::Binding(expr) => expr };

    // Recursively define an element so bodies can nest.
    recursive(|element| {
        // `key: <literal>;`  or  `key: $expr;`
        let prop = ident
            .then_ignore(just(Token::Colon))
            .then(
                literal
                    .map(PropValue::Literal)
                    .or(binding_expr.clone().map(PropValue::Binding)),
            )
            .then_ignore(just(Token::Semicolon))
            .map(|(key, value)| Entry::Prop(Prop { key, value }));

        // `(<lit>, <lit>, ...)` — a possibly-empty, comma-separated argument
        // list of literals.
        let args = literal
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .delimited_by(just(Token::LParen), just(Token::RParen));

        // `on <event>: <name>(<args>);`
        let callback = just(Token::On)
            .ignore_then(ident)
            .then_ignore(just(Token::Colon))
            .then(ident)
            .then(args)
            .then_ignore(just(Token::Semicolon))
            .map(|((event, action), args)| {
                Entry::Callback(Callback {
                    event,
                    action,
                    args,
                })
            });

        // `( $expr )` — a parenthesized binding expression, used by if/for.
        let paren_expr = binding_expr
            .clone()
            .delimited_by(just(Token::LParen), just(Token::RParen));

        // A child body shared by control-flow blocks: `{ entries }` collected
        // into props/callbacks/children. Declared via a forward closure so it
        // can reuse `element` recursively.
        let plain_element = element.clone();

        // `if ($cond) { ...children... }` → synthetic `If` element.
        let if_block = just(Token::If)
            .ignore_then(paren_expr.clone())
            .then(
                plain_element
                    .clone()
                    .repeated()
                    .delimited_by(just(Token::LBrace), just(Token::RBrace)),
            )
            .map(|(cond, children)| {
                let mut el = Element::new("If".to_string(), Vec::new(), Vec::new(), children);
                el.element_bindings.push(("cond".to_string(), cond));
                Entry::Child(el)
            });

        // `for ($items) { ...template... }` → synthetic `For` element.
        let for_block = just(Token::For)
            .ignore_then(paren_expr)
            .then(
                plain_element
                    .repeated()
                    .delimited_by(just(Token::LBrace), just(Token::RBrace)),
            )
            .map(|(items, children)| {
                let mut el = Element::new("For".to_string(), Vec::new(), Vec::new(), children);
                el.element_bindings.push(("items".to_string(), items));
                Entry::Child(el)
            });

        // A nested ordinary child element.
        let child = element.map(Entry::Child);

        // Body = any number of props / callbacks / control-flow / children,
        // freely interleaved. Order the alternatives so the keyword-led
        // constructs are tried before the generic element.
        let entry = prop
            .or(callback)
            .or(if_block)
            .or(for_block)
            .or(child);
        let entries = entry.repeated();

        // `Kind { entries }`
        ident
            .then(entries.delimited_by(just(Token::LBrace), just(Token::RBrace)))
            .map(|(kind, entries)| {
                let mut props = Vec::new();
                let mut callbacks = Vec::new();
                let mut children = Vec::new();
                for entry in entries {
                    match entry {
                        Entry::Prop(p) => props.push(p),
                        Entry::Callback(c) => callbacks.push(c),
                        Entry::Child(c) => children.push(c),
                    }
                }
                Element::new(kind, props, callbacks, children)
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
