pub(super) enum Reading<'a> {
    Number(TextNumber<'a>),
    NumberFollowedByText,
    NoNumber,
}

pub(super) struct TextNumber<'a> {
    negative: bool,
    whole: &'a [u8],
    fraction: &'a [u8],
    exponent: i64,
}

pub(super) fn read(bytes: &[u8]) -> Reading<'_> {
    let Some((number, rest)) = split_number(bytes) else {
        return Reading::NoNumber;
    };
    if skip_spaces(rest).is_empty() {
        Reading::Number(number)
    } else {
        Reading::NumberFollowedByText
    }
}

/// The number at the start of the text, ignoring what follows it, as MySQL
/// reads text in a numeric context.
pub(super) fn leading_number(bytes: &[u8]) -> Option<TextNumber<'_>> {
    split_number(bytes).map(|(number, _)| number)
}

fn split_number(bytes: &[u8]) -> Option<(TextNumber<'_>, &[u8])> {
    let rest = skip_spaces(bytes);
    let (negative, rest) = match rest.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, rest),
    };
    let (whole, rest) = split_digits(rest);
    let (fraction, rest) = match rest.split_first() {
        Some((b'.', after_point)) => split_digits(after_point),
        _ => (&rest[..0], rest),
    };
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    let (exponent, rest) = read_exponent(rest);
    let number = TextNumber {
        negative,
        whole,
        fraction,
        exponent,
    };
    Some((number, rest))
}

impl TextNumber<'_> {
    pub(super) fn is_negative(&self) -> bool {
        self.negative
    }

    pub(super) fn parts(&self) -> (bool, &[u8], &[u8], i64) {
        (self.negative, self.whole, self.fraction, self.exponent)
    }

    pub(super) fn rounded_magnitude(&self) -> Option<u128> {
        let digit = |position: i64| -> u8 {
            usize::try_from(position)
                .ok()
                .and_then(|position| {
                    self.whole
                        .get(position)
                        .or_else(|| self.fraction.get(position - self.whole.len()))
                })
                .map_or(0, |digit| digit - b'0')
        };
        let digit_count = (self.whole.len() + self.fraction.len()) as i64;
        let Some(first_significant) = (0..digit_count).find(|position| digit(*position) != 0)
        else {
            return Some(0);
        };
        let point = (self.whole.len() as i64).saturating_add(self.exponent);
        if point.saturating_sub(first_significant) > 39 {
            return None;
        }
        let mut magnitude: u128 = 0;
        for position in first_significant..point.max(first_significant) {
            magnitude = magnitude
                .checked_mul(10)?
                .checked_add(u128::from(digit(position)))?;
        }
        if digit(point) >= 5 && point >= 0 {
            magnitude = magnitude.checked_add(1)?;
        }
        Some(magnitude)
    }

    pub(super) fn to_f64(&self) -> f64 {
        let digits = |part: &[u8]| {
            if part.is_empty() {
                "0".to_string()
            } else {
                String::from_utf8_lossy(part).into_owned()
            }
        };
        let text = format!(
            "{}{}.{}e{}",
            if self.negative { "-" } else { "" },
            digits(self.whole),
            digits(self.fraction),
            self.exponent
        );
        text.parse()
            .expect("a number read from text is valid float syntax")
    }
}

fn skip_spaces(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C))
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn split_digits(bytes: &[u8]) -> (&[u8], &[u8]) {
    let end = bytes
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(bytes.len());
    bytes.split_at(end)
}

fn read_exponent(bytes: &[u8]) -> (i64, &[u8]) {
    let Some((b'e' | b'E', rest)) = bytes.split_first() else {
        return (0, bytes);
    };
    let (negative, rest) = match rest.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, rest),
    };
    let (digits, rest) = split_digits(rest);
    if digits.is_empty() {
        return (0, bytes);
    }
    let magnitude = digits.iter().fold(0_i64, |value, digit| {
        value
            .saturating_mul(10)
            .saturating_add(i64::from(digit - b'0'))
    });
    (if negative { -magnitude } else { magnitude }, rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rounded(text: &str) -> Option<u128> {
        match read(text.as_bytes()) {
            Reading::Number(number) => number.rounded_magnitude(),
            _ => panic!("{text} is not a whole number"),
        }
    }

    #[test]
    fn rounds_half_away_from_zero_on_the_digits_as_written() {
        for (text, expected) in [
            ("12", Some(12)),
            (" 7 ", Some(7)),
            ("2.5", Some(3)),
            ("2.49", Some(2)),
            ("-3.5", Some(4)),
            ("2.5e0", Some(3)),
            ("1.5e1", Some(15)),
            ("15e-1", Some(2)),
            (".5", Some(1)),
            ("5.", Some(5)),
            ("0.05", Some(0)),
            ("00.5", Some(1)),
            ("0e400", Some(0)),
            ("1e-400", Some(0)),
            ("18446744073709551615", Some(u64::MAX.into())),
            ("1e400", None),
        ] {
            assert_eq!(rounded(text), expected, "{text}");
        }
    }

    #[test]
    fn tells_numbers_from_numbers_followed_by_text() {
        assert!(matches!(read(b"12abc"), Reading::NumberFollowedByText));
        assert!(matches!(read(b"0x10"), Reading::NumberFollowedByText));
        assert!(matches!(read(b"1e"), Reading::NumberFollowedByText));
        for text in ["", "  ", "abc", "- 5", "-", "."] {
            assert!(matches!(read(text.as_bytes()), Reading::NoNumber), "{text}");
        }
    }
}
