//! One handle over every strategy shape that can be run.
//!
//! Five document shapes — single-asset, pairs, basket, multi-asset, portfolio —
//! each build to their own `Dyn*Strategy` wrapper. Those wrappers already have
//! the same surface: each implements
//! `Strategy<Input = Snapshot<String>, Symbol = String>` and exposes
//! `stable_bars()` / `warm_up_bars()`. Since a portfolio became an ordinary
//! strategy that trades the wallet it is handed, all five are driven the same
//! way, and the trait is purely the shared surface plus the save/restore seam.
//!
//! [`RunnableStrategy`] captures that surface, and is **object-safe** —
//! [`StrategySpec::try_build`] hands back a `Box<dyn RunnableStrategy>`, so the
//! driving methods on it cannot be generic. [`drive`](RunnableStrategy::drive)
//! and [`drive_resumable`](RunnableStrategy::drive_resumable) therefore build a
//! [`PaperWallet`] internally, which is what a backtest wants. To drive a spec
//! against an account you supply — a primed paper wallet, or a live venue —
//! reach for [`RunnableStrategyExt`], whose methods *are* generic over the
//! wallet and which is blanket-implemented for every `RunnableStrategy`
//! (including `dyn` ones). Both spellings share one body,
//! [`drive_over`].
//!
//! [`StrategySpec`] is the matching sum over the five spec types, with one
//! `try_build`.
//!
//! Everything downstream — the evaluate / measure / iterate family in
//! [`backtest`](super::backtest), the optimize kernel, the CLI runners, the
//! Python bindings — talks to these two rather than carrying a five-arm match
//! each. Adding a sixth shape means a variant and an impl, not ten new
//! functions.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::costs::TradingCosts;
use crate::market::{Real, Schema};
use crate::types::Snapshot;
use crate::wallet::{PaperWallet, Wallet};
use crate::{RunReport, Strategy};

/// The on-disk format version of a [`RunState`]. Bumped when the serialized
/// shape changes so a stale snapshot is rejected with a clear message rather
/// than mis-parsed.
///
/// **v2** — basket / multi / portfolio blobs gained required keys (the
/// rebalance gate on all three; the children, bar counter and weight-share
/// chains on a portfolio; in-flight netting state under `inner`). There is
/// deliberately no v1 → v2 migration: a v1 portfolio blob does not *contain*
/// its children's state, so a migration could only fabricate it. Re-run the
/// history to regenerate (resuming optimizes that, it doesn't replace it), or
/// finish the run on the build that wrote the state.
pub const RUN_STATE_FORMAT_VERSION: u32 = 2;

/// A persisted run — everything needed to rebuild a strategy from its spec and
/// continue it over new bars with identical behavior.
///
/// The strategy *structure* is not stored here; it is rebuilt from the spec
/// document. What is stored is the runtime *state*: the strategy's serialized
/// indicator/position/book state and the wallet's cash / positions / resting
/// orders. Written as JSON (via [`serde_json`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    /// Schema version; see [`RUN_STATE_FORMAT_VERSION`].
    pub format_version: u32,
    /// The strategy shape (`StrategySpec::kind`) this state was captured from.
    /// A resume into a different shape is rejected.
    pub kind: String,
    /// Timestamp (UTC ms) of the last bar processed when the state was
    /// captured, when known — used to warn about a gap/overlap on resume.
    pub last_bar: Option<i64>,
    /// Total bars the strategy had seen at capture (informational).
    pub bars_seen: usize,
    /// The strategy's serialized state (see
    /// [`RunnableStrategy::save_state`]).
    pub strategy: serde_json::Value,
    /// The account's serialized state (see
    /// [`Wallet::snapshot_state`]).
    ///
    /// [`Null`](serde_json::Value::Null) when the run traded a **live** wallet:
    /// the venue owns the positions and the cash, so they are re-read on resume
    /// rather than replayed from a snapshot that may have gone stale. A paper
    /// run stores the full book.
    pub wallet: serde_json::Value,
}

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
    fn stable_bars(&self) -> usize;

    /// Warm-up only, ignoring IIR settling tails. The `--keep-unstable`
    /// twin of [`stable_bars`](Self::stable_bars).
    fn warm_up_bars(&self) -> usize;

    /// Drive this strategy over `snapshots` to completion against a fresh
    /// [`PaperWallet`] primed with `per_symbol_costs`, and return the run
    /// report.
    ///
    /// To supply the account yourself — a pre-primed paper wallet, or a live
    /// venue — use [`RunnableStrategyExt::drive_resumable_with`]. This spelling
    /// exists because the trait is object-safe and so cannot carry a method
    /// generic over the wallet.
    fn drive(
        &mut self,
        snapshots: &[Snapshot<String>],
        cash: Real,
        per_symbol_costs: &[(String, TradingCosts)],
    ) -> RunReport<String> {
        // Resume-free path: no state to restore, no state to surface. `None` can
        // never produce a restore error, so the unwrap is unreachable.
        self.drive_resumable(snapshots, cash, per_symbol_costs, None, false)
            .expect("drive with no resume state cannot fail")
            .0
    }

    /// Serialize this strategy's full runtime state for run resuming — every
    /// wired indicator/signal chain plus the shared `Position`(s) and `Book`(s).
    /// The default is [`Null`](serde_json::Value::Null) (a strategy that opts
    /// out of resuming); every spec-built shape overrides it.
    fn save_state(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    /// Restore state produced by [`save_state`](Self::save_state) into this
    /// freshly-built strategy. Default: accept and ignore.
    fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        let _ = state;
        Ok(())
    }

    /// Drive this strategy against a fresh [`PaperWallet`], optionally restoring
    /// `resume` state first and surfacing the final [`RunState`] after — the
    /// resumable superset of [`drive`](Self::drive).
    ///
    /// With `resume = Some(state)`, the wallet and strategy are restored from it
    /// before the first bar. With `flatten = true`, any position still open at
    /// the end is closed at the last bar — in the wallet, not just the report —
    /// so `reconstruct_trades`/metrics count it and the returned state holds a
    /// flat book that a later resume continues from.
    ///
    /// [`RunnableStrategyExt::drive_resumable_with`] is the same thing over a
    /// wallet you supply.
    fn drive_resumable(
        &mut self,
        snapshots: &[Snapshot<String>],
        cash: Real,
        per_symbol_costs: &[(String, TradingCosts)],
        resume: Option<&RunState>,
        flatten: bool,
    ) -> Result<(RunReport<String>, RunState), String> {
        let mut wallet: PaperWallet<String> = PaperWallet::new(cash);
        for (sym, costs) in per_symbol_costs {
            let _ = wallet.set_costs_for(sym.clone(), costs.clone());
        }
        drive_over(self, snapshots, &mut wallet, resume, flatten)
    }

    /// The shape's name, used to stamp and validate a [`RunState`]. Mirrors
    /// [`StrategySpec::kind`].
    fn spec_kind(&self) -> &'static str;
}

/// Drive `strategy` over `snapshots` against `wallet`, restoring `resume` first
/// if given and returning the report plus the state to resume from next time.
///
/// The one body behind both [`RunnableStrategy::drive_resumable`] (which builds
/// a [`PaperWallet`] and calls this) and
/// [`RunnableStrategyExt::drive_resumable_with`] (which passes the caller's).
/// It is a free function rather than a trait method because it is generic over
/// the wallet, and `RunnableStrategy` has to stay object-safe.
///
/// The caller owns the account: seed it, prime its costs, and — for a live
/// venue — refresh it from the broker before calling. Nothing here creates or
/// configures a wallet.
pub fn drive_over<W>(
    strategy: &mut (impl RunnableStrategy + ?Sized),
    snapshots: &[Snapshot<String>],
    wallet: &mut W,
    resume: Option<&RunState>,
    flatten: bool,
) -> Result<(RunReport<String>, RunState), String>
where
    W: Wallet<String>,
{
    if let Some(state) = resume {
        if state.format_version != RUN_STATE_FORMAT_VERSION {
            return Err(format!(
                "!resume > state format version {} does not match this build's {}",
                state.format_version, RUN_STATE_FORMAT_VERSION
            ));
        }
        if state.kind != strategy.spec_kind() {
            return Err(format!(
                "!resume > state is for a `{}` strategy but this document is `{}`",
                state.kind,
                strategy.spec_kind()
            ));
        }
        strategy
            .restore_state(&state.strategy)
            .map_err(|e| format!("!resume > strategy > {e}"))?;
        // A live wallet's default `restore_state` ignores this and re-reads the
        // venue — see `Wallet::snapshot_state`.
        wallet
            .restore_state(&state.wallet)
            .map_err(|e| format!("!resume > wallet > {e}"))?;
    }
    let mut report = crate::backtest::run(strategy, wallet, snapshots.iter().cloned());
    if flatten {
        crate::backtest::flatten_open_positions(strategy, wallet, snapshots, &mut report);
    }
    let last_bar = snapshots
        .last()
        .and_then(|snap| snap.iter().find_map(|(_, _, atom)| atom.time))
        .map(|t| t.0);
    let final_state = RunState {
        format_version: RUN_STATE_FORMAT_VERSION,
        kind: strategy.spec_kind().to_string(),
        last_bar,
        bars_seen: resume.map(|r| r.bars_seen).unwrap_or(0) + snapshots.len(),
        strategy: RunnableStrategy::save_state(strategy),
        wallet: wallet.snapshot_state(),
    };
    Ok((report, final_state))
}

/// The wallet-generic half of [`RunnableStrategy`].
///
/// `RunnableStrategy` is object-safe on purpose — [`StrategySpec::try_build`]
/// returns a `Box<dyn RunnableStrategy>` — which rules out a method generic over
/// the wallet type. These live here instead, blanket-implemented for every
/// `RunnableStrategy` including `?Sized` ones, so they are callable straight on
/// a `Box<dyn RunnableStrategy>`:
///
/// ```no_run
/// # use fugazi::spec::{RunnableStrategyExt, StrategySpec};
/// # fn go(spec: &StrategySpec, snaps: &[fugazi::Snapshot<String>], wallet: &mut fugazi::PaperWallet<String>) -> Result<(), String> {
/// let mut built = spec.try_build(10_000.0, &fugazi::market::Schema::empty(), None)?;
/// let (report, state) = built.drive_resumable_with(snaps, wallet, None, false)?;
/// # let _ = (report, state);
/// # Ok(())
/// # }
/// ```
///
/// This is what makes a spec — a portfolio included — runnable against a live
/// venue: [`backtest::run`](crate::backtest::run) is already generic over the
/// wallet and already drains [`Wallet::poll_fills`] each bar, so an
/// `OkxWallet` / `CoinbaseWallet` drops in with no further plumbing.
pub trait RunnableStrategyExt: RunnableStrategy {
    /// [`RunnableStrategy::drive_resumable`] over a wallet you supply — a paper
    /// wallet primed however you like, or a live venue.
    ///
    /// The wallet's own state round-trips through
    /// [`Wallet::snapshot_state`] / [`Wallet::restore_state`], so a live account
    /// (whose default is `Null` / accept-and-ignore) resumes its *strategy*
    /// state while reading positions and cash from the broker.
    fn drive_resumable_with<W: Wallet<String>>(
        &mut self,
        snapshots: &[Snapshot<String>],
        wallet: &mut W,
        resume: Option<&RunState>,
        flatten: bool,
    ) -> Result<(RunReport<String>, RunState), String>;

    /// Advance this strategy over `snapshots` **without trading**, returning the
    /// state to resume from — the "warm but don't trade" entry point.
    ///
    /// Every chain advances and the wallet is marked to market exactly as in a
    /// real run, but [`Strategy::trade`] is never
    /// called, so no order is submitted. That is what closes a *pause gap*: bars
    /// that elapsed while a deployment was stopped should warm indicators
    /// without booking trades at prices nobody could have traded at. Replay the
    /// gap through here, then hand the returned state to
    /// [`drive_resumable_with`](Self::drive_resumable_with) and go live —
    /// instead of discarding the snapshot and re-serving a long-period
    /// indicator's whole warm-up after every pause.
    ///
    /// No [`RunReport`]: nothing happened worth reporting. Fills that arrive
    /// anyway (a resting order left over from before the pause) still route to
    /// [`Strategy::on_fill`], or the strategy's
    /// position would drift from the account's.
    fn warm_up_over<W: Wallet<String>>(
        &mut self,
        snapshots: &[Snapshot<String>],
        wallet: &mut W,
        resume: Option<&RunState>,
    ) -> Result<RunState, String>;
}

impl<T: RunnableStrategy + ?Sized> RunnableStrategyExt for T {
    fn drive_resumable_with<W: Wallet<String>>(
        &mut self,
        snapshots: &[Snapshot<String>],
        wallet: &mut W,
        resume: Option<&RunState>,
        flatten: bool,
    ) -> Result<(RunReport<String>, RunState), String> {
        drive_over(self, snapshots, wallet, resume, flatten)
    }

    fn warm_up_over<W: Wallet<String>>(
        &mut self,
        snapshots: &[Snapshot<String>],
        wallet: &mut W,
        resume: Option<&RunState>,
    ) -> Result<RunState, String> {
        warm_up_over_wallet(self, snapshots, wallet, resume)
    }
}

/// The body behind [`RunnableStrategyExt::warm_up_over`]; see there.
fn warm_up_over_wallet<W: Wallet<String>>(
    strategy: &mut (impl RunnableStrategy + ?Sized),
    snapshots: &[Snapshot<String>],
    wallet: &mut W,
    resume: Option<&RunState>,
) -> Result<RunState, String> {
    if let Some(state) = resume {
        if state.format_version != RUN_STATE_FORMAT_VERSION {
            return Err(format!(
                "!resume > state format version {} does not match this build's {}",
                state.format_version, RUN_STATE_FORMAT_VERSION
            ));
        }
        if state.kind != strategy.spec_kind() {
            return Err(format!(
                "!resume > state is for a `{}` strategy but this document is `{}`",
                state.kind,
                strategy.spec_kind()
            ));
        }
        strategy
            .restore_state(&state.strategy)
            .map_err(|e| format!("!resume > strategy > {e}"))?;
        wallet
            .restore_state(&state.wallet)
            .map_err(|e| format!("!resume > wallet > {e}"))?;
    }
    crate::backtest::warm_up(strategy, wallet, snapshots.iter().cloned());
    let last_bar = snapshots
        .last()
        .and_then(|snap| snap.iter().find_map(|(_, _, atom)| atom.time))
        .map(|t| t.0);
    Ok(RunState {
        format_version: RUN_STATE_FORMAT_VERSION,
        kind: strategy.spec_kind().to_string(),
        last_bar,
        bars_seen: resume.map(|r| r.bars_seen).unwrap_or(0) + snapshots.len(),
        strategy: RunnableStrategy::save_state(strategy),
        wallet: wallet.snapshot_state(),
    })
}

impl RunnableStrategy for DynSingleStrategy {
    fn stable_bars(&self) -> usize {
        DynSingleStrategy::stable_bars(self)
    }
    fn warm_up_bars(&self) -> usize {
        DynSingleStrategy::warm_up_bars(self)
    }
    fn spec_kind(&self) -> &'static str {
        "single"
    }
    fn save_state(&self) -> serde_json::Value {
        DynSingleStrategy::save_state(self)
    }
    fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        DynSingleStrategy::restore_state(self, state)
    }
}

impl RunnableStrategy for DynPairsStrategy {
    fn stable_bars(&self) -> usize {
        DynPairsStrategy::stable_bars(self)
    }
    fn warm_up_bars(&self) -> usize {
        DynPairsStrategy::warm_up_bars(self)
    }
    fn spec_kind(&self) -> &'static str {
        "pairs"
    }
    fn save_state(&self) -> serde_json::Value {
        DynPairsStrategy::save_state(self)
    }
    fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        DynPairsStrategy::restore_state(self, state)
    }
}

impl RunnableStrategy for DynBasketStrategy {
    fn stable_bars(&self) -> usize {
        DynBasketStrategy::stable_bars(self)
    }
    fn warm_up_bars(&self) -> usize {
        DynBasketStrategy::warm_up_bars(self)
    }
    fn spec_kind(&self) -> &'static str {
        "basket"
    }
    fn save_state(&self) -> serde_json::Value {
        DynBasketStrategy::save_state(self)
    }
    fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        DynBasketStrategy::restore_state(self, state)
    }
    // Basket lazy-builds per-symbol chains on first sight, so restore also
    // restores per-symbol state as each symbol reappears — see
    // [`DynBasketStrategy::restore_state`].
}

impl RunnableStrategy for DynMultiAssetStrategy {
    fn stable_bars(&self) -> usize {
        DynMultiAssetStrategy::stable_bars(self)
    }
    fn warm_up_bars(&self) -> usize {
        DynMultiAssetStrategy::warm_up_bars(self)
    }
    fn spec_kind(&self) -> &'static str {
        "multi"
    }
    fn save_state(&self) -> serde_json::Value {
        DynMultiAssetStrategy::save_state(self)
    }
    fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        DynMultiAssetStrategy::restore_state(self, state)
    }
}

impl RunnableStrategy for DynPortfolio {
    fn stable_bars(&self) -> usize {
        DynPortfolio::stable_bars(self)
    }
    fn warm_up_bars(&self) -> usize {
        DynPortfolio::warm_up_bars(self)
    }
    fn spec_kind(&self) -> &'static str {
        "portfolio"
    }
    fn save_state(&self) -> serde_json::Value {
        DynPortfolio::save_state(self)
    }
    fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        DynPortfolio::restore_state(self, state)
    }
    // Uses the default `drive`/`drive_resumable`: a portfolio is now an ordinary
    // strategy that trades the wallet it is handed, so it takes the same
    // `PaperWallet` primed with per-symbol costs as the other four shapes.
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
    /// `costs` is vestigial: every shape now takes its costs from the wallet it
    /// is driven with (see [`RunnableStrategy::drive`]), portfolio included, so
    /// all five arms ignore it. Kept for call-site symmetry.
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
