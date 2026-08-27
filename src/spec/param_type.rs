//! The optional `type:` declaration a `!param` / `!arg` placeholder can carry.
//!
//! A placeholder body is `{ key, default, type }`, and `type:` is the only one
//! of the three that says anything about the *value* rather than about where it
//! comes from. It is **optional**: omitted — or written `type: null` — the
//! placeholder behaves exactly as it always has, and the value's type is
//! whatever the heuristics produced.
//!
//! Those heuristics are real and they are lossy. A `--params NAME=…` term is
//! parsed as JSON with a bare-string fallback ([`params::scalar`]), so
//! `SYM=BTC` is a string but `SYM=123` is a *number* — and a numeric ticker
//! silently stops being a symbol. The same term feeding a `period:` wants the
//! opposite: `FAST=3.5` parses fine and only fails four layers down, inside
//! serde, as an `invalid type` on a `usize`. A `@file.yml` params mapping has
//! YAML's own coercions on top.
//!
//! Declaring the type moves both cases to the load pass, where the message can
//! name the parameter:
//!
//! ```yaml
//! root:   !pick { symbol: !param { key: SYM, type: string } }   # 123 stays "123"
//! period: !param { key: FAST, type: integer }                   # 3.5 is an error
//! ```
//!
//! Four types, chosen to match what a `--params` value can actually be — a
//! scalar. There is deliberately no `list` / `table`: a placeholder standing for
//! a whole subtree (`--params @sizings/atr.yml`) is a *tree* substitution, and
//! the thing that validates it is the typed parse it feeds.
//!
//! [`params::scalar`]: crate::spec::params

use serde_json::Value;

use crate::spec::undefined::RequiredType;

/// The declared type of a placeholder's value.
///
/// Deliberately coarser than Rust's numeric tower and finer than
/// [`RequiredType`]: [`Integer`](Self::Integer) and [`Numeric`](Self::Numeric)
/// are one `RequiredType::Number` as far as *contradiction* goes (no
/// `--params` value can be a number and a string at once, but `20` satisfies
/// both numeric slots), yet they reject different values, which is the point of
/// writing one down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ParamType {
    /// `type: bool` — a YAML/JSON boolean, or the strings `true` / `false`.
    Bool,
    /// `type: integer` — a whole number. `3`, `3.0` and `"3"` all pass; `3.5`
    /// does not.
    Integer,
    /// `type: numeric` — any finite number, integral or not.
    Numeric,
    /// `type: string` — text. A number or bool is *stringified* rather than
    /// rejected, so `--params SYM=123` reaches a `symbol:` slot as `"123"`.
    String,
}

impl ParamType {
    /// Every recognized spelling, in the order an error message lists them.
    pub const ALL: &'static [ParamType] = &[
        ParamType::String,
        ParamType::Numeric,
        ParamType::Bool,
        ParamType::Integer,
    ];

    /// The name written in YAML, and the one printed back at the user.
    pub fn label(self) -> &'static str {
        match self {
            ParamType::Bool => "bool",
            ParamType::Integer => "integer",
            ParamType::Numeric => "numeric",
            ParamType::String => "string",
        }
    }

    /// Parse a `type:` name. `None` for anything unrecognized — the caller
    /// turns that into an error naming the placeholder.
    pub fn from_name(name: &str) -> Option<Self> {
        ParamType::ALL.iter().copied().find(|t| t.label() == name)
    }

    /// The coarse [`RequiredType`] this declaration corresponds to — what
    /// `fugazi check` compares a declaration against when serde, elsewhere in
    /// the document, demands a type of the same placeholder.
    pub fn required(self) -> RequiredType {
        match self {
            ParamType::Bool => RequiredType::Bool,
            ParamType::Integer | ParamType::Numeric => RequiredType::Number,
            ParamType::String => RequiredType::Str,
        }
    }

    /// Coerce a resolved value to this type, or say why it can't be.
    ///
    /// The error is a *predicate phrase* (`is not a whole number`), not a
    /// sentence: the caller knows whether it is talking about a `--params`
    /// parameter or a driver-supplied argument, and prefixes accordingly.
    pub fn coerce(self, value: Value) -> Result<Value, String> {
        match self {
            ParamType::Bool => coerce_bool(value),
            ParamType::Integer => coerce_integer(value),
            ParamType::Numeric => coerce_numeric(value),
            ParamType::String => coerce_string(value),
        }
    }
}

/// How a value reads back in an error message — the JSON spelling, which is
/// what makes `123` and `"123"` distinguishable at the exact moment that
/// distinction is the whole complaint.
///
/// A `--params` value is arbitrary user text, so the elision counts **chars**:
/// slicing bytes would panic mid-codepoint on the one input nobody tests with.
fn shown(value: &Value) -> String {
    /// `text`, cut to `max` chars with an ellipsis if it was longer.
    fn clipped(text: String, max: usize) -> String {
        match text.char_indices().nth(max) {
            Some((cut, _)) => format!("{}…", &text[..cut]),
            None => text,
        }
    }
    match value {
        // Quoted, so a string `"123"` doesn't read as the number the caller is
        // complaining it isn't — and clipped *inside* the quotes, so an elided
        // one still closes them.
        Value::String(s) => format!("{:?}", clipped(s.clone(), 40)),
        other => clipped(other.to_string(), 44),
    }
}

fn coerce_bool(value: Value) -> Result<Value, String> {
    match &value {
        Value::Bool(_) => Ok(value),
        // A params *file* goes through YAML, which already reads `true` as a
        // bool; this arm is for the shapes that don't — a quoted `"true"` in
        // JSON, or a `NAME=true` term whose value someone wrapped in quotes.
        Value::String(s) if s == "true" => Ok(Value::Bool(true)),
        Value::String(s) if s == "false" => Ok(Value::Bool(false)),
        other => Err(format!(
            "is declared `bool`, but {} is not one (write `true` or `false`)",
            shown(other)
        )),
    }
}

fn coerce_integer(value: Value) -> Result<Value, String> {
    let whole = |f: f64| -> Option<Value> {
        (f.is_finite() && f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64)
            .then(|| Value::from(f as i64))
    };
    match &value {
        Value::Number(n) if n.is_i64() || n.is_u64() => Ok(value),
        Value::Number(n) => n
            .as_f64()
            .and_then(whole)
            .ok_or_else(|| not_integer(&value)),
        Value::String(s) => s
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .ok()
            .or_else(|| s.trim().parse::<f64>().ok().and_then(whole))
            .ok_or_else(|| not_integer(&value)),
        other => Err(not_integer(other)),
    }
}

fn not_integer(value: &Value) -> String {
    format!(
        "is declared `integer`, but {} is not a whole number",
        shown(value)
    )
}

fn coerce_numeric(value: Value) -> Result<Value, String> {
    match &value {
        Value::Number(_) => Ok(value),
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(Value::from)
            .ok_or_else(|| not_numeric(&value)),
        other => Err(not_numeric(other)),
    }
}

fn not_numeric(value: &Value) -> String {
    format!(
        "is declared `numeric`, but {} is not a number",
        shown(value)
    )
}

fn coerce_string(value: Value) -> Result<Value, String> {
    match value {
        Value::String(_) => Ok(value),
        // The coercion this type mostly exists for: the `--params` scalar
        // heuristic reads `SYM=123` as a number, and a `symbol:` slot wants
        // the ticker back.
        Value::Number(n) => Ok(Value::String(n.to_string())),
        Value::Bool(b) => Ok(Value::String(b.to_string())),
        other => Err(format!(
            "is declared `string`, but {} is not one",
            shown(&other)
        )),
    }
}

/// Read a placeholder body's `type:` key.
///
/// `None` — the key absent, or explicitly `null` — is *apply the heuristics*,
/// which is what every placeholder written before this existed does. Anything
/// that isn't a recognized name is a document error, not a silent fallback:
/// `type: int` would otherwise mean "untyped", which is the opposite of what
/// was written.
pub fn parse_declaration(value: Option<&Value>) -> Result<Option<ParamType>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) => ParamType::from_name(name).map(Some).ok_or_else(|| {
            format!(
                "has an unknown `type: {name}` — expected one of {}, or null for the default \
                 heuristics",
                names()
            )
        }),
        Some(other) => Err(format!(
            "declares `type: {}`, which has to be a name ({})",
            shown(other),
            names()
        )),
    }
}

fn names() -> String {
    ParamType::ALL
        .iter()
        .map(|t| format!("`{}`", t.label()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_label_round_trips_through_from_name() {
        for &t in ParamType::ALL {
            assert_eq!(ParamType::from_name(t.label()), Some(t), "{}", t.label());
        }
    }

    #[test]
    fn absent_and_null_both_mean_heuristics() {
        assert_eq!(parse_declaration(None), Ok(None));
        assert_eq!(parse_declaration(Some(&Value::Null)), Ok(None));
    }

    #[test]
    fn an_unknown_type_name_is_an_error_not_a_fallback() {
        // `int` is the obvious typo for `integer`, and silently treating it as
        // "untyped" would mean the declaration did nothing.
        let err = parse_declaration(Some(&json!("int"))).unwrap_err();
        assert!(err.contains("has an unknown `type: int`"), "{err}");
        assert!(err.contains("`integer`"), "{err}");
    }

    #[test]
    fn a_non_string_type_is_an_error() {
        let err = parse_declaration(Some(&json!(3))).unwrap_err();
        assert!(err.contains("has to be a name"), "{err}");
    }

    #[test]
    fn string_stringifies_a_scalar_the_heuristic_mistyped() {
        // The case the declaration exists for: `--params SYM=123` parses as a
        // number, and a `symbol:` slot needs the ticker back.
        assert_eq!(ParamType::String.coerce(json!(123)).unwrap(), json!("123"));
        assert_eq!(ParamType::String.coerce(json!(1.5)).unwrap(), json!("1.5"));
        assert_eq!(
            ParamType::String.coerce(json!(true)).unwrap(),
            json!("true")
        );
        assert_eq!(
            ParamType::String.coerce(json!("BTC")).unwrap(),
            json!("BTC")
        );
    }

    #[test]
    fn string_refuses_a_tree() {
        // A subtree param (`--params @file.yml`) is not a scalar, and calling
        // its JSON spelling a "string" would hide the mismatch, not fix it.
        let err = ParamType::String.coerce(json!({"a": 1})).unwrap_err();
        assert!(err.contains("declared `string`"), "{err}");
    }

    #[test]
    fn integer_accepts_whole_numbers_however_spelled() {
        for v in [json!(3), json!(3.0), json!("3"), json!(" 3 ")] {
            assert_eq!(
                ParamType::Integer.coerce(v.clone()).unwrap(),
                json!(3),
                "{v}"
            );
        }
    }

    #[test]
    fn integer_rejects_a_fraction() {
        let err = ParamType::Integer.coerce(json!(3.5)).unwrap_err();
        assert!(err.contains("not a whole number"), "{err}");
        assert!(ParamType::Integer.coerce(json!("3.5")).is_err());
        assert!(ParamType::Integer.coerce(json!(true)).is_err());
    }

    #[test]
    fn numeric_accepts_a_numeric_string_and_rejects_a_word() {
        assert_eq!(ParamType::Numeric.coerce(json!("2.5")).unwrap(), json!(2.5));
        assert_eq!(ParamType::Numeric.coerce(json!(7)).unwrap(), json!(7));
        let err = ParamType::Numeric.coerce(json!("fast")).unwrap_err();
        assert!(err.contains("not a number"), "{err}");
        // `inf` parses as an f64 but is not a value any window length or
        // threshold can use.
        assert!(ParamType::Numeric.coerce(json!("inf")).is_err());
    }

    #[test]
    fn bool_accepts_the_quoted_spellings() {
        assert_eq!(ParamType::Bool.coerce(json!(true)).unwrap(), json!(true));
        assert_eq!(
            ParamType::Bool.coerce(json!("false")).unwrap(),
            json!(false)
        );
        let err = ParamType::Bool.coerce(json!(1)).unwrap_err();
        assert!(err.contains("`true` or `false`"), "{err}");
    }

    #[test]
    fn integer_and_numeric_share_one_coarse_required_type() {
        // What makes `FAST` used as a period and as a threshold not a
        // contradiction under `check`.
        assert_eq!(ParamType::Integer.required(), RequiredType::Number);
        assert_eq!(ParamType::Numeric.required(), RequiredType::Number);
        assert_eq!(ParamType::String.required(), RequiredType::Str);
        assert_eq!(ParamType::Bool.required(), RequiredType::Bool);
    }

    #[test]
    fn a_long_value_is_elided_in_the_message() {
        let long = "x".repeat(200);
        let err = ParamType::Integer.coerce(json!(long)).unwrap_err();
        assert!(err.len() < 120, "{err}");
    }

    #[test]
    fn eliding_a_multibyte_value_does_not_split_a_codepoint() {
        // `--params` values are arbitrary user text. Byte-slicing would panic
        // here, and only here — every ASCII input in the suite would pass.
        for text in ["é".repeat(200), "日本語".repeat(80), "🙂".repeat(60)] {
            let err = ParamType::Numeric.coerce(json!(text)).unwrap_err();
            assert!(err.contains("not a number"), "{err}");
            assert!(err.chars().count() < 100, "{err}");
        }
    }
}
