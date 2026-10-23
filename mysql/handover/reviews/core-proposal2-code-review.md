# Review of 844b3993..ebf5b1a3 (Proposal 2, commits 16–26)

Read-only review. Nothing was built or run. Every finding below comes from reading the code path; "traced" means I followed it through the code but did not run it.

## Plain SQLite (custom types off, no marked tables)

No behaviour or bytecode change found. I checked each path the commits touch:
- INSERT: `emit_custom_type_assign` emits nothing without ASSIGN. `row_number_reg` is None for unmarked tables. `emit_marked_table_key` and `emit_generated_key_range_check` are skipped because `marked_key_column` returns None. The `else if has_user_provided_rowid` branch is the old code.
- UPDATE, UPSERT, ALTER, virtual generated columns: `emit_key_conversion` and `emit_custom_type_assign_columns` emit nothing. `emit_generated_value_conversion` falls back to `emit_column_affinity`.
- CTAS: the body is not STRICT, so neither `resolve_custom_type_affinities` nor `emit_ctas_conversions` runs.
- Optimizer: `typed_column_constraint_can_use_index` and `index_order_serves_sort` are only reached for columns that have a type definition.
- Parser: type parameter lists are parsed only with `with_type_parameter_lists()`. Literal and operator typing use `TypeRules::SQLITE`, and `class_operators` is None.
- Remaining runtime cost on the hot path: see N1.

## BLOCKER

**B1. An index `=` search on a ranged-integer column fails with an error when the value is out of range. The same query without the index returns no rows.**

Files:
- `core/dialect/dialect_types.rs:184-190`: the ranged-integer ENCODE now calls `conversion_error(...)` instead of `RAISE(ABORT, …)` (commit 9129e95c).
- `core/translate/optimizer/constraints.rs:2612` (commit ebf5b1a3).
- `core/vdbe/mod.rs:2671`.

Path, for `CREATE INDEX t_i ON t(i)` with `i INT` in the test dialect, then `SELECT id FROM t WHERE i = 3000000000`:
1. `=` resolves to the class signature `(integer, integer)` with conversions `[IntoClass(Integer), None]`.
2. ebf5b1a3 now accepts this conversion through `conversion_keeps_stored_value`, so the planner searches `t_i`.
3. `emit_typed_seek_key` encodes the key inside `ErrorTarget mismatch` (`seek.rs:572-590`).
4. The search used to catch a RAISE through `raise_target`, which compiles to a Goto. Now the CASE branch calls the `conversion_error` function, which returns `LimboError::TypeConversion`.
5. The `error_target` handler only catches `LimboError::Constraint`, so the statement fails with "integer out of range for int".

A scan (`NOT INDEXED`) returns no rows. The same failure happens for:
- `tiny_col = 1000`, which the design's MySQL table says must be "false, without an error";
- `IN (...)` searches (`in_seek.rs`);
- joins such as `t1.tiny = t2.int_col` where some `t2` value is out of tinyint range. This is realistic.

The new test `ordered_types_use_indexes_for_ranges_and_order_by` only checks ranges, which never ENCODE, so it does not catch this.

Fix: treat `LimboError::TypeConversion(_)` like `Constraint` in the `error_target` check (one line). Add `i = 3000000000` to the indexed-versus-`NOT INDEXED` loop.

## SHOULD-FIX

**S1. VACUUM changes a stored key 0 in a marked AUTOINCREMENT table when `zero_generates_key` is set (traced).**

File: `core/translate/insert.rs:1227-1241`.

UPDATE never generates a key, so `UPDATE t SET id = 0` stores 0, as the design requires. VACUUM then copies each row with `INSERT INTO "t" (rowid, …) VALUES (?, …)`, built in `core/vdbe/vacuum.rs:1257` and prepared through `target_conn.prepare`. That is an ordinary INSERT on a marked table, so `emit_marked_table_key` sends the key 0 to `key_generation`, and the row gets a new id. The engine silently changes data.

Fix: apply the "0 generates a key" rule only when the INSERT names the key column itself, not `rowid`/`_rowid_`/`oid`. Alternatively, prepare VACUUM's copy statements as internal statements and skip the rule for them. Add a test: AUTOINCREMENT table, `UPDATE … SET id = 0`, VACUUM, reopen, check id = 0.

**S2. A key whose type has no ASSIGN skips the STRICT input conversion. `'5'` or `5.0` into a marked `INTEGER PRIMARY KEY` now fails as "key out of range".**

File: `core/translate/expr/arrays.rs:247-286` (`emit_key_encode`), used by INSERT, UPDATE and UPSERT.

Non-key columns get the pre-encode `TypeCheck`, which converts `'5'` to 5. The key only gets ASSIGN, ENCODE, then `IsType Integer`; anything else falls into `emit_key_error("out of range")`. So in the test dialect, `INSERT INTO t VALUES ('5', 1)` and `UPDATE t SET id = '7'` fail with the wrong error kind. Before 5d6d3fde, `MustBeInt` accepted these values.

MySQL is not affected as long as every integer type has an ASSIGN, but the core contract ("without ASSIGN, today's path applies") is broken.

Fix: report out of range only for a BLOB (the `uint64` form above i64::MAX) and leave every other value to the existing `MustBeInt`. Or apply integer affinity to the key before ENCODE.

**S3. User docs are missing (repo rule 10).**

With custom types on, these functions are callable from SQL and are meant for user ENCODE/ASSIGN expressions:
- `conversion_error(kind, message)`;
- `uint64_*`, `int64_*`;
- the `*_encode`/`*_decode` functions;
- `datetime_micros_lt`, `time_micros_lt`.

None of them is in `docs/sql-reference/`. `create-type.mdx:205` still names only `numeric_lt` as a comparison function built into sorting, but this branch adds three more.

Fix: add a short section to `create-type.mdx` (the kinds `conversion_error` accepts, and the list of built-in comparison and storage functions).

## NITS

**N1. `op_function` (`core/vdbe/execute.rs:10181-10199`) now wraps every scalar function call, including in plain SQLite.**

Each call goes through `run_function(...).map_err(...)`, and every function error is unboxed and boxed again. The cost is probably not measurable (the callee has one caller and will likely be inlined), but this is the plain-SQLite hot path.

Fix: add the column and row where step errors are already handled (`vdbe/mod.rs:2670`, which already inspects the error). The function-call path then stays unchanged.

**N2. `ScalarFunc::Storage` clones its arguments into a new `Vec` on every call (`execute.rs:11675-11681`).**

For MySQL this means one allocation per row for every `+`, `-` and `*` on integers (`int64_add` …). Phase C says checked `+` runs on every row of 117k specs.

Fix: pass `&[&Value]` or a fixed array of borrowed register values. Also, `StorageFunc::execute` re-checks arity at run time, which cannot fail because the call was already checked when it was compiled. Drop that check.

**N3. `emit_custom_type_encode_columns` (`arrays.rs:451-456`) emits a `TypeConversionContext` for every column of a marked table, even columns with no ENCODE.**

That adds one extra instruction per column per row. Emit it only for columns whose type has an ENCODE (and likewise for ASSIGN).

**N4. Simplicity.**
- `Func::TypedDialect` duplicates `Func::Dialect`. One variant with argument types (empty for engine-emitted calls) would be enough.
- `translate_expr_by_kind` (`translator.rs:220`) builds a `TypeEnv` for every numeric literal before it checks `decimal_literal_type`. Check `resolver.dialect.type_rules().decimal_literal_type.is_some()` first. This only costs compile time.

**N5. Storage function names now resolve before extension functions in every dialect, even with custom types off.**

An extension function named `int64_add` or `conversion_error` now fails with "no such function". `numeric_*` already behaves this way, so this is consistent but still a small plain-SQLite change.

## Per commit (beyond the findings above)
- 27dfd21a uint64: fine. BLOB form and sort order are correct; the comparator falls back to `partial_cmp` on error.
- 5a10486e date/time: fine. No panics found; fraction rounding and ZERO_DATE are handled.
- 121661b0 parser lists: fine; only enabled by the opt-in flag.
- 9c16f7ec enumerations and label sets: fine. Shifts are bounded by the 64-label check.
- 659624fc class signatures: fine. PostgreSQL and SQLite rules give the same resolution as before.
- 27cdd679 argument types: fine.
- 9129e95c error kinds: see B1 (the ranged ENCODE change) and N1.
- e0252af4 ASSIGN/CAST: fine. Stored CREATE TYPE text still reparses, and old files are unaffected.
- 5d6d3fde integer keys: see S1 and S2. NOT NULL, OR IGNORE, AUTOINCREMENT and the generated-key range check look right.
- a4b7b589 metadata and CTAS: fine.
- ebf5b1a3 ranges and ORDER BY: the range and ORDER BY rules are consistent with the scan; see B1 for `=`.
- No new comments; callers sit above callees.
