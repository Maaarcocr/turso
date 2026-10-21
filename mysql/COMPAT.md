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
  20000101103458.1234)` gives `125000`. Hexadecimal `0x..` literals are
  integers rather than binary strings.
- An integer literal above the `BIGINT` range becomes a double. The doubles
  for `9223372036854775808` and `18446744073709551615` are read as those
  `BIGINT UNSIGNED` values, so `18446744073709551616` behaves like
  `18446744073709551615`.
- Warnings are not reported.

Not implemented: `GET_FORMAT`, locales other than `en_US`
(`lc_time_names`), and leap seconds.

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
  `UNHEX`.
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
`INSTR`, `FIELD`, `REPLACE`, `INSERT`, `LPAD`, `RPAD` and `SUBSTRING_INDEX`
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
