//! Branch-step conditions: JS truthiness, loose equality, `when` evaluation.

use super::lookup_path;
use serde_json::{Map, Value};

/// JS truthiness for expect results (0, "", null, false, NaN are falsy —
/// everything else truthy, arrays/objects included).
pub(super) fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Branch steps (issue #72)
// ---------------------------------------------------------------------------

/// The condition operators a `when` may use.
pub(super) const CONDITION_OPS: &[&str] = &[
    "equals", "not_equals", "in", "not_in", "contains", "at_least", "at_most", "truthy",
    "falsy",
];

/// JSON equality with number looseness: 200 == 200.0 (serde_json's Value
/// PartialEq is representation-sensitive; a flow author comparing an
/// integer fact against a float literal shouldn't care).
pub(super) fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// The branch step's condition — structured JSON, deliberately no
/// expression parser: `{"var": "v.verdict", "is": "equals", "value":
/// "landed"}`. `var` is a dotted path resolved by the same
/// [`lookup_path`] `{{placeholders}}` use, so a branch reads one field
/// off an earlier step's saved result. Unknown var and unknown `is` are
/// loud step errors — the same fail-loudly contract as substitution; a
/// condition that silently evaluated false on a typo would be a flow
/// that skips its own retry loop.
pub(super) fn eval_condition(when: &Value, vars: &Map<String, Value>) -> Result<bool, String> {
    let path = when
        .get("var")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "when.var (a dotted var path) is required".to_string())?;
    let is = when
        .get("is")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("when.is is required (one of {})", CONDITION_OPS.join(" | ")))?;
    let value = when.get("value");
    let val = lookup_path(vars, path).ok_or_else(|| {
        format!("unknown var '{path}' — no step saved it and no var declared it")
    })?;
    match is {
        "equals" => Ok(loose_eq(val, value.ok_or("equals needs a value")?)),
        "not_equals" => Ok(!loose_eq(val, value.ok_or("not_equals needs a value")?)),
        "in" => match value.ok_or("`in` needs a value")? {
            Value::Array(a) => Ok(a.iter().any(|x| loose_eq(val, x))),
            _ => Err("`in` requires an array value".into()),
        },
        "not_in" => match value.ok_or("`not_in` needs a value")? {
            Value::Array(a) => Ok(!a.iter().any(|x| loose_eq(val, x))),
            _ => Err("`not_in` requires an array value".into()),
        },
        "contains" => match val {
            Value::String(hay) => {
                let needle = value
                    .and_then(Value::as_str)
                    .ok_or("contains on a string var needs a string value")?;
                Ok(hay.contains(needle))
            }
            Value::Array(a) => Ok(value.is_some_and(|v| a.iter().any(|x| loose_eq(x, v)))),
            _ => Err("contains applies to string and array vars".into()),
        },
        "at_least" | "at_most" => {
            let x = val
                .as_f64()
                .ok_or("at_least/at_most need a numeric var")?;
            let y = value
                .and_then(Value::as_f64)
                .ok_or("at_least/at_most need a numeric value")?;
            Ok(if is == "at_least" { x >= y } else { x <= y })
        }
        "truthy" => Ok(js_truthy(val)),
        "falsy" => Ok(!js_truthy(val)),
        other => Err(format!(
            "unknown when.is {other:?} (one of {})",
            CONDITION_OPS.join(" | ")
        )),
    }
}
