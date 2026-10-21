use turso_core::{Result, Value};

use super::document::Json;
use super::{
    binary_string_error, boolean_result, is_json, json_result, parse, text_result, Call, ParseError,
};

pub(super) fn extract(call: &Call) -> Result<Value> {
    let Some(document) = call.document(0)? else {
        return Ok(Value::Null);
    };
    let mut wrap_in_array = call.args.len() > 2;
    let mut selected = Vec::new();
    for index in 1..call.args.len() {
        let Some(path) = call.path(index)? else {
            return Ok(Value::Null);
        };
        wrap_in_array |= path.can_match_many();
        selected.extend(path.select(&document, true).into_iter().cloned());
    }
    Ok(match selected.len() {
        0 => Value::Null,
        1 if !wrap_in_array => json_result(&selected[0]),
        _ => json_result(&Json::Array(selected)),
    })
}

pub(super) fn value(call: &Call) -> Result<Value> {
    let Some(document) = call.document(0)? else {
        return Ok(Value::Null);
    };
    let Some(path) = call.path(1)? else {
        return Ok(Value::Null);
    };
    Ok(match path.select(&document, true).as_slice() {
        [Json::Null] => Value::Null,
        [Json::String(text)] => text_result(text.clone()),
        [other] => text_result(other.to_text()),
        _ => Value::Null,
    })
}

pub(super) fn unquote(call: &Call) -> Result<Value> {
    match &call.args[0] {
        Value::Null => Ok(Value::Null),
        Value::Text(text) if is_json(text) => Ok(match parse(text.as_str()) {
            Ok(Json::String(unquoted)) => text_result(unquoted),
            _ => text_result(text.as_str().to_string()),
        }),
        Value::Text(text) => {
            let text = text.as_str();
            if !(text.len() >= 2 && text.starts_with('"') && text.ends_with('"')) {
                return Ok(text_result(text.to_string()));
            }
            match parse(text) {
                Ok(Json::String(unquoted)) => Ok(text_result(unquoted)),
                Ok(_) => Ok(text_result(text.to_string())),
                Err(error) => Err(call.invalid_json_text(0, error)),
            }
        }
        Value::Numeric(_) => Err(call.incorrect_type(0)),
        Value::Blob(_) => Err(binary_string_error()),
    }
}

pub(super) fn type_name(call: &Call) -> Result<Value> {
    Ok(call
        .document(0)?
        .map_or(Value::Null, |document| text_result(document.type_name())))
}

pub(super) fn valid(call: &Call) -> Result<Value> {
    match &call.args[0] {
        Value::Null => Ok(Value::Null),
        Value::Text(text) => match parse(text.as_str()) {
            Ok(_) => Ok(boolean_result(true)),
            Err(ParseError::Syntax { .. }) => Ok(boolean_result(false)),
            Err(error @ ParseError::TooDeep) => Err(call.invalid_json_text(0, error)),
        },
        Value::Numeric(_) | Value::Blob(_) => Ok(boolean_result(false)),
    }
}

pub(super) fn keys(call: &Call) -> Result<Value> {
    let Some(document) = call.document(0)? else {
        return Ok(Value::Null);
    };
    let target = if call.args.len() == 2 {
        let Some(path) = call.single_value_path(1)? else {
            return Ok(Value::Null);
        };
        path.select(&document, false).first().copied().cloned()
    } else {
        Some(document)
    };
    Ok(match target {
        Some(Json::Object(object)) => json_result(&Json::Array(
            object
                .iter()
                .map(|(key, _)| Json::String(key.clone()))
                .collect(),
        )),
        _ => Value::Null,
    })
}

pub(super) fn length(call: &Call) -> Result<Value> {
    let Some(document) = call.document(0)? else {
        return Ok(Value::Null);
    };
    if call.args.len() == 1 {
        return Ok(Value::from_i64(document.length() as i64));
    }
    let Some(path) = call.path(1)? else {
        return Ok(Value::Null);
    };
    let selected = path.select(&document, true);
    Ok(match selected.first() {
        None => Value::Null,
        Some(_) if path.can_match_many() => Value::from_i64(selected.len() as i64),
        Some(value) => Value::from_i64(value.length() as i64),
    })
}

pub(super) fn depth(call: &Call) -> Result<Value> {
    Ok(call.document(0)?.map_or(Value::Null, |document| {
        Value::from_i64(document.depth() as i64)
    }))
}

pub(super) fn pretty(call: &Call) -> Result<Value> {
    Ok(call.document(0)?.map_or(Value::Null, |document| {
        text_result(document.to_pretty_text())
    }))
}
