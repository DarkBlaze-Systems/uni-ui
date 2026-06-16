//! uni-spring — physics-based motion core for the DarkBlaze Uni-UI engine.
//!
//! This crate implements damped-harmonic-oscillator springs of the kind used
//! by Material 3 Expressive motion. CSS/Slint keyframe easings cannot express
//! interruptible, velocity-preserving, overshooting motion; a spring can.
//!
//! # The model
//!
//! Each channel is a 1-D damped harmonic oscillator pulled toward a `target`:
//!
//! ```text
//! force = -stiffness * (value - target) - damping * velocity
//! accel = force / mass
//! ```
//!
//! Integration uses **semi-implicit (symplectic) Euler**: velocity is advanced
//! first, then position is advanced using the *new* velocity. This is the
//! standard stable integrator for game/UI physics — unlike explicit Euler it
//! does not pump energy into the system, so high-stiffness springs at large
//! `dt` stay bounded instead of exploding.
//!
//! ```text
//! v += accel * dt
//! x += v * dt        // uses the just-updated v  <- the "semi-implicit" part
//! ```
//!
//! # The two-spring discipline
//!
//! Material 3 Expressive splits motion into two families:
//!
//! * **Spatial** springs ([`Spring::spatial`], [`Spring::spatial_expressive`])
//!   drive position, size and layout. They are *under-damped* (damping ratio
//!   `< 1`) so they overshoot and settle with a little bounce — that liveliness
//!   is the point.
//! * **Effects** springs ([`Spring::effects`]) drive color, opacity and other
//!   properties where an overshoot would be a visual bug (e.g. opacity > 1, or
//!   a color crossing past its destination). They are *critically* damped
//!   (damping ratio `>= 1`) so the approach is **monotonic — never overshoots**.
//!
//! See the note on the overshoot guarantee at [`Spring::overshoots`].
//!
//! # Interruptible motion
//!
//! Motion is redirectable: setting [`SpringState::set_target`] (or writing
//! `state.target`) mid-flight leaves the current [`SpringState::velocity`]
//! untouched. The spring smoothly curves toward the new target carrying its
//! momentum, which is what makes redirected gestures feel physical.

#![forbid(unsafe_code)]
#![no_std]

/// A damped harmonic oscillator, described by its physical constants.
///
/// A `Spring` is *stateless* — it is just the tuning. The moving quantities
/// (position, velocity, target) live in [`SpringState`]. One `Spring` can drive
/// any number of states.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spring {
    /// Restoring-force constant `k`. Higher = snappier, faster oscillation.
    pub stiffness: f32,
    /// Velocity-damping constant `c`. Higher = more friction, less bounce.
    pub damping: f32,
    /// Mass `m`. Higher = more inertia, slower response.
    pub mass: f32,
}

impl Spring {
    /// Construct a spring from raw physical constants.
    ///
    /// `mass` is clamped to a small positive floor to avoid division by zero.
    #[inline]
    #[must_use]
    pub fn new(stiffness: f32, damping: f32, mass: f32) -> Self {
        Self {
            stiffness,
            damping,
            mass: if mass > MIN_MASS { mass } else { MIN_MASS },
        }
    }

    /// The undamped angular frequency `w0 = sqrt(k / m)`, in rad/s.
    #[inline]
    #[must_use]
    pub fn natural_frequency(&self) -> f32 {
        sqrt(self.stiffness / self.mass)
    }

    /// The **damping ratio** `zeta = c / (2*sqrt(k*m))`.
    ///
    /// * `zeta < 1` — under-damped: the spring overshoots and oscillates.
    /// * `zeta == 1` — critically damped: fastest settle with no overshoot.
    /// * `zeta > 1` — over-damped: slow, no overshoot.
    #[inline]
    #[must_use]
    pub fn damping_ratio(&self) -> f32 {
        let denom = 2.0 * sqrt(self.stiffness * self.mass);
        if denom > 0.0 {
            self.damping / denom
        } else {
            f32::INFINITY
        }
    }

    /// Whether this spring can overshoot its target.
    ///
    /// True iff the spring is **under-damped** (`damping_ratio() < 1`). This is
    /// the formal statement of the two-spring guarantee:
    ///
    /// * spatial presets return `true`  — they may overshoot (the bounce);
    /// * effects presets return `false` — they approach monotonically.
    ///
    /// For an at-rest spring released from one side of its target, a non-
    /// overshooting (`false`) spring will reach the target without ever passing
    /// it. (As always with redirectable motion, this monotonicity describes the
    /// response to a single fixed target; a target moved *past* the current
    /// value will of course be chased in the new direction.)
    #[inline]
    #[must_use]
    pub fn overshoots(&self) -> bool {
        self.damping_ratio() < 1.0
    }

    // ----- Spatial presets (under-damped — position / size / layout) -------

    /// Standard spatial spring: gentle overshoot, lively settle.
    ///
    /// `zeta ~= 0.75` (under-damped). Use for position and size.
    #[inline]
    #[must_use]
    pub fn spatial() -> Self {
        // zeta = c / (2*sqrt(k*m)) = 30 / (2*sqrt(400*1)) = 30/40 = 0.75
        Self::new(400.0, 30.0, 1.0)
    }

    /// Snappier spatial spring for small, quick movements.
    ///
    /// Same damping-ratio family as [`Spring::spatial`], higher frequency.
    #[inline]
    #[must_use]
    pub fn spatial_fast() -> Self {
        // zeta = 42 / (2*sqrt(800)) ~= 0.74
        Self::new(800.0, 42.0, 1.0)
    }

    /// Slower, more deliberate spatial spring for large transitions.
    #[inline]
    #[must_use]
    pub fn spatial_slow() -> Self {
        // zeta = 21 / (2*sqrt(200)) ~= 0.74
        Self::new(200.0, 21.0, 1.0)
    }

    /// Expressive spatial spring: pronounced bounce.
    ///
    /// `zeta ~= 0.45` (clearly under-damped) for a playful, energetic overshoot.
    #[inline]
    #[must_use]
    pub fn spatial_expressive() -> Self {
        // zeta = 18 / (2*sqrt(400)) = 18/40 = 0.45
        Self::new(400.0, 18.0, 1.0)
    }

    // ----- Effects presets (critically/over-damped — color / opacity) ------

    /// Standard effects spring: **no overshoot**, monotonic approach.
    ///
    /// `zeta == 1` (critically damped). Use for opacity, color and other
    /// channels where passing the target is a visual artifact.
    #[inline]
    #[must_use]
    pub fn effects() -> Self {
        // zeta = c / (2*sqrt(k*m)) = 40 / (2*sqrt(400*1)) = 40/40 = 1.0
        Self::new(400.0, 40.0, 1.0)
    }

    /// Snappier effects spring, still non-overshooting.
    ///
    /// Slightly over-damped and higher frequency for quick fades.
    #[inline]
    #[must_use]
    pub fn effects_fast() -> Self {
        // zeta = 58 / (2*sqrt(800)) ~= 1.025  (over-damped -> no overshoot)
        Self::new(800.0, 58.0, 1.0)
    }

    /// Slower effects spring, still non-overshooting.
    #[inline]
    #[must_use]
    pub fn effects_slow() -> Self {
        // zeta = 29 / (2*sqrt(200)) ~= 1.025  (over-damped -> no overshoot)
        Self::new(200.0, 29.0, 1.0)
    }

    /// Default general-purpose spring (alias for [`Spring::spatial`]).
    #[inline]
    #[must_use]
    pub fn default_spring() -> Self {
        Self::spatial()
    }
}

impl Default for Spring {
    #[inline]
    fn default() -> Self {
        Self::spatial()
    }
}

/// The moving state of a single spring-driven channel.
///
/// Holds the current `value`, current `velocity`, and the `target` it is being
/// pulled toward. Advance it with [`SpringState::step`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringState {
    /// Current position / value of the channel.
    pub value: f32,
    /// Current rate of change (units per second).
    pub velocity: f32,
    /// The resting point the spring is pulled toward.
    pub target: f32,
}

impl SpringState {
    /// Create a state at rest (zero velocity) at `value`, targeting `target`.
    #[inline]
    #[must_use]
    pub fn new(value: f32, target: f32) -> Self {
        Self {
            value,
            velocity: 0.0,
            target,
        }
    }

    /// Create a state already at rest *on* `value` (value == target, v == 0).
    #[inline]
    #[must_use]
    pub fn at(value: f32) -> Self {
        Self::new(value, value)
    }

    /// Redirect the spring toward a new target **without** touching velocity.
    ///
    /// This is the interruptible/redirectable path: existing momentum is
    /// preserved, so the motion curves smoothly toward the new destination.
    #[inline]
    pub fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    /// Advance the simulation by `dt` seconds using semi-implicit Euler.
    ///
    /// Velocity is integrated first, then position is integrated using the new
    /// velocity — the symplectic update that keeps the system stable.
    ///
    /// Non-positive `dt` is a no-op.
    pub fn step(&mut self, spring: &Spring, dt: f32) {
        if dt <= 0.0 {
            return;
        }
        let displacement = self.value - self.target;
        // F = -k*x - c*v  ;  a = F / m
        let accel =
            (-spring.stiffness * displacement - spring.damping * self.velocity) / spring.mass;
        // Semi-implicit (symplectic) Euler: update v, then x with the new v.
        self.velocity += accel * dt;
        self.value += self.velocity * dt;
    }

    /// Whether the spring has effectively come to rest at its target.
    ///
    /// True when both the distance to target and the velocity are within `eps`.
    #[inline]
    #[must_use]
    pub fn is_settled(&self, eps: f32) -> bool {
        abs(self.value - self.target) <= eps && abs(self.velocity) <= eps
    }

    /// Snap directly to the target and zero the velocity (instant finish).
    #[inline]
    pub fn settle(&mut self) {
        self.value = self.target;
        self.velocity = 0.0;
    }
}

/// Drive `N` spring channels (e.g. `x, y, w, h`) with a shared [`Spring`].
///
/// Each channel keeps its own value/velocity/target, so they can be redirected
/// independently while sharing one set of physics constants.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringVec<const N: usize> {
    /// The per-channel states.
    pub channels: [SpringState; N],
}

impl<const N: usize> SpringVec<N> {
    /// Create `N` channels at rest, each at the matching `values[i]`.
    #[inline]
    #[must_use]
    pub fn new(values: [f32; N]) -> Self {
        let mut channels = [SpringState::at(0.0); N];
        let mut i = 0;
        while i < N {
            channels[i] = SpringState::at(values[i]);
            i += 1;
        }
        Self { channels }
    }

    /// Redirect every channel to a new target, preserving each velocity.
    #[inline]
    pub fn set_targets(&mut self, targets: [f32; N]) {
        let mut i = 0;
        while i < N {
            self.channels[i].set_target(targets[i]);
            i += 1;
        }
    }

    /// Advance all channels by `dt` using the shared `spring`.
    pub fn step(&mut self, spring: &Spring, dt: f32) {
        let mut i = 0;
        while i < N {
            self.channels[i].step(spring, dt);
            i += 1;
        }
    }

    /// True only when *every* channel is settled within `eps`.
    #[inline]
    #[must_use]
    pub fn is_settled(&self, eps: f32) -> bool {
        let mut i = 0;
        while i < N {
            if !self.channels[i].is_settled(eps) {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Snapshot the current value of each channel.
    #[inline]
    #[must_use]
    pub fn values(&self) -> [f32; N] {
        let mut out = [0.0; N];
        let mut i = 0;
        while i < N {
            out[i] = self.channels[i].value;
            i += 1;
        }
        out
    }
}

// --- SwiftUI-style animation toolkit -----------------------------------------
//
// The `Spring` above is the *physics* core: an interruptible, velocity-preserving
// oscillator with no fixed duration. SwiftUI's `Animation` is the other idiom — a
// declarative *descriptor* pairing a curve with a (mostly) fixed duration that you
// sample at a normalized time. This section adds that idiom **alongside** the
// spring, sharing the same crate and the same `#![no_std]` discipline.
//
// A timed curve maps elapsed time `t` (seconds) to a normalized `progress` in
// `0..=1`. The spring curve has no inherent duration, so its descriptor samples
// the existing physics path: it integrates a unit `SpringState` (0 -> 1) at a
// fixed internal step and reports the value at time `t` — the same integrator the
// rest of the crate is built on, just driven on a clock instead of per-frame.

/// A timing curve: how normalized progress evolves over a fixed duration.
///
/// Every variant maps an elapsed time onto a progress in `0..=1`. The `Spring`
/// variant reuses the crate's physics integrator (see [`Animation::sample`]); the
/// rest are classic CSS/SwiftUI easing functions evaluated in closed form.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Curve {
    /// Constant velocity: `progress == t / duration`.
    Linear,
    /// Cubic ease-in: starts slow, accelerates. `f(x) = x^3`.
    EaseIn,
    /// Cubic ease-out: starts fast, decelerates. `f(x) = 1 - (1-x)^3`.
    EaseOut,
    /// Cubic ease-in-out: slow at both ends, fast in the middle.
    EaseInOut,
    /// Physics spring sampled on a clock, parameterised the SwiftUI way by
    /// `response` (seconds, the natural period) and `damping_fraction` (the
    /// damping ratio `zeta`). Reuses [`SpringState::step`] under the hood.
    Spring {
        /// Natural period in seconds. SwiftUI's `response`. Smaller = snappier.
        response: f32,
        /// Damping ratio `zeta`. `1.0` = critically damped (no overshoot);
        /// `< 1.0` bounces. SwiftUI's `dampingFraction`.
        damping_fraction: f32,
    },
}

/// A SwiftUI-style animation descriptor: a [`Curve`] plus a `duration`.
///
/// Build one with the presets ([`Animation::linear`], [`Animation::ease_in_out`],
/// [`Animation::spring`], …) and read its progress with [`Animation::sample`].
/// This is purely declarative tuning — it carries no moving state, so one
/// `Animation` can drive any number of properties.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Animation {
    /// The timing curve.
    pub curve: Curve,
    /// Total duration in seconds. For [`Curve::Spring`] this is the settle
    /// horizon used to report progress (`1.0` at and beyond it).
    pub duration: f32,
}

impl Animation {
    /// Construct an animation from a curve and duration.
    ///
    /// `duration` is clamped to a small positive floor so `sample` never divides
    /// by zero; a zero-duration animation reports `1.0` for any `t > 0`.
    #[inline]
    #[must_use]
    pub fn new(curve: Curve, duration: f32) -> Self {
        Self {
            curve,
            duration: if duration > MIN_DURATION {
                duration
            } else {
                MIN_DURATION
            },
        }
    }

    /// SwiftUI `.linear(duration:)` — constant velocity.
    #[inline]
    #[must_use]
    pub fn linear(duration: f32) -> Self {
        Self::new(Curve::Linear, duration)
    }

    /// SwiftUI `.easeIn(duration:)` — cubic ease-in.
    #[inline]
    #[must_use]
    pub fn ease_in(duration: f32) -> Self {
        Self::new(Curve::EaseIn, duration)
    }

    /// SwiftUI `.easeOut(duration:)` — cubic ease-out.
    #[inline]
    #[must_use]
    pub fn ease_out(duration: f32) -> Self {
        Self::new(Curve::EaseOut, duration)
    }

    /// SwiftUI `.easeInOut(duration:)` — cubic ease-in-out.
    #[inline]
    #[must_use]
    pub fn ease_in_out(duration: f32) -> Self {
        Self::new(Curve::EaseInOut, duration)
    }

    /// SwiftUI `.spring(response:dampingFraction:)`.
    ///
    /// `response` is the natural period in seconds; `damping_fraction` is the
    /// damping ratio. `duration` is the settle horizon over which progress is
    /// reported (SwiftUI's spring is unbounded in principle; we report `1.0`
    /// once the clock passes `duration`).
    #[inline]
    #[must_use]
    pub fn spring_with(response: f32, damping_fraction: f32, duration: f32) -> Self {
        Self::new(
            Curve::Spring {
                response,
                damping_fraction,
            },
            duration,
        )
    }

    /// SwiftUI `.spring()` — the default spring preset.
    ///
    /// Matches SwiftUI's defaults: `response = 0.55`, `dampingFraction = 0.825`,
    /// reported over a `1.0`-second settle horizon.
    #[inline]
    #[must_use]
    pub fn spring() -> Self {
        Self::spring_with(0.55, 0.825, 1.0)
    }

    /// Convert a [`Curve::Spring`] descriptor into the physics [`Spring`] it
    /// represents (mass fixed at `1.0`).
    ///
    /// `response` is the period `T = 2*pi / w0`, so `k = (2*pi / response)^2`;
    /// `damping_fraction` is `zeta`, so `c = 2 * zeta * sqrt(k)`. Returns `None`
    /// for non-spring curves.
    #[inline]
    #[must_use]
    pub fn as_spring(&self) -> Option<Spring> {
        match self.curve {
            Curve::Spring {
                response,
                damping_fraction,
            } => {
                let resp = if response > MIN_DURATION {
                    response
                } else {
                    MIN_DURATION
                };
                let w0 = TAU / resp;
                let k = w0 * w0;
                let c = 2.0 * damping_fraction * sqrt(k);
                Some(Spring::new(k, c, 1.0))
            }
            _ => None,
        }
    }

    /// Sample the animation at elapsed time `t` (seconds), returning progress.
    ///
    /// * For the timed curves the result is **clamped to `0..=1`**: `0.0` at
    ///   `t <= 0`, the curve in between, and `1.0` at `t >= duration`.
    /// * For [`Curve::Spring`] the value comes from integrating a unit
    ///   `SpringState` (0 -> 1) with [`SpringState::step`] up to `t`, then
    ///   forced to exactly `1.0` once `t >= duration` so timed and physics paths
    ///   share the same "done at duration" contract.
    #[must_use]
    pub fn sample(&self, t: f32) -> f32 {
        if t <= 0.0 {
            return 0.0;
        }
        if t >= self.duration {
            return 1.0;
        }
        match self.curve {
            Curve::Linear => clamp01(t / self.duration),
            Curve::EaseIn => {
                let x = clamp01(t / self.duration);
                x * x * x
            }
            Curve::EaseOut => {
                let x = clamp01(t / self.duration);
                let inv = 1.0 - x;
                1.0 - inv * inv * inv
            }
            Curve::EaseInOut => {
                let x = clamp01(t / self.duration);
                if x < 0.5 {
                    4.0 * x * x * x
                } else {
                    let inv = -2.0 * x + 2.0;
                    1.0 - (inv * inv * inv) / 2.0
                }
            }
            Curve::Spring { .. } => {
                // Reuse the physics integrator: unit state 0 -> 1, stepped to t.
                let spring = self.as_spring().unwrap_or_else(Spring::effects);
                let mut state = SpringState::new(0.0, 1.0);
                // Fixed fine step keeps the closed-clock sample accurate and
                // stable (semi-implicit Euler); the remainder is taken last.
                let step = SPRING_SAMPLE_DT;
                let mut elapsed = 0.0;
                while elapsed + step <= t {
                    state.step(&spring, step);
                    elapsed += step;
                }
                let rem = t - elapsed;
                if rem > 0.0 {
                    state.step(&spring, rem);
                }
                state.value
            }
        }
    }
}

const MIN_DURATION: f32 = 1e-6;
const TAU: f32 = 6.283_185_5;
const SPRING_SAMPLE_DT: f32 = 1.0 / 1000.0;

#[inline]
fn clamp01(x: f32) -> f32 {
    // `f32::clamp` is available in core (no_std-safe).
    x.clamp(0.0, 1.0)
}

// --- Batch pool (std only) ---------------------------------------------------
//
// `SpringVec<N>` drives a handful of channels that belong to *one* widget.
// `SpringPool` is the other axis: thousands of *independent* springs sharing one
// set of physics constants — every animating property in a running UI tree. That
// per-channel integration loop is exactly the planar-SoA, broadcast-the-constants
// workload SIMD wants, so the pool delegates its step to `uni-simd`'s
// runtime-dispatched [`integrate_springs`] kernel. (Requires the `std` feature
// for heap storage; the rest of the crate stays `#![no_std]`.)

#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "std")]
pub use pool::SpringPool;

#[cfg(feature = "std")]
mod pool {
    use crate::{Spring, MIN_MASS};
    use std::vec::Vec;

    /// A pool of independent springs that share one [`Spring`]'s constants.
    ///
    /// Storage is **struct-of-arrays**: values, velocities and targets each live
    /// in their own contiguous `Vec`, which is what lets [`SpringPool::step`]
    /// hand whole registers to `uni-simd` without any per-element gather.
    #[derive(Clone, Debug, PartialEq)]
    pub struct SpringPool {
        values: Vec<f32>,
        velocities: Vec<f32>,
        targets: Vec<f32>,
        spring: Spring,
    }

    impl SpringPool {
        /// Empty pool driven by `spring`.
        #[inline]
        #[must_use]
        pub fn new(spring: Spring) -> Self {
            Self {
                values: Vec::new(),
                velocities: Vec::new(),
                targets: Vec::new(),
                spring,
            }
        }

        /// A pool of `n` springs at rest on `value`, each targeting `value`.
        #[must_use]
        pub fn filled(spring: Spring, n: usize, value: f32) -> Self {
            Self {
                values: std::vec![value; n],
                velocities: std::vec![0.0; n],
                targets: std::vec![value; n],
                spring,
            }
        }

        /// Append a channel; returns its index.
        pub fn push(&mut self, value: f32, target: f32) -> usize {
            let i = self.values.len();
            self.values.push(value);
            self.velocities.push(0.0);
            self.targets.push(target);
            i
        }

        /// Number of channels.
        #[inline]
        #[must_use]
        pub fn len(&self) -> usize {
            self.values.len()
        }

        /// Whether the pool holds no channels.
        #[inline]
        #[must_use]
        pub fn is_empty(&self) -> bool {
            self.values.is_empty()
        }

        /// Redirect channel `i` to `target`, preserving its velocity.
        #[inline]
        pub fn set_target(&mut self, i: usize, target: f32) {
            self.targets[i] = target;
        }

        /// Read channel `i`'s current value.
        #[inline]
        #[must_use]
        pub fn value(&self, i: usize) -> f32 {
            self.values[i]
        }

        /// The current values of every channel (in push order).
        #[inline]
        #[must_use]
        pub fn values(&self) -> &[f32] {
            &self.values
        }

        /// Advance the whole pool by `dt` seconds with one symplectic-Euler step,
        /// offloaded to `uni-simd`'s runtime-dispatched SIMD kernel.
        ///
        /// Non-positive `dt` is a no-op (matching [`crate::SpringState::step`]).
        pub fn step(&mut self, dt: f32) {
            if dt <= 0.0 {
                return;
            }
            let m = if self.spring.mass > MIN_MASS {
                self.spring.mass
            } else {
                MIN_MASS
            };
            uni_simd::integrate_springs(
                &mut self.values,
                &mut self.velocities,
                &self.targets,
                self.spring.stiffness,
                self.spring.damping,
                1.0 / m,
                dt,
            );
        }

        /// Reference scalar step (no SIMD) — the source of truth the batched
        /// kernel is validated against, and the baseline for the bench.
        pub fn step_scalar(&mut self, dt: f32) {
            if dt <= 0.0 {
                return;
            }
            let m = if self.spring.mass > MIN_MASS {
                self.spring.mass
            } else {
                MIN_MASS
            };
            uni_simd::integrate_springs_scalar(
                &mut self.values,
                &mut self.velocities,
                &self.targets,
                self.spring.stiffness,
                self.spring.damping,
                1.0 / m,
                dt,
            );
        }
    }
}

// --- std-free math helpers ---------------------------------------------------
//
// `#![no_std]` means we avoid pulling in the platform libm. These tiny
// implementations keep the crate dependency-free.

const MIN_MASS: f32 = 1e-6;

#[inline]
fn abs(x: f32) -> f32 {
    if x < 0.0 {
        -x
    } else {
        x
    }
}

/// Square root via Newton-Raphson. Sufficient precision for motion physics.
#[inline]
fn sqrt(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    if !x.is_finite() {
        return x; // sqrt(inf) == inf
    }
    // Seed with a rough estimate, then refine. Newton converges quadratically.
    let mut guess = x;
    let mut i = 0;
    while i < 24 {
        let next = 0.5 * (guess + x / guess);
        if abs(next - guess) <= f32::EPSILON * guess {
            return next;
        }
        guess = next;
        i += 1;
    }
    guess
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 240.0; // 240 Hz simulation step
    const MAX_STEPS: usize = 100_000;

    /// Run a state to settle, returning (steps, peak overshoot past target).
    ///
    /// Starts at value=0, target=1, so any value > 1 is an overshoot.
    fn run(state: &mut SpringState, spring: &Spring, eps: f32) -> (usize, f32) {
        let mut peak_past = 0.0f32;
        for steps in 1..=MAX_STEPS {
            state.step(spring, DT);
            let past = state.value - state.target;
            if past > peak_past {
                peak_past = past;
            }
            if state.is_settled(eps) {
                return (steps, peak_past);
            }
        }
        (MAX_STEPS, peak_past)
    }

    #[test]
    fn spatial_spring_overshoots_then_settles() {
        let spring = Spring::spatial();
        assert!(spring.overshoots(), "spatial must be under-damped");
        let mut s = SpringState::new(0.0, 1.0);
        let (steps, peak_past) = run(&mut s, &spring, 1e-4);
        assert!(peak_past > 0.0, "spatial spring should overshoot target");
        assert!(
            steps < MAX_STEPS,
            "spatial spring should settle in finite steps"
        );
        assert!(s.is_settled(1e-4));
    }

    #[test]
    fn expressive_overshoots_more_than_standard() {
        let mut a = SpringState::new(0.0, 1.0);
        let mut b = SpringState::new(0.0, 1.0);
        let (_, peak_std) = run(&mut a, &Spring::spatial(), 1e-4);
        let (_, peak_exp) = run(&mut b, &Spring::spatial_expressive(), 1e-4);
        assert!(
            peak_exp > peak_std,
            "expressive ({peak_exp}) should overshoot more than standard ({peak_std})"
        );
    }

    #[test]
    fn effects_spring_never_exceeds_target() {
        let spring = Spring::effects();
        assert!(!spring.overshoots(), "effects must not be under-damped");
        let mut s = SpringState::new(0.0, 1.0);
        let mut prev = s.value;
        for _ in 0..MAX_STEPS {
            s.step(&spring, DT);
            assert!(
                s.value <= 1.0 + 1e-5,
                "effects spring overshot target: value={}",
                s.value
            );
            assert!(s.value + 1e-5 >= prev, "effects approach not monotonic");
            prev = s.value;
            if s.is_settled(1e-4) {
                break;
            }
        }
        assert!(s.is_settled(1e-4));
    }

    #[test]
    fn effects_fast_and_slow_do_not_overshoot() {
        for spring in [Spring::effects_fast(), Spring::effects_slow()] {
            assert!(!spring.overshoots());
            let mut s = SpringState::new(0.0, 1.0);
            for _ in 0..MAX_STEPS {
                s.step(&spring, DT);
                assert!(s.value <= 1.0 + 1e-5, "overshot: {}", s.value);
                if s.is_settled(1e-4) {
                    break;
                }
            }
            assert!(s.is_settled(1e-4));
        }
    }

    #[test]
    fn redirect_preserves_velocity() {
        let spring = Spring::spatial();
        let mut s = SpringState::new(0.0, 1.0);
        for _ in 0..30 {
            s.step(&spring, DT);
        }
        let v_before = s.velocity;
        assert!(v_before != 0.0, "should have velocity mid-flight");
        s.set_target(5.0);
        assert_eq!(
            s.velocity, v_before,
            "redirecting target must preserve velocity"
        );
        // Writing the field directly behaves the same.
        let v2 = s.velocity;
        s.target = -3.0;
        assert_eq!(s.velocity, v2);
    }

    #[test]
    fn is_settled_becomes_true_in_finite_steps() {
        for spring in [
            Spring::spatial(),
            Spring::spatial_fast(),
            Spring::spatial_slow(),
            Spring::spatial_expressive(),
            Spring::effects(),
            Spring::effects_fast(),
            Spring::effects_slow(),
        ] {
            let mut s = SpringState::new(0.0, 1.0);
            let mut settled = false;
            for _ in 0..MAX_STEPS {
                s.step(&spring, DT);
                if s.is_settled(1e-4) {
                    settled = true;
                    break;
                }
            }
            assert!(settled, "spring failed to settle: {spring:?}");
        }
    }

    #[test]
    fn stable_at_large_dt() {
        // Semi-implicit Euler should stay bounded even at a coarse step.
        let spring = Spring::spatial_expressive();
        let mut s = SpringState::new(0.0, 1.0);
        for _ in 0..MAX_STEPS {
            s.step(&spring, 1.0 / 60.0);
            assert!(
                s.value.is_finite() && abs(s.value) < 1e3,
                "blew up: {}",
                s.value
            );
            if s.is_settled(1e-4) {
                break;
            }
        }
        assert!(s.is_settled(1e-4));
    }

    #[test]
    fn spring_vec_drives_channels_together() {
        let spring = Spring::spatial();
        let mut v = SpringVec::<4>::new([0.0, 0.0, 0.0, 0.0]);
        v.set_targets([10.0, 20.0, 100.0, 50.0]);
        let mut settled = false;
        for _ in 0..MAX_STEPS {
            v.step(&spring, DT);
            if v.is_settled(1e-3) {
                settled = true;
                break;
            }
        }
        assert!(settled);
        let vals = v.values();
        assert!(abs(vals[0] - 10.0) < 1e-2);
        assert!(abs(vals[2] - 100.0) < 1e-2);
    }

    #[test]
    fn sqrt_helper_is_accurate() {
        for &x in &[1.0f32, 2.0, 4.0, 400.0, 800.0, 1e-3, 12345.6] {
            let got = sqrt(x);
            assert!(abs(got * got - x) <= 1e-2 * x, "sqrt({x}) = {got}");
        }
        assert_eq!(sqrt(0.0), 0.0);
        assert_eq!(sqrt(-1.0), 0.0);
    }

    #[test]
    fn step_ignores_nonpositive_dt() {
        let spring = Spring::spatial();
        let mut s = SpringState::new(0.0, 1.0);
        s.step(&spring, 0.0);
        s.step(&spring, -0.5);
        assert_eq!(s.value, 0.0);
        assert_eq!(s.velocity, 0.0);
    }

    // --- Animation toolkit tests --------------------------------------------

    /// Every timed easing must anchor the endpoints and never leave 0..=1.
    #[test]
    fn timed_curves_hit_endpoints_and_stay_bounded() {
        let dur = 0.5;
        for anim in [
            Animation::linear(dur),
            Animation::ease_in(dur),
            Animation::ease_out(dur),
            Animation::ease_in_out(dur),
        ] {
            // 0 at t == 0.
            assert_eq!(anim.sample(0.0), 0.0, "{anim:?} not 0 at t=0");
            assert_eq!(anim.sample(-1.0), 0.0, "{anim:?} not 0 at t<0");
            // 1 at t >= duration.
            assert_eq!(anim.sample(dur), 1.0, "{anim:?} not 1 at t=duration");
            assert_eq!(anim.sample(dur * 2.0), 1.0, "{anim:?} not 1 past duration");
            // Stay inside 0..=1 across the span.
            for i in 0..=100 {
                let t = dur * (i as f32) / 100.0;
                let p = anim.sample(t);
                assert!((0.0..=1.0).contains(&p), "{anim:?} out of range: {p} at {t}");
            }
        }
    }

    /// Every timed easing must be monotonically non-decreasing.
    #[test]
    fn timed_curves_are_monotonic() {
        let dur = 0.5;
        for anim in [
            Animation::linear(dur),
            Animation::ease_in(dur),
            Animation::ease_out(dur),
            Animation::ease_in_out(dur),
        ] {
            let mut prev = anim.sample(0.0);
            for i in 1..=200 {
                let t = dur * (i as f32) / 200.0;
                let p = anim.sample(t);
                assert!(
                    p + 1e-6 >= prev,
                    "{anim:?} not monotonic: {p} < {prev} at t={t}"
                );
                prev = p;
            }
        }
    }

    /// The cubic easings should differ from linear in the expected direction:
    /// ease-in lags, ease-out leads at the midpoint.
    #[test]
    fn ease_in_out_shape_is_correct() {
        let dur = 1.0;
        let mid = 0.5 * dur;
        let lin = Animation::linear(dur).sample(mid);
        assert!((lin - 0.5).abs() < 1e-6);
        // ease-in is below the line early (slow start).
        assert!(Animation::ease_in(dur).sample(mid) < lin);
        // ease-out is above the line early (fast start).
        assert!(Animation::ease_out(dur).sample(mid) > lin);
        // ease-in-out passes through 0.5 at the midpoint by symmetry.
        assert!((Animation::ease_in_out(dur).sample(mid) - 0.5).abs() < 1e-6);
    }

    /// The SwiftUI `.spring()` preset must settle to ~1.0 within its horizon and
    /// be exactly 1.0 once the clock passes `duration`.
    #[test]
    fn spring_preset_settles() {
        let anim = Animation::spring();
        assert_eq!(anim.sample(0.0), 0.0, "spring not 0 at t=0");
        // Underway: moving toward the target, still within a sane band.
        let early = anim.sample(0.1);
        assert!(early > 0.0, "spring made no progress: {early}");
        // By the settle horizon the curve is pinned to exactly 1.0.
        assert_eq!(anim.sample(anim.duration), 1.0);
        assert_eq!(anim.sample(anim.duration + 1.0), 1.0);
        // Just before the horizon it should already be near the target.
        let near = anim.sample(anim.duration - 1e-3);
        assert!(
            (near - 1.0).abs() < 0.1,
            "spring not settled near horizon: {near}"
        );
    }

    /// A critically/over-damped spring preset must not overshoot past 1.0 while
    /// sampling (the no-overshoot guarantee carried over to the timed sampler).
    #[test]
    fn overdamped_spring_preset_does_not_overshoot() {
        let anim = Animation::spring_with(0.4, 1.0, 1.0);
        for i in 0..=1000 {
            let t = anim.duration * (i as f32) / 1000.0;
            // Below duration the raw physics value is exposed; it must not pass 1.
            if t < anim.duration {
                let p = anim.sample(t);
                assert!(p <= 1.0 + 1e-4, "overdamped spring overshot: {p} at {t}");
            }
        }
    }

    /// The spring descriptor maps `response`/`dampingFraction` onto the physics
    /// `Spring` consistently with `damping_ratio`.
    #[test]
    fn spring_descriptor_maps_to_physics() {
        let anim = Animation::spring_with(0.5, 0.825, 1.0);
        let spring = anim.as_spring().expect("spring curve yields a Spring");
        assert!(
            (spring.damping_ratio() - 0.825).abs() < 1e-3,
            "damping ratio mismatch: {}",
            spring.damping_ratio()
        );
        // Non-spring curves yield None.
        assert!(Animation::linear(1.0).as_spring().is_none());
    }

    /// Zero/degenerate durations are floored, not panicking, and still report a
    /// finished animation for any positive `t`.
    #[test]
    fn degenerate_duration_is_safe() {
        let anim = Animation::linear(0.0);
        assert_eq!(anim.sample(0.0), 0.0);
        assert_eq!(anim.sample(1.0), 1.0);
    }

    #[cfg(feature = "std")]
    #[test]
    fn pool_step_matches_single_state_step() {
        // A pool channel must integrate identically to a lone `SpringState`
        // taking the same constants and `dt` — same model, same answer.
        let spring = Spring::spatial();
        let mut pool = SpringPool::new(spring);
        let i = pool.push(0.0, 1.0);

        let mut s = SpringState::new(0.0, 1.0);
        for _ in 0..500 {
            pool.step(DT);
            s.step(&spring, DT);
        }
        // Single-state uses `/ mass`; pool uses `* (1/mass)`. mass == 1 here, so
        // the two are bit-identical along the SIMD-free (single-lane) tail.
        assert!(
            (pool.value(i) - s.value).abs() < 1e-4,
            "{} vs {}",
            pool.value(i),
            s.value
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn pool_settles_a_full_batch() {
        let mut pool = SpringPool::filled(Spring::effects(), 10_000, 0.0);
        for i in 0..pool.len() {
            pool.set_target(i, 1.0);
        }
        for _ in 0..5000 {
            pool.step(DT);
            if pool.values().iter().all(|&v| abs(v - 1.0) < 1e-3) {
                break;
            }
        }
        assert!(
            pool.values().iter().all(|&v| abs(v - 1.0) < 1e-2),
            "batch did not settle"
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn pool_simd_and_scalar_paths_agree() {
        let spring = Spring::spatial_expressive();
        let mut a = SpringPool::filled(spring, 1234, 0.0);
        let mut b = SpringPool::filled(spring, 1234, 0.0);
        for i in 0..a.len() {
            let t = (i as f32) * 0.01 - 6.0;
            a.set_target(i, t);
            b.set_target(i, t);
        }
        for _ in 0..300 {
            a.step(DT);
            b.step_scalar(DT);
        }
        for i in 0..a.len() {
            assert!(
                abs(a.value(i) - b.value(i)) < 1e-2,
                "simd/scalar drift at {i}: {} vs {}",
                a.value(i),
                b.value(i)
            );
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn pool_ignores_nonpositive_dt() {
        let mut pool = SpringPool::filled(Spring::spatial(), 16, 0.0);
        for i in 0..pool.len() {
            pool.set_target(i, 1.0);
        }
        pool.step(0.0);
        pool.step(-1.0);
        assert!(pool.values().iter().all(|&v| v == 0.0));
    }
}
