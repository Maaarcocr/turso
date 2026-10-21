use super::calendar;

pub(super) const MAX_TIME_HOUR: u32 = 838;
pub(super) const MAX_FSP: u8 = 6;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Kind {
    #[default]
    Date,
    DateTime,
    Time,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct MyTime {
    pub kind: Kind,
    pub negative: bool,
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub micro: u32,
}

impl MyTime {
    pub fn date(year: u32, month: u32, day: u32) -> Self {
        Self {
            kind: Kind::Date,
            year,
            month,
            day,
            ..Self::default()
        }
    }

    pub fn time(negative: bool, hour: u32, minute: u32, second: u32, micro: u32) -> Self {
        Self {
            kind: Kind::Time,
            negative,
            hour,
            minute,
            second,
            micro,
            ..Self::default()
        }
    }

    pub fn max_time(negative: bool) -> Self {
        Self::time(negative, MAX_TIME_HOUR, 59, 59, 0)
    }

    pub fn from_day_seconds(seconds: i64, micro: u32) -> Option<Self> {
        let day_number = seconds.div_euclid(86_400);
        let second_of_day = seconds.rem_euclid(86_400) as u32;
        if !(0..=calendar::MAX_DAY_NUMBER).contains(&day_number) {
            return None;
        }
        let (year, month, day) = calendar::date_from_day_number(day_number);
        Some(Self {
            kind: Kind::DateTime,
            negative: false,
            year,
            month,
            day,
            hour: second_of_day / 3_600,
            minute: second_of_day / 60 % 60,
            second: second_of_day % 60,
            micro,
        })
    }

    pub fn with_kind(self, kind: Kind) -> Self {
        Self { kind, ..self }
    }

    pub fn format(&self, fsp: u8) -> String {
        match self.kind {
            Kind::Date => self.format_date(),
            Kind::DateTime => format!("{} {}", self.format_date(), self.format_clock(fsp)),
            Kind::Time => format!(
                "{}{}",
                if self.negative { "-" } else { "" },
                self.format_clock(fsp)
            ),
        }
    }

    pub fn format_date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    fn format_clock(&self, fsp: u8) -> String {
        let clock = format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second);
        if fsp == 0 {
            return clock;
        }
        let digits = format!("{:06}", self.micro);
        format!("{clock}.{}", &digits[..usize::from(fsp)])
    }

    pub fn is_zero_date(&self) -> bool {
        self.year == 0 && self.month == 0 && self.day == 0
    }

    pub fn day_number(&self) -> i64 {
        calendar::day_number(self.year, self.month, self.day)
    }

    pub fn seconds_of_day(&self) -> i64 {
        i64::from(self.hour) * 3_600 + i64::from(self.minute) * 60 + i64::from(self.second)
    }

    pub fn time_in_micros(&self) -> i128 {
        let micros = i128::from(self.seconds_of_day()) * 1_000_000 + i128::from(self.micro);
        if self.negative {
            -micros
        } else {
            micros
        }
    }

    pub fn datetime_in_micros(&self) -> i128 {
        let seconds = i128::from(self.day_number()) * 86_400 + i128::from(self.seconds_of_day());
        let micros = seconds * 1_000_000 + i128::from(self.micro);
        if self.negative {
            -micros
        } else {
            micros
        }
    }

    pub fn date_in_range(&self) -> bool {
        self.year <= 9999
            && self.month <= 12
            && self.day <= 31
            && self.minute <= 59
            && self.second <= 59
            && self.micro <= 999_999
            && self.hour
                <= if self.kind == Kind::Time {
                    MAX_TIME_HOUR
                } else {
                    23
                }
    }

    pub fn round_time(&mut self, fsp: u8) {
        let scale = 10u32.pow(u32::from(MAX_FSP - fsp));
        let remainder = self.micro % scale;
        self.micro -= remainder;
        if fsp < MAX_FSP && remainder >= scale / 2 {
            self.micro += scale;
            if self.micro >= 1_000_000 {
                self.micro -= 1_000_000;
                self.add_second_to_time();
            }
        }
        self.clamp_time();
    }

    pub fn add_nanos_to_time(&mut self, nanos: u32) {
        if nanos < 500 {
            return;
        }
        self.micro += (nanos + 500) / 1000;
        if self.micro >= 1_000_000 {
            self.micro -= 1_000_000;
            self.add_second_to_time();
        }
        self.clamp_time();
    }

    pub fn clamp_time(&mut self) -> bool {
        if self.hour > MAX_TIME_HOUR
            || (self.hour == MAX_TIME_HOUR
                && self.minute == 59
                && self.second == 59
                && self.micro > 0)
        {
            *self = Self::max_time(self.negative);
            return true;
        }
        false
    }

    fn add_second_to_time(&mut self) {
        self.second += 1;
        if self.second == 60 {
            self.second = 0;
            self.minute += 1;
            if self.minute == 60 {
                self.minute = 0;
                self.hour += 1;
            }
        }
    }

    pub fn round_datetime(&mut self, fsp: u8) -> bool {
        let scale = 10u32.pow(u32::from(MAX_FSP - fsp));
        let remainder = self.micro % scale;
        self.micro -= remainder;
        if fsp < MAX_FSP && remainder >= scale / 2 {
            self.micro += scale;
            if self.micro >= 1_000_000 {
                self.micro -= 1_000_000;
                return self.add_second_to_datetime();
            }
        }
        true
    }

    pub fn add_nanos_to_datetime(&mut self, nanos: u32) -> bool {
        if nanos < 500 {
            return true;
        }
        self.micro += (nanos + 500) / 1000;
        if self.micro < 1_000_000 {
            return true;
        }
        self.micro -= 1_000_000;
        self.add_second_to_datetime()
    }

    fn add_second_to_datetime(&mut self) -> bool {
        if self.month == 0 || self.day == 0 {
            return false;
        }
        let seconds = self.day_number() * 86_400 + self.seconds_of_day() + 1;
        match Self::from_day_seconds(seconds, self.micro) {
            Some(next) => {
                *self = Self {
                    kind: self.kind,
                    ..next
                };
                true
            }
            None => false,
        }
    }
}
