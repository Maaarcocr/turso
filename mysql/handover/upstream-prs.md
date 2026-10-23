# Upstream PR drafts: typed expressions

Before posting (delete): tick "allow edits from maintainers"; rewrite in your
own words (CONTRIBUTING). `<...>` = fill in after review fixes and cleanup.

---

## PR 1 — `core/schema: reject AUTOINCREMENT on a primary key that is not the rowid`

### Description

Turso accepts AUTOINCREMENT on a primary key declared `INT` (or `BIGINT`, …)
and then stores NULL ids:

```sql
CREATE TABLE t(id INT PRIMARY KEY AUTOINCREMENT, x);
INSERT INTO t(x) VALUES ('a'), ('b');
SELECT quote(id), x FROM t;     -- Turso: NULL|a  NULL|b
SELECT * FROM sqlite_sequence;  -- Turso: t|2
```

SQLite refuses the CREATE TABLE with `AUTOINCREMENT is only allowed on an
INTEGER PRIMARY KEY`; Turso now does too.

Only a primary key declared exactly `INTEGER` becomes the table's rowid (a
*rowid alias*). The check in `create_table` (`core/schema.rs`) compared the
column's affinity (the INTEGER/REAL/TEXT class SQLite derives from the type
name) with INTEGER instead, and `INT` has INTEGER affinity. So `id` was an
ordinary column: AUTOINCREMENT advanced the counter, but nothing wrote it into
`id`. The check now asks whether the column is the rowid alias, as SQLite does.

Existing databases: <what happens when opening a database where an older build
created such a table? SQLite reports a malformed schema.>

### Motivation and context

SQLite compatibility; today the key is silently lost. Deciding by rowid status
also prepares planned work that makes integer keys of other dialects (MySQL
`INT`, `BIGINT`) rowids.

Fixes #6940 <confirm same bug>

### Description of AI Usage

<...>

---

## PR 2 — Typed expressions for custom types

### Description

#### The problem

Custom types (`CREATE TYPE`; experimental in `tursodb
--experimental-custom-types`, always on in `tursopg`) work only when an
operand is a bare column. The compiler never works out the type of an
expression: `p + 1` uses numeric arithmetic but `(p + 1) + 2` uses REAL, a
numeric result is compared as text, and IN, CASE and UNION ignore the type.
"After" equals PostgreSQL 17 (Turso prints true as 1):

```sql
CREATE TABLE t(p numeric(20,2), i INTEGER) STRICT;
INSERT INTO t VALUES ('12345678901234567.89', 3);
SELECT (p + 1) + 2, p + i FROM t;
-- before: 1.23456789012346e+16 (both)   after: 12345678901234570.89 (both)

CREATE TABLE u(p numeric(10,2)) STRICT;
INSERT INTO u VALUES ('9.00'), ('10.00');
SELECT max(p * 1) FROM u;      -- before: 9.00 (text order)   after: 10.00

CREATE TABLE v(p numeric(10,2)) STRICT;
INSERT INTO v VALUES ('2.00');
SELECT p IN (2), CASE p WHEN 2 THEN 'yes' ELSE 'no' END FROM v;
                               -- before: 0|no                after: 1|yes
SELECT p FROM v UNION SELECT 2;  -- before: 2 and 2.00        after: 2.00
```

Also, `'1.505'` stored into `numeric(10,2)` was cut to `1.50` (now `1.51`).
The design doc lists ten such problems.

#### What changes

- **Every expression has a type at compile time**: a named type (built-in such
  as `numeric`, or from CREATE TYPE) or *dynamic* (SQLite's rules: the value
  decides at run time). Registers still contain ordinary SQLite values; the
  file format does not change. `tursopg` column metadata uses these types, so
  `p + 1` is reported as numeric.
- **CREATE TYPE declares behaviour**: `OPERATOR '+' (money, money) RETURNS
  money f`, prefix operators `OPERATOR '-' (money)`, `CAST FROM integer AS
  IMPLICIT`, `FUNCTION abs(money) RETURNS money f`, `KEY expr`, and parameter
  `DEFAULT`s. Old syntax keeps its meaning. Built-in types take behaviour from
  `Dialect::type_rules()`, so in `tursopg` `numeric + float8` is double
  precision, as in PostgreSQL.
- **Operator resolution**: an operator picks the signature that best fits its
  operand types (exact, then `any`, then an implicit cast; a tie is the error
  `operator is not unique`); if none fits, SQLite's operator, as today. A
  literal keeps its own value instead of taking the column's scale.
- **Every comparison uses the resolved operator**: IN lists, IN (SELECT …),
  row values, CASE, IS, nullif, scalar min/max, joins.
- **Keys**: a type's KEY gives bytes that are equal when values are equal and
  sort in the type's order (numeric: `numeric_key`). ORDER BY, DISTINCT, GROUP
  BY, UNION/INTERSECT/EXCEPT, hash joins and window partitions compare keys;
  output keeps the computed value.
- **Index lookups return the same rows as a scan**: the value looked up (the
  *seek key*) is converted as the scan converts it, then encoded. If encoding
  fails or changes it (`1.555` into `numeric(5,2)`), the lookup finds no row,
  like the scan. New instructions `ErrorTarget`/`ClearErrorTarget` let an
  encoding error jump to a label instead of failing the statement.
- **CAST** to a registered type with all parameters returns what a STRICT
  column of that type gives back: `CAST('1.50' AS numeric(10,2))` is `1.50`
  (was an 18-byte blob); `CAST('12abc' AS bigint)` is an error. Unregistered
  names, and `NUMERIC` without parameters in the SQLite dialect, keep SQLite's
  cast.
- **Numeric**: values are rounded half away from zero instead of cut;
  division keeps at least 16 significant digits (PostgreSQL's rule), not 100.
- Unary minus, `abs`, recursive CTEs and windows follow the same rules.

#### What does not change

With custom types off, the SQLite dialect produces identical bytecode: no type
is registered, so every operator resolves to SQLite's. Non-STRICT tables never
get a type. Checked by:

- EXPLAIN of every SELECT/INSERT/UPDATE/DELETE/WITH/REPLACE/VALUES statement in
  `sqlite/conformance/sqlite-sqltests`, on `main` and on this branch, each on a
  copy of the same database: <N> statements, 0 differences. Custom types on:
  <M> difference(s), all `CAST(… AS BOOL)`, now typed because `boolean` is a
  registered type (expected).
- The EXPLAIN snapshot tests (`CI=1 make -C sqlite/conformance run-rust`):
  unchanged.

#### Release note (custom types and `tursopg`)

> Operators, comparisons, sorting, grouping and DISTINCT follow the type of the
> whole expression, not only of bare columns. Numeric values are rounded half
> away from zero, and division keeps at least 16 significant digits. CAST to a
> custom type returns the value, not its stored bytes. Comparing a typed value
> with an invalid literal is an error (`date_col = 'garbage'`: `invalid date
> value`; before: false). CHECK constraints see decoded values. Materialized
> views that aggregate custom-type values or group by a type with a KEY, and
> RANGE window frames with an offset on a typed ORDER BY, are rejected for now.
> **After upgrading**, run `REINDEX` on expression indexes, partial indexes and
> indexes on virtual generated columns whose expression uses a custom type,
> and drop and recreate materialized views that use custom types: their stored
> values follow the old rules, so lookups can miss rows and UPDATE or DELETE
> can miss the old entry. Writing typed expressions into one database with
> both `tursodb` and `tursopg` is not supported. The `numeric_*` SQL functions,
> callable with custom types off, also round half away from zero and use the
> new division rule (`numeric_div('1','3')`: 20 digits after the point, not
> 100).

#### Trade-off for maintainers

Nothing detects indexes or views built by older builds; we rely on the release
note. Detection needs a version row in `__turso_internal_types`, finding
outdated indexes and views at schema load, making the planner skip them, and
refusing writes to their tables until REINDEX. Design v2 of the doc describes
it (<link to commit 86915929 on the fork; keep a branch or tag there, since
the doc commits get squashed>). We left it out: the problem needs an
experimental feature, a typed expression in a stored index or view, and a
database from an older build, and detection is a lot of core code. If you want
it, it can go in before the typing commits without changing the rest.

#### Not in this PR

- An attached database's own types are still ignored (Proposal 2).
- `sum`/`avg` of numeric still use REAL; index ranges and ORDER BY on numeric
  columns are not used, since stored numeric bytes do not sort as numbers
  (Proposal 4).
- The planner does not use indexes on typed expressions yet.
- <items left open by the review fixes>

#### Verification

Every commit builds, passes clippy, and adds a test that fails before it. On
the last commit: `cargo fmt`, clippy <result>; `cargo test` <result>; `make
test` <result>; `make -C sqlite/conformance run-rust` <result>; `make -C
postgres/conformance run-postgres` (values checked against PostgreSQL 17)
<result>; the EXPLAIN comparison above.

#### How to review

Read the design doc's "Problem", "Proposal 1" and "Implementation notes
(Proposal 1)", then the commits <update after cleanup>:

1. `docs:` the design (Proposals 1–4).
2. `core/numeric: round half away from zero when applying a scale` (small).
3. `core/types: compute expression types and report them in column metadata`:
   new `core/translate/expr/types.rs`; only metadata changes.
4. `parser: parse operator signatures, implicit casts, type functions, keys and
   parameter defaults`.
5. `core/types: resolve operators with dialect type rules and choose seek keys
   from the result`: the main commit. Review resolution, conversions,
   `ErrorTarget` and the optimizer's index rule closely.
6. `core/numeric: use PostgreSQL's scale rule for division`.
7. Resolved operators in IN, row values, CASE, IS, nullif, min/max and joins;
   then operand types computed once per operator.
8. Keys, five commits: ORDER BY; DISTINCT (adds `numeric_key`); GROUP BY; hash
   joins and IN (SELECT …); UNION/INTERSECT/EXCEPT. Check NULL handling.
9. Unary operators and type functions.
10. CAST returns the value read back from a STRICT column.
11. Recursive CTEs, windows, materialized views; then the doc's
    implementation notes.

### Motivation and context

Wrong numeric results today; a MySQL frontend needs the same machinery
(Proposal 2).

### Description of AI Usage

<...>

---

## Later PRs (outline)

### PR 3 — Dialect-defined types (Proposal 2)

- A frontend (MySQL first) puts its own column types on STRICT tables, with no
  MySQL code in core.
- Core writes and checks a table marker (`/* turso_dialect:NAME
  [rowid_alias] */` + SQLite text), kept through ALTER TABLE and VACUUM.
- A name table maps type names onto new core storage types (`ranged_integer`,
  `uint64`, `date_days`, `datetime_micros`, `time_micros`, `enumeration`,
  `label_set`); assignment conversions separate from CAST.
- Integer primary keys of marked tables are rowids, decided once and stored in
  the marker; keys range-checked; AUTOINCREMENT allowed.
- Declared types in metadata, ORDER BY by KEY, index ranges for types whose
  bytes sort correctly, error kinds with column and row.
- For every STRICT table with custom types: attached databases' own types,
  CHECK accepted like WHERE.
- Tested with a test dialect in core, across reopen, ALTER TABLE and VACUUM;
  same EXPLAIN comparison. Needs PR 1 and PR 2.

### PR 4 — PostgreSQL frontend (Proposal 3)

- Exact numeric literals and typed casts (`x::numeric(p,s)`); unconstrained
  numeric parameters default to NULL; bare `NUMERIC` columns stay REAL.
- ALTER TABLE keeps the PostgreSQL marker (today it stores plain SQLite text),
  using PR 3's marker.

### PR 5 — Typed aggregates and ordered numeric storage (Proposal 4)

- Type-declared exact `sum`/`avg`, also with `OVER`; `total` stays REAL.
- Numeric storage v2 whose bytes sort like the numbers; v1 stays readable;
  index ranges and ORDER BY then work on numeric columns.
- Release note: dump and reload old numeric data. May be two PRs.

---
## Upstream bugs found along the way (to file / fix separately)
1. AUTOINCREMENT accepted on non-rowid primary keys (`INT PRIMARY KEY AUTOINCREMENT` stores NULL ids). Upstream issue #6940. Fixed on core/typed-expressions as 8a95a532 (older databases still open).
2. `INTEGER(10) PRIMARY KEY` is treated as a rowid alias by Turso but not by SQLite; opening a sqlite3-created database with such a table panics. Not fixed yet (is_rowid_alias_type in core/util.rs). Needs a fix that never panics on valid databases and handles databases written by older Turso with the rowid layout.
3. Data loss: `ALTER TABLE t ALTER COLUMN id TO id2 INT` on `id INTEGER PRIMARY KEY` drops the PRIMARY KEY and the column reads NULL afterwards (all dialects). Repro: CREATE TABLE t(id INTEGER PRIMARY KEY, a INT); INSERT INTO t VALUES (5,1); ALTER TABLE t ALTER COLUMN id TO id2 INT; SELECT rowid,* FROM t → 5||1. Not fixed (core/typed-expressions refuses ALTER COLUMN of the rowid column for marked dialect tables only).
- Materialized views read custom-type columns without DECODE: `SELECT p FROM view` returns the stored blob (custom types, upstream main too).
- An error from a sorter comparator panics with "Memory allocation failed here" (being fixed on core/typed-expressions).
- BEFORE UPDATE triggers see the encoded value in NEW for typed columns the UPDATE does not set (custom types, upstream main too; being fixed on core/typed-expressions with the virtual-column fix).
- INSERT computes virtual generated columns from already-encoded values (`g INTEGER AS (d) VIRTUAL` over a numeric column fails); same root cause as above.
