use turso_core::{Func, Result, Value};

mod datetime;
mod json;
mod string;

pub(crate) fn resolve(name: &str, arg_count: usize) -> Option<Func> {
    let name = name.to_ascii_uppercase();
    let accepted = datetime::accepts(&name, arg_count)
        || string::accepts(&name, arg_count)
        || json::accepts(&name, arg_count);
    accepted.then_some(Func::Dialect(name))
}

pub(crate) fn execute(name: &str, args: &[Value]) -> Option<Result<Value>> {
    datetime::execute(name, args)
        .or_else(|| string::execute(name, args))
        .or_else(|| json::execute(name, args))
}
