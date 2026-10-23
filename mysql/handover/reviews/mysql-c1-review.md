# Review of C1 (pi/mysql-c1, 894a08fb..9937ed5d, mysql/ and testing/)

Method: read the diff; ran every probe on the MySQL 8.4.10 oracle (utf8mb4
client, fresh `scratch_review` database) and on the C1 build
(`scratchpad/bin/sqltest-c1-full`, built from 2b9c1c4e, which has the same
code as 9937ed5d). "Before" means `scratchpad/bin/sqltest-on-core`. Probe
scripts: `scratchpad/c1-review/{t,m,cmp,both}.sh`. No cargo was run.

Checked and correct: the integer, DOUBLE, CHAR/VARCHAR/TEXT, BINARY/VARBINARY/BLOB
store rules match MySQL. That covers `'  12 '`, `'1e3'`, `'0x10'` (1265), `-0`,
`1.5`/`2.5`/`-2.5` (decimal literals), signed and unsigned limits as text,
`'9223372036854775807.5'`, `'18446744073709551615.4/.5'`, DOUBLE(M,D)
rounding (1.235, 2.675, ±0.125, -0.375, 9999.995), DOUBLE text edge cases,
multibyte VARCHAR/TINYTEXT limits, emoji, CHAR trailing spaces, BINARY
padding and trailing spaces, and the formatting of numbers stored in VARCHAR
(`1e20`, `1e-16`, `5e-324`, `0.0000001`). AUTO_INCREMENT behaves like
MySQL for 0, `'0'`, 0.4, negative keys, and the end of the TINYINT UNSIGNED
range. ALTER TABLE rewrites keep the types and values. A sample of 15 owned
specs in `types-tables.sqltest` reproduces exactly on the oracle. The
`&&`/`@>` refusal only fires on real tokens and replaces the error that
existed before ("Array features require ..."). No new code comments. Replay
time did not grow (2692 s vs 2819 s on-core).

## Blockers

### B1. DECIMAL columns now silently lose integer precision and print large values with an exponent
`mysql/frontend/types/decimal.rs:6,13` (storage `Real`) with
`float.rs:34` (`*number as f64`). The brief says DECIMAL keeps today's
behaviour. Today (NUMERIC affinity) integers stay exact.
```sql
CREATE TABLE t (d DECIMAL(20,0), e DECIMAL(18,2));
INSERT INTO t VALUES (9007199254740993, 1), (1000000000000000, 0.1);
SELECT d, d + 1 FROM t;
```
MySQL: `9007199254740993|9007199254740994`, `1000000000000000|1000000000000001`.
C1: `9.007199254740992e15|9.007199254740992e15`, `1e15|1.000000000000001e15`.
Before: `9007199254740993`, `1000000000000000|1000000000000001`.
Fix: until C2, give DECIMAL a storage that keeps integers as integers (SQLite
NUMERIC conversion on an `any`-class storage), or have ASSIGN refuse integers
it cannot hold exactly.

### B2. SQL can abort the process or panic: a CREATE TABLE the MySQL parser rejects still falls back to SQLite and makes a marked table with unchecked type parameters
`mysql/frontend/lib.rs:41-52` (the fallback), `types/binary.rs:52-57`,
`types/float.rs:66`.
```sql
CREATE TABLE t (b BINARY(4611686018427387904)) STRICT;
INSERT INTO t VALUES ('a');
```
This aborts with "memory allocation of 4611686018427387904 bytes failed"
(exit 134).
```sql
CREATE TABLE t (d DOUBLE(2, 5)) STRICT;
INSERT INTO t VALUES (1);
```
This panics with "attempt to subtract with overflow" (`precision - scale`).
Without `STRICT`, the lowering rejects both statements. Calling the function
directly, as in `SELECT __mysql_assign_binary('a', 4611686018427387904)`, also
aborts. `CREATE TABLE t (a INT) STRICT` shows that any SQLite-only CREATE
TABLE becomes a marked MySQL table that skips the lowering.
Fix: do what design 2.1 says. When the statement is a CREATE TABLE, return the
lowering's own error instead of falling back. Also bound the parameters inside
the ASSIGN functions: BINARY length ≤ 255, `scale <= precision`, and use
checked arithmetic.

## Should-fix

### S1. Hex literals into numeric columns are stored as their text, not their value (before: an error)
`types/integer.rs:61,84`, `float.rs:37`. INSERT is not lowered, so `X'..'`
reaches ASSIGN as a blob, and its bytes are read as digits.
`CREATE TABLE t (i INT); INSERT INTO t VALUES (X'3132'); SELECT i FROM t;` gives
MySQL `12594` and C1 `12`. The same happens for DOUBLE and BIGINT UNSIGNED.
Before C1, the CHECK failed. `X'41'` gives MySQL 65 and C1 error 1366.
Fix: make a blob going into an integer or DOUBLE column an error until hex
literals are typed (C4/C6).

### S2. Integer literals above 9223372036854775807 are silently rounded into BIGINT UNSIGNED and no longer match in WHERE
`integer.rs:76-83`. `INSERT INTO t (u) VALUES (12345678901234567891)` gives
MySQL `12345678901234567891` and C1 `12345678901234567168`, with no error.
With `u` holding `'18446744073709551615'`,
`SELECT id FROM t WHERE u = 18446744073709551615` gives MySQL 1 row and C1 no
rows. Before C1 it returned the row. COMPAT says such a literal "cannot be
stored", which is wrong.
Fix: refuse doubles ≥ 2^63 in `assign_unsigned_bigint` until literals are
typed (C4), and correct COMPAT (it covers WHERE too).

### S3. Comparisons that worked before now give wrong answers or errors
With `i INT=9`, `u BIGINT UNSIGNED=9`, `v VARCHAR='9'`:

| Expression | MySQL | Before | C1 |
|---|---|---|---|
| `'9' = i` | 1 | 1 | 0 |
| `'8' > i` | 0 | 0 | 1 |
| `'9' <=> i` | 1 | 1 | 0 |
| `WHERE '9' = i` | 1 row | 1 row | 0 rows |
| `v = u` | 1 | 1 | 0 |
| `u = '9.0'`, `u = ''`, `u = '1e1'`, `u = '9x'` | 1/0/0/1 | values | error "invalid value for uint64: …" |

Only the `'5' = col` case is in COMPAT, and it is listed as a gap, not as a
regression.
Fix: convert an untyped text operand to the column type on either side. For
`bigint unsigned`, compare with text leniently (as a double, like MySQL)
instead of through `uint64_encode`.

### S4. CREATE TABLE … SELECT drops ENUM members, BIT ranges, JSON validity, NOT NULL and DEFAULT
Source: `s(id INT AUTO_INCREMENT PRIMARY KEY, j JSON, k ENUM('x','y'), l BIT(3), m INT UNSIGNED NOT NULL DEFAULT 5)`.
After `CREATE TABLE t AS SELECT * FROM s`, the stored definition is
`(id int, …, j longtext, k enum, l bit (3), m int unsigned) STRICT`.
| Insert into t | MySQL | C1 |
|---|---|---|
| `INSERT INTO t (k) VALUES ('zzz')` | 1265 | stored |
| `INSERT INTO t (l) VALUES (8)` | 1406 | stored |
| `INSERT INTO t (j) VALUES ('nope')` | 3140 | stored |
| `INSERT INTO t (m) VALUES (NULL)` | 1048 | stored |
| `INSERT INTO t (k) VALUES ('y')` | `id=0, m=5` | `NULL, NULL` |

Fix: refuse CREATE TABLE … SELECT when a result column's rule lives in a CHECK
(ENUM, SET, BIT, JSON) until those types have ASSIGN rules, and copy NOT
NULL/DEFAULT, or document both.

## Nits
- N1. DOUBLE column values also round half away from zero into integer
  columns. `INSERT INTO t SELECT d FROM s` with d = 2.5, -2.5, 0.5 gives
  MySQL 2, -2, 0 and C1 3, -3, 1. COMPAT mentions only double literals.
- N2. The integer text reader differs from MySQL in small cases: `'1e'`,
  `'1e+'`, `'1.5e'` give MySQL 1, 1, 2 and C1 1265; a leading newline or
  carriage return gives MySQL 1366 and C1 12 (`number_text.rs:100,116-127`);
  `-0.4` into an UNSIGNED column gives MySQL 1264 and C1 0 (decimal literal
  as double).
- N3. A refused CREATE TABLE reports "tables in the mysql dialect must be
  STRICT" (`a SERIAL`, `NVARCHAR`, AUTO_INCREMENT in a composite key)
  instead of the lowering's reason. The B2 fix also fixes this.
- N4. Durable format: JSON is stored as `LONGTEXT CHECK(json_valid)`
  (`ddl_type.rs:179`; design 2.1 says `JSON`), and ENUM/SET are stored as bare
  `ENUM`/`"SET"` with members only in a CHECK (`ddl_type.rs:536`). Databases
  created now lose the MySQL type that C7/C9 will need. Adding a `json` text
  type costs one entry.
- N5. `errors.rs:19-23` picks 1265 and 1416 by the start of the message text,
  and NoMoreKeys (1062) keeps core's "no more keys for t.id". MySQL says
  "Duplicate entry '255' for key 't.PRIMARY'".
- N6. `lib.rs:47` puts the `&&` check between the existing fallback comment
  and the SQLite parse that the comment describes. Move it above the comment.
- N7. `string.rs:72` `text_of` is `pub(super)` but only used in that file.
- N8. `->>` in DDL is lowered to `JSON_UNQUOTE(...)`, which core cannot
  resolve in a generated column, and `parser/tests/conformance.rs` asserts
  that lowering. Refuse it with a clear reason instead.
- N9. CREATE TABLE … SELECT types `SELECT 1` as `bigint` where MySQL uses
  `int NOT NULL DEFAULT '0'`.

Not checked: closing and reopening a database (the sqltest backend creates only
`:memory:`/`:temp:` databases). The Rust test covers reopening for CREATE/ALTER
tables, but nothing covers reopening tables made by CREATE TABLE … SELECT.
