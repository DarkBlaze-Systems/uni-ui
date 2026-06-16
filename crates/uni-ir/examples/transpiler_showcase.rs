//! transpiler_showcase — three foreign UI languages, one IR.
//!
//! Run from the engine workspace root:
//!     CARGO_TARGET_DIR=./target cargo run -p uni-ir --example transpiler_showcase
//!
//! The DarkBlaze doctrine is *principled transpilation*: we do not embed
//! foreign UI runtimes, we lower them into our own opinionated vocabulary.
//! This example feeds the SAME little UI — a vertical stack holding a Text and
//! a Button — written three ways:
//!
//!   * a `.slint` snippet           (Slint DSL)
//!   * a Flutter/Dart widget tree   (Dart)
//!   * a SwiftUI view               (Swift)
//!
//! ...through `slint_import::parse`, `flutter_import::parse`, and
//! `swiftui_import::parse`. We then walk each resulting `uni_ir::Document` and
//! pretty-print it as an indented tree. The point you should SEE in the output:
//! a Slint `Column`, a Flutter `Column`, and a SwiftUI `VStack` all converge to
//! the identical normalized kind `Column`; `Text` stays `Text` with a `content`
//! prop; and every button becomes `Button`. Same tree, three front doors.

use std::fmt::Write as _;

use uni_ir::{Document, NodeId, Value};

/// Render a `Value` compactly for the tree view.
fn show_value(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::Text(s) => format!("{s:?}"),
        Value::Color(c) => format!("#{c:08X}"),
        Value::Px(p) => format!("{p}px"),
        Value::List(items) => {
            let inner: Vec<String> = items.iter().map(show_value).collect();
            format!("[{}]", inner.join(", "))
        }
    }
}

/// Walk one node and its subtree, appending an indented description.
fn walk(doc: &Document, id: NodeId, depth: usize, out: &mut String) {
    let Some(node) = doc.get(id) else { return };
    let indent = "  ".repeat(depth);

    // kind + the most telling props, in a stable order.
    let mut line = format!("{indent}{}", node.kind);
    let mut bits: Vec<String> = Vec::new();
    for key in ["content", "size", "color", "background", "corner_radius"] {
        if let Some(val) = node.props.get(key) {
            bits.push(format!("{key}={}", show_value(val)));
        }
    }
    // Any callbacks (e.g. a button's "click") are part of the contract too.
    for (event, action) in &node.callbacks {
        bits.push(format!("on:{event}->{}", action.name));
    }
    if !bits.is_empty() {
        let _ = write!(line, "  ({})", bits.join(", "));
    }
    out.push_str(&line);
    out.push('\n');

    for &child in &node.children {
        walk(doc, child, depth + 1, out);
    }
}

/// Pretty-print a whole document from its root.
fn tree(doc: &Document) -> String {
    let mut out = String::new();
    match doc.root() {
        Some(root) => walk(doc, root, 1, &mut out),
        None => out.push_str("  <empty document>\n"),
    }
    out
}

/// The normalized kind of a document's root — the convergence proof.
fn root_kind(doc: &Document) -> String {
    doc.root()
        .and_then(|r| doc.get(r))
        .map(|n| n.kind.clone())
        .unwrap_or_else(|| "<none>".into())
}

fn main() {
    // ── Source 1: Slint DSL ────────────────────────────────────────────────
    // A `Column` element wrapping a Text and a Button.
    let slint_src = r#"
        Column {
            Text { text: "Welcome to DarkBlaze"; font-size: 22px; }
            Button { text: "Continue"; }
        }
    "#;

    // ── Source 2: Flutter / Dart widget tree ───────────────────────────────
    // A `Column` whose children are a Text and an ElevatedButton.
    let flutter_src = r#"
        Column(children: [
            Text("Welcome to DarkBlaze", style: TextStyle(fontSize: 22.0)),
            ElevatedButton(onPressed: 'onContinue')
        ])
    "#;

    // ── Source 3: SwiftUI view ─────────────────────────────────────────────
    // A `VStack` (which normalizes to `Column`) with a Text and a Button.
    let swiftui_src = r#"
        VStack {
            Text("Welcome to DarkBlaze").font(.title2)
            Button("Continue") { onContinue() }
        }
    "#;

    let slint_doc = slint_import::parse(slint_src).expect("slint snippet should parse");
    let flutter_doc = flutter_import::parse(flutter_src).expect("flutter snippet should parse");
    let swiftui_doc = swiftui_import::parse(swiftui_src).expect("swiftui snippet should parse");

    println!("┌─ DarkBlaze Uni-UI · transpiler_showcase");
    println!("│  one opinionated IR, three foreign front doors");
    println!("└────────────────────────────────────────────────────\n");

    println!("[1] Slint DSL  →  uni-ir");
    print!("{}", tree(&slint_doc));
    println!();

    println!("[2] Flutter (Dart)  →  uni-ir");
    print!("{}", tree(&flutter_doc));
    println!();

    println!("[3] SwiftUI (Swift)  →  uni-ir");
    print!("{}", tree(&swiftui_doc));
    println!();

    // ── The convergence assertion, made visible ────────────────────────────
    let k_slint = root_kind(&slint_doc);
    let k_flutter = root_kind(&flutter_doc);
    let k_swiftui = root_kind(&swiftui_doc);

    println!("── convergence ──────────────────────────────────────");
    println!("  Slint   root kind : {k_slint}");
    println!("  Flutter root kind : {k_flutter}  (Dart `Column`)");
    println!("  SwiftUI root kind : {k_swiftui}  (Swift `VStack`)");

    let converged = k_slint == k_flutter && k_flutter == k_swiftui;
    if converged {
        println!("\n  ✓ all three lower to the SAME normalized kind: {k_slint:?}");
        println!("    VStack and Column are gone — there is only OUR vocabulary.");
    } else {
        println!("\n  ✗ kinds diverged — the IR is not normalizing as intended.");
    }

    // Demonstrate per-kind convergence across the whole tree, not just the root.
    let kinds = |doc: &Document| -> Vec<String> {
        let mut v = Vec::new();
        if let Some(root) = doc.root() {
            let mut stack = vec![root];
            while let Some(id) = stack.pop() {
                if let Some(n) = doc.get(id) {
                    v.push(n.kind.clone());
                    stack.extend(n.children.iter().copied());
                }
            }
        }
        v.sort();
        v
    };
    let ks = kinds(&slint_doc);
    let kf = kinds(&flutter_doc);
    let kw = kinds(&swiftui_doc);
    println!("\n  full kind-set (sorted):");
    println!("    Slint   : {ks:?}");
    println!("    Flutter : {kf:?}");
    println!("    SwiftUI : {kw:?}");
    println!(
        "    identical sets : {}",
        ks == kf && kf == kw
    );

    // Make the example self-checking: fail the run if convergence breaks.
    assert!(converged, "root kinds must converge");
    assert_eq!(ks, kf, "slint and flutter kind-sets must match");
    assert_eq!(kf, kw, "flutter and swiftui kind-sets must match");
}
