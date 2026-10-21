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
fn persisted_mysql_ddl_keeps_column_rules_after_reopen() -> turso_core::Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-ddl-persistence.db";

    {
        let connection = open_with_io(io.clone(), path)?;
        connection.execute(
            "CREATE TABLE items (
                id INT UNSIGNED NOT NULL AUTO_INCREMENT COMMENT 'key',
                code VARCHAR(3) NOT NULL,
                size ENUM('small', 'large') NOT NULL,
                added DATE DEFAULT '2020-1-2',
                PRIMARY KEY (id),
                UNIQUE KEY uq_code (code),
                KEY idx_size (size)
            ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
        )?;
        connection.execute("CREATE INDEX idx_added USING BTREE ON items (added)")?;
        connection.execute("ALTER TABLE items ADD COLUMN qty TINYINT UNSIGNED DEFAULT 1")?;
        connection.execute("INSERT INTO items (code) VALUES ('a'), ('b'), ('c')")?;
        connection.execute("DELETE FROM items WHERE id = 3")?;
        connection.close()?;
    }

    {
        let connection = open_with_io(io, path)?;
        connection.execute("INSERT INTO items (code, size) VALUES ('d', 'large')")?;
        let mut statement =
            connection.prepare("SELECT id, code, size, added, qty FROM items ORDER BY id")?;
        assert_eq!(
            statement.run_collect_rows()?,
            vec![
                item_row(1, "a", "small"),
                item_row(2, "b", "small"),
                vec![
                    Value::from_i64(4),
                    Value::build_text("d"),
                    Value::build_text("large"),
                    Value::build_text("2020-01-02"),
                    Value::from_i64(1),
                ],
            ]
        );
        for rejected in [
            "INSERT INTO items (code) VALUES ('long')",
            "INSERT INTO items (code) VALUES ('a')",
            "INSERT INTO items (code, size) VALUES ('e', 'huge')",
            "INSERT INTO items (code, qty) VALUES ('f', 256)",
            "INSERT INTO items (id, code) VALUES (-1, 'g')",
        ] {
            connection
                .execute(rejected)
                .expect_err("reopened table must keep its MySQL column rules");
        }
        connection.execute("DROP INDEX idx_added ON items")?;
        connection.execute("DROP TABLE items")?;
        connection.close()?;
    }
    Ok(())
}

fn item_row(id: i64, code: &'static str, size: &'static str) -> Vec<Value> {
    vec![
        Value::from_i64(id),
        Value::build_text(code),
        Value::build_text(size),
        Value::build_text("2020-01-02"),
        Value::from_i64(1),
    ]
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
