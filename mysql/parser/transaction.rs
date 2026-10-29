use crate::expr::{function_call, string_expr};
use crate::names::translate_identifier;
use crate::session::{no_op, statement_with_effect};
use crate::MysqlParserError;
use sqlparser::ast::{Ident, Statement, TransactionIsolationLevel, TransactionMode};
use turso_parser::ast;

/// `BEGIN`, `START TRANSACTION`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`,
/// `ROLLBACK TO SAVEPOINT` and `RELEASE SAVEPOINT` in a session with
/// `autocommit = 1`. Outside a transaction `COMMIT`, `ROLLBACK` and
/// `SAVEPOINT` do nothing, as in MySQL, and a savepoint cannot be rolled back
/// to or released (1305). Inside one, starting another transaction, which
/// MySQL does after committing the open one, is refused.
///
/// A transaction starts as `BEGIN IMMEDIATE`, which takes the database's
/// write lock at once, so transactions of different connections run one
/// after the other. Their reads then see the latest committed rows, as
/// MySQL's locking reads (`FOR UPDATE`) and `READ COMMITTED` do, and every
/// isolation level gives the result of running the transactions in order.
pub(crate) fn translate_transaction(
    statement: &Statement,
    in_transaction: bool,
) -> Result<ast::Stmt, MysqlParserError> {
    match statement {
        Statement::StartTransaction {
            modes,
            begin: _,
            transaction: _,
            modifier: None,
            statements,
            exception: None,
            has_end_keyword: false,
        } if modes.is_empty() && statements.is_empty() => {
            if in_transaction {
                return Err(MysqlParserError::Unsupported(format!(
                    "`{statement}` inside a transaction, which MySQL commits before starting another, is not lowered yet"
                )));
            }
            Ok(ast::Stmt::Begin {
                typ: Some(ast::TransactionType::Immediate),
                name: None,
            })
        }
        Statement::Commit {
            chain: false,
            end: false,
            modifier: None,
        } => Ok(if in_transaction {
            ast::Stmt::Commit { name: None }
        } else {
            no_op()
        }),
        Statement::Rollback {
            chain: false,
            savepoint: None,
        } => Ok(if in_transaction {
            ast::Stmt::Rollback {
                tx_name: None,
                savepoint_name: None,
            }
        } else {
            no_op()
        }),
        Statement::Rollback {
            chain: false,
            savepoint: Some(name),
        } => {
            let savepoint_name = Some(savepoint(name, in_transaction)?);
            Ok(ast::Stmt::Rollback {
                tx_name: None,
                savepoint_name,
            })
        }
        Statement::Savepoint { name } if in_transaction => Ok(ast::Stmt::Savepoint {
            name: translate_identifier(name)?,
        }),
        Statement::Savepoint { .. } => Ok(no_op()),
        Statement::ReleaseSavepoint { name } => Ok(ast::Stmt::Release {
            name: savepoint(name, in_transaction)?,
        }),
        _ => Err(MysqlParserError::Unsupported(format!(
            "transaction statement `{statement}` is not lowered yet"
        ))),
    }
}

/// `SET SESSION TRANSACTION ISOLATION LEVEL level` (`scope` is the word
/// after `SET`): the session's `@@transaction_isolation`, which
/// `__mysql_set_transaction_isolation` stores. Since transactions run one
/// after the other, every level behaves as MySQL's would. The form without
/// `SESSION`, which applies to the next transaction only, `GLOBAL` and access
/// modes are refused.
pub(crate) fn translate_set_transaction(
    modes: &[TransactionMode],
    scope: Option<&str>,
    statement: &Statement,
) -> Result<ast::Stmt, MysqlParserError> {
    let level = match (scope, modes) {
        (Some("SESSION" | "LOCAL"), [TransactionMode::IsolationLevel(level)]) => match level {
            TransactionIsolationLevel::ReadUncommitted => "READ-UNCOMMITTED",
            TransactionIsolationLevel::ReadCommitted => "READ-COMMITTED",
            TransactionIsolationLevel::RepeatableRead => "REPEATABLE-READ",
            TransactionIsolationLevel::Serializable => "SERIALIZABLE",
            TransactionIsolationLevel::Snapshot => {
                return Err(MysqlParserError::Rejected(format!(
                    "`{statement}` names an isolation level MySQL does not have"
                )))
            }
        },
        _ => {
            return Err(MysqlParserError::Unsupported(format!(
            "`{statement}` other than SET SESSION TRANSACTION ISOLATION LEVEL is not lowered yet"
        )))
        }
    };
    Ok(statement_with_effect(function_call(
        "__mysql_set_transaction_isolation",
        vec![string_expr(level)],
    )))
}

fn savepoint(name: &Ident, in_transaction: bool) -> Result<ast::Name, MysqlParserError> {
    if !in_transaction {
        return Err(MysqlParserError::Rejected(format!(
            "SAVEPOINT {name} does not exist"
        )));
    }
    translate_identifier(name)
}
