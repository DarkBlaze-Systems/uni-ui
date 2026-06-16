# Uni-UI Roadmap — post-Wave-9 improvements (A1 → G2)

Tracked from the construction-review suggestions. Goal: all items to 100%, workspace green.
Sequence keystone first: **A1 → B1 → A2 → C1 → D2** (A1 unblocks the AGI-facing path).

**STATUS 2026-06-16: A1→G2 delivered. 227 tests pass / 0 fail; clippy `-D warnings`, fmt, and doc `-D warnings` all clean. D3 landed as its spec'd foundation (see note).**

## A — IR / spine durability
- [x] **A1** Serializable IR (`serde`) + JSON wire format + round-trip test — `uni-ir` `[L]`
- [x] **A2** Invertible mutations + `undo_last` on the audited stream — `uni-ir` `[M]`
- [x] **A3** `Document::verify()` invariants (no dangling parent/cycle/single-root) — `uni-ir` `[M]`
- [x] **A4** `diff(old,new) -> Vec<Mutation>` with `apply(old,diff)==new` test — `uni-ir` `[M]`

## B — Cowork / AI-malleability
- [x] **B1** `AuditSink` trait + `JsonlSink`, called from `apply_from`/`fire` — `uni-ir` `[M]`
- [x] **B2** AI-invokable action registry: `Runtime::actions()` + `invoke(.., Origin::Ai)` — `uni-runtime` `[S/M]`
- [x] **B3** Binding expression grammar (`Expr` AST: literal/key/binop/unop) — `uni-reactor` `[M]`
- [x] **B4** `CompositeSynthesizer` (ordered merge) + multi-rule test — `uni-synthesis` `[M]`

## C — Testing / CI / fuzz
- [x] **C1** CI: `fmt --check` + `clippy -D warnings` + `cargo-deny` (licenses/advisories) — `[S]`
- [x] **C2** `cargo-fuzz` targets: slint/flutter/swiftui/uni parsers never panic — `[M]`
- [x] **C3** Differential importer tests: equivalent inputs → structurally equal IR — `[M]`

## D — Performance
- [x] **D1** Route `uni-spring` batch integration through `uni-simd` + criterion bench — `[M]`
- [x] **D2** `TextMeasurer` trait; cosmic-text backend behind feature; feed layout — `uni-core` `[M]`
- [x] **D3** Incremental layout: dirty-`NodeId` set from mutations, skip clean subtrees — `uni-reactor`/`uni-runtime` `[L]` — _landed as dirty-set foundation + test (spec-accepted minimum); full clean-subtree skip is future work._

## E — Importer roadmap
- [x] **E1** `Unsupported{line,text}` telemetry on importers + coverage table — `[S/M]`

## F — Accessibility
- [x] **F1** `accesskit_winit` adapter pushing `build_tree` on commit — `uni-runtime` `[M]`
- [x] **F2** A11y invariant test: every Button-role node has name + action — `[S]`

## G — Docs / ecosystem
- [x] **G1** `docs/UNI-IR-SPEC.md` + `docs/UNI-LANG.md` — `[M]`
- [x] **G2** CI: `RUSTDOCFLAGS="-D warnings" cargo doc` + `cargo test --doc` — `[S]`
