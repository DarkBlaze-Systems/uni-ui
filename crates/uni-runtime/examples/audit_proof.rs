//! `cargo run -p uni-runtime --example audit_proof`
//!
//! Headless proof of the accountability circle: it drives the *same* `Runtime`
//! dispatch path the windowed `counter` demo uses — a simulated human pointer
//! click and an `ai_fire` — then prints the audit log. No window required, so
//! it shows the Human-vs-AI `Origin` trail in plain text (useful in CI / over
//! SSH where a GPU surface isn't available).

use uni_reactor::Store;
use uni_ir::{NodeId, Origin, Value};
use uni_render::{InputEvent, PointerButton};
use uni_runtime::Runtime;

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
    let root = rt.doc().root().unwrap();
    let label: NodeId = rt.doc().get(root).unwrap().children[0];
    let button: NodeId = rt.doc().get(root).unwrap().children[1];

    rt.store_mut().set("count", Value::Int(0));
    rt.store_mut().set("label", Value::Text("Clicks: 0".into()));
    rt.sync_bindings();

    rt.register(
        "increment",
        Box::new(move |store: &mut Store, _origin: Origin| {
            let n = match store.get("count") {
                Some(Value::Int(n)) => n + 1,
                _ => 1,
            };
            store.set("count", Value::Int(n));
            store.set("label", Value::Text(format!("Clicks: {n}")));
        }),
    );

    // HUMAN: simulate a left pointer-down at the center of the button's rect —
    // the same input path the windowed loop feeds from real winit events.
    let r = rt.layout().rect(button).expect("button laid out");
    let center = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    println!("HUMAN clicks button at {center:?}");
    rt.on_input(&InputEvent::PointerDown {
        x: center.0,
        y: center.1,
        button: PointerButton::Left,
    });

    // AI: fire the very same callback on the very same node — identical path.
    println!("AI fires the same button (ai_fire)");
    rt.ai_fire(button, "click");

    // The label now reads "Clicks: 2"; the log proves who did what.
    println!(
        "label content -> {:?}",
        rt.doc().get(label).unwrap().props.get("content")
    );
    rt.print_audit_log();
    Ok(())
}
