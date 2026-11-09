//! Strict JSON profile shared with the Python SDK.
//!
//! RFC 8259 leaves duplicate object names undefined and lets implementations
//! limit nesting, so peers that differ silently disagree. Runpy rejects any
//! duplicate key in any object and any nesting deeper than
//! [`MAX_JSON_DEPTH`] containers, counting the envelope itself.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

/// Deepest container nesting accepted, including the top-level envelope.
pub(crate) const MAX_JSON_DEPTH: usize = 64;

/// Reject bodies nested deeper than [`MAX_JSON_DEPTH`] with one linear scan,
/// before any recursive parsing. Brackets inside strings, including escaped
/// quotes, are ignored; UTF-8 continuation bytes never look like ASCII.
pub(crate) fn check_depth(bytes: &[u8]) -> Result<(), String> {
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > MAX_JSON_DEPTH {
                    return Err(format!(
                        "JSON nesting exceeds the maximum depth of {MAX_JSON_DEPTH}"
                    ));
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

/// Parse a bounded body under the strict profile.
pub(crate) fn parse_strict(bytes: &[u8]) -> Result<Value, String> {
    check_depth(bytes)?;
    let StrictValue(value) = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    Ok(value)
}

/// Container nesting of `value`, computed without recursion.
#[cfg(test)]
fn depth(value: &Value) -> usize {
    nesting(value, 1)
}

/// Container nesting of an object given as its fields.
pub(crate) fn object_depth(fields: &Map<String, Value>) -> usize {
    fields
        .values()
        .map(|value| nesting(value, 2))
        .max()
        .unwrap_or(1)
        .max(1)
}

/// Deepest container level under `value`, which sits at `level`.
fn nesting(value: &Value, level: usize) -> usize {
    let mut deepest = 0;
    let mut pending = vec![(value, level)];
    while let Some((value, level)) = pending.pop() {
        match value {
            Value::Array(items) => {
                deepest = deepest.max(level);
                pending.extend(items.iter().map(|item| (item, level + 1)));
            }
            Value::Object(fields) => {
                deepest = deepest.max(level);
                pending.extend(fields.values().map(|item| (item, level + 1)));
            }
            _ => {}
        }
    }
    deepest
}

/// A JSON value whose objects never repeat a key.
struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(StrictValue)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(StrictValue(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut fields = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let StrictValue(value) = map.next_value()?;
            if fields.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate object key '{key}'")));
            }
            fields.insert(key, value);
        }
        Ok(Value::Object(fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested(levels: usize) -> String {
        format!("{}{}", "[".repeat(levels), "]".repeat(levels))
    }

    #[test]
    fn depth_limit_is_inclusive_and_checked_by_both_paths() {
        assert!(parse_strict(nested(MAX_JSON_DEPTH).as_bytes()).is_ok());
        assert!(parse_strict(nested(MAX_JSON_DEPTH + 1).as_bytes()).is_err());
        let value = parse_strict(nested(MAX_JSON_DEPTH).as_bytes()).unwrap();
        assert_eq!(depth(&value), MAX_JSON_DEPTH);
    }

    #[test]
    fn brackets_in_strings_and_escapes_do_not_count() {
        let tricky = format!(
            r#"{{"a": "{}\"{}", "b": "\\"}}"#,
            "[".repeat(100),
            "{".repeat(100)
        );
        assert_eq!(depth(&parse_strict(tricky.as_bytes()).unwrap()), 1);
    }

    #[test]
    fn duplicate_keys_are_rejected_at_any_level() {
        for body in [
            r#"{"meta": {}, "meta": {}, "data": {}}"#,
            r#"{"meta": {"k": 1, "k": 2}, "data": {}}"#,
            r#"{"meta": {}, "data": {"deep": [{"x": 1, "x": 1}]}}"#,
            // The same key spelled with an escape is still a duplicate.
            r#"{"meta": {}, "data": {"é": 1, "\u00e9": 2}}"#,
        ] {
            let error = parse_strict(body.as_bytes()).unwrap_err();
            assert!(error.contains("duplicate"), "{body}: {error}");
        }
        // Distinct Unicode names (precomposed vs combining) are different keys.
        assert!(parse_strict(r#"{"\u00e9": 1, "e\u0301": 2}"#.as_bytes()).is_ok());
    }
}
