//! `cargo run -p uni-runtime --example audit_proof`
//!
//! Headless proof of the accountability circle: it drives the *same* `Runtime`
//! dispatch path the windowed `counter` demo uses — a simulated human pointer
//! click and an `ai_fire` — then prints the audit log. No window required, so
//! it shows the Human-vs-AI `Origin` trail in plain text (useful in CI / over
//! SSH where a GPU surface isn't available).

use std::cell::RefCell;
use std::rc::Rc;

use uni_ir::{Document, Mutation, NodeId, Origin, Value};
use uni_render::{InputEvent, PointerButton};
use uni_runtime::Runtime;

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
    let root = rt.doc().root().unwrap();
    let label: NodeId = rt.doc().get(root).unwrap().children[0];
    let button: NodeId = rt.doc().get(root).unwrap().children[1];

    let count = Rc::new(RefCell::new(0i64));
    rt.register(
        "increment",
        Box::new(move |doc: &mut Document| {
            *count.borrow_mut() += 1;
            let n = *count.borrow();
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
            .unwrap();
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
