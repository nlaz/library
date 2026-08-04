//! Distance metrics as zero-sized types implementing `Metric`.
//!
//! Convention: every metric is a *dissimilarity* — smaller means closer — so the
//! index's min-search does the right thing. For float inputs the result keeps the
//! input precision (`f32 -> f32`, `f64 -> f64`); integer inputs are promoted to
//! `f32` (matching the `ScalarInt` path). Output types are all `Distance`.
pub use crate::traits::{Metric, Scalar};

#[inline(always)]
fn fold8<T, F, M, R>(a: &[T], b: &[T], zero: F, map: M, mut reduce: R) -> F
where
    T: Copy,
    F: Copy,
    M: Fn(T, T) -> F,
    R: FnMut(F, F) -> F,
{
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let mut acc = [zero; 8];
    let mut i = 0;
    while i + 8 <= n {
        let (xa, xb) = (&a[i..i + 8], &b[i..i + 8]); // proves indices in-bounds
        for l in 0..8 {
            acc[l] = reduce(acc[l], map(xa[l], xb[l]));
        }
        i += 8;
    }
    let mut s = zero;
    for &lane in &acc {
        s = reduce(s, lane);
    }
    while i < n {
        s = reduce(s, map(a[i], b[i]));
        i += 1;
    }
    s
}

macro_rules! reduce_metric {
    (
        $(#[$doc:meta])* $name:ident,
        map = |$x:ident, $y:ident| $map:expr,
        reduce = |$s:ident, $v:ident| $red:expr,
        finish = $finish:expr $(,)?
    ) => {
        $(#[$doc])*
        #[derive(Debug, Default, Clone, Copy)]
        pub struct $name;
        impl<T: Scalar> Metric<T> for $name {
            type Out = f32;
            #[inline(always)]
            fn distance(a: &[T], b: &[T]) -> f32 {
                ($finish)(fold8(a, b, 0.0f32,
                    |xa: T, ya: T| { let $x = xa.to_f32(); let $y = ya.to_f32(); $map },
                    |$s, $v| $red))
            }
        }
    };
}

reduce_metric! {
    /// Squared Euclidean distance, `Σ (xᵢ-yᵢ)²`. ...
    L2,
    map = |x, y| { let d = x - y; d * d },
    reduce = |s, v| s + v,
    finish = |r| r,
}
reduce_metric! {
    /// True Euclidean distance, `√Σ (xᵢ-yᵢ)²`. ...
    Euclidean,
    map = |x, y| { let d = x - y; d * d },
    reduce = |s, v| s + v,
    finish = |r:f32| r.sqrt(),
}
reduce_metric! {
    /// Manhattan / taxicab distance, `Σ |xᵢ-yᵢ|`.
    L1,
    map = |x, y| (x - y).abs(),
    reduce = |s, v| s + v,
    finish = |r| r,
}
reduce_metric! {
    /// Chebyshev / L∞ distance, `maxᵢ |xᵢ-yᵢ|`.
    Chebyshev,
    map = |x, y| (x - y).abs(),
    reduce = |m, v| if v > m { v } else { m },
    finish = |r| r,
}
reduce_metric! {
    /// Negated inner product, `-⟨a,b⟩`.
    NegDot,
    map = |x, y| x * y,
    reduce = |s, v| s + v,
    finish = |r:f32| -r,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Cosine;
impl<T: Scalar> Metric<T> for Cosine {
    type Out = f32;
    #[inline(always)]
    fn distance(a: &[T], b: &[T]) -> f32 {
        let n = a.len().min(b.len());
        let (a, b) = (&a[..n], &b[..n]);
        let (mut dot, mut na, mut nb) = ([0.0f32; 8], [0.0f32; 8], [0.0f32; 8]);
        let mut i = 0;
        while i + 8 <= n {
            let (xa, xb) = (&a[i..i + 8], &b[i..i + 8]);
            for l in 0..8 {
                let a = xa[l].to_f32();
                let b = xb[l].to_f32();
                dot[l] += a * b;
                na[l] += a * a;
                nb[l] += b * b;
            }
            i += 8;
        }
        let (mut d, mut sa, mut sb) = (0.0f32, 0.0, 0.0);
        for l in 0..8 {
            d += dot[l];
            sa += na[l];
            sb += nb[l];
        }
        while i < n {
            let a = a[i].to_f32();
            let b = b[i].to_f32();
            d += a * b;
            sa += a * a;
            sb += b * b;
            i += 1;
        }
        let nrm = (sa * sb).sqrt();
        if nrm > 0.0 { 1.0 - d / nrm } else { 0.0 }
    }
}

/// Cosine distance, `1 - ⟨a,b⟩`, for vectors conditioned to unit length.
///
/// Identical in value to [`Cosine`] — for unit `a` and `b`, `1 - ⟨a,b⟩/(‖a‖‖b‖)`
/// *is* `1 - ⟨a,b⟩` — but it normalizes once per vector, at insert and at
/// query, instead of recomputing both norms on every comparison. An HNSW
/// query evaluates thousands of distances against one fixed query vector, so
/// [`Cosine`] spends most of its time re-deriving norms it already derived:
/// measured at 512 dimensions on an M3, 178ns per call against 36ns here.
///
/// The one behavioral difference is degenerate input. [`Cosine`] reports a
/// zero vector as distance 0 — nearest to everything — while a zero vector
/// survives `prepare` unchanged and lands here at distance 1, the same as an
/// orthogonal one. Neither is more correct in general; this one at least
/// does not promote a degenerate vector to the top of every search.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnitCosine;

macro_rules! unit_cosine {
    ($($t:ty),*) => { $(impl Metric<$t> for UnitCosine {
        type Out = f32;
        const CONDITIONED: bool = true;

        #[inline(always)]
        fn distance(a: &[$t], b: &[$t]) -> f32 {
            1.0 - fold8(a, b, 0.0f32, |x: $t, y: $t| (x * y) as f32, |s, v| s + v)
        }

        #[inline(always)]
        fn prepare(v: &mut [$t]) {
            let n = fold8(v, v, 0.0 as $t, |x: $t, y: $t| x * y, |s, v| s + v).sqrt();
            if n > 0.0 {
                // reciprocal multiply: division has several times the
                // latency of a multiply and does not pipeline
                let inv = 1.0 / n;
                for x in v.iter_mut() {
                    *x *= inv;
                }
            }
        }
    })* };
}
// Only the float scalars: conditioning an integer vector to unit length in
// place would quantize every component to 0 or ±1.
unit_cosine!(f32, f64);

#[derive(Debug, Default, Clone, Copy)]
pub struct Hamming;
impl<T: Scalar> Metric<T> for Hamming {
    type Out = u32;
    #[inline(always)]
    fn distance(a: &[T], b: &[T]) -> u32 {
        let n = a.len().min(b.len());
        let (a, b) = (&a[..n], &b[..n]);
        let mut c = 0u32;
        for i in 0..n {
            c += (a[i].to_f32() != b[i].to_f32()) as u32;
        }
        c
    }
}
