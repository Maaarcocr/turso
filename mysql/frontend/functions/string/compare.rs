use std::cmp::Ordering;

use turso_core::{LimboError, Numeric, Result, Value};

use super::collation::Collation;

use super::convert::{double, integer, string, string_converted_to_text, Str};

#[derive(Clone, Copy, PartialEq)]
enum ComparisonType {
    Integer,
    Double,
    String,
}

#[derive(Clone, Copy)]
enum Number {
    Integer(i64),
    Double(f64),
}

#[derive(Clone, Copy)]
struct Comparison {
    collation: Collation,
    explicit: bool,
}

const DEFAULT_COMPARISON: Comparison = Comparison {
    collation: Collation::AccentInsensitive,
    explicit: false,
};

pub(super) fn field(args: &[Value]) -> Result<Value> {
    field_in(DEFAULT_COMPARISON, args)
}

pub(super) fn collated_field(args: &[Value]) -> Result<Value> {
    let (comparison, args) = collated(args)?;
    field_in(comparison, args)
}

pub(super) fn collated_strcmp(args: &[Value]) -> Result<Value> {
    let (comparison, args) = collated(args)?;
    strcmp_in(comparison, args)
}

pub(super) fn collated_greatest(args: &[Value]) -> Result<Value> {
    let (comparison, args) = collated(args)?;
    extreme(comparison, args, Ordering::Greater)
}

pub(super) fn collated_least(args: &[Value]) -> Result<Value> {
    let (comparison, args) = collated(args)?;
    extreme(comparison, args, Ordering::Less)
}

fn collated(args: &[Value]) -> Result<(Comparison, &[Value])> {
    let [Value::Text(name), rest @ ..] = args else {
        return Err(LimboError::InternalError(
            "a collated comparison function takes a collation name first".to_string(),
        ));
    };
    let collation = Collation::named(name.as_str())
        .ok_or_else(|| LimboError::InternalError(format!("unknown collation {}", name.as_str())))?;
    let comparison = Comparison {
        collation,
        explicit: true,
    };
    Ok((comparison, rest))
}

fn field_in(rules: Comparison, args: &[Value]) -> Result<Value> {
    let comparison = args
        .iter()
        .map(comparison_type)
        .reduce(common_comparison_type)
        .expect("FIELD has arguments");
    let mut candidates = args[1..].iter().enumerate();
    let found = match comparison {
        ComparisonType::String => {
            let convert_binary = args.iter().any(|value| matches!(value, Value::Text(_)));
            let strings = args
                .iter()
                .map(|value| string_converted_to_text(value, convert_binary))
                .collect::<Result<Vec<_>>>()?;
            let Some(target) = &strings[0] else {
                return Ok(Value::from_i64(0));
            };
            let binary = !rules.explicit && strings.iter().flatten().any(Str::is_binary);
            strings[1..].iter().position(|value| {
                value.as_ref().is_some_and(|value| {
                    compare_strings(rules.collation, target, value, binary).is_eq()
                })
            })
        }
        ComparisonType::Integer => {
            let Some(target) = integer(&args[0]) else {
                return Ok(Value::from_i64(0));
            };
            candidates
                .find(|(_, value)| integer(value).is_some_and(|value| value.value == target.value))
                .map(|(index, _)| index)
        }
        ComparisonType::Double => {
            let Some(target) = number(&args[0]) else {
                return Ok(Value::from_i64(0));
            };
            candidates
                .find(|(_, value)| {
                    number(value).is_some_and(|value| compare_numbers(&value, &target).is_eq())
                })
                .map(|(index, _)| index)
        }
    };
    Ok(Value::from_i64(found.map_or(0, |index| index as i64 + 1)))
}

fn comparison_type(value: &Value) -> ComparisonType {
    match value {
        Value::Numeric(Numeric::Integer(_)) => ComparisonType::Integer,
        Value::Numeric(Numeric::Float(_)) => ComparisonType::Double,
        Value::Null | Value::Text(_) | Value::Blob(_) => ComparisonType::String,
    }
}

fn common_comparison_type(left: ComparisonType, right: ComparisonType) -> ComparisonType {
    match (left, right) {
        (ComparisonType::String, ComparisonType::String) => ComparisonType::String,
        (ComparisonType::Integer, ComparisonType::Integer) => ComparisonType::Integer,
        _ => ComparisonType::Double,
    }
}

pub(super) fn interval(args: &[Value]) -> Result<Value> {
    let Some(target) = number(&args[0]) else {
        return Ok(Value::from_i64(-1));
    };
    let bounds = &args[1..];
    if args.len() > 8 && bounds.iter().all(|bound| !matches!(bound, Value::Null)) {
        return Ok(Value::from_i64(sorted_interval(&target, bounds)));
    }
    for (index, bound) in bounds.iter().enumerate() {
        if let Some(bound) = number(bound) {
            if compare_numbers(&bound, &target) == Ordering::Greater {
                return Ok(Value::from_i64(index as i64));
            }
        }
    }
    Ok(Value::from_i64(bounds.len() as i64))
}

fn sorted_interval(target: &Number, bounds: &[Value]) -> i64 {
    let bounds: Vec<Number> = bounds
        .iter()
        .map(|bound| number(bound).expect("the bounds are not NULL"))
        .collect();
    let mut start = 0;
    let mut end = bounds.len() - 1;
    while start != end {
        let middle = (start + end).div_ceil(2);
        if compare_numbers(&bounds[middle], target) != Ordering::Greater {
            start = middle;
        } else {
            end = middle - 1;
        }
    }
    if compare_numbers(target, &bounds[start]) == Ordering::Less {
        0
    } else {
        start as i64 + 1
    }
}

pub(super) fn strcmp(args: &[Value]) -> Result<Value> {
    strcmp_in(DEFAULT_COMPARISON, args)
}

fn strcmp_in(comparison: Comparison, args: &[Value]) -> Result<Value> {
    let (Some(left), Some(right)) = (string(&args[0]), string(&args[1])) else {
        return Ok(Value::Null);
    };
    let binary = !comparison.explicit && (left.is_binary() || right.is_binary());
    Ok(Value::from_i64(
        match compare_strings(comparison.collation, &left, &right, binary) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        },
    ))
}

pub(super) fn greatest(args: &[Value]) -> Result<Value> {
    extreme(DEFAULT_COMPARISON, args, Ordering::Greater)
}

pub(super) fn least(args: &[Value]) -> Result<Value> {
    extreme(DEFAULT_COMPARISON, args, Ordering::Less)
}

fn extreme(comparison: Comparison, args: &[Value], wanted: Ordering) -> Result<Value> {
    if args.iter().any(|value| matches!(value, Value::Null)) {
        return Ok(Value::Null);
    }
    if args
        .iter()
        .any(|value| matches!(value, Value::Text(_) | Value::Blob(_)))
    {
        let binary =
            !comparison.explicit && args.iter().any(|value| matches!(value, Value::Blob(_)));
        let strings: Vec<Str> = args
            .iter()
            .map(|value| string(value).expect("the arguments are not NULL"))
            .collect();
        let mut best = &strings[0];
        for candidate in &strings[1..] {
            let ordering = compare_strings(comparison.collation, candidate, best, binary);
            if ordering == wanted || (wanted == Ordering::Greater && ordering.is_eq()) {
                best = candidate;
            }
        }
        return if binary {
            best.clone().into_binary().into_value()
        } else {
            best.clone().into_owned().into_value()
        };
    }
    let numbers: Vec<Number> = args
        .iter()
        .map(|value| number(value).expect("the arguments are not NULL"))
        .collect();
    let mut best = numbers[0];
    for candidate in &numbers[1..] {
        if compare_numbers(candidate, &best) == wanted {
            best = *candidate;
        }
    }
    let all_integers = numbers
        .iter()
        .all(|number| matches!(number, Number::Integer(_)));
    Ok(match best {
        Number::Integer(value) if all_integers => Value::from_i64(value),
        Number::Integer(value) => Value::from_f64(value as f64),
        Number::Double(value) => Value::from_f64(value),
    })
}

fn compare_strings(collation: Collation, left: &Str, right: &Str, binary: bool) -> Ordering {
    if binary {
        left.as_bytes().cmp(right.as_bytes())
    } else {
        collation.compare_bytes(left.as_bytes(), right.as_bytes())
    }
}

fn number(value: &Value) -> Option<Number> {
    match value {
        Value::Numeric(Numeric::Integer(value)) => Some(Number::Integer(*value)),
        other => double(other).map(Number::Double),
    }
}

fn compare_numbers(left: &Number, right: &Number) -> Ordering {
    match (left, right) {
        (Number::Integer(left), Number::Integer(right)) => left.cmp(right),
        (Number::Integer(left), Number::Double(right)) => {
            compare_integer_with_double(*left, *right)
        }
        (Number::Double(left), Number::Integer(right)) => {
            compare_integer_with_double(*right, *left).reverse()
        }
        (Number::Double(left), Number::Double(right)) => {
            left.partial_cmp(right).unwrap_or(Ordering::Equal)
        }
    }
}

fn compare_integer_with_double(integer: i64, double: f64) -> Ordering {
    const TWO_TO_THE_63: f64 = 9_223_372_036_854_775_808.0;
    if double >= TWO_TO_THE_63 {
        return Ordering::Less;
    }
    if double < -TWO_TO_THE_63 {
        return Ordering::Greater;
    }
    let whole = double.trunc();
    integer.cmp(&(whole as i64)).then_with(|| {
        0.0.partial_cmp(&(double - whole))
            .unwrap_or(Ordering::Equal)
    })
}
