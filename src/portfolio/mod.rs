//! [`Portfolio`]: a top-level composite [`Strategy`] that
//! runs N child strategies against one cash pool, each through its own
//! per-child sub-wallet.
//!
//! # Motivation
//!
//! Two backtests that run "a trend follower plus a mean-reverter" side by
//! side each on their own [`PaperWallet`] tell you what
//! each strategy did in isolation. Neither answers "what would this
//! combination *as a portfolio* have earned?" — that requires (a) a single
//! aggregate equity curve marked to market across every child, (b) a way to
//! decide how much of the shared cash pool each child owns (a **weight
//! policy**), and (c) fill / on-fill routing that keeps each child
//! reasoning about *its own* position and equity rather than the aggregate.
//!
//! `Portfolio` is the composition primitive that turns "a collection of
//! [`Strategy`]s" into a Strategy in its own right, so
//! [`backtest::run`](crate::backtest::run) plugs into it unchanged and every
//! post-run analytic (metrics, windowing, walk-forward) falls out for free.
//!
//! # How composition works
//!
//! `Portfolio` implements `Strategy<Input = Snapshot<Sym>, Symbol = Sym>` —
//! the same shape as [`BasketStrategy`](crate::strategies::BasketStrategy) —
//! and internally owns a `PortfolioInner` carrying one notional ledger per
//! child. The pair share their interior via `Arc<Mutex<_>>`. A caller that
//! wants to drive a portfolio:
//!
//! ```no_run
//! use fugazi::portfolio::{Portfolio, policy::EqualWeight};
//! use fugazi::strategies::SingleAssetStrategy;
//!
//! # fn snaps() -> Vec<fugazi::Snapshot<&'static str>> { vec![] }
//! let mut portfolio: Portfolio<&'static str> = Portfolio::builder()
//!     .with_initial_equity(10_000.0)
//!     .add("hold_a", SingleAssetStrategy::<&'static str>::buy_and_hold("A"))
//!     .add("hold_b", SingleAssetStrategy::<&'static str>::buy_and_hold("B"))
//!     .weights(EqualWeight)
//!     .build();
//! let report = portfolio.run(snaps());
//! let _ = report.equity_curve; // aggregate MTM across every child
//! ```
//!
//! [`Portfolio::run`] is the entry point to prefer.
//! [`backtest::run`](crate::backtest::run) still works — pass
//! the account wallet — and is what you
//! want when you need to inspect the wallet mid-run. But `Portfolio` is the
//! one [`Strategy`] that ignores the wallet it is handed (a composite needs N
//! sub-wallets and the trait offers one), so pairing it with any *other*
//! wallet leaves these sub-wallets unpriced and every number quietly wrong.
//! [`Portfolio::trade`] panics rather than let that pass, and `run` sidesteps
//! the question by having nothing to pair. For the same reason a `Portfolio`
//! cannot be a child of another `Portfolio` — [`PortfolioBuilder::add`]
//! refuses it.
//!
//! Per bar the driver:
//! 1. calls `wallet.update(sym, candle)` — `PortfolioInner` fans
//!    to every sub, so each child's own [`PaperWallet`] queues, fills, and
//!    marks-to-market on the same bar.
//! 2. routes returned fills through [`Portfolio::on_fill`] — which uses the
//!    portfolio-wide [`OrderId`](crate::OrderId) → child-idx table to
//!    dispatch each fill to *only* its owning child (a stop firing on
//!    child A's position never leaks to child B's `on_fill`).
//! 3. calls [`Portfolio::update`] — which fans the snapshot to every child.
//! 4. calls [`Portfolio::trade`] — which hands each child its own
//!    `LedgerWallet`, a per-child
//!    [`Wallet`] view whose `equity()` / `funds()` / `position()` read
//!    the child's own sub-wallet (so `value_frac(1.0)` sizes against the
//!    child's allocated equity, not the aggregate) and whose mutation
//!    methods forward to the child's sub-wallet with id namespacing so
//!    fills still route back correctly.
//!
//! # Sub-wallets, paper and live
//!
//! Each child's wallet comes from a `SubWalletFactory`, installed with
//! `sub_wallets` and defaulting to an
//! in-memory [`PaperWallet`] carrying whatever
//! `costs` bundle was set. The subs are held as
//! `Box<dyn Wallet<Sym> + Send>`, so a portfolio can be driven against **live
//! sub-accounts** — the composite performs only [`Wallet`] trait operations on
//! them, and a child reaches its venue through the same
//! `LedgerWallet` path it reaches paper through.
//!
//! Two constraints come with that. **The sub-wallets must be disjoint**: the
//! aggregate reads are sums over the subs and the rebalance moves value
//! between them, so N handles onto one account reports N× the balance and lets
//! children trade over each other. And the optional parts of the seam degrade
//! rather than break — a sub that refuses [`adjust_funds`](Wallet::adjust_funds)
//! or reports no [`positions`](Wallet::positions) simply gets less
//! rebalancing, per the two-phase description below.
//!
//! [`reset`](Strategy::reset) rebuilds every sub from the factory at its
//! original seed and replays any per-symbol cost bundles installed via
//! `install_costs_for`. That is why [`Wallet`]
//! carries no `reset`: a live venue has no "restore to freshly-constructed",
//! and a defaulted no-op on the seam would silently leave a stale wallet
//! driving the second run of an `optimize` sweep.
//!
//! # Weight policy and rebalancing
//!
//! [`WeightPolicy`] governs both the **initial cash
//! allocation** at build time (each child i gets `initial_equity *
//! weights[i] / sum(weights)` seeded into its sub-wallet) *and* the
//! **rebalance target** on each fire bar of the
//! [`rebalance_on`](PortfolioBuilder::rebalance_on) gate. Two policies
//! ship: [`Fixed`](policy::Fixed) and [`EqualWeight`](policy::EqualWeight).
//!
//! The gate is opt-in — its default (`ValueBool::false`) means "never
//! rebalance", so a portfolio with no explicit
//! [`rebalance_on`](PortfolioBuilder::rebalance_on) call behaves exactly
//! as the pre-rebalance shape (weights set at build, then drift with
//! per-child P&L). Wiring a signal — typically `Every::new(N)` for a
//! fixed cadence — turns on the two-phase rebalance loop:
//!
//! 1. **Cash phase** — each child's equity delta is computed from the
//!    policy's current weights; contributors donate what free cash they
//!    can (capped at available funds) via
//!    [`Wallet::adjust_funds`];
//!    receivers split the pot in proportion to their target. Instant, no
//!    fills. Because the phase routes through the `Wallet` trait, it
//!    works with any wallet impl that supports programmatic cash
//!    adjustment (paper always does; live-broker impls plug into their
//!    venue's deposit / withdrawal / sub-account transfer API, or return
//!    [`WalletError::UnsupportedOperation`](crate::WalletError::UnsupportedOperation)).
//!    Debit refusals fold into the contributor's shortfall for the
//!    position phase; receiver credit refusals trigger a symmetric
//!    refund back to contributors so total equity stays conserved.
//! 2. **Position phase** — for each contributor whose cash phase
//!    couldn't fully cover its donation (either because it was cash-
//!    limited or because its wallet refused the debit), submit
//!    `set_position` scale-downs proportional across its held positions.
//!    Fills land next bar; the freed cash flows to receivers on the
//!    following fire cycle. A shortfall of `0` (fully covered by cash)
//!    skips this phase for that child — no orders, no blotter noise, so
//!    a rebalance that only needs cash movement stays free of fills.
//!    Because this phase uses only `Wallet::set_position` — universally
//!    supported by every wallet impl — it's the wallet-agnostic path
//!    for portfolios whose sub-wallets don't support `adjust_funds`.
//!
//! Adaptive policies (inverse-volatility, performance-weighted) are the
//! natural follow-up: the trait already carries an
//! [`observe`](policy::WeightPolicy::observe) hook that's called every
//! bar with per-child equity / funds samples, and a
//! [`warm_up_bars`](policy::WeightPolicy::warm_up_bars) knob for
//! rolling-window policies to gate readiness through. Ship one when a
//! concrete use case shows up.
//!
//! # Reporting
//!
//! [`backtest::run`](crate::backtest::run) returns a normal
//! [`RunReport<Sym>`](crate::RunReport) whose:
//! - `equity_curve` is aggregate MTM per bar (sum of every sub's equity).
//! - `fills` is the concatenated blotter across children, tagged with
//!   portfolio-wide ids.
//! - `rejections` is the concatenated refusal stream, likewise — every
//!   sub-wallet's booked refusals, translated into portfolio-wide ids and
//!   routed back to the child that caused them through
//!   [`Portfolio::on_reject`].
//! - `initial_equity` is the sum of every seeded sub-wallet.
//!
//! Per-child equity reads are on [`Portfolio::sub_equity`].
//! Trade-level metrics computed off the aggregate `fills` mix owners —
//! two children opening the same symbol on the same bar reconstruct as a
//! scale-in rather than two trades. For clean per-child trade metrics,
//! read each child's own book / positions directly (a `sub_report(i)`
//! surface can come later).
//!

pub mod ledger;
pub mod netting;
pub mod policy;
pub mod rebalance;

use std::any::TypeId;
use std::collections::HashMap;
use std::hash::Hash;

use std::sync::{Arc, Mutex};

use crate::backtest::RunReport;
use crate::indicator::Indicator;
use crate::indicators::{Book, ValueBool};
use crate::strategy::Strategy;
use crate::types::{Real, Snapshot};
use crate::wallet::{Order, PaperWallet, Rejection, Wallet};

use self::policy::{ChildSample, WeightPolicy};
use self::rebalance::{PositionInfo, PositionRebalancer, Proportional};
use self::ledger::LedgerWallet;
use self::netting::{PortfolioInner, allocate_funds};

/// One child slot in a [`Portfolio`]: a user-supplied name and the boxed
/// strategy that trades that slot's sub-wallet.
///
/// Names are attached at [`add`](PortfolioBuilder::add) time for downstream
/// reporting (`sub_report(i)`-style APIs, log messages); the run itself
/// keys on the numeric index the child was added at, which is stable for
/// the life of the portfolio.
struct PortfolioChild<Sym> {
    /// Read back by [`Portfolio::child_name`].
    name: String,
    strategy: Box<dyn Strategy<Input = Snapshot<Sym>, Symbol = Sym> + Send>,
}

/// A boolean chain over the portfolio's `Snapshot<Sym>` — the shape used
/// by the [`rebalance_on`](PortfolioBuilder::rebalance_on) gate.
type RebalanceSignal<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = bool> + Send>;

/// A real chain over the portfolio's `Snapshot<Sym>` — the shape used by
/// each child's [`weight_share`](PortfolioBuilder::weight_shares) template
/// instance. Portfolio normalizes the vector of chain values into weights
/// at each rebalance-fire.
type WeightShareChain<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = Real> + Send>;

/// The composite [`Strategy`] documented on the module: N heterogeneous
/// children netted onto **one** account.
///
/// Build it with [`Portfolio::builder`], then drive it like any other
/// strategy — `backtest::run(&mut portfolio, &mut wallet, snapshots)` over
/// any [`Wallet`], or [`run`](Self::run) for the paper case. The wallet
/// passed in must be the portfolio's alone: each child trades a
/// `LedgerWallet` view whose notional book only balances against the real
/// account if nothing else writes to it
/// ([`assert_books_balance`](Self::assert_books_balance) checks exactly
/// that).
///
/// Per-child reads are by the index the child was added at:
/// [`sub_equity`](Self::sub_equity), [`sub_position`](Self::sub_position),
/// [`child_name`](Self::child_name). The aggregate mark-to-market view is
/// [`book`](Self::book).
pub struct Portfolio<Sym> {
    children: Vec<PortfolioChild<Sym>>,
    inner: Arc<Mutex<PortfolioInner<Sym>>>,
    policy: Box<dyn WeightPolicy + Send>,
    bars_seen: usize,
    /// The **rebalance gate**: on each bar `trade()` runs one rebalance
    /// cycle only when this signal reads `true`. Default is
    /// `ValueBool::false` — never rebalance, matching pre-rebalance v1
    /// behavior. Explicit opt-in via
    /// [`rebalance_on`](PortfolioBuilder::rebalance_on).
    rebalance: RebalanceSignal<Sym>,
    /// One weight-share indicator per child (in `add(...)` order). When
    /// non-empty, each rebalance-fire reads their values, normalizes
    /// `w_i = N_i / Σ N_j`, and uses those as the target weight vector
    /// instead of the fallback [`WeightPolicy::weights`]. Advanced every
    /// bar in [`update`](Strategy::update). Empty vector means "no
    /// per-child overrides — use the policy's weights".
    share_indicators: Vec<WeightShareChain<Sym>>,
    /// The **position-phase policy** — decides which positions to
    /// scale down (and by how much) to raise the residual cash a
    /// contributor's cash-phase donation couldn't cover. Defaults to
    /// [`Proportional`]. Install a custom impl via
    /// [`PortfolioBuilder::position_rebalancer`].
    position_rebalancer: Box<dyn PositionRebalancer<Sym> + Send>,
    /// Aggregate [`Book`] of the portfolio, marked to market on each
    /// [`update`](Strategy::update) from the sum of every sub-wallet's
    /// equity. Handed out by [`book`](Self::book); the CLI's
    /// `PortfolioSpec::build` passes it as the `portfolio_book` build
    /// argument for weight-share templates, so a book-reading node with
    /// `source: !portfolio_book` inside a template resolves to it (bare
    /// nodes default to the child's own book — see [`NodeSpec`]).
    agg_book: Book<Sym>,
    /// The portfolio's total cash budget, kept so the inherent
    /// [`run`](Self::run) can seed a fresh [`PaperWallet`] at it.
    initial_equity: Real,
}

impl<Sym: Clone + Eq + Hash + 'static> Portfolio<Sym> {
    /// A fresh builder — add children with [`add`](PortfolioBuilder::add),
    /// pick a policy with [`weights`](PortfolioBuilder::weights), seed
    /// cash with [`with_initial_equity`](PortfolioBuilder::with_initial_equity),
    /// then [`build`](PortfolioBuilder::build).
    pub fn builder() -> PortfolioBuilder<Sym> {
        PortfolioBuilder::default()
    }

    /// Drive this portfolio over `snapshots` against a fresh in-memory account
    /// seeded at its initial equity, returning the aggregate [`RunReport`].
    ///
    /// A convenience over [`backtest::run`](crate::backtest::run): a portfolio is
    /// now an ordinary [`Strategy`] that trades the wallet it is handed, so
    /// `backtest::run(&mut portfolio, &mut wallet, snapshots)` works with any
    /// wallet — a [`PaperWallet`] for a backtest, a live account to trade the
    /// whole netted portfolio for real. Reach for that spelling when you want to
    /// supply the account (costs, live venue) or inspect the wallet after.
    ///
    /// ```no_run
    /// # use fugazi::portfolio::Portfolio;
    /// # use fugazi::strategies::SingleAssetStrategy;
    /// # fn snaps() -> Vec<fugazi::Snapshot<&'static str>> { vec![] }
    /// let mut portfolio: Portfolio<&'static str> = Portfolio::builder()
    ///     .with_initial_equity(10_000.0)
    ///     .add("hold_a", SingleAssetStrategy::<&'static str>::buy_and_hold("A"))
    ///     .build();
    /// let report = portfolio.run(snaps());
    /// ```
    pub fn run<I, A>(&mut self, snapshots: I) -> RunReport<Sym>
    where
        Sym: PartialEq,
        I: IntoIterator<Item = A>,
        A: Into<Snapshot<Sym>>,
    {
        let mut wallet = PaperWallet::new(self.initial_equity);
        crate::backtest::run(self, &mut wallet, snapshots)
    }

    /// Child `idx`'s mark-to-market equity — its notional slice of the account.
    /// Populated once the run has fed at least one bar (the marks the children
    /// price against are refreshed each [`update`](Strategy::update)).
    ///
    /// # Panics
    /// Panics if `idx` is out of range.
    pub fn sub_equity(&self, idx: usize) -> Real {
        self.inner
            .lock()
            .expect("portfolio lock poisoned")
            .child_equity(idx)
    }

    /// Child `idx`'s signed ledger position in `symbol` (0 if it holds none).
    ///
    /// # Panics
    /// Panics if `idx` is out of range.
    pub fn sub_position(&self, idx: usize, symbol: &Sym) -> Real {
        self.inner.lock().expect("portfolio lock poisoned").ledgers[idx].position(symbol)
    }

    /// Assert the core netting identity against `wallet` (the account the
    /// portfolio was driven with): per symbol Σ ledger positions == account
    /// position, and Σ ledger cash == account cash.
    ///
    /// # Panics
    /// Panics if the ledgers have drifted from the account.
    pub fn assert_books_balance(&self, wallet: &dyn Wallet<Sym>) {
        self.inner
            .lock()
            .expect("portfolio lock poisoned")
            .check_invariants(wallet);
    }

    /// The number of children in this portfolio, in [`add`](PortfolioBuilder::add)
    /// order.
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    /// The name given to child `idx` on the builder.
    ///
    /// # Panics
    /// Panics if `idx` is out of range.
    pub fn child_name(&self, idx: usize) -> &str {
        &self.children[idx].name
    }

    /// The portfolio's aggregate [`Book`] — a shared handle to the
    /// mark-to-market equity / peak / return series that
    /// [`update`](Strategy::update) updates each bar from the sum of every
    /// sub-wallet's equity.
    ///
    /// Cheap to call — cloning shares the same underlying state through
    /// its `Arc<Mutex<_>>`. The CLI's `PortfolioSpec::build` passes this
    /// handle as the `portfolio_book` build argument to weight-share
    /// templates so a book-reading node with
    /// `source: !portfolio_book` inside a template resolves to it. Bare
    /// nodes default to the child's own book — this handle is *only* the
    /// aggregate.
    ///
    /// Trade-level fields (`trade_pnl`, `trade_return`) on the aggregate
    /// book stay `None` — the mark-driven path used to update it doesn't
    /// route fills, and portfolio-wide "trades" have no clean definition.
    pub fn book(&self) -> Book<Sym> {
        self.agg_book.clone()
    }

    /// Serialize the portfolio's resumable state — the per-child notional
    /// [`Ledger`](crate::portfolio) books (cash + positions, the "Σ ledgers ==
    /// account" invariant) and the aggregate [`Book`].
    ///
    /// The children's *own* internal indicator state is not captured here: a
    /// [`Portfolio`] holds them erased behind `Box<dyn Strategy>`, which does not
    /// expose the save/restore seam. A resumed portfolio therefore continues
    /// with the correct cash / positions / aggregate equity, while each child's
    /// indicator chains re-warm — the one shape whose resume is state-level for
    /// the account but warm-up-level for the children.
    // Consumed only by the `spec`-gated `DynPortfolio` wrapper.
    #[cfg_attr(not(feature = "spec"), allow(dead_code))]
    pub(crate) fn save_state(&self) -> serde_json::Value
    where
        Sym: serde::Serialize + serde::de::DeserializeOwned,
    {
        serde_json::json!({
            "inner": self.inner.lock().expect("Portfolio inner lock poisoned").snapshot(),
            "agg_book": self.agg_book.snapshot_state(),
        })
    }

    /// Restore state produced by [`save_state`](Self::save_state).
    #[cfg_attr(not(feature = "spec"), allow(dead_code))]
    pub(crate) fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String>
    where
        Sym: serde::Serialize + serde::de::DeserializeOwned,
    {
        let obj = state
            .as_object()
            .ok_or_else(|| format!("portfolio: expected a state object, got {state}"))?;
        if let Some(v) = obj.get("inner") {
            self.inner
                .lock()
                .expect("Portfolio inner lock poisoned")
                .restore(v)
                .map_err(|e| format!("inner > {e}"))?;
        }
        if let Some(v) = obj.get("agg_book") {
            self.agg_book
                .restore_state(v)
                .map_err(|e| format!("agg_book > {e}"))?;
        }
        Ok(())
    }

    /// Snapshot every sub-wallet's current equity/funds for a
    /// [`WeightPolicy::observe`] call. Kept private because policies
    /// read this indirectly via the trait.
    fn sample_children(&self) -> Vec<ChildSample> {
        let inner = self.inner.lock().expect("portfolio lock poisoned");
        (0..inner.child_count())
            .map(|i| ChildSample {
                equity: inner.child_equity(i),
                funds: inner.ledgers[i].cash,
            })
            .collect()
    }
}

impl<Sym: Clone + PartialEq + Eq + Hash + 'static> Strategy for Portfolio<Sym> {
    type Input = Snapshot<Sym>;
    type Symbol = Sym;

    fn update(&mut self, snap: Snapshot<Sym>) {
        // Refresh the marks cache from this bar's priceable entries (tagged +
        // carrying a candle — exactly `backtest::run`'s own pricing predicate),
        // and capture opens for cross booking. This is what children size
        // against in `trade` and what the aggregate book marks at, in place of
        // the old substrate the driver used to price. It runs first so the mark
        // is fresh for everything below.
        let (crossed_fills, child_rejections) = {
            let mut inner = self.inner.lock().expect("portfolio lock poisoned");
            let mut opens: HashMap<Sym, Real> = HashMap::new();
            for (sym, _freq, atom) in snap.iter() {
                if let (Some(sym), Some(candle)) = (sym, atom.candle) {
                    inner.marks.insert(sym.clone(), candle.close);
                    opens.insert(sym.clone(), candle.open);
                }
            }
            // A flow that crossed entirely last bar submitted no order, so no
            // wallet fill ever arrives to settle it — book it here at this bar's
            // open. And drain the child hard-cap refusals booked in the prior
            // bar's `trade` (no driver channel carries them).
            let child_rejections = inner.take_child_rejections();
            let crossed_fills = inner.book_crosses(&opens);
            (crossed_fills, child_rejections)
        };
        for (idx, rej) in child_rejections {
            self.children[idx].strategy.on_reject(&rej);
        }
        for order in crossed_fills {
            let owner = self
                .inner
                .lock()
                .expect("portfolio lock poisoned")
                .owners
                .remove(&order.id);
            if let Some(idx) = owner {
                self.children[idx].strategy.on_fill(&order);
            }
        }
        // Fan the snapshot to every child so their own signals / sizing
        // advance. Cloning is O(entries) — the same cost basket /
        // multi-asset strategies already pay per bar.
        for child in &mut self.children {
            child.strategy.update(snap.clone());
        }
        // Mark the aggregate book from the sum of every child's ledger equity
        // at the freshly-refreshed marks — equal to the account equity by the
        // netting identity. Weight-share templates and any external consumer
        // reading via `Portfolio::book()` see the marked value on this bar.
        let samples = self.sample_children();
        let agg_equity: Real = samples.iter().map(|s| s.equity).sum();
        self.agg_book.mark_equity(agg_equity);
        // Advance each per-child weight-share indicator (when installed)
        // so they warm on the same schedule as the children. Runs after
        // `mark_equity` so a template reading `!portfolio_return_per_bar`
        // sees this bar's aggregate return, not the prior bar's.
        for chain in self.share_indicators.iter_mut() {
            let _ = chain.update(snap.clone());
        }
        // Advance the rebalance gate over the same snapshot. Reads next
        // in `trade()`; a `None` reading is treated as `false` (safe
        // default — don't rebalance through unsettled data).
        self.rebalance.update(snap);
        // Fold this bar's per-child equity/funds into the policy so
        // adaptive policies (inverse-vol, performance-weighted) can
        // accumulate rolling stats even when the gate hasn't fired yet.
        self.policy.observe(&samples);
        self.bars_seen = self.bars_seen.saturating_add(1);
    }

    fn is_ready(&self) -> bool {
        // A portfolio is ready when every child is ready, the policy is
        // past its own warm-up (which v1 built-ins report as 0), the
        // rebalance signal has settled, and every installed weight-share
        // indicator has settled. A child that's still warming keeps the
        // whole portfolio out of trade() — matching the safe-defaults
        // rule (unsettled data ⇒ wait), just aggregated over every leg.
        let shares_ready = self
            .share_indicators
            .iter()
            .all(|c| self.bars_seen >= c.stable_bars());
        self.bars_seen >= self.policy.warm_up_bars()
            && self.bars_seen >= self.rebalance.stable_bars()
            && shares_ready
            && self.children.iter().all(|c| c.strategy.is_ready())
    }

    fn on_fill(&mut self, order: &Order<Sym>) {
        // The driver hands us the RAW account fill (from `wallet.update` /
        // `poll_fills`). Attribute it across the children whose netted flow
        // produced it — moving their ledgers — then dispatch each child's
        // synthetic share to its own `on_fill`, so a stop firing on child A's
        // position never leaks to child B.
        let synthetics = self
            .inner
            .lock()
            .expect("portfolio lock poisoned")
            .attribute_fill(order);
        for synth in synthetics {
            let owner = self
                .inner
                .lock()
                .expect("portfolio lock poisoned")
                .owners
                .remove(&synth.id);
            if let Some(idx) = owner {
                self.children[idx].strategy.on_fill(&synth);
            }
        }
    }

    fn on_reject(&mut self, rejection: &Rejection<Sym>) {
        // The driver hands us the RAW account rejection (a refused netted order
        // or protective leg). Split it across the children that contributed to
        // it and route each to its own `on_reject`. The account-level entry is
        // already in the run report; the per-child copies are for the children.
        let per_child = self
            .inner
            .lock()
            .expect("portfolio lock poisoned")
            .attribute_rejection(rejection.clone());
        for (idx, rej) in per_child {
            self.children[idx].strategy.on_reject(&rej);
        }
    }

    fn trade(&self, wallet: &mut dyn Wallet<Sym>) {
        // A portfolio is now an ordinary strategy: it trades the wallet the
        // driver hands it. Children each trade a `LedgerWallet` over the shared
        // inner (recording intent against their notional slice); the intents are
        // then netted into one order per symbol on this `wallet`.

        // Cache the account's shorting capability before anything trades, so a
        // child querying its `LedgerWallet` — which has no handle on the account
        // — gets the account's answer, not the trait default.
        self.inner
            .lock()
            .expect("portfolio lock poisoned")
            .account_can_short = wallet.can_short();

        // Ordering: children trade first (against their own pre-rebalance
        // equity for `value_frac` sizing), then — if the gate fires — the
        // rebalance runs. Children on the fire bar therefore see a stable
        // equity value; rebalance is bookkeeping that lands after.
        for i in 0..self.children.len() {
            let child = &self.children[i];
            // Per-child readiness gates each leg independently — the
            // outer is_ready() gate keeps trade() out entirely until
            // *every* child is ready, so this check is only defensive
            // (a future partially-ready mode would flip the gates).
            if !child.strategy.is_ready() {
                continue;
            }
            let mut handle = LedgerWallet::new(Arc::clone(&self.inner), i);
            child.strategy.trade(&mut handle);
        }

        // Rebalance gate: skip the whole rebalance step on bars where the
        // signal doesn't fire. Default gate is `ValueBool::false` so this is
        // a no-op unless the caller wired a signal via
        // `rebalance_on(...)`. It runs before netting so a rebalance's
        // position changes merge into the same order as the children's.
        if self.rebalance.value().unwrap_or(false) {
            self.rebalance_now();
        }

        // Nothing has reached the account yet — every child (and the
        // rebalance) has only recorded what it wants. Combine those into one
        // order per symbol on the passed wallet and rest the most urgent
        // protective leg.
        self.inner
            .lock()
            .expect("portfolio lock poisoned")
            .net_and_submit(wallet);
    }

    fn reset(&mut self) {
        for child in &mut self.children {
            child.strategy.reset();
        }
        self.policy.reset();
        self.rebalance.reset();
        for chain in self.share_indicators.iter_mut() {
            chain.reset();
        }
        // Aggregate book returns to its seed (matches Book::reset — the
        // link stays wired for any indicator handles holding a clone).
        // Sub-wallets each restore to their own seed.
        self.agg_book.reset();
        self.inner.lock().expect("portfolio lock poisoned").reset();
        self.bars_seen = 0;
    }
}

impl<Sym: Clone + PartialEq + Eq + Hash + 'static> Portfolio<Sym> {
    /// Execute one rebalance cycle — cash phase followed by a position
    /// phase for whatever the cash phase couldn't cover. Called by
    /// [`trade`](Strategy::trade) on gate-fire bars, after every child has
    /// traded.
    fn rebalance_now(&self) {
        let n = self.children.len();
        if n == 0 {
            return;
        }

        // Compute target equities from the current weight vector, sized
        // against aggregate equity. When per-child weight-share
        // indicators are installed, they win — read each's `.value()`
        // and normalize; else fall back to the WeightPolicy. Weight
        // magnitudes are normalized on use — the policy contract says
        // they needn't sum to 1.0.
        let weights: Vec<Real> = if !self.share_indicators.is_empty() {
            assert_eq!(
                self.share_indicators.len(),
                n,
                "Portfolio::rebalance_now: {} share indicators installed for {n} children",
                self.share_indicators.len(),
            );
            let raw: Vec<Real> = self
                .share_indicators
                .iter()
                .map(|c| c.value().unwrap_or(0.0).max(0.0))
                .collect();
            let sum: Real = raw.iter().sum();
            if sum > 0.0 {
                raw
            } else {
                // Every share reads 0 (or None) — fall back to the
                // policy so we still produce a rebalance direction.
                self.policy.weights(n)
            }
        } else {
            self.policy.weights(n)
        };
        assert_eq!(
            weights.len(),
            n,
            "Portfolio::rebalance_now: got {} weights for {n} children",
            weights.len()
        );
        let sum_w: Real = weights.iter().sum();
        if sum_w <= 0.0 {
            // Degenerate weight vector — no rebalance direction defined.
            return;
        }

        // Cash phase. On a shared account this is pure bookkeeping — the
        // balance never moves, only the notional split of it — so it cannot
        // fail, costs nothing, and generates no orders.
        let shortfalls = {
            let mut inner = self.inner.lock().expect("portfolio lock poisoned");
            let total: Real = (0..inner.child_count()).map(|i| inner.child_equity(i)).sum();
            let targets: Vec<Real> = weights.iter().map(|w| total * w / sum_w).collect();
            inner.rebalance_ledgers_to(&targets)
        };

        // Position phase: hand each contributor's shortfall + position
        // snapshot to the installed [`PositionRebalancer`] policy,
        // which returns the targeted per-position unit counts. Default
        // policy is [`Proportional`] — scale every leg by
        // `(1 - shortfall/invested)`. Alternatives (largest-first,
        // "sell losers first", per-child bespoke) plug in via
        // [`PortfolioBuilder::position_rebalancer`] without changing
        // anything below.
        //
        // A shortfall of `0` (fully covered by cash) skips this phase
        // for that child — no order, no blotter noise. Fills route back
        // through the sub-wallet's own seam so per-child `on_fill`
        // fires normally.
        for (i, &shortfall) in shortfalls.iter().enumerate() {
            if shortfall <= 0.0 {
                continue;
            }
            // Snapshot per-position marks so the policy can decide by
            // absolute value (largest-first) or a custom rule. Prices
            // come from the sub-wallet's own `price()` — the same mark
            // it uses for equity accounting; positions without a mark
            // are skipped defensively (their value would be undefined).
            let positions_snapshot: Vec<PositionInfo<Sym>> = {
                let inner = self.inner.lock().expect("portfolio lock poisoned");
                inner.ledgers[i]
                    .positions
                    .iter()
                    .filter_map(|(symbol, &units)| {
                        inner.price_of(symbol).map(|price| PositionInfo {
                            symbol: symbol.clone(),
                            units,
                            price,
                        })
                    })
                    .collect()
            };
            if positions_snapshot.is_empty() {
                continue;
            }
            let targets = self
                .position_rebalancer
                .plan_scaledowns(&positions_snapshot, shortfall);
            if targets.is_empty() {
                continue;
            }
            let mut handle = LedgerWallet::new(Arc::clone(&self.inner), i);
            for target in targets {
                // Records intent on the child's ledger; the netting pass at
                // the end of `trade` turns it into account flow. A scale-down
                // never trips the hard cap, so an Err here would be a bug.
                let _ = handle.set_position(target);
            }
        }
    }
}

/// Fluent builder for a [`Portfolio`] — accumulates children, the weight
/// policy, and the initial cash budget, then hands back a ready-to-run
/// portfolio out of [`build`](Self::build).
///
/// Missing pieces default sensibly: no `weights(...)` call means
/// [`EqualWeight`](policy::EqualWeight), no `with_initial_equity(...)`
/// call means `1.0` (matching [`SingleAssetStrategy::new`](crate::strategies::SingleAssetStrategy::new)).
/// [`build`](Self::build) panics if no children were added — a zero-child
/// portfolio has no meaning.
pub struct PortfolioBuilder<Sym> {
    children: Vec<PortfolioChild<Sym>>,
    policy: Option<Box<dyn WeightPolicy + Send>>,
    initial_equity: Real,
    rebalance: Option<RebalanceSignal<Sym>>,
    share_indicators: Vec<WeightShareChain<Sym>>,
    /// Position-phase rebalancer. `None` picks the [`Proportional`]
    /// default at build; set via [`position_rebalancer`](Self::position_rebalancer).
    position_rebalancer: Option<Box<dyn PositionRebalancer<Sym> + Send>>,
    /// Pre-supplied aggregate [`Book`] — when set, the built portfolio
    /// uses this book (rather than a freshly-seeded one) so a caller that
    /// needed the handle *before* `build()` (typically the CLI's
    /// `PortfolioSpec::build`, which passes it as `portfolio_book` when
    /// building each per-child weight-share template) can share the same
    /// handle with the built portfolio.
    agg_book: Option<Book<Sym>>,
}

impl<Sym: 'static> Default for PortfolioBuilder<Sym> {
    fn default() -> Self {
        Self {
            children: Vec::new(),
            policy: None,
            initial_equity: 1.0,
            rebalance: None,
            share_indicators: Vec::new(),
            position_rebalancer: None,
            agg_book: None,
        }
    }
}

impl<Sym: Clone + Eq + Hash + Send + Sync + 'static> PortfolioBuilder<Sym> {
    /// Seed the portfolio's total cash budget. Split across children by
    /// the weight policy at [`build`](Self::build) time.
    ///
    /// # Panics
    /// Panics if `equity` is not strictly positive.
    pub fn with_initial_equity(mut self, equity: Real) -> Self {
        assert!(
            equity > 0.0,
            "PortfolioBuilder::with_initial_equity: equity must be strictly positive"
        );
        self.initial_equity = equity;
        self
    }

    /// Add a child strategy under `name`. Children are trades in
    /// insertion order — [`WeightPolicy::weights`] returns weights in
    /// this same order.
    ///
    /// # Panics
    /// Panics if `strategy` is itself a [`Portfolio`]. A nested portfolio
    /// would satisfy the bounds here and compile, but it can never work: only
    /// the *outer* `PortfolioInner` fans bars out, and it fans
    /// them to the outer portfolio's sub-wallets — the inner portfolio's own
    /// interior is invisible to it and would never be priced. Flatten the
    /// children into one portfolio, or run them as separate portfolios.
    ///
    /// A `Portfolio` hidden behind a user-written adapter type slips past
    /// this check; [`Portfolio::trade`]'s pricing guard is the backstop.
    pub fn add<S>(mut self, name: impl Into<String>, strategy: S) -> Self
    where
        S: Strategy<Input = Snapshot<Sym>, Symbol = Sym> + Send + 'static,
    {
        assert!(
            TypeId::of::<S>() != TypeId::of::<Portfolio<Sym>>(),
            "PortfolioBuilder::add: a Portfolio cannot be a child of a Portfolio — an inner \
             portfolio's sub-wallets are never priced, because only the outer composite wallet \
             receives bars. Flatten the children into one portfolio, or run them separately.",
        );
        self.children.push(PortfolioChild {
            name: name.into(),
            strategy: Box::new(strategy),
        });
        self
    }

    /// Install the [`WeightPolicy`]. Called once per build; the policy's
    /// [`weights`](WeightPolicy::weights) drives the initial cash split.
    ///
    /// Defaults to [`EqualWeight`](policy::EqualWeight) if never set.
    pub fn weights(mut self, policy: impl WeightPolicy + Send) -> Self {
        self.policy = Some(Box::new(policy));
        self
    }

    /// Install the **rebalance gate** — a boolean signal that decides,
    /// on each bar, whether [`trade`](Strategy::trade) runs one rebalance
    /// cycle after children have traded. Defaults to `ValueBool::false` —
    /// **never rebalance** (weights stay at build-time allocation and
    /// drift with per-child P&L).
    ///
    /// A common cadence is `Every::new(N)` — e.g. `!every 28` on a
    /// daily-bar portfolio to rebalance approximately monthly. Compose
    /// with any other snapshot signal (a drawdown gate, a calendar rule)
    /// to trigger on custom conditions.
    ///
    /// Each fire runs the same two-phase rebalance:
    /// 1. **Cash phase** — contributors donate what free cash they have
    ///    (capped at their available funds) via
    ///    [`Wallet::adjust_funds`];
    ///    receivers split the pot in proportion to their target.
    /// 2. **Position phase** — for each contributor whose cash phase
    ///    couldn't fully cover its donation, submit `set_position`
    ///    scale-downs proportional across its held positions. Fills land
    ///    next bar; the freed cash then transfers to receivers on the
    ///    following fire cycle.
    ///
    /// A `None` reading from the gate is treated as `false` (safe
    /// default — don't rebalance during warm-up), same as elsewhere in
    /// the crate.
    pub fn rebalance_on<S>(mut self, signal: S) -> Self
    where
        S: Indicator<Input = Snapshot<Sym>, Output = bool> + Send + 'static,
    {
        self.rebalance = Some(Box::new(signal));
        self
    }

    /// Install one **weight-share indicator per child** — a real-valued
    /// chain over the portfolio's `Snapshot<Sym>` that produces `N_i`
    /// per bar. At each rebalance-fire the portfolio normalizes
    /// `w_i = N_i / Σ N_j` and uses that as the target weight vector,
    /// overriding the fallback [`WeightPolicy`].
    ///
    /// This is the seam for adaptive weighting — an inverse-vol,
    /// Kelly-fraction, or drawdown-throttled weighting is just a matter
    /// of writing the right indicator per child. The
    /// YAML surface (via the `cli` module) exposes this via
    /// `weights: !indicator <template>` where the template is
    /// instantiated per-child with `!arg SYM` / `!arg CHILD_NAME`
    /// substitution.
    ///
    /// The vector must have exactly `children.len()` entries at
    /// [`build`](Self::build). Every share value read `None` (still
    /// warming) or negative-clamped-to-zero on read; if the whole
    /// vector sums to `0.0` the portfolio falls back to
    /// [`WeightPolicy::weights`] for that fire.
    ///
    /// # Panics
    /// Panics at build if the vector length doesn't match the number of
    /// children.
    pub fn weight_shares(mut self, shares: Vec<WeightShareChain<Sym>>) -> Self {
        self.share_indicators = shares;
        self
    }

    /// Install a pre-supplied aggregate [`Book`] to use as the portfolio's
    /// own book. Overrides the freshly-seeded default the portfolio would
    /// otherwise construct at [`build`](Self::build).
    ///
    /// Intended for callers who need the aggregate book handle *before*
    /// `build()` returns — typically to pass it to per-child weight-share
    /// templates as the `portfolio_book` build argument (so a book-reading
    /// node inside a template with `source: !portfolio_book` reads
    /// aggregate state).
    ///
    /// The supplied book should be seeded at the portfolio's initial
    /// equity (same value that would be passed to
    /// [`with_initial_equity`](Self::with_initial_equity)); otherwise
    /// aggregate drawdown and per-bar return readings will start from a
    /// mismatched baseline.
    pub fn aggregate_book(mut self, book: Book<Sym>) -> Self {
        self.agg_book = Some(book);
        self
    }

    /// Install a custom [`PositionRebalancer`] impl — the pluggable
    /// position-phase policy that decides which held positions to
    /// scale down (and by how much) to raise the cash a contributor's
    /// cash-phase donation couldn't cover.
    ///
    /// Defaults to [`Proportional`] — every leg contributes in
    /// proportion to its value, matching the original hardcoded
    /// behavior. Alternatives ship as [`LargestFirst`](crate::portfolio::rebalance::LargestFirst) (liquidates
    /// biggest positions first) and any user-supplied impl of the
    /// trait ("sell losers first", "keep hedges intact", etc.).
    pub fn position_rebalancer<R>(mut self, rebalancer: R) -> Self
    where
        R: PositionRebalancer<Sym> + 'static,
    {
        self.position_rebalancer = Some(Box::new(rebalancer));
        self
    }

    /// Realize the [`Portfolio`] — resolve the initial weight vector from
    /// the policy, split `initial_equity` across children accordingly,
    /// seed one [`PaperWallet`] per child at that
    /// share of cash, and hand back a ready-to-drive portfolio.
    ///
    /// # Panics
    /// Panics if no children were added.
    pub fn build(self) -> Portfolio<Sym> {
        let PortfolioBuilder {
            children,
            policy,
            initial_equity,
            rebalance,
            share_indicators,
            position_rebalancer,
            agg_book,
        } = self;
        assert!(
            !children.is_empty(),
            "PortfolioBuilder::build: at least one child strategy must be added"
        );
        assert!(
            share_indicators.is_empty() || share_indicators.len() == children.len(),
            "PortfolioBuilder::build: {} share indicators supplied for {} children",
            share_indicators.len(),
            children.len(),
        );
        let policy: Box<dyn WeightPolicy + Send> = policy.unwrap_or_else(|| Box::new(policy::EqualWeight));
        let n = children.len();
        let weights = policy.weights(n);
        assert_eq!(
            weights.len(),
            n,
            "PortfolioBuilder::build: policy returned {} weights for {n} children",
            weights.len()
        );
        let allocations = allocate_funds(initial_equity, &weights);
        let inner = Arc::new(Mutex::new(PortfolioInner::new(allocations)));
        let rebalance: RebalanceSignal<Sym> =
            rebalance.unwrap_or_else(|| Box::new(ValueBool::<Snapshot<Sym>>::new(false)));
        // Aggregate book: use the pre-supplied handle when a caller wired
        // one via `aggregate_book(...)` (typically because they needed
        // the handle before `build()` to wire per-child links);
        // otherwise seed a fresh book at the portfolio's initial equity.
        let agg_book = agg_book.unwrap_or_else(|| Book::new(initial_equity));
        let position_rebalancer: Box<dyn PositionRebalancer<Sym> + Send> =
            position_rebalancer.unwrap_or_else(|| Box::new(Proportional));
        Portfolio {
            children,
            inner,
            policy,
            bars_seen: 0,
            rebalance,
            share_indicators,
            position_rebalancer,
            agg_book,
            initial_equity,
        }
    }
}

