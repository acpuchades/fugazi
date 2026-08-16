//! Tolerance-aware comparison operators, as bool-output indicators.
//!
//! Each comparison is a [`Combine`] specialised by a value-carrying operator
//! holding a [`Tolerance`] (default [`DEFAULT_TOLERANCE`]) so floating-point
//! noise does not cause spurious flips; values inside the tolerance band are
//! treated as equal. The six operators are type aliases
//! (`Gt`/`Lt`/`Ge`/`Le`/`Eq`/`Ne`); build them fluently with
//! [`IndicatorExt`](super::IndicatorExt) (`a.gt(b)`, `rsi.above(70.0)`) or
//! explicitly with a custom tolerance via [`Gt::with_epsilon`] (absolute) or
//! [`Gt::with_tolerance`].
//!
//! # Why the default is relative, not absolute
//!
//! The tolerance was a bare absolute `1e-8`. A comparison's operands, though,
//! can be anything the expression grammar produces, and their scale is unbounded
//! — so one constant meant three different things:
//!
//! | operands | `1e-8` is | effect |
//! |---|---|---|
//! | a five-figure price | `1e-13` relative | **below f64 resolution at that magnitude**, so no noise protection at all |
//! | a stochastic in `[0, 1]` | `1e-8` relative | about right |
//! | a per-bar return `~1e-4` | `1e-4` relative | a coarse deadband that can swallow real signal |
//!
//! At price scale it therefore failed its own stated purpose: two chains that
//! are mathematically equal differ by more than `1e-8` in the last bits, and the
//! comparison flipped on that. [`DEFAULT_TOLERANCE`] is hybrid —
//! `max(abs, rel · max(|lhs|, |rhs|))` — which is scale-free where it needs to
//! be and still defined when both operands sit at zero, which
//! `!gt { lhs: !macd_line, rhs: !value 0 }` requires.
//!
//! Like every [`Indicator`](crate::Indicator), a comparison is `None` until both
//! sources are warmed up (it reads `false` through
//! [`BoolIndicatorExt::is_true`](crate::indicators::BoolIndicatorExt::is_true)).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::indicators::ops::{BinaryOp, Combine};
use crate::types::Real;

/// The deadband inside which a comparison treats two values as equal.
///
/// The band for a given pair is `max(abs, rel · max(|lhs|, |rhs|))`, so the
/// **absolute** term governs near zero and the **relative** term takes over as
/// the operands grow. Either can be zero for a purely relative or purely
/// absolute tolerance.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tolerance {
    /// Absolute floor, in the operands' own units. Governs near zero.
    pub abs: Real,
    /// Relative term, as a fraction of the larger operand's magnitude.
    pub rel: Real,
}

impl Tolerance {
    /// A hybrid tolerance from both terms.
    pub const fn new(abs: Real, rel: Real) -> Self {
        Self { abs, rel }
    }

    /// A purely **absolute** tolerance, in the operands' own units — what
    /// [`Combine::with_epsilon`] builds. Use it when the deadband is a quantity
    /// you mean literally ("ignore moves under one tick"), not a float-noise
    /// guard.
    pub const fn absolute(abs: Real) -> Self {
        Self { abs, rel: 0.0 }
    }

    /// A purely **relative** tolerance. Note this collapses to zero when both
    /// operands are zero, so prefer [`new`](Self::new) with a small `abs` floor
    /// for anything compared against a literal `0`.
    pub const fn relative(rel: Real) -> Self {
        Self { abs: 0.0, rel }
    }

    /// The deadband half-width for this pair of operands.
    pub fn band(&self, lhs: Real, rhs: Real) -> Real {
        self.abs.max(self.rel * lhs.abs().max(rhs.abs()))
    }
}

impl Default for Tolerance {
    fn default() -> Self {
        DEFAULT_TOLERANCE
    }
}

/// Default comparison tolerance: an absolute floor of `1e-12` with a relative
/// term of `1e-9`.
///
/// Sized to sit well above accumulated float noise (an f64 carries ~`1e-16`
/// relative, and a deep smoothing chain loses a few orders of that) and well
/// below any economically meaningful move — at a five-figure price the band is
/// `1e-4`, four orders under the smallest real tick. Override per-comparison via
/// [`Combine::with_epsilon`] / [`Combine::with_tolerance`] when you want a
/// deadband you mean literally.
pub const DEFAULT_TOLERANCE: Tolerance = Tolerance::new(1e-12, 1e-9);

/// A tolerance-aware comparison operator. Implement for a value struct carrying
/// its own [`Tolerance`] to define a new operator usable with [`Combine`].
pub trait ComparisonOp {
    /// Build the operator with an explicit tolerance.
    fn with_tolerance(tolerance: Tolerance) -> Self;
}

/// Compare two sources with an explicit tolerance instead of the
/// [`DEFAULT_TOLERANCE`] used by `new` and the
/// [`IndicatorExt`](super::IndicatorExt) builders.
impl<L, R, Op: ComparisonOp + BinaryOp> Combine<L, R, Op> {
    /// With an explicit **absolute** tolerance — e.g.
    /// `Gt::with_epsilon(a, b, 1e-4)`. Equivalent to
    /// [`with_tolerance`](Self::with_tolerance) with [`Tolerance::absolute`].
    pub fn with_epsilon(lhs: L, rhs: R, epsilon: Real) -> Self {
        Self::with_tolerance(lhs, rhs, Tolerance::absolute(epsilon))
    }

    /// With an explicit [`Tolerance`], absolute and/or relative.
    pub fn with_tolerance(lhs: L, rhs: R, tolerance: Tolerance) -> Self {
        Self::with_op(lhs, rhs, Op::with_tolerance(tolerance))
    }
}

/// `lhs > rhs` (beyond the tolerance band).
#[derive(Debug, Clone, Copy, Default)]
pub struct GtOp {
    tolerance: Tolerance,
}
impl ComparisonOp for GtOp {
    fn with_tolerance(tolerance: Tolerance) -> Self {
        Self { tolerance }
    }
}
impl BinaryOp for GtOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = bool;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<bool> {
        Some(lhs - rhs > self.tolerance.band(lhs, rhs))
    }
}

/// `lhs < rhs` (beyond the tolerance band).
#[derive(Debug, Clone, Copy, Default)]
pub struct LtOp {
    tolerance: Tolerance,
}
impl ComparisonOp for LtOp {
    fn with_tolerance(tolerance: Tolerance) -> Self {
        Self { tolerance }
    }
}
impl BinaryOp for LtOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = bool;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<bool> {
        Some(rhs - lhs > self.tolerance.band(lhs, rhs))
    }
}

/// `lhs >= rhs` (within the tolerance band).
#[derive(Debug, Clone, Copy, Default)]
pub struct GeOp {
    tolerance: Tolerance,
}
impl ComparisonOp for GeOp {
    fn with_tolerance(tolerance: Tolerance) -> Self {
        Self { tolerance }
    }
}
impl BinaryOp for GeOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = bool;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<bool> {
        Some(lhs - rhs >= -self.tolerance.band(lhs, rhs))
    }
}

/// `lhs <= rhs` (within the tolerance band).
#[derive(Debug, Clone, Copy, Default)]
pub struct LeOp {
    tolerance: Tolerance,
}
impl ComparisonOp for LeOp {
    fn with_tolerance(tolerance: Tolerance) -> Self {
        Self { tolerance }
    }
}
impl BinaryOp for LeOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = bool;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<bool> {
        Some(lhs - rhs <= self.tolerance.band(lhs, rhs))
    }
}

/// `lhs ≈ rhs` (within the tolerance band).
#[derive(Debug, Clone, Copy, Default)]
pub struct EqOp {
    tolerance: Tolerance,
}
impl ComparisonOp for EqOp {
    fn with_tolerance(tolerance: Tolerance) -> Self {
        Self { tolerance }
    }
}
impl BinaryOp for EqOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = bool;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<bool> {
        Some((lhs - rhs).abs() <= self.tolerance.band(lhs, rhs))
    }
}

/// `lhs != rhs` (beyond the tolerance band).
#[derive(Debug, Clone, Copy, Default)]
pub struct NeOp {
    tolerance: Tolerance,
}
impl ComparisonOp for NeOp {
    fn with_tolerance(tolerance: Tolerance) -> Self {
        Self { tolerance }
    }
}
impl BinaryOp for NeOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = bool;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<bool> {
        Some((lhs - rhs).abs() > self.tolerance.band(lhs, rhs))
    }
}

/// Fires while `lhs` exceeds `rhs` by more than the tolerance band.
pub type Gt<L, R> = Combine<L, R, GtOp>;
/// Fires while `lhs` is below `rhs` by more than the tolerance band.
pub type Lt<L, R> = Combine<L, R, LtOp>;
/// Fires while `lhs` is greater than, or within the tolerance band of, `rhs`.
pub type Ge<L, R> = Combine<L, R, GeOp>;
/// Fires while `lhs` is less than, or within the tolerance band of, `rhs`.
pub type Le<L, R> = Combine<L, R, LeOp>;
/// Fires while `lhs` and `rhs` are within the tolerance band of each other.
pub type Eq<L, R> = Combine<L, R, EqOp>;
/// Fires while `lhs` and `rhs` differ by more than the tolerance band.
pub type Ne<L, R> = Combine<L, R, NeOp>;

// ---------------------------------------------------------------------------
// String equality
// ---------------------------------------------------------------------------

/// `lhs == rhs` on two `Arc<str>` sources. No epsilon — equality is bytewise.
#[derive(Debug, Clone, Copy, Default)]
pub struct StrEqOp;

impl BinaryOp for StrEqOp {
    type Lhs = Arc<str>;
    type Rhs = Arc<str>;
    type Output = bool;
    fn apply(&self, lhs: Arc<str>, rhs: Arc<str>) -> Option<bool> {
        Some(lhs.as_ref() == rhs.as_ref())
    }
}

/// `lhs != rhs` on two `Arc<str>` sources. No epsilon — equality is bytewise.
#[derive(Debug, Clone, Copy, Default)]
pub struct StrNeOp;

impl BinaryOp for StrNeOp {
    type Lhs = Arc<str>;
    type Rhs = Arc<str>;
    type Output = bool;
    fn apply(&self, lhs: Arc<str>, rhs: Arc<str>) -> Option<bool> {
        Some(lhs.as_ref() != rhs.as_ref())
    }
}

/// Fires while `lhs` and `rhs` (both `Arc<str>` sources) are byte-equal.
pub type StrEq<L, R> = Combine<L, R, StrEqOp>;
/// Fires while `lhs` and `rhs` (both `Arc<str>` sources) differ.
pub type StrNe<L, R> = Combine<L, R, StrNeOp>;

#[cfg(test)]
mod str_tests {
    use super::*;
    use crate::indicator::Indicator;
    use crate::indicators::value::ValueStr;
    use crate::types::Atom;

    #[test]
    fn str_eq_fires_on_match() {
        let atom = Atom::new(crate::types::Candle::new(1.0, 2.0, 0.5, 1.5, 10.0));
        let lhs: ValueStr<Atom> = ValueStr::new("bull");
        let rhs: ValueStr<Atom> = ValueStr::new("bull");
        let mut cmp: StrEq<ValueStr<Atom>, ValueStr<Atom>> = Combine::new(lhs, rhs);
        assert_eq!(cmp.update(atom), Some(true));
    }

    #[test]
    fn str_eq_false_on_mismatch() {
        let atom = Atom::new(crate::types::Candle::new(1.0, 2.0, 0.5, 1.5, 10.0));
        let lhs: ValueStr<Atom> = ValueStr::new("bull");
        let rhs: ValueStr<Atom> = ValueStr::new("bear");
        let mut cmp: StrEq<ValueStr<Atom>, ValueStr<Atom>> = Combine::new(lhs, rhs);
        assert_eq!(cmp.update(atom), Some(false));
    }

    #[test]
    fn str_ne_inverts_str_eq() {
        let atom = Atom::new(crate::types::Candle::new(1.0, 2.0, 0.5, 1.5, 10.0));
        let lhs: ValueStr<Atom> = ValueStr::new("bear");
        let rhs: ValueStr<Atom> = ValueStr::new("bull");
        let mut cmp: StrNe<ValueStr<Atom>, ValueStr<Atom>> = Combine::new(lhs, rhs);
        assert_eq!(cmp.update(atom), Some(true));
    }
}

#[cfg(test)]
mod tolerance_tests {
    use super::*;
    use crate::indicator::Indicator;
    use crate::indicators::{Identity, Value};

    /// What the old absolute default was, so the regression cases below can say
    /// *why* they are the cases they are.
    const LEGACY_ABSOLUTE_EPSILON: Real = 1e-8;

    fn gt(lhs: Real, rhs: Real) -> bool {
        let mut c: Gt<Identity<Real>, Value<Real>> = Combine::new(Identity::new(), Value::new(rhs));
        c.update(lhs).expect("both operands are ready")
    }

    fn eq(lhs: Real, rhs: Real) -> bool {
        let mut c: Eq<Identity<Real>, Value<Real>> = Combine::new(Identity::new(), Value::new(rhs));
        c.update(lhs).expect("both operands are ready")
    }

    #[test]
    fn the_band_is_the_larger_of_the_absolute_and_relative_terms() {
        let t = Tolerance::new(1e-12, 1e-9);
        // Near zero the absolute floor governs…
        assert_eq!(t.band(0.0, 0.0), 1e-12);
        // …and the relative term takes over once the operands grow.
        assert_eq!(t.band(1e5, 0.0), 1e-4);
        // It reads the *larger* operand, so an operand of zero on one side does
        // not collapse the band.
        assert_eq!(t.band(0.0, 1e5), 1e-4);
        // The degenerate constructors do what they say.
        assert_eq!(Tolerance::absolute(1e-3).band(1e9, 0.0), 1e-3);
        assert_eq!(Tolerance::relative(1e-6).band(2.0, 0.0), 2e-6);
    }

    /// **The defect.** At a five-figure price the old `1e-8` absolute band was
    /// `1e-13` relative — below what an f64 can even represent there, so it gave
    /// no noise protection at all and two mathematically-equal chains flipped
    /// the comparison on their last bits. The hybrid default covers it.
    #[test]
    fn float_noise_at_price_scale_no_longer_flips_a_comparison() {
        let price = 87_431.25_f64;
        // A difference of the size an f64 accumulates through a smoothing chain
        // at this magnitude — around 1e-11 relative.
        let noise = price * 1e-11;
        assert!(
            noise > LEGACY_ABSOLUTE_EPSILON,
            "precondition: this noise used to exceed the old band ({noise} vs \
             {LEGACY_ABSOLUTE_EPSILON}), which is why the comparison flipped"
        );
        assert!(
            !gt(price + noise, price),
            "float noise at price scale must not read as a genuine excess"
        );
        assert!(eq(price + noise, price), "…and must read as equal");
    }

    /// The other limb: the band scales *down* as well as up, so a difference
    /// that is large in relative terms registers even when both operands are
    /// tiny — where the old `1e-8` floor swallowed everything.
    ///
    /// Operands this small are uncommon in market data, so this is the
    /// theoretical half of the fix; the price-scale case above is the one that
    /// actually bit. Pinned anyway, because it is the same property read from
    /// the other end and a future retune could break it without touching the
    /// case above.
    #[test]
    fn the_band_scales_down_for_small_operands_too() {
        let (a, b) = (1.2e-8, 1.1e-8);
        assert!(
            a - b < LEGACY_ABSOLUTE_EPSILON,
            "precondition: this difference sat under the old absolute floor"
        );
        assert!(
            DEFAULT_TOLERANCE.band(a, b) < LEGACY_ABSOLUTE_EPSILON,
            "the hybrid band must be tighter than the old floor down here"
        );
        assert!(
            gt(a, b),
            "a 9% relative difference must register, whatever the magnitude"
        );
    }

    /// The property that makes the default defensible: a comparison means the
    /// same thing whatever units its operands happen to be in. The old absolute
    /// band failed this by construction.
    #[test]
    fn a_comparison_is_scale_invariant() {
        for scale in [1e-6, 1e-3, 1.0, 1e3, 1e6, 1e9] {
            assert!(gt(1.01 * scale, 1.0 * scale), "1% excess at scale {scale}");
            assert!(!gt(1.0 * scale, 1.0 * scale), "equality at scale {scale}");
            assert!(eq(1.0 * scale, 1.0 * scale), "equality at scale {scale}");
        }
    }

    /// A relative-only tolerance would collapse to zero here, which is why the
    /// default keeps an absolute floor: comparing an oscillator against a
    /// literal `0` is the single most common signal in the grammar
    /// (`!gt { lhs: !macd_line, rhs: !value 0 }`).
    #[test]
    fn comparing_against_a_literal_zero_still_has_a_band() {
        assert!(Tolerance::relative(1e-9).band(0.0, 0.0) == 0.0, "precondition");
        assert!(DEFAULT_TOLERANCE.band(0.0, 0.0) > 0.0);
        assert!(!gt(1e-15, 0.0), "sub-floor noise is not a positive reading");
        assert!(gt(1e-6, 0.0), "a real positive value still reads as one");
    }

    /// `with_epsilon` keeps meaning an **absolute** deadband — the caller is
    /// stating a quantity they mean literally ("ignore moves under a tick"),
    /// which must not be rescaled by the operands' magnitude.
    #[test]
    fn with_epsilon_stays_absolute() {
        let mut c: Gt<Identity<Real>, Value<Real>> =
            Combine::with_epsilon(Identity::new(), Value::new(100.0), 0.5);
        assert_eq!(c.update(100.4), Some(false), "inside the stated deadband");
        assert_eq!(c.update(100.6), Some(true), "beyond it");

        // Same absolute deadband, operands a million times larger: still 0.5.
        let mut big: Gt<Identity<Real>, Value<Real>> =
            Combine::with_epsilon(Identity::new(), Value::new(1e8), 0.5);
        assert_eq!(big.update(1e8 + 0.4), Some(false));
        assert_eq!(big.update(1e8 + 0.6), Some(true));
    }

    /// Every operator reads the same band, so the six stay mutually consistent:
    /// inside it `ge`/`le`/`eq` hold and `gt`/`lt`/`ne` do not.
    #[test]
    fn the_six_operators_agree_on_the_band() {
        let price = 50_000.0;
        let inside = price + DEFAULT_TOLERANCE.band(price, price) * 0.5;

        let mut g: Gt<Identity<Real>, Value<Real>> = Combine::new(Identity::new(), Value::new(price));
        let mut l: Lt<Identity<Real>, Value<Real>> = Combine::new(Identity::new(), Value::new(price));
        let mut ge: Ge<Identity<Real>, Value<Real>> = Combine::new(Identity::new(), Value::new(price));
        let mut le: Le<Identity<Real>, Value<Real>> = Combine::new(Identity::new(), Value::new(price));
        let mut e: Eq<Identity<Real>, Value<Real>> = Combine::new(Identity::new(), Value::new(price));
        let mut n: Ne<Identity<Real>, Value<Real>> = Combine::new(Identity::new(), Value::new(price));

        assert_eq!(g.update(inside), Some(false));
        assert_eq!(l.update(inside), Some(false));
        assert_eq!(ge.update(inside), Some(true));
        assert_eq!(le.update(inside), Some(true));
        assert_eq!(e.update(inside), Some(true));
        assert_eq!(n.update(inside), Some(false));
    }

    /// A crossover is a comparison plus an edge, so it inherits the band — and
    /// must not fire on noise at price scale, which is precisely where a
    /// spurious crossover would have cost real money.
    #[test]
    fn a_crossover_does_not_fire_on_price_scale_noise() {
        use crate::indicators::CrossesAbove;
        let price = 87_431.25_f64;
        let noise = price * 1e-11;

        let mut cross: CrossesAbove<Identity<Real>, Value<Real>> =
            CrossesAbove::new(Identity::new(), Value::new(price));
        // Sit just below, then wobble to just above by less than the band.
        cross.update(price - noise);
        let fired = (0..8).any(|i| {
            let x = if i % 2 == 0 { price + noise } else { price - noise };
            cross.update(x).unwrap_or(false)
        });
        assert!(!fired, "noise below the band must not look like a crossing");

        // A genuine move across still fires.
        assert_eq!(cross.update(price * 1.001), Some(true));
    }
}
