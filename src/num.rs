//! `max_finite` / `min_finite` — the two-operand extremes, without the NaN
//! contract nobody in the per-bar path is paying for.
//!
//! # Why this exists
//!
//! `f64::max` is specified to *ignore* NaN: `f64::max(NaN, x) == x`. Honouring
//! that costs a fixup sequence around the hardware instruction. For
//! `(h-l).max((h-pc).abs()).max((l-pc).abs())` — the true range, computed once
//! per bar by `TrueRange`, `Dmi` and everything built on them — rustc emits:
//!
//! ```text
//! subsd  …            movapd %xmm4, %xmm3      andnpd %xmm0, %xmm3
//! andpd  …            cmpunordsd %xmm4, %xmm3  orpd   %xmm6, %xmm3
//! maxsd  …            andpd  %xmm0, %xmm6      …                     22 instrs
//! ```
//!
//! The same expression over `if a > b { a } else { b }` is **10**, two bare
//! `maxsd`s and no fixup — which is what TA-Lib's C gets from its `>`.
//!
//! Measured (`cargo bench --bench icount`, `atr_candle` vs `atr_manual_max`):
//! ATR is **34.0 instructions/bar** with `f64::max` and **25.0** without it.
//! 26% of the whole indicator, for a NaN guarantee on price data.
//!
//! # The contract, precisely
//!
//! These agree with `f64::max`/`f64::min` bit-for-bit on finite operands, with
//! **two documented exceptions**. Both are pinned by tests below, because an
//! unpinned "it's basically the same" is how a silent numeric change ships.
//!
//! **1. NaN propagates instead of being suppressed.** Deliberate: a NaN high or
//! low is corrupt input, and an ATR that quietly reports a plausible number from
//! it is worse than one that reports NaN. `f64::max` hides the bug; this
//! surfaces it.
//!
//! **2. `±0.0` ties resolve to the second operand.** `0.0 > -0.0` is false, so
//! `max_finite(0.0, -0.0)` is `-0.0`.
//!
//! This one turns out **not to be a divergence from any guarantee**, which took
//! two goes to establish. `f64::max`'s own documentation says that when the
//! inputs compare equal — exactly the `±0.0` case — *"either input may be
//! returned non-deterministically"*. And it is: this machine returns `+0.0`,
//! CI's returns `-0.0`, from the same source. So `max_finite` is inside what
//! `f64::max` already permits here, and code that depended on the sign was
//! already broken.
//!
//! It is still worth knowing, because it is the case a call site could care
//! about. At the sites that use these today it cannot fire:
//!
//! * `TrueRange` / `Dmi` — operands are `high - low` (`a - a` is `+0.0`, never
//!   `-0.0`) and two `.abs()` results (`.abs()` never returns `-0.0`).
//! * `Rsi` — `max_finite(delta, 0.0)` and `max_finite(-delta, 0.0)`: whichever
//!   of `±0.0` arrives, the answer is `+0.0` either way.
//! * `Sar` — prices and acceleration factors, none of which reach `-0.0`.
//!
//! So: use these on values that are finite by construction. If you need a
//! defined answer for a `±0.0` tie, neither these *nor* `f64::max` will give
//! you one — write the comparison you actually mean.

use crate::types::Real;

/// The larger of two **finite** operands, by the plain `>` ordering.
///
/// Bit-identical to [`f64::max`] on finite operands, except that a `±0.0` tie
/// returns `b` — where `f64::max` is documented to return either input
/// non-deterministically, so neither is a guarantee. Differs on NaN, which this
/// propagates rather than suppresses. See the module docs.
#[inline(always)]
pub(crate) fn max_finite(a: Real, b: Real) -> Real {
    if a > b { a } else { b }
}

/// The smaller of two **finite** operands. The twin of [`max_finite`].
#[inline(always)]
pub(crate) fn min_finite(a: Real, b: Real) -> Real {
    if a < b { a } else { b }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The load-bearing half of the contract: on everything a price stream can
    /// contain, these *are* `f64::max`/`f64::min`. If this ever fails, every
    /// expected-value fixture in the suite is suspect.
    #[test]
    fn identical_to_std_on_finite_values() {
        // `-0.0` is deliberately absent: it is the one finite input where these
        // diverge, and it has its own test below. Including it here would either
        // fail (as it did, which is how the divergence was found) or force a
        // weaker assertion that hides it.
        let xs = [
            0.0,
            1.0,
            -1.0,
            1e-300,
            -1e-300,
            1e300,
            -1e300,
            f64::MIN_POSITIVE,
            f64::MAX,
            f64::MIN,
            0.1,
            123.456,
            -987.654,
        ];
        for &a in &xs {
            for &b in &xs {
                assert_eq!(
                    max_finite(a, b).to_bits(),
                    a.max(b).to_bits(),
                    "max_finite({a}, {b})"
                );
                assert_eq!(
                    min_finite(a, b).to_bits(),
                    a.min(b).to_bits(),
                    "min_finite({a}, {b})"
                );
            }
        }
    }

    /// Infinities are finite-*ish* for this purpose — they order normally, so
    /// they must also agree. A zero-volume bar can produce one.
    #[test]
    fn infinities_order_the_same_way() {
        for &a in &[f64::INFINITY, f64::NEG_INFINITY, 0.0, 1.0] {
            for &b in &[f64::INFINITY, f64::NEG_INFINITY, 0.0, 1.0] {
                assert_eq!(max_finite(a, b).to_bits(), a.max(b).to_bits(), "{a} {b}");
                assert_eq!(min_finite(a, b).to_bits(), a.min(b).to_bits(), "{a} {b}");
            }
        }
    }

    /// A `±0.0` tie resolves to the second operand — deterministically, which is
    /// more than `f64::max` offers.
    ///
    /// **The std side is deliberately not asserted.** An earlier version of this
    /// test pinned `0.0f64.max(-0.0) == +0.0`, which passed locally and failed
    /// on CI: `f64::max` documents that when the inputs compare equal *"either
    /// input may be returned non-deterministically"*, and two machines disagreed
    /// from the same source. That is the whole finding — there is no guarantee
    /// here to diverge from.
    #[test]
    fn signed_zero_ties_resolve_to_the_second_operand() {
        assert_eq!(max_finite(0.0, -0.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(max_finite(-0.0, 0.0).to_bits(), 0.0f64.to_bits());
        assert_eq!(min_finite(-0.0, 0.0).to_bits(), 0.0f64.to_bits());
        assert_eq!(min_finite(0.0, -0.0).to_bits(), (-0.0f64).to_bits());
    }

    /// Divergence 1, pinned so it is a decision rather than a surprise: NaN
    /// propagates through the second operand instead of being suppressed.
    #[test]
    fn nan_propagates_rather_than_being_suppressed() {
        assert!(max_finite(1.0, f64::NAN).is_nan(), "std would give 1.0");
        assert!(min_finite(1.0, f64::NAN).is_nan(), "std would give 1.0");
        // The other operand order still suppresses, exactly as `maxsd` does and
        // as C's `>` does. Neither order is a guarantee — the point is that a
        // NaN is no longer *guaranteed* to vanish.
        assert_eq!(max_finite(f64::NAN, 1.0), 1.0);
    }
}
