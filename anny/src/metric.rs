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
    for l in 0..8 {
        s = reduce(s, acc[l]);
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
