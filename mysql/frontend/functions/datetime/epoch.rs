use std::time::{SystemTime, UNIX_EPOCH};

use turso_core::{LimboError, Result, Value};

use super::arg::Arg;
use super::build::{date_value, datetime_value, time_value};
use super::calendar;
use super::format;
use super::parse::DateFlags;
use super::temporal::{MyTime, MAX_FSP};
use super::zone::Zone;

const LATEST_TIMESTAMP: i64 = 32_536_771_199;

pub(super) fn convert_tz(args: &[Arg]) -> Result<Value> {
    let (Some(from), Some(to)) = (args[1].text(), args[2].text()) else {
        return Ok(Value::Null);
    };
    let (Some(from), Some(to)) = (Zone::find(&from), Zone::find(&to)) else {
        return Ok(Value::Null);
    };
    let Some(t) = args[0].datetime(DateFlags::NO_ZERO_DATE) else {
        return Ok(Value::Null);
    };
    let fsp = args[0].datetime_fsp();
    Ok(datetime_value(convert(t, &from, &to), fsp))
}

fn convert(t: MyTime, from: &Zone, to: &Zone) -> MyTime {
    if !in_timestamp_range(&t) {
        return t;
    }
    match from.local_to_utc(local_seconds(&t)) {
        Some(utc) if utc > 0 && utc <= LATEST_TIMESTAMP => {
            from_local_seconds(to.utc_to_local(utc), t.micro).unwrap_or(t)
        }
        _ => t,
    }
}

pub(super) fn from_unixtime(args: &[Arg]) -> Result<Value> {
    let fsp = args[0].number_fsp();
    let parts = if fsp > 0 {
        match args[0].decimal() {
            Some(decimal) => match decimal.whole_and_nanos() {
                Some(parts) => Some(parts),
                None => return Ok(Value::Null),
            },
            None => None,
        }
    } else {
        args[0].int()?.map(|seconds| (seconds, 0))
    };
    let Some((seconds, nanos)) = parts else {
        return Ok(Value::Null);
    };
    if seconds < 0 || nanos < 0 || seconds > LATEST_TIMESTAMP {
        return Ok(Value::Null);
    }
    let Some(mut t) = from_local_seconds(seconds, (nanos / 1000) as u32) else {
        return Ok(Value::Null);
    };
    if !t.add_nanos_to_datetime((nanos % 1000) as u32) || local_seconds(&t) > LATEST_TIMESTAMP {
        return Ok(Value::Null);
    }
    match args.get(1) {
        None => Ok(datetime_value(t, fsp)),
        Some(pattern) => Ok(match pattern.text() {
            Some(pattern) if !pattern.is_empty() => {
                format::format_with_pattern(&t, &pattern, false)
                    .map_or(Value::Null, Value::build_text)
            }
            _ => Value::Null,
        }),
    }
}

pub(super) fn unix_timestamp(args: &[Arg]) -> Result<Value> {
    let Some(arg) = args.first() else {
        return Ok(Value::from_i64(now().0));
    };
    if arg.is_null() {
        return Ok(Value::Null);
    }
    let fsp = arg.datetime_fsp();
    let (seconds, micro) = match arg.datetime(DateFlags::FUZZY) {
        Some(t) if in_timestamp_range(&t) => {
            let seconds = local_seconds(&t);
            if (0..=LATEST_TIMESTAMP).contains(&seconds) {
                (seconds, t.micro)
            } else {
                (0, 0)
            }
        }
        _ => (0, 0),
    };
    if fsp == 0 {
        return Ok(Value::from_i64(seconds));
    }
    let fraction = format!("{micro:06}");
    Ok(Value::build_text(format!(
        "{seconds}.{}",
        &fraction[..usize::from(fsp)]
    )))
}

pub(super) fn now_datetime(args: &[Arg]) -> Result<Value> {
    let fsp = fsp_argument(args)?;
    let (seconds, micro) = now();
    let t = from_local_seconds(seconds, truncate_micro(micro, fsp)).expect("now is a valid date");
    Ok(datetime_value(t, fsp))
}

pub(super) fn now_date(_args: &[Arg]) -> Result<Value> {
    let (seconds, _) = now();
    let t = from_local_seconds(seconds, 0).expect("now is a valid date");
    Ok(date_value(t))
}

pub(super) fn now_time(args: &[Arg]) -> Result<Value> {
    let fsp = fsp_argument(args)?;
    let (seconds, micro) = now();
    let t = from_local_seconds(seconds, truncate_micro(micro, fsp)).expect("now is a valid date");
    Ok(time_value(
        MyTime::time(false, t.hour, t.minute, t.second, t.micro),
        fsp,
    ))
}

fn fsp_argument(args: &[Arg]) -> Result<u8> {
    let Some(arg) = args.first() else {
        return Ok(0);
    };
    match arg.int()? {
        Some(fsp) if (0..=i64::from(MAX_FSP)).contains(&fsp) => Ok(fsp as u8),
        Some(fsp) => Err(LimboError::InvalidArgument(format!(
            "Too-big precision {fsp} specified for 'now'. Maximum is 6."
        ))),
        None => Ok(0),
    }
}

fn truncate_micro(micro: u32, fsp: u8) -> u32 {
    let scale = 10u32.pow(u32::from(MAX_FSP - fsp));
    micro / scale * scale
}

fn now() -> (i64, u32) {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (elapsed.as_secs() as i64, elapsed.subsec_micros())
}

fn in_timestamp_range(t: &MyTime) -> bool {
    if t.month == 0 || t.day == 0 {
        return false;
    }
    match t.year {
        1970..=3000 => true,
        1969 => t.month == 12 && t.day == 31,
        3001 => t.month <= 1 && t.day <= 19,
        _ => false,
    }
}

fn local_seconds(t: &MyTime) -> i64 {
    calendar::seconds_since_epoch(t.year, t.month, t.day, t.hour, t.minute, t.second)
}

fn from_local_seconds(seconds: i64, micro: u32) -> Option<MyTime> {
    MyTime::from_day_seconds(seconds + calendar::day_number(1970, 1, 1) * 86_400, micro)
}
