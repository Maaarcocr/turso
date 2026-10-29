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

## Status

First baseline (`baseline.txt`, 2026-10-21): **183,657 of the 302,599
committed specs pass** (60.7%), replayed through `tursomysql` with
`expect unordered` compared as multisets.

| Source | Specs | Passing | Highest cleared level |
|---|---:|---:|---:|
| sqllogictest | 149,919 | 138,601 | 101 |
| vitess | 143,556 | 42,134 | none (lowest level 3 fails) |
| go-mysql-server | 8,390 | 2,856 | 0 |
| vitess-endtoend | 734 | 66 | none (lowest level 63 fails) |
| all | 302,599 | 183,657 | 0 |

A debug replay with three processes takes about 44 minutes (115 CPU-minutes;
the sqllogictest `index/` setups dominate). CI replays a release build on
every pull request in `.github/workflows/sqltest.yml` and fails when a
baseline spec stops passing.

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
│   └── local/<source>/   specs that must not be committed (ignored), rebuilt
│                         from results/ by tools/materialize.py
├── results/<source>/     MySQL's results for sources whose SQL is not
│                         committed (see Results-only sources)
├── candidates/           importer output: unrecorded candidate JSONL (ignored)
├── .sources/             fetch-sources.sh checkouts (ignored)
├── .record-cache/        per-group oracle runs, makes recording resumable (ignored)
├── baseline.txt          ids of recorded specs that currently pass
├── baseline-local.txt    the same for specs/local/ (test ids only)
└── tools/                importers, recorder, materializer, scoreboard
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
4. **Replay.** `make -C mysql/conformance run-specs` replays the recorded
   specs through the `tursomysql` sqltest backend (embedded Turso with
   `MySqlDialect`), one bounded process per file (see [Replay](#replay)).
   `make -C mysql/conformance run` replays the owned specs, which must all
   pass.
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
`specs/local/<source>/`. TiDB, MatrixOne (some of their tests look ported
from GPL MySQL MTR) and MySQL's own mysql-test suite (GPL-2.0-only) are
recorded with `--results-only`, which also commits their results without SQL
(see [Results-only sources](#results-only-sources)). `--import-report` takes
the importer's JSON report or `IMPORT_REPORT.md` and copies it into
`REPORT.md`.

Scoring a replay (`make -C mysql/conformance run-specs` writes the report to
`target/mysql-specs-report.json`, the default input):

```bash
make -C mysql/conformance run-specs score           # replay, then gate
python3 mysql/conformance/tools/score.py --update   # ratchet
```

`baseline.txt` holds one `<source>/<dir>/<shard>.sqltest::<test>` per line
(paths relative to `specs/`). A baseline spec that fails, errors, is skipped
or disappears from a replayed file is a regression. `--update` refuses to drop
entries unless `--allow-shrink` is given.

## Replay

`make -C mysql/conformance run-specs` checks the syntax of the committed
sources under `specs/` and replays them with `tools/replay.py`
(`INCLUDE_LOCAL=1` adds `specs/local/`):

- Every file runs in its own `sqltest run --backend tursomysql --jobs 1`
  process, `SPECS_JOBS` (default: the CPU count) at a time, largest files
  first. Memory is bounded by the largest file, not by the corpus.
- `SPECS_MEMORY_MB` sets an address-space limit for each process (default 0,
  no limit). A limit makes a spec that allocates without bound abort its
  process at once instead of exhausting the machine, but it has to grow with
  the CPU count: the runtime and the allocator reserve address space per
  thread, so 2048 makes most specs fail on a 24-core machine. A file whose
  process crashes (or
  runs for over an hour) is replayed again one test per process: a test that
  crashes alone is an error carrying the end of its stderr, and every other
  test of the file keeps its real outcome.
- The backend runs each setup or test block on a thread of its database
  instance with a deadline (`SPECS_TIMEOUT`, default 30 seconds). Each
  statement gets the time left as its Turso query timeout, so a long statement
  is interrupted between bytecode steps; the test then fails with `execution
  failed: timeout after 30s`. A statement that does not stop within 10 seconds
  of the deadline (for example while it is being prepared) is abandoned on its
  thread, and the test fails the same way. Timeouts are never a pass, even for
  a spec that expects an error.
- The per-file reports are merged into one report in the `sqltest --output
  json` format for `tools/score.py`.

Specs that cannot finish in Turso today, all failing rather than hanging or
killing the replay (MySQL bounds both cases):

| Specs | Cause |
|---|---|
| 17 tests of `vitess/evalengine/FnRepeat.sqltest` (`REPEAT(x, 1073741825)`) | Turso's `repeat` (`exec_repeat` in `core/functions/string.rs`) allocates the whole `len(x) × n` result, 1 to 21 GB here, with no length limit; MySQL returns `NULL` above `max_allowed_packet`. Without the address-space limit one replay process grew to 12.7 GB RSS within seconds. They crash their process and are reported as errors. |
| `vitess-endtoend/vitess_tester/cte/queries.test.sqltest::l122` | A recursive CTE without a stop condition. MySQL aborts it after `cte_max_recursion_depth` (1000) iterations with error 3636; Turso recurses until the timeout. |

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
  (`duplicate`), or it violates the schema (`invalid`);
- with `--results-only`, the result reproduces the candidate's own SQL: one of
  its statements (12 characters or longer, whitespace normalized), or a
  statement fragment MySQL quotes in a `near '...'` diagnostic
  (`echoes-sql`), so committed results never carry source SQL.

## Results-only sources

GPL or license-unclear suites (`redistribute_generated_cases = false` in
`corpus/sources.lock.toml`) must not have their SQL committed. For them the
repository keeps only what MySQL answered, and anyone (and CI) can rebuild
runnable specs from a pinned checkout without a MySQL server:

```bash
make -C mysql/conformance materialize-local   # fetch, import, join -> specs/local/
make -C mysql/conformance run-local-specs     # replay through tools/replay.py
make -C mysql/conformance score-local         # gate against baseline-local.txt
python3 mysql/conformance/tools/score.py --local --update   # ratchet
```

Recording (needs MySQL; the lock entry must name the importer in
`importers.<source>`):

```bash
mysql/conformance/fetch-sources.sh mysql-mtr-8.4.10
python3 -I mysql/conformance/tools/import_mtr.py \
    --source mysql/conformance/.sources/mysql-mtr-8.4.10 --output mysql/conformance/candidates/mysql-mtr
python3 -I mysql/conformance/tools/record_mysql.py mysql/conformance/candidates/mysql-mtr/*.jsonl \
    --import-report mysql/conformance/candidates/mysql-mtr/IMPORT_REPORT.md --results-only
```

`--results-only` writes the usual ignored `specs/local/<source>/` and the
committed `results/<source>/`:

- `<group>.jsonl`, one file per candidate group (the shard unit) with one line
  per candidate that reached classification, in candidate order:

  ```json
  {"id":"tidb/cte:L44","sha256":"397f2b52…","ordered":true,"bucket":"recorded","rows":["1|1|1|4","1|0|0|4",…]}
  {"id":"tidb/collation_misc:L40","sha256":"d738e35e…","ordered":false,"bucket":"recorded","error":[1253,"42000","COLLATION 'utf8mb4_unicode_ci' is not valid for CHARACTER SET 'latin1'"],"dropped":[[3,1273],[4,1302]]}
  {"id":"tidb/collation_pointget:L72","sha256":"74575753…","ordered":false,"bucket":"error-quarantined","detail":"1054"}
  ```

  `sha256` covers the candidate's effective setup (after `setup_base`), sql,
  `ordered` and `setup_errors` (`record_mysql.candidate_digest`). Recorded
  outcomes carry `rows` (rendered lines; `raw` for `expect raw`) or `error`
  `[errno, SQLSTATE, message]`; `dropped` lists setup statements that failed
  in MySQL as `[position in the effective setup, errno]`. Other buckets keep
  only the `detail` REPORT.md counts (an errno, a fixed reason, or the first
  80 characters of a setup failure). There is never a SQL or setup field.
- `meta.json`: lock entry id, repository, revision, license, the importer
  command, a SHA-256 of the candidate files, recorder version, row and size
  limits and the oracle's server profile.
- `REPORT.md`: the recording's report (the same file as
  `specs/local/<source>/REPORT.md`).

Re-recording the same candidates writes byte-identical results.
`tools/materialize.py [source...]` checks out the pinned revision with
fetch-sources.sh into `.sources/` (or takes `--checkout`), re-runs the stored
importer command under `python3 -I` into `candidates/<source>/`, and joins
candidates with results by id. A result is used only when its SHA-256 and
`ordered` match the regenerated candidate; otherwise the candidate is counted
as `stale-result`, and candidates without a result as `no-result` (results
without a candidate are reported as orphaned). It then writes
`specs/local/<source>/` with the recorder's own `write_specs` and
`write_report`, so when nothing is stale the shards, manifests and REPORT.md
are byte-identical to a direct recording; it reports whether the candidate
digest and REPORT.md match the committed ones (`--strict` fails on any stale
or missing result or a different REPORT.md; `--reuse-candidates` skips the
fetch and import).
`tools/test_results.py` checks that committed results are well formed and,
when a source's candidates are imported locally, that no candidate statement
occurs in its results.

Local specs are replayed and scored separately from the committed corpus:
`score.py` ignores `specs/local/` unless `--local`, which reads
`specs/local/`, `baseline-local.txt` and the `run-local-specs` report, so the
committed `baseline.txt` never depends on locally materialized specs. A
replay with `INCLUDE_LOCAL=1 make run-specs` can be scored both ways.

| Source | Results records | Recorded specs | Withheld (`echoes-sql`) | Results size |
|---|---:|---:|---:|---:|
| tidb | 12,932 | 11,721 | 111 | 2.7 MB |
| matrixone | 24,657 | 21,507 | 0 | 5.2 MB |
| mysql-mtr | 68,190 | 62,440 | 40 | 15.3 MB |

First local baseline (`baseline-local.txt`, 2026-10-21): **15,539 of the
95,668 local specs pass** (tidb 1,309 of 11,721; matrixone 5,483 of 21,507;
mysql-mtr 8,747 of 62,440). Most failures are setup blocks the frontend does
not run yet (`SET`, `PREPARE`, table options). A debug replay with three
processes takes about 6 minutes (15.5 CPU-minutes). Specs that crash their
process and are reported as errors: `lpad`/`rpad`/`repeat` with huge lengths
(Turso allocates the whole result; 37 tests in `mysql-mtr/t/{lpad,rpad}` and
`matrixone/function/func_string_{lpad_rpad,repeat}`), and three
`mysql-mtr/t/func_gconcat` tests whose `GROUP_CONCAT` with three or more
arguments makes Turso print the argument count to stdout
(`core/dialect/sqlite/mod.rs`), which `tools/replay.py` treats like a crash.

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

### Application issues (`tools/import_app_issues.py`)

`specs/app-issues/` holds one shard per GitHub issue filed on
Maaarcocr/turso after running an application's test suite against
turso_mysql. Each statement of an issue's reproduction that MySQL answered in
the issue becomes a test; the statements before it that MySQL ran without
error are its setups. Unlike every other source, the expected values are
copied from the issue (MySQL 8.4.8, default 8.4 profile, `time_zone =
'+00:00'`), not recorded by `tools/record_mysql.py`; `REPORT.md` lists the
issues and the statements left out. They are scored and ratcheted like the
other sources.

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

### MySQL mysql-test (`tools/import_mtr.py`)

Reads MySQL 8.4.10's own `mysql-test` scripts (GPL-2.0-only, results-only)
statically, the way `mysqltest` would run them against the default server, and
never uses MySQL's `.result` files:

- Commands and statements are split like mysqltest's `read_line`: `--command`
  lines end at the line end, everything else at the current delimiter outside
  `'`, `"` and `` ` `` quotes; heredocs of `perl`/`write_file` are skipped.
- `--source` includes are inlined; `let`, `eval`, `inc`, `dec` and `expr` are
  evaluated when their values are known (an unset variable reads as "", as in
  mysqltest; MTR's upper-case environment and `$mysql_errno` are unknown);
  `if`/`while` blocks run when their conditions can be decided, loops are
  unrolled (at most 20,000 iterations and 64 MiB of SQL per file).
  Read-only queries in `let $v = \`...\`` are ordinary statements.
- Statements under an undecidable condition, with unresolved variables, or
  modifying queries in variables are *not statically replayable*: they never
  become candidates or setup, and objects they create or touch taint later
  candidates that need them.
- `--error`, `--sorted_result` (unordered) and the masks (`--replace_*`,
  `--lowercase_result`, `--disable_result_log`, whose statements are not
  emitted) apply as in mysqltest; `--disable_testcase` regions are dropped.
- A file is excluded, with a counted reason, when it opens other connections,
  runs shell or perl commands, sleeps outside undecidable blocks, sources an
  include for debug builds, replication or binary logs, NDB, plugins or
  components, server restarts or asynchronous waits, has server option files
  other than SQL-invisible ones, or reaches a `--skip` that the pinned profile
  is not known to pass (runtime requirement checks of includes that the
  oracle meets, such as `have_innodb_16k.inc`, are listed in
  `IGNORED_INCLUDES`).
- Only `mysql-test/t`, `include/` and the SQL-level suites `collations`,
  `engines`, `funcs_1`, `funcs_2`, `gcol`, `gis`, `innodb_fts`, `innodb_gis`,
  `json` and `parts` are imported; the lock entry fetches exactly these paths
  and an include outside them excludes its file, so a sparse checkout imports
  the same candidates as a full one.

Candidates are `tools/file_replay.py` dependency slices in database `test`
(at most 120 setup statements per candidate, 1,000 candidates and 20,000
setup statements per file). Ids are `mysql-mtr/<path>:L<line>` with the line
of the top-level command (a `--source` or `while` line for statements it
produces) and an ordinal suffix. The pinned import scans 2,253 test files
(1,130 excluded) and emits 68,315 candidates with 818,884 setup statements;
`results/mysql-mtr/REPORT.md` has every count.

## Licensing

Only permissively licensed sources have their specs checked in. Each source's
license is recorded in every manifest record, in `corpus/sources.lock.toml`
and in `NOTICE.md`. GPL or license-unclear sources (MySQL MTR, TiDB,
MatrixOne) commit only MySQL's results under `results/` (ids, SHA-256s and
MySQL's output, never their SQL); their specs live in the ignored
`specs/local/` and are rebuilt from a pinned checkout.
