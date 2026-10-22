# MySQL compatibility

Target: MySQL 8.4.10 with the profile in [`TARGET.md`](TARGET.md).

| Area | Parse | Lower | Execute | Metadata | Wire |
|---|---|---|---|---|---|
| `SELECT` signed integer literals | Yes | Yes | Yes | Partial | No |
| `SELECT` strings, booleans, and `NULL` | Yes | Yes | Yes | Partial | No |
| Projection aliases and backticks | Yes | Partial | Yes | Partial | No |
| Anonymous `?` projection parameters | Yes | Partial | Partial | Partial | No |
| Decimal and unsigned literals | Yes | Explicit error | No | No | No |
| Expressions and functions | Yes | Explicit error | No | No | No |
| `FROM`, filtering, grouping, ordering, limits | Yes | Explicit error | No | No | No |
| DDL and DML | Yes | Explicit error | No | No | No |
| Transactions | Yes | Explicit error | No | No | No |
| MySQL wire protocol | N/A | N/A | N/A | N/A | No |

Anonymous parameters are limited to standalone projection items in the embedded API and are indexed from 1 in encounter order. Callers must bind every slot with `Statement::bind_at`; MySQL missing-argument diagnostics, numbered and named placeholders, parameter expressions, CLI binding, and wire behavior are not supported.

The detailed dependency graph is in [`conformance/curriculum.toml`](conformance/curriculum.toml). The compatibility matrix advances only when owned execution tests pass; unsupported syntax must continue producing an explicit error until then.

## DDL

The matrix row above predates this section: DDL is now partly lowered and executed as described here.

`CREATE TABLE`, `CREATE [UNIQUE] INDEX`, `DROP INDEX ... ON`, `DROP TABLE [IF EXISTS]` (one table), `RENAME TABLE` (one table), and single-operation `ALTER TABLE` (`ADD [COLUMN]` without `FIRST`/`AFTER`, `DROP [COLUMN]`, `RENAME COLUMN`, `RENAME [TO|AS]`, `DROP {INDEX|KEY}`) are lowered to Turso DDL. The lowered table is stored as canonical SQLite text in `sqlite_schema`, so it reloads after reopen with the same rules. Anything else falls back to the SQLite parser like any other statement the frontend does not lower.

Column types keep the MySQL name as the Turso declared type (so affinity follows the usual SQLite rules: integers get INTEGER affinity, strings TEXT, `FLOAT`/`DOUBLE` REAL, decimal and temporal types NUMERIC), and the MySQL value rules are enforced with `CHECK` constraints, so an insert or update that MySQL's strict mode rejects fails instead of storing a different value:

| MySQL type | Stored rule |
|---|---|
| `TINYINT`/`SMALLINT`/`MEDIUMINT`/`INT`/`BIGINT` (`SIGNED` or `UNSIGNED`), `BOOL` | the type's exact range; display widths are ignored, as MySQL 8.4 does without `ZEROFILL` |
| `BIGINT UNSIGNED`, `BIT(64)` | not negative (see gaps) |
| `BIT(n)` | `0 .. 2^n - 1` |
| `DECIMAL`/`FLOAT`/`DOUBLE` `UNSIGNED` | not negative |
| `FLOAT(M,D)`/`DOUBLE(M,D)` | at most `D` decimals and fewer than `M - D` integer digits; a value MySQL would round is rejected |
| `CHAR(n)`/`VARCHAR(n)` | at most `n` characters, not counting trailing spaces |
| `TINYTEXT`/`TEXT`/`MEDIUMTEXT`, `VARBINARY(n)`, `TINYBLOB`/`BLOB`/`MEDIUMBLOB` | the type's byte limit |
| `JSON` | `json_valid`; the declared type is `JSON TEXT`, so documents keep TEXT affinity and stay text |
| `ENUM(...)` | one of the members (stored as `TEXT`) |
| `SET(...)` (at most 6 members) | one of the member combinations written in definition order (stored as `TEXT`) |
| `DATE`, `DATETIME(p)`, `TIMESTAMP(p)`, `TIME(p)`, `YEAR`, `LONGTEXT`, `LONGBLOB`, `DECIMAL(p,s)` | none |

`AUTO_INCREMENT` is available on an integer column that is the whole primary key; it becomes a rowid alias with `AUTOINCREMENT`, so `NULL` or an omitted value takes the next value and a deleted maximum is never reused. A MySQL integer primary key without `AUTO_INCREMENT` is not a rowid alias: it rejects `NULL` like MySQL. Primary key columns are `NOT NULL`. A `NOT NULL` `ENUM` without a default takes its first member, as in MySQL.

`DEFAULT` literals are checked against the column type the way MySQL checks them and stored in the form MySQL stores them (integers rounded, `DECIMAL(p,s)` and `FLOAT(M,D)` rounded to their scale, numbers in string columns kept as written, dates and times normalized to `YYYY-MM-DD HH:MM:SS[.fraction]`, `ENUM`/`SET` members spelled as declared, trailing spaces removed for `CHAR`). `CURRENT_TIMESTAMP`/`NOW()` defaults are available for `DATETIME` and `TIMESTAMP` without fractional seconds, and parenthesized expression defaults are lowered as expressions. A default MySQL rejects, or one that needs rounding of fractional seconds, is not lowered.

`CREATE TABLE` keys are validated like InnoDB does: a `BLOB`/`TEXT` key part needs a prefix length, `JSON` columns cannot be key parts, a prefix must fit the column and be on a string, a key is at most 3072 bytes, key names are distinct, and key columns must exist.

DDL that MySQL itself rejects fails with an error instead of falling back to the SQLite parser (which would often accept it): invalid defaults, invalid keys, a second primary key, an `AUTO_INCREMENT` column outside every key, `NULL` primary key columns, out-of-range type parameters (`VARCHAR(16384)`, `CHAR(256)`, `DECIMAL(5,6)`, `DATETIME(7)`, `BIT(65)`), duplicate `ENUM` members, a collation on a binary string, an index named `PRIMARY`, and identifiers longer than 64 characters. Other DDL MySQL rejects is not detected yet when its SQLite reading is valid, for example foreign keys whose parent table or column types do not match.

Compatibility no-ops (accepted, with no effect in Turso):

- `ENGINE=InnoDB` (other engines are not lowered), `DEFAULT CHARSET`/`CHARACTER SET` `utf8mb4`, `ROW_FORMAT` `DEFAULT`/`DYNAMIC`/`COMPACT`/`REDUNDANT`, `STATS_PERSISTENT`/`STATS_AUTO_RECALC`/`STATS_SAMPLE_PAGES`, `AUTO_INCREMENT=1`, and table `COMMENT`.
- Column and index `COMMENT`, `USING BTREE`/`USING HASH` (InnoDB also builds a B-tree for `HASH`), and `ALGORITHM=`/`LOCK=` on `CREATE INDEX`.
- `CHARACTER SET utf8mb4` and `COLLATE` `utf8mb4_0900_ai_ci`, `utf8mb4_general_ci`, `utf8mb4_unicode_ci`, `utf8mb4_0900_bin`, `utf8mb4_bin` on columns and tables (see gaps).
- Non-unique `KEY`/`INDEX` inside `CREATE TABLE`: secondary indexes do not change query results. The index is not created, so `DROP INDEX` on it fails.
- Inline column `REFERENCES`, which MySQL itself parses and ignores.
- `CHECK (...) NOT ENFORCED`, which MySQL does not check.
- `RESTRICT`/`CASCADE` on `DROP TABLE`, which MySQL ignores.

Known gaps:

- Collations: Turso compares strings byte by byte, so columns declared (or defaulting to) a case- or accent-insensitive collation compare, sort, group and enforce uniqueness case-sensitively. Only the `_bin` collations match exactly (`utf8mb4_bin` also ignores trailing spaces in MySQL).
- `BIGINT UNSIGNED` and `BIT(64)` values above 9223372036854775807 are stored as approximate floating-point values.
- `DECIMAL(p,s)` is not exact: values are not rounded to the scale and are shown without trailing zeros; `FLOAT` is double precision; `FLOAT(M,D)` values are shown without trailing zeros.
- Temporal values are stored as given, without MySQL's validation, normalization, or `TIMESTAMP` time-zone conversion (only defaults are normalized). `CHAR` values keep trailing spaces.
- `ENUM` sorts and compares by text, not by member position; numeric `ENUM` and `SET` values (member indexes and bitmasks) and member names in another letter case are rejected instead of converted; `SET` values must list members in definition order.
- Inserting `0` into an `AUTO_INCREMENT` column fails instead of taking the next value, and updating such a column to `0` fails; unlike InnoDB, a failed insert does not use up a value.
- `FOREIGN KEY` constraints are stored but Turso does not enforce them unless `PRAGMA foreign_keys` is on.
- Generated columns are lowered, but Turso only creates them with the experimental generated-columns option, which the MySQL frontend does not enable.
- Not lowered: `TEMPORARY` tables, `CREATE TABLE ... LIKE`/`AS SELECT`, qualified `db.table` names, `ZEROFILL`, `BINARY(n)`, spatial types, `FULLTEXT`/`SPATIAL` keys, functional key parts, unique keys on column prefixes, `ON UPDATE CURRENT_TIMESTAMP`, `INVISIBLE`, `AUTO_INCREMENT=n`, partitioning, `ALTER TABLE` with several operations or `FIRST`/`AFTER`, `MODIFY`/`CHANGE`/`ALTER COLUMN`, adding or dropping keys and constraints with `ALTER TABLE`, `TRUNCATE` (it also resets `AUTO_INCREMENT` in MySQL), and `CREATE INDEX` with prefix lengths.

## JSON functions

`mysql/frontend/functions/json/` implements MySQL's JSON functions for the
MySQL dialect, replacing SQLite's functions of the same name: `JSON_ARRAY`,
`JSON_OBJECT`, `JSON_QUOTE`, `JSON_UNQUOTE`, `JSON_EXTRACT`, `JSON_VALUE`
(two-argument form), `JSON_TYPE`, `JSON_VALID`, `JSON_KEYS`, `JSON_LENGTH`,
`JSON_DEPTH`, `JSON_PRETTY`, `JSON_CONTAINS`, `JSON_CONTAINS_PATH`,
`JSON_OVERLAPS`, `JSON_SEARCH`, `JSON_SET`, `JSON_INSERT`, `JSON_REPLACE`,
`JSON_REMOVE`, `JSON_ARRAY_APPEND`, `JSON_ARRAY_INSERT`, `JSON_MERGE`,
`JSON_MERGE_PRESERVE` and `JSON_MERGE_PATCH`. They follow MySQL 8.4.10:

- JSON text is parsed like MySQL's RapidJSON parser, including its error
  positions, the nesting limit of 100, the last duplicate key winning, and its
  imprecise conversion of long decimal numbers to doubles. Numbers become
  `INTEGER`, `UNSIGNED INTEGER` (above 4294967295) or `DOUBLE`.
- Values print as MySQL prints them: `", "` and `": "` separators, object
  keys ordered by length and then by bytes, doubles in MySQL's notation
  (`1.0`, `1e15`, `0.0000001`, `1e-16`).
- Paths support members, quoted members, `.*`, `[n]`, `[last-n]`, `[*]`,
  `[m to n]` and `**`, with MySQL's rules for treating a non-array as a
  one-element array.

Turso has no JSON type. A JSON value is TEXT holding MySQL's printed form,
tagged with the JSON text subtype while it moves between expressions:

- A JSON function result passed to another JSON function keeps its type, so
  `JSON_ARRAY(JSON_OBJECT())` is `[{}]` while `JSON_ARRAY('{}')` is `["{}"]`.
- The tag is not stored. A JSON value read back from a table is a string,
  so `JSON_ARRAY(j)` quotes it where MySQL would embed a `JSON` column.
  A column declared `JSON` also has SQLite's numeric affinity, so a stored
  `1` reads back as an integer, which JSON functions reject (error 3146) as
  MySQL does for SQL numbers.
- Comparisons do not use JSON rules: `JSON_EXTRACT('{"a": "x"}', '$.a') =
  'x'` is false (MySQL: true) because the text is `"x"`, and a JSON number
  compares as text with SQL numbers.
- Turso has no `DECIMAL`, unsigned or temporal types, and `TRUE` is the
  integer 1, so these never become JSON decimals, unsigned integers, dates or
  booleans; binary strings become MySQL's `base64:type15:` opaque values.
- `JSON_OBJECT` rejects a double key with error 3144, as MySQL does; a
  `DECIMAL` literal key such as `1.0` is a double in Turso.

The lowering of syntax that MySQL implements with JSON functions can call
these internal functions, which MySQL does not have:

| MySQL syntax | Internal function |
|---|---|
| `CAST(x AS JSON)` | `__mysql_cast_as_json(x)` |
| A boolean expression (`TRUE`, `a = b`, ...) used as a JSON value, or `CAST(<boolean> AS JSON)` | `__mysql_cast_boolean_as_json(x)` |
| `x MEMBER OF (y)` | `__mysql_member_of(x, y)` |
| `doc -> path` | `JSON_EXTRACT(doc, path)` |
| `doc ->> path` | `JSON_UNQUOTE(JSON_EXTRACT(doc, path))` |

Not implemented: the aggregates `JSON_ARRAYAGG` and `JSON_OBJECTAGG` (a
dialect cannot define aggregate functions yet), `JSON_TABLE`,
`JSON_SCHEMA_VALID`, `JSON_SCHEMA_VALIDATION_REPORT`, `JSON_STORAGE_SIZE`,
`JSON_STORAGE_FREE` and the `RETURNING`/`ON EMPTY`/`ON ERROR` clauses of
`JSON_VALUE`.
