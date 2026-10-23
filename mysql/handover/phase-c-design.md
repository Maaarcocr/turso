# Phase C: MySQL types on the core type system

The plan for the MySQL frontend after Proposal 2 (dialect-defined types) of
`docs/internals/typed-expressions.md` is merged. Baseline: 262,353 of 302,599
specs pass (replay of 2026-10-22,
`/tmp/claude-0/-home-user-turso/d15709bb-7f9a-5758-baa6-00ea81e0de51/scratchpad/full-after-query.json`).

Tags: **[FE]** frontend only; **[P2 n]** / **[P4 n]** needs commit n of that
proposal; **[ASK x]** needs a small core addition from section 1.6. Only C0 is
frontend-only today; once Proposal 2 is merged, all of section 2 and the
formatter (1.4) are frontend work.

## 0. Ground rules

- Rules come from recordings: each slice first records an owned
  `my-sqltests/types-<slice>.sqltest` from MySQL 8.4.10. Examples below are
  recorded specs.
- In-memory forms are MySQL's printed text where possible (DECIMAL `'1.50'`,
  unsigned values above i64::MAX as digits, canonical temporal text). What
  depends on the declared type is formatted from the result column's type
  (1.4).

## 1. Type rules

### 1.1 Column types: storage and conversions

ASSIGN is strict-mode storing (errors stop the statement); CAST is lenient (a
warning and a value). All are **[P2 15, 23]** and need the target's parameters
**[ASK D]**.

| MySQL type | Core storage | ASSIGN | CAST |
|---|---|---|---|
| TINYINT … BIGINT [UNSIGNED] [ZEROFILL](width); BOOL = TINYINT(1) | `ranged_integer(min,max)`, BIGINT UNSIGNED `uint64`; UNSIGNED, ZEROFILL, width are declared-type flags | `'12'` → 12, `'12abc'` 1265, `'abc'` 1366; DECIMAL rounds half away from zero, DOUBLE half to even; out of range 1264 | SIGNED: integer prefix of text (`'1.5'` → 1), DECIMAL rounds; UNSIGNED wraps (`CONVERT(-3, UNSIGNED)` = `18446744073709551613`) |
| BIT(n) | `uint64` ("later" in P2) | above 2^n−1 1406 | — |
| DECIMAL(p,s), default (10,0) | `numeric(p,s)` | round half up to s; too many digits 1264 | round; clamp (`CAST(1000 AS DECIMAL(3,1))` = `99.9`); m > 65 1426, d > 30 1425, m < d 1427 |
| FLOAT, DOUBLE [(M,D)] | REAL; FLOAT rounded to 32 bits | (M,D) rounds to D; 1264 | DOUBLE, FLOAT |
| CHAR(n), VARCHAR(n), TEXT types | TEXT | characters (CHAR, VARCHAR) or bytes (TEXT), 1406; CHAR drops trailing spaces; numbers as MySQL prints them (`1e20`) | CHAR(n) cuts |
| BINARY(n), VARBINARY(n), BLOBs | BLOB | bytes, 1406; BINARY pads with zero bytes | BINARY(n) pads |
| DATE; DATETIME(fsp), TIMESTAMP(fsp); TIME(fsp) | `date_days`; `datetime_micros(fsp)`; `time_micros(fsp)` | parser of `functions/datetime/parse.rs`; round to fsp; TIMESTAMP 1970–2038 UTC; TIME ±838:59:59; else 1292 | invalid → NULL |
| YEAR | `ranged_integer(0,2155)` | 1–69 → 20xx, 70–99 → 19xx, `'0'` → 2000, `0` → 0; else 1264 | YEAR |
| JSON | TEXT | normalized by the existing JSON printer; 3140 | `__mysql_cast_as_json` |
| ENUM(…), SET(…) | `enumeration`, `label_set` | labels without case, numbers as position or bit mask; else 1265 | — |

NULL into NOT NULL is 1048 when given, 1364 when left out; the lowering knows
which. The runner only checks that an error happens, so the codes matter for the
wire adapter, not for spec counts.

### 1.2 Computation classes and operators [P2 21]

MySQL computes every expression as INT (signed or unsigned), DECIMAL, DOUBLE,
STRING or TEMPORAL. Integer column types widen to INT and text types to STRING,
so most rules are Dynamic-class signatures; DECIMAL, `uint64`, temporal and
ENUM/SET add Named signatures.

| Operation | Result | Recorded examples |
|---|---|---|
| `+ - *` | INT → BIGINT, overflow 1690 **[ASK B]**; an UNSIGNED operand → BIGINT UNSIGNED, negative 1690 **[ASK C]**; a DECIMAL → DECIMAL, scale max(s1,s2) for `+ -`, s1+s2 for `*`; a DOUBLE or STRING → DOUBLE | `0 * 1.0` = `0.0`; `-9223372036854775808 - 1` → 1690 |
| `/` | a DOUBLE or STRING → DOUBLE; else DECIMAL, scale = dividend scale + 4; `x/0` → NULL | `2/4` = `0.5000`; `13 / 1.0` = `13.0000`; `4/3/1` = `1.33333333` |
| `DIV`, `%` | truncated quotient, BIGINT; `%` by class as `+` | `1.5 % 1.0e0` = `0.5` |
| `& \| ^ ~ << >>` | BIGINT UNSIGNED; operands rounded first | `~(0)` = `18446744073709551615`; `1.5 >> 0` = `2` |
| unary `-` | INT → BIGINT; DECIMAL → DECIMAL, no negative zero; STRING, hex → DOUBLE | `-0.0` = `0.0`; `-''` = `-0` |
| comparisons, IN, `CASE x WHEN`, NULLIF | two STRINGs: collation; two INTs: sign-aware integer; INT and DECIMAL: DECIMAL; TEMPORAL and STRING: temporal; TEMPORAL and number: number; else DOUBLE | `'A' = 0` = 1 |
| CASE, IF, COALESCE, GREATEST, LEAST, UNION | INT, DECIMAL → DECIMAL, largest scale; any DOUBLE → DOUBLE; any STRING (hex literals count) → STRING | `MIN(COALESCE(88, 30/60))` = `88.0000`; `CASE … 0xFF … 1.1 … 64 END` is the byte 0xFF |
| SUM, AVG **[P4 29, ASK E]** | SUM(INT) → DECIMAL(…,0); AVG adds 4 to the scale; DOUBLE stays DOUBLE | `CAST(AVG(col1) AS SIGNED)` = `34` (today `34.333333333333336`) |

- **Hidden digits.** MySQL keeps more digits of a quotient than it shows and
  rounds to the static scale only when the value is shown or stored:
  `64 / 77 * 77` = `64.0000` (rounding 64/77 to 0.8312 first gives 64.0024).
  So `/` returns whole 9-digit groups (check MySQL's `decimal_div`), and the
  result-parameter scale is applied at output (1.4) and by ASSIGN; keys stay a
  gap until **[ASK F]**.
- **Cost.** Checked integer `+` runs on every row of 117,009 sqllogictest table
  specs: it must be a core scalar function, not a `Func::Dialect` call
  dispatched by name.

### 1.3 Literal types [P2 21]

| Literal | Type |
|---|---|
| `12` | BIGINT; 9223372036854775808 to 18446744073709551615 BIGINT UNSIGNED; larger DECIMAL(n,0) (`SELECT 18446744073709551616` prints the digits); `-9223372036854775808` folds to BIGINT |
| `1.5`, `.5` | exact DECIMAL(digits, digits after the point): `CAST(3.14159…459 AS DECIMAL(30,24))` = `3.141592653589793238462643` |
| `1.5e0` | DOUBLE |
| `'x'`, `"x"`, `_utf8mb4'x'` | STRING: Text class, not Unknown, so `'A' = 0` compares as DOUBLE while `date_col = '2024-5-1'` converts the text to DATE |
| `X'..'`, `0x..`, `b'..'` | a hex-literal dialect type: BLOB, a binary string in string positions and in CASE, BIGINT UNSIGNED in numeric positions |
| `_binary '..'` | VARBINARY: in numeric positions its text is parsed (`SIN(_binary "foobar1234")` = `0`) |
| `DATE '..'`, `TIME '..'`, `TIMESTAMP '..'` | lowered to `CAST('..' AS DATE)` etc.; as numbers `20000101`, TIME with a fraction a DECIMAL: `date '2000-01-01' + time '32:34:58'` = `20323559`, `1.1e0 - time '-10:04:58'` = `100459.1` |
| `TRUE`, `FALSE` | BIGINT 1, 0 |
| `NULL`, `?` | Unknown |

### 1.4 Output formatting and metadata [FE, after P2 25]

`format_value` in `testing/sqltest/src/backends/tursomysql.rs` gets each
column's `ColumnTypeInfo` and moves to `mysql/frontend/format.rs` as
`format_text(value, column_type)`, for the wire adapter to reuse:

- DECIMAL: round the in-memory digits to the static scale.
- FLOAT: the shortest text that reads back as the same 32-bit float;
  FLOAT(M,D), DOUBLE(M,D) and fixed-decimal DOUBLEs: exactly D digits.
- ZEROFILL: pad to the display width; YEAR: four digits (`0000`); BIT: bytes.
- Everything else: today's rules.

The same declared types later give the wire column definitions (type code,
length, decimals, flags).

### 1.5 Functions need argument types

Today a decimal literal reaches the dialect functions as a REAL; with types it
is DECIMAL text (`'1.5'`). 30,300 passing vitess specs pass a decimal or big
integer literal to a dialect function (INTERVAL 2,608, CONV 1,833, FIELD 1,614,
SUBSTRING 1,486, LOCATE 1,380, …). `LOCATE(…, 1.5)` would read `'1.5'` as 1
where MySQL rounds to 2, and `GREATEST(1.5, 10)` would compare text. So
dialect functions receive each argument's static type **[ASK A]**;
`functions::execute` converts DECIMAL, unsigned and temporal arguments to
today's values for every function that has not opted in; functions opt in one
at a time (2.4).

### 1.6 Core additions that Proposal 2 does not list

- **A.** Static argument types for `Func::Dialect` calls, stored with the
  `Function` instruction and passed to `exec_scalar_function`.
- **B.** Checked signed 64-bit `+ - *` and negation (PostgreSQL's `int8` needs
  them too).
- **C.** `uint64_add/sub/mul` accept one signed operand
  (`CAST(5 AS UNSIGNED) + -3` = 2).
- **D.** ASSIGN, CAST and implicit-conversion functions receive the target's
  parameters, as ENCODE does (DECIMAL p and s, ENUM labels).
- **E.** **[P4]** AVG is SUM divided by COUNT with the dialect's `/`, which
  gives MySQL scale + 4.
- **F.** Later: round Named values to their static parameters at key sites.

## 2. Lowering changes (`mysql/parser`)

### 2.1 DDL

- `MySqlDialect` (`mysql/frontend/lib.rs`): `marks_tables`,
  `requires_custom_types`, and `type_rules()` with the name table and
  `zero_generates_key`, in a new `mysql/frontend/types/` with one module per
  family so that slices edit different files.
- `ddl_type.rs`: keep the parameter checks (Rejected errors); emit declared
  types for the name table (`INT UNSIGNED(11)`, `DECIMAL(10,2)`,
  `ENUM('a','b')` in the list form of **[P2 18]**, `JSON` instead of
  `JSON TEXT`, `TINYINT(1)` for BOOL). Delete `value_check` with every CHECK it
  builds (ranges, `char_length(rtrim(..))`, `octet_length`, `json_valid`,
  ENUM/SET `IN` lists, the FLOAT(M,D) `round` check), `set_values` and the
  six-member SET limit.
- `ddl_column.rs`: drop the rewrite of AUTO_INCREMENT columns to `INTEGER` with
  `CHECK (id <> 0)`; every single integer primary key becomes the rowid
  **[P2 24]**.
- `ddl_default.rs`: keep MySQL's CREATE-time checks; drop the value
  normalization that ASSIGN now does.
- Newly lowerable: ZEROFILL, BINARY(n), TEMPORARY, `CREATE TABLE … [AS] SELECT`
  (**[P2 25]**).
- A CREATE TABLE the lowering refuses becomes an error instead of falling back,
  because core refuses unmarked tables in a dialect that marks tables.

### 2.2 Expressions

- `literal.rs`: remove the `Context` guesses about how MySQL will read a
  literal (Number, Arithmetic, Bitwise, Text); literals keep their kind and core
  types them. Gone: `bitwise_operand`, the Arithmetic refusal in
  `binary_literal`, the i64 limit of `unsigned_number`, the context split of
  `translate_typed_string`, `translate_temporal_text`. `_binary` strings become
  `CAST(x'..' AS VARBINARY)`.
- `expr.rs`: remove `comparison_context` and the temporal branch of
  `translate_compared_value`. `DIV` becomes a typed `__mysql_div(a, b)` (was
  `CAST(a / b AS INTEGER)` plus the `exceeds_bigint` refusal); bitwise XOR
  becomes `__mysql_bit_xor(a, b)` (was `(a | b) - (a & b)` with a side-effect
  restriction). `translate_cast`/`decimal_type` emit typed CASTs to SIGNED,
  UNSIGNED, DECIMAL(m,d), DOUBLE, FLOAT, CHAR(n), BINARY(n), DATE,
  DATETIME(fsp), TIME(fsp), YEAR. Remove `argument_context` and the
  negative-precision refusal of ROUND/TRUNCATE.

### 2.3 COMPAT.md entries that become obsolete

- DDL: the "Stored rule" table; the known gaps on `BIGINT UNSIGNED` above
  i64::MAX, inexact `DECIMAL(p,s)`, temporal values "stored as given", ENUM
  compared as text, and `0` into AUTO_INCREMENT; "integer primary key without
  `AUTO_INCREMENT` is not a rowid alias"; ZEROFILL, `BINARY(n)` and
  `CREATE TABLE … AS SELECT` under "Not lowered".
- SELECT: the CAST/CONVERT row ("no rounding … no unsigned wrap-around"); "still
  SQLite numbers" in the literal row; the context rules of the hex and temporal
  literal rows; these "Falls back" items: negative ROUND/TRUNCATE precision,
  hex/bit/temporal literals in arithmetic, `_binary` in numeric contexts,
  temporal literals compared with numbers, CAST to DATE/DATETIME/TIME/YEAR/
  CHAR(n); the "Known differences" sentence on division, overflow and bit
  operators.
- Functions: "Turso has no `DECIMAL`, unsigned or temporal types" (JSON and
  datetime sections), "Decimal literals reach the functions as doubles", "An
  integer literal above the `BIGINT` range becomes a double", and "Not supported
  yet: exact DECIMAL arguments … unsigned BIGINT literals".

### 2.4 Functions that opt in to typed arguments

| Functions | Typed behaviour |
|---|---|
| ROUND, TRUNCATE | DECIMAL → DECIMAL with scale max(0, d), half away from zero (`TRUNCATE(1.0, 1)` = `1.0`); negative d rounds to tens |
| ABS, SIGN, FLOOR, CEIL, MOD | DECIMAL and unsigned stay exact; FLOOR/CEIL of a DECIMAL give an integer |
| GREATEST, LEAST, FIELD, INTERVAL, STRCMP | compare in the aggregated class of 1.2 |
| CONCAT, CONCAT_WS, LPAD, RPAD, INSERT, REPLACE, … | DECIMAL as shown (`1.50`), unsigned digits, temporal text |
| JSON_* | DECIMAL becomes a JSON decimal, unsigned a JSON unsigned integer, temporal a JSON date or time |
| datetime functions | DECIMAL seconds keep their digits (`MAKETIME(10,34,58.1234)` = `10:34:58.1234`); results declared DATE, DATETIME or TIME with fsp through FUNCTION signatures |
| CONV, BIN, OCT, HEX, BIT_COUNT | unsigned arguments exact |

## 3. Expected impact and risks

**Method.** `triage.py` gives each of the 40,246 failing specs its first
blocker; 33,766 are in type-related buckets (wrong numeric, DECIMAL, DOUBLE,
temporal and string-vs-number results; missing errors such as 1425, 1690,
1292; syntax buckets for introducers, typed, hex and bit literals; ENUM/SET and
UNSIGNED DDL). Regular expressions over each failing statement and its setup
list the features it needs; the spec is credited to the last slice that
provides them. Specs that also need JSON comparison rules (9,384), other
collations (2,059) or character sets (2,644) are not credited. Counts are upper
bounds: a spec can hit a second blocker, and a pattern can miss a need. "Likely"
is a judgement: 60–80% for mechanical work (CAST AS DECIMAL, division), 30–50%
where functions must be rewritten. Script:
`/tmp/claude-0/-home-user-turso/d15709bb-7f9a-5758-baa6-00ea81e0de51/scratchpad/phase-c/tools/slices.py`.

| Slice | Upper bound | What it is made of | Likely |
|---|---:|---|---:|
| C0 DECIMAL CAST errors | 148 | missing errors 1425 (114), 1427 (34), raised even when the rest of the statement falls back | ~148 |
| C1 tables | 95 | go-mysql-server DML on typed columns | 50–90 |
| C2 DECIMAL | 5,149 | CAST AS DECIMAL 3,094 (vitess DecimalClamping); division 653 (sqllogictest 348); operators 542; functions 755 | 3,000–4,200 |
| C3 SUM, AVG | 100 | sqllogictest 76 | 80–100 |
| C4 integers, unsigned | 2,759 | functions 1,784; operators 411; bit operators 250 | 900–1,600 |
| C5 temporal | 2,860 | typed literals in arithmetic 1,334; bit operators and division 783; functions and CAST 396 | 1,000–1,700 |
| C6 binary strings | 8,100 | hex and bit literals 2,190; introducers 1,995; math functions 2,055 | 2,500–5,000 |
| C7 ENUM, SET | 152 | go-mysql-server | 100–150 |
| C8 FLOAT, display | 40 | go-mysql-server | 20–40 |

Altogether about 19,400 at most and roughly 8,000–13,000 likely, which would
take the corpus to 270,000–275,000 passing specs.

**Risks to passing specs** (counts of specs that pass today):

1. **Every table changes in C1.** 121,455 specs create tables (117,009
   sqllogictest). ASSIGN, rowid keys and checked arithmetic run on every row of
   4 million replayed setup statements under a 30-second block timeout. C1 must
   lose no sqllogictest spec and keep the replay time.
2. **CREATE TABLE no longer falls back.** A CREATE TABLE the lowering refuses
   runs as SQLite today and fails after C1. Patterns find about 52 such passing
   specs (`LIKE`/`SELECT` forms, BINARY(n), spatial, `ON UPDATE`); C0 lists the
   exact set by lowering every CREATE TABLE in the corpus.
3. **Functions.** 30,300 specs (1.5): without [ASK A] and the default
   conversion, C2 loses more than it fixes.
4. **Decimals everywhere.** 84,664 specs contain a decimal or float literal,
   division, MOD or a numeric CAST. In sqllogictest, `float_col > 61.4` becomes
   a DOUBLE comparison and `int_col > 61.4` a DECIMAL one (slower, and the
   column's index is not used).
5. **FLOAT.** 43,467 specs use FLOAT tables. MySQL's FLOAT is single precision,
   but a storage or display mistake breaks the largest suite; C8 is last.
6. **Fallback statements are typed too.** Statements the lowering refuses and
   SQL that core generates (CHECK, DEFAULT, ALTER rewrites) are parsed as SQLite
   but compiled with the MySQL rules, so their `/` becomes DECIMAL division. C1
   reviews the SQL core generates.
7. Also exposed: 33,312 specs with big integers, UNSIGNED or bit operators
   (C4), 28,626 with temporal values (C5), 23,700 with hex literals or
   introducers (C6).

## 4. Slice plan

Every slice records its owned specs first, replays the full corpus, passes
`score.py` (no baseline spec lost) and `mysql/test.sh`, and sends core
additions to the core branch as separate commits with tests. This host runs one
compiling worker at a time, so "in parallel" means the slices edit different
files and merge without conflicts.

| # | Slice | Needs | Files | In parallel with |
|---|---|---|---|---|
| C0 | Errors 1425/1426/1427 for CAST AS DECIMAL; list CREATE TABLE refusals; a fast canary subset (sqllogictest `index/`, vitess `evalengine/`) | [FE], now | `expr.rs` (`decimal_type`), tools | — |
| C1a | Marked STRICT tables; name table for the integer, text, binary and REAL families; ASSIGN; error mapping | P2 14, 15, 22, 23 | `ddl_*.rs`, `frontend/lib.rs`, `types/{mod,integer,string}.rs` | — |
| C1b | Integer rowid keys, AUTO_INCREMENT, key 0 | P2 24 | `ddl_column.rs` | — |
| C2 | DECIMAL: literals, columns, operators, CAST, CASE aggregation, output rounding, argument types | P2 21, 25; A, D | `literal.rs`, `expr.rs`, `types/decimal.rs`, `functions/string/numeric.rs`, `format.rs` | C7 |
| C3 | SUM, AVG | P4 29; E | `types/decimal.rs` | C5 |
| C4 | Checked BIGINT, `uint64`, CAST SIGNED/UNSIGNED, bit operators, hex literal type | P2 16; B, C | `types/integer.rs`, `literal.rs`, `expr.rs` | C5 |
| C5 | Temporal columns, typed literals, temporal results of functions | P2 17, 21 | `types/temporal.rs`, `functions/datetime/`, `translate_typed_string` only | C4, C3 |
| C6 | `_binary` and other introducers, BINARY(n), binary strings in numeric positions | P2 21; A | `literal.rs`, `functions/string/` | — (after C4) |
| C7 | ENUM, SET | P2 18–20 | `ddl_type.rs` (ENUM/SET only), `types/enums.rs` | C2 |
| C8 | Single-precision FLOAT, fixed-decimal display, ZEROFILL, YEAR, BIT | P2 25 | `types/float.rs`, `format.rs`, `tursomysql.rs` | C5 |
| C9 | JSON type (later; 9,384 JSON-blocked specs) | JSON comparison rules | `types/json.rs`, `functions/json/` | — |

Order: C0 can start now. C1 is first after Proposal 2 because the name table
needs marked tables; it has the largest risk and little gain. C2 is the
largest reliable gain. C4 and C6 both change hex literals, so C6 follows C4.
C8 waits until C2 has exercised the formatter.
