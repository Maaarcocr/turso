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
        ConversionErrorKind::InvalidValue if conversion.message.starts_with("Data truncated") => {
            (1265, "01000", format!("Data truncated{place}"))
        }
        ConversionErrorKind::InvalidValue
            if conversion.message.starts_with("Cannot get geometry object") =>
        {
            (1416, "22003", conversion.message.clone())
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
