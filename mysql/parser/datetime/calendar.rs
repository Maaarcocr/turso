pub const MAX_DAY_NUMBER: i64 = 3_652_424;

const DAYS_IN_MONTH: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

pub const WEEK_MONDAY_FIRST: u32 = 1;
pub const WEEK_YEAR: u32 = 2;
pub const WEEK_FIRST_WEEKDAY: u32 = 4;

pub fn week_mode(mode: u32) -> u32 {
    let mut behaviour = mode & 7;
    if behaviour & WEEK_MONDAY_FIRST == 0 {
        behaviour ^= WEEK_FIRST_WEEKDAY;
    }
    behaviour
}

pub fn week(year: u32, month: u32, day: u32, behaviour: u32) -> (u32, u32) {
    let day_number = day_number(year, month, day) as u64;
    let mut first_day_number = self::day_number(year, 1, 1) as u64;
    let monday_first = behaviour & WEEK_MONDAY_FIRST != 0;
    let mut week_year = behaviour & WEEK_YEAR != 0;
    let first_weekday = behaviour & WEEK_FIRST_WEEKDAY != 0;
    let mut weekday = weekday(first_day_number as i64, !monday_first);
    let mut year = year;
    let starts_late =
        |weekday: u32| (first_weekday && weekday != 0) || (!first_weekday && weekday >= 4);
    if month == 1 && day <= 7 - weekday {
        if !week_year && starts_late(weekday) {
            return (0, year);
        }
        week_year = true;
        year = year.wrapping_sub(1);
        let days = days_in_year(year);
        first_day_number = first_day_number.wrapping_sub(u64::from(days));
        weekday = (weekday + 53 * 7 - days) % 7;
    }
    let week_start = if starts_late(weekday) {
        first_day_number.wrapping_add(u64::from(7 - weekday))
    } else {
        first_day_number.wrapping_sub(u64::from(weekday))
    };
    let days = day_number.wrapping_sub(week_start) as u32;
    if week_year && days >= 52 * 7 {
        let weekday = (weekday + days_in_year(year)) % 7;
        if !starts_late(weekday) {
            return (1, year.wrapping_add(1));
        }
    }
    (days / 7 + 1, year)
}

pub fn seconds_since_epoch(
    year: u32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> i64 {
    let days = day_number(year, month, day) - day_number(1970, 1, 1);
    days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second)
}

pub fn date_from_day_number(day_number: i64) -> (u32, u32, u32) {
    if day_number <= 365 || day_number >= 3_652_500 {
        return (0, 0, 0);
    }
    let mut year = (day_number * 100 / 36525) as u32;
    let century_correction = (((year - 1) / 100 + 1) * 3) / 4;
    let mut day_of_year =
        (day_number - i64::from(year) * 365) as u32 - (year - 1) / 4 + century_correction;
    loop {
        let days = days_in_year(year);
        if day_of_year <= days {
            break;
        }
        day_of_year -= days;
        year += 1;
    }
    let mut leap_day = 0;
    if days_in_year(year) == 366 && day_of_year > 31 + 28 {
        day_of_year -= 1;
        if day_of_year == 31 + 28 {
            leap_day = 1;
        }
    }
    let mut month = 1;
    for days in DAYS_IN_MONTH {
        if day_of_year <= days {
            break;
        }
        day_of_year -= days;
        month += 1;
    }
    (year, month, day_of_year + leap_day)
}

pub fn days_in_month(year: u32, month: u32) -> u32 {
    if month == 2 && days_in_year(year) == 366 {
        29
    } else {
        DAYS_IN_MONTH[month as usize - 1]
    }
}

pub fn days_in_year(year: u32) -> u32 {
    if year & 3 == 0 && (year % 100 != 0 || (year % 400 == 0 && year != 0)) {
        366
    } else {
        365
    }
}

pub fn weekday(day_number: i64, sunday_first: bool) -> u32 {
    ((day_number + 5 + i64::from(sunday_first)) % 7) as u32
}

pub fn day_number(year: u32, month: u32, day: u32) -> i64 {
    if year == 0 && month == 0 {
        return 0;
    }
    let mut year = i64::from(year);
    let month = i64::from(month);
    let mut days = 365 * year + 31 * (month - 1) + i64::from(day);
    if month <= 2 {
        year -= 1;
    } else {
        days -= (month * 4 + 23) / 10;
    }
    let century_correction = ((year / 100 + 1) * 3) / 4;
    days + year / 4 - century_correction
}
