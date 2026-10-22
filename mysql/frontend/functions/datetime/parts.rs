use turso_core::{Result, Value};

use super::arg::Arg;
use super::calendar::{self, WEEK_YEAR};
use super::format::{DAY_NAMES, MONTH_NAMES};
use super::parse::DateFlags;
use super::temporal::MyTime;

pub(super) fn year(args: &[Arg]) -> Result<Value> {
    Ok(date_part(&args[0], DateFlags::FUZZY, |t| i64::from(t.year)))
}

pub(super) fn month(args: &[Arg]) -> Result<Value> {
    Ok(date_part(&args[0], DateFlags::FUZZY, |t| {
        i64::from(t.month)
    }))
}

pub(super) fn day_of_month(args: &[Arg]) -> Result<Value> {
    Ok(date_part(&args[0], DateFlags::FUZZY, |t| i64::from(t.day)))
}

pub(super) fn quarter(args: &[Arg]) -> Result<Value> {
    Ok(date_part(&args[0], DateFlags::FUZZY, |t| {
        i64::from(t.month.div_ceil(3))
    }))
}

pub(super) fn day_of_year(args: &[Arg]) -> Result<Value> {
    Ok(date_part(&args[0], DateFlags::NO_ZERO_DATE, |t| {
        t.day_number() - calendar::day_number(t.year, 1, 1) + 1
    }))
}

pub(super) fn weekday(args: &[Arg]) -> Result<Value> {
    Ok(date_part(&args[0], DateFlags::NO_ZERO_DATE, |t| {
        i64::from(calendar::weekday(t.day_number(), false))
    }))
}

pub(super) fn day_of_week(args: &[Arg]) -> Result<Value> {
    Ok(date_part(&args[0], DateFlags::NO_ZERO_DATE, |t| {
        i64::from(calendar::weekday(t.day_number(), true)) + 1
    }))
}

pub(super) fn week(args: &[Arg]) -> Result<Value> {
    let Some(t) = args[0].datetime(DateFlags::NO_ZERO_DATE) else {
        return Ok(Value::Null);
    };
    let behaviour = calendar::week_mode(week_mode_argument(args)?);
    Ok(Value::from_i64(i64::from(
        calendar::week(t.year, t.month, t.day, behaviour).0,
    )))
}

pub(super) fn week_of_year(args: &[Arg]) -> Result<Value> {
    Ok(date_part(&args[0], DateFlags::NO_ZERO_DATE, |t| {
        i64::from(calendar::week(t.year, t.month, t.day, calendar::week_mode(3)).0)
    }))
}

pub(super) fn year_week(args: &[Arg]) -> Result<Value> {
    let Some(t) = args[0].datetime(DateFlags::NO_ZERO_DATE) else {
        return Ok(Value::Null);
    };
    let behaviour = calendar::week_mode(week_mode_argument(args)?) | WEEK_YEAR;
    let (week, year) = calendar::week(t.year, t.month, t.day, behaviour);
    Ok(Value::from_i64(i64::from(
        week.wrapping_add(year.wrapping_mul(100)),
    )))
}

fn week_mode_argument(args: &[Arg]) -> Result<u32> {
    Ok(match args.get(1) {
        Some(mode) => mode.int()?.unwrap_or(0) as u32,
        None => 0,
    })
}

pub(super) fn to_days(args: &[Arg]) -> Result<Value> {
    Ok(date_part(&args[0], DateFlags::NO_ZERO_DATE, |t| {
        t.day_number()
    }))
}

pub(super) fn to_seconds(args: &[Arg]) -> Result<Value> {
    Ok(date_part(&args[0], DateFlags::NO_ZERO_DATE, |t| {
        t.day_number() * 86_400 + t.seconds_of_day()
    }))
}

pub(super) fn month_name(args: &[Arg]) -> Result<Value> {
    Ok(match args[0].datetime(DateFlags::FUZZY) {
        Some(t) if t.month != 0 => Value::build_text(MONTH_NAMES[t.month as usize - 1]),
        _ => Value::Null,
    })
}

pub(super) fn day_name(args: &[Arg]) -> Result<Value> {
    Ok(match args[0].datetime(DateFlags::NO_ZERO_DATE) {
        Some(t) => Value::build_text(DAY_NAMES[calendar::weekday(t.day_number(), false) as usize]),
        None => Value::Null,
    })
}

fn date_part(arg: &Arg, flags: DateFlags, part: impl Fn(&MyTime) -> i64) -> Value {
    arg.datetime(flags)
        .map_or(Value::Null, |t| Value::from_i64(part(&t)))
}

pub(super) fn hour(args: &[Arg]) -> Result<Value> {
    Ok(time_part(&args[0], |t| i64::from(t.hour)))
}

pub(super) fn minute(args: &[Arg]) -> Result<Value> {
    Ok(time_part(&args[0], |t| i64::from(t.minute)))
}

pub(super) fn second(args: &[Arg]) -> Result<Value> {
    Ok(time_part(&args[0], |t| i64::from(t.second)))
}

pub(super) fn microsecond(args: &[Arg]) -> Result<Value> {
    Ok(time_part(&args[0], |t| i64::from(t.micro)))
}

pub(super) fn time_to_sec(args: &[Arg]) -> Result<Value> {
    Ok(time_part(&args[0], |t| {
        if t.negative {
            -t.seconds_of_day()
        } else {
            t.seconds_of_day()
        }
    }))
}

fn time_part(arg: &Arg, part: impl Fn(&MyTime) -> i64) -> Value {
    arg.time()
        .map_or(Value::Null, |t| Value::from_i64(part(&t)))
}
