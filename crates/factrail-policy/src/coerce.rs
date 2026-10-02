//! Value coercion shared by option resolution and store reads.
//!
//! The plugin passes options exactly as the host received them, and a
//! shell-projected configuration carries every scalar as a string. These
//! helpers are the one place that decides what a loosely typed JSON value
//! means, so every option is read by the same rules.

use serde_json::Value;

/// A finite number from a JSON number or a decimal numeric string (trimmed).
/// Anything else, and any non-finite result, is `None`.
pub(crate) fn finite_number(value: Option<&Value>) -> Option<f64> {
    let n = match value? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    n.is_finite().then_some(n)
}

/// JavaScript's `String(value)` for the shapes options can take.
///
/// Used where the ported rule depends on it: a `false` cache-warm option reads
/// as `"false"`, and a list element `7` reads as `"7"`.
pub(crate) fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            Some(f) => f.to_string(),
            None => n.to_string(),
        },
        Value::String(s) => s.clone(),
        // `Array.prototype.join` renders null elements as the empty string.
        Value::Array(items) => items
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    js_string(v)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

/// A non-empty string option, untrimmed, as the TypeScript `str()` read it.
pub(crate) fn non_empty_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// A trimmed, lower-cased keyword from a string option; `None` for any other
/// shape and for a blank string.
pub(crate) fn keyword(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_lowercase()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_and_numeric_strings_are_read_non_finite_are_not() {
        assert_eq!(finite_number(Some(&json!(2.5))), Some(2.5));
        assert_eq!(finite_number(Some(&json!(" 9000 "))), Some(9000.0));
        assert_eq!(finite_number(Some(&json!("1e3"))), Some(1000.0));
        assert_eq!(finite_number(Some(&json!("inf"))), None);
        assert_eq!(finite_number(Some(&json!("NaN"))), None);
        assert_eq!(finite_number(Some(&json!(""))), None);
        assert_eq!(finite_number(Some(&json!("soon"))), None);
        assert_eq!(finite_number(Some(&json!(true))), None);
        assert_eq!(finite_number(Some(&Value::Null)), None);
        assert_eq!(finite_number(None), None);
    }

    #[test]
    fn js_string_matches_javascript_for_option_shapes() {
        assert_eq!(js_string(&json!(false)), "false");
        assert_eq!(js_string(&json!(0)), "0");
        assert_eq!(js_string(&json!(0.0)), "0");
        assert_eq!(js_string(&json!(7)), "7");
        assert_eq!(js_string(&json!(0.5)), "0.5");
        assert_eq!(js_string(&json!(["off"])), "off");
        assert_eq!(js_string(&json!([null, "a"])), ",a");
        assert_eq!(js_string(&json!({})), "[object Object]");
        assert_eq!(js_string(&Value::Null), "null");
    }
}
