//! `cargo run -p uni-synthesis --example synthesis_demo`
//!
//! **"We serve them."** This is the per-user AI synthesis story end to end,
//! headless and console-only.
//!
//! 1. We hand-author a tiny UI [`Document`] (a `Stack` with a `Text` headline
//!    and a small `Button`) entirely via `Origin::System` mutations.
//! 2. We describe *one real person* with an accessibility need through a
//!    [`UserProfile`]: large text (1.5x), high contrast, and roomier spacing
//!    (density 1.2). We pick a phone-sized [`Env`] so touch hit-targets matter.
//! 3. [`BasicSynthesizer`] reads the doc + env + profile and proposes
//!    mutations. We `apply` them through the `Origin::Ai` path.
//! 4. We print BEFORE → AFTER for every affected prop, then walk the
//!    `audit_log()` to prove each adaptation is recorded and attributed to
//!    `Origin::Ai`. The UI was never edited in the dark — the provenance is
//!    legible.

use uni_env::Env;
use uni_ir::{Document, Mutation, NodeId, Origin, Value};
use uni_synthesis::{apply, BasicSynthesizer, Synthesizer, UserProfile};

/// Build the starting UI. Returns the doc plus the ids we care about so we can
/// report their before/after state precisely.
fn build_ui() -> (Document, NodeId, NodeId, NodeId) {
    let mut doc = Document::new();

    // Root: a vertical Stack with a modest 8px gap (touched by density).
    let root = doc.fresh_id();
    doc.apply_from(Origin::System, Mutation::CreateNode { id: root, kind: "Stack".into() }).unwrap();
    doc.apply_from(Origin::System, Mutation::SetRoot { id: root }).unwrap();
    doc.apply_from(Origin::System, Mutation::SetProp { id: root, key: "gap".into(), value: Value::Px(8.0) }).unwrap();

    // Headline Text: 16px, mid-grey (touched by text_scale + high_contrast).
    let headline = doc.fresh_id();
    doc.apply_from(Origin::System, Mutation::CreateNode { id: headline, kind: "Text".into() }).unwrap();
    doc.apply_from(Origin::System, Mutation::SetProp { id: headline, key: "content".into(), value: Value::Text("Welcome back".into()) }).unwrap();
    doc.apply_from(Origin::System, Mutation::SetProp { id: headline, key: "size".into(), value: Value::Px(16.0) }).unwrap();
    doc.apply_from(Origin::System, Mutation::SetProp { id: headline, key: "color".into(), value: Value::Color(0x9A9A_9AFF) }).unwrap();
    doc.apply_from(Origin::System, Mutation::AppendChild { parent: root, child: headline }).unwrap();

    // A small clickable Button: 36x36 (below the 48px touch target floor).
    let button = doc.fresh_id();
    doc.apply_from(Origin::System, Mutation::CreateNode { id: button, kind: "Button".into() }).unwrap();
    doc.apply_from(Origin::System, Mutation::SetProp { id: button, key: "width".into(), value: Value::Px(36.0) }).unwrap();
    doc.apply_from(Origin::System, Mutation::SetProp { id: button, key: "height".into(), value: Value::Px(36.0) }).unwrap();
    doc.apply_from(
        Origin::System,
        Mutation::SetCallback {
            id: button,
            event: "click".into(),
            action: uni_ir::Action { name: "open".into(), args: vec![] },
        },
    )
    .unwrap();
    doc.apply_from(Origin::System, Mutation::AppendChild { parent: root, child: button }).unwrap();

    (doc, root, headline, button)
}

/// Pretty-print the props we track on the three nodes.
fn snapshot(doc: &Document, root: NodeId, headline: NodeId, button: NodeId) {
    let r = doc.get(root).unwrap();
    let h = doc.get(headline).unwrap();
    let b = doc.get(button).unwrap();
    println!("    root.gap       = {:?}", r.props.get("gap").unwrap());
    println!("    headline.size  = {:?}", h.props.get("size").unwrap());
    println!("    headline.color = {:?}", h.props.get("color").unwrap());
    println!("    button.width   = {:?}", b.props.get("width").unwrap());
    println!("    button.height  = {:?}", b.props.get("height").unwrap());
}

fn main() {
    println!("== uni-synthesis :: synthesis_demo ==");
    println!("\"We serve them.\" — adapting one UI to one person, with provenance.\n");

    let (mut doc, root, headline, button) = build_ui();
    println!("Document built: {} nodes, {} audited edits so far.\n", doc.len(), doc.audit_log().len());

    // A real person: large text, high contrast, roomier spacing. Phone-sized
    // touch surface so hit-targets are in play.
    let profile = UserProfile {
        text_scale: 1.5,
        motion: 1.0,
        high_contrast: true,
        density: 1.2,
        dark_mode: true,
    };
    let env = Env::for_window(390.0, 844.0);
    println!("User profile: text_scale={}, high_contrast={}, density={}", profile.text_scale, profile.high_contrast, profile.density);
    println!("Env: {}x{}, width_class={:?}, touch={}\n", env.win_w, env.win_h, env.width_class(), env.is_touch());

    println!("-- BEFORE synthesis --");
    snapshot(&doc, root, headline, button);

    // Run the synthesizer, then apply via Origin::Ai.
    let log_before = doc.audit_log().len();
    let result = BasicSynthesizer.synthesize(&doc, &env, &profile);
    let proposed = result.mutations.len();
    let applied = apply(&mut doc, result);
    println!("\nSynthesizer proposed {proposed} mutation(s); {applied} applied via Origin::Ai.\n");

    println!("-- AFTER synthesis --");
    snapshot(&doc, root, headline, button);

    // Provenance: every new edit must be attributable to the AI.
    println!("\n-- Audit trail (new edits since synthesis) --");
    let new_edits = &doc.audit_log()[log_before..];
    for (i, edit) in new_edits.iter().enumerate() {
        let detail = match &edit.mutation {
            Mutation::SetProp { id, key, value } => {
                format!("SetProp node #{} {key} = {value:?}", id.0)
            }
            other => format!("{other:?}"),
        };
        println!("  [{i}] {:?} :: {detail}", edit.origin);
    }

    let all_ai = new_edits.iter().all(|e| e.origin == Origin::Ai);
    println!(
        "\nAll {} synthesis edits carry Origin::Ai: {}",
        new_edits.len(),
        if all_ai { "yes" } else { "NO" }
    );
    println!("\nThe adaptations are real, applied, and attributable. We served them, on the record.");
}
