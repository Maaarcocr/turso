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
| `UNION [ALL \| DISTINCT]`, `INTERSECT`, `EXCEPT`, parenthesized members with their own `ORDER BY`/`LIMIT`, `WITH [RECURSIVE]` with column lists, `VALUES ROW(...)` | Same; a member that has its own `ORDER BY`/`LIMIT`, or a right operand that is itself a set operation (`INTERSECT` binds tighter), becomes `SELECT * FROM (...)` |
| Window functions with `PARTITION BY`, `ORDER BY`, `ROWS`/`RANGE` frames and named windows | Same |
| Column references, `?` parameters (numbered in text order), `CASE`, `IN` lists and subqueries, `BETWEEN`, `EXISTS`, scalar subqueries, row values, `IS [NOT] NULL/TRUE/FALSE/UNKNOWN`, `= ANY (subquery)`, `<> ALL (subquery)` | Same expressions; `IS UNKNOWN` is `IS NULL`; `= ANY` is `IN`, `<> ALL` is `NOT IN` |
| `a DIV b` | `CAST(a / b AS INTEGER)`: truncates toward zero, `NULL` for a zero divisor |
| `MOD`, `%`, `XOR`, `&&`, `\|\|`, `!`, `^`, `<=>` | `%`; `(NOT a) <> (NOT b)`; `AND`; `OR`; `NOT`; `(a \| b) - (a & b)` (only when neither side calls a function or has a subquery, since both sides appear twice); `IS` |
| `a LIKE p [ESCAPE c]`, `REGEXP`, `RLIKE` | `LIKE` with `ESCAPE '\'` unless the pattern is a string without backslashes (MySQL's default escape character); a dangling escape at the end of a literal pattern matches itself, as in MySQL; `REGEXP` operator |
| Function calls | Passed through by name, so SQLite built-ins and the MySQL functions in `mysql/frontend/functions` both resolve; `COUNT(*)`, `DISTINCT` arguments and `OVER` are kept |
| `GROUP_CONCAT(a, b, ... [ORDER BY ...] [SEPARATOR s])` | `group_concat(a \|\| b \|\| ..., s)` (Turso does not support `ORDER BY` inside aggregates yet and reports an error) |
| `DATE_ADD/ADDDATE(d, INTERVAL v unit)`, `d + INTERVAL v unit`, `INTERVAL v unit + d`; `DATE_SUB/SUBDATE(...)`, `d - INTERVAL v unit`; `EXTRACT(unit FROM d)`; `TIMESTAMPADD/TIMESTAMPDIFF(unit, ...)` | `DATE_ADD(d, v, 'UNIT')`, `DATE_SUB(d, v, 'UNIT')`, `EXTRACT('UNIT', d)`, `TIMESTAMPADD('UNIT', ...)`, `TIMESTAMPDIFF('UNIT', ...)` with the unit as upper-case text (the datetime functions implement these forms) |
| `x->'path'`, `x->>'path'` | `JSON_EXTRACT(x, path)`, `JSON_UNQUOTE(JSON_EXTRACT(x, path))` |
| `CURRENT_DATE`, `CURRENT_TIME`, `CURRENT_TIMESTAMP` (with or without `()`) | SQLite's current-time literals |
| `POSITION(a IN b)`, `SUBSTRING(s FROM a FOR b)`, `TRIM([LEADING \| TRAILING \| BOTH] [c] FROM s)` with `c` one character | `LOCATE(a, b)`, `SUBSTRING(s, a, b)`, `LTRIM/RTRIM/TRIM(s, c)` |
| `CAST`/`CONVERT(x, type)` to `SIGNED [INTEGER]`, `UNSIGNED [INTEGER]`, `CHAR`, `DOUBLE`, `REAL`, `FLOAT`, `DECIMAL[(m[,d])]` | The same SQLite type name the fallback used, so the result is unchanged: no rounding to an integer or to `d` decimals, and no unsigned wrap-around |
| `CAST(x AS BINARY)`, `CONVERT(x USING utf8mb4/utf8mb3/utf8/binary)` | `CAST(x AS BLOB)`, `CAST(x AS TEXT)` |
| `COLLATE binary`, `COLLATE utf8mb4_0900_bin` | `COLLATE BINARY` |
| Integer, decimal and scientific literals, `TRUE`, `FALSE`, `NULL` | The same literal text the SQLite parser keeps (decimals and integers above the signed 64-bit range are still SQLite numbers) |
| `'...'`, `"..."`, `N'...'` | Text (`"..."` is a string, never an identifier) |
| `X'..'`, `0x..`, `b'..'`, `0b..` | A blob; in a comparison with a number, a logical operator, a condition, or `\|`, `&`, `>>`, `^`, the unsigned integer value; in a comparison with a string, text |
| `_utf8mb4'..'`, `_utf8mb3`, `_utf8`, `_binary`, and `_ascii`/`_latin1` with ASCII text, with a string or hex value | Text; `_binary` follows the hex literal rules |
| `DATE '..'`, `TIME '..'`, `TIMESTAMP '..'`, `{d '..'}` | Canonical text (`'2018-05-01'`, `'26:00:00'`, `'2020-01-02 03:04:05.5'`); in a comparison with a number, a logical operator or a condition, the number MySQL uses (`20180501`) |
| `/*+ ... */` optimizer hints | Ignored (they only choose a plan), except `SET_VAR` and `MAX_EXECUTION_TIME`, which fall back |

Falls back to SQLite parsing: `SQL_*`, `HIGH_PRIORITY` and `STRAIGHT_JOIN`
modifiers, `STRAIGHT_JOIN` joins, index hints, `PARTITION (...)` selection,
`INTO`, locking reads (`FOR UPDATE`, `FOR SHARE`), `WITH ROLLUP`, `TABLE t`,
`VALUES` without `ROW`, derived-table column lists `AS t(a, b)`, `EXCEPT ALL`
and `INTERSECT ALL`, `FULL JOIN`, `NULLS FIRST/LAST`, user and system
variables (`@a`, `@@x`), `:name` and `?N` parameters, `MATCH ... AGAINST`,
`MEMBER OF`, `SOUNDS LIKE`, `JSON_TABLE`, `COLLATE` with any other collation,
`CAST` to `DATE`, `DATETIME`, `TIME`, `YEAR`, `JSON`, `CHAR(n)`, `BINARY(n)` or
with a character set, `CONVERT(... USING ...)` with any other character set,
`TRIM` of a longer string, `^` over function calls or subqueries, other
character set introducers, `DATETIME '..'`, invalid temporal literals, odd
`X'...'` digits, hex, bit and temporal literals in arithmetic (`+ - * / DIV %
MOD`, unary `-`/`+`/`~`, `<<`: MySQL computes hex and bit literals as BIGINT
UNSIGNED, which fails on negative results, and temporal arithmetic can
overflow), and `DIV` with a literal outside the BIGINT range.

Known differences that remain on the lowered path, as on the fallback: `/`
is SQLite division (integer division for two integers) rather than MySQL's
DECIMAL division; integer overflow becomes a REAL instead of an error; bit
operators are signed 64-bit rather than BIGINT UNSIGNED; string comparisons and
`LIKE` use binary or ASCII case folding rather than `utf8mb4_0900_ai_ci`;
`WHERE` may refer to select-list aliases; `ONLY_FULL_GROUP_BY` is not
enforced; and unaliased columns are named after the expression as sqlparser
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
