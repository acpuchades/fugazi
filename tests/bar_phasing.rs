//! The whole-bar contract of [`fugazi::Wallet::advance`].
//!
//! A bar is not a sequence of independent per-symbol events. Two things in
//! `PaperWallet` are shared across every symbol in a bar — the mark used to
//! value equity, and the single cash balance every buy is shrunk to fit — so
//! pricing the symbols one at a time makes the booked fills depend on the order
//! the caller happened to iterate them in. That order is meaningless: it comes
//! from however the snapshot was assembled (`--series` order in the CLI,
//! Python dict insertion order in the bindings), and the same spec on the same
//! data used to produce different fills through the two.
//!
//! Both failure modes are asserted here directly, at the wallet, plus the
//! end-to-end property through `backtest::run` that the whole thing exists for:
//! **permuting a bar's symbols must not change a single fill**.
//!
//! The strategy layer is deliberately *not* where this is tested. A basket
//! emits its opens and closes in hash-map order and always will; making that
//! order meaningless is the fix, not making it sorted. See
//! `src/wallet/paper.rs::advance`.

mod common;

use common::bars::flat;
use fugazi::backtest;
use fugazi::prelude::*;
use fugazi::types::Snapshot;
use fugazi::wallet::{PaperWallet, Size};
use fugazi::{Candle, Order, Side, Wallet};

/// A candle that opens at `open` and closes somewhere else — the shape that
/// exposes a fill sized off information from later in its own bar.
fn open_close(open: Real, close: Real) -> Candle {
    Candle {
        open,
        high: open.max(close),
        low: open.min(close),
        close,
        volume: 1.0,
    }
}

/// `(symbol, side, units, price)` for each booked fill — enough to compare two
/// runs exactly without depending on order ids, which legitimately differ.
fn blotter(w: &PaperWallet<String>) -> Vec<(String, Side, Real, Real)> {
    w.orders()
        .iter()
        .map(|o: &Order<String>| (o.symbol.clone(), o.side, o.units, o.price))
        .collect()
}

/// Both permutations of a two-symbol bar, as `advance` takes them.
fn permutations(a: (&str, Candle), b: (&str, Candle)) -> [Vec<(String, Candle)>; 2] {
    let ab = vec![(a.0.to_string(), a.1), (b.0.to_string(), b.1)];
    let ba = vec![(b.0.to_string(), b.1), (a.0.to_string(), a.1)];
    [ab, ba]
}

/// Rotating one fully-invested holding into another: the sale funds the buy,
/// whichever order the two symbols arrive in.
///
/// Before the fix the buy was priced against whatever cash happened to be
/// uninvested at that moment. Reaching it before the sale had settled shrank it
/// to a fraction of its target — silently, because `shrink_buy_to_fit` scales
/// toward feasibility rather than refusing, so the blotter recorded a
/// plausible-looking partial fill and no rejection at all.
#[test]
fn rotation_is_funded_by_its_own_sale_in_either_order() {
    let mut fills = Vec::new();
    for order in permutations(("A", flat(100.0)), ("B", flat(100.0))) {
        let mut w: PaperWallet<String> = PaperWallet::new(10_000.0);
        w.advance(&order);
        // Go all-in on A.
        w.set("A".into(), Side::Buy, Size::value_frac(1.0)).unwrap();
        w.advance(&order);
        assert!(
            w.funds().0 < 1.0,
            "test needs a fully-invested book, has {} cash",
            w.funds().0
        );

        // Rotate A out, B in — the exact shape a basket emits on a rebalance.
        w.close("A".into()).unwrap();
        w.set("B".into(), Side::Buy, Size::value_frac(1.0)).unwrap();
        w.advance(&order);

        let b_units = w.position(&"B".to_string()).amount;
        assert!(
            (b_units - 100.0).abs() < 1e-9,
            "B reached {b_units} units, not the full 100 its target weight asks for"
        );
        assert!(
            w.take_rejections().is_empty(),
            "the rotation should not refuse anything"
        );
        fills.push(blotter(&w));
    }
    assert_eq!(fills[0], fills[1], "fills depended on the symbols' order");
}

/// A fill at this bar's `open` must not be sized off another symbol's `close`
/// from the same bar.
///
/// `value_frac` resolves against equity, and equity marks every *other*
/// position at its last fed price. Feeding symbols one at a time made "last
/// fed" mean this bar's close for the symbols already fed and the previous
/// bar's close for the rest — so a buy could be sized against a co-held asset's
/// same-bar close, which is information from later in the bar than the open it
/// trades at.
#[test]
fn sizing_never_reads_another_symbols_same_bar_close() {
    // A rallies hard *within* the fill bar: open 100, close 200.
    let a_fill_bar = open_close(100.0, 200.0);
    let b_fill_bar = flat(100.0);

    let mut sizes = Vec::new();
    for order in permutations(("A", a_fill_bar), ("B", b_fill_bar)) {
        let mut w: PaperWallet<String> = PaperWallet::new(10_000.0);
        let priming = vec![
            ("A".to_string(), flat(100.0)),
            ("B".to_string(), flat(100.0)),
        ];
        w.advance(&priming);
        // 20% into A, leaving ample cash so the shrink cannot mask the effect.
        w.set("A".into(), Side::Buy, Size::value_frac(0.2)).unwrap();
        w.advance(&priming);
        // Queue B for the bar on which A rallies.
        w.set("B".into(), Side::Buy, Size::value_frac(0.3)).unwrap();
        w.advance(&order);
        sizes.push(w.position(&"B".to_string()).amount);
    }

    assert_eq!(
        sizes[0], sizes[1],
        "B's size depended on whether A was marked first"
    );
    // Equity at the open is 8,000 cash + 20 units of A at *its open*, 100 =
    // 10,000. 30% of that is 3,000, which at 100 is 30 units. Marking A at its
    // close instead would value the book at 12,000 and buy 36.
    assert!(
        (sizes[0] - 30.0).abs() < 1e-9,
        "expected 30 units (sized at the open), got {} — 36 means the close leaked in",
        sizes[0]
    );
}

/// Cash contention between two buys resolves by submission order, not by
/// whichever symbol the caller listed first.
///
/// There is no funding-derived answer here — both sides want cash and neither
/// funds the other — so the tie has to break on something the strategy chose.
/// Submission order is that: it is what the strategy expressed and what a venue
/// would honour first-come-first-served.
#[test]
fn contending_buys_break_ties_by_submission_not_by_bar_order() {
    let mut books = Vec::new();
    for order in permutations(("A", flat(100.0)), ("B", flat(100.0))) {
        let mut w: PaperWallet<String> = PaperWallet::new(10_000.0);
        w.advance(&order);
        // Together these ask for 160% of equity; only the first can be filled
        // whole. A is submitted first, so A is the one that gets it.
        w.set("A".into(), Side::Buy, Size::value_frac(0.8)).unwrap();
        w.set("B".into(), Side::Buy, Size::value_frac(0.8)).unwrap();
        w.advance(&order);
        books.push((
            w.position(&"A".to_string()).amount,
            w.position(&"B".to_string()).amount,
        ));
    }
    assert_eq!(books[0], books[1], "contention depended on the bar's order");
    let (a, b) = books[0];
    assert!(
        (a - 80.0).abs() < 1e-9,
        "A submitted first should fill its full 80 units, got {a}"
    );
    assert!(
        b > 0.0 && b < 80.0,
        "B should take what is left, got {b} units"
    );
}

/// Within the protective phase, a crediting exit settles before a debiting one
/// — so a long stopping out funds a short being covered on the same bar,
/// whichever order the two symbols arrive in.
///
/// This is the phase-5 twin of the rotation case. Covering a short is a *buy*:
/// it consumes cash, and against a tight balance it is only affordable once the
/// long's proceeds have landed. Reaching it first refuses it outright
/// (`InsufficientFunds`), which leaves the strategy holding a short it had asked
/// to be out of.
#[test]
fn crediting_protective_exits_settle_before_debiting_ones() {
    let mut fills = Vec::new();
    for order in permutations(("A", flat(100.0)), ("B", flat(100.0))) {
        let mut w: PaperWallet<String> = PaperWallet::new(10_000.0);
        w.advance(&order);
        // Short B (credits cash), then put nearly everything into a long A, so
        // the account ends the setup with a large book and very little cash.
        w.set("B".into(), Side::Sell, Size::units(100.0)).unwrap();
        w.advance(&order);
        w.set("A".into(), Side::Buy, Size::units(190.0)).unwrap();
        w.advance(&order);
        assert!(
            w.funds().0 < 1_500.0,
            "test needs a cash-tight book, has {}",
            w.funds().0
        );

        // Both brackets trigger on the next bar: A's stop sells 190 (credit
        // ~18.8k), B's stop covers 100 (debit ~10.5k). The cover alone does not
        // fit the ~1k on hand.
        w.set_stop("A".into(), Reference(99.0), Size::position_frac(1.0))
            .unwrap();
        w.set_stop("B".into(), Reference(105.0), Size::position_frac(1.0))
            .unwrap();
        let a_bar = Candle {
            open: 100.0,
            high: 100.0,
            low: 98.0,
            close: 98.0,
            volume: 1.0,
        };
        let b_bar = Candle {
            open: 100.0,
            high: 106.0,
            low: 100.0,
            close: 106.0,
            volume: 1.0,
        };
        let bar = if order[0].0 == "A" {
            vec![("A".to_string(), a_bar), ("B".to_string(), b_bar)]
        } else {
            vec![("B".to_string(), b_bar), ("A".to_string(), a_bar)]
        };
        w.advance(&bar);

        assert_eq!(
            w.position(&"A".to_string()).amount,
            0.0,
            "A's stop should have flattened the long"
        );
        assert_eq!(
            w.position(&"B".to_string()).amount,
            0.0,
            "B's stop should have covered the short — it was refused for cash"
        );
        assert!(
            w.take_rejections().is_empty(),
            "neither exit should have been refused"
        );
        fills.push(blotter(&w));
    }
    assert_eq!(fills[0], fills[1], "fills depended on the symbols' order");
}

/// A stop triggering this bar does **not** fund a market order filling this
/// bar, and that is chronology rather than an oversight.
///
/// A queued market order fills at the `open`; a protective leg triggers when
/// the bar later trades through its level. The entry therefore happens first in
/// wall-clock terms and cannot spend proceeds that do not exist yet. Pinned
/// because the phase ordering that guarantees it is easy to "fix" into
/// lookahead.
#[test]
fn a_stop_does_not_fund_a_market_entry_on_the_same_bar() {
    let bar_order = vec![
        ("A".to_string(), flat(100.0)),
        ("B".to_string(), flat(100.0)),
    ];
    let mut w: PaperWallet<String> = PaperWallet::new(10_000.0);
    w.advance(&bar_order);
    w.set("A".into(), Side::Buy, Size::value_frac(1.0)).unwrap();
    w.advance(&bar_order);
    w.set_stop("A".into(), Reference(95.0), Size::position_frac(1.0))
        .unwrap();

    // A trades through its stop on the same bar B's queued buy fills at the open.
    w.set("B".into(), Side::Buy, Size::value_frac(1.0)).unwrap();
    let stop_bar = Candle {
        open: 100.0,
        high: 100.0,
        low: 90.0,
        close: 90.0,
        volume: 1.0,
    };
    w.advance(&[("A".to_string(), stop_bar), ("B".to_string(), flat(100.0))]);

    assert_eq!(
        w.position(&"A".to_string()).amount,
        0.0,
        "the stop should still have flattened A"
    );
    assert_eq!(
        w.position(&"B".to_string()).amount,
        0.0,
        "B's entry at the open had no cash and must not borrow from a stop that \
         had not triggered yet"
    );
}

/// The end-to-end property, through the real driver: a rotating basket-shaped
/// strategy is invariant to the order symbols occupy in the snapshot.
///
/// This is the shape the CLI and the Python bindings disagreed on. They build
/// the same universe into snapshot rows in different orders — `--series` order
/// versus dict insertion order — and nothing downstream is allowed to notice.
#[test]
fn run_is_invariant_to_snapshot_row_order() {
    /// Holds A for two bars, then rotates the whole book into B — fully
    /// invested throughout, which is what makes the buy depend on the sale.
    struct Rotate {
        bar: std::cell::Cell<usize>,
    }
    impl Strategy for Rotate {
        type Input = Snapshot<Symbol>;
        type Symbol = Symbol;
        fn update(&mut self, _snap: Snapshot<Symbol>) {
            self.bar.set(self.bar.get() + 1);
        }
        fn reset(&mut self) {
            self.bar.set(0);
        }
        fn trade(&self, wallet: &mut dyn Wallet<Symbol>) {
            match self.bar.get() {
                1 => {
                    let _ =
                        wallet.set(fugazi::types::symbol("A"), Side::Buy, Size::value_frac(1.0));
                }
                3 => {
                    // Deliberately buy-then-sell: the wrong order for funding,
                    // which is exactly what a hash-ordered basket emits half
                    // the time and what the wallet now has to absorb.
                    let _ =
                        wallet.set(fugazi::types::symbol("B"), Side::Buy, Size::value_frac(1.0));
                    let _ = wallet.close(fugazi::types::symbol("A"));
                }
                _ => {}
            }
        }
    }

    let run_with = |a_first: bool| {
        let snaps: Vec<Snapshot<Symbol>> = (0..6)
            .map(|_| {
                let mut snap = Snapshot::<Symbol>::new();
                let a = (fugazi::types::symbol("A"), Atom::from(flat(100.0)));
                let b = (fugazi::types::symbol("B"), Atom::from(flat(100.0)));
                let (first, second) = if a_first { (a, b) } else { (b, a) };
                snap.push(Some(first.0), None, first.1);
                snap.push(Some(second.0), None, second.1);
                snap
            })
            .collect();
        let mut strategy = Rotate {
            bar: std::cell::Cell::new(0),
        };
        let mut wallet: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
        let report = backtest::run(&mut strategy, &mut wallet, snaps);
        let fills: Vec<_> = report
            .fills
            .iter()
            .map(|f| {
                (
                    f.bar,
                    f.order.symbol.to_string(),
                    f.order.side,
                    f.order.units,
                    f.order.price,
                )
            })
            .collect();
        (fills, report.equity_curve.clone())
    };

    let (fills_ab, curve_ab) = run_with(true);
    let (fills_ba, curve_ba) = run_with(false);

    assert_eq!(fills_ab, fills_ba, "the blotter depended on snapshot order");
    assert_eq!(
        curve_ab, curve_ba,
        "the equity curve depended on snapshot order"
    );
    // And the rotation actually completed: B ends up holding the whole book,
    // not the sliver of residual cash the old interleaving left it.
    let b_buy = fills_ab
        .iter()
        .find(|(_, sym, side, _, _)| sym == "B" && *side == Side::Buy)
        .expect("B should have been bought");
    assert!(
        (b_buy.3 - 100.0).abs() < 1e-9,
        "B filled {} units, not the full 100 the rotation asked for",
        b_buy.3
    );
}
