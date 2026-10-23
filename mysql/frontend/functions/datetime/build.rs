use turso_core::{LimboError, Result, Value};

use super::arg::Arg;
use super::calendar::{self, MAX_DAY_NUMBER};
use super::format;
use super::parse::DateFlags;
use super::temporal::{Kind, MyTime, MAX_FSP};

const MAX_TIME_SECONDS: i64 = 838 * 3_600 + 59 * 60 + 59;

pub(super) fn make_date(args: &[Arg]) -> Result<Value> {
    let Some(day_of_year) = args[1].int()? else {
        return Ok(Value::Null);
    };
    let Some(year) = args[0].int()? else {
        return Ok(Value::Null);
    };
    if !(0..=9999).contains(&year) || day_of_year <= 0 {
        return Ok(Value::Null);
    }
    let year = if year < 100 {
        two_digit_year(year as u32)
    } else {
        year as u32
    };
    let first_day = calendar::day_number(year, 1, 1);
    match first_day.checked_add(day_of_year - 1) {
        Some(day_number) if (0..=MAX_DAY_NUMBER).contains(&day_number) => {
            let (year, month, day) = calendar::date_from_day_number(day_number);
            Ok(date_value(MyTime::date(year, month, day)))
        }
        _ => Ok(Value::Null),
    }
}

fn two_digit_year(year: u32) -> u32 {
    if year < 70 {
        year + 2000
    } else {
        year + 1900
    }
}

pub(super) fn make_time(args: &[Arg]) -> Result<Value> {
    let Some(hour) = args[0].int()? else {
        return Ok(Value::Null);
    };
    let Some(minute) = args[1].int()? else {
        return Ok(Value::Null);
    };
    let Some(seconds) = args[2].decimal() else {
        return Ok(Value::Null);
    };
    let Some((second, nanos)) = seconds.whole_and_nanos() else {
        return Ok(Value::Null);
    };
    if !(0..=59).contains(&minute) || !(0..=59).contains(&second) || nanos < 0 {
        return Ok(Value::Null);
    }
    let fsp = args[2].number_fsp();
    let negative = hour < 0 && !args[0].is_unsigned();
    let hours = hour.unsigned_abs();
    if hours > u64::from(u32::MAX) || (hour < 0 && args[0].is_unsigned()) {
        return Ok(time_value(MyTime::max_time(negative), fsp));
    }
    let mut time = MyTime::time(
        negative,
        hours as u32,
        minute as u32,
        second as u32,
        (nanos / 1000) as u32,
    );
    if time.clamp_time() {
        return Ok(time_value(time, fsp));
    }
    time.add_nanos_to_time((nanos % 1000) as u32);
    Ok(time_value(time, fsp))
}

pub(super) fn sec_to_time(args: &[Arg]) -> Result<Value> {
    let Some(seconds) = args[0].decimal() else {
        return Ok(Value::Null);
    };
    let fsp = args[0].number_fsp();
    let Some((whole, nanos)) = seconds.whole_and_nanos() else {
        return Ok(time_value(MyTime::max_time(seconds.negative), fsp));
    };
    let negative = whole < 0 || nanos < 0;
    let whole = whole.unsigned_abs();
    let nanos = nanos.unsigned_abs();
    if whole > MAX_TIME_SECONDS as u64 {
        return Ok(time_value(MyTime::max_time(negative), fsp));
    }
    let mut time = MyTime::time(
        negative,
        (whole / 3_600) as u32,
        (whole % 3_600 / 60) as u32,
        (whole % 60) as u32,
        0,
    );
    time.add_nanos_to_time(nanos as u32);
    Ok(time_value(time, fsp))
}

pub(super) fn from_days(args: &[Arg]) -> Result<Value> {
    let Some(day_number) = args[0].int()? else {
        return Ok(Value::Null);
    };
    let (year, month, day) = calendar::date_from_day_number(day_number);
    if year > 9999 {
        return Ok(Value::Null);
    }
    Ok(date_value(MyTime::date(year, month, day)))
}

pub(super) fn last_day(args: &[Arg]) -> Result<Value> {
    let Some(t) = args[0].datetime(DateFlags::FUZZY) else {
        return Ok(Value::Null);
    };
    if t.month == 0 {
        return Ok(Value::Null);
    }
    let day = calendar::days_in_month(t.year, t.month);
    Ok(date_value(MyTime::date(t.year, t.month, day)))
}

pub(super) fn period_add(args: &[Arg]) -> Result<Value> {
    let Some(period) = args[0].int()? else {
        return Ok(Value::Null);
    };
    let Some(months) = args[1].int()? else {
        return Ok(Value::Null);
    };
    if !is_valid_period(period) {
        return Err(wrong_arguments("period_add"));
    }
    let month = period_to_month(period as u64).wrapping_add(months as u64);
    Ok(Value::from_i64(month_to_period(month) as i64))
}

pub(super) fn period_diff(args: &[Arg]) -> Result<Value> {
    let Some(first) = args[0].int()? else {
        return Ok(Value::Null);
    };
    let Some(second) = args[1].int()? else {
        return Ok(Value::Null);
    };
    if !is_valid_period(first) || !is_valid_period(second) {
        return Err(wrong_arguments("period_diff"));
    }
    let difference =
        (period_to_month(first as u64) as i64).wrapping_sub(period_to_month(second as u64) as i64);
    Ok(Value::from_i64(difference))
}

fn is_valid_period(period: i64) -> bool {
    period > 0 && period % 100 != 0 && period % 100 <= 12
}

fn period_to_month(period: u64) -> u64 {
    let mut year = period / 100;
    if year < 70 {
        year += 2000;
    } else if year < 100 {
        year += 1900;
    }
    year.wrapping_mul(12)
        .wrapping_add(period % 100)
        .wrapping_sub(1)
}

fn month_to_period(month: u64) -> u64 {
    if month == 0 {
        return 0;
    }
    let mut year = month / 12;
    if year < 100 {
        year += if year < 70 { 2000 } else { 1900 };
    }
    year.wrapping_mul(100).wrapping_add(month % 12 + 1)
}

fn wrong_arguments(function: &str) -> LimboError {
    LimboError::InvalidArgument(format!("Incorrect arguments to {function}"))
}

pub(super) fn date(args: &[Arg]) -> Result<Value> {
    Ok(match args[0].datetime(DateFlags::CAST) {
        Some(t) => date_value(t),
        None => Value::Null,
    })
}

pub(super) fn time(args: &[Arg]) -> Result<Value> {
    Ok(time_with_precision(&args[0], args[0].time_fsp()))
}

pub(super) fn cast_as_time(args: &[Arg]) -> Result<Value> {
    Ok(time_with_precision(&args[0], cast_precision(&args[1])?))
}

fn time_with_precision(arg: &Arg, fsp: u8) -> Value {
    match arg.time() {
        Some(mut t) => {
            if t.kind != Kind::Time {
                t = MyTime::time(false, t.hour, t.minute, t.second, t.micro);
            }
            t.round_time(fsp);
            time_value(t, fsp)
        }
        None => Value::Null,
    }
}

pub(super) fn timestamp(args: &[Arg]) -> Result<Value> {
    Ok(datetime_with_precision(&args[0], args[0].datetime_fsp()))
}

pub(super) fn cast_as_datetime(args: &[Arg]) -> Result<Value> {
    Ok(datetime_with_precision(&args[0], cast_precision(&args[1])?))
}

fn datetime_with_precision(arg: &Arg, fsp: u8) -> Value {
    let Some(mut t) = arg.datetime(DateFlags::CAST) else {
        return Value::Null;
    };
    if t.round_datetime(fsp) {
        datetime_value(t, fsp)
    } else {
        Value::Null
    }
}

fn cast_precision(precision: &Arg) -> Result<u8> {
    match *precision {
        Arg::Int(fsp @ 0..=6) => Ok(fsp as u8),
        _ => Err(LimboError::InvalidArgument(
            "the precision of a temporal CAST must be an integer from 0 to 6".to_string(),
        )),
    }
}

pub(super) fn date_format(args: &[Arg]) -> Result<Value> {
    let Some(t) = args[0].datetime(DateFlags::FUZZY) else {
        return Ok(Value::Null);
    };
    Ok(format_with(&t, &args[1], false))
}

pub(super) fn time_format(args: &[Arg]) -> Result<Value> {
    let Some(t) = args[0].time() else {
        return Ok(Value::Null);
    };
    let t = MyTime::time(t.negative, t.hour, t.minute, t.second, t.micro);
    Ok(format_with(&t, &args[1], true))
}

fn format_with(t: &MyTime, pattern: &Arg, is_time: bool) -> Value {
    match pattern.text() {
        Some(pattern) if !pattern.is_empty() => {
            format::format_with_pattern(t, &pattern, is_time).map_or(Value::Null, Value::build_text)
        }
        _ => Value::Null,
    }
}

pub(super) fn date_value(t: MyTime) -> Value {
    Value::build_text(t.format_date())
}

pub(super) fn datetime_value(t: MyTime, fsp: u8) -> Value {
    Value::build_text(t.with_kind(Kind::DateTime).format(fsp.min(MAX_FSP)))
}

pub(super) fn time_value(t: MyTime, fsp: u8) -> Value {
    Value::build_text(t.with_kind(Kind::Time).format(fsp.min(MAX_FSP)))
}
