//! Cross-dialect differential test (C3).
//!
//! The whole point of the IR is that it is *the canon, not a passthrough*: a
//! Flutter `Column`, a SwiftUI `VStack`, and a Slint `Column` are the same
//! thing once lowered. This test pins that promise down. It feeds an equivalent
//! "a button inside a column" snippet to all three importers and asserts the
//! resulting `uni_ir::Document`s are *structurally equal*:
//!
//! - same root kind,
//! - same sorted multiset of node kinds across the whole tree,
//! - same tree shape (parent/child arity at every node).
//!
//! Property *values* are deliberately out of scope — dialects spell colours and
//! sizes differently and that is fine. What must converge is the shape of the
//! lowered UI. If a future change to any one importer skews that shape, this
//! test fails loudly instead of letting the three frontends silently diverge.
//!
//! It lives in `slint-import` (with the other two as dev-dependencies) only
//! because the workspace has no neutral cross-crate test home; there is no
//! circular dependency — neither sibling importer depends on `slint-import`.

use uni_ir::{Document, NodeId};

/// A shape fingerprint of a lowered document, independent of property values.
#[derive(Debug, PartialEq, Eq)]
struct Shape {
    /// The kind of the root node.
    root_kind: String,
    /// Every node kind in the tree, sorted (a multiset).
    kind_set: Vec<String>,
    /// Child-arity at each node, in a deterministic pre-order walk.
    arities: Vec<usize>,
}

fn shape_of(doc: &Document) -> Shape {
    let root = doc.root().expect("document has a root");
    let root_kind = doc.get(root).expect("root node").kind.clone();

    let mut kind_set = Vec::new();
    let mut arities = Vec::new();
    walk(doc, root, &mut kind_set, &mut arities);
    kind_set.sort();

    Shape {
        root_kind,
        kind_set,
        arities,
    }
}

/// Deterministic pre-order walk collecting kinds and child-arities.
fn walk(doc: &Document, id: NodeId, kinds: &mut Vec<String>, arities: &mut Vec<usize>) {
    let node = doc.get(id).expect("node exists");
    kinds.push(node.kind.clone());
    arities.push(node.children.len());
    for &child in &node.children {
        walk(doc, child, kinds, arities);
    }
}

#[test]
fn button_in_column_lowers_identically_across_dialects() {
    // Slint: a Column container holding a Button. Slint has no native Column/
    // Button kinds, so they pass through as-is — which is exactly the canon.
    let slint_src = r#"
        Column {
            Button {
                text: "Go";
            }
        }
    "#;

    // Flutter: Column with a single child ElevatedButton (→ Button).
    let flutter_src = r#"Column(children: [ElevatedButton(onPressed: go)])"#;

    // SwiftUI: VStack (→ Column) wrapping a Button (→ Button).
    let swiftui_src = r#"VStack { Button("Go") { go() } }"#;

    let slint_doc = slint_import::parse(slint_src).expect("slint parses");
    let flutter_doc = flutter_import::parse(flutter_src).expect("flutter parses");
    let swiftui_doc = swiftui_import::parse(swiftui_src).expect("swiftui parses");

    let slint_shape = shape_of(&slint_doc);
    let flutter_shape = shape_of(&flutter_doc);
    let swiftui_shape = shape_of(&swiftui_doc);

    // Root kind agrees.
    assert_eq!(slint_shape.root_kind, "Column");
    assert_eq!(flutter_shape.root_kind, "Column");
    assert_eq!(swiftui_shape.root_kind, "Column");

    // The sorted kind-set agrees: exactly one Column and one Button each.
    assert_eq!(slint_shape.kind_set, vec!["Button", "Column"]);

    // Full structural equality across all three.
    assert_eq!(
        slint_shape, flutter_shape,
        "slint and flutter lowered to different shapes"
    );
    assert_eq!(
        flutter_shape, swiftui_shape,
        "flutter and swiftui lowered to different shapes"
    );
}

#[test]
fn empty_column_lowers_identically_across_dialects() {
    // A degenerate case: an empty container. Shape must still converge.
    let slint_doc = slint_import::parse("Column { }").expect("slint parses");
    let flutter_doc = flutter_import::parse("Column(children: [])").expect("flutter parses");
    let swiftui_doc = swiftui_import::parse("VStack { }").expect("swiftui parses");

    let s = shape_of(&slint_doc);
    let f = shape_of(&flutter_doc);
    let w = shape_of(&swiftui_doc);

    assert_eq!(s.root_kind, "Column");
    assert_eq!(s, f);
    assert_eq!(f, w);
}
