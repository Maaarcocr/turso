use std::collections::HashSet;

use turso_core::{Result, Value};

use super::document::{equal, Json};
use super::path::{format_steps, Step};
use super::{boolean_result, json_result, mysql_error, sql_string, Call, OneOrAll};

pub(super) fn contains(call: &Call) -> Result<Value> {
    let Some(document) = call.document(0)? else {
        return Ok(Value::Null);
    };
    let Some(candidate) = call.document(1)? else {
        return Ok(Value::Null);
    };
    if call.args.len() == 2 {
        return Ok(boolean_result(is_contained(&candidate, &document)));
    }
    let Some(path) = call.single_value_path(2)? else {
        return Ok(Value::Null);
    };
    Ok(match path.select(&document, true).first() {
        Some(target) => boolean_result(is_contained(&candidate, target)),
        None => Value::Null,
    })
}

fn is_contained(candidate: &Json, target: &Json) -> bool {
    match (target, candidate) {
        (Json::Object(target), Json::Object(candidate)) => candidate.iter().all(|(key, value)| {
            target
                .get(key)
                .is_some_and(|member| is_contained(value, member))
        }),
        (Json::Object(_), _) => false,
        (Json::Array(items), Json::Array(candidates)) => candidates
            .iter()
            .all(|candidate| items.iter().any(|item| is_contained(candidate, item))),
        (Json::Array(items), candidate) => items.iter().any(|item| is_contained(candidate, item)),
        (scalar, candidate) => equal(scalar, candidate),
    }
}

pub(super) fn contains_path(call: &Call) -> Result<Value> {
    let Some(document) = call.document(0)? else {
        return Ok(Value::Null);
    };
    let Some(mode) = call.one_or_all(1)? else {
        return Ok(Value::Null);
    };
    for index in 2..call.args.len() {
        let Some(path) = call.path(index)? else {
            return Ok(Value::Null);
        };
        let exists = !path.select(&document, true).is_empty();
        match (mode, exists) {
            (OneOrAll::One, true) => return Ok(boolean_result(true)),
            (OneOrAll::All, false) => return Ok(boolean_result(false)),
            _ => {}
        }
    }
    Ok(boolean_result(mode == OneOrAll::All))
}

pub(super) fn overlaps(call: &Call) -> Result<Value> {
    let Some(left) = call.document(0)? else {
        return Ok(Value::Null);
    };
    let Some(right) = call.document(1)? else {
        return Ok(Value::Null);
    };
    Ok(boolean_result(overlap(&left, &right)))
}

fn overlap(left: &Json, right: &Json) -> bool {
    match (left, right) {
        (Json::Array(left), Json::Array(right)) => left
            .iter()
            .any(|item| right.iter().any(|other| equal(item, other))),
        (Json::Array(items), other) | (other, Json::Array(items)) => {
            items.iter().any(|item| equal(item, other))
        }
        (Json::Object(left), Json::Object(right)) => left
            .iter()
            .any(|(key, value)| right.get(key).is_some_and(|other| equal(value, other))),
        (left, right) => equal(left, right),
    }
}

pub(super) fn member_of(call: &Call) -> Result<Value> {
    if matches!(call.args[0], Value::Null) {
        return Ok(Value::Null);
    }
    let value = call.value(0)?;
    let Some(document) = call.document(1)? else {
        return Ok(Value::Null);
    };
    Ok(boolean_result(match &document {
        Json::Array(items) => items.iter().any(|item| equal(item, &value)),
        other => equal(other, &value),
    }))
}

pub(super) fn search(call: &Call) -> Result<Value> {
    let Some(document) = call.document(0)? else {
        return Ok(Value::Null);
    };
    let Some(mode) = call.one_or_all(1)? else {
        return Ok(Value::Null);
    };
    let Some(pattern) = sql_string(&call.args[2]) else {
        return Ok(Value::Null);
    };
    let escape = match call.args.get(3).map(sql_string) {
        None | Some(None) => '\\',
        Some(Some(text)) => {
            let mut characters = text.chars();
            match (characters.next(), characters.next()) {
                (None, _) => '\\',
                (Some(escape), None) => escape,
                (Some(_), Some(_)) => {
                    return Err(mysql_error(1210, "HY000", "Incorrect arguments to ESCAPE"))
                }
            }
        }
    };
    let mut scopes = None;
    for index in 4..call.args.len() {
        let Some(path) = call.path(index)? else {
            return Ok(Value::Null);
        };
        scopes.get_or_insert_with(HashSet::new).extend(
            path.select(&document, false)
                .into_iter()
                .map(|value| value as *const Json),
        );
    }
    let mut search = Search {
        pattern: pattern.chars().collect(),
        escape,
        scopes,
        stop_at_first: mode == OneOrAll::One,
        steps: Vec::new(),
        found: Vec::new(),
    };
    search.visit(&document, search.scopes.is_none());
    Ok(match search.found.len() {
        0 => Value::Null,
        1 => json_result(&search.found.remove(0)),
        _ => json_result(&Json::Array(search.found)),
    })
}

struct Search {
    pattern: Vec<char>,
    escape: char,
    scopes: Option<HashSet<*const Json>>,
    stop_at_first: bool,
    steps: Vec<Step>,
    found: Vec<Json>,
}

impl Search {
    fn visit(&mut self, value: &Json, in_scope: bool) {
        if self.stop_at_first && !self.found.is_empty() {
            return;
        }
        let in_scope = in_scope
            || self
                .scopes
                .as_ref()
                .is_some_and(|scopes| scopes.contains(&(value as *const Json)));
        match value {
            Json::String(text) => {
                let text: Vec<char> = text.chars().collect();
                if in_scope && matches_like(&text, &self.pattern, self.escape) {
                    self.found.push(Json::String(format_steps(&self.steps)));
                }
            }
            Json::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    self.steps.push(Step::Cell(index));
                    self.visit(item, in_scope);
                    self.steps.pop();
                }
            }
            Json::Object(object) => {
                for (key, member) in object.iter() {
                    self.steps.push(Step::Member(key.clone()));
                    self.visit(member, in_scope);
                    self.steps.pop();
                }
            }
            _ => {}
        }
    }
}

fn matches_like(text: &[char], pattern: &[char], escape: char) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some(('%', rest)) => (0..=text.len()).any(|skip| matches_like(&text[skip..], rest, escape)),
        Some(('_', rest)) => !text.is_empty() && matches_like(&text[1..], rest, escape),
        Some((&character, rest)) => {
            let (literal, rest) = match rest.split_first() {
                Some((&escaped, after)) if character == escape => (escaped, after),
                _ => (character, rest),
            };
            text.first() == Some(&literal) && matches_like(&text[1..], rest, escape)
        }
    }
}
