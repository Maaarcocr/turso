use crate::types::{DATA_TRUNCATED, NOT_A_GEOMETRY, SRID_MISMATCH};
use turso_core::{ConversionErrorKind, LimboError, TypeConversionError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MysqlError {
    pub code: u16,
    pub sql_state: &'static str,
    pub message: String,
}

pub fn mysql_error(error: &LimboError) -> Option<MysqlError> {
    let LimboError::TypeConversion(conversion) = error else {
        return None;
    };
    let place = place_in_statement(conversion);
    let (code, sql_state, message) = match conversion.kind {
        ConversionErrorKind::OutOfRange => (1264, "22003", format!("Out of range value{place}")),
        ConversionErrorKind::TooLong => (1406, "22001", format!("Data too long{place}")),
        ConversionErrorKind::NoMoreKeys => (1062, "23000", conversion.message.clone()),
        ConversionErrorKind::InvalidValue if conversion.message == DATA_TRUNCATED => {
            (1265, "01000", format!("{DATA_TRUNCATED}{place}"))
        }
        ConversionErrorKind::InvalidValue if conversion.message == NOT_A_GEOMETRY => {
            (1416, "22003", NOT_A_GEOMETRY.to_string())
        }
        ConversionErrorKind::InvalidValue if conversion.message.starts_with(SRID_MISMATCH) => {
            let mut srids = conversion.message[SRID_MISMATCH.len()..].split_whitespace();
            let (geometry, column) = (srids.next().unwrap_or("?"), srids.next().unwrap_or("?"));
            (
                3643,
                "HY000",
                format!(
                    "The SRID of the geometry does not match the SRID of the column '{}'. The SRID of the geometry is {geometry}, but the SRID of the column is {column}. Consider changing the SRID of the geometry or the SRID property of the column.",
                    conversion.column.as_deref().unwrap_or("?")
                ),
            )
        }
        ConversionErrorKind::InvalidValue => {
            (1366, "HY000", format!("{}{place}", conversion.message))
        }
    };
    Some(MysqlError {
        code,
        sql_state,
        message,
    })
}

fn place_in_statement(conversion: &TypeConversionError) -> String {
    match (&conversion.column, conversion.row) {
        (Some(column), Some(row)) => format!(" for column '{column}' at row {row}"),
        (Some(column), None) => format!(" for column '{column}'"),
        (None, _) => String::new(),
    }
}
