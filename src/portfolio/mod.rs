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
//! # Weight policy in v1
//!
//! [`WeightPolicy`](policy::WeightPolicy) currently governs only the
//! **initial cash allocation** at build time: each child i gets
//! `initial_equity * weights[i] / sum(weights)` seeded into its
//! sub-wallet. Weights aren't re-read once the run begins — child
//! equities drift naturally with per-child P&L. Two policies ship:
//! [`Fixed`](policy::Fixed) and [`EqualWeight`](policy::EqualWeight).
//!
//! Dynamic rebalancing (a `rebalance_on` gate that re-queries
//! [`WeightPolicy::weights`](policy::WeightPolicy::weights) and reshuffles
//! free cash between sub-wallets) and adaptive policies
//! (inverse-volatility, performance-weighted) are follow-ups — the trait
//! carries an [`observe`](policy::WeightPolicy::observe) hook and a
//! [`warm_up_period`](policy::WeightPolicy::warm_up_period) knob for
//! them, but the portfolio doesn't drive them yet.
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
//! Per-child reads (individual equity, funds) are on
//! [`PortfolioWallet::sub_equity`] / [`sub_funds`](PortfolioWallet::sub_funds).
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
use crate::strategy::Strategy;
use crate::types::{Real, Snapshot};
use crate::wallet::{Order, Wallet};

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
pub struct Portfolio<Sym> {
    children: Vec<PortfolioChild<Sym>>,
    inner: Rc<RefCell<PortfolioInner<Sym>>>,
    policy: Box<dyn WeightPolicy>,
    bars_seen: usize,
    /// Total initial equity captured at build. Used by
    /// [`reset`](Strategy::reset) to re-seed sub-wallets at the same
    /// per-child allocation.
    initial_equity: Real,
    /// The per-child seed amounts computed at build from
    /// `initial_equity * weights[i] / sum(weights)`. Cached so
    /// [`reset`](Strategy::reset) restores the same split without
    /// re-querying the policy (some policies are stateful and would
    /// change their answer post-reset).
    initial_allocations: Vec<Real>,
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

    /// The total equity the portfolio was seeded with — the argument to
    /// [`with_initial_equity`](PortfolioBuilder::with_initial_equity).
    pub fn initial_equity(&self) -> Real {
        self.initial_equity
    }

    /// The per-child initial cash allocation computed at build. In v1
    /// weights aren't re-read after this, so these are the seeds each
    /// sub-wallet started at (drift over time reflects P&L only).
    pub fn initial_allocations(&self) -> &[Real] {
        &self.initial_allocations
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
        // Fold this bar's per-child equity/funds into the policy — even
        // though the v1 portfolio only reads weights at build, the hook
        // lets a future dynamic rebalance path pick up rolling stats
        // without a trait break.
        let samples = self.sample_children();
        self.policy.observe(&samples);
        self.bars_seen = self.bars_seen.saturating_add(1);
    }

    fn is_ready(&self) -> bool {
        // A portfolio is ready when every child is ready and the policy
        // is past its own warm-up (which v1 built-ins report as 0). A
        // child that's still warming keeps the whole portfolio out of
        // trade() — matching the safe-defaults rule (unsettled data ⇒
        // wait), just aggregated over every leg.
        self.bars_seen >= self.policy.warm_up_period()
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
    }

    fn reset(&mut self) {
        for child in &mut self.children {
            child.strategy.reset();
        }
        self.policy.reset();
        // Each sub-wallet's own reset() restores it to *its own* seed —
        // the per-child allocation captured at build (since that's what
        // `PaperWallet::new(f)` was called with). No re-splitting needed.
        self.inner.borrow_mut().reset();
        self.bars_seen = 0;
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
}

impl<Sym> Default for PortfolioBuilder<Sym> {
    fn default() -> Self {
        Self {
            children: Vec::new(),
            policy: None,
            initial_equity: 1.0,
            costs: None,
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
        } = self;
        assert!(
            !children.is_empty(),
            "PortfolioBuilder::build: at least one child strategy must be added"
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
        Portfolio {
            children,
            inner,
            policy,
            bars_seen: 0,
            initial_equity,
            initial_allocations: allocations,
        }
    }
}

