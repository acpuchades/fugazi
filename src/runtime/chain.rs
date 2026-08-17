//! Domain-typed erasure: [`DynIndicator`], [`Chain`] and [`AnyChain`].
//!
//! This is the vocabulary a runtime-driven builder (the YAML/spec layer, the
//! Python bindings) uses to hold an indicator whose *concrete* type it learns
//! only at run time — but whose **domain** it knows the moment the node is
//! built.
//!
//! # Why this exists next to [`PayloadIndicator`](super::PayloadIndicator)
//!
//! The older vocabulary is *self-describing*: a node exchanges
//! [`PayloadValue`](super::PayloadValue) and reports its own
//! `input_type()`/`output_type()`, so a builder can ask what it produced. That
//! flexibility costs a payload enum as wide as its widest variant — 88 bytes —
//! moved in and back out at **every level** of an expression, with a
//! discriminant branch and drop glue on each move.
//!
//! Measured (`cargo bench --bench erasure`), on a two-node scalar chain:
//!
//! | | ns/sample |
//! |---|---:|
//! | concrete, no erasure | 1.36 |
//! | payload erasure, 2 levels | 16.00 |
//! | payload erasure, 3 levels | 28.89 |
//! | **this vocabulary, 2 levels** | **3.79** |
//! | **this vocabulary, 3 levels** | **4.20** |
//!
//! Marginal cost of one more level: **+12.9 ns** with a payload, **+0.4 ns**
//! here. Depth stops being a tax, which matters because real expressions are
//! deep.
//!
//! The trade is that a `Chain` cannot describe itself — its domain lives in the
//! type, so a builder that needs to *discover* the domain matches on
//! [`AnyChain`] instead of reading a tag. In practice that is what the builders
//! already do at every coercion point, so nothing is lost; see
//! [`AnyChain::into_real`] and friends.

use std::sync::Arc;

use crate::indicator::Indicator;
use crate::market::{Atom, Candle, Real};
use crate::snapshot::{Snapshot, Symbol};
use crate::time::Timestamp;

/// An [`Indicator`] with its concrete type erased but its **domain kept in the
/// type**: `In` in, `Out` out.
///
/// The same role [`PayloadIndicator`](super::PayloadIndicator) plays, minus the
/// payload enum. `dyn_clone` is here for the same reason it is there — several
/// concrete indicators clone their own source internally (`Hma`, the
/// `crosses_above` pair, the multi-output component accessors), so an erased
/// indicator has to be duplicable to take part in composition at all.
pub trait DynIndicator<In, Out>: Indicator<Input = In, Output = Out> + Send + Sync {
    /// Deep-clone behind the trait object.
    fn dyn_clone(&self) -> Box<dyn DynIndicator<In, Out>>;
}

/// Wraps a concrete [`Indicator`] so it can be held as a [`Chain`].
///
/// Deliberately an explicit adapter rather than a blanket
/// `impl<T: Indicator + Clone> DynIndicator for T`: a blanket impl shadows the
/// compiler's automatic `impl Trait for dyn Trait`, which is what
/// `Clone for Chain` needs in order to dispatch `dyn_clone` through the vtable.
/// The retiring `Adapter` sidesteps the same problem the same way.
#[derive(Debug, Clone)]
pub struct Erased<I>(I);

impl<I> Erased<I> {
    pub fn new(inner: I) -> Self {
        Self(inner)
    }
}

impl<I: Indicator> Indicator for Erased<I> {
    type Input = I::Input;
    type Output = I::Output;
    fn update(&mut self, input: I::Input) -> Option<I::Output> {
        self.0.update(input)
    }
    fn value(&self) -> Option<I::Output> {
        self.0.value()
    }
    fn warm_up_bars(&self) -> usize {
        self.0.warm_up_bars()
    }
    fn unstable_bars(&self) -> usize {
        self.0.unstable_bars()
    }
    fn reset(&mut self) {
        self.0.reset();
    }
    fn save_state(&self) -> serde_json::Value {
        self.0.save_state()
    }
    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.0.load_state(state)
    }
}

impl<In, Out, I> DynIndicator<In, Out> for Erased<I>
where
    I: Indicator<Input = In, Output = Out> + Clone + Send + Sync + 'static,
    In: 'static,
    Out: Clone + 'static,
{
    fn dyn_clone(&self) -> Box<dyn DynIndicator<In, Out>> {
        Box::new(self.clone())
    }
}

/// A boxed, domain-typed indicator chain — the handle a runtime builder holds.
///
/// Generalises the per-shape aliases the strategy layer already used
/// (`strategies::Chain<Sym>`, `multi_asset::SignalChain<Sym>`): those were this
/// type, written twice, for two of its domains.
pub type Chain<In, Out> = Box<dyn DynIndicator<In, Out>>;

impl<In: 'static, Out: Clone + 'static> Clone for Box<dyn DynIndicator<In, Out>> {
    fn clone(&self) -> Self {
        (**self).dyn_clone()
    }
}

/// A chain over a per-bar snapshot producing a real number — by far the common
/// case, and what every arithmetic/statistical node is.
pub type RealChain = Chain<Snapshot<Symbol>, Real>;
/// A snapshot-rooted boolean chain: a signal.
pub type BoolChain = Chain<Snapshot<Symbol>, bool>;
/// A snapshot-rooted chain projecting one asset's bar.
pub type AtomChain = Chain<Snapshot<Symbol>, Atom>;
/// A snapshot-rooted chain producing a whole candle (a resampler).
pub type CandleChain = Chain<Snapshot<Symbol>, Candle>;
/// A snapshot-rooted chain producing a categorical label.
pub type StrChain = Chain<Snapshot<Symbol>, Arc<str>>;
/// A snapshot-rooted chain producing a timestamp (the calendar leaves).
pub type TimeChain = Chain<Snapshot<Symbol>, Timestamp>;

/// A built expression node, tagged with the domain it produces.
///
/// What a spec node builds to. The discriminant carries what
/// `PayloadIndicator::output_type()` used to report at run time, so a slot that
/// needs a particular domain matches instead of comparing tags — see
/// [`into_real`](Self::into_real).
///
/// Named for the `Any*` idiom the Python carriers already use (`AnySource`,
/// `AnySignal`, `AnyMulti`): a sum over the domains one position can hold.
pub enum AnyChain {
    Real(RealChain),
    Bool(BoolChain),
    Atom(AtomChain),
    Candle(CandleChain),
    Str(StrChain),
    Time(TimeChain),
}

impl AnyChain {
    /// The domain this node produces, for diagnostics. Matches the spelling
    /// `PayloadType`'s `Display` used, so error messages are unchanged.
    pub fn kind(&self) -> &'static str {
        match self {
            AnyChain::Real(_) => "Real",
            AnyChain::Bool(_) => "Bool",
            AnyChain::Atom(_) => "Atom",
            AnyChain::Candle(_) => "Candle",
            AnyChain::Str(_) => "Str",
            AnyChain::Time(_) => "Time",
        }
    }
}

/// Per-domain construction, so a builder can hand over any concrete indicator
/// without naming its output type.
///
/// This is what keeps the ~150 build sites mechanical: `any(Sma::new(..))`
/// works out the variant from the indicator's `Output`. A blanket
/// `impl<I: Indicator> From<I> for AnyChain` cannot do that — the compiler
/// cannot see that `Output = Real` and `Output = bool` are disjoint — so the
/// dispatch hangs off the *output type* instead, where each impl is
/// unambiguous.
pub trait ChainDomain: Sized {
    /// Box `inner` into the [`AnyChain`] variant for this output type.
    fn into_any_chain<I>(inner: I) -> AnyChain
    where
        I: Indicator<Input = Snapshot<Symbol>, Output = Self> + Clone + Send + Sync + 'static;
}

macro_rules! chain_domain {
    ($ty:ty => $variant:ident) => {
        impl ChainDomain for $ty {
            fn into_any_chain<I>(inner: I) -> AnyChain
            where
                I: Indicator<Input = Snapshot<Symbol>, Output = Self>
                    + Clone
                    + Send
                    + Sync
                    + 'static,
            {
                AnyChain::$variant(Box::new(Erased::new(inner)))
            }
        }
    };
}

chain_domain!(Real => Real);
chain_domain!(bool => Bool);
chain_domain!(Atom => Atom);
chain_domain!(Candle => Candle);
chain_domain!(Arc<str> => Str);
chain_domain!(Timestamp => Time);

/// Erase `inner` into an [`AnyChain`], picking the variant from its output type.
///
/// The replacement for `dyn_indicator::wrap`.
pub fn any<I, Out>(inner: I) -> AnyChain
where
    Out: ChainDomain,
    I: Indicator<Input = Snapshot<Symbol>, Output = Out> + Clone + Send + Sync + 'static,
{
    Out::into_any_chain(inner)
}

/// The coercions a slot performs when it needs a particular domain.
///
/// Each returns the same message shape the `As*` views produced, so the
/// `!tag > ` breadcrumb `spec::diagnostics` builds is unchanged: a mismatch is
/// still attributed to the child that produced the wrong type.
macro_rules! into_domain {
    ($name:ident, $variant:ident, $ty:ty, $what:literal) => {
        impl AnyChain {
            #[doc = concat!("Take this node as a ", $what, " chain, or report what it produced instead.")]
            pub fn $name(self) -> Result<Chain<Snapshot<Symbol>, $ty>, String> {
                match self {
                    AnyChain::$variant(c) => Ok(c),
                    other => Err(format!(
                        concat!("expected a ", $what, "-valued expression, got {}"),
                        other.kind()
                    )),
                }
            }
        }
    };
}

into_domain!(into_real, Real, Real, "Real");
into_domain!(into_bool, Bool, bool, "Bool");
into_domain!(into_atom, Atom, Atom, "Atom");
into_domain!(into_candle, Candle, Candle, "Candle");
into_domain!(into_str, Str, Arc<str>, "Str");
into_domain!(into_time, Time, Timestamp, "Time");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{Close, Pick, Sma};
    use crate::indicators::IndicatorExt;

    fn close() -> Close<Pick<Symbol>> {
        Close::of(Pick::<Symbol>::new())
    }

    #[test]
    fn any_picks_the_variant_from_the_output_type() {
        assert_eq!(any(Sma::new(close(), 3)).kind(), "Real");
        assert_eq!(any(Sma::new(close(), 3).above(1.0)).kind(), "Bool");
        assert_eq!(any(Pick::<Symbol>::new()).kind(), "Atom");
    }

    #[test]
    fn a_domain_mismatch_names_what_was_produced() {
        let Err(err) = any(Sma::new(close(), 3)).into_bool() else {
            panic!("Real is not Bool");
        };
        assert!(err.contains("expected a Bool-valued expression"), "{err}");
        assert!(err.contains("got Real"), "{err}");
    }

    /// The chain must survive being cloned — several concrete indicators clone
    /// their own source, so this is load-bearing rather than a convenience.
    #[test]
    fn a_chain_clones_behind_the_box() {
        let AnyChain::Real(chain) = any(Sma::new(close(), 2)) else {
            panic!("Sma is Real-valued");
        };
        let mut a = chain.clone();
        let mut b = chain;
        let snap = |px: Real| {
            Snapshot::single(
                crate::snapshot::symbol("X"),
                Atom::new(Candle::new(px, px, px, px, 0.0)),
            )
        };
        for px in [1.0, 2.0] {
            a.update(snap(px));
            b.update(snap(px));
        }
        assert_eq!(a.value(), b.value(), "clones advance identically");
    }
}
