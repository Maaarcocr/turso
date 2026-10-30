use crate::types::unsupported_value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};
use turso_core::dialect::ArgumentType;
use turso_core::types::TextSubtype;
use turso_core::{Connection, Func, LimboError, Numeric, Result, Value};

/// The state of a MySQL session that outlives its statements, kept as the
/// context of the connection: its connection id and user variables. The
/// user-level locks it holds are released when it closes.
pub(crate) struct Session {
    id: u64,
    connection: OnceLock<Weak<Connection>>,
    user_variables: Mutex<HashMap<String, UserVariable>>,
    prepared_statements: Mutex<HashMap<String, String>>,
    transaction_isolation: Mutex<&'static str>,
}

impl Default for Session {
    fn default() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            connection: OnceLock::new(),
            user_variables: Mutex::default(),
            prepared_statements: Mutex::default(),
            transaction_isolation: Mutex::new("REPEATABLE-READ"),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        user_locks().retain(|_, holder| holder.session != self.id);
    }
}

/// A user-level lock (`GET_LOCK`), held by one session as many times as it
/// took it. Like MySQL's, which belong to the server, these belong to the
/// process and are shared by every session in it.
struct LockHolder {
    session: u64,
    count: u64,
}

static USER_LOCKS: LazyLock<Mutex<HashMap<String, LockHolder>>> = LazyLock::new(Mutex::default);

fn user_locks() -> std::sync::MutexGuard<'static, HashMap<String, LockHolder>> {
    USER_LOCKS
        .lock()
        .expect("the user-level locks are not poisoned")
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
        "GET_LOCK" => arg_count == 2,
        "RELEASE_LOCK" | "IS_FREE_LOCK" | "IS_USED_LOCK" => arg_count == 1,
        "RELEASE_ALL_LOCKS" | "CONNECTION_ID" | TRANSACTION_ISOLATION => arg_count == 0,
        SET_TRANSACTION_ISOLATION => arg_count == 1,
        PREPARE => arg_count == 2,
        LIMIT_PARAMETER | RECURSION_DEPTH | RECURSIVE_INTEGER | VALIDATE_PASSWORD_STRENGTH => {
            arg_count == 1
        }
        DEALLOCATE_PREPARE => arg_count == 1,
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
        "GET_LOCK" => session(connection).and_then(|session| session.get_lock(args)),
        "RELEASE_LOCK" => session(connection).and_then(|session| session.release_lock(args)),
        "IS_FREE_LOCK" => lock_name(&args[0])
            .map(|name| Value::from_i64(i64::from(!user_locks().contains_key(&name)))),
        "IS_USED_LOCK" => lock_name(&args[0]).map(|name| {
            user_locks()
                .get(&name)
                .map_or(Value::Null, |holder| connection_id(holder.session))
        }),
        "RELEASE_ALL_LOCKS" => session(connection).map(Session::release_all_locks),
        "CONNECTION_ID" => session(connection).map(|session| connection_id(session.id)),
        TRANSACTION_ISOLATION => session(connection).map(|session| {
            Value::build_text(
                *session
                    .transaction_isolation
                    .lock()
                    .expect("the isolation level is not poisoned"),
            )
        }),
        SET_TRANSACTION_ISOLATION => {
            session(connection).and_then(|session| session.set_transaction_isolation(args))
        }
        PREPARE => session(connection).and_then(|session| session.prepare(connection, args)),
        LIMIT_PARAMETER => match &args[0] {
            Value::Numeric(Numeric::Integer(count)) if *count >= 0 => Ok(args[0].clone()),
            other => Err(unsupported_value(format!(
                "`{other}` as a LIMIT or OFFSET parameter, which is not a count,"
            ))),
        },
        DEALLOCATE_PREPARE => {
            session(connection).and_then(|session| session.deallocate_prepare(connection, args))
        }
        RECURSION_DEPTH => match &args[0] {
            Value::Numeric(Numeric::Integer(depth)) if (0..MAX_RECURSION_DEPTH - 1).contains(depth) => {
                Ok(Value::from_i64(depth + 1))
            }
            Value::Numeric(Numeric::Integer(depth)) if *depth >= MAX_RECURSION_DEPTH - 1 => {
                Err(unsupported_value(format!(
                    "a recursive common table expression that runs {MAX_RECURSION_DEPTH} iterations, near MySQL's cte_max_recursion_depth,"
                )))
            }
            other => Err(LimboError::InternalError(format!(
                "a recursion depth is a count, not {other}"
            ))),
        },
        VALIDATE_PASSWORD_STRENGTH => match &args[0] {
            Value::Null => Err(unsupported_value(
                "VALIDATE_PASSWORD_STRENGTH(NULL)".to_string(),
            )),
            _ => Ok(Value::from_i64(0)),
        },
        RECURSIVE_INTEGER => match &args[0] {
            Value::Null => Ok(Value::Null),
            Value::Numeric(Numeric::Integer(value)) if i32::try_from(*value).is_ok() => {
                Ok(args[0].clone())
            }
            other => Err(unsupported_value(format!(
                "`{other}` in a column of a recursive common table expression whose anchor makes it an INT,"
            ))),
        },
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

/// `__mysql_transaction_isolation()`: the session's
/// `@@transaction_isolation`; `__mysql_set_transaction_isolation(level)`,
/// from `SET SESSION TRANSACTION ISOLATION LEVEL`, stores it and returns 0.
const TRANSACTION_ISOLATION: &str = "__MYSQL_TRANSACTION_ISOLATION";

const SET_TRANSACTION_ISOLATION: &str = "__MYSQL_SET_TRANSACTION_ISOLATION";

const KEEP_SESSION_SETTINGS: &str = "__MYSQL_KEEP_SESSION_SETTINGS";

const USER_VARIABLE: &str = "__MYSQL_USER_VARIABLE";

const USER_VARIABLE_VALUE: &str = "__MYSQL_USER_VARIABLE_VALUE";

const SET_USER_VARIABLES: &str = "__MYSQL_SET_USER_VARIABLES";

/// `__mysql_prepare(name, text)`, from `PREPARE name FROM text`: drops the
/// session's statement `name`, checks `text` as MySQL prepares it and keeps
/// it as `name`; `__mysql_deallocate_prepare(name)` drops it. Both make the
/// connection prepare again the statements it prepared, whose `EXECUTE` may
/// have read the dropped text.
const PREPARE: &str = "__MYSQL_PREPARE";

const DEALLOCATE_PREPARE: &str = "__MYSQL_DEALLOCATE_PREPARE";

/// `__mysql_limit_parameter(value)`: a user variable as the `LIMIT` or
/// `OFFSET` of an `EXECUTE`d statement, which must hold a count; other
/// values are refused.
const LIMIT_PARAMETER: &str = "__MYSQL_LIMIT_PARAMETER";

/// `__mysql_recursion_depth(depth)`: the iteration of a recursive row whose
/// source row is of iteration `depth`, refused from the iteration where
/// MySQL's `cte_max_recursion_depth` (1000) may stop the query.
const RECURSION_DEPTH: &str = "__MYSQL_RECURSION_DEPTH";

const MAX_RECURSION_DEPTH: i64 = 1000;

/// `VALIDATE_PASSWORD_STRENGTH(password)` is 0 for every password in the
/// target profile, which does not install MySQL's `validate_password`
/// component. A NULL argument is refused.
const VALIDATE_PASSWORD_STRENGTH: &str = "VALIDATE_PASSWORD_STRENGTH";

/// `__mysql_recursive_integer(value)`: a recursive value of a column whose
/// anchor is an `INT` literal, refused outside `INT` or when not an integer.
const RECURSIVE_INTEGER: &str = "__MYSQL_RECURSIVE_INTEGER";

/// The key of a user-level lock: MySQL compares lock names without case and
/// rejects a `NULL`, empty or longer than 64 characters name (3057). A
/// number names the lock by its text; other values, and names that are not
/// ASCII, whose case folding is MySQL's, are refused.
fn lock_name(name: &Value) -> Result<String> {
    let wrong_name = |name: &str| {
        LimboError::SqlError(format!(
            "ERROR 3057 (42000): Incorrect user-level lock name '{name}'."
        ))
    };
    let name = match name {
        Value::Null => return Err(wrong_name("NULL")),
        Value::Text(text) if text.subtype != TextSubtype::Json => text.as_str().to_string(),
        Value::Numeric(Numeric::Integer(number)) => number.to_string(),
        other => {
            return Err(unsupported_value(format!(
                "a user-level lock named by `{other}`"
            )))
        }
    };
    if !name.is_ascii() {
        return Err(unsupported_value(format!(
            "a user-level lock named `{name}`, which is not ASCII,"
        )));
    }
    if name.is_empty() || name.len() > 64 {
        return Err(wrong_name(&name));
    }
    Ok(name.to_ascii_lowercase())
}

fn connection_id(session: u64) -> Value {
    Value::from_i64(i64::try_from(session).expect("connection ids fit a BIGINT"))
}

fn session(connection: &Connection) -> Result<&Session> {
    connection.context::<Session>().ok_or_else(|| {
        LimboError::InvalidArgument(
            "user variables need a connection opened by turso_mysql".to_string(),
        )
    })
}

impl Session {
    /// `GET_LOCK(name, timeout)`: 1 when this session takes the lock, also
    /// again; 0 when another session holds it and `timeout` is 0. Waiting for
    /// another session to release it is refused.
    fn get_lock(&self, args: &[Value]) -> Result<Value> {
        let name = lock_name(&args[0])?;
        let mut locks = user_locks();
        match locks.get_mut(&name) {
            None => {
                locks.insert(
                    name,
                    LockHolder {
                        session: self.id,
                        count: 1,
                    },
                );
                Ok(Value::from_i64(1))
            }
            Some(holder) if holder.session == self.id => {
                holder.count += 1;
                Ok(Value::from_i64(1))
            }
            Some(_) if args[1] == Value::from_i64(0) => Ok(Value::from_i64(0)),
            Some(_) => Err(unsupported_value(format!(
                "GET_LOCK('{name}', {}), which waits for another session to release the lock,",
                args[1]
            ))),
        }
    }

    /// `RELEASE_LOCK(name)`: 1 when this session held the lock, which it
    /// then holds once less; 0 when another session holds it; NULL when no
    /// session does.
    fn release_lock(&self, args: &[Value]) -> Result<Value> {
        let name = lock_name(&args[0])?;
        let mut locks = user_locks();
        let Some(holder) = locks.get_mut(&name) else {
            return Ok(Value::Null);
        };
        if holder.session != self.id {
            return Ok(Value::from_i64(0));
        }
        holder.count -= 1;
        if holder.count == 0 {
            locks.remove(&name);
        }
        Ok(Value::from_i64(1))
    }

    /// `RELEASE_ALL_LOCKS()`: releases every lock this session holds and
    /// returns how many times it held them.
    fn release_all_locks(&self) -> Value {
        let mut released = 0;
        user_locks().retain(|_, holder| {
            let own = holder.session == self.id;
            if own {
                released += holder.count;
            }
            !own
        });
        Value::from_i64(i64::try_from(released).expect("lock counts fit a BIGINT"))
    }

    /// Keeps `connection`, whose context this session is, to compile the
    /// statements it prepares.
    pub(crate) fn attach(&self, connection: &Arc<Connection>) {
        let attached = self.connection.set(Arc::downgrade(connection));
        assert!(attached.is_ok(), "a session belongs to one connection");
    }

    pub(crate) fn prepared_statement(&self, name: &str) -> Option<String> {
        self.prepared_statements
            .lock()
            .expect("the prepared statements are not poisoned")
            .get(name)
            .cloned()
    }

    fn prepare(&self, connection: &Connection, args: &[Value]) -> Result<Value> {
        let [Value::Text(name), text] = args else {
            return Err(LimboError::InternalError(
                "a statement is prepared by its name".to_string(),
            ));
        };
        let name = name.as_str().to_string();
        self.prepared_statements
            .lock()
            .expect("the prepared statements are not poisoned")
            .remove(&name);
        connection.invalidate_prepared_statements();
        let text = match text {
            Value::Text(text) => text.as_str().to_string(),
            Value::Null => {
                return Err(LimboError::SqlError(
                    "ERROR 1064 (42000): You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'NULL' at line 1".to_string(),
                ))
            }
            other => {
                return Err(unsupported_value(format!(
                    "PREPARE from `{other}`, which is not a string,"
                )))
            }
        };
        let lowered = turso_mysql_parser::lower_prepared(&text, &crate::Session(connection))
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let connection = self
            .connection
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| {
                LimboError::InternalError("a session prepares on its connection".to_string())
            })?;
        drop(connection.prepare_translated_stmt(lowered, &text)?);
        self.prepared_statements
            .lock()
            .expect("the prepared statements are not poisoned")
            .insert(name, text);
        Ok(Value::from_i64(0))
    }

    fn deallocate_prepare(&self, connection: &Connection, args: &[Value]) -> Result<Value> {
        let [Value::Text(name)] = args else {
            return Err(LimboError::InternalError(
                "a prepared statement is dropped by its name".to_string(),
            ));
        };
        let dropped = self
            .prepared_statements
            .lock()
            .expect("the prepared statements are not poisoned")
            .remove(name.as_str());
        connection.invalidate_prepared_statements();
        match dropped {
            Some(_) => Ok(Value::from_i64(0)),
            None => Err(LimboError::SqlError(format!(
                "ERROR 1243 (HY000): Unknown prepared statement handler ({}) given to DEALLOCATE PREPARE",
                name.as_str()
            ))),
        }
    }

    fn set_transaction_isolation(&self, args: &[Value]) -> Result<Value> {
        let level = match args {
            [Value::Text(level)] => match level.as_str() {
                "READ-UNCOMMITTED" => "READ-UNCOMMITTED",
                "READ-COMMITTED" => "READ-COMMITTED",
                "REPEATABLE-READ" => "REPEATABLE-READ",
                "SERIALIZABLE" => "SERIALIZABLE",
                _ => None.ok_or_else(|| {
                    LimboError::InternalError(format!("no isolation level {}", level.as_str()))
                })?,
            },
            _ => {
                return Err(LimboError::InternalError(
                    "an isolation level is set by its name".to_string(),
                ))
            }
        };
        *self
            .transaction_isolation
            .lock()
            .expect("the isolation level is not poisoned") = level;
        Ok(Value::from_i64(0))
    }

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
