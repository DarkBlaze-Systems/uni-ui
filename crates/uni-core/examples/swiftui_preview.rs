//! `cargo run -p uni-core --example swiftui_preview`
//!
//! The headless SwiftUI `#Preview` path: take a small shapes+text document,
//! lower it through the **software canvas backend** (no GPU, no window), and
//! write a real PNG to the OS temp dir. This is what a devtool would render
//! into a preview pane.
//!
//! No windowing, no event loop — `uni_core::preview_png` does the whole chain:
//! `.uni` source → uni-ir Document → lower → CanvasRenderer → PNG bytes.

use std::io::Write;

use uni_core::preview_png;

/// A compact shapes + text scene. Mirrors a SwiftUI body you'd preview.
const SRC: &str = r#"
Stack {
    padding: 24px;
    gap: 16px;
    background: #0a0a0a;

    Text { content: "SwiftUI #Preview — headless"; size: 28px; color: #ffffff; }

    Row {
        gap: 16px;
        height: 140px;
        Circle  { width: 120px; height: 120px; color: #7d39eb; }
        Capsule { grow: 1; color: #1fb6c8; }
    }

    RoundedRectangle { height: 90px; corner_radius: 18px; color: #eb3970; }
}
"#;

fn main() {
    let doc = match uni_dsl::parse(SRC) {
        Ok(doc) => doc,
        Err(e) => {
            eprintln!("uni-dsl parse error: {e:?}");
            std::process::exit(1);
        }
    };

    let viewport = (480.0, 360.0);
    let png = preview_png(&doc, viewport);

    // Also show the inspector dump a devtool would render beside the preview.
    let layout = uni_core::layout(&doc, viewport);
    print!("{}", uni_core::inspect(&doc, &layout));

    let path = std::env::temp_dir().join("uni_swiftui_preview.png");
    match std::fs::File::create(&path).and_then(|mut f| f.write_all(&png)) {
        Ok(()) => println!(
            "wrote {} bytes of PNG to {}",
            png.len(),
            path.display()
        ),
        Err(e) => {
            eprintln!("failed to write {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}
