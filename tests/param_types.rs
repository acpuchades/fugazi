//! Explicit `type:` on a `!param` / `!arg` placeholder, end to end.
//!
//! A placeholder's value used to be typed by whatever heuristic produced it: a
//! `--params NAME=…` term is JSON-parsed with a bare-string fallback, so
//! `SYM=123` is a *number*; a `@file.yml` mapping has YAML's coercions on top.
//! Both are load-time guesses about a user's intent, and neither is recoverable
//! afterwards — a numeric ticker reaches a `symbol:` slot as an `invalid type`
//! from four layers down, and a `FAST=3.5` reaches a `period:` the same way.
//!
//! `type:` is the author's answer to that, and it is **optional**: the tests
//! here pin both halves — what a declaration buys, and that omitting one (or
//! writing `type: null`) leaves every document that predates it behaving
//! exactly as it did.

mod common;

use common::cli::{Cmd, scratch_file};

/// Two bars of a symbol whose name is all digits — the case the string
/// declaration exists for, and not a contrived one: `700` (Tencent) and `005930`
/// (Samsung) are how those tickers are actually spelled.
const NUMERIC_TICKER_BARS: &str = "\
time,symbol,open,high,low,close,volume
2024-01-01T00:00:00Z,700,100,101,99,100,1000
2024-01-02T00:00:00Z,700,100,102,99,101,1000
2024-01-03T00:00:00Z,700,101,103,100,102,1000
";

fn doc(symbol_param: &str) -> String {
    format!(
        "root: !pick {{ symbol: {symbol_param} }}\n\
         long:\n  \
           enter: !gt {{ lhs: !close, rhs: !value 0 }}\n"
    )
}

/// `--params SYM=700` is a number by the time it reaches the document, and
/// `symbol:` is a string. `type: string` is what puts the ticker back.
#[test]
fn a_declared_string_lets_a_numeric_ticker_through() {
    let (_b, bars) = scratch_file("numeric_ticker.csv", NUMERIC_TICKER_BARS);
    let (_d, declared) = scratch_file(
        "numeric_ticker_typed.yml",
        &doc("!param { key: SYM, type: string }"),
    );
    let out = Cmd::new("run")
        .arg(&declared)
        .series(&bars)
        .args(&["--params", "SYM=700", "--crypto", "--quiet"])
        .output_dir("param_types_numeric_ticker")
        .ok();
    // The fill is the proof: the coerced value is what the order routed on.
    assert!(
        out.read("fills.csv").contains(",700,buy,"),
        "the run has to trade the ticker the param named:\n{}",
        out.read("fills.csv")
    );

    // And the same document without the declaration is the failure the
    // declaration exists to remove — pinned so this test can't pass for the
    // wrong reason.
    let (_u, undeclared) = scratch_file("numeric_ticker_plain.yml", &doc("!param { key: SYM }"));
    let out = Cmd::new("run")
        .arg(&undeclared)
        .series(&bars)
        .args(&["--params", "SYM=700", "--crypto", "--quiet"])
        .output_dir("param_types_numeric_ticker_plain")
        .fails();
    assert!(
        out.stderr.contains("expected a string"),
        "expected the type error the declaration removes:\n{}",
        out.stderr
    );
}

/// A fractional period is caught at the load pass, naming the parameter —
/// rather than as serde's `invalid type: floating point 3.5` at whichever
/// `!sma` happened to be parsed first.
#[test]
fn a_declared_integer_refuses_a_fraction_and_names_the_parameter() {
    let (_d, spec) = scratch_file(
        "integer_param.yml",
        "root: BTCUSDT\nlong:\n  enter: !gt\n    lhs: !sma { period: !param { key: FAST, type: integer } }\n    rhs: !value 0\n",
    );
    let out = Cmd::new("check")
        .arg("strategy")
        .arg(&spec)
        .args(&["--params", "FAST=3.5"])
        .fails();
    assert!(
        out.stderr.contains("parameter `FAST`") && out.stderr.contains("not a whole number"),
        "the message has to name the knob and the rule:\n{}",
        out.stderr
    );

    // A whole number spelled as a string — what a `@params.yml` mapping with a
    // quoted value produces — is coerced rather than refused.
    Cmd::new("check")
        .arg("strategy")
        .arg(&spec)
        .args(&["--params", "FAST=\"8\""])
        .ok();
}

/// `check` cannot know a placeholder's value, but a declaration means it can
/// always name its type — and `integer` is sharper than the `number` any
/// position could have demanded.
#[test]
fn check_reports_the_declared_type_of_an_unset_placeholder() {
    let (_d, spec) = scratch_file(
        "declared_hole.yml",
        "root: BTCUSDT\nlong:\n  enter: !gt\n    lhs: !sma { period: !param { key: FAST, type: integer } }\n    rhs: !value 0\n",
    );
    let out = Cmd::new("check").arg("strategy").arg(&spec).ok();
    assert!(
        out.stdout.contains("1 unset placeholder"),
        "the placeholder is still counted:\n{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("needs --params FAST=<integer>"),
        "and reported at its declared type, not the inferred `number`:\n{}",
        out.stdout
    );
}

/// A declaration that disagrees with the position the placeholder sits in is
/// the same defect as a placeholder used at two types in two places, caught one
/// step earlier and attributable to the declaration.
#[test]
fn check_refuses_a_declaration_the_document_contradicts() {
    let (_d, spec) = scratch_file(
        "contradicted_declaration.yml",
        "root: BTCUSDT\nlong:\n  enter: !gt\n    lhs: !sma { period: !param { key: FAST, type: string } }\n    rhs: !value 0\n",
    );
    let out = Cmd::new("check").arg("strategy").arg(&spec).fails();
    assert!(
        out.stderr
            .contains("`FAST` is declared `string` but used where a number is required"),
        "expected the declaration to be named as the contradiction:\n{}",
        out.stderr
    );
}

/// The typo guard. A body key set that tolerated `typ:` would make the
/// declaration silently mean nothing — the one failure mode a *declaration*
/// feature cannot have.
#[test]
fn a_misspelled_body_key_or_type_name_is_refused() {
    for (name, body, want) in [
        (
            "key",
            "!param { key: FAST, typ: integer }",
            "unknown key `typ`",
        ),
        (
            "type",
            "!param { key: FAST, type: int }",
            "has an unknown `type: int`",
        ),
    ] {
        let (_d, spec) = scratch_file(
            &format!("bad_placeholder_{name}.yml"),
            &format!(
                "root: BTCUSDT\nlong:\n  enter: !gt\n    lhs: !sma {{ period: {body} }}\n    rhs: !value 0\n"
            ),
        );
        let out = Cmd::new("check")
            .arg("strategy")
            .arg(&spec)
            .args(&["--params", "FAST=3"])
            .fails();
        assert!(
            out.stderr.contains(want),
            "{name}: expected `{want}`:\n{}",
            out.stderr
        );
    }
}

/// The compatibility half: no `type:`, or an explicit `type: null`, and the
/// value keeps whatever the heuristics gave it. `SYM=700` stays a *number*, so
/// the document fails exactly as it did before this key existed.
#[test]
fn an_omitted_or_null_type_leaves_the_heuristics_in_charge() {
    let (_b, bars) = scratch_file("null_type.csv", NUMERIC_TICKER_BARS);
    for (name, placeholder) in [
        ("omitted", "!param { key: SYM }"),
        ("null", "!param { key: SYM, type: null }"),
    ] {
        let (_d, spec) = scratch_file(&format!("null_type_{name}.yml"), &doc(placeholder));
        let out = Cmd::new("run")
            .arg(&spec)
            .series(&bars)
            .args(&["--params", "SYM=700", "--crypto", "--quiet"])
            .output_dir(&format!("param_types_null_{name}"))
            .fails();
        assert!(
            out.stderr.contains("expected a string"),
            "{name}: the heuristic must still be what decides:\n{}",
            out.stderr
        );
        // …and a string value goes through untouched, so the untyped
        // placeholder is not simply broken.
        Cmd::new("run")
            .arg(&spec)
            .series(&bars)
            .args(&["--params", "SYM=\"700\"", "--crypto", "--quiet"])
            .output_dir(&format!("param_types_null_{name}_ok"))
            .ok();
    }
}

/// `!arg`'s body is parsed by the same code, so the declaration works inside a
/// deferred template too — where it bites at *build* time, once the driver has
/// bound the name.
#[test]
fn a_basket_template_can_declare_its_arg_type() {
    let bars = "\
time,symbol,open,high,low,close,volume
2024-01-01T00:00:00Z,AAA,100,101,99,100,1000
2024-01-01T00:00:00Z,BBB,50,51,49,50,1000
2024-01-02T00:00:00Z,AAA,100,102,99,101,1000
2024-01-02T00:00:00Z,BBB,50,52,49,51,1000
2024-01-03T00:00:00Z,AAA,101,103,100,102,1000
2024-01-03T00:00:00Z,BBB,51,53,50,52,1000
";
    let (_b, series) = scratch_file("arg_type.csv", bars);
    let template = |ty: &str| {
        format!(
            "score: !close {{ source: !pick {{ symbol: !arg {{ key: SYM, type: {ty} }} }} }}\n\
             selection: !top_bottom {{ longs: 1, shorts: 0 }}\n\
             sizing: !equal_weight 2\n"
        )
    };
    let (_d, spec) = scratch_file("arg_type.yml", &template("string"));
    let out = Cmd::new("run")
        .arg(&format!("basket:{spec}"))
        .series(&series)
        .args(&["--crypto", "--quiet"])
        .output_dir("param_types_arg")
        .ok();
    assert!(
        out.read("fills.csv").contains(",AAA,buy,"),
        "the basket has to run and pick a leg:\n{}",
        out.read("fills.csv")
    );

    // And a declaration the driver's binding can't satisfy is caught by the
    // build-time probe every template already goes through, rather than on the
    // bar that first discovers a symbol.
    let (_d, bad) = scratch_file("arg_type_bad.yml", &template("numeric"));
    let out = Cmd::new("run")
        .arg(&format!("basket:{bad}"))
        .series(&series)
        .args(&["--crypto", "--quiet"])
        .output_dir("param_types_arg_bad")
        .fails();
    assert!(
        out.stderr.contains("argument `SYM` is declared `numeric`"),
        "expected the probe to name the declaration:\n{}",
        out.stderr
    );
}
