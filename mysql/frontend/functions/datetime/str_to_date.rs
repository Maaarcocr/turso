use turso_core::{Result, Value};

use super::arg::{int_from_text, Arg};
use super::build::{date_value, datetime_value, time_value};
use super::calendar::{self, MAX_DAY_NUMBER};
use super::format::{DAY_NAMES, MONTH_NAMES};
use super::parse::{self, DateFlags};
use super::temporal::{Kind, MyTime, MAX_FSP};

pub(super) fn str_to_date(args: &[Arg]) -> Result<Value> {
    let (Some(text), Some(pattern)) = (args[0].text(), args[1].text()) else {
        return Ok(Value::Null);
    };
    let (kind, fsp) = result_type(pattern.as_bytes());
    let flags = if kind == Kind::Time {
        DateFlags::default()
    } else {
        DateFlags::CAST
    };
    let Some(mut t) = extract(pattern.as_bytes(), text.as_bytes(), flags) else {
        return Ok(Value::Null);
    };
    Ok(match kind {
        Kind::Date => date_value(t),
        Kind::DateTime => datetime_value(t, fsp),
        Kind::Time => {
            t.hour += t.day * 24;
            time_value(
                MyTime::time(false, t.hour, t.minute, t.second, t.micro),
                fsp,
            )
        }
    })
}

fn result_type(pattern: &[u8]) -> (Kind, u8) {
    const TIME_SPECIFIERS: &[u8] = b"HISThiklrs";
    const DATE_SPECIFIERS: &[u8] = b"MVUXYWabcjmvuxyw";
    let mut uses_date = false;
    let mut uses_time = false;
    let mut uses_fraction = false;
    let mut pos = 0;
    while pos < pattern.len() {
        if pattern[pos] == b'%' && pos + 1 < pattern.len() {
            pos += 1;
            let spec = pattern[pos];
            if spec == b'f' {
                uses_fraction = true;
                uses_time = true;
            } else if !uses_time && TIME_SPECIFIERS.contains(&spec) {
                uses_time = true;
            } else if !uses_date && DATE_SPECIFIERS.contains(&spec) {
                uses_date = true;
            }
            if uses_date && uses_fraction {
                return (Kind::DateTime, MAX_FSP);
            }
        }
        pos += 1;
    }
    match (uses_fraction, uses_time, uses_date) {
        (true, _, _) => (Kind::Time, MAX_FSP),
        (false, true, true) => (Kind::DateTime, 0),
        (false, true, false) => (Kind::Time, 0),
        _ => (Kind::Date, 0),
    }
}

#[derive(Default)]
struct Extracted {
    time: MyTime,
    weekday: u32,
    day_of_year: i64,
    day_part: u32,
    twelve_hour: bool,
    week_number: Option<u32>,
    sunday_first_week: bool,
    strict_week_number: bool,
    week_year: Option<u32>,
    week_year_sunday_first: bool,
}

fn extract(pattern: &[u8], text: &[u8], flags: DateFlags) -> Option<MyTime> {
    let mut extracted = Extracted::default();
    extract_fields(pattern, text, &mut extracted)?;
    let mut t = extracted.time;
    if extracted.twelve_hour {
        if t.hour > 12 || t.hour < 1 {
            return None;
        }
        t.hour = t.hour % 12 + extracted.day_part;
    }
    if extracted.day_of_year > 0 {
        let day_number = calendar::day_number(t.year, 1, 1) + extracted.day_of_year - 1;
        if day_number <= 0 || day_number > MAX_DAY_NUMBER {
            return None;
        }
        (t.year, t.month, t.day) = calendar::date_from_day_number(day_number);
    }
    if let (Some(week_number), true) = (extracted.week_number, extracted.weekday != 0) {
        apply_week(&mut t, &extracted, week_number)?;
    }
    if t.month > 12 || t.day > 31 || t.hour > 23 || t.minute > 59 || t.second > 59 {
        return None;
    }
    let not_zero_date = !t.is_zero_date();
    if !parse::date_is_allowed(&t, not_zero_date, flags) {
        return None;
    }
    Some(t)
}

fn apply_week(t: &mut MyTime, extracted: &Extracted, week_number: u32) -> Option<()> {
    let sunday_first = extracted.sunday_first_week;
    if extracted.strict_week_number {
        if extracted.week_year.is_none() || extracted.week_year_sunday_first != sunday_first {
            return None;
        }
    } else if extracted.week_year.is_some() {
        return None;
    }
    let year = if extracted.strict_week_number {
        extracted.week_year.unwrap_or(0)
    } else {
        t.year
    };
    let first_day = calendar::day_number(year, 1, 1);
    let first_weekday = i64::from(calendar::weekday(first_day, sunday_first));
    let weekday = i64::from(extracted.weekday);
    let week_offset = (i64::from(week_number) - 1) * 7;
    let day_number = if sunday_first {
        first_day + if first_weekday == 0 { 0 } else { 7 } - first_weekday
            + week_offset
            + weekday % 7
    } else {
        first_day + if first_weekday <= 3 { 0 } else { 7 } - first_weekday + week_offset + weekday
            - 1
    };
    if day_number <= 0 || day_number > MAX_DAY_NUMBER {
        return None;
    }
    (t.year, t.month, t.day) = calendar::date_from_day_number(day_number);
    Some(())
}

fn extract_fields<'a>(pattern: &[u8], text: &'a [u8], out: &mut Extracted) -> Option<&'a [u8]> {
    let mut text = text;
    let mut pos = 0;
    while pos < pattern.len() && !text.is_empty() {
        text = skip_spaces(text);
        if text.is_empty() {
            break;
        }
        let c = pattern[pos];
        if c == b'%' && pos + 1 < pattern.len() {
            pos += 1;
            text = extract_field(pattern[pos], text, out)?;
        } else if !parse::is_space(c) {
            if text[0] != c {
                return None;
            }
            text = &text[1..];
        }
        pos += 1;
    }
    Some(text)
}

fn extract_field<'a>(spec: u8, text: &'a [u8], out: &mut Extracted) -> Option<&'a [u8]> {
    let t = &mut out.time;
    Some(match spec {
        b'Y' => {
            let (year, length, rest) = number(text, 4)?;
            t.year = if length <= 2 {
                two_digit_year(year)
            } else {
                year
            };
            rest
        }
        b'y' => {
            let (year, _, rest) = number(text, 2)?;
            t.year = two_digit_year(year);
            rest
        }
        b'm' | b'c' => store(text, 2, &mut t.month)?,
        b'M' => {
            let (month, rest) = word(text, &MONTH_NAMES.map(|name| name.to_string()))?;
            t.month = month;
            rest
        }
        b'b' => {
            let (month, rest) = word(text, &MONTH_NAMES.map(|name| name[..3].to_string()))?;
            t.month = month;
            rest
        }
        b'd' | b'e' => store(text, 2, &mut t.day)?,
        b'D' => {
            let rest = store(text, 2, &mut t.day)?;
            &rest[rest.len().min(2)..]
        }
        b'h' | b'I' | b'l' => {
            out.twelve_hour = true;
            store(text, 2, &mut t.hour)?
        }
        b'k' | b'H' => store(text, 2, &mut t.hour)?,
        b'i' => store(text, 2, &mut t.minute)?,
        b's' | b'S' => store(text, 2, &mut t.second)?,
        b'f' => {
            let (fraction, length, rest) = number(text, 6)?;
            t.micro = fraction * 10u32.pow(6 - length as u32);
            rest
        }
        b'p' => {
            if text.len() < 2 || !out.twelve_hour {
                return None;
            }
            if text[..2].eq_ignore_ascii_case(b"PM") {
                out.day_part = 12;
            } else if !text[..2].eq_ignore_ascii_case(b"AM") {
                return None;
            }
            &text[2..]
        }
        b'W' => {
            let (weekday, rest) = word(text, &DAY_NAMES.map(|name| name.to_string()))?;
            out.weekday = weekday;
            rest
        }
        b'a' => {
            let (weekday, rest) = word(text, &DAY_NAMES.map(|name| name[..3].to_string()))?;
            out.weekday = weekday;
            rest
        }
        b'w' => {
            let (weekday, _, rest) = number(text, 1)?;
            if weekday >= 7 {
                return None;
            }
            out.weekday = if weekday == 0 { 7 } else { weekday };
            rest
        }
        b'j' => {
            let (day, _, rest) = number(text, 3)?;
            out.day_of_year = i64::from(day);
            rest
        }
        b'V' | b'U' | b'v' | b'u' => {
            out.sunday_first_week = matches!(spec, b'U' | b'V');
            out.strict_week_number = matches!(spec, b'V' | b'v');
            let (week, _, rest) = number(text, 2)?;
            if (out.strict_week_number && week == 0) || week > 53 {
                return None;
            }
            out.week_number = Some(week);
            rest
        }
        b'X' | b'x' => {
            out.week_year_sunday_first = spec == b'X';
            let (year, _, rest) = number(text, 4)?;
            out.week_year = Some(year);
            rest
        }
        b'r' => extract_fields(b"%I:%i:%S %p", text, out)?,
        b'T' => extract_fields(b"%H:%i:%S", text, out)?,
        b'.' => skip_while(text, |b| b.is_ascii_punctuation()),
        b'@' => skip_while(text, |b| b.is_ascii_alphabetic()),
        b'#' => skip_while(text, |b| b.is_ascii_digit()),
        _ => return None,
    })
}

fn store<'a>(text: &'a [u8], width: usize, field: &mut u32) -> Option<&'a [u8]> {
    let (value, _, rest) = number(text, width)?;
    *field = value;
    Some(rest)
}

fn number(text: &[u8], width: usize) -> Option<(u32, usize, &[u8])> {
    let window = &text[..text.len().min(width)];
    let digits_start = window
        .iter()
        .position(|b| !parse::is_space(*b))
        .unwrap_or(window.len());
    let mut end = digits_start;
    if end < window.len() && (window[end] == b'-' || window[end] == b'+') {
        end += 1;
    }
    let sign_end = end;
    while end < window.len() && window[end].is_ascii_digit() {
        end += 1;
    }
    if end == sign_end {
        return None;
    }
    let value = int_from_text(&window[..end]);
    Some((value as u32, end, &text[end..]))
}

fn word<'a>(text: &'a [u8], names: &[String]) -> Option<(u32, &'a [u8])> {
    let length = text.iter().take_while(|b| b.is_ascii_alphabetic()).count();
    let candidate = &text[..length];
    if candidate.is_empty() {
        return None;
    }
    let exact = names
        .iter()
        .position(|name| name.as_bytes().eq_ignore_ascii_case(candidate));
    let index = match exact {
        Some(index) => index,
        None => {
            let mut matches = names.iter().enumerate().filter(|(_, name)| {
                name.len() >= length && name.as_bytes()[..length].eq_ignore_ascii_case(candidate)
            });
            let (index, _) = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            index
        }
    };
    Some((index as u32 + 1, &text[length..]))
}

fn skip_while(text: &[u8], keep: impl Fn(u8) -> bool) -> &[u8] {
    let length = text.iter().take_while(|b| keep(**b)).count();
    &text[length..]
}

fn skip_spaces(text: &[u8]) -> &[u8] {
    skip_while(text, parse::is_space)
}

fn two_digit_year(year: u32) -> u32 {
    if year < 70 {
        year + 2000
    } else if year < 100 {
        year + 1900
    } else {
        year
    }
}
