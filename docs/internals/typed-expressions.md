# Typed expressions

Status: design proposal for review. Nothing here is implemented yet.

## Problem

### Background

Turso compiles SQL from several frontends (the SQLite dialect, and the
PostgreSQL frontend `tursopg`) into the same bytecode. On top of SQLite's
four storage classes (INTEGER, REAL, TEXT, BLOB), Turso has **custom types**,
defined with `CREATE TYPE` (experimental: `tursodb
--experimental-custom-types`; `tursopg` always enables them):

```sql
CREATE TYPE name(value any, param integer, ...) BASE blob
  ENCODE expr DECODE expr OPERATOR '+' function ...
```

- `BASE` is the storage class used on disk.
- `ENCODE` turns a value into its stored form when a row is written; `DECODE`
  turns the stored form back into the value that queries see. In both, `value`
  is the input.
- `OPERATOR '+' numeric_add` makes `+` call the function `numeric_add`.
  Choosing which function implements an operator is called *dispatch* below.
- Custom types apply only to columns of STRICT tables (SQLite tables that
  enforce column types). `tursopg` creates every table as STRICT.

The built-in type `numeric(precision, scale)` is an exact decimal number, like
PostgreSQL's NUMERIC and MySQL's DECIMAL. `numeric(10,2)` has at most 10
digits, 2 of them after the decimal point (for example 12345678.90). Turso
stores it as a blob, DECODE returns text such as `'1.50'`, and `+ - * / < =`
call `numeric_add`, `numeric_sub`, and so on. REAL, by contrast, is a 64-bit
floating-point number that keeps only about 16 significant digits.

The compiler never works out the type of an expression. For each operator it
only asks: "is one operand a bare column of a custom type, and the other a
column of the same type or a literal?" (`expr_custom_type_info` and
`find_custom_type_operator` in `core/translate/expr/custom_types.rs`; for
arithmetic on types whose DECODE returns the value unchanged, such as `date`,
any other operand is accepted). In
`(p + 1) + 2` the inner `+` has the bare column `p`, so it calls `numeric_add`;
the outer `+` has `p + 1` on its left, which is not a column, so Turso does not
know it is a numeric and uses SQLite's `+`, which computes with REAL. Types are
also guessed in two other unrelated places: the column types `tursopg` reports
to PostgreSQL clients (*wire metadata*, `infer_expression_primitive` in
`core/statement.rs`), and SQLite *affinity*, the per-column rule that converts
values to INTEGER, REAL or TEXT before storing or comparing them
(`get_expr_affinity`).

### Examples

Each example is complete and starts from an empty database. "Turso today" is
`tursodb --experimental-custom-types`;
`tursopg` prints the same unless noted. PostgreSQL 17 runs the same statements
without `STRICT`.

**1. Values are cut off instead of rounded.**

```sql
CREATE TABLE t(p numeric(10,2)) STRICT;
INSERT INTO t VALUES ('1.505');
SELECT p FROM t;
-- Turso today:   1.50
-- PostgreSQL 17: 1.51
```

Fitting a value to 2 digits after the point drops the extra digits
(`with_scale` in `core/numeric/decimal.rs`) instead of rounding. The other
examples use values that need no rounding, so each shows one bug.

Examples 2 to 4 use one table with one row:

```sql
CREATE TABLE t(p numeric(20,2), i INTEGER) STRICT;
INSERT INTO t VALUES ('12345678901234567.89', 3);
SELECT p + 1 FROM t;
-- Turso today and PostgreSQL 17: 12345678901234568.89 (correct)
```

**2. Only a bare column gets numeric arithmetic.**

```sql
SELECT (p + 1) + 2 FROM t;
-- Turso today:   1.23456789012346e+16
-- PostgreSQL 17: 12345678901234570.89
SELECT coalesce(p, 0) + 1 FROM t;
-- Turso today:   1.23456789012346e+16
-- PostgreSQL 17: 12345678901234568.89
```

`p + 1` and `coalesce(p, 0)` are not bare columns, so the outer `+` is SQLite's,
which converts the text `'12345678901234568.89'` to REAL and loses digits.

**3. Unary minus, functions and aggregates do not know numeric.**

```sql
SELECT -p FROM t;      -- Turso today: -1.23456789012346e+16
                       -- PostgreSQL 17: -12345678901234567.89
SELECT abs(p) FROM t;  -- Turso today: 1.23456789012346e+16
                       -- PostgreSQL 17: 12345678901234567.89
SELECT sum(p) FROM t;  -- Turso today: 1.23456789012346e+16
                       -- PostgreSQL 17: 12345678901234567.89
SELECT avg(p) FROM t;  -- Turso today: 1.23456789012346e+16
                       -- PostgreSQL 17: 12345678901234567.8900
```

Custom types cannot declare a unary operator, and `abs`, `sum` and `avg` are
SQLite's, which convert text to REAL.

**4. A numeric plus an INTEGER column is computed as REAL.**

```sql
SELECT p + i FROM t;
-- Turso today:   1.23456789012346e+16
-- PostgreSQL 17: 12345678901234570.89
```

Numeric `+` is used only when the other operand is a numeric column or a
literal, so an INTEGER column gets SQLite's `+`.

**5. A literal is converted with the column's own precision and scale.**

```sql
CREATE TABLE t(p numeric(5,2), q numeric(5,0)) STRICT;
INSERT INTO t VALUES ('1.55', '2');
SELECT q * 1.5 FROM t;                -- Turso today: 2
                                      -- PostgreSQL 17: 3.0
SELECT p FROM t WHERE p < 1.555;      -- Turso today: (no row)
                                      -- PostgreSQL 17: 1.55
SELECT p FROM t WHERE p = 1.555;      -- Turso today: 1.55
                                      -- PostgreSQL 17: (no row)
SELECT p FROM t WHERE p < 100000;     -- Turso today: error: numeric value
                                      --   out of range: precision 5, scale 2
                                      -- PostgreSQL 17: 1.55
```

Before calling the operator, Turso passes the literal through the column's
ENCODE with the column's parameters: 1.5 becomes 1 for `q numeric(5,0)` (2
with rounding, still not 1.5), 1.555 becomes 1.55 for `p numeric(5,2)`, and
100000 does not fit in 5 digits.

**6. Division keeps 100 digits.** (Same table as example 5.)

```sql
SELECT p / 3 FROM t;
-- Turso today:   0.5166666666666666 ... 6667  (100 digits after the point)
-- PostgreSQL 17: 0.51666666666666666667
```

`numeric_div` uses the decimal library's default of 100 digits; PostgreSQL
chooses at least 16 significant digits (20 after the point here).

**7. The result of a numeric operator is compared as text.**

```sql
CREATE TABLE t(p numeric(10,2)) STRICT;
INSERT INTO t VALUES ('1.50');
SELECT p FROM t WHERE p * 1 > 5;
-- Turso today:   1.50
-- PostgreSQL 17: (no row)
```

`numeric_mul` returns the text `'1.50'`, Turso does not know it is a number,
and in SQLite's rules any text is greater than any number. Sorting has the same
problem; with two rows, `9.00` and `10.00`:

```sql
CREATE TABLE t(p numeric(10,2)) STRICT;
INSERT INTO t VALUES ('9.00'), ('10.00');
SELECT max(p * 1) FROM t;
-- Turso today:   9.00
-- PostgreSQL 17: 10.00
```

Only a bare numeric column is sorted with the numeric comparison
(`custom_type_comparator` in `core/translate/order_by.rs`).

**8. CAST returns the stored form.**

```sql
SELECT CAST('1.50' AS numeric(10,2));
-- Turso today:   tursodb: an 18-byte blob (typeof returns 'blob');
--                tursopg: 1.5 (it translates the CAST to REAL)
-- PostgreSQL 17: 1.50
```

CAST to a custom type only applies ENCODE, so the result is the storage bytes.

Indexes have a related limit: values are stored as blobs whose byte order is
not numeric order, so an index on a numeric column is used only for `=`, never
for `<`, `>` or ORDER BY (Phase B).

## Model

Types exist only at compile time. At run time a register (a value slot of
Turso's bytecode machine) contains an ordinary SQLite value; for an expression
of a custom type, it is the type's *in-memory form*: what DECODE returns
(numeric: text such as `'1.50'`). ENCODE runs only when a row or index entry is
written, and when a *seek key* (the value looked up in an index, e.g. 1.555 in
`WHERE p = 1.555`) is built. No new kinds of runtime values, no B-tree change,
no file format change for plain SQLite databases.

Every expression gets a type: **Named** (a registered type: built-in or made
with CREATE TYPE) or **Dynamic** (SQLite's rules: each value's storage class
decides at run time; the compiler records integer, real, text, blob or
unknown). New module `core/translate/expr/types.rs`:

```rust
pub(crate) enum ExprType { Named(NamedType), Dynamic(DynamicClass) }
pub(crate) struct NamedType { def: Arc<TypeDef>, params: Vec<ast::Expr> }
pub(crate) enum DynamicClass { Integer, Real, Text, Blob, Unknown }
pub(crate) fn expr_type(expr: &ast::Expr, tables: Option<&TableReferences>,
                        resolver: &Resolver) -> Result<ExprType>;
```

- Column: `Named` when its declared type is registered and the table allows
  custom types, else `Dynamic` from its affinity. This also covers columns
  read by CHECK constraints, generated columns and expression indexes, column
  references that UPDATE and UPSERT rewrite into registers (a `register_types`
  map like today's `register_affinities`), and subquery, CTE and view columns
  (the type of the inner result expression).
- `CAST(x AS T)`: `Named(T)` when T is registered and the CAST gives all of
  T's required parameters. Otherwise it is SQLite's *affinity cast* (the type
  name only chooses INTEGER, REAL, TEXT, NUMERIC or BLOB conversion) and the
  result is `Dynamic`, in two cases:
  - T is not registered. SQLite accepts any type name in a CAST:
    `CAST(x AS SIGNED)`, `CAST(x AS VARCHAR(10))` and `CAST(x AS FOOBAR)` are
    all legal. An error here would break SQLite compatibility; a frontend that
    wants strict type names rejects unknown ones in its own parser.
  - T is registered but parameters are missing: `CAST(x AS NUMERIC)` takes
    the affinity cast today (the "fall through to regular CAST" branch of
    `translate_cast_expr`, `core/translate/expr/translator.rs`), because
    built-in names such as `numeric` are also names SQLite programs cast to.
    See open question 9.
- Operators: the result type of the operator chosen below.
- Functions: a type-declared function or the dialect's return type.
  `coalesce`, `ifnull`, `nullif`, `iif`, `CASE`, scalar `min`/`max`: the
  type of the non-NULL branches when they all have it or convert to it.
- Literals: integer, real, blob by kind. String literals, `NULL` and `?` are
  `Unknown`: they take the other operand's type, as in PostgreSQL. A frontend
  that wants a typed literal emits a CAST.
- Otherwise `Dynamic`, with `infer_expression_primitive`'s classes.

`expr_type` runs on demand while bytecode is generated, and returns `Dynamic`
at once when no types are registered (custom types off). Result column types
are computed once and stored in `Program`, so the wire metadata
(`get_column_type_info`) reports `numeric` for `p + 1`; Dynamic columns keep
today's output.

## Declarations

`CREATE TYPE` gains clauses (`TypeOperator` in `sqlite/parser/src/ast.rs`,
`parse_create_type`, `TypeDefKind::Custom`, `build_create_type_sql`):

```sql
OPERATOR 'op' [([left-type,] right-type)] [RETURNS type] [function]
CAST {FROM | TO} type [, ...] AS IMPLICIT
FUNCTION name(arg-type, ...) RETURNS type function
ORDERED                                          -- Phase B
```

A type is a registered type, `integer`, `real`, `text`, `blob`, or `any`
(passed unchanged). One type in parentheses is a prefix operator.
Comparisons return a Dynamic integer. The SQLite dialect's numeric:

```sql
CREATE TYPE numeric(value any, precision integer, scale integer) BASE blob
  ENCODE numeric_encode(value, precision, scale) DECODE numeric_decode(value)
  CAST FROM integer, real AS IMPLICIT
  OPERATOR '+' (numeric, numeric) RETURNS numeric numeric_add  -- and - * /
  OPERATOR '-' (numeric) RETURNS numeric numeric_neg
  OPERATOR '<' (numeric, numeric) numeric_lt
  OPERATOR '=' (numeric, numeric) numeric_eq
  FUNCTION abs(numeric) RETURNS numeric numeric_abs
```

Stored user types (canonical SQL in `__turso_internal_types`) keep their
meaning: a clause without a signature (`OPERATOR '+' f`, `OPERATOR '<'`, the
old `OPERATOR '+' (t) -> f`) means `(T, T)`, plus `(T, any)` and `(any, T)`
for arithmetic when DECODE returns the value unchanged, with a Dynamic result;
`>`, `>=`, `<=`, `!=` are still derived. Only two things change: these
operators also apply to typed sub-expressions, and a literal operand gets the
type's in-memory form, not the column's storage form.

## Operator resolution

*Resolution* chooses the function that implements an operator. For `a OP b`
(or `OP a`), where a *signature* is one declared `(left, right)` pair:

1. No `Named` operand: today's SQLite operator; bytecode unchanged.
2. Candidates are OP's signatures from the operands' types. An operand fits a
   slot if it has that type or converts implicitly: `Unknown` converts to
   anything; X converts to T if T declares `CAST FROM X` or X declares
   `CAST TO T`.
3. The candidate with the most exact matches wins. On a tie, prefer the one
   whose slot for an `Unknown` operand is the other operand's type; still
   tied: error `operator is not unique`.
4. No candidate, but the `Named` operands declare `CAST TO` a Dynamic class:
   convert them and use the SQLite operator.
5. Otherwise: the SQLite operator on the in-memory values (today's fallback).

Conversion into T is `DECODE(ENCODE(x))` with no parameters (NULL means "no
limit": `numeric_encode` skips rounding and the precision check, `varchar`
the length check). Constant conversions are emitted once, before the loop.
`emit_typed_operator` replaces `emit_custom_type_operator` in both callers
(`translate_binary_expr`, `condition.rs`).

## CAST and the encode boundary

`CAST(x AS T(params))` returns `DECODE(ENCODE(x))`: validated, rounded,
in-memory (domains: encode chain, today's checks, decode chain). INSERT and
UPDATE still encode; column reads still decode
(`emit_user_facing_column_value`). The CAST section of `create-type.mdx`
changes. `validate_precision_scale` (`core/numeric/decimal.rs`) rounds half
away from zero (`with_scale_round(s, RoundingMode::HalfUp)`) instead of
truncating with `with_scale`.

## Dialects

New `Dialect` methods, with defaults that keep today's behaviour:
`builtin_types()` (the SQL now in `bootstrap_builtin_types`),
`custom_types_on_all_tables()` (false; `get_type_def(ty, is_strict)` callers
switch to a table-level check) and `function_return_type()` (None).

PostgreSQL gets its own numeric: `CAST FROM integer` and `CAST TO real` (so
`p + float8_col` stays `double precision`, as in PostgreSQL), optional
parameters (`precision integer DEFAULT NULL`), PostgreSQL's division and AVG
scale. The frontend
(`postgres/parser/translator.rs`) emits `CAST('1.555' AS numeric)` for
decimal literals and for `x::numeric`. Bare `NUMERIC` columns stay REAL:
changing that mapping would reinterpret existing tables.

## Phase B

**Aggregates.** `SUM`/`AVG` use the type's `FUNCTION sum(T)`/`avg(T)` (native
exact `numeric_sum`, `numeric_avg`). Without one, a new kind of accumulator
adds each value with the type's `+` (AVG: `+`, then `/` by the count). `MIN`, `MAX`, ORDER BY,
GROUP BY, DISTINCT and compound selects take the comparator from `expr_type`,
not only for bare columns (`custom_type_comparator`). `TOTAL` stays REAL.

**Order-preserving numeric storage (v2).** Today's stored format (v1) is a
version byte 0x01, a sign byte, the scale, and the digits as little-endian
32-bit words, so comparing two stored blobs byte by byte does not compare the
numbers. v2: byte 0 is 0x10
negative, 0x20 zero, 0x30 positive. Then the exponent E (value = 0.d1d2… ×
10^E, d1 ≠ 0) as 4 bytes big-endian with the sign bit flipped, then the digits
without trailing zeros in base-100 pairs (byte = pair + 1). Negative values
invert the exponent bytes, use byte = 100 − pair and end with 0xFF. Byte order
equals numeric order and equal values have equal bytes. DECODE becomes
`numeric_decode(value, scale)`.

`ORDERED` declares that comparing stored values the way SQLite compares their
BASE storage class gives the same order as the type's `<`. `optimizer/constraints.rs` and `optimizer/order.rs` then allow
ranges and index order for ORDERED types (numeric v2, uuid, date, time,
timestamp, varchar, smallint, bigint, bytea) when the comparison resolved to
the type's operator, and check typed expressions, not only columns
(expression indexes). Seek keys are encoded without parameters.

**Compatibility.** Decoding accepts v1 and v2 forever. A column must not mix
formats, so the format is per database: when a numeric column is first
created in a database without v1 numeric columns, a marker row is written to
`__turso_internal_types` in the same transaction: a `CREATE TYPE numeric` using
v2 functions, so older versions fail loudly instead of writing v1. Databases
with numeric columns and no marker keep v1 and today's index limits.

## SQLite dialect is untouched

Custom types off: no types are registered, so every expression is Dynamic,
step 1 always applies and code generation takes today's path. Custom types on:
non-STRICT columns never get a type, SQLite type names cannot be registered,
and a CAST to a registered name is already Turso-specific. Verification:

- `CI=1 make -C sqlite/conformance run-rust CARGO_PROFILE=` (debug build): all
  427 EXPLAIN snapshots (saved bytecode listings that the tests compare)
  unchanged.
- A one-off script diffs `EXPLAIN` of every `sqlite-sqltests` statement between
  `main` and the branch, custom types off and on; result in the PR.
- `make test`, the sqltest suites and the wire metadata tests pass.

## Test plan

Each commit adds a test that fails before and passes after (both runs
recorded).

- Rust unit tests: rounding; v2 encoding (quickcheck: byte order equals
  numeric order; v1 still decodes).
- `sqlite/conformance/turso-sqltests/typed_expressions.sqltest`: every
  example above, user types with signatures, old syntax, `EXPLAIN QUERY PLAN`.
- `tests/integration/`: a database file written by today's `main` (v1
  numeric data, an index, a user type with old OPERATOR syntax): reads,
  equality lookups, new writes stay v1, `integrity_check`; close/reopen for v2.
- `postgres/conformance/pg-sqltests/numeric.sqltest`: `make -C
  postgres/conformance run`; expected values checked with `run-postgres`.

## Commits

1. `core/numeric: round half away from zero when applying a scale`
2. `core/types: compute expression types` (dispatch uses `expr_type`)
3. `parser: parse operator signatures, implicit casts and type functions`
4. `core/dialect: let dialects define built-in types`
5. `core/types: resolve operators by signature` (numeric signatures for both
   dialects, so PostgreSQL's `numeric + float8` never becomes numeric)
6. `core/types: unary operators and type functions`
7. `core/types: return the in-memory value from CAST to a custom type`
8. `core/types: report typed result columns in column metadata`
9. `postgres: exact numeric literals, casts and division scale`
10. `core/dialect: allow custom types on non-STRICT tables`
11. `core/types: typed comparators for ORDER BY, GROUP BY, MIN and MAX`
12. `core/types: typed SUM and AVG`
13. `core/numeric: add an order-preserving storage format`
14. `core/schema: choose the numeric storage format per database`
15. `core/optimizer: use indexes on ORDERED types for ranges and order`

## Open questions

1. Format choice: marker row (proposed), a header flag, or an explicit
   migration command for v1 databases?
2. v2 drops the display scale of unconstrained numeric values (PostgreSQL
   keeps it). Acceptable while PostgreSQL maps bare `NUMERIC` to REAL?
3. SQLite dialect division/AVG scale: today's 100 digits or PostgreSQL's rule?
4. The generic SUM/AVG accumulator (a new VDBE accumulator kind) or only
   type-declared aggregates?
5. Persisted expressions whose result changes (expression indexes, generated
   columns, CHECK, PostgreSQL table SQL translated at load): require
   `REINDEX`, detect, or accept?
6. SQLite dialect converts REAL to numeric implicitly (shortest decimal text);
   PostgreSQL converts numeric to REAL. Agreed?
7. Sort comparators: keep the fixed name table (`order_by.rs`) or let the
   sorter call the type's `<` function?
8. `smallint_col = 100000` errors because ENCODE validates the seek key, as
   today. Defer?
9. CAST to a registered type without its parameters. Recommendation: make it
   `Named(T)` with NULL ("no limit") parameters, the same rule as the implicit
   conversions above. The clash with SQLite's `CAST(x AS NUMERIC)` is handled
   by each dialect owning its registry: PostgreSQL and MySQL register their own
   exact numeric/decimal, and the SQLite dialect keeps the affinity cast for
   type names SQLite programs already use (such as NUMERIC and VARCHAR), so
   existing SQLite SQL keeps its meaning. The clash is real today: with custom
   types on, `CAST(12 AS VARCHAR(10))` returns the integer 12, where SQLite
   returns the text '12'. Agreed?
