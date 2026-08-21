//! Scaffolding shared by every live venue backend.
//!
//! `live/mod.rs` used to hold nothing but `LiveError`, so each backend grew its
//! own copy of the same helpers. `coinbase.rs` and `okx.rs` ended up with six
//! **byte-identical** free functions between them — most of it exchange-precision
//! arithmetic, which is exactly the code where a bug fixed in one copy and not
//! the other stays invisible until a live order is rejected for a malformed size.
//!
//! Anything a third venue would also need belongs here rather than in whichever
//! backend happened to need it first.
//!
//! | Module | Holds |
//! |---|---|
//! | this one | the exchange-precision arithmetic every venue quantises with |
//! | [`rest`] | [`HttpCore`] — the client, the runtime, the base URL |
//! | [`state`] | the bookkeeping every backend keeps, venue-independent |
//! | [`fills`] | the normalized fill feed and its two dedupe models |

mod fills;
mod rest;
mod state;

pub(in crate::live) use fills::{CursorModel, FillCursor, VenueFill};
pub(in crate::live) use rest::HttpCore;
pub(in crate::live) use state::{Bracket, InstrumentGrid, LiveLog, OrderRegistry, RestingOrder};

use crate::types::Real;

/// Append `params` to `path` as a query string.
pub(super) fn with_query(path: &str, params: &[(&str, String)]) -> String {
    if params.is_empty() {
        return path.to_string();
    }
    let q = params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    format!("{path}?{q}")
}

/// A venue number that may arrive as a JSON string (`"0.5"`), an empty string,
/// or a bare number. Both venues quote numerics as strings, and both use `""`
/// for "not applicable" rather than `null`.
pub(super) fn parse_num(v: &serde_json::Value) -> Option<Real> {
    match v {
        serde_json::Value::String(s) if s.is_empty() => None,
        serde_json::Value::String(s) => s.parse::<Real>().ok(),
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Round a size **down** to a multiple of `step` (so we never submit more than
/// the diff we intend). A non-positive step leaves the value untouched.
pub(super) fn floor_to_step(value: Real, step: Real) -> Real {
    if step <= 0.0 {
        return value;
    }
    (value / step).floor() * step
}

/// Round a price to the nearest multiple of `tick`. A non-positive tick leaves
/// the value untouched.
pub(super) fn round_to_tick(value: Real, tick: Real) -> Real {
    if tick <= 0.0 {
        return value;
    }
    (value / tick).round() * tick
}

/// Format a value with a fixed number of decimals — the string form both venues
/// want for size / price (no scientific notation, matches the product grid).
pub(super) fn format_decimals(value: Real, decimals: usize) -> String {
    format!("{value:.decimals$}")
}

/// Count the significant decimal places in an increment string (`"0.001"` → 3,
/// `"1"` → 0), the precision to format that field to.
pub(super) fn decimals_of(s: &str) -> usize {
    match s.split_once('.') {
        Some((_, frac)) => frac.trim_end_matches('0').len(),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both rounding helpers divide by the increment and multiply back, so the
    /// result carries representation error — `round_to_tick(100.567, 0.01)` is
    /// `100.57000000000001`, not `100.57`. That is fine, because the value is
    /// only ever handed to `format_decimals` before it reaches a venue; but it
    /// means these must be compared with a tolerance, which is what both
    /// backends' copies of this case did.
    #[test]
    fn decimals_and_step_rounding() {
        assert_eq!(decimals_of("0.001"), 3);
        assert_eq!(decimals_of("1"), 0);
        assert_eq!(decimals_of("0.100"), 1);
        assert!((floor_to_step(1.2345, 0.001) - 1.234).abs() < 1e-9);
        assert!((round_to_tick(100.567, 0.01) - 100.57).abs() < 1e-9);
        assert_eq!(format_decimals(1.5, 3), "1.500");
        // The formatted form is what actually goes on the wire.
        assert_eq!(format_decimals(round_to_tick(100.567, 0.01), 2), "100.57");
    }

    #[test]
    fn a_non_positive_increment_is_a_no_op() {
        // A venue that reports no grid for a product must not have its sizes
        // silently zeroed by a division.
        assert_eq!(floor_to_step(1.2345, 0.0), 1.2345);
        assert_eq!(round_to_tick(1.2345, -1.0), 1.2345);
    }

    #[test]
    fn parse_num_reads_both_json_shapes_and_treats_empty_as_absent() {
        use serde_json::json;
        assert_eq!(parse_num(&json!("0.5")), Some(0.5));
        assert_eq!(parse_num(&json!(0.5)), Some(0.5));
        assert_eq!(parse_num(&json!("")), None);
        assert_eq!(parse_num(&json!(null)), None);
    }

    #[test]
    fn with_query_leaves_a_bare_path_alone() {
        assert_eq!(with_query("/x", &[]), "/x");
        assert_eq!(
            with_query("/x", &[("a", "1".into()), ("b", "2".into())]),
            "/x?a=1&b=2"
        );
    }
}
