use super::parse;

const MAX_DIGITS: i64 = 200;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Decimal {
    pub negative: bool,
    pub integer: String,
    pub fraction: String,
}

impl Decimal {
    pub fn from_int(number: i64) -> Self {
        Self {
            negative: number < 0,
            ..Self::from_unsigned(number.unsigned_abs())
        }
    }

    pub fn from_unsigned(number: u64) -> Self {
        Self::from_digits(false, &number.to_string(), "", 0)
    }

    pub fn from_double(number: f64) -> Self {
        Self::parse(format!("{number:e}").as_bytes())
    }

    pub fn parse(text: &[u8]) -> Self {
        let mut pos = 0;
        while pos < text.len() && parse::is_space(text[pos]) {
            pos += 1;
        }
        let negative = pos < text.len() && text[pos] == b'-';
        if pos < text.len() && (text[pos] == b'-' || text[pos] == b'+') {
            pos += 1;
        }
        let integer = digits_at(text, pos);
        pos += integer.len();
        let mut fraction = "";
        if pos < text.len() && text[pos] == b'.' {
            fraction = digits_at(text, pos + 1);
            pos += 1 + fraction.len();
        }
        if integer.is_empty() && fraction.is_empty() {
            return Self::default();
        }
        let exponent = exponent_at(text, pos).unwrap_or(0);
        Self::from_digits(negative, integer, fraction, exponent)
    }

    fn from_digits(negative: bool, integer: &str, fraction: &str, exponent: i64) -> Self {
        let exponent = exponent.clamp(-MAX_DIGITS, MAX_DIGITS);
        let all_digits = format!("{integer}{fraction}");
        let point = integer.len() as i64 + exponent;
        let (integer, fraction) = if point <= 0 {
            (
                String::new(),
                format!("{}{all_digits}", "0".repeat((-point) as usize)),
            )
        } else if point as usize >= all_digits.len() {
            (
                format!(
                    "{all_digits}{}",
                    "0".repeat(point as usize - all_digits.len())
                ),
                String::new(),
            )
        } else {
            let (integer, fraction) = all_digits.split_at(point as usize);
            (integer.to_string(), fraction.to_string())
        };
        let integer = integer.trim_start_matches('0').to_string();
        let is_zero = integer.is_empty() && fraction.bytes().all(|b| b == b'0');
        Self {
            negative: negative && !is_zero,
            integer,
            fraction,
        }
    }

    pub fn whole_and_nanos(&self) -> Option<(i64, i64)> {
        if self.integer.len() > 18 {
            return None;
        }
        let whole: i64 = if self.integer.is_empty() {
            0
        } else {
            self.integer.parse().expect("at most 18 digits fit in i64")
        };
        let nanos_digits = format!("{:0<9}", &self.fraction[..self.fraction.len().min(9)]);
        let nanos: i64 = nanos_digits.parse().expect("9 digits fit in i64");
        Some(if self.negative {
            (-whole, -nanos)
        } else {
            (whole, nanos)
        })
    }
}

fn digits_at(text: &[u8], start: usize) -> &str {
    let start = start.min(text.len());
    let length = text[start..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    std::str::from_utf8(&text[start..start + length]).expect("ASCII digits are UTF-8")
}

fn exponent_at(text: &[u8], pos: usize) -> Option<i64> {
    if pos >= text.len() || (text[pos] != b'e' && text[pos] != b'E') {
        return None;
    }
    let mut start = pos + 1;
    let negative = start < text.len() && text[start] == b'-';
    if start < text.len() && (text[start] == b'-' || text[start] == b'+') {
        start += 1;
    }
    let digits = digits_at(text, start);
    if digits.is_empty() {
        return None;
    }
    let magnitude: i64 = digits.parse().unwrap_or(i64::MAX);
    Some(if negative { -magnitude } else { magnitude })
}
