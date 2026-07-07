//! Projecting one `Real` component out of a multi-output indicator.
//!
//! Two flavours ship: [`Component`] holds its own clone of the source, so two
//! components of the same indicator run two independent computations (the
//! historical default, straightforward but wasteful when several accessors
//! target the same instance). [`SharedComponent`] handles that case: hand the
//! indicator to [`Shared::new`] (or call the generated `.shared()` on the
//! indicator), then every accessor on the resulting [`Shared`] borrows into the
//! same source and advances it **at most once per bar** — the first accessor
//! updated each bar drives the shared source, the rest read the cached output.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use crate::indicator::Indicator;
use crate::types::Real;

/// Declare the standard component accessors on a multi-output indicator: one
/// `pub fn <method>(&self) -> Component<Self>` per output field, each a clone
/// + [`Component::new`] with a selector to that field. The same list also
///   generates a `.shared()` method that wraps the indicator in a [`Shared`]
///   handle and a mirror set of accessors on that handle returning
///   [`SharedComponent`]s so all accessors of one handle share one source.
///
/// Style: `component_accessors!(Type<G, ...>, ValueType; method => field, ...)`.
/// The `impl` bound is `Type<G, ...>: Indicator<Output = ValueType> + Clone`
/// (for the bare `Component` variant; the shared variant additionally requires
/// `ValueType: Copy`, which every multi-output indicator's value struct already
/// derives).
///
/// ```ignore
/// component_accessors!(
///     Bollinger<S>, BollingerValue;
///     /// Upper band.
///     upper => upper,
///     /// Middle band.
///     middle => middle,
///     /// Lower band.
///     lower => lower,
/// );
/// ```
macro_rules! component_accessors {
    (
        $parent:ident<$($gen:ident),+>, $value:ty;
        $(
            $(#[$doc:meta])*
            $method:ident => $field:ident
        ),+ $(,)?
    ) => {
        impl<$($gen),+> $parent<$($gen),+>
        where
            $parent<$($gen),+>: $crate::indicator::Indicator<Output = $value> + Clone,
        {
            $(
                $(#[$doc])*
                pub fn $method(&self) -> $crate::indicators::component::Component<Self> {
                    $crate::indicators::component::Component::new(
                        self.clone(),
                        |v| v.$field,
                    )
                }
            )+

            /// Wrap this indicator in a [`Shared`] handle so every component
            /// accessor called on the handle drives the same underlying
            /// computation — advancing it once per bar regardless of how many
            /// accessors the surrounding expression tree contains.
            pub fn shared(self) -> $crate::indicators::component::Shared<Self> {
                $crate::indicators::component::Shared::new(self)
            }
        }

        impl<$($gen),+> $crate::indicators::component::Shared<$parent<$($gen),+>>
        where
            $parent<$($gen),+>: $crate::indicator::Indicator<Output = $value>,
        {
            $(
                $(#[$doc])*
                pub fn $method(
                    &self,
                ) -> $crate::indicators::component::SharedComponent<
                    <Self as $crate::indicators::component::SharedHandle>::Source,
                > {
                    $crate::indicators::component::SharedComponent::new(
                        self,
                        |v| v.$field,
                    )
                }
            )+
        }
    };
}

pub(crate) use component_accessors;

// ---------------------------------------------------------------------------
// Independent-clone Component: one accessor, one full source.
// ---------------------------------------------------------------------------

/// Adapts a multi-output indicator into a single-output [`Indicator`] that
/// yields just one of its component fields.
///
/// Multi-output indicators ([`Macd`](super::Macd), [`Bollinger`](super::Bollinger),
/// …) set their [`Output`](Indicator::Output) to a small struct, which means a
/// component like the MACD signal line or a Bollinger band cannot, on its own,
/// feed the [`Real`]-only composition and comparison machinery (`gt`, `add`,
/// `crosses_above`, …). `Component` closes that gap: it wraps the source and a
/// field selector and presents the chosen field as an ordinary
/// `Indicator<Output = Real>`, so it composes like any other source:
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Current, Macd};
///
/// let macd = Macd::new(Current::close(), 12, 26, 9);
/// // The MACD line crossing above its signal line — a single composed Signal.
/// let _cross = macd.line().crosses_above(macd.signal());
/// ```
///
/// You build one through the component accessors on each multi-output indicator
/// (`macd.line()`, `bands.upper()`, `adx.plus_di()`, …) rather than naming it
/// directly. Each accessor **clones** the source and pairs it with the selector,
/// so two components of the same indicator are two independently-advanced
/// instances — the same clone-the-operands tradeoff [`crosses_above`] already
/// makes (correct, at roughly the source work per component). Use
/// [`Shared`]/[`SharedComponent`] when the same indicator drives several
/// accessors and the duplicate work matters.
///
/// [`crosses_above`]: crate::indicators::IndicatorExt::crosses_above
#[derive(Debug, Clone)]
pub struct Component<I: Indicator> {
    source: I,
    select: fn(I::Output) -> Real,
    /// Latest projected component; `None` until the source is warmed up.
    pub value: Option<Real>,
}

impl<I: Indicator> Component<I> {
    /// Wrap `source`, projecting the field picked out by `select`.
    ///
    /// Prefer the named accessors on the indicators themselves (`macd.line()`,
    /// …); this is the underlying constructor they call.
    pub fn new(source: I, select: fn(I::Output) -> Real) -> Self {
        Self {
            source,
            select,
            value: None,
        }
    }
}

impl<I: Indicator> Indicator for Component<I> {
    type Input = I::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = self.source.update(input).map(self.select);
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        // `max(1)` guards a `warm_up = 0` inner (e.g. `Value`) — projection
        // still needs one `update` to advance the source.
        self.source.warm_up_period().max(1)
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.value = None;
    }
}

// ---------------------------------------------------------------------------
// Shared source: one indicator, many accessors, one advance per bar.
// ---------------------------------------------------------------------------

/// The inner cell every [`SharedComponent`] built from one [`Shared`] borrows
/// into. The `generation` counter is what makes the "advance once per bar"
/// guarantee work: it increments each time the source is fed, and every
/// [`SharedComponent`] holds its own `local_gen` — see the invariant on
/// [`SharedComponent::update`].
struct SharedInner<M: Indicator>
where
    M::Output: Copy,
{
    source: M,
    generation: u64,
    last_output: Option<M::Output>,
}

/// A handle to a multi-output indicator shared by multiple accessors, so a
/// single per-bar `update` on the underlying source serves every accessor
/// derived from this handle.
///
/// Construct with the generated `.shared()` method on any multi-output
/// indicator, or with [`Shared::new`] directly. The `component_accessors!`
/// macro that ships with each multi-output indicator generates the same
/// accessor list on `Shared<Self>` as on the bare indicator, so
/// `macd.shared().line()` and `macd.shared().signal()` return
/// [`SharedComponent`]s that both target the same [`Macd`](super::Macd) — the
/// underlying MACD advances exactly once per bar regardless of how many
/// accessors the surrounding signal tree contains.
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Current, Macd};
///
/// let macd = Macd::new(Current::close(), 12, 26, 9).shared();
/// // Both `line()` and `signal()` project out of the same shared MACD; the
/// // full MACD math runs once per bar, not once per accessor.
/// let _cross = macd.line().crosses_above(macd.signal());
/// ```
///
/// [`Clone`] returns another handle to the same inner cell (an `Rc::clone`,
/// not a deep copy), so a caller who wants to hand the same indicator to
/// several expressions can pass `handle.clone()` around freely.
pub struct Shared<M: Indicator>
where
    M::Output: Copy,
{
    inner: Rc<RefCell<SharedInner<M>>>,
}

impl<M: Indicator> Shared<M>
where
    M::Output: Copy,
{
    /// Wrap `source` in a [`Shared`] handle. The generation counter starts at
    /// `0`; [`SharedComponent`]s produced from this handle initialise their
    /// own `local_gen` from whatever the shared counter reads at the moment
    /// they are constructed, so components created mid-run don't spuriously
    /// re-trigger an update.
    pub fn new(source: M) -> Self {
        Self {
            inner: Rc::new(RefCell::new(SharedInner {
                source,
                generation: 0,
                last_output: None,
            })),
        }
    }
}

impl<M: Indicator> Clone for Shared<M>
where
    M::Output: Copy,
{
    fn clone(&self) -> Self {
        Self {
            inner: Rc::clone(&self.inner),
        }
    }
}

/// Names the inner source of a [`Shared`] handle so the `component_accessors!`
/// macro can spell out shared-accessor return types without nesting the outer
/// indicator's generics inside the per-accessor repetition (`macro_rules!` gets
/// confused by that; the trait lets us write
/// `SharedComponent<<Self as SharedHandle>::Source>` instead).
pub trait SharedHandle {
    /// The underlying multi-output indicator behind the handle.
    type Source: Indicator;
}

impl<M: Indicator> SharedHandle for Shared<M>
where
    M::Output: Copy,
{
    type Source = M;
}

impl<M: Indicator> fmt::Debug for Shared<M>
where
    M::Output: Copy,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Avoid recursing into M/M::Output — the interesting bit is that this
        // handle is shared, so a fingerprint of the Rc count is what a Debug
        // reader wants to see.
        f.debug_struct("Shared")
            .field("refs", &Rc::strong_count(&self.inner))
            .finish()
    }
}

/// One projection out of a [`Shared`] handle — the shared analogue of
/// [`Component`]. Multiple [`SharedComponent`]s built from the same handle
/// (or from clones of the same handle) all borrow into the same source, so
/// however many end up in the surrounding expression tree the underlying
/// multi-output indicator advances **at most once per bar**.
///
/// ## The "advance once per bar" invariant
///
/// Each [`SharedComponent`] holds its own `local_gen`. The shared cell holds
/// a `generation` counter that ticks on every source `update`. On the first
/// [`SharedComponent`]-of-a-bar's `update`, its `local_gen` equals the shared
/// `generation` (both were last synced at the end of the previous bar), so it
/// takes the "advance" branch: it feeds the source, ticks the shared counter,
/// caches the output, and syncs its `local_gen` to the bumped counter. Every
/// later [`SharedComponent`] this bar sees `local_gen < generation`, takes
/// the "read cached" branch, and syncs. At the end of the bar, all
/// components' `local_gen` again equals `generation`, ready for the next bar.
///
/// Call order within a bar doesn't matter (whichever accessor is updated
/// first drives the source); what matters is that each accessor is called at
/// most once per bar per input. Feeding two different inputs to the same
/// shared source in one bar is user error — the second call reads the cache
/// of the first — but that mirrors the semantics of the bare [`Component`],
/// where calling `update` twice on one instance just advances it twice.
pub struct SharedComponent<M: Indicator>
where
    M::Output: Copy,
{
    handle: Rc<RefCell<SharedInner<M>>>,
    select: fn(M::Output) -> Real,
    /// Last `generation` this component observed; equal to the shared counter
    /// once it has synced this bar, strictly less than it between the shared
    /// advance and this component's own catch-up call.
    local_gen: u64,
    /// Cached projected value returned by [`Indicator::value`]. Set on every
    /// `update` (whether we advanced the source or read the cache).
    value: Option<Real>,
}

impl<M: Indicator> SharedComponent<M>
where
    M::Output: Copy,
{
    /// Build a component that projects `select` out of the source behind
    /// `shared`. Prefer the generated accessors (`shared.line()`, …) — this
    /// is the constructor those call.
    ///
    /// `local_gen` is initialised to the shared cell's current `generation`,
    /// so a component created after the source has already been updated for
    /// N bars starts in sync with the shared counter and will not spuriously
    /// re-advance on its first `update`.
    pub fn new(shared: &Shared<M>, select: fn(M::Output) -> Real) -> Self {
        let local_gen = shared.inner.borrow().generation;
        Self {
            handle: Rc::clone(&shared.inner),
            select,
            local_gen,
            value: None,
        }
    }
}

impl<M: Indicator> Clone for SharedComponent<M>
where
    M::Output: Copy,
{
    fn clone(&self) -> Self {
        Self {
            handle: Rc::clone(&self.handle),
            select: self.select,
            local_gen: self.local_gen,
            value: self.value,
        }
    }
}

impl<M: Indicator> fmt::Debug for SharedComponent<M>
where
    M::Output: Copy,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedComponent")
            .field("local_gen", &self.local_gen)
            .field("value", &self.value)
            .finish()
    }
}

impl<M: Indicator> Indicator for SharedComponent<M>
where
    M::Output: Copy,
{
    type Input = M::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        let mut inner = self.handle.borrow_mut();
        if self.local_gen == inner.generation {
            // First accessor of this bar — drive the shared source.
            let out = inner.source.update(input);
            inner.last_output = out;
            inner.generation = inner.generation.wrapping_add(1);
        }
        // Sync up to the (possibly just-bumped) shared counter and read.
        self.local_gen = inner.generation;
        self.value = inner.last_output.map(self.select);
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.handle.borrow().source.warm_up_period().max(1)
    }

    fn unstable_period(&self) -> usize {
        self.handle.borrow().source.unstable_period()
    }

    fn reset(&mut self) {
        let mut inner = self.handle.borrow_mut();
        inner.source.reset();
        inner.last_output = None;
        // Leave `generation` alone. This component is now back in sync with
        // it, and any *other* SharedComponents holding the same handle
        // strictly lag the counter, so the invariant holds: whichever
        // component is called first next bar will take the advance branch.
        self.local_gen = inner.generation;
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{Bollinger, Current, Macd};
    use crate::indicators::{BoolIndicatorExt, IndicatorExt};
    use crate::types::{Candle, Real};

    fn bar(close: Real) -> Candle {
        Candle::new(close, close, close, close, 0.0)
    }

    #[test]
    fn projects_the_same_value_as_the_indicator_field() {
        let mut line = Macd::new(Current::close(), 3, 6, 4).line();
        let mut reference = Macd::new(Current::close(), 3, 6, 4);
        for p in [10.0, 11.0, 12.0, 11.5, 13.0, 14.0, 13.5, 15.0, 16.0, 15.0] {
            let projected = line.update(bar(p).into());
            let whole = reference.update(bar(p).into());
            assert_eq!(projected, whole.map(|v| v.macd));
        }
    }

    #[test]
    fn components_compose_into_a_crossover() {
        let macd = Macd::new(Current::close(), 3, 6, 4);
        // Exactly the user's target: line crosses above signal, as one Signal.
        let mut bullish = macd.line().crosses_above(macd.signal());
        let mut fired = false;
        // A dip then a sustained rally drives the MACD line up through its signal.
        for p in [
            20.0, 19.0, 18.0, 17.0, 18.0, 20.0, 22.0, 24.0, 26.0, 28.0, 30.0, 32.0,
        ] {
            bullish.update(bar(p).into());
            fired |= bullish.is_true();
        }
        assert!(fired, "expected a bullish MACD crossover");
    }

    #[test]
    fn band_component_projects_the_band_field() {
        // The lower band, projected as a source, matches the indicator's field —
        // so `Current::close().lt(bands.lower())` means exactly "close below the
        // lower band".
        let mut lower = Bollinger::new(Current::close(), 5, 2.0).lower();
        let mut reference = Bollinger::new(Current::close(), 5, 2.0);
        for p in [10.0, 10.1, 9.9, 10.0, 10.05, 18.0, 12.0, 11.0, 9.0, 8.5] {
            assert_eq!(
                lower.update(bar(p).into()),
                reference.update(bar(p).into()).map(|v| v.lower)
            );
        }
    }

    // ---- Shared-source tests ----

    /// A shared MACD's `.line()` and `.signal()` return the same numbers each
    /// bar as an independent, bare MACD — proving the shared-source
    /// optimisation is behaviour-preserving.
    #[test]
    fn shared_and_bare_produce_identical_values() {
        let macd_shared = Macd::new(Current::close(), 3, 6, 4).shared();
        let mut line = macd_shared.line();
        let mut signal = macd_shared.signal();
        let mut histogram = macd_shared.histogram();

        let mut reference = Macd::new(Current::close(), 3, 6, 4);

        for p in [
            10.0, 11.0, 12.0, 11.5, 13.0, 14.0, 13.5, 15.0, 16.0, 15.0, 14.0, 13.5,
        ] {
            let candle: crate::types::Atom = bar(p).into();
            // Advance the shared components in a fixed order — the first
            // triggers the shared source, the rest read the cache. Order
            // shouldn't matter for correctness.
            let l = line.update(candle.clone());
            let s = signal.update(candle.clone());
            let h = histogram.update(candle.clone());
            let r = reference.update(candle);
            assert_eq!(l, r.map(|v| v.macd));
            assert_eq!(s, r.map(|v| v.signal));
            assert_eq!(h, r.map(|v| v.histogram));
        }
    }

    /// The shared source is only advanced *once* per bar, regardless of how
    /// many components read out of it. We check this with a spy indicator
    /// that counts its own `update` calls, wrap it in a `Shared`, and read
    /// through several accessors.
    #[test]
    fn shared_source_advances_at_most_once_per_bar() {
        // A minimal multi-output spy. The output struct has two `Real`
        // fields so we can build two accessors that select different ones —
        // the shared source must advance exactly `bars` times regardless.
        use std::cell::Cell;
        use std::rc::Rc;

        #[derive(Copy, Clone, Debug, PartialEq)]
        struct Pair {
            a: Real,
            b: Real,
        }

        #[derive(Debug, Clone)]
        struct Spy {
            counter: Rc<Cell<usize>>,
            last: Option<Pair>,
        }

        impl Spy {
            fn new(counter: Rc<Cell<usize>>) -> Self {
                Self {
                    counter,
                    last: None,
                }
            }
        }

        impl Indicator for Spy {
            type Input = Real;
            type Output = Pair;
            fn update(&mut self, input: Real) -> Option<Pair> {
                self.counter.set(self.counter.get() + 1);
                let out = Pair {
                    a: input,
                    b: input * 2.0,
                };
                self.last = Some(out);
                Some(out)
            }
            fn value(&self) -> Option<Pair> {
                self.last
            }
            fn warm_up_period(&self) -> usize {
                1
            }
            fn reset(&mut self) {
                self.last = None;
            }
        }

        let counter = Rc::new(Cell::new(0));
        let shared: Shared<Spy> = Shared::new(Spy::new(Rc::clone(&counter)));

        // Two accessors on the same shared handle, each selecting a
        // different field. Neither uses `component_accessors!` (Spy is
        // local to this test), so we build them directly.
        let mut acc_a = SharedComponent::new(&shared, |p| p.a);
        let mut acc_b = SharedComponent::new(&shared, |p| p.b);
        // A third — a clone of `acc_a` — so we also cover the "same
        // handle, same selector, extra clone" case that `crosses_above`
        // produces when a comparison's operands get duplicated.
        let mut acc_a_clone = acc_a.clone();

        for bar_idx in 0..10 {
            let x = bar_idx as Real;
            let a = acc_a.update(x);
            let b = acc_b.update(x);
            let a2 = acc_a_clone.update(x);
            assert_eq!(a, Some(x));
            assert_eq!(b, Some(x * 2.0));
            // The clone selects `p.a` too and reads the same cached bar.
            assert_eq!(a2, a);
        }

        assert_eq!(
            counter.get(),
            10,
            "shared source advanced {} times over 10 bars — expected exactly 10",
            counter.get()
        );
    }

    /// Sharing across the whole crossover — the exact composition
    /// `macd_crossover` uses — advances the shared MACD once per bar even
    /// though the expression contains four line/signal reads.
    #[test]
    fn shared_crossover_advances_source_once_per_bar() {
        use std::cell::Cell;
        use std::rc::Rc;

        // Reuse the same Pair/Spy setup as the previous test.
        #[derive(Copy, Clone, Debug, PartialEq)]
        struct Pair {
            a: Real,
            b: Real,
        }

        #[derive(Debug, Clone)]
        struct Spy {
            counter: Rc<Cell<usize>>,
            last: Option<Pair>,
            t: Real,
        }

        impl Spy {
            fn new(counter: Rc<Cell<usize>>) -> Self {
                Self {
                    counter,
                    last: None,
                    t: 0.0,
                }
            }
        }

        impl Indicator for Spy {
            type Input = Real;
            type Output = Pair;
            fn update(&mut self, _input: Real) -> Option<Pair> {
                self.counter.set(self.counter.get() + 1);
                // Force a crossover: a = t, b = 5.0 (constant), so a rises
                // through b on bar 5 (0-indexed 5). One clean edge event.
                self.t += 1.0;
                let out = Pair {
                    a: self.t,
                    b: 5.0,
                };
                self.last = Some(out);
                Some(out)
            }
            fn value(&self) -> Option<Pair> {
                self.last
            }
            fn warm_up_period(&self) -> usize {
                1
            }
            fn reset(&mut self) {
                self.last = None;
                self.t = 0.0;
            }
        }

        let counter = Rc::new(Cell::new(0));
        let shared: Shared<Spy> = Shared::new(Spy::new(Rc::clone(&counter)));

        // `a` crosses above `b` — the whole crossover expression clones its
        // operands, so the tree has four SharedComponents pointing at the
        // one Spy. All four are advanced per bar; the shared source must
        // still tick exactly once per bar.
        let a1 = SharedComponent::new(&shared, |p| p.a);
        let b1 = SharedComponent::new(&shared, |p| p.b);
        let mut crossover = a1.crosses_above(b1);

        let mut fired = false;
        for _ in 0..10 {
            crossover.update(0.0);
            fired |= crossover.is_true();
        }
        assert!(fired, "expected the crossover to fire");
        assert_eq!(
            counter.get(),
            10,
            "shared source advanced {} times over 10 bars — expected exactly 10",
            counter.get()
        );
    }

    /// A [`SharedComponent`] created after the source has already been
    /// updated for several bars starts in sync with the shared counter, so
    /// its first update reads the cached output from the accessor that
    /// drove that bar rather than re-advancing.
    #[test]
    fn shared_component_created_mid_run_does_not_spuriously_advance() {
        use std::cell::Cell;
        use std::rc::Rc;

        #[derive(Copy, Clone, Debug, PartialEq)]
        struct Pair {
            a: Real,
            b: Real,
        }

        #[derive(Debug, Clone)]
        struct Spy {
            counter: Rc<Cell<usize>>,
            last: Option<Pair>,
        }

        impl Indicator for Spy {
            type Input = Real;
            type Output = Pair;
            fn update(&mut self, input: Real) -> Option<Pair> {
                self.counter.set(self.counter.get() + 1);
                let out = Pair {
                    a: input,
                    b: input + 1.0,
                };
                self.last = Some(out);
                Some(out)
            }
            fn value(&self) -> Option<Pair> {
                self.last
            }
            fn warm_up_period(&self) -> usize {
                1
            }
            fn reset(&mut self) {
                self.last = None;
            }
        }

        let counter = Rc::new(Cell::new(0));
        let shared: Shared<Spy> = Shared::new(Spy {
            counter: Rc::clone(&counter),
            last: None,
        });

        let mut acc_a = SharedComponent::new(&shared, |p| p.a);
        for i in 0..5 {
            acc_a.update(i as Real);
        }
        assert_eq!(counter.get(), 5);

        // Create a second accessor now (after 5 updates). It should
        // *observe* the current shared cache, not double-count.
        let mut acc_b = SharedComponent::new(&shared, |p| p.b);
        for i in 5..10 {
            let x = i as Real;
            let a = acc_a.update(x);
            let b = acc_b.update(x);
            assert_eq!(a, Some(x));
            // b sees the same bar acc_a just drove, projected differently.
            assert_eq!(b, Some(x + 1.0));
        }
        assert_eq!(
            counter.get(),
            10,
            "shared source advanced {} times — expected 5 + 5 = 10",
            counter.get()
        );
    }

    /// Every classical strategy that opts into sharing wires accessors up in
    /// the shape `macd.line().crosses_above(macd.signal())` twice (once per
    /// entry/exit side). This test rebuilds `macd_crossover`'s inner shape by
    /// hand against a spy and verifies the shared source advances once per
    /// bar despite the four crossover subtrees the strategy would generate.
    #[test]
    fn shared_multiple_signals_all_share_one_source() {
        use std::cell::Cell;
        use std::rc::Rc;

        #[derive(Copy, Clone, Debug, PartialEq)]
        struct Pair {
            a: Real,
            b: Real,
        }

        #[derive(Debug, Clone)]
        struct Spy {
            counter: Rc<Cell<usize>>,
            last: Option<Pair>,
            t: Real,
        }

        impl Indicator for Spy {
            type Input = Real;
            type Output = Pair;
            fn update(&mut self, _: Real) -> Option<Pair> {
                self.counter.set(self.counter.get() + 1);
                self.t += 1.0;
                let out = Pair {
                    a: self.t,
                    b: 5.0,
                };
                self.last = Some(out);
                Some(out)
            }
            fn value(&self) -> Option<Pair> {
                self.last
            }
            fn warm_up_period(&self) -> usize {
                1
            }
            fn reset(&mut self) {
                self.last = None;
                self.t = 0.0;
            }
        }

        let counter = Rc::new(Cell::new(0));
        let shared: Shared<Spy> = Shared::new(Spy {
            counter: Rc::clone(&counter),
            last: None,
            t: 0.0,
        });

        // Four independent crossover expressions — each mimics one of
        // `SingleAssetStrategy`'s four signal slots (long_enter, long_exit,
        // short_enter, short_exit). All four share the *same* Spy.
        let up = || {
            let a = SharedComponent::new(&shared, |p| p.a);
            let b = SharedComponent::new(&shared, |p| p.b);
            a.crosses_above(b)
        };
        let down = || {
            let a = SharedComponent::new(&shared, |p| p.a);
            let b = SharedComponent::new(&shared, |p| p.b);
            a.crosses_below(b)
        };

        let mut long_enter = up();
        let mut long_exit = down();
        let mut short_enter = down();
        let mut short_exit = up();

        for _ in 0..12 {
            long_enter.update(0.0);
            long_exit.update(0.0);
            short_enter.update(0.0);
            short_exit.update(0.0);
        }
        assert_eq!(
            counter.get(),
            12,
            "shared source advanced {} times across 12 bars — expected exactly 12",
            counter.get()
        );
    }
}
