use super::{sqlite, Dialect, SqliteDialect, POSTGRES_TABLE_SQL_PREFIX};
use crate::schema::BTreeTable;
use crate::{LimboError, Result};
use turso_parser::ast;

pub const TABLE_MARKER_PREFIX: &str = "/* turso_dialect:";
const TABLE_MARKER_END: &str = "*/";
const ROWID_ALIAS_WORD: &str = "rowid_alias";
const POSTGRES_DIALECT_NAME: &str = "postgres";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableMarker {
    pub dialect: String,
    pub rowid_alias: bool,
}

impl TableMarker {
    pub fn new(dialect: &str, rowid_alias: bool) -> Self {
        Self {
            dialect: dialect.to_string(),
            rowid_alias,
        }
    }

    pub fn read(sql: &str) -> Result<Option<(TableMarker, &str)>> {
        let Some(after_prefix) = sql.strip_prefix(TABLE_MARKER_PREFIX) else {
            return Ok(None);
        };
        let Some(end) = after_prefix.find(TABLE_MARKER_END) else {
            return Err(LimboError::Corrupt(format!(
                "malformed database schema: table marker is not closed: {sql}"
            )));
        };
        let mut words = after_prefix[..end].split_ascii_whitespace();
        let Some(dialect) = words.next() else {
            return Err(LimboError::Corrupt(format!(
                "malformed database schema: table marker has no dialect name: {sql}"
            )));
        };
        let mut rowid_alias = false;
        for word in words {
            if word != ROWID_ALIAS_WORD || rowid_alias {
                return Err(LimboError::Corrupt(format!(
                    "malformed database schema: unknown word {word:?} in table marker: {sql}"
                )));
            }
            rowid_alias = true;
        }
        let table_sql = after_prefix[end + TABLE_MARKER_END.len()..].trim_start();
        Ok(Some((TableMarker::new(dialect, rowid_alias), table_sql)))
    }

    pub fn write(&self, table_sql: &str) -> String {
        let rowid_alias = if self.rowid_alias { " rowid_alias" } else { "" };
        format!(
            "{TABLE_MARKER_PREFIX}{}{rowid_alias} {TABLE_MARKER_END} {table_sql}",
            self.dialect
        )
    }
}

pub(crate) fn parse_statement(
    dialect: &dyn Dialect,
    sql: &str,
) -> Result<(Option<ast::Cmd>, usize)> {
    let statement_sql = sql.trim_start();
    let Some((marker, table_sql)) = TableMarker::read(statement_sql)? else {
        return dialect.parse(sql);
    };
    if !dialect_writes_marker(dialect, &marker) {
        crate::bail_parse_error!(
            "a table marked for the {} dialect cannot be created in the {} dialect",
            marker.dialect,
            dialect.name()
        );
    }
    let (cmd, consumed) = sqlite::parse(table_sql)?;
    if !matches!(cmd, Some(ast::Cmd::Stmt(ast::Stmt::CreateTable { .. }))) {
        crate::bail_parse_error!("a table marker must be followed by CREATE TABLE");
    }
    Ok((cmd, sql.len() - table_sql.len() + consumed))
}

pub(crate) fn parse_stored_table(
    dialect: &dyn Dialect,
    sql: &str,
    root_page: i64,
) -> Result<BTreeTable> {
    match stored_table_marker(dialect, sql)? {
        Some((marker, table_sql)) => BTreeTable::from_marked_sql(table_sql, root_page, marker),
        None => dialect.parse_table_sql(sql, root_page),
    }
}

pub(crate) fn parse_stored_table_ast(
    dialect: &dyn Dialect,
    sql: &str,
) -> Result<(ast::Stmt, Option<TableMarker>)> {
    match stored_table_marker(dialect, sql)? {
        Some((marker, table_sql)) => Ok((sqlite::parse_table_sql_ast(table_sql)?, Some(marker))),
        None => Ok((dialect.parse_table_sql_ast(sql)?, None)),
    }
}

pub(crate) fn stored_table_sql_for_replay(dialect: &dyn Dialect, sql: &str) -> Result<String> {
    match stored_table_marker(dialect, sql)? {
        Some((marker, table_sql)) => Ok(marker.write(&sqlite::table_sql_for_replay(table_sql)?)),
        None => dialect.table_sql_for_replay(sql),
    }
}

pub(crate) fn format_rewritten_table_sql(
    dialect: &dyn Dialect,
    stmt: &ast::Stmt,
    marker: Option<&TableMarker>,
) -> Result<String> {
    match marker {
        Some(marker) => Ok(marker.write(&SqliteDialect.format_rewritten_table_sql(stmt)?)),
        None => dialect.format_rewritten_table_sql(stmt),
    }
}

fn stored_table_marker<'a>(
    dialect: &dyn Dialect,
    sql: &'a str,
) -> Result<Option<(TableMarker, &'a str)>> {
    if sql.starts_with(POSTGRES_TABLE_SQL_PREFIX) && dialect.name() != POSTGRES_DIALECT_NAME {
        return Err(LimboError::Corrupt(format!(
            "malformed database schema: table was created by the postgres frontend: {sql}"
        )));
    }
    let Some((marker, table_sql)) = TableMarker::read(sql)? else {
        return Ok(None);
    };
    if !dialect_writes_marker(dialect, &marker) {
        return Err(LimboError::Corrupt(format!(
            "malformed database schema: table was created by the {} dialect: {sql}",
            marker.dialect
        )));
    }
    Ok(Some((marker, table_sql)))
}

fn dialect_writes_marker(dialect: &dyn Dialect, marker: &TableMarker) -> bool {
    dialect.marks_tables() && marker.dialect == dialect.name()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::database::DatabaseFile;
    use crate::{ConnectOpts, Connection, Database, MemoryIO, OpenFlags, Value, IO};
    use std::sync::Arc;

    struct MarkingDialect {
        name: &'static str,
    }

    impl Dialect for MarkingDialect {
        fn name(&self) -> &'static str {
            self.name
        }

        fn parse(&self, sql: &str) -> Result<(Option<ast::Cmd>, usize)> {
            if sql.trim_start().starts_with("/*") {
                crate::bail_parse_error!("the marking dialect's parser does not read comments");
            }
            sqlite::parse(sql)
        }

        fn parse_table_sql(&self, sql: &str, root_page: i64) -> Result<BTreeTable> {
            BTreeTable::from_sql(sql, root_page)
        }

        fn parse_table_sql_ast(&self, sql: &str) -> Result<ast::Stmt> {
            sqlite::parse_table_sql_ast(sql)
        }

        fn table_sql_for_replay(&self, sql: &str) -> Result<String> {
            sqlite::table_sql_for_replay(sql)
        }

        fn format_table_sql(
            &self,
            input: &str,
            tbl_name: &ast::QualifiedName,
            body: &ast::CreateTableBody,
        ) -> Result<String> {
            SqliteDialect.format_table_sql(input, tbl_name, body)
        }

        fn register_catalog(
            &self,
            schema: &mut crate::schema::Schema,
            enable_custom_types: bool,
        ) -> Result<()> {
            sqlite::register_builtin_catalog(schema, enable_custom_types)
        }

        fn resolve_function(
            &self,
            name: &str,
            arg_count: usize,
        ) -> Result<Option<crate::function::Func>> {
            sqlite::resolve_builtin_function(name, arg_count)
        }

        fn marks_tables(&self) -> bool {
            true
        }
    }

    fn marking() -> Arc<dyn Dialect> {
        Arc::new(MarkingDialect { name: "marking" })
    }

    fn open_db(io: &Arc<dyn IO>, path: &str, dialect: Arc<dyn Dialect>) -> Result<Arc<Database>> {
        let file = io.open_file(path, OpenFlags::Create, true)?;
        let db_file = Arc::new(DatabaseFile::new(file));
        Database::open(
            io.clone(),
            path,
            crate::OpenOptions::new(dialect).storage(db_file),
        )
    }

    fn query(conn: &Arc<Connection>, sql: &str) -> Vec<Vec<Value>> {
        conn.prepare(sql).unwrap().run_collect_rows().unwrap()
    }

    fn stored_sql(conn: &Arc<Connection>, name: &str) -> String {
        let rows = query(
            conn,
            &format!("SELECT sql FROM sqlite_schema WHERE name = '{name}'"),
        );
        assert_eq!(rows.len(), 1, "no schema row for {name}");
        rows[0][0].to_string().trim_matches('\'').to_string()
    }

    fn rowid_and_id(conn: &Arc<Connection>, table: &str) -> Vec<Vec<Value>> {
        query(conn, &format!("SELECT rowid, id FROM {table} ORDER BY id"))
    }

    fn ints(rows: &[&[i64]]) -> Vec<Vec<Value>> {
        rows.iter()
            .map(|row| row.iter().map(|v| Value::from_i64(*v)).collect())
            .collect()
    }

    fn create_keyed_tables(conn: &Arc<Connection>) {
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT) STRICT")
            .unwrap();
        conn.execute("CREATE TABLE u(id INT PRIMARY KEY, a INT) STRICT")
            .unwrap();
        conn.execute(
            "/* turso_dialect:marking */ CREATE TABLE w(id INTEGER PRIMARY KEY, a INT) STRICT",
        )
        .unwrap();
        conn.execute("INSERT INTO t VALUES (5, 1)").unwrap();
        conn.execute("INSERT INTO u VALUES (5, 1)").unwrap();
        conn.execute("INSERT INTO w VALUES (5, 1)").unwrap();
    }

    #[test]
    fn marker_is_read_back_as_written() {
        let marked = TableMarker::new("marking", true).write("CREATE TABLE t (x)");
        assert_eq!(
            marked,
            "/* turso_dialect:marking rowid_alias */ CREATE TABLE t (x)"
        );
        let (marker, table_sql) = TableMarker::read(&marked).unwrap().unwrap();
        assert_eq!(marker, TableMarker::new("marking", true));
        assert_eq!(table_sql, "CREATE TABLE t (x)");

        let unmarked_key = TableMarker::new("marking", false).write("CREATE TABLE t (x)");
        assert_eq!(
            unmarked_key,
            "/* turso_dialect:marking */ CREATE TABLE t (x)"
        );
        let (marker, _) = TableMarker::read(&unmarked_key).unwrap().unwrap();
        assert!(!marker.rowid_alias);

        assert!(TableMarker::read("CREATE TABLE t (x)").unwrap().is_none());
    }

    #[test]
    fn marker_with_an_unknown_word_is_corrupt() {
        for sql in [
            "/* turso_dialect:marking rowid_alias compressed */ CREATE TABLE t (x)",
            "/* turso_dialect:marking rowid_alias rowid_alias */ CREATE TABLE t (x)",
            "/* turso_dialect: */ CREATE TABLE t (x)",
            "/* turso_dialect:marking CREATE TABLE t (x)",
        ] {
            assert!(
                matches!(TableMarker::read(sql), Err(LimboError::Corrupt(_))),
                "{sql}"
            );
        }
    }

    #[test]
    fn postgres_tables_are_refused_outside_postgres() {
        let sql = "/* turso_frontend:postgres */ CREATE TABLE t (x int)";
        for dialect in [marking(), Arc::new(SqliteDialect) as Arc<dyn Dialect>] {
            assert!(matches!(
                parse_stored_table(dialect.as_ref(), sql, 2),
                Err(LimboError::Corrupt(_))
            ));
        }
    }

    #[test]
    fn create_table_writes_the_marker_with_the_rowid_decision() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "marker-create.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        create_keyed_tables(&conn);

        assert_eq!(
            stored_sql(&conn, "t"),
            "/* turso_dialect:marking rowid_alias */ CREATE TABLE t (id INTEGER PRIMARY KEY, a INT) STRICT"
        );
        assert_eq!(
            stored_sql(&conn, "u"),
            "/* turso_dialect:marking */ CREATE TABLE u (id INT PRIMARY KEY, a INT) STRICT"
        );
        assert_eq!(rowid_and_id(&conn, "t"), ints(&[&[5, 5]]));
        assert_eq!(rowid_and_id(&conn, "u"), ints(&[&[1, 5]]));
        conn.close().unwrap();
    }

    #[test]
    fn alter_table_keeps_the_marker() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "marker-alter.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        create_keyed_tables(&conn);

        let marker = "/* turso_dialect:marking rowid_alias */ ";
        for (statement, table) in [
            ("ALTER TABLE t ADD COLUMN b TEXT", "t"),
            ("ALTER TABLE t RENAME COLUMN b TO c", "t"),
            ("ALTER TABLE t DROP COLUMN c", "t"),
            ("ALTER TABLE t ALTER COLUMN a TO b INTEGER", "t"),
            ("ALTER TABLE t RENAME TO t2", "t2"),
        ] {
            conn.execute(statement).unwrap();
            let sql = stored_sql(&conn, table);
            assert!(sql.starts_with(marker), "{statement}: {sql}");
        }
        assert_eq!(rowid_and_id(&conn, "t2"), ints(&[&[5, 5]]));
        let error = conn
            .execute("ALTER TABLE t2 ALTER COLUMN id TO key INT")
            .unwrap_err();
        assert!(error.to_string().contains("rowid column"), "{error}");
        conn.execute("ALTER TABLE t2 RENAME COLUMN id TO key")
            .unwrap();
        assert!(stored_sql(&conn, "t2").starts_with(marker));

        conn.execute("ALTER TABLE u ADD COLUMN b TEXT").unwrap();
        assert!(stored_sql(&conn, "u").starts_with("/* turso_dialect:marking */ CREATE TABLE"));
        conn.close().unwrap();
        drop(db);

        let db = open_db(&io, "marker-alter.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        assert_eq!(query(&conn, "SELECT rowid, key FROM t2"), ints(&[&[5, 5]]));
        conn.close().unwrap();
    }

    #[test]
    fn reopened_tables_keep_the_rowid_decision_of_the_marker() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        {
            let db = open_db(&io, "marker-reopen.db", marking()).unwrap();
            let conn = db.connect(ConnectOpts::new()).unwrap();
            create_keyed_tables(&conn);
            conn.close().unwrap();
        }
        let db = open_db(&io, "marker-reopen.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        assert_eq!(rowid_and_id(&conn, "t"), ints(&[&[5, 5]]));
        assert_eq!(rowid_and_id(&conn, "u"), ints(&[&[1, 5]]));
        assert_eq!(rowid_and_id(&conn, "w"), ints(&[&[1, 5]]));
        assert!(conn.execute("INSERT INTO u VALUES (5, 2)").is_err());
        assert!(conn.execute("INSERT INTO w VALUES (5, 2)").is_err());
        conn.execute("INSERT INTO u VALUES (6, 2)").unwrap();
        conn.close().unwrap();
    }

    #[test]
    fn other_dialects_refuse_marked_tables() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        {
            let db = open_db(&io, "marker-refuse.db", marking()).unwrap();
            let conn = db.connect(ConnectOpts::new()).unwrap();
            create_keyed_tables(&conn);
            conn.close().unwrap();
        }
        for dialect in [
            Arc::new(SqliteDialect) as Arc<dyn Dialect>,
            Arc::new(MarkingDialect { name: "other" }),
        ] {
            let error = open_db(&io, "marker-refuse.db", dialect)
                .and_then(|db| db.connect(ConnectOpts::new()))
                .and_then(|conn| conn.prepare("SELECT * FROM t"))
                .expect_err("a marked table must not open in another dialect");
            assert!(
                error.to_string().contains("created by the marking dialect"),
                "{error}"
            );
        }
    }

    #[test]
    fn a_statement_with_the_marker_keeps_its_rowid_decision() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "marker-statement.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        conn.execute(
            "/* turso_dialect:marking */ CREATE TABLE t(id INTEGER PRIMARY KEY, a INT) STRICT",
        )
        .unwrap();
        assert_eq!(
            stored_sql(&conn, "t"),
            "/* turso_dialect:marking */ CREATE TABLE t (id INTEGER PRIMARY KEY, a INT) STRICT"
        );
        conn.execute("INSERT INTO t VALUES (5, 1)").unwrap();
        assert_eq!(rowid_and_id(&conn, "t"), ints(&[&[1, 5]]));
        conn.close().unwrap();
    }

    #[test]
    fn the_marker_is_refused_where_it_does_not_belong() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let sqlite_db = open_db(&io, "marker-sqlite.db", Arc::new(SqliteDialect)).unwrap();
        let sqlite_conn = sqlite_db.connect(ConnectOpts::new()).unwrap();
        assert!(sqlite_conn
            .execute("/* turso_dialect:marking */ CREATE TABLE t(x INT) STRICT")
            .is_err());

        let db = open_db(&io, "marker-misplaced.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        for statement in [
            "/* turso_dialect:other */ CREATE TABLE t(x INT) STRICT",
            "/* turso_dialect:marking */ SELECT 1",
            "/* turso_dialect:marking rowid_alias */ CREATE TABLE t(x TEXT PRIMARY KEY) STRICT",
        ] {
            assert!(conn.execute(statement).is_err(), "{statement}");
        }
    }

    #[test]
    fn marking_dialects_create_only_strict_tables() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "marker-strict.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        let error = conn.execute("CREATE TABLE t(x INT)").unwrap_err();
        assert!(error.to_string().contains("must be STRICT"), "{error}");
        let error = conn.execute("CREATE TABLE c AS SELECT 1 AS x").unwrap_err();
        assert!(
            error.to_string().contains("CREATE TABLE AS SELECT"),
            "{error}"
        );
    }

    #[test]
    fn engine_tables_are_not_marked() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "marker-engine.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, a INT) STRICT")
            .unwrap();
        conn.execute("PRAGMA capture_data_changes_conn('full')")
            .unwrap();
        conn.execute("INSERT INTO t(a) VALUES (1)").unwrap();

        let rows = query(
            &conn,
            "SELECT name, sql FROM sqlite_schema WHERE type = 'table' ORDER BY name",
        );
        let names: Vec<String> = rows.iter().map(|row| row[0].to_string()).collect();
        assert!(
            names.iter().any(|name| name == "sqlite_sequence"),
            "{names:?}"
        );
        assert!(names.iter().any(|name| name == "turso_cdc"), "{names:?}");
        for row in rows {
            let name = row[0].to_string();
            let sql = row[1].to_string();
            assert_eq!(
                sql.starts_with(TABLE_MARKER_PREFIX),
                name == "t",
                "{name}: {sql}"
            );
        }
    }

    #[cfg(all(feature = "fs", not(target_family = "wasm")))]
    #[test]
    fn vacuum_keeps_the_marker_and_the_rowid_decision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.db");
        let output_path = dir.path().join("output.db");
        let io: Arc<dyn IO> = Arc::new(crate::io::PlatformIO::new().unwrap());
        let db = Database::open_file_with_flags(
            io.clone(),
            path.to_str().unwrap(),
            OpenFlags::Create,
            crate::DatabaseOpts::new().with_vacuum(true),
            None,
            marking(),
        )
        .unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        create_keyed_tables(&conn);
        let t_sql = stored_sql(&conn, "t");
        let u_sql = stored_sql(&conn, "u");
        let w_sql = stored_sql(&conn, "w");

        conn.execute(format!("VACUUM INTO '{}'", output_path.display()))
            .unwrap();
        conn.execute("VACUUM").unwrap();

        let output_db = Database::open_file(io, output_path.to_str().unwrap(), marking()).unwrap();
        let output_conn = output_db.connect(ConnectOpts::new()).unwrap();
        for conn in [&conn, &output_conn] {
            assert_eq!(stored_sql(conn, "t"), t_sql);
            assert_eq!(stored_sql(conn, "u"), u_sql);
            assert_eq!(stored_sql(conn, "w"), w_sql);
            assert_eq!(rowid_and_id(conn, "t"), ints(&[&[5, 5]]));
            assert!(conn.execute("INSERT INTO u VALUES (5, 2)").is_err());
            assert!(conn.execute("INSERT INTO w VALUES (5, 2)").is_err());
        }
    }
}
