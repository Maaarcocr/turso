use turso_core::{Result, Value};

pub(super) fn accepts(_name: &str, _arg_count: usize) -> bool {
    false
}

pub(super) fn execute(_name: &str, _args: &[Value]) -> Option<Result<Value>> {
    None
}
