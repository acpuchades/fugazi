//! Tolerance-aware comparison operators, as bool-output indicators.
//!
//! Each comparison is a [`Combine`] specialised by a value-carrying operator
//! holding an absolute `epsilon` tolerance (default [`DEFAULT_EPSILON`]) so
//! floating-point noise does not cause spurious flips; values within `epsilon`
//! are treated as equal. The six operators are type aliases
//! (`Gt`/`Lt`/`Ge`/`Le`/`Eq`/`Ne`); build them fluently with
//! [`IndicatorExt`](super::IndicatorExt) (`a.gt(b)`, `rsi.above(70.0)`) or
//! explicitly with a custom tolerance via [`Gt::with_epsilon`].
//!
//! Like every [`Indicator`](crate::Indicator), a comparison is `None` until both
//! sources are warmed up (it reads `false` through
//! [`BoolIndicatorExt::is_true`](crate::indicators::BoolIndicatorExt::is_true)).

use std::sync::Arc;

use crate::indicators::ops::{BinaryOp, Combine};
use crate::types::Real;

/// Default absolute tolerance for comparisons.
///
/// An absolute (not relative) epsilon; override per-comparison via
/// [`Gt::with_epsilon`] (and the other aliases) when working at very large or
/// very small scales.
pub const DEFAULT_EPSILON: Real = 1e-8;

/// A tolerance-aware comparison operator. Implement for a value struct carrying
/// its own `epsilon` to define a new operator usable with [`Combine`].
pub trait ComparisonOp {
    /// Build the operator with an explicit absolute tolerance.
    fn with_epsilon(epsilon: Real) -> Self;
}

/// Compare two sources with an explicit absolute tolerance, instead of the
/// [`DEFAULT_EPSILON`] used by `new` and the
/// [`IndicatorExt`](super::IndicatorExt) builders — e.g.
/// `Gt::with_epsilon(a, b, 1e-4)`.
impl<L, R, Op: ComparisonOp + BinaryOp> Combine<L, R, Op> {
    pub fn with_epsilon(lhs: L, rhs: R, epsilon: Real) -> Self {
        Self::with_op(lhs, rhs, Op::with_epsilon(epsilon))
    }
}

/// `lhs > rhs` (beyond `epsilon`).
#[derive(Debug, Clone, Copy)]
pub struct GtOp {
    epsilon: Real,
}
impl Default for GtOp {
    fn default() -> Self {
        Self {
            epsilon: DEFAULT_EPSILON,
        }
    }
}
impl ComparisonOp for GtOp {
    fn with_epsilon(epsilon: Real) -> Self {
        Self { epsilon }
    }
}
impl BinaryOp for GtOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = bool;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<bool> {
        Some(lhs - rhs > self.epsilon)
    }
}

/// `lhs < rhs` (beyond `epsilon`).
#[derive(Debug, Clone, Copy)]
pub struct LtOp {
    epsilon: Real,
}
impl Default for LtOp {
    fn default() -> Self {
        Self {
            epsilon: DEFAULT_EPSILON,
        }
    }
}
impl ComparisonOp for LtOp {
    fn with_epsilon(epsilon: Real) -> Self {
        Self { epsilon }
    }
}
impl BinaryOp for LtOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = bool;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<bool> {
        Some(rhs - lhs > self.epsilon)
    }
}

/// `lhs >= rhs` (within `epsilon`).
#[derive(Debug, Clone, Copy)]
pub struct GeOp {
    epsilon: Real,
}
impl Default for GeOp {
    fn default() -> Self {
        Self {
            epsilon: DEFAULT_EPSILON,
        }
    }
}
impl ComparisonOp for GeOp {
    fn with_epsilon(epsilon: Real) -> Self {
        Self { epsilon }
    }
}
impl BinaryOp for GeOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = bool;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<bool> {
        Some(lhs - rhs >= -self.epsilon)
    }
}

/// `lhs <= rhs` (within `epsilon`).
#[derive(Debug, Clone, Copy)]
pub struct LeOp {
    epsilon: Real,
}
impl Default for LeOp {
    fn default() -> Self {
        Self {
            epsilon: DEFAULT_EPSILON,
        }
    }
}
impl ComparisonOp for LeOp {
    fn with_epsilon(epsilon: Real) -> Self {
        Self { epsilon }
    }
}
impl BinaryOp for LeOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = bool;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<bool> {
        Some(lhs - rhs <= self.epsilon)
    }
}

/// `lhs ≈ rhs` (within `epsilon`).
#[derive(Debug, Clone, Copy)]
pub struct EqOp {
    epsilon: Real,
}
impl Default for EqOp {
    fn default() -> Self {
        Self {
            epsilon: DEFAULT_EPSILON,
        }
    }
}
impl ComparisonOp for EqOp {
    fn with_epsilon(epsilon: Real) -> Self {
        Self { epsilon }
    }
}
impl BinaryOp for EqOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = bool;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<bool> {
        Some((lhs - rhs).abs() <= self.epsilon)
    }
}

/// `lhs != rhs` (beyond `epsilon`).
#[derive(Debug, Clone, Copy)]
pub struct NeOp {
    epsilon: Real,
}
impl Default for NeOp {
    fn default() -> Self {
        Self {
            epsilon: DEFAULT_EPSILON,
        }
    }
}
impl ComparisonOp for NeOp {
    fn with_epsilon(epsilon: Real) -> Self {
        Self { epsilon }
    }
}
impl BinaryOp for NeOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = bool;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<bool> {
        Some((lhs - rhs).abs() > self.epsilon)
    }
}

/// Fires while `lhs` exceeds `rhs` by more than `epsilon`.
pub type Gt<L, R> = Combine<L, R, GtOp>;
/// Fires while `lhs` is below `rhs` by more than `epsilon`.
pub type Lt<L, R> = Combine<L, R, LtOp>;
/// Fires while `lhs` is greater than or within `epsilon` of `rhs`.
pub type Ge<L, R> = Combine<L, R, GeOp>;
/// Fires while `lhs` is less than or within `epsilon` of `rhs`.
pub type Le<L, R> = Combine<L, R, LeOp>;
/// Fires while `lhs` and `rhs` are within `epsilon` of each other.
pub type Eq<L, R> = Combine<L, R, EqOp>;
/// Fires while `lhs` and `rhs` differ by more than `epsilon`.
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
