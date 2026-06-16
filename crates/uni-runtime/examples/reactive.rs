//! `cargo run -p uni-runtime --example reactive`
//!
//! The **rung-4 capstone**: the DSL's `$bindings` go *live* in the interactive
//! loop. State lives in a [`uni_reactor::Store`]; handlers mutate **state**, and
//! the runtime pushes that state into the bound props (id-stable) before each
//! repaint. The bound label therefore updates *via state*, never by a direct
//! prop write.
//!
//! - **`uni_widgets::button(...)`** builds the button subtree (proving the
//!   widget library integrates), carrying a `"click"` → `increment` callback.
//! - A `Text` whose `content` is **bound** (`bindings["content"] = "label"`)
//!   shows the counter. Nothing writes its `content` directly.
//! - **Click the button** (Human) → `fire(target, "click", Origin::Human)`
//!   (audited) → the `increment` handler does `store.set("label", ...)` →
//!   `sync_bindings` pushes it into the bound Text → repaint.
//! - **Press `A`** → `ai_fire` → the *exact same* path, `Origin::Ai`.
//! - Each action prints the audit log so Human vs AI invocations are visible.

use uni_ir::{Document, Mutation, NodeId, Origin, Value};
use uni_render::{InputEvent, PointerButton};
use uni_runtime::Runtime;
use uni_tokens::{Tokens, Variant};
use uni_widgets::button;

/// Assemble the UI: a root `Stack` containing a **bound** label `Text` and a
/// `uni-widgets` button. Returns `(doc, label_id)`.
fn build_ui() -> (Document, NodeId) {
    let tokens = Tokens::for_variant(Variant::Internal);
    let mut doc = Document::new();

    // Root container.
    let root = doc.fresh_id();
    doc.apply_from(Origin::System, Mutation::CreateNode { id: root, kind: "Stack".into() })
        .unwrap();
    doc.apply_from(Origin::System, Mutation::SetRoot { id: root }).unwrap();
    doc.apply_from(Origin::System, Mutation::SetProp { id: root, key: "padding".into(), value: Value::Px(24.0) }).unwrap();
    doc.apply_from(Origin::System, Mutation::SetProp { id: root, key: "gap".into(), value: Value::Px(16.0) }).unwrap();
    doc.apply_from(Origin::System, Mutation::SetProp { id: root, key: "background".into(), value: Value::Color(0x0a0a0aff) }).unwrap();

    // The bound label: a `Text` whose `content` is driven by the `"label"`
    // state key. The literal is just the first-frame fallback before the store
    // is seeded; once `sync_bindings` runs, the store value wins.
    let label = doc.fresh_id();
    doc.apply_from(Origin::System, Mutation::CreateNode { id: label, kind: "Text".into() }).unwrap();
    doc.apply_from(Origin::System, Mutation::SetProp { id: label, key: "content".into(), value: Value::Text("Clicks: 0".into()) }).unwrap();
    doc.apply_from(Origin::System, Mutation::SetProp { id: label, key: "size".into(), value: Value::Px(32.0) }).unwrap();
    doc.apply_from(Origin::System, Mutation::SetProp { id: label, key: "color".into(), value: Value::Color(0xffffffff) }).unwrap();
    doc.apply_from(
        Origin::System,
        Mutation::SetBinding {
            id: label,
            key: "content".into(),
            binding: uni_ir::Binding { expr: "label".into() },
        },
    )
    .unwrap();
    doc.apply_from(Origin::System, Mutation::AppendChild { parent: root, child: label }).unwrap();

    // The button: built by the widget library, firing `increment` on click.
    let btn = button(&mut doc, &tokens, "Click me", "increment");
    doc.apply_from(Origin::System, Mutation::AppendChild { parent: root, child: btn }).unwrap();

    (doc, label)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (doc, _label) = build_ui();
    let mut rt = Runtime::new(doc, (800.0, 600.0));

    // Seed initial state and register the handler. The handler mutates STATE
    // only — it never touches the doc directly.
    rt.store_mut().set("count", Value::Int(0));
    rt.store_mut().set("label", Value::Text("Clicks: 0".into()));
    rt.sync_bindings();

    rt.register(
        "increment",
        Box::new(move |store: &mut uni_reactor::Store, origin: Origin| {
            let n = match store.get("count") {
                Some(Value::Int(n)) => n + 1,
                _ => 1,
            };
            // The effect is a STATE change; the bound Text updates from this.
            store.set("count", Value::Int(n));
            store.set("label", Value::Text(format!("Clicks: {n}")));
            eprintln!("increment -> Clicks: {n}  (fired by {origin:?}, via state)");
        }),
    );

    // ----- headless demonstration so the proof is visible without a window ---
    // Find the button (second child of the root) to fire programmatically.
    let root = rt.doc().root().unwrap();
    let button_id = rt.doc().get(root).unwrap().children[1];
    let label_id = rt.doc().get(root).unwrap().children[0];

    let label_content = |rt: &Runtime| {
        rt.doc()
            .get(label_id)
            .unwrap()
            .props
            .get("content")
            .cloned()
    };

    eprintln!("=== Human path: clicking the widget button ===");
    // Drive the REAL input chain: a pointer-down at the button's center
    // hit-tests + bubbles to its "click" handler, fired as Origin::Human.
    let r = rt.layout().rect(button_id).expect("button laid out");
    let handled = rt.on_input(&InputEvent::PointerDown {
        x: r.x + r.w / 2.0,
        y: r.y + r.h / 2.0,
        button: PointerButton::Left,
    });
    assert!(handled, "the widget button should handle the click");
    eprintln!("bound label is now: {:?} (updated VIA STATE)", label_content(&rt));
    rt.print_audit_log();

    eprintln!("\n=== AI path: ai_fire on the same node ===");
    rt.ai_fire(button_id, "click");
    eprintln!("bound label is now: {:?} (updated VIA STATE)", label_content(&rt));
    rt.print_audit_log();

    // Confirm the bound label changed via state, not a direct prop write: there
    // is no SetProp on the label by a Human/Ai origin — only System pushes from
    // sync_bindings.
    let direct_human_or_ai_writes = rt
        .doc()
        .audit_log()
        .iter()
        .filter(|e| {
            matches!(&e.mutation, Mutation::SetProp { id, .. } if *id == label_id)
                && e.origin != Origin::System
        })
        .count();
    eprintln!(
        "\nDirect (non-System) prop writes to the bound label: {direct_human_or_ai_writes} \
         (expected 0 — it updates purely from state)"
    );
    assert_eq!(direct_human_or_ai_writes, 0);

    // If launched with a window-capable environment, hand off to the live loop.
    // (Comment out the early return to drive it interactively on a real window.)
    if std::env::var_os("UNI_RUN_WINDOW").is_some() {
        return rt.run();
    }
    Ok(())
}
