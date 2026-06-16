//! The renderer-agnostic scene description.
//!
//! A [`Scene`] is just an ordered list of [`DrawCmd`]s. Commands paint in
//! order (painter's algorithm): later commands draw on top of earlier ones.
//! This is the lowering target for `uni-core`: a `uni-ir` `Document` becomes a
//! `Vec<DrawCmd>` and that's all a [`crate::Renderer`] ever sees.

/// The geometric outline of a [`DrawCmd::FilledShape`].
///
/// Every variant is sized by the command's `(x, y, w, h)` *frame* (the SwiftUI
/// model: a shape draws to fill the frame it's given). This mirrors SwiftUI's
/// shape set so a transpiled `Circle()`/`Capsule()`/`RoundedRectangle(...)`
/// lowers to a single filled primitive that honors its fill and frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    /// A sharp-cornered rectangle filling the frame.
    Rect,
    /// A rectangle with uniform corner rounding (`RoundedRectangle`). `radius`
    /// is clamped to half the shorter side by the backend.
    RoundedRect { radius: f32 },
    /// A circle inscribed in the frame — diameter = the shorter side, centered.
    Circle,
    /// An ellipse filling the frame (independent x/y radii).
    Ellipse,
    /// A capsule (stadium): a rounded rectangle whose corner radius is half the
    /// shorter side, so the short ends are semicircles.
    Capsule,
}

/// A fill style for a [`DrawCmd::FilledShape`] / [`DrawCmd::Path`].
///
/// `Solid` is the packed-`u32` color path (identical to the legacy
/// [`DrawCmd::FilledRect`] color). The gradient variants describe SwiftUI's
/// `LinearGradient` / `RadialGradient` / `AngularGradient`; the wgpu backend
/// honors them by interpolating vertex colors (exact for linear; radial/angular
/// are approximated per-vertex on the tessellated mesh).
#[derive(Clone, Debug, PartialEq)]
pub enum Fill {
    /// A single packed `0xRRGGBBAA` color.
    Solid(u32),
    /// A linear gradient between `start` and `end`, given as **unit** points in
    /// the shape's frame (`(0,0)` = top-left, `(1,1)` = bottom-right). `stops`
    /// are `(offset 0..=1, color)` pairs, sorted by offset.
    Linear {
        start: (f32, f32),
        end: (f32, f32),
        stops: Vec<(f32, u32)>,
    },
    /// A radial gradient centered at `center` (unit point in the frame) ramping
    /// from `start_radius` to `end_radius` (unit fractions of the frame's
    /// shorter side). `stops` are `(offset 0..=1, color)` pairs.
    Radial {
        center: (f32, f32),
        start_radius: f32,
        end_radius: f32,
        stops: Vec<(f32, u32)>,
    },
    /// An angular (conic) gradient swept about `center` (unit point). Honored
    /// only partially: the backend approximates it by the first/last stop, so it
    /// reads as a flat-ish fill rather than a true sweep.
    Angular { center: (f32, f32), stops: Vec<(f32, u32)> },
}

impl Fill {
    /// A representative single color for backends that cannot render gradients
    /// (the software canvas): the first stop's color, or the solid color.
    pub fn representative_color(&self) -> u32 {
        match self {
            Fill::Solid(c) => *c,
            Fill::Linear { stops, .. }
            | Fill::Radial { stops, .. }
            | Fill::Angular { stops, .. } => stops.first().map(|s| s.1).unwrap_or(0),
        }
    }

    /// Sample the fill color at unit point `(u, v)` in the shape's frame.
    /// Used by backends that interpolate gradients per-vertex.
    pub fn sample(&self, u: f32, v: f32) -> u32 {
        match self {
            Fill::Solid(c) => *c,
            Fill::Linear { start, end, stops } => {
                let dx = end.0 - start.0;
                let dy = end.1 - start.1;
                let len2 = dx * dx + dy * dy;
                let t = if len2 <= f32::EPSILON {
                    0.0
                } else {
                    ((u - start.0) * dx + (v - start.1) * dy) / len2
                };
                sample_stops(stops, t)
            }
            Fill::Radial {
                center,
                start_radius,
                end_radius,
                stops,
            } => {
                let du = u - center.0;
                let dv = v - center.1;
                let dist = (du * du + dv * dv).sqrt();
                let span = end_radius - start_radius;
                let t = if span.abs() <= f32::EPSILON {
                    0.0
                } else {
                    (dist - start_radius) / span
                };
                sample_stops(stops, t)
            }
            Fill::Angular { center, stops } => {
                let du = u - center.0;
                let dv = v - center.1;
                // angle in 0..1 starting at +x, clockwise (y-down space).
                let mut a = dv.atan2(du) / std::f32::consts::TAU;
                if a < 0.0 {
                    a += 1.0;
                }
                sample_stops(stops, a)
            }
        }
    }
}

/// Linearly interpolate a sorted `(offset, 0xRRGGBBAA)` stop list at `t`.
fn sample_stops(stops: &[(f32, u32)], t: f32) -> u32 {
    if stops.is_empty() {
        return 0;
    }
    let t = t.clamp(0.0, 1.0);
    if t <= stops[0].0 {
        return stops[0].1;
    }
    if t >= stops[stops.len() - 1].0 {
        return stops[stops.len() - 1].1;
    }
    for w in stops.windows(2) {
        let (o0, c0) = w[0];
        let (o1, c1) = w[1];
        if t >= o0 && t <= o1 {
            let span = (o1 - o0).max(f32::EPSILON);
            let f = (t - o0) / span;
            return lerp_color(c0, c1, f);
        }
    }
    stops[stops.len() - 1].1
}

/// Lerp two packed `0xRRGGBBAA` colors in straight (non-premultiplied) space.
fn lerp_color(a: u32, b: u32, f: f32) -> u32 {
    let f = f.clamp(0.0, 1.0);
    let ch = |shift: u32| {
        let av = ((a >> shift) & 0xff) as f32;
        let bv = ((b >> shift) & 0xff) as f32;
        ((av + (bv - av) * f).round().clamp(0.0, 255.0) as u32) << shift
    };
    ch(24) | ch(16) | ch(8) | ch(0)
}

/// A single segment in a [`DrawCmd::Path`]. Coordinates are absolute logical
/// pixels (the lowering pass resolves any frame-relative coords before
/// building the command).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathOp {
    /// Start a new subpath at `(x, y)`.
    MoveTo { x: f32, y: f32 },
    /// Straight line from the current point to `(x, y)`.
    LineTo { x: f32, y: f32 },
    /// Quadratic Bézier to `(x, y)` with control point `(cx, cy)`.
    QuadTo { cx: f32, cy: f32, x: f32, y: f32 },
    /// Close the current subpath (line back to its start).
    Close,
}

/// A single drawing primitive. All coordinates/sizes are **logical pixels**,
/// origin top-left, y increasing downward. Colors are packed `0xRRGGBBAA`.
#[derive(Clone, Debug, PartialEq)]
pub enum DrawCmd {
    /// A solid-filled rectangle with optional uniform corner rounding.
    FilledRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        /// Fill color, packed `0xRRGGBBAA`.
        color: u32,
        /// Corner radius in logical pixels. `0.0` == sharp corners. Clamped to
        /// half the shorter side by the backend.
        corner_radius: f32,
        /// Clockwise rotation in **degrees** about the rect's own center
        /// (`(x + w/2, y + h/2)`). `0.0` == axis-aligned (the identity); this
        /// is the default for every rect emitted by the layout/paint pass that
        /// carries no `rotationEffect`, so existing scenes are unchanged. A
        /// backend that cannot rotate may treat the rect as axis-aligned.
        rotation: f32,
    },
    /// A **frosted-glass panel**: a rounded rectangle whose interior shows the
    /// scene drawn *before* it, blurred (a real backdrop blur), with a
    /// translucent `tint` laid over the blur and a subtle 1px light inner edge.
    ///
    /// Painter's order holds: a `FrostedRect` blurs only what was drawn before
    /// it in the [`Scene`]; commands after it paint on top of the panel as
    /// usual. The backend implements this with an offscreen render-graph
    /// (render-to-texture + separable Gaussian blur + composite), so the effect
    /// is a genuine blur of the backdrop rather than a flat translucent fill.
    FrostedRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        /// Corner radius in logical pixels. `0.0` == sharp corners. Clamped to
        /// half the shorter side by the backend.
        corner_radius: f32,
        /// Tint laid over the blurred backdrop, packed `0xRRGGBBAA`. Usually a
        /// translucent white (light glass) or near-black (dark glass).
        tint: u32,
        /// Gaussian blur radius in logical pixels. Larger == frostier. `0.0`
        /// disables the blur (tint-only panel).
        blur_radius: f32,
    },
    /// A run of text. Layout/shaping is the backend's job; the position is the
    /// top-left of the text box's content origin.
    Text {
        x: f32,
        y: f32,
        content: String,
        /// Font size in logical pixels.
        size: f32,
        /// Text color, packed `0xRRGGBBAA`.
        color: u32,
    },
    /// A filled [`Shape`] occupying the frame `(x, y, w, h)`, painted with a
    /// [`Fill`] (solid or gradient), optionally rotated about its center.
    ///
    /// This is the SwiftUI shape primitive: `Circle`, `Ellipse`, `Capsule`,
    /// `RoundedRectangle`, `Rectangle` all lower here. A backend that cannot
    /// render gradients may use [`Fill::representative_color`]; a backend that
    /// cannot rotate may treat `rotation == 0.0`.
    FilledShape {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        shape: Shape,
        fill: Fill,
        /// Clockwise rotation in **degrees** about the frame's center. `0.0`
        /// (the default constructor's value) leaves the shape axis-aligned.
        rotation: f32,
    },
    /// A tessellated path: a list of [`PathOp`]s, optionally filled and/or
    /// stroked. Curves (`QuadTo`) are flattened by the backend; backends that
    /// only do straight segments may treat `QuadTo` as a `LineTo` to its end
    /// point (marked *partial*).
    Path {
        /// Absolute logical-pixel path ops.
        ops: Vec<PathOp>,
        /// Optional interior fill.
        fill: Option<Fill>,
        /// Optional stroke `(color, width)` painted along the outline.
        stroke: Option<(u32, f32)>,
    },
}

impl DrawCmd {
    /// An axis-aligned [`DrawCmd::FilledRect`] (`rotation == 0.0`).
    ///
    /// Convenience for the common, un-rotated case so callers don't have to
    /// spell out `rotation: 0.0`. Use the struct literal directly when you do
    /// want a `rotationEffect`.
    pub fn filled_rect(x: f32, y: f32, w: f32, h: f32, color: u32, corner_radius: f32) -> Self {
        DrawCmd::FilledRect {
            x,
            y,
            w,
            h,
            color,
            corner_radius,
            rotation: 0.0,
        }
    }

    /// An axis-aligned [`DrawCmd::FilledShape`] (`rotation == 0.0`).
    pub fn filled_shape(x: f32, y: f32, w: f32, h: f32, shape: Shape, fill: Fill) -> Self {
        DrawCmd::FilledShape {
            x,
            y,
            w,
            h,
            shape,
            fill,
            rotation: 0.0,
        }
    }
}

/// An ordered list of draw commands. Painted front-to-back in `Vec` order.
pub type Scene = Vec<DrawCmd>;

#[cfg(test)]
mod scene_tests {
    use super::*;

    #[test]
    fn linear_fill_samples_endpoints_and_midpoint() {
        let f = Fill::Linear {
            start: (0.0, 0.0),
            end: (1.0, 0.0),
            stops: vec![(0.0, 0x000000FF), (1.0, 0xFFFFFFFF)],
        };
        assert_eq!(f.sample(0.0, 0.0), 0x000000FF, "start = first stop");
        assert_eq!(f.sample(1.0, 0.0), 0xFFFFFFFF, "end = last stop");
        // Midpoint blends to ~mid-gray.
        let mid = f.sample(0.5, 0.0);
        let r = (mid >> 24) & 0xff;
        assert!((120..=135).contains(&r), "midpoint near 50% ({r})");
    }

    #[test]
    fn radial_fill_center_is_first_stop() {
        let f = Fill::Radial {
            center: (0.5, 0.5),
            start_radius: 0.0,
            end_radius: 0.5,
            stops: vec![(0.0, 0xFF0000FF), (1.0, 0x0000FFFF)],
        };
        assert_eq!(f.sample(0.5, 0.5), 0xFF0000FF, "center = first stop");
    }

    #[test]
    fn representative_color_is_first_stop() {
        let f = Fill::Linear {
            start: (0.0, 0.0),
            end: (1.0, 1.0),
            stops: vec![(0.0, 0x112233FF), (1.0, 0x445566FF)],
        };
        assert_eq!(f.representative_color(), 0x112233FF);
        assert_eq!(Fill::Solid(0xABCDEF12).representative_color(), 0xABCDEF12);
    }

    #[test]
    fn filled_shape_constructor_is_axis_aligned() {
        let c = DrawCmd::filled_shape(1.0, 2.0, 3.0, 4.0, Shape::Circle, Fill::Solid(0xFF));
        match c {
            DrawCmd::FilledShape { rotation, shape, .. } => {
                assert_eq!(rotation, 0.0);
                assert_eq!(shape, Shape::Circle);
            }
            _ => panic!("expected FilledShape"),
        }
    }
}
