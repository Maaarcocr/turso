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

## SELECT statements and expressions

`SELECT` statements are lowered by `mysql/parser/query.rs` (statement
structure), `expr.rs` (expressions) and `literal.rs` (literals) into the shared
Turso AST. Anything listed under "Falls back" is rejected by the lowering, so
the statement is parsed by the SQLite fallback exactly as before.

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
| `a LIKE p [ESCAPE c]`, `REGEXP`, `RLIKE` | `LIKE` with `ESCAPE '\'` unless the pattern is a string without backslashes (MySQL's default escape character); a dangling escape at the end of a literal pattern matches itself, as in MySQL; `REGEXP` operator |
| Function calls | Passed through by name, so SQLite built-ins and the MySQL functions in `mysql/frontend/functions` both resolve; `COUNT(*)`, `DISTINCT` arguments and `OVER` are kept. Arguments of the numeric functions (`ABS`, `ACOS`, `ASIN`, `ATAN`, `ATAN2`, `CEIL`, `CEILING`, `CHAR`, `COS`, `COT`, `DEGREES`, `EXP`, `FLOOR`, `LN`, `LOG`, `LOG10`, `LOG2`, `MOD`, `POW`, `POWER`, `RADIANS`, `ROUND`, `SIGN`, `SIN`, `SQRT`, `TAN`, `TRUNCATE`) and the condition of `IF` are a numeric context for literals; the separator of `CONCAT_WS` is text |
| `CAST(x AS JSON)` used as a value (a selected column, a `CASE`/`IF` result, a `JSON_*` argument, or an argument of another function whose other arguments are non-NULL literals), a boolean expression (`TRUE`, `a = b`, `NOT a`, `IN`, `LIKE`, ...) used as a value of `JSON_ARRAY`, `JSON_OBJECT`, `JSON_SET`, `JSON_INSERT`, `JSON_REPLACE`, `JSON_ARRAY_APPEND`, `JSON_ARRAY_INSERT` or `CAST(... AS JSON)`, and `x MEMBER OF (y)` | `__mysql_cast_as_json(x)`, `__mysql_cast_boolean_as_json(x)`, `__mysql_member_of(x, y)` (see JSON functions) |
| `GET_FORMAT(DATE \| TIME \| DATETIME \| TIMESTAMP, f)` | `GET_FORMAT('DATE', f)` |
| `GROUP_CONCAT(a, b, ... [ORDER BY ...] [SEPARATOR s])` | `group_concat(a \|\| b \|\| ..., s)` (Turso does not support `ORDER BY` inside aggregates yet and reports an error) |
| `DATE_ADD/ADDDATE(d, INTERVAL v unit)`, `d + INTERVAL v unit`, `INTERVAL v unit + d`; `DATE_SUB/SUBDATE(...)`, `d - INTERVAL v unit`; `EXTRACT(unit FROM d)`; `TIMESTAMPADD/TIMESTAMPDIFF(unit, ...)` | `DATE_ADD(d, v, 'UNIT')`, `DATE_SUB(d, v, 'UNIT')`, `EXTRACT('UNIT', d)`, `TIMESTAMPADD('UNIT', ...)`, `TIMESTAMPDIFF('UNIT', ...)` with the unit as upper-case text (the datetime functions implement these forms) |
| `x->'path'`, `x->>'path'` | `JSON_EXTRACT(x, path)`, `JSON_UNQUOTE(JSON_EXTRACT(x, path))` |
| `CURRENT_DATE`, `CURRENT_TIME`, `CURRENT_TIMESTAMP`, `UTC_DATE`, `UTC_TIME`, `UTC_TIMESTAMP`, `LOCALTIME`, `LOCALTIMESTAMP` (with or without `()`) | SQLite's current-time literals, which are UTC like the target time zone |
| `POSITION(a IN b)`, `SUBSTRING(s FROM a FOR b)` | `LOCATE(a, b)`, `SUBSTRING(s, a, b)` |
| `TRIM([[LEADING \| TRAILING \| BOTH] [r] FROM] s)`, `LTRIM(s)`, `RTRIM(s)` | `__mysql_trim(s, r)`, `__mysql_ltrim(s, r)`, `__mysql_rtrim(s, r)` with `r` a space when it is left out (see string functions); `TRIM(LEADING r)` without `FROM` is a syntax error, as in MySQL |
| `CAST`/`CONVERT(x, type)` to `SIGNED [INTEGER]`, `UNSIGNED [INTEGER]`, `CHAR`, `DOUBLE`, `REAL`, `FLOAT`, `DECIMAL[(m[,d])]` | The same SQLite type name the fallback used, so the result is unchanged: no rounding to an integer or to `d` decimals, and no unsigned wrap-around. A `DECIMAL(m,d)` or `DEC(m,d)` target MySQL rejects (`m < d`, `m > 65`, `d > 30`) fails the whole statement when it is parsed, as in MySQL, also when the rest of the statement falls back or is never evaluated; `DECIMAL(0)` and `DECIMAL(0,0)` are accepted, as MySQL reads them as `DECIMAL(10,0)` |
| `CONVERT(x USING utf8mb4/utf8mb3/utf8)`, `CONVERT(x USING binary)` | `CAST(x AS TEXT)`, `CAST(x AS BLOB)` |
| `COLLATE binary`, `COLLATE utf8mb4_0900_bin` | `COLLATE BINARY` |
| Integer, decimal and scientific literals, `TRUE`, `FALSE`, `NULL` | The same literal text the SQLite parser keeps (decimals and integers above the signed 64-bit range are still SQLite numbers) |
| `'...'`, `"..."`, `N'...'` | Text (`"..."` is a string, never an identifier) |
| `X'..'`, `0x..`, `b'..'`, `0b..` | A blob; in a comparison with a number, a logical operator, a condition, a numeric function argument, a numeric `CAST`, or `\|`, `&`, `>>`, `^`, the unsigned integer value; in a comparison with a string, text. Function arguments MySQL reads as numbers are also the value: the index of `ELT`, the bits of `MAKE_SET` and `EXPORT_SET`, the counts and positions of `LEFT`, `RIGHT`, `SUBSTRING`/`SUBSTR`/`MID`, `INSERT`, `LOCATE`, `SUBSTRING_INDEX`, `LPAD`, `RPAD`, `REPEAT` and `SPACE`, the length of `SHA2`, the bases of `CONV`, every argument of `INTERVAL`, and every argument of `FIELD` when another argument is a number (`FIELD(0x61, 97)` is 1, `FIELD(0x61, 'a')` compares bytes). `CONV`, `BIN` and `OCT` of a hex or bit literal convert its value whatever the base: `__mysql_conv_binary_literal(bytes, from, to)` |
| `_utf8mb4'..'`, `_utf8mb3`, `_utf8`, `_binary`, and `_ascii`/`_latin1` with ASCII text, with a string or hex value | Text; `_binary` is a blob |
| `DATE '..'`, `TIME '..'`, `TIMESTAMP '..'`, `{d '..'}` | Canonical text (`'2018-05-01'`, `'26:00:00'`, `'2020-01-02 03:04:05.5'`); a string literal compared with one is converted the same way (`DATE '2018-05-01' = '2018-5-1'`); in a logical operator, a condition or a numeric function argument, the number MySQL uses (`20180501`) |
| `/*+ ... */` optimizer hints | Ignored (they only choose a plan), except `SET_VAR` and `MAX_EXECUTION_TIME`, which fall back |

Falls back to SQLite parsing, because Turso cannot run it with MySQL's meaning
yet:

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
- `CAST(x AS JSON)` in a comparison, arithmetic or a condition: MySQL
  compares and computes JSON values by JSON rules, while the lowered value is
  JSON text.
- `CAST(x AS JSON)` as an argument of a function other than `JSON_*` when the
  function is `CONCAT_WS`, `ELT` or `MAKE_SET`, or when another argument is
  not a non-NULL literal: MySQL stops evaluating arguments at the first NULL
  (`MAKEDATE(CAST('x' AS JSON), NULL)` is NULL) or never evaluates unused
  ones, while Turso evaluates every argument and reports the invalid JSON.
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
  collation, `CAST` to `DATE`, `DATETIME`, `TIME`, `YEAR`, `CHAR(n)` or with a
  character set, `CONVERT(... USING ...)` with any other character set, and
  `^` over function calls or subqueries.

Known differences that remain on the lowered path, as on the fallback: `/`
is SQLite division (integer division for two integers) rather than MySQL's
DECIMAL division; integer overflow becomes a REAL instead of an error; bit
operators are signed 64-bit rather than BIGINT UNSIGNED; string comparisons and
`LIKE` use binary or ASCII case folding rather than `utf8mb4_0900_ai_ci`;
`WHERE` may refer to select-list aliases; `ORDER BY` with `DISTINCT` may use
columns outside the select list; and unaliased columns are named after the expression as sqlparser
prints it (string literals after their value).

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
`LOCALTIME`, `LOCALTIMESTAMP` and the `CURRENT_*` functions. They follow
MySQL 8.4.10:

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
  it is also read as bytes, where MySQL parses its text. Through the SQLite
  fallback, `0x..` literals are integers.
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
`\Q..\E`, `$n` replacements, and errors for empty patterns, invalid flags and
out-of-range positions.

Not supported yet: exact DECIMAL arguments and results (a decimal literal
reaches the functions as a DOUBLE, so `TRUNCATE(1.0, 1)` prints `1` instead
of `1.0` and halves round to even), unsigned BIGINT literals above the signed
range, `0x..` hex literals in the SQLite fallback (they arrive as integers),
other collations and character sets, warnings, ICU-only regular expression
syntax (`\uhhhh`, `\N{..}`, Python-style `(?P<name>)` is rejected), and
`RANDOM_BYTES`. `INSERT(str, pos, len, newstr)` only runs once the statement
is lowered, because `INSERT` is a keyword to the SQLite parser.
