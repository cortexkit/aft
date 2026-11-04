//! Today's meaning-search arithmetic, reproduced operation for operation,
//! and the int8 approximation with its proven error bound.
//!
//! The exact score of a chunk is what AFT's resident semantic index computes
//! today (`crates/aft/src/semantic_index.rs`, `search_filtered`): the f32 dot
//! product summed left to right, divided by the product of the two f32
//! norms, then the exported-chunk boost. Rust never fuses or reorders float
//! operations on its own, so the same expressions give the same bits on
//! every platform.
//!
//! The approximate score reads the int8 codes. For a stored vector `v` with
//! scale `s` and codes `c`, and a query `q`, the real dot product `q·v`
//! lies within `min(e∞·‖q‖₁, r·‖q‖₂)` of `s·(q·c)`, where `r = ‖v − s·c‖₂`
//! and `e∞ = max |vᵢ − s·cᵢ|` are measured when the vector is written
//! (Cauchy–Schwarz and Hölder's inequality: true for any codes, however they
//! were chosen). Float rounding in both the approximation and today's f32
//! sum is bounded on top of that, so the interval is guaranteed to hold
//! today's computed dot product, not just the real one.

use std::cmp::Ordering;

/// Unit roundoff of f32: every correctly rounded f32 operation is off by at
/// most this share of its result.
const F32_ROUNDOFF: f64 = 1.0 / 16_777_216.0;

/// Absolute error one f32 operation can make when its result underflows into
/// the subnormal range, where relative bounds do not hold: half the spacing of
/// subnormals, 2^-150, doubled for margin.
const F32_SUBNORMAL_STEP: f64 = 1.401_298_464_324_817e-45;

/// Today's L2 norm of a vector, as the resident index caches it.
#[must_use]
pub fn vector_norm(vector: &[f32]) -> f32 {
    vector.iter().map(|value| value * value).sum::<f32>().sqrt()
}

/// Today's dot product: products summed in order, in f32.
#[must_use]
pub fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(a, b)| a * b).sum::<f32>()
}

/// Today's score of a chunk from its dot product with the query and the
/// product of the two norms, with the exported-chunk boost.
///
/// It never decreases as `dot` grows (for a fixed, non-negative `denom`):
/// division by a positive number and both boost factors preserve order, and
/// correct rounding is monotonic. Bounds on the dot product therefore give
/// bounds on the score.
#[must_use]
pub fn score(dot: f32, denom: f32, exported: bool) -> f32 {
    let mut score = if denom == 0.0 { 0.0 } else { dot / denom };
    if exported {
        // Preserve nonnegative weighting; an export bonus must not lower a
        // negative similarity.
        score *= if score >= 0.0 { 1.1 } else { 0.9 };
    }
    score
}

/// Today's ranking order: higher scores first, any non-finite score after
/// every finite one, and equal scores (including +0 and −0) by position.
#[must_use]
pub fn rank_order(a: (f32, u32), b: (f32, u32)) -> Ordering {
    let score_a = if a.0.is_finite() {
        a.0
    } else {
        f32::NEG_INFINITY
    };
    let score_b = if b.0.is_finite() {
        b.0
    } else {
        f32::NEG_INFINITY
    };
    if score_a == score_b {
        a.1.cmp(&b.1)
    } else {
        score_b.total_cmp(&score_a)
    }
}

/// The largest f32 at or below `value`.
pub(crate) fn f32_at_or_below(value: f64) -> f32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "rounding to f32 is the point; the result is corrected below"
    )]
    let rounded = value as f32;
    if f64::from(rounded) > value {
        rounded.next_down()
    } else {
        rounded
    }
}

/// The smallest f32 at or above `value`.
pub(crate) fn f32_at_or_above(value: f64) -> f32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "rounding to f32 is the point; the result is corrected below"
    )]
    let rounded = value as f32;
    if f64::from(rounded) < value {
        rounded.next_up()
    } else {
        rounded
    }
}

/// `n·u / (1 − n·u)`: the classic bound on the relative error of a sum of
/// `n` rounded f32 products, whatever order the additions run in.
fn gamma(terms: usize) -> f64 {
    let nu = terms as f64 * F32_ROUNDOFF;
    nu / (1.0 - nu)
}

/// What the scan needs of a query, computed once per query.
#[derive(Clone, Debug)]
pub(crate) struct QueryTerms {
    /// Today's f32 norm of the query, the first factor of every denominator.
    pub(crate) norm: f32,
    l1: f64,
    l2: f64,
    /// Rounding bound for one sum over the query's dimensions, with margin
    /// for the extra additions of the lane-wise kernel.
    gamma: f64,
    subnormal_slack: f64,
}

impl QueryTerms {
    pub(crate) fn new(query: &[f32]) -> Self {
        let l1 = query
            .iter()
            .map(|value| f64::from(*value).abs())
            .sum::<f64>();
        let l2 = query
            .iter()
            .map(|value| f64::from(*value) * f64::from(*value))
            .sum::<f64>()
            .sqrt();
        Self {
            norm: vector_norm(query),
            l1,
            l2,
            gamma: gamma(query.len() + 2 * LANES),
            subnormal_slack: 4.0 * (query.len() + 2 * LANES) as f64 * F32_SUBNORMAL_STEP,
        }
    }

    /// Bounds on today's f32 dot product of the query with a stored vector,
    /// from the vector's codes.
    ///
    /// `approx` is [`approx_dot`] of the codes; the result is `(low, high)`
    /// with `low ≤ dot_product(query, v) ≤ high`.
    pub(crate) fn dot_bounds(&self, approx: f32, params: &ScanParams) -> (f32, f32) {
        let scale = f64::from(params.scale);
        let center = scale * f64::from(approx);
        // Each of these bounds |q·v − s·(q·c)| on its own.
        let quantization =
            (f64::from(params.max_residual) * self.l1).min(f64::from(params.residual) * self.l2);
        // A bound on ‖v‖₂ from today's f32 norm, whose own rounding is at most
        // a relative γ.
        let vector_l2 = f64::from(params.norm) * (1.0 + 2.0 * self.gamma);
        // Rounding: today's f32 sum is within γ·Σ|qᵢvᵢ| ≤ γ·‖q‖‖v‖ of the
        // real dot product, and the int8 sum within γ·Σ|qᵢ·s·cᵢ| ≤
        // γ·‖q‖(‖v‖ + r) of s·(q·c), since ‖s·c‖ ≤ ‖v‖ + r.
        let rounding = self.gamma * self.l2 * (2.0 * vector_l2 + f64::from(params.residual))
            + self.subnormal_slack * (1.0 + scale);
        // The bound terms are themselves computed in f64; a relative margin
        // far above f64's rounding covers that.
        let error = (quantization + rounding) * (1.0 + 1e-9);
        (
            f32_at_or_above(center - error),
            f32_at_or_below(center + error),
        )
    }
}

/// The per-vector values the scan reads beside the codes.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct ScanParams {
    /// Code `c` stands for `c · scale`.
    pub(crate) scale: f32,
    /// At least `‖v − scale·c‖₂`.
    pub(crate) residual: f32,
    /// Today's f32 norm of the vector, exactly as `vector_norm` computes it.
    pub(crate) norm: f32,
    /// At least `max |vᵢ − scale·cᵢ|`.
    pub(crate) max_residual: f32,
}

/// Width of the lane-wise accumulation in [`approx_dot`]: wide enough for
/// the compiler to use full vector registers.
const LANES: usize = 16;

/// The fastest [`approx_dot`] this CPU runs. AFT's x86-64 builds target the
/// baseline instruction set, whose 4-wide conversions make the int8 scan
/// about three times slower than AVX2's 8-wide ones, so the AVX2 version is
/// chosen at run time where the CPU has it. Both compute the same sum in the
/// same order.
pub(crate) fn approx_kernel() -> fn(&[f32], &[u8]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        return approx_dot_avx2_detected;
    }
    approx_dot
}

/// [`approx_dot`] compiled for AVX2. Only [`approx_kernel`] hands it out,
/// and only after detecting AVX2.
#[cfg(target_arch = "x86_64")]
fn approx_dot_avx2_detected(query: &[f32], codes: &[u8]) -> f32 {
    #[target_feature(enable = "avx2")]
    fn avx2(query: &[f32], codes: &[u8]) -> f32 {
        approx_dot(query, codes)
    }
    // SAFETY: `approx_kernel` returns this function only when the CPU
    // reports AVX2, and nothing else refers to it.
    unsafe { avx2(query, codes) }
}

/// `q·c` for int8 codes, summed in f32 across [`LANES`] independent lanes so
/// the compiler can vectorize it. The order of additions differs from a
/// plain sum; [`QueryTerms::dot_bounds`] bounds the rounding of any order.
#[inline(always)]
pub(crate) fn approx_dot(query: &[f32], codes: &[u8]) -> f32 {
    let mut lanes = [0f32; LANES];
    let (query_chunks, query_rest) = query.as_chunks::<LANES>();
    let (code_chunks, code_rest) = codes.as_chunks::<LANES>();
    for (q, c) in query_chunks.iter().zip(code_chunks) {
        for ((lane, q), c) in lanes.iter_mut().zip(q).zip(c) {
            *lane += q * f32::from(c.cast_signed());
        }
    }
    let mut total = 0f32;
    for lane in lanes {
        total += lane;
    }
    for (q, c) in query_rest.iter().zip(code_rest) {
        total += q * f32::from(c.cast_signed());
    }
    total
}

/// Quantizes one vector: per-vector symmetric int8, `cᵢ = round(vᵢ / s)` with
/// `s = max |vᵢ| / 127`, plus the residuals the error bound needs, rounded
/// up so the bound stays a bound.
pub(crate) fn quantize(vector: &[f32], codes: &mut Vec<u8>) -> ScanParams {
    let peak = vector
        .iter()
        .fold(0f32, |peak, value| peak.max(value.abs()));
    let scale = peak / 127.0;
    let mut squares = 0f64;
    let mut max_residual = 0f64;
    for value in vector {
        let code = if scale > 0.0 {
            (f64::from(*value) / f64::from(scale))
                .round()
                .clamp(-127.0, 127.0)
        } else {
            0.0
        };
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the code is a whole number clamped to -127..=127"
        )]
        let code = code as i8;
        codes.push(code.cast_unsigned());
        // An f32 times a small integer is exact in f64.
        let residual = (f64::from(*value) - f64::from(scale) * f64::from(code)).abs();
        squares += residual * residual;
        max_residual = max_residual.max(residual);
    }
    // f64 sums and roots are off by far less than this margin.
    let margin = 1.0 + 1e-12;
    ScanParams {
        scale,
        residual: f32_at_or_above(squares.sqrt() * margin),
        norm: vector_norm(vector),
        max_residual: f32_at_or_above(max_residual * margin),
    }
}
