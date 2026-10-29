use super::calendar;
use super::temporal::{Kind, MyTime};

#[derive(Clone, Copy, Default)]
pub struct DateFlags {
    pub fuzzy: bool,
    pub no_zero_in_date: bool,
    pub no_zero_date: bool,
    pub datetime_only: bool,
}

impl DateFlags {
    pub const FUZZY: Self = Self {
        fuzzy: true,
        no_zero_in_date: false,
        no_zero_date: false,
        datetime_only: false,
    };
    pub const NO_ZERO_DATE: Self = Self {
        fuzzy: false,
        no_zero_in_date: false,
        no_zero_date: true,
        datetime_only: false,
    };
    pub const CAST: Self = Self {
        fuzzy: true,
        no_zero_in_date: true,
        no_zero_date: true,
        datetime_only: false,
    };
}

#[derive(Clone, Copy, Debug)]
pub struct Parsed {
    pub time: MyTime,
    pub fraction_digits: u8,
    pub rounding_nanos: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotParsed {
    NotDatetime,
    InvalidDatetime,
}

const MAX_TIME_NUMBER: i64 = 8_385_959;

pub fn parse_datetime(text: &[u8], flags: DateFlags) -> Result<Parsed, NotParsed> {
    let start = skip_spaces(text, 0);
    if start == text.len() || !text[start].is_ascii_digit() {
        return Err(NotParsed::NotDatetime);
    }
    let fields = DatetimeFields::scan(text, start)?;
    if fields.found_delimiter && !fields.found_space && flags.datetime_only {
        return Err(NotParsed::NotDatetime);
    }
    let fraction_digits = fields.lengths[6].min(6);
    let mut time = MyTime {
        kind: if fields.count <= 3 {
            Kind::Date
        } else {
            Kind::DateTime
        },
        negative: false,
        year: fields.values[0] as u32,
        month: fields.values[1] as u32,
        day: fields.values[2] as u32,
        hour: fields.values[3] as u32,
        minute: fields.values[4] as u32,
        second: fields.values[5] as u32,
        micro: (fields.values[6] * 10u64.pow((6 - fraction_digits) as u32)) as u32,
    };
    if fields.year_length == 2 && fields.not_zero_date {
        time.year += if time.year < 70 { 2000 } else { 1900 };
    }
    if fields.count < 3
        || !time.date_in_range()
        || !date_is_allowed(&time, fields.not_zero_date, flags)
    {
        return Err(NotParsed::InvalidDatetime);
    }
    let mut pos = fields.end;
    let mut rounding_nanos = 0;
    if fraction_digits == 6 && pos < text.len() && text[pos].is_ascii_digit() {
        rounding_nanos = 100 * u32::from(text[pos] - b'0');
        while pos < text.len() && text[pos].is_ascii_digit() {
            pos += 1;
        }
    }
    if let Some(displacement) = fields.displacement {
        let utc = time.day_number() * 86_400 + time.seconds_of_day() - displacement;
        time = MyTime::from_day_seconds(utc, time.micro).ok_or(NotParsed::InvalidDatetime)?;
    }
    Ok(Parsed {
        time,
        fraction_digits: fraction_digits as u8,
        rounding_nanos,
    })
}

struct DatetimeFields {
    values: [u64; 8],
    lengths: [usize; 8],
    count: usize,
    year_length: usize,
    not_zero_date: bool,
    found_delimiter: bool,
    found_space: bool,
    displacement: Option<i64>,
    end: usize,
}

impl DatetimeFields {
    fn scan(text: &[u8], start: usize) -> Result<Self, NotParsed> {
        let end = text.len();
        let digits = text[start..]
            .iter()
            .take_while(|b| b.is_ascii_digit() || **b == b'T')
            .count();
        let only_digits = start + digits == end || text[start + digits] == b'.';
        let packed_year_length = if digits == 4 || digits == 8 || digits >= 14 {
            4
        } else {
            2
        };
        let mut fields = Self {
            values: [0; 8],
            lengths: [0; 8],
            count: 0,
            year_length: packed_year_length,
            not_zero_date: false,
            found_delimiter: false,
            found_space: false,
            displacement: None,
            end: start,
        };
        let mut field_length = if only_digits { packed_year_length } else { 4 };
        let mut pos = start;
        let mut i = 0;
        while i < 7 && pos < end && text[pos].is_ascii_digit() {
            let field_start = pos;
            let mut value = u64::from(text[pos] - b'0');
            pos += 1;
            let scan_until_delimiter = !only_digits && i != 6;
            while pos < end
                && text[pos].is_ascii_digit()
                && (scan_until_delimiter || {
                    field_length -= 1;
                    field_length != 0
                })
            {
                value = value
                    .saturating_mul(10)
                    .saturating_add(u64::from(text[pos] - b'0'));
                pos += 1;
            }
            fields.lengths[i] = pos - field_start;
            if value > 999_999 {
                return Err(NotParsed::NotDatetime);
            }
            fields.values[i] = value;
            fields.not_zero_date |= value != 0;
            field_length = 2;
            fields.end = pos;
            if pos == end {
                i += 1;
                break;
            }
            if i == 2 && text[pos] == b'T' {
                pos += 1;
                i += 1;
                continue;
            }
            if i == 5 {
                if text[pos] == b'.' {
                    pos += 1;
                    fields.end = pos;
                    field_length = 6;
                } else if text[pos].is_ascii_digit() {
                    i += 1;
                    break;
                } else if text[pos] == b'+' || text[pos] == b'-' {
                    fields.displacement = Some(parse_displacement(&text[pos..])?);
                    pos = end;
                    fields.end = pos;
                }
                i += 1;
                continue;
            }
            if i == 6 && (text[pos] == b'+' || text[pos] == b'-') {
                fields.displacement = Some(parse_displacement(&text[pos..])?);
                pos = end;
            }
            while pos < end && (text[pos].is_ascii_punctuation() || is_space(text[pos])) {
                if is_space(text[pos]) {
                    if i != 2 && i != 6 {
                        return Err(NotParsed::NotDatetime);
                    }
                    fields.found_space = true;
                }
                pos += 1;
                fields.found_delimiter = true;
            }
            if i == 6 {
                i += 1;
            }
            fields.end = pos;
            i += 1;
        }
        fields.count = i;
        if !only_digits {
            fields.year_length = fields.lengths[0];
        }
        Ok(fields)
    }
}

fn parse_displacement(text: &[u8]) -> Result<i64, NotParsed> {
    let digit = |i: usize| {
        text.get(i)
            .filter(|b| b.is_ascii_digit())
            .map(|b| i64::from(b - b'0'))
            .ok_or(NotParsed::NotDatetime)
    };
    let sign = match text.first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return Err(NotParsed::NotDatetime),
    };
    let hours = digit(1)? * 10 + digit(2)?;
    if text.get(3) != Some(&b':') {
        return Err(NotParsed::NotDatetime);
    }
    let minutes = digit(4)? * 10 + digit(5)?;
    let seconds = hours * 3_600 + minutes * 60;
    let valid = minutes < 60
        && seconds <= 14 * 3_600
        && !(sign == -1 && seconds == 0)
        && text[6..].iter().all(|b| b.is_ascii_whitespace());
    if valid {
        Ok(sign * seconds)
    } else {
        Err(NotParsed::NotDatetime)
    }
}

pub fn parse_time(text: &[u8]) -> Option<Parsed> {
    let end = text.len();
    let mut pos = skip_spaces(text, 0);
    let negative = pos < end && text[pos] == b'-';
    if negative {
        pos += 1;
    }
    if pos == end {
        return None;
    }
    let start = pos;
    if end - pos >= 12 {
        let datetime_only = DateFlags {
            fuzzy: true,
            datetime_only: true,
            ..DateFlags::default()
        };
        match parse_datetime(&text[pos..], datetime_only) {
            Ok(parsed) => return Some(parsed),
            Err(NotParsed::InvalidDatetime) => return None,
            Err(NotParsed::NotDatetime) => {}
        }
    }
    let mut value: u64 = 0;
    while pos < end && text[pos].is_ascii_digit() {
        value = value * 10 + u64::from(text[pos] - b'0');
        if value > u64::from(u32::MAX) {
            return None;
        }
        pos += 1;
    }
    let end_of_days = pos;
    pos = skip_spaces(text, pos);
    let mut date = [0u64; 5];
    let mut fraction_digits = 0;
    let mut rounding_nanos = 0;
    let has_clock_fields = if end - pos > 1 && pos != end_of_days && text[pos].is_ascii_digit() {
        date[0] = value;
        Some(1)
    } else if end - pos > 1 && text[pos] == b':' && text[pos + 1].is_ascii_digit() {
        date[1] = value;
        pos += 1;
        Some(2)
    } else {
        date[1] = value / 10_000;
        date[2] = value / 100 % 100;
        date[3] = value % 100;
        None
    };
    if let Some(mut state) = has_clock_fields {
        loop {
            let mut part: u64 = 0;
            while pos < end && text[pos].is_ascii_digit() {
                part = part
                    .saturating_mul(10)
                    .saturating_add(u64::from(text[pos] - b'0'));
                pos += 1;
            }
            date[state] = part;
            state += 1;
            if state == 4 || end - pos < 2 || text[pos] != b':' || !text[pos + 1].is_ascii_digit() {
                break;
            }
            pos += 1;
        }
    }
    if end - pos >= 2 && text[pos] == b'.' && text[pos + 1].is_ascii_digit() {
        let mut remaining_digits: i32 = 5;
        pos += 1;
        let mut fraction = u64::from(text[pos] - b'0');
        pos += 1;
        while pos < end && text[pos].is_ascii_digit() {
            if remaining_digits > 0 {
                fraction = fraction * 10 + u64::from(text[pos] - b'0');
            }
            remaining_digits -= 1;
            pos += 1;
        }
        if remaining_digits >= 0 {
            fraction_digits = (6 - remaining_digits) as u8;
            fraction *= 10u64.pow(remaining_digits as u32);
        } else {
            fraction_digits = 6;
            rounding_nanos = 100 * u32::from(text[pos - 1] - b'0');
        }
        date[4] = fraction;
    } else if end - pos == 1 && text[pos] == b'.' {
        pos += 1;
    }
    if end - pos > 1
        && (text[pos] == b'e' || text[pos] == b'E')
        && (text[pos + 1].is_ascii_digit()
            || ((text[pos + 1] == b'-' || text[pos + 1] == b'+')
                && end - pos > 2
                && text[pos + 2].is_ascii_digit()))
    {
        return None;
    }
    if pos == start || date.iter().any(|part| *part > u64::from(u32::MAX)) {
        return None;
    }
    let mut time = MyTime::time(
        negative,
        (date[1] + date[0] * 24) as u32,
        date[2] as u32,
        date[3] as u32,
        date[4] as u32,
    );
    if time.minute > 59 || time.second > 59 || time.micro > 999_999 {
        return None;
    }
    time.clamp_time();
    Some(Parsed {
        time,
        fraction_digits,
        rounding_nanos,
    })
}

pub fn time_from_number(number: i64) -> Option<MyTime> {
    if number > MAX_TIME_NUMBER {
        if number >= 10_000_000_000 {
            return datetime_from_number(number, DateFlags::default());
        }
        return None;
    }
    if number < -MAX_TIME_NUMBER {
        return None;
    }
    let negative = number < 0;
    let number = number.unsigned_abs();
    if number % 100 >= 60 || number / 100 % 100 >= 60 {
        return None;
    }
    Some(MyTime::time(
        negative,
        (number / 10_000) as u32,
        (number / 100 % 100) as u32,
        (number % 100) as u32,
        0,
    ))
}

pub fn datetime_from_number(number: i64, flags: DateFlags) -> Option<MyTime> {
    let mut kind = Kind::Date;
    let packed = if number == 0 || number >= 10_000_101_000_000 {
        kind = Kind::DateTime;
        if number > 99_999_999_999_999 {
            return None;
        }
        number
    } else if number < 101 {
        return None;
    } else if number <= 691_231 {
        (number + 20_000_000) * 1_000_000
    } else if number < 700_101 {
        return None;
    } else if number <= 991_231 {
        (number + 19_000_000) * 1_000_000
    } else if number < 10_000_101 && !flags.fuzzy {
        return None;
    } else if number <= 99_991_231 {
        number * 1_000_000
    } else if number < 101_000_000 {
        return None;
    } else {
        kind = Kind::DateTime;
        if number <= 691_231_235_959 {
            number + 20_000_000_000_000
        } else if number < 700_101_000_000 {
            return None;
        } else if number <= 991_231_235_959 {
            number + 19_000_000_000_000
        } else {
            number
        }
    };
    let date_part = packed / 1_000_000;
    let time_part = packed % 1_000_000;
    let time = MyTime {
        kind,
        negative: false,
        year: (date_part / 10_000) as u32,
        month: (date_part / 100 % 100) as u32,
        day: (date_part % 100) as u32,
        hour: (time_part / 10_000) as u32,
        minute: (time_part / 100 % 100) as u32,
        second: (time_part % 100) as u32,
        micro: 0,
    };
    (time.date_in_range() && date_is_allowed(&time, packed != 0, flags)).then_some(time)
}

pub fn date_is_allowed(time: &MyTime, not_zero_date: bool, flags: DateFlags) -> bool {
    if !not_zero_date {
        return !flags.no_zero_date;
    }
    if (flags.no_zero_in_date || !flags.fuzzy) && (time.month == 0 || time.day == 0) {
        return false;
    }
    time.month == 0 || time.day <= calendar::days_in_month(time.year, time.month)
}

fn skip_spaces(text: &[u8], mut pos: usize) -> usize {
    while pos < text.len() && is_space(text[pos]) {
        pos += 1;
    }
    pos
}

pub fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}
