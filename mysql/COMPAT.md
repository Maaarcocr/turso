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
| Transactions | Yes | Partial | Partial | No | No |
| MySQL wire protocol | N/A | N/A | N/A | N/A | No |

Anonymous parameters are limited to standalone projection items in the embedded API and are indexed from 1 in encounter order. Callers must bind every slot with `Statement::bind_at`; MySQL missing-argument diagnostics, numbered and named placeholders, parameter expressions, CLI binding, and wire behavior are not supported.

The detailed dependency graph is in [`conformance/curriculum.toml`](conformance/curriculum.toml). The compatibility matrix advances only when owned execution tests pass; unsupported syntax must continue producing an explicit error until then.

## DDL

The matrix row above predates this section: DDL is now partly lowered and executed as described here.

`CREATE [TEMPORARY] TABLE`, `CREATE TABLE ... [AS] SELECT`, `CREATE [UNIQUE] INDEX`, `DROP INDEX ... ON`, `DROP TABLE [IF EXISTS]` (one table), `RENAME TABLE` (one table), and single-operation `ALTER TABLE` (`ADD [COLUMN]` without `FIRST`/`AFTER`, `DROP [COLUMN]`, `RENAME COLUMN`, `RENAME [TO|AS]`, `DROP {INDEX|KEY}`, `CHANGE [COLUMN] old new definition` with a new name, as core's `ALTER COLUMN`, which converts the stored values through the new type and fails on a value the new type rejects, as MySQL does in strict mode (refused for a column of a primary or unique key and for a table with checks, references or generated columns, whose definitions core would drop; `MODIFY` and a `CHANGE` that keeps the name are refused, as core's `ALTER COLUMN` must rename), `ADD [UNIQUE] {INDEX|KEY} name (...)` and `ADD CONSTRAINT name UNIQUE (...)`, as `CREATE [UNIQUE] INDEX`; `ADD {INDEX|KEY} (c, ...)` without a name is the index named `c`, as MySQL names it, and is refused when an index is already named so, as MySQL adds a suffix; an unnamed unique key is refused) are lowered to Turso DDL. `CREATE INDEX` also takes functional key parts of columns and integer literals under `+`, `-` and `*` (`((c1 * 10))`), as an index on that expression; other functional key parts are refused. Turso's index names belong to the database and MySQL's to a table, so `DROP INDEX i ON t` and `ALTER TABLE t DROP INDEX i` look up the table of `i` in the session's schema when they are lowered, and fail as MySQL does (1091) when `i` is not an index of `t`; creating an index named like an index of another table is refused. Other DDL statements are refused with an unsupported-syntax error.

Typed tables are STRICT tables that core marks with `/* turso_dialect:mysql */` in `sqlite_schema` and keeps marked through `ALTER TABLE`, reopen and `RENAME TABLE`. Each column has a MySQL type from the frontend's type table (`mysql/frontend/types/`), which maps it onto a core storage type and says how a value is converted when it is stored (strict mode: a value MySQL rejects makes the statement fail, see `mysql_error` for the MySQL error codes):

| MySQL type | Stored as | Converting a stored value |
|---|---|---|
| `TINYINT`/`SMALLINT`/`MEDIUMINT`/`INT`/`BIGINT` (`SIGNED` or `UNSIGNED`), `BOOL` (`TINYINT(1)`) | integer within the type's range | text is read as a number (`'12'`, `' 7 '`, `'1e1'`; only spaces and tabs may come before it, and an exponent without digits ends it, so `'1e'` is 1); decimals and text round half away from zero (`2.5` → 3, `'-3.5'` → -4); text with other characters fails (1265), text without digits fails (1366), values outside the range fail (1264); display widths are kept in the declared type |
| `BIGINT UNSIGNED` | unsigned 64-bit integer (exact up to 18446744073709551615) | as above |
| `FLOAT`/`DOUBLE` [`UNSIGNED`] [`(M,D)`] | double | text is read as a number; other text fails (1265); out of range or negative `UNSIGNED` fails (1264); `(M,D)` rounds to `D` decimals as MySQL does and fails above `M` digits |
| `CHAR(n)`/`VARCHAR(n)` | text | at most `n` characters; extra trailing spaces are cut, other extra characters fail (1406); `CHAR` drops trailing spaces; numbers become MySQL's text (`1e20`); bytes that are not UTF-8 fail (1366) |
| `TINYTEXT`/`TEXT`/`MEDIUMTEXT`/`LONGTEXT` | text | as `VARCHAR`, with the type's limit in bytes |
| `BINARY(n)`/`VARBINARY(n)`, `TINYBLOB`/`BLOB`/`MEDIUMBLOB`/`LONGBLOB` | blob | at most the type's length in bytes, else 1406; `BINARY(n)` pads with zero bytes; text and numbers are stored as their bytes. Binary columns compare with text literals byte by byte |
| `GEOMETRY`, `POINT`, `LINESTRING`, `POLYGON`, `MULTI*`, `GEOMETRYCOLLECTION` (or `GEOMCOLLECTION`), with an optional `SRID 0` or `SRID 4326` | blob: MySQL's storage format, the SRID as four little-endian bytes and then little-endian WKB | a geometry of the column's kind (any for `GEOMETRY`) in that format; other values fail (1416), and a geometry whose SRID is not the column's fails (3643). Another `SRID` is refused |
| `DECIMAL(p,s)` [`UNSIGNED`] | integer when the value is a whole number in the signed 64-bit range, otherwise double (until exact decimals) | text is read as a number; other text fails with 1265 where MySQL reports 1366; negative `UNSIGNED` fails (1264) |
| `DATE`, `DATETIME(p)`, `TIMESTAMP(p)`, `TIME(p)` | text in MySQL's form: `YYYY-MM-DD`, `YYYY-MM-DD HH:MM:SS[.fraction]`, `[-]HH:MM:SS[.fraction]`, with the column's `p` fractional digits | a string of numbers separated by `-`, `:`, a space or `T` is normalized (`2024-3-5 9:4:5` is `2024-03-05 09:04:05`, `10:00:00.5` in `DATETIME(6)` is `10:00:00.500000`, a `DATE` takes a datetime whose time is zero); fractional digits beyond `p` are rounded as MySQL rounds them, to microseconds by the seventh digit and then half up (`23:59:59.5` in a `DATETIME` is the next day); an invalid or zero date, a time out of range and a `TIMESTAMP` outside 1970-2038 fail like MySQL (1292); a rounding past year 9999 or `838:59:59`, a negative zero time, a time in a `DATE`, other string forms, numbers, binary strings and JSON values are refused |

A `DATE`, `DATETIME` or `TIMESTAMP` value compares with a value of its own type as text, and with a string or a value of the other two types as the datetimes MySQL reads them as (`d = '2020-1-2'` is true for `2020-01-02`, `d = dt` compares `2020-01-02` as `2020-01-02 00:00:00`). A string MySQL's datetime parser rejects fails (1525, `d >= 'not a valid date'`); a zero date or a date with a zero month or day, a number, a binary string, a JSON value and a `TIME` value are refused there.
| `YEAR` | integer from 0 to 2155 | as the integer types |
| `BIT(n)` | integer from 0 to 2^n - 1 (at most 9223372036854775807 for `BIT(64)`) | as the integer types; values outside the range fail (1406). MySQL reads text as a binary string, Turso as a number |
| `JSON` | text: the document in MySQL's form (`{"a": [1, 2], "b": 2, "aa": true}`: keys ordered by length and then bytes, the last of duplicate keys kept, `", "` and `": "` between items) | text must be a JSON document (3140); numbers fail as in MySQL (3140, "may need CAST") and binary strings with 3144. JSON functions read a value of the column as a JSON document, as they read the result of another JSON function (`JSON_ARRAY(j)` nests the document). Comparisons of the column with other values, `ORDER BY` and `MIN`/`MAX` still compare its text, where MySQL compares JSON values |
| `ENUM(...)` | the member position, read as the member's text: an assigned value becomes the member it names (ignoring trailing spaces, and ASCII letter case unless the column's collation is case-sensitive) or the member at its position (an integer, or a string of at most five digits); anything else fails (1265) | numbers become text |
| `SET(...)` | the bitmask, read as the members' text: an assigned value becomes the members it names, or those of the bits of an integer, listed in definition order; anything else fails (1265) | numbers become text |

The conversion also applies to `UPDATE`, `INSERT ... SELECT`, defaults, generated columns and rows copied by `ALTER TABLE`. `CAST` to these type names keeps SQLite's conversion for now.

A single integer primary key column is the table's rowid, so rows are read in primary key order, as InnoDB returns them. The exception is a `BIGINT UNSIGNED` key without `AUTO_INCREMENT`: it is an ordinary primary key, so it can hold values above 9223372036854775807, and rows are read in insertion order. `AUTO_INCREMENT` is available on that column: `NULL`, `0` or an omitted value takes the next value, a deleted maximum is never reused, and when the next value does not fit the type the insert fails (MySQL reports a duplicate key, 1062). `UPDATE` stores `0` and rejects `NULL` (1048). A primary key without `AUTO_INCREMENT` rejects `NULL` like MySQL. Primary key columns are `NOT NULL`. A `NOT NULL` `ENUM` without a default takes its first member, as in MySQL.

`DEFAULT` literals are checked against the column type the way MySQL checks them and stored in the form MySQL stores them (integers rounded, `DECIMAL(p,s)` and `FLOAT(M,D)` rounded to their scale, numbers in string columns kept as written, dates and times normalized to `YYYY-MM-DD HH:MM:SS[.fraction]`, `ENUM`/`SET` members spelled as declared, trailing spaces removed for `CHAR`). `CURRENT_TIMESTAMP(n)`/`NOW(n)` defaults are available for `DATETIME(n)` and `TIMESTAMP(n)` with the same `n` (MySQL rejects any other), and parenthesized expression defaults are lowered as expressions. A default MySQL rejects fails, and one in a temporal form the assignment of a value does not take (see the column types above) is not lowered.

`CREATE TABLE` keys are validated like InnoDB does: a `BLOB`/`TEXT` key part needs a prefix length, `JSON` columns cannot be key parts, a prefix must fit the column and be on a string, a key is at most 3072 bytes, key names are distinct, and key columns must exist.

DDL that MySQL itself rejects fails with MySQL's error rather than an unsupported-syntax error: invalid defaults, invalid keys, a second primary key, an `AUTO_INCREMENT` column outside every key or of a type other than an integer or floating-point one (1063; refused on `FLOAT` and `DOUBLE`), `NULL` primary key columns, out-of-range type parameters (`VARCHAR(16384)`, `CHAR(256)`, `DECIMAL(5,6)`, `DATETIME(7)`, `BIT(65)`), duplicate `ENUM` members, a collation on a binary string, an index named `PRIMARY`, identifiers longer than 64 characters, an unknown storage engine (1286), and SQLite syntax MySQL does not have (`STRICT`, `WITHOUT ROWID`, column type names such as `ANY`). Every table gets its column types from the MySQL lowering. A function call in a column default, generated column or `CHECK` constraint is lowered as in a `SELECT` (`CONCAT` with a `NULL` argument is `NULL`, `LENGTH` counts bytes), and a default that calls a function Turso does not have is refused, where Turso would fail only the first `INSERT` that uses it. `CREATE INDEX` on a `JSON` column, or on a `TEXT` or `BLOB` column without a key length, fails as in MySQL (3152, 1170). Other DDL MySQL rejects is not detected yet when its SQLite reading is valid.

Compatibility no-ops (accepted, with no effect in Turso):

- `ENGINE=InnoDB` (other engines are not lowered), `DEFAULT CHARSET`/`CHARACTER SET` `utf8mb4`, `ROW_FORMAT` `DEFAULT`/`DYNAMIC`/`COMPACT`/`REDUNDANT`, `STATS_PERSISTENT`/`STATS_AUTO_RECALC`/`STATS_SAMPLE_PAGES`, `AUTO_INCREMENT=1`, and table `COMMENT`.
- Column and index `COMMENT`, `USING BTREE`/`USING HASH` (InnoDB also builds a B-tree for `HASH`), and `ALGORITHM=`/`LOCK=` on `CREATE INDEX`.
- `CHARACTER SET utf8mb4` and `COLLATE` `utf8mb4_0900_ai_ci`, `utf8mb4_0900_as_ci`, `utf8mb4_0900_as_cs`, `utf8mb4_ja_0900_as_cs`, `utf8mb4_ja_0900_as_cs_ks`, `utf8mb4_0900_bin`, `utf8mb4_bin` on columns and tables. `CHAR`, `VARCHAR` and `TEXT` columns get a `COLLATE` constraint with the matching ICU collation (`und-u-ks-level1` for `utf8mb4_0900_ai_ci`, `BINARY` for `utf8mb4_0900_bin`, `RTRIM` for the PAD SPACE `utf8mb4_bin`), taken from the column, else `utf8mb4_0900_ai_ci` when the column names a character set, else the table default, so comparisons, sorting, grouping, joins and unique keys on them follow the MySQL collation. `utf8mb4_general_ci` and `utf8mb4_unicode_ci` (PAD SPACE, older weights) are refused.
- Non-unique `KEY`/`INDEX` inside `CREATE TABLE`: secondary indexes do not change query results. The index is not created, so `DROP INDEX` on it fails, and `CREATE INDEX` or `ALTER TABLE ... ADD INDEX` does not see its name: an index named like it is created where MySQL rejects the duplicate name (1061), and one without a name is named after its first column where MySQL may add `_2`. The schema also keeps the unique keys of `CREATE TABLE` without their names, so an index named like a named unique key is created where MySQL rejects it; an index named like the first column of a unique key, or without a name next to one, is refused.
- Inline column `REFERENCES`, which MySQL itself parses and ignores.
- `CHECK (...) NOT ENFORCED`, which MySQL does not check.
- `RESTRICT`/`CASCADE` on `DROP TABLE`, which MySQL ignores.

Known gaps:

- Collations: `ALTER TABLE ... ADD COLUMN` gives a string column without its own collation `utf8mb4_0900_ai_ci`, also when the table was created with another default collation. `LIKE`, `REGEXP` and the string functions use `utf8mb4_0900_ai_ci` on columns declared with another collation. `ENUM` and `SET` columns compare byte by byte. Comparisons between string literals use `utf8mb4_0900_ai_ci`, also next to a `NULL` literal, whose collation MySQL ignores (`'FOO' IN ('foo', NULL)` is 1); comparisons of other string expressions without a column (function results, subqueries) still compare byte by byte.
- Decimal literals reach a column as doubles until exact decimals exist, so `1.50` is stored in a `VARCHAR` as `1.5`, a double literal such as `2.5e0` or a `DOUBLE` column value rounds half away from zero into an integer column (MySQL rounds it to even), a negative double such as `-0.4e0` fails in an unsigned integer column (MySQL stores 0, and fails only for the decimal `-0.4`), and `DECIMAL(p,s)` values are neither rounded to the scale nor checked against the precision and are shown without trailing zeros. `FLOAT` is double precision; `FLOAT(M,D)`/`DOUBLE(M,D)` values are shown without trailing zeros.
- A double stored in a `CHAR`/`VARCHAR` whose shortest MySQL text is longer than the column fails (1406), where MySQL rounds it to fit.
- A binary string or hexadecimal literal stored in an integer, `BIT`, `YEAR`, `FLOAT`/`DOUBLE` or `DECIMAL` column fails (1366), because a hexadecimal literal is not yet told apart from a binary string; MySQL stores `X'3132'` as 12594 and a binary string `'12'` as 12.
- An integer literal above 9223372036854775807 is read as a double, except where the literal row below says it is an exact DECIMAL. Storing one in a `BIGINT UNSIGNED` column fails (1366) instead of storing a rounded value; written as text (`'18446744073709551615'`) the value is stored exactly. In comparisons such a literal stays a double, so `u = 18446744073709551615` compares as doubles and also matches nearby values that round to the same double, where MySQL compares exactly. A `BIGINT UNSIGNED AUTO_INCREMENT` primary key, being the rowid, holds at most 9223372036854775807; without `AUTO_INCREMENT` it is an ordinary primary key over the whole range.
- `<`, `<=`, `>` and `>=` between a `BIGINT UNSIGNED` column and text that is not a whole number (`u < '9.5'`) fail; `=` compares them as doubles, as MySQL does.
- Temporal values are stored as given, without MySQL's validation, normalization, or `TIMESTAMP` time-zone conversion (only defaults are normalized).
- `ENUM` sorts by member position and `SET` by bitmask, as in MySQL, and both compare with strings and with each other by text, also as in MySQL (`MIN` and `MAX` too). An expression over such a column that keeps its type in Turso (`COALESCE`, `IFNULL`, `CASE`, `MIN`, `MAX`) also sorts by position or bitmask, where MySQL may sort its text. Arithmetic on an `ENUM` or `SET` value, comparing one with a number and a `CAST` of one to a number, where MySQL uses the member position or bitmask, are refused, as is comparing an `ENUM` with a `SET`. A `CAST` of a `DATE`, `DATETIME`, `TIMESTAMP` or `TIME` value to `DECIMAL`, `DOUBLE`, `FLOAT` or `REAL` is refused too, as MySQL reads it as a number such as 20200102030405. Databases whose MySQL tables were created before `ENUM` and `SET` stored positions and bitmasks hold text there, which now fails to read as a corrupt value. A DOUBLE or DECIMAL assigned to an `ENUM` or `SET` column reads as its text (`1.5` names no member and fails), and a name other than an ASCII one that differs from every member is rejected instead of converted.
- Unlike InnoDB, a failed insert does not use up an `AUTO_INCREMENT` value, and a multi-row `INSERT ... VALUES` that gives the `AUTO_INCREMENT` key in some rows and generates it (`NULL`, `0`, `DEFAULT`) in others does not lose the values InnoDB reserves for it: InnoDB reserves one value per row when the statement first generates a key, so after `INSERT INTO t (pk) VALUES (10), (DEFAULT)` it generates 13 next where Turso generates 12. Modeling it needs a statement that also moves the table's counter, which the lowering cannot run yet.
- `CREATE TABLE` checks its `FOREIGN KEY`s as MySQL 8.4 does when it creates them: the referenced table must exist (unless `FOREIGN_KEY_CHECKS` is 0) and have the referenced columns as its primary key or a unique key, in the key's order; `TEXT`, `BLOB` and `JSON` columns cannot be keys; string columns must have the same collation and integer columns the same size and sign; a `SET NULL` action needs nullable columns; and constraint names are unique in the database. MySQL compares keys as stored: keys between `DECIMAL` columns that store values alike (`DECIMAL(3,2)` and `DECIMAL(4,2)`) match as in Turso, and keys between `DECIMAL` columns that do not (`DECIMAL(4,1)` and `DECIMAL(4,2)`) or between `DATETIME`, `TIMESTAMP` and `TIME` columns or fractional precisions stored in different sizes are created but match no value: while `FOREIGN_KEY_CHECKS` is 1, an `INSERT ... VALUES` of literal rows fails as in MySQL (1452) when a row's key is not NULL, and other writes of the key are refused. Keys between `ENUM` (or `SET`) columns of different members match by member position (or bitmask), as in MySQL. Keys between `BINARY` and `VARBINARY` columns match as in Turso, which pads `BINARY` alike; keys between `CHAR` and `VARCHAR` columns too, except for values that end in a space, whose writes are refused. Refused: keys between columns of other different types, that name a column twice, or named like MySQL's generated names (`t_ibfk_1`); and, with `FOREIGN_KEY_CHECKS` 0, keys to an existing table that fail a check, which MySQL accepts in some cases. Inline `REFERENCES` column options are ignored, as in MySQL.
- `FOREIGN KEY` constraints are enforced from the start of a connection, as MySQL starts with `FOREIGN_KEY_CHECKS = 1`. Turso checks them like SQLite: values are compared as stored, so a key between `ENUM` columns compares member positions and one between `SET` columns bitmasks, as MySQL does (`'cat'` matches `'rat'` when both are member 1).
- `CREATE TABLE ... AS SELECT` gives a result column the type of the column it reads (also through `MAX`/`MIN`), `BIGINT` to integer literals, `COUNT(...)` and integer arithmetic, `DOUBLE` to double literals and double arithmetic, and `LONGTEXT` to everything else (string literals, `CONCAT(...)`, `SUM(...)`, `AVG(...)`, arithmetic on `DECIMAL`), where MySQL picks a `VARCHAR(n)`, `INT` or `DECIMAL(p,s)` from the expression.
- `CREATE TABLE ... AS SELECT` copies only the column type. `NOT NULL` and `DEFAULT` of the columns it reads are not copied, and neither is the `CHECK` that holds the validity of `JSON` columns, so the new table accepts any text there. MySQL keeps all of these.
- Not lowered: `CREATE TABLE ... LIKE`, `CREATE TABLE` with both columns and a `SELECT`, qualified `db.table` names, `ZEROFILL`, `SPATIAL` keys, `FULLTEXT` keys, functional key parts in `CREATE TABLE`, unique keys on column prefixes, `ON UPDATE CURRENT_TIMESTAMP`, `INVISIBLE`, `AUTO_INCREMENT=n`, `AUTO_INCREMENT` on a column that is not the whole primary key, partitioning, `ALTER TABLE` with several operations or `FIRST`/`AFTER`, `MODIFY`, `CHANGE` keeping the name, `ALTER COLUMN`, adding primary keys, foreign keys and checks or dropping keys and constraints other than indexes with `ALTER TABLE`, `TRUNCATE` (it also resets `AUTO_INCREMENT` in MySQL), and `CREATE INDEX` with prefix lengths.

`REPLACE` in each of these forms lowers to `INSERT OR REPLACE`: like MySQL,
it deletes every row that conflicts with the new row on the primary key or a
unique key, then inserts. Known differences: a `NULL` for a `NOT NULL` column
that has a default stores the default where MySQL fails (1048). `REPLACE`
into a table with triggers is refused (see Triggers).

## Views

`mysql/parser/ddl_view.rs` lowers `CREATE VIEW v [(columns)] AS SELECT ...`
to a Turso view over the lowered query, and `DROP VIEW [IF EXISTS] v` for one
view. `ALGORITHM`, `DEFINER` and `SQL SECURITY` are accepted and have no
effect: Turso has no privileges, and the algorithm only chooses a plan.

Refused: `CREATE OR REPLACE VIEW`, `DROP VIEW` of several views, and view
columns with types or options. Known differences: Turso views are read-only,
so `INSERT`, `UPDATE` and `DELETE` through a view fail where MySQL changes
the base table, and `WITH CHECK OPTION` does not parse.

## Stored procedures

`CREATE PROCEDURE` is not available, so a session has no stored procedures:
`CALL p(...)` fails as MySQL's call of a procedure that does not exist
(1305). A `CALL` of a procedure of a MySQL system schema (`sys.p`, `mysql.p`)
is refused.

## Triggers

`mysql/parser/trigger.rs` lowers `CREATE TRIGGER name {BEFORE|AFTER}
{INSERT|UPDATE|DELETE} ON table FOR EACH ROW statement` (or `BEGIN ... END`
around statements) to a Turso trigger whose statements are the body's
`INSERT`, `REPLACE`, `UPDATE` and `DELETE`, each lowered as outside a
trigger, and `DROP TRIGGER [IF EXISTS] name`. As in MySQL, a failing
statement of a trigger fails the statement that fired it, and `NEW` in a
`BEFORE INSERT` trigger has the defaults of columns the `INSERT` leaves out.

These fail as in MySQL when the trigger is created: `OLD` in an `INSERT`
trigger (1363), `NEW` in a `DELETE` trigger (1363), `SET OLD.column` (1362),
`SET NEW.column` in an `AFTER` trigger (1362), and a `SELECT` (1415).

Refused where Turso would run triggers differently from MySQL (the lowering
reads the triggers, foreign keys and columns of the session's schema):

- a second trigger with the timing and event of one the table has, since
  MySQL fires them in the order they were created and Turso in the opposite
  order; `FOLLOWS` and `PRECEDES` do not parse;
- in a `BEFORE` trigger, `NEW` of a generated column, or (`BEFORE INSERT`)
  of the `AUTO_INCREMENT` key, which MySQL has as 0 before it is generated;
- a statement whose triggers (also through other triggers) write a table
  that the statement, or a trigger that fired them, names: MySQL fails with
  1442 when such a trigger runs. A table counts as named when its name
  appears anywhere in the statement or trigger, so a column with a table's
  name is refused too, and so is a statement that would fire such a trigger
  on no row;
- a statement whose foreign key actions (`CASCADE`, `SET NULL`, `SET
  DEFAULT`) change a table with triggers for that change: MySQL does not fire
  triggers for foreign key actions, Turso does;
- `REPLACE`, `INSERT IGNORE` and `ON DUPLICATE KEY UPDATE` on a table with
  triggers, whose firing is not verified against MySQL's;
- `ALTER TABLE` and `RENAME TABLE` while the database has triggers, since
  Turso rewrites the stored triggers through the MySQL lowering;
- `SET NEW.column = ...` in a `BEFORE` trigger (Turso triggers cannot change
  the row being written), other statements in a body, `DEFINER`, and
  triggers inside a transaction (MySQL commits it first).

## SELECT statements and expressions

`SELECT` statements are lowered by `mysql/parser/query.rs` (statement
structure), `expr.rs` (expressions) and `literal.rs` (literals) into the shared
Turso AST. Anything listed under "Refused" fails with an unsupported-syntax
error.

A statement that starts with `TABLE t` (`TABLE t1 UNION TABLE t2 ORDER BY 1`)
is read with each `TABLE t` operand written as `SELECT * FROM t`, which is
what MySQL defines it as; a `TABLE` operand followed by anything but the end,
`ORDER BY`, `LIMIT`, a set operator or `)` is refused. A column qualified by a
table in the `ORDER BY` of a `UNION`, `EXCEPT` or `INTERSECT` fails with 1250,
as in MySQL.

A grouped `SELECT` is checked as MySQL's `ONLY_FULL_GROUP_BY` checks it: an
expression of the select list or `HAVING` may use a column outside `GROUP BY`
when the grouped columns hold its table's primary key or a unique key of
`NOT NULL` columns, or the `WHERE` clause equates the column with a constant
or (with an inner join's `ON`) with a column that is grouped or dependent;
another column fails with MySQL's error (1055, or 1140 without `GROUP BY`).
The check reads the session's tables, so a grouped `SELECT` over a view, a
derived table, an outer join or columns it cannot resolve is refused when it
may use a column outside `GROUP BY`. A `GROUP BY` name that is both a select
alias and a column of the `FROM` groups by the column, as in MySQL (`SELECT b
- 1 AS a FROM t GROUP BY a` fails with 1055 when `t` has a column `a`); when the
check cannot read the tables and the alias decides it, it is refused.

A name in `HAVING` resolves as in MySQL: inside an aggregate it names a column
of the `FROM` before a select alias, and outside one a grouped column before
an alias of the same name, and otherwise the alias (`SELECT a AS b FROM t
GROUP BY a HAVING AVG(b) > 2` averages the column `b`). A `HAVING` naming a
select alias that may also name a column is refused over a view, a derived
table, a common table expression or columns of several tables, and when it
holds a subquery.

A string literal compared with a `COUNT`, a length function (`LENGTH`,
`CHAR_LENGTH`, `OCTET_LENGTH`, `BIT_LENGTH`), a ranking window function or a
`HAVING` alias of one becomes MySQL's DOUBLE of the string, as MySQL compares
a number with a string (`COUNT(*) = '2'` is true for two rows). Beside a
column, a string literal stays a string, which the column's type converts;
beside another computed number, Turso compares a number with a string as
SQLite does (every number below every string), unlike MySQL.

An operand of `LIKE` that is not a string or an integer literal is matched as
the text `CONCAT` gives it, MySQL's text of the value (`1e0 LIKE '1'` and a
`DOUBLE` column holding 1 `LIKE '1'` are true, where SQLite's text is
`'1.0'`). A `BLOB` or `VARBINARY` column still matches ASCII letters without
case, where MySQL compares its bytes.

A call whose result MySQL types as a `DATE` (`DATE`, `CURDATE`, `FROM_DAYS`,
`LAST_DAY`, `MAKEDATE`), a `TIME` (`TIME`, `SEC_TO_TIME`, `MAKETIME`,
`TIMEDIFF`, `CURTIME`) or a `DATETIME` (`NOW`, `FROM_UNIXTIME` of one
argument, `TIMESTAMP`, `CONVERT_TZ`) is `__mysql_temporal_number(x)` in
arithmetic, a bit operation, a comparison with a number and a numeric function
argument: the number MySQL reads it as (`SEC_TO_TIME(2378) + 0` is 3938,
`DATE(dt) + 0` is 20000102). One with a fraction, which MySQL reads as a
DECIMAL, is refused, and so is `DATE_ADD` of such a `TIME`. A `DATE`,
`DATETIME`, `TIMESTAMP` or `TIME` column in arithmetic is refused, as MySQL
reads it as such a number. `SUM` and `AVG` of such a column add the start of
its text (the year), unlike MySQL.

`x REGEXP ''` and `x NOT REGEXP ''` as a whole select item are
`__mysql_empty_regexp(x)`, which fails with MySQL's error 3685 when it is
evaluated, as MySQL rejects the empty pattern then (not on an empty table). A
select item of a `SELECT` that reads one row where MySQL always reaches such a
test of a literal fails the same way. Elsewhere it is refused, as MySQL skips
it after a decided `AND` or `OR` or a `NULL` operand, and so is a `NULL` or
binary subject.

`HAVING` in a `SELECT` without `GROUP BY` or aggregates, which MySQL applies to
each row, joins the `WHERE` condition with each select alias it names replaced
by the alias's expression (`SELECT a + 1 AS b FROM t HAVING b > 2`). Refused:
a name that is neither a select alias nor a selected column, unless the select
list has `*` or the `SELECT` is a subquery with no `FROM` (where it names an
outer column), an alias of a window function, a subquery in `HAVING` that
names an alias, and expressions other than operators, `IN`, `BETWEEN`, `LIKE`,
`CASE`, casts and plain function calls.

A derived table, common table expression or view that names a column twice
fails with MySQL's error 1060: in its column list, or, without one, in the
select list of its query (of the first `SELECT` of a set operation), where `*`
names the columns of the `FROM` tables in order and names compare without
case. An unnamed expression is named by its text as written, which lowering
does not keep, so one that prints like another name is refused. A `*` whose
columns lowering cannot know (over a view, a common table expression, a
`USING` or `NATURAL` join or a table function) is not checked, and may give
rows where MySQL fails with 1060.

Statements MySQL rejects when it resolves names fail with MySQL's error: two
tables of one `FROM` with the same name or alias (1066; a derived table's
alias is apart from the names of tables, and a table named with its database
next to one named without it is refused), a join of more than
61 tables (1116; refused when the tables of derived tables, which MySQL may
merge, take it past 61), `LIMIT` in an `IN`, `ANY` or `ALL` subquery (1235),
and an `ORDER BY` name that is the alias of two different selected
expressions (1052; refused when it is also a selected column, or both
expressions may be the same column). A view's tables are not counted.

Parsing uses sqlparser's MySQL dialect wrapped in `mysql/parser/dialect.rs`,
which applies MySQL 8.4 operator precedence (`!` and unary `-`/`~` above `^`,
then `* / DIV % MOD`, `+ -`, `<< >>`, `&`, `|`, `IN`/`LIKE`/`BETWEEN`,
comparisons, `IS`, `NOT`, `AND &&`, `XOR`, `OR ||`), reads `INTERVAL expr
unit` and `EXTRACT(unit FROM expr)` with every MySQL unit, and applies MySQL's
string escapes: `\0 \b \n \r \t \Z`, a doubled quote, `\%` and `\_` keep their
backslash, and `\x` is `x` for any other character (so `\a` and `\f` are `a`
and `f`).

Lowered:

| Syntax | Lowering |
|---|---|
| `SELECT [ALL \| DISTINCT \| DISTINCTROW]`, `*`, `t.*`, aliases (also `'string'` and `"string"` aliases) | Same clauses |
| `FROM` tables (`db.t` as a schema), aliases, comma joins, `[INNER] JOIN`, `CROSS JOIN`, `LEFT/RIGHT [OUTER] JOIN`, `NATURAL`, `ON`, `USING`, derived tables (with a column list, `AS t (a, b)`, except `LATERAL` ones), `LATERAL`, parenthesized joins, `FROM DUAL` | Same clauses (`FROM DUAL` is no `FROM`; a derived column list names the columns of a `WITH` inside the derived table) |
| `WHERE`, `GROUP BY` (expressions, aliases, ordinals), `HAVING`, `WINDOW`, `ORDER BY [ASC \| DESC]`, `LIMIT n`, `LIMIT n OFFSET m`, `LIMIT m, n` | Same clauses |
| `UNION [ALL \| DISTINCT]`, `INTERSECT`, `EXCEPT`, parenthesized members with their own `ORDER BY`/`LIMIT`, `WITH` with column lists, `VALUES ROW(...)` | Same; a member that has its own `ORDER BY`/`LIMIT`, or a right operand that is itself a set operation (`INTERSECT` binds tighter), becomes `SELECT * FROM (...)` |
| Window functions with `PARTITION BY`, `ORDER BY`, `ROWS`/`RANGE` frames and named windows | Same |
| Column references, `?` parameters (numbered in text order), `CASE`, `IN` lists and subqueries, `BETWEEN`, `EXISTS`, scalar subqueries, row values, `IS [NOT] NULL/TRUE/FALSE/UNKNOWN`, `= ANY (subquery)`, `<> ALL (subquery)` | Same expressions; `IS UNKNOWN` is `IS NULL`; `= ANY` is `IN`, `<> ALL` is `NOT IN` |
| `a DIV b` | `CAST(a / b AS INTEGER)`: truncates toward zero, `NULL` for a zero divisor. A JSON operand is `__mysql_json_decimal(x)`, the DECIMAL MySQL divides with. A hex or bit literal up to 9223372036854775807 is its integer value when the other operand is a number literal, `NULL`, such a hex or bit literal, or a string whose number is not negative, so the BIGINT UNSIGNED quotient cannot be negative (`0xFF DIV 13` is 19). A hex or bit literal, or a number literal at or beyond the BIGINT range, next to a number, string (bare or `_utf8mb4`), boolean, `NULL`, `DATE`, `TIME` or `TIMESTAMP` literal, or another such literal, is folded to MySQL's quotient: the truncated exact quotient of the literals' DECIMAL values (a DOUBLE literal's shortest digits, a temporal literal's number, a string's leading number), `NULL` for a zero divisor (`0xFF666F6F626172FF DIV TIMESTAMP '2024-12-30 10:34:58'` is 909209, `TIME '-5 10:34:58' DIV 0b1001` is -144828). A negative quotient next to a hex literal or an integer literal above BIGINT, which MySQL reads as BIGINT UNSIGNED and rejects, is refused, as are a quotient outside BIGINT and strings with an exponent or leading whitespace (`18446744073709551615 DIV '103458.123456'` is 178301552913385). A JSON value next to a hex or bit literal or such a number literal is `__mysql_json_quotient(a, b, unsigned, text)`, the truncated DECIMAL quotient, which fails as MySQL does (1690) when it is negative next to a hex literal; the error names the division as written, not as MySQL prints it |
| `CAST(n AS JSON)` of an integer literal between 9223372036854775808 and 18446744073709551615 | `__mysql_cast_as_json('n')`: MySQL's unsigned JSON integer. Read as a DECIMAL (`DIV`, `CAST(.. AS DECIMAL)`) or a BIGINT it is its signed 64 bits, as in MySQL (`CAST(CAST(18446744073709551615 AS JSON) AS DECIMAL)` is -1), and as a DOUBLE its value |
| `MOD`, `%`, `XOR`, `&&`, `\|\|`, `!`, `<=>` | `%`, or `MOD(a, b)` when an operand is a DOUBLE or string literal, which keeps the fraction of a DOUBLE remainder (`1.5e0 % 1.0e0` is 0.5); `(NOT a) <> (NOT b)`; `AND`; `OR`; `NOT`; `IS` |
| `a \| b`, `a & b`, `a ^ b`, `~a`, `a << b`, `a >> b` | `__mysql_bit_or`, `__mysql_bit_and`, `__mysql_bit_xor`, `__mysql_bit_not`, `__mysql_shift_left`, `__mysql_shift_right` over the 64 bits of each operand read as MySQL's BIGINT UNSIGNED: an integer literal up to 18446744073709551615 as its bits, a DECIMAL rounded half away from zero and a DOUBLE rounded half to even, each saturating at the BIGINT range, a string literal, bare or after a `_utf8mb4`, `_utf8mb3` or `_utf8` introducer or an `_ascii` or `_latin1` one on ASCII text, as the integer at its start (`'1.5'` is 1), a `_binary` string literal the same way when the other operand is a number, a string that is not binary, a temporal literal or a JSON value, or when it is the count of a shift (`_binary 'foobar' ^ 1.5` is 2; MySQL computes bytewise on binary strings otherwise, which is refused), a hex or bit literal as its value, a `DATE`, `TIME` or `TIMESTAMP` literal as its number rounded half up to whole seconds, with the carry MySQL makes into minutes and hours (`TIME '00:59:59.5'` is 10000, `TIME '-10:04:58'` is the bits of -100458), refusing a `TIME` that rounds past 838 hours and a `TIMESTAMP` that rounds into the next day, a JSON value as MySQL's integer of it, and any other operand through `__mysql_bit_operand(x)`, which takes an integer, a DECIMAL and a whole DOUBLE in the BIGINT range. A shift by 64 or more is 0. A negated DOUBLE literal below the BIGINT range (`-18446744073709540000e0`), which MySQL rejects where it reads it (1690), is refused, except in a select item of a `SELECT` that reads one row where MySQL always reaches it (no `NULL` operand before it, no decided `AND` or `OR`): that item is `__mysql_bigint_out_of_range('-(18446744073709540000e0)')`, which fails with MySQL's error. `CHAR` of such a literal is the same. The result is `__mysql_unsigned_shown(bits)` when the operation is a whole select item of a `SELECT` that reads one row or the operand of `IS NULL` or `IS NOT NULL`, which shows a result above 9223372036854775807 as its digits (`~0` is 18446744073709551615), and `__mysql_unsigned_value(bits)` elsewhere, which fails on such a result as not supported |
| `a LIKE p [ESCAPE c]`, `REGEXP`, `RLIKE` | `LIKE` with `ESCAPE '\'` unless the pattern is a string without backslashes (MySQL's default escape character); a dangling escape at the end of a literal pattern matches itself, as in MySQL; when either operand is a binary literal (`0x..`, `X'..'`, `b'..'`, `_binary '..'`), `__mysql_like_bin(s, p, escape)` matches the bytes case-sensitively, `_` matching one byte, as MySQL's `binary` collation does (`0x61 LIKE 'A'` is 0); `REGEXP` operator, whose operands may be `CAST(x AS JSON)` (MySQL matches the JSON text); when an operand is a binary literal, binary and temporal literals are their bytes, number and boolean literals stay numbers, JSON values are their text and strings stay text, and the match is byte by byte and case-sensitive, failing like MySQL (3995) between a binary string and a string |
| `COLLATION(x)` of a literal, an introduced string (`_utf8mb4`, `_utf8mb3`, `_utf8`, `_latin1`, `_ascii`, `_binary`), `'..' COLLATE c` with a `utf8mb4` collation, `RANDOM_BYTES(literal)` or `UUID()` | The collation name MySQL derives from the type (`'utf8mb4_0900_ai_ci'`, `'binary'` for numbers, `NULL` and binary strings), without evaluating `x`, as in MySQL; other arguments fall back |
| Function calls | Passed through by name, so SQLite built-ins and the MySQL functions in `mysql/frontend/functions` both resolve; `COUNT(*)`, `DISTINCT` arguments and `OVER` are kept. Arguments of the numeric functions (`ABS`, `ACOS`, `ASIN`, `ATAN`, `ATAN2`, `CEIL`, `CEILING`, `CHAR`, `COS`, `COT`, `DEGREES`, `EXP`, `FLOOR`, `LN`, `LOG`, `LOG10`, `LOG2`, `MOD`, `POW`, `POWER`, `RADIANS`, `ROUND`, `SIGN`, `SIN`, `SQRT`, `TAN`, `TRUNCATE`) and the condition of `IF` are a numeric context for literals; the separator of `CONCAT_WS` is text |
| `CAST(x AS JSON)` used as a value (a selected column, a `CASE`/`IF` result, a `JSON_*` argument, or an argument of another function whose other arguments are non-NULL literals), a boolean expression (`TRUE`, `a = b`, `NOT a`, `IN`, `LIKE`, ...) used as a value of `JSON_ARRAY`, `JSON_OBJECT`, `JSON_SET`, `JSON_INSERT`, `JSON_REPLACE`, `JSON_ARRAY_APPEND`, `JSON_ARRAY_INSERT` or `CAST(... AS JSON)`, and `x MEMBER OF (y)` | `__mysql_cast_as_json(x)`, `__mysql_cast_boolean_as_json(x)`, `__mysql_member_of(x, y)` (see JSON functions) |
| A select item, in a `SELECT` with no table (or `FROM DUAL`), `WHERE`, `GROUP BY`, `HAVING`, `WINDOW`, `ORDER BY` or `LIMIT`, built only from literals, operators, casts and function calls, in which MySQL always evaluates `CAST('text' AS JSON)` of a text its JSON parser rejects, or `+`, `-` or `*` of integer literals whose exact result is outside BIGINT UNSIGNED when an operand is a hex literal or an integer above the BIGINT range (which MySQL types as unsigned), or outside BIGINT otherwise, or a comparison of a `DATE` or `TIMESTAMP` literal with a printable ASCII string literal MySQL cannot convert to a DATETIME (one that does not start with a digit after spaces, has letters after a date or datetime of the forms above, or names a day, month or time that does not exist, as `'0000-02-29'`) | `__mysql_cast_as_json('text')`, which fails with MySQL's error 3141, since the item fails whatever surrounds the cast (`SELECT -1.5 - CAST('[1,' AS JSON)`), and `__mysql_bigint_out_of_range('(a op b)', unsigned)`, which fails with MySQL's error 1690 (`SELECT 0x0 - 0x1`, `SELECT 9223372036854775807 + 1`), and `__mysql_incorrect_temporal(type, text)`, which fails with MySQL's error 1525 (`Incorrect DATETIME value: 'foobar'`, `DATE` beside a `DATE` literal); the integer literals are integer numbers, `TRUE`, `FALSE`, hex and bit literals (bit literals are signed, as in MySQL) and DATE, TIME and TIMESTAMP literals without a fraction, and the error names the operation as written rather than as MySQL prints it. MySQL evaluates the left operand of an operator first and the right one only when the left one is a value that cannot be NULL (always for `<=>`; for `AND` a true literal, for `OR` a false literal, or `NULL`, where a string literal is as true as the number at its start, a `DATE`, `TIME` or `TIMESTAMP` literal is true unless all its digits are zero, and a hex or bit literal of at most 8 bytes is true unless its bytes are all zero); the subject of `IS`, `IN` and `BETWEEN`; `LIKE` before its pattern and `REGEXP` after its pattern; the operand of a cast; the arguments of `CONCAT`, `STRCMP` and `MAKETIME` in order and those of `MAKEDATE` from the second, up to the first NULL; and every argument of another function when the other arguments cannot be NULL, of `CHAR` and `INTERVAL` always, of `CONCAT_WS` when the separator cannot be NULL, and never of `IF`, `IFNULL`, `COALESCE`, `NULLIF`, `ELT`, `FIELD` or `MAKE_SET`. An item that MySQL finds NULL before it reaches an operand that fails, through a NULL operand of an operator other than `AND`, `OR`, `||` and `<=>` or a NULL argument of those four functions, is `NULL` (`NULL - 1 <= CAST('x' AS JSON)`, `CONCAT(NULL * NULL, CAST('x' AS JSON))` and `MAKEDATE(CAST('x' AS JSON), NULL)`; `NULL <=> CAST('x' AS JSON)` and `CONCAT(CAST('x' AS JSON), NULL)` fail). An `AND` or `OR` of such operands that a literal decides before MySQL reaches an operand that fails is that decision, 0 or 1, in any clause (`0 AND CAST('x' AS JSON)` is 0, `'2000-01-01' OR CAST('x' AS JSON)` is 1) |
| A JSON value (`CAST(x AS JSON)`, `x->'path'`, or a call of a `JSON_*` function that returns JSON) compared with `=`, `<>`, `<`, `<=`, `>`, `>=` or `<=>` to another JSON value or to a literal (integer literals beyond BIGINT too, as the BIGINT UNSIGNED or DECIMAL values MySQL reads them as), a temporal literal, a boolean expression, arithmetic on number literals, or a column (a JSON, integer, `DOUBLE`, `DECIMAL`, temporal or text column; other column types raise an error) | `__mysql_json_compare(a, b) OP 0`, `__mysql_json_null_safe_equal(a, b)`: MySQL's JSON comparison. Numbers compare exactly across integers, doubles and decimals; strings by their bytes; arrays element by element and then by length; objects by member count, then by keys and values; `false` before `true`; values of different JSON types by MySQL's type order (null, numbers, strings, objects, arrays, booleans, dates, times, datetimes, opaque values). The other operand becomes a JSON value: a string a JSON string, a number a JSON number, a boolean expression a JSON boolean, a binary string an opaque value, and a `DATE`/`TIME`/`TIMESTAMP` literal a JSON date, time or datetime |
| `CAST(x AS JSON)` of a binary string (`0x..`, `X'..'`, `b'..'`, `_binary '..'`) | `__mysql_cast_as_json(x'..')`: MySQL's opaque JSON value of the bytes, whatever they are; `UNIX_TIMESTAMP` of any JSON value is a `DECIMAL(65,6)`, since MySQL reads six fractional digits from it |
| `CAST`/`CONVERT` of a JSON value to `SIGNED`, `UNSIGNED`, `DECIMAL[(m[,d])]`, `DOUBLE`, `REAL` or `FLOAT` | For a JSON cast of an integer or decimal literal within the BIGINT range, the same cast of the literal, since the JSON keeps its type. Otherwise `__mysql_json_integer(x)` (MySQL's integer of a JSON value: doubles rounded half to even and clamped to BIGINT, an unsigned value's bits, `true` 1, other non-numbers 0), `__mysql_json_decimal(x)` (the exact decimal) or `__mysql_json_double(x)`, then the cast. JSON strings holding digits, which MySQL parses for their leading number, are refused |
| A JSON value used as a condition (`WHERE`, `NOT`, `AND`, `OR`, `XOR`, the condition of `IF`), in arithmetic (`+ - * / % MOD`, unary `-`), as an argument of `CHAR`, or as an operand of `LIKE` | `__mysql_json_truth(x)` (MySQL's `x <> 0` under the JSON rules); `__mysql_json_double(x)`, MySQL's DOUBLE of the JSON value (a string's leading number, `true` 1, other non-numbers 0), `__mysql_json_integer(x)` for `CHAR`, MySQL's integer of the JSON value as the bit operations read it (a JSON string is refused), and `__mysql_json_remainder(a, b)`, the DOUBLE remainder (`1.5e0 % CAST('1.1' AS JSON)` is `0.3999999999999999`, `NULL` for a zero divisor); `__mysql_like_bin(s, p, escape)`, which matches the JSON text with `utf8mb4_bin` (case-sensitive), as MySQL does. `REGEXP`, `REGEXP_LIKE` and `STRCMP` with a JSON operand also compare case-sensitively, and a JSON operand next to a binary string is not an error, except in `LIKE`, where a binary string that is not UTF-8 fails with MySQL's conversion error (3854) |
| `CAST(DATE '..' AS JSON)`, `CAST(TIME '..' AS JSON)` and `CAST(TIMESTAMP '..' AS JSON)` as a shown select item, the operand of a `CHAR` or `NCHAR` cast, an argument of `CONCAT`, `CONCAT_WS`, `STRCMP`, `SHA`, `SHA1`, `SHA2` (the string), `MD5`, `CRC32`, `TO_BASE64`, `FROM_BASE64`, `HEX` or `UNHEX`, or an operand of `LIKE`, which read only its text | `__mysql_cast_as_json('"2000-01-01"')`: the JSON text MySQL prints for the date, time or datetime, with six fractional digits for times and datetimes (`"12:34:56.000000"`) and the hours of a time taken modulo 32, as MySQL stores them (`TIME '5 12:34:58'` prints `"04:34:58.000000"`); a negative time that prints as zero is refused. Also through further casts to JSON (`CAST(CAST(CAST(TIME '12:34:56' AS JSON) AS JSON) AS CHAR)`) |
| The same JSON dates and times, also through further casts to JSON, in `CAST` or `CONVERT` to another type | The literal MySQL converts the JSON value to, cast to the type: 0 for a number type (`CAST(CAST(TIME '12:34:58' AS JSON) AS DECIMAL(12, 4))` is `0.0000`); the same JSON value for `JSON`; for `TIME` the time of a JSON time, from its JSON text (`CAST(CAST(TIME '32:34:58.5' AS JSON) AS TIME)` is `00:34:59`), and `NULL` for a JSON date or datetime; for `DATE` and `DATETIME` the date or datetime of a JSON date or datetime and `NULL` for a JSON time |
| The same JSON dates and times as an argument MySQL reads as a number (`MAKEDATE`, `MAKETIME`, `PERIOD_ADD`, `PERIOD_DIFF`, the first argument of `FROM_UNIXTIME`, `SEC_TO_TIME` and `FROM_DAYS`, the mode of `WEEK` and `YEARWEEK`), as a datetime (the first argument of `CONVERT_TZ`, `DATE`, `DATE_FORMAT`, `DAY`, `DAYOFMONTH`, `DAYOFWEEK`, `DAYOFYEAR`, `LAST_DAY`, `MONTH`, `MONTHNAME`, `QUARTER`, `TO_DAYS`, `TO_SECONDS`, `UNIX_TIMESTAMP`, `WEEK`, `WEEKDAY`, `WEEKOFYEAR`, `YEAR` and `YEARWEEK`) or as a time (the argument of `HOUR`, `MINUTE`, `SECOND`, `MICROSECOND`, `TIME` and `TIME_TO_SEC`) | As a number, the DOUBLE `0e0`: MySQL reads a JSON date or time as 0 there, with six fractional digits where the result shows them. As a datetime, a JSON date or datetime is `__mysql_cast_as_datetime(text, 6)` and a JSON time is `NULL`, since MySQL fails that read and these functions return NULL for it. As a time, a JSON time is `__mysql_cast_as_time(text, 6)` with its hours modulo 32, and a JSON date or datetime is `NULL`. A JSON time in `UNIX_TIMESTAMP` is the DOUBLE `0e0`, as MySQL returns 0 for the failed read. A JSON date or time as the hour or minute of `MAKETIME` with DECIMAL seconds, whose decimals are not kept, is refused |
| `GET_FORMAT(DATE \| TIME \| DATETIME \| TIMESTAMP, f)` | `GET_FORMAT('DATE', f)` |
| `VARIANCE(x)`, `VAR_POP(x)`, `VAR_SAMP(x)`, `STD(x)`, `STDDEV(x)`, `STDDEV_POP(x)`, `STDDEV_SAMP(x)` | `__mysql_var_pop(CAST(x AS DOUBLE))`, `__mysql_var_samp`, `__mysql_stddev_pop` or `__mysql_stddev_samp`: aggregates of the dialect that run MySQL's recurrence over the rows read as DOUBLE (so a temporal or `ENUM` argument is refused, as its `CAST`), NULL without rows (or with one row, for the sample forms). As window functions they are refused, as Turso runs no extension aggregate over a window |
| `GROUP_CONCAT(a, b, ... [ORDER BY ...] [SEPARATOR s])` | `group_concat(a \|\| b \|\| ..., s)` (Turso does not support `ORDER BY` inside aggregates yet and reports an error) |
| `DATE_ADD/ADDDATE(d, INTERVAL v unit)`, `d + INTERVAL v unit`, `INTERVAL v unit + d`; `DATE_SUB/SUBDATE(...)`, `d - INTERVAL v unit`; `EXTRACT(unit FROM d)`; `TIMESTAMPADD/TIMESTAMPDIFF(unit, ...)` | `DATE_ADD(d, v, 'UNIT')`, `DATE_SUB(d, v, 'UNIT')`, `EXTRACT('UNIT', d)`, `TIMESTAMPADD('UNIT', ...)`, `TIMESTAMPDIFF('UNIT', ...)` with the unit as upper-case text (the datetime functions implement these forms). A compound unit (`DAY_HOUR`, `HOUR_SECOND`, ...) reads the value's MySQL text, `CONCAT(v)`, so a DECIMAL keeps its scale (`INTERVAL 6/4 DAY_HOUR` is 1 day 5000 hours, as in MySQL). When `d` is a `TIMESTAMP '..'` or `DATE '..'` literal, MySQL's result is a DATETIME with the most fractional digits of `d` and the interval (6 for units with `MICROSECOND`, the decimals of a `SECOND` value: a DECIMAL literal's scale, 6 for strings and doubles), so the call is wrapped in `__mysql_cast_as_datetime(.., digits)`; a `DATE` literal with a date unit stays a `DATE`. A `TIME` literal, and a `SECOND` value whose decimals are not known while lowering, are refused |
| `x->'path'`, `x->>'path'` | `JSON_EXTRACT(x, path)`, `JSON_UNQUOTE(JSON_EXTRACT(x, path))` |
| `CURRENT_DATE`, `CURRENT_TIME`, `CURRENT_TIMESTAMP`, `UTC_DATE`, `UTC_TIME`, `UTC_TIMESTAMP`, `LOCALTIME`, `LOCALTIMESTAMP` (with or without `()`) | SQLite's current-time literals, which are UTC like the target time zone |
| `POSITION(a IN b)`, `SUBSTRING(s FROM a FOR b)` | `LOCATE(a, b)`, `SUBSTRING(s, a, b)` |
| `TRIM([[LEADING \| TRAILING \| BOTH] [r] FROM] s)`, `LTRIM(s)`, `RTRIM(s)` | `__mysql_trim(s, r)`, `__mysql_ltrim(s, r)`, `__mysql_rtrim(s, r)` with `r` a space when it is left out (see string functions); `TRIM(LEADING r)` without `FROM` is a syntax error, as in MySQL |
| `CAST`/`CONVERT(x, type)` to `SIGNED [INTEGER]` and `UNSIGNED [INTEGER]` | `__mysql_cast_as_signed(x)`, `__mysql_cast_as_unsigned(x)`: a DECIMAL rounds half away from zero, a double rounds to the nearest even integer and saturates at the BIGINT range (also for `UNSIGNED`, as in MySQL), text and binary strings give the integer they start with (`'-1.5'` is -1), and `UNSIGNED` keeps the 64 bits of a negative result. An integer literal above 9223372036854775807 is passed as text so it stays exact. An `UNSIGNED` result above 9223372036854775807 is shown as its digits when the cast is a whole select item of a `SELECT` that reads one row (`__mysql_unsigned_shown(__mysql_cast_as_unsigned_bits(x))`, `CAST(-3 AS UNSIGNED)` is 18446744073709551613) and fails elsewhere (Turso has no unsigned 64-bit value type yet), as does a cast of a `DATE`, `DATETIME`, `TIMESTAMP`, `TIME`, `ENUM` or `SET` column (stored as text as given, and MySQL casts an `ENUM` by its index) |
| `CAST`/`CONVERT(x, type)` to `CHAR` | `CAST(CONCAT(x) AS TEXT)`: MySQL's text of `x`, which `CONCAT` prints with the type's decimals (`CAST(6/4 AS CHAR)` is `1.5000`). A DOUBLE prints as MySQL prints it (`CAST(1e0 AS CHAR)` is `1`, `1e300` stays `1e300`), a DECIMAL column keeps its scale, and bytes that are not UTF-8 are `NULL` |
| `CAST`/`CONVERT(x, type)` to `DOUBLE`, `REAL`, `FLOAT`, `DECIMAL[(m[,d])]` | The SQLite type name of the same affinity. A `DECIMAL(m,d)` or `DEC(m,d)` target MySQL rejects (`m < d`, `m > 65`, `d > 30`) fails the whole statement when it is parsed, as in MySQL, also when the rest of the statement is refused or the cast is never evaluated; `DECIMAL(0)` and `DECIMAL(0,0)` are accepted, as MySQL reads them as `DECIMAL(10,0)` |
| `CAST`/`CONVERT(x, type)` to `NCHAR` | `__mysql_cast_as_nchar(CONCAT(x), 16777216)`, as `NCHAR(n)` below |
| `CAST`/`CONVERT(x, type)` to `CHAR(n)` and `NCHAR(n)` (`n` up to 16,777,216) | `__mysql_cast_as_char(CONCAT(x), n)`, `__mysql_cast_as_nchar(CONCAT(x), n)`: the MySQL text of `x` (numbers print as MySQL prints them, a binary string that is not UTF-8 is `NULL`) cut to `n` characters; `NCHAR` is `utf8mb3`, so a 4-byte character becomes `?` |
| `CAST`/`CONVERT(x, type)` to `DATE`, `DATETIME[(p)]` and `TIME[(p)]` used as a value (a selected column, an argument of a function other than a JSON one, the operand of a `CHAR` cast) | `__mysql_cast_as_date(x)`, `__mysql_cast_as_datetime(x, p)`, `__mysql_cast_as_time(x, p)`: MySQL's conversion of `DATE(x)`, `TIMESTAMP(x)` and `TIME(x)`, rounded to `p` fractional digits (0 by default), so `CAST('1.5' AS TIME)` is `00:00:02`. `p` above 6 fails the statement when it is parsed (error 1426). The operand must have no DATE or TIME type MySQL converts differently from its text: literals, operators other than `INTERVAL` arithmetic, `TIMESTAMP '..'` literals, `JSON_*` calls and JSON casts of those are lowered, as are `DATE '..'` to `DATE`/`DATETIME` and `TIME '..'` to `TIME` |
| `CONVERT(x USING utf8mb4/utf8mb3/utf8)`, `CONVERT(x USING binary)` | `CAST(CONCAT(x) AS TEXT)`, `CAST(CONCAT(x) AS BLOB)`: MySQL's text of `x` |
| `CAST(x AS CHAR[(n)] CHARACTER SET utf8mb4)` (or `CHARSET`), `CONVERT(x, CHAR CHARACTER SET utf8mb4)` | The same as `CAST(x AS CHAR[(n)])` |
| `CONVERT(x USING ascii)`, `CAST(x AS CHAR[(n)] CHARACTER SET ascii)`, as a select item of a `SELECT` that shows its values without `ORDER BY`, `DISTINCT`, `GROUP BY` or `HAVING` (where the ASCII collation does not matter) | `__mysql_convert_to_ascii(x)`: each character outside ASCII becomes `?`. A binary string with bytes outside ASCII raises an error |
| `WEIGHT_STRING(literal)`, `WEIGHT_STRING(literal AS BINARY(n))` | The bytes MySQL sorts the literal by, computed when the statement is lowered: of an integer, its 8 big-endian bytes with the sign bit flipped for a signed value (also with `AS CHAR(n)`); of a `DECIMAL` or `DOUBLE`, `NULL`; of a `_latin1` text of ASCII letters, digits and spaces, the text in uppercase; with `AS BINARY(n)`, the first `n` bytes of the literal's text when it has at least `n`. Other arguments (a `utf8mb4` text, whose weights are its collation's, a column, a shorter text that MySQL pads) are refused |
| `HOUR`, `MINUTE`, `SECOND`, `MICROSECOND`, `TIME_TO_SEC` of a `DATE` literal; `UNHEX` of a `DECIMAL` literal; `REVERSE` of a binary string literal; `CAST` of an integer literal beyond BIGINT `AS DECIMAL` | 0 (MySQL reads the date as midnight); `NULL` (the text has a `.`); the reversed bytes; the exact digits, which a DOUBLE would round |
| `HEX(CONVERT(literal USING utf16/ucs2/utf32/latin1/ascii))` | The hex of the literal's bytes in that character set, computed when the statement is lowered: a text's characters encoded (big-endian for the wide sets), and a binary string read as the set's bytes, padded on the left with zero bytes to a whole character (`NULL` when it is not valid `utf32`) |
| `BINARY x`, `CAST`/`CONVERT(x, type)` to `BINARY` and `BINARY(n)` (`n` up to 16,777,216) used as a value (a selected column, a function argument other than of `GREATEST`, `LEAST`, `FIELD`, `NULLIF` and `INTERVAL`, `ORDER BY`, `GROUP BY`) | `CAST(CONCAT(x) AS BLOB)`, with `x` as `CAST(x AS CHAR)` would print it when it is a JSON value and its bytes when it is a binary literal; `BINARY(n)` is `__mysql_cast_as_binary(.., n)`, cut to `n` bytes and padded with zero bytes to `n`. `BINARY x` binds as tightly as unary minus (`BINARY 'a' = 'A'` compares `BINARY 'a'`) |
| `COLLATE binary`, `COLLATE utf8mb4_0900_bin` | `COLLATE BINARY` |
| `COLLATE utf8mb4_0900_ai_ci`, `_as_ci`, `_as_cs`, `utf8mb4_ja_0900_as_cs`, `utf8mb4_ja_0900_as_cs_ks` on a `utf8mb4` operand; `COLLATE utf8mb4_bin` | `COLLATE "und-u-ks-level1"`, `"und-u-ks-level2"`, `"und-u-ks-level3"`, `"ja-u-ks-level3"`, `"ja-u-ks-level4"`; `COLLATE RTRIM`. `FIELD`, `GREATEST`, `LEAST` and `STRCMP` with an explicit collation compare under it (a `TIME` literal compares as its text, a `DATE` or `TIMESTAMP` literal is handled as in the next row, other temporal arguments are refused, and a binary string that is not UTF-8 fails with MySQL's conversion error); two different explicit collations, each the whole operand, fail with MySQL's illegal mix error (1267), other mixes, and `LIKE`, `REGEXP`, `LOCATE`, `INSTR`, `POSITION`, `FIND_IN_SET` and `NULLIF` under a collation other than `utf8mb4_0900_ai_ci`, are refused, except `LIKE` under `utf8mb4_0900_as_cs`, `utf8mb4_0900_bin` or `utf8mb4_bin` whose operands are string literals of printable ASCII, number, boolean, temporal and `NULL` literals, hex, bit or `_binary` literals of printable ASCII and `CAST`s of such literals `AS JSON` (their text): those collations order printable ASCII as its bytes, so `__mysql_like_bin` matches as MySQL does, and a hex, bit or `_binary` literal that is not UTF-8 fails with MySQL's conversion error |
| `GREATEST` and `LEAST` whose arguments are all `DATE`, `TIME` or `TIMESTAMP` literals or `NULL` | Computed while lowering, as MySQL compares them as temporal values: `NULL` with a `NULL` argument, else the greatest or least value shown as a `DATE` when every argument is one, a `TIME` when every argument is one, else a `DATETIME`, with the most fractional digits of any argument (`LEAST(TIME '23:59:59.5432', TIME '120:59:59')` is `'23:59:59.5432'`). A `TIME` next to a `DATE` or `TIMESTAMP` is refused, as MySQL compares it as a datetime on the current date |
| `GREATEST`, `LEAST` and `STRCMP` of a `DATE` or `TIMESTAMP` literal and literals that cannot be read as a date (strings that do not start with a digit, numbers of at most four integer digits, digit strings whose month or day is out of range, `CAST(n AS UNSIGNED)`, `NULL`) | `GREATEST` is the latest date, computed while lowering, as MySQL compares there as datetimes and reads the other values as zero: a `DATE` when every temporal argument is a `DATE`, else a `DATETIME` with the most fractional digits of any argument (six for strings and DOUBLE literals, the scale for DECIMAL literals, none for integers and hex or bit literals, so `GREATEST('foobar', TIMESTAMP '2020-12-31 23:59:59')` is `'2020-12-31 23:59:59.000000'`), and `NULL` with a NULL argument. `LEAST` compares the arguments as strings, as MySQL does once the least value is not a date. Collated `STRCMP` fails, as MySQL fails to read the string as a date (1525). `STRCMP` of such a literal and a string literal MySQL's datetime parser rejects (`'12_4'`, `'20000101foo'`, `''`) fails the same way, collated or not; next to a valid date string it compares the strings, as MySQL does once the string reads as a date, and a zero date is refused. A `TIME` literal next to a date, a string that may be a date, or any other argument is refused, and so are temporal arguments alone |
| `BIN`, `OCT` and `CONV` of an integer or decimal literal beyond the BIGINT range; `HEX` and `BIT_COUNT` of an integer literal above it | `BIN`, `OCT` and `CONV` read the literal's text, as MySQL converts it: saturating at the BIGINT UNSIGNED limit and wrapping a negative number to its 64 bits (`BIN(-9223372036854775809)` is 63 ones), and with a negative base `CONV` reads it signed, saturating at the BIGINT range. `HEX` and `BIT_COUNT` are the result for its BIGINT UNSIGNED value (`HEX(9223372036854775808)` is `'8000000000000000'`) |
| `FIELD`, `IN` and `CASE x WHEN` of number literals only; a comparison (`=`, `<>`, `<`, `<=`, `>`, `>=`, `<=>`) or `BETWEEN` of number and hex literals of which one is an integer or decimal beyond the BIGINT range or a hex literal above it (exactly, as MySQL compares them as BIGINT UNSIGNED or DECIMAL values: `18446744073709551615 = 18446744073709551614` is 0; as DOUBLE values beside a double literal) | The result MySQL computes comparing them as DOUBLE when one is a double, else as DECIMAL when one has decimals or is beyond the BIGINT and BIGINT UNSIGNED ranges, else as BIGINT, where a literal above the BIGINT range reads as its 64 bits; `IN` compares its left operand with each item this way pair by pair (`9223372036854775808 IN (9223372036854775807, 9e0)` is 0) (`FIELD(18446744073709551615, -1, 9)` is 1, `FIELD(18446744073709551615, 1.0, 18446744073709551616)` is 0). `IN` and `CASE` of BIGINTs mixing such a literal with a negative one are left to Turso |
| A comparison, `IN` list, `BETWEEN` or `CASE x WHEN` whose operands are all `utf8mb4` string literals, or `_latin1`, `_ascii` or `_utf8` string literals of ASCII letters and digits, which their case-insensitive collations compare and order alike | The left operand (the `CASE` operand) gets `COLLATE "und-u-ks-level1"`, MySQL's `utf8mb4_0900_ai_ci` |
| A comparison (`=`, `<>`, `<`, `<=`, `>`, `>=`, `<=>`) of a number literal, `TRUE` or `FALSE` with a string literal (bare or with a `_utf8mb4`, `_utf8mb3`, `_utf8`, `_latin1`, `_ascii` or `_binary` introducer), an `IN` list of such literals where the left operand and an item are a number and a string, and `CASE x WHEN ..` where the operand and the `WHEN` values are such literals of both kinds | Both sides as the DOUBLE literals MySQL compares: a string reads as its longest numeric prefix after leading whitespace (`' 1.5e1x'` is 15, `'foo'` is 0, past the DOUBLE range it is the largest DOUBLE), so `64 = '64'` is `6.4e1 = 6.4e1`; the `IN` list becomes the `OR` of its comparisons, each lowered by these rules, and every `CASE` value becomes a DOUBLE |
| Integer, decimal and scientific literals, `TRUE`, `FALSE`, `NULL` | The same literal text the SQLite parser keeps. A negative decimal, an integer above the BIGINT UNSIGNED range, a negated integer below the BIGINT range (`-9223372036854775809`) and `- -9223372036854775808` are exact DECIMALs, as in MySQL; `-9223372036854775808` is the smallest BIGINT, `(-9223372036854775807 - 1)`. An integer literal of more than 15 digits next to a DOUBLE or string literal in `+ - * /` is a DOUBLE, as MySQL computes it. An integer between the BIGINT and BIGINT UNSIGNED limits is an exact DECIMAL of its digits in a select item, a function result position, a string function argument, the separator of `CONCAT_WS`, the argument of `ABS`, `CEIL`, `CEILING`, `FLOOR`, `ROUND` and `TRUNCATE` (by a literal number of places, `ROUND` and `TRUNCATE` of it are computed as MySQL's BIGINT UNSIGNED result, and refused above that range), an operand of `MOD`, `%` and `/`, and an operand of `+`, `-` and `*` next to a DECIMAL or temporal literal, where it shows as MySQL's BIGINT UNSIGNED does or MySQL computes a DECIMAL; `+`, `-` and `*` of it and another integer literal is the BIGINT UNSIGNED result MySQL computes, and is refused outside that range or inside other arithmetic, where MySQL rejects it or keeps it unsigned; elsewhere it is still a SQLite number (a DOUBLE) |
| `'...'`, `"..."`, `N'...'` | Text (`"..."` is a string, never an identifier) |
| `X'..'`, `0x..`, `b'..'`, `0b..` | A blob; in a comparison with a number, a logical operator, a condition, a numeric function argument, a numeric `CAST`, or a bit operation, the unsigned integer value; in a comparison with a string, text. Function arguments MySQL reads as numbers are also the value: the index of `ELT`, the bits of `MAKE_SET` and `EXPORT_SET`, the counts and positions of `LEFT`, `RIGHT`, `SUBSTRING`/`SUBSTR`/`MID`, `INSERT`, `LOCATE`, `SUBSTRING_INDEX`, `LPAD`, `RPAD`, `REPEAT` and `SPACE`, the length of `SHA2` and `RANDOM_BYTES`, the address of `INET_NTOA`, the places of `FORMAT`, the bases of `CONV`, every argument of `INTERVAL`, and every argument of `FIELD` when another argument is a number (`FIELD(0x61, 97)` is 1, `FIELD(0x61, 'a')` compares bytes). `CONV`, `BIN` and `OCT` of a hex or bit literal convert its value whatever the base: `__mysql_conv_binary_literal(bytes, from, to)`. Any bytes, UTF-8 or not, are a blob in a select item and in the string arguments of the string functions MySQL implements over binary strings (`HEX(0xAACC)`, `LENGTH(0xAACC)`, `REPLACE(0xAACC, 'a', 'b')`) and of the date and time functions (`DATE_FORMAT(0x..)` and `FROM_UNIXTIME(0x..)` among them), and as the separator of `CONCAT_WS` unless it is not UTF-8 next to a JSON value. Compared with `=`, `<>`, `<`, `<=`, `>`, `>=` or `<=>` to another binary or string literal, both sides are their bytes when the binary side has a NUL or is not UTF-8 (`'a' < 0xFF` is 1), as MySQL compares binary strings, without padding. `CAST(x AS DATE/TIME/DATETIME)` reads its bytes; `CAST(x AS CHAR)` of bytes that are not UTF-8 is `NULL`. As an operand of a JSON comparison or of `LIKE` with a JSON value, its bytes. `CONCAT`, `CONCAT_WS` or `STRCMP` with a JSON value and a binary literal that is not UTF-8 is rejected, as MySQL fails to convert it. A negated hex or bit literal is a DOUBLE (`-0x1` is `-1e0`) in a select item, a string or numeric function argument, `PERIOD_ADD`/`PERIOD_DIFF`, a `MOD` whose other arguments hide decimals, and `+ - * /` with a double or string literal. In `+ - * / %` MySQL computes the literal as BIGINT UNSIGNED: with a double or string literal, or in `+ - * / %` with a JSON value, it is the DOUBLE of its value (`X'40' + -1.5e0` is 62.5, `0xff + CAST('[]' AS JSON)` is 255); with a decimal literal, `NULL`, or in a division by or of an integer, its integer value, since the result is no longer unsigned (`0x1 / 7` is 0.1429); with integer literals up to 18446744073709551615, `TRUE`, `FALSE`, DATE, TIME and TIMESTAMP literals without a fraction (their numbers) or other hex literals, the result MySQL computes, known from the literals, when it is in the BIGINT UNSIGNED range next to a hex literal or in the BIGINT range next to bit literals only, and the operation is not itself an operand of arithmetic (`0xff + b'1001'` is 264, `TIME '10:04:58' * 0b1001` is 904122, `-9223372036854775808 % 0x9` is -8, `x'09' % 0` is `NULL`); with `NULL` or arithmetic of `NULL` and a number literal, a temporal literal with a fraction, or in a division, its exact value, the exact DECIMAL above the BIGINT range. As a truth value (`AND`, `OR`, `XOR`, `NOT`, a condition) a literal above the BIGINT range is 1. The value rounded by `ROUND` and `TRUNCATE` is the DOUBLE of its value, as MySQL rounds a binary string (`ROUND(X'ff', 255)` is `255.00000000000003`) |
| `_latin1 X'..'`/`_latin1 0x..` with bytes other than ASCII and NUL, in a select item of a single-row `SELECT` built only from number and string literals (bare or with a `_utf8mb4`, `_utf8mb3` or `_utf8` introducer), `NULL`, `DATE`/`TIME`/`TIMESTAMP` literals, `+ - * / DIV MOD %` of these, `CAST(.. AS JSON)` of these, `_latin1` hex values, hex, bit and `_binary` literals where MySQL reads a number (the positions of `INSERT`, `LOCATE`, the lengths of `LPAD`, `RPAD`, `REPEAT`, the index of `ELT`, `FIELD` compared as numbers), and `CONCAT`, `CONCAT_WS`, `ELT`, `FIELD`, `INSERT`, `INSTR`, `LEFT`, `LOCATE`, `LPAD`, `LTRIM`, `MID`, `POSITION`, `REPLACE`, `REVERSE`, `RIGHT`, `RPAD`, `RTRIM`, `SUBSTR`, `SUBSTRING` and `TRIM` | The text of the bytes in MySQL's `latin1` (cp1252, so `0x80` is `€`), since these functions work on characters and the result is only shown. Where MySQL converts the other arguments to the subject's `latin1` (`INSERT`, `LPAD`, `REPLACE`, `RPAD`, `TRIM`) a string with a character `latin1` lacks is refused, and rejected as MySQL's illegal mix of collations when the subject is a `_latin1` hex literal and the other arguments of `REPLACE`, `LPAD` or `RPAD`, or of a `FIELD` comparing strings, are string, number, hex or bit literals (a `NULL` argument can spare the call, so it is still refused), and a hex, bit or `_binary` literal without NUL is the `latin1` text of its bytes (`LPAD(_latin1 0xFF, 3, 0xAACC)` is `ªÌÿ`). `INSERT` of a `latin1` subject is lowered only at a position that cannot fall past its characters and within its UTF-8 bytes, since MySQL checks the position against its byte length. `FIELD`, `INSTR`, `LOCATE` and `POSITION` comparing strings are lowered only on literal operands whose characters outside ASCII are accented Latin letters (`à`-`ã`, `ç`, `è`-`ë`, `ì`-`ï`, `ñ`, `ò`-`õ`, `ù`-`û`, `ý`, `ÿ` and their capitals) that no other character with the same base letter meets, so `latin1_swedish_ci` and `utf8mb4_0900_ai_ci` agree, without an empty needle after a position where non-ASCII text is in play, and for `FIELD` without trailing spaces. Items that may grow past 16777216 characters, where the byte size of `latin1` decides MySQL's `max_allowed_packet` limit, are refused |
| `_latin1 X'..'`/`_latin1 0x..` as an argument of `CONCAT`, `CONCAT_WS`, `ELT`, `FIELD` (compared as strings), `FIND_IN_SET`, `INSTR`, `LOCATE` or `SUBSTRING_INDEX` next to a hex, bit or `_binary` literal in another string argument, or of `INSERT`, `LPAD`, `REPLACE` or `RPAD` whose subject is such a literal | A blob of its bytes, since MySQL computes these calls on binary strings |
| `_utf8mb4'..'`, `_utf8mb3`, `_utf8`, `_binary`, and `_ascii`/`_latin1` with ASCII text, with a string or hex value | Text; `_binary` is a blob where a hex literal is one, text in a date or time function argument, and in a numeric function argument the text MySQL parses (its bytes up to the first NUL or non-ASCII byte, so `_binary X'40'` is `'@'`, 0), and in `+ - * /` the DOUBLE MySQL parses from its text (`_binary 'foobar' * -1` is `-0`). A plain or `_utf8` string literal in `+ - * /` or under unary `-` is the DOUBLE MySQL reads from its text (the number it starts with, 0 for none), so `-'foobar'` is `-0` and `'' * TIME '-10:04:58'` is `-0`; compared with a column or other non-literal it keeps Turso's number conversion, as Turso compares a text column with a DOUBLE as text (`v IN (0e0)` is false for a `VARCHAR` `'0'`, where MySQL compares doubles) |
| `DATE '..'`, `TIME '..'`, `TIMESTAMP '..'`, `{d '..'}` | Canonical text (`'2018-05-01'`, `'26:00:00'`, `'2020-01-02 03:04:05.5'`); a string literal compared with a `DATE` or `TIMESTAMP` one is converted the same way (`DATE '2018-05-01' = '2018-5-1'`), and one compared with a `TIME` stays a string, which MySQL compares with the canonical text under the default collation (`'5 10:34:58' > TIME '130:34:58.6'` is 1), as is one after a `_utf8`, `_utf8mb3` or `_utf8mb4` introducer; a hex, bit or `_binary` literal compared with a `TIME` one is compared with the bytes of the canonical text (`TIME '-31:34:58' > X'00'` is 1, `TIME '-31:34:58' > 0xFF` is 0), as MySQL compares them as binary strings; in arithmetic, a comparison with a number, a logical operator, a condition or a numeric function argument (including `MAKEDATE`, `MAKETIME`, `SEC_TO_TIME`, `FROM_UNIXTIME`, `FROM_DAYS` and `PERIOD_*`), the number MySQL uses (`20180501`, `-1013458`, `103458.5`), with fractions past 6 digits rounded; `TIME '..'` compared with `TIME '..'` as numbers. A `DATE 'YYYY-MM-DD'` with a month above 12, a day past the end of its month, or `'0000-00-00'` fails like MySQL (1525); two such literals of different types, or two `TIMESTAMP` literals with different numbers of fractional digits, compare as DATETIME values, as MySQL does: the text `YYYY-MM-DD HH:MM:SS.ffffff` of each, a `DATE` at midnight and a `TIME` added to the start of the current (UTC) day (`TIME '31:34:58' > TIMESTAMP '2000-01-01 10:34:58'` is 1, `TIMESTAMP '2000-01-01 10:34:58' = DATE '2000-01-01'` is 0); a string literal compared with a `DATE` or `TIMESTAMP` literal in the form `YYYY-M-D`, `YYYY-M-D H:M:S[.fraction]`, `YYYYMMDD` or `YYYYMMDDHHMMSS[.fraction]` (after spaces) compares as a DATETIME too (`DATE '2000-01-01' < '2000-01-01 00:00:01'` is 1, `TIMESTAMP '2000-01-01 10:34:58.5' = '2000-01-01 10:34:58.50'` is 1) |
| `/*+ ... */` optimizer hints | Ignored: they choose a plan or bound the run time (`MAX_EXECUTION_TIME`), which leaves the result of a statement that finishes unchanged. `SET_VAR` is accepted for variables that only size buffers or steer the optimizer (`sort_buffer_size`, `optimizer_switch`, ...) and refused for any other |
| Index hints (`USE`, `FORCE`, `IGNORE` `INDEX`/`KEY`) | Ignored, in `SELECT` and `UPDATE`. An index name the table does not have is not rejected (MySQL's error 1176), since the frontend does not see the schema |

`WITH RECURSIVE` lowers a common table expression that reads itself, an
anchor `SELECT` `UNION ALL` a recursive `SELECT` that reads it once (as the
first table of a join or an inner-joined one), to an inner expression
`__mysql_recursive_<name>` with a hidden column holding the iteration of each
row, which `__mysql_recursion_depth(depth)` computes and refuses from the
1000th iteration, and the expression itself, which selects the other columns.
MySQL gives the columns the types of the anchor: a column whose anchor is an
integer literal or an `INT` column has its recursive values checked by
`__mysql_recursive_integer(value)`, which refuses a value outside `INT` or not
an integer; a column whose anchor and recursive values are both base table
columns of the same declared type and collation (of the same table, for a text
type, whose character set the table may set) keeps its values; and a column
with another anchor is accepted only when the recursive `SELECT` passes it on
unchanged. Refused: a recursive `SELECT` with
`DISTINCT`, grouping, a window or a subquery, `UNION DISTINCT`, an outer join
of the expression, and a recursive expression in a subquery of an expression or
in a `LATERAL` table, which may read an outer row (MySQL may evaluate it once).
A common table expression of `WITH RECURSIVE` that does not read itself is an
ordinary one.

Refused, because Turso cannot run it with MySQL's meaning yet:

- `WITH RECURSIVE` beyond one anchor `SELECT` `UNION ALL` one recursive
  `SELECT` (see below), and a recursive query that reaches its 1000th
  iteration, where MySQL's `cte_max_recursion_depth` stops it.
- `BINARY x` and `CAST(x AS BINARY)` in a comparison, `IN`, `BETWEEN`, a
  `CASE` operand, arithmetic, a condition, or an argument of `GREATEST`,
  `LEAST`, `FIELD`, `NULLIF` or `INTERVAL`: MySQL compares a binary string with
  a number as numbers, and Turso orders every blob after every number.
- `ROUND`/`TRUNCATE` with a negative literal precision: Turso's `round` does
  not round to tens.
- `REGEXP` with an empty pattern or over a binary string and an operand that
  is not a literal, row values inside row
  values, `VALUES` rows of different lengths: MySQL raises an error.
- Hex and bit literals in arithmetic outside the cases above (`DIV` with an
  operand that is not a literal or a negative result next to a hex literal, unary
  `+`, operands that are not literals, results outside the BIGINT range,
  operations nested in arithmetic, literals above the BIGINT range with
  integers or decimals): MySQL computes them as
  BIGINT UNSIGNED, which fails on negative results. Temporal literals in bit
  operations when negative or fractional, and negated when fractional outside
  a select item that is only the negation (which shows MySQL's DOUBLE with the
  literal's decimals, `-TIMESTAMP '2000-01-01 10:34:58.123456'` is
  `-20000101103458.125000`): MySQL
  computes those as BIGINT UNSIGNED or DOUBLE. `DIV` with a
  literal outside the BIGINT range beside a non-literal, or whose quotient
  MySQL rejects.
- Bit operations on a DOUBLE literal below the BIGINT range, which MySQL
  rejects, on a string literal whose number has an exponent or lies outside
  the BIGINT range, on hex or bit literals longer than 8 bytes, and, while the
  statement runs, on a string, a fractional DOUBLE or a value outside the
  BIGINT range, or with a result above 9223372036854775807 outside a select
  item of a one-row `SELECT`.
- Binary strings that are not NUL-free UTF-8 outside numeric contexts, select
  items, binary-string function arguments, arguments of `GREATEST`, `LEAST`
  and `STRCMP` (their bytes), comparisons with literals and a
  hex, bit or `_binary` literal written by `INSERT`, `UPDATE` or `ON DUPLICATE
  KEY UPDATE` to a `BINARY`, `VARBINARY` or `BLOB` column (its bytes), and `_binary` strings in bit
  operations, which MySQL computes on their bytes. A `_binary` operand of `%`,
  `MOD` or `DIV` is read as the string of its bytes up to the first that is not
  ASCII, as MySQL reads its number; one that starts with whitespace or a control
  character is refused.
- A negated hex or bit literal where MySQL can show the decimals of its
  DOUBLE (`IF(c, -0x1, a)`, `MOD(-0x1, a)`, `-0x1 / 1.5`) and the result is not
  a shown select item of literals, or of 10^15 or more, which MySQL shows with
  fixed digits rather than an exponent. A shown select item that is such a
  literal of 10^15 or more is the text MySQL shows, its DOUBLE's shortest
  round-trip digits padded with zeros (`-0xffffffffffffffff` is
  `'-18446744073709552000'`). A shown select item `a + b`, `a - b`,
  `a % b` or `MOD(a, b)` of such a literal and another one, a hex or bit
  literal (in `%` and `MOD`) or an exact number literal is lowered to the
  text MySQL shows, the DOUBLE with the most decimals of the operands rounded
  from its shortest round-trip digits (`-0.0 + -0xffff` is `'-65535.0'`,
  `-0 + -0xffffffffffffffff` is `'-18446744073709552000'`,
  `MOD(3.141592653589793, -0x1)` is `'0.141592653589793'`, `-255 % -0x1` is
  `'-0'`), unless those digits stop halfway between two shown values. So is a
  shown `CASE`, `IF`, `IFNULL` or `COALESCE` whose results are such literals,
  exact number literals and `NULL`: each result is its text with the most
  decimals of any (`IF(c, 1.5, -0x1)` is `'1.5'` or `'-1.0'`). With a DOUBLE
  or string literal result or operand MySQL shows no fixed decimals, and the
  literal is its DOUBLE (`'64' % -0x1` is `MOD('64', -1e0)`).
- `STRCMP` of a binary string and a `DATE` or `TIMESTAMP` literal: MySQL
  reads the string as a date and fails on it.
- `ROUND`/`TRUNCATE` of an integer literal above `BIGINT`, unless the places
  are `NULL` or -20 or less: MySQL rounds it as `BIGINT UNSIGNED` (and fails
  on overflow), while Turso would round its DOUBLE.
- `CHAR` of a negated DOUBLE literal outside `BIGINT`: MySQL fails on it.
- A JSON value in a comparison, arithmetic or a condition, except the
  failing select items above and the JSON comparisons, conditions and
  arithmetic above, when it is a cast that may fail (of a column, a function
  result or an integer literal above the BIGINT UNSIGNED range) or contains
  one: MySQL may never evaluate it. Other `JSON_*` calls and `->` are lowered
  like JSON functions elsewhere, so an error they raise is reported even
  where MySQL would not evaluate them.
  Also a JSON value compared with a column or a function result, or used with
  `IN`, `BETWEEN` or a bit operator: MySQL converts the other operand by its
  type.
- `CAST(x AS JSON)` as an argument of a function other than `JSON_*` when the
  function is `CONCAT_WS`, `ELT` or `MAKE_SET`, or when another argument is
  not a non-NULL literal, unless the cast cannot fail (the separator of
  `CONCAT_WS` may be such a cast): MySQL stops evaluating arguments at the first NULL
  (`MAKEDATE(CAST('x' AS JSON), NULL)` is NULL) or never evaluates unused
  ones, while Turso evaluates every argument and reports the invalid JSON.
  `FIELD`, `GREATEST`, `LEAST`, `LOCATE`, `INSTR`, `POSITION` and
  `FIND_IN_SET` with a JSON argument are refused too: MySQL compares with
  `utf8mb4_bin` there.
- `DATE` and `TIMESTAMP` literals compared with a string in another form, or
  one MySQL rejects outside such a select item, temporal literals compared with a DECIMAL literal that has the same DOUBLE value (MySQL compares
  them as DOUBLE); `DATETIME '..'`; invalid temporal literals; odd
  `X'...'` digits; character set introducers other than the ones above.
- Expressions nested deeper than 100 levels (the SQLite parser's limit).
- A grouped `SELECT` (with `GROUP BY`, or with an aggregate and no `GROUP BY`)
  whose select list or `HAVING` may use a column that is neither grouped nor
  inside an aggregate, including `*`: MySQL's default `ONLY_FULL_GROUP_BY`
  rejects it (error 1055 or 1140) unless the column depends on a key, and
  Turso does not check it. The same for an `ORDER BY` of a grouped `SELECT`
  that may use such a column, and of a `SELECT DISTINCT` that may use a column
  outside the select list (error 3065).
- `CAST(x AS JSON)` of a DECIMAL literal a DOUBLE cannot show exactly (more
  than 15 significant digits, or trailing zeros such as `1.50`): MySQL keeps
  the decimal in the JSON value, Turso a DOUBLE. And of a `BINARY` cast:
  MySQL's opaque JSON value reaches other functions as a JSON string.
- `SQL_*`, `HIGH_PRIORITY` and `STRAIGHT_JOIN` modifiers, `STRAIGHT_JOIN`
  joins, `PARTITION (...)` selection, `INTO`, locking reads that name
  their tables (`FOR UPDATE OF t`; see Transactions), `WITH ROLLUP`, `TABLE t` in a statement that does not start with it, `VALUES` without
  `ROW`, derived-table column lists `AS t(a, b)`, `EXCEPT ALL`, `INTERSECT
  ALL`, `FULL JOIN`, `NULLS FIRST/LAST`.
- User and system variables outside the cases under Session settings, `:name` and `?N` parameters,
  `MATCH ... AGAINST`, `SOUNDS LIKE`, `COLLATE` with any other
  collation or on a non-`utf8mb4` operand, `CAST` to `YEAR`, `BINARY` or with a character set other than the cases above,
  `CONVERT(... USING ...)` with any other character set (except in `HEX` of a
  literal, above) or to ASCII anywhere else, and `^` over
  function calls or subqueries.
- `CAST` to `DATE`, `DATETIME` or `TIME` in a comparison, arithmetic, a
  condition, a numeric function argument or a JSON cast, where MySQL uses the
  temporal value or its number (`CAST('2000-01-01' AS DATE) + 0` is
  20000101), and of an operand that may have a DATE or TIME type (a column, a
  function result, `TIME '..'` to a date, `DATE '..'` to `TIME`): MySQL
  converts those from the type, not from the text (`CAST(TIME '10:00:00' AS
  DATETIME)` is today at 10:00).
- `CAST(x AS JSON)` of a `DATE`, `TIME` or `TIMESTAMP` value outside the
  comparisons and the shown, text, number, datetime and time positions above:
  MySQL makes a JSON date or time, which keeps its type inside JSON, while a
  JSON value in Turso is its text, so a position whose read is not known is
  refused.

Known differences that remain on the lowered path: `/`
is SQLite division (integer division for two integers) rather than MySQL's
DECIMAL division; integer overflow becomes a REAL instead of an error; string comparisons and
`LIKE` use binary or ASCII case folding rather than `utf8mb4_0900_ai_ci`;
`WHERE` may refer to select-list aliases; `ORDER BY` with `DISTINCT` may use
columns outside the select list; unaliased columns are named after the expression as sqlparser
prints it (string literals after their value); and the result of a `DATE`,
`DATETIME` or `TIME` cast is text, so an aggregate such as `SUM` reads its
leading number rather than MySQL's 20000101, and a decimal literal reaches the
cast as a DOUBLE (`CAST(20000101103458.1234 AS DATETIME(6))` ends in
`.125000`).

## `INSERT`

A column list that names a column twice fails with MySQL's error 1110.

`DEFAULT` as a value of `VALUES`, `INSERT ... SET`, `ON DUPLICATE KEY UPDATE`
or `UPDATE ... SET` writes the stored default of the column: NULL for the
`AUTO_INCREMENT` key (the next value) and for a nullable column without a
default. A generated column is left out of the insert when every row writes
`DEFAULT` to it, and fails with MySQL's error 3105 otherwise. A `NOT NULL`
column without a default fails a plain `INSERT` with error 1364 (as in strict
mode) and is refused elsewhere.

`mysql/parser/insert.rs` lowers `INSERT [INTO] t [(columns)]` with a
`VALUES` list (several rows allowed), a `SELECT` (also a `UNION` or `WITH`
query), or `SET c = expr, ...`, and `INSERT INTO t () VALUES ()` to
`DEFAULT VALUES`. `LOW_PRIORITY`, `DELAYED` and `HIGH_PRIORITY` are
accepted and have no effect.

`ON DUPLICATE KEY UPDATE c = expr, ...` after `VALUES` lowers to Turso's
upsert, `INSERT ... ON CONFLICT DO UPDATE SET ...` without a conflict
target, which like MySQL updates the row whose primary or unique key the
inserted row duplicates (also a row inserted earlier by the same
statement). `VALUES(c)` is `excluded.c`, the value the row would have
inserted, and `t.c` targets and references of the inserted table drop the
qualifier.

`INSERT IGNORE` lowers to `INSERT ... ON CONFLICT DO NOTHING`, which like
MySQL skips a row whose primary or unique key duplicates a stored row or one
inserted earlier by the same statement. MySQL's `IGNORE` also turns other
errors into warnings: it stores the implicit default for `NULL` in a `NOT
NULL` column, skips rows that fail a `CHECK` or foreign key, and adjusts
values that do not fit the column. Those rows still fail here, with the
error the statement has without `IGNORE`.

Refused: `INSERT IGNORE` with `ON DUPLICATE KEY UPDATE` or `() VALUES ()`, `DEFAULT` as a value, a schema-qualified
table, `ON DUPLICATE KEY UPDATE` after a `SELECT` (MySQL lets the update read
the selected columns), the `AS alias` row alias, an `INSERT ... SET` or
update that assigns a column twice, an `INSERT ... SET` value that reads a
column, and an update assignment that reads a column an earlier assignment
wrote (MySQL assigns left to right, so `a = a + 1, b = a` sets `b` to the new
`a`, while the upsert reads the old row).

Known differences: the affected-row count is SQLite's (1 per inserted or
updated row) rather than MySQL's 2 for an update and 0 for an unchanged
row; and an update consumes no `AUTO_INCREMENT` value.

## `UPDATE` and `DELETE`

`mysql/parser/dml.rs` lowers single-table `UPDATE` and `DELETE` as they are.
The other forms find their rows by Turso's row id:

- `UPDATE t SET ... WHERE c ORDER BY o LIMIT n` and `DELETE FROM t WHERE c
  ORDER BY o LIMIT n` change the rows of `rowid IN (SELECT rowid FROM t WHERE
  c ORDER BY o LIMIT n)`.
- `DELETE t FROM t JOIN ... WHERE c` and `DELETE FROM t USING t JOIN ...` are
  `DELETE FROM t WHERE rowid IN (SELECT t.rowid FROM t JOIN ... WHERE c)`.
  Turso reads the whole subquery before it deletes a row, as MySQL finds the
  rows before deleting them.
- `UPDATE t JOIN ... SET t.c = v WHERE c` is an `UPDATE ... FROM` over the
  joined rows grouped by `t.rowid`. Turso computes the grouped rows before it
  changes one, so a join of `t` with itself (or with a derived table that
  reads `t`) reads the old rows, as MySQL does when it buffers such a join.
  MySQL changes each target row once, with the values of the first joined row
  that matches it. When a value may read another table, a target row that
  several joined rows match is refused at run time, and the statement changes
  no row.

Refused: `LIMIT` without `ORDER BY` (MySQL changes the first rows it reads,
in an order Turso does not share); an ordered `UPDATE` that assigns a column
it orders by (`SET k = k + 1 ORDER BY k DESC` avoids duplicate keys only in
MySQL's order); `ORDER BY` a position; a multi-table `UPDATE` that changes
more than one table, assigns a column without its table name (MySQL finds the
table by its columns), or whose values or condition may hold an aggregate or a
window function; a multi-table `DELETE` from several tables; a target that
is not exactly one table of the join; a subquery in the values or condition
that reads the target table (MySQL's error 1093); and the `LOW_PRIORITY`,
`QUICK` and `IGNORE` modifiers. Since the row id is how these statements find
their rows, `CREATE TABLE` and `ALTER TABLE ... RENAME COLUMN` refuse a column
named `rowid`, which MySQL allows.

`TRUNCATE [TABLE] t` lowers to `DELETE FROM t`. It is refused where the two
differ: inside a transaction (which `TRUNCATE` commits), for a table with an
`AUTO_INCREMENT` column (whose counter `TRUNCATE` resets), with triggers
(which `TRUNCATE` does not fire), or referenced by a foreign key (which MySQL
refuses with 1701, or ignores for one of the table itself).

## `SHOW TABLES`

`SHOW [FULL] TABLES [LIKE 'pattern']` lists the tables and views of the
database from `sqlite_schema`, with the case each was created with (read from
its stored SQL, since Turso keeps `sqlite_schema.name` in lower case), in byte
order and with a pattern that matches with case, as MySQL does with its
default `lower_case_table_names = 0`. `FULL` adds `Table_type` (`BASE TABLE`
or `VIEW`). The column is named `Tables_in_main`, after Turso's name for the
database. A pattern with a `\` escape, `WHERE`, `FROM`/`IN` and the other
`SHOW` statements are refused.

## `SHOW VARIABLES`

`SHOW [SESSION] VARIABLES LIKE 'name'` and `SHOW [SESSION] VARIABLES WHERE
Variable_name = 'name'` of a system variable that `@@name` reads (see Session
settings) give one row of `Variable_name` and `Value`, the value as MySQL
shows it there (`ON` or `OFF` for `autocommit`, `unique_checks`,
`explicit_defaults_for_timestamp`, `sql_auto_is_null`, `transaction_read_only`
and `sql_safe_updates`). The name compares without case, and a `_` in the
pattern matches itself, as no other MySQL variable differs from a known one
only there. Refused: `GLOBAL`, a pattern with `%` or an escape, another
`WHERE`, no filter, and other variables, since those may match variables the
frontend does not know.

## Transactions

The lowering of a statement depends on whether the session is inside a
transaction. Core asks the dialect to parse with the connection
(`Dialect::parse_for`), and the MySQL dialect passes its autocommit state to
`turso_mysql_parser::parse_and_translate_in`. A prepared statement records
that state, and core prepares it again when it runs in the other state
(`Dialect::parse_reads_transaction_state`; other dialects keep the plain
`Dialect::parse`). `mysql/parser/transaction.rs` lowers, for a session with
the default `autocommit = 1`:

| Statement | Outside a transaction | Inside one |
|---|---|---|
| `START TRANSACTION`, `BEGIN [WORK]` | `BEGIN IMMEDIATE` | Refused (MySQL commits the open transaction first) |
| `COMMIT [WORK]`, `ROLLBACK [WORK]` | No-op, as in MySQL | `COMMIT`, `ROLLBACK` |
| `SAVEPOINT s` | No-op | `SAVEPOINT s` |
| `ROLLBACK TO [SAVEPOINT] s`, `RELEASE SAVEPOINT s` | Fails: the savepoint does not exist (1305) | `ROLLBACK TO s`, `RELEASE s` |

A failed statement inside a transaction is undone and the transaction stays
open, as in InnoDB. DDL, `TRUNCATE`, `LOCK TABLES`, `UNLOCK TABLES` and
`SET autocommit` inside a transaction are refused, since MySQL commits the
transaction before them. `AND CHAIN`, `RELEASE` after `COMMIT`/`ROLLBACK`,
and access modes (`READ ONLY`, `READ WRITE`, `WITH CONSISTENT SNAPSHOT`) are
refused.

`BEGIN IMMEDIATE` takes the database's write lock when the transaction
starts, so the transactions of different connections run one after the other
(a connection that writes while another holds a transaction fails with
`database is locked` instead of waiting, as InnoDB does). A transaction's
reads then see the latest committed rows, and every isolation level gives
the result of running the transactions in that order. So locking reads
(`FOR UPDATE`, `FOR SHARE`, with `NOWAIT` or `SKIP LOCKED`; `LOCK IN
SHARE MODE` does not parse yet) read what a plain read does: no other transaction can hold a row.
`SET SESSION TRANSACTION ISOLATION LEVEL level` (or `LOCAL`) stores the
session's `@@transaction_isolation` through
`__mysql_set_transaction_isolation(level)`. The form without `SESSION`, which
applies to the next transaction only, and `GLOBAL` are refused.

## Server functions

`VALIDATE_PASSWORD_STRENGTH(password)` is 0, as in the target profile, which
does not install MySQL's `validate_password` component (MySQL then rates every
password 0). A NULL argument is refused.

## Session settings (`SET`)

`mysql/parser/session.rs` lowers `SET` statements (session scope, with or
without `SESSION`, `LOCAL`, `@@`, `@@session.` or `@@local.`; several
assignments in one statement are allowed):

| Statement | Lowering |
|---|---|
| `SET NAMES utf8mb4 [COLLATE utf8mb4_0900_ai_ci]`, `time_zone` set to `'+00:00'` or `'UTC'`, `autocommit` set to 1 or `ON` (outside a transaction), `sql_mode` set to exactly the default modes of the target profile (in any order), `character_set_client`, `character_set_connection` or `character_set_results` set to `utf8mb4`, `collation_connection` set to `utf8mb4_0900_ai_ci` | A compatibility no-op (`SELECT 0 WHERE 0`, which returns no rows): the session already has these values in the target profile |
| `FOREIGN_KEY_CHECKS` set to 0, 1, `ON`, `OFF`, `TRUE` or `FALSE` | `PRAGMA foreign_keys = 0/1`, which Turso applies at once, also inside a transaction, as MySQL does. Turso then checks foreign keys like SQLite: an `UPDATE` that writes the same key value is checked too, where InnoDB skips it |

| `time_zone`, `sql_mode`, the character sets or `collation_connection` set to an expression (`SET character_set_client = @saved_cs_client` in mysqldump output) | `__mysql_keep_session_settings(name, value, ...)`, which accepts the values above when it runs and refuses any other |
| `SET @a = x, @b = y` (user variables) | `SELECT 0 WHERE __mysql_set_user_variables('a', default_collation, x, ...)`, which stores the values in the connection's session (`turso_mysql::open_*` connections only) |
| `PREPARE name FROM 'text'` or `FROM @variable` | `SELECT 0 WHERE __mysql_prepare('name', text)`, which drops the session's statement `name`, then lowers `text` with each `?` read from a user variable and compiles it without running it, so a syntax error, an unknown table or column, more than one statement or a nested `PREPARE`/`EXECUTE`/`DEALLOCATE` (1295) fails as in MySQL, and keeps `text` as `name` (names are compared without case). A NULL variable fails (1064) and a number is refused |
| `EXECUTE name [USING @a, @b, ...]` | The kept text lowered with its `?` parameters replaced, in order, by the user variables, so each reads as `@a` does above; an unknown name fails (1243) and a different number of variables (1210). A `LIMIT` or `OFFSET` parameter must hold a count of rows; another value is refused |
| `DEALLOCATE PREPARE name`, `DROP PREPARE name` | `SELECT 0 WHERE __mysql_deallocate_prepare('name')`, which drops it; an unknown name fails (1243) |

Everything else is refused: other values of these variables (`sql_mode`,
`time_zone`, `autocommit = 0` and character sets change the semantics of
later statements), `GLOBAL` scope, other variables, and an assignment that
reads a user variable assigned earlier in the same `SET` (the values are all
read before any is stored).

`PREPARE` and `DEALLOCATE PREPARE` make the connection prepare its statements
again before they next run, so an API statement of `EXECUTE name` runs the
text `name` has then. A parameter reads its variable when the statement runs,
as MySQL binds it, so a temporal or `DECIMAL` value, which a user variable
does not keep, is refused when it is assigned.

### User and system variables

A user variable `@name` (case-insensitive) reads
`__mysql_user_variable('name')`: NULL until it is set, then the value last
assigned in the session. MySQL gives a variable the type and collation of the
value assigned, so the frontend keeps what a plain value carries and refuses
the rest:

- integers, doubles, binary strings and NULL are kept;
- a string is read back with `utf8mb4_0900_ai_ci`, so it is kept only when
  its collation is known to be that: a string literal or another user
  variable. A string from a column, a function such as `CONCAT` or a JSON
  value, whose collation MySQL derives from that source, is refused when
  read (it can still be passed to a `SET` of a setting);
- `DECIMAL`, temporal, `ENUM`, `SET` and other typed values are refused when
  assigned;
- `@a := x` and `SELECT ... INTO @a` are not lowered.

A system variable `@@name` or `@@session.name` whose value the target profile
fixes reads as that constant: `sql_mode`, `time_zone` (`+00:00`), the
`character_set_*` and
`collation_*` variables of the connection, server and database, `version`
(`8.4.10`), `autocommit`, `auto_increment_increment`,
`auto_increment_offset`, `unique_checks`, `explicit_defaults_for_timestamp`,
`lower_case_table_names`, `sql_auto_is_null`, `transaction_read_only`,
`max_execution_time`, `sql_safe_updates`, `default_week_format`,
`div_precision_increment`, `group_concat_max_len`, `max_allowed_packet` and
`default_storage_engine`. `@@transaction_isolation` reads the session's
level (`REPEATABLE-READ` until `SET SESSION TRANSACTION ISOLATION LEVEL`
changes it). `@@global.` and other variables are refused.

### Connection id and user-level locks

`CONNECTION_ID()` is a number that identifies the session among those of the
process. `GET_LOCK`, `RELEASE_LOCK`, `RELEASE_ALL_LOCKS`, `IS_FREE_LOCK` and
`IS_USED_LOCK` keep MySQL's user-level locks: a lock belongs to the session
that took it (as many times as it took it) until it releases it or closes,
names are compared without case, and a `NULL`, empty or longer than 64
characters name fails (3057). Like MySQL's, which belong to the server, the
locks are shared by every session of the process. `GET_LOCK` of a lock that
another session holds returns 0 with a timeout of 0 and is refused with any
other timeout, since it would wait. Names that are not ASCII and values other
than strings and integers are refused.

## JSON functions

`mysql/frontend/functions/json/` implements MySQL's JSON functions for the
MySQL dialect, replacing SQLite's functions of the same name: `JSON_ARRAY`,
`JSON_OBJECT`, `JSON_QUOTE`, `JSON_UNQUOTE`, `JSON_EXTRACT`, `JSON_VALUE`
(two-argument form), `JSON_TYPE`, `JSON_VALID`, `JSON_KEYS`, `JSON_LENGTH`,
`JSON_DEPTH`, `JSON_PRETTY`, `JSON_CONTAINS`, `JSON_CONTAINS_PATH`,
`JSON_OVERLAPS`, `JSON_SEARCH`, `JSON_SET`, `JSON_INSERT`, `JSON_REPLACE`,
`JSON_REMOVE`, `JSON_ARRAY_APPEND`, `JSON_ARRAY_INSERT`, `JSON_MERGE`,
`JSON_MERGE_PRESERVE` and `JSON_MERGE_PATCH`, and the aggregates
`JSON_ARRAYAGG` and `JSON_OBJECTAGG`. They follow MySQL 8.4.10:

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
- Comparisons, conditions and arithmetic of JSON values use JSON rules only
  where the lowering above sees the JSON value (a cast, `->` or a `JSON_*`
  call). A JSON value from a column, a subquery or `CASE` is text there.
- Turso has no `DECIMAL`, unsigned or temporal types, and `TRUE` is the
  integer 1, so these never become JSON decimals, unsigned integers, dates or
  booleans; binary strings become MySQL's `base64:type15:` opaque values.
- `JSON_OBJECT` rejects a double key with error 3144, as MySQL does. A
  `DECIMAL` literal key is the literal's text, as in MySQL (`01.50` is
  `"1.50"`); another `DECIMAL` key, such as a column or a computation, is a
  double in Turso and fails.

The lowering of syntax that MySQL implements with JSON functions can call
these internal functions, which MySQL does not have:

| MySQL syntax | Internal function |
|---|---|
| `CAST(x AS JSON)` | `__mysql_cast_as_json(x)` |
| A boolean expression (`TRUE`, `a = b`, ...) used as a JSON value, or `CAST(<boolean> AS JSON)` | `__mysql_cast_boolean_as_json(x)` |
| `x MEMBER OF (y)` | `__mysql_member_of(x, y)` |
| `doc -> path` | `JSON_EXTRACT(doc, path)` |
| `doc ->> path` | `JSON_UNQUOTE(JSON_EXTRACT(doc, path))` |
| `JSON_ARRAYAGG(x)` | `__mysql_json_arrayagg(json_group_array(JSON_ARRAY(x)))`: each row converts as a `JSON_ARRAY` value; no rows give NULL |
| `JSON_OBJECTAGG(k, v)` | `__mysql_json_objectagg(json_group_array(JSON_OBJECT(k, v)))`: keys as in `JSON_OBJECT` (a NULL key fails with 3158), the last value of a repeated key wins; no rows give NULL |
| `JSON_TABLE(doc, path COLUMNS (...)) AS t` | `(SELECT __mysql_json_table_cell(r.value, 0) AS c1, ... FROM json_each(__mysql_json_table(doc, spec)) AS r) AS t`, `LATERAL` when `doc` reads a column: `__mysql_json_table` computes MySQL's rows (`FOR ORDINALITY`, `PATH`, `EXISTS PATH`, `NESTED PATH`, `ON EMPTY`/`ON ERROR` with `NULL`, `ERROR` (3665, 3156) or `DEFAULT`) as a JSON array, and `__mysql_json_table_cell` reads one column of a row |

`JSON_TABLE` columns may be integer types up to `BIGINT`, `FLOAT`, `DOUBLE`,
`CHAR`, `VARCHAR` (comparing with the default collation) and `JSON`. A
document that is a subquery or reads a common table expression fails with
1210, as in MySQL. Refused: other column types, an alias with a column list,
a document holding a subquery elsewhere or reading a derived table, a
conversion whose MySQL result is not certain (a double or a string with a
leading number into an integer, a string longer than its column, a `FLOAT`
that is not exact, a JSON `null` into a `JSON` column, JSON booleans), a
`DEFAULT` that does not convert, and the error of a column that is not an
integer or not on the first row.

Not implemented: `JSON_ARRAYAGG` and `JSON_OBJECTAGG` as window functions,
`JSON_SCHEMA_VALID`, `JSON_SCHEMA_VALIDATION_REPORT`, `JSON_STORAGE_SIZE`,
`JSON_STORAGE_FREE` and the `RETURNING`/`ON EMPTY`/`ON ERROR` clauses of
`JSON_VALUE`.

## Date and time functions

`mysql/frontend/functions/datetime/` implements MySQL's date and time
functions for the MySQL dialect, replacing SQLite's `DATE`, `TIME` and
`TIMEDIFF`: `YEAR`, `QUARTER`, `MONTH`, `MONTHNAME`, `DAY`, `DAYOFMONTH`,
`DAYOFYEAR`, `DAYOFWEEK`, `WEEKDAY`, `DAYNAME`, `WEEK`, `WEEKOFYEAR`,
`YEARWEEK`, `HOUR`, `MINUTE`, `SECOND`, `MICROSECOND`, `TO_DAYS`,
`TO_SECONDS`, `FROM_DAYS`, `LAST_DAY`, `MAKEDATE`, `MAKETIME`, `TIME_TO_SEC`,
`SEC_TO_TIME`, `PERIOD_ADD`, `PERIOD_DIFF`, `DATE`, `TIME`, `TIMESTAMP`
(one and two arguments), `DATEDIFF`, `TIMEDIFF`, `ADDTIME`, `SUBTIME`,
`ADDDATE`/`SUBDATE` with a number of days, `DATE_FORMAT`, `TIME_FORMAT`,
`STR_TO_DATE`, `FROM_UNIXTIME`, `UNIX_TIMESTAMP`, `CONVERT_TZ`, and `NOW`,
`SYSDATE`, `CURDATE`, `CURTIME`, `UTC_DATE`, `UTC_TIME`, `UTC_TIMESTAMP`,
`LOCALTIME`, `LOCALTIMESTAMP` and the `CURRENT_*` functions, and the
lowered `CAST(x AS DATE/DATETIME(p)/TIME(p))` as `__mysql_cast_as_date(x)`,
`__mysql_cast_as_datetime(x, p)` and `__mysql_cast_as_time(x, p)`, which
convert like `DATE`, `TIMESTAMP` and `TIME` but round to `p` digits. They
follow MySQL 8.4.10:

- Arguments convert as MySQL converts them: strings with MySQL's datetime
  and time parsers (delimiters, `T`, two-digit years, packed `YYYYMMDDhhmmss`
  forms, trailing garbage, `+hh:mm` displacements, rounding of extra
  fractional digits), integers and doubles as packed numbers, and the
  `sql_mode` of the target profile (`NO_ZERO_DATE` rejects `'0000-00-00'`
  strings; `DATE()` and `TIMESTAMP()` also reject zero months and days).
- JSON values from the JSON functions convert as MySQL converts JSON: a
  JSON string as its text, numbers and booleans only in numeric positions;
  other JSON values are `NULL` in date and time positions and `0` in numeric
  positions.
- Results print as MySQL prints them: `TIME` values from `-838:59:59` to
  `838:59:59` (larger values are clamped), and fractional seconds with the
  precision MySQL derives from the argument (the digits written in a string,
  6 for a double, 0 for an integer).
- `WEEK` modes 0 to 7 and `default_week_format=0`, including MySQL's unsigned
  arithmetic around year 0.
- `CONVERT_TZ` accepts offsets from `-13:59` to `+14:00`, `SYSTEM` (UTC in
  the target profile) and named zones. Named zones are read from the host's
  zoneinfo files (`TZDIR` or `/usr/share/zoneinfo`, names matched without
  case) using only the transitions MySQL's `mysql_tzinfo_to_sql` loads, so
  times after 2037 keep the last offset as in MySQL; without the files a
  named zone is unknown and the result is `NULL`, as in MySQL without time
  zone tables.
- `PERIOD_ADD` and `PERIOD_DIFF` raise error 1210 for invalid periods, and
  converting a negative double beyond the `BIGINT` range to an integer raises
  error 1690.

The lowering of MySQL syntax that has no function form calls these internal
forms, which take the unit as an upper-case string:

| MySQL syntax | Function |
|---|---|
| `DATE_ADD(d, INTERVAL v unit)`, `ADDDATE(d, INTERVAL v unit)`, `d + INTERVAL v unit` | `DATE_ADD(d, v, 'UNIT')` |
| `DATE_SUB(d, INTERVAL v unit)`, `SUBDATE(d, INTERVAL v unit)`, `d - INTERVAL v unit` | `DATE_SUB(d, v, 'UNIT')` |
| `EXTRACT(unit FROM d)` | `EXTRACT('UNIT', d)` |
| `TIMESTAMPADD(unit, n, d)`, `TIMESTAMPDIFF(unit, a, b)` | `TIMESTAMPADD('UNIT', n, d)`, `TIMESTAMPDIFF('UNIT', a, b)`; `TIMESTAMPADD(SECOND, n, d)` of a literal `n` with decimals and a literal `d` is wrapped in `__mysql_cast_as_datetime(.., digits)` with the digits MySQL shows: those of a DECIMAL `n`, four more than the dividend's for a division of exact numbers, six for a double or string, or the date's when it has more |
| `GET_FORMAT(DATE, 'USA')` | `GET_FORMAT('DATE', 'USA')` |

Turso has no `DATE`, `TIME`, `DATETIME`, `DECIMAL` or unsigned types, so:

- Temporal results are strings. In a numeric context MySQL turns them into
  numbers (`SEC_TO_TIME(2378) + 0` is `3938`); Turso does not. A
  `DATE '...'`, `TIME '...'` or `TIMESTAMP '...'` literal is a string, so
  interval arithmetic on other temporal values (columns, `CAST(.. AS
  DATETIME)`, `NOW()`) uses MySQL's string form: 6 fractional digits when
  there is a fraction, none otherwise.
- Decimal literals reach the functions as doubles, so their precision and
  fractional digits would be lost. Where the result's fractional digits come
  from a literal argument (`SEC_TO_TIME`, `TIME`, `FROM_UNIXTIME` with one
  argument, the seconds of `MAKETIME`, the first argument of `CONVERT_TZ`),
  a DECIMAL, `TIME` or `TIMESTAMP` literal there sets them: the call is
  wrapped in `__mysql_cast_as_time(.., digits)` or
  `__mysql_cast_as_datetime(.., digits)`, so `MAKETIME(10, 34, 58.1234)` is
  `10:34:58.1234`. Such a DECIMAL with more than 15 significant digits is
  refused, except a `CONVERT_TZ` datetime number with 14 integer digits,
  which is passed as its text. A DECIMAL literal where `MAKEDATE`,
  `MAKETIME` (hours, minutes), `PERIOD_ADD`, `PERIOD_DIFF` or `FROM_DAYS`
  read an integer is rounded half away from zero while lowering, as MySQL
  rounds a DECIMAL; for `PERIOD_ADD` and `PERIOD_DIFF` one beyond the BIGINT
  range (a number literal with decimals, above 18446744073709551615 or below
  -9223372036854775808) is the BIGINT limit it saturates to
  (`PERIOD_ADD(18446744073709551616, 1)` reads 9223372036854775807).
  `TIME(DATE '..')` is `'00:00:00'`. `UNIX_TIMESTAMP` of a literal is
  `CAST(UNIX_TIMESTAMP(x) AS DECIMAL(11 + d, d))`, with the `d` decimals
  MySQL gives it: those of a DECIMAL literal, 6 for a double, and for a
  string the fractional digits it reads with as a datetime, or 6 when it
  does not read as one, so `UNIX_TIMESTAMP('foo') + 1` is `1.000000`. The
  parser reads the string with the frontend's datetime parser, which lives
  in `mysql/parser/datetime/` for that. A DECIMAL literal there with more
  than 6 decimals, or with more than 15 significant digits other than a
  14-digit date and time (passed as its text), is refused. Such a DECIMAL
  with a 14-digit integer part read as a datetime or time (the first argument
  of `DATE`, `DATE_FORMAT`, `DAY` and the other part functions, `HOUR`,
  `MINUTE`, `SECOND`, `MICROSECOND`, `TIME_TO_SEC`, `TO_DAYS`, `TO_SECONDS`,
  `WEEK`, `YEARWEEK`, and a `DATE`, `DATETIME` or `TIME` cast) is passed as
  its text, which MySQL reads alike, so `MICROSECOND(20000101103458.1234)` is
  `123400`. Other decimal arguments still lose digits
  (`MICROSECOND(2000010110345.1234)`).
- A `BLOB` argument is read as MySQL reads a hexadecimal literal: as a
  string in date and time positions and as a `BIGINT UNSIGNED` made of its
  bytes in numeric positions (`MAKEDATE(X'31', 1)` is `2049-01-01`). A
  `_binary '..'` string would arrive as the same `BLOB`, so it is lowered to
  its text instead, which these functions parse as MySQL does.
- An integer literal above the `BIGINT` range becomes a double. The doubles
  for `9223372036854775808` and `18446744073709551615` are read as those
  `BIGINT UNSIGNED` values, so `18446744073709551616` behaves like
  `18446744073709551615`.
- Warnings are not reported.

Not implemented: locales other than `en_US` (`lc_time_names`) and leap
seconds.

## String, numeric, hashing, network and regular expression functions

`mysql/frontend/functions/string/` gives the MySQL dialect its own versions of
these scalar functions, for lowered statements and for statements that fall
back to SQLite parsing. Owned specs: `conformance/my-sqltests/functions-string.sqltest`.

- Numeric: `ABS`, `ACOS`, `ASIN`, `ATAN` (one or two arguments), `ATAN2`,
  `BIN`, `BIT_COUNT`, `CEIL`/`CEILING`, `CONV`, `COS`, `COT`, `DEGREES`, `EXP`,
  `FLOOR`, `LN`, `LOG` (one or two arguments), `LOG10`, `LOG2`, `MOD`, `OCT`,
  `POW`/`POWER`, `RADIANS`, `ROUND`, `SIGN`, `SIN`, `SQRT`, `TAN`, `TRUNCATE`,
  and `FORMAT(x, d[, locale])`, which rounds a DECIMAL or integer exactly,
  half away from zero, and a DOUBLE or string half to even after scaling, as
  MySQL does, to at most 30 places (read as MySQL reads an integer, so a
  DECIMAL `1.5` is 2 places), then groups thousands for `en_US` (the default
  and a NULL locale), `da_DK` or `de_DE`; other locales and a negative value
  that rounds to zero are refused.
- Comparison: `FIELD`, `GREATEST`, `INTERVAL`, `LEAST`, `STRCMP`.
- String: `ASCII`, `BIT_LENGTH`, `CHAR`, `CHAR_LENGTH`/`CHARACTER_LENGTH`,
  `CONCAT`, `CONCAT_WS`, `ELT`, `EXPORT_SET`, `FIND_IN_SET`, `HEX`, `INSERT`,
  `INSTR`, `LCASE`/`LOWER`, `LEFT`, `LENGTH`/`OCTET_LENGTH`, `LOCATE`,
  `POSITION`, `LPAD`, `RPAD`, `MAKE_SET`, `MID`, `ORD`, `REPEAT`, `REPLACE`,
  `RIGHT`, `SPACE`, `SUBSTR`/`SUBSTRING`, `SUBSTRING_INDEX`, `UCASE`/`UPPER`,
  `UNHEX`, and `TRIM`/`LTRIM`/`RTRIM` as `__mysql_trim`, `__mysql_ltrim` and
  `__mysql_rtrim(s, r)`, which remove whole repeats of `r` byte by byte (case
  matters, an empty `r` removes nothing).
- Hashing and encoding: `CRC32`, `MD5`, `SHA`/`SHA1`, `SHA2`, `TO_BASE64`,
  `FROM_BASE64`.
- Network and UUID: `INET_ATON`, `INET_NTOA`, `INET6_ATON`, `INET6_NTOA`,
  `IS_IPV4`, `IS_IPV6`, `IS_IPV4_COMPAT`, `IS_IPV4_MAPPED`, `IS_UUID`,
  `UUID_TO_BIN`, `BIN_TO_UUID`, and `UUID()` (also in a column default),
  `__mysql_uuid(randomblob(8))`: a version 1 UUID of the current time, like
  MySQL's, whose clock sequence and node are random per process where MySQL
  uses the server's.
- Regular expressions: `REGEXP_LIKE`, `REGEXP_INSTR`, `REGEXP_SUBSTR`,
  `REGEXP_REPLACE` and the `REGEXP` operator.

Arguments are converted the way MySQL converts them: a string used as a number
takes its longest numeric prefix (`'12abc'` is 12, `'0x10'` is 0), an integer
argument read from a string stops at the first non-digit and wraps above the
signed range, and a DOUBLE used as an integer rounds half to even. A BLOB
argument is a MySQL binary string: string functions work on its bytes and
return binary strings, and used as a number it is parsed like text (MySQL
reads hex literals as numbers instead, so those must reach numeric arguments
as integers). Numbers used as strings print as MySQL prints them. Strings
compare with `utf8mb4_0900_ai_ci` (ICU root collation at primary strength: no
case or accent differences, trailing spaces count). Binary strings compare
byte by byte in `STRCMP`, `GREATEST`, `LEAST` and `FIND_IN_SET`. `STRCMP`
with a number argument (a number literal, or a value that is an integer or a
DOUBLE when it is compared) compares the texts byte by byte, whatever the
collation, as MySQL does when either argument is not a string
(`STRCMP('10:04:58', 103458)` is 1). A `DECIMAL` column or a `DECIMAL`
result of an expression still compares under the default collation. `LOCATE`,
`INSTR`, `FIELD`, `REPLACE`, `INSERT`, `LPAD`, `RPAD`, `SUBSTRING_INDEX` and `TRIM`
convert a binary argument used with a text string to `utf8mb4`, failing on
invalid UTF-8 as MySQL does. The string or number that decides is the subject
(`REPLACE`, `INSERT`, `LPAD`, `RPAD`, `SUBSTRING_INDEX`), the string searched
(`LOCATE`, `INSTR`, `POSITION`) or the string looked for (`FIELD`), and the
conversion happens when MySQL evaluates the argument: `REPLACE` and `FIELD`
stop at the first `NULL` or match, the others evaluate every argument first.
`ROUND` of a `BIGINT` to tens fails when the result leaves the `BIGINT` range.
Results above the signed BIGINT range are returned as text, and functions
whose result would exceed `max_allowed_packet` (64 MiB) return `NULL` instead
of allocating it. Regular expressions run on `fancy-regex` with MySQL's ICU
behavior translated where it differs: case-insensitive by default, the `c`,
`i`, `m`, `n` and `u` match types, `.` and `$` with ICU line terminators,
`\Q..\E`, `$n` replacements, and errors for empty patterns, invalid flags,
out-of-range positions, and `{` or `}` outside a `{m}`, `{m,}` or `{m,n}`
interval that follows something to repeat (ICU's errors 3688 and 3692).

Spatial functions work on geometries in MySQL's storage format, in the
Cartesian plane (SRID 0) or WGS 84 (SRID 4326), whose coordinates are read
and written latitude first and stored longitude first, as MySQL does; a
geometry in another spatial reference system is refused, since Turso does
not have MySQL's table of them. Lowered: the constructors `POINT`,
`LINESTRING`, `POLYGON`, `MULTIPOINT`, `MULTILINESTRING`, `MULTIPOLYGON` and
`GEOMETRYCOLLECTION`/`GEOMCOLLECTION`; `ST_GeomFromText` and `ST_GeomFromWKB`
with their per-kind forms (`ST_PointFromText`, `ST_MPolyFromWKB`, ...) and an
optional SRID; `ST_AsText`/`ST_AsWKT` and `ST_AsBinary`/`ST_AsWKB`; `ST_SRID`
(also setting it), `ST_X` and `ST_Y` of a Cartesian point (also setting
them), `ST_Dimension`, `ST_GeometryType`, `ST_IsEmpty`, `ST_NumPoints`,
`ST_PointN`, `ST_StartPoint`, `ST_EndPoint`, `ST_NumGeometries`,
`ST_GeometryN`, `ST_ExteriorRing`, `ST_NumInteriorRings`, `ST_InteriorRingN`,
`ST_Envelope` and `ST_AsGeoJSON` (one argument) of a Cartesian geometry, and
`ST_GeoHash`, `ST_LatFromGeoHash`, `ST_LongFromGeoHash` and
`ST_PointFromGeoHash` (SRID 0), which round the decoded coordinates as MySQL
does; a point at the center of a cell before the last character, where
MySQL may stop early, and a hash with upper-case or other characters are
refused. Text MySQL cannot read, and a
geometry argument that is not one, fail with MySQL's error 3037; a WGS 84
coordinate out of range fails with 3616 or 3617. Refused: the options
argument of these functions, constructors with geometries of different
SRIDs, a `LINESTRING` of one point or a `POLYGON` ring that is not closed or
has fewer than four points (where MySQL's checks are not modeled), and
functions that compute over the shapes (`ST_Contains`, `ST_Distance`,
`ST_Area`, `ST_Centroid`, ...), which are not implemented.

Not supported yet: exact DECIMAL arguments and results (a decimal literal
reaches the functions as a DOUBLE, so `TRUNCATE(1.0, 1)` prints `1` instead
of `1.0` and halves round to even), unsigned BIGINT literals above the signed
range, other collations and character sets, warnings, ICU-only regular expression
syntax (`\uhhhh`, `\N{..}`, Python-style `(?P<name>)` is rejected).
`RANDOM_BYTES(n)` is `__mysql_random_bytes(n, randomblob(1024))`, whose first
`n` bytes are the result, so every row draws new ones; `n` is read as MySQL
reads an integer, and outside 1 to 1024 fails with MySQL's error 1690.
`INSERT(str, pos, len, newstr)` only runs once the statement
is lowered, because `INSERT` is a keyword to the SQLite parser.
