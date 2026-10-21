//! Embedded API behavior the `.sqltest` specs in
//! `mysql/conformance/my-sqltests` cannot express: result metadata,
//! parameter binding, persistence across reopen, and connection recovery.

use std::sync::Arc;
use turso_core::{MemoryIO, Value, IO};
use turso_mysql::{open_memory, open_with_io};

#[test]
fn persisted_create_table_reloads_through_mysql_dialect() -> turso_core::Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-create-table-persistence.db";

    {
        let connection = open_with_io(io.clone(), path)?;
        connection.execute(
            "CREATE TABLE `select` (`id` INTEGER PRIMARY KEY, `display``name` VARCHAR(100))",
        )?;
        connection.close()?;
    }

    {
        let connection = open_with_io(io, path)?;
        let duplicate = connection
            .execute(
                "CREATE TABLE `select` (`id` INTEGER PRIMARY KEY, `display``name` VARCHAR(100))",
            )
            .expect_err("reopened schema must contain the persisted table");
        assert!(duplicate.to_string().contains("already exists"));
        connection.execute(
            "CREATE TABLE IF NOT EXISTS `select` (`id` INTEGER PRIMARY KEY, `display``name` VARCHAR(100))",
        )?;
        connection.close()?;
    }
    Ok(())
}

#[test]
fn reports_projection_metadata() -> turso_core::Result<()> {
    let connection = open_memory()?;
    let statement =
        connection.prepare("SELECT 1 AS answer, 'Ada' AS `display name`, NULL AS missing")?;
    assert_eq!(statement.num_columns(), 3);
    assert_eq!(statement.get_column_name(0), "answer");
    assert_eq!(statement.get_column_name(1), "display name");
    assert_eq!(statement.get_column_name(2), "missing");
    connection.close()?;
    Ok(())
}

#[test]
fn executes_bound_anonymous_projection_parameters() -> turso_core::Result<()> {
    let connection = open_memory()?;
    let mut statement = connection.prepare("SELECT 1, ?, 'Ada', ? AS named, ?")?;
    assert_eq!(statement.parameters_count(), 3);
    assert_eq!(statement.num_columns(), 5);
    assert_eq!(statement.get_column_name(1), "?");
    assert_eq!(statement.get_column_name(3), "named");
    assert_eq!(statement.get_column_name(4), "?");

    statement.bind_at(1.try_into().unwrap(), Value::from_i64(42))?;
    statement.bind_at(2.try_into().unwrap(), Value::build_text("Grüße 世界 🦀"))?;
    statement.bind_at(3.try_into().unwrap(), Value::Null)?;
    let rows = statement.run_collect_rows()?;
    assert_eq!(
        rows,
        vec![vec![
            Value::from_i64(1),
            Value::from_i64(42),
            Value::build_text("Ada"),
            Value::build_text("Grüße 世界 🦀"),
            Value::Null,
        ]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn resets_and_rebinds_anonymous_projection_parameters() -> turso_core::Result<()> {
    let connection = open_memory()?;
    let mut statement = connection.prepare("SELECT ?, ?, ?")?;

    statement.bind_at(1.try_into().unwrap(), Value::build_text("stale text"))?;
    statement.bind_at(2.try_into().unwrap(), Value::from_i64(7))?;
    statement.bind_at(3.try_into().unwrap(), Value::Null)?;
    assert_eq!(
        statement.run_collect_rows()?,
        vec![vec![
            Value::build_text("stale text"),
            Value::from_i64(7),
            Value::Null,
        ]]
    );

    statement.reset()?;
    statement.bind_at(1.try_into().unwrap(), Value::build_text("fresh"))?;
    statement.bind_at(2.try_into().unwrap(), Value::Null)?;
    statement.bind_at(3.try_into().unwrap(), Value::from_i64(9))?;
    assert_eq!(
        statement.run_collect_rows()?,
        vec![vec![
            Value::build_text("fresh"),
            Value::Null,
            Value::from_i64(9),
        ]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn connection_remains_usable_after_a_prepare_error() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection
        .prepare("SELECT 1 FROM unsupported_table")
        .expect_err("unsupported statement must fail");
    let mut statement = connection.prepare("SELECT 'still usable'")?;
    let rows = statement.run_collect_rows()?;
    assert_eq!(rows, vec![vec![Value::build_text("still usable")]]);
    connection.close()?;
    Ok(())
}
