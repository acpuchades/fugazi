//! The in-memory paper broker: order queueing, fill simulation against the
//! next bar, protective-leg matching, and mark-to-market accounting.
//!
//! This is the bulk of the old `wallet.rs`. Nothing outside it needs
//! `Pending` / `Leg` / `Protective` / `FillPricing` / `RestingLimit`, which is
//! why they are private here rather than in [`super::types`].

use std::hash::Hash;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::costs::TradingCosts;
use crate::types::{Candle, Real, Timestamp};

use super::types::{
    Ack, Order, OrderId, OrderKind, POSITION_EPSILON, PRICE_EPSILON, Reference, Rejection, Side,
    Size, Units, WalletError, cash_tolerance,
};
use super::{Wallet, marked_sum, trim_front};

/// Calendar seconds in a year, matching the 30-day-month / 7-day-week
/// convention [`Frequency::calendar_seconds_per_bar`](crate::time::Frequency::calendar_seconds_per_bar)
/// uses. **Calendar, not trading time** — a broker charges interest over the
/// weekend; the market does not pay returns over it.
const SECONDS_PER_YEAR: Real = 365.25 * 86_400.0;
use crate::hash::SymMap;

/// A market order queued on a [`PaperWallet`] to fill at the next bar's `open`.
///
/// The two market entry points differ only in *when* the target is known: an
/// absolute unit target is fixed at queue time, while a [`Side`] + [`Size`] is
/// resolved against the fill (`open`) price — so an all-in
/// [`value_frac(1.0)`](Size::value_frac) stays affordable even when the bar gaps.
/// Each carries the [`OrderId`] minted when it was submitted.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum Pending {
    /// Drive to an absolute signed-unit target (from [`set_position`](Wallet::set_position)).
    Target(Real, OrderId),
    /// A side + size, resolved against the fill bar's `open` (from [`set`](Wallet::set)).
    Sized(Side, Size, OrderId),
}

/// One resting protective leg: the `trigger` level, **how much of the position
/// it takes off**, and the [`OrderId`] it fills under.
///
/// `size` resolves at the fill price and is clamped to the position's
/// magnitude, so a protective leg is always *reduce-only* — it can flatten a
/// position but never flip it. [`Size::position_frac(1.0)`](Size::position_frac)
/// is the whole-position exit every single-asset strategy wants; an explicit
/// [`Size::Units`] is what lets one account carry several owners' exits on the
/// same symbol.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Leg {
    trigger: Real,
    size: Size,
    id: OrderId,
}

/// The resting protective bracket for a symbol — a stop-loss and/or take-profit
/// leg. Holding both together makes them one-cancels-the-other: a fill on either
/// (or a market exit/reversal) drops the whole record.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct Protective {
    stop: Option<Leg>,
    take_profit: Option<Leg>,
}

/// Where and how a fill prices: the bar it lands on, the pre-cost price, and
/// the [`OrderKind`]. Travels together through the cost pipeline, so it is one
/// argument rather than three.
#[derive(Debug, Clone, Copy)]
struct FillPricing<'a> {
    bar: &'a Candle,
    price: Real,
    kind: OrderKind,
}

/// Every position **except** the one being traded, marked at some set of
/// prices and summed two ways: `marked` signed (what it contributes to equity)
/// and `gross` absolute (what it contributes to exposure).
///
/// The two sums are what turn the leverage cap into a bound on the one symbol a
/// fill actually moves — the rest of the book is a constant as far as that fill
/// is concerned, so the caller measures it once, at whatever prices are honest
/// for its phase, and hands the result down.
///
/// **Which prices those are is the caller's call, and it matters.** A queued
/// market order fills at the bar's `open`, so
/// [`advance`](Wallet::advance) marks the rest of the book at *its* opens too —
/// reading a `close` there would size a fill off information from later in the
/// same bar. A resting limit fills mid-bar and marks at the last close, the
/// same prices its own equity-at-fill already uses.
#[derive(Debug, Clone, Copy, Default)]
struct RestOfBook {
    /// `Σ position × mark` over the other symbols — signed, so shorts subtract.
    marked: Real,
    /// `Σ |position| × mark` over the other symbols — unsigned, so shorts add.
    gross: Real,
}

/// What a fill was *asked* for, and what the rest of the account looks like
/// underneath it.
///
/// Travels with the target into [`PaperWallet::fill_at`] rather than as two
/// more positional arguments, and keeps the "nothing was shrunk" case a named
/// constructor instead of a repeated pair of defaults.
#[derive(Debug, Clone, Copy)]
struct FillContext {
    /// The signed target the caller's sizing resolved to **before**
    /// [`PaperWallet::fit_to_account`] fitted it — equal to the target itself
    /// on every path that fits nothing. Becomes
    /// [`Order::requested_units`] once the current position is known.
    requested: Real,
    rest: RestOfBook,
}

impl FillContext {
    /// A fill whose target is exactly what was asked for, against a book
    /// measured as `rest`.
    fn exact(target: Real, rest: RestOfBook) -> Self {
        Self {
            requested: target,
            rest,
        }
    }

    /// A fill that can only ever *reduce* the position's magnitude — an exit, a
    /// protective leg, a [`flatten`](Wallet::flatten).
    ///
    /// Nothing was shrunk, and the rest of the book is left unmeasured because
    /// the leverage cap exempts a reducing fill outright: measuring it would be
    /// dead work whose only effect could be to refuse an exit.
    fn reducing(target: Real) -> Self {
        Self::exact(target, RestOfBook::default())
    }
}

/// The half-spread a fill of `kind` crosses.
///
/// Zero for a [`Limit`](OrderKind::Limit): a resting limit provides liquidity
/// instead of taking it, so it crosses no spread. This is wallet policy rather
/// than a per-model decision — every spread model gives the same answer for a
/// passive fill — which is why [`SpreadModel::half_spread`] doesn't take the
/// kind. Together with the zero slippage multiplier `costs` gives `Limit`, it
/// is what keeps a limit fill from ever pricing worse than the caller's limit.
///
/// Shared by [`PaperWallet::fill_at`] and
/// [`fit_to_account`](PaperWallet::fit_to_account) so the price a buy is
/// sized against and the price it books at can't disagree.
fn half_spread_for(costs: &TradingCosts, kind: OrderKind, price: Real, bar: &Candle) -> Real {
    match kind {
        OrderKind::Limit => 0.0,
        _ => costs.spread.half_spread(price, bar),
    }
}

/// The slack the leverage check allows, scaled to the magnitudes being
/// compared.
///
/// Shared by [`PaperWallet::fit_to_account`] and [`PaperWallet::fill_at`] so a
/// magnitude the shrink just fitted to the cap cannot then be refused by it —
/// the two sides of the same inequality must not disagree over a ULP.
fn gross_tolerance(held: Real, equity: Real) -> Real {
    cash_tolerance(held.abs().max(equity.abs()))
}

/// A resting limit order: a target `side · size` to be reached once the bar
/// trades through `limit`.
///
/// The [`Size`] is stored unresolved and resolved at the fill price, so an
/// all-in sizes against the equity at the moment the limit is actually hit
/// rather than at submission — the same reason [`Pending::Sized`] defers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct RestingLimit {
    side: Side,
    size: Size,
    limit: Real,
    id: OrderId,
}

/// What [`PaperWallet::fit_to_account`] made of a fractional sizing: the
/// magnitude the account can actually carry, and — when that is less than what
/// was asked for — which of the two solvency rules cut it down.
///
/// The `bound` exists for the case where the fit collapses to **no trade at
/// all**. Shrinking a request to what fits is ordinary and is recorded on the
/// fill itself as [`Order::requested_units`]; shrinking it to zero produces no
/// fill to record it on, so without this the leg simply vanished — no order, no
/// rejection, nothing in the blotter. The same economic situation reached
/// through an explicit [`Size::Units`] is a loud `InsufficientFunds`, and an
/// unlevered basket whose earlier legs have used the whole gross budget reaches
/// it on the last leg routinely.
#[derive(Debug, Clone, Copy)]
struct Fitted {
    magnitude: Real,
    /// `None` when the whole request fitted.
    bound: Option<WalletError>,
}

impl Fitted {
    /// The request fitted as asked.
    fn whole(magnitude: Real) -> Self {
        Self {
            magnitude,
            bound: None,
        }
    }
}

/// One queued market order, resolved against the bar's opens and awaiting its
/// turn in [`PaperWallet::advance`]'s credit-then-debit ordering.
///
/// `sizing` is `Some` only for a [`Pending::Sized`] submission, and carries what
/// [`PaperWallet::fit_to_account`] needs to re-fit the buy against cash as it
/// stands when the fill is finally booked — which is the whole point of holding
/// the order here rather than filling it where it was resolved.
struct QueuedFill<Sym> {
    symbol: Sym,
    candle: Candle,
    target: Real,
    sizing: Option<(Side, Size)>,
    id: OrderId,
    /// Whether booking this fill *adds* cash. Credits settle first, so a
    /// rotation's sale funds its replacement buy.
    credits: bool,
}

/// The built-in **pure**, in-memory [`Wallet`]: a paper book of `funds`,
/// per-symbol positions, the prices fed to it, a queue of market orders awaiting
/// their next-open fill, the resting protective brackets, and a blotter of
/// executed [`Order`]s, with no IO.
///
/// The **market** movements ([`set_position`](Wallet::set_position) /
/// [`set`](Wallet::set) / [`close`](Wallet::close)) *queue*: they record the
/// intended move, return [`Ack::Working`], and the next [`update`](Wallet::update)
/// fills it at that bar's `open` (one queued move per symbol, latest wins) — which
/// keeps a backtest from filling on the same bar whose `close` triggered the
/// signal. The **resting** movements ([`set_stop`](Wallet::set_stop) /
/// [`set_take_profit`](Wallet::set_take_profit)) register a trigger level (one
/// bracket per symbol, latest wins); [`update`](Wallet::update) triggers and
/// prices them itself, filling at the level or — when the bar gaps past it — at
/// the `open`. Use it for backtests and dry runs; a downstream `Wallet` impl
/// handles live execution / bus publishing.
/// How many blotter / rejection entries a [`PaperWallet`] keeps by default.
///
/// The two logs are **reporting artifacts**, not state: nothing in the fill,
/// pricing or resume path reads them. Left unbounded they are a slow leak in
/// exactly the deployment the resumable driver exists for — a strategy driven
/// live for years records every fill it ever books and frees none of them.
///
/// 10k entries is far more than a report needs and costs on the order of a
/// megabyte. A caller who genuinely wants the whole history says so with
/// [`with_retention(None)`](PaperWallet::with_retention) — and a caller who
/// needs it to survive a restart wants their own durable store, not this.
pub const DEFAULT_RETENTION: usize = 10_000;

#[derive(Debug)]
pub struct PaperWallet<Sym> {
    positions: SymMap<Sym, Real>,
    bars: SymMap<Sym, Candle>,
    pending: SymMap<Sym, Pending>,
    protective: SymMap<Sym, Protective>,
    /// One resting limit order per symbol, latest-wins — the same convention
    /// `pending` and `protective` use.
    limits: SymMap<Sym, RestingLimit>,
    funds: Real,
    initial_funds: Real,
    blotter: Vec<Order<Sym>>,
    rejections: Vec<Rejection<Sym>>,
    /// How many of `rejections` have already been yielded by
    /// [`take_rejections`](Wallet::take_rejections), so the drain yields each
    /// entry exactly once. Counted against the *current* head of the vec:
    /// [`trim`](Self::trim) drops entries off the front once the log passes
    /// `retention`, and shifts this cursor by the same amount.
    rejections_drained: usize,
    /// How many blotter / rejection entries to keep, or `None` for every one
    /// ever recorded. See [`with_retention`](Self::with_retention).
    retention: Option<usize>,
    next_id: u64,
    costs: TradingCosts,
    per_symbol_costs: SymMap<Sym, TradingCosts>,
    /// The label reported by [`quote_ccy`](Wallet::quote_ccy), or `None` when
    /// the caller never said. Purely descriptive — nothing in the fill or
    /// pricing path reads it, because simulated money has no venue to check it
    /// against.
    quote_ccy: Option<String>,
    /// The most gross notional this account may hold, as a multiple of equity.
    /// `1.0` — the default — is an unlevered book. See
    /// [`with_max_gross`](Self::with_max_gross); unlike `quote_ccy` this one is
    /// enforced on every fill.
    max_gross: Real,
    /// What fraction of a year one bar spans, on the **calendar**. `None` until
    /// the caller says; a time-denominated [`CarryModel`] charges nothing
    /// without it rather than inventing a year length. See
    /// [`with_bar_year_fraction`](Self::with_bar_year_fraction).
    bar_year_fraction: Option<Real>,
    /// Bar-open time of the most recently advanced bar — the left endpoint of
    /// the interval the *next* bar's carry is charged over. `None` until a bar
    /// carrying a time has been observed. See
    /// [`bar_year_fraction`](Self::with_bar_year_fraction).
    last_bar_time: Option<Timestamp>,
    /// This bar's open time, recorded by [`observe`](Wallet::observe) before
    /// [`advance`](Wallet::advance) prices anything, and consumed by
    /// [`accrue_carry`](Self::accrue_carry).
    pending_bar_time: Option<Timestamp>,
    /// Annualized interest charged on a **negative** cash balance. See
    /// [`with_margin_rate`](Self::with_margin_rate).
    margin_rate: Real,
    /// Equity/gross ratio below which the book is force-closed, or `None` for
    /// no margin call at all (the default). See
    /// [`with_maintenance_margin`](Self::with_maintenance_margin).
    maintenance_margin: Option<Real>,
    /// This bar's carry rate per symbol, as fed by [`observe`](Wallet::observe)
    /// and consumed by the next [`advance`](Wallet::advance).
    carry_rates: SymMap<Sym, Real>,
    /// `(bars a carry model wanted a rate for, bars one actually arrived)` —
    /// what [`carry_coverage`](Self::carry_coverage) reports.
    carry_wanted: usize,
    carry_seen: usize,
}

impl<Sym> PaperWallet<Sym> {
    /// A wallet seeded with `funds` of cash, no positions and no prices, and
    /// **no trading costs** — every fill books at the theoretical price with
    /// zero commission, matching the pre-costs release. Byte-identical to the
    /// pre-costs behavior on any driver.
    pub fn new(funds: Real) -> Self {
        Self::with_costs(funds, TradingCosts::none())
    }

    /// A wallet seeded with `funds` of cash and the given `costs` model — every
    /// fill goes through the spread → slippage → commission pipeline
    /// documented on [`crate::costs`]. Pass [`TradingCosts::none`] for a
    /// zero-cost wallet (equivalent to [`new`](Self::new)).
    pub fn with_costs(funds: Real, costs: TradingCosts) -> Self {
        Self {
            positions: SymMap::default(),
            bars: SymMap::default(),
            pending: SymMap::default(),
            protective: SymMap::default(),
            limits: SymMap::default(),
            funds,
            initial_funds: funds,
            blotter: Vec::new(),
            rejections: Vec::new(),
            rejections_drained: 0,
            retention: Some(DEFAULT_RETENTION),
            next_id: 0,
            costs,
            per_symbol_costs: SymMap::default(),
            quote_ccy: None,
            max_gross: 1.0,
            bar_year_fraction: None,
            last_bar_time: None,
            pending_bar_time: None,
            margin_rate: 0.0,
            maintenance_margin: None,
            carry_rates: SymMap::default(),
            carry_wanted: 0,
            carry_seen: 0,
        }
    }

    /// Label the currency this wallet's cash is denominated in, reported back
    /// through [`quote_ccy`](Wallet::quote_ccy).
    ///
    /// Descriptive only: nothing here converts, and a labelled wallet trades
    /// identically to an unlabelled one. It exists so a paper account can carry
    /// the same fact a live one reports from its venue — a simulation of a EUR
    /// book can say so, instead of leaving every caller to assume dollars.
    pub fn with_quote_ccy(mut self, ccy: impl Into<String>) -> Self {
        self.quote_ccy = Some(ccy.into());
        self
    }

    /// The most gross notional this wallet may hold, as a multiple of equity:
    /// no fill may leave `Σ |position| × price` above `max_gross × equity`.
    /// Defaults to `1.0` — an unlevered account.
    ///
    /// **This is the one bound both sides of the book share.** Cash alone
    /// cannot provide it: a buy is limited by the funds it spends, but a short
    /// *credits* cash, so nothing stops one piling up exposure. Before this
    /// existed a `sizing: 3.0` document executed its long leg at 1x (shrunk to
    /// what cash could pay for) and its short leg at 3x, under one spec value —
    /// so a long/short backtest reported a number describing neither leg.
    ///
    /// For a **long-only** book at `1.0` this is not a new rule, it is the old
    /// one restated: `gross <= equity` and `funds >= 0` are the same inequality
    /// when every position is long, so an unlevered long backtest fills exactly
    /// as it always did. What changes is that a short is now bounded by the
    /// same number, and that raising it above `1.0` lets cash go negative — the
    /// account borrows, which is what leverage *is*.
    ///
    /// Set it to the leverage the live account it models runs at, so the two
    /// curves measure the same strategy; read it back off either wallet with
    /// [`Wallet::leverage`]. A request that overshoots is fitted to the cap when
    /// the sizing is fractional (and the gap recorded on
    /// [`Order::requested_units`]) and refused with
    /// [`WalletError::ExceedsMaxGross`] when it is an explicit unit count —
    /// the same split [`InsufficientFunds`](WalletError::InsufficientFunds)
    /// already draws.
    ///
    /// **It is a ceiling on the result, never a re-denomination of the
    /// request.** A [`Size::ValueFraction`] means the same multiple of equity
    /// on every account — `value_frac(1.0)` is 1x equity here and 1x equity on
    /// a 10x wallet — and raising this number does not enlarge what a document
    /// asks for, it only stops truncating it. So a document reaching for real
    /// leverage says so in its own `sizing:`, and the account says how much of
    /// that it is willing to carry. The two are separate on purpose; see
    /// [`Size`] for why folding one into the other would break
    /// [`vol_target`](crate::indicators::sizing::vol_target).
    ///
    /// The consequence worth stating plainly: **a document whose sizing never
    /// exceeds `1.0` is insensitive to this knob.** That is not most documents.
    /// Every recipe in [`sizing`](crate::indicators::sizing) is unbounded above
    /// — measured on a 1,200-bar three-regime fixture,
    /// [`vol_target`](crate::indicators::sizing::vol_target) at a 20% target
    /// exceeded `1.0` on 54% of bars (median 1.07, max 3.81) and
    /// [`atr_risk`](crate::indicators::sizing::atr_risk) at 2%/2×ATR on 33%
    /// (max 1.96). At the default `1.0` that run had 38 of 139 fills fitted,
    /// the worst to 33.5% of its request, and realized 12.4% vol against its
    /// 20% target. Raising the cap to `3.0` fitted **none** of them and
    /// realized 15.8%. A vol target is only a vol target on an account that can
    /// hold it.
    ///
    /// **Exits are exempt.** No fill that leaves the position's magnitude at or
    /// below where it started is ever refused, so an account carried over its
    /// limit by a mark can always trade its way back — and
    /// [`flatten`](Wallet::flatten) always works.
    ///
    /// **Nothing charges for the borrowing.** fugazi models no cost of carry —
    /// no perpetual funding, no margin interest on a negative cash balance, no
    /// borrow fee — and the [`TradingCosts`] pipeline structurally cannot,
    /// because all three of its legs are per-*fill* and carry accrues on bars
    /// that do not trade. At `1.0` nothing is borrowed and nothing is missing;
    /// above it, a levered run is **optimistic** by an amount that scales with
    /// the leverage and the holding period. Charge it yourself through
    /// [`Wallet::adjust_funds`] if it matters at your horizon. See
    /// `docs/COSTS.md`, *What the pipeline cannot express*.
    ///
    /// Like the cost models and [`retention`](Self::with_retention), this is
    /// *configuration*: it is not carried in
    /// [`snapshot_state`](Self::snapshot_state), so a resumed run takes the cap
    /// of the wallet the caller constructed.
    ///
    /// # Panics
    ///
    /// If `max_gross` is not finite and strictly positive.
    pub fn with_max_gross(mut self, max_gross: Real) -> Self {
        assert!(
            max_gross > 0.0 && max_gross.is_finite(),
            "max_gross must be finite and > 0, got {max_gross}"
        );
        self.max_gross = max_gross;
        self
    }

    /// The gross-exposure multiple this wallet enforces. See
    /// [`with_max_gross`](Self::with_max_gross).
    pub fn max_gross(&self) -> Real {
        self.max_gross
    }

    /// What fraction of a year one bar of this run spans, on the **calendar**.
    ///
    /// Required by any time-denominated cost of carry — an annualized margin
    /// rate has to be pro-rated to the bar before it can be charged. Without it
    /// [`AnnualRate`](crate::costs::AnnualRate) and
    /// [`with_margin_rate`](Self::with_margin_rate) charge **nothing**, because
    /// the alternative is for the wallet to invent a year length and bill
    /// against it.
    ///
    /// **Calendar, not trading time.** `Frequency::calendar_seconds_per_bar /
    /// SECONDS_PER_YEAR` is the number; the trading-seconds figure the metrics
    /// layer annualizes returns with is the *wrong* one here and under-charges a
    /// US equity `1d` bar by nearly 4x. A broker charges interest over the
    /// weekend; the market does not pay returns over it.
    ///
    /// Settlement-denominated carry — [`FundingRate`](crate::costs::FundingRate),
    /// where the venue states the charge in full and the column already sums it
    /// per bar — ignores this entirely.
    ///
    /// # Panics
    ///
    /// If `fraction` is not finite and strictly positive.
    pub fn with_bar_year_fraction(mut self, fraction: Real) -> Self {
        assert!(
            fraction > 0.0 && fraction.is_finite(),
            "bar_year_fraction must be finite and > 0, got {fraction}"
        );
        self.bar_year_fraction = Some(fraction);
        self
    }

    /// [`with_bar_year_fraction`](Self::with_bar_year_fraction) resolved from a
    /// bar cadence — the spelling a caller who knows the run's `Frequency` wants.
    pub fn with_bar_frequency(self, freq: crate::time::Frequency) -> Self {
        self.with_bar_year_fraction(freq.calendar_seconds_per_bar() as Real / SECONDS_PER_YEAR)
    }

    /// The fraction of a year one bar spans, if this wallet was told. See
    /// [`with_bar_year_fraction`](Self::with_bar_year_fraction).
    pub fn bar_year_fraction(&self) -> Option<Real> {
        self.bar_year_fraction
    }

    /// The year fraction **this** bar's carry is charged over.
    ///
    /// Measured from the gap between this bar's open time and the previous
    /// bar's whenever both are known, and only otherwise falling back to the
    /// configured [`bar_year_fraction`](Self::with_bar_year_fraction).
    ///
    /// **Measured beats declared, and that is a fix, not a preference.** A
    /// declared cadence says every bar spans the same interval; the calendar
    /// disagrees on any series with a gap in it. A daily equity bar stamped
    /// Monday follows one stamped Friday, so the position was held for three
    /// days of interest and `Frequency::Day(1)` bills for one — an under-charge
    /// of 3x across every weekend, and worse across a holiday. The same
    /// arithmetic is what lets an **index-sampled** stream (volume, dollar or
    /// tick bars, whose bars span no fixed interval by construction) charge
    /// carry correctly at all.
    ///
    /// `None` — charge nothing — when the stream carries no times *and* no
    /// cadence was declared. Not a licence to guess: see
    /// [`CarryContext::year_fraction`](crate::costs::CarryContext::year_fraction).
    ///
    /// A non-positive gap (a duplicate or out-of-order stamp) is not a
    /// negative charge; it falls back to the declared value like any other
    /// unusable measurement.
    fn effective_year_fraction(&self) -> Option<Real> {
        match (self.last_bar_time, self.pending_bar_time) {
            (Some(prev), Some(now)) if now.0 > prev.0 => {
                Some((now.0 - prev.0) as Real / 1_000.0 / SECONDS_PER_YEAR)
            }
            _ => self.bar_year_fraction,
        }
    }

    /// Annualized interest charged on a **negative** cash balance — what a
    /// margin account bills for the cash it lent you.
    ///
    /// Account-level, not per symbol, because that is what the balance is: once
    /// [`max_gross`](Self::with_max_gross) is above `1.0` a levered long drives
    /// `funds` below zero, and the debt belongs to the account rather than to
    /// any one position. Charged once per bar, on the balance carried *into* the
    /// bar, pro-rated by
    /// [`bar_year_fraction`](Self::with_bar_year_fraction) — and charged
    /// nothing at all without one.
    ///
    /// A positive balance earns nothing: credit interest is real but small, its
    /// rate is not the borrow rate, and paying it would flatter a backtest.
    /// Modelling it would be a second rate, and this one exists to stop a
    /// levered run reporting free money.
    ///
    /// # Panics
    ///
    /// If `annual_rate` is negative or not finite.
    pub fn with_margin_rate(mut self, annual_rate: Real) -> Self {
        assert!(
            annual_rate >= 0.0 && annual_rate.is_finite(),
            "margin_rate must be finite and >= 0, got {annual_rate}"
        );
        self.margin_rate = annual_rate;
        self
    }

    /// The annualized rate charged on borrowed cash. See
    /// [`with_margin_rate`](Self::with_margin_rate).
    pub fn margin_rate(&self) -> Real {
        self.margin_rate
    }

    /// Force-close the whole book when equity falls below `ratio × gross
    /// notional` — a margin call.
    ///
    /// **Off by default**, and that default is a deliberate one rather than an
    /// oversight: liquidation is the one thing here that needs a *venue*
    /// assumption fugazi does not otherwise make. What the maintenance ratio is,
    /// which tier it falls in, what the position is marked against — all of it
    /// varies by exchange and by instrument, so the number is yours to state,
    /// not the library's to guess. Setting it is you supplying the assumption.
    ///
    /// **Why it matters more than carry does.** Omitting funding makes a levered
    /// backtest optimistic by a few percent. Omitting liquidation makes it
    /// describe a *different strategy*: a 3x book that draws down 33% is gone,
    /// and a run that trades on through reports the recovery of an account that
    /// no longer existed. No amount of cost modelling fixes that.
    ///
    /// **Triggered on the bar's adverse extreme, filled at its close.** Equity
    /// is tested with each position marked where the bar hurt it most — the
    /// `low` for a long, the `high` for a short — because a wick is exactly what
    /// liquidates a levered account, and a close-only test would miss the event
    /// that actually happened. The resulting fills book at the `close`, as
    /// [`OrderKind::Liquidation`], since the price at which the breach occurred
    /// is not identifiable from a bar once more than one symbol is involved. So:
    /// the trigger is conservative, the fill price is a simplification, and the
    /// two are documented rather than blended into a single number that looks
    /// exact. See `docs/TRADING.md`.
    ///
    /// Nothing stops the strategy re-entering on the next bar. That is
    /// realistic — a liquidated account with equity left can trade again — and
    /// the blotter's `liquidation` rows are what tell you it happened.
    ///
    /// # Panics
    ///
    /// If `ratio` is not finite, or is outside `(0, 1]`.
    pub fn with_maintenance_margin(mut self, ratio: Real) -> Self {
        assert!(
            ratio > 0.0 && ratio <= 1.0 && ratio.is_finite(),
            "maintenance_margin must be finite and in (0, 1], got {ratio}"
        );
        self.maintenance_margin = Some(ratio);
        self
    }

    /// The maintenance-margin ratio, or `None` when no margin call is modelled.
    /// See [`with_maintenance_margin`](Self::with_maintenance_margin).
    pub fn maintenance_margin(&self) -> Option<Real> {
        self.maintenance_margin
    }

    /// `(bars that wanted a carry rate, bars that got one)` since this wallet
    /// was built.
    ///
    /// A data-driven [`CarryModel`](crate::costs::CarryModel) charges nothing on
    /// a bar whose column carried no sample, which is the honest reading of a
    /// missing value — and also exactly what a run against a series that never
    /// had the column looks like. The two are indistinguishable from the equity
    /// curve, so this counts them: `(1200, 0)` means the funding model was
    /// configured, wanted a rate on twelve hundred bars, and was silently free
    /// on every one of them.
    ///
    /// `(0, 0)` means no carry model asked for data at all.
    pub fn carry_coverage(&self) -> (usize, usize) {
        (self.carry_wanted, self.carry_seen)
    }

    /// How many blotter / rejection entries to retain, or `None` to keep every
    /// one ever recorded. Defaults to [`DEFAULT_RETENTION`].
    ///
    /// Both logs are reporting artifacts that no fill, pricing or resume path
    /// reads, so the default bounds them rather than growing forever in a
    /// long-lived run. `None` restores the unbounded behavior for a caller who
    /// wants the full in-process history and knows the run is finite.
    ///
    /// Retention is *configuration*, like the cost models: it is not carried in
    /// [`snapshot_state`](Self::snapshot_state), so a resumed wallet takes the
    /// limit of the wallet the caller constructed.
    pub fn with_retention(mut self, entries: Option<usize>) -> Self {
        self.set_retention(entries);
        self
    }

    /// [`with_retention`](Self::with_retention) on an existing wallet. Tightening
    /// the limit trims on the spot rather than waiting for the next fill.
    pub fn set_retention(&mut self, entries: Option<usize>) {
        self.retention = entries;
        self.trim();
    }

    /// How many blotter / rejection entries this wallet retains, or `None` if it
    /// keeps every one.
    pub fn retention(&self) -> Option<usize> {
        self.retention
    }

    /// Drop the oldest blotter / rejection entries once either log has grown to
    /// twice [`retention`](Self::with_retention), bringing it back down to the
    /// limit.
    ///
    /// Trimming in batches at `2 × limit` rather than on every push keeps this
    /// amortized O(1) — a `drain` from the front is O(n), so trimming one entry
    /// per push past the limit would make a long run quadratic.
    fn trim(&mut self) {
        trim_front(&mut self.blotter, self.retention);
        let dropped = trim_front(&mut self.rejections, self.retention);
        // The drain cursor indexes the vec's head, which just moved. Any
        // dropped entry that had not been drained yet is gone for good —
        // inherent to a bounded log — but saturating here keeps every
        // *surviving* undrained entry reachable rather than skipping past
        // them. (A zero limit drops everything, so this lands on 0.)
        self.rejections_drained = self.rejections_drained.saturating_sub(dropped);
    }

    /// The most recent executed orders, oldest first (the trade blotter).
    ///
    /// Bounded by [`with_retention`](Self::with_retention) — by default the last
    /// [`DEFAULT_RETENTION`] — and **not** carried across a
    /// [`restore_state`](Self::restore_state), so after a resume this reports
    /// the resumed chunk. It is an observability accessor; durable trade history
    /// is the caller's to keep.
    pub fn orders(&self) -> &[Order<Sym>] {
        &self.blotter
    }

    /// Every queued order [`update`](Wallet::update) tried and failed to fill,
    /// in submission order. Populated by any [`WalletError`] the flush hit —
    /// typically `InsufficientFunds` on a [`Size::Units`] buy larger than
    /// cash on hand (fractional sizings shrink to fit and never end up here),
    /// or `InvalidPrice` on a zero-opening bar. Lets a driver report why a
    /// bar produced no fill instead of the silent drop the pre-fix wallet
    /// left callers with.
    ///
    /// Bounded and resume-scoped exactly like [`orders`](Self::orders).
    pub fn rejections(&self) -> &[Rejection<Sym>] {
        &self.rejections
    }

    /// Restore the wallet to its freshly-constructed state — the seed `funds`
    /// it was built with, no positions, no fed prices, no pending or resting
    /// orders, and an empty blotter. Lets one wallet drive successive runs.
    pub fn reset(&mut self) {
        self.positions.clear();
        self.bars.clear();
        self.pending.clear();
        self.protective.clear();
        self.limits.clear();
        self.blotter.clear();
        self.rejections.clear();
        self.rejections_drained = 0;
        self.funds = self.initial_funds;
        self.next_id = 0;
    }

    /// Mint the next unique [`OrderId`].
    fn mint(&mut self) -> OrderId {
        let id = OrderId(self.next_id);
        self.next_id += 1;
        id
    }
}

/// The serializable slice of a [`PaperWallet`]'s state, for run resuming.
///
/// Everything the wallet needs to continue a run except the cost models, which
/// are configuration re-primed from the caller (the CLI's `--costs`, the
/// strategy spec) rather than persisted — a venue owns its own fees.
///
/// **History is not state.** The blotter and the rejection log used to be
/// persisted too, and they dominated the file: on a 1500-bar 8-symbol basket
/// they were 98% of it (253 KB of 258 KB), growing without bound in the number
/// of bars while everything else here stays bounded by the universe and the
/// indicators' periods. Nothing reads them across the seam — no logic consults
/// the blotter at all ([`orders`](PaperWallet::orders) is an observability
/// accessor), the [`RunReport`](crate::RunReport)'s fills come from
/// [`Wallet::update`]'s return value rather than from here, and
/// [`take_rejections`](Wallet::take_rejections) only needs "everything so far
/// has been drained", which an empty log with a zero cursor states exactly. So
/// a resumed wallet starts both fresh, and `orders()` reports the resumed
/// chunk — which is what the per-chunk `RunReport` already did.
///
/// Reading an older state that still carries those keys is unaffected: serde
/// ignores unknown fields, so a pre-existing snapshot resumes identically and
/// the format version does not move.
#[derive(Serialize, Deserialize)]
#[serde(bound(
    serialize = "Sym: Serialize + Eq + Hash",
    deserialize = "Sym: Deserialize<'de> + Eq + Hash"
))]
// Mirrors the wallet's own map type so save/load is a move rather than a
// rehash. The persisted shape is unaffected: serde writes either as a JSON
// object, and `serde_json`'s map is a `BTreeMap`, so the key order on disk is
// sorted regardless of the hasher. Existing saved states load unchanged.
struct WalletSnapshot<Sym> {
    positions: SymMap<Sym, Real>,
    bars: SymMap<Sym, Candle>,
    pending: SymMap<Sym, Pending>,
    protective: SymMap<Sym, Protective>,
    limits: SymMap<Sym, RestingLimit>,
    funds: Real,
    initial_funds: Real,
    next_id: u64,
    /// Bar-open time of the last advanced bar, as raw epoch milliseconds —
    /// `Timestamp` is deliberately not `Serialize` (it keeps `time` out of the
    /// core ABI), and the flat `i64` is its whole content.
    ///
    /// `#[serde(default)]` so a state file written before carry was measured
    /// per bar still loads; it resumes with no left endpoint and falls back to
    /// the declared cadence for one bar, exactly as it did when it was saved.
    #[serde(default)]
    last_bar_time: Option<i64>,
}

impl<Sym: Clone + Eq + Hash + Serialize + DeserializeOwned> PaperWallet<Sym> {
    /// Serialize the wallet's resumable state — cash, positions, fed prices,
    /// queued and resting orders, and the id counter. The cost models are
    /// deliberately excluded (see `WalletSnapshot`); a resumed run re-primes
    /// them from the caller. So are the blotter and the rejection log, which are
    /// history rather than state — see `WalletSnapshot` for why.
    pub fn snapshot_state(&self) -> serde_json::Value {
        let snapshot = WalletSnapshot {
            positions: self.positions.clone(),
            bars: self.bars.clone(),
            pending: self.pending.clone(),
            protective: self.protective.clone(),
            limits: self.limits.clone(),
            funds: self.funds,
            initial_funds: self.initial_funds,
            next_id: self.next_id,
            last_bar_time: self.last_bar_time.map(|t| t.0),
        };
        serde_json::to_value(&snapshot).expect("WalletSnapshot is serializable")
    }

    /// Restore state produced by [`snapshot_state`](Self::snapshot_state). Leaves
    /// the cost models untouched — they were set by the freshly-constructed
    /// wallet (via `--costs` / the spec) before this call.
    ///
    /// The blotter and rejection log are left as the fresh wallet has them —
    /// empty, with the drain cursor at zero — so the resumed run reports its own
    /// fills rather than replaying the previous chunk's.
    pub fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        let snapshot: WalletSnapshot<Sym> =
            serde_json::from_value(state.clone()).map_err(|e| format!("wallet: {e}"))?;
        self.positions = snapshot.positions;
        self.bars = snapshot.bars;
        self.pending = snapshot.pending;
        self.protective = snapshot.protective;
        self.limits = snapshot.limits;
        self.funds = snapshot.funds;
        self.initial_funds = snapshot.initial_funds;
        self.next_id = snapshot.next_id;
        // Carried across the chunk boundary so the first bar of a resumed run
        // charges the interval since the last bar of the previous one, rather
        // than starting from no left endpoint and silently skipping it.
        self.last_bar_time = snapshot.last_bar_time.map(Timestamp);
        Ok(())
    }
}

impl<Sym: Clone + Eq + Hash> PaperWallet<Sym> {
    /// Cash plus every position marked at `mark(symbol)`, summed in the
    /// canonical order [`marked_sum`] defines.
    ///
    /// Single-symbol runs — the common case — were never affected by the
    /// ordering, which is why the drift this guards against survived so long.
    fn marked_equity(&self, mark: impl Fn(&Sym) -> Real) -> Reference {
        Reference(marked_sum(
            self.funds,
            self.positions
                .iter()
                .map(|(symbol, &amount)| amount * mark(symbol)),
        ))
    }

    /// Every position except `traded`, marked at `mark` and summed both signed
    /// (equity) and absolute (exposure) — see [`RestOfBook`].
    ///
    /// The traded symbol contributes `0.0` rather than being filtered out, so
    /// both sums keep the [`ExactSizeIterator`] [`marked_sum`] needs for its
    /// stack buffer — and, more to the point, land in the same canonical order
    /// [`marked_equity`](Self::marked_equity) uses. A leverage check summed in
    /// `HashMap` order would admit a fill on one run and refuse it on the next.
    fn rest_of_book(&self, traded: &Sym, mark: impl Fn(&Sym) -> Real) -> RestOfBook {
        let marked = marked_sum(
            0.0,
            self.positions.iter().map(|(symbol, &amount)| {
                if symbol == traded {
                    0.0
                } else {
                    amount * mark(symbol)
                }
            }),
        );
        let gross = marked_sum(
            0.0,
            self.positions.iter().map(|(symbol, &amount)| {
                if symbol == traded {
                    0.0
                } else {
                    (amount * mark(symbol)).abs()
                }
            }),
        );
        RestOfBook { marked, gross }
    }

    /// Charge one bar of carry — the cost of *holding*, as opposed to the cost
    /// of trading — and return what was taken out of cash.
    ///
    /// Two charges, on two different things:
    ///
    /// 1. **Per position**, through each symbol's [`CarryModel`]: funding, a
    ///    borrow fee, position-denominated interest. Signed, so a credit is a
    ///    credit.
    /// 2. **Per account**, on a negative cash balance, at
    ///    [`margin_rate`](Self::margin_rate). This is what a levered *long*
    ///    actually pays: above `max_gross = 1.0` cash goes negative, and the
    ///    debt is the account's rather than any one position's.
    ///
    /// Charged on the position and the balance **carried into** the bar, marked
    /// at that bar's `open` — you pay for what you held through the interval,
    /// not for what you ended up with. Which is also why this runs before the
    /// bar's fills and before `equity_at_open`: the charge is a fact about the
    /// interval that just elapsed, and this bar's sizing should see the account
    /// it left the caller with.
    ///
    /// **Only symbols that ticked this bar are charged.** A position in a
    /// symbol the snapshot skipped has no mark to value the charge at, and
    /// carrying the last close forward would bill against a price this bar never
    /// saw. It is the same rule the wallet's mark-to-market already follows, and
    /// it means a gappy series under-charges — stated here rather than hidden.
    fn accrue_carry(&mut self, bars: &[(Sym, Candle)]) -> Real {
        // The balance the bar opened with, before any of this bar's carry.
        let opening_funds = self.funds;
        let mut charge = 0.0;
        // Resolved once for the bar: every symbol in a snapshot shares its
        // open time, so a per-symbol answer could only differ by being wrong.
        let year_fraction = self.effective_year_fraction();

        for (symbol, candle) in bars {
            let position = self.positions.get(symbol).copied().unwrap_or(0.0);
            if position.abs() <= POSITION_EPSILON {
                continue;
            }
            let costs = self.per_symbol_costs.get(symbol).unwrap_or(&self.costs);
            let rate = if costs.carry.column().is_some() {
                self.carry_wanted += 1;
                let sample = self.carry_rates.get(symbol).copied();
                if sample.is_some() {
                    self.carry_seen += 1;
                }
                sample
            } else {
                None
            };
            charge += costs.carry.carry(&crate::costs::CarryContext {
                position,
                price: candle.open,
                year_fraction,
                rate,
            });
        }

        // Interest on borrowed cash. `opening_funds` rather than the running
        // balance, so the per-position leg above cannot change what the account
        // is billed for having borrowed.
        if self.margin_rate > 0.0
            && opening_funds < 0.0
            && let Some(year_fraction) = year_fraction
        {
            charge += -opening_funds * self.margin_rate * year_fraction;
        }

        if charge != 0.0 {
            self.funds -= charge;
        }
        charge
    }

    /// Whether this bar breached the maintenance margin, marking every position
    /// where the bar hurt it most.
    ///
    /// A long is marked at the `low` and a short at the `high`, because a wick
    /// is what liquidates a levered account and a close-only test would miss the
    /// event that actually happened. Symbols the bar skipped keep their last
    /// close — an unmarked position is still exposure, and dropping it from the
    /// test would let a gappy series hide a breach.
    fn breaches_maintenance(&self, bars: &[(Sym, Candle)]) -> bool {
        let Some(ratio) = self.maintenance_margin else {
            return false;
        };
        let adverse = |symbol: &Sym, amount: Real| -> Real {
            let candle = bars
                .iter()
                .find(|(sym, _)| sym == symbol)
                .map(|(_, c)| *c)
                .or_else(|| self.bars.get(symbol).copied());
            match candle {
                Some(c) if amount >= 0.0 => c.low,
                Some(c) => c.high,
                None => 0.0,
            }
        };
        let gross = marked_sum(
            0.0,
            self.positions
                .iter()
                .map(|(symbol, &amount)| (amount * adverse(symbol, amount)).abs()),
        );
        if gross <= 0.0 {
            return false;
        }
        let equity = marked_sum(
            self.funds,
            self.positions
                .iter()
                .map(|(symbol, &amount)| amount * adverse(symbol, amount)),
        );
        equity < ratio * gross
    }

    /// Force-close every open position at its last close, as
    /// [`OrderKind::Liquidation`].
    ///
    /// [`flatten`](Wallet::flatten)'s twin, and deliberately a separate body
    /// rather than a parameter on it: the *kind* is the whole point. A blotter
    /// that books a margin call as an ordinary market exit cannot answer whether
    /// a levered run's flat stretch was the strategy standing aside or the
    /// account being closed out from under it.
    fn liquidate(&mut self) -> Vec<Order<Sym>> {
        let mut open: Vec<Sym> = self
            .positions
            .iter()
            .filter(|(_, amount)| amount.abs() > POSITION_EPSILON)
            .map(|(symbol, _)| symbol.clone())
            .collect();
        open.sort_by_key(|s| self.bars.get(s).map_or(0, |c| c.close.to_bits()));

        let mut fills = Vec::new();
        for symbol in open {
            let Some(price) = self.bars.get(&symbol).map(|c| c.close) else {
                continue;
            };
            let id = self.mint();
            match self.fill_at(
                symbol.clone(),
                0.0,
                price,
                OrderKind::Liquidation,
                id,
                FillContext::reducing(0.0),
            ) {
                Ok(Some(order)) => fills.push(order),
                Ok(None) => {}
                Err(error) => self.push_rejection(&symbol, id, error, OrderKind::Liquidation),
            }
        }
        self.pending.clear();
        self.protective.clear();
        self.limits.clear();
        fills
    }

    /// Whether holding `held` gross notional against `equity` breaches this
    /// wallet's [`max_gross`](Self::max_gross), given `others` already on the
    /// book — and by how much room it is over or under.
    ///
    /// Returns the notional the traded symbol is *allowed* to hold. Negative
    /// means the rest of the book has already used the whole budget.
    fn gross_budget(&self, equity_after: Real, others: Real) -> Real {
        self.max_gross * equity_after - others
    }

    /// Book a fill: drive `symbol` to `target` signed units, using
    /// `theoretical_price` as the pre-cost trigger price (bar `open` for a
    /// market order, the trigger level — or the `open` on a gap — for a stop /
    /// take-profit). The wallet's [`TradingCosts`] pipeline then applies
    /// **spread → slippage → commission**, and the final price is what lands on
    /// the [`Order`]. `kind`/`id` tag the resulting fill.
    ///
    /// The engine behind every fill — the queued market flush and the
    /// resting-order triggers both route here. Returns the [`Order`] (also
    /// pushed to the blotter), `Ok(None)` if already at `target`, or a
    /// [`WalletError`] (`UnknownPrice`, `InvalidPrice`, `PriceOutOfRange`,
    /// `InsufficientFunds`, `ExceedsMaxGross`).
    ///
    /// `ctx` carries what the caller *asked* for (which becomes
    /// [`Order::requested_units`]) and the rest of the book as that caller
    /// marks it — this is the one place the account's two solvency rules are
    /// enforced, so it is the one place both have to be answerable.
    fn fill_at(
        &mut self,
        symbol: Sym,
        target: Real,
        theoretical_price: Real,
        kind: OrderKind,
        id: OrderId,
        ctx: FillContext,
    ) -> Result<Option<Order<Sym>>, WalletError> {
        let current = self.positions.get(&symbol).copied().unwrap_or(0.0);
        let delta = target - current;
        if delta.abs() <= POSITION_EPSILON {
            return Ok(None);
        }
        // Last guard, and the one that matters: a `NaN` reads false against
        // every `>` and `<` below, so it passes both solvency rules and the
        // range check, books a `NaN` position, and takes cash and equity with it
        // for the rest of the run. `delta` covers `target`; the fraction-sized
        // paths resolve here rather than at submission, so this is where they
        // are caught.
        if !delta.is_finite() {
            return Err(WalletError::InvalidQuantity);
        }
        let bar = *self.bars.get(&symbol).ok_or(WalletError::UnknownPrice)?;
        if !theoretical_price.is_finite() || theoretical_price <= 0.0 {
            return Err(WalletError::InvalidPrice);
        }
        // The pre-cost price must be one the bar actually traded at; cost
        // adjustments (spread, slippage) may push the *final* fill price
        // outside the bar's range and that is fine — a real market fill can
        // execute above the tape.
        if theoretical_price < bar.low - PRICE_EPSILON
            || theoretical_price > bar.high + PRICE_EPSILON
        {
            return Err(WalletError::PriceOutOfRange);
        }

        // Apply the costs pipeline: spread → slippage → commission. Direction
        // is derived from `delta`'s sign (buys pay the ask, sells receive the
        // bid), and the fill kind threads through so a stop can slip further
        // than a plain market fill. Per-symbol overrides installed via
        // [`set_costs_for`](Self::set_costs_for) win over the default bundle.
        let side = if delta > 0.0 { Side::Buy } else { Side::Sell };
        let units = delta.abs();
        let costs = self.per_symbol_costs.get(&symbol).unwrap_or(&self.costs);
        let half_spread = half_spread_for(costs, kind, theoretical_price, &bar);
        let post_spread = match side {
            Side::Buy => theoretical_price + half_spread,
            Side::Sell => theoretical_price - half_spread,
        };
        let final_price = costs.slippage.adjust(side, post_spread, units, &bar, kind);
        // A pathological cost config could drive the fill non-positive; refuse
        // rather than book a negative-value trade.
        if final_price <= 0.0 {
            return Err(WalletError::InvalidPrice);
        }
        let notional = final_price * units;
        let commission = costs.commission.commission(notional, units).max(0.0);

        // Solvency, in two rules — see `fit_to_account`, which fits a
        // fractional sizing to exactly these before it ever gets here.
        //
        // 1. Cash. On an unlevered wallet a net buy plus its commission can't
        //    drive cash below zero (tolerant of the epsilon rounding in an
        //    all-in `value_frac(1.0)`, whose cost equals funds when zero-cost).
        //    Above `max_gross = 1.0` the account may borrow, so this lifts.
        if delta > 0.0 && self.max_gross <= 1.0 {
            let cost = delta * final_price + commission;
            let tolerance = cash_tolerance(self.funds);
            if cost - self.funds > tolerance {
                return Err(WalletError::InsufficientFunds);
            }
        }
        // 2. Leverage. Gross notional after this fill must fit
        //    `max_gross × equity`. Only a fill that *raises* the position's
        //    magnitude is bound: an exit lowers gross, so an account carried
        //    over the line by a mark can always trade its way back — and
        //    `flatten` can always flatten. For a long-only book at the default
        //    `1.0` this is rule 1 restated (`gross <= equity` **is**
        //    `funds >= 0` when nothing is short), which is why an unlevered
        //    long backtest fills exactly as it did before this existed; what it
        //    adds is the same bound on the short side, where crediting cash
        //    means rule 1 never fires.
        if target.abs() > current.abs() {
            let equity_after = self.funds + current * final_price + ctx.rest.marked - commission;
            let allowed = self.gross_budget(equity_after, ctx.rest.gross);
            let held = target.abs() * final_price;
            if held - allowed > gross_tolerance(held, equity_after) {
                return Err(WalletError::ExceedsMaxGross);
            }
        }
        let order = Order::from_delta(symbol.clone(), delta, final_price, kind, id)
            .expect("delta exceeds POSITION_EPSILON, so the order is non-empty")
            .with_commission(commission)
            .with_requested_units((ctx.requested - current).abs());
        // Pay for a buy, receive for a sell — and pay commission out of cash
        // on both sides.
        self.funds -= order.signed_units() * final_price + commission;
        let new_position = current + delta;
        if new_position.abs() <= POSITION_EPSILON {
            self.positions.remove(&symbol);
        } else {
            self.positions.insert(symbol.clone(), new_position);
        }
        // A fill that flattens or flips the sign voids any resting bracket (so a
        // bare market exit / reversal drops a now-stale stop even without an
        // explicit cancel).
        if new_position.abs() <= POSITION_EPSILON || current * new_position < 0.0 {
            self.protective.remove(&symbol);
        }
        self.blotter.push(order.clone());
        self.trim();
        Ok(Some(order))
    }

    /// Fit a resolved [`Size`] magnitude to what the account can actually
    /// carry: a net buy within available cash, and **either** side within the
    /// wallet's [`max_gross`](Self::max_gross) leverage cap.
    ///
    /// Only fractional sizings (`ValueFraction` / `FundsFraction`) come through
    /// here — [`Size::Units`] is a caller-explicit unit count that should fail
    /// loudly rather than silently truncate. What it returns is what fills; the
    /// magnitude that was *asked* for rides along to the blotter as
    /// [`Order::requested_units`], so a rounding adjustment and a 3× reduction
    /// are no longer the same silence.
    ///
    /// **Two constraints, and each binds a different side of the book.**
    ///
    /// 1. *Cash*, on a net buy, when the wallet is unlevered
    ///    (`max_gross <= 1.0`): spread + slippage + commission must fit
    ///    available funds. Above `1.0` the account may borrow, and this stops
    ///    applying — a negative cash balance is what leverage buys.
    /// 2. *Gross exposure*, on any fill that raises the position's magnitude:
    ///    `|target| × price` plus the rest of the book must stay within
    ///    `max_gross × equity`. This is the one a **short** runs into. A sale
    ///    credits cash, so constraint 1 never fires on one; without this a
    ///    `value_frac(3.0)` short simply took 3× the exposure its long twin
    ///    took under the same spec value.
    ///
    /// A fill that does not raise the magnitude is bounded by neither: an exit
    /// frees cash and lowers gross, so it is returned untouched.
    ///
    /// The cost pipeline is opaque behind [`CommissionModel`] / [`SpreadModel`]
    /// / [`SlippageModel`] so the fit is a fixed-point iteration rather
    /// than a closed-form invert: probe both constraints at the current
    /// magnitude, scale down by the tighter deficit ratio, repeat. Converges in
    /// one step for linear cost shapes (`PercentageCommission`,
    /// `FixedBpsSpread`), quickly for the others; an 8-iteration cap keeps a
    /// pathological composite bounded.
    ///
    /// `price` and `kind` are parameters rather than `candle.open` /
    /// `OrderKind::Market` because a resting limit fills at its own price: sized
    /// against the open, an all-in limit buy below the market would shrink to
    /// the units the *open* could afford rather than the (larger) number its
    /// own cheaper fill can.
    fn fit_to_account(
        &self,
        symbol: &Sym,
        side: Side,
        current: Real,
        magnitude: Real,
        pricing: FillPricing<'_>,
        rest: RestOfBook,
    ) -> Fitted {
        let FillPricing {
            bar: candle,
            price,
            kind,
        } = pricing;
        if magnitude <= 0.0 {
            return Fitted::whole(magnitude);
        }
        // A pure reduction — a sell that doesn't flip through zero, or a buy
        // that only covers part of a short — credits cash and lowers gross.
        // Neither constraint can bind, so skip pricing it at all.
        let asked = side.sign() * magnitude;
        if asked - current <= 0.0 && asked.abs() <= current.abs() {
            return Fitted::whole(magnitude);
        }
        let costs = self.per_symbol_costs.get(symbol).unwrap_or(&self.costs);
        let mut m = magnitude;
        let mut last_bound = None;
        for _ in 0..8 {
            let target = side.sign() * m;
            let delta = target - current;
            // Price the fill exactly as `fill_at` will: direction from the
            // delta's sign, so a reversal's *sell* leg is priced as a sell.
            let direction = if delta > 0.0 { Side::Buy } else { Side::Sell };
            let half_spread = half_spread_for(costs, kind, price, candle);
            let post_spread = match direction {
                Side::Buy => price + half_spread,
                Side::Sell => price - half_spread,
            };
            let units = delta.abs();
            let final_price = costs
                .slippage
                .adjust(direction, post_spread, units, candle, kind);
            if final_price <= 0.0 {
                return Fitted {
                    magnitude: 0.0,
                    bound: Some(WalletError::InvalidPrice),
                };
            }
            let notional = final_price * units;
            let commission = costs.commission.commission(notional, units).max(0.0);

            // The tighter of the two deficits wins; `1.0` means nothing binds.
            // `bound` records *which* one did, so a fit that collapses to no
            // trade at all can say why — see `Fitted`.
            let mut scale: Real = 1.0;
            let mut bound = None;
            if delta > 0.0 && self.max_gross <= 1.0 {
                let cost = notional + commission;
                if cost - self.funds > cash_tolerance(self.funds) && cost > 0.0 {
                    scale = scale.min(self.funds / cost);
                    bound = Some(WalletError::InsufficientFunds);
                }
            }
            if target.abs() > current.abs() {
                // Equity after this fill is independent of its size (a trade at
                // its own fill price moves nothing but the commission), so this
                // is a genuine bound on `|target|` rather than a moving one.
                let equity_after = self.funds + current * final_price + rest.marked - commission;
                let allowed = self.gross_budget(equity_after, rest.gross);
                let held = target.abs() * final_price;
                if held - allowed > gross_tolerance(held, equity_after) && held > 0.0 {
                    let gross_scale = (allowed / held).max(0.0);
                    if gross_scale < scale {
                        scale = gross_scale;
                    }
                    // The *scale* is the tighter of the two deficits; the
                    // *label* is not, and follows the convention on
                    // `ExceedsMaxGross`: at `max_gross = 1.0` on a long the two
                    // rules are algebraically the same condition, and the cash
                    // check is the one reported under its own name. Saying "this
                    // fill would exceed the account's gross exposure limit"
                    // about a one-unit buy on a flat unlevered account — which
                    // is what an unaffordable flat fee produces, since the fee
                    // drives `equity_after` negative and the gross budget with
                    // it — is true and useless.
                    bound.get_or_insert(WalletError::ExceedsMaxGross);
                }
            }
            if scale >= 1.0 {
                // Nothing binds *at this size* — but an earlier pass may be the
                // reason the size is what it is, and when the shrink converged
                // all the way to zero this is the iteration that sees a clear
                // board. Carrying `last_bound` is what keeps a leg that was
                // fitted out of existence from reporting itself as fitted whole.
                return Fitted {
                    magnitude: m,
                    bound: last_bound,
                };
            }
            let next = m * scale.clamp(0.0, 1.0);
            if (m - next).abs() <= cash_tolerance(m) {
                return Fitted {
                    magnitude: next.max(0.0),
                    bound,
                };
            }
            m = next;
            last_bound = bound;
        }
        Fitted {
            magnitude: m.max(0.0),
            bound: last_bound,
        }
    }

    /// Pre-flight a market submission against the last close: reject
    /// synchronously when the symbol has never been priced, its close is
    /// non-positive, or the move from `current` to `target` clearly breaches
    /// either solvency rule at that price.
    ///
    /// Used by [`Wallet::set_position`](Wallet::set_position) — the
    /// unit-explicit market path — and mirrors what a live venue does with an
    /// unfillable order. See [`check_solvency`](Self::check_solvency) for why
    /// the answer is approximate.
    fn preflight_market(
        &self,
        symbol: &Sym,
        current: Real,
        target: Real,
    ) -> Result<(), WalletError> {
        if !target.is_finite() {
            return Err(WalletError::InvalidQuantity);
        }
        let close = self.price(symbol).ok_or(WalletError::UnknownPrice)?.0;
        if close <= 0.0 {
            return Err(WalletError::InvalidPrice);
        }
        self.check_solvency(symbol, current, target, close)
    }

    /// Reject a net-buy `delta` whose notional at `price` clearly exceeds
    /// cash on hand. Sells and no-ops always clear. Uses the same
    /// funds-plus-tolerance comparison [`fill_at`](Self::fill_at) does at
    /// fill time so a submission passing this is virtually certain to fill
    /// (barring a big gap in the open).
    /// Book a **submission-time** refusal and hand the error back.
    ///
    /// The pre-flight paths on [`set`](Wallet::set) /
    /// [`set_position`](Wallet::set_position) return `Err` synchronously, which
    /// a [`Strategy`](crate::Strategy) has no way to report — its `trade`
    /// returns `()`, so the caller's only option is `let _ = ...`. Recording the
    /// refusal here puts it on the same failure stream as a fill-time drop, so
    /// the driver surfaces both through
    /// [`take_rejections`](Wallet::take_rejections) regardless of *when* the
    /// order died. An id is minted for it so it correlates like any other
    /// submission, even though no order was ever queued.
    fn reject_submission(&mut self, symbol: &Sym, error: WalletError) -> WalletError {
        let id = self.mint();
        self.push_rejection(symbol, id, error, OrderKind::Market);
        error
    }

    /// Record one refused order on the rejection log, honoring the retention
    /// bound. Every rejection goes through here so the trim can't be forgotten
    /// at a new refusal site.
    fn push_rejection(&mut self, symbol: &Sym, id: OrderId, error: WalletError, kind: OrderKind) {
        self.rejections.push(Rejection {
            symbol: symbol.clone(),
            id,
            error,
            kind,
        });
        self.trim();
    }

    /// Refuse a move from `current` to `target` units of `symbol` that the
    /// account plainly cannot carry at `price` — the submission-time twin of the
    /// two solvency rules [`fill_at`](Self::fill_at) enforces, in the same order
    /// and under the same names.
    ///
    /// **Approximate on purpose.** It prices at the last close rather than the
    /// fill the caller will actually get, and ignores the cost pipeline, so a
    /// submission that just clears here can still land in the rejection log at
    /// fill time if the open gaps. The point is to refuse the plainly
    /// infeasible synchronously, the way a live venue does, not to predict the
    /// fill.
    ///
    /// Only unit-explicit paths ([`Size::Units`], [`set_position`](Wallet::set_position))
    /// come through here: a fractional sizing is *fitted* to both rules at fill
    /// time instead of refused, so pre-flighting one would reject an order the
    /// wallet was about to make feasible.
    fn check_solvency(
        &self,
        symbol: &Sym,
        current: Real,
        target: Real,
        price: Real,
    ) -> Result<(), WalletError> {
        let delta = target - current;
        if delta > 0.0 && self.max_gross <= 1.0 {
            let cost = delta * price;
            if cost - self.funds > cash_tolerance(self.funds) {
                return Err(WalletError::InsufficientFunds);
            }
        }
        // Only a move that raises exposure is bound — the same exemption that
        // keeps an exit, and `flatten`, always feasible. This is the check a
        // short hits: `set_position(sym, -500)` costs no cash at all, so
        // without it the only thing standing between a spec and a 5× book was
        // that nobody had asked.
        if target.abs() > current.abs() {
            let rest = self.rest_of_book(symbol, |s| self.bars.get(s).map_or(0.0, |c| c.close));
            let equity = self.funds + current * price + rest.marked;
            let allowed = self.gross_budget(equity, rest.gross);
            let held = target.abs() * price;
            if held - allowed > gross_tolerance(held, equity) {
                return Err(WalletError::ExceedsMaxGross);
            }
        }
        Ok(())
    }

    /// Trigger and fill a resting protective leg on `symbol` against `candle`, if
    /// one is crossed. Stop-loss takes precedence over take-profit, and at most one
    /// leg fills per bar (the fill flattens, which drops the whole bracket).
    /// Fill any resting limit on `symbol` that this bar traded through.
    ///
    /// A buy triggers when `low` reaches the limit and fills at
    /// `min(limit, open)`; a sell triggers on `high` and fills at
    /// `max(limit, open)`. Both spellings say the same thing: **at the limit
    /// or better, never worse** — a bar that gapped past the limit hands the
    /// caller the better `open` rather than their stale price.
    ///
    /// The [`Size`] resolves here, at the fill price, so an all-in sizes
    /// against the equity when the limit is hit. Equity marks this symbol at
    /// the fill price rather than the bar's `close` for the same reason
    /// [`Pending::Sized`] marks at the `open`: sizing must not see information
    /// from later in the bar than the fill.
    ///
    /// The order is consumed whether or not it books — a limit that triggers
    /// but can't be afforded is a rejection, not something to silently retry
    /// next bar at a price the market has already left behind.
    /// Whether `symbol`'s resting limit triggers on `candle`, and the price it
    /// would fill at. **Pure** — the split from [`execute_limit`] is what lets
    /// [`advance`](Wallet::advance) evaluate every symbol's trigger before
    /// booking any of them, so no fill is priced against a cash balance that
    /// depends on which symbol the caller listed first.
    fn limit_trigger(&self, symbol: &Sym, candle: &Candle) -> Option<(RestingLimit, Real)> {
        let resting = *self.limits.get(symbol)?;
        let fill = match resting.side {
            Side::Buy if candle.low <= resting.limit + POSITION_EPSILON => {
                resting.limit.min(candle.open)
            }
            Side::Sell if candle.high >= resting.limit - POSITION_EPSILON => {
                resting.limit.max(candle.open)
            }
            _ => return None,
        };
        Some((resting, fill))
    }

    /// Book a limit fill [`limit_trigger`] has already found. Sizing resolves
    /// here, not at trigger time, so it sees the cash balance as of this
    /// phase — after every credit this bar has settled.
    fn execute_limit(
        &mut self,
        symbol: &Sym,
        resting: RestingLimit,
        fill: Real,
        candle: &Candle,
    ) -> Option<Order<Sym>> {
        self.limits.remove(symbol);

        let position = self.positions.get(symbol).copied().unwrap_or(0.0);
        // The rest of the book at its last closes; this symbol at the fill
        // price. A limit settles late in the bar (phase 6), so the closes are
        // the honest marks here — the same ones the equity this sizes against
        // has always used.
        let rest = self.rest_of_book(symbol, |s| self.bars.get(s).map_or(0.0, |c| c.close));
        let equity_at_fill = self.funds + position * fill + rest.marked;
        let magnitude = resting
            .size
            .resolve(fill, position, self.funds, equity_at_fill);
        let requested = resting.side.sign() * magnitude;
        // Same rule as the queued-market path: a fractional sizing means "as
        // much as fits", so fit it to cash and to the leverage cap; an explicit
        // unit count is a specific intent and is left alone.
        let (magnitude, bound) = match resting.size {
            Size::ValueFraction(_) | Size::FundsFraction(_) => {
                let fitted = self.fit_to_account(
                    symbol,
                    resting.side,
                    position,
                    magnitude,
                    FillPricing {
                        bar: candle,
                        price: fill,
                        kind: OrderKind::Limit,
                    },
                    rest,
                );
                (fitted.magnitude, fitted.bound)
            }
            Size::Units(_) | Size::PositionFraction(_) => (magnitude, None),
        };
        let target = resting.side.sign() * magnitude;

        match self.fill_at(
            symbol.clone(),
            target,
            fill,
            OrderKind::Limit,
            resting.id,
            FillContext::exact(requested, rest),
        ) {
            // See the market path: a fit that removed the trade entirely leaves
            // no order to carry `requested_units`, so the refusal is booked here
            // or it is invisible.
            Ok(None) => {
                if let Some(error) = bound
                    && (requested - position).abs() > POSITION_EPSILON
                {
                    self.push_rejection(symbol, resting.id, error, OrderKind::Limit);
                }
                None
            }
            Ok(order) => order,
            Err(error) => {
                self.push_rejection(symbol, resting.id, error, OrderKind::Limit);
                None
            }
        }
    }

    /// Whether a resting protective leg on `symbol` triggers on `candle`, and
    /// the price and kind it would fill at. **Pure**, for the same reason
    /// [`limit_trigger`] is.
    fn protective_trigger(&self, symbol: &Sym, candle: &Candle) -> Option<(Leg, Real, OrderKind)> {
        let pos = self.positions.get(symbol).copied().unwrap_or(0.0);
        let prot = *self.protective.get(symbol)?;
        // Downside exits (long stop, short target) fill at the level, or lower at
        // the open on a gap: `min(level, open)`. Upside exits are `max`. Either way
        // the fill stays within the bar's range.
        let (leg, fill, kind) = if pos > POSITION_EPSILON {
            if let Some(leg) = prot.stop
                && candle.low <= leg.trigger + POSITION_EPSILON
            {
                (leg, leg.trigger.min(candle.open), OrderKind::Stop)
            } else if let Some(leg) = prot.take_profit
                && candle.high >= leg.trigger - POSITION_EPSILON
            {
                (leg, leg.trigger.max(candle.open), OrderKind::TakeProfit)
            } else {
                return None;
            }
        } else if pos < -POSITION_EPSILON {
            if let Some(leg) = prot.stop
                && candle.high >= leg.trigger - POSITION_EPSILON
            {
                (leg, leg.trigger.max(candle.open), OrderKind::Stop)
            } else if let Some(leg) = prot.take_profit
                && candle.low <= leg.trigger + POSITION_EPSILON
            {
                (leg, leg.trigger.min(candle.open), OrderKind::TakeProfit)
            } else {
                return None;
            }
        } else {
            return None;
        };
        Some((leg, fill, kind))
    }

    /// Book a protective fill [`protective_trigger`] has already found.
    ///
    /// A protective leg that triggers but cannot be booked is the worst
    /// silent failure in the wallet: the strategy believes its stop is
    /// protecting it, and the bracket stays resting (`fill_at` only clears it
    /// on success) so it retries next bar — but without this nobody is ever
    /// told the exit did not happen.
    fn execute_protective(
        &mut self,
        symbol: &Sym,
        leg: Leg,
        fill: Real,
        kind: OrderKind,
    ) -> Option<Order<Sym>> {
        let pos = self.positions.get(symbol).copied().unwrap_or(0.0);
        // Reduce-only: resolve the leg's size at the fill price, clamp it to the
        // position's magnitude, and step *toward* zero. `position_frac(1.0)` —
        // what every whole-position exit passes — resolves to `|pos|` and so
        // flattens, exactly as an unsized leg used to.
        let magnitude = leg
            .size
            .resolve(fill, pos, self.funds, self.equity().0)
            .min(pos.abs());
        let target = pos - pos.signum() * magnitude;
        match self.fill_at(
            symbol.clone(),
            target,
            fill,
            kind,
            leg.id,
            FillContext::reducing(target),
        ) {
            Ok(order) => order,
            Err(error) => {
                self.push_rejection(symbol, leg.id, error, kind);
                None
            }
        }
    }
}

impl<Sym: Clone + Eq + Hash> Wallet<Sym> for PaperWallet<Sym> {
    fn funds(&self) -> Reference {
        Reference(self.funds)
    }

    /// The inherent [`carry_coverage`](Self::carry_coverage), lifted onto the
    /// trait so a generic driver can carry it into the report.
    fn carry_coverage(&self) -> Option<(usize, usize)> {
        Some(PaperWallet::carry_coverage(self))
    }

    fn position(&self, symbol: &Sym) -> Units<Sym> {
        Units {
            symbol: symbol.clone(),
            amount: self.positions.get(symbol).copied().unwrap_or(0.0),
        }
    }

    fn positions(&self) -> Vec<Units<Sym>> {
        self.positions
            .iter()
            .map(|(symbol, &amount)| Units {
                symbol: symbol.clone(),
                amount,
            })
            .collect()
    }

    fn set_costs_for(&mut self, symbol: Sym, costs: TradingCosts) -> Result<(), WalletError> {
        self.per_symbol_costs.insert(symbol, costs);
        Ok(())
    }

    /// `true` — the paper account has no spot restriction: a sell credits cash,
    /// so a position may go as negative as the strategy asks. Stated explicitly
    /// (rather than left to the trait default) because this is the reference
    /// answer every backtest reads.
    fn can_short(&self) -> bool {
        true
    }

    /// Whatever [`with_quote_ccy`](PaperWallet::with_quote_ccy) was told, and
    /// `None` otherwise — simulated money has no venue to ask, so an unlabelled
    /// paper wallet genuinely does not know what unit it is counting in.
    /// The gross-exposure multiple this wallet enforces —
    /// [`max_gross`](PaperWallet::max_gross), the same for every symbol.
    ///
    /// Unlike [`quote_ccy`](Wallet::quote_ccy), which a paper wallet can only
    /// answer if it was told, this one it *knows*: the number is a rule it
    /// applies to every fill, not a label. `Some(1.0)` from a default wallet is
    /// a claim about behaviour — an unlevered book — and not the "does not say"
    /// the trait's `None` means.
    ///
    /// Which is the point of answering at all: a live account's leverage is set
    /// out of band and readable only from the venue, so the paper side has to
    /// be able to state its own for the two to be compared.
    fn leverage(&self, _symbol: &Sym) -> Option<Real> {
        Some(self.max_gross)
    }

    /// Take this bar's carry rate for `symbol`, if a
    /// [`CarryModel`](crate::costs::CarryModel) here asked for one.
    ///
    /// The wallet reads its own inputs: it looks up the column *its* cost
    /// bundle for this symbol named, so nothing above has to know that carry
    /// exists. Per-symbol cost overrides may therefore name different columns,
    /// and each is read against the atom for the symbol it applies to.
    ///
    /// An absent column, or an absent sample on a bar that has the column,
    /// leaves nothing recorded — and the difference between "no sample" and
    /// "zero" is preserved all the way to
    /// [`CarryContext::rate`](crate::costs::CarryContext::rate), because an
    /// accrual is one of the few places where they are not the same statement.
    fn observe(&mut self, symbol: &Sym, atom: &crate::types::Atom) {
        // Recorded before any early return below: the bar's time is what carry
        // is pro-rated over, and it is wanted whether or not this symbol's
        // model reads a rate column. First `Some` wins — every entry in a
        // snapshot shares an open time, since snapshots group by exact key.
        if self.pending_bar_time.is_none() {
            self.pending_bar_time = atom.time;
        }
        let costs = self.per_symbol_costs.get(symbol).unwrap_or(&self.costs);
        let Some(column) = costs.carry.column() else {
            return;
        };
        let Some(overlays) = atom.overlays.as_ref() else {
            return;
        };
        if let Some(crate::types::OverlayValue::Real(rate)) = overlays.get_by_key(column) {
            let rate = *rate;
            // A `Real` overlay slot has no `None`, so an **absent sample is
            // stored as a `NaN`** — a blank cell, or a full join that gave this
            // symbol a column another carries. That is precisely the case this
            // method's contract calls "leaves nothing recorded", and recording
            // it was catastrophic rather than merely wrong: `accrue_carry`
            // computes `position × price × rate` and subtracts it from `funds`,
            // so one blank funding cell turned the account's cash *and* every
            // equity reading after it into `NaN`, permanently — while
            // `carry_coverage` counted the bar as *seen* and reported full
            // coverage, which is the one diagnostic that exists to catch a
            // funding series with holes in it.
            if rate.is_finite() {
                self.carry_rates.insert(symbol.clone(), rate);
            }
        }
    }

    fn quote_ccy(&self) -> Option<&str> {
        self.quote_ccy.as_deref()
    }

    fn price(&self, symbol: &Sym) -> Option<Reference> {
        self.bars.get(symbol).map(|c| Reference(c.close))
    }

    fn equity(&self) -> Reference {
        self.marked_equity(|symbol| self.bars.get(symbol).map_or(0.0, |c| c.close))
    }

    fn update(&mut self, symbol: Sym, candle: Candle) -> Vec<Order<Sym>> {
        // The single-symbol special case of `advance`, and routed through it so
        // there is exactly one fill path. With one entry every phase below
        // degenerates to the straight-line sequence this used to be.
        self.advance(&[(symbol, candle)])
    }

    /// Fill a whole bar in phases, so the booked fills do not depend on the
    /// order `bars` happens to be in.
    ///
    /// Two things make the naive per-symbol loop order-dependent, and each gets
    /// a phase here:
    ///
    /// 1. **Marking is interleaved with pricing.** A fill priced while only
    ///    part of the bar is marked sizes its `value_frac` against an equity
    ///    built from *this* bar's close for the symbols already marked and the
    ///    *previous* bar's close for the rest. The first is lookahead — the
    ///    close is information from later in the bar than the `open` the fill
    ///    trades at. Phase 1 marks everything, and phase 2 computes one equity
    ///    from every symbol's `open`, which is what a fill at the open may see.
    ///
    /// 2. **Cash is shared, and buys are shrunk to fit it.** A rotation that
    ///    sells one holding to fund another is only affordable once the sale
    ///    has settled, so a buy priced first gets silently scaled down to
    ///    whatever residual cash was lying around (see the shrink helper
    ///    `fit_to_account`). Phases 3–6 settle
    ///    every cash-crediting fill before any cash-consuming one, so a
    ///    rotation is funded by its own proceeds no matter what order the
    ///    symbols arrive in.
    ///
    /// Within a phase, ties break by [`OrderId`] — submission order, which the
    /// strategy chose and a venue would honour — never by symbol or by
    /// position in `bars`. Cash contention *between* two buys is therefore
    /// resolved first-come-first-served rather than arbitrarily.
    fn advance(&mut self, bars: &[(Sym, Candle)]) -> Vec<Order<Sym>> {
        // Phase 1 — mark every symbol, before anything is priced.
        //
        // `get_mut`-then-`insert` rather than a bare `insert`: after the first
        // bar the key is already present, and `insert` would clone the symbol
        // every bar only to drop the clone again. For `Sym = Symbol` — what the
        // spec/CLI layer uses — that is one heap allocation per symbol per bar
        // for the whole run.
        for (symbol, candle) in bars {
            match self.bars.get_mut(symbol) {
                Some(slot) => *slot = *candle,
                None => {
                    self.bars.insert(symbol.clone(), *candle);
                }
            }
        }

        // Phase 1b — carry, on what was held *through* the interval that just
        // ended. Before the queued orders resolve, so this bar's sizing sees the
        // account the charge left behind rather than one that still holds cash
        // it has already spent on funding.
        self.accrue_carry(bars);
        self.carry_rates.clear();
        // Roll the interval forward. `take` rather than a plain copy so a bar
        // whose atoms carried no time leaves `last_bar_time` where it was
        // instead of silently re-charging the previous interval: the next
        // stamped bar then measures across the gap, which is the interval that
        // actually elapsed.
        if let Some(now) = self.pending_bar_time.take() {
            self.last_bar_time = Some(now);
        }

        // Phase 2 — resolve every queued market order against ONE equity, built
        // from this bar's opens.
        //
        // A market order queued last bar fills at this bar's `open`, so the
        // account it sizes against is the account as of the open: every symbol
        // this bar carries marked at its own `open`, and any other position the
        // wallet holds at the last close it was fed. Reading a close for a
        // symbol that *is* in this bar would size the fill off information from
        // later in the same bar.
        let equity_at_open = self
            .marked_equity(|s| match bars.iter().find(|(sym, _)| sym == s) {
                Some((_, candle)) => candle.open,
                None => self.bars.get(s).map_or(0.0, |c| c.close),
            })
            .0;
        // `(symbol, candle, target-or-sizing, id)`. The shrink is deliberately
        // NOT applied here — it reads live cash, so it has to happen at the
        // moment the fill is booked, after this bar's credits have landed.
        let mut queued: Vec<QueuedFill<Sym>> = Vec::new();
        for (symbol, candle) in bars {
            let Some(pending) = self.pending.remove(symbol) else {
                continue;
            };
            let position = self.positions.get(symbol).copied().unwrap_or(0.0);
            let (target, sizing, id) = match pending {
                Pending::Target(amount, id) => (amount, None, id),
                Pending::Sized(side, size, id) => {
                    let magnitude = size.resolve(candle.open, position, self.funds, equity_at_open);
                    (side.sign() * magnitude, Some((side, size)), id)
                }
            };
            queued.push(QueuedFill {
                symbol: symbol.clone(),
                candle: *candle,
                target,
                sizing,
                id,
                // Classified on the *unshrunk* target: `fit_to_account`
                // returns early once the delta is non-positive, so shrinking can
                // never turn a buy into a sell and the classification is stable.
                credits: target - position <= 0.0,
            });
        }
        queued.sort_by_key(|q| (!q.credits, q.id));

        let mut fills = Vec::new();
        // Phases 3 and 4 — market credits, then market debits.
        for q in queued {
            let position = self.positions.get(&q.symbol).copied().unwrap_or(0.0);
            // The rest of the book as of *this bar's opens* — the prices these
            // fills happen at. Reading a `close` for a symbol that also trades
            // this bar would let information from later in the bar decide how
            // big this fill is, which is the lookahead `equity_at_open` above
            // is built to avoid. Recomputed per fill because the fills ahead of
            // this one in the phase have already moved the book.
            let rest =
                self.rest_of_book(&q.symbol, |s| match bars.iter().find(|(sym, _)| sym == s) {
                    Some((_, candle)) => candle.open,
                    None => self.bars.get(s).map_or(0.0, |c| c.close),
                });
            // For a fractional sizing ("as much of my equity/funds as fits"),
            // fit the target to what the account can carry: a net buy to
            // available cash, either side to the leverage cap. Without the cash
            // half, an all-in `value_frac(1.0)` under any positive cost model
            // would size the notional to the entire equity, and paying
            // commission on top would fail the affordability check in `fill_at`
            // and silently drop the fill. An explicit `Size::Units(n)` or
            // `Size::PositionFraction(f)` carries a specific unit intent and is
            // left alone — an infeasible request is a caller error, not a
            // sizing target.
            let (target, bound) = match q.sizing {
                Some((side, Size::ValueFraction(_) | Size::FundsFraction(_))) => {
                    let fitted = self.fit_to_account(
                        &q.symbol,
                        side,
                        position,
                        q.target.abs(),
                        FillPricing {
                            bar: &q.candle,
                            price: q.candle.open,
                            kind: OrderKind::Market,
                        },
                        rest,
                    );
                    (side.sign() * fitted.magnitude, fitted.bound)
                }
                _ => (q.target, None),
            };
            match self.fill_at(
                q.symbol.clone(),
                target,
                q.candle.open,
                OrderKind::Market,
                q.id,
                FillContext::exact(q.target, rest),
            ) {
                Ok(Some(order)) => fills.push(order),
                // No fill. Ordinarily that means the position was already where
                // it was asked to be — but if the fit is what removed the trade,
                // the leg did not happen *as specified* and there is no
                // `requested_units` on any order to say so. Book the refusal.
                Ok(None) => {
                    if let Some(error) = bound
                        && (q.target - position).abs() > POSITION_EPSILON
                    {
                        self.push_rejection(&q.symbol, q.id, error, OrderKind::Market);
                    }
                }
                Err(error) => self.push_rejection(&q.symbol, q.id, error, OrderKind::Market),
            }
        }

        // Phase 5 — protective legs, triggers evaluated across the whole bar
        // before any is booked. Reduce-only, so the cash direction follows the
        // sign of the position it is closing: exiting a long credits, covering
        // a short debits.
        let mut triggered: Vec<(usize, Leg, Real, OrderKind, bool)> = Vec::new();
        for (i, (symbol, candle)) in bars.iter().enumerate() {
            if let Some((leg, fill, kind)) = self.protective_trigger(symbol, candle) {
                let credits = self.positions.get(symbol).copied().unwrap_or(0.0) > 0.0;
                triggered.push((i, leg, fill, kind, credits));
            }
        }
        triggered.sort_by_key(|(_, leg, _, _, credits)| (!*credits, leg.id));
        for (i, leg, fill, kind, _) in triggered {
            if let Some(order) = self.execute_protective(&bars[i].0, leg, fill, kind) {
                fills.push(order);
            }
        }

        // Phase 6 — limits last: a protective leg guards a position that already
        // exists, so letting a fresh entry fill ahead of the exit it was meant
        // to trigger would leave the strategy holding something it had asked to
        // be out of. Classified by the resting side, which is what decides the
        // cash direction in every case but a buy that reduces an existing long.
        let mut resting: Vec<(usize, RestingLimit, Real)> = Vec::new();
        for (i, (symbol, candle)) in bars.iter().enumerate() {
            if let Some((limit, fill)) = self.limit_trigger(symbol, candle) {
                resting.push((i, limit, fill));
            }
        }
        resting.sort_by_key(|(_, limit, _)| (limit.side == Side::Buy, limit.id));
        for (i, limit, fill) in resting {
            let (symbol, candle) = &bars[i];
            if let Some(order) = self.execute_limit(symbol, limit, fill, candle) {
                fills.push(order);
            }
        }

        // Phase 7 — the margin call, last, on the book as this bar leaves it.
        // Off unless `with_maintenance_margin` was set; see there for why the
        // trigger reads the bar's adverse extreme while the fill books at its
        // close.
        if self.breaches_maintenance(bars) {
            fills.extend(self.liquidate());
        }
        fills
    }

    fn set_position(&mut self, target: Units<Sym>) -> Result<Ack<Sym>, WalletError> {
        // Pre-flight against last close so an infeasible submission errors
        // synchronously (mirroring a live venue's rejection) rather than
        // queuing an order that fill_at will drop into the rejections log.
        let current = self.positions.get(&target.symbol).copied().unwrap_or(0.0);
        if let Err(e) = self.preflight_market(&target.symbol, current, target.amount) {
            return Err(self.reject_submission(&target.symbol, e));
        }
        let id = self.mint();
        self.pending
            .insert(target.symbol, Pending::Target(target.amount, id));
        Ok(Ack::Working(id))
    }

    fn set(&mut self, symbol: Sym, side: Side, size: Size) -> Result<Ack<Sym>, WalletError> {
        // Pre-flight what we can at submission: price validity always, and the
        // solvency checks for an explicit Size::Units target. Fractional
        // sizings (ValueFraction / FundsFraction) are always fitted to cash and
        // to the leverage cap at fill time, so they never fail a
        // submission-time solvency check and only need the price-validity
        // guards here.
        let close = match self.price(&symbol) {
            Some(p) => p.0,
            None => return Err(self.reject_submission(&symbol, WalletError::UnknownPrice)),
        };
        if close <= 0.0 {
            return Err(self.reject_submission(&symbol, WalletError::InvalidPrice));
        }
        if !size.is_finite() {
            return Err(self.reject_submission(&symbol, WalletError::InvalidQuantity));
        }
        if let Size::Units(units) = size {
            let current = self.positions.get(&symbol).copied().unwrap_or(0.0);
            let target = side.sign() * units.abs();
            if let Err(e) = self.check_solvency(&symbol, current, target, close) {
                return Err(self.reject_submission(&symbol, e));
            }
        }
        let id = self.mint();
        self.pending.insert(symbol, Pending::Sized(side, size, id));
        Ok(Ack::Working(id))
    }

    fn set_stop(
        &mut self,
        symbol: Sym,
        trigger: Reference,
        size: Size,
    ) -> Result<Ack<Sym>, WalletError> {
        // A non-finite trigger never compares true against a bar's range, so
        // the leg would rest forever without ever firing — a stop the caller
        // believes is protecting a position and is not.
        if !trigger.0.is_finite() || !size.is_finite() {
            return Err(self.reject_submission(&symbol, WalletError::InvalidQuantity));
        }
        let id = self.mint();
        self.protective.entry(symbol).or_default().stop = Some(Leg {
            trigger: trigger.0,
            size,
            id,
        });
        Ok(Ack::Working(id))
    }

    fn set_take_profit(
        &mut self,
        symbol: Sym,
        trigger: Reference,
        size: Size,
    ) -> Result<Ack<Sym>, WalletError> {
        // See `set_stop`: a leg that can never fire is worse than no leg.
        if !trigger.0.is_finite() || !size.is_finite() {
            return Err(self.reject_submission(&symbol, WalletError::InvalidQuantity));
        }
        let id = self.mint();
        self.protective.entry(symbol).or_default().take_profit = Some(Leg {
            trigger: trigger.0,
            size,
            id,
        });
        Ok(Ack::Working(id))
    }

    fn cancel_protective(&mut self, symbol: &Sym) -> Result<(), WalletError> {
        self.protective.remove(symbol);
        Ok(())
    }

    fn set_limit(
        &mut self,
        symbol: Sym,
        side: Side,
        size: Size,
        limit: Reference,
    ) -> Result<Ack<Sym>, WalletError> {
        if !limit.0.is_finite() || limit.0 <= 0.0 {
            return Err(WalletError::InvalidPrice);
        }
        if !size.is_finite() {
            return Err(self.reject_submission(&symbol, WalletError::InvalidQuantity));
        }
        let id = self.mint();
        self.limits.insert(
            symbol,
            RestingLimit {
                side,
                size,
                limit: limit.0,
                id,
            },
        );
        Ok(Ack::Working(id))
    }

    fn cancel_limit(&mut self, symbol: &Sym) -> Result<(), WalletError> {
        self.limits.remove(symbol);
        Ok(())
    }

    /// Close every open position **synchronously**, at each symbol's last known
    /// close, and return the fills.
    ///
    /// The trait default (queue a `close` per symbol, then drain `poll_fills`)
    /// is wrong here: a `PaperWallet`'s queued moves settle at the *next* bar's
    /// open and it reports no out-of-band fills, so the default would flatten
    /// nothing at all. This goes straight to `fill_at` — the same engine every
    /// other fill routes through, so spread, slippage and commission apply
    /// exactly as they would to a strategy-issued exit — and mints a real
    /// [`OrderId`] per leg, so `reconstruct_trades` pairs them like any other
    /// close.
    ///
    /// Terminal: every queued move, resting bracket and resting limit is
    /// dropped, since none of them can fill after this.
    fn flatten(&mut self) -> Vec<Order<Sym>> {
        // Sorted so a multi-symbol flatten books its legs in a stable order
        // regardless of `positions`' hash seed.
        let mut open: Vec<Sym> = self
            .positions
            .iter()
            .filter(|(_, amount)| amount.abs() > POSITION_EPSILON)
            .map(|(symbol, _)| symbol.clone())
            .collect();
        open.sort_by_key(|s| self.bars.get(s).map_or(0, |c| c.close.to_bits()));

        let mut fills = Vec::new();
        for symbol in open {
            let Some(price) = self.bars.get(&symbol).map(|c| c.close) else {
                // No price ever fed for this symbol: nothing to value the close
                // at, and inventing one would poison the final equity point.
                continue;
            };
            let id = self.mint();
            match self.fill_at(
                symbol.clone(),
                0.0,
                price,
                OrderKind::Market,
                id,
                FillContext::reducing(0.0),
            ) {
                Ok(Some(order)) => fills.push(order),
                Ok(None) => {}
                Err(e) => {
                    let _ = self.reject_submission(&symbol, e);
                }
            }
        }
        self.pending.clear();
        self.protective.clear();
        self.limits.clear();
        fills
    }

    fn take_rejections(&mut self) -> Vec<Rejection<Sym>> {
        // Yield the not-yet-drained tail and advance the cursor rather than
        // truncating — `rejections()` still reports the full run history.
        let fresh = self.rejections[self.rejections_drained..].to_vec();
        self.rejections_drained = self.rejections.len();
        fresh
    }

    // The paper wallet *is* the book, so unlike a live venue it has to
    // round-trip its own state. Both forward to the inherent pair below, which
    // predates the trait methods and stays the direct spelling.
    fn snapshot_state(&self) -> serde_json::Value
    where
        Sym: Serialize + DeserializeOwned,
    {
        PaperWallet::snapshot_state(self)
    }

    fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String>
    where
        Sym: Serialize + DeserializeOwned,
    {
        PaperWallet::restore_state(self, state)
    }

    /// Directly credit / debit the cash balance — the paper impl of the
    /// [`Wallet::adjust_funds`] hook. Returns
    /// [`WalletError::InsufficientFunds`] if the resulting balance would
    /// be negative, otherwise applies the delta atomically. Booked
    /// outside the blotter (no `Order`, no `on_fill`).
    fn adjust_funds(&mut self, delta: Real) -> Result<(), WalletError> {
        let new_funds = self.funds + delta;
        if new_funds < 0.0 {
            return Err(WalletError::InsufficientFunds);
        }
        self.funds = new_funds;
        Ok(())
    }

    /// Drop a working order by id — the paper impl of [`Wallet::cancel`]. A
    /// queued market order (one per symbol) is removed by matching its
    /// [`OrderId`]; a resting protective leg is cleared (and its bracket
    /// dropped when both legs are gone). An id the wallet no longer holds is a
    /// no-op `Ok(())`, per the trait contract.
    fn cancel(&mut self, id: OrderId) -> Result<(), WalletError> {
        // A queued market order carries its id in either `Pending` variant.
        let queued = self.pending.iter().find_map(|(sym, pending)| {
            let pid = match pending {
                Pending::Target(_, pid) | Pending::Sized(_, _, pid) => *pid,
            };
            (pid == id).then(|| sym.clone())
        });
        if let Some(sym) = queued {
            self.pending.remove(&sym);
            return Ok(());
        }
        // Otherwise clear a matching resting protective leg, then discard any
        // now-empty bracket.
        for prot in self.protective.values_mut() {
            if prot.stop.is_some_and(|l| l.id == id) {
                prot.stop = None;
            }
            if prot.take_profit.is_some_and(|l| l.id == id) {
                prot.take_profit = None;
            }
        }
        self.protective
            .retain(|_, prot| prot.stop.is_some() || prot.take_profit.is_some());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::types::Symbol;
    use std::collections::HashMap;

    use super::*;
    use crate::indicators::{BoolIndicatorExt, IndicatorExt, Sma};
    use crate::signal::Signal;
    use crate::strategy::Strategy;
    use crate::types::Candle;
    use crate::wallet::SleeveWallet;

    fn bar(close: Real) -> Candle {
        Candle::new(close, close, close, close, 0.0)
    }

    /// Assert an order's fields, ignoring its (wallet-minted) id.
    fn assert_fill(o: &Order<&str>, side: Side, units: Real, price: Real, kind: OrderKind) {
        assert_eq!(o.side, side, "side");
        assert!(
            (o.units - units).abs() < 1e-9,
            "units {} != {}",
            o.units,
            units
        );
        assert!(
            (o.price - price).abs() < 1e-9,
            "price {} != {}",
            o.price,
            price
        );
        assert_eq!(o.kind, kind, "kind");
    }

    /// An OHLC bar, for the limit tests — the flat `bar()` helper can't
    /// express "traded down to X and back".
    fn ohlc(open: Real, high: Real, low: Real, close: Real) -> Candle {
        Candle::new(open, high, low, close, 1_000.0)
    }

    /// Equity must not depend on the order legs were *inserted*.
    ///
    /// `positions` is a `HashMap`, so its iteration order is a function of the
    /// insertion history and the per-process hash seed. Summing floats in that
    /// order made a multi-symbol equity curve differ by a ULP between runs — see
    /// `PaperWallet::marked_equity`. Values here are deliberately chosen to be
    /// inexact in binary and to span several magnitudes, so a different addition
    /// order really does land on a different `f64`.
    /// Equity must sum the legs in **ascending value order**, not in `HashMap`
    /// order.
    ///
    /// `positions` is a `HashMap` with a per-process `RandomState`, so iterating
    /// it yields the legs in an order that varies *between runs of the same
    /// binary on the same data*. Floating addition is not associative, so a
    /// multi-symbol equity curve drifted by a ULP from one invocation to the
    /// next, and a ULP either side of a threshold is a different trade.
    ///
    /// That cross-process drift cannot be reproduced inside one test process —
    /// the seed is fixed for the run, and re-inserting the same keys in a
    /// different order gives the same bucket layout, so "insert forwards vs
    /// backwards" proves nothing. What *is* testable is the convention itself:
    /// pin equity to an independently-computed ascending-order fold, built from
    /// the public `funds` / `position` / `price` accessors. Without the sort in
    /// `marked_equity` this fails.
    /// Populate `positions` / `bars` directly with leg values spanning ~16
    /// decades, then check equity against an independent ascending fold.
    ///
    /// The wide magnitude spread is the point: summing ascending accumulates the
    /// tiny legs into something big enough to survive being added to the large
    /// ones, whereas any other order absorbs them one at a time. With `n` legs
    /// scrambled across that range, the probability that `HashMap` order happens
    /// to coincide with ascending order is negligible, so this fails whenever
    /// `marked_equity` stops sorting.
    ///
    /// Built by hand rather than by trading, so the values can be chosen to
    /// discriminate — a realistic book of same-magnitude legs sums identically
    /// in every order and would prove nothing.
    fn assert_equity_is_canonically_ordered(n: usize) {
        let mut w: PaperWallet<Symbol> = PaperWallet::new(0.0);
        for i in 0..n {
            // Scramble the exponent so neither insertion order nor symbol order
            // correlates with magnitude.
            let exp = ((i * 7 + 3) % 17) as i32 - 8; // -8 ..= 8
            let px = 10.0_f64.powi(exp);
            let units = 1.0 + (i as Real) * 0.5;
            let sym = crate::types::symbol(format!("S{i:03}"));
            w.positions.insert(sym.clone(), units);
            w.bars.insert(sym, bar(px));
        }

        let mut asc: Vec<Real> = w
            .positions
            .iter()
            .map(|(s, &a)| a * w.bars.get(s).map_or(0.0, |c| c.close))
            .collect();
        asc.sort_by(|a, b| a.total_cmp(b));
        let want = asc.iter().fold(0.0 as Real, |acc, v| acc + v);

        // Guard: if descending gives the same bits, the fixture is not
        // discriminating and the assertion below would be vacuous.
        let desc = asc.iter().rev().fold(0.0 as Real, |acc, v| acc + v);
        assert_ne!(
            want.to_bits(),
            desc.to_bits(),
            "n = {n}: fixture does not discriminate between summation orders",
        );

        let got = w.equity().0;
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "n = {n}: equity {got:?} is not the ascending-order fold {want:?} \
             — `marked_equity` must sum in a canonical order",
        );
    }

    /// Swept across the `INLINE` spill boundary (32): both the stack and heap
    /// paths must hold the convention, or crossing the threshold would quietly
    /// reintroduce the drift on large universes.
    ///
    /// Several sizes because a single one can coincide with ascending order by
    /// luck — at n = 12 it does. Verified to fail when the sort in
    /// `marked_equity` is removed.
    #[test]
    fn equity_sums_legs_in_canonical_order() {
        for n in [12usize, 31, 32, 33, 64] {
            assert_equity_is_canonically_ordered(n);
        }
    }

    #[test]
    fn a_buy_limit_rests_until_the_bar_trades_down_to_it() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        assert!(matches!(
            w.set_limit("X", Side::Buy, Size::units(5.0), Reference(98.0)),
            Ok(Ack::Working(_))
        ));

        // Bar never reaches 98 — nothing fills, the order still rests.
        let fills = w.update("X", ohlc(100.0, 102.0, 98.5, 101.0));
        assert!(fills.is_empty(), "limit must not fill above its price");
        assert_eq!(w.position(&"X").amount, 0.0);

        // Bar trades through it.
        let fills = w.update("X", ohlc(100.0, 101.0, 97.0, 99.0));
        assert_eq!(fills.len(), 1);
        assert_fill(&fills[0], Side::Buy, 5.0, 98.0, OrderKind::Limit);
        assert_eq!(w.position(&"X").amount, 5.0);
    }

    #[test]
    fn a_sell_limit_mirrors_on_the_high() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_limit("X", Side::Sell, Size::units(3.0), Reference(105.0))
            .unwrap();

        assert!(w.update("X", ohlc(100.0, 104.0, 99.0, 103.0)).is_empty());
        let fills = w.update("X", ohlc(104.0, 106.0, 103.0, 105.0));
        assert_eq!(fills.len(), 1);
        assert_fill(&fills[0], Side::Sell, 3.0, 105.0, OrderKind::Limit);
        assert_eq!(w.position(&"X").amount, -3.0);
    }

    #[test]
    fn a_gap_through_the_limit_fills_at_the_better_open() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_limit("X", Side::Buy, Size::units(5.0), Reference(98.0))
            .unwrap();
        // Opens at 95 — below the limit. The caller asked for "98 or better"
        // and the market opened better, so they get 95, not 98.
        let fills = w.update("X", ohlc(95.0, 96.0, 94.0, 95.5));
        assert_fill(&fills[0], Side::Buy, 5.0, 95.0, OrderKind::Limit);
    }

    #[test]
    fn a_limit_never_fills_worse_than_its_price_under_costs() {
        // The invariant that makes a limit a limit. An aggressive spread +
        // slippage bundle would push a market buy well above 98; a passive fill
        // crosses no spread and takes no impact, so it prices exactly at the
        // limit.
        use crate::costs::{FixedBpsSlippage, FixedBpsSpread, NoCommission};
        let costs = TradingCosts::new(
            Box::new(NoCommission),
            Box::new(FixedBpsSpread::new(50.0)),
            Box::new(FixedBpsSlippage::new(100.0)),
        );
        let mut w: PaperWallet<&str> = PaperWallet::with_costs(10_000.0, costs);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_limit("X", Side::Buy, Size::units(5.0), Reference(98.0))
            .unwrap();
        let fills = w.update("X", ohlc(100.0, 101.0, 97.0, 99.0));
        assert_eq!(fills.len(), 1);
        assert!(
            fills[0].price <= 98.0 + 1e-9,
            "limit buy filled at {}, worse than its 98.0 limit",
            fills[0].price
        );
        assert_fill(&fills[0], Side::Buy, 5.0, 98.0, OrderKind::Limit);
    }

    #[test]
    fn a_limit_is_latest_wins_and_cancellable() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_limit("X", Side::Buy, Size::units(5.0), Reference(98.0))
            .unwrap();
        // Re-submitting replaces rather than stacking — the convention the
        // protective legs use, so a strategy can walk the price each bar.
        w.set_limit("X", Side::Buy, Size::units(2.0), Reference(96.0))
            .unwrap();
        let fills = w.update("X", ohlc(100.0, 101.0, 95.0, 97.0));
        assert_eq!(fills.len(), 1, "only the latest order rests");
        assert_fill(&fills[0], Side::Buy, 2.0, 96.0, OrderKind::Limit);

        w.set_limit("X", Side::Buy, Size::units(1.0), Reference(90.0))
            .unwrap();
        w.cancel_limit(&"X").unwrap();
        assert!(w.update("X", ohlc(95.0, 96.0, 85.0, 90.0)).is_empty());
    }

    #[test]
    fn a_limit_sizes_against_equity_at_the_fill_not_at_submission() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_limit("X", Side::Buy, Size::value_frac(1.0), Reference(50.0))
            .unwrap();
        let fills = w.update("X", ohlc(100.0, 101.0, 40.0, 45.0));
        // All-in at 50 with 1000 of equity — 20 units, not the 10 that sizing
        // at the submission price of 100 would have produced.
        assert_fill(&fills[0], Side::Buy, 20.0, 50.0, OrderKind::Limit);
    }

    #[test]
    fn a_protective_exit_fills_before_a_limit_entry_on_the_same_bar() {
        // Both trigger on one bar. The stop guards a position that already
        // exists; filling the entry first would leave the strategy holding
        // something it had asked to be out of.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_position(Units {
            symbol: "X",
            amount: 10.0,
        })
        .unwrap();
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_stop("X", Reference(95.0), Size::position_frac(1.0))
            .unwrap();
        w.set_limit("X", Side::Buy, Size::units(4.0), Reference(94.0))
            .unwrap();

        let fills = w.update("X", ohlc(99.0, 99.5, 93.0, 94.0));
        assert_eq!(fills.len(), 2);
        assert_eq!(fills[0].kind, OrderKind::Stop, "stop books first");
        assert_eq!(fills[1].kind, OrderKind::Limit);
        // Flattened by the stop, then re-entered 4 long by the limit.
        assert_eq!(w.position(&"X").amount, 4.0);
    }

    #[test]
    fn an_unaffordable_limit_is_rejected_not_silently_retried() {
        let mut w: PaperWallet<&str> = PaperWallet::new(100.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_limit("X", Side::Buy, Size::units(1_000.0), Reference(98.0))
            .unwrap();
        let fills = w.update("X", ohlc(100.0, 101.0, 97.0, 99.0));
        assert!(fills.is_empty());
        let rejections = w.take_rejections();
        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].error, WalletError::InsufficientFunds);
        assert_eq!(rejections[0].kind, OrderKind::Limit);
        // Consumed, not left resting at a price the market has moved past.
        assert!(w.update("X", ohlc(97.0, 98.0, 96.0, 97.0)).is_empty());
    }

    #[test]
    fn set_limit_defaults_to_unsupported_for_a_wallet_that_opts_out() {
        // The trait default, so a downstream wallet whose venue has no resting
        // limit keeps compiling and says so rather than silently doing nothing.
        struct Minimal;
        impl Wallet<&'static str> for Minimal {
            fn funds(&self) -> Reference {
                Reference(0.0)
            }
            fn position(&self, _s: &&'static str) -> Units<&'static str> {
                Units {
                    symbol: "X",
                    amount: 0.0,
                }
            }
            fn price(&self, _s: &&'static str) -> Option<Reference> {
                None
            }
            fn equity(&self) -> Reference {
                Reference(0.0)
            }
            fn update(&mut self, _s: &'static str, _c: Candle) -> Vec<Order<&'static str>> {
                Vec::new()
            }
            fn set_position(
                &mut self,
                _t: Units<&'static str>,
            ) -> Result<Ack<&'static str>, WalletError> {
                Err(WalletError::UnsupportedOperation)
            }
            fn set_stop(
                &mut self,
                _s: &'static str,
                _t: Reference,
                _size: Size,
            ) -> Result<Ack<&'static str>, WalletError> {
                Err(WalletError::UnsupportedOperation)
            }
            fn set_take_profit(
                &mut self,
                _s: &'static str,
                _t: Reference,
                _size: Size,
            ) -> Result<Ack<&'static str>, WalletError> {
                Err(WalletError::UnsupportedOperation)
            }
            fn cancel_protective(&mut self, _s: &&'static str) -> Result<(), WalletError> {
                Ok(())
            }
        }
        let mut w = Minimal;
        assert_eq!(
            w.set_limit("X", Side::Buy, Size::units(1.0), Reference(10.0)),
            Err(WalletError::UnsupportedOperation)
        );
        assert!(w.cancel_limit(&"X").is_ok());
        // Same default-shape question for the capability read: signed positions
        // are the trait's model, so an impl that says nothing can short.
        assert!(w.can_short());
        // The denomination default goes the other way, and deliberately: there
        // is no numeraire it would be safe to assume, so silence reads as "does
        // not say" rather than as a guess at dollars.
        assert_eq!(w.quote_ccy(), None);
        // And the third capability read defaults the same direction as the
        // denomination: silence is "does not say", not "nothing quotes this".
        assert!(w.data_sources().is_empty());
    }

    #[test]
    fn can_short_is_introspected_and_a_sleeve_delegates_it() {
        // The paper account is the permissive reference: a sell credits cash.
        let paper: PaperWallet<&str> = PaperWallet::new(1_000.0);
        assert!(paper.can_short());

        // A spot-shaped wallet — the shape `CoinbaseWallet` has — reports the
        // limit up front instead of leaving it to be discovered one clamped
        // order at a time.
        struct SpotOnly(PaperWallet<&'static str>);
        impl Wallet<&'static str> for SpotOnly {
            fn can_short(&self) -> bool {
                false
            }
            fn funds(&self) -> Reference {
                self.0.funds()
            }
            fn position(&self, s: &&'static str) -> Units<&'static str> {
                self.0.position(s)
            }
            fn positions(&self) -> Vec<Units<&'static str>> {
                self.0.positions()
            }
            fn price(&self, s: &&'static str) -> Option<Reference> {
                self.0.price(s)
            }
            fn equity(&self) -> Reference {
                self.0.equity()
            }
            fn update(&mut self, s: &'static str, c: Candle) -> Vec<Order<&'static str>> {
                self.0.update(s, c)
            }
            fn set_position(
                &mut self,
                t: Units<&'static str>,
            ) -> Result<Ack<&'static str>, WalletError> {
                // Spot: clamp a short to flat, as a real spot venue must.
                self.0.set_position(Units {
                    symbol: t.symbol,
                    amount: t.amount.max(0.0),
                })
            }
            fn set_stop(
                &mut self,
                s: &'static str,
                t: Reference,
                size: Size,
            ) -> Result<Ack<&'static str>, WalletError> {
                self.0.set_stop(s, t, size)
            }
            fn set_take_profit(
                &mut self,
                s: &'static str,
                t: Reference,
                size: Size,
            ) -> Result<Ack<&'static str>, WalletError> {
                self.0.set_take_profit(s, t, size)
            }
            fn cancel_protective(&mut self, s: &&'static str) -> Result<(), WalletError> {
                self.0.cancel_protective(s)
            }
        }

        let spot = SpotOnly(PaperWallet::new(1_000.0));
        assert!(!spot.can_short());

        // A sleeve is a view, not an account: it answers for what it wraps,
        // either way.
        let over_spot = SleeveWallet::new(spot, HashMap::new());
        assert!(!over_spot.can_short());
        let over_paper: SleeveWallet<&str, PaperWallet<&str>> =
            SleeveWallet::new(PaperWallet::new(1_000.0), HashMap::new());
        assert!(over_paper.can_short());
    }

    #[test]
    fn quote_ccy_is_unlabelled_until_told_and_a_sleeve_delegates_it() {
        // Simulated money has no venue to ask, so the paper wallet says nothing
        // rather than assuming. `None` is "unlabelled", not "no currency" — the
        // funds below are in *some* unit either way.
        let plain: PaperWallet<&str> = PaperWallet::new(1_000.0);
        assert_eq!(plain.quote_ccy(), None);
        assert_eq!(plain.funds().0, 1_000.0);

        // Labelling is descriptive only: same funds, same fills, one more fact.
        let eur: PaperWallet<&str> = PaperWallet::new(1_000.0).with_quote_ccy("EUR");
        assert_eq!(eur.quote_ccy(), Some("EUR"));
        assert_eq!(eur.funds().0, 1_000.0);

        // A sleeve carves a share out of one account's cash; it does not
        // redenominate it. Both directions, as for `can_short`.
        let over_eur = SleeveWallet::new(eur, HashMap::new());
        assert_eq!(over_eur.quote_ccy(), Some("EUR"));
        let over_plain: SleeveWallet<&str, PaperWallet<&str>> =
            SleeveWallet::new(PaperWallet::new(1_000.0), HashMap::new());
        assert_eq!(over_plain.quote_ccy(), None);
    }

    #[test]
    fn data_sources_are_unstated_until_a_venue_answers_and_a_sleeve_delegates_them() {
        // A paper account has no venue whose prices are the *right* ones — it is
        // fed by whoever ran it — so it names none rather than guessing. Empty
        // is "does not say", the same reading `quote_ccy`'s `None` asks for.
        let paper: PaperWallet<&str> = PaperWallet::new(1_000.0);
        assert!(paper.data_sources().is_empty());

        // A venue-backed wallet — the shape the live wallets have — names the
        // provider that quotes what it trades, so a caller can preflight the
        // pairing instead of discovering a mismatched feed from the fills.
        struct Venue(PaperWallet<&'static str>);
        impl Wallet<&'static str> for Venue {
            fn data_sources(&self) -> &'static [&'static str] {
                &["coinbase"]
            }
            fn funds(&self) -> Reference {
                self.0.funds()
            }
            fn position(&self, s: &&'static str) -> Units<&'static str> {
                self.0.position(s)
            }
            fn price(&self, s: &&'static str) -> Option<Reference> {
                self.0.price(s)
            }
            fn equity(&self) -> Reference {
                self.0.equity()
            }
            fn update(&mut self, s: &'static str, c: Candle) -> Vec<Order<&'static str>> {
                self.0.update(s, c)
            }
            fn set_position(
                &mut self,
                t: Units<&'static str>,
            ) -> Result<Ack<&'static str>, WalletError> {
                self.0.set_position(t)
            }
            fn set_stop(
                &mut self,
                s: &'static str,
                t: Reference,
                size: Size,
            ) -> Result<Ack<&'static str>, WalletError> {
                self.0.set_stop(s, t, size)
            }
            fn set_take_profit(
                &mut self,
                s: &'static str,
                t: Reference,
                size: Size,
            ) -> Result<Ack<&'static str>, WalletError> {
                self.0.set_take_profit(s, t, size)
            }
            fn cancel_protective(&mut self, s: &&'static str) -> Result<(), WalletError> {
                self.0.cancel_protective(s)
            }
        }

        // A sleeve is a view, not an account: it does not move what it wraps to
        // another venue. Both directions, as for `can_short` and `quote_ccy`.
        let over_venue = SleeveWallet::new(Venue(PaperWallet::new(1_000.0)), HashMap::new());
        assert_eq!(over_venue.data_sources(), &["coinbase"]);
        let over_paper: SleeveWallet<&str, PaperWallet<&str>> =
            SleeveWallet::new(PaperWallet::new(1_000.0), HashMap::new());
        assert!(over_paper.data_sources().is_empty());
    }

    #[test]
    fn set_position_queues_and_fills_at_next_open() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(100.0));
        // A market order only queues (Ack::Working) — nothing is booked yet.
        assert!(matches!(
            w.set_position(Units {
                symbol: "X",
                amount: 3.0
            }),
            Ok(Ack::Working(_))
        ));
        assert_eq!(w.position(&"X").amount, 0.0);
        assert!(w.orders().is_empty());
        // The next bar fills it at that bar's open, returning it in the fill stream.
        let fills = w.update("X", bar(100.0));
        assert_eq!(w.position(&"X").amount, 3.0);
        assert_eq!(w.funds().0, 1_000.0 - 3.0 * 100.0);
        assert_fill(&fills[0], Side::Buy, 3.0, 100.0, OrderKind::Market);
        assert_fill(
            w.orders().last().unwrap(),
            Side::Buy,
            3.0,
            100.0,
            OrderKind::Market,
        );
        // Setting a larger target buys the difference (scale in), again next open.
        w.set_position(Units {
            symbol: "X",
            amount: 5.0,
        })
        .unwrap();
        w.update("X", bar(100.0));
        assert_eq!(w.position(&"X").amount, 5.0);
        assert_eq!(w.funds().0, 1_000.0 - 5.0 * 100.0);
    }

    #[test]
    fn set_targets_absolute_and_reverses() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(50.0));
        w.set("X", Side::Buy, Size::units(4.0)).unwrap();
        w.update("X", bar(50.0)); // fills the +4 at the open
        assert_eq!(w.position(&"X").amount, 4.0);
        // Re-targeting the same side is idempotent: the queued fill is a no-op.
        let before = w.orders().len();
        w.set("X", Side::Buy, Size::units(4.0)).unwrap();
        w.update("X", bar(50.0));
        assert_eq!(w.orders().len(), before);
        // Opposite side reverses: +4 -> -4 is a sell of 8.
        w.set("X", Side::Sell, Size::units(4.0)).unwrap();
        w.update("X", bar(50.0));
        assert_fill(
            w.orders().last().unwrap(),
            Side::Sell,
            8.0,
            50.0,
            OrderKind::Market,
        );
        assert_eq!(w.position(&"X").amount, -4.0);
    }

    #[test]
    fn close_flattens() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(10.0)).unwrap();
        w.update("X", bar(100.0)); // fill the buy at 100
        w.update("X", bar(110.0)); // mark to 110
        assert!(matches!(w.close("X"), Ok(Ack::Working(_)))); // queued
        w.update("X", bar(110.0)); // fills the close at the open 110
        assert_fill(
            w.orders().last().unwrap(),
            Side::Sell,
            10.0,
            110.0,
            OrderKind::Market,
        );
        assert!(w.positions().is_empty());
        assert_eq!(w.funds().0, 1_100.0);
    }

    #[test]
    fn relative_sizing_resolves_against_funds_and_position() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(25.0));
        // 10% of 1000 = 100 / price 25 = 4 units, resolved at the fill (open 25).
        w.set("X", Side::Buy, Size::funds_frac(0.1)).unwrap();
        w.update("X", bar(25.0));
        assert_fill(
            w.orders().last().unwrap(),
            Side::Buy,
            4.0,
            25.0,
            OrderKind::Market,
        );
        // Set to 50% of the 4-unit position -> sell 2.
        w.set("X", Side::Buy, Size::position_frac(0.5)).unwrap();
        w.update("X", bar(25.0));
        assert_fill(
            w.orders().last().unwrap(),
            Side::Sell,
            2.0,
            25.0,
            OrderKind::Market,
        );
        assert_eq!(w.position(&"X").amount, 2.0);
    }

    #[test]
    fn value_fraction_sizes_against_equity_and_flips_all_in() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(100.0));
        // All-in long: 100% of equity (== funds when flat) / 100 = 10 units.
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        w.update("X", bar(100.0));
        assert_eq!(w.position(&"X").amount, 10.0);
        assert!(w.funds().0.abs() <= 1e-6);
        // Equity is still 1000; flip all-in short -> -10 units (a sell of 20).
        w.set("X", Side::Sell, Size::value_frac(1.0)).unwrap();
        w.update("X", bar(100.0));
        assert_fill(
            w.orders().last().unwrap(),
            Side::Sell,
            20.0,
            100.0,
            OrderKind::Market,
        );
        assert_eq!(w.position(&"X").amount, -10.0);
    }

    #[test]
    fn value_fraction_reversal_sizes_against_open_not_close() {
        // Regression: on a reversal the sizing must mark the existing position at
        // the fill (open) price, not this bar's close — otherwise a bar whose
        // open ≠ close leaks close information into the size.
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        w.update("X", bar(100.0)); // long 10 @ 100; funds 0
        // Reverse all-in on a bar with open 95 (fill price) and close 105.
        // Equity-at-open = 0 + 10*95 = 950, magnitude = 950/95 = 10 -> target -10,
        // delta = -20. Using close (105) would give ~21.05 units sold — the bug.
        w.set("X", Side::Sell, Size::value_frac(1.0)).unwrap();
        w.update("X", Candle::new(95.0, 106.0, 94.0, 105.0, 0.0));
        assert_fill(
            w.orders().last().unwrap(),
            Side::Sell,
            20.0,
            95.0,
            OrderKind::Market,
        );
        assert_eq!(w.position(&"X").amount, -10.0);
    }

    #[test]
    fn fill_time_rejection_is_recorded_on_a_gap_up() {
        // A submission that clears the last-close pre-flight can still be
        // dropped at fill time if the next open gaps meaningfully higher.
        // The wallet stashes the drop in `rejections()` so a driver can
        // report why a bar produced no fill.
        let mut w: PaperWallet<&str> = PaperWallet::new(100.0);
        w.update("X", bar(50.0));
        // 1 unit @ close 50 = 50, comfortably within 100 funds — pre-flight passes.
        let ack = w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        let id = match ack {
            Ack::Working(id) => id,
            Ack::Filled(_) => panic!("market order should queue, not fill"),
        };
        // But the bar gaps up: open 200 > 100 funds. fill_at rejects.
        let fills = w.update("X", Candle::new(200.0, 210.0, 195.0, 205.0, 0.0));
        assert!(fills.is_empty(), "expected no fill");
        assert!(w.positions().is_empty());
        assert_eq!(w.rejections().len(), 1);
        assert_eq!(w.rejections()[0].symbol, "X");
        assert_eq!(w.rejections()[0].id, id);
        assert_eq!(w.rejections()[0].error, WalletError::InsufficientFunds);
    }

    #[test]
    fn drain_yields_each_rejection_once_without_disturbing_the_accessor() {
        let mut w: PaperWallet<&str> = PaperWallet::new(100.0);
        w.update("X", bar(50.0));
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        w.update("X", Candle::new(200.0, 210.0, 195.0, 205.0, 0.0));

        // The drain is the driver-facing stream: each entry exactly once.
        let drained = w.take_rejections();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].error, WalletError::InsufficientFunds);
        assert_eq!(drained[0].kind, OrderKind::Market);
        assert!(w.take_rejections().is_empty(), "already yielded");

        // ...but the non-destructive accessor still reports the full history.
        assert_eq!(w.rejections().len(), 1, "drain must not truncate history");
    }

    #[test]
    fn a_triggered_stop_that_cannot_be_booked_is_reported() {
        // The protective-leg drop site: a stop triggers, but booking the exit
        // fails. Before this was reported, the strategy was left holding a
        // position it believed was protected, with nothing said.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Sell, Size::units(50.0)).unwrap();
        w.update("X", bar(100.0));
        assert_eq!(w.position(&"X").amount, -50.0, "short 50");

        // Rest a stop above, then gap through it to a price that makes buying
        // back the short unaffordable.
        w.set_stop("X", Reference(120.0), Size::position_frac(1.0))
            .unwrap();
        let fills = w.update("X", Candle::new(900.0, 950.0, 890.0, 940.0, 0.0));

        assert!(fills.is_empty(), "the stop could not be booked");
        assert_eq!(w.position(&"X").amount, -50.0, "still exposed");
        let drained = w.take_rejections();
        assert_eq!(drained.len(), 1, "the refusal must be surfaced");
        assert_eq!(drained[0].kind, OrderKind::Stop, "reported as a stop");
        assert_eq!(drained[0].error, WalletError::InsufficientFunds);
    }

    #[test]
    fn value_fraction_all_in_shrinks_to_fit_under_costs() {
        // Regression: `value_frac(1.0)` under any positive cost model used to
        // silently produce zero fills — the resolved size was `equity/open`,
        // but paying commission on top drove `cost > funds` and the fill was
        // rejected. The wallet now shrinks the resolved magnitude so the fill
        // clears the affordability check.
        use crate::costs::{FixedBpsSpread, NoSlippage, PercentageCommission};
        let costs = TradingCosts::new(
            Box::new(PercentageCommission::new(0.001)), // 10 bps
            Box::new(FixedBpsSpread::new(10.0)),        // 10 bps round-trip
            Box::new(NoSlippage),
        );
        let mut w: PaperWallet<&str> = PaperWallet::with_costs(1_000.0, costs);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        let fills = w.update("X", bar(100.0));
        // A fill happens (was zero before the fix) and cash never goes negative.
        assert_eq!(fills.len(), 1, "expected one fill, got {}", fills.len());
        assert!(w.position(&"X").amount > 0.0);
        assert!(w.funds().0 >= -1e-6, "funds went negative: {}", w.funds().0);
        // The resulting notional is just under equity (deducted spread +
        // commission), not equal to it.
        let fill = &fills[0];
        assert!(
            fill.units < 10.0,
            "units {} should be shrunk below 10.0",
            fill.units
        );
        assert!(
            fill.units > 9.9,
            "units {} shrunk too aggressively",
            fill.units
        );
    }

    #[test]
    fn equity_marks_positions_to_fed_prices() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(4.0)).unwrap();
        w.update("X", bar(100.0)); // fill: funds 600, +4 units
        w.update("X", bar(120.0));
        assert_eq!(w.equity().0, 600.0 + 4.0 * 120.0);
    }

    #[test]
    fn unknown_price_is_flagged_at_submission_and_at_fill() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        // "X" was never fed a bar. fill_at flags it directly...
        assert_eq!(
            w.fill_at(
                "X",
                1.0,
                50.0,
                OrderKind::Market,
                OrderId(0),
                FillContext::exact(1.0, RestOfBook::default())
            ),
            Err(WalletError::UnknownPrice)
        );
        // ...and the submission-time pre-flight refuses to queue an order
        // that can never be priced, so the caller learns synchronously
        // instead of via the rejections log.
        assert_eq!(
            w.set_position(Units {
                symbol: "X",
                amount: 1.0
            }),
            Err(WalletError::UnknownPrice)
        );
        assert_eq!(
            w.set("X", Side::Buy, Size::units(1.0)),
            Err(WalletError::UnknownPrice)
        );
    }

    #[test]
    fn insufficient_funds_is_flagged_at_submission_and_a_short_is_bounded_too() {
        let mut w: PaperWallet<&str> = PaperWallet::new(100.0);
        w.update("X", bar(50.0));
        // 3 units cost 150 > 100 funds, and there is no margin. fill_at
        // flags it directly...
        assert_eq!(
            w.fill_at(
                "X",
                3.0,
                50.0,
                OrderKind::Market,
                OrderId(0),
                FillContext::exact(3.0, RestOfBook::default())
            ),
            Err(WalletError::InsufficientFunds)
        );
        // ...and set/set_position pre-flight against last close so a caller
        // learns synchronously instead of waiting for the fill-time rejection.
        assert_eq!(
            w.set("X", Side::Buy, Size::units(3.0)),
            Err(WalletError::InsufficientFunds)
        );
        assert_eq!(
            w.set_position(Units {
                symbol: "X",
                amount: 3.0
            }),
            Err(WalletError::InsufficientFunds)
        );
        // A short sale credits cash, so the *cash* rule can never bound it —
        // which is exactly why the leverage rule has to. 3 units short is 150
        // of gross against 100 of equity: 1.5x on an unlevered wallet.
        assert_eq!(
            w.set("X", Side::Sell, Size::units(3.0)),
            Err(WalletError::ExceedsMaxGross)
        );
        assert_eq!(
            w.set_position(Units {
                symbol: "X",
                amount: -3.0
            }),
            Err(WalletError::ExceedsMaxGross)
        );
        // 2 units short is 100 of gross against 100 of equity — exactly 1x, and
        // the mirror image of the 2-unit long the cash rule allows.
        w.set("X", Side::Sell, Size::units(2.0)).unwrap();
        w.update("X", bar(50.0));
        assert_eq!(w.position(&"X").amount, -2.0);
        assert_eq!(w.funds().0, 200.0);
        assert_eq!(w.equity().0, 100.0);
    }

    /// The bug this cap exists for: one `sizing` value meaning two different
    /// exposures depending only on which side the document took.
    ///
    /// `value_frac(3.0)` used to be shrunk to 1x on the long side (the buy ran
    /// out of cash) and honoured in full on the short (a sale *credits* cash,
    /// so nothing bounded it). A long/short document therefore reported a
    /// number describing neither leg. Both sides now land on the same 1x, and
    /// both record that 3x was what was asked for.
    #[test]
    fn a_short_is_bounded_by_the_same_gross_limit_as_a_long() {
        let mut sides = Vec::new();
        for side in [Side::Buy, Side::Sell] {
            let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
            w.update("X", bar(100.0));
            w.set("X", side, Size::value_frac(3.0)).unwrap();
            let fills = w.update("X", bar(100.0));

            assert_eq!(fills.len(), 1);
            // 100 units at 100 is 10,000 of gross against 10,000 of equity.
            assert!((fills[0].units - 100.0).abs() < 1e-9, "{:?}", fills[0]);
            // ...and the blotter says 300 was the ask, so the gap is findable.
            assert!(
                (fills[0].requested_units - 300.0).abs() < 1e-9,
                "{:?}",
                fills[0]
            );
            assert!((fills[0].fill_ratio() - 1.0 / 3.0).abs() < 1e-9);
            assert!(
                w.take_rejections().is_empty(),
                "a fitted fill is not a refusal"
            );

            let position = w.position(&"X").amount;
            sides.push(position.abs() * 100.0 / w.equity().0);
            assert!((position.abs() - 100.0).abs() < 1e-9);
        }
        assert!(
            (sides[0] - sides[1]).abs() < 1e-9,
            "long took {:.2}x and short took {:.2}x under one spec value",
            sides[0],
            sides[1]
        );
        assert!(
            (sides[0] - 1.0).abs() < 1e-9,
            "expected 1x, got {:.2}x",
            sides[0]
        );
    }

    /// And the knob lifts both sides by the same multiple — which is what makes
    /// a 3x live account comparable to a backtest at all.
    #[test]
    fn max_gross_lifts_both_sides_together() {
        for side in [Side::Buy, Side::Sell] {
            let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0).with_max_gross(3.0);
            w.update("X", bar(100.0));
            w.set("X", side, Size::value_frac(3.0)).unwrap();
            let fills = w.update("X", bar(100.0));

            assert_eq!(fills.len(), 1);
            assert!((fills[0].units - 300.0).abs() < 1e-9, "{:?}", fills[0]);
            // Nothing was fitted this time, so the two agree.
            assert!((fills[0].fill_ratio() - 1.0).abs() < 1e-12);
            assert!((w.position(&"X").amount.abs() - 300.0).abs() < 1e-9);
            // Equity is untouched by the trade itself; gross is 3x it. On the
            // long side that means cash went negative — the account borrowed,
            // which is what leverage is.
            assert!((w.equity().0 - 10_000.0).abs() < 1e-9);
            let expect_funds = if side == Side::Buy {
                -20_000.0
            } else {
                40_000.0
            };
            assert!((w.funds().0 - expect_funds).abs() < 1e-9, "{}", w.funds().0);
        }
    }

    /// An account can drift over its limit on a mark, and must always be able
    /// to trade its way back. Every fill that lowers exposure is exempt.
    #[test]
    fn the_cap_never_blocks_a_way_out() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Sell, Size::value_frac(1.0)).unwrap();
        w.update("X", bar(100.0));
        assert!((w.position(&"X").amount + 100.0).abs() < 1e-9);

        // The short doubles against the account: 20,000 of gross on 0 of equity.
        w.update("X", bar(200.0));
        assert!(w.position(&"X").amount.abs() * 200.0 > w.equity().0);

        // Adding to it is refused (`set` targets an absolute position, so 110
        // short is the 100 it holds plus 10 more)...
        assert_eq!(
            w.set("X", Side::Sell, Size::units(110.0)),
            Err(WalletError::ExceedsMaxGross)
        );
        // ...and so is holding it at the same size but *further* out, while
        // trimming it is allowed even though the book stays over the line.
        w.set("X", Side::Sell, Size::units(90.0)).unwrap();
        // ...but covering it is not, at any size, including all of it. Drain
        // the deliberate refusal above first, so what is asserted is that
        // `flatten` books cleanly rather than that nothing was ever refused.
        let _ = w.take_rejections();
        let fills = w.flatten();
        assert_eq!(fills.len(), 1);
        assert_eq!(w.position(&"X").amount, 0.0);
        assert!(w.take_rejections().is_empty(), "flatten was refused");
    }

    /// A sizing that lands **exactly** on the ceiling must fill whole — not
    /// shaved by a ULP, and not refused for being one over.
    ///
    /// The two sides of the inequality are computed differently (`fraction *
    /// equity / price` on the way in, `|target| * fill_price` on the way out),
    /// so they disagree in the last bits, and this project has shipped four
    /// bugs where a float difference either side of a threshold changed which
    /// trade happened. Swept over deliberately unfriendly capital, price and
    /// ceiling values, long-only and long+short: the measured worst case is
    /// **1.6 ULPs, on the overshooting side**, absorbed with ~13 orders of
    /// magnitude to spare by `gross_tolerance`, which is relative for exactly
    /// this reason. Nothing is refused anywhere in the sweep.
    #[test]
    fn a_sizing_that_lands_on_the_ceiling_is_neither_shaved_nor_refused() {
        let a = crate::types::symbol("A");
        let b = crate::types::symbol("B");
        // Values chosen to make the two computations disagree: a capital with
        // no exact binary form, a sub-cent price, a huge one, and a repeating
        // ceiling.
        for cap in [10_000.0, 10_000.03, 7_919.170_000_000_1, 1e5 / 3.0] {
            for px in [100.0, 137.77, 0.000_012_345_6, 61_803.398_874_989_5] {
                for g in [1.0, 1.5, 2.0, 3.0, 10.0] {
                    // Long only, all of it in one leg.
                    let mut w: PaperWallet<Symbol> = PaperWallet::new(cap).with_max_gross(g);
                    let bars = [(a.clone(), bar(px))];
                    w.advance(&bars);
                    w.set(a.clone(), Side::Buy, Size::value_frac(g)).unwrap();
                    w.advance(&bars);
                    let held = w.position(&a).amount.abs() * px;
                    let budget = g * w.equity().0;
                    let ulps = (held - budget).abs() / (budget * Real::EPSILON);
                    assert!(
                        ulps <= 8.0,
                        "long cap={cap} px={px} g={g}: held {held:e} vs budget {budget:e} \
                         ({ulps} ULPs)",
                    );
                    assert!(
                        w.take_rejections().is_empty(),
                        "long cap={cap} px={px} g={g}: refused a request that lands on the cap",
                    );

                    // Split across a long and a short: the bound is `Σ|pos|`,
                    // so half each has to reach the same ceiling.
                    let mut w: PaperWallet<Symbol> = PaperWallet::new(cap).with_max_gross(g);
                    let bars = [(a.clone(), bar(px)), (b.clone(), bar(px))];
                    w.advance(&bars);
                    w.set(a.clone(), Side::Buy, Size::value_frac(0.5 * g))
                        .unwrap();
                    w.set(b.clone(), Side::Sell, Size::value_frac(0.5 * g))
                        .unwrap();
                    w.advance(&bars);
                    let held = (w.position(&a).amount.abs() + w.position(&b).amount.abs()) * px;
                    let budget = g * w.equity().0;
                    let ulps = (held - budget).abs() / (budget * Real::EPSILON);
                    assert!(
                        ulps <= 8.0,
                        "long/short cap={cap} px={px} g={g}: held {held:e} vs budget \
                         {budget:e} ({ulps} ULPs)",
                    );
                    assert!(
                        w.take_rejections().is_empty(),
                        "long/short cap={cap} px={px} g={g}: refused a request on the cap",
                    );
                }
            }
        }
    }

    /// `funds_frac` sizes against **cash**, and a levered book's cash is
    /// negative — so it degenerates to zero the moment gross passes equity.
    ///
    /// Pinned rather than left to fall out of `funds.max(0.0)`, because the
    /// zero is silent: it produces no trade, no rejection and no fill, and a
    /// document that switched to leverage would simply stop sizing. The
    /// docstring on `Size::FundsFraction` states these numbers; this is what
    /// keeps the two from drifting.
    #[test]
    fn funds_frac_degenerates_to_zero_on_a_levered_book() {
        let sym = crate::types::symbol("S");
        let bars = [(sym.clone(), bar(100.0))];
        // (max_gross, deployed fraction of the budget) -> expected cash
        for (g, deployed, cash) in [
            (1.0, 0.0, 10_000.0),
            (1.0, 0.5, 5_000.0),
            (1.0, 1.0, 0.0),
            (3.0, 0.5, -5_000.0),
            (3.0, 1.0, -20_000.0),
        ] {
            let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0).with_max_gross(g);
            w.advance(&bars);
            if deployed > 0.0 {
                w.set(sym.clone(), Side::Buy, Size::value_frac(deployed * g))
                    .unwrap();
                w.advance(&bars);
            }
            let (funds, equity) = (w.funds().0, w.equity().0);
            assert!(
                (funds - cash).abs() < 1e-6,
                "g={g} deployed={deployed}: cash {funds}, expected {cash}",
            );
            let pos = w.position(&sym).amount;
            let ff = Size::funds_frac(1.0).resolve(100.0, pos, funds, equity);
            assert!(
                (ff - (cash.max(0.0) / 100.0)).abs() < 1e-9,
                "g={g} deployed={deployed}: funds_frac(1.0) resolved to {ff}",
            );
            // Equity, by contrast, is defined on both sides of zero cash: the
            // reason every strategy in the crate sizes on `value_frac`.
            let vf = Size::value_frac(1.0).resolve(100.0, pos, funds, equity);
            assert!((vf - 100.0).abs() < 1e-9, "value_frac moved: {vf}");
        }
    }

    /// The cap counts *gross*, so a long and a short share one budget rather
    /// than each getting their own — the netting a real margin account does not
    /// give you for free.
    #[test]
    fn a_long_and_a_short_share_one_gross_budget() {
        let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
        let bars = [
            (crate::types::symbol("A"), bar(100.0)),
            (crate::types::symbol("B"), bar(100.0)),
        ];
        w.advance(&bars);
        // Half the budget long...
        w.set(crate::types::symbol("A"), Side::Buy, Size::value_frac(0.5))
            .unwrap();
        w.advance(&bars);
        // ...leaves half for the short, however much cash the sale credits.
        w.set(crate::types::symbol("B"), Side::Sell, Size::value_frac(1.0))
            .unwrap();
        w.advance(&bars);

        let a = w.position(&crate::types::symbol("A")).amount;
        let b = w.position(&crate::types::symbol("B")).amount;
        assert!((a - 50.0).abs() < 1e-9, "A held {a}");
        assert!(
            (b + 50.0).abs() < 1e-9,
            "B held {b} — the short took its own budget"
        );
        let gross = (a.abs() + b.abs()) * 100.0;
        assert!((gross - w.equity().0).abs() < 1e-6, "gross {gross}");
    }

    /// `leverage` reports the rule the wallet applies, not a label it was
    /// handed — which is what makes it comparable to a live venue's answer.
    #[test]
    fn leverage_reports_the_cap_it_enforces() {
        let w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        assert_eq!(w.leverage(&"X"), Some(1.0));
        assert_eq!(w.max_gross(), 1.0);

        let w: PaperWallet<&str> = PaperWallet::new(1_000.0).with_max_gross(2.5);
        assert_eq!(w.leverage(&"X"), Some(2.5));
        // Answered for a symbol it has never been fed, because the cap is a
        // property of the account rather than of the instrument.
        assert_eq!(w.leverage(&"never-seen"), Some(2.5));

        // A sleeve is a view onto the account, so it reports the account's rule.
        let sleeve = SleeveWallet::new(w, HashMap::new());
        assert_eq!(sleeve.leverage(&"X"), Some(2.5));
    }

    #[test]
    #[should_panic(expected = "max_gross must be finite and > 0")]
    fn a_non_positive_max_gross_is_a_construction_error() {
        let _: PaperWallet<&str> = PaperWallet::new(1_000.0).with_max_gross(0.0);
    }

    /// An all-in under commission sheds a sliver, and that must stay
    /// distinguishable from a request scaled to a third of itself — the whole
    /// reason the requested magnitude is carried rather than a "was shrunk" flag.
    #[test]
    fn a_rounding_adjustment_and_a_real_reduction_are_told_apart() {
        use crate::costs::{FixedBpsSpread, NoSlippage, PercentageCommission};
        let costs = TradingCosts::new(
            Box::new(PercentageCommission::new(0.001)),
            Box::new(FixedBpsSpread::new(0.0)),
            Box::new(NoSlippage),
        );
        let mut w: PaperWallet<&str> = PaperWallet::with_costs(10_000.0, costs);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        let fills = w.update("X", bar(100.0));
        let ratio = fills[0].fill_ratio();
        assert!(ratio < 1.0, "an all-in under costs must shed something");
        assert!(ratio > 0.99, "but only a sliver, got {ratio}");

        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(3.0)).unwrap();
        let fills = w.update("X", bar(100.0));
        assert!((fills[0].fill_ratio() - 1.0 / 3.0).abs() < 1e-9);
    }

    /// Build an atom carrying one real overlay column.
    fn atom_with(candle: Candle, key: &str, value: Real) -> crate::types::Atom {
        use crate::types::{OverlayInfo, OverlayValue, Schema};
        let mut b = Schema::builder();
        b.add_real(key);
        let overlays = OverlayInfo::new(b.finish(), [OverlayValue::Real(value)]);
        crate::types::Atom::with_overlays(candle, overlays)
    }

    fn funding_wallet(funds: Real) -> PaperWallet<&'static str> {
        let costs = TradingCosts::none().with_carry(Box::new(crate::costs::FundingRate::default()));
        PaperWallet::with_costs(funds, costs)
    }

    /// Funding is charged on what was held *through* the bar, signed both ways.
    #[test]
    fn funding_charges_the_long_and_credits_the_short() {
        for side in [Side::Buy, Side::Sell] {
            let mut w = funding_wallet(10_000.0);
            w.update("X", bar(100.0));
            w.set("X", side, Size::value_frac(1.0)).unwrap();
            w.update("X", bar(100.0));
            let position = w.position(&"X").amount;
            assert!((position.abs() - 100.0).abs() < 1e-9);
            let funds_before = w.funds().0;

            // Next bar carries a 1% funding rate. The position was held through
            // it, so it is charged: a long pays, a short is paid.
            w.observe(&"X", &atom_with(bar(100.0), "funding_rate", 0.01));
            w.advance(&[("X", bar(100.0))]);

            let charge = funds_before - w.funds().0;
            let expected = position * 100.0 * 0.01;
            assert!(
                (charge - expected).abs() < 1e-9,
                "{side:?}: charged {charge}, expected {expected}"
            );
            assert_eq!(w.carry_coverage(), (1, 1));
        }
    }

    /// A position opened *this* bar pays no carry for it — you are charged for
    /// what you held through the interval, not for what you ended up with.
    #[test]
    fn carry_skips_the_bar_a_position_was_opened_on() {
        let mut w = funding_wallet(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        let funds_before = w.funds().0;

        // The fill and the funding sample land on the same bar.
        w.observe(&"X", &atom_with(bar(100.0), "funding_rate", 0.01));
        w.advance(&[("X", bar(100.0))]);

        // Cash moved by the purchase alone: 100 units at 100, and no carry.
        assert!((funds_before - w.funds().0 - 10_000.0).abs() < 1e-9);
        assert_eq!(w.carry_coverage(), (0, 0), "nothing was held to charge");
    }

    /// The distinction the funding column is built around: an absent sample is
    /// "no carry recorded", not "carry was nil" — and the wallet counts the
    /// difference so a run against a series that never had the column is
    /// findable rather than silently free.
    #[test]
    fn a_missing_funding_sample_is_counted_not_assumed_zero() {
        let mut w = funding_wallet(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        w.update("X", bar(100.0));
        let funds_before = w.funds().0;

        // A bar with no overlay at all — the shape of a series that simply has
        // no funding history joined onto it.
        w.advance(&[("X", bar(100.0))]);
        assert_eq!(w.funds().0, funds_before, "nothing to charge");
        assert_eq!(
            w.carry_coverage(),
            (1, 0),
            "one bar wanted a rate and none arrived"
        );
    }

    /// One calendar day in years, the unit the assertions below are written in.
    const DAY: Real = 86_400.0 / SECONDS_PER_YEAR;

    /// One day of carry on the position [`carrying_wallet`] establishes:
    /// `value_frac(1.0)` of 10,000 at a price of 100 is 100 units, so the
    /// notional the annualized rate is charged on is the full 10,000.
    const ONE_DAY_CARRY: Real = 10_000.0 * 0.10 * DAY;

    /// Epoch millis for `2024-01-<day>T00:00:00Z`. 2024-01-05 is a Friday, so
    /// 05 → 08 is the weekend gap the daily-bar case turns on.
    fn jan(day: i64) -> crate::types::Timestamp {
        crate::types::Timestamp(1_704_067_200_000 + (day - 1) * 86_400_000)
    }

    fn timed(candle: Candle, day: i64) -> crate::types::Atom {
        crate::types::Atom::with_time(candle, jan(day))
    }

    /// A wallet holding one unit of `X` at 100, primed with an annualized carry
    /// rate and whatever cadence the caller declares, positioned to charge on
    /// the next `advance`.
    fn carrying_wallet(declared: Option<crate::time::Frequency>) -> PaperWallet<&'static str> {
        let costs = TradingCosts::none().with_carry(Box::new(crate::costs::AnnualRate::flat(0.10)));
        let mut w: PaperWallet<&str> = PaperWallet::with_costs(10_000.0, costs);
        if let Some(freq) = declared {
            w = w.with_bar_frequency(freq);
        }
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        w
    }

    /// **Regression.** Carry is charged over the interval that actually
    /// elapsed, not over the declared cadence. A daily series stamped Friday →
    /// Monday spans three days of a broker's interest; billing `Frequency::Day(1)`
    /// for it under-charges every weekend by 3x.
    #[test]
    fn carry_measures_the_weekend_gap_rather_than_the_declared_cadence() {
        let mut w = carrying_wallet(Some(crate::time::Frequency::Day(1)));

        // Friday: the position is established and the clock takes its first
        // reading. Nothing is held through this bar, so nothing is charged.
        w.observe(&"X", &timed(bar(100.0), 5));
        w.advance(&[("X", bar(100.0))]);

        // Monday: three calendar days later.
        let before = w.funds().0;
        w.observe(&"X", &timed(bar(100.0), 8));
        w.advance(&[("X", bar(100.0))]);

        let charged = before - w.funds().0;
        assert!(
            (charged - 3.0 * ONE_DAY_CARRY).abs() < 1e-9,
            "charged {charged}, expected three days ({})",
            3.0 * ONE_DAY_CARRY
        );
    }

    /// The same arithmetic is what makes an **index-sampled** stream chargeable
    /// at all: volume/dollar/tick bars span no fixed interval by construction,
    /// so there is no cadence to declare and the elapsed gap is the only
    /// honest answer.
    #[test]
    fn carry_prices_irregular_bars_with_no_declared_cadence() {
        let mut w = carrying_wallet(None);

        w.observe(&"X", &timed(bar(100.0), 1));
        w.advance(&[("X", bar(100.0))]);

        // A bar that took five days to fill its bucket.
        let before = w.funds().0;
        w.observe(&"X", &timed(bar(100.0), 6));
        w.advance(&[("X", bar(100.0))]);

        let charged = before - w.funds().0;
        assert!(
            (charged - 5.0 * ONE_DAY_CARRY).abs() < 1e-9,
            "charged {charged} over a five-day bucket"
        );
    }

    /// A stream with no times at all still charges the declared cadence — the
    /// measurement is an upgrade where it exists, never a precondition.
    #[test]
    fn carry_falls_back_to_the_declared_cadence_without_times() {
        let mut w = carrying_wallet(Some(crate::time::Frequency::Day(1)));
        w.advance(&[("X", bar(100.0))]);
        let before = w.funds().0;
        w.advance(&[("X", bar(100.0))]);
        let charged = before - w.funds().0;
        assert!(
            (charged - ONE_DAY_CARRY).abs() < 1e-9,
            "charged {charged}, expected one declared day"
        );
    }

    /// [`carrying_wallet`] with an owned symbol — save/restore needs
    /// `DeserializeOwned`, which `&str` cannot satisfy.
    fn carrying_wallet_owned() -> PaperWallet<String> {
        let costs = TradingCosts::none().with_carry(Box::new(crate::costs::AnnualRate::flat(0.10)));
        let mut w: PaperWallet<String> = PaperWallet::with_costs(10_000.0, costs);
        w.update("X".to_string(), bar(100.0));
        w.set("X".to_string(), Side::Buy, Size::value_frac(1.0))
            .unwrap();
        w
    }

    /// **Regression.** The interval's left endpoint has to survive a chunk
    /// boundary, or a resumed run silently skips the carry between the last bar
    /// of one chunk and the first of the next.
    #[test]
    fn the_carry_clock_survives_save_and_restore() {
        let mut w = carrying_wallet_owned();
        w.observe(&"X".to_string(), &timed(bar(100.0), 5));
        w.advance(&[("X".to_string(), bar(100.0))]);
        let state = w.snapshot_state();

        let mut resumed = carrying_wallet_owned();
        resumed.restore_state(&state).unwrap();
        let before = resumed.funds().0;
        resumed.observe(&"X".to_string(), &timed(bar(100.0), 8));
        resumed.advance(&[("X".to_string(), bar(100.0))]);

        let charged = before - resumed.funds().0;
        assert!(
            (charged - 3.0 * ONE_DAY_CARRY).abs() < 1e-9,
            "charged {charged} across the chunk boundary, expected three days"
        );
    }

    /// A state file written before the clock existed carries no left endpoint,
    /// and must load rather than fail the resume.
    #[test]
    fn a_state_without_a_carry_clock_still_loads() {
        let mut w = carrying_wallet_owned();
        let mut state = w.snapshot_state();
        state.as_object_mut().unwrap().remove("last_bar_time");
        assert!(w.restore_state(&state).is_ok());
    }

    /// An annualized rate needs to know how long a bar is, and refuses to guess.
    #[test]
    fn an_annual_rate_charges_nothing_without_a_cadence() {
        let costs = TradingCosts::none().with_carry(Box::new(crate::costs::AnnualRate::flat(0.10)));
        let mut w: PaperWallet<&str> = PaperWallet::with_costs(10_000.0, costs.clone());
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        w.update("X", bar(100.0));
        let funds_before = w.funds().0;
        w.advance(&[("X", bar(100.0))]);
        assert_eq!(w.funds().0, funds_before, "no year fraction, no charge");

        // Told how long a bar is, it charges: 10% a year on 10,000 of notional,
        // for one day, is 10_000 * 0.10 / 365.25.
        let mut w: PaperWallet<&str> = PaperWallet::with_costs(10_000.0, costs)
            .with_bar_frequency(crate::time::Frequency::Day(1));
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        w.update("X", bar(100.0));
        let funds_before = w.funds().0;
        w.advance(&[("X", bar(100.0))]);
        let expected = 10_000.0 * 0.10 / 365.25;
        assert!(
            (funds_before - w.funds().0 - expected).abs() < 1e-6,
            "charged {}, expected {expected}",
            funds_before - w.funds().0
        );
    }

    /// Margin interest is what a levered *long* actually pays, and it is
    /// account-level: the debt is the negative cash balance, not any position.
    #[test]
    fn margin_interest_is_charged_on_borrowed_cash_only() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0)
            .with_max_gross(3.0)
            .with_margin_rate(0.10)
            .with_bar_frequency(crate::time::Frequency::Day(1));
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(3.0)).unwrap();
        w.update("X", bar(100.0));
        // 300 units at 100 against 10,000 of equity: 20,000 borrowed.
        assert!((w.funds().0 + 20_000.0).abs() < 1e-9, "{}", w.funds().0);

        let funds_before = w.funds().0;
        w.advance(&[("X", bar(100.0))]);
        let expected = 20_000.0 * 0.10 / 365.25;
        assert!(
            (funds_before - w.funds().0 - expected).abs() < 1e-6,
            "charged {}, expected {expected}",
            funds_before - w.funds().0
        );

        // An unlevered book never borrows, so it is never billed.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0)
            .with_margin_rate(0.10)
            .with_bar_frequency(crate::time::Frequency::Day(1));
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        w.update("X", bar(100.0));
        let funds_before = w.funds().0;
        w.advance(&[("X", bar(100.0))]);
        assert_eq!(w.funds().0, funds_before);
    }

    /// The gap that makes an unliquidated levered backtest describe a different
    /// strategy: a 3x book does not survive a drawdown that a 1x book shrugs off.
    #[test]
    fn a_levered_book_is_closed_out_when_it_breaches_maintenance() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0)
            .with_max_gross(3.0)
            .with_maintenance_margin(0.10);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(3.0)).unwrap();
        w.update("X", bar(100.0));
        assert!((w.position(&"X").amount - 300.0).abs() < 1e-9);

        // Down 25%: equity is 10,000 - 300*25 = 2,500 against 22,500 of gross,
        // an 11% ratio — still above the 10% threshold.
        let fills = w.advance(&[("X", bar(75.0))]);
        assert!(fills.is_empty(), "not yet: {fills:?}");
        assert!((w.position(&"X").amount - 300.0).abs() < 1e-9);

        // Down 27%: equity 1,000 against 21,900 of gross is 4.6%. Gone.
        let fills = w.advance(&[("X", bar(73.0))]);
        assert_eq!(fills.len(), 1, "expected a forced close: {fills:?}");
        assert_eq!(fills[0].kind, OrderKind::Liquidation);
        assert_eq!(w.position(&"X").amount, 0.0);
        assert!(w.equity().0 > 0.0, "solvent, but closed out");
    }

    /// The trigger reads the bar's adverse extreme, because a wick is what
    /// actually liquidates a levered account — a close-only test would report a
    /// strategy that survived an event it did not.
    #[test]
    fn a_wick_liquidates_even_when_the_close_recovers() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0)
            .with_max_gross(3.0)
            .with_maintenance_margin(0.10);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(3.0)).unwrap();
        w.update("X", bar(100.0));

        // Opens and closes at 99, but traded down to 73 inside the bar.
        let wick = Candle::new(99.0, 99.0, 73.0, 99.0, 0.0);
        let fills = w.advance(&[("X", wick)]);
        assert_eq!(
            fills.len(),
            1,
            "the wick should have closed the account out"
        );
        assert_eq!(fills[0].kind, OrderKind::Liquidation);
        // Booked at the close, which is the documented simplification: the
        // price the breach happened at is not recoverable from one bar.
        assert!((fills[0].price - 99.0).abs() < 1e-9);
    }

    /// Off unless asked for. The ratio is a venue assumption, so the default has
    /// to be "no margin call", not a guess at someone's tier.
    #[test]
    fn no_maintenance_margin_means_no_margin_call() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0).with_max_gross(3.0);
        assert_eq!(w.maintenance_margin(), None);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(3.0)).unwrap();
        w.update("X", bar(100.0));
        let fills = w.advance(&[("X", bar(70.0))]);
        assert!(fills.is_empty());
        assert!((w.position(&"X").amount - 300.0).abs() < 1e-9);
    }

    #[test]
    fn non_positive_price_is_flagged_at_submission_and_at_fill() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(0.0));
        // fill_at flags a non-positive theoretical price directly...
        assert_eq!(
            w.fill_at(
                "X",
                1.0,
                0.0,
                OrderKind::Market,
                OrderId(0),
                FillContext::exact(1.0, RestOfBook::default())
            ),
            Err(WalletError::InvalidPrice)
        );
        // ...and submissions against a symbol whose last close is
        // non-positive refuse to queue at all.
        assert_eq!(
            w.set("X", Side::Buy, Size::value_frac(1.0)),
            Err(WalletError::InvalidPrice)
        );
        assert_eq!(
            w.set_position(Units {
                symbol: "X",
                amount: 1.0
            }),
            Err(WalletError::InvalidPrice)
        );
    }

    #[test]
    fn fill_outside_candle_range_is_rejected() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", Candle::new(100.0, 110.0, 90.0, 105.0, 0.0));
        // 120 is above the bar's high — it never traded there this bar.
        assert_eq!(
            w.fill_at(
                "X",
                1.0,
                120.0,
                OrderKind::Stop,
                OrderId(0),
                FillContext::exact(1.0, RestOfBook::default())
            ),
            Err(WalletError::PriceOutOfRange)
        );
    }

    #[test]
    fn resting_stop_fills_at_the_level() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0)); // long 1 @ 100
        w.set_stop("X", Reference(90.0), Size::position_frac(1.0))
            .unwrap();
        // The bar trades down through 90 (low 88) but opens above it.
        let fills = w.update("X", Candle::new(95.0, 96.0, 88.0, 89.0, 0.0));
        assert_eq!(fills.len(), 1);
        assert_fill(&fills[0], Side::Sell, 1.0, 90.0, OrderKind::Stop);
        assert!(w.positions().is_empty());
    }

    #[test]
    fn resting_stop_gaps_to_the_open() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0));
        w.set_stop("X", Reference(90.0), Size::position_frac(1.0))
            .unwrap();
        // Gaps down opening at 85, already below the stop -> fills at the open.
        let fills = w.update("X", Candle::new(85.0, 86.0, 84.0, 84.0, 0.0));
        assert_fill(&fills[0], Side::Sell, 1.0, 85.0, OrderKind::Stop);
        assert!(w.positions().is_empty());
    }

    #[test]
    fn resting_take_profit_on_a_short_fills_at_the_level() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Sell, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0)); // short 1 @ 100
        // A short take-profit sits below entry; the bar trades down to it.
        w.set_take_profit("X", Reference(90.0), Size::position_frac(1.0))
            .unwrap();
        let fills = w.update("X", Candle::new(95.0, 96.0, 88.0, 92.0, 0.0));
        assert_fill(&fills[0], Side::Buy, 1.0, 90.0, OrderKind::TakeProfit);
        assert!(w.positions().is_empty());
    }

    #[test]
    fn oco_stop_takes_precedence_and_cancels_the_target() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0));
        w.set_stop("X", Reference(90.0), Size::position_frac(1.0))
            .unwrap();
        w.set_take_profit("X", Reference(110.0), Size::position_frac(1.0))
            .unwrap();
        // A wide bar crosses both legs; the stop wins, and the fill flattens and
        // drops the whole bracket.
        let fills = w.update("X", Candle::new(100.0, 111.0, 89.0, 105.0, 0.0));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].kind, OrderKind::Stop);
        assert!(w.positions().is_empty());
        // No leftover leg: a later bar does nothing.
        let more = w.update("X", Candle::new(105.0, 112.0, 88.0, 100.0, 0.0));
        assert!(more.is_empty());
    }

    #[test]
    fn market_exit_auto_cancels_the_bracket() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0));
        w.set_stop("X", Reference(90.0), Size::position_frac(1.0))
            .unwrap();
        // Flatten with a market close; the fill drops the resting stop.
        w.close("X").unwrap();
        w.update("X", bar(100.0));
        assert!(w.positions().is_empty());
        // The old stop no longer fires even if price revisits 90.
        let fills = w.update("X", Candle::new(95.0, 96.0, 88.0, 89.0, 0.0));
        assert!(fills.is_empty());
    }

    #[test]
    fn cancel_protective_removes_both_legs() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0));
        w.set_stop("X", Reference(90.0), Size::position_frac(1.0))
            .unwrap();
        w.cancel_protective(&"X").unwrap();
        let fills = w.update("X", Candle::new(95.0, 96.0, 88.0, 89.0, 0.0));
        assert!(fills.is_empty());
        assert!(!w.positions().is_empty());
    }

    #[test]
    fn a_sized_stop_takes_off_only_its_share() {
        // The capability a shared account needs: several owners resting exits
        // on one position, each reducing its own share rather than flattening
        // everyone. Previously inexpressible — a stop always targeted 0.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(10.0)).unwrap();
        w.update("X", bar(100.0));

        w.set_stop("X", Reference(90.0), Size::units(4.0)).unwrap();
        let fills = w.update("X", Candle::new(95.0, 96.0, 88.0, 89.0, 0.0));

        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].kind, OrderKind::Stop);
        assert!(
            (fills[0].units - 4.0).abs() < 1e-9,
            "units {}",
            fills[0].units
        );
        assert!(
            (w.position(&"X").amount - 6.0).abs() < 1e-9,
            "6 units should survive the partial stop, got {}",
            w.position(&"X").amount,
        );
    }

    #[test]
    fn a_whole_position_stop_still_flattens() {
        // `position_frac(1.0)` is what every existing caller passes, and it has
        // to behave exactly as the old unsized leg did.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(10.0)).unwrap();
        w.update("X", bar(100.0));

        w.set_stop("X", Reference(90.0), Size::position_frac(1.0))
            .unwrap();
        let fills = w.update("X", Candle::new(95.0, 96.0, 88.0, 89.0, 0.0));

        assert_eq!(fills.len(), 1);
        assert!((fills[0].units - 10.0).abs() < 1e-9);
        assert!(w.positions().is_empty() || w.position(&"X").amount.abs() < 1e-9);
    }

    #[test]
    fn a_sized_stop_is_reduce_only_and_never_flips() {
        // An oversized share is clamped to the position rather than reversing
        // it — a protective leg must never open an opposite position.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(3.0)).unwrap();
        w.update("X", bar(100.0));

        w.set_stop("X", Reference(90.0), Size::units(50.0)).unwrap();
        w.update("X", Candle::new(95.0, 96.0, 88.0, 89.0, 0.0));

        assert!(
            w.position(&"X").amount.abs() < 1e-9,
            "expected flat, got {}",
            w.position(&"X").amount,
        );
    }

    #[test]
    fn a_sized_stop_on_a_short_covers_only_its_share() {
        // Mirror of the long case: "reduce" is toward zero from below.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Sell, Size::units(10.0)).unwrap();
        w.update("X", bar(100.0));

        w.set_stop("X", Reference(110.0), Size::units(4.0)).unwrap();
        let fills = w.update("X", Candle::new(105.0, 112.0, 104.0, 111.0, 0.0));

        assert_eq!(fills.len(), 1);
        assert!((fills[0].units - 4.0).abs() < 1e-9);
        assert!(
            (w.position(&"X").amount + 6.0).abs() < 1e-9,
            "6 short units should survive, got {}",
            w.position(&"X").amount,
        );
    }

    /// A self-contained strategy type: long the golden cross, flat the death
    /// cross, on a configurable symbol. It owns only the symbol and its signals;
    /// the wallet owns the portfolio.
    struct GoldenCross {
        symbol: &'static str,
        enter: Box<dyn Signal<crate::types::Snapshot<&'static str>>>,
        exit: Box<dyn Signal<crate::types::Snapshot<&'static str>>>,
    }
    impl GoldenCross {
        fn new(symbol: &'static str, fast: usize, slow: usize) -> Self {
            use crate::indicators::{Close, Pick};
            let close = || Close::of(Pick::<&'static str>::new());
            Self {
                symbol,
                enter: Box::new(Sma::new(close(), fast).crosses_above(Sma::new(close(), slow))),
                exit: Box::new(Sma::new(close(), fast).crosses_below(Sma::new(close(), slow))),
            }
        }
    }
    impl Strategy for GoldenCross {
        type Input = crate::types::Snapshot<&'static str>;
        type Symbol = &'static str;
        fn update(&mut self, snap: crate::types::Snapshot<&'static str>) {
            // Advance both signals every bar.
            self.enter.update(snap.clone());
            self.exit.update(snap);
        }
        fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
            let flat = wallet.position(&self.symbol).amount.abs() <= POSITION_EPSILON;
            if self.enter.is_true() && flat {
                let _ = wallet.set(self.symbol, Side::Buy, Size::value_frac(1.0));
            } else if self.exit.is_true() && !flat {
                let _ = wallet.close(self.symbol);
            }
        }
        fn reset(&mut self) {
            self.enter.reset();
            self.exit.reset();
        }
    }

    #[test]
    fn custom_strategy_trades_into_its_wallet() {
        let mut strat = GoldenCross::new("X", 2, 4);
        let mut w: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
        // Decline first so the fast/slow MAs warm up with fast *below* slow, then
        // rally (a genuine golden cross) and fall again (a death cross). The
        // initial decline matters: a comparison reads `None` until warmed, so an
        // edge only registers once both MAs are ready — the cross must happen
        // after warm-up, not coincide with it.
        for px in [
            14.0, 13.0, 12.0, 11.0, 10.0, 11.0, 13.0, 15.0, 17.0, 15.0, 12.0, 9.0, 7.0,
        ] {
            w.update("X", bar(px));
            strat.update(crate::types::Snapshot::<&'static str>::of_atom(
                bar(px).into(),
            ));
            strat.trade(&mut w);
        }
        // Market orders fill a bar late, so settle any order the last bar queued.
        w.update("X", bar(7.0));
        // It entered and later exited at least once; ends flat with funds back.
        assert!(!w.orders().is_empty());
        assert!(w.positions().is_empty());
        assert!(w.funds().0 > 0.0);
    }

    /// A two-symbol snapshot — the multi-asset case.
    #[derive(Clone, Copy)]
    struct Pair {
        a: Real,
        b: Real,
    }

    /// A market-neutral pairs leg-in: while flat, go long A and short B. "Am I
    /// in?" is read from the wallet, not stored on the strategy.
    struct PairsTrade;
    impl Strategy for PairsTrade {
        type Input = Pair;
        type Symbol = &'static str;
        fn update(&mut self, _snap: Pair) {}
        fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
            if wallet.position(&"A").amount == 0.0 && wallet.position(&"B").amount == 0.0 {
                let _ = wallet.set_position(Units {
                    symbol: "A",
                    amount: 3.0,
                });
                let _ = wallet.set_position(Units {
                    symbol: "B",
                    amount: -2.0,
                });
            }
        }
        fn reset(&mut self) {}
    }

    #[test]
    fn multi_asset_strategy_acts_on_several_symbols_per_bar() {
        let mut strat = PairsTrade;
        let mut w: PaperWallet<&'static str> = PaperWallet::new(100_000.0);
        let snap = Pair { a: 10.0, b: 20.0 };
        w.update("A", bar(snap.a));
        w.update("B", bar(snap.b));
        strat.update(snap);
        strat.trade(&mut w); // queues both legs
        // The legs fill on each symbol's next bar, at its open.
        w.update("A", bar(snap.a));
        w.update("B", bar(snap.b));
        assert_eq!(w.orders().len(), 2);
        assert_fill(&w.orders()[0], Side::Buy, 3.0, 10.0, OrderKind::Market);
        assert_fill(&w.orders()[1], Side::Sell, 2.0, 20.0, OrderKind::Market);
        assert_eq!(w.position(&"A").amount, 3.0);
        assert_eq!(w.position(&"B").amount, -2.0);
        // Bought 3@10 (-30), shorted 2@20 (+40): net +10 vs start.
        assert_eq!(w.funds().0, 100_000.0 + 10.0);
    }

    /// A basket-shaped setup: no wallet default, every traded symbol enters
    /// via [`PaperWallet::set_costs_for`], and each pays its own commission.
    /// The shape a future N-symbol `BasketStrategy` would use.
    #[test]
    fn per_symbol_costs_scale_to_many_symbols() {
        use crate::costs::{FixedCommission, NoSlippage, NoSpread};
        let mut w: PaperWallet<&'static str> = PaperWallet::new(100_000.0);
        // Universe of five symbols, each on its own commission model.
        let universe = [("A", 1.0), ("B", 2.0), ("C", 3.0), ("D", 4.0), ("E", 5.0)];
        for &(sym, fee) in &universe {
            let _ = w.set_costs_for(
                sym,
                TradingCosts::new(
                    Box::new(FixedCommission::new(fee)),
                    Box::new(NoSpread),
                    Box::new(NoSlippage),
                ),
            );
        }
        // Prime every symbol, then queue and fill one buy per symbol.
        for &(sym, _) in &universe {
            w.update(sym, bar(10.0));
        }
        for &(sym, _) in &universe {
            w.set_position(Units {
                symbol: sym,
                amount: 1.0,
            })
            .unwrap();
        }
        for &(sym, _) in &universe {
            w.update(sym, bar(10.0));
        }
        for &(sym, expected) in &universe {
            let fill = w.orders().iter().find(|o| o.symbol == sym).expect("fill");
            assert!(
                (fill.commission - expected).abs() < 1e-9,
                "{sym}: expected {expected}, got {}",
                fill.commission
            );
        }
    }

    /// A symbol with no per-symbol installation falls back to the wallet's
    /// default bundle — the safe zero-cost default when the wallet is built
    /// via [`PaperWallet::new`].
    #[test]
    fn fill_on_unconfigured_symbol_uses_default_costs() {
        use crate::costs::{FixedCommission, NoSlippage, NoSpread};
        let mut w: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        // Only "A" gets a custom model; "B" trades on the (zero-cost) fallback.
        let _ = w.set_costs_for(
            "A",
            TradingCosts::new(
                Box::new(FixedCommission::new(7.0)),
                Box::new(NoSpread),
                Box::new(NoSlippage),
            ),
        );
        w.update("A", bar(10.0));
        w.update("B", bar(20.0));
        w.set_position(Units {
            symbol: "A",
            amount: 1.0,
        })
        .unwrap();
        w.set_position(Units {
            symbol: "B",
            amount: 1.0,
        })
        .unwrap();
        w.update("A", bar(10.0));
        w.update("B", bar(20.0));
        let a = w.orders().iter().find(|o| o.symbol == "A").unwrap();
        let b = w.orders().iter().find(|o| o.symbol == "B").unwrap();
        assert!((a.commission - 7.0).abs() < 1e-9, "A: {}", a.commission);
        assert!(b.commission.abs() < 1e-9, "B (default): {}", b.commission);
    }

    #[test]
    fn per_symbol_costs_override_the_default_bundle() {
        use crate::costs::{FixedCommission, NoSlippage, NoSpread};
        // Default: $1 per fill. A leg gets its own override: $5 per fill.
        let default = TradingCosts::new(
            Box::new(FixedCommission::new(1.0)),
            Box::new(NoSpread),
            Box::new(NoSlippage),
        );
        let leg_override = TradingCosts::new(
            Box::new(FixedCommission::new(5.0)),
            Box::new(NoSpread),
            Box::new(NoSlippage),
        );
        let mut w: PaperWallet<&'static str> = PaperWallet::with_costs(100_000.0, default);
        let _ = w.set_costs_for("B", leg_override);
        // Prime both symbols and queue a buy on each.
        w.update("A", bar(10.0));
        w.update("B", bar(20.0));
        w.set_position(Units {
            symbol: "A",
            amount: 3.0,
        })
        .unwrap();
        w.set_position(Units {
            symbol: "B",
            amount: 2.0,
        })
        .unwrap();
        // Fill both at the next open.
        w.update("A", bar(10.0));
        w.update("B", bar(20.0));
        // A uses the default: $1 commission. B uses the override: $5.
        let a_fill = w.orders().iter().find(|o| o.symbol == "A").unwrap();
        let b_fill = w.orders().iter().find(|o| o.symbol == "B").unwrap();
        assert!(
            (a_fill.commission - 1.0).abs() < 1e-9,
            "A: got {}",
            a_fill.commission
        );
        assert!(
            (b_fill.commission - 5.0).abs() < 1e-9,
            "B: got {}",
            b_fill.commission
        );
        // Cash out: 100000 − (3·10 + 1) − (2·20 + 5) = 100000 − 31 − 45 = 99924.
        assert!(
            (w.funds().0 - 99_924.0).abs() < 1e-6,
            "funds: {}",
            w.funds().0
        );
    }

    #[test]
    fn adjust_funds_credits_debits_and_rejects_overdraft() {
        let mut w: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
        // Deposit / credit: funds go up, no blotter entry.
        assert!(w.adjust_funds(500.0).is_ok());
        assert_eq!(w.funds().0, 1_500.0);
        assert!(w.orders().is_empty());
        // Withdrawal within available: fine.
        assert!(w.adjust_funds(-1_000.0).is_ok());
        assert_eq!(w.funds().0, 500.0);
        // Overdraft: refused, funds unchanged.
        assert_eq!(
            w.adjust_funds(-1_000.0),
            Err(WalletError::InsufficientFunds)
        );
        assert_eq!(w.funds().0, 500.0);
    }

    #[test]
    fn trait_default_adjust_funds_returns_unsupported() {
        // A minimal Wallet impl that only fills the required methods and
        // relies on the trait defaults — exercising the default
        // `adjust_funds` path a live-broker wallet without account-transfer
        // support would inherit unchanged.
        struct NoTransferWallet;
        impl Wallet<&'static str> for NoTransferWallet {
            fn funds(&self) -> Reference {
                Reference(0.0)
            }
            fn position(&self, symbol: &&'static str) -> Units<&'static str> {
                Units {
                    symbol: *symbol,
                    amount: 0.0,
                }
            }
            fn price(&self, _symbol: &&'static str) -> Option<Reference> {
                None
            }
            fn equity(&self) -> Reference {
                Reference(0.0)
            }
            fn update(
                &mut self,
                _symbol: &'static str,
                _candle: Candle,
            ) -> Vec<Order<&'static str>> {
                Vec::new()
            }
            fn set_position(
                &mut self,
                _target: Units<&'static str>,
            ) -> Result<Ack<&'static str>, WalletError> {
                Err(WalletError::UnsupportedOperation)
            }
            fn set_stop(
                &mut self,
                _symbol: &'static str,
                _trigger: Reference,
                _size: Size,
            ) -> Result<Ack<&'static str>, WalletError> {
                Err(WalletError::UnsupportedOperation)
            }
            fn set_take_profit(
                &mut self,
                _symbol: &'static str,
                _trigger: Reference,
                _size: Size,
            ) -> Result<Ack<&'static str>, WalletError> {
                Err(WalletError::UnsupportedOperation)
            }
            fn cancel_protective(&mut self, _symbol: &&'static str) -> Result<(), WalletError> {
                Ok(())
            }
        }
        let mut w = NoTransferWallet;
        assert_eq!(
            w.adjust_funds(100.0),
            Err(WalletError::UnsupportedOperation)
        );
        assert_eq!(
            w.adjust_funds(-50.0),
            Err(WalletError::UnsupportedOperation)
        );
        // The two Tier-B additions also fall through to their trait defaults:
        // no out-of-band fills, and cancel is unsupported for a bare impl.
        assert!(w.poll_fills().is_empty());
        assert_eq!(w.cancel(OrderId(7)), Err(WalletError::UnsupportedOperation));
    }

    #[test]
    fn paper_wallet_poll_fills_is_empty() {
        // A backtest never has out-of-band fills, so the paper impl keeps the
        // empty default — the driver draining it must be a no-op.
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(100.0));
        w.set_position(Units {
            symbol: "X",
            amount: 3.0,
        })
        .unwrap();
        w.update("X", bar(100.0));
        assert!(w.poll_fills().is_empty());
    }

    #[test]
    fn cancel_drops_a_queued_market_order() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(100.0));
        let id = match w
            .set_position(Units {
                symbol: "X",
                amount: 3.0,
            })
            .unwrap()
        {
            Ack::Working(id) => id,
            Ack::Filled(_) => panic!("market order should queue, not fill"),
        };
        // Cancel before the next bar flushes the queue -> no fill happens.
        assert_eq!(w.cancel(id), Ok(()));
        let fills = w.update("X", bar(100.0));
        assert!(fills.is_empty(), "cancelled order should not fill");
        assert_eq!(w.position(&"X").amount, 0.0);
        // Cancelling an unknown / already-gone id is a no-op, not an error.
        assert_eq!(w.cancel(id), Ok(()));
        assert_eq!(w.cancel(OrderId(999)), Ok(()));
    }

    #[test]
    fn cancel_clears_a_resting_protective_leg() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(10.0)).unwrap();
        w.update("X", bar(100.0)); // long 10 @ 100
        let stop = match w
            .set_stop("X", Reference(90.0), Size::position_frac(1.0))
            .unwrap()
        {
            Ack::Working(id) => id,
            Ack::Filled(_) => panic!("resting order returns Working"),
        };
        w.set_take_profit("X", Reference(120.0), Size::position_frac(1.0))
            .unwrap();
        // Cancel only the stop; the take-profit leg survives and still fires.
        assert_eq!(w.cancel(stop), Ok(()));
        let through_stop = w.update("X", Candle::new(95.0, 96.0, 85.0, 88.0, 0.0));
        assert!(through_stop.is_empty(), "cancelled stop must not fill");
        assert_eq!(w.position(&"X").amount, 10.0);
        let through_tp = w.update("X", Candle::new(115.0, 125.0, 114.0, 121.0, 0.0));
        assert_eq!(through_tp.len(), 1, "take-profit leg should still fire");
        assert_fill(
            &through_tp[0],
            Side::Sell,
            10.0,
            120.0,
            OrderKind::TakeProfit,
        );
    }

    // -- retention -------------------------------------------------------

    /// Book `n` fills. `set` takes a *target* position, so the side has to
    /// alternate — repeating one target is a no-op after the first fill.
    fn churn(w: &mut PaperWallet<&'static str>, n: usize) {
        w.update("X", bar(100.0)); // prime the price
        for i in 0..n {
            let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
            w.set("X", side, Size::units(1.0)).unwrap();
            w.update("X", bar(100.0));
        }
    }

    /// The blotter is a reporting artifact, so it is bounded: a strategy driven
    /// live for years must not accumulate every fill it ever booked.
    #[test]
    fn the_blotter_is_bounded_by_retention() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1e12).with_retention(Some(4));
        churn(&mut w, 50);
        assert!(
            w.orders().len() <= 8,
            "blotter grew past the 2x trim threshold: {}",
            w.orders().len()
        );
        // Trimming drops the *oldest*, so the newest fill is always retained.
        let newest = w.orders().last().expect("a retained fill");
        assert_eq!(newest.id.0, w.next_id - 1, "the newest fill must survive");
    }

    /// `None` is the named opt-out: the caller asked for the whole history and
    /// gets it.
    #[test]
    fn retention_none_keeps_every_order() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1e12).with_retention(None);
        churn(&mut w, 50);
        assert_eq!(w.orders().len(), 50, "opting out must retain everything");
    }

    /// Lowering the limit on an already-populated wallet trims it immediately,
    /// rather than waiting for the next push.
    #[test]
    fn tightening_retention_trims_on_the_spot() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1e12).with_retention(None);
        churn(&mut w, 50);
        let w = w.with_retention(Some(0));
        assert!(w.orders().is_empty(), "a zero limit keeps nothing");
    }

    /// Trimming the rejection log moves the drain cursor with it, so
    /// `take_rejections` still yields each surviving entry exactly once instead
    /// of mis-slicing past the new head.
    #[test]
    fn trimming_rejections_keeps_the_drain_cursor_aligned() {
        // Buy far more than the wallet can afford, in units, so each submission
        // is refused rather than shrunk to fit.
        let mut w: PaperWallet<&str> = PaperWallet::new(100.0).with_retention(Some(3));
        w.update("X", bar(100.0));
        for _ in 0..20 {
            let _ = w.set("X", Side::Buy, Size::units(1e6));
            w.update("X", bar(100.0));
        }
        assert!(!w.rejections().is_empty(), "expected refused submissions");
        assert!(
            w.rejections().len() <= 6,
            "rejection log grew past the trim threshold: {}",
            w.rejections().len()
        );

        let drained = w.take_rejections();
        assert_eq!(
            drained.len(),
            w.rejections().len(),
            "a drain after trimming must yield every surviving entry"
        );
        assert!(
            w.take_rejections().is_empty(),
            "a second drain must yield nothing"
        );
    }

    /// A `NaN` is the one quantity the account cannot recover from: it reads
    /// false against **every** `>` and `<`, so it clears both solvency rules and
    /// the bar-range check, books a `NaN` position, and takes cash and equity
    /// with it for the rest of the run. An expression can manufacture one —
    /// `!mul` near the top of the range overflows to infinity and `!sub` of two
    /// infinities is a `NaN` — and so can a direct `set_position` from Rust or
    /// Python. Every entry point refuses it.
    #[test]
    fn a_non_finite_quantity_is_refused_at_every_entry_point() {
        let bad = [Real::NAN, Real::INFINITY, Real::NEG_INFINITY];

        for x in bad {
            let mut w = PaperWallet::new(10_000.0);
            w.update("BTC", ohlc(100.0, 101.0, 99.0, 100.0));
            assert_eq!(
                w.set_position(Units {
                    symbol: "BTC",
                    amount: x,
                }),
                Err(WalletError::InvalidQuantity),
                "set_position accepted {x}"
            );
            assert_eq!(
                w.set("BTC", Side::Buy, Size::Units(x)),
                Err(WalletError::InvalidQuantity),
                "set(units) accepted {x}"
            );
            assert_eq!(
                w.set("BTC", Side::Buy, Size::ValueFraction(x)),
                Err(WalletError::InvalidQuantity),
                "set(value_frac) accepted {x}"
            );
            assert_eq!(
                w.set_stop("BTC", Reference(x), Size::PositionFraction(1.0)),
                Err(WalletError::InvalidQuantity),
                "set_stop accepted a {x} trigger"
            );
            assert_eq!(
                w.set_take_profit("BTC", Reference(x), Size::PositionFraction(1.0)),
                Err(WalletError::InvalidQuantity),
                "set_take_profit accepted a {x} trigger"
            );
            // A limit's price guard is `InvalidPrice` (it already had one for
            // non-positive); its *size* is the quantity.
            assert!(
                w.set_limit("BTC", Side::Buy, Size::Units(1.0), Reference(x))
                    .is_err(),
                "set_limit accepted a {x} price"
            );
            assert_eq!(
                w.set_limit("BTC", Side::Buy, Size::Units(x), Reference(100.0)),
                Err(WalletError::InvalidQuantity),
                "set_limit accepted a {x} size"
            );
            // Nothing was booked, and nothing rests.
            assert_eq!(w.position(&"BTC").amount, 0.0);
            assert!(w.equity().0.is_finite());
        }

        // The refusals are on the same failure stream as a fill-time drop, so a
        // strategy that ignores the `Err` still leaves a trace.
        let mut w = PaperWallet::new(10_000.0);
        w.update("BTC", ohlc(100.0, 101.0, 99.0, 100.0));
        let _ = w.set_position(Units {
            symbol: "BTC",
            amount: Real::NAN,
        });
        assert!(
            w.rejections()
                .iter()
                .any(|r| r.error == WalletError::InvalidQuantity),
            "the refusal was not recorded"
        );

        // …and an ordinary request is untouched.
        let mut w = PaperWallet::new(10_000.0);
        w.update("BTC", ohlc(100.0, 101.0, 99.0, 100.0));
        assert!(
            w.set_position(Units {
                symbol: "BTC",
                amount: 5.0,
            })
            .is_ok()
        );
    }

    /// The OCO precedence is symmetric. `oco_stop_takes_precedence_and_cancels_the_target`
    /// pins the long branch; `protective_trigger` has a **separate mirror
    /// branch** for a short, and nothing held the two to the same rule — a
    /// short whose take-profit won a wide bar would report the optimistic side
    /// of an ambiguity the long side resolves pessimistically.
    #[test]
    fn oco_stop_takes_precedence_on_a_short_too() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Sell, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0));
        w.set_stop("X", Reference(110.0), Size::position_frac(1.0))
            .unwrap();
        w.set_take_profit("X", Reference(90.0), Size::position_frac(1.0))
            .unwrap();
        // A bar that crosses both. The stop is the adverse leg, and it wins.
        let fills = w.update("X", Candle::new(100.0, 111.0, 89.0, 105.0, 0.0));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].kind, OrderKind::Stop);
        assert_fill(&fills[0], Side::Buy, 1.0, 110.0, OrderKind::Stop);
        assert!(w.positions().is_empty(), "the cover should flatten");
        // The whole bracket goes with it.
        assert!(
            w.update("X", Candle::new(105.0, 112.0, 88.0, 100.0, 0.0))
                .is_empty(),
            "a leg survived the flatten"
        );
    }

    /// A bracket set **before** the position exists guards it from the bar the
    /// entry fills on, not from the bar after.
    ///
    /// That falls out of the phase order — market fills (3, 4) precede
    /// protective legs (5) — but it is the case a caller is most likely to get
    /// wrong in the other direction, and `protective_trigger` returns `None`
    /// while flat, so a reader could reasonably expect the leg to be discarded
    /// on the bar it was set.
    #[test]
    fn a_bracket_set_while_flat_guards_the_entry_from_its_own_bar() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        // Stop first, entry second — both resting when the next bar arrives.
        w.set_stop("X", Reference(95.0), Size::position_frac(1.0))
            .unwrap();
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();

        // One bar: opens at 100 (the entry fills there) and trades down through
        // 95 (the stop fires on the same bar).
        let fills = w.update("X", ohlc(100.0, 101.0, 94.0, 96.0));
        assert_eq!(fills.len(), 2, "expected entry then stop: {fills:?}");
        assert_fill(&fills[0], Side::Buy, 1.0, 100.0, OrderKind::Market);
        assert_fill(&fills[1], Side::Sell, 1.0, 95.0, OrderKind::Stop);
        assert!(w.positions().is_empty());
    }

    /// **A reversal through zero drops the bracket**, and this is the one that
    /// would have hurt.
    ///
    /// `protective_trigger` reads the position's *current* sign to decide which
    /// side of the trigger is adverse, so a long's stop at 90 left resting
    /// against a fresh short becomes a short's stop at 90 — which fires the
    /// moment the bar's high reaches it, i.e. almost immediately, closing a
    /// position the strategy had just opened. It does not, because the fill
    /// that crosses zero clears the bracket; nothing pinned that.
    #[test]
    fn reversing_through_zero_drops_the_brackets_of_the_position_it_left() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0));
        w.set_stop("X", Reference(90.0), Size::position_frac(1.0))
            .unwrap();

        // Long 1 → short 1 in one market order.
        w.set_position(Units {
            symbol: "X",
            amount: -1.0,
        })
        .unwrap();
        let fills = w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        assert_eq!(fills.len(), 1, "the reversal is one fill: {fills:?}");
        assert_fill(&fills[0], Side::Sell, 2.0, 100.0, OrderKind::Market);
        assert_eq!(w.position(&"X").amount, -1.0);

        // A bar whose high is well above the old long-stop's 90. If the leg had
        // survived, it would now read as the short's stop and cover here.
        let after = w.update("X", ohlc(100.0, 120.0, 99.0, 118.0));
        assert!(
            after.is_empty(),
            "the old long's stop fired against the new short: {after:?}"
        );
        assert_eq!(
            w.position(&"X").amount,
            -1.0,
            "the short should still be open"
        );
    }

    /// `requested_units` is what tells a rounding sliver apart from a 17×
    /// reduction, and the two shrink paths reach it differently: cash binds a
    /// long, the gross cap binds a short. Both, plus the guarantee that an
    /// explicit unit count is never quietly shrunk.
    #[test]
    fn a_fitted_fill_records_what_it_was_asked_for() {
        use crate::costs::{PercentageCommission, TradingCosts};

        let account = |cash: Real, max_gross: Real, commission: Real| {
            let c = TradingCosts {
                commission: Box::new(PercentageCommission::new(commission)),
                ..Default::default()
            };
            let mut w: PaperWallet<&str> =
                PaperWallet::with_costs(cash, c).with_max_gross(max_gross);
            w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
            w
        };

        // Cash binds: an all-in long has to shed exactly the commission.
        let mut w = account(10_000.0, 1.0, 0.001);
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        let f = w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        assert_fill(&f[0], Side::Buy, 99.9, 100.0, OrderKind::Market);
        assert_eq!(f[0].requested_units, 100.0);
        assert!(
            (f[0].fill_ratio() - 0.999).abs() < 1e-9,
            "ratio {}",
            f[0].fill_ratio()
        );
        assert!(w.funds().0 >= 0.0, "an all-in must not overdraw");

        // The gross cap binds: `sizing: 3.0` on an unlevered account executes at
        // 1x, and says so. This is the case the field exists for — a rounding
        // sliver and a 3× reduction used to look identical in the blotter.
        let mut w = account(10_000.0, 1.0, 0.0);
        w.set("X", Side::Sell, Size::value_frac(3.0)).unwrap();
        let f = w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        assert_fill(&f[0], Side::Sell, 100.0, 100.0, OrderKind::Market);
        assert_eq!(f[0].requested_units, 300.0);
        assert!((f[0].fill_ratio() - 1.0 / 3.0).abs() < 1e-9);

        // An explicit unit count carries a specific intent, so an infeasible one
        // fails loudly at submission rather than being fitted down.
        let mut w = account(1_000.0, 1.0, 0.0);
        assert_eq!(
            w.set("X", Side::Buy, Size::units(1_000.0)),
            Err(WalletError::InsufficientFunds)
        );
    }

    /// **A `funds_frac` buy on an account whose cash has gone negative
    /// flattens the position.**
    ///
    /// Two documented rules compose into it. `set` targets a position rather
    /// than adding to one, and `FundsFraction` reads `funds.max(0.0)` — chosen
    /// so a negative balance cannot come back through `side.sign() * magnitude`
    /// as a fill on the opposite side. The consequence is that "go as long as
    /// my cash allows", on an account with no spare cash, resolves to a target
    /// of zero units, and a target of zero is a full liquidation.
    ///
    /// It is not reachable from a YAML document — the spec layer only ever
    /// builds `ValueFraction`, which sizes against equity and stays positive on
    /// a levered book — so this is a Rust/Python caller's sharp edge. Pinned
    /// because it is surprising, not because it is settled: a `Buy` that books
    /// a `Sell` of the whole position is worth someone deciding on
    /// deliberately.
    #[test]
    fn a_funds_fraction_buy_with_no_spare_cash_targets_zero_and_so_flattens() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0).with_max_gross(3.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set("X", Side::Buy, Size::value_frac(2.5)).unwrap();
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        assert_eq!(w.position(&"X").amount, 250.0);
        assert!(w.funds().0 < 0.0, "precondition: the book is levered");

        w.set("X", Side::Buy, Size::funds_frac(1.0)).unwrap();
        let f = w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        assert_fill(&f[0], Side::Sell, 250.0, 100.0, OrderKind::Market);
        assert_eq!(w.position(&"X").amount, 0.0);

        // `value_frac` does not have the edge: equity survives the leverage that
        // cash does not, so the same request holds the position.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0).with_max_gross(3.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set("X", Side::Buy, Size::value_frac(2.5)).unwrap();
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set("X", Side::Buy, Size::value_frac(2.5)).unwrap();
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        assert_eq!(w.position(&"X").amount, 250.0, "the position should hold");
    }

    /// The short side of the margin call, which the long-only tests above do
    /// not reach: the breach comes from a price **rise**, and closing it is a
    /// *buy* — the one direction `fill_at`'s cash rule can refuse. It clears,
    /// because an account levered enough to be margin-called has
    /// `max_gross > 1` and the cash rule is lifted there; a rejection would
    /// leave the book open past its own maintenance floor.
    #[test]
    fn a_levered_short_is_covered_when_it_breaches_maintenance() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0)
            .with_max_gross(3.0)
            .with_maintenance_margin(0.10);
        w.update("X", bar(100.0));
        w.set("X", Side::Sell, Size::value_frac(3.0)).unwrap();
        w.update("X", bar(100.0));
        assert_eq!(w.position(&"X").amount, -300.0);

        // Up 20%: equity 4,000 against 36,000 of gross is 11% — still above.
        assert!(w.advance(&[("X", bar(120.0))]).is_empty());
        assert_eq!(w.position(&"X").amount, -300.0);

        // Up 25%: equity 2,500 against 37,500 is 6.7%. Covered.
        let fills = w.advance(&[("X", bar(125.0))]);
        assert_eq!(fills.len(), 1, "expected a forced cover: {fills:?}");
        assert_fill(&fills[0], Side::Buy, 300.0, 125.0, OrderKind::Liquidation);
        assert_eq!(w.position(&"X").amount, 0.0);
        assert!(
            w.rejections().is_empty(),
            "the cover must not be refused: {:?}",
            w.rejections()
        );
        assert!(w.equity().0 > 0.0, "solvent, but closed out");
    }

    /// A margin call is an account-level event, so it closes **every** symbol,
    /// not the one whose bar tripped it — and in a deterministic order, since
    /// the fills land in a blotter a resumed run has to reproduce.
    #[test]
    fn a_margin_call_closes_every_open_symbol() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0)
            .with_max_gross(3.0)
            .with_maintenance_margin(0.10);
        w.update("A", bar(100.0));
        w.update("B", bar(50.0));
        w.set("A", Side::Buy, Size::units(150.0)).unwrap();
        w.set("B", Side::Buy, Size::units(300.0)).unwrap();
        w.advance(&[("A", bar(100.0)), ("B", bar(50.0))]);
        assert_eq!(w.position(&"A").amount, 150.0);
        assert_eq!(w.position(&"B").amount, 300.0);

        let fills = w.advance(&[("A", bar(73.0)), ("B", bar(36.5))]);
        assert_eq!(fills.len(), 2, "both legs should close: {fills:?}");
        assert!(fills.iter().all(|f| f.kind == OrderKind::Liquidation));
        // Ordered by mark, ascending — arbitrary, but the same every run.
        assert_eq!(fills[0].symbol, "B");
        assert_eq!(fills[1].symbol, "A");
        assert!(w.positions().is_empty());
    }

    /// The call clears the *resting* book too. A stop or a limit left behind
    /// would fire against a position that no longer exists, or re-enter one the
    /// account was just closed out of.
    #[test]
    fn a_margin_call_cancels_the_resting_book() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0)
            .with_max_gross(3.0)
            .with_maintenance_margin(0.10);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(3.0)).unwrap();
        w.update("X", bar(100.0));
        w.set_stop("X", Reference(50.0), Size::position_frac(1.0))
            .unwrap();
        w.set_limit("X", Side::Buy, Size::units(1.0), Reference(60.0))
            .unwrap();

        let fills = w.advance(&[("X", bar(73.0))]);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].kind, OrderKind::Liquidation);

        // A bar that would have triggered both: down through 60 and through 50.
        let after = w.advance(&[("X", ohlc(73.0, 74.0, 40.0, 45.0))]);
        assert!(
            after.is_empty(),
            "a resting order survived the call: {after:?}"
        );
        assert_eq!(w.position(&"X").amount, 0.0);
    }

    /// **An absent funding cell must not be charged as a rate.**
    ///
    /// A `Real` overlay slot has no `None`, so an absent sample is stored as a
    /// `NaN` — a blank cell, or a full join that gave this symbol a column
    /// another carries. `observe` reads the column straight off the atom rather
    /// than through `GetReal`, so it saw the sentinel as data: `accrue_carry`
    /// computed `position × price × NaN` and subtracted it from `funds`, and
    /// **one blank cell turned the account's cash and every later equity
    /// reading into `NaN`, permanently**. Worse, the bar counted as *seen*, so
    /// `carry_coverage` reported full coverage — the one diagnostic that exists
    /// to catch a funding series with holes in it said there were none.
    #[test]
    fn an_absent_funding_cell_is_no_sample_rather_than_a_nan_charge() {
        use crate::costs::{FundingRate, TradingCosts};
        use crate::market::{OverlayInfo, Schema};
        use crate::types::{Atom, OverlayValue};
        use std::sync::Arc;

        let mut b = Schema::builder();
        b.add_real("funding_rate");
        let schema = b.finish();
        let with_rate = |rate: Real| {
            Atom::with_overlays(
                ohlc(100.0, 101.0, 99.0, 100.0),
                OverlayInfo::new(Arc::clone(&schema), [OverlayValue::Real(rate)]),
            )
        };

        let costs = TradingCosts {
            carry: Box::new(FundingRate::default()),
            ..Default::default()
        };
        let mut w: PaperWallet<&str> = PaperWallet::with_costs(10_000.0, costs);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set("X", Side::Buy, Size::units(10.0)).unwrap();
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        assert_eq!(w.position(&"X").amount, 10.0);

        // A real rate: 10 units at 100 times 0.0001 is 0.10.
        Wallet::observe(&mut w, &"X", &with_rate(0.0001));
        w.advance(&[("X", ohlc(100.0, 101.0, 99.0, 100.0))]);
        assert!(
            (w.funds().0 - 8_999.9).abs() < 1e-9,
            "funds {}",
            w.funds().0
        );
        assert_eq!(w.carry_coverage(), (1, 1));

        // An absent cell. Nothing is charged, and coverage says so.
        for absent in [Real::NAN, Real::INFINITY, Real::NEG_INFINITY] {
            let before = w.funds().0;
            let (wanted, seen) = w.carry_coverage();
            Wallet::observe(&mut w, &"X", &with_rate(absent));
            w.advance(&[("X", ohlc(100.0, 101.0, 99.0, 100.0))]);
            assert!(
                w.funds().0.is_finite() && w.equity().0.is_finite(),
                "a {absent} funding cell reached the cash balance"
            );
            assert!(
                (w.funds().0 - before).abs() < 1e-12,
                "a {absent} funding cell was charged"
            );
            assert_eq!(
                w.carry_coverage(),
                (wanted + 1, seen),
                "a {absent} cell was counted as a sample the run actually got"
            );
        }

        // And a real rate afterwards still charges — the hole is not sticky.
        Wallet::observe(&mut w, &"X", &with_rate(0.0001));
        w.advance(&[("X", ohlc(100.0, 101.0, 99.0, 100.0))]);
        assert!(
            (w.funds().0 - 8_999.8).abs() < 1e-9,
            "funds {}",
            w.funds().0
        );
    }

    /// **A fractional sizing fitted out of existence is a refusal, and says so.**
    ///
    /// Shrinking a request to what the account can carry is ordinary, and is
    /// recorded on the fill as `requested_units`. Shrinking it to *zero*
    /// produces no fill to record it on — so the leg simply vanished: no order,
    /// no rejection, nothing in the blotter, and a strategy that never traded
    /// looked like one that chose not to.
    ///
    /// The same economic situation reached through an explicit `Size::Units` is
    /// a loud `InsufficientFunds` at submission, and `value_frac` is what the
    /// spec layer builds for *every* sizing — so the silent spelling was the
    /// default one. An unlevered basket whose earlier legs have used the whole
    /// gross budget reaches it on the last leg routinely.
    #[test]
    fn a_sizing_fitted_down_to_no_trade_is_booked_as_a_refusal() {
        let exhausted = || {
            let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
            w.update("A", ohlc(100.0, 101.0, 99.0, 100.0));
            w.update("B", ohlc(100.0, 101.0, 99.0, 100.0));
            // A takes the entire budget at 1x.
            w.set("A", Side::Buy, Size::value_frac(1.0)).unwrap();
            w.advance(&[
                ("A", ohlc(100.0, 101.0, 99.0, 100.0)),
                ("B", ohlc(100.0, 101.0, 99.0, 100.0)),
            ]);
            assert_eq!(w.position(&"A").amount, 100.0);
            w
        };

        // The fractional spelling: no fill, and now a rejection naming why.
        let mut w = exhausted();
        w.set("B", Side::Buy, Size::value_frac(0.33)).unwrap();
        let fills = w.advance(&[
            ("A", ohlc(100.0, 101.0, 99.0, 100.0)),
            ("B", ohlc(100.0, 101.0, 99.0, 100.0)),
        ]);
        assert!(fills.is_empty(), "nothing should fill: {fills:?}");
        assert_eq!(w.position(&"B").amount, 0.0);
        let rejections: Vec<_> = w.rejections().iter().map(|r| (r.symbol, r.error)).collect();
        assert_eq!(
            rejections,
            vec![("B", WalletError::InsufficientFunds)],
            "the fitted-away leg left no trace"
        );

        // The explicit spelling reports the same thing, at submission.
        let mut w = exhausted();
        assert_eq!(
            w.set("B", Side::Buy, Size::units(33.0)),
            Err(WalletError::InsufficientFunds)
        );

        // A fit that merely *shrinks* still fills, and is not a refusal — the
        // fill carries what it was asked for instead.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set("X", Side::Sell, Size::value_frac(3.0)).unwrap();
        let fills = w.advance(&[("X", ohlc(100.0, 101.0, 99.0, 100.0))]);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].requested_units, 300.0);
        assert!(
            w.rejections().is_empty(),
            "a shrunk-but-filled leg is not a refusal: {:?}",
            w.rejections()
        );

        // And an ordinary no-op — already at the target — stays silent.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set("X", Side::Buy, Size::value_frac(0.5)).unwrap();
        w.advance(&[("X", ohlc(100.0, 101.0, 99.0, 100.0))]);
        w.set("X", Side::Buy, Size::value_frac(0.5)).unwrap();
        let fills = w.advance(&[("X", ohlc(100.0, 101.0, 99.0, 100.0))]);
        assert!(fills.is_empty(), "already there: {fills:?}");
        assert!(
            w.rejections().is_empty(),
            "a no-op is not a refusal: {:?}",
            w.rejections()
        );
    }

    /// A flat per-trade fee is the non-convex case the shrink loop has to
    /// survive: `cost = notional + fee` cannot fall below `fee`, so the scale
    /// never reaches `1` and the eight-iteration cap is what ends it. Each of
    /// these converges to the largest size the account can actually pay for,
    /// and the one that cannot afford the fee at all reports a refusal rather
    /// than a silent nothing.
    #[test]
    fn a_flat_fee_shrinks_to_what_the_account_can_pay_for() {
        use crate::costs::{FixedCommission, TradingCosts};

        let attempt = |cash: Real, fee: Real| {
            let costs = TradingCosts {
                commission: Box::new(FixedCommission::new(fee)),
                ..Default::default()
            };
            let mut w: PaperWallet<&str> = PaperWallet::with_costs(cash, costs);
            w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
            w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
            let fills = w.advance(&[("X", ohlc(100.0, 101.0, 99.0, 100.0))]);
            (fills, w)
        };

        // The fee is the whole account: nothing is affordable, and that is a
        // refusal rather than a leg that quietly never happened.
        let (fills, w) = attempt(100.0, 500.0);
        assert!(fills.is_empty());
        assert_eq!(
            w.rejections().iter().map(|r| r.error).collect::<Vec<_>>(),
            vec![WalletError::InsufficientFunds]
        );

        // Otherwise it converges on exactly what is left after the fee, and
        // never overdraws.
        for (cash, fee, units) in [
            (100.0, 99.0, 0.01),
            (100.0, 50.0, 0.5),
            (10_000.0, 5.0, 99.95),
        ] {
            let (fills, w) = attempt(cash, fee);
            assert_eq!(fills.len(), 1, "cash {cash} fee {fee}");
            assert!(
                (fills[0].units - units).abs() < 1e-9,
                "cash {cash} fee {fee}: filled {} not {units}",
                fills[0].units
            );
            assert!(w.funds().0 >= 0.0, "cash {cash} fee {fee} overdrew");
            assert!(w.rejections().is_empty(), "cash {cash} fee {fee}");
        }
    }
}
