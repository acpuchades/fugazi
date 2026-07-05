//! End-to-end tests for `fugazi run` batch mode — multi-`(symbol, freq)`
//! frames driven by a `%SYMBOL`-templated strategy through the CLI binary.
//! Focus is on the *shape* of the output (which files exist, which columns
//! are present, how many rows) rather than exact numeric values.

use std::io::Write;
use std::path::Path;
use std::process::Command;

/// Write a tiny CSV `contents` to a fresh path under `dir` and return its
/// stringified path (the format `--series @path` accepts).
fn write_csv(dir: &Path, name: &str, contents: &str) -> String {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).expect("creating CSV fixture");
    f.write_all(contents.as_bytes()).expect("writing CSV fixture");
    path.to_string_lossy().into_owned()
}

/// A candles fixture covering two symbols on a shared daily timeline —
/// enough to force at least one crossover per symbol under a short EMA
/// pair, so both iterations produce fills.
fn two_symbol_daily_csv() -> String {
    let mut rows = String::from("symbol;time;open;high;low;close;volume\n");
    let btc = [100.0, 102.0, 104.0, 103.0, 105.0, 108.0, 110.0, 112.0, 111.0, 113.0];
    let aapl = [20.0, 21.0, 20.0, 22.0, 21.0, 23.0, 24.0, 23.0, 25.0, 26.0];
    for (i, price) in btc.iter().enumerate() {
        rows.push_str(&format!(
            "BTC;2024-01-{:02};{price};{price};{price};{price};1000\n",
            i + 1
        ));
    }
    for (i, price) in aapl.iter().enumerate() {
        rows.push_str(&format!(
            "AAPL;2024-01-{:02};{price};{price};{price};{price};500\n",
            i + 1
        ));
    }
    rows
}

/// A `%SYMBOL`-templated MA-crossover strategy that iterates cleanly per
/// group when the CLI substitutes the sigil into `--params SYMBOL=%SYMBOL`.
const CROSSOVER_STRATEGY: &str = "\
symbol: !param SYMBOL
long:
  enter: !crosses_above
    lhs: !ema { source: close, period: 2 }
    rhs: !ema { source: close, period: 4 }
  exit: !crosses_below
    lhs: !ema { source: close, period: 2 }
    rhs: !ema { source: close, period: 4 }
";

/// Drive the CLI with the given args and return `(exit_success, stderr)`.
fn cli(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args(args)
        .output()
        .expect("failed to launch fugazi");
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn batch_multi_symbol_aggregates_into_one_output_dir() {
    // Two symbols in one frame + `--output-dir out/` (no sigil) → all
    // iterations bucket together; loose `symbol` column appears on every
    // CSV; `metrics.csv` has one row per iteration.
    let tmp = tempdir();
    let candles = write_csv(&tmp, "candles.csv", &two_symbol_daily_csv());
    let strategy = write_csv(&tmp, "strategy.yml", CROSSOVER_STRATEGY);
    let out = tmp.join("out");

    let (ok, err) = cli(&[
        "run",
        &format!("@{}", strategy),
        "--series",
        &format!("@{}", candles),
        "--params",
        "SYMBOL=%SYMBOL",
        "--output-dir",
        out.to_str().unwrap(),
        "--quiet",
    ]);
    assert!(ok, "batch run should succeed; stderr:\n{err}");

    // No `metrics.yml` — multiple iterations, so shape flips to CSV.
    assert!(!out.join("metrics.yml").exists());
    let metrics = std::fs::read_to_string(out.join("metrics.csv")).expect("metrics.csv");
    let header = metrics.lines().next().expect("metrics.csv header");
    assert!(header.starts_with("symbol;"), "loose symbol column: `{header}`");
    // Two data rows (one per symbol), sorted alphabetically → AAPL first, BTC second.
    let rows: Vec<&str> = metrics.lines().skip(1).collect();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].starts_with("AAPL;"), "first row is AAPL: `{}`", rows[0]);
    assert!(rows[1].starts_with("BTC;"), "second row is BTC: `{}`", rows[1]);

    // `trades.csv` carries the leading `symbol` column and drops the
    // intra-CSV `symbol` (SingleAssetStrategy always trades its own).
    let trades = std::fs::read_to_string(out.join("trades.csv")).expect("trades.csv");
    let th = trades.lines().next().expect("trades.csv header");
    assert!(th.starts_with("symbol;time;"), "trades header: `{th}`");
    assert!(!th.contains(";symbol;"), "no duplicate symbol column: `{th}`");
    // At least one fill row per symbol under a 2-vs-4 EMA crossover.
    assert!(trades.lines().any(|l| l.starts_with("AAPL;")));
    assert!(trades.lines().any(|l| l.starts_with("BTC;")));
}

#[test]
fn batch_symbol_in_output_dir_produces_per_symbol_subdirs() {
    // `--output-dir out/%SYMBOL/` → each symbol's iteration lands in its
    // own subdir with today's per-subdir file names (metrics.yml, since
    // each bucket has exactly one iteration).
    let tmp = tempdir();
    let candles = write_csv(&tmp, "candles.csv", &two_symbol_daily_csv());
    let strategy = write_csv(&tmp, "strategy.yml", CROSSOVER_STRATEGY);
    let out = tmp.join("out");

    let (ok, err) = cli(&[
        "run",
        &format!("@{}", strategy),
        "--series",
        &format!("@{}", candles),
        "--params",
        "SYMBOL=%SYMBOL",
        "--output-dir",
        &format!("{}/%SYMBOL", out.display()),
        "--quiet",
    ]);
    assert!(ok, "batch run should succeed; stderr:\n{err}");

    for symbol in ["BTC", "AAPL"] {
        let sub = out.join(symbol);
        assert!(sub.exists(), "per-symbol subdir `{}` missing", sub.display());
        assert!(sub.join("metrics.yml").exists(), "{symbol}/metrics.yml missing");
        assert!(sub.join("trades.csv").exists(), "{symbol}/trades.csv missing");
        assert!(sub.join("returns.csv").exists(), "{symbol}/returns.csv missing");
        // No loose columns — bucket has one iteration.
        let trades = std::fs::read_to_string(sub.join("trades.csv")).unwrap();
        let th = trades.lines().next().unwrap();
        assert_eq!(th, "time;symbol;side;units;price;kind");
    }
}

#[test]
fn sigils_resolve_in_single_group_frame() {
    // A one-symbol frame + `%SYMBOL` in --output-dir/--params should still
    // interpolate. Sigil substitution is a `--single`-mode feature, not a
    // batch-mode-only one — the strategy templated on `SYMBOL=%SYMBOL`
    // must work whether the frame carries one series or many.
    let tmp = tempdir();
    let only_btc: String = two_symbol_daily_csv()
        .lines()
        .filter(|l| !l.starts_with("AAPL"))
        .collect::<Vec<_>>()
        .join("\n");
    let candles = write_csv(&tmp, "candles.csv", &only_btc);
    let strategy = write_csv(&tmp, "strategy.yml", CROSSOVER_STRATEGY);
    let out_root = tmp.join("out");
    let out_pattern = format!("{}/%SYMBOL/%FREQ", out_root.display());

    let (ok, err) = cli(&[
        "run",
        &format!("@{}", strategy),
        "--series",
        &format!("@{}", candles),
        "--params",
        "SYMBOL=%SYMBOL",
        "--output-dir",
        &out_pattern,
        "--quiet",
    ]);
    assert!(ok, "single-group run should succeed; stderr:\n{err}");

    // %SYMBOL → BTC; %FREQ → 1d (auto-detected).
    let expected = out_root.join("BTC").join("1d");
    assert!(expected.join("metrics.yml").exists(), "metrics.yml at {}", expected.display());
    assert!(expected.join("trades.csv").exists());
    assert!(expected.join("returns.csv").exists());
}

#[test]
fn multiple_flag_is_rejected() {
    // Reserved for a future MultiAssetStrategy — must fail cleanly.
    let tmp = tempdir();
    let candles = write_csv(&tmp, "candles.csv", &two_symbol_daily_csv());
    let strategy = write_csv(&tmp, "strategy.yml", CROSSOVER_STRATEGY);

    let (ok, err) = cli(&[
        "run",
        &format!("@{}", strategy),
        "--series",
        &format!("@{}", candles),
        "--params",
        "SYMBOL=%SYMBOL",
        "--multiple",
        "--output-dir",
        tmp.join("out").to_str().unwrap(),
        "--quiet",
    ]);
    assert!(!ok, "--multiple should fail until MultiAssetStrategy exists");
    assert!(
        err.contains("--multiple") && err.contains("not yet implemented"),
        "error message names the feature and status; got:\n{err}",
    );
}

#[test]
fn percent_prefixed_param_names_are_rejected() {
    // `%SYMBOL` and `%FREQ` are a reserved namespace — the user can *use*
    // them in values, not declare their own `%FOO`.
    let tmp = tempdir();
    let candles = write_csv(&tmp, "candles.csv", &two_symbol_daily_csv());
    let strategy = write_csv(&tmp, "strategy.yml", CROSSOVER_STRATEGY);

    let (ok, err) = cli(&[
        "run",
        &format!("@{}", strategy),
        "--series",
        &format!("@{}", candles),
        "--params",
        "%FOO=1",
        "--output-dir",
        tmp.join("out").to_str().unwrap(),
        "--quiet",
    ]);
    assert!(!ok, "--params %FOO=1 should fail");
    assert!(
        err.contains("reserved") || err.contains("%"),
        "error names the reserved namespace; got:\n{err}",
    );
}

/// Create a fresh scratch dir for one test. Auto-cleaned via a random suffix
/// under `std::env::temp_dir()` — we don't do RAII since the fixture files
/// are small and tests are idempotent.
fn tempdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("fugazi_batch_{pid}_{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creating scratch dir");
    dir
}
