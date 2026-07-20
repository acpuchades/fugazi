//! Bridge a parsed YAML document into a `serde_json::Value`.
//!
//! The strategy spec is parsed from YAML (using `!tags` for enum variants). To run
//! the param-substitution pass and the final deserialize over an untyped tree, the
//! `serde_norway::Value` is normalized to a `serde_json::Value` here. The one
//! interesting case is a YAML tag, which becomes serde_json's external-tag spelling
//! — the singleton object `{tag: value}` — so `serde_json::from_value` reads `!sma`
//! and `{"sma": …}` identically. (JSON is a subset of YAML, so a JSON-shaped
//! document parses through this same path with no special handling.)

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value as Json};
use serde_norway::Value as Yaml;

/// Convert a YAML value into the equivalent JSON value.
pub fn yaml_to_json(value: Yaml) -> Result<Json> {
    Ok(match value {
        Yaml::Null => Json::Null,
        Yaml::Bool(b) => Json::Bool(b),
        Yaml::Number(n) => number(n)?,
        Yaml::String(s) => Json::String(s),
        Yaml::Sequence(seq) => {
            Json::Array(seq.into_iter().map(yaml_to_json).collect::<Result<_>>()?)
        }
        Yaml::Mapping(map) => {
            let mut obj = Map::new();
            for (k, v) in map {
                obj.insert(key(k)?, yaml_to_json(v)?);
            }
            Json::Object(obj)
        }
        // `!tag value` → `{tag: value}`, serde_json's external-tag form.
        Yaml::Tagged(tagged) => {
            let tag = tagged.tag.to_string();
            let name = tag.strip_prefix('!').unwrap_or(&tag).to_string();
            let mut obj = Map::new();
            obj.insert(name, yaml_to_json(tagged.value)?);
            Json::Object(obj)
        }
    })
}

/// A YAML number → a JSON number (rejecting the non-finite values JSON can't hold).
fn number(n: serde_norway::Number) -> Result<Json> {
    if let Some(i) = n.as_i64() {
        Ok(Json::Number(i.into()))
    } else if let Some(u) = n.as_u64() {
        Ok(Json::Number(u.into()))
    } else if let Some(f) = n.as_f64() {
        serde_json::Number::from_f64(f)
            .map(Json::Number)
            .ok_or_else(|| anyhow!("non-finite number `{f}` is not representable in JSON"))
    } else {
        bail!("unrepresentable YAML number")
    }
}

/// A scalar YAML mapping key → a JSON object key (object keys must be strings).
fn key(k: Yaml) -> Result<String> {
    match k {
        Yaml::String(s) => Ok(s),
        Yaml::Bool(b) => Ok(b.to_string()),
        Yaml::Number(n) => Ok(n.to_string()),
        other => bail!("unsupported non-scalar mapping key: {other:?}"),
    }
}
