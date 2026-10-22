use crate::numeric::Numeric;
use crate::{ConversionErrorKind, LimboError, Result, TypeConversionError, Value};
use chrono::{Datelike, NaiveDate};
use std::cmp::Ordering;

const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;
pub(crate) const ZERO_DATE: i64 = i64::MIN;
const MAX_FSP: i64 = 6;

pub(crate) fn date_days_encode(value: &Value) -> Result<Value> {
    let text = expect_text(value, "date")?;
    let days = match parse_date(text, "date")? {
        Some(date) => days_since_epoch(date),
        None => ZERO_DATE,
    };
    Ok(Value::from_i64(days))
}

pub(crate) fn date_days_decode(value: &Value) -> Result<Value> {
    let days = expect_stored_integer(value, "date")?;
    if days == ZERO_DATE {
        return Ok(Value::build_text("0000-00-00"));
    }
    Ok(Value::build_text(format_date(date_from_days(days, value)?)))
}

pub(crate) fn datetime_micros_encode(value: &Value, fsp: &Value) -> Result<Value> {
    let fsp = fsp_digits(fsp)?;
    let text = expect_text(value, "datetime")?;
    let invalid = || invalid_value("datetime", text);
    let (date_text, time_text) = text.split_once(' ').ok_or_else(invalid)?;
    let date = parse_date(date_text, "datetime")?;
    let clock = parse_clock(time_text, fsp, "datetime")?;
    if clock.negative || clock.hours > 23 || !time_text.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(invalid());
    }
    match date {
        Some(date) => Ok(Value::from_i64(
            days_since_epoch(date) * MICROS_PER_DAY + clock.micros(),
        )),
        None if clock.micros() == 0 => Ok(Value::from_i64(ZERO_DATE)),
        None => Err(invalid()),
    }
}

pub(crate) fn datetime_micros_decode(value: &Value, fsp: &Value) -> Result<Value> {
    let fsp = fsp_digits(fsp)?;
    let micros = expect_stored_integer(value, "datetime")?;
    if micros == ZERO_DATE {
        return Ok(Value::build_text(format!(
            "0000-00-00 00:00:00{}",
            fraction_text(0, fsp)
        )));
    }
    let days = micros.div_euclid(MICROS_PER_DAY);
    let time = micros.rem_euclid(MICROS_PER_DAY);
    let date = date_from_days(days, value)?;
    Ok(Value::build_text(format!(
        "{} {}",
        format_date(date),
        format_clock(time, fsp, value)?
    )))
}

pub(crate) fn time_micros_encode(value: &Value, fsp: &Value) -> Result<Value> {
    let fsp = fsp_digits(fsp)?;
    let text = expect_text(value, "time")?;
    let clock = parse_clock(text, fsp, "time")?;
    Ok(Value::from_i64(if clock.negative {
        -clock.micros()
    } else {
        clock.micros()
    }))
}

pub(crate) fn time_micros_decode(value: &Value, fsp: &Value) -> Result<Value> {
    let fsp = fsp_digits(fsp)?;
    let micros = expect_stored_integer(value, "time")?;
    let sign = if micros < 0 { "-" } else { "" };
    let clock = format_clock(micros.unsigned_abs() as i64, fsp, value)?;
    Ok(Value::build_text(format!("{sign}{clock}")))
}

pub(crate) fn time_micros_compare(lhs: &Value, rhs: &Value) -> Result<Ordering> {
    let six = Value::from_i64(MAX_FSP);
    let lhs = time_micros_encode(lhs, &six)?;
    let rhs = time_micros_encode(rhs, &six)?;
    Ok(lhs.cmp(&rhs))
}

fn expect_text<'a>(value: &'a Value, type_name: &str) -> Result<&'a str> {
    match value {
        Value::Text(text) => Ok(text.as_str()),
        other => Err(invalid_value(type_name, &other.to_string())),
    }
}

fn expect_stored_integer(value: &Value, type_name: &str) -> Result<i64> {
    match value {
        Value::Numeric(Numeric::Integer(number)) => Ok(*number),
        other => Err(LimboError::Corrupt(format!(
            "stored {type_name} value is not an integer: {other}"
        ))),
    }
}

fn fsp_digits(fsp: &Value) -> Result<u32> {
    match fsp {
        Value::Numeric(Numeric::Integer(digits)) if (0..=MAX_FSP).contains(digits) => {
            Ok(*digits as u32)
        }
        other => Err(LimboError::Constraint(format!(
            "fractional seconds precision must be between 0 and 6, got {other}"
        ))),
    }
}

fn parse_date(text: &str, type_name: &str) -> Result<Option<NaiveDate>> {
    let invalid = || invalid_value(type_name, text);
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(invalid());
    }
    let year = digits(&text[0..4]).ok_or_else(invalid)?;
    let month = digits(&text[5..7]).ok_or_else(invalid)?;
    let day = digits(&text[8..10]).ok_or_else(invalid)?;
    if year == 0 && month == 0 && day == 0 {
        return Ok(None);
    }
    NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)
        .map(Some)
        .ok_or_else(invalid)
}

struct Clock {
    negative: bool,
    hours: i64,
    minutes: i64,
    seconds: i64,
    fraction_micros: i64,
}

impl Clock {
    fn micros(&self) -> i64 {
        ((self.hours * 60 + self.minutes) * 60 + self.seconds) * MICROS_PER_SECOND
            + self.fraction_micros
    }
}

fn parse_clock(text: &str, fsp: u32, type_name: &str) -> Result<Clock> {
    let invalid = || invalid_value(type_name, text);
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (clock, fraction) = match unsigned.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (unsigned, None),
    };
    let mut parts = clock.split(':');
    let (Some(hours), Some(minutes), Some(seconds), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid());
    };
    if hours.is_empty() || hours.len() > 9 || minutes.len() != 2 || seconds.len() != 2 {
        return Err(invalid());
    }
    let hours = digits(hours).ok_or_else(invalid)?;
    let minutes = digits(minutes).ok_or_else(invalid)?;
    let seconds = digits(seconds).ok_or_else(invalid)?;
    if minutes > 59 || seconds > 59 {
        return Err(invalid());
    }
    let fraction_micros = match fraction {
        Some(fraction) => rounded_fraction(fraction, fsp).ok_or_else(invalid)?,
        None => 0,
    };
    Ok(Clock {
        negative,
        hours,
        minutes,
        seconds,
        fraction_micros,
    })
}

fn rounded_fraction(fraction: &str, fsp: u32) -> Option<i64> {
    if fraction.is_empty() || fraction.len() > 9 {
        return None;
    }
    let value = digits(fraction)?;
    let scale = 10_i64.pow(9 - fraction.len() as u32);
    let nanos = value * scale;
    let unit = 10_i64.pow(9 - fsp);
    let rounded = (nanos + unit / 2) / unit * unit;
    Some(rounded / 1_000)
}

fn digits(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn days_since_epoch(date: NaiveDate) -> i64 {
    i64::from(date.num_days_from_ce() - epoch().num_days_from_ce())
}

fn date_from_days(days: i64, stored: &Value) -> Result<NaiveDate> {
    i32::try_from(days)
        .ok()
        .and_then(|days| epoch().checked_add_signed(chrono::Duration::days(i64::from(days))))
        .filter(|date| (0..=9999).contains(&date.year()))
        .ok_or_else(|| LimboError::Corrupt(format!("stored date is out of range: {stored}")))
}

fn epoch() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("1970-01-01 is a valid date")
}

fn format_date(date: NaiveDate) -> String {
    format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day())
}

fn format_clock(micros: i64, fsp: u32, stored: &Value) -> Result<String> {
    let seconds = micros / MICROS_PER_SECOND;
    let fraction = micros % MICROS_PER_SECOND;
    let unit = 10_i64.pow(6 - fsp);
    if fraction % unit != 0 {
        return Err(LimboError::Corrupt(format!(
            "stored value has more than {fsp} fraction digits: {stored}"
        )));
    }
    Ok(format!(
        "{:02}:{:02}:{:02}{}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        fraction_text(fraction / unit, fsp)
    ))
}

fn fraction_text(fraction: i64, fsp: u32) -> String {
    if fsp == 0 {
        return String::new();
    }
    format!(".{:0width$}", fraction, width = fsp as usize)
}

fn invalid_value(type_name: &str, text: &str) -> LimboError {
    TypeConversionError::raise(
        ConversionErrorKind::InvalidValue,
        format!("invalid value for {type_name}: {text}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Value {
        Value::build_text(value.to_string())
    }

    fn fsp(digits: i64) -> Value {
        Value::from_i64(digits)
    }

    #[test]
    fn dates_are_days_since_1970_and_the_zero_date_is_the_smallest_integer() {
        assert_eq!(
            date_days_encode(&text("1970-01-02")).unwrap(),
            Value::from_i64(1)
        );
        assert_eq!(
            date_days_encode(&text("1969-12-31")).unwrap(),
            Value::from_i64(-1)
        );
        assert_eq!(
            date_days_encode(&text("0000-00-00")).unwrap(),
            Value::from_i64(i64::MIN)
        );
        assert_eq!(
            date_days_decode(&Value::from_i64(i64::MIN)).unwrap(),
            text("0000-00-00")
        );
        for day in ["2024-05-01", "0001-01-01", "9999-12-31", "2024-02-29"] {
            let stored = date_days_encode(&text(day)).unwrap();
            assert_eq!(date_days_decode(&stored).unwrap(), text(day));
        }
        for bad in ["2024-00-10", "2024-01-00", "2023-02-29", "2024-5-1", "x"] {
            assert!(date_days_encode(&text(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn date_times_keep_their_fraction_digits() {
        let stored = datetime_micros_encode(&text("2024-05-01 10:30:00.25"), &fsp(2)).unwrap();
        assert_eq!(
            datetime_micros_decode(&stored, &fsp(2)).unwrap(),
            text("2024-05-01 10:30:00.25")
        );
        assert_eq!(
            datetime_micros_decode(&stored, &fsp(6)).unwrap(),
            text("2024-05-01 10:30:00.250000")
        );
        let rounded = datetime_micros_encode(&text("2024-05-01 23:59:59.6"), &fsp(0)).unwrap();
        assert_eq!(
            datetime_micros_decode(&rounded, &fsp(0)).unwrap(),
            text("2024-05-02 00:00:00")
        );
        let before_epoch = datetime_micros_encode(&text("1969-12-31 23:59:59.5"), &fsp(1)).unwrap();
        assert_eq!(before_epoch, Value::from_i64(-500_000));
        assert_eq!(
            datetime_micros_decode(&before_epoch, &fsp(1)).unwrap(),
            text("1969-12-31 23:59:59.5")
        );
        assert_eq!(
            datetime_micros_encode(&text("0000-00-00 00:00:00"), &fsp(0)).unwrap(),
            Value::from_i64(i64::MIN)
        );
        assert_eq!(
            datetime_micros_decode(&Value::from_i64(i64::MIN), &fsp(3)).unwrap(),
            text("0000-00-00 00:00:00.000")
        );
        assert!(datetime_micros_decode(&Value::from_i64(1), &fsp(0)).is_err());
        for bad in [
            "2024-05-01",
            "2024-05-01 24:00:00",
            "2024-05-01 10:60:00",
            "2024-05-01T10:00:00",
        ] {
            assert!(
                datetime_micros_encode(&text(bad), &fsp(0)).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn times_can_be_negative_and_exceed_24_hours() {
        for (time, micros) in [
            ("-01:00:00", -3_600_000_000_i64),
            ("100:00:00", 360_000_000_000),
            ("00:00:00.5", 500_000),
        ] {
            let stored = time_micros_encode(&text(time), &fsp(1)).unwrap();
            assert_eq!(stored, Value::from_i64(micros), "{time}");
        }
        assert_eq!(
            time_micros_decode(&Value::from_i64(-3_600_500_000), &fsp(1)).unwrap(),
            text("-01:00:00.5")
        );
        assert_eq!(
            time_micros_decode(&Value::from_i64(360_000_000_000), &fsp(0)).unwrap(),
            text("100:00:00")
        );
        let mut times = ["100:00:00", "20:00:00", "-01:00:00", "-02:00:00"].map(text);
        times.sort_by(|a, b| time_micros_compare(a, b).unwrap());
        assert_eq!(
            times,
            ["-02:00:00", "-01:00:00", "20:00:00", "100:00:00"].map(text)
        );
    }
}
