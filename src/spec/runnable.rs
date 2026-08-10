//! One handle over every strategy shape that can be run.
//!
//! Five document shapes — single-asset, pairs, basket, multi-asset, portfolio —
//! each build to their own `Dyn*Strategy` wrapper. Those wrappers already have
//! the same surface: each implements
//! `Strategy<Input = Snapshot<String>, Symbol = String>` and exposes
//! `stable_period()` / `warm_up_period()`. The only genuine divergence is how a
//! run is *driven*: four go through a plain [`PaperWallet`] primed with
//! per-symbol costs, while a portfolio owns a composite wallet with one
//! sub-wallet per child and takes its costs at build time instead.
//!
//! [`RunnableStrategy`] captures exactly that: the shared surface as required
//! methods, and the wallet difference as [`drive`](RunnableStrategy::drive),
//! whose default body is the `PaperWallet` path and which the portfolio
//! overrides. The name is for what the trait *enables* — driving a strategy to
//! completion, the same act [`backtest::run`](crate::backtest::run) performs —
//! rather than for where the value came from.
//! [`StrategySpec`] is the matching sum over the five spec types, with one
//! `try_build`.
//!
//! Everything downstream — the evaluate / measure / iterate family in
//! [`backtest`](super::backtest), the optimize kernel, the CLI runners, the
//! Python bindings — talks to these two rather than carrying a five-arm match
//! each. Adding a sixth shape means a variant and an impl, not ten new
//! functions.

use std::sync::Arc;

use crate::costs::TradingCosts;
use crate::market::{Real, Schema};
use crate::types::Snapshot;
use crate::wallet::{PaperWallet, Wallet};
use crate::{RunReport, Strategy};

use super::basket::{BasketStrategySpec, DynBasketStrategy};
use super::multi_asset::{DynMultiAssetStrategy, MultiAssetStrategySpec};
use super::pairs::{DynPairsStrategy, PairsStrategySpec};
use super::portfolio::{DynPortfolio, PortfolioSpec};
use super::preset::StrategyRef;
use super::strategy::DynSingleStrategy;

/// A strategy of any shape, ready to be driven over a snapshot stream.
///
/// Object-safe: the associated types of [`Strategy`] are pinned to the
/// `String`-keyed snapshot space every spec-driven strategy runs in, so
/// `Box<dyn RunnableStrategy>` is a usable handle.
pub trait RunnableStrategy: Strategy<Input = Snapshot<String>, Symbol = String> {
    /// Samples before every wired chain is both warmed and settled — what
    /// `optimize --walkforward` skips at the head of the series.
    fn stable_period(&self) -> usize;

    /// Warm-up only, ignoring IIR settling tails. The `--keep-unstable`
    /// twin of [`stable_period`](Self::stable_period).
    fn warm_up_period(&self) -> usize;

    /// Drive this strategy over `snapshots` to completion and return the run
    /// report.
    ///
    /// The wallet is the strategy's business, not the caller's: four shapes
    /// want a [`PaperWallet`] primed with `per_symbol_costs` (the default body
    /// here), and a portfolio must be driven through its own composite view
    /// with costs already baked into each sub-wallet at build time.
    fn drive(
        &mut self,
        snapshots: &[Snapshot<String>],
        cash: Real,
        per_symbol_costs: &[(String, TradingCosts)],
    ) -> RunReport<String> {
        let mut wallet: PaperWallet<String> = PaperWallet::new(cash);
        for (sym, costs) in per_symbol_costs {
            // Always Ok on a PaperWallet; the Result exists for wallets
            // whose fees the venue owns.
            let _ = wallet.set_costs_for(sym.clone(), costs.clone());
        }
        crate::backtest::run(self, &mut wallet, snapshots.iter().cloned())
    }
}

impl RunnableStrategy for DynSingleStrategy {
    fn stable_period(&self) -> usize {
        DynSingleStrategy::stable_period(self)
    }
    fn warm_up_period(&self) -> usize {
        DynSingleStrategy::warm_up_period(self)
    }
}

impl RunnableStrategy for DynPairsStrategy {
    fn stable_period(&self) -> usize {
        DynPairsStrategy::stable_period(self)
    }
    fn warm_up_period(&self) -> usize {
        DynPairsStrategy::warm_up_period(self)
    }
}

impl RunnableStrategy for DynBasketStrategy {
    fn stable_period(&self) -> usize {
        DynBasketStrategy::stable_period(self)
    }
    fn warm_up_period(&self) -> usize {
        DynBasketStrategy::warm_up_period(self)
    }
}

impl RunnableStrategy for DynMultiAssetStrategy {
    fn stable_period(&self) -> usize {
        DynMultiAssetStrategy::stable_period(self)
    }
    fn warm_up_period(&self) -> usize {
        DynMultiAssetStrategy::warm_up_period(self)
    }
}

impl RunnableStrategy for DynPortfolio {
    fn stable_period(&self) -> usize {
        DynPortfolio::stable_period(self)
    }
    fn warm_up_period(&self) -> usize {
        DynPortfolio::warm_up_period(self)
    }
    // Uses the default `drive`: a portfolio is now an ordinary strategy that
    // trades the wallet it is handed, so it takes the same `PaperWallet` primed
    // with per-symbol costs as the other four shapes.
}

/// The five spec shapes as one type, so a driver takes a strategy document
/// without caring which it is.
///
/// The single-asset arm holds a [`StrategyRef`] rather than a
/// `SingleStrategySpec` so a preset tag (`!ma_crossover { … }`) is accepted
/// wherever a spelled-out document is.
#[derive(Debug, Clone)]
pub enum StrategySpec {
    Single(Box<StrategyRef>),
    Pairs(Box<PairsStrategySpec>),
    Basket(Box<BasketStrategySpec>),
    Multi(Box<MultiAssetStrategySpec>),
    Portfolio(Box<PortfolioSpec>),
}

impl StrategySpec {
    /// The shape's name, as the CLI prefix and Python's `kind` spell it.
    pub fn kind(&self) -> &'static str {
        match self {
            StrategySpec::Single(_) => "single",
            StrategySpec::Pairs(_) => "pairs",
            StrategySpec::Basket(_) => "basket",
            StrategySpec::Multi(_) => "multi",
            StrategySpec::Portfolio(_) => "portfolio",
        }
    }

    /// Build the live strategy, reporting a malformed document as an `Err`
    /// carrying its `!tag > ` breadcrumb (see
    /// [`NodeSpec::try_build`](super::expr::NodeSpec::try_build)).
    ///
    /// `costs` is only consulted by the portfolio arm, which bakes a bundle
    /// into each sub-wallet at construction rather than priming a wallet
    /// afterwards; the other four ignore it and take their costs through
    /// [`RunnableStrategy::drive`].
    pub fn try_build(
        &self,
        cash: Real,
        schema: &Arc<Schema>,
        costs: Option<TradingCosts>,
    ) -> Result<Box<dyn RunnableStrategy>, String> {
        Ok(match self {
            StrategySpec::Single(s) => Box::new(s.try_build(cash, schema)?),
            StrategySpec::Pairs(s) => Box::new(s.try_build(cash, schema)?),
            StrategySpec::Basket(s) => Box::new(s.try_build(cash, schema)?),
            StrategySpec::Multi(s) => Box::new(s.try_build(cash, schema)?),
            StrategySpec::Portfolio(s) => Box::new(s.try_build(cash, schema, costs)?),
        })
    }

    /// Build with this run's costs applied the way the shape needs them.
    ///
    /// Every shape — portfolio included, now that it trades the wallet it is
    /// handed like the other four — takes its costs through
    /// [`RunnableStrategy::drive`], which primes the `PaperWallet` with the
    /// per-symbol bundles. So this is exactly [`try_build`](Self::try_build); the
    /// cost/universe arguments are kept for call-site symmetry and a future shape
    /// that genuinely needs build-time costs.
    pub fn try_build_priced(
        &self,
        cash: Real,
        schema: &Arc<Schema>,
        _cost_config: &super::costs::CostConfig,
        _frequency: Option<crate::time::Frequency>,
        _universe: &[String],
    ) -> Result<Box<dyn RunnableStrategy>, String> {
        self.try_build(cash, schema, None)
    }

    /// The symbols this strategy may trade — the set per-symbol cost bundles
    /// are resolved for.
    ///
    /// The two shapes that name their symbols up front say so exactly; the
    /// N-symbol shapes discover theirs from the stream, which is also what
    /// their runners already did.
    pub fn universe(&self, snapshots: &[Snapshot<String>]) -> Vec<String> {
        match self {
            StrategySpec::Single(s) => vec![s.symbol().to_string()],
            StrategySpec::Pairs(s) => vec![s.left.clone(), s.right.clone()],
            StrategySpec::Basket(_) | StrategySpec::Multi(_) | StrategySpec::Portfolio(_) => {
                super::backtest::universe_from_snapshots(snapshots)
            }
        }
    }
}
