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
        assert!(steps < MAX_STEPS, "spatial spring should settle in finite steps");
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
}
