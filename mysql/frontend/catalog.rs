use turso_core::{Func, LimboError, Result, Value};
use turso_parser::ast;

const SCHEMA_NAME: &str = "__MYSQL_SCHEMA_NAME";

pub(crate) fn resolve(name: &str, arg_count: usize) -> Option<Func> {
    (name.eq_ignore_ascii_case(SCHEMA_NAME) && arg_count == 1)
        .then(|| Func::Dialect(SCHEMA_NAME.to_string()))
}

pub(crate) fn execute(name: &str, args: &[Value]) -> Option<Result<Value>> {
    (name == SCHEMA_NAME).then(|| schema_name(args))
}

/// `__mysql_schema_name(sql)`: the name of the table or view that the
/// stored `sql` of `sqlite_schema` creates, with the case it was created
/// with. Turso keeps `sqlite_schema.name` in lower case, where MySQL keeps
/// the case of the name.
fn schema_name(args: &[Value]) -> Result<Value> {
    let [Value::Text(sql)] = args else {
        return Err(LimboError::InternalError(format!(
            "a table or view is named by its stored SQL, not {args:?}"
        )));
    };
    let mut parser = turso_parser::parser::Parser::new(sql.as_str().as_bytes());
    let name = match parser.next_cmd() {
        Ok(Some(ast::Cmd::Stmt(ast::Stmt::CreateTable { tbl_name, .. }))) => tbl_name.name,
        Ok(Some(ast::Cmd::Stmt(ast::Stmt::CreateView { view_name, .. }))) => view_name.name,
        other => {
            return Err(LimboError::InternalError(format!(
                "stored SQL `{}` does not create a table or view: {other:?}",
                sql.as_str()
            )))
        }
    };
    Ok(Value::build_text(name.as_str().to_string()))
}
