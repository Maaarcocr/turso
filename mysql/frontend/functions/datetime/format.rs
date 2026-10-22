use turso_core::{LimboError, Result, Value};

use super::arg::Arg;
use super::calendar::{self, WEEK_FIRST_WEEKDAY, WEEK_MONDAY_FIRST, WEEK_YEAR};
use super::temporal::MyTime;

pub(super) const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

pub(super) const DAY_NAMES: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

pub(super) fn format_with_pattern(time: &MyTime, pattern: &str, is_time: bool) -> Option<String> {
    let mut out = String::new();
    if time.negative {
        out.push('-');
    }
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(spec) = chars.next() else {
            out.push('%');
            break;
        };
        let has_date = !is_time && (time.month != 0 || time.year != 0);
        match spec {
            'M' => out.push_str(MONTH_NAMES.get(time.month.checked_sub(1)? as usize)?),
            'b' => out.push_str(&MONTH_NAMES.get(time.month.checked_sub(1)? as usize)?[..3]),
            'W' | 'a' => {
                if !has_date {
                    return None;
                }
                let name = DAY_NAMES[calendar::weekday(time.day_number(), false) as usize];
                out.push_str(if spec == 'W' { name } else { &name[..3] });
            }
            'D' => {
                if is_time {
                    return None;
                }
                out.push_str(&time.day.to_string());
                out.push_str(ordinal_suffix(time.day));
            }
            'Y' => out.push_str(&format!("{:04}", time.year)),
            'y' => out.push_str(&format!("{:02}", time.year % 100)),
            'm' => out.push_str(&format!("{:02}", time.month)),
            'c' => out.push_str(&time.month.to_string()),
            'd' => out.push_str(&format!("{:02}", time.day)),
            'e' => out.push_str(&time.day.to_string()),
            'f' => out.push_str(&format!("{:06}", time.micro)),
            'H' => out.push_str(&format!("{:02}", time.hour)),
            'h' | 'I' => out.push_str(&format!("{:02}", twelve_hour(time.hour % 24))),
            'i' => out.push_str(&format!("{:02}", time.minute)),
            'j' => {
                if is_time {
                    return None;
                }
                let day_of_year = time.day_number() - calendar::day_number(time.year, 1, 1) + 1;
                out.push_str(&format!("{day_of_year:03}"));
            }
            'k' => out.push_str(&time.hour.to_string()),
            'l' => out.push_str(&twelve_hour(time.hour % 24).to_string()),
            'p' => out.push_str(if time.hour % 24 < 12 { "AM" } else { "PM" }),
            'r' => out.push_str(&format!(
                "{:02}:{:02}:{:02} {}",
                twelve_hour(time.hour),
                time.minute,
                time.second,
                if time.hour % 24 < 12 { "AM" } else { "PM" }
            )),
            'S' | 's' => out.push_str(&format!("{:02}", time.second)),
            'T' => out.push_str(&format!(
                "{:02}:{:02}:{:02}",
                time.hour, time.minute, time.second
            )),
            'U' | 'u' | 'V' | 'v' | 'X' | 'x' => {
                if is_time {
                    return None;
                }
                let behaviour = match spec {
                    'U' => WEEK_FIRST_WEEKDAY,
                    'u' => WEEK_MONDAY_FIRST,
                    'V' | 'X' => WEEK_YEAR | WEEK_FIRST_WEEKDAY,
                    _ => WEEK_YEAR | WEEK_MONDAY_FIRST,
                };
                let (week, year) = calendar::week(time.year, time.month, time.day, behaviour);
                if matches!(spec, 'X' | 'x') {
                    out.push_str(&format!("{year:04}"));
                } else {
                    out.push_str(&format!("{week:02}"));
                }
            }
            'w' => {
                if !has_date {
                    return None;
                }
                out.push_str(&calendar::weekday(time.day_number(), true).to_string());
            }
            other => out.push(other),
        }
    }
    Some(out)
}

fn ordinal_suffix(day: u32) -> &'static str {
    if (10..=19).contains(&day) {
        return "th";
    }
    match day % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    }
}

fn twelve_hour(hour: u32) -> u32 {
    (hour + 11) % 12 + 1
}

pub(super) fn get_format(args: &[Arg]) -> Result<Value> {
    let kind = args[0].text().unwrap_or_default().to_ascii_uppercase();
    let Some(standard) = args[1].text() else {
        return Ok(Value::Null);
    };
    let formats = match kind.as_str() {
        "DATE" => ["%m.%d.%Y", "%Y-%m-%d", "%Y-%m-%d", "%d.%m.%Y", "%Y%m%d"],
        "TIME" => ["%h:%i:%s %p", "%H:%i:%s", "%H:%i:%s", "%H.%i.%s", "%H%i%s"],
        "DATETIME" | "TIMESTAMP" => [
            "%Y-%m-%d %H.%i.%s",
            "%Y-%m-%d %H:%i:%s",
            "%Y-%m-%d %H:%i:%s",
            "%Y-%m-%d %H.%i.%s",
            "%Y%m%d%H%i%s",
        ],
        _ => {
            return Err(LimboError::InvalidArgument(format!(
                "unknown GET_FORMAT type: {kind}"
            )))
        }
    };
    let index = ["USA", "JIS", "ISO", "EUR", "INTERNAL"]
        .iter()
        .position(|name| name.eq_ignore_ascii_case(&standard));
    Ok(index.map_or(Value::Null, |index| Value::build_text(formats[index])))
}
