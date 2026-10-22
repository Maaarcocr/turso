# Typed expressions

Status: design proposal for review. Nothing here is implemented yet.

## Problem

The compiler never computes the type of an expression. Custom type operators
are chosen by expression *shape*: `expr_custom_type_info`
(`core/translate/expr/custom_types.rs`) sees only a bare column, and a literal
operand goes through the *column's* ENCODE with the column's parameters.
Operator dispatch, wire metadata (`infer_expression_primitive`) and affinity
(`get_expr_affinity`) are three separate inferences.

`p numeric(10,2)` (rows 1.505, 1.55), `q numeric(10,0)` = 2, `i INTEGER` = 3.
"Today" checked with tursopg and `tursodb --experimental-custom-types`, "After"
with PostgreSQL 17:

| Expression | Today | After (= PostgreSQL) |
|---|---|---|
| stored 1.505 | 1.50 (truncated) | 1.51 |
| `(p+1)+2`, `coalesce(p,0)+1` | 4.5, 2.5 (REAL) | 4.51, 2.51 |
| `-p`, `abs(p)` | -1.5, 1.5 | -1.51, 1.51 |
| `p + i` | 4.5 | 4.51 |
| `q * 1.5` | 2 (1.5 encoded with scale 0) | 3.0 |
| `p < 1.555` | loses 1.55 | 1.51, 1.55 |
| `p = 1.555` | matches 1.55 | no row |
| `p < 100000`, `p numeric(5,2)` | error: out of range | 1.50 |
| `CAST(1.505 AS numeric(10,2))` | blob (tursodb), 1.505 (tursopg) | 1.51 |
| `sum(p)` | 3.05 REAL | 3.06 |
| `p / 3` (p = 1.55) | 100 digits | 0.51666666666666666667 |
| index on `p*1`, `WHERE p*1 > 5` | returns 1.50 (text keys) | no 1.50 |

## Model

Types exist only at compile time. A register contains an ordinary SQLite
value; for a typed expression, the type's *in-memory form* (what DECODE
returns; numeric: canonical text such as `'1.50'`). ENCODE runs only when a row,
an index key or a seek key is built. No new `Value` variants, no B-tree or
file format change for plain SQLite databases.

New module `core/translate/expr/types.rs`:

```rust
pub(crate) enum ExprType { Named(NamedType), Dynamic(DynamicClass) }
pub(crate) struct NamedType { def: Arc<TypeDef>, params: Vec<ast::Expr> }
pub(crate) enum DynamicClass { Integer, Real, Text, Blob, Unknown }
pub(crate) fn expr_type(expr: &ast::Expr, tables: Option<&TableReferences>,
                        resolver: &Resolver) -> Result<ExprType>;
```

- Column: `Named` when its declared type is registered and the table allows
  custom types, else `Dynamic` from its affinity. Also self-table columns
  (CHECK, generated columns, expression indexes), `Expr::Register` (a
  `register_types` map like `register_affinities`) and subquery, CTE and view
  columns (the type of the inner result expression).
- `CAST(x AS T)`: `Named(T)` when T is registered and its required
  parameters are given (today's rule), else `Dynamic`.
- Operators: the result type of the operator chosen below.
- Functions: a type-declared function or the dialect's return type.
  `coalesce`, `ifnull`, `nullif`, `iif`, `CASE`, scalar `min`/`max`: the
  type of the non-NULL branches when they all have it or convert to it.
- Literals: integer, real, blob by kind. String literals, `NULL` and `?` are
  `Unknown`: they take the other operand's type, as in PostgreSQL. A frontend
  that wants a typed literal emits a CAST.
- Otherwise `Dynamic`, with `infer_expression_primitive`'s classes.

`expr_type` runs on demand during emission and returns `Dynamic` at once when
the registry is empty (custom types off). Result column types are computed
once and stored in `Program`, so `get_column_type_info` reports `numeric` for
`p + 1`; Dynamic columns keep today's output.

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

For `a OP b` (or `OP a`):

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
exact `numeric_sum`, `numeric_avg`). Without one, a new fold accumulator applies
the type's `+` (AVG: `+`, then `/` by the count). `MIN`, `MAX`, ORDER BY,
GROUP BY, DISTINCT and compound selects take the comparator from `expr_type`,
not only for bare columns (`custom_type_comparator`). `TOTAL` stays REAL.

**Order-preserving numeric storage (v2).** Byte 0: 0x10 negative, 0x20 zero,
0x30 positive (v1 starts with 0x01). Then the exponent E (value = 0.d1d2… ×
10^E, d1 ≠ 0) as 4 bytes big-endian with the sign bit flipped, then the digits
without trailing zeros in base-100 pairs (byte = pair + 1). Negative values
invert the exponent bytes, use byte = 100 − pair and end with 0xFF. Byte order
equals numeric order and equal values have equal bytes. DECODE becomes
`numeric_decode(value, scale)`.

`ORDERED` declares that base-type comparison of encoded values matches the
type's `<`. `optimizer/constraints.rs` and `optimizer/order.rs` then allow
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

Custom types off: the registry is empty, `expr_type` is always Dynamic and
step 1 always applies, so the emitter takes today's path. Custom types on:
non-STRICT columns never resolve, SQLite type names cannot be registered, and
a CAST to a registered name is already Turso-specific today. Verification:

- `CI=1 make -C sqlite/conformance run-rust CARGO_PROFILE=` (debug build): all
  427 EXPLAIN snapshots unchanged.
- A one-off script diffs `EXPLAIN` of every `sqlite-sqltests` statement between
  `main` and the branch, custom types off and on; result in the PR.
- `make test`, the sqltest corpora and the PostgreSQL wire metadata tests
  pass.

## Test plan

Each commit adds a test that fails before and passes after (both runs
recorded).

- Rust unit tests: rounding; v2 encoding (quickcheck: byte order equals
  numeric order; v1 still decodes).
- `sqlite/conformance/turso-sqltests/typed_expressions.sqltest`: every row of
  the table, user types with signatures, old syntax, `EXPLAIN QUERY PLAN`.
- `tests/integration/`: a database written by today's `main` (v1 numeric data,
  an index, a user type with old OPERATOR syntax) as a fixture: reads, equality
  seeks, writes stay v1, `integrity_check`; close/reopen for v2.
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
4. Fold accumulator (a VDBE change) or only type-declared aggregates?
5. Persisted expressions whose result changes (expression indexes, generated
   columns, CHECK, PostgreSQL table SQL translated at load): require
   `REINDEX`, detect, or accept?
6. SQLite dialect converts REAL to numeric implicitly (shortest decimal text);
   PostgreSQL converts numeric to REAL. Agreed?
7. Sort comparators: keep the fixed name table (`order_by.rs`) or let the
   sorter call the type's `<` function?
8. `smallint_col = 100000` errors because ENCODE validates the seek key, as
   today. Defer?
