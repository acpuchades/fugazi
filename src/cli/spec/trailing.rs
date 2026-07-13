//! CLI builder for the trailing risk indicators (`!sharpe` / `!sortino` /
//! `!volatility` / `!max_drawdown` / `!calmar`).
//!
//! The library indicators ([`fugazi::indicators::Sharpe`] and friends) each own
//! a [`Strategy`](fugazi::Strategy) and drive it internally. The runtime
//! type-erasure layer ([`DynIndicator`]) requires the wrapped indicator to be
//! [`Clone`], but a built [`DynSingleStrategy`](super::strategy::DynSingleStrategy)
//! is **not** `Clone` (it holds `Box<dyn Signal>` slots). So this module wraps
//! the trailing indicator in a [`RebuildIndicator`] that carries the strategy
//! *spec* plus a rebuild closure and mints a **fresh** indicator instance on
//! every clone — matching the "clone = an independently-advanced instance"
//! convention the component accessors already use.
//!
//! The wallet seed is a fixed [`SEED`]: every metric here is a ratio of
//! equity-curve returns, and the returns are scale-invariant in the seed, so
//! exposing it as a knob would add surface with no effect on the reading. The
//! embedded strategy's [`Book`](fugazi::indicators::Book) is seeded to the same
//! value so its book-anchored sizing recipes stay meaningful.

use std::rc::Rc;
use std::sync::Arc;

use fugazi::indicators::{Calmar, MaxDrawdown, Sharpe, Sortino, Volatility};
use fugazi::prelude::*;
use fugazi::types::{Real, Snapshot};

use super::strategy::SingleStrategySpec;
use crate::dyn_indicator::{self, DynIndicator};

/// The wallet / book seed for every embedded strategy. Arbitrary and positive
/// — the ratio metrics are scale-invariant in it (see the module docs).
const SEED: Real = 1_000.0;

/// Which trailing metric a [`build`] call constructs.
#[derive(Debug, Clone, Copy)]
pub(super) enum TrailingMetric {
    Sharpe,
    Sortino,
    Volatility,
    MaxDrawdown,
    Calmar,
}

/// A boxed real-valued source over the single-asset snapshot stream — the
/// erased form every trailing indicator collapses to.
type BoxedReal = Box<dyn Indicator<Input = Snapshot<String>, Output = Real>>;

/// A `Clone`-able wrapper around a non-`Clone` trailing indicator: it holds the
/// closure that builds a fresh instance (rebuilding the embedded strategy from
/// its spec) and rebuilds on every clone. See the module docs.
struct RebuildIndicator {
    build: Rc<dyn Fn() -> BoxedReal>,
    inner: BoxedReal,
}

impl Clone for RebuildIndicator {
    fn clone(&self) -> Self {
        let inner = (self.build)();
        Self {
            build: Rc::clone(&self.build),
            inner,
        }
    }
}

impl Indicator for RebuildIndicator {
    type Input = Snapshot<String>;
    type Output = Real;

    fn update(&mut self, input: Snapshot<String>) -> Option<Real> {
        self.inner.update(input)
    }

    fn value(&self) -> Option<Real> {
        self.inner.value()
    }

    fn warm_up_period(&self) -> usize {
        self.inner.warm_up_period()
    }

    fn unstable_period(&self) -> usize {
        self.inner.unstable_period()
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

/// Build the runtime-typed trailing indicator `metric` over the strategy
/// `spec` describes, reading a rolling `period`-bar window.
///
/// `risk_free_rate` (annualized fraction) is consumed only by
/// [`TrailingMetric::Sharpe`] / [`TrailingMetric::Sortino`]; `bars_per_year`
/// annualizes every metric except [`TrailingMetric::MaxDrawdown`]. `schema` is
/// the overlay schema the embedded strategy's `!get` leaves resolve against.
pub(super) fn build(
    metric: TrailingMetric,
    spec: &SingleStrategySpec,
    period: usize,
    risk_free_rate: Real,
    bars_per_year: Real,
    schema: &Arc<Schema>,
) -> Box<dyn DynIndicator> {
    let spec = Arc::new(spec.clone());
    let schema = Arc::clone(schema);
    let symbol = spec.symbol.clone();

    let build_fn: Rc<dyn Fn() -> BoxedReal> = Rc::new(move || {
        let strat = spec.build(SEED, &schema);
        let sym = symbol.clone();
        match metric {
            TrailingMetric::Sharpe => Box::new(Sharpe::new(
                strat,
                sym,
                SEED,
                period,
                risk_free_rate,
                bars_per_year,
            )),
            TrailingMetric::Sortino => Box::new(Sortino::new(
                strat,
                sym,
                SEED,
                period,
                risk_free_rate,
                bars_per_year,
            )),
            TrailingMetric::Volatility => {
                Box::new(Volatility::new(strat, sym, SEED, period, bars_per_year))
            }
            TrailingMetric::MaxDrawdown => Box::new(MaxDrawdown::new(strat, sym, SEED, period)),
            TrailingMetric::Calmar => {
                Box::new(Calmar::new(strat, sym, SEED, period, bars_per_year))
            }
        }
    });

    let inner = build_fn();
    dyn_indicator::wrap(RebuildIndicator {
        build: build_fn,
        inner,
    })
}
