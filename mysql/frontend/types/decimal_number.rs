use super::number_text::TextNumber;
use num_bigint::{BigInt, Sign};
use num_traits::{Signed, ToPrimitive, Zero};
use std::cmp::Ordering;

const DIGITS_PER_WORD: u32 = 9;
const WORDS_PER_VALUE: u32 = 9;
const DIVISION_SCALE_INCREMENT: u32 = 4;
const LARGEST_WHOLE_DIGITS: u32 = DIGITS_PER_WORD * WORDS_PER_VALUE;
const LARGEST_READ_SCALE: u32 = 90;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rounding {
    HalfAwayFromZero,
    TowardZero,
    Floor,
    Ceiling,
}

/// An exact decimal number: `digits / 10^scale`.
#[derive(Clone, Debug)]
pub(crate) struct Decimal {
    digits: BigInt,
    scale: u32,
}

impl Decimal {
    pub(crate) fn zero() -> Decimal {
        Decimal {
            digits: BigInt::zero(),
            scale: 0,
        }
    }

    pub(crate) fn from_i64(value: i64) -> Decimal {
        Decimal {
            digits: BigInt::from(value),
            scale: 0,
        }
    }

    /// Reads the text MySQL prints for a decimal: an optional sign, digits
    /// and an optional point with more digits.
    pub(crate) fn parse(text: &str) -> Option<Decimal> {
        let bytes = text.as_bytes();
        let (negative, unsigned) = match bytes.split_first() {
            Some((b'-', rest)) => (true, rest),
            Some((b'+', rest)) => (false, rest),
            _ => (false, bytes),
        };
        let (whole, fraction) = match unsigned.iter().position(|byte| *byte == b'.') {
            Some(point) => (&unsigned[..point], &unsigned[point + 1..]),
            None => (unsigned, &unsigned[unsigned.len()..]),
        };
        let all_digits = |part: &[u8]| part.iter().all(u8::is_ascii_digit);
        if (whole.is_empty() && fraction.is_empty()) || !all_digits(whole) || !all_digits(fraction)
        {
            return None;
        }
        Some(Decimal::from_digits(negative, whole, fraction, 0))
    }

    /// Converts a number read from text, applying its exponent. `None` when
    /// the number has more whole digits than MySQL keeps.
    pub(super) fn from_text_number(number: &TextNumber) -> Option<Decimal> {
        let (negative, whole, fraction, exponent) = number.parts();
        let first_significant = whole
            .iter()
            .chain(fraction)
            .position(|digit| *digit != b'0');
        let Some(first_significant) = first_significant else {
            return Some(Decimal::zero());
        };
        let whole_digits = (whole.len() as i64 - first_significant as i64).saturating_add(exponent);
        if whole_digits > i64::from(LARGEST_WHOLE_DIGITS) {
            return None;
        }
        let smallest_place = -(i64::from(LARGEST_READ_SCALE) + 1);
        if whole_digits < smallest_place {
            return Some(Decimal::zero());
        }
        Some(Decimal::from_digits(negative, whole, fraction, exponent))
    }

    /// The decimal MySQL converts a DOUBLE to: the shortest digits that
    /// read back as the same double.
    pub(crate) fn from_double(value: f64) -> Option<Decimal> {
        if !value.is_finite() {
            return None;
        }
        Decimal::parse(&format!("{value}"))
    }

    fn from_digits(negative: bool, whole: &[u8], fraction: &[u8], exponent: i64) -> Decimal {
        let mut text = String::with_capacity(whole.len() + fraction.len() + 1);
        if negative {
            text.push('-');
        }
        text.push_str(std::str::from_utf8(whole).expect("decimal digits are ASCII"));
        text.push_str(std::str::from_utf8(fraction).expect("decimal digits are ASCII"));
        if whole.is_empty() && fraction.is_empty() {
            text.push('0');
        }
        let digits = text
            .parse::<BigInt>()
            .expect("decimal digits form an integer");
        let scale = fraction.len() as i64 - exponent;
        let decimal = if scale >= 0 {
            Decimal {
                digits,
                scale: scale as u32,
            }
        } else {
            Decimal {
                digits: digits * power_of_ten(scale.unsigned_abs() as u32),
                scale: 0,
            }
        };
        if decimal.scale > LARGEST_READ_SCALE {
            decimal.rounded(LARGEST_READ_SCALE as i64, Rounding::HalfAwayFromZero)
        } else {
            decimal
        }
    }

    /// The largest value of `DECIMAL(precision, scale)`.
    pub(crate) fn largest(precision: u32, scale: u32) -> Decimal {
        Decimal {
            digits: power_of_ten(precision) - 1,
            scale,
        }
    }

    pub(crate) fn scale(&self) -> u32 {
        self.scale
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.digits.is_zero()
    }

    pub(crate) fn is_negative(&self) -> bool {
        self.digits.is_negative()
    }

    /// The number of digits before the point, without leading zeros.
    pub(crate) fn whole_digits(&self) -> u32 {
        let digit_count = self.digits.magnitude().to_string().len() as u32;
        if self.digits.is_zero() {
            0
        } else {
            digit_count.saturating_sub(self.scale)
        }
    }

    /// The text MySQL prints: exactly `scale` digits after the point and no
    /// negative zero.
    pub(crate) fn to_text(&self) -> String {
        let magnitude = self.digits.magnitude().to_string();
        let sign = if self.digits.sign() == Sign::Minus {
            "-"
        } else {
            ""
        };
        let scale = self.scale as usize;
        if scale == 0 {
            return format!("{sign}{magnitude}");
        }
        let padded = format!("{magnitude:0>width$}", width = scale + 1);
        let (whole, fraction) = padded.split_at(padded.len() - scale);
        format!("{sign}{whole}.{fraction}")
    }

    pub(crate) fn to_f64(&self) -> f64 {
        self.to_text()
            .parse::<f64>()
            .expect("decimal text is valid float syntax")
            .clamp(f64::MIN, f64::MAX)
    }

    pub(crate) fn to_i64(&self) -> Option<i64> {
        (&self.digits / power_of_ten(self.scale)).to_i64()
    }

    pub(crate) fn negated(&self) -> Decimal {
        Decimal {
            digits: -&self.digits,
            scale: self.scale,
        }
    }

    pub(crate) fn abs(&self) -> Decimal {
        Decimal {
            digits: self.digits.abs(),
            scale: self.scale,
        }
    }

    pub(crate) fn signum(&self) -> i64 {
        match self.digits.sign() {
            Sign::Minus => -1,
            Sign::NoSign => 0,
            Sign::Plus => 1,
        }
    }

    pub(crate) fn add(&self, other: &Decimal) -> Decimal {
        let scale = self.scale.max(other.scale);
        Decimal {
            digits: self.digits_at(scale) + other.digits_at(scale),
            scale,
        }
    }

    pub(crate) fn subtract(&self, other: &Decimal) -> Decimal {
        self.add(&other.negated())
    }

    pub(crate) fn multiply(&self, other: &Decimal) -> Decimal {
        Decimal {
            digits: &self.digits * &other.digits,
            scale: self.scale + other.scale,
        }
    }

    /// Divides as MySQL's `decimal_div` does: the quotient keeps whole
    /// groups of nine digits after the point, enough for both operands'
    /// digits plus four, and further digits are cut off. `None` when the
    /// divisor is zero.
    pub(crate) fn divide(&self, divisor: &Decimal) -> Option<Decimal> {
        if divisor.is_zero() {
            return None;
        }
        if self.is_zero() {
            return Some(Decimal::zero());
        }
        let dividend_words = words(self.scale);
        let divisor_words = words(divisor.scale);
        let padding = (dividend_words * DIGITS_PER_WORD - self.scale)
            + (divisor_words * DIGITS_PER_WORD - divisor.scale);
        let increment = DIVISION_SCALE_INCREMENT.saturating_sub(padding);
        let mut scale_words = words((dividend_words + divisor_words) * DIGITS_PER_WORD + increment);
        let numerator = self.digits.abs() * power_of_ten(divisor.scale);
        let denominator = divisor.digits.abs() * power_of_ten(self.scale);
        let whole = &numerator / &denominator;
        let whole_words = if whole.is_zero() {
            0
        } else {
            words(whole.to_string().len() as u32)
        };
        if whole_words + scale_words > WORDS_PER_VALUE {
            scale_words = WORDS_PER_VALUE.saturating_sub(whole_words);
        }
        let scale = scale_words * DIGITS_PER_WORD;
        let magnitude = numerator * power_of_ten(scale) / denominator;
        let negative = self.is_negative() != divisor.is_negative();
        Some(Decimal {
            digits: if negative { -magnitude } else { magnitude },
            scale,
        })
    }

    /// The remainder of a division toward zero, with the sign of `self` and
    /// the larger scale of the two. `None` when the divisor is zero.
    pub(crate) fn remainder(&self, divisor: &Decimal) -> Option<Decimal> {
        if divisor.is_zero() {
            return None;
        }
        let scale = self.scale.max(divisor.scale);
        Some(Decimal {
            digits: self.digits_at(scale) % divisor.digits_at(scale),
            scale,
        })
    }

    /// Rounds to a multiple of `10^-places`; a negative `places` rounds to
    /// tens, hundreds and so on. The result has `max(places, 0)` digits
    /// after the point.
    pub(crate) fn rounded(&self, places: i64, rounding: Rounding) -> Decimal {
        let target_scale = places.max(0) as u32;
        if places >= i64::from(self.scale) {
            return Decimal {
                digits: self.digits_at(target_scale),
                scale: target_scale,
            };
        }
        let dropped = i64::from(self.scale) - places;
        let digit_count = self.digits.magnitude().to_string().len() as i64;
        if dropped > digit_count + 1 {
            let away_from_zero = match rounding {
                Rounding::Floor => self.is_negative(),
                Rounding::Ceiling => !self.is_negative() && !self.is_zero(),
                Rounding::HalfAwayFromZero | Rounding::TowardZero => false,
            };
            let mut digits = BigInt::from(if away_from_zero { self.signum() } else { 0 });
            if places < 0 {
                digits *= power_of_ten(places.unsigned_abs() as u32);
            }
            return Decimal {
                digits,
                scale: target_scale,
            };
        }
        let unit = power_of_ten(dropped as u32);
        let magnitude = self.digits.magnitude();
        let mut kept = BigInt::from_biguint(Sign::Plus, magnitude / unit.magnitude());
        let remainder = magnitude % unit.magnitude();
        let away_from_zero = match rounding {
            Rounding::HalfAwayFromZero => remainder * 2u32 >= *unit.magnitude(),
            Rounding::TowardZero => false,
            Rounding::Floor => self.is_negative() && !remainder.is_zero(),
            Rounding::Ceiling => !self.is_negative() && !remainder.is_zero(),
        };
        if away_from_zero {
            kept += 1;
        }
        if places < 0 {
            kept *= power_of_ten(places.unsigned_abs() as u32);
        }
        Decimal {
            digits: if self.is_negative() { -kept } else { kept },
            scale: target_scale,
        }
    }

    /// `ROUND(x, places)` and `TRUNCATE(x, places)` of a DECIMAL: rounds as
    /// [`Decimal::rounded`] but keeps at most the scale of `x`, so
    /// `ROUND(1.5, 5)` is `1.5`.
    pub(crate) fn rounded_within_scale(&self, places: i64, rounding: Rounding) -> Decimal {
        self.rounded(places.min(i64::from(self.scale)), rounding)
    }

    pub(crate) fn compare(&self, other: &Decimal) -> Ordering {
        let scale = self.scale.max(other.scale);
        self.digits_at(scale).cmp(&other.digits_at(scale))
    }

    fn digits_at(&self, scale: u32) -> BigInt {
        if scale == self.scale {
            self.digits.clone()
        } else {
            &self.digits * power_of_ten(scale - self.scale)
        }
    }
}

fn words(digits: u32) -> u32 {
    digits.div_ceil(DIGITS_PER_WORD)
}

fn power_of_ten(exponent: u32) -> BigInt {
    num_traits::pow(BigInt::from(10), exponent as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decimal(text: &str) -> Decimal {
        Decimal::parse(text).unwrap_or_else(|| panic!("{text} is a decimal"))
    }

    fn quotient(dividend: &str, divisor: &str) -> String {
        decimal(dividend)
            .divide(&decimal(divisor))
            .expect("the divisor is not zero")
            .to_text()
    }

    /// The digits MySQL 8.4.10 keeps: `SELECT CAST(a / b AS DECIMAL(65, 30))`
    /// shows them up to 30 places, and `a / b * 10^n` shows the rest.
    #[test]
    fn division_keeps_whole_groups_of_nine_digits_and_cuts_off_the_rest() {
        assert_eq!(quotient("64", "77"), "0.831168831");
        assert_eq!(quotient("2", "3"), "0.666666666");
        assert_eq!(quotient("-2", "3"), "-0.666666666");
        assert_eq!(quotient("1.5", "3"), "0.500000000");
        assert_eq!(quotient("1.5", "0.3"), "5.000000000000000000");
        assert_eq!(quotient("1.000000001", "3"), "0.333333333666666666");
        assert_eq!(quotient("0.333333333", "3"), "0.111111111000000000");
        assert_eq!(quotient("0", "7"), "0");
        assert!(decimal("1").divide(&decimal("0.00")).is_none());
    }

    #[test]
    fn rounds_half_away_from_zero_also_to_tens() {
        let rounded = |text: &str, places: i64| {
            decimal(text)
                .rounded(places, Rounding::HalfAwayFromZero)
                .to_text()
        };
        assert_eq!(rounded("1.005", 2), "1.01");
        assert_eq!(rounded("-1.005", 2), "-1.01");
        assert_eq!(rounded("-0.004", 2), "0.00");
        assert_eq!(rounded("1.5", 4), "1.5000");
        assert_eq!(rounded("1250", -2), "1300");
        assert_eq!(rounded("-1249.99", -2), "-1200");
        assert_eq!(rounded("0.000000001", 2), "0.00");
        assert_eq!(
            decimal("-1.999").rounded(1, Rounding::TowardZero).to_text(),
            "-1.9"
        );
    }

    #[test]
    fn keeps_scales_through_arithmetic() {
        assert_eq!(decimal("1.5").add(&decimal("2.25")).to_text(), "3.75");
        assert_eq!(decimal("1.50").subtract(&decimal("2")).to_text(), "-0.50");
        assert_eq!(
            decimal("1.10").multiply(&decimal("1.10")).to_text(),
            "1.2100"
        );
        assert_eq!(
            decimal("-5.5")
                .remainder(&decimal("2"))
                .expect("the divisor is not zero")
                .to_text(),
            "-1.5"
        );
        assert_eq!(decimal("-0.0").to_text(), "0.0");
        assert_eq!(decimal(".5").to_text(), "0.5");
        assert_eq!(decimal("5.").to_text(), "5");
    }
}
