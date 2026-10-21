# MySQL Spec Corpus

The MySQL frontend is measured by a large corpus of recorded specifications.
Each spec is ordinary `.sqltest` input for the shared runner in
`testing/sqltest`. Its expected output was captured **once** from the pinned
MySQL 8.4.10 oracle (`mysql/conformance/target.toml`) and checked in, so normal
test runs replay the recorded behavior without a MySQL server.

This follows the process of [liquid-spec](https://github.com/Shopify/liquid-spec):
observable behavior is recorded from the reference implementation, every spec
carries provenance and a difficulty level, and the scoreboard reports the
highest level that is fully cleared plus the next failures to work on.

## Layout

```text
mysql/conformance/
├── my-sqltests/          owned specs; every one must pass (CI gate)
├── corpus/
│   └── sources.lock.toml pinned upstream repositories, revisions, licenses
├── specs/
│   ├── <source>/         recorded imported specs (ratcheted, see below)
│   │   ├── REPORT.md     extraction/recording bucket counts for the source
│   │   └── <dir>/
│   │       ├── *.sqltest      shards of at most 1,000 tests
│   │       └── manifest.jsonl one record per shard in this directory
│   └── local/<source>/   recordings that must not be committed (ignored)
├── candidates/           importer output: unrecorded candidate JSONL (ignored)
├── .sources/             fetch-sources.sh checkouts (ignored)
├── .record-cache/        per-group oracle runs, makes recording resumable (ignored)
├── baseline.txt          ids of recorded specs that currently pass
└── tools/                importers, recorder, scoreboard
```

Each `manifest.jsonl` line describes one shard of its directory:

```json
{"shard": "random/select/slt_good_0.sqltest", "group": "sqllogictest/random/select/slt_good_0",
 "repo": "https://github.com/gregrahn/sqllogictest.git", "revision": "c93ac416...",
 "license": "CC0-1.0", "paths": ["test/random/select/slt_good_0.test"],
 "tests": [["l1234", "sqllogictest/random/select/slt_good_0:L1234", 352, 0, 1234], ...]}
```

`shard` is relative to `specs/<source>/`, and each test is
`[name, candidate id, level, index into paths, source line]`. Grouping the
tests per shard keeps the manifests at about a third of the size of one JSON
object per test; feature tags are counted in `REPORT.md` and kept in the
candidates.

## Pipeline

1. **Fetch.** `./fetch-sources.sh [id...]` checks out every source in
   `corpus/sources.lock.toml` at its pinned revision into `.sources/`.
2. **Import.** A source importer turns upstream tests into candidate JSONL
   (schema below). Importers never invent expected results.
3. **Record.** `tools/record_mysql.py` runs every candidate against the pinned
   MySQL container, twice in independent databases, and writes `.sqltest`
   shards plus manifests and `REPORT.md`. Only the recorder talks to MySQL.
   `--verify` re-runs checked-in specs against MySQL and reports drift.
4. **Replay.** `make -C mysql/conformance run` replays specs through the
   `tursomysql` sqltest backend (embedded Turso with `MySqlDialect`).
5. **Score.** `tools/score.py` compares a replay with `baseline.txt`. A spec in
   the baseline that fails is a regression and fails the gate. A newly passing
   spec must be added with `--update` (the baseline only grows). It also
   reports pass counts per source and the highest fully cleared level.

Regenerating the sqllogictest corpus:

```bash
mysql/conformance/fetch-sources.sh sqllogictest
python3 -I mysql/conformance/tools/import_sqllogictest.py \
    --source mysql/conformance/.sources/sqllogictest \
    --output mysql/conformance/candidates/sqllogictest.jsonl \
    --report mysql/conformance/candidates/sqllogictest.report.json
python3 -I mysql/conformance/tools/record_mysql.py \
    mysql/conformance/candidates/sqllogictest.jsonl \
    --import-report mysql/conformance/candidates/sqllogictest.report.json \
    --sqltest-bin target/debug/sqltest
python3 -I mysql/conformance/tools/record_mysql.py --verify --source sqllogictest
```

The recorder starts the pinned image itself (or reuses a server through
`--dsn mysql://root@HOST:3306`) and refuses to record when the server's version,
`sql_mode`, charset, collation, time zone or `lower_case_table_names` differ
from `target.toml`. Every session is also checked for the target
`collation_connection`, result charset, `autocommit=1`, `sql_mode` and time
zone. Its container adds only durability options
(`innodb_flush_log_at_trx_commit=2`, `sync_binlog=0`), which change no
SQL-visible behavior. Re-recording the same candidates is byte-identical;
interrupted runs resume from `.record-cache/`. Every emitted file is validated
with `sqltest check`.

A source is committed under `specs/<source>/` only when its
`corpus/sources.lock.toml` entry lists it in `spec_sources` and has
`redistribute_generated_cases = true`, and no candidate is GPL-licensed;
otherwise (or with `--local`) it is recorded into the ignored
`specs/local/<source>/`. TiDB and MatrixOne are local for now: some of their
tests look ported from GPL MySQL MTR. `--import-report` takes the importer's
JSON report or `IMPORT_REPORT.md` and copies it into `REPORT.md`.

Scoring a replay (`make -C mysql/conformance run-specs` writes the report to
`target/mysql-specs-report.json`, the default input):

```bash
python3 mysql/conformance/tools/score.py            # gate
python3 mysql/conformance/tools/score.py --update   # ratchet
```

`baseline.txt` holds one `<source>/<dir>/<shard>.sqltest::<test>` per line
(paths relative to `specs/`). A baseline spec that fails, errors, is skipped
or disappears from a replayed file is a regression. `--update` refuses to drop
entries unless `--allow-shrink` is given.

## Candidate schema (JSONL, one object per line)

```json
{
  "id": "sqllogictest/select1:L1234",
  "source": {"repo": "...", "revision": "...", "path": "...", "line": 1234,
             "license": "..."},
  "group": "sqllogictest/select1",
  "setup": ["CREATE TABLE t1(a INTEGER, b INTEGER)", "INSERT INTO t1 VALUES(1,2)"],
  "sql": ["SELECT a FROM t1 ORDER BY a"],
  "ordered": true,
  "level": 120,
  "features": ["select", "order-by"],
  "setup_errors": "fail"
}
```

- `id` is stable and source-derived. Re-importing the same revision yields
  the same ids. The part after the group becomes the test name.
- `group` is the shard unit: one group becomes one `.sqltest` file named after
  it (split at 1,000 tests). Keep groups at or below ~1,000 candidates.
- `setup` statements run before `sql` in a fresh database and session.
  Candidates whose setups are prefixes of each other share chained setup
  blocks.
- `setup_base` (optional) is the id of an earlier candidate of the same group
  in the same file. The effective setup is that candidate's effective setup
  followed by `setup`. Importers with long shared setups (sqllogictest) use it
  to keep candidate files small.
- `setup_errors` (optional) is `"fail"` (default) or `"drop"`. With `"fail"`,
  a setup statement that errors in MySQL puts the candidate in `setup-failed`.
  With `"drop"`, the statement is left out of the emitted setup and counted in
  `REPORT.md`; use it only when a failed statement provably has no effect
  (MySQL statements are atomic), as for sqllogictest's mixed-dialect scripts.
- `sql` statements run in order. The recorded result is the concatenation of
  the rows of every statement; the first error ends the test as an error.
- `ordered` is false when the source says the row order is unspecified (for
  example sqllogictest `rowsort`/`valuesort`, or no `ORDER BY`). Unordered
  specs use `expect unordered`.
- `level` (0–1000) orders specs from simplest to hardest, as in liquid-spec.
  Feature tags drive the level (`tools/spec_lib.py` `classify`): the level of
  a spec is the base level of its hardest feature, including features its
  setup needs, plus a small complexity bonus. Classification can improve later
  without changing ids.

## Recorded output format

```text
# Recorded from MySQL by mysql/conformance/tools/record_mysql.py. Do not edit;
# re-record instead (see mysql/conformance/SPECS.md).
# oracle: MySQL 8.4.10 (mysql:8.4.10), default profile from target.toml
# source: https://github.com/gregrahn/sqllogictest.git @ c93ac416f056...
#   test/select1.test
# license: CC0-1.0
@database :memory:

setup s1 {
    CREATE TABLE t1(a INTEGER, b INTEGER);
    INSERT INTO t1 VALUES(1,2);
}

setup s2 {
    DELETE FROM t1 WHERE a > 1;
}

@setup s1
@setup s2
test l1234 {
    SELECT a FROM t1 ORDER BY a;
}
expect {
    1
    NULL
}

# mysql-error: 1690 22003 BIGINT UNSIGNED value is out of range in '(cast(1 as unsigned) - 2)'
test l1240 {
    SELECT CAST(1 AS UNSIGNED) - 2;
}
expect error {
}
```

- Setup blocks are segments of one statement sequence, cut where tests attach
  or the sequence branches. A test composes them with `@setup` decorators in
  order; the runner applies multiple setups in declaration order
  (`testing/sqltest/src/runner/mod.rs`).
- SQL is written verbatim (only `{`, `}` and backslashes before them are
  escaped for the block lexer); statements end with `;`, on a new line when
  the statement ends in a line comment.
- Test names come from the candidate id after its group (`:L1234` → `l1234`).

Values use MySQL's text-protocol representation, which is what a MySQL client
receives:

| MySQL value | Rendered as |
|---|---|
| `NULL` | `NULL` |
| integers, `DECIMAL`, temporal values | MySQL's text exactly (`0.5000`, `2024-01-02`) |
| `DOUBLE`/`FLOAT` | MySQL's text exactly (`0.3333333333333333`, `1e21`) |
| strings | UTF-8 text; binary values that are not UTF-8 are lossily decoded |

The string `'NULL'` and SQL `NULL` render identically. In exact mode an empty
result and a single empty value both render as an empty block. `expect
unordered` compares rows as a multiset (the runner's comparator): order is
free, but every row must appear as often as MySQL returned it. Its rows are
written sorted so that re-recording is byte-identical. When a value starts or ends with whitespace, which `expect`
blocks trim, an ordered or single-row result is written as an unindented
`expect raw` block instead.

A test whose statements fail in MySQL is recorded as `expect error {}` with
the MySQL errno, SQLSTATE and message in a comment. The runner checks only
that an error occurs; error-code parity comes later. Database names in error
messages are normalized to `test`.

A test is not emitted, and is counted in `REPORT.md`, when:

- MySQL returns different results in the two independent runs, or setup
  statements fail differently (`nondeterministic`); unordered results are
  compared as sorted multisets, ordered results exactly. The two runs freeze
  the session clock at different instants (`SET timestamp`), so results that
  depend on the current time are caught here;
- a value contains `|`, a line break or another control character (tab
  excepted), a multi-row unordered result has edge whitespace or an empty
  single-value row, a statement contains a top-level `;` (the replay splits
  compound `BEGIN ... END` bodies like the `mysql` client), NUL or a carriage
  return, or the SQL or output cannot otherwise round-trip through the
  `.sqltest` lexer and comparator (`unrepresentable`);
- the result exceeds 100 rows or 16 KiB of expected output (`too-large`);
- the setup fails in MySQL and the candidate does not allow dropping
  (`setup-failed`), so a missing-setup error is never recorded as expected
  behavior;
- the result is MySQL error 1064 (syntax), 1146 (no such table), 1049 (unknown
  database), 1054 (unknown column) or 1051 (unknown table)
  (`error-quarantined`): for imported cases these mostly mean the importer did
  not reproduce the case's dialect or context;
- a statement times out or loses the connection (`timeout`);
- a statement would change server-global state that dropping the test's
  databases and users cannot undo: global or persisted variables, plugins,
  tablespaces, replication, the root account, XA transactions
  (`unsupported-environment`; never executed);
- the importer could not extract it statically (`dynamic`), it is a duplicate
  id or a duplicate of another candidate's setup, sql and ordering
  (`duplicate`), or it violates the schema (`invalid`).

## Execution model of the recorder

Semantics are always "fresh database and session, run the setup, run the
sql". Leading and trailing whitespace of every statement is dropped. Candidates
are scheduled into lanes:

- **chain**: read-only `sql` (`SELECT`, `WITH`, `SHOW`, ... without user
  variables, `INTO`, locking or previous-statement functions such as
  `LAST_INSERT_ID()`/`ROW_COUNT()`). One database walks the group's setup trie
  and runs these candidates where they attach; a sibling branch replays its
  prefix in a fresh database.
- **isolated**: anything else gets its own database, session and setup.
- **exclusive**: candidates touching server-global state (databases, users,
  grants, `information_schema`, `mysql.*`) run serially after the parallel
  phase; databases and users they create are dropped afterwards.

Each run uses uniquely named databases, so a result that leaks the database
name is classified `nondeterministic`.

## Importers

### sqllogictest (`tools/import_sqllogictest.py`)

Reads the scripts exactly like `sqllogictest.c` for the `mysql` engine
(`skipif`/`onlyif` conditions, comments, CRLF, `halt`) and never uses SQLite's
expected results except the result size, to skip queries over 100 rows. Each
runnable `query` and `statement error` record becomes a candidate whose setup is
every runnable statement before it in the file (`setup_errors: drop`); a query
that reads no table gets an empty setup. `rowsort`/`valuesort` and `nosort`
without `ORDER BY` are unordered. Queries whose FROM list has 21 to 61 tables
(`select5.test`) are skipped (`excluded-wide-join`): MySQL's default
exhaustive join-order search (`optimizer_search_depth=62`) needs seconds to
hours for each of them. Lists over MySQL's 61-table limit fail at once with
error 1116 and are kept.

Sampling is deterministic (stable hash of the id) and bounded by the replay
cost: the Turso replay runs each test's full setup in a fresh database (the
debug runner needs about 4 ms per test plus 0.75 ms per setup statement). The
default sample keeps about 150,000 candidates, skips table queries that need
more than 2,100 setup statements (`excluded-long-setup`: most of
`index/delete` and `index/view` at 1,000+ rows), and keeps at most 25,000
replayed setup statements and 1,000 tests per file, at least 100 tests per file
and all of `evidence/` and `select*` within those caps; the whole sample
replays about 4M setup statements (guard: 6M). `--all` emits every eligible
candidate for local use.

## Licensing

Only permissively licensed sources are checked in. Each source's license is
recorded in every manifest record, in `corpus/sources.lock.toml` and in
`NOTICE.md`. GPL sources (MySQL MTR, MariaDB) are recorded into `specs/local/`,
which is ignored by git and never committed.
