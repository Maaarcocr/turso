# Typed expressions

Status: Proposal 1 is implemented (see
[Implementation notes](#implementation-notes-proposal-1)); Proposals 2 to 4
are design proposals for review.

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

**9. Other comparisons ignore the type.**

```sql
CREATE TABLE t(p numeric(10,2), q numeric(10,0)) STRICT;
INSERT INTO t VALUES ('2.00', '10');
SELECT p IN (2) FROM t;                                -- Turso today: 0
                                                       -- PostgreSQL 17: true
SELECT CASE p WHEN 2 THEN 'yes' ELSE 'no' END FROM t;  -- Turso today: no
                                                       -- PostgreSQL 17: yes
SELECT nullif(p, 2) FROM t;                            -- Turso today: 2.00
                                                       -- PostgreSQL 17: NULL
SELECT max(p, q) FROM t;                               -- Turso today: 2.00
                     -- PostgreSQL 17, greatest(p, q): 10
SELECT p FROM t UNION SELECT 2 ORDER BY 1;             -- Turso today: 2, 2.00
                                                       -- PostgreSQL 17: 2.00
```

Only `+ - * / < =` written as operators use the numeric functions. IN, CASE,
`nullif`, `max` and UNION compare the text `'2.00'` with the integer 2 or 10
using SQLite's rules. The values are also not *canonical* (equal values can
look different, `'2'` and `'2.00'`), so anything that compares bytes or hashes
(UNION, DISTINCT, GROUP BY, hash joins) treats equal numbers as different.

**10. Indexes.** Stored numeric values are blobs whose byte order is not
numeric order, so an index on a numeric column is used only for `=`, never for
`<`, `>` or ORDER BY. An index on an expression stores the expression's value
as it is computed today: an index on `t(CAST(x AS numeric(10,2)))` with x =
`'1.505'` stores 1.50 (example 1), and an index on `t(p * 2)` with p = 20.50
stores the text `'41.0000'`. Any change to how those expressions are computed,
including the rounding fix, makes later UPDATEs and DELETEs look for keys that
are not in the index.

## Overview

Custom types and `tursopg` are experimental, so this design changes their
behaviour where needed and asks users to rebuild data written by older builds
(see [Upgrading](#upgrading)) instead of adding compatibility machinery. Plain
SQLite behaviour, and databases used with custom types off, do not change at
all.

Four proposals, each reviewed and merged on its own:

1. **Typed expressions.** The compiler works out the type of every expression
   and uses it for operators, comparisons, keys, index use and CAST; numeric
   values are rounded instead of cut off.
2. **Dialect-defined types.** What a frontend such as MySQL needs to put its
   own types on STRICT tables: a table marker that core writes and checks,
   type names mapped onto core storage types, assignment conversions, and
   integer rowid aliases.
3. **PostgreSQL frontend.** Exact numeric literals and casts (a separate
   PostgreSQL PR).
4. **Typed aggregates and ordered numeric storage.** SUM/AVG for types, and a
   numeric storage format whose bytes sort like the numbers.

Not proposed: custom types on non-STRICT tables, until a frontend needs them.

## Proposal 1: typed expressions

### Types

Types exist only at compile time. At run time a register (a value slot of
Turso's bytecode machine) contains an ordinary SQLite value; for an expression
of a custom type, it is the type's *in-memory form*: what DECODE returns
(numeric: text such as `'1.50'`). ENCODE runs only when a row or index entry is
written and when a *seek key* (the value looked up in an index, e.g. 1.555 in
`WHERE p = 1.555`) is built. No new kinds of runtime values, no B-tree change.

Every expression gets a type: **Named** (a registered type: built-in or made
with CREATE TYPE) or **Dynamic** (SQLite's rules: each value's storage class
decides at run time; the compiler records integer, real, text, blob or
unknown). New module `core/translate/expr/types.rs`:

```rust
pub(crate) enum ExprType { Named(NamedType), Dynamic(DynamicClass) }
pub(crate) struct NamedType { def: Arc<TypeDef>, params: Option<Vec<ast::Expr>> }
pub(crate) enum DynamicClass { Integer, Real, Text, Blob, Unknown }
pub(crate) fn expr_type(expr: &ast::Expr, tables: Option<&TableReferences>,
                        resolver: &Resolver) -> Result<ExprType>;
```

`params` is `None` when the parameters are not known statically (e.g. after a
PostgreSQL division, whose scale depends on the values).

- Column: `Named` when its declared type is registered and the table allows
  custom types, else `Dynamic` from its affinity. The type is looked up in the
  database that owns the table: each attached database loads its own types
  table (today an attached database's own types are ignored, and an INSERT
  through `ATTACH` stores values without ENCODE). This also covers columns read
  by CHECK constraints, generated columns and expression indexes, column
  references that UPDATE and UPSERT rewrite into registers (a `register_types`
  map like today's `register_affinities`), and subquery, CTE and view columns
  (the type of the inner result expression).
- `CAST(x AS T)`: see [CAST](#cast).
- Operators and functions: the result type of the chosen signature (below).
- `coalesce`, `ifnull`, `nullif`, `iif`, `CASE`, scalar `min`/`max`, and the
  columns of UNION, INTERSECT and EXCEPT: the type that all non-NULL branches
  have or convert to; each branch is converted to it. Otherwise `Dynamic`.
- Literals: the class the dialect gives them (SQLite and PostgreSQL rules:
  integer, real, blob by kind; string literals, `NULL` and `?` are Unknown and
  take the other operand's type).
- Otherwise `Dynamic`, with `infer_expression_primitive`'s classes.

A type has two parts. Its *storage* is BASE, parameters, ENCODE, DECODE and
KEY (below): it decides what is written to the file, so it is defined once in
core for built-in types and is the same in every dialect. Its *behaviour* is
operators, implicit casts, functions and parameter defaults: it comes from the
dialect's type rules for built-in types, and from the definition itself for
types made with CREATE TYPE.

### Declarations

`CREATE TYPE` gains clauses (`TypeOperator` in `sqlite/parser/src/ast.rs`,
`parse_create_type`, `TypeDefKind::Custom`, `build_create_type_sql`):

```sql
CREATE TYPE name(value any, p integer DEFAULT NULL, ...) BASE ...
OPERATOR 'op' [([left-type,] right-type)] [RETURNS type] [function]
CAST {FROM | TO} type [, ...] AS IMPLICIT
FUNCTION name(arg-type, ...) RETURNS type function
KEY expr
```

A type in a signature is a registered type, `integer`, `real`, `text`, `blob`,
or `any` (accepts any value as it is). One type in parentheses is a prefix
operator. Comparisons return a Dynamic integer. A parameter `DEFAULT` is used
when a conversion or CAST omits that parameter. `DEFAULT NULL` means "no
limit", so the type's ENCODE must accept NULL there: `numeric_encode` skips
rounding and the precision check, and varchar's ENCODE changes from
`length(value) <= maxlen` (which raises an error when maxlen is NULL) to
`maxlen IS NULL OR length(value) <= maxlen`. `KEY expr` gives the *canonical
key* of an in-memory value: bytes that are equal exactly when the values are
equal under the type's `=`, and that sort like the type's `<`.

Stored user types (canonical SQL in `__turso_internal_types`) keep their
meaning: a clause without a signature (`OPERATOR '+' f`, `OPERATOR '<'`, the
old `OPERATOR '+' (t) -> f`) means `(T, T)`, plus `(T, any)` and `(any, T)`
for arithmetic when DECODE returns the value unchanged, with a Dynamic result;
`>`, `>=`, `<=`, `!=` are still derived. Only two things change: these
operators also apply to typed sub-expressions, and a literal operand gets the
type's in-memory form, not the column's storage form.

### Dialect type rules

`Dialect::type_rules()` returns the dialect's rules; the default is the SQLite
rules. They contain:

- the behaviour part of each built-in type;
- signatures over Dynamic classes, and implicit casts between classes (MySQL:
  `integer / integer` returns DECIMAL; text compared with a number is compared
  as REAL);
- function signatures: name and argument types give the implementation and
  the result type (MySQL: `avg(integer)` returns DECIMAL);
- *result-parameter rules*: functions that compute the result's parameters
  from the operands' (MySQL `/`: scale + 4; PostgreSQL `+`: the larger scale;
  PostgreSQL `/`: not known statically), used for column metadata and keys;
- the class of each kind of literal;
- the type names of Proposal 2.

The SQLite rules have no Dynamic-class signatures, so an expression without a
Named operand compiles exactly as today. The MySQL frontend must be expressible
with these rules alone, with no MySQL code in core.

A table records the dialect that created it with today's table-SQL markers:
unmarked SQL is the SQLite dialect, `POSTGRES_TABLE_SQL_PREFIX` is PostgreSQL.
The SQLite dialect refuses tables of other dialects; `tursopg` opens unmarked
tables. Proposal 2 adds a marker that core writes and checks for dialects with
their own type names. Expressions stored with a
table (index expressions, partial-index WHERE, generated columns, CHECK,
DEFAULT) compile with the rules of the dialect that opened the database, so
writing typed expressions into one database with both `tursodb` and `tursopg`
is not supported (they can compute different index keys).

Built-in numeric under the SQLite rules, written in clause syntax. The
PostgreSQL rules differ in `CAST TO real` instead of `CAST FROM real` (so
`numeric + float8` stays `double precision`, as in PostgreSQL) and in applying
the parameter defaults to CAST as well:

```sql
-- storage (core, all dialects)
CREATE TYPE numeric(value any, precision integer, scale integer) BASE blob
  ENCODE numeric_encode(value, precision, scale) DECODE numeric_decode(value)
  KEY numeric_key(value)
-- behaviour (SQLite rules)
  -- parameter defaults precision NULL, scale NULL (conversions only)
  CAST FROM integer, real AS IMPLICIT
  OPERATOR '+' (numeric, numeric) RETURNS numeric numeric_add  -- and - * /
  OPERATOR '-' (numeric) RETURNS numeric numeric_neg
  OPERATOR '<' (numeric, numeric) numeric_lt
  OPERATOR '=' (numeric, numeric) numeric_eq
  FUNCTION abs(numeric) RETURNS numeric numeric_abs
```

The varchar-like built-ins (varchar, and the types with a length or range
parameter) get `DEFAULT NULL` parameters the same way. Built-in types with a
plain `OPERATOR '<'` also get a plain `(T, T)` `=`, so `uuid_col = 'A0EE...'`
converts the literal to a uuid first.

### Operator resolution

*Resolution* chooses the function that implements an operator. A *signature*
is one declared `(left, right)` pair. For `a OP b` (or `OP a`):

1. Candidates are OP's signatures from each Named operand's type and the
   dialect's Dynamic-class signatures. No candidate (always the case for
   Dynamic operands under the SQLite rules): SQLite's operator, bytecode
   unchanged.
2. An operand fits a slot *exactly* (same type, or same Dynamic class), as
   *any*, or by *implicit conversion*: Unknown converts to anything; X converts
   to T if T declares `CAST FROM X` or X declares `CAST TO T`. Converting into
   T needs a value for each of T's parameters (given or declared DEFAULT).
3. The candidate with the most exact fits wins, then the one with the most
   `any` fits. On a tie, prefer the one whose slot for an Unknown operand is
   the other operand's type; still tied: error `operator is not unique`.
   With the old date `+`: `date_col + ?` picks `(date, any)` (one exact, one
   any) over `(date, date)` (one exact, one conversion), so `?` still adds
   days; `t + t` picks `(T, T)` (two exact).
4. No candidate fits, but the Named operands declare `CAST TO` a Dynamic class:
   convert them and use SQLite's operator.
5. Otherwise SQLite's operator on the in-memory values (today's fallback).

Converting into T is [CAST](#cast) to T, so an operand that is not a valid T
is an error, as in PostgreSQL: `date_col = 'garbage'` now fails with `invalid
date value` (today it is false). Conversions of constants are emitted once,
before the loop. Resolution of WHERE and join terms runs during constraint
analysis and is stored with each term, so the optimizer and code generation
use the same choice. `emit_typed_operator` replaces `emit_custom_type_operator`
in `translate_binary_expr` and `condition.rs`.

### Every comparison uses the resolved operator

IN lists, `x IN (SELECT ...)` and `(a, b) IN (SELECT ...)`, row-value
comparisons such as `(a, b) < (c, d)`, `CASE x WHEN`, `IS` and `IS NOT`,
`nullif`, scalar `min`/`max`, BETWEEN (already rewritten into `<=` and `>=`),
and join conditions including USING and NATURAL resolve `=` and `<` like
binary operators. Example 9 then gives PostgreSQL's results.

### Keys for hashing, sorting and removing duplicates

Hash joins, Bloom filters, IN with an ephemeral index, DISTINCT, GROUP BY,
`count(DISTINCT)`, PARTITION BY, UNION, INTERSECT, EXCEPT, ORDER BY and
MIN/MAX on a Named value use the type's canonical key, not the in-memory form.
The site stores the key in front of the value and compares only the key, so
the output keeps the value as computed. Duplicate checks use `Found` on a
record of the key columns, as DISTINCT does today (`result_row.rs`), not
`NoConflict`, which never reports a duplicate when a key is NULL and would
emit several NULL rows. Numeric's key is the ordered encoding described in
Proposal 4; here it is only computed in memory and stored in indexes on Named
expressions.

A type without `KEY`:

- sorts use a comparator that calls the type's `<` function, replacing the
  fixed name table in `core/translate/order_by.rs`. CREATE TYPE (and prepare,
  for built-ins) rejects a `<` function that is not deterministic or needs
  the connection, so sorting never fails halfway;
- if the type declares its own `=`, the planner does not choose plans that
  hash or probe by bytes when another plan exists: no hash join (nested loop
  instead), no Bloom filter, no ephemeral index for IN (each value compared
  with `=`);
- DISTINCT, UNION and GROUP BY compare the in-memory form, as today. Every
  built-in type except numeric already has a canonical in-memory form, and
  rejecting these statements would break existing queries on user types.

### Indexes and seek keys

- An index on a typed column c is used for a term `c OP e` only if OP resolved
  to c's type's own operator, or to SQLite's operator (steps 4 and 5) when c's
  DECODE returns stored values unchanged (smallint, date, varchar). Ranges and
  ORDER BY need an ordered storage format (Proposal 4); until then only `=`.
- The seek key is e converted to c's type exactly as a table scan converts it,
  then ENCODEd with c's parameters in an *error-to-label* mode: a RAISE inside
  ENCODE, and an error returned by a function it calls (`numeric_encode` out
  of range, `validate_ipaddr`), jump to a fallback label instead of failing the
  statement. This needs an optional error target on the `Function`
  instruction. If ENCODE fails or changes the value (`1.555` into
  `numeric(5,2)` gives 1.56; `100000` does not fit), an equality seek matches
  no row, as the scan does; a range seek drops or widens that bound and keeps
  the term as a filter. An index lookup and a scan always return the same rows.
- An index on a Named expression stores the expression's canonical key, so
  `=`, ranges and ORDER BY work on it. Code generation never reads the
  expression's value back from such an index when the key loses information
  (numeric keys drop the display scale: 1.5 and 1.50), so output does not
  depend on the plan.

### CAST

`CAST(x AS T(params))` is `Named(T)` when T is registered and every parameter
is given, or (outside the SQLite dialect) has a declared default. It gives what
storing x in a STRICT column of type `T(params)` and reading it back gives:
ENCODE, the STRICT conversion to BASE (an error when it would lose data, as
for INSERT), then DECODE (Proposal 2 lets a dialect run its assignment
conversion first). Domains run today's checks between ENCODE and DECODE.
So `CAST('1.50' AS numeric(10,2))` is `1.50`, `CAST('12' AS bigint)` the
integer 12 (today the text `'12'`), `CAST(1 AS varchar(10))` the text `'1'`
(today the integer 1), and `CAST('12abc' AS bigint)` an error (SQLite's affinity
cast gives 12).

Otherwise the CAST is SQLite's *affinity cast* (the type name only chooses
INTEGER, REAL, TEXT, NUMERIC or BLOB conversion) and the result is `Dynamic`:

- T is not registered. SQLite accepts any type name in a CAST:
  `CAST(x AS SIGNED)` and `CAST(x AS FOOBAR)` are legal. An error here would
  break SQLite compatibility; a frontend that wants strict type names rejects
  unknown ones in its own parser.
- In the SQLite dialect, T is registered but a parameter is missing:
  `CAST(x AS NUMERIC)` and `CAST(x AS VARCHAR)` keep today's affinity cast (the
  "fall through to regular CAST" branch of `translate_cast_expr`), so this SQL
  keeps its SQLite meaning.

So in the SQLite dialect with custom types on, a CAST to a registered type
without parameters (`bigint`, `date`, `boolean`, `uuid`) or with all of them
(`varchar(10)`, `numeric(10,2)`) is typed, as it is today, and its result
differs from SQLite's affinity cast. With custom types off nothing changes.
INSERT and UPDATE still encode; column reads still decode
(`emit_user_facing_column_value`). The CAST section of `create-type.mdx`
changes.

### Rounding and division

`validate_precision_scale` (`core/numeric/decimal.rs`) calls `with_scale`,
which cuts off digits (example 1). It will call
`with_scale_round(scale, RoundingMode::HalfUp)`, which rounds half away from
zero as PostgreSQL and MySQL do (`-1.505` becomes `-1.51`). `numeric_div` uses
PostgreSQL's scale rule (at least 16 significant digits) instead of 100
digits; the rule belongs to the type's `/`, so MySQL's rules can choose a
function with scale + 4.

### Other query shapes

- Recursive CTEs: a column has the anchor's type; values from the recursive
  part are converted to it; if they cannot be, the column is Dynamic.
- Window functions: PARTITION BY and ORDER BY use keys; `sum(...) OVER` follows
  Proposal 4; a RANGE frame with an offset on a Named ORDER BY key is an error
  until the frame code uses the type's `+`, `-` and keys.
- Materialized views compile scalar expressions with the same translator, so
  they are typed. They compute SUM and AVG themselves, so a view with an
  aggregate over a Named value is rejected until they support Proposal 4.

### Cost

- A statement whose tables have no typed column, with no CAST to a registered
  name, under rules without Dynamic-class signatures, skips typing.
- Otherwise `expr_type` results are cached per expression tree during one
  translation, keyed by node address. This is sound because the tree is
  borrowed and unchanged during the call, and only nodes of that tree are
  cached; long `a + b + c + ...` chains stay linear.
- Result column types are stored in `Program`; `get_column_type_info` reports
  `numeric` for `p + 1` and keeps today's output for Dynamic columns.
- Each numeric operator parses and formats a decimal, because the in-memory
  form is text. Aggregates keep native accumulators (Proposal 4). A binary
  in-memory form could be added later without changing this design.

### The SQLite dialect is unchanged

Custom types off: no types are registered, so every expression is Dynamic and
step 1 always gives SQLite's operator. Custom types on: non-STRICT columns never
get a type, SQLite type names cannot be registered, the SQLite rules have no
Dynamic-class signatures, and a CAST to a registered name is already
Turso-specific. Verification, recorded in the PR:

- `CI=1 make -C sqlite/conformance run-rust CARGO_PROFILE=` (debug build): the
  427 EXPLAIN snapshots (saved bytecode listings) are unchanged.
- A script diffs `EXPLAIN` between `main` and the branch, custom types off and
  on, for every statement in `sqlite-sqltests` (must be identical),
  `turso-sqltests/custom_type_*.sqltest` and `pg-sqltests` (every difference
  explained).
- `make test`, the sqltest suites and the wire metadata tests pass.

### Tests

Each commit adds a test that fails before and passes after (both runs
recorded).

- `sqlite/conformance/turso-sqltests/typed_expressions.sqltest`: examples 1 to
  9, user types with signatures, old syntax, CAST to BASE.
- Index and scan agree: every typed comparison runs with `NOT INDEXED` and with
  `INDEXED BY` and must return the same rows (`p = 1.555`, `p < 100000`,
  `smallint_col = 100000`, an upper-case uuid literal, expression indexes).
- Duplicates and NULL: DISTINCT, UNION and GROUP BY over a typed key with
  several NULLs return one NULL row.
- Rust unit tests: resolution (`date_col + ?`, `t + t`, ties), numeric keys
  (byte order equals numeric order; quickcheck).
- `tests/integration/`: an attached database with its own types; close and
  reopen.
- `postgres/conformance/pg-sqltests/numeric.sqltest`, values checked with
  `make -C postgres/conformance run-postgres`.

### Implementation notes (Proposal 1)

Where the implementation differs from the text above:

- **Types of attached databases (commit 2) are deferred** to Proposal 2
  (commit 13). It needs the database id at about 40 call sites, and its main
  reason left with the per-database numeric format flag. Until then an
  attached database's own types are ignored and an INSERT through ATTACH
  stores values without ENCODE, as before.
- **Signatures over Dynamic classes and result-parameter rules** ([Dialect
  type rules](#dialect-type-rules)) are not implemented: no dialect in this
  repository uses them. They moved to Proposal 2 (commit 21), where the test
  dialect and later the MySQL frontend use them.
- **Custom types without `=`** (and not domains) compare with SQLite's `=` on
  the decoded values, after converting a literal or parameter to the type.
- **Conversions are emitted where the value is used**, not hoisted before the
  loop; hoisting constant conversions is a possible later optimization. A
  conversion emits the same code as a CAST, including the STRICT check of the
  encoded value and domain checks.
- **Column type metadata** is computed on demand in
  `Statement::get_column_type_info`, not stored in `Program`.
- **Types are cached during one call.** `expr_type` stores the type of every
  node it computes while one outer call is running, keyed by node address, and
  forgets them when it returns; a function's argument types and the columns of
  compound subqueries are each computed once. Typing is linear in the size of
  the expression for each call, and translation is at most quadratic in the
  nesting depth. The rule of skipping typing for statements without typed
  values is not implemented: skipping a single expression would lose the
  literal classes that resolution needs when the other operand is typed
  (`smallint_col = 100000` must compare as integers, not convert 100000), and
  skipping a whole statement needs a walk over its views and triggers.
- **CASE, coalesce, ifnull, iif, min and max** convert a branch to the common
  type only when it is a `?` parameter or its class has a declared `CAST FROM`;
  a text literal or a value of unknown type is not converted and the result
  is Dynamic, so `coalesce(p, 'none')` keeps working.
- **Sorting**: ORDER BY, GROUP BY and window PARTITION BY and ORDER BY sort
  a value whose type has a KEY by the key: the sorter stores the key in the
  sort column and the value in a separate column. min and max, and types
  without a KEY, still use the fixed table of known `<` functions in
  `order_by.rs`; the generic comparator and the CREATE TYPE check of
  decision 7 are not done.
- **Scalar subqueries and aggregate min and max** have the type of their
  value, so `(SELECT min(p) FROM t) > 9.5` and `max(p) + 1` use numeric's
  operators.
- **Domains** resolve operators, type functions and sort comparators through
  their base type: `price_col * 2` for `CREATE DOMAIN price AS numeric` calls
  `numeric_mul`.
- **IN (SELECT ...)** resolves `=` between the left value and the subquery
  column. Without a function or a conversion, values are probed as bytes, as
  for untyped values, and an index on a typed column is used when its stored
  values are read unchanged. When the type has a KEY, the subquery converts its
  values and stores their keys, and the left value is converted and keyed
  before the probe. Only a type whose `=` is a function and that has no KEY
  scans the subquery rows and compares each with `=`; for a row value, and for
  a compound subquery, that case compares bytes.
- **Hash joins** on a typed `=` hash the keys of both sides (converted as the
  operator requires); a typed `=` without a KEY is not hashed. Bloom filters
  are only used on encoded seek keys, which are already canonical.
- **UNION, INTERSECT and EXCEPT** store keys followed by the keyed values in
  the dedupe index and check duplicates with `Found` on the key prefix. VALUES
  branches get keys without a conversion; a compound that writes into an
  IN (SELECT ...) index keeps the byte comparison.
- **Expression indexes do not store keys yet.** The planner does not use an
  index on an expression with a named type (commit 5), so results stay
  correct.
- **CHECK constraints** now see decoded column values, consistent with CAST
  returning the read-back value; before they saw the stored form.
- **Windows**: a `RANGE` frame with an offset on a typed ORDER BY value is an
  error. **Materialized views** reject aggregates over typed values and
  GROUP BY on types with a KEY.
- **Recursive CTEs** convert the rows of the recursive part to the anchor's
  type when they are queued, and a recursive UNION removes rows whose keys are
  equal.
- **Numeric values** read by the numeric functions are limited to
  PostgreSQL's 131072 digits before the decimal point and 16383 after it;
  larger values and results are an `out of range` error. `numeric_key`,
  `numeric_neg` and `numeric_abs` exist only with custom types on. The other
  numeric functions can be called with custom types off, so two changes reach
  plain SQLite databases and go in the PR description: `numeric_encode` rounds
  (`numeric_decode(numeric_encode('1.505', 10, 2))` is `1.51`, before `1.50`),
  and `numeric_div` uses PostgreSQL's scale (`numeric_div('1', '3')` has 20
  digits after the point, before 100).
- **Errors of a typed CAST** are reported as datatype constraint errors
  (`invalid input for type bigint`).
- **`PRAGMA list_types`** shows parameter defaults, operator signatures, casts,
  type functions and keys.
- **Tests**: operator resolution is tested through
  `turso-sqltests/typed_expressions.sqltest` (and PostgreSQL's
  `pg-sqltests/numeric.sqltest`) rather than Rust unit tests; numeric keys
  have unit tests and a quickcheck property. The attached-database test waits
  for the deferred commit 2. The prepare-time tests nest 60 functions and 12
  compound subqueries; before the type cache each took more than a minute.

## Proposal 2: dialect-defined types

A MySQL 8.4 frontend, developed in a fork, will run on Turso. It needs its own
column types on STRICT tables: TINYINT to BIGINT, each optionally UNSIGNED;
DECIMAL(p,s); FLOAT and DOUBLE; CHAR, VARCHAR and TEXT; BINARY, VARBINARY and
BLOB; DATE, DATETIME, TIMESTAMP, TIME and YEAR; JSON; ENUM and SET; BOOL.
Ordinary application schemas must work, such as the table Laravel creates for
every model:

```sql
create table `users` (
  `id` bigint unsigned not null auto_increment primary key,
  `name` varchar(255) not null,
  `created_at` timestamp null
)
```

This proposal adds what such a frontend needs, with no MySQL code in core:

- a table marker that core writes and checks, so a table keeps its dialect
  through ALTER TABLE and VACUUM;
- a *name table* in the dialect's type rules that maps type names onto
  storage types defined in core, and new core storage types;
- assignment conversions, separate from CAST and from the conversions used in
  comparisons;
- integer primary keys that are rowids;
- declared types in column metadata, sorting by KEY, and error kinds.

The MySQL frontend lowers the table above to a Turso AST, and core stores it
as:

```sql
/* turso_dialect:mysql rowid_alias */ CREATE TABLE users (id BIGINT UNSIGNED NOT NULL PRIMARY KEY AUTOINCREMENT, name VARCHAR (255) NOT NULL, created_at TIMESTAMP) STRICT
```

`id` is the rowid. `BIGINT UNSIGNED` is stored as the core type `uint64`,
`VARCHAR(255)` as TEXT whose length is checked by MySQL's rule when a value is
stored, and `TIMESTAMP` as integer microseconds.

Scope:

- Nearly everything here applies only to *marked tables*: tables created in a
  dialect that has a name table. SQLite-dialect tables, PostgreSQL tables and
  engine tables are unchanged. Engine tables are `sqlite_sequence`,
  `__turso_internal_*` and every table that the engine creates with its own
  helper statements, such as the CDC tables (`turso_cdc`, whose name can be
  configured, and `turso_cdc_version`).
- Three changes apply to every STRICT table with custom types, because they
  fix gaps left by Proposal 1: the types of attached databases, CHECK
  constraints that compare typed values, and sorting by KEY. With custom types
  off nothing changes.
- Tests use a test dialect in core, as `core/dialect/mod.rs` does today,
  because the MySQL frontend is not in this repository.

MySQL behaviour quoted below was recorded with MySQL 8.4.10 in its default
strict mode.

### Marked tables

Today the PostgreSQL frontend stores `/* turso_frontend:postgres */` followed
by the PostgreSQL DDL, and each dialect checks markers itself. That is not
enough for a dialect whose tables are laid out differently:

- ALTER TABLE loses the marker: PostgreSQL's `format_rewritten_table_sql`
  stores plain SQLite text, so after an ALTER the SQLite dialect opens the
  table.
- Core does not know which tables are marked (`BTreeTable` has no field for
  it).
- A dialect refuses only the markers it knows: the SQLite dialect checks for
  the PostgreSQL prefix, and `tursopg` reads the text of an unknown marker as
  SQLite.

If a MySQL table lost its marker, an `INT PRIMARY KEY` would stop being the
rowid after the database is reopened, and its rows would be read with the
wrong layout.

Core now writes and checks the marker of marked tables:

- **Format.** `/* turso_dialect:NAME */ ` or
  `/* turso_dialect:NAME rowid_alias */ ` (see
  [Rowid aliases](#rowid-aliases)), followed by canonical SQLite text: the
  CREATE TABLE statement printed by core. NAME is `Dialect::name()`. The
  prefix is new because PostgreSQL's existing rows have PostgreSQL DDL after
  `turso_frontend:postgres`, which core must not read as SQLite.
- **Which tables.** A dialect opts in with `Dialect::marks_tables()`; a dialect
  with a name table must. Every CREATE TABLE in it is marked, including TEMP
  tables and CREATE TABLE AS SELECT, and must be STRICT (a non-STRICT CREATE
  TABLE is an error). Engine tables are never marked: they are recognised by
  their name and by the origin of the statement (a CREATE TABLE prepared by the
  engine itself, `StatementOrigin::InternalHelper`, is not marked).
  CREATE TABLE AS SELECT creates a marked STRICT table (commit 25).
- **Writing.** Core writes the marker on CREATE TABLE and on every rewrite of a
  marked table: RENAME TABLE, ADD, DROP and RENAME COLUMN, ALTER COLUMN, and
  bound sequence names. The dialect's `format_table_sql` and
  `format_rewritten_table_sql` are not called for marked tables.
- **Loading.** Core reads the marker of every table row before it calls a
  dialect hook. It refuses with a Corrupt error, naming the dialect, a
  `turso_dialect:` marker with another dialect's name, any marker in the
  SQLite dialect (this replaces `reject_postgres_table_sql`), and
  `turso_frontend:postgres` outside PostgreSQL. For its own marker, core strips
  it, parses the rest with core's parser, and records on `BTreeTable` that the
  table is marked and whether its primary key is the rowid.
- **Rules for the marker.** After NAME only the word `rowid_alias` is known.
  Any other word is a Corrupt error, so an older build never ignores an
  attribute that a newer build wrote. In a marked table, whether the primary
  key is the rowid is read from the marker only; `is_rowid_alias_type` is never
  consulted.
- **Stored text that runs again.** VACUUM, the sync engine and `.dump` run the
  stored text of a table as a statement. `Connection::parse_sql`, which every
  prepare and every reprepare uses, recognises a leading `turso_dialect:`
  marker: it checks the name as when loading, parses the rest with core's
  parser instead of the frontend's (which could read canonical SQLite text
  differently: MySQL reads `"x"` as a string), and CREATE TABLE takes the
  rowid decision from the marker instead of deciding again. This covers VACUUM,
  sync and dump and restore in one place. A marker before any other statement
  is an error.

### Rowid aliases

Today a column is the rowid only when its declared type is exactly `INTEGER`
and it is the only primary key column of a rowid table, without `DESC` in its
column constraint (`is_rowid_alias_type`). MySQL keys are declared `INT`,
`BIGINT UNSIGNED` and so on; as ordinary columns they need a second index and
a second lookup for every row.

**Decided once and stored.** At CREATE TABLE, the primary key of a marked table
is the rowid when SQLite's other conditions are met (one primary key column, a
rowid table, no column-level `DESC`) and its storage type is an integer type:
INTEGER, `ranged_integer` or `uint64`. Core writes the result into the marker
(`rowid_alias`) and later reads it from there, never from the name table, so a
new version of a dialect that maps a name differently cannot move existing keys
into or out of the rowid. The marker has `rowid_alias` from its first commit
(14), decided by SQLite's rule until commit 24 widens the rule, so tables
created in between keep their layout. At load, a table marked `rowid_alias`
must have one primary key column with an integer storage type, or it is a
Corrupt error. ALTER COLUMN cannot give the rowid column a storage type that is
not an integer type.

**Writing a key.** Today an explicit key goes straight to `MustBeInt`, and the
column's register in the record is NULL, so ENCODE never sees the key; a
generated key is only checked against the largest i64; and NOT NULL is not
checked for the rowid. In a marked table, INSERT, UPDATE of the key, UPSERT DO
UPDATE and REPLACE do this instead (only INSERT and REPLACE generate keys):

1. An explicit key goes through the column's assignment conversion and ENCODE,
   which checks the range, before `MustBeInt`. `2147483648` into an INT key is
   an out-of-range error, as in MySQL.
2. A key is generated only for a column declared AUTOINCREMENT (the MySQL
   frontend writes it for AUTO_INCREMENT), when the value is NULL, or 0 when
   the dialect's rules set `zero_generates_key` (MySQL does, unless
   `NO_AUTO_VALUE_ON_ZERO`). AUTOINCREMENT is allowed on the rowid column of
   any marked table, not only on `INTEGER`, and is an error on a primary key
   that is not the rowid, as in SQLite. UPDATE and UPSERT DO UPDATE never
   generate a key: MySQL's `UPDATE t SET id = 0` stores 0, and
   `UPDATE t SET id = NULL` fails with 1048.
3. Without AUTOINCREMENT, a NULL key is checked against NOT NULL like any other
   column. The MySQL frontend writes NOT NULL on primary keys, so a NULL into
   `id INT PRIMARY KEY` fails as in MySQL (error 1048). A rowid column that is
   neither NOT NULL nor AUTOINCREMENT gets a generated key, as in SQLite.
4. A generated key goes through ENCODE too: after `2147483647`, an INT key
   cannot get `2147483648`. Turso reports the error kind "no more keys", which
   a frontend can report as MySQL does: its counter stops at the largest value
   and reports a duplicate key (1062).
5. A `uint64` rowid accepts 0 to 9223372036854775807. A larger key is an
   out-of-range error, where MySQL accepts up to 18446744073709551615; this is
   a documented limit. Laravel's `BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY`
   is a rowid under these rules.

AUTOINCREMENT keeps SQLite's rules (`sqlite_sequence`; a key is never reused).
MySQL does not reuse keys either, but it does not give back the keys of a
rolled-back insert, so MySQL can leave gaps where Turso does not.

### Type names

The dialect's type rules gain a *name table*, `TypeRules::dialect_types`: a
static list of *dialect types*, each found by its declared name (`INT`,
`INT UNSIGNED`, `DECIMAL`); the declared parameters (`DECIMAL(10,2)`) are the
type's parameters. A dialect type has:

- the core storage type and its parameters (next section): `INT` is
  `ranged_integer(-2147483648, 2147483647)`, `DECIMAL(10,2)` is
  `numeric(10, 2)`;
- the behaviour, in CREATE TYPE clause syntax as in today's
  `builtin_behaviour_overrides`: ASSIGN, CAST, OPERATOR, FUNCTION and implicit
  casts;
- the declared type for column metadata: name, parameters, and flags that the
  dialect defines (MySQL: UNSIGNED, ZEROFILL, display width), so `TINYINT(1)`
  keeps its 1.

In a marked table every declared type goes through the name table:

- The dialect's names come first. Core's built-in types `date`, `time`,
  `timestamp`, `json`, `varchar`, `smallint`, `bigint`, `numeric` and
  `boolean` have PostgreSQL meanings and are not used, and neither are
  SQLite's `INT`, `INTEGER`, `TEXT`, `BLOB` and `REAL`, which core handles
  today before any lookup.
- A name that the table does not know is an error at CREATE TABLE.
- A column's affinity comes from its storage type, not from SQLite's rules on
  the declared name. Today a STRICT column declared `JSON`, `ENUM` or
  `DATETIME` gets NUMERIC affinity, which is applied when it is read.

Declared types are spelled so that SQLite's type-name grammar accepts them:
words first, then parameters (`DECIMAL UNSIGNED(10,2)`, `INT UNSIGNED(11)`).
ENUM and SET need parameter lists of any length with string values
(`ENUM('small','large')`), but `ast::TypeSize` has room for at most two
numbers. The parser gains a list form that is accepted in two places only:
when core parses a marked table's text, and in an AST that a frontend builds.
The SQLite parser keeps reporting a syntax error for `foo(1,2,3)`.

**Resolved once per database.** Today about 60 call sites look up `col.ty_str`
in the main database's type registry each time a column is used
(`get_type_def`, `resolve_type`). This is why an attached database's own types
are ignored and why an INSERT through ATTACH stores values without ENCODE
(Proposal 1's deferred commit 2). Column types are now resolved when a
database's schema is loaded or changed, from that database's types table (or
the name table, for marked tables), and stored on `Column`; call sites read the
column. This applies to every STRICT table with custom types.

### Core storage types

Storage decides what is written to the file, so it is defined once in core and
is the same in every dialect. These core types are added. They are reached only
through a dialect's name table, never by name from the SQLite dialect.

| Core type | Stored as | In memory | KEY |
|---|---|---|---|
| `ranged_integer(min, max)` | INTEGER; ENCODE refuses values outside min..max | INTEGER | the stored value |
| `uint64` | INTEGER up to 9223372036854775807, above it an 8-byte big-endian BLOB | INTEGER, or canonical TEXT above 9223372036854775807 | the stored value |
| `date_days` | INTEGER days since 1970-01-01 | `'2024-05-01'` | the stored value |
| `datetime_micros(fsp)` | INTEGER microseconds since 1970-01-01 00:00:00, without a time zone | `'2024-05-01 10:30:00.25'` for fsp 2: fsp digits after the point | the stored value |
| `time_micros(fsp)` | INTEGER microseconds, possibly negative | `'-01:00:00'`, `'100:00:00'`; hours may exceed 24 | the stored value |
| `enumeration(label, ...)` | INTEGER position of the label, from 1 | the label | the position |
| `label_set(label, ...)` | the `uint64` form of a bit mask; bit n is label n + 1 | the labels in declared order, joined by `,` | the bit mask |

- "The stored value" as KEY means the type is `ORDERED` (Proposal 4's flag,
  moved here): the stored form sorts like the values, so DISTINCT, GROUP BY
  and joins compare stored forms, and an index can be used for ranges and
  ORDER BY. `uint64` sorts correctly because a record sorts INTEGER before
  BLOB, and BLOBs byte by byte.
- For `date_days` and `datetime_micros`, the smallest i64 is reserved for the
  zero date (`'0000-00-00'`, sorted first as in MySQL), so a dialect that
  allows it later needs no new format. MySQL's default mode refuses it. Dates
  with a zero month or day (`'2024-00-10'`) cannot be stored.
- Core provides `uint64_lt`, `uint64_eq`, and `uint64_add`, `uint64_sub` and
  `uint64_mul`, which report out of range instead of wrapping (MySQL: error
  1690) and accept one signed operand (`CAST(5 AS UNSIGNED) + -3` is 2; a
  negative result is out of range); `int64_add`, `int64_sub`, `int64_mul` and
  `int64_neg`, signed 64-bit arithmetic that reports out of range where
  SQLite's operators return a REAL (MySQL BIGINT: 1690; PostgreSQL's `int8`
  needs the same); `time_micros_lt`; and the functions behind each type's
  ENCODE and DECODE. A dialect chooses which ones its operators use. These are
  core functions, not dialect functions, because they run for every row.
- `numeric(p, s)` and the four SQLite classes are used as they are.

### The MySQL types

How the MySQL frontend is expected to use this. The mapping is in the fork,
not in core; ASSIGN is the dialect's assignment conversion (next section).

| MySQL | Core storage | Done by the dialect (in ASSIGN unless noted) |
|---|---|---|
| TINYINT to BIGINT | `ranged_integer` with the type's range | accept text and decimals: `'12'` is 12, `1.5` and `'1.5'` are 2, `'12abc'` and `'abc'` are errors |
| the same, UNSIGNED | `ranged_integer(0, max)`; BIGINT UNSIGNED is `uint64` | as above |
| BOOL, BOOLEAN | as `TINYINT(1)` | written as `TINYINT(1)` when the table is created |
| DECIMAL(p,s) | `numeric(p, s)`; DECIMAL alone is (10,0) | accept text; UNSIGNED refuses negatives |
| FLOAT, DOUBLE | REAL | FLOAT rounds to 32 bits |
| CHAR(n), VARCHAR(n), TINYTEXT to LONGTEXT | TEXT | lengths in characters (CHAR, VARCHAR) or bytes (TEXT types); CHAR removes trailing spaces |
| BINARY(n), VARBINARY(n), TINYBLOB to LONGBLOB | BLOB | lengths in bytes; BINARY pads with zero bytes |
| DATE | `date_days` | parse, check the range |
| DATETIME(fsp), TIMESTAMP(fsp) | `datetime_micros(fsp)` | parse, round to fsp digits, check the range (TIMESTAMP: 1970 to 2038, UTC) |
| TIME(fsp) | `time_micros(fsp)` | parse, check the range (±838:59:59) |
| YEAR | `ranged_integer(0, 2155)` | `69` is 2069, `70` is 1970, `'0'` is 2000, `0` is 0 |
| JSON | TEXT | normalise: `{"b":1,"a":[1,2]}` is stored as `{"a": [1, 2], "b": 1}` |
| ENUM(...), SET(...) | `enumeration`, `label_set` | match labels case-insensitively |

Collations (MySQL's default `utf8mb4_0900_ai_ci` compares text
case-insensitively) are column COLLATE clauses with Turso's existing
collations; they are outside this proposal.

### Assignment, CAST and comparison conversions

Converting a value to a column's type happens in three places, and MySQL
treats each one differently:

| Value, type | Stored in a column | CAST | Compared with a column |
|---|---|---|---|
| `'12abc'`, INT | error 1265 | `CAST('12abc' AS SIGNED)` is 12, with a warning | `int_col = '1abc'` is true when int_col is 1, with a warning |
| `1.5`, INT | 2 | `CAST(1.5 AS SIGNED)` is 2 | `int_col = 1.5` is false for 1 and for 2 |
| `1000`, TINYINT | error 1264 | no such CAST | `tiny_col = 1000` is false, without an error |
| `1000`, DECIMAL(3,1) | error 1264 | `CAST(1000 AS DECIMAL(3,1))` is 99.9, with a warning | |

Turso today has one conversion for all of them, the STRICT check, and it
differs from MySQL's assignment:

| Value into column | Turso STRICT | MySQL 8.4 |
|---|---|---|
| `'12'` into INT | 12 | 12 |
| `'abc'`, `'12abc'` into INT | error | error |
| `1.5`, `'1.5'` into INT | error | 2 |
| `99999999999` into INT, `100000` into SMALLINT | accepted (INTEGER only checks the 64-bit range) | error: out of range |
| `12`, `1.5` into VARCHAR | `'12'`, `'1.5'` | `'12'`, `'1.5'` |
| `1e20` into VARCHAR | `'1.0e+20'` | `'1e20'` |

A type's behaviour therefore has three separate conversions:

- **`ASSIGN function`** runs when a value is stored in a column of the type,
  before ENCODE, and returns the in-memory form or an error. It runs where
  SQLite applies column affinity for non-STRICT tables: after the row's new
  values are computed, before BEFORE triggers, the STRICT input check
  (`TypeCheck`) and the rowid's `MustBeInt`. It is used by INSERT,
  INSERT ... SELECT, UPDATE, UPSERT DO UPDATE and REPLACE, for column DEFAULTs
  (also the default that is read for rows written before ADD COLUMN), for
  stored generated columns and for ALTER COLUMN rewrites. A virtual generated
  column's value is converted when it is read, as if it were stored and read
  back (today only its affinity is applied). ASSIGN is not called for NULL. It
  must return an in-memory value unchanged, because VACUUM and ALTER TABLE
  copy rows through it. Without ASSIGN, today's path applies: ENCODE, then the
  STRICT check.
- **`CAST function`** is what `CAST(x AS T)` calls instead of ASSIGN, before
  ENCODE and DECODE. Without it, CAST is Proposal 1's "store and read back",
  which now includes ASSIGN.
- **Implicit conversions** used by operator resolution can name their own
  function: `CAST FROM text AS IMPLICIT mysql_text_to_double`. Without one, the
  conversion is the CAST, as in Proposal 1. The MySQL rules let every integer
  type widen to the integer class, and compare integers with decimals as
  decimals, so `tiny_col = 1000` and `int_col = 1.5` never convert a value into
  the column's type.

All three receive the target type's parameters, as ENCODE does today:
DECIMAL's precision and scale, ENUM's labels, VARCHAR's length, the fsp of the
time types.

These functions may be dialect functions (`Func::Dialect`, which receive the
connection). Proposal 1's rule that a function must be deterministic and must
not need the connection applies only to `<` and KEY, which sorting calls.

MySQL's warnings, and lenient assignment (INSERT IGNORE and non-strict modes
store a clipped value with a warning instead of failing), are left for later;
conversion functions will then get a `lenient` argument and a way to add a
warning.

### Expression types and metadata

The parts of Proposal 1's type rules that only a dialect with its own types
uses are implemented here:

- **Signatures over Dynamic classes**, and implicit casts between classes:
  MySQL `integer / integer` returns DECIMAL; text compared with a number is
  compared as DOUBLE.
- **Result-parameter rules**: functions in the type rules that compute the
  result's parameters from the operands' parameters. MySQL `/` adds 4 to the
  scale (`7/2` is `3.5000`), and `VARCHAR(10)` and `VARCHAR(20)` in a UNION or
  CASE give `VARCHAR(20)`. They are used for metadata, keys and CREATE TABLE AS
  SELECT; the operator's function still computes the value. The static
  parameters reach column metadata for every expression result, not only for
  columns, because the value can have more digits than its static scale:
  MySQL keeps hidden digits in a quotient and applies the scale only when it
  shows or stores a value (`64 / 77 * 77` is `64.0000`; rounding `64 / 77` to 4
  digits first would give `64.0024`). The frontend rounds to the scale in
  `ColumnTypeInfo` when it shows a value, and ASSIGN rounds when it stores
  one.
- **Literal types**: a kind of literal can have a dialect type; MySQL `1.5` is
  `DECIMAL(2,1)`.
- **Argument types for dialect functions**: the `Function` instruction of a
  `Func::Dialect` call keeps the static type of each argument (the resolved
  type with its parameters, or the Dynamic class), and
  `Dialect::exec_scalar_function` receives them with the values. Once MySQL
  decimals are typed text, `LOCATE(..., 1.5)` must know that `'1.5'` is a
  DECIMAL to round it to 2; what a function does with the types is up to the
  dialect.

The Named type of a column of a marked table includes the declared type (name,
parameters, flags), and so does the result type of a dialect signature, with
the parameters from its rule. The declared type is kept through views,
subqueries, CTEs and compound selects, and `ColumnTypeInfo` gains its
parameters and flags, so a frontend can report `TINYINT(1)` for a BOOL column
and `DECIMAL(7,5)` for `d / 3` when `d` is `DECIMAL(3,1)`.

CREATE TABLE AS SELECT in a dialect with a name table creates a marked STRICT
table whose columns have the declared types of the result columns. A Dynamic
column gets the type that the dialect's rules name for its class (MySQL:
BIGINT, DOUBLE, LONGTEXT, LONGBLOB). In MySQL, `CREATE TABLE c AS SELECT i,
d / 3 AS q, i + 1 AS s FROM t` with `i INT, d DECIMAL(3,1)` gives `i int`,
`q decimal(7,5)` and `s bigint`. Today CREATE TABLE AS SELECT writes a
non-STRICT table without parameters.

### Sorting and index ranges

Sorting the in-memory form is wrong for TIME (as text, `'100:00:00'` sorts
before `'20:00:00'`, and `'-01:00:00'` before `'-02:00:00'`), for `uint64`
values above i64::MAX (canonical TEXT), and for ENUM, which MySQL sorts by
position (`small` before `large` for `ENUM('small','large')`).

- Proposal 1 already sorts ORDER BY, GROUP BY and window PARTITION BY and
  ORDER BY by the type's KEY when it has one, so every core storage type here
  sorts correctly there.
- min and max compare with the type's `<` through the table of known `<`
  functions in `order_by.rs`, as comparisons do. MySQL declares no `<` for
  ENUM, so min and max compare ENUM values as text, which is MySQL's rule
  (`max(e)` is `small`). The core functions `uint64_lt` and `time_micros_lt`
  are added to that table by the commits that add their types.
- A comparator that calls any `<` function (decision 7) is not needed by any
  type here and stays open.

The optimizer change of Proposal 4 that uses an index for ranges and ORDER BY
on an `ORDERED` type moves here. Without it, `WHERE created_at > ?` with an
index on `created_at` reads the whole table. An index is used for a range only
when the type's resolved `<` sorts in KEY order. That holds for the types whose
KEY is the stored value and whose `<` compares the same way (`ranged_integer`,
`uint64`, `date_days`, `datetime_micros`, `time_micros`), but not for
`enumeration` and `label_set`: their KEY is the position or the bit mask, while
MySQL's `<` compares their text. Their indexes are used for ORDER BY and `=`
only.

### CHECK constraints

Proposal 1 made CHECK see in-memory values, but CREATE TABLE still checks CHECK
expressions of STRICT tables with its own validator (`validate_check_expr`),
which refuses `CHECK (p > 0)` on a `numeric(10,2)` column: "cannot compare
numeric with INTEGER". A comparison with a Named operand is now accepted when
operator resolution finds an operator for it, as it would be in a WHERE clause.
Comparisons between SQLite classes keep today's checks, which also run with
custom types off (`CHECK (a > 'x')` on an INTEGER column is still refused).

### Errors

An error from a type function has a kind: out of range, invalid value, too
long, or no more keys (a generated rowid would leave the column's range). A
RAISE inside ENCODE or ASSIGN states its kind with the new function
`conversion_error(kind, message)`. When such an error stops an INSERT or UPDATE
of a marked table, core adds the column name and the row number, counted from
1, which MySQL's messages include ("Out of range value for column 'i' at row
1"). The frontend maps kinds to its codes (MySQL 1264, 1366, 1406; PostgreSQL
22003, 22P02, 22001). Existing messages do not change. NOT NULL failures keep
their error; MySQL reports 1364 when the column was left out and 1048 when NULL
was given, which the frontend knows when it lowers the INSERT.

### Later, outside this proposal

Neutral core features that MySQL applications need, each its own proposal:

- LAST_INSERT_ID: the first key generated by the last statement that generated
  one (`last_insert_rowid()` also counts explicit keys).
- AUTO_INCREMENT on a column that is not the rowid, such as part of a composite
  key, using core sequences.
- `ON UPDATE CURRENT_TIMESTAMP` on columns.
- The session time zone for TIMESTAMP (the target runs in UTC).
- Warnings and lenient assignment (above).
- BIT(n) (`uint64` storage); JSON comparison and sorting rules.
- Unique constraints under case-insensitive collations.
- Indexes on expressions of these types: the planner ignores them, as in
  Proposal 1, so results are correct but such an index is not used.
- Keys above 9223372036854775807 in a `uint64` rowid; dates with a zero month
  or day.
- Rounding typed values to their static parameters where they become keys of
  GROUP BY and DISTINCT (MySQL rounds a quotient's hidden digits there).
- Table attributes that a frontend does not lower (MySQL `COMMENT`, `ENGINE`,
  `CHARSET`) cannot be kept in canonical SQLite text; `SHOW CREATE TABLE` and
  ORM introspection need a place to keep them.

### Tests (Proposal 2)

A test dialect in core (`core/dialect/mod.rs` tests and `tests/integration/`)
has a name table with one name for each core storage type, ASSIGN functions
with MySQL-like rules, and `zero_generates_key`. Each commit's test runs
through it. Changes to stored data (marker, rowid alias, name table, storage
types) are tested after closing and reopening the database, after ALTER TABLE
and after VACUUM. For each commit, `EXPLAIN` of every statement in
`sqlite-sqltests` is compared with custom types off, as for Proposal 1.

### Implementation notes (Proposal 2)

- **Types of attached databases (commit 13).** Each column keeps the type that
  its own database resolved when the schema was loaded, and INSERT, UPDATE,
  SELECT, indexes and ALTER TABLE encode and decode with it. Typed expressions
  (operators, keys and conversions) still look types up by name in the main
  database, because operator signatures and type functions name their types.
  So a column of an attached database is typed in expressions only when the
  main database defines a type of the same name the same way (the same CREATE
  TYPE text, also for each base type of a domain). Otherwise the column is
  untyped in expressions: its decoded values are compared and sorted with
  SQLite's rules, and a main type of the same name is never used for it.
- **Index ranges and ORDER BY (commit 26).** Only columns whose type is
  built on a core storage type (`TypeDef::storage`) are affected; other typed
  columns, including every PostgreSQL type, keep the old rules. A range term
  uses an index when its `<` resolves to the column type's own `<`, or to a
  class signature that converts the column only into the class its storage
  type already stores, and when that `<` follows the order of the stored
  values: no function for the integer, real, text and blob storages and for
  `date_days` (its in-memory text is fixed width), and `datetime_micros_lt`,
  `time_micros_lt` and `uint64_lt` for the other three. `datetime_micros_lt`
  is a new core function and sort comparator, because comparing date-time
  texts with different numbers of fraction digits is not time order. The
  bound of a range seek is converted as the comparison converts it and then
  goes through the type's KEY when it has one (the storage types whose KEY is
  the stored value), or is used as converted (storages that store values
  unchanged); no ENCODE runs, so a bound outside the column's range is not an
  error and does not end the scan early. An index gives ORDER BY when the KEY
  is the stored value or the type stores values unchanged and its `<` has no
  function; for MIN and MAX the `<` must follow the stored order, so an
  `enumeration` index serves ORDER BY and `=` but not ranges, MIN or MAX. An
  equality on a column converted to the class it stores (`int_col = 5` through
  the integer class signature) now uses the index too.
- **Declared types and CREATE TABLE AS SELECT (commit 25).** Column metadata
  already reported the declared name and parameters of a typed result through
  views, subqueries, CTEs and compound selects (commit 21's typed results); a
  test now covers it. In a marking dialect, CREATE TABLE AS SELECT creates a
  marked STRICT table. A result column of a type gets that type with its
  parameters; a column of a Dynamic class gets the name that
  `TypeRules::class_column_types` gives the class; a column of unknown class
  (NULL, `?`, a function without a typed result, or a compound select whose
  arms differ) is an error that asks for a CAST. The rows go through ASSIGN,
  the input check, ENCODE and the STRICT check, as INSERT does. In a compound
  select the class is used only when every arm has the same class.
- **Integer keys (commit 24).** At CREATE TABLE in a marking dialect, core
  parses the table once with every single primary key column as the rowid
  candidate (`RowidDecision::AnyDeclaredType`, which keeps SQLite's other
  conditions: one key column, a rowid table, no column-level `DESC`), and the
  key is the rowid when the name table maps its declared type directly to an
  integer storage type (`TypeDef::storage`; a domain over one does not count).
  A statement that carries its own marker keeps that decision, but
  `rowid_alias` on a key of another type is an error. At load,
  `rowid_alias` on a key whose declared type is not an integer storage type is
  a Corrupt error. In a marked table the key goes through ASSIGN before BEFORE
  triggers, then ENCODE; a BLOB (a `uint64` above 9223372036854775807) is an
  out-of-range error, and every other value goes on to `MustBeInt`;
  UPDATE and UPSERT DO UPDATE do the same and check NOT NULL. INSERT
  generates a key for NULL or a missing key only when the table has
  AUTOINCREMENT or the key is not NOT NULL, and for 0 when AUTOINCREMENT and
  `TypeRules::zero_generates_key` are both set and the INSERT names the key
  column (not `rowid`, which VACUUM uses to copy rows); otherwise a NULL key fails NOT
  NULL (OR IGNORE skips the row). A generated key larger than the type's
  largest value fails with "no more keys". ALTER COLUMN of the key of a marked
  table stays refused, because ALTER COLUMN loses the values of an INTEGER
  PRIMARY KEY column in every dialect (a separate bug).
- **Assignment conversions (commit 23).** CREATE TYPE takes `ASSIGN expr`,
  `CAST expr` and a function name after `CAST FROM ... AS IMPLICIT`, also for
  user types; the stored CREATE TYPE text keeps them. ASSIGN runs where SQLite
  applies affinity for non-STRICT tables (INSERT before BEFORE triggers,
  UPDATE, UPSERT DO UPDATE, ALTER COLUMN rewrites), for DEFAULTs (also the
  default read for rows written before ADD COLUMN) and for virtual generated
  columns, everywhere their value is computed (reads, index keys, the INSERT
  check). A virtual generated column of a type without ASSIGN keeps today's
  affinity, so PostgreSQL types are unchanged. CAST uses the type's CAST
  expression, or else its ASSIGN, then ENCODE and DECODE. An implicit
  conversion function receives the value and every declared parameter of the
  target type: the other operand's parameters when it has the same type,
  otherwise the defaults (NULL without a default). Values of unknown class
  (string literals in the SQLite dialect, `?`) still take the other operand's
  type without the function. In a marked table, ADD COLUMN does not apply
  SQLite's check of the DEFAULT against the names INT, REAL, TEXT and BLOB;
  when the table has rows it converts the DEFAULT into the column's type
  once, so an invalid DEFAULT fails the ALTER TABLE instead of every later
  read. Turso has no stored generated columns. In INSERT, a virtual generated column is computed
  from the already-encoded values of the other columns, so one that reads an
  encoded column (`numeric`, `uint64`) fails the STRICT check; this is older
  than this commit and not fixed here.
- **Error kinds (commit 22).** A conversion error is
  `LimboError::TypeConversion` with a `ConversionErrorKind`, the message, and
  the column and row when known; its text is the message, followed by the
  column and row in parentheses. The core storage functions report their
  kinds; `conversion_error(kind, message)` lets ENCODE and ASSIGN expressions
  report one (ranged integers use it). For INSERT and UPDATE of a marked
  table, a `TypeConversionContext` instruction before each column's ENCODE
  names the column and a counter register counts rows from 1, so the error
  gets both; other statements and unmarked tables emit nothing new. The C API
  reports these errors as SQLITE_CONSTRAINT.
- **Argument types of dialect functions (after commit 21).** When core
  compiles a call to a `Func::Dialect` function it records each argument's
  static type (`ArgumentType`: a class, or a type name with its parameters as
  SQL text) in a `Func::TypedDialect` call, and runs it through
  `Dialect::exec_typed_scalar_function`, which by default calls
  `exec_scalar_function`, so existing dialects do not change.
- **Class signatures, literal types and result parameters (commit 21).**
  `TypeRules` gains `class_operators` (OPERATOR clauses over classes and
  types, in CREATE TYPE clause syntax, parsed once per schema),
  `class_conversions` (implicit conversions between classes, through a named
  function or SQLite's CAST), `decimal_literal_type` (a literal with a decimal
  point and no exponent is a value of this type, with precision and scale from
  its digits, and is emitted as its text), `string_literals_are_text` (string
  literals are the Text class instead of Unknown) and `result_parameters` (a
  function from an operator and its operands' integer parameters to the
  result's parameters). A class signature without a function, conversion or
  typed result is SQLite's operator and does not change the bytecode. A
  dialect that declares class conversions must also declare the same-class
  operators it wants kept (`OPERATOR '=' (text, text)`), or two texts compare
  through the conversion. A unary operator on a typed value that declares
  `CAST TO` a class uses the class's unary signature. Engine helper statements
  compile with SQLite's rules (their resolver uses the SQLite dialect); the
  optimizer's checks use the database's rules. `ColumnTypeInfo` gains
  `parameters`. `int64_add`, `int64_sub`, `int64_mul` and `int64_neg` are
  core functions.
- **Enumeration and label-set storage (commit 19).** The storage type
  declares one parameter, `labels`; a type built on it accepts any number of
  column parameters (at least one), and core passes them to ENCODE, DECODE and
  KEY as one text value, the labels as SQL string literals joined by commas
  (`'small','large'`). `enumeration_encode`/`_decode` and
  `label_set_encode`/`_decode` are in `core/dialect/label_storage.rs`; ENCODE
  matches labels exactly (matching without case is ASSIGN's job). A label set
  has at most 64 labels; its mask is stored in the `uint64` form.
- **Type parameter lists (commit 18).** `ast::TypeSize::List` holds a
  parameter list with a text value or more than two values. Only a parser made
  with `Parser::with_type_parameter_lists()` reads it; core uses one for the
  text after a table marker (load, ALTER TABLE, VACUUM and statements with a
  marker). One or two numbers still give `MaxSize` and `TypeSize`.
- **Date and time storage (commit 17).** `date_days_encode`/`_decode`,
  `datetime_micros_encode`/`_decode` and `time_micros_encode`/`_decode` (the
  last two take the fsp) and `time_micros_lt` are in
  `core/dialect/time_storage.rs`. ENCODE reads the canonical in-memory text
  only (`YYYY-MM-DD`, `YYYY-MM-DD HH:MM:SS[.f]`, `[-]H:MM:SS[.f]` with any
  number of hour digits up to 9), years 0000 to 9999, and rounds the fraction
  to fsp digits half away from zero; converting other input is ASSIGN's job.
  The storage type adds the parameter `fsp integer DEFAULT 0` to date-time and
  time types. KEY encodes with 6 digits, so it equals the stored value.
  DECODE refuses a stored value with more fraction digits than the column's
  fsp, instead of dropping digits.
- **Unsigned 64-bit storage (commit 16).** `uint64_encode`, `uint64_decode`,
  `uint64_lt`, `uint64_eq`, `uint64_add`, `uint64_sub` and `uint64_mul` are
  in `core/dialect/storage_functions.rs`; the KEY of `uint64` is
  `uint64_encode`. With custom types off they are unknown functions, and
  `PRAGMA function_list` does not list them. A `uint64` value is stored as an
  INTEGER or a BLOB, so CREATE TYPE now accepts `BASE any` (also for user
  types). The comparators used by sorting cannot return errors, so `uint64_lt`
  in a sort compares values that are not unsigned integers by SQLite's rules,
  as `numeric_lt` does.
- **Name table (commit 15).** Core builds a CREATE TYPE statement for each
  dialect type from its storage type, its extra parameters and its behaviour,
  and registers it in every schema of the dialect instead of core's built-in
  types, when the schema is created. So every lookup by name (column types,
  CAST, operator signatures, type functions) finds the dialect's types, and
  in marked tables `INT`, `INTEGER`, `REAL`, `TEXT`, `BLOB` and `ANY` are
  looked up like any other name. The BASE of a built-in or dialect type is
  always a storage class, never another type, so a dialect type may be named
  `integer` and have BASE `integer`. Like every custom type, a dialect type
  is sorted and compared only through its own `OPERATOR '<'` and `'='`.
- **Marker (commit 14).** `core/dialect/marker.rs` reads and writes the
  marker; schema loading, ALTER TABLE, VACUUM and `Connection::parse_sql` go
  through it, and the table keeps it in `BTreeTable::dialect_marker`. The
  check that refuses PostgreSQL tables outside PostgreSQL moved there from the
  SQLite dialect. ALTER COLUMN of the rowid column of a marked table is refused:
  Turso's ALTER COLUMN removes PRIMARY KEY from the column, which would leave
  a `rowid_alias` marker on a table without a key. (In every
  dialect, ALTER COLUMN of an INTEGER PRIMARY KEY column also loses the
  column's values; that is a separate bug.)

## Proposal 3: PostgreSQL frontend

A separate PostgreSQL PR. `postgres/parser/translator.rs` emits
`CAST('1.555' AS numeric)` for decimal literals (`translate_const`) and typed
casts for `x::numeric` and `x::numeric(p,s)`. PostgreSQL's numeric parameters
default to NULL (unconstrained). Bare `NUMERIC` columns stay REAL, so existing
tables keep their storage.

The same PR keeps the marker of a rewritten PostgreSQL table. Today ALTER
TABLE stores it as plain SQLite text, so the SQLite dialect opens it
afterwards. Rewritten tables will use the marker of Proposal 2
(`/* turso_dialect:postgres */` with canonical SQLite text and today's rowid
rule recorded in it); tables never rewritten keep their PostgreSQL DDL.

### Implementation notes (Proposal 3)

- **Literals and casts (commit 27).** Decimal literals and integer
  literals too large for bigint become `CAST('<text>' AS numeric)`;
  integer literals that fit bigint stay integers, because pg_query
  reports every integer outside int4 as a float constant. `::numeric(p)`
  has scale 0. The fraction of `percentile_cont` and `percentile_disc`
  WITHIN GROUP is cast to double precision, the type of that parameter in
  PostgreSQL.
- **Marker of rewritten tables (commit 28).** `Dialect::marks_rewritten_tables`
  (by default `marks_tables`) lets a dialect mark only the tables that core
  rewrites. PostgreSQL turns it on: ALTER TABLE (ADD, DROP and RENAME
  COLUMN, ALTER COLUMN, RENAME TABLE and the foreign keys that a rename
  changes) and CREATE TABLE with bound sequence names store
  `/* turso_dialect:postgres [rowid_alias] */` with canonical SQLite text,
  where `rowid_alias` is SQLite's rule applied to the rewritten table.
  Statements carrying that marker (VACUUM, dump) are accepted again with
  the same decision. The behaviours that exist for dialects with a name
  table (every type name looked up in it, ASSIGN of DEFAULTs in ADD
  COLUMN, conversion of keys, key generation, the column and row of
  conversion errors) depend on the dialect having one
  (`Schema::uses_name_table`), not on the marker alone, so PostgreSQL
  tables behave the same before and after a rewrite. In a dialect without
  a name table the key of a `rowid_alias` table must be declared exactly
  INTEGER, as in SQLite. ALTER COLUMN of the key of a marked table is
  refused in every dialect.

## Proposal 4: typed aggregates and ordered numeric storage

### Implementation notes (Proposal 4)

- **SUM and AVG (commit 29).** `numeric_sum` is a core aggregate (exact
  decimal addition, with an inverse step so sliding window frames work; it
  needs custom types). A type declares it with `FUNCTION sum(T) RETURNS R
  numeric_sum`; numeric does, in core and in PostgreSQL. When an expression
  is bound, `sum(x)` of such a type becomes `numeric_sum(x)` and `avg(x)`
  becomes `numeric_sum(x) / count(x)`, keeping DISTINCT, FILTER and OVER, so
  the division is the type's `/` and the result's static parameters come from
  the dialect's result rule (`"sum"` gives the sum's parameters, then `/` its
  own). The column name stays the one written. Types without a declared sum,
  and all queries with custom types off, are unchanged.
- **Ordered numeric storage (commit 30).** `numeric_encode` writes the v2
  bytes (the same bytes as `numeric_key`), and `numeric_decode(value[,
  scale])` reads v1 and v2; for v2 it applies the declared scale, or drops
  trailing zeros when the scale is NULL. The built-in numeric type and the
  `StorageType::Numeric` dialect types decode with `numeric_decode(value,
  scale)`. The built-in numeric type now has `TypeDef.storage =
  Some(StorageType::Numeric)`, and `StorageType::Numeric` is a stored-key
  type whose `<` (`numeric_lt`) follows the index order, so ranges, ORDER BY
  and min/max on an index of a numeric column use the index. The PostgreSQL
  frontend gives a decimal literal the precision and scale of its digits
  (`2.50` is `CAST('2.50' AS numeric(3,2))`), so `2.50 * 2` is `5.00` as in
  PostgreSQL. An explicit cast to an unconstrained numeric
  (`'1.50'::numeric`) reads back as `1.5`; PostgreSQL keeps `1.50`.
  Materialized views read the stored value of a typed column without
  DECODE (this was already true before), so expressions over a numeric
  column in a view see the value without its declared scale.

**Aggregates.** Only type-declared aggregates: `FUNCTION sum(numeric) RETURNS
numeric numeric_sum` and `avg`, with native exact accumulators. A type without
one keeps SQLite's aggregate on the in-memory values, as today. `TOTAL` stays
REAL. `sum(...) OVER` uses the same functions. AVG of a type that declares
`sum` is SUM divided by COUNT with the dialect's `/`, so MySQL's AVG gets the
scale + 4 of its division (`AVG` of `DECIMAL(10,2)` values has scale 6).

**Ordered numeric storage (v2).** Today's format (v1) is a version byte 0x01,
a sign byte, the scale, and the digits as little-endian 32-bit words, so
comparing two stored blobs byte by byte does not compare the numbers. v2 is
the numeric key of Proposal 1:

- byte 0: 0x08 −Infinity, 0x10 negative, 0x20 zero, 0x30 positive,
  0x38 +Infinity, 0x40 NaN (infinities and NaN reserved; never 0x01);
- the exponent E (value = 0.d1d2… × 10^E, d1 ≠ 0) as 4 bytes big-endian with
  the sign bit flipped; encoding asserts that E fits in an i32;
- the digits without trailing zeros, in base-100 pairs (byte = pair + 1);
- negative values invert the exponent bytes, use byte = 100 − pair, and end
  with 0xFF.

Byte order equals numeric order and equal values have equal bytes. All new
numeric values are written in v2; there is no per-database format flag.
Reading still accepts v1 (the decoder already exists and the first byte tells
the formats apart), but indexes need rebuilding (see [Upgrading](#upgrading)).
DECODE becomes `numeric_decode(value, scale)`; unconstrained values lose their
display scale (decision 2). Numeric is then declared `ORDERED` (Proposal 2):
its stored form is its canonical key, so the optimizer allows ranges and ORDER
BY on an index of a numeric column, and seek keys follow the rule of
Proposal 1.

## Upgrading

Release note for databases written by older builds with custom types on (or
by `tursopg`); databases used with custom types off are not affected:

> After upgrading, run `REINDEX` on every expression index, partial index and
> index on a virtual generated column whose expression uses a custom type, and
> drop and create again materialized views that use custom types: their
> stored keys were computed with the old rules, so lookups can miss rows and
> UPDATE or DELETE can fail to find the old entry (example 10). Numeric values
> written by older builds are still read correctly, but indexes on numeric
> columns sort them in the old format: dump and reload the database to rewrite
> them. Writing typed expressions into one database with both `tursodb` and
> `tursopg` is not supported.

### Trade-off to raise in the PR description

The change does not detect objects built by older builds; it relies on the
release note. Detection would need all of: a version row in
`__turso_internal_types`, finding stale indexes and views at load, making the
planner ignore them and refusing writes to their tables until REINDEX (design
v2 of this document, commit 86915929). We left it out because the case needs
an experimental feature, a typed expression in a stored index or view, and a
database written by an older build, and because the detection is a lot of
core code. If maintainers want it, it can be added before the typing commits
without changing the rest of the design.

## Commit plan

Each commit has its own test that fails before it and passes after it.

Proposal 1, as implemented (commit 2 became commit 13 of Proposal 2, commits
8 and 11 were split, and fixes were added after the code review; see
[Implementation notes](#implementation-notes-proposal-1)):
1. `core/numeric: round half away from zero when applying a scale`
2. (deferred) `core/schema: look up column types in the database that owns the
   table`
3. `core/types: compute expression types and report them in column metadata`
4. `parser: parse operator signatures, implicit casts, type functions, keys and
   parameter defaults`
5. `core/types: resolve operators with dialect type rules and choose seek keys
   from the result`
6. `core/numeric: use PostgreSQL's scale rule for division`
7. `core/types: use resolved operators in IN, row values, CASE, IS, nullif,
   min/max and joins`
   - `core/types: compute operand types once per operator`
8. keys and comparators, in five commits:
   - `core/types: sort typed expressions by their type's order`
   - `core/types: compare DISTINCT values by the type's key` (adds
     `numeric_key`)
   - `core/types: group typed values by the type's key`
   - `core/types: hash joins and IN (SELECT ...) compare typed values with the
     type's '='`
   - `core/types: remove duplicates in UNION, INTERSECT and EXCEPT by the
     type's key`
9. `core/types: unary operators and type functions`
10. `core/types: CAST returns the value read back from a STRICT column`
11. in three commits:
    - `core/types: recursive CTE columns have the anchor's type`
    - `core/types: window partitions and peer groups compare typed values by
      key`
    - `core/incremental: reject materialized views that aggregate typed values`

After the code review of Proposal 1:
- `core/numeric: limit numeric values and return errors instead of panicking`
- `core/types: require custom types for numeric_key, numeric_neg and
  numeric_abs`
- `core/types: report typed CAST errors as datatype errors and keep outer
  value bindings`
- `core/types: compute each expression's type once per call`
- `core/types: type scalar subqueries, aggregate min and max, domains,
  recursive CTE rows and IN (SELECT ...)`
- `core/types: sort ORDER BY, GROUP BY and window partitions by the type's
  key`
- `core: show every CREATE TYPE clause in PRAGMA list_types`

Proposal 2, with the test that fails before each commit. Commits 14 to 26 are
tested with the test dialect described in [Tests](#tests-proposal-2); commits
12 and 13 use the SQLite dialect with custom types on. The first commit is a
SQLite compatibility fix that can go upstream alone:

- `core/translate: reject AUTOINCREMENT on a primary key that is not the
  rowid` (Fixes #6940). `CREATE TABLE t(id INT PRIMARY KEY AUTOINCREMENT)` is
  refused as in SQLite; a database where an older Turso created such a table
  still opens.

12. `core/schema: accept CHECK comparisons that operator resolution accepts`.
    `CHECK (p > 0)` on `numeric(10,2)` is accepted and refuses `-1`;
    `CHECK (a > 'x')` on INTEGER is still refused.
13. `core/schema: resolve column types once per database when the schema
    loads` (commit 2 of Proposal 1). An attached database with its own CREATE
    TYPE: an INSERT through ATTACH encodes and a SELECT decodes, also after
    reopening.
14. `core/dialect: write and check the marker of dialect tables in core`. The
    marker, with `rowid_alias` decided by SQLite's rule, is kept after CREATE
    TABLE, each kind of ALTER TABLE, VACUUM, and closing and reopening; the
    SQLite dialect and a second test dialect refuse the table; an unknown
    word in the marker is a Corrupt error; a statement with the marker is
    parsed by core and keeps the marker's rowid decision; engine tables
    (also CDC tables, by statement origin) are not marked; a non-STRICT
    CREATE TABLE and CREATE TABLE AS SELECT are refused.
15. `core/types: map dialect type names onto core storage types` (name table,
    `ranged_integer`, affinity from storage, STRICT required, declared type in
    Named). A column of a range -128..127 refuses 200; a column declared
    `date` uses the dialect's type, not the built-in one; both after
    reopening.
16. `core/types: unsigned 64-bit storage`. 18446744073709551615 and
    9223372036854775808 are stored, read, sorted, deduplicated and found
    through an index; `uint64_add` reports out of range and accepts one
    signed operand; min and max use `uint64_lt`. The tests write
    `'18446744073709551615'` as text, because the SQLite parser reads that
    literal as a REAL.
17. `core/types: date, date-time and time storage`. Values with fraction
    digits are stored and read back; the zero date's reserved value
    (unit test of ENCODE and DECODE).
18. `parser: type parameter lists of any length`. Parser unit test: the list
    form parses where it is allowed, and `foo(1,2,3)` stays a syntax error in
    the SQLite parser.
19. `core/types: enumeration and label-set storage`. Labels are stored as
    positions and bit masks, read back as text, after reopening and VACUUM.
20. (done in Proposal 1 after its code review: ORDER BY, GROUP BY and window
    partitions sort by the type's key; commits 16, 17 and 19 add their `<`
    functions for min and max, with tests that `'100:00:00'`, `'20:00:00'`,
    `'-01:00:00'`, `'-02:00:00'` sort as times, enumerations sort by
    position, and min and max of an enumeration compare text.)
21. `core/types: Dynamic-class signatures, result-parameter rules and literal
    types` (moved from Proposal 1). `integer / integer` returns a decimal
    with scale + 4, in values and in column metadata; `tiny_col = 1000` is
    false without an error; `int_col = 1.5` is false; an implicit conversion
    calls its own function; the static scale of `64 / 77 * 77` reaches
    column metadata while the value keeps its digits. Adds `int64_add`,
    `int64_sub`, `int64_mul` and `int64_neg`: `9223372036854775807 + 1`
    through a signature that uses `int64_add` is an out-of-range error.
    - `core/types: dialect functions receive the static types of their
      arguments`. A test dialect function reports the type and parameters of
      a typed argument and of a literal.
22. `core/types: conversion error kinds with column and row`. A multi-row
    INSERT reports the kind, the column and row 2; a generated key past the
    range reports "no more keys".
23. `core/types: dialect assignment conversions` (ASSIGN, the CAST function,
    virtual generated columns). `1.5` into a 32-bit column stores 2, `'12abc'`
    fails with invalid value, a BEFORE trigger sees the converted value, a
    virtual generated column is converted, and CAST calls the CAST function;
    ASSIGN, CAST and implicit conversion functions receive the target type's
    parameters.
24. `core/schema: integer primary keys of marked tables are rowids`. Key
    conversion; generation only with AUTOINCREMENT and only by INSERT and
    REPLACE, also for 0 with `zero_generates_key` (UPDATE stores 0 and
    refuses NULL); NOT NULL; the range of generated keys; the `uint64`
    limit; `rowid_alias` in the marker; after reopening, ALTER TABLE and
    VACUUM; a table marked `rowid_alias` whose key no longer maps to an
    integer type is a Corrupt error.
25. `core/types: declared types in column metadata, views and CREATE TABLE AS
    SELECT`. `ColumnTypeInfo` reports declared names and parameters through
    a view and a UNION; CREATE TABLE AS SELECT creates a marked STRICT table
    with declared types.
26. `core/optimizer: index ranges and ORDER BY for ORDERED types` (moved from
    Proposal 4). A range on an indexed `datetime_micros` column uses the
    index, and the index and a scan return the same rows; a range on an
    indexed `enumeration` column does not use the index, ORDER BY and `=`
    do.

Proposal 3:
27. `postgres: exact numeric literals and casts`
28. `postgres: keep the marker of rewritten tables`

Proposal 4:
29. `core/types: type-declared SUM and AVG` (AVG is SUM / COUNT with the
    dialect's `/`)
30. `core/numeric: store numeric values in the ordered format`

## Decisions

1. Numeric storage: new values are always written in v2; no per-database
   flag; v1 stays readable; users dump and reload old numeric data.
2. v2 drops the display scale of unconstrained numeric values; values are
   never read back from expression indexes whose keys lose it.
3. Division: PostgreSQL's scale rule, part of the type's `/` (MySQL: scale + 4).
4. Aggregates: type-declared only.
5. Objects built with older rules: no detection; release note and the PR
   trade-off section above.
6. REAL and numeric: each dialect's `CAST FROM`/`CAST TO` declarations.
7. Sorting: ORDER BY, GROUP BY and window partitions and peers by canonical
   key; min and max by the type's `<`, through the table of core `<` functions
   in `order_by.rs`. A comparator that calls any `<` function, checked when
   the type is created, is left open until a type needs it.
8. `smallint_col = 100000`: fixed by the error-to-label seek-key rule.
9. Missing CAST parameters: declared parameter DEFAULTs (PostgreSQL numeric
   NULL, MySQL DECIMAL (10,0)). The SQLite dialect keeps the affinity cast when
   a registered type's parameter is missing; other dialects report an error
   for a missing parameter without a default.
10. A database written with typed expressions by two dialects is unsupported;
    no mechanism compiles stored expressions with the creating dialect's rules.
11. Types without `KEY`: no hash join, Bloom filter or IN ephemeral index when
    they declare `=`; DISTINCT, UNION and GROUP BY keep comparing the in-memory
    form.
12. Unsigned 64-bit values are always exact, never REAL: the in-memory form is
    an INTEGER when the value fits in an i64, otherwise canonical TEXT. They
    are stored as INTEGER up to 9223372036854775807 and as an 8-byte
    big-endian BLOB above it, which sorts correctly. As a rowid they are
    limited to 0..9223372036854775807.
13. Time storage: dates are integer days, date-times and times integer
    microseconds; the in-memory form is canonical text; the smallest i64 is
    reserved for the zero date.
14. Table marker: core writes and checks `/* turso_dialect:NAME */` followed by
    canonical SQLite text. It is a new prefix so that PostgreSQL's existing
    rows are never read as SQLite; every dialect refuses another dialect's
    marker.
15. Rowids in marked tables: decided at CREATE TABLE and recorded in the
    marker; keys are converted and range-checked; keys are generated only
    with AUTOINCREMENT.
16. Type names in marked tables: the dialect's name table first; unknown names
    are errors. Column types are resolved once per database when the schema
    loads.
17. Conversions: ASSIGN, CAST and implicit conversions are separate functions;
    warnings and lenient assignment come later.
18. Core storage types are reached only through a dialect's name table.
