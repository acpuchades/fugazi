//! The `meta:` contract: every fugazi YAML document accepts a free-form
//! `meta:` key, carries it, hands it back, and is otherwise unchanged by it.
//!
//! The point of these tests is the *negative* half — that a document with
//! `meta:` and the same document without it build the same strategy and produce
//! the same numbers. `meta:` exists so an external service can stash its own
//! record next to a strategy; the moment adding one could move a backtest, the
//! feature is a liability rather than a convenience.
//!
//! Layer: spec/document (see docs/TESTING.md). No network, no fixtures.

mod common;

use std::collections::HashMap;
use std::path::Path;

use common::{bars, cli};
use fugazi::Schema;
use fugazi::spec::StrategySpec;
use fugazi::spec::costs::{CostSpec, config};
use fugazi::spec::input::StrategyKind;
use fugazi::spec::optimize::build_any_spec;
use serde_json::json;

use std::str::FromStr;
use std::sync::Arc;

/// Load a document of `kind` through the real load path (`!import` / `!param`
/// resolution, YAML→JSON bridge, typed parse) into the any-shape handle.
fn load(kind: StrategyKind, yaml: &str) -> StrategySpec {
    let value = fugazi::spec::load_value(
        yaml,
        &HashMap::new(),
        Path::new("."),
        Path::new("."),
        "(test)",
    )
    .expect("document loads");
    build_any_spec(kind, &value, &HashMap::new()).expect("document builds")
}

/// A `meta:` block exercising every JSON shape a service might send: nested
/// maps, lists, strings, numbers, booleans, and an explicit null.
const RICH_META: &str = "\
meta:
  service: strategy-lab
  id: 4f1c-9a2b
  revision: 17
  live: true
  retired_at: ~
  tags: [momentum, crypto]
  owner:
    desk: systematic
    contact: quant@example.invalid
";

/// One document shape: its `kind`, the document *without* `meta:`, and the
/// indentation `meta:` needs to land in the right place.
///
/// The indent is the whole reason this is a struct. Four of the six shapes take
/// `meta:` at column 0, but a **preset** document *is* the tag — `!buy_and_hold
/// { … }` bridges to a single-key `{buy_and_hold: {…}}` map, so a sibling
/// `meta:` would make it a two-key map and stop it being detected as a preset
/// at all. Its `meta:` belongs inside the tag's body, indented under it.
struct Shape {
    name: &'static str,
    kind: StrategyKind,
    body: &'static str,
    meta_indent: usize,
}

fn shapes() -> Vec<Shape> {
    vec![
        Shape {
            name: "single",
            kind: StrategyKind::Single,
            body: "root: BTC\nlong:\n  enter: !gt { lhs: !close, rhs: !sma { period: 2 } }\n",
            meta_indent: 0,
        },
        Shape {
            name: "preset",
            kind: StrategyKind::Single,
            body: "buy_and_hold:\n  root: BTC\n",
            meta_indent: 2,
        },
        Shape {
            name: "pairs",
            kind: StrategyKind::Pairs,
            body: "left: BTC\nright: ETH\nenter: !gt { lhs: !close { source: !pick { symbol: BTC } }, rhs: !value 0 }\n",
            meta_indent: 0,
        },
        Shape {
            name: "basket",
            kind: StrategyKind::Basket,
            body: "selection: !top_bottom { longs: 1, shorts: 1 }\nscore: !close\nsizing: !value 1.0\n",
            meta_indent: 0,
        },
        Shape {
            name: "multi",
            kind: StrategyKind::Multi,
            body: "long:\n  enter: !gt { lhs: !close, rhs: !sma { period: 2 } }\n",
            meta_indent: 0,
        },
        Shape {
            name: "portfolio",
            kind: StrategyKind::Portfolio,
            body: "children:\n  - name: a\n    strategy:\n      root: BTC\n      long:\n        enter: !value true\n",
            meta_indent: 0,
        },
    ]
}

impl Shape {
    /// This shape's document with [`RICH_META`] spliced in at the right depth.
    fn with_meta(&self) -> String {
        let pad = " ".repeat(self.meta_indent);
        let block: String = RICH_META.lines().map(|l| format!("{pad}{l}\n")).collect();
        format!("{}{block}", self.body)
    }
}

/// Every shape parses `meta:` and hands the *same* value back through the
/// any-shape accessor — including the preset spelling, which is the shape most
/// likely to have been forgotten (it is an enum of recipes, not a document
/// struct).
#[test]
fn every_shape_accepts_and_returns_meta() {
    for shape in shapes() {
        let name = shape.name;
        let spec = load(shape.kind, &shape.with_meta());
        let meta = spec
            .meta()
            .unwrap_or_else(|| panic!("{name}: meta is None"));

        assert_eq!(meta["service"], json!("strategy-lab"), "{name}");
        assert_eq!(meta["revision"], json!(17), "{name}");
        assert_eq!(meta["live"], json!(true), "{name}");
        assert_eq!(meta["retired_at"], json!(null), "{name}");
        assert_eq!(meta["tags"], json!(["momentum", "crypto"]), "{name}");
        assert_eq!(meta["owner"]["desk"], json!("systematic"), "{name}");

        // And the same document without it reads `None` — `meta` is genuinely
        // optional, not defaulted to an empty object a caller has to tell apart
        // from a real one.
        assert!(
            load(shape.kind, shape.body).meta().is_none(),
            "{name}: absent meta should read None"
        );
    }
}

/// The load-bearing guarantee: `meta:` cannot move a number. Same document ±
/// `meta:`, driven over the same bars, must produce byte-identical equity.
#[test]
fn meta_does_not_change_a_backtest() {
    let snaps = bars::series(
        "BTC",
        &[100.0, 101.0, 99.0, 104.0, 103.0, 108.0],
        bars::flat,
    );
    let schema = Arc::new(Schema::empty());

    for shape in shapes() {
        let name = shape.name;
        // Pairs needs both legs quoting; skip it here rather than build a
        // second stream — the parse-level equality above already covers it, and
        // this test is about the *driver*, which is shape-agnostic.
        if name == "pairs" {
            continue;
        }
        let plain = load(shape.kind, shape.body)
            .try_build(10_000.0, &schema, None)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let tagged = load(shape.kind, &shape.with_meta())
            .try_build(10_000.0, &schema, None)
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        let a = drive(plain, &snaps);
        let b = drive(tagged, &snaps);
        assert_eq!(a, b, "{name}: meta changed the equity curve");
    }
}

/// Drive a built strategy over `snaps` against a fresh paper wallet and return
/// the equity curve, as the exact bit patterns — a 1-ULP difference is still a
/// difference.
fn drive(
    mut strat: Box<dyn fugazi::spec::RunnableStrategy>,
    snaps: &[fugazi::types::Snapshot<fugazi::types::Symbol>],
) -> Vec<u64> {
    let report = strat.drive(snaps, 10_000.0, &[]);
    report.equity_curve.iter().map(|e| e.to_bits()).collect()
}

/// `meta:` rides the same load pipeline as the rest of the document, so
/// `!import` and `!param` resolve inside it. That is deliberate — a service
/// sharing one metadata block across a family of strategies shouldn't have to
/// inline it — and it is the one way `meta:` is *not* opaque, so it is pinned.
#[test]
fn import_and_param_resolve_inside_meta() {
    let dir = cli::unique_path("meta_import_dir");
    std::fs::create_dir_all(&dir).expect("create dir");
    std::fs::write(
        dir.join("owner.yml"),
        "desk: systematic\ncontact: quant@example.invalid\n",
    )
    .expect("write import");

    let yaml = "root: BTC\n\
                long:\n  enter: !value true\n\
                meta:\n  owner: !import owner.yml\n  revision: !param REV\n";
    let params = HashMap::from([("REV".to_string(), json!(17))]);
    let value = fugazi::spec::load_value(yaml, &params, &dir, &dir, "(test)").expect("loads");
    let spec = build_any_spec(StrategyKind::Single, &value, &HashMap::new()).expect("builds");

    let meta = spec.meta().expect("meta present");
    assert_eq!(meta["owner"]["desk"], json!("systematic"));
    assert_eq!(meta["revision"], json!(17));
}

/// A portfolio child's `meta:` is its own, distinct from the `meta:` of the
/// nested strategy document and from the portfolio's.
#[test]
fn a_portfolio_child_carries_its_own_meta() {
    let spec = load(
        StrategyKind::Portfolio,
        "meta: { level: portfolio }\n\
         children:\n\
         \x20 - name: a\n\
         \x20   meta: { level: slot }\n\
         \x20   strategy:\n\
         \x20     root: BTC\n\
         \x20     meta: { level: nested }\n\
         \x20     long:\n\
         \x20       enter: !value true\n",
    );
    let StrategySpec::Portfolio(p) = &spec else {
        panic!("expected a portfolio");
    };
    assert_eq!(p.meta.as_ref().unwrap()["level"], json!("portfolio"));
    assert_eq!(p.children[0].meta.as_ref().unwrap()["level"], json!("slot"));
}

/// `meta:` widens what parses; it must not widen it to *everything*. A typo'd
/// field is still a hard error — that is the whole reason `meta:` is a named
/// subtree rather than a relaxed `deny_unknown_fields`.
#[test]
fn a_typoed_field_is_still_rejected() {
    let value = fugazi::spec::load_value(
        "root: BTC\nsizng: !value 1.0\nlong:\n  enter: !value true\n",
        &HashMap::new(),
        Path::new("."),
        Path::new("."),
        "(test)",
    )
    .expect("loads as untyped YAML");
    let err = build_any_spec(StrategyKind::Single, &value, &HashMap::new())
        .expect_err("a misspelled field must not be silently ignored");
    let err = format!("{err:#}");
    assert!(
        err.contains("sizng"),
        "the error should name the offending key, got: {err}"
    );
}

/// The costs document takes `meta:` too, and it survives the untyped
/// fold-and-merge pass that turns `--costs` layers into one `CostConfig`.
#[test]
fn a_costs_document_accepts_meta() {
    let (_path, arg) = cli::scratch_file(
        "meta_costs.yml",
        "meta:\n  venue: binance\n  reviewed: 2026-01-31\ncommission: !percentage { rate: 0.001 }\n",
    );

    let spec = CostSpec::from_str(&arg).expect("parses");
    let cfg = config(&[spec]).expect("folds");

    assert_eq!(cfg.meta().expect("meta present")["venue"], json!("binance"));
    // The cost side is untouched: the commission leg still resolves.
    assert!(!cfg.is_none(), "the commission leg should still be set");
}

/// An overlay document is the deliberate exception: it has no envelope — every
/// key *is* a column name — so `meta` there stays an ordinary column, not
/// metadata. Widening what parses is cheap; taking a name away is not.
///
/// Pinned so nobody "completes" the feature by reserving it later and silently
/// dropping someone's column.
#[test]
fn an_overlay_document_treats_meta_as_an_ordinary_column() {
    let cols = fugazi::spec::overlay::columns_from_yaml(
        "meta: !sma { period: 20 }\nrsi14: !rsi { period: 14 }\n",
        &HashMap::new(),
        Path::new("."),
        Path::new("."),
        "(test)",
    )
    .expect("overlay document loads");

    assert_eq!(
        cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["meta", "rsi14"],
        "`meta` must stay a column name in an overlay document"
    );
}

/// The published document JSON Schema must allow `meta:` on every shape —
/// `additionalProperties: false` mirrors `deny_unknown_fields`, so a schema
/// that forgot it would reject documents fugazi accepts.
#[test]
fn the_document_json_schema_allows_meta_on_every_shape() {
    let schema = fugazi::spec::grammar::spec_document_json_schema();
    for shape in ["single", "pairs", "basket", "multi", "portfolio"] {
        assert!(
            schema["$defs"][shape]["properties"].get("meta").is_some(),
            "the {shape} document schema is missing `meta`"
        );
        assert!(
            !schema["$defs"][shape]["required"]
                .as_array()
                .expect("required is a list")
                .iter()
                .any(|r| r == "meta"),
            "`meta` must stay optional on {shape}"
        );
    }
    assert!(
        schema["$defs"]["portfolio_child"]["properties"]
            .get("meta")
            .is_some(),
        "the portfolio child schema is missing `meta`"
    );
}
