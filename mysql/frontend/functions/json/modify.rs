use turso_core::{Result, Value};

use super::document::{Json, Object};
use super::path::{locate_legs, value_at_mut, Leg, Step};
use super::{json_result, mysql_error, Call};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Change {
    Set,
    Insert,
    Replace,
}

impl Change {
    fn adds_missing_values(self) -> bool {
        self != Change::Replace
    }

    fn replaces_existing_values(self) -> bool {
        self != Change::Insert
    }
}

pub(super) fn set(call: &Call, change: Change) -> Result<Value> {
    let Some(mut document) = call.document(0)? else {
        return Ok(Value::Null);
    };
    for path_index in (1..call.args.len()).step_by(2) {
        let Some(path) = call.single_value_path(path_index)? else {
            return Ok(Value::Null);
        };
        let value = call.value(path_index + 1)?;
        let Some((last, parent_legs)) = path.legs.split_last() else {
            if change.replaces_existing_values() {
                document = value;
            }
            continue;
        };
        let Some(steps) = locate_legs(parent_legs, &document, true) else {
            continue;
        };
        set_child(value_at_mut(&mut document, &steps), last, value, change);
    }
    Ok(json_result(&document))
}

fn set_child(parent: &mut Json, leg: &Leg, value: Json, change: Change) {
    match (leg, parent) {
        (Leg::Member(name), Json::Object(object)) => {
            let exists = object.get(name).is_some();
            if (exists && change.replaces_existing_values())
                || (!exists && change.adds_missing_values())
            {
                object.insert(name.clone(), value);
            }
        }
        (Leg::Member(_), _) => {}
        (Leg::Cell(index), Json::Array(items)) => {
            let position = index.position(items.len());
            if position.within_bounds {
                if change.replaces_existing_values() {
                    items[position.index] = value;
                }
            } else if change.adds_missing_values() {
                items.insert(position.index, value);
            }
        }
        (Leg::Cell(index), parent) => {
            let position = index.position(1);
            if position.within_bounds {
                if change.replaces_existing_values() {
                    *parent = value;
                }
            } else if change.adds_missing_values() {
                let mut items = vec![std::mem::replace(parent, Json::Null)];
                items.insert(position.index, value);
                *parent = Json::Array(items);
            }
        }
        (other, _) => unreachable!("{other:?} must be rejected before changing a document"),
    }
}

pub(super) fn remove(call: &Call) -> Result<Value> {
    let Some(mut document) = call.document(0)? else {
        return Ok(Value::Null);
    };
    for path_index in 1..call.args.len() {
        let Some(path) = call.single_value_path(path_index)? else {
            return Ok(Value::Null);
        };
        if path.legs.is_empty() {
            return Err(mysql_error(
                3153,
                "42000",
                "The path expression '$' is not allowed in this context.",
            ));
        }
        let Some(steps) = path.locate(&document, true) else {
            continue;
        };
        let Some((last, parent_steps)) = steps.split_last() else {
            continue;
        };
        match (last, value_at_mut(&mut document, parent_steps)) {
            (Step::Member(name), Json::Object(object)) => {
                object.remove(name);
            }
            (Step::Cell(index), Json::Array(items)) => {
                items.remove(*index);
            }
            _ => unreachable!("located steps follow the document"),
        }
    }
    Ok(json_result(&document))
}

pub(super) fn array_append(call: &Call) -> Result<Value> {
    let Some(mut document) = call.document(0)? else {
        return Ok(Value::Null);
    };
    for path_index in (1..call.args.len()).step_by(2) {
        let Some(path) = call.single_value_path(path_index)? else {
            return Ok(Value::Null);
        };
        let value = call.value(path_index + 1)?;
        let Some(steps) = path.locate(&document, true) else {
            continue;
        };
        match value_at_mut(&mut document, &steps) {
            Json::Array(items) => items.push(value),
            other => {
                let first = std::mem::replace(other, Json::Null);
                *other = Json::Array(vec![first, value]);
            }
        }
    }
    Ok(json_result(&document))
}

pub(super) fn array_insert(call: &Call) -> Result<Value> {
    let Some(mut document) = call.document(0)? else {
        return Ok(Value::Null);
    };
    for path_index in (1..call.args.len()).step_by(2) {
        let Some(path) = call.single_value_path(path_index)? else {
            return Ok(Value::Null);
        };
        let Some((Leg::Cell(index), parent_legs)) = path.legs.split_last() else {
            return Err(mysql_error(
                3165,
                "42000",
                "A path expression is not a path to a cell in an array.",
            ));
        };
        let value = call.value(path_index + 1)?;
        let Some(steps) = locate_legs(parent_legs, &document, false) else {
            continue;
        };
        if let Json::Array(items) = value_at_mut(&mut document, &steps) {
            let position = index.position(items.len());
            items.insert(position.index, value);
        }
    }
    Ok(json_result(&document))
}

pub(super) fn merge_preserve(call: &Call) -> Result<Value> {
    let Some(mut merged) = call.document(0)? else {
        return Ok(Value::Null);
    };
    for index in 1..call.args.len() {
        let Some(next) = call.document(index)? else {
            return Ok(Value::Null);
        };
        merged = preserve_both(merged, next);
    }
    Ok(json_result(&merged))
}

fn preserve_both(left: Json, right: Json) -> Json {
    match (left, right) {
        (Json::Object(mut left), Json::Object(right)) => {
            for (key, value) in right.into_members() {
                let merged = match left.remove(&key) {
                    Some(existing) => preserve_both(existing, value),
                    None => value,
                };
                left.insert(key, merged);
            }
            Json::Object(left)
        }
        (Json::Array(mut left), Json::Array(right)) => {
            left.extend(right);
            Json::Array(left)
        }
        (Json::Array(mut left), right) => {
            left.push(right);
            Json::Array(left)
        }
        (left, Json::Array(right)) => {
            let mut items = vec![left];
            items.extend(right);
            Json::Array(items)
        }
        (left, right) => Json::Array(vec![left, right]),
    }
}

pub(super) fn merge_patch(call: &Call) -> Result<Value> {
    let mut merged = call.document(0)?;
    for index in 1..call.args.len() {
        merged = match call.document(index)? {
            None => None,
            Some(Json::Object(patch)) => merged.map(|target| apply_patch(target, patch)),
            Some(replacement) => Some(replacement),
        };
    }
    Ok(merged.map_or(Value::Null, |merged| json_result(&merged)))
}

fn apply_patch(target: Json, patch: Object) -> Json {
    let mut object = match target {
        Json::Object(object) => object,
        _ => Object::default(),
    };
    for (key, value) in patch.into_members() {
        match value {
            Json::Null => {
                object.remove(&key);
            }
            Json::Object(inner) => {
                let existing = object.remove(&key).unwrap_or(Json::Null);
                object.insert(key, apply_patch(existing, inner));
            }
            other => object.insert(key, other),
        }
    }
    Json::Object(object)
}
