# Uni-UI

**A clean-room, Rust-native declarative UI engine** — one engine for every surface, co-driven by human and AI.

> Status: **early but real.** As of the first public push, a declarative `.uni`
> source already renders end-to-end through an engine we own top to bottom.
> Built in the open, under our own (forthcoming) terms — see [`LICENSE`](./LICENSE).

## What we're building

A **low-resource yet beautiful, multiplatform, declarative UI engine in Rust** —
one codebase that runs from an embedded screen to a phone, tablet, desktop, OS
shell, server dashboard, and the web, and looks *considered* on every one.

- **Low-resource.** Rust, no garbage collector, a lean all-permissive stack, a
  small runtime. Built to run where heavier toolkits can't — down to embedded —
  without surrendering fidelity.
- **Beautiful.** A restrained design law: a sparse monochrome substrate, depth
  carried by **light (glow) and shadow rather than color**, accent used
  sparingly, and physics-based ("expressive") motion. Calm by default, alive
  when it matters.
- **Multiplatform — truly universal.** Desktop · phone · tablet · embedded · OS
  shell · server dashboards · web (wasm). One declarative source, adapted per
  surface *and* per user — universal without being uniform.
- **Declarative.** You *declare* what the UI is; the engine makes it so — and
  the AI can reshape that declaration live.

### The ideals beneath it

- **Order out of chaos** — a coherent, legible whole instead of fragmented sprawl.
- **Serve the user** — the interface is built *for* the person, per-user; we
  serve them, not the other way round.
- **AI / Human cowork** — every action a human can take, the AI can take and
  observe, every change audited.
- **Sovereign** — owned outright, clean-room, shipped under our own terms.

## What it is

Most UI toolkits speak one language and target one shape. Uni-UI is built to be
an **"LLVM-for-UI"**: many declarative-UI languages lowered into one opinionated
intermediate representation (IR), rendered to every device, **malleable by AI at
runtime**, and **synthesized per-user**.

Three invariants are baked into the architecture, not bolted on:

- **AI / Human cowork (dual-control).** The live UI is an *IR mutated by an
  Origin-tagged stream* — so a human gesture and an AI action drive the *same*
  surface through the *same* audited mechanism. Neither has a back door.
- **Principled, not mimicking.** Importers re-express foreign UIs (Slint /
  Flutter / SwiftUI) in *our* grammar and design law, rather than copying them.
- **Per-user.** The same IR is generated and adapted per person — universal does
  not mean uniform.

## Why clean-room + permissive

Uni-UI assembles a fully **permissive (MIT / Apache-2.0 / BSD)** foundation —
`winit`, `wgpu`, `lyon`, `cosmic-text`/`glyphon`, `taffy`, `accesskit` — and
builds only the differentiated layers on top. No GPL/copyleft source, none of
Slint's `i-slint-*` crates, no proprietary framework code. That's what lets the
engine ship under its own terms. See [`LICENSE`](./LICENSE).

## Crates

| Crate | Role |
|---|---|
| `uni-ir` | The opinionated, AI-malleable IR + Origin-tagged mutation stream (the spine). |
| `uni-tokens` | Design tokens — variant palette, spacing/type/motion/shape. |
| `uni-react` | Fine-grained reactive signal graph (signals / memos / effects). |
| `uni-dsl` | `.uni` lexer + parser (logos + chumsky) lowering to `uni-ir`. |
| `uni-render` | winit + wgpu + lyon + cosmic-text backend; GPU-free `Scene`/`Renderer`. |
| `uni-core` | Lowers a `uni-ir` Document → a `uni-render` Scene (layout + paint). |

## Try it

```sh
# the full chain: .uni source → uni-dsl → uni-ir → uni-core → uni-render
cargo run -p uni-core --example render_uni
```

```text
Stack {
    padding: 24px; background: #0a0a0a;
    Text { content: "Uni-UI"; size: 48px; color: #ffffff; }
    Rect { width: 240px; height: 96px; color: #7d39eb; corner_radius: 16px; }
}
```

Edit the `SRC` in the example and rerun — the window follows the words.

> Build note: build on a normal filesystem, not a small `tmpfs` — the GPU
> dependency tree is large.

## License

See [`LICENSE`](./LICENSE). Interim all-rights-reserved while the DarkBlaze
Sanity License (a BSL-1.1 variant) is finalized; a permissive open-source
conversion is part of that license by design.
