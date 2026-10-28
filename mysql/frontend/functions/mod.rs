use crate::types::decimal_number::Rounding;
use decimal::shown_decimal;
use std::borrow::Cow;
use turso_core::dialect::ArgumentType;
use turso_core::{Func, Result, Value};

mod datetime;
mod decimal;
mod json;
mod string;

pub(crate) use string::format_double;

pub(crate) fn resolve(name: &str, arg_count: usize) -> Option<Func> {
    let name = name.to_ascii_uppercase();
    let accepted = datetime::accepts(&name, arg_count)
        || string::accepts(&name, arg_count)
        || json::accepts(&name, arg_count);
    accepted.then_some(Func::Dialect(name))
}

/// Runs a function with the static types of its arguments. A DECIMAL
/// argument is a value with exact digits, rounded away from its hidden
/// digits as MySQL shows it: string functions read its text (`1.50`), and
/// their counts and positions round it half away from zero. Other functions
/// read it as a DOUBLE, as before DECIMAL had a type.
pub(crate) fn execute_typed(
    name: &str,
    args: &[Value],
    arg_types: &[ArgumentType],
) -> Option<Result<Value>> {
    if let Some(result) = decimal::execute_typed(name, args, arg_types) {
        return Some(result);
    }
    let args = with_decimals_read(name, args, arg_types);
    execute(name, &args)
}

pub(crate) fn execute(name: &str, args: &[Value]) -> Option<Result<Value>> {
    datetime::execute(name, args)
        .or_else(|| string::execute(name, args))
        .or_else(|| json::execute(name, args))
}

fn with_decimals_read<'a>(
    name: &str,
    args: &'a [Value],
    arg_types: &[ArgumentType],
) -> Cow<'a, [Value]> {
    let shown = |position: usize| shown_decimal(args.get(position)?, arg_types.get(position)?);
    if !(0..args.len()).any(|position| shown(position).is_some()) {
        return Cow::Borrowed(args);
    }
    Cow::Owned(
        (0..args.len())
            .map(|position| match shown(position) {
                None => args[position].clone(),
                Some(decimal) => match string::decimal_argument(name, position) {
                    DecimalArgument::Text => Value::build_text(decimal.to_text()),
                    DecimalArgument::Count => {
                        let whole = decimal.rounded(0, Rounding::HalfAwayFromZero);
                        Value::from_i64(whole.to_i64().unwrap_or_else(|| {
                            if whole.is_negative() {
                                i64::MIN
                            } else {
                                i64::MAX
                            }
                        }))
                    }
                    DecimalArgument::Double => Value::from_f64(decimal.to_f64()),
                },
            })
            .collect(),
    )
}

pub(crate) enum DecimalArgument {
    Text,
    Count,
    Double,
}
