use std::sync::LazyLock;
use turso_core::dialect::{DialectType, TypeRules};
use turso_core::{ConversionErrorKind, Func, LimboError, Result, TypeConversionError, Value};

mod binary;
mod bit;
mod decimal;
pub(crate) mod decimal_number;
mod float;
mod integer;
mod labels;
mod number_text;
mod spatial;
mod string;
mod temporal;
mod year;

pub(crate) const DATA_TRUNCATED: &str = "Data truncated";

pub(crate) const NOT_A_GEOMETRY: &str =
    "Cannot get geometry object from data you send to the GEOMETRY field";

pub(crate) fn rules() -> &'static TypeRules {
    &RULES
}

pub(crate) fn resolve(name: &str, arg_count: usize) -> Option<Func> {
    let name = name.to_ascii_uppercase();
    conversion(&name, arg_count).map(|_| Func::Dialect(name))
}

pub(crate) fn execute(name: &str, args: &[Value]) -> Option<Result<Value>> {
    conversion(name, args.len()).map(|conversion| conversion(args))
}

static RULES: LazyLock<TypeRules> = LazyLock::new(|| TypeRules {
    builtin_behaviour_overrides: &[],
    cast_uses_parameter_defaults: false,
    dialect_types: dialect_types().leak(),
    class_operators: "OPERATOR '=' (integer, integer) OPERATOR '<' (integer, integer) \
        OPERATOR '/' (integer, integer) RETURNS decimal __mysql_divide_nn",
    class_conversions: &[],
    decimal_literal_type: Some("decimal"),
    string_literals_are_text: false,
    result_parameters: Some(decimal::result_parameters),
    zero_generates_key: true,
    class_column_types: &[
        ("integer", "bigint"),
        ("real", "double"),
        ("text", "longtext"),
        ("blob", "longblob"),
        ("any", "longtext"),
    ],
});

fn dialect_types() -> Vec<DialectType> {
    integer::TYPES
        .into_iter()
        .chain(string::TYPES)
        .chain(binary::TYPES)
        .chain(float::TYPES)
        .chain(decimal::TYPES)
        .chain(temporal::TYPES)
        .chain(year::TYPES)
        .chain(bit::TYPES)
        .chain(labels::TYPES)
        .chain(spatial::TYPES)
        .collect()
}

type Conversion = fn(&[Value]) -> Result<Value>;

fn conversion(name: &str, arg_count: usize) -> Option<Conversion> {
    if !name.starts_with("__MYSQL_") {
        return None;
    }
    let conversion: Conversion = match (name, arg_count) {
        ("__MYSQL_SQLITE_CAST", 2) => sqlite_cast,
        ("__MYSQL_ASSIGN_INTEGER", 1) => integer::assign_integer,
        ("__MYSQL_ASSIGN_UNSIGNED_INTEGER", 1) => integer::assign_unsigned_integer,
        ("__MYSQL_ASSIGN_UNSIGNED_BIGINT", 1) => integer::assign_unsigned_bigint,
        ("__MYSQL_UNSIGNED_EQUALS_VALUE", 2) => integer::unsigned_equals_value,
        ("__MYSQL_CAST_AS_SIGNED", 1) => integer::cast_as_signed,
        ("__MYSQL_CAST_AS_UNSIGNED", 1) => integer::cast_as_unsigned,
        ("__MYSQL_CAST_AS_INTEGER_UNSUPPORTED", 1) => integer::cast_as_integer_unsupported,
        ("__MYSQL_DECIMAL_CAST_AS_SIGNED", 1) => decimal::decimal_cast_as_signed,
        ("__MYSQL_DECIMAL_CAST_AS_UNSIGNED", 1) => decimal::decimal_cast_as_unsigned,
        ("__MYSQL_VALUE_EQUALS_UNSIGNED", 2) => integer::value_equals_unsigned,
        ("__MYSQL_ASSIGN_CHAR", 2) => string::assign_char,
        ("__MYSQL_ASSIGN_VARCHAR", 2) => string::assign_varchar,
        ("__MYSQL_ASSIGN_TEXT", 2) => string::assign_text,
        ("__MYSQL_ASSIGN_BINARY", 2) => binary::assign_binary,
        ("__MYSQL_ASSIGN_VARBINARY", 2) => binary::assign_varbinary,
        ("__MYSQL_ASSIGN_DOUBLE", 4) => float::assign_double,
        ("__MYSQL_ASSIGN_BIT", 2) => bit::assign_bit,
        ("__MYSQL_ASSIGN_GEOMETRY", 1) => spatial::assign_geometry,
        _ => return decimal::function(name, arg_count),
    };
    Some(conversion)
}

fn sqlite_cast(args: &[Value]) -> Result<Value> {
    let [value, Value::Text(type_name)] = args else {
        return Err(LimboError::InternalError(
            "__mysql_sqlite_cast takes a value and a type name".to_string(),
        ));
    };
    Ok(value.exec_cast(type_name.as_str())?)
}

pub(crate) fn unsupported_value(message: String) -> LimboError {
    LimboError::InvalidArgument(format!(
        "{UNSUPPORTED_MYSQL_VALUE} {message} is not supported yet"
    ))
}

pub(crate) const UNSUPPORTED_MYSQL_VALUE: &str = "unsupported MySQL value:";

fn out_of_range() -> LimboError {
    TypeConversionError::raise(ConversionErrorKind::OutOfRange, "Out of range value")
}

fn data_truncated() -> LimboError {
    TypeConversionError::raise(ConversionErrorKind::InvalidValue, DATA_TRUNCATED)
}

fn data_too_long() -> LimboError {
    TypeConversionError::raise(ConversionErrorKind::TooLong, "Data too long")
}

fn binary_string_in_number_column(type_name: &str) -> LimboError {
    invalid_value(format!(
        "Incorrect {type_name} value: binary strings and hexadecimal literals \
        cannot be stored in number columns yet"
    ))
}

fn invalid_value(message: String) -> LimboError {
    TypeConversionError::raise(ConversionErrorKind::InvalidValue, message)
}

fn integer_parameter(value: &Value) -> Option<u64> {
    match value {
        Value::Numeric(turso_core::Numeric::Integer(number)) => u64::try_from(*number).ok(),
        _ => None,
    }
}
