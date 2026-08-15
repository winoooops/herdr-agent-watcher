//! Wrong-type-tolerant serde deserializers shared across adapter DTOs.
//!
//! Step A-status of the v4-frozen refactor plan (#246) migrates Claude
//! and Codex parsers off `serde_json::Value` pull-style extraction and
//! onto typed DTOs. The old pull-style code was unintentionally lenient:
//! a wrong-typed field (e.g. `"42"` where an `u64` was expected) yielded
//! `Value::as_u64() == None` → field-level fallback to `0`, while the
//! rest of the document continued parsing.
//!
//! Strict `#[derive(Deserialize)]` does NOT have that property — a
//! single wrong-typed field fails the whole document. The helpers in
//! this module restore the per-field tolerance by deserializing to
//! `Value` first, then asking the value to coerce. A wrong type
//! produces `Ok(None)`, NOT `Err(_)`. The caller maps `None` to a
//! domain-specific default (usually `0` for token counts /
//! durations).
//!
//! Use via the `deserialize_with` attribute, paired with
//! `#[serde(default)]` so missing / `null` reach the same `None`
//! branch:
//!
//! ```ignore
//! #[derive(Deserialize)]
//! struct Foo {
//!     #[serde(default, deserialize_with = "super::serde_helpers::lenient_u64")]
//!     count: Option<u64>,
//! }
//! ```

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// Deserialize an `Option<u64>` field with wrong-type tolerance.
///
/// Returns `Ok(None)` when the value is missing, `null`, a non-integer
/// number, a string, an array, or any other JSON type that cannot
/// represent a `u64`. Returns `Ok(Some(_))` when the value is a JSON
/// integer that fits in `u64`.
///
/// Mirrors the pre-A-status behavior of
/// `value.get(key).and_then(Value::as_u64)`.
pub(super) fn lenient_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Value::deserialize(deserializer)?.as_u64())
}

/// Deserialize an `Option<f64>` field with wrong-type tolerance.
///
/// Returns `Ok(None)` for missing / `null` / non-numeric / non-finite
/// inputs. Returns `Ok(Some(_))` for any JSON number (including
/// integers, which `serde_json` coerces to `f64` lossily for large
/// magnitudes — same coercion `Value::as_f64` already applies).
///
/// Mirrors the pre-A-status behavior of
/// `value.get(key).and_then(Value::as_f64)`.
pub(super) fn lenient_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Value::deserialize(deserializer)?.as_f64())
}

/// Deserialize an `Option<String>` field with wrong-type tolerance.
///
/// Returns `Ok(None)` when the value is missing, `null`, a number, an
/// array, or any non-string JSON type. Returns `Ok(Some(_))` for
/// strings.
///
/// Mirrors the pre-A-status behavior of
/// `value.get(key).and_then(Value::as_str).map(str::to_string)`.
pub(super) fn lenient_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Value::deserialize(deserializer)?
        .as_str()
        .map(str::to_string))
}

/// Deserialize an `Option<T>` nested-struct field with wrong-type
/// tolerance.
///
/// Returns `Ok(None)` when the value is missing, `null`, OR any
/// non-object JSON type (an array, number, string, or bool sitting
/// where the schema expects an object). Returns `Ok(Some(T))` when
/// the value is a JSON object that successfully decodes into `T` —
/// which itself happens via `serde_json::from_value::<T>(value)`,
/// relying on `T`'s leaf fields to be `lenient_*`-decorated for
/// inner wrong-type tolerance. If the inner `from_value` fails (e.g.
/// a leaf that should be `lenient_*` was left strict), we still
/// return `Ok(None)` — consistent with the scalar helpers' "wrong
/// shape becomes a missing block" contract.
///
/// Mirrors the pre-A-status `has_<block>(value).then(|| ...)` /
/// `is_some_and(Value::is_object)` guards that fell back to per-block
/// defaults when an outer block was the wrong shape (rather than
/// erroring the whole document). Closes the round-1 Claude review
/// MEDIUM + codex connector P1/P2 on PR #257.
pub(super) fn lenient_object<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = Value::deserialize(deserializer)?;
    if !value.is_object() {
        return Ok(None);
    }
    Ok(serde_json::from_value::<T>(value).ok())
}

/// Deserialize an `Option<bool>` field with wrong-type tolerance.
///
/// Returns `Ok(None)` for missing / `null` / non-bool inputs (a `"true"`
/// string or a number is NOT a bool). Returns `Ok(Some(_))` for JSON
/// booleans.
///
/// Mirrors the pre-A-transcript behavior of
/// `value.get(key).and_then(Value::as_bool)`.
pub(super) fn lenient_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Value::deserialize(deserializer)?.as_bool())
}

/// Deserialize an `Option<i64>` field with wrong-type tolerance.
///
/// Returns `Ok(None)` for missing / `null` / non-integer inputs (a numeric
/// string or a non-integer float is NOT an i64). Returns `Ok(Some(_))` for
/// JSON integers in i64 range.
///
/// Mirrors the pre-A-transcript behavior of
/// `value.get(key).and_then(Value::as_i64)`.
pub(super) fn lenient_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Value::deserialize(deserializer)?.as_i64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize, Debug, PartialEq)]
    struct Probe {
        #[serde(default, deserialize_with = "lenient_u64")]
        count: Option<u64>,
        #[serde(default, deserialize_with = "lenient_f64")]
        ratio: Option<f64>,
        #[serde(default, deserialize_with = "lenient_string")]
        label: Option<String>,
        #[serde(default, deserialize_with = "lenient_bool")]
        flag: Option<bool>,
        #[serde(default, deserialize_with = "lenient_i64")]
        code: Option<i64>,
    }

    #[test]
    fn lenient_u64_accepts_integer_rejects_others() {
        // Happy paths
        let p: Probe = serde_json::from_str(r#"{"count": 42}"#).expect("integer ok");
        assert_eq!(p.count, Some(42));

        // Wrong types → None (NOT Err); whole document continues parsing
        let p: Probe = serde_json::from_str(r#"{"count": "42"}"#).expect("string ok→None");
        assert_eq!(p.count, None);

        let p: Probe = serde_json::from_str(r#"{"count": 42.5}"#).expect("float ok→None");
        assert_eq!(p.count, None);

        let p: Probe = serde_json::from_str(r#"{"count": -5}"#).expect("negative ok→None");
        assert_eq!(p.count, None);

        let p: Probe = serde_json::from_str(r#"{"count": null}"#).expect("null ok→None");
        assert_eq!(p.count, None);

        let p: Probe = serde_json::from_str(r#"{}"#).expect("missing ok→None");
        assert_eq!(p.count, None);
    }

    #[test]
    fn lenient_f64_accepts_numbers_rejects_others() {
        let p: Probe = serde_json::from_str(r#"{"ratio": 1.5}"#).expect("float ok");
        assert_eq!(p.ratio, Some(1.5));

        // Integer coerces to f64 — matches `Value::as_f64` behavior
        let p: Probe = serde_json::from_str(r#"{"ratio": 42}"#).expect("integer ok→Some");
        assert_eq!(p.ratio, Some(42.0));

        // Wrong types → None
        let p: Probe = serde_json::from_str(r#"{"ratio": "1.5"}"#).expect("string ok→None");
        assert_eq!(p.ratio, None);

        let p: Probe = serde_json::from_str(r#"{"ratio": null}"#).expect("null ok→None");
        assert_eq!(p.ratio, None);
    }

    #[test]
    fn lenient_string_accepts_strings_rejects_others() {
        let p: Probe = serde_json::from_str(r#"{"label": "hello"}"#).expect("string ok");
        assert_eq!(p.label.as_deref(), Some("hello"));

        let p: Probe = serde_json::from_str(r#"{"label": 42}"#).expect("number ok→None");
        assert_eq!(p.label, None);

        let p: Probe = serde_json::from_str(r#"{"label": null}"#).expect("null ok→None");
        assert_eq!(p.label, None);
    }

    #[test]
    fn lenient_bool_accepts_bools_rejects_others() {
        let p: Probe = serde_json::from_str(r#"{"flag": true}"#).expect("bool ok");
        assert_eq!(p.flag, Some(true));

        // Wrong types → None (matches `Value::as_bool`): a "true" string or 1 is not a bool.
        let p: Probe = serde_json::from_str(r#"{"flag": "true"}"#).expect("string ok→None");
        assert_eq!(p.flag, None);

        let p: Probe = serde_json::from_str(r#"{"flag": 1}"#).expect("number ok→None");
        assert_eq!(p.flag, None);

        let p: Probe = serde_json::from_str(r#"{"flag": null}"#).expect("null ok→None");
        assert_eq!(p.flag, None);

        let p: Probe = serde_json::from_str(r#"{}"#).expect("missing ok→None");
        assert_eq!(p.flag, None);
    }

    #[test]
    fn lenient_i64_accepts_ints_rejects_others() {
        let p: Probe = serde_json::from_str(r#"{"code": -3}"#).expect("negative int ok");
        assert_eq!(p.code, Some(-3));

        let p: Probe = serde_json::from_str(r#"{"code": 0}"#).expect("zero ok");
        assert_eq!(p.code, Some(0));

        // Wrong types → None (matches `Value::as_i64`): strings and non-integer floats.
        let p: Probe = serde_json::from_str(r#"{"code": "3"}"#).expect("string ok→None");
        assert_eq!(p.code, None);

        let p: Probe = serde_json::from_str(r#"{"code": 1.5}"#).expect("float ok→None");
        assert_eq!(p.code, None);

        let p: Probe = serde_json::from_str(r#"{"code": null}"#).expect("null ok→None");
        assert_eq!(p.code, None);

        let p: Probe = serde_json::from_str(r#"{}"#).expect("missing ok→None");
        assert_eq!(p.code, None);
    }

    /// Per-field tolerance — one wrong-typed field does NOT poison
    /// the rest of the document. This is the defining property the
    /// helpers exist to preserve.
    #[test]
    fn one_wrong_field_does_not_poison_the_rest() {
        let p: Probe = serde_json::from_str(r#"{"count": "garbage", "ratio": 0.5, "label": "ok"}"#)
            .expect("partial degradation");
        assert_eq!(p.count, None);
        assert_eq!(p.ratio, Some(0.5));
        assert_eq!(p.label.as_deref(), Some("ok"));
    }

    // ----- lenient_object tests -----

    #[derive(Deserialize, Debug, PartialEq, Default)]
    struct Inner {
        #[serde(default, deserialize_with = "lenient_u64")]
        n: Option<u64>,
    }

    #[derive(Deserialize, Debug, PartialEq)]
    struct ObjectProbe {
        #[serde(default, deserialize_with = "lenient_object")]
        block: Option<Inner>,
    }

    #[test]
    fn lenient_object_accepts_objects_rejects_others() {
        // Happy path: an object decodes to Some(_).
        let p: ObjectProbe = serde_json::from_str(r#"{"block": {"n": 7}}"#).expect("object ok");
        assert_eq!(p.block, Some(Inner { n: Some(7) }));

        // Empty object → Some(default) (the block IS present).
        let p: ObjectProbe = serde_json::from_str(r#"{"block": {}}"#).expect("empty object ok");
        assert_eq!(p.block, Some(Inner { n: None }));

        // null → None (treated identical to missing — same as the
        // scalar helpers).
        let p: ObjectProbe = serde_json::from_str(r#"{"block": null}"#).expect("null ok→None");
        assert_eq!(p.block, None);

        // Wrong types → None (this is the round-1 fix point — without
        // `lenient_object` these would have errored the whole parse).
        let p: ObjectProbe = serde_json::from_str(r#"{"block": 42}"#).expect("integer ok→None");
        assert_eq!(p.block, None);

        let p: ObjectProbe = serde_json::from_str(r#"{"block": []}"#).expect("array ok→None");
        assert_eq!(p.block, None);

        let p: ObjectProbe =
            serde_json::from_str(r#"{"block": "string"}"#).expect("string ok→None");
        assert_eq!(p.block, None);

        let p: ObjectProbe = serde_json::from_str(r#"{}"#).expect("missing ok→None");
        assert_eq!(p.block, None);
    }

    /// Round-1 fix invariant: a wrong-typed nested block does NOT
    /// poison sibling fields. Mirror of the scalar-tolerance test
    /// above, but for the new `lenient_object` helper.
    #[test]
    fn lenient_object_wrong_typed_block_does_not_poison_siblings() {
        #[derive(Deserialize, PartialEq, Debug)]
        struct MixedProbe {
            #[serde(default, deserialize_with = "lenient_object")]
            block: Option<Inner>,
            #[serde(default, deserialize_with = "lenient_u64")]
            sibling: Option<u64>,
        }
        let p: MixedProbe =
            serde_json::from_str(r#"{"block": 42, "sibling": 99}"#).expect("partial degrade");
        assert_eq!(p.block, None);
        assert_eq!(p.sibling, Some(99));
    }
}
