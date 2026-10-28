use super::DecimalArgument;
use turso_core::{Result, Value};

mod collation;
mod compare;
mod convert;
mod hash;
mod network;
mod numeric;
mod regexp;
mod text;

pub(crate) use convert::format_double;

type Function = fn(&[Value]) -> Result<Value>;

/// A value read as MySQL reads it where it needs a DOUBLE.
pub(super) fn double_argument(value: &Value) -> f64 {
    convert::double(value).unwrap_or(0.0)
}

/// The integer at the start of a string, as MySQL reads a string where it
/// needs an integer.
pub(super) fn leading_integer(bytes: &[u8]) -> i64 {
    convert::string_to_integer(bytes)
}

/// How a string function reads a DECIMAL argument at `position`: as its
/// text, or as a count or position. `Double` for every other function.
pub(super) fn decimal_argument(name: &str, position: usize) -> DecimalArgument {
    if name == "CHAR" {
        return DecimalArgument::Count;
    }
    let counts: &[usize] = match name {
        "LEFT" | "RIGHT" | "LPAD" | "RPAD" | "REPEAT" | "SHA2" => &[1],
        "SUBSTR" | "SUBSTRING" | "MID" | "INSERT" => &[1, 2],
        "LOCATE" | "SUBSTRING_INDEX" | "REGEXP_SUBSTR" => &[2, 3],
        "REGEXP_INSTR" => &[2, 3, 4],
        "REGEXP_REPLACE" => &[3, 4],
        "ELT" | "MAKE_SET" | "SPACE" | "HEX" => &[0],
        "EXPORT_SET" => &[0, 4],
        "ASCII" | "BIT_LENGTH" | "CHAR_LENGTH" | "CHARACTER_LENGTH" | "CONCAT" | "CONCAT_WS"
        | "FIND_IN_SET" | "INSTR" | "LCASE" | "LENGTH" | "LOWER" | "OCTET_LENGTH" | "ORD"
        | "POSITION" | "REPLACE" | "REVERSE" | "STRCMP" | "UCASE" | "UPPER" | "CRC32" | "MD5"
        | "SHA" | "SHA1" | "TO_BASE64" | "REGEXP" | "REGEXP_LIKE" => &[],
        _ => return DecimalArgument::Double,
    };
    if counts.contains(&position) {
        DecimalArgument::Count
    } else {
        DecimalArgument::Text
    }
}

pub(super) fn accepts(name: &str, arg_count: usize) -> bool {
    function(name, arg_count).is_some()
}

pub(super) fn execute(name: &str, args: &[Value]) -> Option<Result<Value>> {
    function(name, args.len()).map(|function| function(args))
}

fn function(name: &str, arg_count: usize) -> Option<Function> {
    let function: Function = match (name, arg_count) {
        ("ABS", 1) => numeric::abs,
        ("ACOS", 1) => numeric::acos,
        ("ASIN", 1) => numeric::asin,
        ("ATAN", 1 | 2) => numeric::atan,
        ("ATAN2", 2) => numeric::atan2,
        ("BIN", 1) => numeric::bin,
        ("BIT_COUNT", 1) => numeric::bit_count,
        ("CEIL" | "CEILING", 1) => numeric::ceiling,
        ("CONV", 3) => numeric::conv,
        ("COS", 1) => numeric::cos,
        ("COT", 1) => numeric::cot,
        ("DEGREES", 1) => numeric::degrees,
        ("EXP", 1) => numeric::exp,
        ("FLOOR", 1) => numeric::floor,
        ("LN", 1) => numeric::ln,
        ("LOG", 1 | 2) => numeric::log,
        ("LOG10", 1) => numeric::log10,
        ("LOG2", 1) => numeric::log2,
        ("MOD", 2) => numeric::modulo,
        ("OCT", 1) => numeric::oct,
        ("POW" | "POWER", 2) => numeric::pow,
        ("RADIANS", 1) => numeric::radians,
        ("ROUND", 1 | 2) => numeric::round,
        ("SIGN", 1) => numeric::sign,
        ("SIN", 1) => numeric::sin,
        ("SQRT", 1) => numeric::sqrt,
        ("TAN", 1) => numeric::tan,
        ("TRUNCATE", 2) => numeric::truncate,
        ("__MYSQL_CONV_BINARY_LITERAL", 3) => numeric::conv_binary_literal,

        ("FIELD", 2..) => compare::field,
        ("GREATEST", 2..) => compare::greatest,
        ("INTERVAL", 2..) => compare::interval,
        ("LEAST", 2..) => compare::least,
        ("STRCMP", 2) => compare::strcmp,

        ("ASCII", 1) => text::ascii,
        ("BIT_LENGTH", 1) => text::bit_length,
        ("CHAR", 1..) => text::char,
        ("CHAR_LENGTH" | "CHARACTER_LENGTH", 1) => text::char_length,
        ("CONCAT", 1..) => text::concat,
        ("CONCAT_WS", 2..) => text::concat_ws,
        ("ELT", 2..) => text::elt,
        ("EXPORT_SET", 3..=5) => text::export_set,
        ("FIND_IN_SET", 2) => text::find_in_set,
        ("HEX", 1) => text::hex,
        ("INSERT", 4) => text::insert,
        ("INSTR", 2) => text::instr,
        ("LCASE" | "LOWER", 1) => text::lower,
        ("LEFT", 2) => text::left,
        ("LENGTH" | "OCTET_LENGTH", 1) => text::length,
        ("LOCATE", 2 | 3) => text::locate,
        ("LPAD", 3) => text::lpad,
        ("MAKE_SET", 2..) => text::make_set,
        ("MID", 3) => text::substring,
        ("ORD", 1) => text::ord,
        ("POSITION", 2) => text::locate,
        ("REPEAT", 2) => text::repeat,
        ("REPLACE", 3) => text::replace,
        ("RIGHT", 2) => text::right,
        ("RPAD", 3) => text::rpad,
        ("SPACE", 1) => text::space,
        ("SUBSTR" | "SUBSTRING", 2 | 3) => text::substring,
        ("SUBSTRING_INDEX", 3) => text::substring_index,
        ("UCASE" | "UPPER", 1) => text::upper,
        ("UNHEX", 1) => text::unhex,

        ("CRC32", 1) => hash::crc32,
        ("FROM_BASE64", 1) => hash::from_base64,
        ("MD5", 1) => hash::md5,
        ("SHA" | "SHA1", 1) => hash::sha1,
        ("SHA2", 2) => hash::sha2,
        ("TO_BASE64", 1) => hash::to_base64,

        ("BIN_TO_UUID", 1 | 2) => network::bin_to_uuid,
        ("INET6_ATON", 1) => network::inet6_aton,
        ("INET6_NTOA", 1) => network::inet6_ntoa,
        ("INET_ATON", 1) => network::inet_aton,
        ("INET_NTOA", 1) => network::inet_ntoa,
        ("IS_IPV4", 1) => network::is_ipv4,
        ("IS_IPV4_COMPAT", 1) => network::is_ipv4_compat,
        ("IS_IPV4_MAPPED", 1) => network::is_ipv4_mapped,
        ("IS_IPV6", 1) => network::is_ipv6,
        ("IS_UUID", 1) => network::is_uuid,
        ("UUID_TO_BIN", 1 | 2) => network::uuid_to_bin,

        ("REGEXP", 2) => regexp::regexp_operator,
        ("REGEXP_INSTR", 2..=6) => regexp::regexp_instr,
        ("REGEXP_LIKE", 2 | 3) => regexp::regexp_like,
        ("REGEXP_REPLACE", 3..=6) => regexp::regexp_replace,
        ("REGEXP_SUBSTR", 2..=5) => regexp::regexp_substr,
        _ => return None,
    };
    Some(function)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_replaces_characters_like_mysql() {
        let cases = [
            ("Quadratic", 3, 4, "What", "QuWhattic"),
            ("Quadratic", -1, 4, "What", "Quadratic"),
            ("Quadratic", 3, 100, "What", "QuWhat"),
            ("hello", 3, -1, "xyz", "hexyz"),
            ("Grüße", 3, 1, "X", "GrXße"),
            ("Grüße", 6, 1, "X", "GrüßeX"),
            ("Grüße", 7, 1, "X", "Grüße"),
            ("Grüß", 5, 1, "X", "GrüßX"),
            ("abc", 4, 1, "X", "abc"),
        ];
        for (original, position, length, replacement, expected) in cases {
            assert_eq!(
                insert(original, position, length, text(replacement)).to_text(),
                Some(expected),
                "INSERT('{original}', {position}, {length}, '{replacement}')"
            );
        }
    }

    #[test]
    fn insert_with_a_null_argument_is_null() {
        assert!(matches!(insert("a", 1, 1, Value::Null), Value::Null));
    }

    fn insert(text: &str, position: i64, length: i64, replacement: Value) -> Value {
        execute(
            "INSERT",
            &[
                Value::build_text(text.to_string()),
                Value::from_i64(position),
                Value::from_i64(length),
                replacement,
            ],
        )
        .expect("INSERT is a MySQL function")
        .expect("INSERT succeeds")
    }

    fn text(value: &str) -> Value {
        Value::build_text(value.to_string())
    }
}
