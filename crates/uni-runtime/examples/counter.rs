//! `cargo run -p uni-runtime --example counter`
//!
//! The **capstone demo**: an interactive counter that closes the accountability
//! circle into the live UI. Both the human and the AI drive the *same* audited
//! surface — neither has a back door.
//!
//! - **Click the button** → `doc.fire(button, "click", Origin::Human)` (audited)
//!   → the `"increment"` handler bumps a counter **in the store** and sets the
//!   bound `"label"` state. `sync_bindings` then pushes it into the bound Text.
//! - **Press the `A` key** → `Runtime::ai_fire(button, "click")` → the *exact
//!   same* path, but `Origin::Ai`. The AI increments the very same counter.
//! - **Close the window** → the audit log prints, showing the interleaved
//!   `Human` and `Ai` `Invoke` records: the accountability circle, made visible.

use uni_ir::{Origin, Value};
use uni_reactor::Store;
use uni_runtime::Runtime;

/// The interactive UI, in our `.uni` DSL. The Button carries `on click:
/// increment();`, which `uni-dsl` lowers to a `SetCallback` → `Action { name:
/// "increment", .. }`. The label's `content` is **bound** to the `"label"`
/// state key (rung 4), so it updates from state rather than a direct write.
const COUNTER_UNI: &str = r#"
    Stack { padding: 24px; gap: 16px; background: #0a0a0a;
      Text { content: $label; size: 32px; color: #ffffff; }
      Button { width: 200px; height: 64px; color: #7d39eb; corner_radius: 16px;
               on click: increment();
               Text { content: "Click me"; size: 20px; color: #ffffff; } }
    }
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = Runtime::from_uni(COUNTER_UNI, (800.0, 600.0))?;

    // Seed state and push it into the bound label for the first frame.
    rt.store_mut().set("count", Value::Int(0));
    rt.store_mut().set("label", Value::Text("Clicks: 0".into()));
    rt.sync_bindings();

    rt.register(
        "increment",
        Box::new(move |store: &mut Store, origin: Origin| {
            // The handler mutates STATE only — the bound Text follows.
            let n = match store.get("count") {
                Some(Value::Int(n)) => n + 1,
                _ => 1,
            };
            store.set("count", Value::Int(n));
            store.set("label", Value::Text(format!("Clicks: {n}")));
            eprintln!("increment -> Clicks: {n}  (fired by {origin:?}, via state)");
        }),
    );

    // Hand off to the windowed event loop. Clicking fires Human; pressing 'A'
    // fires Ai; closing the window prints the audit log.
    rt.run()
}
