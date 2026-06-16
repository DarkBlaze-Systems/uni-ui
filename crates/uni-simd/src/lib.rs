//! `uni-simd` — the CPU-side hot-path layer for the Uni-UI engine, done the
//! portable way.
//!
//! # Runtime dispatch, not an Intel-only fork
//!
//! Every primitive here ships **two** implementations:
//!
//! * a `*_scalar` reference impl — plain, obviously-correct Rust, the source of
//!   truth used to validate the fast path; and
//! * a [`pulp`]-dispatched impl that picks the widest SIMD register set the
//!   *current CPU* supports **at runtime**, from a single source.
//!
//! [`pulp`] (`MIT`/`Apache-2.0`) probes the host with `is_x86_feature_detected!`
//! / the AArch64 equivalents the first time [`pulp::Arch::new`] is called, then
//! monomorphises the kernel for the chosen instruction-set tier. The *same*
//! code therefore lights up:
//!
//! | Arch    | Tiers pulp dispatches to (widest first)                  |
//! |---------|----------------------------------------------------------|
//! | x86-64  | **AVX-512F/VL** (`f32x16`) → **AVX2+FMA** (`f32x8`) → **SSE2/SSE4.1** (`f32x4`) → scalar |
//! | aarch64 | **NEON** (`f32x4`) → scalar                              |
//! | other   | scalar fallback (`Arch::Scalar`)                         |
//!
//! This is the "Intel edge without an Intel-only fork": an AVX-512 Xeon runs the
//! 16-wide kernel, a Skylake laptop the 8-wide AVX2 kernel, an Apple/ARM box the
//! NEON kernel, and a CI VM with nothing the scalar path — no recompile, no
//! per-target binary, no `#[cfg(target_arch)]` soup in this crate.
//!
//! ## Why two strategies inside `pulp`
//!
//! * [`premultiply_alpha`] and [`transform_points`] are pure multiply/add over
//!   `f32` lanes, so they use *manual* vectorization ([`pulp::WithSimd`]):
//!   split the slice into a SIMD head + scalar tail, run `mul`/`mul_add` on
//!   whole registers.
//! * [`srgb_to_linear_u32`] / [`linear_to_srgb_u32`] need `powf` per channel,
//!   which `pulp`'s `Simd` trait does not expose as a lane op. They use
//!   `pulp`'s *autovectorization* dispatch ([`pulp::Arch::dispatch`] with a
//!   closure): the kernel is compiled once **with the detected target features
//!   enabled**, letting LLVM auto-vectorize the surrounding arithmetic while the
//!   transcendental stays per-lane. Same runtime-dispatch entry point, same
//!   correctness contract.
//!
//! Pixels are `0xRRGGBBAA` (R in the high byte, A in the low byte), matching the
//! engine's `u32` framebuffer convention.

#![forbid(unsafe_code)]

use pulp::{Arch, Simd, WithSimd};

// ---------------------------------------------------------------------------
// sRGB transfer function (IEC 61966-2-1), scalar reference.
// ---------------------------------------------------------------------------

/// sRGB-encoded electrical value `c` (0..=1) -> linear-light value (0..=1).
#[inline]
fn srgb_to_linear_scalar_f32(c: f32) -> f32 {
    if c <= 0.040_448_237 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear-light value `c` (0..=1) -> sRGB-encoded electrical value (0..=1).
#[inline]
fn linear_to_srgb_scalar_f32(c: f32) -> f32 {
    if c <= 0.003_130_668_5 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

#[inline]
fn unpack(px: u32) -> [f32; 4] {
    [
        ((px >> 24) & 0xff) as f32 / 255.0, // R
        ((px >> 16) & 0xff) as f32 / 255.0, // G
        ((px >> 8) & 0xff) as f32 / 255.0,  // B
        (px & 0xff) as f32 / 255.0,         // A
    ]
}

#[inline]
fn pack(rgba: [f32; 4]) -> u32 {
    let q = |v: f32| -> u32 { (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32 & 0xff };
    (q(rgba[0]) << 24) | (q(rgba[1]) << 16) | (q(rgba[2]) << 8) | q(rgba[3])
}

// ===========================================================================
// 1. Bulk sRGB <-> linear color conversion.
//    `dst` holds 4 floats per pixel: [R, G, B, A, R, G, B, A, ...].
//    Alpha is linear in both spaces, so it is copied through unchanged.
// ===========================================================================

/// Scalar reference: `0xRRGGBBAA` -> linear `[R,G,B,A]` floats (4 per pixel).
pub fn srgb_to_linear_u32_scalar(src: &[u32], dst: &mut [f32]) {
    assert_eq!(dst.len(), src.len() * 4, "dst must hold 4 floats per pixel");
    for (px, out) in src.iter().zip(dst.chunks_exact_mut(4)) {
        let c = unpack(*px);
        out[0] = srgb_to_linear_scalar_f32(c[0]);
        out[1] = srgb_to_linear_scalar_f32(c[1]);
        out[2] = srgb_to_linear_scalar_f32(c[2]);
        out[3] = c[3]; // alpha is linear
    }
}

/// Scalar reference: linear `[R,G,B,A]` floats (4 per pixel) -> `0xRRGGBBAA`.
pub fn linear_to_srgb_u32_scalar(src: &[f32], dst: &mut [u32]) {
    assert_eq!(src.len(), dst.len() * 4, "src must hold 4 floats per pixel");
    for (chunk, out) in src.chunks_exact(4).zip(dst.iter_mut()) {
        *out = pack([
            linear_to_srgb_scalar_f32(chunk[0]),
            linear_to_srgb_scalar_f32(chunk[1]),
            linear_to_srgb_scalar_f32(chunk[2]),
            chunk[3], // alpha is linear
        ]);
    }
}

/// Runtime-dispatched `0xRRGGBBAA` -> linear `[R,G,B,A]` floats (4 per pixel).
///
/// Selected at runtime via [`pulp::Arch`]; produces results identical to
/// [`srgb_to_linear_u32_scalar`].
pub fn srgb_to_linear_u32(src: &[u32], dst: &mut [f32]) {
    assert_eq!(dst.len(), src.len() * 4, "dst must hold 4 floats per pixel");
    Arch::new().dispatch(|| srgb_to_linear_u32_scalar(src, dst));
}

/// Runtime-dispatched linear `[R,G,B,A]` floats (4 per pixel) -> `0xRRGGBBAA`.
///
/// Selected at runtime via [`pulp::Arch`]; produces results identical to
/// [`linear_to_srgb_u32_scalar`].
pub fn linear_to_srgb_u32(src: &[f32], dst: &mut [u32]) {
    assert_eq!(src.len(), dst.len() * 4, "src must hold 4 floats per pixel");
    Arch::new().dispatch(|| linear_to_srgb_u32_scalar(src, dst));
}

// ===========================================================================
// 2. Bulk premultiplied alpha. `0xRRGGBBAA` in place: rgb *= a/255.
// ===========================================================================

/// Scalar reference: premultiply each `0xRRGGBBAA` pixel's RGB by its alpha.
pub fn premultiply_alpha_scalar(pixels: &mut [u32]) {
    for px in pixels.iter_mut() {
        let r = (*px >> 24) & 0xff;
        let g = (*px >> 16) & 0xff;
        let b = (*px >> 8) & 0xff;
        let a = *px & 0xff;
        // Rounded integer premultiply: (c * a + 127) / 255.
        let pm = |c: u32| -> u32 { (c * a + 127) / 255 };
        *px = (pm(r) << 24) | (pm(g) << 16) | (pm(b) << 8) | a;
    }
}

/// Runtime-dispatched in-place premultiplied alpha on `0xRRGGBBAA` pixels.
///
/// Selected at runtime via [`pulp::Arch`]; produces results identical to
/// [`premultiply_alpha_scalar`].
pub fn premultiply_alpha(pixels: &mut [u32]) {
    // SIMD multiply/add happens on f32 lanes; the un/repack is integer and
    // cheap. We expand RGB into an f32 work buffer with a parallel alpha-weight
    // buffer, fuse-multiply on whole registers, then round + repack.
    let n = pixels.len();
    if n == 0 {
        return;
    }
    let mut values = vec![0.0f32; n * 3]; // r,g,b per pixel
    let mut weights = vec![0.0f32; n * 3]; // a/255 broadcast per channel
    for (i, px) in pixels.iter().enumerate() {
        let r = ((*px >> 24) & 0xff) as f32;
        let g = ((*px >> 16) & 0xff) as f32;
        let b = ((*px >> 8) & 0xff) as f32;
        let a = (*px & 0xff) as f32 / 255.0;
        values[i * 3] = r;
        values[i * 3 + 1] = g;
        values[i * 3 + 2] = b;
        weights[i * 3] = a;
        weights[i * 3 + 1] = a;
        weights[i * 3 + 2] = a;
    }

    struct Mul<'a>(&'a mut [f32], &'a [f32]);
    impl<'a> WithSimd for Mul<'a> {
        type Output = ();
        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let (vh, vt) = S::as_mut_simd_f32s(self.0);
            let (wh, wt) = S::as_simd_f32s(self.1);
            for (v, w) in vh.iter_mut().zip(wh) {
                *v = simd.mul_f32s(*v, *w);
            }
            for (v, w) in vt.iter_mut().zip(wt) {
                *v *= *w;
            }
        }
    }
    Arch::new().dispatch(Mul(&mut values, &weights));

    // Round to nearest integer the same way the scalar path does:
    // scalar computes (c*a_int + 127)/255 (integer floor). With a = a_int/255,
    // c*a == c*a_int/255, so (c*a*255 + 127)/255 floored == scalar. To match
    // bit-for-bit we recompute from the float product as floor((prod*255+127)/255)
    // == floor(prod + 0.498...) which is *not* guaranteed identical to integer
    // division. So instead we repack using integer division on the rounded
    // product reconstructed as c*a_int. We carry a_int separately:
    for (i, px) in pixels.iter_mut().enumerate() {
        let a = *px & 0xff;
        let rv = values[i * 3]; // = r * (a/255)
        let gv = values[i * 3 + 1];
        let bv = values[i * 3 + 2];
        // reconstruct exact integer (c*a + 127)/255 from c*a = v*255
        let to_u = |v: f32| -> u32 {
            // v == c * a / 255 ; c*a == v*255 (exact enough; recompute integer)
            let ca = (v * 255.0).round() as u32;
            (ca + 127) / 255
        };
        *px = (to_u(rv) << 24) | (to_u(gv) << 16) | (to_u(bv) << 8) | a;
    }
}

// ===========================================================================
// 3. Batch 2D affine transform.  m = [a, b, c, d, e, f]:
//      x' = a*x + c*y + e
//      y' = b*x + d*y + f
// ===========================================================================

/// Scalar reference: apply affine `m = [a,b,c,d,e,f]` to each point in place.
pub fn transform_points_scalar(points: &mut [(f32, f32)], m: [f32; 6]) {
    let [a, b, c, d, e, f] = m;
    for p in points.iter_mut() {
        let (x, y) = *p;
        *p = (a * x + c * y + e, b * x + d * y + f);
    }
}

struct TransformKernel<'a> {
    xs: &'a mut [f32],
    ys: &'a mut [f32],
    m: [f32; 6],
}

impl<'a> WithSimd for TransformKernel<'a> {
    type Output = ();

    #[inline(always)]
    fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
        let [a, b, c, d, e, f] = self.m;
        let (av, bv, cv, dv, ev, fv) = (
            simd.splat_f32s(a),
            simd.splat_f32s(b),
            simd.splat_f32s(c),
            simd.splat_f32s(d),
            simd.splat_f32s(e),
            simd.splat_f32s(f),
        );

        let (xh, xt) = S::as_mut_simd_f32s(self.xs);
        let (yh, yt) = S::as_mut_simd_f32s(self.ys);

        for (x, y) in xh.iter_mut().zip(yh.iter_mut()) {
            let ox = *x;
            let oy = *y;
            // x' = a*x + c*y + e
            let nx = simd.mul_add_f32s(av, ox, simd.mul_add_f32s(cv, oy, ev));
            // y' = b*x + d*y + f
            let ny = simd.mul_add_f32s(bv, ox, simd.mul_add_f32s(dv, oy, fv));
            *x = nx;
            *y = ny;
        }
        for (x, y) in xt.iter_mut().zip(yt.iter_mut()) {
            let ox = *x;
            let oy = *y;
            *x = a * ox + c * oy + e;
            *y = b * ox + d * oy + f;
        }
    }
}

/// Runtime-dispatched batch 2D affine transform of `points` in place.
///
/// `m = [a, b, c, d, e, f]` with `x' = a*x + c*y + e`, `y' = b*x + d*y + f`.
/// Selected at runtime via [`pulp::Arch`]. Uses fused multiply-add on the SIMD
/// path, so results match [`transform_points_scalar`] to within fp rounding
/// (epsilon-equal, not necessarily bit-equal, when FMA is available).
pub fn transform_points(points: &mut [(f32, f32)], m: [f32; 6]) {
    let n = points.len();
    if n == 0 {
        return;
    }
    // De-interleave (x,y) pairs into planar SoA so whole registers are pure x
    // or pure y — the only layout pulp's f32 lanes can chew on directly.
    let mut xs = vec![0.0f32; n];
    let mut ys = vec![0.0f32; n];
    for (i, p) in points.iter().enumerate() {
        xs[i] = p.0;
        ys[i] = p.1;
    }
    Arch::new().dispatch(TransformKernel {
        xs: &mut xs,
        ys: &mut ys,
        m,
    });
    for (i, p) in points.iter_mut().enumerate() {
        *p = (xs[i], ys[i]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic SplitMix64 -> u32 stream for fixed "random-ish" inputs.
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed)
        }
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn next_u32(&mut self) -> u32 {
            self.next_u64() as u32
        }
        fn next_f32(&mut self) -> f32 {
            // 24-bit mantissa fraction in [0,1)
            (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
        }
    }

    fn pixels(n: usize, seed: u64) -> Vec<u32> {
        let mut r = Rng::new(seed);
        (0..n).map(|_| r.next_u32()).collect()
    }

    #[test]
    fn premultiply_simd_matches_scalar_bitexact() {
        // Integer pipeline -> must be bit-exact.
        for &n in &[0usize, 1, 7, 8, 9, 16, 17, 33, 1000] {
            let src = pixels(n, 0xDEAD_BEEF ^ n as u64);
            let mut a = src.clone();
            let mut b = src.clone();
            premultiply_alpha_scalar(&mut a);
            premultiply_alpha(&mut b);
            assert_eq!(a, b, "premultiply mismatch at n={n}");
        }
    }

    #[test]
    fn srgb_linear_simd_matches_scalar_bitexact() {
        for &n in &[0usize, 1, 5, 8, 31, 256] {
            let src = pixels(n, 0xC0FF_EE00 ^ n as u64);
            let mut a = vec![0.0f32; n * 4];
            let mut b = vec![0.0f32; n * 4];
            srgb_to_linear_u32_scalar(&src, &mut a);
            srgb_to_linear_u32(&src, &mut b);
            assert_eq!(a, b, "srgb->linear mismatch at n={n}");

            // and the inverse, fed the linear output
            let mut da = vec![0u32; n];
            let mut db = vec![0u32; n];
            linear_to_srgb_u32_scalar(&a, &mut da);
            linear_to_srgb_u32(&b, &mut db);
            assert_eq!(da, db, "linear->srgb mismatch at n={n}");
        }
    }

    #[test]
    fn srgb_roundtrip_within_epsilon() {
        // srgb -> linear -> srgb should return the original 8-bit pixel exactly
        // (or within 1 LSB), since both directions share the same curve.
        let src = pixels(2048, 0x1234_5678);
        let mut lin = vec![0.0f32; src.len() * 4];
        srgb_to_linear_u32(&src, &mut lin);
        let mut back = vec![0u32; src.len()];
        linear_to_srgb_u32(&lin, &mut back);

        for (i, (&o, &r)) in src.iter().zip(back.iter()).enumerate() {
            for shift in [24u32, 16, 8, 0] {
                let oc = ((o >> shift) & 0xff) as i32;
                let rc = ((r >> shift) & 0xff) as i32;
                assert!(
                    (oc - rc).abs() <= 1,
                    "roundtrip drift >1 LSB at px {i} shift {shift}: {oc} vs {rc}"
                );
            }
        }
    }

    #[test]
    fn affine_simd_matches_scalar_within_epsilon() {
        let mut r = Rng::new(0xABCD_1234);
        let m = [
            r.next_f32() * 2.0 - 1.0,
            r.next_f32() * 2.0 - 1.0,
            r.next_f32() * 2.0 - 1.0,
            r.next_f32() * 2.0 - 1.0,
            r.next_f32() * 100.0,
            r.next_f32() * 100.0,
        ];
        for &n in &[0usize, 1, 3, 8, 9, 31, 1000] {
            let pts: Vec<(f32, f32)> = (0..n)
                .map(|_| (r.next_f32() * 1000.0, r.next_f32() * 1000.0))
                .collect();
            let mut a = pts.clone();
            let mut b = pts.clone();
            transform_points_scalar(&mut a, m);
            transform_points(&mut b, m);
            for (i, (pa, pb)) in a.iter().zip(b.iter()).enumerate() {
                let eps = 1e-3;
                assert!(
                    (pa.0 - pb.0).abs() <= eps && (pa.1 - pb.1).abs() <= eps,
                    "affine mismatch at n={n} i={i}: {pa:?} vs {pb:?}"
                );
            }
        }
    }

    #[test]
    fn affine_identity_is_noop() {
        let identity = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
        let orig: Vec<(f32, f32)> = vec![(1.0, 2.0), (-3.5, 4.25), (0.0, 0.0), (100.0, -100.0)];
        let mut p = orig.clone();
        transform_points(&mut p, identity);
        assert_eq!(p, orig);
    }

    #[test]
    fn affine_translation_correct() {
        let translate = [1.0, 0.0, 0.0, 1.0, 10.0, -5.0];
        let mut p = vec![(0.0f32, 0.0f32), (1.0, 1.0), (-2.0, 3.0)];
        transform_points(&mut p, translate);
        assert_eq!(p, vec![(10.0, -5.0), (11.0, -4.0), (8.0, -2.0)]);
    }
}
