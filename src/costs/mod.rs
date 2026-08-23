//! Trading-cost models: commission, spread and slippage the
//! [`PaperWallet`](crate::PaperWallet) folds into every fill.
//!
//! Three small composable traits, each one a function from the incoming
//! theoretical fill to the frictional adjustment for that leg:
//!
//! * [`CommissionModel`] — cash paid on top of the trade, from `(notional, units)`.
//! * [`SpreadModel`] — half-spread added on buys / subtracted on sells before the
//!   fill price is stamped.
//! * [`SlippageModel`] — adverse-to-the-side price adjustment given side, units,
//!   the bar being filled on, and the [`OrderKind`] (so a resting stop can slip
//!   further than a plain market fill).
//!
//! A [`TradingCosts`] bundles one of each. Pass it to
//! [`PaperWallet::with_costs`](crate::PaperWallet::with_costs) and the wallet
//! applies the pipeline **spread → slippage → commission** on every market,
//! stop and take-profit fill:
//!
//! 1. Start from the theoretical trigger price (bar `open` for a market order,
//!    the trigger level — or `open` on a gap — for a stop / take-profit).
//! 2. Apply the spread: buys pay `+half_spread`, sells receive `−half_spread`.
//! 3. Apply the slippage: adverse to the *trading side* (buys slip up, sells
//!    slip down). The [`OrderKind`] threads through so a stop can be given an
//!    extra multiplier over a market fill.
//! 4. The resulting price is what's stamped on the [`Order`](crate::Order) and
//!    recorded as `fills.csv`'s `price` column. Commission is computed from
//!    the *final* price × units and lands on
//!    [`Order::commission`](crate::Order::commission) as a separate figure —
//!    never netted into the price.
//!
//! [`PaperWallet::new`](crate::PaperWallet::new) uses [`NoCommission`],
//! [`NoSpread`] and [`NoSlippage`] internally, so the default construction is
//! byte-identical to the pre-costs release (zero deductions, `Order::commission
//! == 0.0`, and `Order::price` equal to the theoretical price).

use crate::types::{Candle, Real};
use crate::wallet::{OrderKind, Side};

/// A per-fill commission charge, in the wallet's reference currency.
///
/// Called with the fill's `notional` (`final_price × units`, post-spread and
/// post-slippage) and `units` magnitude. The returned amount is deducted from
/// the wallet's cash **on top of** the fill's cash flow and recorded on
/// [`Order::commission`](crate::Order::commission).
///
/// [`clone_box`](Self::clone_box) makes `Box<dyn CommissionModel>` cloneable
/// (via the [`Clone`] impl below), which in turn makes [`TradingCosts`]
/// cloneable — needed so a caller can install one cost bundle onto
/// several wallets (e.g. a [`Portfolio`](crate::portfolio::Portfolio) of
/// N sub-wallets). Every concrete impl in this module implements it as
/// `Box::new(self.clone())`; a downstream impl over an untypeable
/// closure can `unimplemented!()` if that particular model doesn't need
/// to survive cloning.
pub trait CommissionModel: Send + Sync {
    fn commission(&self, notional: Real, units: Real) -> Real;
    /// Return a boxed deep clone of `self`. See the trait doc.
    fn clone_box(&self) -> Box<dyn CommissionModel>;
}

impl Clone for Box<dyn CommissionModel> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// A per-fill half-spread (buys pay it, sells receive it), in reference currency
/// per unit.
///
/// Called with the theoretical fill price and the bar being filled on (some
/// models may want the bar's range to size the spread proportionally).
///
/// [`clone_box`](Self::clone_box) mirrors [`CommissionModel::clone_box`] —
/// see that trait's doc.
pub trait SpreadModel: Send + Sync {
    fn half_spread(&self, price: Real, candle: &Candle) -> Real;
    /// Return a boxed deep clone of `self`.
    fn clone_box(&self) -> Box<dyn SpreadModel>;
}

impl Clone for Box<dyn SpreadModel> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// A per-fill price adjustment, adverse to the trading side.
///
/// Called with the [`Side`] (buys always slip up; sells always slip down), the
/// spread-adjusted `price`, the fill's `units` magnitude, the bar it fills on
/// (for size / volume relative models), and the [`OrderKind`] (a stop or
/// take-profit may be multiplied over a market fill). Returns the **final** fill
/// price the wallet stamps on the order.
///
/// [`clone_box`](Self::clone_box) mirrors [`CommissionModel::clone_box`] —
/// see that trait's doc.
pub trait SlippageModel: Send + Sync {
    fn adjust(
        &self,
        side: Side,
        price: Real,
        units: Real,
        candle: &Candle,
        kind: OrderKind,
    ) -> Real;
    /// Return a boxed deep clone of `self`.
    fn clone_box(&self) -> Box<dyn SlippageModel>;
}

impl Clone for Box<dyn SlippageModel> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// What one bar of *holding* costs — the cost of **carry**.
///
/// The other three legs price a fill; this one prices the gap between fills.
/// Perpetual funding, margin interest, a short's borrow fee and overnight
/// financing all accrue on a position that does nothing, so none of them can be
/// expressed as a function of a trade: `commission(notional, units)` has nowhere
/// to put an elapsed interval, and on a bar that books no fill it is not called
/// at all.
///
/// Charged once per bar by [`PaperWallet::advance`](crate::Wallet::advance), on
/// the position **carried into** the bar and marked at that bar's `open` — you
/// pay for what you held through the interval, not for what you ended up with,
/// so a position opened this bar first pays next bar. Positive means cash leaves
/// the account; **negative is a credit**, which is not a rounding case: the
/// short side of a positive funding rate is paid, and a model that could only
/// charge would be wrong for half the book.
///
/// [`clone_box`](Self::clone_box) mirrors [`CommissionModel::clone_box`] — see
/// that trait's doc.
pub trait CarryModel: Send + Sync {
    /// Cash to charge for carrying `ctx.position` across one bar.
    fn carry(&self, ctx: &CarryContext) -> Real;

    /// The overlay column this model reads a per-bar rate from, if it is
    /// data-driven.
    ///
    /// The wallet asks once per bar and hands the value back as
    /// [`CarryContext::rate`]; a model that needs no data (a constant rate)
    /// leaves this `None` and never sees one. This is how a funding *series*
    /// reaches a cost model at all — the rate is data, not configuration, and
    /// it arrives on the same atom as the bar it is charged for.
    fn column(&self) -> Option<&str> {
        None
    }

    /// Whether this model charges nothing under any input — the tag the CLI's
    /// "no cost model set" banner reads.
    fn is_no_op(&self) -> bool {
        false
    }

    /// Whether this model is **time**-denominated and so needs
    /// [`CarryContext::year_fraction`] to charge anything at all.
    ///
    /// A caller that cannot resolve the run's cadence should say so rather than
    /// let an annualized rate quietly evaluate to zero on every bar — the
    /// failure looks exactly like "carry was free".
    fn needs_year_fraction(&self) -> bool {
        false
    }

    /// Return a boxed deep clone of `self`. See the trait doc.
    fn clone_box(&self) -> Box<dyn CarryModel>;
}

impl Clone for Box<dyn CarryModel> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// What a [`CarryModel`] is given for one symbol, for one bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CarryContext {
    /// The **signed** position carried into the bar, in instrument units —
    /// positive long, negative short. Signed rather than absolute because
    /// funding's direction depends on which side you are: at a positive rate
    /// longs pay shorts.
    pub position: Real,
    /// The mark this bar's carry values the position at — the bar's `open`, the
    /// same price its fills settle against, so nothing here reads information
    /// from later in the bar than the charge.
    pub price: Real,
    /// What fraction of a year this bar spans, or `None` when the caller could
    /// not resolve the run's cadence.
    ///
    /// Only a time-denominated model needs it (an annual interest rate has to
    /// be pro-rated); a settlement-denominated one — funding, which is quoted
    /// *per settlement* and already summed into the bar — must ignore it, or it
    /// would scale a charge the venue states in full. `None` is not a licence to
    /// guess: a rate model with no cadence charges nothing and the CLI warns,
    /// rather than inventing a year length.
    pub year_fraction: Option<Real>,
    /// This bar's value of the model's [`column`](CarryModel::column), when it
    /// named one and the bar carried a sample.
    ///
    /// **`None` is "no sample", not "zero"** — the distinction the
    /// `binance-vision-futures` funding column is built around, and the
    /// difference between "no carry was recorded for this bar" and "carry was
    /// nil". A model that cannot tell them apart will silently under-charge
    /// every bar whose data is missing.
    pub rate: Option<Real>,
}

// ---------------------------------------------------------------------------
// Bundle
// ---------------------------------------------------------------------------

/// The three cost models a [`PaperWallet`](crate::PaperWallet) applies to every
/// fill. Bundles cheaply into a caller-owned [`Clone`] so one bundle can seed
/// multiple wallets (a portfolio of N sub-wallets, per-symbol overrides via
/// [`Wallet::set_costs_for`](crate::Wallet::set_costs_for)). See
/// the trait docs for the [`clone_box`](CommissionModel::clone_box) contract.
#[derive(Clone)]
/// fill, bundled together so the wallet holds one field and a caller passes one
/// value to [`PaperWallet::with_costs`](crate::PaperWallet::with_costs).
pub struct TradingCosts {
    pub commission: Box<dyn CommissionModel>,
    pub spread: Box<dyn SpreadModel>,
    pub slippage: Box<dyn SlippageModel>,
    /// The cost of **holding**, charged per bar rather than per fill. Defaults
    /// to [`NoCarry`]; set it with [`with_carry`](TradingCosts::with_carry).
    pub carry: Box<dyn CarryModel>,
}

impl TradingCosts {
    /// A brand-new bundle with the given components.
    pub fn new(
        commission: Box<dyn CommissionModel>,
        spread: Box<dyn SpreadModel>,
        slippage: Box<dyn SlippageModel>,
    ) -> Self {
        Self {
            commission,
            spread,
            slippage,
            carry: Box::new(NoCarry),
        }
    }

    /// Install the [`CarryModel`] — the per-bar cost of holding.
    ///
    /// A builder rather than a fourth parameter on [`new`](Self::new): the three
    /// fill legs are what almost every bundle configures, carry matters only
    /// once an account holds something overnight or borrows to hold it, and
    /// every existing three-argument call keeps working.
    pub fn with_carry(mut self, carry: Box<dyn CarryModel>) -> Self {
        self.carry = carry;
        self
    }

    /// The zero-friction bundle: [`NoCommission`] + [`NoSpread`] + [`NoSlippage`].
    /// What [`PaperWallet::new`](crate::PaperWallet::new) is built with.
    pub fn none() -> Self {
        Self {
            commission: Box::new(NoCommission),
            spread: Box::new(NoSpread),
            slippage: Box::new(NoSlippage),
            carry: Box::new(NoCarry),
        }
    }

    /// Whether every component is a no-op (i.e. this bundle deducts nothing on
    /// any fill). Cheap struct-tag check — the CLI uses it to decide whether to
    /// print the "no cost model set" warning banner.
    pub fn is_none(&self) -> bool {
        self.commission.is_no_op()
            && self.spread.is_no_op()
            && self.slippage.is_no_op()
            && self.carry.is_no_op()
    }
}

impl std::fmt::Debug for TradingCosts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TradingCosts").finish_non_exhaustive()
    }
}

impl Default for TradingCosts {
    fn default() -> Self {
        Self::none()
    }
}

// ---------------------------------------------------------------------------
// No-op implementations
// ---------------------------------------------------------------------------

/// The zero-commission model: every fill costs `0.0`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCommission;

impl CommissionModel for NoCommission {
    fn commission(&self, _notional: Real, _units: Real) -> Real {
        0.0
    }
    fn clone_box(&self) -> Box<dyn CommissionModel> {
        Box::new(*self)
    }
}

/// The zero-spread model: half-spread is `0.0` on every fill.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSpread;

impl SpreadModel for NoSpread {
    fn half_spread(&self, _price: Real, _candle: &Candle) -> Real {
        0.0
    }
    fn clone_box(&self) -> Box<dyn SpreadModel> {
        Box::new(*self)
    }
}

/// The zero-slippage model: the fill price passes through unchanged.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSlippage;

impl SlippageModel for NoSlippage {
    fn adjust(
        &self,
        _side: Side,
        price: Real,
        _units: Real,
        _candle: &Candle,
        _kind: OrderKind,
    ) -> Real {
        price
    }
    fn clone_box(&self) -> Box<dyn SlippageModel> {
        Box::new(*self)
    }
}

/// The zero-carry model: holding is free. What every wallet starts with.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCarry;

impl CarryModel for NoCarry {
    fn carry(&self, _ctx: &CarryContext) -> Real {
        0.0
    }
    fn is_no_op(&self) -> bool {
        true
    }
    fn clone_box(&self) -> Box<dyn CarryModel> {
        Box::new(*self)
    }
}

// ---------------------------------------------------------------------------
// Carry models
// ---------------------------------------------------------------------------

/// Perpetual **funding**, read per bar from an overlay column.
///
/// `carry = position × price × rate`, signed both ways: at a positive rate a
/// long pays and a short is credited, and when the rate goes negative — which
/// it regularly does — the two swap. That sign flip is the reason funding
/// cannot be approximated by a constant: a fixed rate is not a conservative
/// stand-in for it, it is simply wrong in a direction nobody chose.
///
/// The rate is **data**, so it arrives on the same atom as the bar it is charged
/// for, through [`CarryModel::column`]. `binance-vision-futures` publishes it as
/// `funding_rate` already shaped for this: a bar's value is the **sum** of the
/// settlements inside it, so a `1d` bar carries all three of its 8-hourly
/// charges and the model needs no notion of settlement cadence at all.
///
/// For the same reason [`CarryContext::year_fraction`] is deliberately ignored:
/// funding is quoted per settlement and already totalled per bar, so pro-rating
/// it by a year fraction would scale a charge the venue states in full.
///
/// A bar with **no sample** charges nothing — the honest reading of an absent
/// column, and why the column and the rate are `Option` all the way down. A
/// series that simply lacks funding data will therefore under-charge silently,
/// which is what [`PaperWallet::carry_coverage`](crate::PaperWallet::carry_coverage)
/// exists to let a caller check.
#[derive(Debug, Clone)]
pub struct FundingRate {
    column: String,
}

impl FundingRate {
    /// Read the per-bar rate from `column`.
    pub fn new(column: impl Into<String>) -> Self {
        Self {
            column: column.into(),
        }
    }

    /// The conventional column name, as `binance-vision-futures` publishes it.
    pub const DEFAULT_COLUMN: &'static str = "funding_rate";
}

impl Default for FundingRate {
    fn default() -> Self {
        Self::new(Self::DEFAULT_COLUMN)
    }
}

impl CarryModel for FundingRate {
    fn carry(&self, ctx: &CarryContext) -> Real {
        match ctx.rate {
            Some(rate) => ctx.position * ctx.price * rate,
            None => 0.0,
        }
    }
    fn column(&self) -> Option<&str> {
        Some(&self.column)
    }
    fn clone_box(&self) -> Box<dyn CarryModel> {
        Box::new(self.clone())
    }
}

/// A constant **annualized** rate on the position's notional, pro-rated to the
/// bar — margin interest on a levered long, a borrow fee on a short.
///
/// `carry = |position| × price × rate_for_side × year_fraction`, with separate
/// long and short rates because they are separate arrangements: a long pays to
/// borrow cash, a short pays to borrow stock, and the two are not the same
/// number. Both are charges, so both are non-negative and neither side is ever
/// credited — unlike [`FundingRate`], where the sign is the point.
///
/// This is the model for the things that genuinely *are* near-constant. Reaching
/// for it to stand in for perpetual funding is the mistake `FundingRate`'s doc
/// warns about.
///
/// **Charges nothing when [`year_fraction`](CarryContext::year_fraction) is
/// `None`.** An annual rate is meaningless without knowing how much of a year a
/// bar spans, and guessing 252 or 365 would be an invented venue assumption; the
/// CLI warns rather than letting the omission pass as "carry was free".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnnualRate {
    long: Real,
    short: Real,
}

impl AnnualRate {
    /// Separate annualized rates for a long and a short position, as decimal
    /// fractions (`0.06` is 6% a year). Negatives are clamped to zero — this
    /// model charges, it does not pay.
    pub fn new(long: Real, short: Real) -> Self {
        Self {
            long: long.max(0.0),
            short: short.max(0.0),
        }
    }

    /// The same annualized rate whichever side the position is.
    pub fn flat(rate: Real) -> Self {
        Self::new(rate, rate)
    }
}

impl CarryModel for AnnualRate {
    fn carry(&self, ctx: &CarryContext) -> Real {
        let Some(year_fraction) = ctx.year_fraction else {
            return 0.0;
        };
        let rate = if ctx.position >= 0.0 {
            self.long
        } else {
            self.short
        };
        ctx.position.abs() * ctx.price * rate * year_fraction
    }
    fn is_no_op(&self) -> bool {
        self.long == 0.0 && self.short == 0.0
    }
    fn needs_year_fraction(&self) -> bool {
        !self.is_no_op()
    }
    fn clone_box(&self) -> Box<dyn CarryModel> {
        Box::new(*self)
    }
}

/// Both at once: a [`FundingRate`] and an [`AnnualRate`], summed.
///
/// A margined perp account pays both — the exchange's funding on the position
/// *and* interest on the cash it borrowed to hold it — and they are denominated
/// differently, so neither model can absorb the other.
#[derive(Debug, Clone)]
pub struct CompositeCarry {
    funding: FundingRate,
    rate: AnnualRate,
}

impl CompositeCarry {
    /// Charge `funding` and `rate` together.
    pub fn new(funding: FundingRate, rate: AnnualRate) -> Self {
        Self { funding, rate }
    }
}

impl CarryModel for CompositeCarry {
    fn carry(&self, ctx: &CarryContext) -> Real {
        self.funding.carry(ctx) + self.rate.carry(ctx)
    }
    fn column(&self) -> Option<&str> {
        self.funding.column()
    }
    fn is_no_op(&self) -> bool {
        // Never claims to be free: the funding leg's charge depends on data
        // this cannot see, and the banner it feeds is a warning about silence.
        false
    }
    fn needs_year_fraction(&self) -> bool {
        self.rate.needs_year_fraction()
    }
    fn clone_box(&self) -> Box<dyn CarryModel> {
        Box::new(self.clone())
    }
}

// ---------------------------------------------------------------------------
// Commission models
// ---------------------------------------------------------------------------

/// A flat per-ticket commission: every fill costs `amount`, regardless of size.
#[derive(Debug, Clone, Copy)]
pub struct FixedCommission {
    pub amount: Real,
}

impl FixedCommission {
    pub fn new(amount: Real) -> Self {
        Self { amount }
    }
}

impl CommissionModel for FixedCommission {
    fn commission(&self, _notional: Real, _units: Real) -> Real {
        self.amount.max(0.0)
    }
    fn clone_box(&self) -> Box<dyn CommissionModel> {
        Box::new(*self)
    }
}

/// A percentage-of-notional commission: `rate × notional` (e.g. `rate = 0.001`
/// for a 10 bps taker fee).
#[derive(Debug, Clone, Copy)]
pub struct PercentageCommission {
    pub rate: Real,
}

impl PercentageCommission {
    pub fn new(rate: Real) -> Self {
        Self { rate }
    }
}

impl CommissionModel for PercentageCommission {
    fn commission(&self, notional: Real, _units: Real) -> Real {
        (self.rate * notional.abs()).max(0.0)
    }
    fn clone_box(&self) -> Box<dyn CommissionModel> {
        Box::new(*self)
    }
}

/// A per-unit commission: `rate × units` (e.g. `rate = 0.005` for a
/// half-cent-per-share equities fee).
#[derive(Debug, Clone, Copy)]
pub struct PerUnitCommission {
    pub rate: Real,
}

impl PerUnitCommission {
    pub fn new(rate: Real) -> Self {
        Self { rate }
    }
}

impl CommissionModel for PerUnitCommission {
    fn commission(&self, _notional: Real, units: Real) -> Real {
        (self.rate * units.abs()).max(0.0)
    }
    fn clone_box(&self) -> Box<dyn CommissionModel> {
        Box::new(*self)
    }
}

/// Sum of several commission components. Useful when a venue charges multiple
/// concurrent legs (e.g. an exchange fee plus a regulatory fee).
#[derive(Clone)]
pub struct CompositeCommission {
    parts: Vec<Box<dyn CommissionModel>>,
}

impl CompositeCommission {
    pub fn new(parts: Vec<Box<dyn CommissionModel>>) -> Self {
        Self { parts }
    }
}

impl CommissionModel for CompositeCommission {
    fn commission(&self, notional: Real, units: Real) -> Real {
        self.parts
            .iter()
            .map(|p| p.commission(notional, units))
            .sum()
    }
    fn clone_box(&self) -> Box<dyn CommissionModel> {
        Box::new(self.clone())
    }
}

impl std::fmt::Debug for CompositeCommission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeCommission")
            .field("parts", &self.parts.len())
            .finish()
    }
}

/// `max(a, b)` of two commission models — the shape a per-order-minimum fee
/// takes (e.g. IBKR's US-equities schedule: `max($1.00, $0.0035 × shares)`).
#[derive(Clone)]
pub struct MaxCommission {
    pub lhs: Box<dyn CommissionModel>,
    pub rhs: Box<dyn CommissionModel>,
}

impl MaxCommission {
    pub fn new(lhs: Box<dyn CommissionModel>, rhs: Box<dyn CommissionModel>) -> Self {
        Self { lhs, rhs }
    }
}

impl CommissionModel for MaxCommission {
    fn commission(&self, notional: Real, units: Real) -> Real {
        let a = self.lhs.commission(notional, units);
        let b = self.rhs.commission(notional, units);
        a.max(b)
    }
    fn clone_box(&self) -> Box<dyn CommissionModel> {
        Box::new(self.clone())
    }
}

impl std::fmt::Debug for MaxCommission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaxCommission").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Spread models
// ---------------------------------------------------------------------------

/// A fixed basis-point half-spread: `bps × 1e-4 × price / 2`. So `bps = 10`
/// means a 10 bp round-trip spread — 5 bp per side.
#[derive(Debug, Clone, Copy)]
pub struct FixedBpsSpread {
    pub bps: Real,
}

impl FixedBpsSpread {
    pub fn new(bps: Real) -> Self {
        Self { bps }
    }
}

impl SpreadModel for FixedBpsSpread {
    fn half_spread(&self, price: Real, _candle: &Candle) -> Real {
        (self.bps.abs() * price.abs() * 1e-4) / 2.0
    }
    fn clone_box(&self) -> Box<dyn SpreadModel> {
        Box::new(*self)
    }
}

/// A fixed absolute half-spread in reference currency units (`amount` per unit
/// on each side).
#[derive(Debug, Clone, Copy)]
pub struct FixedAbsoluteSpread {
    pub amount: Real,
}

impl FixedAbsoluteSpread {
    pub fn new(amount: Real) -> Self {
        Self { amount }
    }
}

impl SpreadModel for FixedAbsoluteSpread {
    fn half_spread(&self, _price: Real, _candle: &Candle) -> Real {
        self.amount.abs() / 2.0
    }
    fn clone_box(&self) -> Box<dyn SpreadModel> {
        Box::new(*self)
    }
}

// ---------------------------------------------------------------------------
// Slippage models
// ---------------------------------------------------------------------------

/// Default multiplier applied to a stop/take-profit's slippage over a plain
/// market fill (1.5× — a triggered stop in a fast market typically slips more
/// than a planned entry). See [`FixedBpsSlippage::stop_multiplier`] /
/// [`VolumeParticipationSlippage::stop_multiplier`].
pub const DEFAULT_STOP_MULTIPLIER: Real = 1.5;

/// A fixed basis-point slippage: the price moves `bps × 1e-4` adverse to the
/// trading side (buys up, sells down), scaled by `stop_multiplier` on stop or
/// take-profit fills.
#[derive(Debug, Clone, Copy)]
pub struct FixedBpsSlippage {
    pub bps: Real,
    /// Multiplier applied when the fill is a resting stop or take-profit
    /// (default [`DEFAULT_STOP_MULTIPLIER`]).
    pub stop_multiplier: Real,
}

impl FixedBpsSlippage {
    pub fn new(bps: Real) -> Self {
        Self {
            bps,
            stop_multiplier: DEFAULT_STOP_MULTIPLIER,
        }
    }

    pub fn with_stop_multiplier(mut self, stop_multiplier: Real) -> Self {
        self.stop_multiplier = stop_multiplier;
        self
    }
}

impl SlippageModel for FixedBpsSlippage {
    fn adjust(
        &self,
        side: Side,
        price: Real,
        _units: Real,
        _candle: &Candle,
        kind: OrderKind,
    ) -> Real {
        let mult = kind_multiplier(kind, self.stop_multiplier);
        let move_frac = self.bps.abs() * 1e-4 * mult;
        adverse(side, price, price.abs() * move_frac)
    }
    fn clone_box(&self) -> Box<dyn SlippageModel> {
        Box::new(*self)
    }
}

/// Almgren–Chriss-style volume-participation slippage: impact grows as
/// `coefficient × (units / candle.volume).powf(exponent) × price`, always
/// adverse to the trading side. Default `exponent = 0.5` (square-root impact).
///
/// Guards against a zero-volume bar (impact reads `0.0`) so a series without
/// volume data doesn't produce `NaN`/`Infinity`.
///
/// This is a **single-bar** approximation — no order-book state or participation
/// caps carry across bars. A fill uses only its own bar's volume; a caller
/// modeling a genuinely large order should split it across bars in strategy
/// code rather than expect the wallet to.
#[derive(Debug, Clone, Copy)]
pub struct VolumeParticipationSlippage {
    pub coefficient: Real,
    pub exponent: Real,
    /// Multiplier applied when the fill is a resting stop or take-profit
    /// (default [`DEFAULT_STOP_MULTIPLIER`]).
    pub stop_multiplier: Real,
}

impl VolumeParticipationSlippage {
    pub fn new(coefficient: Real) -> Self {
        Self {
            coefficient,
            exponent: 0.5,
            stop_multiplier: DEFAULT_STOP_MULTIPLIER,
        }
    }

    pub fn with_exponent(mut self, exponent: Real) -> Self {
        self.exponent = exponent;
        self
    }

    pub fn with_stop_multiplier(mut self, stop_multiplier: Real) -> Self {
        self.stop_multiplier = stop_multiplier;
        self
    }
}

impl SlippageModel for VolumeParticipationSlippage {
    fn adjust(
        &self,
        side: Side,
        price: Real,
        units: Real,
        candle: &Candle,
        kind: OrderKind,
    ) -> Real {
        if candle.volume <= 0.0 {
            return price;
        }
        let participation = (units.abs() / candle.volume).max(0.0);
        // Guard against a nan/inf `powf` output (participation of 0 with a
        // non-positive exponent is a modeling error, not a runtime one).
        let mult = kind_multiplier(kind, self.stop_multiplier);
        let impact_frac = self.coefficient * participation.powf(self.exponent) * mult;
        if !impact_frac.is_finite() {
            return price;
        }
        adverse(side, price, price.abs() * impact_frac)
    }
    fn clone_box(&self) -> Box<dyn SlippageModel> {
        Box::new(*self)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The slippage multiplier for a [`OrderKind`]: `1.0` on a market fill,
/// `stop_multiplier` on a stop or take-profit, and `0.0` on a limit.
///
/// A limit order rests until the market comes to it, so it provides liquidity
/// rather than taking it — there is no impact to model, and any adverse
/// adjustment would fill it worse than the price the caller named, which is
/// precisely what a limit order rules out. Commission still applies; the crate
/// has no maker/taker distinction to price the difference with.
fn kind_multiplier(kind: OrderKind, stop_multiplier: Real) -> Real {
    match kind {
        OrderKind::Market => 1.0,
        // A forced close takes liquidity on someone else's schedule, so it
        // prices at least as badly as a stop — never better.
        OrderKind::Stop | OrderKind::TakeProfit | OrderKind::Liquidation => {
            stop_multiplier.max(0.0)
        }
        OrderKind::Limit => 0.0,
    }
}

/// Move `price` `magnitude` cash units adverse to `side`: buys pay more (`+`),
/// sells receive less (`−`).
fn adverse(side: Side, price: Real, magnitude: Real) -> Real {
    match side {
        Side::Buy => price + magnitude,
        Side::Sell => price - magnitude,
    }
}

// ---------------------------------------------------------------------------
// Zero-op detection (for the CLI's "no costs" warning banner)
// ---------------------------------------------------------------------------

/// Whether a model is provably a no-op — the [`No*`](NoCommission) built-ins
/// return `true`; every other model returns `false` by default. Not part of the
/// public trait: a downstream impl doesn't get to lie about being free.
trait IsNoOp {
    fn is_no_op(&self) -> bool;
}

impl IsNoOp for dyn CommissionModel {
    fn is_no_op(&self) -> bool {
        // Sniff the tag through a canonical probe: a no-op returns 0 on any
        // input. This is a slight over-detection (`FixedCommission { amount: 0 }`
        // reads as no-op too), which is deliberate — a zero fixed fee should
        // not print the "no cost model set" warning.
        self.commission(1.0, 1.0) == 0.0 && self.commission(1_000_000.0, 1_000.0) == 0.0
    }
}

impl IsNoOp for dyn SpreadModel {
    fn is_no_op(&self) -> bool {
        let bar = Candle::new(100.0, 100.0, 100.0, 100.0, 1.0);
        self.half_spread(100.0, &bar) == 0.0 && self.half_spread(1_000_000.0, &bar) == 0.0
    }
}

impl IsNoOp for dyn SlippageModel {
    fn is_no_op(&self) -> bool {
        let bar = Candle::new(100.0, 100.0, 100.0, 100.0, 1.0);
        // A no-op passes the price through unchanged on both sides and both
        // kinds; anything else adjusts at least one probe away from the input.
        for kind in [OrderKind::Market, OrderKind::Stop, OrderKind::TakeProfit] {
            for side in [Side::Buy, Side::Sell] {
                if self.adjust(side, 100.0, 1.0, &bar, kind) != 100.0 {
                    return false;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(price: Real, volume: Real) -> Candle {
        Candle::new(price, price, price, price, volume)
    }

    #[test]
    fn no_op_costs_are_free_and_transparent() {
        let costs = TradingCosts::none();
        assert!(costs.is_none());
        let b = bar(100.0, 1_000.0);
        assert_eq!(costs.commission.commission(1_000.0, 1.0), 0.0);
        assert_eq!(costs.spread.half_spread(100.0, &b), 0.0);
        assert_eq!(
            costs
                .slippage
                .adjust(Side::Buy, 100.0, 1.0, &b, OrderKind::Market),
            100.0
        );
        assert_eq!(
            costs
                .slippage
                .adjust(Side::Sell, 100.0, 1.0, &b, OrderKind::Stop),
            100.0
        );
    }

    #[test]
    fn fixed_commission_is_flat_per_ticket() {
        let c = FixedCommission::new(1.5);
        assert_eq!(c.commission(1_000.0, 1.0), 1.5);
        assert_eq!(c.commission(1_000_000.0, 100.0), 1.5);
    }

    #[test]
    fn percentage_commission_scales_with_notional() {
        let c = PercentageCommission::new(0.001);
        assert_eq!(c.commission(1_000.0, 10.0), 1.0);
        assert_eq!(c.commission(50_000.0, 5.0), 50.0);
    }

    #[test]
    fn per_unit_commission_scales_with_units() {
        let c = PerUnitCommission::new(0.005);
        assert_eq!(c.commission(5_000.0, 100.0), 0.5);
        assert_eq!(c.commission(9_999.0, 1_000.0), 5.0);
    }

    #[test]
    fn composite_commission_sums_components() {
        let c = CompositeCommission::new(vec![
            Box::new(FixedCommission::new(0.25)),
            Box::new(PercentageCommission::new(0.001)),
        ]);
        // 0.25 + 0.001 * 10_000 = 10.25
        assert_eq!(c.commission(10_000.0, 100.0), 10.25);
    }

    #[test]
    fn max_commission_picks_the_larger_leg() {
        let c = MaxCommission::new(
            Box::new(FixedCommission::new(1.0)),
            Box::new(PerUnitCommission::new(0.0035)),
        );
        // Small ticket: fixed wins.
        assert_eq!(c.commission(100.0, 10.0), 1.0);
        // Large ticket: per-unit wins.
        assert_eq!(c.commission(100_000.0, 1_000.0), 3.5);
    }

    #[test]
    fn fixed_bps_spread_is_half_the_full_spread() {
        // 10 bps round-trip = 5 bps per side; on 100 that's 0.05.
        let s = FixedBpsSpread::new(10.0);
        assert!((s.half_spread(100.0, &bar(100.0, 0.0)) - 0.05).abs() < 1e-12);
    }

    #[test]
    fn fixed_absolute_spread_is_half_the_amount() {
        let s = FixedAbsoluteSpread::new(0.02);
        assert!((s.half_spread(999.0, &bar(999.0, 0.0)) - 0.01).abs() < 1e-12);
    }

    #[test]
    fn fixed_bps_slippage_is_adverse_and_kind_aware() {
        let s = FixedBpsSlippage::new(10.0); // 10 bps = 0.1% impact
        let b = bar(100.0, 0.0);
        // Buy on a market fill: +0.1% -> 100.1
        assert!((s.adjust(Side::Buy, 100.0, 1.0, &b, OrderKind::Market) - 100.1).abs() < 1e-9);
        // Sell on a market fill: -0.1% -> 99.9
        assert!((s.adjust(Side::Sell, 100.0, 1.0, &b, OrderKind::Market) - 99.9).abs() < 1e-9);
        // Stop fill: multiplied by 1.5 by default -> 100.15
        assert!((s.adjust(Side::Buy, 100.0, 1.0, &b, OrderKind::Stop) - 100.15).abs() < 1e-9);
    }

    #[test]
    fn volume_participation_scales_by_units_over_volume() {
        // coef=1, exp=0.5. units=100, volume=10_000: participation=0.01,
        // impact_frac = sqrt(0.01) = 0.1 -> 10% adverse.
        let s = VolumeParticipationSlippage::new(1.0);
        let b = bar(100.0, 10_000.0);
        assert!((s.adjust(Side::Buy, 100.0, 100.0, &b, OrderKind::Market) - 110.0).abs() < 1e-9);
    }

    #[test]
    fn volume_participation_guards_zero_volume() {
        // Some series don't have volume; a zero-volume bar shouldn't produce
        // NaN/Infinity but a no-slippage fill.
        let s = VolumeParticipationSlippage::new(1.0);
        let b = bar(100.0, 0.0);
        assert_eq!(
            s.adjust(Side::Buy, 100.0, 1.0, &b, OrderKind::Market),
            100.0
        );
    }

    #[test]
    fn volume_participation_stop_multiplier_takes_effect() {
        let s = VolumeParticipationSlippage::new(1.0).with_stop_multiplier(2.0);
        let b = bar(100.0, 10_000.0);
        // Market fill: 10% adverse -> 110.
        assert!((s.adjust(Side::Buy, 100.0, 100.0, &b, OrderKind::Market) - 110.0).abs() < 1e-9);
        // Stop fill: 20% adverse (2x multiplier) -> 120.
        assert!((s.adjust(Side::Buy, 100.0, 100.0, &b, OrderKind::Stop) - 120.0).abs() < 1e-9);
    }

    #[test]
    fn is_none_detects_zero_op_bundles() {
        let a = TradingCosts::none();
        assert!(a.is_none());
        let b = TradingCosts::new(
            Box::new(PercentageCommission::new(0.001)),
            Box::new(NoSpread),
            Box::new(NoSlippage),
        );
        assert!(!b.is_none());
    }
}
