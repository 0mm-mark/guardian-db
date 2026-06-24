use crate::odm::error::{OdmError, Result};
use serde_json::{Map, Number, Value};
use std::collections::BTreeSet;

pub(crate) fn apply_update(
    document: &mut Value,
    operations: &Value,
    immutable_fields: &BTreeSet<String>,
) -> Result<bool> {
    let operations = operations.as_object().ok_or_else(|| {
        OdmError::InvalidUpdate("update operations must be a JSON object".to_string())
    })?;

    if operations.is_empty() {
        return Ok(false);
    }
    if operations.keys().any(|key| !key.starts_with('$')) {
        return Err(OdmError::InvalidUpdate(
            "replacement updates are not supported; use operators such as $set".to_string(),
        ));
    }

    let mut changed = false;
    for (operator, operand) in operations {
        match operator.as_str() {
            "$set" => {
                let fields = object_operand(operator, operand)?;
                for (path, value) in fields {
                    ensure_mutable(path, immutable_fields)?;
                    changed |= set_path(document, path, value.clone())?;
                }
            }
            "$unset" => {
                let fields = object_operand(operator, operand)?;
                for path in fields.keys() {
                    ensure_mutable(path, immutable_fields)?;
                    changed |= unset_path(document, path)?;
                }
            }
            "$inc" => {
                let fields = object_operand(operator, operand)?;
                for (path, increment) in fields {
                    ensure_mutable(path, immutable_fields)?;
                    changed |= increment_path(document, path, increment)?;
                }
            }
            other => {
                return Err(OdmError::InvalidUpdate(format!(
                    "unsupported update operator `{other}`"
                )));
            }
        }
    }
    Ok(changed)
}

fn object_operand<'a>(operator: &str, value: &'a Value) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| OdmError::InvalidUpdate(format!("{operator} expects an object")))
}

fn ensure_mutable(path: &str, immutable_fields: &BTreeSet<String>) -> Result<()> {
    if let Some(field) = immutable_fields
        .iter()
        .find(|field| path == field.as_str() || path.starts_with(&format!("{field}.")))
    {
        return Err(OdmError::ImmutableField(field.clone()));
    }
    Ok(())
}

fn set_path(document: &mut Value, path: &str, value: Value) -> Result<bool> {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return Err(OdmError::InvalidUpdate(format!(
            "invalid field path `{path}`"
        )));
    }

    let mut current = document;
    for segment in &segments[..segments.len() - 1] {
        let object = current.as_object_mut().ok_or_else(|| {
            OdmError::InvalidUpdate(format!("cannot traverse `{path}` through a non-object"))
        })?;
        current = object
            .entry((*segment).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }

    let object = current
        .as_object_mut()
        .ok_or_else(|| OdmError::InvalidUpdate(format!("cannot set `{path}` on a non-object")))?;
    let key = segments
        .last()
        .expect("path has at least one segment")
        .to_string();
    let changed = object.get(&key) != Some(&value);
    object.insert(key, value);
    Ok(changed)
}

fn unset_path(document: &mut Value, path: &str) -> Result<bool> {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return Err(OdmError::InvalidUpdate(format!(
            "invalid field path `{path}`"
        )));
    }

    let mut current = document;
    for segment in &segments[..segments.len() - 1] {
        let Some(next) = current
            .as_object_mut()
            .and_then(|object| object.get_mut(*segment))
        else {
            return Ok(false);
        };
        current = next;
    }
    Ok(current.as_object_mut().is_some_and(|object| {
        object
            .remove(*segments.last().expect("path is non-empty"))
            .is_some()
    }))
}

fn increment_path(document: &mut Value, path: &str, increment: &Value) -> Result<bool> {
    let increment = increment.as_number().ok_or_else(|| {
        OdmError::InvalidUpdate(format!("$inc value for `{path}` must be numeric"))
    })?;

    let zero = Number::from(0);
    let current = match crate::odm::query::value_at_path(document, path) {
        None => &zero,
        Some(value) => value.as_number().ok_or_else(|| {
            OdmError::InvalidUpdate(format!("$inc target `{path}` must be numeric"))
        })?,
    };
    let number = add_numbers(current, increment, path)?;
    set_path(document, path, Value::Number(number))
}

fn add_numbers(left: &Number, right: &Number, path: &str) -> Result<Number> {
    // serde_json distinguishes integer and floating-point JSON numbers. Keep
    // integer arithmetic integer whenever both operands are integers instead
    // of routing every $inc through f64 (which turned 1024 + 1 into 1025.0).
    if let (Some(left), Some(right)) = (integer_value(left), integer_value(right)) {
        let next = left.checked_add(right).ok_or_else(|| {
            OdmError::InvalidUpdate(format!(
                "$inc for `{path}` overflowed the JSON integer range"
            ))
        })?;

        if let Ok(value) = i64::try_from(next) {
            return Ok(Number::from(value));
        }
        if let Ok(value) = u64::try_from(next) {
            return Ok(Number::from(value));
        }
        return Err(OdmError::InvalidUpdate(format!(
            "$inc for `{path}` overflowed the JSON integer range"
        )));
    }

    let next = left.as_f64().unwrap_or(f64::NAN) + right.as_f64().unwrap_or(f64::NAN);
    Number::from_f64(next).ok_or_else(|| {
        OdmError::InvalidUpdate(format!("$inc for `{path}` produced a non-finite number"))
    })
}

fn integer_value(number: &Number) -> Option<i128> {
    number
        .as_i64()
        .map(i128::from)
        .or_else(|| number.as_u64().map(i128::from))
}

#[cfg(test)]
mod tests {
    use super::apply_update;
    use crate::odm::error::OdmError;
    use serde_json::json;
    use std::collections::BTreeSet;

    #[test]
    fn inc_preserves_integer_representation() {
        let mut document = json!({ "counter": 1024 });
        let changed = apply_update(
            &mut document,
            &json!({ "$inc": { "counter": 1, "created": 2 } }),
            &BTreeSet::new(),
        )
        .unwrap();

        assert!(changed);
        assert_eq!(document["counter"], json!(1025));
        assert_eq!(document["created"], json!(2));
        assert!(document["counter"].as_i64().is_some());
    }

    #[test]
    fn inc_keeps_fractional_arithmetic_fractional() {
        let mut document = json!({ "counter": 1.25 });
        apply_update(
            &mut document,
            &json!({ "$inc": { "counter": 0.5 } }),
            &BTreeSet::new(),
        )
        .unwrap();

        assert_eq!(document["counter"], json!(1.75));
    }

    #[test]
    fn inc_reports_integer_overflow() {
        let mut document = json!({ "counter": u64::MAX });
        let error = apply_update(
            &mut document,
            &json!({ "$inc": { "counter": 1 } }),
            &BTreeSet::new(),
        )
        .unwrap_err();

        assert!(
            matches!(error, OdmError::InvalidUpdate(message) if message.contains("overflowed"))
        );
    }
}
