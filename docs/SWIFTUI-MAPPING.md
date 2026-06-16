<!--
  SWIFTUI-MAPPING.md — the comprehensive SwiftUI → Uni-UI reference table.
  Derived directly from crates/swiftui-import/src/lib.rs (the importer that
  lowers SwiftUI into uni_ir) and SWIFTUI-PARITY.md (the coverage matrix).
  Where this prose and the source disagree, the source wins.
-->

# SwiftUI → Uni-UI Mapping Reference

This is the construct-by-construct reference for the `swiftui-import` front door:
what each SwiftUI view / container / modifier lowers to in the Uni-UI IR
(`uni_ir::Document` — nodes with a `kind`, a `props` map of `Value`s, `callbacks`,
and `children`), and how complete that lowering is today.

It tracks `crates/swiftui-import/src/lib.rs` exactly. The companion coverage
matrix lives in `SWIFTUI-PARITY.md`; this file is the detailed per-construct view
behind that matrix.

## How the importer works (read this first)

`swiftui_import::parse(src) -> Document` lowers a subset of the public SwiftUI
DSL. The richer `parse_with_report(src) -> ImportReport` returns the same document
plus two things the bare `parse` discards:

- `unsupported: Vec<Unsupported>` — every modifier the importer *recognized but
  deliberately dropped* (e.g. `.shadow`, `.accessibilityLabel`), each with its
  source line. Drops are never silent in the report path.
- `state_vars: Vec<String>` — the names of every `@State var`/`@State let`
  declared at top level, in source order.

Lowering is **opinionated and normalizing**: SwiftUI names are mapped onto our
own vocabulary (`VStack` → `Column`, etc.), several SwiftUI spellings collapse to
one IR prop (`Image(name:)` and `Image(systemName:)` both → `content`), and a
modifier with no IR home is dropped-and-reported rather than faked.

### Status legend

- **full** — the construct lowers to a stable IR shape with no known loss for the
  common case.
- **partial** — it lowers, but with a documented gap (axis-aligned only, name
  captured but not resolved, one variant unhandled, etc.).
- **planned** — recognized in the parity program but not lowered today; today it
  is dropped-and-reported (or simply unrecognized).

### IR `Value` types referenced below

From `uni_ir`: `Text(String)`, `Bool`, `Int`, `Float`, `Color(u32)` (RGBA-packed),
`Px(f32)`, `List(Vec<Value>)`. A `$binding` cannot be resolved in a clean-room
importer, so it is carried as the **bound name** in a `Value::Text` (e.g.
`isOn: $wifiEnabled` → `props["isOn"] = Text("wifiEnabled")`).

---

## 1. Views (leaf + control)

| SwiftUI | IR kind | Key props / behavior | Status |
|---|---|---|---|
| `Text("…")` | `Text` | positional string → `content`; also flagged `localizable=true` + `l10n_key="…"` (treated as a `LocalizedStringKey`) | full |
| `Button("…") { }` | `Button` | label → `content`; trailing closure → `"click"` callback | full |
| `Label("…", systemImage:)` | `Label` | label → `content` | full |
| `TextField(…)` | `TextField` | builder via `uni-widgets::text_input`; bound text carried by name | partial (text binding by name) |
| `Image("name")` / `Image(systemName:)` | `Image` | both positional `name` and `systemName:` normalize to one `content` prop | full |
| `Spacer()` | `Rect` | lowers to a `Rect` (a flexible box stand-in) | partial (no `grow` flag emitted) |
| `Divider()` | `Divider` | leaf rule | full |
| `Toggle("…", isOn: $x)` | `Toggle` | label → `content`; `isOn:` bound name → `isOn` prop | full |
| `Slider(value: $v, in: 0...100)` | `Slider` | `value:` bound name; `in:` range → `range_min` / `range_max` floats | full |
| `ProgressView(value: $v)` | `ProgressView` | `value:` bound name → prop | full |
| `Picker(…, selection: $s) { }` | `Picker` | `selection:` bound name; option views become children | full |
| `Stepper(…, value: $v)` | `Stepper` | label → `content`; `value:` bound name | full |
| `Menu("…") { }` | `Menu` | label → `content`; closure entries become children | full |
| `Link(…)` | — | not lowered; recognized as missing in the parity matrix | planned |

Drawing-shape "views" (`Rectangle`, `Circle`, …) are in §6.

---

## 2. Containers / layout

| SwiftUI | IR kind | Notes | Status |
|---|---|---|---|
| `VStack { }` | `Column` | vertical stack | full |
| `HStack { }` | `Row` | horizontal stack | full |
| `ZStack { }` | `Stack` | depth/overlay stack | full |
| `Group { }` | `Stack` | shares `Stack` (transparent grouping) | partial (no distinct grouping marker) |
| `ScrollView { }` | `Stack` | maps to `Stack`; scroll affordance via `uni-widgets::scroll_view` | partial (kind does not itself encode "scrollable") |
| `List` / `List(data)` | `List` | positional collection ident → `data` prop; virtualized in `uni-widgets` | full |
| `LazyVStack { }` | `List` | plus `lazy=true` marker prop | full |
| `LazyHStack { }` | `Row` | plus `lazy=true` marker prop | full |
| `Grid { }` | `Grid` | CSS-grid lowering | full |
| `GridRow { }` | `Grid` | shares the `Grid` kind; tagged `grid_row=true` to distinguish a row from its container | full |
| `Form { }` | `Form` | grouped settings container | full |
| `Section(header:)` / `Section("…")` | `Section` | a `Section`'s positional string is its **header** (`header` prop), not `content` | full |
| `GeometryReader { }` | — | not lowered (no layout-feedback model yet) | planned |

---

## 3. Modifiers

Modifiers lower to **props on the node they are chained onto** (a few become
child *layers* — see §4). Numeric props for `size`/`width`/`height`/`padding`/
`corner_radius`/`opacity`/`offset_x`/`offset_y` are stored as `Value::Px`; other
floats stay `Value::Float`.

| SwiftUI modifier | IR effect | Status |
|---|---|---|
| `.padding(n)` | `padding` (Px) | full |
| `.background(Color)` / `.background(.red)` | `background` prop (Color) | full |
| `.background(View)` / `.background { View }` | **underlay layer** child of kind `Underlay` (see §4) | full |
| `.foregroundColor(c)` / `.foregroundStyle(c)` | `color` prop | full |
| `.fill(Color)` (on a shape) | `background` prop (Color) | full |
| `.font(.title)` etc. | resolves the text role to a px size → `size` (Px) | partial (role→px table; custom `.system(size:)` falls through) |
| `.fontWeight(.bold)` / `.bold()` | `weight` prop (`"bold"`, …) | full |
| `.italic()` | `italic=true` | full |
| `.frame(width:height:)` | `width` / `height` props | full |
| `.frame(minWidth:/minHeight:)` | min folds onto `width` / `height` | partial (min treated as the dimension) |
| `.frame(maxWidth:.infinity)` | `grow=1.0` (fill cross axis); a *finite* max folds onto the dimension | partial (only `.infinity` → grow) |
| `.cornerRadius(r)` | `corner_radius` (Px) | full |
| `.shadow(…)` | **dropped + reported** (`"modifier .shadow"`) — no IR home yet | planned |
| `.opacity(o)` | `opacity` (Px-typed float) | full |
| `.hidden()` / `.isHidden` | `hidden=true` | full |
| `.clipShape(RoundedRectangle(cornerRadius:))` | reaches in for `corner_radius`; other clip shapes carry no radius | partial (only RoundedRectangle radius surfaced) |
| `.overlay { View }` / `.overlay(View)` | **overlay layer** child of kind `Overlay` (see §4) | full |
| `.offset(x:y:)` / `.offset(CGSize)` / `.offset(10,20)` | `offset_x` / `offset_y` (Px) | full |
| `.scaleEffect(s)` | `scale` prop (uniform factor) | partial (uniform only; per-axis `CGSize` scale not split) |
| `.rotationEffect(.degrees/.radians)` / `Angle(…)` | `rotation` prop in degrees (radians converted) | partial (rect-accurate; text is axis-aligned in v0) |
| `.animation(.curve(duration:), value:)` | `animation` descriptor prop `"<curve>:<dur>"` (e.g. `"easeInOut:0.3"`); `value:` ignored | full |
| `.transition(.opacity/.slide/.scale)` | `transition` prop = named transition | full |
| `.dynamicTypeSize(.large)` | `type_scale` prop = size name | full |
| `.dynamicTypeSize(.xSmall ... .accessibility5)` | `type_scale_min` / `type_scale_max` props | full |
| `.tabItem { Label(…) }` | `tab_item=true` + `tab_label="…"` on the tab page (see §4) | partial (first label string only) |
| any other `.modifier(…)` | **dropped + reported** as `"modifier .<name>"` with its line | n/a (by design) |

---

## 4. Navigation & presentation

Presentation/layering modifiers are special: their trailing `{ content }` closure
becomes a **synthetic child layer** rather than props. A bound presentation state
(`isPresented: $x`) is carried as `bound_to="x"`, and a positional title as
`title`.

| SwiftUI | IR shape | Status |
|---|---|---|
| `NavigationStack { }` / `NavigationView { }` | kind `NavigationStack` (both spellings collapse) | full |
| `NavigationLink("…") { dest }` / `NavigationLink(value:)` | kind `NavigationLink`; destination subtree as children | full |
| `.navigationDestination(isPresented:/for:) { dest }` | child layer kind `NavigationDestination`, `bound_to=…` | full |
| `TabView { … }` | kind `TabView`; pages tagged via `.tabItem` metadata | full |
| `.tabItem { Label(…) }` | marks the page `tab_item=true` + `tab_label` | partial |
| `.sheet(isPresented: $x) { }` | child layer kind `Sheet`, `bound_to="x"` | full |
| `.fullScreenCover(isPresented:) { }` | child layer kind `FullScreenCover` | full |
| `.popover(isPresented: $x) { }` | child layer kind `Popover`, `bound_to="x"` | full |
| `.alert(title, isPresented: $x) { actions }` | child layer kind `Alert`, `title` + `bound_to`; action buttons + optional message closure folded into the layer's children | full |
| `.confirmationDialog(…) { }` | child layer kind `ConfirmationDialog` | full |
| `Menu("…") { }` | kind `Menu` (see §1) | full |

---

## 5. State & bindings

| SwiftUI | Uni-UI handling | Status |
|---|---|---|
| `@State var name = …` | recorded in `ImportReport::state_vars` (name only); the whole declaration line is consumed so an initializer is not mis-parsed as a view | partial (declaration tracked; value graph not resolved) |
| `@Binding var name` (and other `@Attr var name`) | read with the same `@Attr var name` shape; only `@State` names are collected today | partial |
| `$binding` at a call site (e.g. `isOn: $flag`) | resolved to the **bound name** as `Value::Text("flag")` on the relevant prop (`isOn`, `selection`, `value`, …) | partial (name carried, not reconnected) |
| `State<T>` / `Binding<T>` handle API | provided over the reactive store in `uni-widgets` / store layer (`@State`/`@Binding`-style) | full (per parity matrix) |
| `@Environment(\.…)` | not lowered | planned |

The reactive store, literal/binding split, and `Expr` grammar live in the IR /
store layer, not the importer — see `UNI-IR-SPEC.md`. The importer's job is only
to surface which names are state and where `$` projections point.

---

## 6. Drawing & shapes

Shape primitives carry a `shape` prop so a renderer can pick the right drawing
path regardless of the (sometimes shared) IR `kind`.

| SwiftUI | IR kind | `shape` prop | Status |
|---|---|---|---|
| `Rectangle()` | `Rect` | `rect` | full |
| `RoundedRectangle(cornerRadius: r)` | `Rect` | `rounded_rect` (+ `corner_radius`) | full |
| `Circle()` | `Circle` | `circle` | full |
| `Ellipse()` | `Ellipse` | `ellipse` | full |
| `Capsule()` | `Capsule` | `capsule` | full |
| `Path { p in … }` | `Path` | — | partial (ops captured as a string descriptor) |
| `Path(arg)` | `Path` | — | partial (`path_source="arg"`; ops not enumerable) |
| `Canvas { ctx, size in … }` | `Canvas` | — | partial (node created; draw closure skipped) |

### Path op capture

A `Path { }` closure's builder calls are captured into two props:
`path_op_count` (Float) and `path_ops` (a `;`-separated string, each entry
`op` or `op:x,y`). Recognized ops: `move`, `addLine`, `addQuadCurve`, `addCurve`,
`closeSubpath`, `addRect`, `addEllipse`, `addArc`. Points are captured for
`move`/`addLine`/`addQuadCurve`/`addCurve` when they parse; cubic `addCurve`
control points beyond the endpoint are not individually surfaced.

### Gradients

`LinearGradient` / `RadialGradient` / `AngularGradient` used in `.fill(…)`,
`.background(…)`, or `.foregroundStyle(…)` lower to a **family** of props keyed off
the base prop name `<p>` they fill:

- `<p>` = `Value::Text("gradient:<kind>")` (kind = `linear`/`radial`/`angular`)
- `<p>_stops` = `Value::List` of `Color` stops, in source order
- `<p>_start` / `<p>_end` = named direction anchors (`top`, `bottom`, `center`, …)

`.fill(Color)` on a shape stays a plain `background` color (not a gradient).

| Gradient | Status |
|---|---|
| `LinearGradient(colors:startPoint:endPoint:)` / `(gradient:…)` | full |
| `RadialGradient(colors:center:startRadius:endRadius:)` | partial (center captured; start/end radii dropped) |
| `AngularGradient(…)` | partial (stops/center captured; angle has no scalar home yet) |

A gradient stop that cannot be resolved to a concrete color (a named asset, etc.)
is skipped rather than guessed.

---

## 7. Animation

| SwiftUI | Uni-UI handling | Status |
|---|---|---|
| `.animation(.easeInOut(duration:), value:)` | `animation` prop `"easeInOut:0.3"` | full |
| `.easeInOut` / `.linear` / `.easeIn` / `.easeOut` curves | curve name preserved in the descriptor; engine timing curves in `uni-spring` | full |
| `.spring()` | curve name `spring` in the descriptor; spring core in `uni-spring` | full |
| `.transition(.opacity/.slide/.scale)` | `transition` prop = name | full |
| `withAnimation(.curve) { … }` | recognized and **consumed** (carries only imperative state mutation, no view content) | full |
| `.matchedGeometryEffect(…)` | not lowered | planned |

Implicit/explicit animation, transitions, and the spring/timing core land in the
`uni-spring` crate per the S4 milestone; the importer's role is to capture the
declarative descriptor.

---

## 8. Gestures

Gesture modifiers lower to **callbacks** (and sometimes props) on the node, not
to view content. The trailing action closure is recognized and skipped.

| SwiftUI | Uni-UI callback / prop | Status |
|---|---|---|
| `.onTapGesture { }` | `"click"` callback | full |
| `.onTapGesture(count: 2) { }` | `tap_count` prop + `"click"` | full |
| `.onLongPressGesture { }` | `"longpress"` callback (optional `minimumDuration:` skipped) | full |
| `.gesture(DragGesture().onChanged{}.onEnded{})` | `"drag_changed"` / `"drag_ended"` — only the phases present (both if none named) | full |
| `.gesture(MagnificationGesture())` / `MagnifyGesture` | `"magnify"` callback | partial (driven programmatically; headless has no multitouch) |
| `.gesture(RotationGesture())` / `RotateGesture` | `"rotate"` callback | partial (driven programmatically) |
| `.gesture(TapGesture())` / `.gesture(LongPressGesture())` | `"click"` / `"longpress"` | full |
| `.simultaneousGesture(…)` | as above + `gesture_priority="simultaneous"` | full |
| `.highPriorityGesture(…)` | as above + `gesture_priority="high"` | full |
| unknown recognizer inside `.gesture(…)` | recorded as an `Unsupported` drop `"gesture <Name>"` | n/a (by design) |

---

## 9. Text & localization

| SwiftUI | Uni-UI handling | Status |
|---|---|---|
| `Text("key")` | treated as a `LocalizedStringKey`: `localizable=true` + `l10n_key="key"` alongside `content` | full |
| `.dynamicTypeSize(.large)` | `type_scale` prop | full |
| `.dynamicTypeSize(.xSmall ... .accessibility5)` | `type_scale_min` / `type_scale_max` props | full |
| `.font(role)` text styles | role → px `size` (see §3) | partial (role table only) |
| Right-to-left / bidirectional text | not modeled | partial (bidi later) |
| `.rotationEffect` on `Text` | `rotation` prop is set, but text rendering is **axis-aligned in v0** | partial |

---

## 10. Honest gaps (single index)

The constructs the parity program tracks as not-yet-full, gathered in one place:

- **planned (not lowered today):** `Link`, `GeometryReader`, `@Environment`,
  `.matchedGeometryEffect`, `.shadow` (dropped + reported).
- **partial (lowers with a documented gap):** angular gradient (angle has no
  scalar home), radial-gradient radii, `.rotationEffect` on text (axis-aligned in
  v0), bidirectional text, `Group`/`ScrollView` collapsing onto shared kinds,
  `Spacer` (no `grow` flag), `.scaleEffect` (uniform only), `@State`/`@Binding`
  name-tracking without value-graph resolution, `.font` custom sizes, `Path`/
  `Canvas` (descriptor/skip rather than full geometry), magnify/rotation gestures
  (programmatic in headless tests).

Every other row above is **full** for the common case. No "equivalent" claim is
made here that the importer source and `SWIFTUI-PARITY.md` do not back.
