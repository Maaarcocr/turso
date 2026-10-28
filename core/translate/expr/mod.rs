use crate::error::{SQLITE_CONSTRAINT, SQLITE_CONSTRAINT_TRIGGER, SQLITE_ERROR};
use crate::translate::optimizer::constraints::ConstraintOperator;
use crate::turso_assert;
use tracing::{instrument, Level};
use turso_parser::ast::{self, Expr, ResolveType, SubqueryType, TableInternalId, UnaryOperator};

use super::collate::{resolve_comparison_collseq_with_resolver, CollationSeq};
use super::emitter::Resolver;
use super::optimizer::Optimizable;
use super::plan::TableReferences;
#[cfg(all(feature = "fts", not(target_family = "wasm")))]
use crate::function::FtsFunc;
#[cfg(feature = "json")]
use crate::function::JsonFunc;
use crate::function::{AggFunc, Func, FuncCtx, MathFuncArity, ScalarFunc, VectorFunc};
use crate::schema::{
    BTreeTable, ColDef, Column, ColumnLayout, GeneratedType, ParenthesizedJoinColumn,
    ParenthesizedJoinColumnVisibility, Table, Type, TypeDef,
};
use crate::sync::Arc;
use crate::translate::expression_index::selected_expression_index;
use crate::translate::plan::{ColumnMask, Operation, ResultSetColumn, Search};
use crate::translate::planner::parse_row_id;
use crate::util::{exprs_are_equivalent, normalize_ident, parse_numeric_literal};
use crate::vdbe::affinity::Affinity;
use crate::vdbe::builder::{CursorKey, DmlColumnContext, SelfTableContext};
use crate::vdbe::{
    builder::ProgramBuilder,
    insn::{CmpInsFlags, InsertFlags, Insn},
    BranchOffset, CursorID,
};
use crate::{LimboError, Numeric, Result, Value};

#[macro_use]
mod metadata;

mod affinity;
mod arrays;
mod binary;
mod binding;
mod columns;
mod condition;
mod custom_types;
mod emission;
pub(crate) mod functions;
mod translator;
mod types;
mod utils;
mod vectors;
mod walk;

#[allow(unused_imports)]
use affinity::*;
#[allow(unused_imports)]
use arrays::*;
#[allow(unused_imports)]
use binary::*;
#[allow(unused_imports)]
use binding::*;
#[allow(unused_imports)]
use columns::*;
#[allow(unused_imports)]
use condition::*;
#[allow(unused_imports)]
use custom_types::*;
#[allow(unused_imports)]
use emission::*;
#[allow(unused_imports)]
use functions::*;
#[allow(unused_imports)]
use metadata::*;
#[allow(unused_imports)]
use translator::*;
#[allow(unused_imports)]
use types::*;
#[allow(unused_imports)]
use utils::*;
#[allow(unused_imports)]
use vectors::*;
#[allow(unused_imports)]
use walk::*;

pub(crate) use affinity::{
    comparison_affinity, expr_data_type, get_expr_affinity, StorageClassMask,
};
pub(crate) use arrays::{
    emit_array_decode, emit_column_encode, emit_custom_type_assign_columns,
    emit_custom_type_decode_columns, emit_custom_type_encode_columns,
    emit_generated_key_range_check, emit_key_assign, emit_key_conversion, emit_key_encode,
    emit_row_number_counter,
};
pub(crate) use binary::expr_is_array;
pub use binding::{bind_and_rewrite_expr, BindingBehavior};
pub(super) use binding::{find_unqualified_column, lookup_unqualified_column};
pub use columns::{emit_stored_column, emit_table_column, emit_table_column_for_dml};
pub use condition::translate_condition_expr;
pub(crate) use custom_types::{
    emit_dml_expr_index_value, emit_trigger_decode_registers, emit_type_expr,
    emit_user_facing_column_value,
};
pub use emission::{
    emit_function_call, emit_literal, process_returning_clause, ReturningBufferCtx,
};
pub(crate) use emission::{
    emit_returning_results, emit_returning_scan_back, restore_returning_row_image_in_cache,
    seed_returning_row_image_in_cache,
};
pub use metadata::ConditionMetadata;
pub use translator::{
    resolve_expr, translate_expr, translate_expr_no_constant_opt, NoConstantOptReason,
};
pub(crate) use types::{
    branch_type, class_column_type, column_named_type, common_branch_type,
    conversion_keeps_stored_value, emit_assign_into_type, emit_generated_stored_value,
    emit_generated_value_conversion, emit_type_key, emit_type_keys, emit_value_conversion,
    emit_value_conversion_in_place, expr_type, hash_join_can_compare_values, hash_join_key_form,
    in_subquery_comparison, index_order_serves_sort, operator_type, plan_column_types,
    plan_result_types, recursive_cte_column_types, recursive_cte_conversions,
    recursive_cte_row_keys, resolve_operator, resolve_typed_operator, select_column_types,
    type_key, type_keys, typed_range_can_use_index, Conversion, DynamicClass, ExprType,
    HashKeyForm, InSubqueryComparison, IndexRowKey, IndexRowKeys, NamedType, ResolvedOperator,
    SubqueryColumnTypes, TypeEnv, TypeKey,
};
pub use utils::{
    as_binary_components, maybe_apply_affinity, sanitize_string, truth_test_rhs, unwrap_parens,
    unwrap_parens_owned,
};
pub use vectors::expr_vector_size;
pub use walk::{
    expr_contains_nondeterministic_scalar_function, expr_references_any_subquery,
    expr_references_outer_query, expr_references_subquery_id, expression_can_fail_on_input,
    walk_expr, walk_expr_mut, WalkControl,
};
