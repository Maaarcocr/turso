use crate::types::decimal_number::{Decimal, Rounding};
use turso_core::{ColumnTypeInfo, Numeric, Value};

/// Renders a value as MySQL's text protocol sends it. `column_type` is the
/// declared type of the value's result column: a DECIMAL shows exactly the
/// digits of its scale, rounding away the hidden digits of a division.
pub fn format_text(value: &Value, column_type: Option<&ColumnTypeInfo>) -> String {
    format_with_decimal_scale(value, column_type.and_then(decimal_scale))
}

fn decimal_scale(column_type: &ColumnTypeInfo) -> Option<i64> {
    let name = column_type.declared_name.to_ascii_lowercase();
    if name != "decimal" && name != "decimal unsigned" {
        return None;
    }
    column_type.parameters.get(1)?.parse().ok()
}

fn format_with_decimal_scale(value: &Value, decimal_scale: Option<i64>) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Numeric(Numeric::Integer(number)) => match decimal_scale {
            Some(scale) => Decimal::from_i64(*number)
                .rounded(scale, Rounding::HalfAwayFromZero)
                .to_text(),
            None => number.to_string(),
        },
        Value::Numeric(Numeric::Float(double)) => {
            let double = f64::from(*double);
            if double.is_finite() {
                format_double(double)
            } else {
                double.to_string()
            }
        }
        Value::Text(text) => match (decimal_scale, Decimal::parse(text.as_str())) {
            (Some(scale), Some(decimal)) => {
                decimal.rounded(scale, Rounding::HalfAwayFromZero).to_text()
            }
            _ => text.as_str().to_string(),
        },
        Value::Blob(bytes) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Renders a DOUBLE as MySQL 8.4 does when the value has no fixed number of
/// decimals (for example `1e0/3`, but not `PI()` or a `DOUBLE(10,2)` column).
///
/// MySQL prints the shortest digit string that round-trips. With the decimal
/// exponent `e` of the first digit (`d.ddd × 10^e`), it uses scientific
/// notation when `e < -15`, or when `e >= 15` and the value is an integer;
/// otherwise it uses plain notation without a trailing `.0`. Scientific
/// notation has no `+` and no exponent padding. Recorded from MySQL 8.4.10:
/// `1e15` → `1e15`, `999999999999999e0` → `999999999999999`,
/// `1234567890123456.7e0` → `1234567890123456.8`, `1e-15` →
/// `0.000000000000001`, `1e-16` → `1e-16`, `-0e0` → `-0`. The rule also
/// matched MySQL on 325 random values from 1e-25 to 1e25.
pub(crate) fn format_double(value: f64) -> String {
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if value == 0.0 {
        return format!("{sign}0");
    }
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("`{:e}` output has an exponent");
    let exponent: i32 = exponent.parse().expect("`{:e}` exponent is an integer");
    let digits = mantissa.replace('.', "");
    let integer_digits = exponent + 1;
    let is_integer = digits.len() as i32 <= integer_digits;
    if exponent < -15 || (exponent >= 15 && is_integer) {
        format!("{sign}{mantissa}e{exponent}")
    } else if exponent < 0 {
        let zeros = "0".repeat((-integer_digits) as usize);
        format!("{sign}0.{zeros}{digits}")
    } else if is_integer {
        let zeros = "0".repeat(integer_digits as usize - digits.len());
        format!("{sign}{digits}{zeros}")
    } else {
        let (integer, fraction) = digits.split_at(integer_digits as usize);
        format!("{sign}{integer}.{fraction}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Values and outputs recorded from MySQL 8.4.10 with
    /// `mysql --batch --raw -e 'SELECT <value>'`.
    #[test]
    fn formats_doubles_like_mysql() {
        let cases: &[(f64, &str)] = &[
            (1e0 / 3.0, "0.3333333333333333"),
            (0.1 + 0.2, "0.30000000000000004"),
            (2f64.sqrt(), "1.4142135623730951"),
            (std::f64::consts::E, "2.718281828459045"),
            (1.0000000000000002, "1.0000000000000002"),
            (2.5, "2.5"),
            (-0.5, "-0.5"),
            (100.0, "100"),
            (125.0, "125"),
            (1e14, "100000000000000"),
            (123456789012345.0, "123456789012345"),
            (999999999999999.0, "999999999999999"),
            (123456789012345.6, "123456789012345.6"),
            (999999999999999.9, "999999999999999.9"),
            (12345678901234.567, "12345678901234.566"),
            (1234567890123456.7, "1234567890123456.8"),
            (1e15, "1e15"),
            (-1e15, "-1e15"),
            (1.5e15, "1.5e15"),
            (1234567890123456.0, "1.234567890123456e15"),
            (9007199254740993.0, "9.007199254740992e15"),
            (1e16, "1e16"),
            (12345678901234567.5, "1.2345678901234568e16"),
            (99999999999999999.0, "1e17"),
            (123456789012345678.0, "1.2345678901234568e17"),
            (1e21, "1e21"),
            (123456e17, "1.23456e22"),
            (1e100, "1e100"),
            (1.5e300, "1.5e300"),
            (1.7976931348623157e308, "1.7976931348623157e308"),
            (0.1, "0.1"),
            (1e-4, "0.0001"),
            (1e-5, "0.00001"),
            (1e-7, "0.0000001"),
            (-1e-7, "-0.0000001"),
            (1e-10, "0.0000000001"),
            (1.234e-10, "0.0000000001234"),
            (1.2345678901234567e-10, "0.00000000012345678901234568"),
            (1.2345678901234567e-5, "0.000012345678901234568"),
            (-1.2345678901234567e-5, "-0.000012345678901234568"),
            (1.2345678901234567e-3, "0.0012345678901234567"),
            (1e-15, "0.000000000000001"),
            (-1e-15, "-0.000000000000001"),
            (1.5e-15, "0.0000000000000015"),
            (1.2345678901234567e-15, "0.0000000000000012345678901234568"),
            (9.99e-16, "9.99e-16"),
            (1e-16, "1e-16"),
            (-1e-16, "-1e-16"),
            (1.2345678901234567e-16, "1.2345678901234568e-16"),
            (1e-100, "1e-100"),
            (2.2250738585072014e-308, "2.2250738585072014e-308"),
            (5e-324, "5e-324"),
            (0.0, "0"),
            (-0.0, "-0"),
        ];
        for (value, expected) in cases {
            assert_eq!(
                format_text(&Value::from_f64(*value), None),
                *expected,
                "{value:e}"
            );
        }
    }

    #[test]
    fn formats_values_like_the_mysql_text_protocol() {
        assert_eq!(format_text(&Value::Null, None), "NULL");
        assert_eq!(format_text(&Value::from_i64(-42), None), "-42");
        assert_eq!(format_text(&Value::build_text("Grüße"), None), "Grüße");
        assert_eq!(format_text(&Value::from_blob(b"abc".to_vec()), None), "abc");
        assert_eq!(
            format_text(&Value::from_blob(vec![0x61, 0xFF]), None),
            "a\u{FFFD}"
        );
    }

    /// `SELECT 64/77*77, 1/100000 * -1, 2/4` in MySQL 8.4.10.
    #[test]
    fn decimals_show_exactly_the_scale_of_their_column() {
        let shown = |value: Value| format_with_decimal_scale(&value, Some(4));
        assert_eq!(shown(Value::build_text("63.999999987")), "64.0000");
        assert_eq!(shown(Value::build_text("-0.000010000")), "0.0000");
        assert_eq!(shown(Value::build_text("0.5")), "0.5000");
        assert_eq!(shown(Value::from_i64(3)), "3.0000");
        assert_eq!(shown(Value::from_f64(0.25)), "0.25");
        assert_eq!(
            format_text(&Value::build_text("1.50"), None),
            "1.50",
            "a decimal of unknown scale shows its digits"
        );
    }
}
