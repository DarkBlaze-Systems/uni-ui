//! Pure-Rust blur-kernel math, factored out of the wgpu backend so it can be
//! unit-tested without a GPU.
//!
//! The fragment shader (`frost.wgsl::fs_blur`) evaluates a Gaussian
//! analytically per tap: `w(i) = exp(-(i^2) / (2 sigma^2))`, summed over taps
//! `i in [-radius, radius]` and normalised by the total weight. These helpers
//! pick a `(sigma, radius)` pair from a requested blur radius and reproduce the
//! same kernel on the CPU so tests can pin the math down.

/// Hard cap on the per-pass tap radius, matching the host-side clamp. Keeps the
/// shader loop bounded on weak iGPUs even if a caller asks for a huge blur.
pub const MAX_BLUR_RADIUS: i32 = 64;

/// Pick the Gaussian `sigma` for a requested blur radius (in blur-target px).
///
/// We treat the requested radius as ~3 sigma (a Gaussian is visually "done" by
/// 3 sigma), so `sigma = radius / 3`, with a small floor so a tiny non-zero
/// radius still blurs perceptibly.
pub fn sigma_for_radius(radius: f32) -> f32 {
    (radius / 3.0).max(0.5)
}

/// The integer tap radius to actually sample for a given sigma: `ceil(3 sigma)`
/// (taps past 3 sigma contribute <2% combined), clamped to [1, MAX].
pub fn taps_for_sigma(sigma: f32) -> i32 {
    let r = (3.0 * sigma).ceil() as i32;
    r.clamp(1, MAX_BLUR_RADIUS)
}

/// Reproduce the shader's 1-D normalised Gaussian weights for taps
/// `0..=radius` (the kernel is symmetric, so index 0 is the centre and index
/// `i>0` is shared by `+i` and `-i`). The returned weights are normalised so
/// that `w[0] + 2*sum(w[1..]) == 1`.
///
/// Mirror of what the shader sums per-fragment; kept for tests / introspection.
#[allow(dead_code)]
pub fn normalized_half_kernel(sigma: f32, radius: i32) -> Vec<f32> {
    let two_s2 = 2.0 * sigma * sigma;
    let radius = radius.max(0);
    let mut raw = Vec::with_capacity(radius as usize + 1);
    for i in 0..=radius {
        let fi = i as f32;
        raw.push((-(fi * fi) / two_s2).exp());
    }
    // Total weight counts the off-centre taps twice (both sides).
    let total: f32 = raw[0] + 2.0 * raw[1..].iter().sum::<f32>();
    for w in &mut raw {
        *w /= total;
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigma_scales_with_radius_and_has_floor() {
        assert!((sigma_for_radius(12.0) - 4.0).abs() < 1e-6);
        // Floor keeps a tiny radius from collapsing to a no-op.
        assert!(sigma_for_radius(0.0) >= 0.5);
        assert!(sigma_for_radius(0.3) >= 0.5);
    }

    #[test]
    fn taps_cover_three_sigma_and_clamp() {
        assert_eq!(taps_for_sigma(4.0), 12); // ceil(12)
        assert_eq!(taps_for_sigma(0.5), 2); // ceil(1.5)
        assert!(taps_for_sigma(0.0) >= 1); // never zero
        assert_eq!(taps_for_sigma(1000.0), MAX_BLUR_RADIUS); // clamped
    }

    #[test]
    fn half_kernel_is_normalized() {
        let sigma = 3.0;
        let radius = taps_for_sigma(sigma);
        let k = normalized_half_kernel(sigma, radius);
        let total = k[0] + 2.0 * k[1..].iter().sum::<f32>();
        assert!(
            (total - 1.0).abs() < 1e-5,
            "kernel must sum to 1, got {total}"
        );
    }

    #[test]
    fn half_kernel_is_monotonically_decreasing() {
        let k = normalized_half_kernel(3.0, 9);
        for w in k.windows(2) {
            assert!(w[0] >= w[1], "Gaussian weights must not increase outward");
        }
        // Centre tap is the heaviest.
        assert!(k[0] > k[k.len() - 1]);
    }

    #[test]
    fn larger_sigma_spreads_weight_outward() {
        // A wider Gaussian puts relatively less mass at the centre.
        let narrow = normalized_half_kernel(1.0, taps_for_sigma(1.0));
        let wide = normalized_half_kernel(6.0, taps_for_sigma(6.0));
        assert!(wide[0] < narrow[0]);
    }
}
