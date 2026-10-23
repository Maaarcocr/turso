# MySQL frontend handover (2026-10-23)

This file is the starting point for continuing the MySQL 8.4 frontend work on a
new machine. It replaces the "Current Managed-Cycle Status" part of the MySQL
section in `CLAUDE.md`, which is out of date. The rest of that section (target
profile, architecture decisions, spec rules) still applies.

## Where things stand

- Turso runs MySQL SQL text through an embedded library (`turso_mysql`) and a
  CLI (`tursomysql`). There is no MySQL wire protocol server yet, and by
  decision it comes last (after types, MySQL-only DML, introspection and
  charsets).
- Correctness is measured by 302,599 committed specs recorded once from MySQL
  8.4.10 (plus 95,668 local-only specs from GPL sources). `main` passes
  263,084 of them.
- The main gap is MySQL's type semantics (exact DECIMAL, UNSIGNED, temporal
  values, binary strings, ENUM/SET). A core change, "typed expressions",
  lets a dialect define its own column types; it is finished on
  `core/typed-expressions` and MySQL types are being built on it ("Phase C").

## Branches (all pushed to origin)

| Branch | Head | What it is | State |
|---|---|---|---|
| `main` | 894a08fb | upstream 49e7e7ea → core ebf5b1a3 → MySQL frontend | verified: 263,084 specs, baseline matches |
| `core/typed-expressions` | dc8964c7 | upstream 49e7e7ea + core typed expressions only (no MySQL code), meant for upstream | Proposals 1–4 done; full core gate passed at dc8964c7 |
| `pi/mysql-c1` | b0eaa2d4 | main + core c19c1984 + slice C1 (typed tables) + review fixes | full replay NOT finished; see below |
| `pi/mysql-c2` | 21730129 | pi/mysql-c1 + core dc8964c7 + slice C2 (exact DECIMAL) + C3 (SUM/AVG) | sqllogictest and full replay NOT run |
| `pi/mysql-r2` | d9ba87c0 | main + core c19c1984 + round 2 (non-type fixes) | full replay NOT finished; last commit is `wip:` |
| `wip/core-sorter-comparator-errors` | 6d3e9548 | core dc8964c7 + uncommitted sorter fix, as a `wip:` commit | not gated |
| `mysql/on-core` | 894a08fb | same as main | can be deleted |
| `mysql/on-upstream`, `backup/main-before-rebase`, `core/typed-expressions-pre-cleanup`, `wip/core-typed-expressions` | | old backups | can be deleted |

Merges between these branches are merge commits; no branch was rebased after
`main` was moved. `pi/mysql-c2` already contains `pi/mysql-c1`.

## Immediate next steps

1. **Integrate C1 + C2.** Replay the full corpus on `pi/mysql-c2` (it contains
   C1), then `score.py`. Known losses against `baseline.txt`:
   - C1: 15 specs (`handover/reviews/mysql-c1-lost-specs.txt`): 6 passed by
     accident (AUTO_INCREMENT on a column that is not the whole primary key,
     `ADD COLUMN … AFTER`, one row-order match) and 9 BIGINT UNSIGNED specs whose
     setup inserts `18446744073709551615` through the SQLite fallback, which
     reads it as the double 2^64; they need exact integer literals (slice C4)
     or lowered INSERT.
   - C2 (last partial replay of the files that had losses): 8 specs. 2
     go-mysql-server `varchar_col IN (false, '1')` (VARCHAR compared as text
     with the DOUBLE 0.0), and 6 vitess specs that passed by accident and need
     C4/C6/C9 (hex literals in arithmetic, JSON comparison, CAST AS
     CHAR(16)/BINARY(16)).
   - Earlier partial replays: C1 gained 76 then 19 more; C2 gained ~6,400 on
     vitess + go-mysql-server. sqllogictest (149,405 passing) was never replayed
     on C2: check it first, with per-file times (DECIMAL comparisons on integer
     columns are the performance risk).
   Then `score.py <report> --update --allow-shrink` only for the accepted
   losses, commit `baseline.txt`, merge into `main`.
2. **Integrate round 2.** Replay `pi/mysql-r2`, score. Measured on the affected
   files only (vs the on-core replay):
   - invalid JSON cast error 3141: 862/937; TRIM forms (incl. the
     "double-quoted string" bucket, which was really TRIM): +1,461 specs;
     CONVERT/CAST to DATE/DATETIME/TIME/CHAR(n): 87/152 + 194/295; regex error
     3688: 33/70; COLLATION(): 58/59; SET of target-profile values and
     FOREIGN_KEY_CHECKS: 46/348 (the rest need user variables, sql_mode or
     autocommit=0).
   - `d9ba87c0` (`wip:` ON DUPLICATE KEY UPDATE as an upsert) passes 49/61 but
     loses 2 baseline specs (InsertScripts l1640, l1646) that expect foreign-key
     error 1452 and only passed because the statement did not parse. Either drop
     that commit or turn foreign keys on (below).
   - Foreign keys: MySQL checks them by default. `PRAGMA foreign_keys=ON` on
     connect gained 22 specs and lost 3 (ScriptTests-2 l11452, l11466, l13847:
     foreign keys over ENUM/DECIMAL compare stored values). Revisit after C2/C7.
3. **Then the remaining Phase C slices** (`handover/phase-c-design.md`, section
   4): C4 checked BIGINT / unsigned / bit operators / exact integer literals; C5
   temporal types and typed literals; C6 `_binary` and other introducers, hex
   and bit literals (largest failure group, ~9,700 specs); C7 ENUM/SET; C8
   single-precision FLOAT, ZEROFILL, YEAR, BIT display; C9 JSON type. Design
   estimate for all of Phase C: 270,000–275,000 passing specs.
4. **After types:** MySQL-only DML (INSERT IGNORE, REPLACE … SET, multi-table
   UPDATE/DELETE, affected rows, LAST_INSERT_ID, warnings), introspection
   (SHOW, DESCRIBE, information_schema, session variables), charsets and
   collations, and last the wire protocol server.

## Core work

- Design: `docs/internals/typed-expressions.md` on `core/typed-expressions`
  (problem statement, Proposals 1–4, implementation notes per commit).
- Upstream PR drafts and the list of upstream bugs found along the way:
  `handover/upstream-prs.md`.
- Gate used for every core commit (`handover/tools/core-gate.sh`, paths need
  adapting): tursodb/tursopg build, `cargo test -p turso_core --lib`,
  `core_tester` integration tests, PostgreSQL Rust tests, clippy, fmt,
  turso-sqltests, pg-sqltests, and the EXPLAIN comparison: every
  sqlite-sqltest run through `handover/tools/tursodb-explain-diff` against a
  `tursodb` built from upstream 49e7e7ea with custom types off must show 0
  bytecode differences (skip `big.sqltest` and `big-mvcc.sqltest`; they need
  more than 6 GB of disk).
- Queued core items (not started):
  - Commit the sorter fix (`wip/core-sorter-comparator-errors`): a failing sort
    comparator panics with "Memory allocation failed here".
  - CREATE TABLE … AS SELECT should copy NOT NULL and DEFAULT for result
    columns that are plain column references (MySQL does).
  - `id = 5` on a BIGINT UNSIGNED primary key without AUTO_INCREMENT scans the
    table; allow an index search for `(T, integer)` comparisons, treating a
    conversion error as "no match".
  - Result parameters for unary operators, CASE/COALESCE/UNION common types and
    function results (`CASE WHEN 1 THEN 2 ELSE 1.5 END` shows `2`, MySQL
    `2.0`); typed AVG of an expression (`AVG(-i)`); a UNION of INT and DECIMAL
    has no type.
  - Dialect functions with constant arguments run once before the row loop, so
    `SELECT 'a' REGEXP '(' FROM empty_table` errors where MySQL returns no rows.
  - Duplicate-key errors lack the key value (MySQL: `Duplicate entry '127' for
    key 't.PRIMARY'`); low priority until the wire adapter.
  - Known gaps: materialized views read typed columns without DECODE;
    `'1.50'::numeric` in tursopg reads back as 1.5.

## Reviews

Reviews were paused to reach a working MySQL faster; the plan is to review
commit by commit afterwards. Not yet independently reviewed:
- core `ebf5b1a3..dc8964c7` (review fixes, Proposals 3 and 4, the C1 core
  items, virtual columns computed from decoded values);
- `pi/mysql-c1` review fixes `9937ed5d..b0eaa2d4` (findings they answer:
  `handover/reviews/mysql-c1-review.md`);
- all of round 2 and C2.
Earlier reviews and their findings are in `handover/reviews/`.

## Decisions to keep

- Specs first: lots of recorded specs, all expected values captured once from
  the pinned MySQL 8.4.10 container and checked in. Hand-written unit tests are
  not a goal.
- Minimal core additions, mirroring what the PostgreSQL frontend needs. A
  frontend-only fix is preferred when it is enough (e.g. the `&&`/`@>` refusal
  in the SQLite fallback stays in `mysql/frontend/lib.rs`).
- Custom types and the PostgreSQL frontend are experimental and may break;
  plain SQLite with custom types off must stay exactly unchanged (same
  bytecode).
- The MySQL parse falls back to SQLite for anything not lowered, like the
  PostgreSQL frontend; no statement-origin plumbing in core.
- GPL/unclear-license sources (MySQL MTR, TiDB, MatrixOne) are used locally
  only: download scripts plus committed results (ids, hashes, outcomes), never
  their SQL.
- Ask before any force push of `main`.
- The wire protocol server comes last.

## Running things

```bash
# MySQL oracle (only needed to record new expected values)
python3 -c "import sys; sys.path.insert(0, 'mysql/conformance/tools'); \
  import record_mysql as r; c = r.Container(keep=True); \
  c.name = 'turso-mysql-oracle'; print(c.start())"
docker exec turso-mysql-oracle mysql -uroot --batch --raw -e "SELECT 1"

# MySQL crates and owned specs
cargo test -p turso_mysql_parser -p turso_mysql -p tursomysql
make -C mysql/conformance check run          # owned specs, must all pass
./mysql/test.sh                              # full MySQL gate

# Full corpus replay, scoring and failure buckets (from mysql/conformance)
cargo build -p sqltest && cp ../../target/debug/sqltest /tmp/sqltest-x
python3 tools/replay.py specs --sqltest /tmp/sqltest-x --output /tmp/x.json --jobs <cores>
python3 tools/score.py /tmp/x.json                 # lost specs vs baseline.txt
python3 tools/score.py /tmp/x.json --update        # add newly passing specs
python3 tools/triage.py /tmp/x.json --markdown FAILURES.md --json /tmp/triage.json
```

- A full replay took about 50 minutes on 4 cores with nothing else running.
  `--timeout 30` (per block) can fail slow specs on an overloaded machine:
  re-run any lost file alone before believing it.
- The memory-saving cargo settings in `CLAUDE.md` (one job, no debug info, no
  incremental) were for the old 4-core / 15 GB host; a bigger machine does not
  need them.
- Several worktrees sharing one `CARGO_TARGET_DIR` will link each other's
  workspace crates (cargo identifies them by relative path). Give each worktree
  its own target directory, or use `handover/tools/mysql-locked`, which takes a
  lock and touches the right sources before building.
- GitHub Actions on the fork never start: the workflows ask for Blacksmith
  runners, which only the upstream organisation has. Running the MySQL job on
  GitHub needs a separate workflow on `ubuntu-latest` (not written yet).
- Paths under `/tmp/claude-0/...` mentioned in the copied documents refer to
  the old machine and no longer exist.
