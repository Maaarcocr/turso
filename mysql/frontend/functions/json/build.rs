use turso_core::{LimboError, Numeric, Result, Value};

use super::document::{quote, Json, Object};
use super::{binary_string_error, is_json, json_result, mysql_error, text_result, Call};

pub(super) fn array(call: &Call) -> Result<Value> {
    let items = (0..call.args.len())
        .map(|index| call.value(index))
        .collect::<Result<Vec<_>>>()?;
    Ok(json_result(&Json::Array(items)))
}

pub(super) fn array_aggregate(call: &Call) -> Result<Value> {
    let items = aggregated_rows(call)?
        .into_iter()
        .map(|row| match row {
            Json::Array(mut items) if items.len() == 1 => Ok(items.remove(0)),
            _ => Err(LimboError::InternalError(
                "a JSON_ARRAYAGG row is not a one-element array".to_string(),
            )),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(if items.is_empty() {
        Value::Null
    } else {
        json_result(&Json::Array(items))
    })
}

pub(super) fn object_aggregate(call: &Call) -> Result<Value> {
    let rows = aggregated_rows(call)?;
    if rows.is_empty() {
        return Ok(Value::Null);
    }
    let mut object = Object::default();
    for row in rows {
        let Json::Object(members) = row else {
            return Err(LimboError::InternalError(
                "a JSON_OBJECTAGG row is not an object".to_string(),
            ));
        };
        for (key, value) in members.into_members() {
            object.insert(key, value);
        }
    }
    Ok(json_result(&Json::Object(object)))
}

fn aggregated_rows(call: &Call) -> Result<Vec<Json>> {
    match call.document(0)? {
        Some(Json::Array(rows)) => Ok(rows),
        _ => Err(LimboError::InternalError(
            "json_group_array did not collect an array".to_string(),
        )),
    }
}

pub(super) fn object(call: &Call) -> Result<Value> {
    let mut object = Object::default();
    for key_index in (0..call.args.len()).step_by(2) {
        let key = member_name(&call.args[key_index])?;
        let value = call.value(key_index + 1)?;
        object.insert(key, value);
    }
    Ok(json_result(&Json::Object(object)))
}

fn member_name(value: &Value) -> Result<String> {
    match value {
        Value::Null => Err(mysql_error(
            3158,
            "22032",
            "JSON documents may not contain NULL member names.",
        )),
        Value::Numeric(Numeric::Integer(value)) => Ok(value.to_string()),
        Value::Text(text) => Ok(text.as_str().to_string()),
        Value::Numeric(Numeric::Float(_)) | Value::Blob(_) => Err(binary_string_error()),
    }
}

pub(super) fn quote_string(call: &Call) -> Result<Value> {
    match &call.args[0] {
        Value::Null => Ok(Value::Null),
        Value::Text(text) if !is_json(text) => Ok(text_result(quote(text.as_str()))),
        _ => Err(call.incorrect_type(0)),
    }
}

pub(super) fn cast_as_json(call: &Call) -> Result<Value> {
    match &call.args[0] {
        Value::Null => Ok(Value::Null),
        Value::Text(_) => {
            let document = call.document(0)?.expect("text is not NULL");
            Ok(json_result(&document))
        }
        _ => Ok(json_result(&call.value(0)?)),
    }
}

pub(super) fn cast_boolean_as_json(call: &Call) -> Result<Value> {
    let truth = match &call.args[0] {
        Value::Null => return Ok(Value::Null),
        Value::Numeric(Numeric::Integer(value)) => *value != 0,
        Value::Numeric(Numeric::Float(value)) => f64::from(*value) != 0.0,
        Value::Text(_) | Value::Blob(_) => {
            return Err(mysql_error(
                3064,
                "HY000",
                "Incorrect type for argument 1 in function cast_as_json.",
            ))
        }
    };
    Ok(json_result(&Json::Boolean(truth)))
}
