# Uni-UI → SwiftUI Parity Program

> North star: SwiftUI is our **directional spine** — we mirror its shape (views +
> chainable modifiers + state + navigation + animation + gestures). Goal: bring
> Uni-UI to **functional equivalence** for the practical SwiftUI surface.
>
> This is a multi-milestone program (SwiftUI is a decade of Apple work). We land
> **one bounded, green, committed milestone at a time** and track honest coverage
> here. Legend: ✅ have · 🟡 partial · ⬜ missing.

## Coverage matrix (as of S0 baseline)
**Views:** Text ✅ · Button ✅ · Label ✅ · TextField ✅(text_input) · Image ✅ · Spacer ✅ · Divider ✅ · Toggle ✅ · Slider ✅ · ProgressView ✅ · Picker ✅ · Stepper ✅ · Menu ✅ · Link ⬜
**Containers:** VStack→Column ✅ · HStack→Row ✅ · ZStack→Stack ✅ · ScrollView ✅ · Group ✅ · List ✅(virtualized) · LazyV/HStack ✅ · Grid ✅(CSS grid) · Form/Section ✅ · GeometryReader ⬜
**Modifiers:** padding ✅ · background ✅ · foregroundColor ✅ · font 🟡 · frame ✅(w/h) · cornerRadius ✅ · shadow ✅ · opacity ✅ · hidden ✅ · clipShape ✅(import) · overlay ✅ · offset ✅ · scale ✅ · rotation ✅(rect; text axis-aligned in v0) · animation ✅
**State:** reactive store ✅ + bindings ✅ + Expr grammar ✅ · `State<T>`/`Binding<T>` handles ✅(@State/@Binding-style) · `@Environment` ⬜
**Navigation:** NavigationStack ✅ · TabView ✅ · Sheet/Alert/Popover/Menu ✅
**Animation:** spring core ✅ · timing/easing curves ✅ · implicit ✅ · explicit ✅ · transitions ✅ · matchedGeometry ⬜
**Gestures:** tap ✅ · longPress ✅ · drag ✅ · magnify ✅ · rotation ✅ · combined(simultaneous/sequenced) ✅
**Drawing/Text:** Shape ✅ · Path ✅(+ quad bezier) · Canvas ✅ · gradients ✅(linear/radial; angular partial) · dynamic type ✅ · localization ✅ · bidi 🟡

## Milestones
- **S1 — Essential views + modifier surface.** Image, Divider, Spacer, Toggle, Slider, ProgressView rendered; modifiers `opacity`/`hidden`/`shadow` honored in paint; matching `swiftui-import` coverage + differential tests; `uni-widgets` builders. ← ✅ **DONE** (258 tests / 0 fail, clippy+doc clean).
- **S2 — Containers + state ergonomics.** List virtualization, LazyVStack/HStack, real Grid, Form/Section; `@State`/`@Binding`-style API over the store; Picker/Stepper. ← ✅ **DONE** (283 tests / 0 fail, clippy+doc clean).
- **S3 — Navigation + presentation.** NavigationStack, TabView, Sheet/Alert/Popover/Menu, overlay/background-view modifiers. ← ✅ **DONE** (313 tests / 0 fail, clippy+doc clean).
- **S4 — Animation + transforms.** Implicit/explicit animation, transitions, offset/rotation/scale effects (on uni-spring). ← ✅ **DONE** (348 tests / 0 fail, clippy+doc clean; rotation full for rects, text axis-aligned in v0).
- **S5 — Gestures.** tap/longPress/drag/magnify/rotation, combined, gesture state. ← ✅ **DONE** (379 tests / 0 fail, clippy+doc clean; magnify/rotation driven programmatically — headless has no multitouch).
- **S6 — Drawing + text.** Path/Shape/Canvas/gradients; dynamic type, localization. ← ✅ **DONE** (430 tests / 0 fail, clippy+doc clean; angular gradient partial, bidi later).
- **S7 — Tooling + ergonomics.** Preview harness (`preview_png` headless render→PNG, hand-rolled encoder), IR/layout `inspect()`, `reload_from_uni` hot-reload, `docs/SWIFTUI-MAPPING.md`. ← ✅ **DONE** (442 tests / 0 fail, clippy+doc clean).

---

## ✅ Planned program S1→S7 COMPLETE (442 tests, all gates green)
The **common, practical SwiftUI surface** lowers, lays out, renders, animates, and is inspectable/previewable in Uni-UI. See `docs/SWIFTUI-MAPPING.md` for the full construct-by-construct reference.

**Honest residuals (NOT equivalence with Apple's depth):** angular gradient (partial), rotation-on-text (axis-aligned v0), bidi text, matchedGeometry, `@Environment`, GeometryReader, Link — all ⬜/partial. No app dogfooded on a real device yet; this is *functional coverage of the common surface*, not Apple-grade polish/ecosystem. That depth is the ongoing road.

_Each milestone ships green (cargo test + clippy + doc) and updates this matrix. No "equivalent" claim without the matrix backing it._
