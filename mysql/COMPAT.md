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
signed range, a DOUBLE used as an integer rounds half to even, and a binary
string (an `X'..'` literal) used as a number is its big-endian unsigned value
(0 above 8 bytes). Numbers used as strings print as MySQL prints them. Strings
compare with `utf8mb4_0900_ai_ci` (ICU root collation at primary strength: no
case or accent differences, trailing spaces count); binary strings compare
byte by byte, and `LOCATE`, `INSTR` and `FIELD` convert a binary string
compared with text to `utf8mb4`, failing on invalid UTF-8 as MySQL does.
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
