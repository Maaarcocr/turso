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

`CREATE [TEMPORARY] TABLE`, `CREATE TABLE ... [AS] SELECT`, `CREATE [UNIQUE] INDEX`, `DROP INDEX ... ON`, `DROP TABLE [IF EXISTS]` (one table), `RENAME TABLE` (one table), and single-operation `ALTER TABLE` (`ADD [COLUMN]` without `FIRST`/`AFTER`, `DROP [COLUMN]`, `RENAME COLUMN`, `RENAME [TO|AS]`, `DROP {INDEX|KEY}`) are lowered to Turso DDL. Other DDL statements are refused with an unsupported-syntax error.

Typed tables are STRICT tables that core marks with `/* turso_dialect:mysql */` in `sqlite_schema` and keeps marked through `ALTER TABLE`, reopen and `RENAME TABLE`. Each column has a MySQL type from the frontend's type table (`mysql/frontend/types/`), which maps it onto a core storage type and says how a value is converted when it is stored (strict mode: a value MySQL rejects makes the statement fail, see `mysql_error` for the MySQL error codes):

| MySQL type | Stored as | Converting a stored value |
|---|---|---|
| `TINYINT`/`SMALLINT`/`MEDIUMINT`/`INT`/`BIGINT` (`SIGNED` or `UNSIGNED`), `BOOL` (`TINYINT(1)`) | integer within the type's range | text is read as a number (`'12'`, `' 7 '`, `'1e1'`; only spaces and tabs may come before it, and an exponent without digits ends it, so `'1e'` is 1); decimals and text round half away from zero (`2.5` → 3, `'-3.5'` → -4); text with other characters fails (1265), text without digits fails (1366), values outside the range fail (1264); display widths are kept in the declared type |
| `BIGINT UNSIGNED` | unsigned 64-bit integer (exact up to 18446744073709551615) | as above |
| `FLOAT`/`DOUBLE` [`UNSIGNED`] [`(M,D)`] | double | text is read as a number; other text fails (1265); out of range or negative `UNSIGNED` fails (1264); `(M,D)` rounds to `D` decimals as MySQL does and fails above `M` digits |
| `CHAR(n)`/`VARCHAR(n)` | text | at most `n` characters; extra trailing spaces are cut, other extra characters fail (1406); `CHAR` drops trailing spaces; numbers become MySQL's text (`1e20`); bytes that are not UTF-8 fail (1366) |
| `TINYTEXT`/`TEXT`/`MEDIUMTEXT`/`LONGTEXT` | text | as `VARCHAR`, with the type's limit in bytes |
| `BINARY(n)`/`VARBINARY(n)`, `TINYBLOB`/`BLOB`/`MEDIUMBLOB`/`LONGBLOB` | blob | at most the type's length in bytes, else 1406; `BINARY(n)` pads with zero bytes; text and numbers are stored as their bytes. Binary columns compare with text literals byte by byte |
| `GEOMETRY`, `POINT`, `LINESTRING`, `POLYGON`, `MULTI*`, `GEOMETRYCOLLECTION` | blob | only `NULL`: every other value fails (1416) |
| `DECIMAL(p,s)` [`UNSIGNED`] | integer when the value is a whole number in the signed 64-bit range, otherwise double (until exact decimals) | text is read as a number; other text fails with 1265 where MySQL reports 1366; negative `UNSIGNED` fails (1264) |
| `DATE`, `DATETIME(p)`, `TIMESTAMP(p)`, `TIME(p)` | text, as given (until temporal storage) | numbers become text |
| `YEAR` | integer from 0 to 2155 | as the integer types |
| `BIT(n)` | integer from 0 to 2^n - 1 (at most 9223372036854775807 for `BIT(64)`) | as the integer types; values outside the range fail (1406). MySQL reads text as a binary string, Turso as a number |
| `JSON` | `LONGTEXT` with `CHECK (json_valid(...))` (until a JSON type) | as `LONGTEXT` |
| `ENUM(...)` | text, `CHECK` against the members | numbers become text |
| `SET(...)` (at most 6 members) | text, `CHECK` against the member combinations in definition order | numbers become text |

The conversion also applies to `UPDATE`, `INSERT ... SELECT`, defaults, generated columns and rows copied by `ALTER TABLE`. `CAST` to these type names keeps SQLite's conversion for now.

A single integer primary key column is the table's rowid, so rows are read in primary key order, as InnoDB returns them. The exception is a `BIGINT UNSIGNED` key without `AUTO_INCREMENT`: it is an ordinary primary key, so it can hold values above 9223372036854775807, and rows are read in insertion order. `AUTO_INCREMENT` is available on that column: `NULL`, `0` or an omitted value takes the next value, a deleted maximum is never reused, and when the next value does not fit the type the insert fails (MySQL reports a duplicate key, 1062). `UPDATE` stores `0` and rejects `NULL` (1048). A primary key without `AUTO_INCREMENT` rejects `NULL` like MySQL. Primary key columns are `NOT NULL`. A `NOT NULL` `ENUM` without a default takes its first member, as in MySQL.

`DEFAULT` literals are checked against the column type the way MySQL checks them and stored in the form MySQL stores them (integers rounded, `DECIMAL(p,s)` and `FLOAT(M,D)` rounded to their scale, numbers in string columns kept as written, dates and times normalized to `YYYY-MM-DD HH:MM:SS[.fraction]`, `ENUM`/`SET` members spelled as declared, trailing spaces removed for `CHAR`). `CURRENT_TIMESTAMP`/`NOW()` defaults are available for `DATETIME` and `TIMESTAMP` without fractional seconds, and parenthesized expression defaults are lowered as expressions. A default MySQL rejects, or one that needs rounding of fractional seconds, is not lowered.

`CREATE TABLE` keys are validated like InnoDB does: a `BLOB`/`TEXT` key part needs a prefix length, `JSON` columns cannot be key parts, a prefix must fit the column and be on a string, a key is at most 3072 bytes, key names are distinct, and key columns must exist.

DDL that MySQL itself rejects fails with MySQL's error rather than an unsupported-syntax error: invalid defaults, invalid keys, a second primary key, an `AUTO_INCREMENT` column outside every key, `NULL` primary key columns, out-of-range type parameters (`VARCHAR(16384)`, `CHAR(256)`, `DECIMAL(5,6)`, `DATETIME(7)`, `BIT(65)`), duplicate `ENUM` members, a collation on a binary string, an index named `PRIMARY`, and identifiers longer than 64 characters. Every table gets its column types from the MySQL lowering. Other DDL MySQL rejects is not detected yet when its SQLite reading is valid, for example foreign keys whose parent table or column types do not match.

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
- Decimal literals reach a column as doubles until exact decimals exist, so `1.50` is stored in a `VARCHAR` as `1.5`, a double literal such as `2.5e0` or a `DOUBLE` column value rounds half away from zero into an integer column (MySQL rounds it to even), a negative double such as `-0.4e0` fails in an unsigned integer column (MySQL stores 0, and fails only for the decimal `-0.4`), and `DECIMAL(p,s)` values are neither rounded to the scale nor checked against the precision and are shown without trailing zeros. `FLOAT` is double precision; `FLOAT(M,D)`/`DOUBLE(M,D)` values are shown without trailing zeros.
- A double stored in a `CHAR`/`VARCHAR` whose shortest MySQL text is longer than the column fails (1406), where MySQL rounds it to fit.
- A binary string or hexadecimal literal stored in an integer, `BIT`, `YEAR`, `FLOAT`/`DOUBLE` or `DECIMAL` column fails (1366), because a hexadecimal literal is not yet told apart from a binary string; MySQL stores `X'3132'` as 12594 and a binary string `'12'` as 12.
- An integer literal above 9223372036854775807 is read as a double. Storing one in a `BIGINT UNSIGNED` column fails (1366) instead of storing a rounded value; written as text (`'18446744073709551615'`) the value is stored exactly. In comparisons such a literal stays a double, so `u = 18446744073709551615` compares as doubles and also matches nearby values that round to the same double, where MySQL compares exactly. A `BIGINT UNSIGNED AUTO_INCREMENT` primary key, being the rowid, holds at most 9223372036854775807; without `AUTO_INCREMENT` it is an ordinary primary key over the whole range.
- `<`, `<=`, `>` and `>=` between a `BIGINT UNSIGNED` column and text that is not a whole number (`u < '9.5'`) fail; `=` compares them as doubles, as MySQL does.
- Temporal values are stored as given, without MySQL's validation, normalization, or `TIMESTAMP` time-zone conversion (only defaults are normalized).
- `ENUM` sorts and compares by text, not by member position; numeric `ENUM` and `SET` values (member indexes and bitmasks) and member names in another letter case are rejected instead of converted; `SET` values must list members in definition order.
- Unlike InnoDB, a failed insert does not use up an `AUTO_INCREMENT` value.
- `FOREIGN KEY` constraints are stored but Turso does not enforce them unless `PRAGMA foreign_keys` is on.
- `CREATE TABLE ... AS SELECT` gives a result column the type of the column it reads (also through `MAX`/`MIN`), `BIGINT` to integer literals, `COUNT(...)` and integer arithmetic, `DOUBLE` to double literals and double arithmetic, and `LONGTEXT` to everything else (string literals, `CONCAT(...)`, `SUM(...)`, `AVG(...)`, arithmetic on `DECIMAL`), where MySQL picks a `VARCHAR(n)`, `INT` or `DECIMAL(p,s)` from the expression.
- `CREATE TABLE ... AS SELECT` copies only the column type. `NOT NULL` and `DEFAULT` of the columns it reads are not copied, and neither are the `CHECK`s that hold the members of `ENUM` and `SET` columns and the validity of `JSON` columns, so the new table accepts any text there. MySQL keeps all of these.
- Not lowered: `CREATE TABLE ... LIKE`, `CREATE TABLE` with both columns and a `SELECT`, qualified `db.table` names, `ZEROFILL`, spatial `SRID` and `SPATIAL` keys, `FULLTEXT` keys, functional key parts, unique keys on column prefixes, `ON UPDATE CURRENT_TIMESTAMP`, `INVISIBLE`, `AUTO_INCREMENT=n`, `AUTO_INCREMENT` on a column that is not the whole primary key, partitioning, `ALTER TABLE` with several operations or `FIRST`/`AFTER`, `MODIFY`/`CHANGE`/`ALTER COLUMN`, adding or dropping keys and constraints with `ALTER TABLE`, `TRUNCATE` (it also resets `AUTO_INCREMENT` in MySQL), and `CREATE INDEX` with prefix lengths.

## SELECT statements and expressions

`SELECT` statements are lowered by `mysql/parser/query.rs` (statement
structure), `expr.rs` (expressions) and `literal.rs` (literals) into the shared
Turso AST. Anything listed under "Refused" fails with an unsupported-syntax
error.

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
| `FROM` tables (`db.t` as a schema), aliases, comma joins, `[INNER] JOIN`, `CROSS JOIN`, `LEFT/RIGHT [OUTER] JOIN`, `NATURAL`, `ON`, `USING`, derived tables, `LATERAL`, parenthesized joins, `FROM DUAL` | Same clauses (`FROM DUAL` is no `FROM`) |
| `WHERE`, `GROUP BY` (expressions, aliases, ordinals), `HAVING`, `WINDOW`, `ORDER BY [ASC \| DESC]`, `LIMIT n`, `LIMIT n OFFSET m`, `LIMIT m, n` | Same clauses |
| `UNION [ALL \| DISTINCT]`, `INTERSECT`, `EXCEPT`, parenthesized members with their own `ORDER BY`/`LIMIT`, `WITH` with column lists, `VALUES ROW(...)` | Same; a member that has its own `ORDER BY`/`LIMIT`, or a right operand that is itself a set operation (`INTERSECT` binds tighter), becomes `SELECT * FROM (...)` |
| Window functions with `PARTITION BY`, `ORDER BY`, `ROWS`/`RANGE` frames and named windows | Same |
| Column references, `?` parameters (numbered in text order), `CASE`, `IN` lists and subqueries, `BETWEEN`, `EXISTS`, scalar subqueries, row values, `IS [NOT] NULL/TRUE/FALSE/UNKNOWN`, `= ANY (subquery)`, `<> ALL (subquery)` | Same expressions; `IS UNKNOWN` is `IS NULL`; `= ANY` is `IN`, `<> ALL` is `NOT IN` |
| `a DIV b` | `CAST(a / b AS INTEGER)`: truncates toward zero, `NULL` for a zero divisor |
| `MOD`, `%`, `XOR`, `&&`, `\|\|`, `!`, `^`, `<=>` | `%`; `(NOT a) <> (NOT b)`; `AND`; `OR`; `NOT`; `(a \| b) - (a & b)` (only when neither side calls a function or has a subquery, since both sides appear twice); `IS` |
| `a LIKE p [ESCAPE c]`, `REGEXP`, `RLIKE` | `LIKE` with `ESCAPE '\'` unless the pattern is a string without backslashes (MySQL's default escape character); a dangling escape at the end of a literal pattern matches itself, as in MySQL; `REGEXP` operator, whose operands may be `CAST(x AS JSON)` (MySQL matches the JSON text) |
| `COLLATION(x)` of a literal, an introduced string (`_utf8mb4`, `_utf8mb3`, `_utf8`, `_latin1`, `_ascii`, `_binary`), `'..' COLLATE c` with a `utf8mb4` collation, `RANDOM_BYTES(literal)` or `UUID()` | The collation name MySQL derives from the type (`'utf8mb4_0900_ai_ci'`, `'binary'` for numbers, `NULL` and binary strings), without evaluating `x`, as in MySQL; other arguments fall back |
| Function calls | Passed through by name, so SQLite built-ins and the MySQL functions in `mysql/frontend/functions` both resolve; `COUNT(*)`, `DISTINCT` arguments and `OVER` are kept. Arguments of the numeric functions (`ABS`, `ACOS`, `ASIN`, `ATAN`, `ATAN2`, `CEIL`, `CEILING`, `CHAR`, `COS`, `COT`, `DEGREES`, `EXP`, `FLOOR`, `LN`, `LOG`, `LOG10`, `LOG2`, `MOD`, `POW`, `POWER`, `RADIANS`, `ROUND`, `SIGN`, `SIN`, `SQRT`, `TAN`, `TRUNCATE`) and the condition of `IF` are a numeric context for literals; the separator of `CONCAT_WS` is text |
| `CAST(x AS JSON)` used as a value (a selected column, a `CASE`/`IF` result, a `JSON_*` argument, or an argument of another function whose other arguments are non-NULL literals), a boolean expression (`TRUE`, `a = b`, `NOT a`, `IN`, `LIKE`, ...) used as a value of `JSON_ARRAY`, `JSON_OBJECT`, `JSON_SET`, `JSON_INSERT`, `JSON_REPLACE`, `JSON_ARRAY_APPEND`, `JSON_ARRAY_INSERT` or `CAST(... AS JSON)`, and `x MEMBER OF (y)` | `__mysql_cast_as_json(x)`, `__mysql_cast_boolean_as_json(x)`, `__mysql_member_of(x, y)` (see JSON functions) |
| A select item, in a `SELECT` with no table (or `FROM DUAL`), `WHERE`, `GROUP BY`, `HAVING`, `WINDOW`, `ORDER BY` or `LIMIT`, built only from literals, operators, casts and function calls, in which MySQL always evaluates `CAST('text' AS JSON)` of a text its JSON parser rejects | `__mysql_cast_as_json('text')`, which fails with MySQL's error 3141, since the item fails whatever surrounds the cast (`SELECT -1.5 - CAST('[1,' AS JSON)`). MySQL evaluates the left operand of an operator first and the right one only when the left one is a value that cannot be NULL (for `AND` a true number, for `OR` a false number, or `NULL`); the subject of `IS`, `IN` and `BETWEEN`; `LIKE` before its pattern and `REGEXP` after its pattern; the operand of a cast; and every argument of a function when the other arguments cannot be NULL, of `CHAR` and `INTERVAL` always, of `CONCAT_WS` when the separator cannot be NULL, and never of `IF`, `IFNULL`, `COALESCE`, `NULLIF`, `ELT`, `FIELD` or `MAKE_SET` (`0 AND CAST('x' AS JSON)` is 0 and `CONCAT(NULL, CAST('x' AS JSON))` is `NULL`, which keep falling back) |
| A JSON value (`CAST(x AS JSON)`, `x->'path'`, or a call of a `JSON_*` function that returns JSON) compared with `=`, `<>`, `<`, `<=`, `>`, `>=` or `<=>` to another JSON value or to a literal, a temporal literal, a boolean expression or arithmetic on number literals | `__mysql_json_compare(a, b) OP 0`, `__mysql_json_null_safe_equal(a, b)`: MySQL's JSON comparison. Numbers compare exactly across integers, doubles and decimals; strings by their bytes; arrays element by element and then by length; objects by member count, then by keys and values; `false` before `true`; values of different JSON types by MySQL's type order (null, numbers, strings, objects, arrays, booleans, dates, times, datetimes, opaque values). The other operand becomes a JSON value: a string a JSON string, a number a JSON number, a boolean expression a JSON boolean, a binary string an opaque value, and a `DATE`/`TIME`/`TIMESTAMP` literal a JSON date, time or datetime |
| A JSON value used as a condition (`WHERE`, `NOT`, `AND`, `OR`, `XOR`, the condition of `IF`), in arithmetic (`+ - * / % MOD`, unary `-`), or as an operand of `LIKE` | `__mysql_json_truth(x)` (MySQL's `x <> 0` under the JSON rules); `__mysql_json_double(x)`, MySQL's DOUBLE of the JSON value (a string's leading number, `true` 1, other non-numbers 0), and `__mysql_json_remainder(a, b)`, the DOUBLE remainder (`1.5e0 % CAST('1.1' AS JSON)` is `0.3999999999999999`, `NULL` for a zero divisor); `__mysql_like_bin(s, p, escape)`, which matches the JSON text with `utf8mb4_bin` (case-sensitive), as MySQL does. `REGEXP`, `REGEXP_LIKE` and `STRCMP` with a JSON operand also compare case-sensitively, and a JSON operand next to a binary string is not an error |
| `CAST(DATE '..' AS JSON)`, `CAST(TIME '..' AS JSON)` (from 00:00:00 to 23:59:59) and `CAST(TIMESTAMP '..' AS JSON)` as an argument of `CONCAT`, `CONCAT_WS`, `STRCMP`, `SHA`, `SHA1`, `SHA2` (the string), `MD5`, `CRC32`, `TO_BASE64`, `FROM_BASE64`, `HEX` or `UNHEX`, or as an operand of `LIKE`, which read only its text | `__mysql_cast_as_json('"2000-01-01"')`: the JSON text MySQL prints for the date, time or datetime, with six fractional digits for times and datetimes (`"12:34:56.000000"`) |
| `GET_FORMAT(DATE \| TIME \| DATETIME \| TIMESTAMP, f)` | `GET_FORMAT('DATE', f)` |
| `GROUP_CONCAT(a, b, ... [ORDER BY ...] [SEPARATOR s])` | `group_concat(a \|\| b \|\| ..., s)` (Turso does not support `ORDER BY` inside aggregates yet and reports an error) |
| `DATE_ADD/ADDDATE(d, INTERVAL v unit)`, `d + INTERVAL v unit`, `INTERVAL v unit + d`; `DATE_SUB/SUBDATE(...)`, `d - INTERVAL v unit`; `EXTRACT(unit FROM d)`; `TIMESTAMPADD/TIMESTAMPDIFF(unit, ...)` | `DATE_ADD(d, v, 'UNIT')`, `DATE_SUB(d, v, 'UNIT')`, `EXTRACT('UNIT', d)`, `TIMESTAMPADD('UNIT', ...)`, `TIMESTAMPDIFF('UNIT', ...)` with the unit as upper-case text (the datetime functions implement these forms) |
| `x->'path'`, `x->>'path'` | `JSON_EXTRACT(x, path)`, `JSON_UNQUOTE(JSON_EXTRACT(x, path))` |
| `CURRENT_DATE`, `CURRENT_TIME`, `CURRENT_TIMESTAMP`, `UTC_DATE`, `UTC_TIME`, `UTC_TIMESTAMP`, `LOCALTIME`, `LOCALTIMESTAMP` (with or without `()`) | SQLite's current-time literals, which are UTC like the target time zone |
| `POSITION(a IN b)`, `SUBSTRING(s FROM a FOR b)` | `LOCATE(a, b)`, `SUBSTRING(s, a, b)` |
| `TRIM([[LEADING \| TRAILING \| BOTH] [r] FROM] s)`, `LTRIM(s)`, `RTRIM(s)` | `__mysql_trim(s, r)`, `__mysql_ltrim(s, r)`, `__mysql_rtrim(s, r)` with `r` a space when it is left out (see string functions); `TRIM(LEADING r)` without `FROM` is a syntax error, as in MySQL |
| `CAST`/`CONVERT(x, type)` to `SIGNED [INTEGER]`, `UNSIGNED [INTEGER]`, `CHAR`, `DOUBLE`, `REAL`, `FLOAT`, `DECIMAL[(m[,d])]` | The SQLite type name of the same affinity: no rounding to an integer or to `d` decimals, and no unsigned wrap-around. A `DECIMAL(m,d)` or `DEC(m,d)` target MySQL rejects (`m < d`, `m > 65`, `d > 30`) fails the whole statement when it is parsed, as in MySQL, also when the rest of the statement is refused or the cast is never evaluated; `DECIMAL(0)` and `DECIMAL(0,0)` are accepted, as MySQL reads them as `DECIMAL(10,0)` |
| `CAST`/`CONVERT(x, type)` to `NCHAR` | `CAST(x AS NCHAR)`, which SQLite reads as text like `CHAR` |
| `CAST`/`CONVERT(x, type)` to `CHAR(n)` and `NCHAR(n)` (`n` up to 16,777,216) | `__mysql_cast_as_char(x, n)`, `__mysql_cast_as_nchar(x, n)`: the MySQL text of `x` (numbers print as MySQL prints them, a binary string that is not UTF-8 is `NULL`) cut to `n` characters; `NCHAR` is `utf8mb3`, so a 4-byte character becomes `?` |
| `CAST`/`CONVERT(x, type)` to `DATE`, `DATETIME[(p)]` and `TIME[(p)]` used as a value (a selected column, an argument of a function other than a JSON one, the operand of a `CHAR` cast) | `__mysql_cast_as_date(x)`, `__mysql_cast_as_datetime(x, p)`, `__mysql_cast_as_time(x, p)`: MySQL's conversion of `DATE(x)`, `TIMESTAMP(x)` and `TIME(x)`, rounded to `p` fractional digits (0 by default), so `CAST('1.5' AS TIME)` is `00:00:02`. `p` above 6 fails the statement when it is parsed (error 1426). The operand must have no DATE or TIME type MySQL converts differently from its text: literals, operators other than `INTERVAL` arithmetic, `TIMESTAMP '..'` literals, `JSON_*` calls and JSON casts of those are lowered, as are `DATE '..'` to `DATE`/`DATETIME` and `TIME '..'` to `TIME` |
| `CONVERT(x USING utf8mb4/utf8mb3/utf8)`, `CONVERT(x USING binary)` | `CAST(x AS TEXT)`, `CAST(x AS BLOB)` |
| `COLLATE binary`, `COLLATE utf8mb4_0900_bin` | `COLLATE BINARY` |
| Integer, decimal and scientific literals, `TRUE`, `FALSE`, `NULL` | The same literal text the SQLite parser keeps (decimals and integers above the signed 64-bit range are still SQLite numbers) |
| `'...'`, `"..."`, `N'...'` | Text (`"..."` is a string, never an identifier) |
| `X'..'`, `0x..`, `b'..'`, `0b..` | A blob; in a comparison with a number, a logical operator, a condition, a numeric function argument, a numeric `CAST`, or `\|`, `&`, `>>`, `^`, the unsigned integer value; in a comparison with a string, text. Function arguments MySQL reads as numbers are also the value: the index of `ELT`, the bits of `MAKE_SET` and `EXPORT_SET`, the counts and positions of `LEFT`, `RIGHT`, `SUBSTRING`/`SUBSTR`/`MID`, `INSERT`, `LOCATE`, `SUBSTRING_INDEX`, `LPAD`, `RPAD`, `REPEAT` and `SPACE`, the length of `SHA2`, the bases of `CONV`, every argument of `INTERVAL`, and every argument of `FIELD` when another argument is a number (`FIELD(0x61, 97)` is 1, `FIELD(0x61, 'a')` compares bytes). `CONV`, `BIN` and `OCT` of a hex or bit literal convert its value whatever the base: `__mysql_conv_binary_literal(bytes, from, to)` |
| `_utf8mb4'..'`, `_utf8mb3`, `_utf8`, `_binary`, and `_ascii`/`_latin1` with ASCII text, with a string or hex value | Text; `_binary` is a blob |
| `DATE '..'`, `TIME '..'`, `TIMESTAMP '..'`, `{d '..'}` | Canonical text (`'2018-05-01'`, `'26:00:00'`, `'2020-01-02 03:04:05.5'`); a string literal compared with one is converted the same way (`DATE '2018-05-01' = '2018-5-1'`); in a logical operator, a condition or a numeric function argument, the number MySQL uses (`20180501`) |
| `/*+ ... */` optimizer hints | Ignored (they only choose a plan), except `SET_VAR` and `MAX_EXECUTION_TIME`, which are refused |

Refused, because Turso cannot run it with MySQL's meaning yet:

- `WITH RECURSIVE`: MySQL stops after `cte_max_recursion_depth` (1000)
  iterations; Turso does not.
- `BINARY x` and `CAST(x AS BINARY)`: sqlparser cannot tell them apart, and
  Turso does not enforce `ONLY_FULL_GROUP_BY` for them.
- `ROUND`/`TRUNCATE` with a negative literal precision: Turso's `round` does
  not round to tens.
- `REGEXP` with an empty pattern or over binary strings, row values inside row
  values, `VALUES` rows of different lengths: MySQL raises an error.
- Hex, bit and temporal literals in arithmetic (`+ - * / DIV % MOD`, unary
  `-`/`+`/`~`, `<<`): MySQL computes hex and bit literals as BIGINT UNSIGNED,
  which fails on negative results, and temporal arithmetic can overflow.
  `DIV` with a literal outside the BIGINT range, bit operations on fractional
  or out-of-range number literals, and `_binary` strings in numeric contexts.
- Binary strings that are not NUL-free UTF-8 outside numeric contexts.
- A JSON value in a comparison, arithmetic or a condition, except the
  failing select items above and the JSON comparisons, conditions and
  arithmetic above, when it may fail (a cast of a column, a function result or
  an integer literal above the signed BIGINT range, or a `JSON_*` call other
  than `JSON_ARRAY` or `JSON_OBJECT` of literals): MySQL may never evaluate it.
  Also a JSON value compared with a column or a function result, or used with
  `IN`, `BETWEEN`, `DIV` or a bit operator: MySQL converts the other operand
  by its type.
- `CAST(x AS JSON)` as an argument of a function other than `JSON_*` when the
  function is `CONCAT_WS`, `ELT` or `MAKE_SET`, or when another argument is
  not a non-NULL literal, unless the cast cannot fail and no argument is a
  `_binary` string: MySQL stops evaluating arguments at the first NULL
  (`MAKEDATE(CAST('x' AS JSON), NULL)` is NULL) or never evaluates unused
  ones, while Turso evaluates every argument and reports the invalid JSON.
  `FIELD`, `GREATEST`, `LEAST`, `LOCATE`, `INSTR`, `POSITION` and
  `FIND_IN_SET` with a JSON argument are refused too: MySQL compares with
  `utf8mb4_bin` there.
- Temporal literals compared with a number, or with a string that is not a
  valid value of their type; `DATETIME '..'`; invalid temporal literals; odd
  `X'...'` digits; character set introducers other than the ones above.
- Expressions nested deeper than 100 levels (the SQLite parser's limit).
- A grouped `SELECT` (with `GROUP BY`, or with an aggregate and no `GROUP BY`)
  whose select list or `HAVING` may use a column that is neither grouped nor
  inside an aggregate, including `*`: MySQL's default `ONLY_FULL_GROUP_BY`
  rejects it (error 1055 or 1140) unless the column depends on a key, and
  Turso does not check it.
- `SQL_*`, `HIGH_PRIORITY` and `STRAIGHT_JOIN` modifiers, `STRAIGHT_JOIN`
  joins, index hints, `PARTITION (...)` selection, `INTO`, locking reads
  (`FOR UPDATE`, `FOR SHARE`), `WITH ROLLUP`, `TABLE t`, `VALUES` without
  `ROW`, derived-table column lists `AS t(a, b)`, `EXCEPT ALL`, `INTERSECT
  ALL`, `FULL JOIN`, `NULLS FIRST/LAST`.
- User and system variables (`@a`, `@@x`), `:name` and `?N` parameters,
  `MATCH ... AGAINST`, `SOUNDS LIKE`, `JSON_TABLE`, `COLLATE` with any other
  collation, `CAST` to `YEAR`, `BINARY` or with a character set,
  `CONVERT(... USING ...)` with any other character set, and `^` over
  function calls or subqueries.
- `CAST` to `DATE`, `DATETIME` or `TIME` in a comparison, arithmetic, a
  condition, a numeric function argument or a JSON cast, where MySQL uses the
  temporal value or its number (`CAST('2000-01-01' AS DATE) + 0` is
  20000101), and of an operand that may have a DATE or TIME type (a column, a
  function result, `TIME '..'` to a date, `DATE '..'` to `TIME`): MySQL
  converts those from the type, not from the text (`CAST(TIME '10:00:00' AS
  DATETIME)` is today at 10:00).
- `CAST(x AS JSON)` of a `DATE`, `TIME` or `TIMESTAMP` value outside the
  comparisons and text-reading functions above, and of a `TIME` outside
  00:00:00 to 23:59:59 there: MySQL makes a JSON date or time, which reads
  as 0 in numbers and as NULL in a function of the other temporal kind, while
  a JSON value in Turso is its text, and MySQL prints a JSON time of 24 hours
  or more with fewer hours.

Known differences that remain on the lowered path: `/`
is SQLite division (integer division for two integers) rather than MySQL's
DECIMAL division; integer overflow becomes a REAL instead of an error; bit
operators are signed 64-bit rather than BIGINT UNSIGNED; string comparisons and
`LIKE` use binary or ASCII case folding rather than `utf8mb4_0900_ai_ci`;
`WHERE` may refer to select-list aliases; `ORDER BY` with `DISTINCT` may use
columns outside the select list; unaliased columns are named after the expression as sqlparser
prints it (string literals after their value); and the result of a `DATE`,
`DATETIME` or `TIME` cast is text, so an aggregate such as `SUM` reads its
leading number rather than MySQL's 20000101, and a decimal literal reaches the
cast as a DOUBLE (`CAST(20000101103458.1234 AS DATETIME(6))` ends in
`.125000`).

## `INSERT`

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

Refused: `INSERT IGNORE`, `REPLACE`, `DEFAULT` as a value, a schema-qualified
table, `ON DUPLICATE KEY UPDATE` after a `SELECT` (MySQL lets the update read
the selected columns), the `AS alias` row alias, an `INSERT ... SET` or
update that assigns a column twice, an `INSERT ... SET` value that reads a
column, and an update assignment that reads a column an earlier assignment
wrote (MySQL assigns left to right, so `a = a + 1, b = a` sets `b` to the new
`a`, while the upsert reads the old row).

Known differences: the affected-row count is SQLite's (1 per inserted or
updated row) rather than MySQL's 2 for an update and 0 for an unchanged
row; an update consumes no `AUTO_INCREMENT` value; and Turso checks foreign
keys only after `SET FOREIGN_KEY_CHECKS = 1`.

## Session settings (`SET`)

`mysql/parser/session.rs` lowers `SET` statements (session scope, with or
without `SESSION`, `LOCAL`, `@@`, `@@session.` or `@@local.`; several
assignments in one statement are allowed):

| Statement | Lowering |
|---|---|
| `SET NAMES utf8mb4 [COLLATE utf8mb4_0900_ai_ci]`, `time_zone` set to `'+00:00'` or `'UTC'`, `autocommit` set to 1 or `ON`, `sql_mode` set to exactly the default modes of the target profile (in any order), `character_set_client`, `character_set_connection` or `character_set_results` set to `utf8mb4`, `collation_connection` set to `utf8mb4_0900_ai_ci` | A compatibility no-op (`SELECT 0 WHERE 0`, which returns no rows): the session already has these values in the target profile |
| `FOREIGN_KEY_CHECKS` set to 0, 1, `ON`, `OFF`, `TRUE` or `FALSE` | `PRAGMA foreign_keys = 0/1`, which Turso applies at once, also inside a transaction, as MySQL does. Turso then checks foreign keys like SQLite: an `UPDATE` that writes the same key value is checked too, where InnoDB skips it |

Everything else is refused: user variables
(`SET @a = 1`, Turso has no session variables yet), other values of these
variables (`sql_mode`, `time_zone`, `autocommit = 0` and character sets
change the semantics of later statements), `GLOBAL` scope, and other
variables. Turso does not check foreign keys until `FOREIGN_KEY_CHECKS` is
set to 1, while MySQL checks them by default.

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
- Comparisons, conditions and arithmetic of JSON values use JSON rules only
  where the lowering above sees the JSON value (a cast, `->` or a `JSON_*`
  call). A JSON value from a column, a subquery or `CASE` is text there.
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
| `TIMESTAMPADD(unit, n, d)`, `TIMESTAMPDIFF(unit, a, b)` | `TIMESTAMPADD('UNIT', n, d)`, `TIMESTAMPDIFF('UNIT', a, b)` |
| `GET_FORMAT(DATE, 'USA')` | `GET_FORMAT('DATE', 'USA')` |

Turso has no `DATE`, `TIME`, `DATETIME`, `DECIMAL` or unsigned types, so:

- Temporal results are strings. In a numeric context MySQL turns them into
  numbers (`SEC_TO_TIME(2378) + 0` is `3938`); Turso does not. A
  `DATE '...'`, `TIME '...'` or `TIMESTAMP '...'` literal is a string, so
  `DATE_ADD(TIME '10:00:00', INTERVAL 1 HOUR)` is `NULL` as for a string.
  Interval results use MySQL's string form: 6 fractional digits when there
  is a fraction, none otherwise.
- Decimal literals reach the functions as doubles, so their precision and
  fractional digits are lost: `MAKETIME(10, 34, 58.1234)` gives
  `10:34:58.123400` (MySQL: `10:34:58.1234`) and `MICROSECOND(
  20000101103458.1234)` gives `125000`.
- A `BLOB` argument is read as MySQL reads a hexadecimal literal: as a
  string in date and time positions and as a `BIGINT UNSIGNED` made of its
  bytes in numeric positions (`MAKEDATE(X'31', 1)` is `2049-01-01`). A
  `_binary '..'` string arrives as the same `BLOB`, so in numeric positions
  it is also read as bytes, where MySQL parses its text.
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
  `POW`/`POWER`, `RADIANS`, `ROUND`, `SIGN`, `SIN`, `SQRT`, `TAN`, `TRUNCATE`.
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
  `UUID_TO_BIN`, `BIN_TO_UUID`.
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
byte by byte in `STRCMP`, `GREATEST`, `LEAST` and `FIND_IN_SET`; `LOCATE`,
`INSTR`, `FIELD`, `REPLACE`, `INSERT`, `LPAD`, `RPAD`, `SUBSTRING_INDEX` and `TRIM`
convert a binary argument used with a text string to `utf8mb4`, failing on
invalid UTF-8 as MySQL does.
Results above the signed BIGINT range are returned as text, and functions
whose result would exceed `max_allowed_packet` (64 MiB) return `NULL` instead
of allocating it. Regular expressions run on `fancy-regex` with MySQL's ICU
behavior translated where it differs: case-insensitive by default, the `c`,
`i`, `m`, `n` and `u` match types, `.` and `$` with ICU line terminators,
`\Q..\E`, `$n` replacements, and errors for empty patterns, invalid flags,
out-of-range positions, and `{` or `}` outside a `{m}`, `{m,}` or `{m,n}`
interval that follows something to repeat (ICU's errors 3688 and 3692).

Not supported yet: exact DECIMAL arguments and results (a decimal literal
reaches the functions as a DOUBLE, so `TRUNCATE(1.0, 1)` prints `1` instead
of `1.0` and halves round to even), unsigned BIGINT literals above the signed
range, other collations and character sets, warnings, ICU-only regular expression
syntax (`\uhhhh`, `\N{..}`, Python-style `(?P<name>)` is rejected), and
`RANDOM_BYTES`. `INSERT(str, pos, len, newstr)` only runs once the statement
is lowered, because `INSERT` is a keyword to the SQLite parser.
