//! `cargo run -p uni-runtime --example counter`
//!
//! The **capstone demo**: an interactive counter that closes the accountability
//! circle into the live UI. Both the human and the AI drive the *same* audited
//! surface — neither has a back door.
//!
//! - **Click the button** → `doc.fire(button, "click", Origin::Human)` → the
//!   `"increment"` handler bumps the counter and rewrites the label's `content`
//!   via an `Origin::Human` `SetProp`. The UI repaints.
//! - **Press the `A` key** → `Runtime::ai_fire(button, "click")` → the *exact
//!   same* path, but `Origin::Ai`. The AI increments the very same counter.
//! - **Close the window** → the audit log prints, showing the interleaved
//!   `Human` and `Ai` `Invoke` records: the accountability circle, made visible.

use std::cell::RefCell;
use std::rc::Rc;

use uni_ir::{Document, Mutation, NodeId, Origin, Value};
use uni_runtime::Runtime;

/// The interactive UI, in our `.uni` DSL. The Button carries `on click:
/// increment();`, which `uni-dsl` lowers to a `SetCallback` → `Action { name:
/// "increment", .. }`.
const COUNTER_UNI: &str = r#"
    Stack { padding: 24px; gap: 16px; background: #0a0a0a;
      Text { content: "Clicks: 0"; size: 32px; color: #ffffff; }
      Button { width: 200px; height: 64px; color: #7d39eb; corner_radius: 16px;
               on click: increment();
               Text { content: "Click me"; size: 20px; color: #ffffff; } }
    }
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = Runtime::from_uni(COUNTER_UNI, (800.0, 600.0))?;

    // The label whose `content` the handler rewrites: first child of the root.
    let root = rt.doc().root().expect("root set by the parser");
    let label: NodeId = rt.doc().get(root).unwrap().children[0];

    // Shared counter state owned by the handler closure.
    let count = Rc::new(RefCell::new(0i64));

    rt.register(
        "increment",
        Box::new(move |doc: &mut Document| {
            *count.borrow_mut() += 1;
            let n = *count.borrow();
            // Attribute the SetProp to whoever just fired: the most recent
            // audited Invoke carries that Origin (Human or Ai). This keeps the
            // mutation's provenance honest — the AI's increments are tagged Ai,
            // the human's tagged Human, on the very same code path.
            let origin = doc
                .audit_log()
                .iter()
                .rev()
                .find(|e| matches!(e.mutation, Mutation::Invoke { .. }))
                .map(|e| e.origin)
                .unwrap_or(Origin::System);
            doc.apply_from(
                origin,
                Mutation::SetProp {
                    id: label,
                    key: "content".into(),
                    value: Value::Text(format!("Clicks: {n}")),
                },
            )
            .expect("label SetProp");
            eprintln!("increment -> Clicks: {n}  (fired by {origin:?})");
        }),
    );

    // Hand off to the windowed event loop. Clicking fires Human; pressing 'A'
    // fires Ai; closing the window prints the audit log.
    rt.run()
}
