# Uni-UI — Dynamic Phased Dev Plan

> Operational execution plan for the clean-room engine. Operationalizes `../research/engine/00-ENGINE-ARCHITECTURE.md`. Goal: take it past its ceiling, fast. Authorized full-autonomy multi-agent build, 2026-06-16.

## Rules (the "dynamic" part)
- **Always green, always demoable** — every phase ends in a runnable artifact.
- **Three parallel tracks** from Phase 1, fanned across agents: **A) Pixels** (render/core), **B) Language** (DSL→IR), **C) Doctrine** (tokens/react/cowork/Env). Converge at Phase 4.
- **Reuse the permissive stack; build only the 4 differentiated layers.** winit/wgpu/taffy/parley(cosmic-text)/accesskit/lyon — all MIT/Apache/BSD.
- **Clean-room discipline** — reconstruct from public docs; never read GPL `i-slint-*` or Apple-private code.
- **Secrets**: `darkai_api-approved.md` keys are NEVER committed/persisted; used only to back CLI helper-agents. `.gitignore` excludes target/ and env files.

## Status
- **Phase 0 — DONE ✅**: workspace + `uni-ir` (opinionated, AI-malleable, Origin-tagged IR + mutation stream). Green, tested.

## Phase 1 — First pixels  *(Track A leads; B+C parallel)*
Goal: make the IR visible. Deliverable: window drawing a styled rect + text from a `uni-ir` Document.
- Build `uni-render` (anyrender-style `Renderer` trait + `Scene`/`DrawCmd`) with a `winit`+`wgpu`+`lyon` backend; text via `cosmic-text`+`glyphon`.
- Parallel: `uni-tokens` (variant palette), `uni-react` (signals), `uni-dsl` grammar draft.
- Gate: renderer path (lyon+wgpu now, Vello later).

## Phase 2 — Living surface: reactive + retained core  *(Track A)*
Goal: interaction + layout + a11y. Deliverable: a hovering/pressable button that updates state and reflows, accessible.
- Build `uni-core` (retained tree: focus, hit-test, a11y pass first-class, pass order) + `uni-react` graph. Wire `taffy`.
- Reuse taffy, accesskit. Gate: signals lib (own vs reactive_graph).

## Phase 3 — Speak it into being: uni-dsl → IR  *(Track B converges)*
Goal: declarative creation. Deliverable: write a `.uni` file, watch it render.
- `uni-dsl` parser (logos+chumsky) → lowers to `uni-ir`. Constructs: components, props, one/two-way bindings, layouts, if/for, callbacks.
- Gate: canonical grammar locked (everything lowers TO this).

## Phase 4 — Differentiators: cowork + responsive  *(Track C converges — becomes OURS)*
Goal: dual-control + universal adaptation. Deliverable: AI drives+observes the same UI as the human; reflows phone↔desktop; design-law palette live.
- Cowork Contract end-to-end (host/AI-invokable actions + Origin audit → darkai-bridge); `global Env` (size-class/density/input-mode/build-variant); `uni-spring`; `uni-tokens` variant palette.
- This is the thesis-proving MVP.

## Phase 5 — Adaptive shells + Smart-Topbar  *(L)*
AdaptiveScaffold/Nav/panes across form factors; Smart-Topbar (Dynamic Island ⊕ Lattice 4-stage) — the AI companion seat.

## Phase 6 — Principled transpilation (the integration matrix)  *(L, ongoing)*
slint-import (our own parser) → flutter-import (BSD, tree-sitter-dart) → swiftui-import (pinned subset via swift-syntax). Foreign UIs re-expressed in OUR grammar.

## Phase 7 — Per-user synthesis + scale + Sanity License  *(ongoing)*
AI synthesizes/adapts per-user surfaces over the IR; embedded/web-wasm targets; BSL "Sanity License" + attribution/NOTICE + counsel/FTO review before distribution.

## Fastest path
Phase 1 + parallel B/C → Phase 3 → Phase 4 = the MVP (declarative, responsive, AI-cowork-able UI from our own language, under our own license). Tracks converge at Phase 4.

## Crate map
`uni-ir` (done) · `uni-tokens` · `uni-react` · `uni-render` · `uni-dsl` · `uni-core` (integration) · later: `uni-spring`, `slint-import`, `flutter-import`, `swiftui-import`.
