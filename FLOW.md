# The Flow — Uni-UI's architecture spine

Uni-UI is organized by one principle, borrowed from the DarkBlaze Flow-Kernel:

> **Own the spine. Isolate everything else as swappable backend leaves behind it,
> never above it. Whoever owns the flow owns the system — freedom and portability
> follow ownership of the *flow*, not of every line.**

## The spine (DarkBlaze-owned)

The **UI-Flow** is `uni-ir` (the data flow) plus a minimal set of capability
traits (the control flow). The upper stack — widgets, the `.uni` DSL, the cowork
layer, per-user synthesis — depends **only** on these, never on a concrete
backend type:

```
   upper stack: widgets · .uni DSL · cowork · per-user synthesis
                     │ depends ONLY on the Flow ↓
   THE UI-FLOW (owned):
     uni-ir  +  trait Renderer · Layout · TextShaper · Platform(Window+Input) · WebBackend · Signals
                     │ filled by swappable leaves ↓
     wgpu(→vello/software) · taffy(→own) · cosmic-text(→own) · winit(→canvas/android/KMS) · servo/OS-webview
```

**Rule:** our logic lives *above or in* the Flow; every backend (renderer,
layout, text, window, web engine) is an isolated leaf *behind* it. New behavior
goes in an owned file behind a Flow trait — never patched into a backend.

## What it buys

1. **Sovereign replaceability.** Own the spine → swap any backend (taffy, wgpu,
   cosmic-text) for our own implementation *at leisure*, with the upper stack
   unchanged. Full clean-room ownership without a big-bang rewrite.
2. **Multiplatform by backend, not by fork.** The same core compiles the shape
   that fits — desktop (`winit`+`wgpu`), web (canvas+WebGPU), embedded
   (KMS+software). Different `Platform`/`Renderer` leaf, one core.
3. **Foreign engines isolated as leaves.** A `WebBackend` (Servo / OS-webview)
   or borrowed-3D sits *behind* the Flow as a droppable leaf — it can never
   contaminate the core or the license.

## Discipline: tracer-bullet, not scaffold

We do **not** define abstract traits speculatively. Each Flow seam is *discovered*
by building a real second backend — exactly as the `Renderer` trait was
discovered by writing the wgpu backend. The next seams (the `Platform`/`Renderer`
web backend; the `WebBackend` leaf) are proven by doing the swap, not by drawing
the interface first.
