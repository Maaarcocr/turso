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
- MySQL rejects some DDL that SQLite accepts (for example `INT DEFAULT 'abc'` or `TEXT DEFAULT 'x'`). The lowering refuses such statements, but the frontend then parses them as SQLite, which can accept them.
- Not lowered: `TEMPORARY` tables, `CREATE TABLE ... LIKE`/`AS SELECT`, qualified `db.table` names, `ZEROFILL`, `BINARY(n)`, spatial types, `FULLTEXT`/`SPATIAL` keys, functional key parts, unique keys on column prefixes, `ON UPDATE CURRENT_TIMESTAMP`, `INVISIBLE`, `AUTO_INCREMENT=n`, partitioning, `ALTER TABLE` with several operations or `FIRST`/`AFTER`, `MODIFY`/`CHANGE`/`ALTER COLUMN`, adding or dropping keys and constraints with `ALTER TABLE`, `TRUNCATE` (it also resets `AUTO_INCREMENT` in MySQL), and `CREATE INDEX` with prefix lengths.
