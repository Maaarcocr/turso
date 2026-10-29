use crate::names::quote_string;
use crate::MysqlParserError;
use sqlparser::ast::{
    BinaryLength, CharacterLength, DataType, EnumMember, ExactNumberInfo, Ident, ObjectName,
    ObjectNamePart, TimezoneInfo,
};
use turso_parser::ast;

const MAX_VARCHAR_LENGTH: u64 = 16_383;
const MAX_CHAR_LENGTH: u64 = 255;
pub(crate) const MAX_DECIMAL_PRECISION: u64 = 65;
pub(crate) const MAX_DECIMAL_SCALE: u64 = 30;
const MAX_FLOAT_DIGITS: u64 = 255;
const MAX_FRACTIONAL_SECONDS: u64 = 6;
const MAX_SET_MEMBERS: usize = 64;
const MAX_KEY_BYTES: u64 = 3072;
const BYTES_PER_CHARACTER: u64 = 4;
const TINY_LENGTH: u64 = 255;
const REGULAR_LENGTH: u64 = 65_535;
const MEDIUM_LENGTH: u64 = 16_777_215;

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
    Text,
    Binary {
        length: u64,
    },
    Varbinary {
        length: u64,
    },
    Blob,
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
    Spatial,
}

pub(crate) fn translate_column_type(data_type: &DataType) -> Result<ColumnType, MysqlParserError> {
    let column_type = match data_type {
        DataType::TinyInt(width) => integer("TINYINT", *width, -128, 127),
        DataType::TinyIntUnsigned(width) => integer("TINYINT UNSIGNED", *width, 0, 255),
        DataType::SmallInt(width) => integer("SMALLINT", *width, -32_768, 32_767),
        DataType::SmallIntUnsigned(width) => integer("SMALLINT UNSIGNED", *width, 0, 65_535),
        DataType::MediumInt(width) => integer("MEDIUMINT", *width, -8_388_608, 8_388_607),
        DataType::MediumIntUnsigned(width) => integer("MEDIUMINT UNSIGNED", *width, 0, 16_777_215),
        DataType::Int(width) | DataType::Integer(width) => {
            integer("INT", *width, i32::MIN.into(), i32::MAX.into())
        }
        DataType::IntUnsigned(width) | DataType::IntegerUnsigned(width) => {
            integer("INT UNSIGNED", *width, 0, u32::MAX.into())
        }
        DataType::BigInt(width) => integer("BIGINT", *width, i64::MIN.into(), i64::MAX.into()),
        DataType::BigIntUnsigned(width) => integer("BIGINT UNSIGNED", *width, 0, u64::MAX.into()),
        DataType::Bool | DataType::Boolean => integer("TINYINT", Some(1), -128, 127),
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
        DataType::TinyText => text("TINYTEXT"),
        DataType::Text => text("TEXT"),
        DataType::MediumText => text("MEDIUMTEXT"),
        DataType::LongText => text("LONGTEXT"),
        DataType::Custom(name, modifiers) => custom(name, modifiers)?,
        DataType::Varbinary(Some(BinaryLength::IntegerLength { length }))
            if *length <= REGULAR_LENGTH =>
        {
            ColumnType {
                declared: sized_type("VARBINARY", *length),
                family: Family::Varbinary { length: *length },
            }
        }
        DataType::Varbinary(Some(BinaryLength::IntegerLength { length })) => {
            return Err(MysqlParserError::Rejected(format!(
                "VARBINARY({length}) exceeds the MySQL maximum of {REGULAR_LENGTH}"
            )));
        }
        DataType::Binary(length) => {
            let length = length.unwrap_or(1);
            if length > MAX_CHAR_LENGTH {
                return Err(MysqlParserError::Rejected(format!(
                    "BINARY({length}) exceeds the MySQL maximum of {MAX_CHAR_LENGTH}"
                )));
            }
            ColumnType {
                declared: sized_type("BINARY", length),
                family: Family::Binary { length },
            }
        }
        DataType::TinyBlob => blob("TINYBLOB"),
        DataType::Blob(None) => blob("BLOB"),
        DataType::MediumBlob => blob("MEDIUMBLOB"),
        DataType::LongBlob => blob("LONGBLOB"),
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
            declared: simple_type("LONGTEXT"),
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
        matches!(
            self.family,
            Family::Binary { .. } | Family::Varbinary { .. } | Family::Blob
        )
    }

    pub(crate) fn is_character_string(&self) -> bool {
        matches!(
            self.family,
            Family::Char { .. }
                | Family::Varchar { .. }
                | Family::Text
                | Family::Enum(_)
                | Family::Set(_)
        )
    }

    pub(crate) fn value_check(&self, column: &ast::Name) -> Option<ast::Expr> {
        let id = || ast::Expr::Id(column.clone());
        match &self.family {
            Family::Json => Some(call("json_valid", vec![id()])),
            Family::Enum(_) | Family::Set(_) => None,
            Family::Integer { .. }
            | Family::Bit { .. }
            | Family::Decimal { .. }
            | Family::Float { .. }
            | Family::Char { .. }
            | Family::Varchar { .. }
            | Family::Text
            | Family::Binary { .. }
            | Family::Varbinary { .. }
            | Family::Blob
            | Family::Date
            | Family::Datetime { .. }
            | Family::Timestamp { .. }
            | Family::Time { .. }
            | Family::Year
            | Family::Spatial => None,
        }
    }

    pub(crate) fn key_part_bytes(
        &self,
        column: &str,
        prefix: Option<u64>,
    ) -> Result<u64, MysqlParserError> {
        let bytes = match (&self.family, prefix) {
            (Family::Text | Family::Blob, None) => {
                return Err(MysqlParserError::Rejected(format!(
                    "BLOB/TEXT column `{column}` used in a key needs a key length"
                )));
            }
            (Family::Json, _) => {
                return Err(MysqlParserError::Rejected(format!(
                    "JSON column `{column}` cannot be indexed directly"
                )));
            }
            (Family::Spatial, _) => {
                return Err(MysqlParserError::Unsupported(format!(
                    "spatial column `{column}` in a key is not available"
                )));
            }
            (Family::Text, Some(prefix)) => prefix.saturating_mul(BYTES_PER_CHARACTER),
            (Family::Blob, Some(prefix)) => prefix,
            (Family::Char { length } | Family::Varchar { length }, prefix)
                if prefix.is_none_or(|prefix| prefix <= *length) =>
            {
                prefix
                    .unwrap_or(*length)
                    .saturating_mul(BYTES_PER_CHARACTER)
            }
            (Family::Binary { length } | Family::Varbinary { length }, prefix)
                if prefix.is_none_or(|prefix| prefix <= *length) =>
            {
                prefix.unwrap_or(*length)
            }
            (Family::Integer { max, .. }, None) => match max {
                ..=255 => 1,
                ..=65_535 => 2,
                ..=16_777_215 => 3,
                ..=4_294_967_295 => 4,
                _ => 8,
            },
            (Family::Bit { width }, None) => width.div_ceil(8),
            (
                Family::Decimal {
                    precision, scale, ..
                },
                None,
            ) => decimal_bytes(precision - scale) + decimal_bytes(*scale),
            (Family::Float { .. }, None) if self.declared.name.starts_with("FLOAT") => 4,
            (Family::Float { .. }, None) => 8,
            (Family::Date, None) => 3,
            (Family::Time { precision }, None) => 3 + precision.div_ceil(2),
            (Family::Timestamp { precision }, None) => 4 + precision.div_ceil(2),
            (Family::Datetime { precision }, None) => 5 + precision.div_ceil(2),
            (Family::Year, None) => 1,
            (Family::Enum(members), None) if members.len() <= 255 => 1,
            (Family::Enum(_), None) => 2,
            (Family::Set(members), None) => match members.len().div_ceil(8) {
                0 | 1 => 1,
                bytes @ 2..=4 => bytes as u64,
                _ => 8,
            },
            (
                Family::Char { .. }
                | Family::Varchar { .. }
                | Family::Binary { .. }
                | Family::Varbinary { .. },
                None,
            ) => unreachable!("a whole string column is measured by its length"),
            (_, Some(_)) => {
                return Err(MysqlParserError::Rejected(format!(
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

/// The bytes of InnoDB's binary DECIMAL format for `digits` digits on one
/// side of the point: four per nine digits and the rest by `DIG2BYTES`.
fn decimal_bytes(digits: u64) -> u64 {
    const DIG2BYTES: [u64; 9] = [0, 1, 1, 2, 2, 3, 3, 4, 4];
    digits / 9 * 4 + DIG2BYTES[(digits % 9) as usize]
}

pub(crate) fn ensure_key_length(total_bytes: u64) -> Result<(), MysqlParserError> {
    if total_bytes > MAX_KEY_BYTES {
        return Err(key_too_long());
    }
    Ok(())
}

fn integer(name: &str, width: Option<u64>, min: i128, max: i128) -> ColumnType {
    let declared = match width {
        Some(width) => sized_type(name, width),
        None => simple_type(name),
    };
    ColumnType {
        declared,
        family: Family::Integer { min, max },
    }
}

fn bit(width: u64) -> Result<ColumnType, MysqlParserError> {
    if !(1..=64).contains(&width) {
        return Err(MysqlParserError::Rejected(format!(
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
                return Err(MysqlParserError::Rejected(format!(
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
            return Err(MysqlParserError::Rejected(format!(
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
        Some(CharacterLength::IntegerLength { length, unit: None }) => {
            Err(MysqlParserError::Rejected(format!(
                "character length `{length}` exceeds the MySQL maximum of {max_length}"
            )))
        }
        Some(length) => Err(MysqlParserError::Unsupported(format!(
            "character length `{length}` is outside the available range 0..={max_length}"
        ))),
    }
}

fn text(name: &str) -> ColumnType {
    ColumnType {
        declared: simple_type(name),
        family: Family::Text,
    }
}

fn blob(name: &str) -> ColumnType {
    ColumnType {
        declared: simple_type(name),
        family: Family::Blob,
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
        (
            name @ ("GEOMETRY" | "POINT" | "LINESTRING" | "POLYGON" | "MULTIPOINT"
            | "MULTILINESTRING" | "MULTIPOLYGON" | "GEOMETRYCOLLECTION"),
            [],
        ) => Ok(ColumnType {
            declared: simple_type(name),
            family: Family::Spatial,
        }),
        ("FIXED", _) => {
            let number = |text: &String| {
                text.parse::<u64>()
                    .map_err(|_| unsupported_type(name, modifiers))
            };
            let info = match modifiers {
                [] => ExactNumberInfo::None,
                [precision] => ExactNumberInfo::Precision(number(precision)?),
                [precision, scale] => ExactNumberInfo::PrecisionAndScale(
                    number(precision)?,
                    i64::try_from(number(scale)?).map_err(|_| unsupported_type(name, modifiers))?,
                ),
                _ => return Err(unsupported_type(name, modifiers)),
            };
            decimal(&info, false)
        }
        ("TEXT", [length]) => {
            let characters = length
                .parse::<u64>()
                .map_err(|_| unsupported_type(name, modifiers))?;
            let bytes = characters.saturating_mul(BYTES_PER_CHARACTER);
            Ok(if bytes <= TINY_LENGTH {
                text("TINYTEXT")
            } else if bytes <= REGULAR_LENGTH {
                text("TEXT")
            } else if bytes <= MEDIUM_LENGTH {
                text("MEDIUMTEXT")
            } else {
                text("LONGTEXT")
            })
        }
        (name, _) if !MYSQL_TYPE_NAMES.contains(&name) => Err(MysqlParserError::Rejected(format!(
            "`{name}` is not a MySQL data type"
        ))),
        _ => Err(unsupported_type(name, modifiers)),
    }
}

const MYSQL_TYPE_NAMES: &[&str] = &[
    "BIT",
    "TINYINT",
    "SMALLINT",
    "MEDIUMINT",
    "MIDDLEINT",
    "INT",
    "INTEGER",
    "BIGINT",
    "INT1",
    "INT2",
    "INT3",
    "INT4",
    "INT8",
    "REAL",
    "DOUBLE",
    "FLOAT",
    "FLOAT4",
    "FLOAT8",
    "DECIMAL",
    "DEC",
    "NUMERIC",
    "FIXED",
    "BOOL",
    "BOOLEAN",
    "SERIAL",
    "DATE",
    "TIME",
    "TIMESTAMP",
    "DATETIME",
    "YEAR",
    "CHAR",
    "CHARACTER",
    "NCHAR",
    "NATIONAL",
    "VARCHAR",
    "VARCHARACTER",
    "NVARCHAR",
    "BINARY",
    "VARBINARY",
    "TINYBLOB",
    "BLOB",
    "MEDIUMBLOB",
    "LONGBLOB",
    "LONG",
    "TINYTEXT",
    "TEXT",
    "MEDIUMTEXT",
    "LONGTEXT",
    "ENUM",
    "SET",
    "JSON",
    "GEOMETRY",
    "POINT",
    "LINESTRING",
    "POLYGON",
    "MULTIPOINT",
    "MULTILINESTRING",
    "MULTIPOLYGON",
    "GEOMETRYCOLLECTION",
    "GEOMCOLLECTION",
    "VECTOR",
];

fn fractional_seconds(precision: Option<u64>) -> Result<u64, MysqlParserError> {
    match precision {
        None => Ok(0),
        Some(precision) if precision <= MAX_FRACTIONAL_SECONDS => Ok(precision),
        Some(precision) => Err(MysqlParserError::Rejected(format!(
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
        declared: labeled_type("ENUM", &members),
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
        declared: labeled_type("\"SET\"", members),
        family: Family::Set(members.to_vec()),
    })
}

impl ColumnType {
    /// Marks an `ENUM` or `SET` type to match names with their letter case,
    /// as under a case-sensitive collation.
    pub(crate) fn match_labels_exactly(&mut self) {
        let Some(ast::TypeSize::MaxSize(labels)) = self.declared.size.take() else {
            unreachable!("an ENUM or SET type has its labels as its one parameter");
        };
        self.declared.size = Some(ast::TypeSize::TypeSize(
            labels,
            Box::new(ast::Expr::Literal(ast::Literal::Numeric("1".to_string()))),
        ));
    }
}

/// An `ENUM` or `SET` type with its members, quoted and joined with commas,
/// as its parameter.
fn labeled_type(name: &str, members: &[String]) -> ast::Type {
    let labels = members
        .iter()
        .map(|member| quote_string(member))
        .collect::<Vec<_>>()
        .join(",");
    ast::Type {
        name: name.to_string(),
        size: Some(ast::TypeSize::MaxSize(Box::new(ast::Expr::Literal(
            ast::Literal::String(quote_string(&labels)),
        )))),
        array_dimensions: 0,
    }
}

fn ensure_distinct_members(members: &[String]) -> Result<(), MysqlParserError> {
    for (position, member) in members.iter().enumerate() {
        let duplicate = members[..position]
            .iter()
            .any(|earlier| earlier.to_lowercase() == member.to_lowercase());
        if duplicate {
            return Err(MysqlParserError::Rejected(format!(
                "member `{member}` is listed twice"
            )));
        }
        if member.ends_with(' ') {
            return Err(MysqlParserError::Unsupported(format!(
                "member `{member}` needs MySQL's trailing-space trimming"
            )));
        }
    }
    Ok(())
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
    MysqlParserError::Rejected("the key is longer than the 3072-byte InnoDB maximum".to_string())
}

fn invalid_decimal(info: &ExactNumberInfo) -> MysqlParserError {
    MysqlParserError::Rejected(format!("DECIMAL{info} is not a valid MySQL decimal type"))
}

fn unsupported_type(name: &ObjectName, modifiers: &[String]) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "column type `{name}({})` is not available",
        modifiers.join(",")
    ))
}
