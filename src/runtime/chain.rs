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
//! Measured (`cargo bench -p fugazi --bench erasure`), on a scalar chain:
//!
//! | | ns/sample |
//! |---|---:|
//! | concrete, no erasure, 1 node | 1.36 |
//! | payload erasure, 2 levels | 24.77 |
//! | payload erasure, 5 levels | 65.83 |
//! | **this vocabulary, 2 levels** | **3.81** |
//! | **this vocabulary, 5 levels** | **11.18** |
//! | hand-rolled single-method trait, 2 levels (the floor) | 3.66 |
//!
//! Marginal cost of one more level: **+13.7 ns** with a payload, **+2.5 ns**
//! here — and the last row says +2.5 is the floor, not a starting point. Depth
//! stops being a tax, which matters because real expressions are deep: the
//! YAML layer's depth-8 benchmark executes **39% fewer instructions** on this
//! vocabulary than on the payload one.
//!
//! If you re-run that benchmark, note that it builds its chains behind
//! `#[inline(never)]` deliberately. An earlier version did not, LLVM
//! devirtualised the whole chain, and it reported +0.4 ns/level — flattering
//! this design by 6×.
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

    /// Fold a **slice** of samples, writing one output per input.
    ///
    /// Semantically identical to calling [`update`](Indicator::update) once per
    /// element, and the default body is exactly that. [`Erased`] overrides it
    /// with a version that is materially faster, for a reason worth stating
    /// because it is not the obvious one.
    ///
    /// # Why this exists
    ///
    /// Driving an erased chain one sample at a time costs ~21 instructions/sample
    /// more than driving the same concrete indicator, and **that is not the
    /// vtable** — an indirect call with a predictable target is about two
    /// instructions. It is that the indicator's state lives behind the box, so
    /// the compiler cannot prove it does not alias the caller's output buffer and
    /// must reload and store every field on every sample. Held in a local, those
    /// fields promote to registers for the whole loop.
    ///
    /// `Erased::update_slice` copies the concrete indicator into a local, runs
    /// the loop, and writes it back once. Measured (`benches/icount.rs`,
    /// `sma_scalar_*`, net of a control):
    ///
    /// | | instr/sample |
    /// |---|---:|
    /// | concrete indicator, no erasure at all | 16.00 |
    /// | erased, this, whole slice | **16.04** |
    /// | erased, this, 128-sample chunks | 20.56 |
    /// | erased, one `update` per sample | 37.02 |
    ///
    /// Two shapes that do **not** work, both measured, so they are not retried:
    /// batching without the local copy (37.02 — no change), and streaming the
    /// samples through `&mut dyn FnMut` instead of a slice (46.03 — worse than
    /// doing nothing, because a closure's captures are themselves behind a
    /// pointer). The win requires a slice *and* the local copy together.
    ///
    /// # Contract
    ///
    /// `out` is written for `min(inputs.len(), out.len())` elements and the rest
    /// is left alone. Warm-up is reported as `None`, exactly as `update` does.
    /// A caller that needs per-sample state between samples must not use this.
    fn update_slice(&mut self, inputs: &[In], out: &mut [Option<Out>])
    where
        In: Clone,
    {
        for (o, i) in out.iter_mut().zip(inputs) {
            *o = self.update(i.clone());
        }
    }

    /// Fold a slice, writing each output **flattened** through `flatten` —
    /// no `Option` in the destination.
    ///
    /// The `Option` form above stages 16 bytes per sample (`Option<f64>` has no
    /// niche) that the caller then reads back, branches on, and writes again.
    /// Callers whose destination is a plain buffer — the Python bindings, whose
    /// `None` becomes `NaN` — want to write once. This is the scalar twin of
    /// `MultiOutput::write_strided`, which removed the same double hop from the
    /// multi-output path.
    fn update_slice_flat(&mut self, inputs: &[In], out: &mut [Real])
    where
        In: Clone,
        Out: Into<Real> + Copy,
    {
        for (o, i) in out.iter_mut().zip(inputs) {
            *o = self.update(i.clone()).map_or(Real::NAN, Into::into);
        }
    }
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
    fn stable_bars(&self) -> usize {
        self.0.stable_bars()
    }
    fn is_ready(&self) -> bool {
        self.0.is_ready()
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

    /// The point of the whole method — see the trait. `local` is what lets the
    /// indicator's state live in registers for the loop instead of round-tripping
    /// through memory once per sample.
    fn update_slice_flat(&mut self, inputs: &[In], out: &mut [Real])
    where
        In: Clone,
        Out: Into<Real> + Copy,
    {
        // Same local-state trick as `update_slice` — see there — with the
        // destination written once instead of staged through an `Option`.
        let mut local = self.0.clone();
        for (o, i) in out.iter_mut().zip(inputs) {
            *o = local.update(i.clone()).map_or(Real::NAN, Into::into);
        }
        self.0 = local;
    }

    fn update_slice(&mut self, inputs: &[In], out: &mut [Option<Out>])
    where
        In: Clone,
    {
        let mut local = self.0.clone();
        for (o, i) in out.iter_mut().zip(inputs) {
            *o = local.update(i.clone());
        }
        self.0 = local;
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

/// A [`Chain`] is itself an [`Indicator`], so it drops straight into any
/// constructor's source slot: `Sma::new(chain, 10)` needs no adapter.
///
/// This is what makes the migration off the payload vocabulary mechanical. The
/// old carrier had to be a newtype (`TypedSource`, `As<Out>`) purely to attach
/// `Input`/`Output` associated types to a payload box that had erased them; here
/// they are already in the type, so the newtype has nothing left to do.
impl<In, Out: Clone> Indicator for Box<dyn DynIndicator<In, Out>> {
    type Input = In;
    type Output = Out;
    fn update(&mut self, input: In) -> Option<Out> {
        (**self).update(input)
    }
    fn value(&self) -> Option<Out> {
        (**self).value()
    }
    fn warm_up_bars(&self) -> usize {
        (**self).warm_up_bars()
    }
    fn unstable_bars(&self) -> usize {
        (**self).unstable_bars()
    }
    fn stable_bars(&self) -> usize {
        (**self).stable_bars()
    }
    fn is_ready(&self) -> bool {
        (**self).is_ready()
    }
    fn reset(&mut self) {
        (**self).reset();
    }
    fn save_state(&self) -> serde_json::Value {
        (**self).save_state()
    }
    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        (**self).load_state(state)
    }
}

/// Erase a concrete indicator into a [`Chain`], keeping its domain in the type.
///
/// The narrow counterpart of [`wrap`](super::wrap) / [`wrap_sync`](super::wrap_sync):
/// same role, but the input and output types survive, so the result needs no
/// typed view to be usable again.
pub fn erase<I, In, Out>(inner: I) -> Chain<In, Out>
where
    I: Indicator<Input = In, Output = Out> + Clone + Send + Sync + 'static,
    In: 'static,
    Out: Clone + 'static,
{
    Box::new(Erased::new(inner))
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

/// Call one `Indicator` reader on whichever variant is present.
///
/// A closure would need a trait object common to all six `Chain` types, and
/// `dyn DynIndicator<In, Out>` cannot unsize-coerce to one — so the dispatch is
/// a macro instead of a higher-order function.
macro_rules! readiness {
    ($self:expr, $method:ident) => {
        match $self {
            AnyChain::Real(c) => c.$method(),
            AnyChain::Bool(c) => c.$method(),
            AnyChain::Atom(c) => c.$method(),
            AnyChain::Candle(c) => c.$method(),
            AnyChain::Str(c) => c.$method(),
            AnyChain::Time(c) => c.$method(),
        }
    };
}

impl AnyChain {
    /// The domain this node produces.
    ///
    /// Reuses [`PayloadType`](super::PayloadType) rather than introducing a
    /// second tag enum: that type is also the *static* type vocabulary
    /// `spec::typecheck` works in, so a builder can compare what a subtree
    /// declared against what it built without a translation step. Nothing here
    /// carries a payload — only the tag is shared.
    pub fn output_type(&self) -> super::PayloadType {
        use super::PayloadType as T;
        match self {
            AnyChain::Real(_) => T::Real,
            AnyChain::Bool(_) => T::Bool,
            AnyChain::Atom(_) => T::Atom,
            AnyChain::Candle(_) => T::Candle,
            AnyChain::Str(_) => T::Str,
            AnyChain::Time(_) => T::Time,
        }
    }

    /// Readiness of the built node, without having to name its domain first —
    /// the numbers a caller wants before deciding how much history to replay.
    pub fn warm_up_bars(&self) -> usize {
        readiness!(self, warm_up_bars)
    }

    /// Extra samples after warm-up before any recursive source inside has
    /// converged. See [`Indicator::unstable_bars`].
    pub fn unstable_bars(&self) -> usize {
        readiness!(self, unstable_bars)
    }

    /// `warm_up_bars() + unstable_bars()`, as the node itself reports it —
    /// which is *not* always the sum, since `!unstable` forces the tail to `0`.
    pub fn stable_bars(&self) -> usize {
        readiness!(self, stable_bars)
    }

    /// The domain's name, for diagnostics. Matches `PayloadType`'s `Display`,
    /// so error messages are unchanged.
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
                    // Word-for-word what the `As<Out>` views reported, so a
                    // migrated build error is byte-identical to the old one.
                    other => Err(format!(
                        concat!("expected a ", $what, " expression, got {}"),
                        other.kind()
                    )),
                }
            }
        }
    };
}

impl AnyChain {
    /// Opt this subtree out of the strategy-readiness wait for its IIR settling
    /// tail — the `!unstable { source }` tag, and the narrow twin of
    /// [`unstable_wrap`](super::unstable_wrap).
    ///
    /// Nothing runtime-typed is needed: [`Unstable`](crate::indicators::Unstable) is an ordinary library
    /// wrapper and a [`Chain`] is an ordinary [`Indicator`], so this is the same
    /// `.unstable()` a hand-written strategy calls, applied per domain.
    pub fn unstable(self) -> AnyChain {
        use crate::indicators::Unstable;
        match self {
            AnyChain::Real(c) => AnyChain::Real(erase(Unstable::new(c))),
            AnyChain::Bool(c) => AnyChain::Bool(erase(Unstable::new(c))),
            AnyChain::Atom(c) => AnyChain::Atom(erase(Unstable::new(c))),
            AnyChain::Candle(c) => AnyChain::Candle(erase(Unstable::new(c))),
            AnyChain::Str(c) => AnyChain::Str(erase(Unstable::new(c))),
            AnyChain::Time(c) => AnyChain::Time(erase(Unstable::new(c))),
        }
    }
}

macro_rules! probed {
    ($name:ident, $variant:ident, $ty:ty, $what:literal) => {
        impl AnyChain {
            #[doc = concat!("Take this node as a ", $what, " chain, panicking if it is not.")]
            ///
            /// **Only for the per-symbol lazy factories** in `BasketStrategy` /
            /// `MultiAssetStrategy`. Those build their chains inside `update`,
            /// where there is no error path to return through — so instead each
            /// template is probed once at build time against `PROBE_SYMBOL`
            /// (`spec::basket::probe_template`,
            /// `spec::multi_asset::probe_signal`/`probe_expr`), and a template
            /// that builds for the probe builds for every symbol.
            ///
            /// Reaching this panic therefore means a per-symbol slot was added
            /// without being added to the probe. Everywhere else, use the
            /// `into_*` twin and report the error.
            pub fn $name(self, slot: &str) -> Chain<Snapshot<Symbol>, $ty> {
                match self {
                    AnyChain::$variant(c) => c,
                    other => panic!(
                        concat!(
                            "`{}` built a {} chain, expected ",
                            $what,
                            " — the per-symbol template was not probed at build time"
                        ),
                        slot,
                        other.kind(),
                    ),
                }
            }
        }
    };
}

probed!(probed_real, Real, Real, "Real");
probed!(probed_bool, Bool, bool, "Bool");

impl AnyChain {
    /// Re-erase into the payload vocabulary.
    ///
    /// The bridge for the one consumer that genuinely needs self-description:
    /// `spec::overlay` drives a column set whose members differ in *input*
    /// domain — spec-built columns read the whole snapshot, while a Python
    /// carrier can be rooted on a single bar — and asks each column what it
    /// wants before feeding it. A `Vec<AnyChain>` cannot express that, since
    /// every `AnyChain` is snapshot-rooted by construction.
    ///
    /// Costs one `Box` per column at build time and re-introduces the payload's
    /// per-bar cost *for overlay columns only*. Everything upstream of here
    /// stays narrow.
    pub fn into_payload(self) -> Box<dyn super::PayloadIndicator> {
        match self {
            AnyChain::Real(c) => super::wrap(c),
            AnyChain::Bool(c) => super::wrap(c),
            AnyChain::Atom(c) => super::wrap(c),
            AnyChain::Candle(c) => super::wrap(c),
            AnyChain::Str(c) => super::wrap(c),
            AnyChain::Time(c) => super::wrap(c),
        }
    }
}

/// Feed a candle-producing chain's output into a snapshot-rooted one.
///
/// The shape `!resample { every, inner }` needs: `Resample` aggregates base bars
/// into a higher-timeframe [`Candle`], and on the bars that complete a bucket
/// that candle drives `inner`, which was built as an ordinary snapshot-rooted
/// subtree. Between the two sits the same lift the payload vocabulary spelled as
/// a `TryFrom` arm — a candle becomes an **untagged size-1 snapshot**, which is
/// what an empty-selector `!pick` unpacks.
///
/// `None` from the outer means "no bucket completed this bar", so `inner` is not
/// advanced at all — that is the whole point of the node, and why this cannot be
/// an ordinary source slot.
#[derive(Clone)]
struct CandleOver<Out: 'static> {
    outer: CandleChain,
    inner: Chain<Snapshot<Symbol>, Out>,
    value: Option<Out>,
}

impl<Out: Clone + 'static> Indicator for CandleOver<Out> {
    type Input = Snapshot<Symbol>;
    type Output = Out;
    fn update(&mut self, input: Snapshot<Symbol>) -> Option<Out> {
        self.value = match self.outer.update(input) {
            Some(candle) => self
                .inner
                .update(Snapshot::<Symbol>::of_atom(candle.into())),
            None => None,
        };
        self.value.clone()
    }
    fn value(&self) -> Option<Out> {
        self.value.clone()
    }
    fn warm_up_bars(&self) -> usize {
        // Both stages count in *base* bars: the outer's own warm-up, plus the
        // extra higher-timeframe emissions the inner still needs (one of which
        // coincides with the outer's first).
        self.outer
            .warm_up_bars()
            .saturating_add(self.inner.warm_up_bars().saturating_sub(1))
    }
    fn unstable_bars(&self) -> usize {
        self.outer
            .unstable_bars()
            .saturating_add(self.inner.unstable_bars())
    }
    fn reset(&mut self) {
        self.outer.reset();
        self.inner.reset();
        self.value = None;
    }
    fn save_state(&self) -> serde_json::Value {
        serde_json::json!({ "outer": self.outer.save_state(), "inner": self.inner.save_state() })
    }
    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        let obj = state
            .as_object()
            .ok_or_else(|| "expected an object with `outer` and `inner`".to_string())?;
        if let Some(v) = obj.get("outer") {
            self.outer.load_state(v)?;
        }
        if let Some(v) = obj.get("inner") {
            self.inner.load_state(v)?;
        }
        Ok(())
    }
}

/// Chain a candle-producing `outer` into a snapshot-rooted `inner`, preserving
/// `inner`'s output domain — the shape `!resample { every, inner }` needs.
pub fn chain_over_candle(outer: CandleChain, inner: AnyChain) -> AnyChain {
    macro_rules! over {
        ($variant:ident, $inner:expr) => {
            AnyChain::$variant(erase(CandleOver {
                outer,
                inner: $inner,
                value: None,
            }))
        };
    }
    match inner {
        AnyChain::Real(c) => over!(Real, c),
        AnyChain::Bool(c) => over!(Bool, c),
        AnyChain::Atom(c) => over!(Atom, c),
        AnyChain::Candle(c) => over!(Candle, c),
        AnyChain::Str(c) => over!(Str, c),
        AnyChain::Time(c) => over!(Time, c),
    }
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
    use crate::indicators::IndicatorExt;
    use crate::indicators::{Close, Pick, Sma};

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
        assert!(err.contains("expected a Bool expression"), "{err}");
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
