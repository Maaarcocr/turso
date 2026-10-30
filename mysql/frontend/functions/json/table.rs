//! `JSON_TABLE`. `__mysql_json_table(document, spec)` computes the rows of
//! the table as MySQL does and returns them as a JSON array of rows, each an
//! array of tagged cells; `__mysql_json_table_cell(row, index)` reads one.
//! The spec, which the lowering writes, is a JSON object:
//! `{"path": row path, "columns": [column, ...]}`, where a column is
//! `{"ordinality": true}`, `{"exists": path, "type": type}`,
//! `{"path": path, "type": type, "empty": handling, "error": handling}` or
//! `{"nested": path, "columns": [...]}`; a type is `{"kind": "integer",
//! "min": n, "max": n}`, `{"kind": "text", "length": n}`, `{"kind":
//! "double"}`, `{"kind": "float"}` or `{"kind": "json"}`; and a handling is
//! `"null"`, `"error"` or `{"default": text}`.

use super::document::{format_double, Json};
use super::parse::parse;
use super::path::{Path, PathError};
use super::{json_result, mysql_error, text_result, Call};
use crate::types::unsupported_value;
use turso_core::{LimboError, Numeric, Result, Value};

pub(super) fn rows(call: &Call) -> Result<Value> {
    let Value::Text(spec) = &call.args[1] else {
        return Err(internal("a JSON_TABLE spec is text"));
    };
    let spec = Spec::read(spec.as_str())?;
    let Some(document) = call.document(0)? else {
        return Ok(text_result("[]"));
    };
    let mut rows = Vec::new();
    for (index, value) in spec.path.select(&document, true).into_iter().enumerate() {
        rows.extend(expand(value, &spec.columns, index + 1, rows.len())?);
    }
    let mut text = String::from("[");
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        text.push('[');
        for (index, cell) in row.iter().enumerate() {
            if index > 0 {
                text.push(',');
            }
            cell.write(&mut text);
        }
        text.push(']');
    }
    text.push(']');
    Ok(text_result(text))
}

pub(super) fn cell(call: &Call) -> Result<Value> {
    let (Value::Text(row), Value::Numeric(Numeric::Integer(index))) =
        (&call.args[0], &call.args[1])
    else {
        return Err(internal("a JSON_TABLE cell is read from its row by index"));
    };
    let Ok(Json::Array(cells)) = parse(row.as_str()) else {
        return Err(internal("a JSON_TABLE row is an array"));
    };
    let cell = usize::try_from(*index)
        .ok()
        .and_then(|index| cells.get(index))
        .ok_or_else(|| internal("a JSON_TABLE row has each of its columns"))?;
    Ok(match cell {
        Json::Null => Value::Null,
        Json::Array(tagged) => match tagged.as_slice() {
            [Json::String(tag), Json::String(payload)] => match tag.as_str() {
                "i" => Value::from_i64(
                    payload
                        .parse()
                        .map_err(|_| internal("an integer cell holds an integer"))?,
                ),
                "d" => Value::from_f64(f64::from_bits(
                    payload
                        .parse()
                        .map_err(|_| internal("a double cell holds its bits"))?,
                )),
                "s" => text_result(payload.clone()),
                "j" => json_result(
                    &parse(payload).map_err(|_| internal("a JSON cell holds a document"))?,
                ),
                _ => return Err(internal("a JSON_TABLE cell has a known tag")),
            },
            _ => return Err(internal("a JSON_TABLE cell is a tagged pair")),
        },
        _ => return Err(internal("a JSON_TABLE cell is NULL or a tagged pair")),
    })
}

enum Cell {
    Null,
    Integer(i64),
    Double(f64),
    Text(String),
    Json(String),
}

impl Cell {
    fn write(&self, text: &mut String) {
        let (tag, payload) = match self {
            Cell::Null => {
                text.push_str("null");
                return;
            }
            Cell::Integer(value) => ("i", value.to_string()),
            Cell::Double(value) => ("d", value.to_bits().to_string()),
            Cell::Text(value) => ("s", value.clone()),
            Cell::Json(value) => ("j", value.clone()),
        };
        text.push_str("[\"");
        text.push_str(tag);
        text.push_str("\",");
        text.push_str(&Json::String(payload).to_text());
        text.push(']');
    }
}

struct Spec {
    path: Path,
    columns: Vec<Column>,
}

enum Column {
    Ordinality,
    Exists {
        path: Path,
        kind: Kind,
    },
    Value {
        name: String,
        path: Path,
        kind: Kind,
        empty: Handling,
        error: Handling,
    },
    Nested {
        path: Path,
        columns: Vec<Column>,
    },
}

#[derive(Clone, Copy)]
enum Kind {
    Integer { min: i64, max: i64 },
    Text { length: usize },
    Double,
    Float,
    Json,
}

enum Handling {
    Null,
    Error,
    Default(String),
}

impl Spec {
    fn read(text: &str) -> Result<Spec> {
        let Ok(Json::Object(spec)) = parse(text) else {
            return Err(internal("a JSON_TABLE spec is an object"));
        };
        Ok(Spec {
            path: path(spec.get("path"))?,
            columns: columns(spec.get("columns"))?,
        })
    }
}

fn columns(value: Option<&Json>) -> Result<Vec<Column>> {
    let Some(Json::Array(list)) = value else {
        return Err(internal("JSON_TABLE columns are an array"));
    };
    list.iter()
        .map(|column| {
            let Json::Object(column) = column else {
                return Err(internal("a JSON_TABLE column is an object"));
            };
            if let Some(nested) = column.get("nested") {
                return Ok(Column::Nested {
                    path: path(Some(nested))?,
                    columns: columns(column.get("columns"))?,
                });
            }
            if column.get("ordinality").is_some() {
                return Ok(Column::Ordinality);
            }
            let kind = kind(column.get("type"))?;
            if let Some(exists) = column.get("exists") {
                return Ok(Column::Exists {
                    path: path(Some(exists))?,
                    kind,
                });
            }
            let Some(Json::String(name)) = column.get("name") else {
                return Err(internal("a JSON_TABLE value column has its name"));
            };
            Ok(Column::Value {
                name: name.clone(),
                path: path(column.get("path"))?,
                kind,
                empty: handling(column.get("empty"))?,
                error: handling(column.get("error"))?,
            })
        })
        .collect()
}

fn path(value: Option<&Json>) -> Result<Path> {
    let Some(Json::String(text)) = value else {
        return Err(internal("a JSON_TABLE path is a string"));
    };
    Path::parse(text).map_err(|PathError(position)| {
        mysql_error(
            3143,
            "42000",
            format!(
                "Invalid JSON path expression. The error is around character position {position}."
            ),
        )
    })
}

fn kind(value: Option<&Json>) -> Result<Kind> {
    let Some(Json::Object(kind)) = value else {
        return Err(internal("a JSON_TABLE type is an object"));
    };
    let number = |key: &str| match kind.get(key) {
        Some(Json::Integer(value)) => Ok(*value),
        Some(Json::Unsigned(value)) => {
            i64::try_from(*value).map_err(|_| internal("a JSON_TABLE bound fits a BIGINT"))
        }
        _ => Err(internal("a JSON_TABLE type has its bounds")),
    };
    Ok(match kind.get("kind") {
        Some(Json::String(name)) => match name.as_str() {
            "integer" => Kind::Integer {
                min: number("min")?,
                max: number("max")?,
            },
            "text" => Kind::Text {
                length: usize::try_from(number("length")?)
                    .map_err(|_| internal("a JSON_TABLE length is a count"))?,
            },
            "double" => Kind::Double,
            "float" => Kind::Float,
            "json" => Kind::Json,
            _ => return Err(internal("a JSON_TABLE type has a known kind")),
        },
        _ => return Err(internal("a JSON_TABLE type has a kind")),
    })
}

fn handling(value: Option<&Json>) -> Result<Handling> {
    Ok(match value {
        Some(Json::String(name)) if name == "null" => Handling::Null,
        Some(Json::String(name)) if name == "error" => Handling::Error,
        Some(Json::Object(default)) => match default.get("default") {
            Some(Json::String(text)) => Handling::Default(text.clone()),
            _ => return Err(internal("a JSON_TABLE default is a string")),
        },
        _ => return Err(internal("a JSON_TABLE handling is known")),
    })
}

/// The rows of one match of a path whose columns are `columns`: its own
/// cells, then one row for each row of each nested path, with the cells of
/// the other nested paths NULL, or one row with all of them NULL when no
/// nested path matches. `row` is the number of rows before these.
fn expand(value: &Json, columns: &[Column], ordinal: usize, row: usize) -> Result<Vec<Vec<Cell>>> {
    let mut own = Vec::with_capacity(columns.len());
    let mut widths = Vec::new();
    let mut nested = Vec::new();
    for column in columns {
        match column {
            Column::Ordinality => own.push(Some(Cell::Integer(
                i64::try_from(ordinal).expect("an ordinal fits a BIGINT"),
            ))),
            Column::Exists { path, kind } => {
                let exists = i64::from(!path.select(value, true).is_empty());
                own.push(Some(match kind {
                    Kind::Text { .. } => Cell::Text(exists.to_string()),
                    Kind::Integer { .. } => Cell::Integer(exists),
                    _ => {
                        return Err(unsupported_value(
                            "EXISTS PATH of a JSON_TABLE column that is not an integer or a string"
                                .to_string(),
                        ))
                    }
                }));
            }
            Column::Value {
                name,
                path,
                kind,
                empty,
                error,
            } => own.push(Some(value_cell(
                value, name, path, *kind, empty, error, row,
            )?)),
            Column::Nested { path, columns } => {
                let mut rows = Vec::new();
                for (index, matched) in path.select(value, true).into_iter().enumerate() {
                    rows.extend(expand(matched, columns, index + 1, row + rows.len())?);
                }
                own.push(None);
                widths.push(width(columns));
                nested.push(rows);
            }
        }
    }
    let assemble = |active: Option<(usize, Vec<Cell>)>| -> Vec<Cell> {
        let mut cells = Vec::new();
        let mut nested_index = 0;
        let mut active = active;
        for cell in &own {
            match cell {
                Some(cell) => cells.push(cell.copy()),
                None => {
                    let width = widths[nested_index];
                    match active.take() {
                        Some((index, row)) if index == nested_index => cells.extend(row),
                        other => {
                            active = other;
                            cells.extend((0..width).map(|_| Cell::Null));
                        }
                    }
                    nested_index += 1;
                }
            }
        }
        cells
    };
    if nested.is_empty() {
        return Ok(vec![assemble(None)]);
    }
    let mut rows = Vec::new();
    for (index, nested_rows) in nested.into_iter().enumerate() {
        for nested_row in nested_rows {
            rows.push((index, nested_row));
        }
    }
    if rows.is_empty() {
        return Ok(vec![assemble(None)]);
    }
    Ok(rows
        .into_iter()
        .map(|active| assemble(Some(active)))
        .collect())
}

fn width(columns: &[Column]) -> usize {
    columns
        .iter()
        .map(|column| match column {
            Column::Nested { columns, .. } => width(columns),
            _ => 1,
        })
        .sum()
}

impl Cell {
    fn copy(&self) -> Cell {
        match self {
            Cell::Null => Cell::Null,
            Cell::Integer(value) => Cell::Integer(*value),
            Cell::Double(value) => Cell::Double(*value),
            Cell::Text(value) => Cell::Text(value.clone()),
            Cell::Json(value) => Cell::Json(value.clone()),
        }
    }
}

/// What converting a JSON value to a column type gives: a cell, an error
/// that the column's `ON ERROR` handles, or a conversion this does not know
/// MySQL's answer for.
enum Converted {
    Cell(Cell),
    Error,
    Refused,
}

fn value_cell(
    value: &Json,
    name: &str,
    path: &Path,
    kind: Kind,
    empty: &Handling,
    error: &Handling,
    row: usize,
) -> Result<Cell> {
    let found = path.select(value, true);
    let converted = match found.as_slice() {
        [] => {
            return handle(empty, kind, || {
                mysql_error(
                    3665,
                    "22035",
                    format!("Missing value for JSON_TABLE column '{name}'"),
                )
            })
        }
        [value] => convert(value, kind),
        _ => Converted::Error,
    };
    match converted {
        Converted::Cell(cell) => Ok(cell),
        Converted::Error => handle(error, kind, || {
            match kind {
            Kind::Integer { .. } if row == 0 => mysql_error(
                3156,
                "22018",
                format!("Invalid JSON value for CAST to INTEGER from column {name} at row 1"),
            ),
            _ => unsupported_value(format!(
                "the error of JSON_TABLE column `{name}`, whose message MySQL words by its type and row,"
            )),
        }
        }),
        Converted::Refused => Err(refused(value)),
    }
}

fn handle(handling: &Handling, kind: Kind, error: impl FnOnce() -> LimboError) -> Result<Cell> {
    match handling {
        Handling::Null => Ok(Cell::Null),
        Handling::Error => Err(error()),
        Handling::Default(text) => {
            let value = match kind {
                Kind::Json => parse(text).map_err(|_| {
                    unsupported_value(format!("JSON_TABLE default `{text}`, which is not JSON,"))
                })?,
                _ => Json::String(text.clone()),
            };
            match convert(&value, kind) {
                Converted::Cell(cell) => Ok(cell),
                Converted::Error | Converted::Refused => Err(unsupported_value(format!(
                    "JSON_TABLE default `{text}`, which does not convert to its column,"
                ))),
            }
        }
    }
}

fn convert(value: &Json, kind: Kind) -> Converted {
    match (kind, value) {
        (Kind::Json, Json::Null) => Converted::Refused,
        (Kind::Json, value) => Converted::Cell(Cell::Json(value.to_text())),
        (_, Json::Null) => Converted::Cell(Cell::Null),
        (_, Json::Array(_) | Json::Object(_)) => Converted::Error,
        (_, Json::Opaque(_) | Json::Boolean(_)) => Converted::Refused,
        (Kind::Integer { min, max }, value) => {
            let integer = match value {
                Json::Integer(value) => Some(i128::from(*value)),
                Json::Unsigned(value) => Some(i128::from(*value)),
                Json::String(text) => match integer_text(text) {
                    Some(value) => Some(value),
                    None if starts_like_number(text) => return Converted::Refused,
                    None => return Converted::Error,
                },
                Json::Double(_) => return Converted::Refused,
                _ => unreachable!("other values are handled above"),
            };
            match integer {
                Some(value) if (i128::from(min)..=i128::from(max)).contains(&value) => {
                    Converted::Cell(Cell::Integer(
                        i64::try_from(value).expect("the range is within BIGINT"),
                    ))
                }
                _ => Converted::Error,
            }
        }
        (Kind::Text { length }, value) => {
            let text = match value {
                Json::String(text) => text.clone(),
                Json::Integer(_) | Json::Unsigned(_) | Json::Double(_) => value.to_text(),
                _ => unreachable!("other values are handled above"),
            };
            if text.chars().count() > length {
                Converted::Refused
            } else {
                Converted::Cell(Cell::Text(text))
            }
        }
        (Kind::Double | Kind::Float, value) => {
            let double = match value {
                Json::Integer(value) if value.unsigned_abs() <= 1 << 53 => *value as f64,
                Json::Unsigned(value) if *value <= 1 << 53 => *value as f64,
                Json::Double(value) => *value,
                Json::String(text) if !starts_like_number(text) => return Converted::Error,
                _ => return Converted::Refused,
            };
            if matches!(kind, Kind::Float) && f64::from(double as f32) != double {
                return Converted::Refused;
            }
            Converted::Cell(Cell::Double(double))
        }
    }
}

/// The integer a string holds when it is only an optionally signed run of
/// digits.
fn integer_text(text: &str) -> Option<i128> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || digits.len() > 30 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Whether MySQL may read part of `text` as a number.
fn starts_like_number(text: &str) -> bool {
    text.trim_start()
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.'))
}

fn refused(value: &Json) -> LimboError {
    let shown = match value {
        Json::Double(value) => format_double(*value),
        other => other.to_text(),
    };
    unsupported_value(format!(
        "the JSON_TABLE value `{shown}`, whose conversion to its column MySQL does with its own rules,"
    ))
}

fn internal(message: &str) -> LimboError {
    LimboError::InternalError(message.to_string())
}
