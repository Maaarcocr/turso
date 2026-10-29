use super::string;
use crate::types::decimal_number::{Decimal, Rounding};
use std::cmp::Ordering;
use turso_core::dialect::ArgumentType;
use turso_core::{Numeric, Result, Value};

/// Functions that keep a DECIMAL argument exact where core does not pick a
/// DECIMAL function by the argument types: ROUND and TRUNCATE whose number
/// of places is not an integer, and GREATEST and LEAST.
pub(super) fn execute_typed(
    name: &str,
    args: &[Value],
    arg_types: &[ArgumentType],
) -> Option<Result<Value>> {
    let decimals: Vec<Option<Decimal>> = args
        .iter()
        .zip(arg_types)
        .map(|(value, arg_type)| shown_decimal(value, arg_type))
        .collect();
    match (name, decimals.as_slice()) {
        ("ROUND", [Some(decimal), _]) => Some(Ok(rounded(
            decimal,
            &args[1],
            &arg_types[1],
            Rounding::HalfAwayFromZero,
        ))),
        ("TRUNCATE", [Some(decimal), _]) => Some(Ok(rounded(
            decimal,
            &args[1],
            &arg_types[1],
            Rounding::TowardZero,
        ))),
        ("FORMAT", [Some(decimal), places, ..]) => {
            let mut args = args.to_vec();
            if let Some(places) = places {
                args[1] = Value::from_i64(whole(places));
            }
            Some(string::format_decimal(decimal, &args))
        }
        ("MOD", [_, _]) if decimals.iter().any(Option::is_some) => {
            Some(Ok(remainder(args, &decimals)))
        }
        ("INTERVAL", [_, _, ..]) if decimals.iter().any(Option::is_some) => {
            interval(args, &decimals)
        }
        ("GREATEST", _) if decimals.iter().any(Option::is_some) => {
            extreme(name, args, &decimals, Ordering::Greater)
        }
        ("LEAST", _) if decimals.iter().any(Option::is_some) => {
            extreme(name, args, &decimals, Ordering::Less)
        }
        _ => None,
    }
}

/// The integer MySQL reads from a DECIMAL: rounded half away from zero and
/// clamped to BIGINT.
pub(super) fn whole(decimal: &Decimal) -> i64 {
    decimal
        .rounded(0, Rounding::HalfAwayFromZero)
        .to_i64()
        .unwrap_or_else(|| {
            if decimal.is_negative() {
                i64::MIN
            } else {
                i64::MAX
            }
        })
}

fn rounded(
    decimal: &Decimal,
    places: &Value,
    places_type: &ArgumentType,
    rounding: Rounding,
) -> Value {
    let places = match (shown_decimal(places, places_type), places) {
        (Some(places), _) => whole(&places),
        (None, Value::Null) => return Value::Null,
        (None, Value::Numeric(Numeric::Integer(places))) => *places,
        (None, Value::Numeric(Numeric::Float(places))) => {
            f64::from(*places).round_ties_even() as i64
        }
        (None, Value::Text(text)) => string::leading_integer(text.as_str().as_bytes()),
        (None, Value::Blob(bytes)) => string::leading_integer(bytes),
    }
    .clamp(-LARGEST_PLACES, LARGEST_PLACES);
    Value::build_text(decimal.rounded_within_scale(places, rounding).to_text())
}

const LARGEST_PLACES: i64 = 1000;

/// `MOD(a, b)` with a DECIMAL argument: exact when the other argument is an
/// integer or a DECIMAL, else a DOUBLE as for strings and doubles.
fn remainder(args: &[Value], decimals: &[Option<Decimal>]) -> Value {
    let exact = |position: usize| match (&args[position], &decimals[position]) {
        (_, Some(decimal)) => Some(decimal.clone()),
        (Value::Numeric(Numeric::Integer(number)), None) => Some(Decimal::from_i64(*number)),
        _ => None,
    };
    if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
        return Value::Null;
    }
    match (exact(0), exact(1)) {
        (Some(dividend), Some(divisor)) => dividend
            .remainder(&divisor)
            .map_or(Value::Null, |result| Value::build_text(result.to_text())),
        _ => {
            let double = |position: usize| match &decimals[position] {
                Some(decimal) => decimal.to_f64(),
                None => string::double_argument(&args[position]),
            };
            let (dividend, divisor) = (double(0), double(1));
            if divisor == 0.0 {
                Value::Null
            } else {
                Value::from_f64(dividend % divisor)
            }
        }
    }
}

/// `INTERVAL(n, n1, n2, ...)` with a DECIMAL argument and an integer or
/// DECIMAL `n`: MySQL compares `n` with each integer or DECIMAL bound as
/// DECIMAL and with any other bound as DOUBLE. Over more than eight bounds,
/// none NULL, it binary searches them; otherwise it returns the position
/// before the first bound above `n`, skipping NULL bounds.
fn interval(args: &[Value], decimals: &[Option<Decimal>]) -> Option<Result<Value>> {
    let exact = |position: usize| match (&args[position], &decimals[position]) {
        (_, Some(decimal)) => Some(decimal.clone()),
        (Value::Numeric(Numeric::Integer(number)), None) => Some(Decimal::from_i64(*number)),
        _ => None,
    };
    if matches!(args[0], Value::Null) {
        return Some(Ok(Value::from_i64(-1)));
    }
    let target = exact(0)?;
    let target_double = target.to_f64();
    let above = |position: usize| match exact(position) {
        Some(bound) => bound.compare(&target) == Ordering::Greater,
        None => string::double_argument(&args[position]) > target_double,
    };
    let bounds = args.len() - 1;
    if bounds > 8 && args[1..].iter().all(|bound| !matches!(bound, Value::Null)) {
        let (mut start, mut end) = (0, bounds - 1);
        while start != end {
            let middle = (start + end).div_ceil(2);
            if above(middle + 1) {
                end = middle - 1;
            } else {
                start = middle;
            }
        }
        let position = if above(start + 1) { 0 } else { start + 1 };
        return Some(Ok(Value::from_i64(position as i64)));
    }
    let position = (1..args.len())
        .find(|&position| !matches!(args[position], Value::Null) && above(position))
        .map_or(bounds, |position| position - 1);
    Some(Ok(Value::from_i64(position as i64)))
}

/// GREATEST and LEAST with a DECIMAL argument: compared as DECIMAL when
/// every argument is an integer or a DECIMAL, and shown with the largest
/// scale among them; as strings when an argument is a string, with each
/// DECIMAL as its text; as DOUBLE when an argument is a DOUBLE.
fn extreme(
    name: &str,
    args: &[Value],
    decimals: &[Option<Decimal>],
    wanted: Ordering,
) -> Option<Result<Value>> {
    let has_string = args.iter().zip(decimals).any(|(value, decimal)| {
        decimal.is_none() && matches!(value, Value::Text(_) | Value::Blob(_))
    });
    if has_string {
        let strings: Vec<Value> = args
            .iter()
            .zip(decimals)
            .map(|(value, decimal)| match decimal {
                Some(decimal) => Value::build_text(decimal.to_text()),
                None => value.clone(),
            })
            .collect();
        return string::execute(name, &strings);
    }
    let mut exact = Vec::with_capacity(args.len());
    for (value, decimal) in args.iter().zip(decimals) {
        match (value, decimal) {
            (Value::Null, _) => return Some(Ok(Value::Null)),
            (_, Some(decimal)) => exact.push(decimal.clone()),
            (Value::Numeric(Numeric::Integer(number)), None) => {
                exact.push(Decimal::from_i64(*number))
            }
            _ => return None,
        }
    }
    let scale = exact.iter().map(Decimal::scale).max().unwrap_or(0);
    let best = exact
        .into_iter()
        .reduce(|best, candidate| {
            if candidate.compare(&best) == wanted {
                candidate
            } else {
                best
            }
        })
        .expect("GREATEST and LEAST have arguments");
    Some(Ok(Value::build_text(
        best.rounded(i64::from(scale), Rounding::HalfAwayFromZero)
            .to_text(),
    )))
}

/// A DECIMAL argument rounded to the scale of its type, as MySQL shows it.
pub(super) fn shown_decimal(value: &Value, arg_type: &ArgumentType) -> Option<Decimal> {
    let ArgumentType::Named { name, parameters } = arg_type else {
        return None;
    };
    if !name.eq_ignore_ascii_case("decimal") && !name.eq_ignore_ascii_case("decimal unsigned") {
        return None;
    }
    let Value::Text(text) = value else {
        return None;
    };
    let decimal = Decimal::parse(text.as_str())?;
    Some(
        match parameters
            .get(1)
            .and_then(|scale| scale.parse::<i64>().ok())
        {
            Some(scale) => decimal.rounded(scale, Rounding::HalfAwayFromZero),
            None => decimal,
        },
    )
}
