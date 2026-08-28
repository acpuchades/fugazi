//! One handle over every strategy shape that can be run.
//!
//! Five document shapes — single-asset, pairs, basket, multi-asset, portfolio —
//! each build to their own `Dyn*Strategy` wrapper. Those wrappers already have
//! the same surface: each implements
//! `Strategy<Input = Snapshot<Symbol>, Symbol = Symbol>` and exposes
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

use crate::backtest::Closeout;
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
use super::meta::Meta;
use super::multi_asset::{DynMultiAssetStrategy, MultiAssetStrategySpec};
use super::pairs::{DynPairsStrategy, PairsStrategySpec};
use super::portfolio::{DynPortfolio, PortfolioSpec};
use super::preset::StrategyRef;
use super::strategy::DynSingleStrategy;
use crate::types::Symbol;

/// A strategy of any shape, ready to be driven over a snapshot stream.
///
/// Object-safe: the associated types of [`Strategy`] are pinned to the
/// `String`-keyed snapshot space every spec-driven strategy runs in, so
/// `Box<dyn RunnableStrategy>` is a usable handle.
///
/// **`Send` is a supertrait** so that handle can cross a thread boundary — which
/// is what lets the Python bindings drop the GIL for the duration of a run
/// instead of blocking every other thread in the process. It costs the five
/// implementors nothing: each is already `Send`, since every shared handle in
/// the crate is an `Arc<Mutex<…>>` and [`DynIndicator`](crate::runtime::DynIndicator)
/// is declared `Send + Sync`. Declaring it here is what makes the compiler
/// *check* that, rather than the bindings assuming it.
pub trait RunnableStrategy: Strategy<Input = Snapshot<Symbol>, Symbol = Symbol> + Send {
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
        snapshots: &[Snapshot<Symbol>],
        cash: Real,
        per_symbol_costs: &[(String, TradingCosts)],
    ) -> RunReport<Symbol> {
        // Resume-free path: no state to restore, no state to surface. `None` can
        // never produce a restore error, so the unwrap is unreachable.
        self.drive_resumable(snapshots, cash, per_symbol_costs, None, &Closeout::Carry)
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

    /// The signed units **this strategy** believes it holds, per symbol — its
    /// [`Book`](crate::indicators::Book)'s non-flat legs.
    ///
    /// Not the same question as [`Wallet::positions`], and the gap between the
    /// two answers is what it is for: an account may hold positions this
    /// strategy never opened, and a
    /// [`SleeveWallet`](crate::wallet::SleeveWallet) exists to keep those out
    /// of its sight. Deciding *which* of the account's positions are foreign
    /// means asking the strategy what is already its own — trivially nothing on
    /// a cold start, but not after
    /// [`restore_state`](Self::restore_state) has walked a resumed strategy
    /// back in holding last chunk's position. See
    /// [`external_baseline_net_of`](crate::wallet::external_baseline_net_of),
    /// which is the caller this exists for.
    ///
    /// Required rather than defaulted: a sixth shape that answered "nothing"
    /// by omission would resume into a sleeve that hid its own position from
    /// it, which is a silently wrong backtest rather than a loud one.
    fn owned_positions(&self) -> Vec<crate::wallet::Units<Symbol>>;

    /// Drive this strategy against a fresh [`PaperWallet`], optionally restoring
    /// `resume` state first and surfacing the final [`RunState`] after — the
    /// resumable superset of [`drive`](Self::drive).
    ///
    /// With `resume = Some(state)`, the wallet and strategy are restored from it
    /// before the first bar. `closeout` says what to do with whatever is still
    /// open once the last bar has been driven — nothing
    /// ([`Carry`](crate::backtest::Closeout::Carry), the backtest's answer),
    /// close everything
    /// ([`Flatten`](crate::backtest::Closeout::Flatten)), or drive named
    /// symbols to given targets ([`Hold`](crate::backtest::Closeout::Hold)).
    /// Anything but `Carry` acts on the wallet, not just the report, so
    /// `reconstruct_trades`/metrics count the closing legs and the returned
    /// state is the book a later resume actually continues from.
    ///
    /// [`RunnableStrategyExt::drive_resumable_with`] is the same thing over a
    /// wallet you supply.
    fn drive_resumable(
        &mut self,
        snapshots: &[Snapshot<Symbol>],
        cash: Real,
        per_symbol_costs: &[(String, TradingCosts)],
        resume: Option<&RunState>,
        closeout: &Closeout,
    ) -> Result<(RunReport<Symbol>, RunState), String> {
        self.drive_resumable_warmed(snapshots, 0, cash, per_symbol_costs, resume, closeout)
    }

    /// [`drive_resumable`](Self::drive_resumable) with the first `warmup`
    /// snapshots used to **warm the chains and nothing else**.
    ///
    /// Across that prefix [`Strategy::trade`] is never
    /// called, so no order is submitted and no equity is booked; every
    /// indicator still advances and the wallet is still marked to market. The
    /// returned [`RunReport`] therefore covers only `snapshots[warmup..]` — one
    /// equity point per *evaluated* bar — which is what lets `--from` read bars
    /// back out of the series to settle a strategy without those bars landing
    /// in the metrics.
    ///
    /// The two halves share one wallet and one strategy instance, so nothing
    /// round-trips through a [`RunState`] in between: the warmed chains are
    /// already in memory when the evaluated half begins. `resume` is restored
    /// once, before the warm-up prefix.
    fn drive_resumable_warmed(
        &mut self,
        snapshots: &[Snapshot<Symbol>],
        warmup: usize,
        cash: Real,
        per_symbol_costs: &[(String, TradingCosts)],
        resume: Option<&RunState>,
        closeout: &Closeout,
    ) -> Result<(RunReport<Symbol>, RunState), String> {
        let mut wallet: PaperWallet<Symbol> = PaperWallet::new(cash);
        for (sym, costs) in per_symbol_costs {
            let _ = wallet.set_costs_for(crate::types::symbol(sym), costs.clone());
        }
        drive_warmed_over_wallet(self, snapshots, warmup, &mut wallet, resume, closeout)
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
    snapshots: &[Snapshot<Symbol>],
    wallet: &mut W,
    resume: Option<&RunState>,
    closeout: &Closeout,
) -> Result<(RunReport<Symbol>, RunState), String>
where
    W: Wallet<Symbol>,
{
    if let Some(state) = resume {
        check_resumable(state, strategy.spec_kind())?;
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
    crate::backtest::apply_closeout(strategy, wallet, snapshots, &mut report, closeout);
    let last_bar = last_bar_of(snapshots, resume);
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
/// # use fugazi::backtest::Closeout;
/// # use fugazi::spec::{RunnableStrategyExt, StrategySpec};
/// # use fugazi::types::Symbol;
/// # fn go(spec: &StrategySpec, snaps: &[fugazi::Snapshot<Symbol>], wallet: &mut fugazi::PaperWallet<Symbol>) -> Result<(), String> {
/// let mut built = spec.try_build(10_000.0, &fugazi::market::Schema::empty(), None)?;
/// let (report, state) = built.drive_resumable_with(snaps, wallet, None, &Closeout::Carry)?;
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
    fn drive_resumable_with<W: Wallet<Symbol>>(
        &mut self,
        snapshots: &[Snapshot<Symbol>],
        wallet: &mut W,
        resume: Option<&RunState>,
        closeout: &Closeout,
    ) -> Result<(RunReport<Symbol>, RunState), String>;

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
    fn warm_up_over<W: Wallet<Symbol>>(
        &mut self,
        snapshots: &[Snapshot<Symbol>],
        wallet: &mut W,
        resume: Option<&RunState>,
    ) -> Result<RunState, String>;

    /// [`RunnableStrategy::drive_resumable_warmed`] over a wallet you supply —
    /// the warm-up-prefix form of
    /// [`drive_resumable_with`](Self::drive_resumable_with), and the exact body
    /// the fresh-`PaperWallet` spelling delegates to once it has built one.
    ///
    /// Reach for it when the account needs configuring beyond the cash it
    /// starts with — a leverage cap
    /// ([`PaperWallet::with_max_gross`](crate::PaperWallet::with_max_gross)), a
    /// currency label, a retention bound — while still wanting `--from`'s
    /// "warm the chains over this prefix, evaluate the rest" split. Composing
    /// [`warm_up_over`](Self::warm_up_over) and `drive_resumable_with` by hand
    /// gets the `resume` handling subtly wrong: the state is restored once,
    /// before the prefix, and restoring it again would rewind what the warm-up
    /// just advanced.
    fn drive_warmed_over<W: Wallet<Symbol>>(
        &mut self,
        snapshots: &[Snapshot<Symbol>],
        warmup: usize,
        wallet: &mut W,
        resume: Option<&RunState>,
        closeout: &Closeout,
    ) -> Result<(RunReport<Symbol>, RunState), String>;
}

impl<T: RunnableStrategy + ?Sized> RunnableStrategyExt for T {
    fn drive_resumable_with<W: Wallet<Symbol>>(
        &mut self,
        snapshots: &[Snapshot<Symbol>],
        wallet: &mut W,
        resume: Option<&RunState>,
        closeout: &Closeout,
    ) -> Result<(RunReport<Symbol>, RunState), String> {
        drive_over(self, snapshots, wallet, resume, closeout)
    }

    fn warm_up_over<W: Wallet<Symbol>>(
        &mut self,
        snapshots: &[Snapshot<Symbol>],
        wallet: &mut W,
        resume: Option<&RunState>,
    ) -> Result<RunState, String> {
        warm_up_over_wallet(self, snapshots, wallet, resume)
    }

    fn drive_warmed_over<W: Wallet<Symbol>>(
        &mut self,
        snapshots: &[Snapshot<Symbol>],
        warmup: usize,
        wallet: &mut W,
        resume: Option<&RunState>,
        closeout: &Closeout,
    ) -> Result<(RunReport<Symbol>, RunState), String> {
        drive_warmed_over_wallet(self, snapshots, warmup, wallet, resume, closeout)
    }
}

/// The body behind [`RunnableStrategyExt::drive_warmed_over`], and — once it
/// has built its own [`PaperWallet`] — behind
/// [`RunnableStrategy::drive_resumable_warmed`] too. One body, so the warm-up
/// split and the `resume`-once rule cannot drift between the two spellings.
fn drive_warmed_over_wallet<W: Wallet<Symbol>>(
    strategy: &mut (impl RunnableStrategy + ?Sized),
    snapshots: &[Snapshot<Symbol>],
    warmup: usize,
    wallet: &mut W,
    resume: Option<&RunState>,
    closeout: &Closeout,
) -> Result<(RunReport<Symbol>, RunState), String> {
    let (warm, evaluated) = snapshots.split_at(warmup.min(snapshots.len()));
    if warm.is_empty() {
        return drive_over(strategy, evaluated, wallet, resume, closeout);
    }
    warm_up_over_wallet(strategy, warm, wallet, resume)?;
    // `resume` is already applied — restoring it a second time would rewind
    // the state the warm-up just advanced.
    drive_over(strategy, evaluated, wallet, None, closeout)
}

/// The body behind [`RunnableStrategyExt::warm_up_over`]; see there.
fn warm_up_over_wallet<W: Wallet<Symbol>>(
    strategy: &mut (impl RunnableStrategy + ?Sized),
    snapshots: &[Snapshot<Symbol>],
    wallet: &mut W,
    resume: Option<&RunState>,
) -> Result<RunState, String> {
    if let Some(state) = resume {
        check_resumable(state, strategy.spec_kind())?;
        strategy
            .restore_state(&state.strategy)
            .map_err(|e| format!("!resume > strategy > {e}"))?;
        wallet
            .restore_state(&state.wallet)
            .map_err(|e| format!("!resume > wallet > {e}"))?;
    }
    crate::backtest::warm_up(strategy, wallet, snapshots.iter().cloned());
    let last_bar = last_bar_of(snapshots, resume);
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
    fn owned_positions(&self) -> Vec<crate::wallet::Units<Symbol>> {
        DynSingleStrategy::book(self).owned_positions()
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
    fn owned_positions(&self) -> Vec<crate::wallet::Units<Symbol>> {
        DynPairsStrategy::book(self).owned_positions()
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
    fn owned_positions(&self) -> Vec<crate::wallet::Units<Symbol>> {
        DynBasketStrategy::book(self).owned_positions()
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
    fn owned_positions(&self) -> Vec<crate::wallet::Units<Symbol>> {
        DynMultiAssetStrategy::book(self).owned_positions()
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
    /// Off the child **ledgers**, not off the aggregate book: a portfolio's
    /// book is mark-driven and never sees a fill, so its legs stay empty no
    /// matter what the portfolio is holding. See
    /// [`Portfolio::owned_positions`](crate::portfolio::Portfolio::owned_positions).
    fn owned_positions(&self) -> Vec<crate::wallet::Units<Symbol>> {
        DynPortfolio::owned_positions(self)
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

/// The timestamp to stamp a captured [`RunState`] with: this chunk's last
/// timestamped bar, or — if it had none — whatever the state being resumed
/// already said.
///
/// The fallback is what makes a **zero-bar** chunk a valid resume point. That is
/// not a hypothetical: replaying a pause gap that turned out to be empty, or
/// restoring a book to inspect it, hands `warm_up` an empty slice, and
/// recomputing `last_bar` from nothing would answer `None` — throwing away a
/// position in time the state already knew and leaving behind a state that
/// cannot be resumed from. Nothing was processed, so nothing about *when* we
/// are changed.
fn last_bar_of(snapshots: &[Snapshot<Symbol>], resume: Option<&RunState>) -> Option<i64> {
    snapshots
        .last()
        .and_then(|snap| snap.iter().find_map(|(_, _, atom)| atom.time))
        .map(|t| t.0)
        .or_else(|| resume.and_then(|r| r.last_bar))
}

/// The seed [`StrategySpec::positions_at_resume`] builds its throwaway probe
/// with.
///
/// Any strictly-positive number does: the probe exists only to have
/// [`RunnableStrategy::restore_state`] write a book into it, and a restore
/// replaces the book whole — `initial_equity` and all. It is *not* a fallback
/// for a real run's seed, which is checked (see `check_seed`).
pub const RESUME_PROBE_SEED: Real = 1.0;

/// Reject a [`RunState`] this build or this document cannot continue — a
/// format version from another build, or a state captured from a different
/// shape.
///
/// One body, called by every entry point that takes a `resume`, so the two
/// checks cannot drift apart or be forgotten by a third.
fn check_resumable(state: &RunState, kind: &str) -> Result<(), String> {
    if state.format_version != RUN_STATE_FORMAT_VERSION {
        return Err(format!(
            "!resume > state format version {} does not match this build's {}",
            state.format_version, RUN_STATE_FORMAT_VERSION
        ));
    }
    if state.kind != kind {
        return Err(format!(
            "!resume > state is for a `{}` strategy but this document is `{}`",
            state.kind, kind
        ));
    }
    Ok(())
}

/// Reject a seed a [`Book`](crate::indicators::Book) could not be built from,
/// as a *value* rather than an abort.
///
/// Every shape seeds its book with the capital the strategy is deemed to start
/// with, and `Book::new` asserts that is strictly positive — a real invariant,
/// since equity, drawdown and every book-anchored sizing recipe divide by it.
/// But the number reaching it comes from the caller's account, so a flat wallet,
/// a wallet whose whole balance is already deployed, or a
/// [`SleeveWallet`](crate::wallet::SleeveWallet) carve-out with nothing in it
/// all arrive here as zero. That is bad input, and input is reported.
///
/// `NaN` is spelled out rather than left to `<=`, which it reads false against:
/// a `NaN` seed would otherwise sail through and poison every equity reading
/// downstream of it.
pub(crate) fn check_seed(cash: Real) -> Result<(), String> {
    if cash.is_nan() || cash <= 0.0 {
        return Err(format!(
            "initial equity must be strictly positive, but the account reports {cash}"
        ));
    }
    Ok(())
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

    /// The document's free-form `meta:`, whatever shape it is — the one place
    /// a caller that took "any strategy document" can read the metadata an
    /// external service attached. fugazi never reads it itself; see
    /// [`spec::meta`](super::meta).
    pub fn meta(&self) -> Option<&Meta> {
        match self {
            StrategySpec::Single(s) => s.meta(),
            StrategySpec::Pairs(s) => s.meta.as_ref(),
            StrategySpec::Basket(s) => s.meta.as_ref(),
            StrategySpec::Multi(s) => s.meta.as_ref(),
            StrategySpec::Portfolio(s) => s.meta.as_ref(),
        }
    }

    /// Build the live strategy, reporting a malformed document as an `Err`
    /// carrying its `!tag > ` breadcrumb (see
    /// [`NodeSpec::try_build`](super::expr::NodeSpec::try_build)).
    ///
    /// `costs` is vestigial: every shape now takes its costs from the wallet it
    /// is driven with (see [`RunnableStrategy::drive`]), portfolio included, so
    /// all five arms ignore it. Kept for call-site symmetry.
    ///
    /// A non-positive `cash` is reported here, as an ordinary build error. It is
    /// the seed for the strategy's [`Book`](crate::indicators::Book), whose
    /// constructor asserts on it — and that assert is an internal invariant, not
    /// a diagnostic: it aborts, which across the Python boundary is a
    /// `PanicException` no `except Exception` can catch. An account with no
    /// capital to trade is *input*, so it gets the same treatment as any other
    /// bad document.
    pub fn try_build(
        &self,
        cash: Real,
        schema: &Arc<Schema>,
        costs: Option<TradingCosts>,
    ) -> Result<Box<dyn RunnableStrategy>, String> {
        check_seed(cash)?;
        Ok(match self {
            StrategySpec::Single(s) => Box::new(s.try_build(cash, schema)?),
            StrategySpec::Pairs(s) => Box::new(s.try_build(cash, schema)?),
            StrategySpec::Basket(s) => Box::new(s.try_build(cash, schema)?),
            StrategySpec::Multi(s) => Box::new(s.try_build(cash, schema)?),
            StrategySpec::Portfolio(s) => Box::new(s.try_build(cash, schema, costs)?),
        })
    }

    /// The positions a run resuming from `resume` walks back in **already
    /// holding** — empty for a cold start, which owns nothing yet.
    ///
    /// Answer this *before* seeding the account, or a shared account is
    /// mis-split. The rule a
    /// [`SleeveWallet`](crate::wallet::SleeveWallet) carve-out is built on —
    /// "whatever the account holds at run start is the user's own" — is only
    /// true the first time. Resume, and the position last chunk opened is
    /// sitting right there in the account looking exactly like somebody else's,
    /// so a baseline snapshotted naively hides the strategy's own position from
    /// it and nets its own equity down to the cash beside it. Feed this to
    /// [`external_baseline_net_of`](crate::wallet::external_baseline_net_of)
    /// and the residual is the genuinely foreign part:
    ///
    /// ```no_run
    /// # use fugazi::spec::{RunnableStrategyExt, StrategySpec, RunState};
    /// # use fugazi::wallet::{SleeveWallet, external_baseline_net_of, own_equity};
    /// # fn go(spec: &StrategySpec, schema: &std::sync::Arc<fugazi::market::Schema>,
    /// #       snaps: &[fugazi::Snapshot<fugazi::types::Symbol>],
    /// #       account: fugazi::PaperWallet<fugazi::types::Symbol>,
    /// #       state: Option<&RunState>) -> Result<(), String> {
    /// let owned = spec.positions_at_resume(schema, state)?;
    /// let baseline = external_baseline_net_of(&account, &owned);
    /// let mut sleeve = SleeveWallet::new(account, baseline);
    /// let seed = own_equity(&sleeve, &Default::default());
    /// let mut built = spec.try_build(seed, schema, None)?;
    /// built.drive_resumable_with(snaps, &mut sleeve, state, &fugazi::backtest::Closeout::Carry)?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// It builds a throwaway strategy and restores `resume` into it purely to
    /// read the book back out, because only the restored book knows. That build
    /// is seeded with [`RESUME_PROBE_SEED`] rather than the account's equity —
    /// the number this call exists to make computable — and nothing reads it: a
    /// restore replaces the book, `initial_equity` included. The throwaway is
    /// deliberate. Restoring into the strategy that is about to be *driven*
    /// would leave it restored twice, once here and once inside
    /// [`drive_over`], and betting the run on that being idempotent buys
    /// nothing but one JSON parse.
    pub fn positions_at_resume(
        &self,
        schema: &Arc<Schema>,
        resume: Option<&RunState>,
    ) -> Result<Vec<crate::wallet::Units<Symbol>>, String> {
        let Some(state) = resume else {
            return Ok(Vec::new());
        };
        check_resumable(state, self.kind())?;
        let mut probe = self.try_build(RESUME_PROBE_SEED, schema, None)?;
        probe
            .restore_state(&state.strategy)
            .map_err(|e| format!("!resume > strategy > {e}"))?;
        Ok(probe.owned_positions())
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
        _universe: &[Symbol],
    ) -> Result<Box<dyn RunnableStrategy>, String> {
        self.try_build(cash, schema, None)
    }

    /// The symbols this strategy may trade — the set per-symbol cost bundles
    /// are resolved for.
    ///
    /// The two shapes that name their symbols up front say so exactly — via the
    /// `!pick` walk over their `root:` expression, which is what "names a
    /// symbol" means now that the root is an expression rather than a string.
    /// The N-symbol shapes discover theirs from the stream, which is also what
    /// their runners already did.
    pub fn universe(&self, snapshots: &[Snapshot<Symbol>]) -> Vec<Symbol> {
        match self {
            StrategySpec::Single(s) => s
                .root()
                .named_symbols()
                .iter()
                .map(crate::types::symbol)
                .collect(),
            StrategySpec::Pairs(s) => s
                .left
                .named_symbols()
                .union(&s.right.named_symbols())
                .map(crate::types::symbol)
                .collect(),
            StrategySpec::Basket(_) | StrategySpec::Multi(_) | StrategySpec::Portfolio(_) => {
                super::backtest::universe_from_snapshots(snapshots)
            }
        }
    }

    /// The symbols this document **declares that it trades** — the ones that
    /// have to be in the input for the run to mean anything.
    ///
    /// Distinct from [`universe`](Self::universe) in what it does about the
    /// N-symbol shapes: those *discover* their universe from the stream, so
    /// they declare nothing and contribute no entries here. `universe` answers
    /// "which symbols do I resolve costs for", which is a question about the
    /// data; this answers "which symbols did the author name", which is a
    /// question about the document — and only the second can be checked
    /// against the data and found wrong.
    ///
    /// A portfolio recurses into its children, so a typo in one child of nine
    /// is named rather than hidden behind the aggregate.
    ///
    /// Consumed by [`backtest::validate_universe`](super::backtest::validate_universe).
    pub fn declared_symbols(&self) -> Vec<String> {
        fn from_child(child: &super::portfolio::PortfolioChildStrategy, out: &mut Vec<String>) {
            use super::portfolio::PortfolioChildStrategy as C;
            match child {
                C::Single(s) => out.extend(s.root().named_symbols()),
                C::Pairs(s) => {
                    out.extend(s.left.named_symbols());
                    out.extend(s.right.named_symbols());
                }
                // Discovered from the stream — nothing was declared.
                C::Basket(_) | C::Multi(_) => {}
            }
        }

        let mut out = Vec::new();
        match self {
            StrategySpec::Single(s) => out.extend(s.root().named_symbols()),
            StrategySpec::Pairs(s) => {
                out.extend(s.left.named_symbols());
                out.extend(s.right.named_symbols());
            }
            StrategySpec::Basket(_) | StrategySpec::Multi(_) => {}
            StrategySpec::Portfolio(p) => {
                for child in &p.children {
                    from_child(&child.strategy, &mut out);
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }
}
