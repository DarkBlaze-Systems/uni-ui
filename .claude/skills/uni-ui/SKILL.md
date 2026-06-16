---
name: uni-ui
description: Load the architecture, doctrine, crate map, and IR model of the DarkBlaze Uni-UI engine. Invoke this before working anywhere under engine/ — it explains WHAT the engine is and HOW it is organized so you (or a subagent) can act correctly without re-deriving the design.
---

# Uni-UI — engine context

DarkBlaze **Uni-UI** is a clean-room, Rust-native, sovereign declarative UI engine.
It is **not** a Slint/Flutter/SwiftUI fork — it shares none of their source. Every
dependency is permissive (MIT/Apache/BSD). Repo: `github.com/DarkBlaze-Systems/uni-ui`
(public). License: BSL "Sanity License" (BSL 1.1 + no-compete Additional Use Grant).

## The north star (why)

Create **legible, hand-off-able order** across UI → OS → hardware, in a shape an AGI
can eventually steward, while serving the individual user. The engineering bite:
- **Every human action has a matching AI handle** on the *same audited path* (cowork
  dual-control). No privileged back door.
- **The IR is the ledger.** Every mutation is `Origin`-tagged (`Human`/`Ai`/`System`)
  and lands in an append-only audit log. Accounting/attribution is THE POINT, not a
  feature. "In numbers we trust."
- **Least-action design** = fiscal policy in the true currency (time × space):
  low-resource yet beautiful — event-driven redraw, runtime-detected SIMD, small
  runtime, cheap frosted glass.
- **Per-user synthesis**: we build the UI *for* the user; the AI adapts it (uni-synthesis).

## THE FLOW architecture

Own the trait spine; fuse the best permissive wheels beneath it, each isolated as a
swappable leaf. Reinvent only what's missing or encumbered. Replaceability is a
safety-net, not a mandate. See `engine/FLOW.md`.

## The IR model (uni-ir — the keystone)

```
NodeId(u64)
Value = Bool | Int | Float | Text | Color(u32 0xRRGGBBAA) | Px(f32) | List(Vec<Value>)
Node  = { kind: String, props: BTreeMap<String,Value>, children, parent,
          callbacks: BTreeMap<String,Action>, bindings: BTreeMap<String,Binding> }
Origin = Human | Ai | System
Mutation = CreateNode | SetRoot | SetProp | RemoveProp | AppendChild | RemoveChild
         | RemoveNode | SetCallback | SetBinding | Invoke(audit-only)
Document::fire(id, event, origin) -> Option<Action>   // audited
Document::apply_from(origin, mutation) -> Result<(), IrError>
Document::audit_log() -> &[Edit]
```

Everything lowers TO this one opinionated IR. Transpilers normalize foreign UIs
into it (principled transpilation, not mimicry).

## Crate map (layered; lower depends on nothing above it)

| Crate | Role |
|-------|------|
| `uni-ir` | keystone IR + Origin-tagged mutation/audit log |
| `uni-tokens` | design law: Palette (violet internal / lime public, **no emerald**), Space, Type (Role.base/.emphasized), Motion (two-spring), Shape, `ThemeMode` (Dark default / Light) |
| `uni-react` | fine-grained signals/memos/effects |
| `uni-reactor` | `Store` (signal-backed), `resolve()` static expand (If/For incl. `Value::List`), used by runtime's live `sync_bindings`; `snapshot()`/`restore()` persistence |
| `uni-render` | `Scene`/`DrawCmd`/`Renderer` trait; `WgpuRenderer` (Vulkan/LowPower, frosted glass); `CanvasRenderer` (software, wasm/test); winit input translation |
| `uni-core` | taffy layout, `paint`, `hit_test`; only Stack/Row/Column/Grid lay out children |
| `uni-dsl` | `.uni` parser → IR (`Kind { prop: value; on event: act(); if/for; key: $bind }`) |
| `uni-spring` | `#![no_std]` damped springs; spatial (ζ<1 overshoot) vs effects (ζ≥1 monotonic) |
| `uni-env` | `Env`: WidthClass (600/840), InputMode, BuildVariant, accent(), vw/vh |
| `uni-simd` | pulp runtime SIMD: srgb↔linear, premultiply, transform_points |
| `uni-a11y` | `build_tree(doc, layout, focused) -> accesskit::TreeUpdate` |
| `uni-webframe` | WebBackend trait + stub (heterogeneous surface seam) |
| `uni-widgets` | builders: button/label/checkbox/card/list, adaptive_scaffold/nav, list_detail_pane, scroll_view, text_input, tooltip |
| `uni-shells` | SmartTopbar (4-stage Silence/Notify/Morph/Chat spring morph) |
| `uni-synthesis` | `Synthesizer` trait + `BasicSynthesizer` (per-user adaptation via Origin::Ai) |
| `uni-runtime` | capstone: input→hit-test→`fire(Origin)`→handler(`&mut Store`,Origin)→sync_bindings→spring tick→a11y→repaint; `ai_fire` proves identical AI path; keyboard nav + focus |
| `slint-import` / `flutter-import` / `swiftui-import` | clean-room transpilers → uni-ir |

## Key invariants you must respect

- **Widgets build on CONTAINER kinds** (Stack/Row/Column/Grid) so children get rects.
  A "button" is a styled `Stack`, never a `Button` leaf.
- **Widgets emit `Origin::System`** (library chrome, not Human/Ai edits).
- **`tokens.r#type`** — `type` is a keyword; the field is `r#type`. Sizes live at
  `tokens.r#type.body.base.size` (Role → `.base`/`.emphasized` → TextStyle → `.size`).
- **Colors are `0xRRGGBBAA`** packed u32, drop straight into `Value::Color`.
- **Live binding sync is id-stable** (`SetProp` on existing ids); structural If/For
  expansion mints fresh ids and is the static `resolve()` path, not the live loop.

## Companion skill

For build/test mechanics and the **anti-fabrication verification protocol** every
agent must follow, see the `uni-crate` skill. Load it before editing or authoring crates.
