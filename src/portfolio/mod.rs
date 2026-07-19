//! [`Portfolio`]: a top-level composite [`Strategy`](crate::Strategy) that
//! runs N child strategies against one cash pool, each through its own
//! per-child sub-wallet.
//!
//! # Motivation
//!
//! Two backtests that run "a trend follower plus a mean-reverter" side by
//! side each on their own [`PaperWallet`](crate::PaperWallet) tell you what
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
//! and internally owns a [`PortfolioWallet`] carrying one
//! [`PaperWallet`](crate::PaperWallet) per child. The pair share their
//! interior via [`Rc<RefCell<_>>`]. A caller that wants to drive a
//! portfolio:
//!
//! ```no_run
//! use fugazi::backtest;
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
//! let mut wallet = portfolio.wallet_view();
//! let report = backtest::run(&mut portfolio, &mut wallet, snaps());
//! let _ = report.equity_curve; // aggregate MTM across every child
//! ```
//!
//! Per bar the driver:
//! 1. calls `wallet.update(sym, candle)` — [`PortfolioWallet::update`] fans
//!    to every sub, so each child's own [`PaperWallet`] queues, fills, and
//!    marks-to-market on the same bar.
//! 2. routes returned fills through [`Portfolio::on_fill`] — which uses the
//!    portfolio-wide [`OrderId`](crate::OrderId) → child-idx table to
//!    dispatch each fill to *only* its owning child (a stop firing on
//!    child A's position never leaks to child B's `on_fill`).
//! 3. calls [`Portfolio::update`] — which fans the snapshot to every child.
//! 4. calls [`Portfolio::trade`] — which hands each child its own
//!    [`SubWalletHandle`](wallet::SubWalletHandle), a per-child
//!    [`Wallet`] view whose `equity()` / `funds()` / `position()` read
//!    the child's own sub-wallet (so `value_frac(1.0)` sizes against the
//!    child's allocated equity, not the aggregate) and whose mutation
//!    methods forward to the child's sub-wallet with id namespacing so
//!    fills still route back correctly.
//!
//! # Weight policy and rebalancing
//!
//! [`WeightPolicy`](policy::WeightPolicy) governs both the **initial cash
//! allocation** at build time (each child i gets `initial_equity *
//! weights[i] / sum(weights)` seeded into its sub-wallet) *and* the
//! **rebalance target** on each fire bar of the
//! [`rebalance_on`](PortfolioBuilder::rebalance_on) gate. Two policies
//! ship: [`Fixed`](policy::Fixed) and [`EqualWeight`](policy::EqualWeight).
//!
//! The gate is opt-in — its default (`Const::false`) means "never
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
//!    [`WalletError::UnsupportedOperation`]).
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
//! [`warm_up_period`](policy::WeightPolicy::warm_up_period) knob for
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
//! - `initial_equity` is the sum of every seeded sub-wallet.
//!
//! Per-child equity reads are on [`PortfolioWallet::sub_equity`].
//! Trade-level metrics computed off the aggregate `fills` mix owners —
//! two children opening the same symbol on the same bar reconstruct as a
//! scale-in rather than two trades. For clean per-child trade metrics,
//! read each child's own book / positions directly (a `sub_report(i)`
//! surface can come later).
//!
//! [`PortfolioWallet`]: crate::portfolio::PortfolioWallet
//! [`Rc<RefCell<_>>`]: std::rc::Rc

pub mod policy;
pub mod wallet;

use std::cell::RefCell;
use std::hash::Hash;
use std::rc::Rc;

use crate::costs::TradingCosts;
use crate::indicator::Indicator;
use crate::indicators::{Book, Const};
use crate::strategy::Strategy;
use crate::types::{Real, Snapshot};
use crate::wallet::{Order, Units, Wallet};

use self::policy::{ChildSample, WeightPolicy};
use self::wallet::{PortfolioInner, SubWalletHandle, allocate_funds, seed_subs};

pub use self::wallet::PortfolioWallet;

/// One child slot in a [`Portfolio`]: a user-supplied name and the boxed
/// strategy that trades that slot's sub-wallet.
///
/// Names are attached at [`add`](PortfolioBuilder::add) time for downstream
/// reporting (`sub_report(i)`-style APIs, log messages); the run itself
/// keys on the numeric index the child was added at, which is stable for
/// the life of the portfolio.
struct PortfolioChild<Sym> {
    #[allow(dead_code)] // reserved for future per-child reporting.
    name: String,
    strategy: Box<dyn Strategy<Input = Snapshot<Sym>, Symbol = Sym>>,
}

/// The composite [`Strategy`] documented on the module. Own it, hand its
/// [`wallet_view`](Self::wallet_view) to [`backtest::run`](crate::backtest::run),
/// read the resulting [`RunReport`](crate::RunReport).
///
/// The public surface is intentionally small — builder in, `Strategy` +
/// `wallet_view` out — because everything else (per-bar plumbing, fill
/// routing, aggregate reporting) is on the composed [`PortfolioWallet`]
/// and the [`Strategy`] impl.
/// A boolean chain over the portfolio's `Snapshot<Sym>` — the shape used
/// by the [`rebalance_on`](PortfolioBuilder::rebalance_on) gate.
type RebalanceSignal<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = bool>>;

/// A real chain over the portfolio's `Snapshot<Sym>` — the shape used by
/// each child's [`weight_share`](PortfolioBuilder::weight_shares) template
/// instance. Portfolio normalizes the vector of chain values into weights
/// at each rebalance-fire.
type WeightShareChain<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = Real>>;

pub struct Portfolio<Sym> {
    children: Vec<PortfolioChild<Sym>>,
    inner: Rc<RefCell<PortfolioInner<Sym>>>,
    policy: Box<dyn WeightPolicy>,
    bars_seen: usize,
    /// The **rebalance gate**: on each bar `trade()` runs one rebalance
    /// cycle only when this signal reads `true`. Default is
    /// `Const::false` — never rebalance, matching pre-rebalance v1
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
    /// Aggregate [`Book`] of the portfolio, marked to market on each
    /// [`update`](Strategy::update) from the sum of every sub-wallet's
    /// equity. Handed out by [`book`](Self::book); the CLI's
    /// `PortfolioSpec::build` uses it as the default anchor for
    /// weight-share templates (so `!drawdown`, `!return_per_bar`, …
    /// inside a template read *aggregate* state) and pairs each per-child
    /// instantiation with the corresponding child's book via
    /// [`Book::linked_to`](crate::indicators::Book::linked_to) so an
    /// `!at_child { ... }` scope inside a template can walk to per-child
    /// state on demand.
    agg_book: Book<Sym>,
}

impl<Sym: Clone + Eq + Hash + 'static> Portfolio<Sym> {
    /// A fresh builder — add children with [`add`](PortfolioBuilder::add),
    /// pick a policy with [`weights`](PortfolioBuilder::weights), seed
    /// cash with [`with_initial_equity`](PortfolioBuilder::with_initial_equity),
    /// then [`build`](PortfolioBuilder::build).
    pub fn builder() -> PortfolioBuilder<Sym> {
        PortfolioBuilder::default()
    }

    /// A [`PortfolioWallet`] sharing this portfolio's interior — hand it
    /// to [`backtest::run`](crate::backtest::run) alongside the portfolio
    /// itself.
    ///
    /// Multiple views share the same underlying [`PortfolioInner`] (a
    /// plain [`Rc`](std::rc::Rc) bump), so a second view for
    /// side-inspection is cheap; only one should be handed to the driver.
    pub fn wallet_view(&self) -> PortfolioWallet<Sym> {
        PortfolioWallet::from_inner(Rc::clone(&self.inner))
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
    /// its `Arc<Mutex<_>>`. The natural use is *as the default anchor*
    /// for a weight-share expression built inside a
    /// [`Portfolio`](crate::portfolio::Portfolio): the aggregate book is
    /// what a weight template most often needs (aggregate drawdown,
    /// aggregate return, etc.), and per-child access is reached via the
    /// `!at_child` scope (implemented by pairing each per-child
    /// instantiation's aggregate book handle with the child's book via
    /// [`Book::linked_to`](crate::indicators::Book::linked_to)).
    ///
    /// Trade-level fields (`trade_pnl`, `trade_return`) on the aggregate
    /// book stay `None` — the mark-driven path used to update it doesn't
    /// route fills, and portfolio-wide "trades" have no clean definition.
    pub fn book(&self) -> Book<Sym> {
        self.agg_book.clone()
    }

    /// Install per-symbol [`TradingCosts`] on every sub-wallet. Whichever
    /// child ends up filling `symbol` will book at this bundle instead of
    /// the wallet's default.
    ///
    /// This is the seam CLI runners use to thread `--costs SYM:...` scoped
    /// overrides through the composite: rather than a portfolio-wide
    /// uniform bundle (the [`PortfolioBuilder::costs`] path), each symbol's
    /// resolved bundle gets installed on every sub — safe because
    /// [`PaperWallet::set_costs_for`](crate::PaperWallet::set_costs_for) is
    /// idempotent and per-symbol lookup wins over the fallback default.
    pub fn install_costs_for(&mut self, symbol: &Sym, costs: TradingCosts) {
        self.inner
            .borrow_mut()
            .subs
            .iter_mut()
            .for_each(|w| w.set_costs_for(symbol.clone(), costs.clone()));
    }

    /// Snapshot every sub-wallet's current equity/funds for a
    /// [`WeightPolicy::observe`] call. Kept private because policies
    /// read this indirectly via the trait.
    fn sample_children(&self) -> Vec<ChildSample> {
        let inner = self.inner.borrow();
        inner
            .subs
            .iter()
            .map(|w| ChildSample {
                equity: w.equity().0,
                funds: w.funds().0,
            })
            .collect()
    }
}

impl<Sym: Clone + PartialEq + Eq + Hash + 'static> Strategy for Portfolio<Sym> {
    type Input = Snapshot<Sym>;
    type Symbol = Sym;

    fn update(&mut self, snap: Snapshot<Sym>) {
        // Fan the snapshot to every child so their own signals / sizing
        // advance. Cloning is O(entries) — the same cost basket /
        // multi-asset strategies already pay per bar.
        for child in &mut self.children {
            child.strategy.update(snap.clone());
        }
        // Mark the aggregate book from the sum of every sub-wallet's
        // marked-to-market equity — the driver has already run
        // `wallet.update(...)` for this bar before calling us, so
        // sub-wallets are already priced against this bar's closes.
        // Weight-share templates and any external consumer reading via
        // `Portfolio::book()` see the freshly-marked value on this bar.
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
            .all(|c| self.bars_seen >= c.stable_period());
        self.bars_seen >= self.policy.warm_up_period()
            && self.bars_seen >= self.rebalance.stable_period()
            && shares_ready
            && self.children.iter().all(|c| c.strategy.is_ready())
    }

    fn on_fill(&mut self, order: &Order<Sym>) {
        // Look up the owner recorded when this order was submitted, then
        // route the fill to only that child. Fills whose id isn't in the
        // map either weren't tracked (defensive — shouldn't happen with
        // paper subs) or already routed; drop silently either way.
        let owner = self.inner.borrow_mut().owners.remove(&order.id);
        if let Some(idx) = owner {
            self.children[idx].strategy.on_fill(order);
        }
    }

    fn trade(&self, _wallet: &mut dyn Wallet<Sym>) {
        // The wallet argument is ignored: the portfolio reaches into its
        // own inner state via `self.inner`, and each child trades
        // through a SubWalletHandle over the same inner. Well-formed
        // drivers pass `self.wallet_view()` as this argument, so nothing
        // observable changes — this is documented on the module.
        //
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
            let mut handle = SubWalletHandle::new(Rc::clone(&self.inner), i);
            child.strategy.trade(&mut handle);
        }

        // Rebalance gate: skip the whole rebalance step on bars where the
        // signal doesn't fire. Default gate is `Const::false` so this is
        // a no-op unless the caller wired a signal via
        // `rebalance_on(...)`.
        if !self.rebalance.value().unwrap_or(false) {
            return;
        }
        self.rebalance_now();
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
        self.inner.borrow_mut().reset();
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

        let shortfalls = {
            let mut inner = self.inner.borrow_mut();
            let total: Real = inner.subs.iter().map(|w| w.equity().0).sum();
            let targets: Vec<Real> = weights.iter().map(|w| total * w / sum_w).collect();
            inner.rebalance_cash_to(&targets)
        };

        // Position phase: for each contributor whose cash phase couldn't
        // fully cover its donation, scale down every held position
        // proportionally so the freed cash lands next bar and can flow to
        // receivers on the following fire cycle. A shortfall of `0` (fully
        // covered by cash) skips this phase for that child — no order,
        // no blotter noise. Fills route back through the sub-wallet's own
        // seam so per-child `on_fill` fires normally.
        for (i, &shortfall) in shortfalls.iter().enumerate() {
            if shortfall <= 0.0 {
                continue;
            }
            // After cash phase this child is fully cash-out; its equity
            // is entirely invested. Fraction to unwind = shortfall /
            // invested value, clamped to [0, 1].
            let (invested, positions_snapshot): (Real, Vec<(Sym, Real)>) = {
                let inner = self.inner.borrow();
                let sub = &inner.subs[i];
                let inv = sub.equity().0 - sub.funds().0;
                let snap: Vec<(Sym, Real)> = sub
                    .positions()
                    .map(|u| (u.symbol.clone(), u.amount))
                    .collect();
                (inv, snap)
            };
            if invested <= 0.0 || positions_snapshot.is_empty() {
                continue;
            }
            let f = (shortfall / invested).clamp(0.0, 1.0);
            if f <= 0.0 {
                continue;
            }
            let scale = 1.0 - f;
            let mut handle = SubWalletHandle::new(Rc::clone(&self.inner), i);
            for (sym, amt) in positions_snapshot {
                let target = Units {
                    symbol: sym,
                    amount: amt * scale,
                };
                // Ignore the Ack — the fill routing tables are already
                // updated by SubWalletHandle::set_position, and any
                // WalletError here is a genuine bug (PaperWallet queues
                // market moves without checking funds).
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
    policy: Option<Box<dyn WeightPolicy>>,
    initial_equity: Real,
    costs: Option<TradingCosts>,
    rebalance: Option<RebalanceSignal<Sym>>,
    share_indicators: Vec<WeightShareChain<Sym>>,
    /// Pre-supplied aggregate [`Book`] — when set, the built portfolio
    /// uses this book (rather than a freshly-seeded one) so a caller that
    /// needed the handle *before* `build()` (typically the CLI's
    /// `PortfolioSpec::build`, which uses this book as the default anchor
    /// for weight-share templates and pairs each per-child instantiation
    /// with the child's own book via [`Book::linked_to`]) can share the
    /// same handle with the built portfolio.
    agg_book: Option<Book<Sym>>,
}

impl<Sym: 'static> Default for PortfolioBuilder<Sym> {
    fn default() -> Self {
        Self {
            children: Vec::new(),
            policy: None,
            initial_equity: 1.0,
            costs: None,
            rebalance: None,
            share_indicators: Vec::new(),
            agg_book: None,
        }
    }
}

impl<Sym: Clone + Eq + Hash + 'static> PortfolioBuilder<Sym> {
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
    pub fn add(
        mut self,
        name: impl Into<String>,
        strategy: impl Strategy<Input = Snapshot<Sym>, Symbol = Sym> + 'static,
    ) -> Self {
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
    pub fn weights(mut self, policy: impl WeightPolicy) -> Self {
        self.policy = Some(Box::new(policy));
        self
    }

    /// Install a [`TradingCosts`] model applied to every child's
    /// sub-wallet. The bundle is cloned per-sub at
    /// [`build`](Self::build), so a downstream broker with per-symbol
    /// cost overrides can still install those on each sub via
    /// [`PaperWallet::set_costs_for`](crate::PaperWallet::set_costs_for)
    /// after the portfolio is built.
    ///
    /// Skipped by default — every sub is built via
    /// [`PaperWallet::new`](crate::PaperWallet::new), the zero-friction
    /// no-op bundle.
    pub fn costs(mut self, costs: TradingCosts) -> Self {
        self.costs = Some(costs);
        self
    }

    /// Install the **rebalance gate** — a boolean signal that decides,
    /// on each bar, whether [`trade`](Strategy::trade) runs one rebalance
    /// cycle after children have traded. Defaults to `Const::false` —
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
        S: Indicator<Input = Snapshot<Sym>, Output = bool> + 'static,
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
    /// [`YAML surface`](crate::cli) exposes this via
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
    /// `build()` returns — typically to use it as the default anchor for
    /// weight-share templates while pairing each per-child instantiation
    /// with the corresponding child's own book via
    /// [`Book::linked_to`](crate::indicators::Book::linked_to), so that a
    /// template's `!at_child { ... }` scope can walk to per-child state.
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

    /// Realize the [`Portfolio`] — resolve the initial weight vector from
    /// the policy, split `initial_equity` across children accordingly,
    /// seed one [`PaperWallet`](crate::PaperWallet) per child at that
    /// share of cash, and hand back a ready-to-drive portfolio.
    ///
    /// # Panics
    /// Panics if no children were added.
    pub fn build(self) -> Portfolio<Sym> {
        let PortfolioBuilder {
            children,
            policy,
            initial_equity,
            costs,
            rebalance,
            share_indicators,
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
        let policy: Box<dyn WeightPolicy> = policy.unwrap_or_else(|| Box::new(policy::EqualWeight));
        let n = children.len();
        let weights = policy.weights(n);
        assert_eq!(
            weights.len(),
            n,
            "PortfolioBuilder::build: policy returned {} weights for {n} children",
            weights.len()
        );
        let allocations = allocate_funds(initial_equity, &weights);
        let subs = seed_subs::<Sym>(&allocations, costs.as_ref());
        let inner = Rc::new(RefCell::new(PortfolioInner::new(subs)));
        let rebalance: RebalanceSignal<Sym> =
            rebalance.unwrap_or_else(|| Box::new(Const::<Snapshot<Sym>>::new(false)));
        // Aggregate book: use the pre-supplied handle when a caller wired
        // one via `aggregate_book(...)` (typically because they needed
        // the handle before `build()` to wire per-child links);
        // otherwise seed a fresh book at the portfolio's initial equity.
        let agg_book = agg_book.unwrap_or_else(|| Book::new(initial_equity));
        Portfolio {
            children,
            inner,
            policy,
            bars_seen: 0,
            rebalance,
            share_indicators,
            agg_book,
        }
    }
}

