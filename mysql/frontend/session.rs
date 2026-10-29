use crate::types::unsupported_value;
use std::collections::HashMap;
use std::sync::Mutex;
use turso_core::dialect::ArgumentType;
use turso_core::types::TextSubtype;
use turso_core::{Connection, Func, LimboError, Numeric, Result, Value};

/// The state of a MySQL session that outlives its statements, kept as the
/// context of the connection: the user variables.
#[derive(Default)]
pub(crate) struct Session {
    user_variables: Mutex<HashMap<String, UserVariable>>,
}

/// A user variable's value, and whether a string value has MySQL's default
/// collation, which comparisons give every `@name`.
#[derive(Clone)]
struct UserVariable {
    value: Value,
    default_collation: bool,
}

pub(crate) fn resolve(name: &str, arg_count: usize) -> Option<Func> {
    let name = name.to_ascii_uppercase();
    let accepted = match name.as_str() {
        USER_VARIABLE | USER_VARIABLE_VALUE | ONE_JOINED_ROW => arg_count == 1,
        SET_USER_VARIABLES => arg_count >= 3 && arg_count % 3 == 0,
        KEEP_SESSION_SETTINGS => arg_count >= 2 && arg_count % 2 == 0,
        _ => false,
    };
    accepted.then_some(Func::Dialect(name))
}

/// Runs a session function: `__mysql_user_variable(name)` reads `@name`
/// (NULL until it is set) and refuses a string whose collation is not known
/// to be the default; `__mysql_user_variable_value(name)` reads it for `SET`
/// of a setting, where its collation does not matter; and
/// `__mysql_set_user_variables(name, default_collation, value, ...)` stores
/// each value in order and returns 0. A value with a MySQL type that a plain
/// value does not carry (`DECIMAL`, temporal, `ENUM`, ...) is refused, as
/// reading it back would lose that type.
pub(crate) fn execute(
    connection: &Connection,
    name: &str,
    args: &[Value],
    arg_types: Option<&[ArgumentType]>,
) -> Option<Result<Value>> {
    let result = match name {
        USER_VARIABLE => session(connection).and_then(|session| session.user_variable(args, true)),
        USER_VARIABLE_VALUE => {
            session(connection).and_then(|session| session.user_variable(args, false))
        }
        SET_USER_VARIABLES => {
            let Some(arg_types) = arg_types else {
                return Some(Err(LimboError::InternalError(
                    "user variables are set with the types of their values".to_string(),
                )));
            };
            session(connection).and_then(|session| session.set_user_variables(args, arg_types))
        }
        KEEP_SESSION_SETTINGS => keep_session_settings(args),
        ONE_JOINED_ROW => one_joined_row(args),
        _ => return None,
    };
    Some(result)
}

/// `__mysql_keep_session_settings(variable, value, ...)`: `SET variable =
/// expression` of a setting, accepted when the value is the target profile's
/// and refused otherwise.
fn keep_session_settings(args: &[Value]) -> Result<Value> {
    for pair in args.chunks_exact(2) {
        let [Value::Text(variable), value] = pair else {
            return Err(LimboError::InternalError(
                "a setting is checked by its name".to_string(),
            ));
        };
        let keeps = match value {
            Value::Text(text) => {
                turso_mysql_parser::keeps_target_profile(variable.as_str(), text.as_str())
            }
            _ => false,
        };
        if !keeps {
            return Err(unsupported_value(format!(
                "SET {} = {value}, which changes the session from the target profile,",
                variable.as_str()
            )));
        }
    }
    Ok(Value::from_i64(0))
}

/// `__mysql_one_joined_row(matches)`: true when one joined row matches a row
/// that a multi-table UPDATE changes. MySQL takes the new values from the
/// first joined row it meets, which Turso does not know, so more are refused.
fn one_joined_row(args: &[Value]) -> Result<Value> {
    match args {
        [Value::Numeric(Numeric::Integer(1))] => Ok(Value::from_i64(1)),
        [Value::Numeric(Numeric::Integer(matches))] => Err(unsupported_value(format!(
            "a multi-table UPDATE of a row that {matches} joined rows match, which MySQL changes with the values of the first it meets,"
        ))),
        _ => Err(LimboError::InternalError(
            "the joined rows of an updated row are counted".to_string(),
        )),
    }
}

const ONE_JOINED_ROW: &str = "__MYSQL_ONE_JOINED_ROW";

const KEEP_SESSION_SETTINGS: &str = "__MYSQL_KEEP_SESSION_SETTINGS";

const USER_VARIABLE: &str = "__MYSQL_USER_VARIABLE";

const USER_VARIABLE_VALUE: &str = "__MYSQL_USER_VARIABLE_VALUE";

const SET_USER_VARIABLES: &str = "__MYSQL_SET_USER_VARIABLES";

fn session(connection: &Connection) -> Result<&Session> {
    connection.context::<Session>().ok_or_else(|| {
        LimboError::InvalidArgument(
            "user variables need a connection opened by turso_mysql".to_string(),
        )
    })
}

impl Session {
    fn user_variable(&self, args: &[Value], compared: bool) -> Result<Value> {
        let [Value::Text(name)] = args else {
            return Err(LimboError::InternalError(
                "a user variable is read by its name".to_string(),
            ));
        };
        let Some(variable) = self
            .user_variables
            .lock()
            .expect("the user variables are not poisoned")
            .get(name.as_str())
            .cloned()
        else {
            return Ok(Value::Null);
        };
        if compared && matches!(variable.value, Value::Text(_)) && !variable.default_collation {
            return Err(unsupported_value(format!(
                "`@{}`, a string whose collation MySQL derives from the expression it was set to,",
                name.as_str()
            )));
        }
        Ok(variable.value)
    }

    fn set_user_variables(&self, args: &[Value], arg_types: &[ArgumentType]) -> Result<Value> {
        let mut assigned = Vec::with_capacity(args.len() / 3);
        for (triple, types) in args.chunks_exact(3).zip(arg_types.chunks_exact(3)) {
            let [Value::Text(name), Value::Numeric(Numeric::Integer(default_collation)), value] =
                triple
            else {
                return Err(LimboError::InternalError(
                    "a user variable is set by its name".to_string(),
                ));
            };
            let variable = UserVariable {
                value: user_value(value, &types[2])?,
                default_collation: *default_collation != 0,
            };
            assigned.push((name.as_str().to_string(), variable));
        }
        self.user_variables
            .lock()
            .expect("the user variables are not poisoned")
            .extend(assigned);
        Ok(Value::from_i64(0))
    }
}

fn user_value(value: &Value, value_type: &ArgumentType) -> Result<Value> {
    if let ArgumentType::Named { name, .. } = value_type {
        return Err(unsupported_value(format!(
            "a user variable assigned a `{name}` value"
        )));
    }
    Ok(match value {
        Value::Text(text) if text.subtype == TextSubtype::Json => {
            Value::build_text(text.as_str().to_string())
        }
        other => other.clone(),
    })
}
