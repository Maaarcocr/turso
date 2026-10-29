use std::borrow::Cow;

use turso_core::dialect::ArgumentType;
use turso_core::types::{Text, TextSubtype};
use turso_core::{LimboError, Numeric, Result, Value};

mod build;
mod compare;
mod document;
mod modify;
mod parse;
mod path;
mod read;
mod search;

use document::{format_double, Json};
use parse::{parse, ParseError};
use path::{Path, PathError};

pub(super) use document::format_json_double;

pub(super) fn accepts(name: &str, arg_count: usize) -> bool {
    Function::from_name(name).is_some() || compare::accepts(name, arg_count)
}

pub(super) fn execute_typed(
    name: &str,
    args: &[Value],
    arg_types: &[ArgumentType],
) -> Option<Result<Value>> {
    compare::execute_typed(name, args, arg_types)
}

pub(super) fn execute(name: &str, args: &[Value]) -> Option<Result<Value>> {
    if compare::accepts(name, args.len()) {
        return compare::execute_typed(name, args, &vec![ArgumentType::Unknown; args.len()]);
    }
    let function = Function::from_name(name)?;
    if !function.accepts_arg_count(args.len()) {
        return Some(Err(mysql_error(
            1582,
            "42000",
            format!("Incorrect parameter count in the call to native function '{name}'"),
        )));
    }
    let call = Call { function, args };
    Some(match function {
        Function::Array => build::array(&call),
        Function::Object => build::object(&call),
        Function::ArrayAggregate => build::array_aggregate(&call),
        Function::ObjectAggregate => build::object_aggregate(&call),
        Function::Quote => build::quote_string(&call),
        Function::CastAsJson => build::cast_as_json(&call),
        Function::CastBooleanAsJson => build::cast_boolean_as_json(&call),
        Function::Extract => read::extract(&call),
        Function::Value => read::value(&call),
        Function::Unquote => read::unquote(&call),
        Function::Type => read::type_name(&call),
        Function::Valid => read::valid(&call),
        Function::Keys => read::keys(&call),
        Function::Length => read::length(&call),
        Function::Depth => read::depth(&call),
        Function::Pretty => read::pretty(&call),
        Function::Contains => search::contains(&call),
        Function::ContainsPath => search::contains_path(&call),
        Function::Overlaps => search::overlaps(&call),
        Function::Search => search::search(&call),
        Function::MemberOf => search::member_of(&call),
        Function::Set => modify::set(&call, modify::Change::Set),
        Function::Insert => modify::set(&call, modify::Change::Insert),
        Function::Replace => modify::set(&call, modify::Change::Replace),
        Function::Remove => modify::remove(&call),
        Function::ArrayAppend => modify::array_append(&call),
        Function::ArrayInsert => modify::array_insert(&call),
        Function::MergePreserve => modify::merge_preserve(&call),
        Function::MergePatch => modify::merge_patch(&call),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Function {
    Array,
    Object,
    ArrayAggregate,
    ObjectAggregate,
    Quote,
    CastAsJson,
    CastBooleanAsJson,
    Extract,
    Value,
    Unquote,
    Type,
    Valid,
    Keys,
    Length,
    Depth,
    Pretty,
    Contains,
    ContainsPath,
    Overlaps,
    Search,
    MemberOf,
    Set,
    Insert,
    Replace,
    Remove,
    ArrayAppend,
    ArrayInsert,
    MergePreserve,
    MergePatch,
}

impl Function {
    fn from_name(name: &str) -> Option<Function> {
        Some(match name {
            "JSON_ARRAY" => Function::Array,
            "JSON_OBJECT" => Function::Object,
            "__MYSQL_JSON_ARRAYAGG" => Function::ArrayAggregate,
            "__MYSQL_JSON_OBJECTAGG" => Function::ObjectAggregate,
            "JSON_QUOTE" => Function::Quote,
            "__MYSQL_CAST_AS_JSON" => Function::CastAsJson,
            "__MYSQL_CAST_BOOLEAN_AS_JSON" => Function::CastBooleanAsJson,
            "JSON_EXTRACT" => Function::Extract,
            "JSON_VALUE" => Function::Value,
            "JSON_UNQUOTE" => Function::Unquote,
            "JSON_TYPE" => Function::Type,
            "JSON_VALID" => Function::Valid,
            "JSON_KEYS" => Function::Keys,
            "JSON_LENGTH" => Function::Length,
            "JSON_DEPTH" => Function::Depth,
            "JSON_PRETTY" => Function::Pretty,
            "JSON_CONTAINS" => Function::Contains,
            "JSON_CONTAINS_PATH" => Function::ContainsPath,
            "JSON_OVERLAPS" => Function::Overlaps,
            "JSON_SEARCH" => Function::Search,
            "__MYSQL_MEMBER_OF" => Function::MemberOf,
            "JSON_SET" => Function::Set,
            "JSON_INSERT" => Function::Insert,
            "JSON_REPLACE" => Function::Replace,
            "JSON_REMOVE" => Function::Remove,
            "JSON_ARRAY_APPEND" => Function::ArrayAppend,
            "JSON_ARRAY_INSERT" => Function::ArrayInsert,
            "JSON_MERGE" | "JSON_MERGE_PRESERVE" => Function::MergePreserve,
            "JSON_MERGE_PATCH" => Function::MergePatch,
            _ => return None,
        })
    }

    fn accepts_arg_count(self, count: usize) -> bool {
        match self {
            Function::Array => true,
            Function::Object => count % 2 == 0,
            Function::Quote
            | Function::ArrayAggregate
            | Function::ObjectAggregate
            | Function::CastAsJson
            | Function::CastBooleanAsJson
            | Function::Unquote
            | Function::Type
            | Function::Valid
            | Function::Depth
            | Function::Pretty => count == 1,
            Function::Value | Function::Overlaps | Function::MemberOf => count == 2,
            Function::Keys | Function::Length => (1..=2).contains(&count),
            Function::Contains => (2..=3).contains(&count),
            Function::Extract | Function::Remove => count >= 2,
            Function::MergePreserve | Function::MergePatch => count >= 2,
            Function::ContainsPath | Function::Search => count >= 3,
            Function::Set
            | Function::Insert
            | Function::Replace
            | Function::ArrayAppend
            | Function::ArrayInsert => count >= 3 && count % 2 == 1,
        }
    }

    fn diagnostic_name(self) -> &'static str {
        match self {
            Function::Array => "json_array",
            Function::Object => "json_object",
            Function::ArrayAggregate => "json_arrayagg",
            Function::ObjectAggregate => "json_objectagg",
            Function::Quote => "json_quote",
            Function::CastAsJson | Function::CastBooleanAsJson => "cast_as_json",
            Function::Extract => "json_extract",
            Function::Value => "json_value",
            Function::Unquote => "json_unquote",
            Function::Type => "json_type",
            Function::Valid => "json_valid",
            Function::Keys => "json_keys",
            Function::Length => "json_length",
            Function::Depth => "json_depth",
            Function::Pretty => "json_pretty",
            Function::Contains => "json_contains",
            Function::ContainsPath => "json_contains_path",
            Function::Overlaps => "json_overlaps",
            Function::Search => "json_search",
            Function::MemberOf => "member of",
            Function::Set => "json_set",
            Function::Insert => "json_insert",
            Function::Replace => "json_replace",
            Function::Remove => "json_remove",
            Function::ArrayAppend => "json_array_append",
            Function::ArrayInsert => "json_array_insert",
            Function::MergePreserve => "json_merge_preserve",
            Function::MergePatch => "json_merge_patch",
        }
    }
}

struct Call<'a> {
    function: Function,
    args: &'a [Value],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OneOrAll {
    One,
    All,
}

impl Call<'_> {
    fn document(&self, index: usize) -> Result<Option<Json>> {
        match &self.args[index] {
            Value::Null => Ok(None),
            Value::Text(text) => parse(text.as_str())
                .map(Some)
                .map_err(|error| self.invalid_json_text(index, error)),
            Value::Blob(_) => Err(binary_string_error()),
            Value::Numeric(_) => Err(mysql_error(
                3146,
                "22032",
                format!(
                    "Invalid data type for JSON data in argument {} to function {}; a JSON string or JSON type is required.",
                    index + 1,
                    self.function.diagnostic_name()
                ),
            )),
        }
    }

    fn value(&self, index: usize) -> Result<Json> {
        Ok(match &self.args[index] {
            Value::Null => Json::Null,
            Value::Numeric(Numeric::Integer(value)) => Json::Integer(*value),
            Value::Numeric(Numeric::Float(value)) => Json::Double(f64::from(*value)),
            Value::Text(text) if is_json(text) => {
                parse(text.as_str()).map_err(|error| self.invalid_json_text(index, error))?
            }
            Value::Text(text) => Json::String(text.as_str().to_string()),
            Value::Blob(bytes) => Json::Opaque(bytes.to_vec()),
        })
    }

    fn path(&self, index: usize) -> Result<Option<Path>> {
        let Some(text) = sql_string(&self.args[index]) else {
            return Ok(None);
        };
        Path::parse(&text).map(Some).map_err(|PathError(position)| {
            mysql_error(
                3143,
                "42000",
                format!(
                    "Invalid JSON path expression. The error is around character position {position}."
                ),
            )
        })
    }

    fn single_value_path(&self, index: usize) -> Result<Option<Path>> {
        let path = self.path(index)?;
        if path.as_ref().is_some_and(Path::can_match_many) {
            return Err(mysql_error(
                3149,
                "42000",
                "In this situation, path expressions may not contain the * and ** tokens or an array range.",
            ));
        }
        Ok(path)
    }

    fn one_or_all(&self, index: usize) -> Result<Option<OneOrAll>> {
        let Some(text) = sql_string(&self.args[index]) else {
            return Ok(None);
        };
        if text.eq_ignore_ascii_case("one") {
            Ok(Some(OneOrAll::One))
        } else if text.eq_ignore_ascii_case("all") {
            Ok(Some(OneOrAll::All))
        } else {
            Err(mysql_error(
                3154,
                "42000",
                format!(
                    "The oneOrAll argument to {} may take these values: 'one' or 'all'.",
                    self.function.diagnostic_name()
                ),
            ))
        }
    }

    fn invalid_json_text(&self, index: usize, error: ParseError) -> LimboError {
        match error {
            ParseError::Syntax { message, offset } => mysql_error(
                3141,
                "22032",
                format!(
                    "Invalid JSON text in argument {} to function {}: \"{message}\" at position {offset}.",
                    index + 1,
                    self.function.diagnostic_name()
                ),
            ),
            ParseError::TooDeep => too_deep_error(),
        }
    }

    fn incorrect_type(&self, index: usize) -> LimboError {
        mysql_error(
            3064,
            "HY000",
            format!(
                "Incorrect type for argument {} in function {}.",
                index + 1,
                self.function.diagnostic_name()
            ),
        )
    }
}

/// The text MySQL stores for a value assigned to a JSON column: the
/// document in MySQL's form (`{"a": [1, 2], "b": 2, "aa": true}`, keys
/// ordered by length and then bytes, the last of duplicate keys kept). Text
/// that is not a JSON document fails (3140), as do numbers, which MySQL does
/// not read as JSON text, and binary strings (3144).
pub(crate) fn assign_column(args: &[Value]) -> Result<Value> {
    let invalid = |message: &str, offset: usize| {
        mysql_error(
            3140,
            "22032",
            format!("Invalid JSON text: \"{message}\" at position {offset} in value for column."),
        )
    };
    match &args[0] {
        Value::Null => Ok(Value::Null),
        Value::Text(text) => match parse(text.as_str()) {
            Ok(document) => Ok(Value::build_text(document.to_text())),
            Err(ParseError::Syntax { message, offset }) => Err(invalid(message, offset)),
            Err(ParseError::TooDeep) => Err(too_deep_error()),
        },
        Value::Blob(_) => Err(binary_string_error()),
        Value::Numeric(_) => Err(invalid("not a JSON text, may need CAST", 0)),
    }
}

fn is_json(text: &Text) -> bool {
    text.subtype == TextSubtype::Json
}

fn sql_string(value: &Value) -> Option<Cow<'_, str>> {
    match value {
        Value::Null => None,
        Value::Numeric(Numeric::Integer(value)) => Some(Cow::Owned(value.to_string())),
        Value::Numeric(Numeric::Float(value)) => Some(Cow::Owned(format_double(f64::from(*value)))),
        Value::Text(text) => Some(Cow::Borrowed(text.as_str())),
        Value::Blob(bytes) => Some(String::from_utf8_lossy(bytes)),
    }
}

fn json_result(value: &Json) -> Value {
    Value::Text(Text::json(value.to_text()))
}

fn text_result(text: impl Into<Cow<'static, str>>) -> Value {
    Value::Text(Text::new(text))
}

fn boolean_result(value: bool) -> Value {
    Value::from_i64(i64::from(value))
}

fn binary_string_error() -> LimboError {
    mysql_error(
        3144,
        "22032",
        "Cannot create a JSON value from a string with CHARACTER SET 'binary'.",
    )
}

fn too_deep_error() -> LimboError {
    mysql_error(
        3157,
        "22032",
        "The JSON document exceeds the maximum depth.",
    )
}

fn mysql_error(code: u16, state: &str, message: impl std::fmt::Display) -> LimboError {
    LimboError::SqlError(format!("ERROR {code} ({state}): {message}"))
}
