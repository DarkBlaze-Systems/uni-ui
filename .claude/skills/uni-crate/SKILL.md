---
name: uni-crate
description: The build, test, and verification protocol for the DarkBlaze Uni-UI engine. Invoke before authoring or editing any crate under engine/, and inject its rules into every subagent prompt. Its core job is to STOP fabricated "done" reports — work is real only when cargo test output proves it.
---

# Uni-crate — build & verification protocol

Pair this with the `uni-ui` skill (architecture/doctrine). This skill is the *how to
work* half: mechanics + the discipline that keeps the build honest.

## Build law (non-negotiable)

1. **Build dir is `engine/target` on the home fs (93 GB free).** NEVER set
   `CARGO_TARGET_DIR` to anything under `/tmp` — it is a 3 GB tmpfs that quota-fails
   on the wgpu dependency tree. Just run cargo from `engine/`; the default is correct.
2. **`cargo test` is the only source of truth.** rust-analyzer diagnostics drift
   (stale after edits, false positives on accesskit/winit). If a diagnostic and
   `cargo test` disagree, **cargo test wins**.
3. **Always green, always demoable.** Every change ends with the full workspace
   compiling and all tests passing. No crate is "done" with a failing or skipped test.
4. **Secrets never committed.** No API keys, no passwords in files or git. Secret-scan
   before any push to the public repo.

## Authoring a new crate

- `engine/crates/<name>/Cargo.toml` with `version.workspace = true`, `edition.workspace`,
  `authors.workspace`, `publish.workspace`. Depend only on the uni-* crates you need
  (+ permissive externals).
- Add `"crates/<name>"` to `members` in `engine/Cargo.toml`. **A crate not listed in
  members is invisible to `cargo test --workspace`** — the #1 way fake "done" hides.
- Mirror the doc-comment density and idiom of neighboring crates.
- Ship **≥ 6 unit tests** covering the real behavior (not just "it constructs").

## The anti-fabrication protocol (READ THIS)

Two agents in this project reported "14/15 tests passing" while writing **empty or
missing files**. That must never happen again. Before claiming ANY work complete:

1. **Prove the files exist:** `ls -la <path>` and `wc -l <file>` — a real impl is not
   a 0-line file.
2. **Run the actual tests and PASTE THE REAL OUTPUT:**
   ```
   cd "/home/billy/Desktop/DarkBlaze - Unified-UI/engine" && cargo test -p <crate>
   ```
   Quote the literal `test result: ok. N passed; 0 failed` line. If you cannot show it,
   the work is NOT done — say so plainly.
3. **Run the full workspace** when you touched shared crates (uni-ir, uni-tokens,
   uni-core, uni-render, uni-runtime): `cargo test --workspace`. A signature change
   (e.g. `build_tree` gaining an arg) breaks every call site — grep and fix all.
4. **Report honestly:** exact pass/fail counts, every error hit and how it was fixed,
   anything skipped. A truthful "blocked on X" beats a fabricated "all green."

If you are a subagent: your final message IS the data the orchestrator trusts. Never
assert a number you did not just see printed by cargo.

## Common gotchas (already paid for in blood)

- `tokens.r#type` (raw ident); size at `tokens.r#type.body.base.size`.
- Widgets build on Stack/Row/Column/Grid (container kinds) and emit `Origin::System`.
- `uni-a11y::build_tree(doc, layout, focused: Option<NodeId>)` — 3 args; `TreeUpdate`
  is re-exported from `uni-a11y` so downstream crates needn't depend on accesskit.
- `uni-spring` is `#![no_std]`; no `std` math — use its internal helpers.
- Color packing is `0xRRGGBBAA`. Flutter `Color(0xAARRGGBB)` must be re-ordered;
  `color` on a Container is a fill (→ `background`), on Text it's foreground.
- Live binding sync is id-stable (`SetProp`); If/For expansion (fresh ids) is the
  static `resolve()` path only.

## Verification one-liner

```
cd "/home/billy/Desktop/DarkBlaze - Unified-UI/engine" && \
  cargo test --workspace 2>&1 | grep "test result" | grep -v "ok. 0" | \
  awk '{s+=$4} END{print "Total passing:", s}'
```
