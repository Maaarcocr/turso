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
├── specs/<source>/       recorded imported specs (ratcheted, see below)
│   ├── *.sqltest         shards of at most 1,000 tests
│   ├── manifest.jsonl    one record per test: id, file, level, provenance
│   └── REPORT.md         extraction/recording bucket counts for the source
├── candidates/           importer output: unrecorded candidate JSONL (ignored)
├── baseline.txt          ids of recorded specs that currently pass
└── tools/                importers, recorder, scoreboard
```

## Pipeline

1. **Import.** A source importer turns upstream tests into candidate JSONL
   (schema below). Importers never invent expected results.
2. **Record.** `tools/record_mysql.py` runs every candidate against the pinned
   MySQL container, twice in independent databases, and writes `.sqltest`
   shards plus `manifest.jsonl` and `REPORT.md`. Only the recorder talks to
   MySQL. `--verify` re-runs checked-in specs against MySQL and reports drift.
3. **Replay.** `make -C mysql/conformance run` replays specs through the
   `tursomysql` sqltest backend (embedded Turso with `MySqlDialect`).
4. **Score.** `tools/score.py` compares a replay with `baseline.txt`. A spec in
   the baseline that fails is a regression and fails the gate. A newly passing
   spec must be added with `--update` (the baseline only grows). It also
   reports pass counts per source and the highest fully cleared level.

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
  "features": ["select", "order-by"]
}
```

- `id` is stable and source-derived. Re-importing the same revision yields
  the same ids.
- `setup` statements run before `sql` in a fresh database. Candidates in one
  `group` with an identical `setup` share one named setup block.
- `ordered` is false when the source says the row order is unspecified (for
  example sqllogictest `rowsort`/`valuesort`, or no `ORDER BY`). Unordered
  specs use `expect unordered`.
- `level` (0–1000) orders specs from simplest to hardest, as in liquid-spec.
  Feature tags drive the level. Classification can improve later without
  changing ids.

## Recorded output format

```text
# source: sqllogictest@<rev>:test/select1.test:1234 (public domain)
@setup s3
test sqllogictest-select1-l1234 {
    SELECT a FROM t1 ORDER BY a;
}
expect {
    1
    NULL
}
```

Values use MySQL's text-protocol representation, which is what a MySQL client
receives:

| MySQL value | Rendered as |
|---|---|
| `NULL` | `NULL` |
| integers, `DECIMAL`, temporal values | MySQL's text exactly (`0.5000`, `2024-01-02`) |
| `DOUBLE`/`FLOAT` | MySQL's text exactly (`0.3333333333333333`, `1e21`) |
| strings | UTF-8 text; binary values that are not UTF-8 are lossily decoded |

A test whose statements fail in MySQL is recorded as `expect error {}` with
the MySQL errno, SQLSTATE and message in a comment. The runner checks only
that an error occurs; error-code parity comes later.

A test is not emitted, and is counted in `REPORT.md`, when:

- MySQL returns different results in the two independent runs
  (`nondeterministic`);
- a value contains `|`, a newline, leading or trailing whitespace, or the
  output cannot otherwise round-trip through the `.sqltest` expect syntax
  (`unrepresentable`);
- the result exceeds the row limit (`too-large`);
- the setup fails in MySQL (`setup-failed`), so a missing-setup error is never
  recorded as expected behavior;
- the importer could not extract it statically (`dynamic`), or it is a
  duplicate (`duplicate`).

## Licensing

Only permissively licensed sources are checked in. Each source's license is
recorded in every manifest record and in `NOTICE.md`. GPL sources (MySQL MTR,
MariaDB) are recorded into `specs/local/`, which is ignored by git and never
committed.
