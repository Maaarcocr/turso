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
   own types on STRICT tables: type names mapped onto core storage types,
   assignment conversions, and integer rowid aliases.
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
unmarked SQL is the SQLite dialect, `POSTGRES_TABLE_SQL_PREFIX` is PostgreSQL,
and the MySQL frontend will add its own. The SQLite dialect refuses tables of
other dialects; `tursopg` opens unmarked tables. Expressions stored with a
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

- **Types of attached databases (commit 2) are deferred** to Proposal 2. It
  needs the database id at about 40 call sites, and its main reason left with
  the per-database numeric format flag. Until then an attached database's own
  types are ignored and an INSERT through ATTACH stores values without ENCODE,
  as before.
- **Custom types without `=`** (and not domains) compare with SQLite's `=` on
  the decoded values, after converting a literal or parameter to the type.
- **Conversions are emitted where the value is used**, not hoisted before the
  loop; hoisting constant conversions is a possible later optimization.
- **Column type metadata** is computed on demand in
  `Statement::get_column_type_info`, not stored in `Program`.
- **No per-expression type cache.** Each operator computes its operand types
  once (the first version computed them twice per level, which made long
  `a + b + ...` chains exponential); translation is quadratic in the depth of
  a chain, which the parser limits to 100.
- **CASE, coalesce, ifnull, iif, min and max** convert a branch to the common
  type only when it is a `?` parameter or its class has a declared `CAST FROM`;
  a text literal or a value of unknown type is not converted and the result
  is Dynamic, so `coalesce(p, 'none')` keeps working.
- **Sorting** takes the comparator from the expression's type but still uses
  the fixed table of known `<` functions in `order_by.rs`; the generic
  comparator and the CREATE TYPE check of decision 7 are not done.
- **IN (SELECT ...)** with a typed left value scans the subquery's rows and
  compares each with the type's `=`, instead of probing keys stored in the
  ephemeral index; on a typed column it is not used as an index seek. Row
  values on the left, and an untyped left value with a typed subquery column,
  keep the byte lookup.
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
- **Recursive CTEs** convert the recursive part with a CAST to the anchor's
  type, so a type with more than two parameters (not expressible as a CAST)
  leaves the column untyped.
- **Tests**: operator resolution is tested through
  `turso-sqltests/typed_expressions.sqltest` (and PostgreSQL's
  `pg-sqltests/numeric.sqltest`) rather than Rust unit tests; numeric keys
  have unit tests and a quickcheck property. The attached-database test waits
  for the deferred commit 2.

## Proposal 2: dialect-defined types

The MySQL frontend will create STRICT tables and needs three things that are
not MySQL-specific. All three apply only to tables marked by a dialect other
than SQLite, so SQLite-dialect tables are unchanged.

**Type names mapped onto core storage types.** The dialect's type rules map
its type names onto neutral storage types in core, for example a ranged
integer (`INT` is `ranged_integer(-2147483648, 2147483647)`), an unsigned
64-bit integer, numeric (`DECIMAL(p,s)`), and an ordered time or interval
form. The column keeps the declared name (`INT`) for column metadata, while
storage, KEY and behaviour come from the mapped type. The table has the
dialect's table-SQL marker, so the SQLite dialect refuses it. Unsigned 64-bit
values are always exact, never REAL: the in-memory form is an INTEGER when
the value fits in an i64 and canonical TEXT otherwise, with an ordered KEY
(the storage format is chosen when the type is added). Time and interval
values are stored as integer microseconds; their in-memory form is canonical
text.

The type rules also gain the parts of Proposal 1's rules that only a
dialect with its own types uses: signatures over Dynamic classes (MySQL
`integer / integer` returns DECIMAL) and result-parameter rules (MySQL `/`
has scale + 4), consumed first by the test dialect and later by MySQL.

**Assignment conversion is behaviour.** Converting a value into a column
(INSERT, UPDATE, and CAST in that dialect) is part of a type's behaviour, run
before the storage ENCODE: a type may declare `ASSIGN function`; without it,
today's path applies (ENCODE, then the STRICT check). This matters because
the dialects disagree. Checked with Turso STRICT tables and MySQL 8.4.10 in its
default strict mode:

| Value into column | Turso STRICT | MySQL 8.4 |
|---|---|---|
| `'12'` into INT | 12 | 12 |
| `'abc'`, `'12abc'` into INT | error | error |
| `1.5`, `'1.5'` into INT | error | 2 |
| `99999999999` into INT, `100000` into SMALLINT | accepted (INTEGER only checks the 64-bit range) | error: out of range |
| `12`, `1.5` into VARCHAR | `'12'`, `'1.5'` | `'12'`, `'1.5'` |
| `1e20` into VARCHAR | `'1.0e+20'` | `'1e20'` |

**Integer rowid aliases.** Today only a column declared exactly `INTEGER
PRIMARY KEY` is the rowid (`is_rowid_alias_type`). A primary key whose mapped
storage type is a ranged integer is a rowid alias too, so `INT AUTO_INCREMENT
PRIMARY KEY` keeps the fast rowid path; the range check is the assignment
conversion's.

Not blocking: type functions return structured error kinds (out of range,
invalid input, too long) instead of only a message, so a frontend can map them
to its own codes (MySQL 1264, 1366, 1406; PostgreSQL 22003, 22P02, 22001).
Out of scope: unique constraints under case-insensitive collations.

Tests use a test dialect in core (as `core/dialect/mod.rs` does today), since
the MySQL frontend is not in this repository.

## Proposal 3: PostgreSQL frontend

A separate PostgreSQL PR. `postgres/parser/translator.rs` emits
`CAST('1.555' AS numeric)` for decimal literals (`translate_const`) and typed
casts for `x::numeric` and `x::numeric(p,s)`. PostgreSQL's numeric parameters
default to NULL (unconstrained). Bare `NUMERIC` columns stay REAL, so existing
tables keep their storage.

## Proposal 4: typed aggregates and ordered numeric storage

**Aggregates.** Only type-declared aggregates: `FUNCTION sum(numeric) RETURNS
numeric numeric_sum` and `avg`, with native exact accumulators. A type without
one keeps SQLite's aggregate on the in-memory values, as today. `TOTAL` stays
REAL. `sum(...) OVER` uses the same functions.

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
display scale (decision 2). `ORDERED` declares that the stored form is the
canonical key, so the optimizer (`optimizer/constraints.rs`,
`optimizer/order.rs`) allows ranges and ORDER BY on the index, and seek keys
follow the rule of Proposal 1.

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

Proposal 1, as implemented (commit 2 is deferred, commits 8 and 11 were
split, and one fix was added; see [Implementation notes](#implementation-notes-proposal-1)):
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

Proposal 2:
12. `core/types: map dialect type names onto core storage types` (also the
    Dynamic-class signatures and result-parameter rules moved from
    Proposal 1, and column type lookup in the database that owns the table,
    deferred from commit 2)
13. `core/types: dialect assignment conversions`
14. `core/schema: ranged-integer primary keys are rowid aliases in dialect
    tables`
15. `core/types: structured error kinds from type functions`

Proposal 3: 16. `postgres: exact numeric literals and casts`

Proposal 4:
17. `core/types: type-declared SUM and AVG`
18. `core/numeric: store numeric values in the ordered format`
19. `core/optimizer: index ranges and ORDER BY for ORDERED types`

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
7. Sorting: by canonical key; types without a key use a comparator that calls
   their `<`, checked when the type is created; the fixed name table goes.
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
    an INTEGER when the value fits in an i64, otherwise canonical TEXT; the
    storage format is chosen with the type, with an ordered KEY.
13. Time and interval storage is integer microseconds; the in-memory form is
    canonical text.
