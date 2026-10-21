# Turso Agent Guidelines

SQLite rewrite in Rust. 40+ crate workspace.

## Quick Reference

```bash
cargo build                    # build. never build with --release
cargo check                    # type check. prefer to `cargo build`
cargo test                     # rust unit/integration tests
cargo fmt                      # format (required)
cargo clippy --workspace --all-features --all-targets -- --deny=warnings  # lint
cargo run -q --bin tursodb -- -q # run the interactive cli. never run with --release

make test                      # TCL compat + sqlite3 + extensions + MVCC
make test-single TEST=foo.test # single TCL test
make -C sqlite/conformance run-rust ARGS='--snapshot-filter __never__'  # sqltest runner (preferred for new tests)
CI=1 make -C sqlite/conformance run-rust  # use only if snapshot tests are required

scripts/diff.sh "SQL" [label]  # compare sqlite3 vs tursodb output
```

## Testing

### Running Tests

- `cargo test` - Rust unit and integration tests
- `make test` - broad compatibility suite (TCL, sqlite3, extensions, MVCC)
- `make test-single TEST=foo.test` - single legacy TCL test
- `make -C sqlite/conformance run-rust ARGS='--snapshot-filter __never__'` - preferred `.sqltest` runner for new coverage
- `CI=1 make -C sqlite/conformance run-rust` - only when snapshot tests are required
- `make -C postgres/conformance run-postgres` - run the PostgreSQL `.sqltest` corpus against a real PostgreSQL server (needs Docker, or a server at `PG_DSN`)

### Test Organization

Default: add coverage to the narrowest existing test harness that can express the bug. Prefer extending an existing test file or directory over creating a new one.

- `sqlite/conformance/sqlite-sqltests/` - preferred for SQL conformance coverage. These tests run the same scenario against both Turso and SQLite, so use them first for parser, planner, executor, and SQL semantics work that fits the `.sqltest` DSL.
- `tests/integration/` - primary fallback when the behavior cannot be expressed cleanly in `.sqltest`. Put API-level regressions, multi-connection orchestration, storage assertions, injected failures, timeout behavior, and other Rust-driven scenarios here.
- `sqlite/conformance/upstream/` - imported upstream SQLite golden tests. Do not modify these for Turso behavior changes; use them as fixed compatibility coverage, and only touch them for intentional upstream sync or harness maintenance.
- `postgres/conformance/pg-sqltests/` - `.sqltest` coverage for the PostgreSQL frontend.
- `testing/cli_tests/` - CLI-focused Python coverage for shell behavior and end-to-end command workflows.
- `tests/fuzz/` - minimized fuzz regressions and targeted edge cases that are easier to keep as Rust tests.
- `testing/simulator/` and `testing/concurrent-simulator/` - deterministic concurrency, scheduling, and failure-injection coverage for state-machine and I/O correctness.
- `testing/differential-oracle/` and `testing/stress/` - differential and long-running stress tooling. Use these for deeper investigation or specialized validation, not as the first stop for a focused regression test.

### Differential-Fuzzer Regression Rule

Every differential-fuzzer failure that is successfully minimized must become a
permanent `*.sqltest` regression case before its fix is considered complete.
The seed and minimized fuzzer artifact are debugging aids, not substitutes for
the focused SQL regression.

## Structure

```
limbo/
├── core/           # Database engine (translate/, storage/, vdbe/, io/, mvcc/)
├── sqlite/
│   └── parser/     # SQL parser (lexer, AST, grammar)
├── cli/            # tursodb CLI (REPL, MCP server, sync server)
├── bindings/       # Python, JS, Java, .NET, Go, Rust
├── extensions/     # crypto, regexp, csv, fuzzy, ipaddr, percentile
├── testing/        # simulator/, concurrent-simulator/, differential-oracle/
├── sync/           # engine/, sdk-kit/ (Turso Cloud sync)
├── sdk-kit/        # High-level SDK abstraction
└── tools/          # dbhash utility, thesis (local Antithesis runner)
```

## Where to Look

| Task | Location | Notes |
|------|----------|-------|
| Query execution | `core/vdbe/execute.rs` | 12k LOC bytecode interpreter |
| SQL compilation | `core/translate/` | AST → bytecode, optimizer in `optimizer/` |
| B-tree/pages | `core/storage/btree.rs` | 10k LOC, SQLite-compatible format |
| WAL/durability | `core/storage/wal.rs` | Write-ahead log, checkpointing |
| SQL parsing | `sqlite/parser/src/parser.rs` | 11k LOC recursive descent |
| Add extension | `extensions/core/` | ExtensionApi, scalar/aggregate/vtab traits |
| Add binding | `bindings/` | PyO3, NAPI, JNI, FRB, CGO patterns |
| Deterministic tests | `testing/simulator/` | Fault injection, differential testing |
| New SQL tests | `sqlite/conformance/sqlite-sqltests/` | `.sqltest` format preferred |
| Quick sqlite3 diff | `scripts/diff.sh` | Compare sqlite3 vs tursodb output for a query |
| MVCC testing REPL | `cli/mvcc_repl.rs` | Multi-conn concurrent txn testing REPL        |
| User docs | `docs/sql-reference/` | statements/, functions/, pragmas.mdx, cli/ — update with any user-visible change |

## Guides

- **[Testing](docs/agent-guides/testing.md)** - test types, when to use, how to write
- **[Code Quality](docs/agent-guides/code-quality.md)** - correctness rules, Rust patterns, comments
- **[Debugging](docs/agent-guides/debugging.md)** - bytecode comparison, logging, sanitizers
- **[PR Workflow](docs/agent-guides/pr-workflow.md)** - commits, CI, dependencies
- **[Transaction Correctness](docs/agent-guides/transaction-correctness.md)** - WAL, checkpointing, concurrency
- **[Storage Format](docs/agent-guides/storage-format.md)** - file format, B-trees, pages
- **[Async I/O Model](docs/agent-guides/async-io-model.md)** - IOResult, state machines, re-entrancy
- **[MVCC](docs/agent-guides/mvcc.md)** - multi-version concurrency, snapshot isolation, checkpointing

## Commit Messages

Use an optional component scope followed by a lowercase imperative summary with
no trailing period:

```text
[scope: ]<imperative summary>

<why the change is needed and what invariant or bug it addresses>

<non-obvious implementation details or tradeoffs, if needed>

Tests: <relevant validation, if useful>

Fixes #1234
```

For example: `core/mvcc: preserve B-tree cleanup markers in commit logs`.
Explain intent rather than narrating the diff. Omit the body only when the
subject fully explains a trivial change. Conventional Commit prefixes such as
`feat(scope):` are not required. See [CONTRIBUTING.md](CONTRIBUTING.md) for a
complete example.

Before opening a PR, read the "PR Workflow" document linked above.

## Benchmark Naming

- Criterion benchmark functions must use `#[turso_macros::codspeed_criterion_benchmark]` so stable and nightly CodSpeed runs get distinct benchmark names.
- Divan benchmark functions must use `#[turso_macros::divan_bench]` for the same stable/nightly naming behavior.

## Core Principles

1. **Correctness paramount.** Production DB, not a toy. Crash > corrupt
2. **SQLite compatibility.** Compare bytecode with `EXPLAIN`
3. **MANDATORY: Every change MUST have a test. NO EXCEPTIONS.** Add or update a test that fails without the change and passes with it. You MUST run the test in both cases and make sure that it produces those results. Never skip this requirement or treat it as optional. Do not declare a change complete without evidence of both results. If you cannot run the test, report the blocker and leave the change incomplete.
4. **Assert invariants.** Don't silently fail. Don't hedge with if-statements
5. **Own your regressions.** If tests fail after your change, they are your regressions. Debug them directly. Never stash/revert to "check if they fail on main" — that wastes time and is categorically banned.
6. **Validate your hypotheses.** If you suspect a given cause for a bug, validate it and provide incontrovertible evidence. NEVER make unearned assumptions.
7. **Fix root causes, not symptoms** When designing a fix, ask yourself: is this a symptom of a deeper root cause? would my fix hide a problem that's created upstream?
8. **Driver API parity** Embedded (`bindings/rust`) and serverless (`serverless/rust`) drivers expose the same public API; add features to both in the same change. Spec: `serverless/conformance/differential/README.md`.
9. **Backward compatibility** No breaking changes. New versions of Turso must work with databases and sidecar files that were created from previous versions.  
10. **Keep user docs current.** If a change adds, removes, or changes user-visible SQL behavior (syntax, functions, pragmas, data types, CLI commands, errors), update the matching page in `docs/sql-reference/` in the same change. For example, a new `JOIN` type goes in `docs/sql-reference/statements/select.mdx`.

## Always use plain language instead of complex jargon

OOGA BOOGA! Programming already complex! Use simple word! Say what you mean! Examples:

```diff
-    /// Number of generated statements outside the engines' shared executable domain.
+    /// Number of statements skipped because EXPLAIN failed in at least one engine.

...

-    fn empty_schema_only_selects_bootstrap_safe_statements() {
+    fn empty_schema_never_chooses_a_statement_that_needs_a_table() {
```

No-one knows what the hell a bootstrap-safe statement is. Everyone knows what "a statement that needs a table" is. Do
not use metaphorical language, such as the following terms: load-bearing, pin, bite, sharp, arm, guard, bless, wedge,
retire, retarget, answer, settle, carry, land, honor, rebase, hold.
Do not make up terms if they have equivalents that are commonly used in the domain.

## Code flows from top to bottom

A reader should be able to read a file from the top down without jumping
ahead to find what a name means. The general rule: place a function after
all of its call sites. Callers come first, callees follow.

```text
pub fn commit()         // entry point
fn write_frames()       // called by commit
fn sync_wal()           // called by write_frames
```

When adding a helper, put it below the functions that call it, not at the
end of the file or wherever the cursor happened to be.

## Do not add comments

- Do not add comments. Instead, focus on making your code expressive.

## CI Note

Running in GitHub Action? Max-turns limit in `.github/workflows/claude.yml`. OK to push WIP and continue in another action. Stay focused, avoid rabbit holes.

## MySQL 8.4 Frontend Project

The repository contains an active MySQL-compatible frontend bootstrap under
`mysql/`. Treat this section as the handoff and operating contract for that
work.

### Managed Agent Workflow

The primary coding assistant acts as the integration manager for this project.
It owns scope, sequencing, acceptance criteria, the primary worktree, and final
integration. Implementation agents do not integrate their own work.

Use the following process:

1. The manager selects a coherent vertical slice and records explicit supported
   and rejected SQL shapes plus executable acceptance tests.
2. A read-only scout may inspect parser/core paths and recommend the smallest
   reusable implementation. Scouting is optional when the path is already
   clear; it must not replace implementation work.
3. One implementation worker operates in an isolated `pi/*` branch and
   worktree. The worker commits only its slice and never edits the primary
   integration worktree.
4. Core, planner, IR, VDBE, storage, or dialect-boundary changes require an
   architecture check or independent review. Prefer a neutral engine invariant
   over a MySQL-specific exception.
5. The manager inspects the complete diff, verifies test evidence, and
   cherry-picks accepted commits. An agent saying that work is done is not
   acceptance.
6. Run focused tests while developing, then the complete `mysql/test.sh` gate
   after integration. Record any deliberately skipped or interrupted gate
   explicitly; never describe a checkpoint as complete.
7. Use an independent reviewer every few accepted slices and immediately for
   shared-core changes. Only the manager applies review fixes or delegates them
   back to the owning worker.

This host is resource constrained, so only one mutating or compiling worker may
run at a time. Keep Cargo at one build job with debug information and
incremental compilation disabled as documented below. Prefer substantial
embedded SQL slices over microfeatures, and batch prose/curriculum polish at
milestones unless stale documentation would mislead the next implementer.
Manager state and prompts may also be tracked outside the repository under
`/home/deploy/turso-agent-manager/`; `AGENTS.md` remains the durable repository
handoff.

### Current Managed-Cycle Status

- Cycle 1, anonymous projection parameters, is integrated.
- Cycle 2, basic executable `CREATE TABLE`, is integrated on top of upstream
  `main` without any shared-core changes.

Parser policy (mirrors the PostgreSQL frontend): `MySqlDialect::parse` falls
back to SQLite parsing whenever the MySQL parser or lowering cannot handle the
text. Engine-generated helper SQL is canonical SQLite text and depends on this
fallback, so no statement-origin plumbing exists in core. The consequence is
that SQL the MySQL layer does not lower yet may still execute with SQLite
semantics; the recorded MySQL spec corpus, not hand-written rejection tests, is
what catches any resulting divergence.

Priority: specs first. Grow the recorded MySQL 8.4.10 spec corpus and make it
pass. Hand-written unit tests are not a goal in themselves.

### Desired Outcome

Build application-facing MySQL 8.4 compatibility on Turso without forking the
storage engine or replacing embedded operation:

1. Parse MySQL with an established parser.
2. Strictly lower the MySQL AST into `turso_parser::ast`.
3. Reuse Turso binding, planning, optimization, and VDBE execution.
4. Add neutral core capabilities or generic VDBE instructions only when
   correctness requires them.
5. Make the embedded SQL path useful before adding a MySQL wire adapter.
6. Use a large, source-attributed specification corpus as an implementation
   curriculum, not merely a small collection of hand-written unit tests.
7. Eventually support ordinary MySQL drivers and ORMs with correct values,
   diagnostics, metadata, affected rows, session behavior, and protocol
   transcripts.

The target is application compatibility. InnoDB file compatibility,
replication, binlogs, plugins, and MariaDB-specific compatibility are out of
scope unless the project explicitly changes that decision.

### Frozen Compatibility Target

The oracle is MySQL 8.4.10 LTS. Exact values are in `mysql/TARGET.md` and
`mysql/conformance/target.toml`:

- Source tag: `mysql-8.4.10`
- Source commit: `6adc159923b7b6abbe649949551ec25264c2daf9`
- Docker image:
  `mysql:8.4.10@sha256:8dbcf531a03aade657e181b9cf2f1d1803ce621a1d55610cb44cb531ab7d7db6`
- Default MySQL 8.4 `sql_mode`
- Server charset: `utf8mb4`
- Server collation: `utf8mb4_0900_ai_ci`
- Time zone: UTC (`+00:00`)
- `lower_case_table_names=0`

Do not silently change the target profile or use MariaDB as the behavioral
oracle.

### Architecture Decisions

- Parser: Apache-2.0 `sqlparser-rs` 0.62 with `MySqlDialect`.
- Lowering target: shared `turso_parser::ast`, not a parallel planner or VDBE.
- Catalog/schema: the bootstrap `MySqlDialect` delegates to the existing SQLite
  schema and catalog implementation.
- Execution: embedded `Database`/`Connection` first; wire protocol is a later
  adapter over the same path.
- Unsupported syntax policy: faithfully lower it, explicitly document it as a
  compatibility no-op, or reject it. Never discard syntax silently.
- Parser acceptance is not execution compatibility. Report parse, lower,
  prepare, execute, values, diagnostics, affected rows, metadata, and wire
  behavior separately.

`ENGINE=InnoDB` may eventually be an explicitly documented compatibility
no-op. Semantically relevant features such as `UNSIGNED`, charset, collation,
generated columns, and partitioning must not be ignored.

The parser selection was informed by a MySQL MTR bake-off over 1,457 files and
104,903 SQL occurrences. `sqlparser-rs` accepted 88,402 of 100,673 literal
statements (87.81%) with zero panics; the evaluated alternative accepted
approximately 70.66%.

### Implemented Bootstrap

Workspace packages:

- `mysql/parser` (`turso_mysql_parser`)
- `mysql/frontend` (`turso_mysql`)
- `mysql/cli` (`tursomysql`)
- `mysql/conformance`

Implemented APIs and behavior include:

- `parse`, `parse_one`, `translate`, and `parse_and_translate`
- `split_statements`/`first_statement`: MySQL-aware statement splitting with
  exact byte offsets; `MySqlDialect::parse` consumes one statement per call,
  so multi-statement text runs through `Connection`
- Strict MySQL AST-to-Turso AST lowering
- In-memory and file-backed embedded execution through a `Dialect`
  (`open_memory`, `open_file`, `open_with_io`)
- A CLI that executes supplied SQL or a default hello-world query
- Projection metadata and aliases, including backtick aliases
- Explicit errors for unsupported statement and query shapes
- Connection usability after prepare errors

The accepted execution slice remains intentionally narrow: literal and
anonymous `?` parameter `SELECT` projections. It handles `NULL`, booleans,
`utf8mb4` strings, aliases, non-negative integer literals representable exactly
as `i64`, and embedded binding of `NULL`, `i64`, and UTF-8 text. Parameters are
indexed from 1 in encounter order; numbered and named placeholders, parameter
expressions, CLI binding, missing-argument diagnostic parity, and wire behavior
remain unsupported.

The pending cycle-2 checkpoint additionally lowers the strict basic
`CREATE TABLE` subset described above. Until its remaining gate and review are
complete, treat that implementation as checkpointed rather than accepted.
Unary negative expressions, decimals, scientific notation, unsigned overflow,
identifiers in queries, functions, `FROM`, filtering, all other DDL, and DML
are not yet lowered. Decimal and unsigned literals that need exact MySQL
semantics are rejected rather than rounded or coerced through SQLite semantics.

`SELECT 1 FOR UPDATE` is a known `sqlparser-rs` MySQL parser gap and has an
explicit regression assertion. Keep known valid-MySQL parser gaps distinct
from lowering rejections.

### Immediate SQL Implementation Curriculum

The first useful vertical slice remains:

```sql
CREATE TABLE users(id INTEGER PRIMARY KEY, name VARCHAR(100));
INSERT INTO users VALUES (1, 'Ada');
SELECT name FROM users WHERE id = ?;
```

Recommended implementation order:

1. Core identifiers, expressions, and parameters.
2. `CREATE TABLE` with core integer and string types.
3. `INSERT ... VALUES` and affected-row accounting.
4. Table scans and `WHERE` predicates.
5. Ordering and limits.
6. Constraints and indexes.
7. Updates and deletes.
8. Transactions.
9. Joins, subqueries, grouping, and functions.
10. `AUTO_INCREMENT`, last-insert ID, and upsert semantics.
11. Exact decimal/unsigned, temporal, string/collation, and JSON semantics.
12. Introspection, warnings, diagnostics, and session state.
13. MySQL wire protocol and driver/ORM suites.

The rough feature DAG and stage statuses are in
`mysql/conformance/curriculum.toml`. Update it when behavior actually becomes
covered; parser acceptance alone does not make an execution feature covered.

### Owned Test Gate

Run the complete deterministic local gate with:

```bash
CARGO=/path/to/cargo ./mysql/test.sh
```

It performs:

- Root and standalone-miner formatting checks
- Python miner unit tests
- Validation of the tracked mined corpus
- Parser, frontend, and CLI Rust tests
- Standalone MTR miner tests
- Clippy over every MySQL target with warnings denied
- A direct `tursomysql` smoke test

The host used for this work has limited memory. Preserve these defaults for
large Rust invocations:

```bash
export CARGO_BUILD_JOBS=1
export CARGO_PROFILE_TEST_DEBUG=0
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_INCREMENTAL=0
```

Without dev/test debug suppression, compiling `turso_core` for Clippy or tests
may be killed by the OOM killer. Two existing `turso_core` unused-import
warnings are unrelated to the MySQL work.

The optional differential tier runs owned value cases against the pinned
MySQL container:

```bash
CARGO=/path/to/cargo ./mysql/conformance/differential.py
```

The current 13 differential cases pass against MySQL 8.4.10. They cover
integers, booleans, `NULL`, strings and escaping, `utf8mb4`, mixed projections,
aliases, and comments.

### Quantity-First Specification Mining

The mining system is under `mysql/conformance/corpus/` and
`mysql/conformance/tools/`. It intentionally prioritizes many source-attributed
requirements over a perfect hand-curated curriculum.

Shared corpus properties:

- JSONL schema: `mysql/conformance/corpus/schema-v1.json`
- Pinned sources: `mysql/conformance/corpus/sources.lock.toml`
- Stable source-derived case IDs
- Original repository, revision, file, line, command role, and license
- Syntax expectation and original source outcome
- Connection/dynamic/order context
- Rough generated level, feature tags, and complexity
- Shards of 1,000 records
- Validation that rejects malformed records and duplicate IDs

Generated levels are an approximate ordering mechanism. Do not delay bulk
imports to perfect them. Improve classification independently while keeping
case IDs stable.

#### Tracked go-mysql-server Corpus

`mysql/conformance/corpus/go-mysql-server/` contains 5,000 checked-in cases
mined from Apache-2.0 go-mysql-server enginetest fixtures at commit
`478bed778dd282aa13aa48ebd75b0549ebe75698`:

- 67 Go fixture files scanned
- 13,130 static query assertions extracted
- 5,004 setup statements extracted
- 13,519 unique candidates after deduplication
- 4,494 positive syntax expectations
- 506 source error cases kept as `unknown` pending MySQL-oracle classification

Regenerate the exact sample:

```bash
CARGO=/path/to/cargo ./mysql/conformance/mine-go-mysql-server.sh
```

Mine every currently extractable case:

```bash
GO_MYSQL_SERVER_LIMIT=0 \
CARGO=/path/to/cargo \
  ./mysql/conformance/mine-go-mysql-server.sh
```

The importer uses Go lexical structure but does not require a Go toolchain. Its
output was verified to regenerate byte-for-byte from the pinned source.
go-mysql-server source expectations are useful input but are not authoritative
MySQL behavior; oracle-classify them before promoting error or value semantics.

#### Local Official MySQL MTR Corpus

`mysql/conformance/mine-mtr.sh` uses the standalone Apache-2.0 `mtparser` tool
in `mysql/conformance/tools/mtr-miner/` to read a pinned external MySQL source
checkout. The initial run found:

- 1,457 mysqltest files
- 104,903 SQL commands
- 100,719 static SQL commands
- 4,151 commands requiring mysqltest variable evaluation
- 59,634 unique static cases after exact SQL/expectation deduplication

The default balanced sample emits 5,000 cases: 4,923 positive syntax
expectations and 77 negative syntax expectations.

```bash
CARGO=/path/to/cargo ./mysql/conformance/mine-mtr.sh
```

Mine all eligible static MTR cases:

```bash
MYSQL_MTR_LIMIT=0 \
CARGO=/path/to/cargo \
  ./mysql/conformance/mine-mtr.sh
```

Official MTR files are GPL-2.0-only. Generated MTR SQL is intentionally ignored
under `mysql/conformance/corpus/local/` and must not be committed or
redistributed until licensing is reviewed. Keep using a pinned external sparse
checkout in the meantime.

The static miner understands mysqltest statements, error directives,
connection switches, sorted-result directives, conditional blocks, and literal
versus interpolated SQL. Dynamic `$variable` statements are counted but omitted
by default. Full dynamic MTR mining should instrument the upstream `mysqltest`
runtime rather than reimplement its complete DSL.

#### Parser Progress Reporter

Run a corpus without making unfinished features fail the ordinary gate:

```bash
cargo run -p turso_mysql_parser --example corpus -- \
  mysql/conformance/corpus/go-mysql-server
```

Useful options include `--through LEVEL`, `--frontier N`,
`--json-report PATH`, and `--strict`. The default mode reports progress and the
simplest implementation frontier; parser panics always fail. Use `--strict`
only for a slice expected to be complete.

Baselines from the initial 5,000-case samples:

- go-mysql-server: 4,271/4,494 graded cases passing (95.04%), 506 unknown,
  zero panics
- MySQL MTR: 4,260/5,000 passing (85.20%), zero panics

These percentages measure syntax only. Do not describe them as overall MySQL
compatibility.

### Next Mining Layer: Behavioral Oracle Specs

Bulk syntax extraction is only the first layer. The next mining work should:

1. Preserve go-mysql-server `ScriptTest` setup/assertion groupings instead of
   only flattening SQL strings.
2. Run each candidate in a fresh database against the pinned MySQL 8.4.10
   container.
3. Capture typed values without lossy string conversion, column names and type
   metadata, affected rows, last insert ID, warnings, errno, SQLSTATE, message,
   and transaction/session effects.
4. Repeat candidates in independent databases and quarantine nondeterministic
   results.
5. Never promote an accidental missing-table or missing-setup failure as an
   expected diagnostic.
6. Keep every candidate in an explicit bucket: promoted, negative,
   nondeterministic, missing setup, unsupported environment, source disagrees
   with MySQL, dynamic pending, duplicate, or out of scope.
7. Add sqllogictest and fixed-seed SQLancer importers after structured
   go-mysql-server scripts.
8. Add a protocol recorder for MySQL driver and ORM suites only when the wire
   adapter becomes relevant.

Expected outputs should be checked in after oracle capture and human review so
ordinary curriculum runs remain deterministic and do not require a live MySQL
server.

### Correctness and Maintenance Rules

- Never weaken a case merely because implementation work makes it start
  succeeding; promote it from rejection/frontier coverage to positive
  execution coverage.
- Never silently skip mined candidates. Report extraction, promotion,
  quarantine, duplicate, and out-of-scope counts.
- Keep source revisions and licenses in each generated record.
- Preserve checked-in go-mysql-server attribution and license files under
  `licenses/mysql/` and `NOTICE.md`.
- Keep MTR-generated SQL local until licensing review permits otherwise.
- Add owned, focused regressions for bugs even when a mined case already
  catches them; mined corpora provide breadth, while owned tests explain the
  intended invariant.
- Test persistence-sensitive DDL by close/reopen/reparse, not only through an
  in-memory connection.
- Do not begin wire-specific implementation merely to increase protocol test
  counts before the embedded SQL vertical slice works.
