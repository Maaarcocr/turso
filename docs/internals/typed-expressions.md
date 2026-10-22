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

Four proposals, each reviewed and merged on its own:

1. **Expression rules version and rounding.** Detect indexes and views whose
   stored entries were computed with older rules, then round numeric values.
2. **Typed expressions.** The compiler works out the type of every expression
   and uses it for operators, comparisons, keys, index use and CAST. Column
   values are stored as today.
3. **PostgreSQL frontend.** Emit exact numeric literals and casts (a separate
   PostgreSQL PR).
4. **Typed aggregates and ordered numeric storage.** SUM/AVG for types, and a
   numeric storage format whose bytes sort like the numbers (a file format
   change with its own compatibility review).

Not proposed: custom types on non-STRICT tables, until a frontend needs them.
Such tables will need a marker in their stored SQL, like
`POSTGRES_TABLE_SQL_PREFIX`, so that the SQLite dialect refuses them.

## Proposal 1: expression rules version and rounding

### Objects whose stored entries depend on expression rules

Four kinds of objects store the result of an expression and must compute
exactly the same result again on every UPDATE and DELETE: expression indexes,
partial indexes (their WHERE decides whether a row has an entry), indexes on
virtual generated columns (which deliberately use plain SQLite operators today,
`SelfTableContext::ForDML` in `core/vdbe/builder.rs`), and materialized views.
If the rules change between the INSERT and the DELETE, the DELETE looks for a
key that is not there (example 10) and the index no longer matches the table.
Every proposal here changes such results, starting with the rounding fix.

Each database records the version of the expression rules its objects were
built with, in a row of `__turso_internal_types` named `turso_format`. The
row's value is not SQL, so older versions that read that table (custom types
on) report the schema as malformed instead of misreading it. Proposal 4 adds the
numeric storage format to the same row. When a database is loaded:

- If the recorded version is older than the binary's (a missing row counts as
  version 0), every object of the four kinds whose expressions contain a Named
  sub-expression (worked out with the current rules) is *stale*: the planner
  does not use it, and writes to its table fail with `index ix was built with
  older expression rules; run REINDEX ix` (a materialized view must be dropped
  and created again).
- When no stale object is left, after REINDEX or at once, the next write
  transaction records the current version. Creating a typed object of the four
  kinds also records it; while stale objects exist, that fails with the same
  message.

Objects that only compute values (CHECK constraints, DEFAULT, stored generated
columns, views, triggers) are not rebuilt: they now give the corrected results,
and a stored generated value changes only when its row is updated. The same
applies to PostgreSQL table SQL, which is translated again at every load.

Why REINDEX rather than a rules version stored per object: per-object versions
would keep today's shape-based operator choice as a second compiler forever,
every typed feature would need both, and the planner would have to stop a
query compiled with new rules from using an index built with old ones. The
affected objects need custom types (experimental) and a typed expression in an
index or view, so they are rare, found at load, and fixed by an existing
statement.

A binary with custom types off skips `__turso_internal_types`, so it can write
plain SQLite values into such a database (a REAL next to numeric blobs). New
binaries read the `turso_format` row even with custom types off and refuse
writes when it exists. Older binaries cannot be changed.

### Rounding

`validate_precision_scale` (`core/numeric/decimal.rs`) calls `with_scale`,
which cuts off digits (example 1). It will call
`with_scale_round(scale, RoundingMode::HalfUp)`, which rounds half away from
zero as PostgreSQL and MySQL do (`-1.505` becomes `-1.51`). Values already
stored are not changed.

## Proposal 2: typed expressions

### Types

Types exist only at compile time. At run time a register (a value slot of
Turso's bytecode machine) contains an ordinary SQLite value; for an expression
of a custom type, it is the type's *in-memory form*: what DECODE returns
(numeric: text such as `'1.50'`). ENCODE runs only when a row or index entry is
written and when a *seek key* (the value looked up in an index, e.g. 1.555 in
`WHERE p = 1.555`) is built. No new kinds of runtime values, no B-tree change,
no change to how column values are stored.

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
- `CAST(x AS T)`: `Named(T)` when T is registered and the CAST gives all of
  T's parameters, or (outside the SQLite dialect) T declares defaults for the
  missing ones. Otherwise it is SQLite's *affinity cast* (the type name only
  chooses INTEGER, REAL, TEXT, NUMERIC or BLOB conversion) and the result is
  `Dynamic`, in two cases:
  - T is not registered. SQLite accepts any type name in a CAST:
    `CAST(x AS SIGNED)`, `CAST(x AS VARCHAR(10))` and `CAST(x AS FOOBAR)` are
    all legal. An error here would break SQLite compatibility; a frontend that
    wants strict type names rejects unknown ones in its own parser.
  - In the SQLite dialect, T is registered but parameters are missing:
    `CAST(x AS NUMERIC)` keeps today's affinity cast (the "fall through to
    regular CAST" branch of `translate_cast_expr`,
    `core/translate/expr/translator.rs`). See decision 9.
- Operators and functions: the result type of the chosen signature (below).
- `coalesce`, `ifnull`, `nullif`, `iif`, `CASE`, scalar `min`/`max`, and the
  columns of UNION, INTERSECT and EXCEPT: the type that all non-NULL branches
  have or convert to; each branch is converted to it. Otherwise `Dynamic`.
- Literals: the class the dialect gives them (SQLite and PostgreSQL rules:
  integer, real, blob by kind; string literals, `NULL` and `?` are Unknown and
  take the other operand's type).
- Otherwise `Dynamic`, with `infer_expression_primitive`'s classes.

A type has two parts. Its *storage* (BASE, parameters, ENCODE, DECODE) is
defined once in core for built-in types and is the same in every dialect,
because a file written through one dialect may be read through another. Its
*behaviour* (operators, implicit casts, functions, parameter defaults, KEY)
comes from the dialect's type rules for built-in types, and from the
definition itself for types made with CREATE TYPE.

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
when a conversion or CAST omits that parameter; `DEFAULT NULL` means "no
limit" only for types whose ENCODE handles NULL that way (`numeric_encode`
skips rounding and the precision check). `KEY expr` gives the *canonical key*
of an in-memory value: bytes that are equal exactly when the values are equal
under the type's `=`, and that sort like the type's `<`.

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
- the class of each kind of literal.

The SQLite rules have no Dynamic-class signatures, so an expression without a
Named operand compiles exactly as today. The MySQL frontend must be expressible
with these rules alone, with no MySQL code in core.

Built-in numeric under the SQLite rules, written in clause syntax. The
PostgreSQL rules differ in `CAST TO real` instead of `CAST FROM real` (so
`numeric + float8` stays `double precision`, as in PostgreSQL) and in applying
the parameter defaults to CAST as well:

```sql
-- storage (core, all dialects)
CREATE TYPE numeric(value any, precision integer, scale integer) BASE blob
  ENCODE numeric_encode(value, precision, scale) DECODE numeric_decode(value)
-- behaviour (SQLite rules)
  -- parameter defaults precision NULL, scale NULL (conversions only)
  CAST FROM integer, real AS IMPLICIT
  OPERATOR '+' (numeric, numeric) RETURNS numeric numeric_add  -- and - * /
  OPERATOR '-' (numeric) RETURNS numeric numeric_neg
  OPERATOR '<' (numeric, numeric) numeric_lt
  OPERATOR '=' (numeric, numeric) numeric_eq
  FUNCTION abs(numeric) RETURNS numeric numeric_abs
  KEY numeric_key(value)
```

`numeric_div` uses PostgreSQL's scale rule (at least 16 significant digits)
instead of 100 digits; the rule is part of the type's `/` (MySQL's rules will
choose a function with scale + 4).

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

Converting into T is `CAST` to T (below). Conversions of constants are emitted
once, before the loop. Resolution of WHERE and join terms runs during
constraint analysis and is stored with each term, so the optimizer and code
generation use the same choice. `emit_typed_operator` replaces
`emit_custom_type_operator` in `translate_binary_expr` and `condition.rs`.

### Every comparison uses the resolved operator

IN lists and IN subqueries, `CASE x WHEN`, `IS` and `IS NOT`, `nullif`, scalar
`min`/`max`, BETWEEN (already rewritten into `<=` and `>=`), and join
conditions including USING and NATURAL resolve `=` and `<` like binary
operators. Example 9 then gives PostgreSQL's results.

### Keys for hashing, sorting and removing duplicates

Hash joins, Bloom filters, IN with an ephemeral index, DISTINCT, GROUP BY,
`count(DISTINCT)`, PARTITION BY, UNION, INTERSECT, EXCEPT, ORDER BY and
MIN/MAX on a Named value use the type's canonical key (`KEY`), not the
in-memory form. The site stores the key in front of the value and compares
only the key (duplicate checks use `NoConflict` on the key columns), so the
output keeps the value as computed. Numeric's key is the order-preserving
encoding described in Proposal 4. In Proposal 2 it is computed in memory and
stored only in indexes on Named expressions (below); column values keep the
v1 format.

A type without `KEY`: sorts use a comparator that calls the type's `<`
function (replacing the fixed name table in `core/translate/order_by.rs`; only
functions that need no connection state); hashing and duplicate checks use the
in-memory form as today. All built-in types except numeric already have a
canonical in-memory form, and keeping today's behaviour for user types avoids
breaking their existing queries.

### Indexes and seek keys

- An index on a typed column c is used for a term `c OP e` only if OP resolved
  to c's type's own operator, or to SQLite's operator (steps 4 and 5) when c's
  DECODE returns stored values unchanged (smallint, date, varchar). Built-in
  types with a plain `OPERATOR '<'` also get a plain `(T, T)` `=`, so
  `uuid_col = 'A0EE...'` converts the literal to a uuid and still uses the
  index. Ranges and ORDER BY still need an ordered storage format (Proposal
  4); until then only `=`.
- The seek key is e converted to c's type exactly as a table scan converts it,
  then ENCODEd with c's parameters, with any RAISE inside ENCODE jumping to a
  fallback label instead of failing. If ENCODE fails or changes the value
  (`1.555` into `numeric(5,2)` gives 1.56; `100000` does not fit), an equality
  seek matches no row, as the scan does; a range seek (Proposal 4) drops or
  widens that bound and keeps the term as a filter. An index lookup and a scan
  always return the same rows.
- An index on a Named expression stores its canonical key, not the in-memory
  form, so `=`, ranges and ORDER BY work on it. Such indexes are stale after
  the upgrade anyway (Proposal 1), and the `turso_format` row stops older
  versions with custom types on from reading them. Code generation never reads
  the expression's value back from such an index when the key loses
  information (numeric keys drop the display scale: 1.5 and 1.50), so output
  does not depend on the plan.

### CAST

`CAST(x AS T(params))` gives what storing x in a STRICT column of type
`T(params)` and reading it back gives: ENCODE, the STRICT conversion to BASE
(an error when it would lose data, as for INSERT), then DECODE. Domains run
today's checks between ENCODE and DECODE. So `CAST('1.50' AS numeric(10,2))`
is `1.50`, `CAST('12' AS bigint)` the integer 12 (today the text `'12'`), and
`CAST(1 AS varchar(10))` the text `'1'` (today the integer 1), as in SQLite.
INSERT and UPDATE still encode; column reads still decode
(`emit_user_facing_column_value`). The CAST section of `create-type.mdx`
changes.

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

- `sqlite/conformance/turso-sqltests/typed_expressions.sqltest`: examples 2 to
  9, user types with signatures, old syntax, CAST to BASE.
- Index and scan agree: every typed comparison runs with `NOT INDEXED` and with
  `INDEXED BY` and must return the same rows (`p = 1.555`, `p < 100000`,
  `smallint_col = 100000`, an upper-case uuid literal, expression indexes).
- Rust unit tests: resolution (`date_col + ?`, `t + t`, ties), numeric keys
  (byte order equals numeric order; quickcheck).
- `tests/integration/`: a database file written by today's `main` with an
  expression index on a CAST, a typed partial index and a materialized view:
  stale detection, refused writes, REINDEX; an attached database with its own
  types; close and reopen.
- `postgres/conformance/pg-sqltests/numeric.sqltest`, values checked with
  `make -C postgres/conformance run-postgres`.

## Proposal 3: PostgreSQL frontend

A separate PostgreSQL PR. `postgres/parser/translator.rs` emits
`CAST('1.555' AS numeric)` for decimal literals (`translate_const`) and typed
casts for `x::numeric` and `x::numeric(p,s)`. PostgreSQL's numeric parameters
default to NULL (unconstrained). Bare `NUMERIC` columns stay REAL: changing that
mapping would reinterpret existing tables. PostgreSQL table SQL is translated
again at every load, so this raises the expression rules version (Proposal 1).

## Proposal 4: typed aggregates and ordered numeric storage

Needs its own compatibility review.

**Aggregates.** Only type-declared aggregates: `FUNCTION sum(numeric) RETURNS
numeric numeric_sum` and `avg`, with native exact accumulators. A type without
one keeps SQLite's aggregate on the in-memory values, as today. `TOTAL` stays
REAL. `sum(...) OVER` uses the same functions.

**Ordered numeric storage (v2).** Today's format (v1) is a version byte 0x01,
a sign byte, the scale, and the digits as little-endian 32-bit words, so
comparing two stored blobs byte by byte does not compare the numbers. v2 is
the numeric key of Proposal 2:

- byte 0: 0x08 −Infinity, 0x10 negative, 0x20 zero, 0x30 positive,
  0x38 +Infinity, 0x40 NaN (infinities and NaN reserved; never 0x01);
- the exponent E (value = 0.d1d2… × 10^E, d1 ≠ 0) as 4 bytes big-endian with
  the sign bit flipped; encoding asserts that E fits in an i32;
- the digits without trailing zeros, in base-100 pairs (byte = pair + 1);
- negative values invert the exponent bytes, use byte = 100 − pair, and end
  with 0xFF.

Byte order equals numeric order and equal values have equal bytes. DECODE
becomes `numeric_decode(value, scale)`; unconstrained values lose their
display scale (decision 2). `ORDERED` declares that the stored form is the
canonical key, so the optimizer (`optimizer/constraints.rs`,
`optimizer/order.rs`) allows ranges and ORDER BY on the index, and seek keys
follow the rule of Proposal 2.

**Format flag.** Decoding accepts v1 and v2 forever, but one column must not
mix them. The `turso_format` row records `numeric 2` when the first numeric
column is created in a database without v1 numeric columns; each attached
database is checked on its own. Databases with v1 numeric columns keep v1 and
`=`-only index use. No flag in the SQLite file header (its reserved bytes stay
zero). A migration pragma can come later.

## Commit plan

Proposal 1:
1. `core/schema: record the expression rules version and require REINDEX of
   stale indexes`
2. `core/numeric: round half away from zero when applying a scale`

Proposal 2:
3. `core/types: compute expression types and report them in column metadata`
4. `parser: parse operator signatures, implicit casts, type functions, keys and
   parameter defaults`
5. `core/types: resolve operators with dialect type rules` (storage and
   behaviour split; SQLite and PostgreSQL rules; numeric signatures and
   division scale)
6. `core/types: use resolved operators in IN, CASE, IS, nullif, min/max and
   joins`
7. `core/types: canonical keys and comparators for hash, sort and duplicate
   checks` (adds the numeric key function)
8. `core/optimizer: decide index use and seek keys from resolved operators`
9. `core/types: unary operators and type functions`
10. `core/types: CAST returns the value read back from a STRICT column`
11. `core/types: types for recursive CTEs, windows and materialized views`
12. `core/schema: look up column types in the database that owns the table`

Proposal 3: 13. `postgres: exact numeric literals and casts`

Proposal 4:
14. `core/types: type-declared SUM and AVG`
15. `core/numeric: store numeric column values in the ordered format`
16. `core/schema: record the numeric storage format per database`
17. `core/optimizer: index ranges and ORDER BY for ORDERED types`

## Decisions

1. Numeric format: a per-database `turso_format` row, not a type definition,
   unreadable by older versions, checked per attached database. No header flag.
   A migration pragma later.
2. v2 drops the display scale of unconstrained numeric values; values are
   never read back from expression indexes whose keys lose it.
3. Division: PostgreSQL's scale rule, part of the type's `/` (MySQL: scale + 4).
4. Aggregates: type-declared only.
5. Objects built with older rules: REINDEX detection (Proposal 1), for the
   reasons given there.
6. REAL and numeric: each dialect's `CAST FROM`/`CAST TO` declarations.
7. Sorting: by canonical key; types without a key use a comparator that calls
   their `<`; the fixed name table goes.
8. `smallint_col = 100000`: fixed by the seek-key rule.
9. Missing CAST parameters: declared parameter DEFAULTs (PostgreSQL numeric
   NULL, MySQL DECIMAL (10,0)). The SQLite dialect keeps the affinity cast
   (SQLite matches type names by substring, so `bigint`, `varchar(10)` and
   `date` are ordinary SQLite casts, and persisted `CAST(x AS NUMERIC)` in
   views and indexes must keep its meaning). Other dialects: a missing
   parameter without a default is an error.

## Open questions

1. Hashing and duplicate checks on a user type without `KEY` use the in-memory
   form, as today, instead of rejecting the plan. Agreed?
2. The comparator for types without `KEY` can call only functions that need no
   connection state. Is an error for the others acceptable?
