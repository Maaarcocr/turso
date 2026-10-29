use super::{BackendError, DatabaseFileHandle, DatabaseInstance, QueryResult, SqlBackend};
use crate::parser::ast::{Backend, Capability, DatabaseConfig, DatabaseLocation};
use async_trait::async_trait;
use std::any::Any;
use std::collections::HashSet;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;
use tokio::sync::oneshot;
use turso_core::{ColumnTypeInfo, Connection, LimboError};
use turso_mysql::format_text;

/// How long a block may run when the runner sets no timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(90);

/// How long past its deadline a block may take to stop before its thread is
/// abandoned. Statements stop at the deadline only while their bytecode runs,
/// so this covers work such as preparing a statement or a long opcode.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// Backend that runs tests in process on Turso with the MySQL frontend
/// dialect. Statements are split the way the `mysql` client splits them, and
/// values are rendered as MySQL's text protocol sends them, so expected
/// output recorded from MySQL compares directly.
///
/// Each setup or test block runs on a thread owned by the database instance,
/// so a block that outlives the timeout fails with [`BackendError::Timeout`]
/// instead of blocking the runner.
pub struct TursomysqlBackend {
    timeout: Duration,
}

impl TursomysqlBackend {
    pub fn new() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Limits how long one setup or test block may run.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Default for TursomysqlBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SqlBackend for TursomysqlBackend {
    fn name(&self) -> &str {
        "tursomysql"
    }

    fn backend_type(&self) -> Backend {
        Backend::Tursomysql
    }

    fn capabilities(&self) -> HashSet<Capability> {
        // The MySQL frontend's feature surface is distinct from the sqlite
        // capabilities tests can require.
        HashSet::new()
    }

    async fn create_database(
        &self,
        config: &DatabaseConfig,
    ) -> Result<Box<dyn DatabaseInstance>, BackendError> {
        if !config.is_writable() {
            return Err(BackendError::CreateDatabase(format!(
                "the tursomysql backend only creates new :memory: and :temp: databases, not {}",
                config.location
            )));
        }
        let (conn, temp_file) = match config.location {
            DatabaseLocation::TempFile => {
                let temp = NamedTempFile::new()
                    .map_err(|e| BackendError::CreateDatabase(e.to_string()))?;
                let path = temp.path().to_str().ok_or_else(|| {
                    BackendError::CreateDatabase(format!(
                        "temporary database path is not UTF-8: {}",
                        temp.path().display()
                    ))
                })?;
                (turso_mysql::open_file(path), Some(temp))
            }
            _ => (turso_mysql::open_memory(), None),
        };
        let conn = conn.map_err(|e| BackendError::CreateDatabase(e.to_string()))?;
        let (blocks, thread) = spawn_block_thread(Arc::clone(&conn))
            .map_err(|e| BackendError::CreateDatabase(format!("cannot start a thread: {e}")))?;
        Ok(Box::new(TursomysqlDatabaseInstance {
            conn,
            blocks,
            thread,
            timeout: self.timeout,
            abandoned: false,
            _temp_file: temp_file,
        }))
    }
}

/// A database instance with one in-process MySQL-dialect connection.
pub struct TursomysqlDatabaseInstance {
    conn: Arc<Connection>,
    /// Sends blocks to the thread that runs them on `conn`. Dropping it ends
    /// the thread once its current block finishes.
    blocks: mpsc::Sender<Block>,
    /// The thread, joined on close so that the session of `conn` (its
    /// user-level locks, which every session of the process shares) ends
    /// before the next database is created.
    thread: thread::JoinHandle<()>,
    timeout: Duration,
    /// Set when a block did not stop after its deadline. Its thread may still
    /// be using the connection, so no further block runs on it.
    abandoned: bool,
    /// Keep temp file alive - deleted when dropped
    _temp_file: Option<NamedTempFile>,
}

/// A block of SQL for the instance's thread, and where to send the outcome.
struct Block {
    sql: String,
    deadline: Instant,
    reply: oneshot::Sender<Result<BlockOutcome, Box<dyn Any + Send>>>,
}

enum BlockOutcome {
    Rows(Vec<Vec<String>>),
    Error(String),
    TimedOut,
}

/// Starts the thread that runs every block of one connection in order. A
/// panic while running a block is sent back to be resumed in the runner's
/// task, which reports it like any panic of a test.
fn spawn_block_thread(
    conn: Arc<Connection>,
) -> std::io::Result<(mpsc::Sender<Block>, thread::JoinHandle<()>)> {
    let (sender, receiver) = mpsc::channel::<Block>();
    let thread = thread::Builder::new()
        .name("tursomysql".to_string())
        .spawn(move || {
            for block in receiver {
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                    run_block(&conn, &block.sql, block.deadline)
                }));
                // The runner stops waiting when a block outlives the grace
                // period; nobody needs the outcome then.
                let _ = block.reply.send(outcome);
            }
        })?;
    Ok((sender, thread))
}

/// Runs every statement in order and concatenates their rows. The first
/// failing statement ends the block, as in the `mysql` client. Each statement
/// gets the time left until `deadline` as its query timeout, so a long
/// statement is interrupted between bytecode steps once the deadline passes.
fn run_block(conn: &Arc<Connection>, sql: &str, deadline: Instant) -> BlockOutcome {
    let ranges = match turso_mysql_parser::split_statements(sql) {
        Ok(ranges) => ranges,
        Err(e) => return BlockOutcome::Error(e.to_string()),
    };
    let mut rows = Vec::new();
    for range in ranges {
        // The dialect parses one statement per call; keep consuming so no
        // text of the statement could be skipped.
        let mut remaining = &sql[range];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return BlockOutcome::TimedOut;
            }
            // A query timeout below one millisecond would disable it.
            conn.set_query_timeout(left.max(Duration::from_millis(1)));
            let (mut stmt, consumed) = match conn.consume_stmt(remaining) {
                Ok(Some(next)) => next,
                Ok(None) => break,
                Err(e) => return failure(e, deadline),
            };
            let column_types: Vec<Option<ColumnTypeInfo>> = (0..stmt.num_columns())
                .map(|column| stmt.get_column_type_info(column).ok().flatten())
                .collect();
            let result = stmt.run_with_row_callback(|row| {
                rows.push(
                    row.get_values()
                        .zip(&column_types)
                        .map(|(value, column_type)| format_text(value, column_type.as_ref()))
                        .collect(),
                );
                Ok(())
            });
            if let Err(e) = result {
                return failure(e, deadline);
            }
            remaining = &remaining[consumed..];
        }
    }
    BlockOutcome::Rows(rows)
}

/// Classifies a failed statement. The deadline can also surface as another
/// error, for example from a nested statement, so any failure after the
/// deadline is a timeout rather than an error a spec could expect.
fn failure(error: LimboError, deadline: Instant) -> BlockOutcome {
    if matches!(error, LimboError::Interrupt) || Instant::now() >= deadline {
        BlockOutcome::TimedOut
    } else {
        BlockOutcome::Error(error.to_string())
    }
}

#[async_trait]
impl DatabaseInstance for TursomysqlDatabaseInstance {
    async fn execute(&mut self, sql: &str) -> Result<QueryResult, BackendError> {
        if self.abandoned {
            return Err(BackendError::Execute(
                "an earlier block did not stop after its timeout".to_string(),
            ));
        }
        let (reply, outcome) = oneshot::channel();
        let block = Block {
            sql: sql.to_string(),
            deadline: Instant::now() + self.timeout,
            reply,
        };
        self.blocks
            .send(block)
            .map_err(|_| BackendError::Execute("the connection thread has exited".to_string()))?;
        match tokio::time::timeout(self.timeout + STOP_GRACE, outcome).await {
            Ok(Ok(Ok(BlockOutcome::Rows(rows)))) => Ok(QueryResult::success(rows)),
            Ok(Ok(Ok(BlockOutcome::Error(message)))) => Ok(QueryResult::error(message)),
            Ok(Ok(Ok(BlockOutcome::TimedOut))) => Err(BackendError::Timeout(self.timeout)),
            Ok(Ok(Err(payload))) => panic::resume_unwind(payload),
            Ok(Err(_)) => Err(BackendError::Execute(
                "the connection thread exited without an outcome".to_string(),
            )),
            Err(_) => {
                // The statement ignored its deadline. Interrupting may still
                // stop it; either way the thread is left behind.
                self.conn.interrupt();
                self.abandoned = true;
                Err(BackendError::Execute(format!(
                    "timeout after {:?}: the statement did not stop {:?} after its deadline",
                    self.timeout, STOP_GRACE
                )))
            }
        }
    }

    async fn close(self: Box<Self>) -> Result<DatabaseFileHandle, BackendError> {
        let Self {
            conn,
            blocks,
            thread,
            timeout: _,
            abandoned,
            _temp_file,
        } = *self;
        drop(blocks);
        drop(conn);
        if !abandoned {
            let joined = tokio::task::spawn_blocking(move || thread.join())
                .await
                .map_err(|e| BackendError::Execute(format!("cannot join the thread: {e}")))?;
            if let Err(payload) = joined {
                panic::resume_unwind(payload);
            }
        }
        match _temp_file {
            Some(tf) => Ok(DatabaseFileHandle::temp(tf)),
            None => Ok(DatabaseFileHandle::none()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(location: DatabaseLocation, sql: &str) -> QueryResult {
        let config = DatabaseConfig {
            location,
            readonly: false,
        };
        let mut db = TursomysqlBackend::new()
            .create_database(&config)
            .await
            .unwrap();
        let result = db.execute(sql).await.unwrap();
        db.close().await.unwrap();
        result
    }

    #[tokio::test]
    async fn runs_statements_in_order_and_concatenates_rows() {
        for location in [DatabaseLocation::Memory, DatabaseLocation::TempFile] {
            let result = run(
                location,
                "SELECT 'a;b', NULL; -- c;\nCREATE TABLE t(x INTEGER);\nSELECT 1e0/3 # d;\n;",
            )
            .await;
            assert_eq!(result.error, None);
            assert_eq!(
                result.rows,
                vec![vec!["a;b", "NULL"], vec!["0.3333333333333333"]]
            );
        }
    }

    #[tokio::test]
    async fn first_error_ends_the_block() {
        let result = run(
            DatabaseLocation::Memory,
            "CREATE TABLE t(x INTEGER); CREATE TABLE t(x INTEGER); SELECT 1",
        )
        .await;
        assert!(result.error.unwrap().contains("already exists"));
        assert!(result.rows.is_empty());

        let result = run(DatabaseLocation::Memory, "SELECT 1; SELECT 'open").await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn setup_persists_on_the_instance() {
        let config = DatabaseConfig {
            location: DatabaseLocation::Memory,
            readonly: false,
        };
        let mut db = TursomysqlBackend::new()
            .create_database(&config)
            .await
            .unwrap();
        db.execute_setup("CREATE TABLE t(x INTEGER);")
            .await
            .unwrap();
        let duplicate = db.execute("CREATE TABLE t(x INTEGER)").await.unwrap();
        assert!(duplicate.is_error());
        assert!(db.execute_setup("SELECT 'open").await.is_err());
    }

    /// A count of the 10^10 rows of ten joined tables of ten digits, which
    /// only the timeout ends.
    fn endless_count() -> String {
        let digits = (0..10)
            .map(|digit| format!("SELECT {digit} AS n"))
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        let tables = (0..10)
            .map(|table| format!("({digits}) AS d{table}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("SELECT COUNT(*) FROM {tables}")
    }

    #[tokio::test]
    async fn a_block_that_outlives_the_timeout_fails_with_a_timeout() {
        let timeout = Duration::from_millis(500);
        let backend = TursomysqlBackend::new().with_timeout(timeout);
        let config = DatabaseConfig {
            location: DatabaseLocation::Memory,
            readonly: false,
        };
        let mut db = backend.create_database(&config).await.unwrap();
        let start = Instant::now();
        let result = db.execute(&endless_count()).await;
        assert!(
            matches!(result, Err(BackendError::Timeout(t)) if t == timeout),
            "{result:?}"
        );
        // The statement stops at its deadline, well before the grace period.
        assert!(
            start.elapsed() < timeout + STOP_GRACE,
            "{:?}",
            start.elapsed()
        );
        // The interrupted statement leaves the connection usable.
        let result = db.execute("SELECT 1").await.unwrap();
        assert_eq!(result.rows, vec![vec!["1"]]);
        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn the_timeout_covers_the_whole_block() {
        let timeout = Duration::from_millis(500);
        let backend = TursomysqlBackend::new().with_timeout(timeout);
        let config = DatabaseConfig {
            location: DatabaseLocation::Memory,
            readonly: false,
        };
        let mut db = backend.create_database(&config).await.unwrap();
        // Once the first statement used up the time, the block ends without
        // running the rest.
        let result = db
            .execute(&format!("{}; CREATE TABLE t(x INTEGER)", endless_count()))
            .await;
        assert!(
            matches!(result, Err(BackendError::Timeout(_))),
            "{result:?}"
        );
        let result = db.execute("CREATE TABLE t(x INTEGER)").await.unwrap();
        assert_eq!(result.error, None);
    }

    #[tokio::test]
    async fn rejects_databases_it_cannot_create() {
        let config = DatabaseConfig {
            location: DatabaseLocation::Default,
            readonly: true,
        };
        assert!(
            TursomysqlBackend::new()
                .create_database(&config)
                .await
                .is_err()
        );
    }
}
