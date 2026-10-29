use decimal::shown_decimal;
use std::borrow::Cow;
use turso_core::dialect::ArgumentType;
use turso_core::types::Text;
use turso_core::{Func, Result, Value};

mod datetime;
mod decimal;
mod json;
mod string;
mod variance;

pub(crate) use json::assign_column as assign_json;
pub(crate) use string::format_double;
pub(crate) use variance::register as register_variances;

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
/// read it as a DOUBLE, as before DECIMAL had a type. A value of a JSON
/// column is a JSON document, as the result of a JSON function is.
pub(crate) fn execute_typed(
    name: &str,
    args: &[Value],
    arg_types: &[ArgumentType],
) -> Option<Result<Value>> {
    let args = &*with_json_columns_read(args, arg_types);
    if let Some(result) = json::execute_typed(name, args, arg_types) {
        return Some(result);
    }
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

fn with_json_columns_read<'a>(args: &'a [Value], arg_types: &[ArgumentType]) -> Cow<'a, [Value]> {
    let is_json_column = |position: usize| {
        matches!(
            (&args[position], arg_types.get(position)),
            (Value::Text(_), Some(ArgumentType::Named { name, .. })) if name.eq_ignore_ascii_case("json")
        )
    };
    if !(0..args.len()).any(is_json_column) {
        return Cow::Borrowed(args);
    }
    Cow::Owned(
        args.iter()
            .enumerate()
            .map(|(position, value)| match value {
                Value::Text(text) if is_json_column(position) => {
                    Value::Text(Text::json(text.as_str().to_string()))
                }
                other => other.clone(),
            })
            .collect(),
    )
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
                    DecimalArgument::Count => Value::from_i64(decimal::whole(&decimal)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lowering_names_functions_this_frontend_implements() {
        for name in turso_mysql_parser::BINARY_STRING_FUNCTIONS {
            assert!(
                (1..=6).any(|count| string::accepts(name, count)),
                "{name} is not a string function"
            );
        }
        for name in turso_mysql_parser::DATE_TIME_FUNCTIONS {
            assert!(
                (1..=3).any(|count| datetime::accepts(name, count)),
                "{name} is not a date and time function"
            );
        }
    }
}
