//! Cross-symbol references resolve — in overlay columns and in a single-asset
//! strategy spec — because a `source:`-omitted leaf reads the *blessed series*
//! of whatever context built it, not "the sole entry in the snapshot".
//!
//! These need **two symbols in one snapshot** to mean anything: the bug they
//! guard (a `!pick { symbol: B }` reading `None` on every bar while the column
//! header sits there looking like a warm-up) is invisible to any single-symbol
//! fixture, which is how it shipped.
//!
//! Every expected value below is arithmetic on the literal closes in
//! [`stream`], so a wrong answer is a wrong number rather than a missing one.

use fugazi::types::{Symbol, symbol as intern};
use std::collections::HashMap;
use std::sync::Arc;

use fugazi::spec::SingleStrategySpec;
use fugazi::spec::overlay::{self, OverlayColumn};
use fugazi::{Atom, Candle, PaperWallet, Real, Schema, Snapshot, Timestamp};

const DAY_MS: i64 = 86_400_000;

/// Closes for two symbols over six aligned daily bars.
///
/// `A` rises 10 → 60, `B` falls 100 → 50. Chosen so every cross-symbol
/// assertion has a distinct, memorable expected value and the two series can
/// never be confused for one another.
const A_CLOSES: [Real; 6] = [10.0, 20.0, 30.0, 40.0, 50.0, 60.0];
const B_CLOSES: [Real; 6] = [100.0, 90.0, 80.0, 70.0, 60.0, 50.0];

fn bar(close: Real) -> Candle {
    // Flat OHLC: `open == close` so a market order filling at the next bar's
    // open fills at that bar's close, which keeps fill-price arithmetic
    // checkable by eye.
    Candle::new(close, close, close, close, 1_000.0)
}

/// Six two-entry snapshots, `A` then `B`, one per day.
fn stream() -> Vec<Snapshot<Symbol>> {
    (0..6)
        .map(|i| {
            let t = Timestamp(i as i64 * DAY_MS);
            let mut s = Snapshot::<Symbol>::new();
            s.push(
                Some(intern("A")),
                None,
                Atom::with_time(bar(A_CLOSES[i]), t),
            );
            s.push(
                Some(intern("B")),
                None,
                Atom::with_time(bar(B_CLOSES[i]), t),
            );
            s
        })
        .collect()
}

fn column(name: &str, yaml: &str) -> OverlayColumn {
    let cols = overlay::columns_from_yaml(
        &format!("{name}: {yaml}"),
        &HashMap::new(),
        std::path::Path::new("."),
        std::path::Path::new("."),
        "(test)",
    )
    .expect("overlay parses");
    cols.into_iter().next().expect("one column")
}

/// Read column `name` off every entry tagged `symbol`, in bar order.
fn read(
    snaps: &[Snapshot<Symbol>],
    schema: &Arc<Schema>,
    symbol: &str,
    name: &str,
) -> Vec<Option<Real>> {
    let idx = schema.index_of(name).expect("column registered");
    snaps
        .iter()
        .map(|s| {
            let atom = s
                .iter()
                .find(|(sym, _, _)| sym.map(|s| s.as_ref()) == Some(symbol))
                .map(|(_, _, a)| a)
                .expect("symbol present in snapshot");
            atom.overlays.as_ref().and_then(|ov| ov.get_real(idx))
        })
        .collect()
}

#[test]
fn overlay_reads_its_own_series_and_another_in_the_same_column_set() {
    let cols = vec![
        // No `source:` — the blessed series, i.e. whichever symbol this
        // instantiation is for.
        column("own", "!close"),
        // Explicitly another symbol. This is the column that used to be
        // empty on every row.
        column("b_close", "!close { source: !pick { symbol: B } }"),
        // Both readings in one expression: own close minus B's close.
        column(
            "spread",
            "!sub { lhs: !close, rhs: !close { source: !pick { symbol: B } } }",
        ),
    ];

    let (schema, out) =
        overlay::compute_snapshots(&Schema::empty(), &cols, &stream()).expect("overlays build");

    // `own` tracks each series' own bar — the pre-existing behaviour, which
    // must not regress now that the snapshot carries both symbols.
    assert_eq!(
        read(&out, &schema, "A", "own"),
        A_CLOSES.iter().copied().map(Some).collect::<Vec<_>>(),
    );
    assert_eq!(
        read(&out, &schema, "B", "own"),
        B_CLOSES.iter().copied().map(Some).collect::<Vec<_>>(),
    );

    // `b_close` is B's close on *both* series' rows — that's the whole point.
    let b_expected: Vec<Option<Real>> = B_CLOSES.iter().copied().map(Some).collect();
    assert_eq!(read(&out, &schema, "A", "b_close"), b_expected);
    assert_eq!(read(&out, &schema, "B", "b_close"), b_expected);

    // A's spread: 10-100, 20-90, … B's is identically zero.
    assert_eq!(
        read(&out, &schema, "A", "spread"),
        vec![
            Some(-90.0),
            Some(-70.0),
            Some(-50.0),
            Some(-30.0),
            Some(-10.0),
            Some(10.0)
        ],
    );
    assert_eq!(read(&out, &schema, "B", "spread"), vec![Some(0.0); 6]);
}

#[test]
fn overlay_indicator_state_stays_per_series() {
    // Each series gets its own indicator set, so a rolling window over the
    // blessed leaf must see only that series' bars — never an interleaving of
    // both, which is the obvious way a shared-snapshot rewrite could go wrong.
    let cols = vec![column("sma2", "!sma { period: 2 }")];
    let (schema, out) =
        overlay::compute_snapshots(&Schema::empty(), &cols, &stream()).expect("overlays build");

    // A: (10+20)/2, (20+30)/2, … — not (10+100)/2 or similar.
    assert_eq!(
        read(&out, &schema, "A", "sma2"),
        vec![
            None,
            Some(15.0),
            Some(25.0),
            Some(35.0),
            Some(45.0),
            Some(55.0)
        ],
    );
    assert_eq!(
        read(&out, &schema, "B", "sma2"),
        vec![
            None,
            Some(95.0),
            Some(85.0),
            Some(75.0),
            Some(65.0),
            Some(55.0)
        ],
    );
}

/// The `corr_to_spy` shape from the real overlay files: this series' returns
/// against another symbol's, over a rolling window.
const CORR: &str = "!correlation { lhs: !roc { period: 1 }, \
      rhs: !roc { period: 1, source: !close { source: !pick { symbol: B } } }, \
      period: 4 }";

#[test]
fn overlay_cross_symbol_correlation_matches_a_hand_computation() {
    // The strongest available check that the cross-symbol wiring produces
    // *correct arithmetic* and not merely non-`None` output: assert the exact
    // Pearson r, derived independently from the literal closes.
    //
    // Over the last 4 bars, A's returns are 0.5, 1/3, 0.25, 0.2 and B's are
    // -1/9, -0.125, -1/7, -1/6. Note both series' returns *decrease* — A rises
    // by a constant absolute step so its returns shrink, B falls by one so its
    // returns deepen — which makes r strongly **positive**, not negative, even
    // though the prices move in opposite directions.
    //
    //   mean A = 0.32083…      mean B = -0.13641…
    //   cov    = 0.00878802…   var A  = 0.051875   var B = 0.00172726…
    //   r      = cov / sqrt(varA * varB) = 0.928398442524347
    let cols = vec![column("corr", CORR)];
    let (schema, out) =
        overlay::compute_snapshots(&Schema::empty(), &cols, &stream()).expect("overlays build");

    let last = read(&out, &schema, "A", "corr")
        .last()
        .copied()
        .flatten()
        .expect("correlation warmed up");
    assert!(
        (last - 0.928_398_442_524_347).abs() < 1e-12,
        "cross-symbol correlation must match the hand computation, got {last}",
    );

    // B against B is +1 exactly — the free self-consistency check, and the
    // same one SPY's own rows give you in the real dataset.
    let self_corr = read(&out, &schema, "B", "corr")
        .last()
        .copied()
        .flatten()
        .expect("correlation warmed up");
    assert!(
        (self_corr - 1.0).abs() < 1e-12,
        "a series correlated with itself must read +1, got {self_corr}",
    );
}

#[test]
fn overlay_cross_symbol_correlation_reaches_minus_one_when_returns_oppose() {
    // Prices that genuinely anti-correlate *in returns*: A alternates
    // ×2, ÷2 while B does the mirror, so A's return series is
    // [+1, -0.5, +1, -0.5, +1] and B's is [-0.5, +1, -0.5, +1, -0.5].
    // B = -A + 0.5 exactly — an affine map with negative slope — so Pearson r
    // is -1 to floating point over any window.
    let a = [100.0, 200.0, 100.0, 200.0, 100.0, 200.0];
    let b = [200.0, 100.0, 200.0, 100.0, 200.0, 100.0];
    let snaps: Vec<Snapshot<Symbol>> = (0..6)
        .map(|i| {
            let t = Timestamp(i as i64 * DAY_MS);
            let mut s = Snapshot::<Symbol>::new();
            s.push(Some(intern("A")), None, Atom::with_time(bar(a[i]), t));
            s.push(Some(intern("B")), None, Atom::with_time(bar(b[i]), t));
            s
        })
        .collect();

    let cols = vec![column("corr", CORR)];
    let (schema, out) =
        overlay::compute_snapshots(&Schema::empty(), &cols, &snaps).expect("overlays build");
    let last = read(&out, &schema, "A", "corr")
        .last()
        .copied()
        .flatten()
        .expect("correlation warmed up");
    assert!(
        (last + 1.0).abs() < 1e-12,
        "opposing return series must read r = -1, got {last}",
    );
}

#[test]
fn overlay_pick_of_an_absent_symbol_reads_none_rather_than_another_series() {
    // The failure mode the empty-column warning exists for: a typo'd symbol
    // must stay empty, not silently fall through to whatever else is in the
    // snapshot.
    let cols = vec![column("typo", "!close { source: !pick { symbol: NOPE } }")];
    let (schema, out) =
        overlay::compute_snapshots(&Schema::empty(), &cols, &stream()).expect("overlays build");
    assert_eq!(read(&out, &schema, "A", "typo"), vec![None; 6]);
}

#[test]
fn single_series_untagged_input_still_resolves() {
    // `Vec<Candle>` drivers produce untagged size-1 snapshots. `Pick::rooted`
    // falls back to the lone atom for exactly this case; without the fallback
    // a rooted leaf would find nothing and the column would go empty.
    let atoms: Vec<Atom> = A_CLOSES.iter().map(|&c| Atom::new(bar(c))).collect();
    let cols = vec![column("own", "!close")];
    let (schema, mut prepared) = overlay::prepare(&Schema::empty(), &cols).expect("builds");
    let out = overlay::compute_series(None, &atoms, &schema, 0, &mut prepared);

    let idx = schema.index_of("own").unwrap();
    let got: Vec<Option<Real>> = out
        .iter()
        .map(|a| a.overlays.as_ref().and_then(|ov| ov.get_real(idx)))
        .collect();
    assert_eq!(got, A_CLOSES.iter().copied().map(Some).collect::<Vec<_>>());
}

#[test]
fn overlay_build_failure_is_an_error_naming_the_column_and_document() {
    // `!get` against a stream with no side channel used to abort the process
    // with a stack trace. It must come back as an ordinary error, and say
    // which column of which document is at fault.
    let cols = vec![column(
        "n_trades_ma",
        "!sma { source: !get { key: n_trades }, period: 3 }",
    )];
    let err = overlay::compute_snapshots(&Schema::empty(), &cols, &stream())
        .expect_err("unknown !get key must not build");
    let msg = format!("{err:#}");
    assert!(msg.contains("n_trades_ma"), "names the column: {msg}");
    assert!(msg.contains("(test)"), "names the source document: {msg}");
    assert!(
        msg.contains("no overlay side channel"),
        "keeps the diagnosis: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Single-asset strategy
// ---------------------------------------------------------------------------

/// Build and run a single-asset spec over [`stream`], returning the fills.
fn run_spec(yaml: &str) -> Vec<fugazi::Fill<Symbol>> {
    let spec = SingleStrategySpec::from_text_with_params_in(
        yaml,
        &HashMap::new(),
        std::path::Path::new("."),
        std::path::Path::new("."),
        "(test)",
    )
    .expect("spec parses");
    let mut strat = spec.build(1_000.0, &Schema::empty());
    let mut wallet = PaperWallet::<Symbol>::new(1_000.0);
    fugazi::backtest::run(&mut strat, &mut wallet, stream()).fills
}

#[test]
fn single_strategy_bare_leaf_reads_its_declared_symbol_in_a_multi_symbol_frame() {
    // `symbol: A` with a bare `!close`. Before the blessed root this tripped
    // `sole_atom_or_panic`'s panic, because the frame carries two symbols; now the
    // declared symbol means the same thing for signals as it does for trading.
    //
    // A's closes are 10,20,30,40,50,60 — `!close > 35` first holds on bar 3
    // (close 40), so the market order fills at bar 4's open, which is 50.
    let fills =
        run_spec("root: A\nlong:\n  enter: !gt { lhs: !close, rhs: !value 35 }\n  exit: !never\n");
    assert_eq!(fills.len(), 1, "expected one entry fill, got {fills:?}");
    assert_eq!(fills[0].order.symbol.as_ref(), "A");
    assert_eq!(fills[0].bar, 4);
    assert_eq!(fills[0].order.price, 50.0);
}

#[test]
fn single_strategy_enters_on_another_symbols_price() {
    // Trades A, but the trigger reads B: enter when B's close drops below 75.
    // B is 100,90,80,70,… so that first holds on bar 3, filling at bar 4's
    // open — and the fill must be in A (50.0), not B (60.0).
    let fills = run_spec(
        "root: A\nlong:\n  enter: !lt { lhs: !close { source: !pick { symbol: B } }, rhs: !value 75 }\n  exit: !never\n",
    );
    assert_eq!(fills.len(), 1, "expected one entry fill, got {fills:?}");
    assert_eq!(
        fills[0].order.symbol.as_ref(),
        "A",
        "must trade A, not the symbol it read"
    );
    assert_eq!(fills[0].bar, 4);
    assert_eq!(
        fills[0].order.price, 50.0,
        "filled at A's bar-4 open, not B's",
    );
}

#[test]
fn single_strategy_mixes_its_own_price_with_another_symbols() {
    // Enter A once A's close exceeds B's — the crossover is between bars 4
    // (50 vs 60, false) and 5 (60 vs 50, true), so the signal first holds on
    // bar 5. That's the last bar, so there is no next open to fill at and the
    // order never books: proof the comparison is reading two *different*
    // series rather than one against itself (which would be false throughout,
    // or true from bar 0).
    let fills = run_spec(
        "root: A\nlong:\n  enter: !gt { lhs: !close, rhs: !close { source: !pick { symbol: B } } }\n  exit: !never\n",
    );
    assert!(
        fills.is_empty(),
        "signal fires on the final bar, so nothing fills: {fills:?}"
    );

    // Same comparison one bar earlier in effect: A's close over B's *lagged*
    // close. Lagged B is -,100,90,80,70,60; A first exceeds it on bar 5
    // (60 > 60 is false — so still nothing). Shift the threshold instead:
    // A > B - 15 first holds on bar 4 (50 > 45), filling at bar 5's open = 60.
    let fills = run_spec(
        "root: A\nlong:\n  enter: !gt { lhs: !close, rhs: !sub { lhs: !close { source: !pick { symbol: B } }, rhs: !value 15 } }\n  exit: !never\n",
    );
    assert_eq!(fills.len(), 1, "expected one entry fill, got {fills:?}");
    assert_eq!(fills[0].bar, 5);
    assert_eq!(fills[0].order.price, 60.0);
}
