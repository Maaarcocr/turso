use crate::names::quote_string;
use crate::MysqlParserError;
use sqlparser::ast::{
    BinaryLength, CharacterLength, DataType, EnumMember, ExactNumberInfo, Ident, ObjectName,
    ObjectNamePart, TimezoneInfo,
};
use turso_parser::ast;

const MAX_VARCHAR_LENGTH: u64 = 16_383;
const MAX_CHAR_LENGTH: u64 = 255;
const MAX_DECIMAL_PRECISION: u64 = 65;
const MAX_DECIMAL_SCALE: u64 = 30;
const MAX_FLOAT_DIGITS: u64 = 255;
const MAX_FRACTIONAL_SECONDS: u64 = 6;
const MAX_SET_MEMBERS: usize = 6;
const MAX_KEY_BYTES: u64 = 3072;
const BYTES_PER_CHARACTER: u64 = 4;
const TINY_LENGTH: u64 = 255;
const REGULAR_LENGTH: u64 = 65_535;
const MEDIUM_LENGTH: u64 = 16_777_215;
const NUMERIC_KEY_BYTES: u64 = 8;

/// A MySQL column type: the declared type Turso stores and the MySQL type
/// family whose value rules the column must keep.
pub(crate) struct ColumnType {
    pub(crate) declared: ast::Type,
    pub(crate) family: Family,
}

pub(crate) enum Family {
    Integer {
        min: i128,
        max: i128,
    },
    Bit {
        width: u64,
    },
    Decimal {
        precision: u64,
        scale: u64,
        unsigned: bool,
    },
    Float {
        unsigned: bool,
        digits: Option<(u64, u64)>,
    },
    Char {
        length: u64,
    },
    Varchar {
        length: u64,
    },
    Text {
        max_bytes: Option<u64>,
    },
    Varbinary {
        length: u64,
    },
    Blob {
        max_bytes: Option<u64>,
    },
    Json,
    Date,
    Datetime {
        precision: u64,
    },
    Timestamp {
        precision: u64,
    },
    Time {
        precision: u64,
    },
    Year,
    Enum(Vec<String>),
    Set(Vec<String>),
}

pub(crate) fn translate_column_type(data_type: &DataType) -> Result<ColumnType, MysqlParserError> {
    let column_type = match data_type {
        DataType::TinyInt(_) => integer("TINYINT", -128, 127),
        DataType::TinyIntUnsigned(_) => integer("TINYINT UNSIGNED", 0, 255),
        DataType::SmallInt(_) => integer("SMALLINT", -32_768, 32_767),
        DataType::SmallIntUnsigned(_) => integer("SMALLINT UNSIGNED", 0, 65_535),
        DataType::MediumInt(_) => integer("MEDIUMINT", -8_388_608, 8_388_607),
        DataType::MediumIntUnsigned(_) => integer("MEDIUMINT UNSIGNED", 0, 16_777_215),
        DataType::Int(_) | DataType::Integer(_) => integer("INT", i32::MIN.into(), i32::MAX.into()),
        DataType::IntUnsigned(_) | DataType::IntegerUnsigned(_) => {
            integer("INT UNSIGNED", 0, u32::MAX.into())
        }
        DataType::BigInt(_) => integer("BIGINT", i64::MIN.into(), i64::MAX.into()),
        DataType::BigIntUnsigned(_) => integer("BIGINT UNSIGNED", 0, u64::MAX.into()),
        DataType::Bool | DataType::Boolean => integer("TINYINT", -128, 127),
        DataType::Bit(width) => bit(width.unwrap_or(1))?,
        DataType::Decimal(info) | DataType::Dec(info) | DataType::Numeric(info) => {
            decimal(info, false)?
        }
        DataType::DecimalUnsigned(info) | DataType::DecUnsigned(info) => decimal(info, true)?,
        DataType::Float(info) => float("FLOAT", info, false)?,
        DataType::FloatUnsigned(info) => float("FLOAT", info, true)?,
        DataType::Double(
            info @ (ExactNumberInfo::None | ExactNumberInfo::PrecisionAndScale(..)),
        ) => float("DOUBLE", info, false)?,
        DataType::DoubleUnsigned(
            info @ (ExactNumberInfo::None | ExactNumberInfo::PrecisionAndScale(..)),
        ) => float("DOUBLE", info, true)?,
        DataType::DoublePrecision | DataType::Real => {
            float("DOUBLE", &ExactNumberInfo::None, false)?
        }
        DataType::DoublePrecisionUnsigned | DataType::RealUnsigned => {
            float("DOUBLE", &ExactNumberInfo::None, true)?
        }
        DataType::Char(length) | DataType::Character(length) => {
            let length = character_length(length.as_ref(), 1, MAX_CHAR_LENGTH)?;
            ColumnType {
                declared: sized_type("CHAR", length),
                family: Family::Char { length },
            }
        }
        DataType::Varchar(Some(length))
        | DataType::CharVarying(Some(length))
        | DataType::CharacterVarying(Some(length)) => {
            let length = character_length(Some(length), 0, MAX_VARCHAR_LENGTH)?;
            ColumnType {
                declared: sized_type("VARCHAR", length),
                family: Family::Varchar { length },
            }
        }
        DataType::TinyText => text("TINYTEXT", Some(TINY_LENGTH)),
        DataType::Text => text("TEXT", Some(REGULAR_LENGTH)),
        DataType::MediumText => text("MEDIUMTEXT", Some(MEDIUM_LENGTH)),
        DataType::LongText => text("LONGTEXT", None),
        DataType::Custom(name, modifiers) => custom(name, modifiers)?,
        DataType::Varbinary(Some(BinaryLength::IntegerLength { length }))
            if *length <= REGULAR_LENGTH =>
        {
            ColumnType {
                declared: sized_type("VARBINARY", *length),
                family: Family::Varbinary { length: *length },
            }
        }
        DataType::TinyBlob => blob("TINYBLOB", Some(TINY_LENGTH)),
        DataType::Blob(None) => blob("BLOB", Some(REGULAR_LENGTH)),
        DataType::MediumBlob => blob("MEDIUMBLOB", Some(MEDIUM_LENGTH)),
        DataType::LongBlob => blob("LONGBLOB", None),
        DataType::Date => ColumnType {
            declared: simple_type("DATE"),
            family: Family::Date,
        },
        DataType::Datetime(precision) => {
            let precision = fractional_seconds(*precision)?;
            temporal("DATETIME", precision, Family::Datetime { precision })
        }
        DataType::Timestamp(precision, TimezoneInfo::None) => {
            let precision = fractional_seconds(*precision)?;
            temporal("TIMESTAMP", precision, Family::Timestamp { precision })
        }
        DataType::Time(precision, TimezoneInfo::None) => {
            let precision = fractional_seconds(*precision)?;
            temporal("TIME", precision, Family::Time { precision })
        }
        DataType::JSON => ColumnType {
            declared: simple_type("JSON TEXT"),
            family: Family::Json,
        },
        DataType::Enum(members, None) => enumeration(members)?,
        DataType::Set(members) => set(members)?,
        _ => {
            return Err(MysqlParserError::Unsupported(format!(
                "column type `{data_type}` is not available"
            )));
        }
    };
    Ok(column_type)
}

impl ColumnType {
    pub(crate) fn is_integer(&self) -> bool {
        matches!(self.family, Family::Integer { .. })
    }

    pub(crate) fn is_binary(&self) -> bool {
        matches!(self.family, Family::Varbinary { .. } | Family::Blob { .. })
    }

    pub(crate) fn is_character_string(&self) -> bool {
        matches!(
            self.family,
            Family::Char { .. }
                | Family::Varchar { .. }
                | Family::Text { .. }
                | Family::Enum(_)
                | Family::Set(_)
        )
    }

    /// The CHECK expression that keeps stored values inside the MySQL type,
    /// or `None` when every value Turso can store is valid.
    pub(crate) fn value_check(&self, column: &ast::Name) -> Option<ast::Expr> {
        let id = || ast::Expr::Id(column.clone());
        let at_most = |measured: ast::Expr, limit: u64| {
            ast::Expr::Binary(
                Box::new(measured),
                ast::Operator::LessEquals,
                Box::new(number(limit.into())),
            )
        };
        let non_negative = || {
            ast::Expr::Binary(
                Box::new(id()),
                ast::Operator::GreaterEquals,
                Box::new(number(0)),
            )
        };
        match &self.family {
            Family::Integer { min: 0, max } if *max > i64::MAX.into() => Some(non_negative()),
            Family::Integer { min, max } => Some(ast::Expr::Between {
                lhs: Box::new(id()),
                not: false,
                start: Box::new(number(*min)),
                end: Box::new(number(*max)),
            }),
            Family::Bit { width: 64 } => Some(non_negative()),
            Family::Bit { width } => Some(ast::Expr::Between {
                lhs: Box::new(id()),
                not: false,
                start: Box::new(number(0)),
                end: Box::new(number((1_i128 << width) - 1)),
            }),
            Family::Float {
                unsigned,
                digits: Some((precision, scale)),
            } => {
                let rounded = ast::Expr::Binary(
                    Box::new(id()),
                    ast::Operator::Equals,
                    Box::new(call("round", vec![id(), number((*scale).into())])),
                );
                let limit =
                    ast::Expr::Literal(ast::Literal::Numeric(format!("1e{}", precision - scale)));
                let in_range = ast::Expr::Binary(
                    Box::new(call("abs", vec![id()])),
                    ast::Operator::Less,
                    Box::new(limit),
                );
                let checked =
                    ast::Expr::Binary(Box::new(rounded), ast::Operator::And, Box::new(in_range));
                Some(if *unsigned {
                    ast::Expr::Binary(
                        Box::new(checked),
                        ast::Operator::And,
                        Box::new(non_negative()),
                    )
                } else {
                    checked
                })
            }
            Family::Decimal { unsigned: true, .. } | Family::Float { unsigned: true, .. } => {
                Some(non_negative())
            }
            Family::Char { length } | Family::Varchar { length } => Some(at_most(
                call("char_length", vec![call("rtrim", vec![id()])]),
                *length,
            )),
            Family::Text {
                max_bytes: Some(limit),
            }
            | Family::Blob {
                max_bytes: Some(limit),
            }
            | Family::Varbinary { length: limit } => {
                Some(at_most(call("octet_length", vec![id()]), *limit))
            }
            Family::Json => Some(call("json_valid", vec![id()])),
            Family::Enum(members) => Some(one_of(id(), members.iter().cloned())),
            Family::Set(members) => Some(one_of(id(), set_values(members))),
            Family::Decimal { .. }
            | Family::Float { .. }
            | Family::Text { max_bytes: None }
            | Family::Blob { max_bytes: None }
            | Family::Date
            | Family::Datetime { .. }
            | Family::Timestamp { .. }
            | Family::Time { .. }
            | Family::Year => None,
        }
    }

    /// The bytes a key part on this column takes in an InnoDB index, which
    /// MySQL limits to 3072. A prefix is a number of characters for text and
    /// a number of bytes for binary strings.
    pub(crate) fn key_part_bytes(
        &self,
        column: &str,
        prefix: Option<u64>,
    ) -> Result<u64, MysqlParserError> {
        let bytes = match (&self.family, prefix) {
            (Family::Text { .. } | Family::Blob { .. }, None) => {
                return Err(MysqlParserError::Unsupported(format!(
                    "BLOB/TEXT column `{column}` used in a key needs a key length"
                )));
            }
            (Family::Json, _) => {
                return Err(MysqlParserError::Unsupported(format!(
                    "JSON column `{column}` cannot be indexed directly"
                )));
            }
            (Family::Text { .. }, Some(prefix)) => prefix.saturating_mul(BYTES_PER_CHARACTER),
            (Family::Blob { .. }, Some(prefix)) => prefix,
            (Family::Char { length } | Family::Varchar { length }, prefix)
                if prefix.is_none_or(|prefix| prefix <= *length) =>
            {
                prefix
                    .unwrap_or(*length)
                    .saturating_mul(BYTES_PER_CHARACTER)
            }
            (Family::Varbinary { length }, prefix)
                if prefix.is_none_or(|prefix| prefix <= *length) =>
            {
                prefix.unwrap_or(*length)
            }
            (_, None) => NUMERIC_KEY_BYTES,
            (_, Some(_)) => {
                return Err(MysqlParserError::Unsupported(format!(
                    "prefix key on column `{column}` is not a valid MySQL key part"
                )));
            }
        };
        if bytes > MAX_KEY_BYTES {
            return Err(key_too_long());
        }
        Ok(bytes)
    }
}

pub(crate) fn ensure_key_length(total_bytes: u64) -> Result<(), MysqlParserError> {
    if total_bytes > MAX_KEY_BYTES {
        return Err(key_too_long());
    }
    Ok(())
}

pub(crate) fn set_values(members: &[String]) -> impl Iterator<Item = String> + '_ {
    (0_u32..1 << members.len()).map(move |mask| {
        members
            .iter()
            .enumerate()
            .filter(|(position, _)| mask & (1 << position) != 0)
            .map(|(_, member)| member.as_str())
            .collect::<Vec<_>>()
            .join(",")
    })
}

fn integer(name: &str, min: i128, max: i128) -> ColumnType {
    ColumnType {
        declared: simple_type(name),
        family: Family::Integer { min, max },
    }
}

fn bit(width: u64) -> Result<ColumnType, MysqlParserError> {
    if !(1..=64).contains(&width) {
        return Err(MysqlParserError::Unsupported(format!(
            "BIT({width}) is outside the MySQL range 1..=64"
        )));
    }
    Ok(ColumnType {
        declared: sized_type("BIT", width),
        family: Family::Bit { width },
    })
}

fn decimal(info: &ExactNumberInfo, unsigned: bool) -> Result<ColumnType, MysqlParserError> {
    let (precision, scale) = match info {
        ExactNumberInfo::None => (10, 0),
        ExactNumberInfo::Precision(precision) => (*precision, 0),
        ExactNumberInfo::PrecisionAndScale(precision, scale) => (
            *precision,
            u64::try_from(*scale).map_err(|_| invalid_decimal(info))?,
        ),
    };
    if !(1..=MAX_DECIMAL_PRECISION).contains(&precision)
        || scale > MAX_DECIMAL_SCALE
        || scale > precision
    {
        return Err(invalid_decimal(info));
    }
    let name = if unsigned {
        "DECIMAL UNSIGNED"
    } else {
        "DECIMAL"
    };
    Ok(ColumnType {
        declared: ast::Type {
            name: name.to_string(),
            size: Some(ast::TypeSize::TypeSize(
                Box::new(number(precision.into())),
                Box::new(number(scale.into())),
            )),
            array_dimensions: 0,
        },
        family: Family::Decimal {
            precision,
            scale,
            unsigned,
        },
    })
}

fn float(
    name: &str,
    info: &ExactNumberInfo,
    unsigned: bool,
) -> Result<ColumnType, MysqlParserError> {
    let (name, size, digits) = match info {
        ExactNumberInfo::None => (name, None, None),
        ExactNumberInfo::Precision(precision) if *precision <= 24 => ("FLOAT", None, None),
        ExactNumberInfo::Precision(precision) if *precision <= 53 => ("DOUBLE", None, None),
        ExactNumberInfo::PrecisionAndScale(precision, scale) => {
            let scale = u64::try_from(*scale).ok().filter(|scale| {
                *scale <= MAX_DECIMAL_SCALE
                    && *scale <= *precision
                    && *precision <= MAX_FLOAT_DIGITS
            });
            let Some(scale) = scale else {
                return Err(MysqlParserError::Unsupported(format!(
                    "{name}{info} is not a valid MySQL floating-point type"
                )));
            };
            (
                name,
                Some(ast::TypeSize::TypeSize(
                    Box::new(number((*precision).into())),
                    Box::new(number(scale.into())),
                )),
                Some((*precision, scale)),
            )
        }
        ExactNumberInfo::Precision(_) => {
            return Err(MysqlParserError::Unsupported(format!(
                "{name}{info} is not a valid MySQL floating-point type"
            )));
        }
    };
    let name = if unsigned {
        format!("{name} UNSIGNED")
    } else {
        name.to_string()
    };
    Ok(ColumnType {
        declared: ast::Type {
            name,
            size,
            array_dimensions: 0,
        },
        family: Family::Float { unsigned, digits },
    })
}

fn character_length(
    length: Option<&CharacterLength>,
    default_length: u64,
    max_length: u64,
) -> Result<u64, MysqlParserError> {
    match length {
        None => Ok(default_length),
        Some(CharacterLength::IntegerLength { length, unit: None }) if *length <= max_length => {
            Ok(*length)
        }
        Some(length) => Err(MysqlParserError::Unsupported(format!(
            "character length `{length}` is outside the available range 0..={max_length}"
        ))),
    }
}

fn text(name: &str, max_bytes: Option<u64>) -> ColumnType {
    ColumnType {
        declared: simple_type(name),
        family: Family::Text { max_bytes },
    }
}

fn blob(name: &str, max_bytes: Option<u64>) -> ColumnType {
    ColumnType {
        declared: simple_type(name),
        family: Family::Blob { max_bytes },
    }
}

fn custom(name: &ObjectName, modifiers: &[String]) -> Result<ColumnType, MysqlParserError> {
    let [ObjectNamePart::Identifier(Ident {
        value,
        quote_style: None,
        ..
    })] = name.0.as_slice()
    else {
        return Err(unsupported_type(name, modifiers));
    };
    match (value.to_ascii_uppercase().as_str(), modifiers) {
        ("YEAR", []) => Ok(ColumnType {
            declared: simple_type("YEAR"),
            family: Family::Year,
        }),
        ("TEXT", [length]) => {
            let characters = length
                .parse::<u64>()
                .map_err(|_| unsupported_type(name, modifiers))?;
            let bytes = characters.saturating_mul(BYTES_PER_CHARACTER);
            Ok(if bytes <= TINY_LENGTH {
                text("TINYTEXT", Some(TINY_LENGTH))
            } else if bytes <= REGULAR_LENGTH {
                text("TEXT", Some(REGULAR_LENGTH))
            } else if bytes <= MEDIUM_LENGTH {
                text("MEDIUMTEXT", Some(MEDIUM_LENGTH))
            } else {
                text("LONGTEXT", None)
            })
        }
        _ => Err(unsupported_type(name, modifiers)),
    }
}

fn fractional_seconds(precision: Option<u64>) -> Result<u64, MysqlParserError> {
    match precision {
        None => Ok(0),
        Some(precision) if precision <= MAX_FRACTIONAL_SECONDS => Ok(precision),
        Some(precision) => Err(MysqlParserError::Unsupported(format!(
            "fractional seconds precision {precision} exceeds the MySQL maximum of 6"
        ))),
    }
}

fn temporal(name: &str, precision: u64, family: Family) -> ColumnType {
    let declared = if precision == 0 {
        simple_type(name)
    } else {
        sized_type(name, precision)
    };
    ColumnType { declared, family }
}

fn enumeration(members: &[EnumMember]) -> Result<ColumnType, MysqlParserError> {
    let members = members
        .iter()
        .map(|member| match member {
            EnumMember::Name(name) => Ok(name.clone()),
            EnumMember::NamedValue(..) => Err(MysqlParserError::Unsupported(
                "ENUM members with explicit values are not MySQL syntax".to_string(),
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    ensure_distinct_members(&members)?;
    Ok(ColumnType {
        declared: simple_type("TEXT"),
        family: Family::Enum(members),
    })
}

fn set(members: &[String]) -> Result<ColumnType, MysqlParserError> {
    if members.len() > MAX_SET_MEMBERS || members.iter().any(|member| member.contains(',')) {
        return Err(MysqlParserError::Unsupported(format!(
            "SET columns are available with at most {MAX_SET_MEMBERS} members without commas"
        )));
    }
    ensure_distinct_members(members)?;
    Ok(ColumnType {
        declared: simple_type("TEXT"),
        family: Family::Set(members.to_vec()),
    })
}

fn ensure_distinct_members(members: &[String]) -> Result<(), MysqlParserError> {
    for (position, member) in members.iter().enumerate() {
        let duplicate = members[..position]
            .iter()
            .any(|earlier| earlier.to_lowercase() == member.to_lowercase());
        if duplicate || member.ends_with(' ') {
            return Err(MysqlParserError::Unsupported(format!(
                "member `{member}` needs MySQL's case-insensitive or space-trimming member rules"
            )));
        }
    }
    Ok(())
}

fn one_of(column: ast::Expr, values: impl Iterator<Item = String>) -> ast::Expr {
    ast::Expr::InList {
        lhs: Box::new(column),
        not: false,
        rhs: values
            .map(|value| {
                Box::new(ast::Expr::Literal(ast::Literal::String(quote_string(
                    &value,
                ))))
            })
            .collect(),
    }
}

fn call(name: &str, args: Vec<ast::Expr>) -> ast::Expr {
    ast::Expr::FunctionCall {
        name: ast::Name::exact(name.to_string()),
        distinctness: None,
        args: args.into_iter().map(Box::new).collect(),
        order_by: Vec::new(),
        within_group: Vec::new(),
        filter_over: ast::FunctionTail {
            filter_clause: None,
            over_clause: None,
        },
    }
}

pub(crate) fn number(value: i128) -> ast::Expr {
    let literal = ast::Expr::Literal(ast::Literal::Numeric(value.unsigned_abs().to_string()));
    if value < 0 {
        ast::Expr::Unary(ast::UnaryOperator::Negative, Box::new(literal))
    } else {
        literal
    }
}

pub(crate) fn simple_type(name: &str) -> ast::Type {
    ast::Type {
        name: name.to_string(),
        size: None,
        array_dimensions: 0,
    }
}

fn sized_type(name: &str, size: u64) -> ast::Type {
    ast::Type {
        name: name.to_string(),
        size: Some(ast::TypeSize::MaxSize(Box::new(number(size.into())))),
        array_dimensions: 0,
    }
}

fn key_too_long() -> MysqlParserError {
    MysqlParserError::Unsupported("the key is longer than the 3072-byte InnoDB maximum".to_string())
}

fn invalid_decimal(info: &ExactNumberInfo) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("DECIMAL{info} is not a valid MySQL decimal type"))
}

fn unsupported_type(name: &ObjectName, modifiers: &[String]) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "column type `{name}({})` is not available",
        modifiers.join(",")
    ))
}
