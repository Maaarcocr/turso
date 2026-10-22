use bigdecimal::BigDecimal;
use num_bigint::{BigInt, Sign};

use crate::alloc::TursoVecExt;
use crate::{LimboError, ValueBlob};

const NUMERIC_BLOB_VERSION: u8 = 0x01;
const FLAG_NEGATIVE: u8 = 0x01;
const MAX_SCALE_MAGNITUDE: i64 = 1_000_000;
const MAX_INTEGER_DIGITS: i64 = 131_072;
const MAX_FRACTION_DIGITS: i64 = 16_383;
const DIVISION_MIN_SIGNIFICANT_DIGITS: i64 = 16;
const DIVISION_MAX_SCALE: i64 = 1000;
const DECIMAL_DIGITS_PER_BASE_10000_DIGIT: i64 = 4;
const KEY_NEGATIVE: u8 = 0x10;
const KEY_ZERO: u8 = 0x20;
const KEY_POSITIVE: u8 = 0x30;
const KEY_NEGATIVE_END: u8 = 0xFF;

/// Serialize a BigDecimal to our portable blob format.
///
/// Format (all multi-byte values little-endian):
/// ```text
/// [version: 1 byte (0x01)]
/// [flags:   1 byte (bit 0 = sign, 0=positive/zero, 1=negative)]
/// [scale:   8 bytes, i64 LE]
/// [num_limbs: 4 bytes, u32 LE]
/// [limb0: 4 bytes u32 LE] [limb1: 4 bytes u32 LE] ... [limbN: 4 bytes u32 LE]
/// ```
pub fn bigdecimal_to_blob(val: &BigDecimal) -> ValueBlob {
    let (bigint, scale) = val.as_bigint_and_exponent();
    let (sign, magnitude) = bigint.into_parts();

    // magnitude.to_u32_digits() returns limbs in little-endian order
    let limbs = magnitude.to_u32_digits();
    debug_assert!(
        limbs.len() <= u32::MAX as usize,
        "limb count {} exceeds u32::MAX",
        limbs.len()
    );
    let num_limbs = limbs.len() as u32;

    let mut buf = <ValueBlob as TursoVecExt<u8>>::with_capacity(14 + limbs.len() * 4);

    // version
    buf.push(NUMERIC_BLOB_VERSION);

    // flags
    let flags = if sign == Sign::Minus {
        FLAG_NEGATIVE
    } else {
        0
    };
    buf.push(flags);

    // scale (i64 LE)
    buf.extend_from_slice(&scale.to_le_bytes());

    // num_limbs (u32 LE)
    buf.extend_from_slice(&num_limbs.to_le_bytes());

    // limbs (u32 LE each)
    for limb in &limbs {
        buf.extend_from_slice(&limb.to_le_bytes());
    }

    buf
}

/// Deserialize a BigDecimal from our portable blob format.
pub fn blob_to_bigdecimal(blob: &[u8]) -> crate::Result<BigDecimal> {
    // Minimum size: version(1) + flags(1) + scale(8) + num_limbs(4) = 14
    if blob.len() < 14 {
        return Err(LimboError::Constraint(
            "invalid numeric blob: too short".to_string(),
        ));
    }

    let version = blob[0];
    if version != NUMERIC_BLOB_VERSION {
        return Err(LimboError::Constraint(format!(
            "unsupported numeric blob version: {version}"
        )));
    }

    let flags = blob[1];
    let sign = if flags & FLAG_NEGATIVE != 0 {
        Sign::Minus
    } else {
        Sign::Plus
    };

    // SAFETY: blob[2..10] is exactly 8 bytes (checked by min-length guard above)
    let scale = i64::from_le_bytes(blob[2..10].try_into().unwrap());

    // Reject absurd scales from corrupted blobs.
    if !(-MAX_SCALE_MAGNITUDE..=MAX_SCALE_MAGNITUDE).contains(&scale) {
        return Err(LimboError::Constraint(format!(
            "invalid numeric blob: scale {scale} out of range"
        )));
    }

    // SAFETY: blob[10..14] is exactly 4 bytes (checked by min-length guard above)
    let num_limbs = u32::from_le_bytes(blob[10..14].try_into().unwrap()) as usize;

    // Cap limb count to prevent OOM from crafted blobs. 65536 limbs = 256KB,
    // which supports numbers with ~600,000 decimal digits — far beyond any
    // reasonable precision.
    const MAX_LIMBS: usize = 65536;
    if num_limbs > MAX_LIMBS {
        return Err(LimboError::Constraint(format!(
            "invalid numeric blob: limb count {num_limbs} exceeds maximum {MAX_LIMBS}"
        )));
    }

    let expected_len = num_limbs
        .checked_mul(4)
        .and_then(|n| n.checked_add(14))
        .ok_or_else(|| {
            LimboError::Constraint("invalid numeric blob: limb count overflow".to_string())
        })?;
    if blob.len() != expected_len {
        return Err(LimboError::Constraint(format!(
            "invalid numeric blob: expected {expected_len} bytes, got {}",
            blob.len()
        )));
    }

    let mut limbs = Vec::with_capacity(num_limbs);
    for i in 0..num_limbs {
        let offset = 14 + i * 4;
        let limb = u32::from_le_bytes(blob[offset..offset + 4].try_into().unwrap());
        limbs.push(limb);
    }

    // Handle zero case: BigInt::new with empty limbs and Plus sign = 0
    let sign = if limbs.is_empty() { Sign::NoSign } else { sign };
    let bigint = BigInt::new(sign, limbs);
    Ok(BigDecimal::new(bigint, scale))
}

/// Format a BigDecimal as a string, preserving trailing zeros for the scale.
/// BigDecimal::to_string() may drop trailing zeros, but we want "1.10" to
/// remain "1.10" and "0.00" to remain "0.00" for a value stored with scale=2.
pub fn format_numeric(bd: &BigDecimal) -> String {
    let (bigint, scale) = bd.as_bigint_and_exponent();
    if scale <= 0 {
        // No decimal places: just print the integer (possibly scaled up)
        if scale == 0 {
            return bigint.to_string();
        }
        // Negative scale means multiply by 10^(-scale).
        // Cap to MAX_SCALE_MAGNITUDE (validated on read) to avoid runaway allocation.
        let neg_scale = scale.unsigned_abs().min(MAX_SCALE_MAGNITUDE as u64) as u32;
        let factor = num_bigint::BigInt::from(10).pow(neg_scale);
        return (bigint * factor).to_string();
    }
    let scale = scale as usize;
    let is_negative = bigint.sign() == Sign::Minus;
    let digits = bigint.magnitude().to_string();
    let digits_len = digits.len();

    let (integer_part, frac_part) = if digits_len > scale {
        let split = digits_len - scale;
        (&digits[..split], digits[split..].to_string())
    } else {
        let zeros = "0".repeat(scale - digits_len);
        ("0", format!("{zeros}{digits}"))
    };

    if is_negative {
        format!("-{integer_part}.{frac_part}")
    } else {
        format!("{integer_part}.{frac_part}")
    }
}

pub fn parse_decimal(text: &str) -> crate::Result<BigDecimal> {
    use std::str::FromStr;

    let value = BigDecimal::from_str(text).map_err(|_| {
        LimboError::Constraint(format!("invalid input for type numeric: \"{text}\""))
    })?;
    check_numeric_range(value)
}

pub fn check_numeric_range(value: BigDecimal) -> crate::Result<BigDecimal> {
    use bigdecimal::Zero;

    if value.is_zero() {
        return Ok(value);
    }
    let (_, scale) = value.as_bigint_and_exponent();
    let integer_digits = (value.digits() as i64).saturating_sub(scale);
    if scale > MAX_FRACTION_DIGITS || integer_digits > MAX_INTEGER_DIGITS {
        return Err(LimboError::Constraint(format!(
            "numeric value out of range: at most {MAX_INTEGER_DIGITS} digits before the decimal point and {MAX_FRACTION_DIGITS} after it"
        )));
    }
    Ok(value)
}

pub fn numeric_key(value: &BigDecimal) -> crate::Result<Vec<u8>> {
    use bigdecimal::Zero;

    if value.is_zero() {
        return Ok(vec![KEY_ZERO]);
    }
    let (digits, scale) = value.as_bigint_and_exponent();
    let negative = digits.sign() == Sign::Minus;
    let digit_text = digits.magnitude().to_string();
    let exponent = i32::try_from(digit_text.len() as i64 - scale)
        .map_err(|_| LimboError::Constraint("numeric value out of range".to_string()))?;
    let significant = digit_text.trim_end_matches('0').as_bytes();
    let mut key = Vec::with_capacity(6 + significant.len().div_ceil(2));
    key.push(if negative { KEY_NEGATIVE } else { KEY_POSITIVE });
    let exponent_bytes = ((exponent as u32) ^ 0x8000_0000).to_be_bytes();
    key.extend(
        exponent_bytes
            .iter()
            .map(|byte| if negative { !byte } else { *byte }),
    );
    for pair in significant.chunks(2) {
        let high = pair[0] - b'0';
        let low = pair.get(1).map_or(0, |digit| digit - b'0');
        let pair_value = high * 10 + low;
        key.push(if negative {
            100 - pair_value
        } else {
            pair_value + 1
        });
    }
    if negative {
        key.push(KEY_NEGATIVE_END);
    }
    Ok(key)
}

pub fn divide(dividend: &BigDecimal, divisor: &BigDecimal) -> crate::Result<BigDecimal> {
    use bigdecimal::Zero;

    if divisor.is_zero() {
        return Err(LimboError::Constraint("division by zero".to_string()));
    }
    let result_scale = division_scale(dividend, divisor)?;
    let (dividend_digits, dividend_scale) = dividend.as_bigint_and_exponent();
    let (divisor_digits, divisor_scale) = divisor.as_bigint_and_exponent();
    let shift = divisor_scale - dividend_scale + result_scale;
    let (numerator, denominator) = if shift >= 0 {
        (dividend_digits * power_of_ten(shift)?, divisor_digits)
    } else {
        (dividend_digits, divisor_digits * power_of_ten(-shift)?)
    };
    let quotient = &numerator / &denominator;
    let remainder = &numerator % &denominator;
    let rounds_away_from_zero = remainder.magnitude() * 2u32 >= *denominator.magnitude();
    let quotient = if !rounds_away_from_zero {
        quotient
    } else if numerator.sign() == denominator.sign() {
        quotient + 1
    } else {
        quotient - 1
    };
    check_numeric_range(BigDecimal::new(quotient, result_scale))
}

fn division_scale(dividend: &BigDecimal, divisor: &BigDecimal) -> crate::Result<i64> {
    let (dividend_weight, dividend_leading_digit) = leading_base_10000_digit(dividend)?;
    let (divisor_weight, divisor_leading_digit) = leading_base_10000_digit(divisor)?;
    let mut quotient_weight = dividend_weight - divisor_weight;
    if dividend_leading_digit <= divisor_leading_digit {
        quotient_weight -= 1;
    }
    let scale =
        DIVISION_MIN_SIGNIFICANT_DIGITS - quotient_weight * DECIMAL_DIGITS_PER_BASE_10000_DIGIT;
    Ok(scale
        .max(dividend.fractional_digit_count())
        .max(divisor.fractional_digit_count())
        .clamp(0, DIVISION_MAX_SCALE))
}

fn leading_base_10000_digit(value: &BigDecimal) -> crate::Result<(i64, u32)> {
    use bigdecimal::Zero;

    if value.is_zero() {
        return Ok((0, 0));
    }
    let (digits, scale) = value.as_bigint_and_exponent();
    let magnitude = BigInt::from_biguint(Sign::Plus, digits.magnitude().clone());
    let leading_power_of_ten = value.digits() as i64 - 1 - scale;
    let weight = leading_power_of_ten.div_euclid(DECIMAL_DIGITS_PER_BASE_10000_DIGIT);
    let shift = -scale - weight * DECIMAL_DIGITS_PER_BASE_10000_DIGIT;
    let leading_digit = if shift >= 0 {
        magnitude * power_of_ten(shift)?
    } else {
        magnitude / power_of_ten(-shift)?
    };
    let leading_digit = u32::try_from(&leading_digit)
        .ok()
        .filter(|digit| (1..10000).contains(digit));
    let Some(leading_digit) = leading_digit else {
        return Err(LimboError::InternalError(format!(
            "leading base-10000 digit of {value} is out of range"
        )));
    };
    Ok((weight, leading_digit))
}

fn power_of_ten(exponent: i64) -> crate::Result<BigInt> {
    let limit = 2 * (MAX_INTEGER_DIGITS + MAX_FRACTION_DIGITS) + DIVISION_MAX_SCALE;
    if !(0..=limit).contains(&exponent) {
        return Err(LimboError::Constraint(
            "numeric value out of range".to_string(),
        ));
    }
    Ok(BigInt::from(10).pow(exponent as u32))
}

/// Validate that a BigDecimal fits within the given precision and scale.
/// Precision = total number of significant digits.
/// Scale = number of digits after the decimal point.
pub fn validate_precision_scale(
    val: &BigDecimal,
    precision: i64,
    scale: i64,
) -> crate::Result<BigDecimal> {
    use bigdecimal::Zero;

    if precision <= 0 {
        return Err(LimboError::Constraint(format!(
            "numeric precision must be positive, got {precision}"
        )));
    }
    if scale < 0 {
        return Err(LimboError::Constraint(format!(
            "numeric scale must be non-negative, got {scale}"
        )));
    }
    if scale > precision {
        return Err(LimboError::Constraint(format!(
            "numeric scale ({scale}) must not exceed precision ({precision})"
        )));
    }

    if val.is_zero() {
        return Ok(BigDecimal::new(BigInt::from(0), scale));
    }

    let rounded = val.with_scale_round(scale, bigdecimal::RoundingMode::HalfUp);

    // Count total significant digits (excluding leading zeros)
    let (bigint, _) = rounded.as_bigint_and_exponent();
    let abs_str = bigint.magnitude().to_string();
    let total_digits = abs_str.len() as i64;

    if total_digits > precision {
        return Err(LimboError::Constraint(format!(
            "numeric value out of range: precision {precision}, scale {scale}"
        )));
    }

    Ok(rounded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bigdecimal::Zero;
    use std::str::FromStr;

    #[test]
    fn test_numeric_key_is_equal_for_equal_values() {
        let key = |text: &str| numeric_key(&BigDecimal::from_str(text).unwrap()).unwrap();
        assert_eq!(key("1.5"), key("1.50"));
        assert_eq!(key("100"), key("1e2"));
        assert_eq!(key("0"), key("-0.000"));
        assert_ne!(key("1.5"), key("1.51"));
    }

    #[test]
    fn test_numeric_key_sorts_like_the_numbers() {
        let mut values: Vec<BigDecimal> = [
            "-1000", "-99.99", "-1.23", "-1.2", "-1", "-0.5", "-0.05", "0", "0.05", "0.5", "1",
            "1.2", "1.23", "99.99", "100", "1e40", "-1e40", "1e-40", "-1e-40",
        ]
        .iter()
        .map(|text| BigDecimal::from_str(text).unwrap())
        .collect();
        values.sort();
        let keys: Vec<Vec<u8>> = values.iter().map(|v| numeric_key(v).unwrap()).collect();
        for pair in keys.windows(2) {
            assert!(pair[0] < pair[1], "{:?} < {:?}", pair[0], pair[1]);
        }
    }

    #[test]
    fn test_divide_chooses_postgres_scale() {
        let cases = [
            ("1.55", "3", "0.51666666666666666667"),
            ("100.00", "10", "10.0000000000000000"),
            ("1", "3", "0.33333333333333333333"),
            ("10", "3", "3.3333333333333333"),
            ("0", "7", "0.00000000000000000000"),
            ("1234567", "0.001", "1234567000.00000000"),
            ("0.00001", "3", "0.000003333333333333333333"),
            ("2.5", "-2", "-1.25000000000000000000"),
            ("-7", "3", "-2.3333333333333333"),
            ("1.0", "9999", "0.00010001000100010001"),
            ("99999", "1", "99999.000000000000"),
            ("2", "3", "0.66666666666666666667"),
            ("-2", "3", "-0.66666666666666666667"),
        ];
        for (dividend, divisor, expected) in cases {
            let dividend = BigDecimal::from_str(dividend).unwrap();
            let divisor = BigDecimal::from_str(divisor).unwrap();
            let quotient = divide(&dividend, &divisor).unwrap();
            assert_eq!(
                format_numeric(&quotient),
                expected,
                "{dividend} / {divisor}"
            );
        }
    }

    #[test]
    fn test_parse_decimal_refuses_values_outside_the_numeric_range() {
        for text in ["1e-5000000000", "1e-16384", "1e131072", "1e5000000000"] {
            assert!(parse_decimal(text).is_err(), "{text}");
        }
        for text in ["1e-16383", "1e131071", "-12.5"] {
            assert!(parse_decimal(text).is_ok(), "{text}");
        }
    }

    #[test]
    fn test_divide_returns_an_error_for_huge_exponents() {
        let one = BigDecimal::from_str("1").unwrap();
        let tiny = BigDecimal::new(BigInt::from(1), 5_000_000_000);
        let huge = BigDecimal::new(BigInt::from(1), -5_000_000_000);
        assert!(divide(&one, &tiny).is_err());
        assert!(divide(&one, &huge).is_err());
        assert!(divide(&tiny, &one).is_err());
    }

    #[test]
    fn test_divide_by_zero_is_an_error() {
        let one = BigDecimal::from_str("1").unwrap();
        let zero = BigDecimal::from_str("0.00").unwrap();
        assert!(divide(&one, &zero).is_err());
    }

    #[test]
    fn test_roundtrip_positive() {
        let val = BigDecimal::from_str("123.45").unwrap();
        let blob = bigdecimal_to_blob(&val);
        let decoded = blob_to_bigdecimal(&blob).unwrap();
        assert_eq!(val, decoded);
    }

    #[test]
    fn test_roundtrip_negative() {
        let val = BigDecimal::from_str("-987.654").unwrap();
        let blob = bigdecimal_to_blob(&val);
        let decoded = blob_to_bigdecimal(&blob).unwrap();
        assert_eq!(val, decoded);
    }

    #[test]
    fn test_roundtrip_zero() {
        let val = BigDecimal::from_str("0").unwrap();
        let blob = bigdecimal_to_blob(&val);
        let decoded = blob_to_bigdecimal(&blob).unwrap();
        assert!(decoded.is_zero());
    }

    #[test]
    fn test_roundtrip_large() {
        // 100+ digit number
        let s = "12345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890.99";
        let val = BigDecimal::from_str(s).unwrap();
        let blob = bigdecimal_to_blob(&val);
        let decoded = blob_to_bigdecimal(&blob).unwrap();
        assert_eq!(val, decoded);
    }

    #[test]
    fn test_validate_precision_scale_ok() {
        let val = BigDecimal::from_str("123.45").unwrap();
        let result = validate_precision_scale(&val, 5, 2);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_precision_scale_rounds_half_away_from_zero() {
        let cases = [
            ("1.505", "1.51"),
            ("-1.505", "-1.51"),
            ("1.504", "1.50"),
            ("2.995", "3.00"),
        ];
        for (input, expected) in cases {
            let val = BigDecimal::from_str(input).unwrap();
            let rounded = validate_precision_scale(&val, 10, 2).unwrap();
            assert_eq!(format_numeric(&rounded), expected, "input {input}");
        }
    }

    #[test]
    fn test_validate_precision_scale_rounding_can_overflow() {
        let val = BigDecimal::from_str("9.995").unwrap();
        assert!(validate_precision_scale(&val, 3, 2).is_err());
    }

    #[test]
    fn test_validate_precision_scale_overflow() {
        let val = BigDecimal::from_str("12345.67").unwrap();
        let result = validate_precision_scale(&val, 5, 2);
        assert!(result.is_err());
    }

    // ================================================================
    // Property-based fuzz tests (quickcheck)
    // ================================================================

    use quickcheck::{Arbitrary, Gen};
    use quickcheck_macros::quickcheck;

    /// Newtype for generating arbitrary BigDecimal values via quickcheck.
    #[derive(Debug, Clone)]
    struct ArbDecimal(BigDecimal);

    impl Arbitrary for ArbDecimal {
        fn arbitrary(g: &mut Gen) -> Self {
            let mantissa = i64::arbitrary(g);
            // Scale in [-100, 100] — covers negative scale (large integers),
            // zero scale (plain integers), and positive scale (decimals).
            let scale = (i64::arbitrary(g) % 101).abs();
            let sign_flip = bool::arbitrary(g);
            let s = if sign_flip { -scale } else { scale };
            ArbDecimal(BigDecimal::new(BigInt::from(mantissa), s))
        }
    }

    /// Newtype for generating valid (precision, scale) pairs.
    #[derive(Debug, Clone)]
    struct ArbPrecisionScale {
        precision: i64,
        scale: i64,
    }

    impl Arbitrary for ArbPrecisionScale {
        fn arbitrary(g: &mut Gen) -> Self {
            let precision = (u8::arbitrary(g) % 38) as i64 + 1; // 1..=38
            let scale = (u8::arbitrary(g) as i64) % (precision + 1); // 0..=precision
            ArbPrecisionScale { precision, scale }
        }
    }

    // P1: Roundtrip encode/decode — blob_to_bigdecimal(bigdecimal_to_blob(x)) == x
    #[quickcheck]
    fn prop_blob_roundtrip(arb: ArbDecimal) -> bool {
        let blob = bigdecimal_to_blob(&arb.0);
        let decoded = blob_to_bigdecimal(&blob).unwrap();
        decoded == arb.0
    }

    // P2: format_numeric parse roundtrip — for non-negative scale,
    // parsing the formatted string should produce a numerically equal value.
    #[quickcheck]
    fn prop_format_parse_roundtrip(arb: ArbDecimal) -> bool {
        let (_, scale) = arb.0.as_bigint_and_exponent();
        if scale < 0 {
            return true; // skip negative scale — format_numeric expands it
        }
        let formatted = format_numeric(&arb.0);
        let parsed = BigDecimal::from_str(&formatted).unwrap();
        // Numerically equal (may differ in internal scale representation)
        parsed == arb.0
    }

    #[quickcheck]
    fn prop_numeric_key_order_equals_numeric_order(a: ArbDecimal, b: ArbDecimal) -> bool {
        let key_order = numeric_key(&a.0).unwrap().cmp(&numeric_key(&b.0).unwrap());
        key_order == a.0.cmp(&b.0)
    }

    // P3: validate_precision_scale idempotence — applying it twice gives same result.
    #[quickcheck]
    fn prop_validate_idempotent(arb: ArbDecimal, ps: ArbPrecisionScale) -> bool {
        let first = validate_precision_scale(&arb.0, ps.precision, ps.scale);
        match first {
            Ok(y) => {
                let second = validate_precision_scale(&y, ps.precision, ps.scale).unwrap();
                second == y
            }
            Err(_) => true, // rejection is fine
        }
    }

    // P4: validate_precision_scale bounds — result fits within declared precision/scale.
    #[quickcheck]
    fn prop_validate_respects_bounds(arb: ArbDecimal, ps: ArbPrecisionScale) -> bool {
        match validate_precision_scale(&arb.0, ps.precision, ps.scale) {
            Ok(y) => {
                let (bigint, result_scale) = y.as_bigint_and_exponent();
                // Scale must match requested scale
                if result_scale != ps.scale {
                    return false;
                }
                // Digit count must be <= precision
                let digits = bigint.magnitude().to_string();
                let digit_count = if digits == "0" {
                    0
                } else {
                    digits.len() as i64
                };
                digit_count <= ps.precision
            }
            Err(_) => true,
        }
    }

    // P5: blob_to_bigdecimal never panics on arbitrary bytes.
    #[quickcheck]
    fn prop_blob_decode_no_panic(data: Vec<u8>) -> bool {
        let _ = blob_to_bigdecimal(&data);
        true // just checking it doesn't panic
    }

    // P6: format_numeric never panics.
    #[quickcheck]
    fn prop_format_no_panic(arb: ArbDecimal) -> bool {
        let _ = format_numeric(&arb.0);
        true
    }

    // P7: Ordering is preserved through encode/decode roundtrip.
    #[quickcheck]
    fn prop_ordering_preserved(a: ArbDecimal, b: ArbDecimal) -> bool {
        let blob_a = bigdecimal_to_blob(&a.0);
        let blob_b = bigdecimal_to_blob(&b.0);
        let decoded_a = blob_to_bigdecimal(&blob_a).unwrap();
        let decoded_b = blob_to_bigdecimal(&blob_b).unwrap();
        // The ordering of decoded values must match the ordering of originals
        a.0.cmp(&b.0) == decoded_a.cmp(&decoded_b)
    }
}
