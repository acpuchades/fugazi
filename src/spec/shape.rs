//! Which strategy document shape a `strategy:` payload describes.
//!
//! Two places accept "any strategy" as a nested field and have to decide
//! which of the shapes it is before deserializing:
//! [`PortfolioChildStrategy`](crate::spec::portfolio::PortfolioChildStrategy)
//! (a portfolio's `children:`) and
//! [`AnyStrategyRef`](crate::spec::trailing::AnyStrategyRef) (the
//! `strategy:` subtree of `!sharpe` and its four siblings).
//!
//! They used to carry a copy each. The copies drifted: the portfolio's
//! routed preset tags first and the trailing one didn't, so a preset under
//! `!sharpe { strategy: … }` was mis-detected as multi-asset on every real
//! load path. This module is the single decision both now read.

use serde_norway::Value;

use super::preset::PRESET_TAGS;

/// The shape a nested strategy payload describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShapeHint {
    /// A catalogue preset — `!buy_and_hold { … }`, `!ma_crossover { … }`, …
    Preset,
    /// A `left:` + `right:` map.
    Pairs,
    /// A map carrying `selection:`.
    Basket,
    /// A bare map with none of the above and no `symbol:`.
    Multi,
    /// A `symbol:`-carrying single-asset spec map.
    Single,
}

/// Classify a nested strategy payload by its distinctive top-level key.
///
/// **Order matters.** A preset is checked first because it arrives either as
/// a YAML `!tag { … }` ([`Value::Tagged`]) or — once
/// [`convert::yaml_to_json`](crate::spec::convert::yaml_to_json) has
/// normalised the document, which every real load path does — as a
/// single-key `{ tag: { … } }` mapping. That second form has no `symbol:` at
/// the top, so the multi-asset arm would otherwise swallow it and fail on an
/// unknown field.
pub(crate) fn detect_shape(v: &Value) -> ShapeHint {
    let is_preset_shape = matches!(v, Value::Tagged(_))
        || matches!(v, Value::Mapping(m) if m.len() == 1 && matches!(
            m.iter().next(),
            Some((Value::String(k), _)) if is_preset_tag(k)
        ));
    if is_preset_shape {
        return ShapeHint::Preset;
    }

    let Value::Mapping(m) = v else {
        // Not a mapping and not a tag: a bare scalar. `StrategyRef` owns the
        // error message for that, so route it there.
        return ShapeHint::Single;
    };
    let has = |key: &str| {
        m.iter()
            .any(|(k, _)| matches!(k, Value::String(s) if s == key))
    };

    if has("left") && has("right") {
        ShapeHint::Pairs
    } else if has("selection") {
        ShapeHint::Basket
    } else if has("symbol") {
        ShapeHint::Single
    } else {
        // The shape with no upfront symbol declaration — its universe floats.
        ShapeHint::Multi
    }
}

/// Whether `name` is one of [`PRESET_TAGS`].
///
/// Reads the constant directly rather than re-listing it: the two used to be
/// hand-duplicated, with a doc comment claiming a test kept them in sync that
/// had never existed.
fn is_preset_tag(name: &str) -> bool {
    PRESET_TAGS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Value {
        serde_norway::from_str(yaml).unwrap()
    }

    /// The JSON bridge every real load path runs before a nested strategy is
    /// deserialized. `!tag v` becomes `{tag: v}`, which is why the preset
    /// check cannot rely on `Value::Tagged` alone.
    fn bridged(yaml: &str) -> Value {
        let json = crate::spec::convert::yaml_to_json(parse(yaml)).unwrap();
        serde_norway::to_value(json).unwrap()
    }

    #[test]
    fn a_preset_is_detected_in_both_its_tagged_and_bridged_forms() {
        for v in [
            parse("!ma_crossover { symbol: X, fast: 2, slow: 4 }"),
            bridged("!ma_crossover { symbol: X, fast: 2, slow: 4 }"),
        ] {
            assert_eq!(detect_shape(&v), ShapeHint::Preset, "{v:?}");
        }
    }

    #[test]
    fn every_preset_tag_routes_to_preset_after_the_bridge() {
        // The regression this module exists for: post-bridge a preset is a
        // bare single-key map, indistinguishable from a multi-asset spec
        // except by the tag name.
        for tag in PRESET_TAGS {
            let v = bridged(&format!("!{tag} {{ symbol: X }}"));
            assert_eq!(detect_shape(&v), ShapeHint::Preset, "{tag}");
        }
    }

    #[test]
    fn the_four_spec_map_shapes_are_told_apart() {
        assert_eq!(
            detect_shape(&parse("{ left: A, right: B, entry: !value true }")),
            ShapeHint::Pairs,
        );
        assert_eq!(
            detect_shape(&parse("{ score: !close, selection: !top_bottom { longs: 1 } }")),
            ShapeHint::Basket,
        );
        assert_eq!(detect_shape(&parse("{ symbol: X, long: {} }")), ShapeHint::Single);
        assert_eq!(detect_shape(&parse("{ long: {}, sizing: !value 1.0 }")), ShapeHint::Multi);
    }

    #[test]
    fn a_non_preset_single_key_map_is_not_a_preset() {
        // `{long: {...}}` is a one-key map too — only the *tag name* separates
        // it from a bridged preset.
        assert_eq!(detect_shape(&parse("{ long: {} }")), ShapeHint::Multi);
    }
}
