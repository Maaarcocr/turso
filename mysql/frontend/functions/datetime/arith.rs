use turso_core::{LimboError, Result, Value};

use super::arg::Arg;
use super::build::{date_value, datetime_value, time_value};
use super::calendar::{self, MAX_DAY_NUMBER};
use super::parse::DateFlags;
use super::temporal::{Kind, MyTime, MAX_FSP};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    Year,
    Quarter,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
    Microsecond,
    YearMonth,
    DayHour,
    DayMinute,
    DaySecond,
    HourMinute,
    HourSecond,
    MinuteSecond,
    DayMicrosecond,
    HourMicrosecond,
    MinuteMicrosecond,
    SecondMicrosecond,
}

#[derive(Clone, Copy, Debug, Default)]
struct Interval {
    negative: bool,
    year: u64,
    month: u64,
    day: u64,
    hour: u64,
    minute: u64,
    second: u64,
    micro: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Micro,
}

pub(super) fn date_add(args: &[Arg]) -> Result<Value> {
    add_interval(&args[0], &args[1], unit_argument(&args[2])?, false)
}

pub(super) fn date_sub(args: &[Arg]) -> Result<Value> {
    add_interval(&args[0], &args[1], unit_argument(&args[2])?, true)
}

pub(super) fn add_days(args: &[Arg]) -> Result<Value> {
    add_interval(&args[0], &args[1], Unit::Day, false)
}

pub(super) fn subtract_days(args: &[Arg]) -> Result<Value> {
    add_interval(&args[0], &args[1], Unit::Day, true)
}

pub(super) fn timestamp_add(args: &[Arg]) -> Result<Value> {
    add_interval(&args[2], &args[1], simple_unit_argument(&args[0])?, false)
}

fn add_interval(date: &Arg, amount: &Arg, unit: Unit, subtract: bool) -> Result<Value> {
    let Some(mut t) = date.datetime(DateFlags::NO_ZERO_DATE) else {
        return Ok(Value::Null);
    };
    let Some(mut interval) = interval_value(amount, unit)? else {
        return Ok(Value::Null);
    };
    interval.negative ^= subtract;
    if !add_to_datetime(&mut t, unit, &interval) {
        return Ok(Value::Null);
    }
    Ok(match t.kind {
        Kind::Date => date_value(t),
        _ => datetime_value(t, if t.micro == 0 { 0 } else { MAX_FSP }),
    })
}

fn interval_value(arg: &Arg, unit: Unit) -> Result<Option<Interval>> {
    let mut interval = Interval::default();
    if unit == Unit::Second && arg.number_fsp() > 0 {
        let Some(decimal) = arg.decimal() else {
            return Ok(None);
        };
        let Some((whole, nanos)) = decimal.whole_and_nanos() else {
            return Ok(None);
        };
        interval.negative = whole < 0 || nanos < 0;
        interval.second = whole.unsigned_abs();
        interval.micro = nanos.unsigned_abs() / 1000;
        return Ok(Some(interval));
    }
    if is_simple(unit) {
        let Some(value) = arg.int()? else {
            return Ok(None);
        };
        if value == i64::MIN {
            return Ok(None);
        }
        interval.negative = value < 0;
        let value = value.unsigned_abs();
        match unit {
            Unit::Year => interval.year = value,
            Unit::Quarter => interval.month = value.saturating_mul(3),
            Unit::Month => interval.month = value,
            Unit::Week => interval.day = value.saturating_mul(7),
            Unit::Day => interval.day = value,
            Unit::Hour => interval.hour = value,
            Unit::Minute => interval.minute = value,
            Unit::Second => interval.second = value,
            _ => interval.micro = value,
        }
        return Ok(Some(interval));
    }
    let Some(text) = arg.text() else {
        return Ok(None);
    };
    let text = text.trim_start_matches(|c: char| c.is_ascii() && super::parse::is_space(c as u8));
    let text = match text.strip_prefix('-') {
        Some(rest) => {
            interval.negative = true;
            rest
        }
        None => text,
    };
    let layout: &[Field] = match unit {
        Unit::YearMonth => &[Field::Year, Field::Month],
        Unit::DayHour => &[Field::Day, Field::Hour],
        Unit::DayMinute => &[Field::Day, Field::Hour, Field::Minute],
        Unit::DaySecond => &[Field::Day, Field::Hour, Field::Minute, Field::Second],
        Unit::DayMicrosecond => &[
            Field::Day,
            Field::Hour,
            Field::Minute,
            Field::Second,
            Field::Micro,
        ],
        Unit::HourMinute => &[Field::Hour, Field::Minute],
        Unit::HourSecond => &[Field::Hour, Field::Minute, Field::Second],
        Unit::HourMicrosecond => &[Field::Hour, Field::Minute, Field::Second, Field::Micro],
        Unit::MinuteSecond => &[Field::Minute, Field::Second],
        Unit::MinuteMicrosecond => &[Field::Minute, Field::Second, Field::Micro],
        _ => &[Field::Second, Field::Micro],
    };
    let has_micro = layout.last() == Some(&Field::Micro);
    let Some(values) = interval_fields(text.as_bytes(), layout.len(), has_micro) else {
        return Ok(None);
    };
    for (field, value) in layout.iter().zip(values) {
        let target = match field {
            Field::Year => &mut interval.year,
            Field::Month => &mut interval.month,
            Field::Day => &mut interval.day,
            Field::Hour => &mut interval.hour,
            Field::Minute => &mut interval.minute,
            Field::Second => &mut interval.second,
            Field::Micro => &mut interval.micro,
        };
        *target = value;
    }
    Ok(Some(interval))
}

fn interval_fields(text: &[u8], count: usize, has_micro: bool) -> Option<Vec<u64>> {
    let end = text.len();
    let mut pos = 0;
    let skip_non_digits = |mut pos: usize| {
        while pos < end && !text[pos].is_ascii_digit() {
            pos += 1;
        }
        pos
    };
    pos = skip_non_digits(pos);
    let mut values = vec![0u64; count];
    let mut last_length = 0;
    for i in 0..count {
        let start = pos;
        let mut value: u64 = 0;
        while pos < end && text[pos].is_ascii_digit() {
            value = value
                .saturating_mul(10)
                .saturating_add(u64::from(text[pos] - b'0'));
            pos += 1;
        }
        last_length = pos - start;
        pos = skip_non_digits(pos);
        values[i] = value;
        if pos == end && i != count - 1 {
            let found = i + 1;
            values.rotate_right(count - found);
            values[..count - found].fill(0);
            break;
        }
    }
    if has_micro && last_length > 0 {
        let micro = &mut values[count - 1];
        if last_length < 6 {
            *micro *= 10u64.pow((6 - last_length) as u32);
        } else if last_length > 6 {
            *micro /= 10u64.saturating_pow((last_length - 6) as u32);
        }
    }
    (pos == end).then_some(values)
}

fn add_to_datetime(t: &mut MyTime, unit: Unit, interval: &Interval) -> bool {
    t.negative = false;
    let sign: i128 = if interval.negative { -1 } else { 1 };
    match unit {
        Unit::Day | Unit::Week => {
            let day_number = i128::from(t.day_number()) + sign * i128::from(interval.day);
            if !(0..=i128::from(MAX_DAY_NUMBER)).contains(&day_number) {
                return false;
            }
            (t.year, t.month, t.day) = calendar::date_from_day_number(day_number as i64);
            true
        }
        Unit::Year => {
            let year = i128::from(t.year) + sign * i128::from(interval.year);
            if !(0..10_000).contains(&year) {
                return false;
            }
            t.year = year as u32;
            if t.month == 2 && t.day == 29 && calendar::days_in_year(t.year) != 366 {
                t.day = 28;
            }
            true
        }
        Unit::YearMonth | Unit::Quarter | Unit::Month => {
            let months = i128::from(t.year) * 12 + i128::from(t.month) - 1
                + sign * (i128::from(interval.year) * 12 + i128::from(interval.month));
            if !(0..120_000).contains(&months) {
                return false;
            }
            t.year = (months / 12) as u32;
            t.month = (months % 12) as u32 + 1;
            t.day = t.day.min(calendar::days_in_month(t.year, t.month));
            true
        }
        _ => add_time_units(t, interval, sign),
    }
}

fn add_time_units(t: &mut MyTime, interval: &Interval, sign: i128) -> bool {
    let max_days = MAX_DAY_NUMBER as u64;
    if interval.day > max_days
        || interval.hour > max_days * 24
        || interval.minute > max_days * 24 * 60
        || interval.second > max_days * 24 * 60 * 60
    {
        return false;
    }
    t.kind = Kind::DateTime;
    let micros = i128::from(t.micro) + sign * i128::from(interval.micro);
    let interval_seconds = i128::from(interval.day) * 86_400
        + i128::from(interval.hour) * 3_600
        + i128::from(interval.minute) * 60
        + i128::from(interval.second);
    let mut seconds = (i128::from(t.day) - 1) * 86_400
        + i128::from(t.seconds_of_day())
        + sign * interval_seconds
        + micros / 1_000_000;
    let mut micros = micros % 1_000_000;
    if micros < 0 {
        micros += 1_000_000;
        seconds -= 1;
    }
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let day_number = i128::from(calendar::day_number(t.year, t.month, 1)) + days;
    if !(0..=i128::from(MAX_DAY_NUMBER)).contains(&day_number) {
        return false;
    }
    (t.year, t.month, t.day) = calendar::date_from_day_number(day_number as i64);
    t.hour = (second_of_day / 3_600) as u32;
    t.minute = (second_of_day / 60 % 60) as u32;
    t.second = (second_of_day % 60) as u32;
    t.micro = micros as u32;
    true
}

pub(super) fn timestamp_diff(args: &[Arg]) -> Result<Value> {
    let unit = simple_unit_argument(&args[0])?;
    let (Some(begin), Some(end)) = (
        args[1].datetime(DateFlags::NO_ZERO_DATE),
        args[2].datetime(DateFlags::NO_ZERO_DATE),
    ) else {
        return Ok(Value::Null);
    };
    let micros = end.datetime_in_micros() - begin.datetime_in_micros();
    let sign: i128 = if micros < 0 { -1 } else { 1 };
    let seconds = micros.abs() / 1_000_000;
    let result = match unit {
        Unit::Year => months_between(&begin, &end, sign) / 12 * sign,
        Unit::Quarter => months_between(&begin, &end, sign) / 3 * sign,
        Unit::Month => months_between(&begin, &end, sign) * sign,
        Unit::Week => seconds / 86_400 / 7 * sign,
        Unit::Day => seconds / 86_400 * sign,
        Unit::Hour => seconds / 3_600 * sign,
        Unit::Minute => seconds / 60 * sign,
        Unit::Second => seconds * sign,
        _ => micros,
    };
    Ok(Value::from_i64(result as i64))
}

fn months_between(begin: &MyTime, end: &MyTime, sign: i128) -> i128 {
    let (first, last) = if sign < 0 { (end, begin) } else { (begin, end) };
    let ends_earlier_in_month =
        last.month < first.month || (last.month == first.month && last.day < first.day);
    let mut years = i128::from(last.year) - i128::from(first.year);
    if ends_earlier_in_month {
        years -= 1;
    }
    let mut months = 12 * years;
    if ends_earlier_in_month {
        months += 12 - (i128::from(first.month) - i128::from(last.month));
    } else {
        months += i128::from(last.month) - i128::from(first.month);
    }
    let first_clock = (first.seconds_of_day(), first.micro);
    let last_clock = (last.seconds_of_day(), last.micro);
    if last.day < first.day || (last.day == first.day && last_clock < first_clock) {
        months -= 1;
    }
    months
}

pub(super) fn extract(args: &[Arg]) -> Result<Value> {
    let unit = unit_argument(&args[0])?;
    let uses_date = matches!(
        unit,
        Unit::Year | Unit::YearMonth | Unit::Quarter | Unit::Month | Unit::Week | Unit::Day
    );
    let t = if uses_date {
        args[1].datetime(DateFlags::FUZZY)
    } else {
        args[1].time()
    };
    let Some(t) = t else {
        return Ok(Value::Null);
    };
    let sign: i64 = if !uses_date && t.negative { -1 } else { 1 };
    let day = i64::from(t.day);
    let hour = i64::from(t.hour);
    let minute = i64::from(t.minute);
    let second = i64::from(t.second);
    let micro = i64::from(t.micro);
    let result = match unit {
        Unit::Year => i64::from(t.year),
        Unit::YearMonth => i64::from(t.year) * 100 + i64::from(t.month),
        Unit::Quarter => i64::from(t.month.div_ceil(3)),
        Unit::Month => i64::from(t.month),
        Unit::Week => i64::from(calendar::week(t.year, t.month, t.day, calendar::week_mode(0)).0),
        Unit::Day => day,
        Unit::DayHour => (day * 100 + hour) * sign,
        Unit::DayMinute => (day * 10_000 + hour * 100 + minute) * sign,
        Unit::DaySecond => (day * 1_000_000 + hour * 10_000 + minute * 100 + second) * sign,
        Unit::Hour => hour * sign,
        Unit::HourMinute => (hour * 100 + minute) * sign,
        Unit::HourSecond => (hour * 10_000 + minute * 100 + second) * sign,
        Unit::Minute => minute * sign,
        Unit::MinuteSecond => (minute * 100 + second) * sign,
        Unit::Second => second * sign,
        Unit::Microsecond => micro * sign,
        Unit::DayMicrosecond => {
            ((day * 1_000_000 + hour * 10_000 + minute * 100 + second) * 1_000_000 + micro) * sign
        }
        Unit::HourMicrosecond => {
            ((hour * 10_000 + minute * 100 + second) * 1_000_000 + micro) * sign
        }
        Unit::MinuteMicrosecond => ((minute * 100 + second) * 1_000_000 + micro) * sign,
        Unit::SecondMicrosecond => (second * 1_000_000 + micro) * sign,
    };
    Ok(Value::from_i64(result))
}

pub(super) fn date_diff(args: &[Arg]) -> Result<Value> {
    let (Some(first), Some(second)) = (
        args[0].datetime(DateFlags::NO_ZERO_DATE),
        args[1].datetime(DateFlags::NO_ZERO_DATE),
    ) else {
        return Ok(Value::Null);
    };
    Ok(Value::from_i64(first.day_number() - second.day_number()))
}

pub(super) fn time_diff(args: &[Arg]) -> Result<Value> {
    let (Some(first), Some(second)) = (args[0].time(), args[1].time()) else {
        return Ok(Value::Null);
    };
    if first.kind != second.kind {
        return Ok(Value::Null);
    }
    let fsp = args[0].time_fsp().max(args[1].time_fsp());
    let micros = signed_micros(&first) - signed_micros(&second);
    Ok(time_value(time_from_micros(micros), fsp))
}

fn signed_micros(t: &MyTime) -> i128 {
    match t.kind {
        Kind::Time => t.time_in_micros(),
        _ => t.datetime_in_micros(),
    }
}

pub(super) fn add_time(args: &[Arg]) -> Result<Value> {
    add_time_value(args, 1)
}

pub(super) fn subtract_time(args: &[Arg]) -> Result<Value> {
    add_time_value(args, -1)
}

fn add_time_value(args: &[Arg], sign: i128) -> Result<Value> {
    let (Some(base), Some(amount)) = (args[0].time(), args[1].time()) else {
        return Ok(Value::Null);
    };
    if amount.kind == Kind::DateTime {
        return Ok(Value::Null);
    }
    let Some(result) = shift_by_time(&base, &amount, sign) else {
        return Ok(Value::Null);
    };
    let fsp = if result.micro == 0 { 0 } else { MAX_FSP };
    Ok(match result.kind {
        Kind::Time => time_value(result, fsp),
        _ => datetime_value(result, fsp),
    })
}

pub(super) fn timestamp_with_time(args: &[Arg]) -> Result<Value> {
    let Some(base) = args[0].datetime(DateFlags::FUZZY) else {
        return Ok(Value::Null);
    };
    let Some(amount) = args[1].time() else {
        return Ok(Value::Null);
    };
    if amount.kind != Kind::Time {
        return Ok(Value::Null);
    }
    let fsp = args[0].datetime_fsp().max(args[1].time_fsp());
    let base = base.with_kind(Kind::DateTime);
    Ok(match shift_by_time(&base, &amount, 1) {
        Some(result) => datetime_value(result, fsp),
        None => Value::Null,
    })
}

fn shift_by_time(base: &MyTime, amount: &MyTime, sign: i128) -> Option<MyTime> {
    let amount_micros = amount.time_in_micros() * sign;
    if base.kind == Kind::Time {
        return Some(time_from_micros(base.time_in_micros() + amount_micros));
    }
    let micros = base.datetime_in_micros() + amount_micros;
    if micros < 0 {
        return None;
    }
    let seconds = micros / 1_000_000;
    let day_number = (seconds / 86_400) as i64;
    let (year, month, day) = calendar::date_from_day_number(day_number);
    if day == 0 || year > 9999 {
        return None;
    }
    let second_of_day = (seconds % 86_400) as u32;
    Some(MyTime {
        kind: Kind::DateTime,
        negative: false,
        year,
        month,
        day,
        hour: second_of_day / 3_600,
        minute: second_of_day / 60 % 60,
        second: second_of_day % 60,
        micro: (micros % 1_000_000) as u32,
    })
}

fn time_from_micros(micros: i128) -> MyTime {
    let negative = micros < 0;
    let micros = micros.unsigned_abs();
    let seconds = micros / 1_000_000;
    let hours = seconds / 3_600;
    if hours > u128::from(u32::MAX) {
        return MyTime::max_time(negative);
    }
    let mut t = MyTime::time(
        negative,
        hours as u32,
        (seconds / 60 % 60) as u32,
        (seconds % 60) as u32,
        (micros % 1_000_000) as u32,
    );
    t.clamp_time();
    t
}

fn unit_argument(arg: &Arg) -> Result<Unit> {
    let name = arg.text().unwrap_or_default().to_ascii_uppercase();
    let unit = match name.as_str() {
        "YEAR" => Unit::Year,
        "QUARTER" => Unit::Quarter,
        "MONTH" => Unit::Month,
        "WEEK" => Unit::Week,
        "DAY" => Unit::Day,
        "HOUR" => Unit::Hour,
        "MINUTE" => Unit::Minute,
        "SECOND" => Unit::Second,
        "MICROSECOND" => Unit::Microsecond,
        "YEAR_MONTH" => Unit::YearMonth,
        "DAY_HOUR" => Unit::DayHour,
        "DAY_MINUTE" => Unit::DayMinute,
        "DAY_SECOND" => Unit::DaySecond,
        "HOUR_MINUTE" => Unit::HourMinute,
        "HOUR_SECOND" => Unit::HourSecond,
        "MINUTE_SECOND" => Unit::MinuteSecond,
        "DAY_MICROSECOND" => Unit::DayMicrosecond,
        "HOUR_MICROSECOND" => Unit::HourMicrosecond,
        "MINUTE_MICROSECOND" => Unit::MinuteMicrosecond,
        "SECOND_MICROSECOND" => Unit::SecondMicrosecond,
        _ => {
            return Err(LimboError::InvalidArgument(format!(
                "unknown interval unit: {name}"
            )))
        }
    };
    Ok(unit)
}

fn simple_unit_argument(arg: &Arg) -> Result<Unit> {
    let unit = unit_argument(arg)?;
    if is_simple(unit) {
        Ok(unit)
    } else {
        Err(LimboError::InvalidArgument(format!(
            "interval unit {unit:?} is not allowed here"
        )))
    }
}

fn is_simple(unit: Unit) -> bool {
    matches!(
        unit,
        Unit::Year
            | Unit::Quarter
            | Unit::Month
            | Unit::Week
            | Unit::Day
            | Unit::Hour
            | Unit::Minute
            | Unit::Second
            | Unit::Microsecond
    )
}
