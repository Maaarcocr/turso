use crate::schema::{BTreeTable, ColumnLayout, ColumnsTopologicalSort, GeneratedType};
use crate::translate::expr::{
    emit_column_encode, emit_generated_value_conversion, emit_user_facing_column_value,
    translate_expr,
};
use crate::vdbe::builder::{DmlColumnContext, SelfTableContext};
use crate::{Arc, Result};
use turso_parser::ast;

use super::{ProgramBuilder, Resolver};

/// Emit bytecode to compute virtual generated columns for a row.
#[turso_macros::trace_stack]
pub fn compute_virtual_columns(
    program: &mut ProgramBuilder,
    columns: &ColumnsTopologicalSort<'_>,
    dml_ctx: &DmlColumnContext,
    resolver: &Resolver,
    table: &Arc<BTreeTable>,
) -> Result<()> {
    let ctx = SelfTableContext::ForDML {
        dml_ctx: dml_ctx.clone(),
        table: Arc::clone(table),
        uses_custom_type_operators: false,
    };
    resolver.with_self_table_context(program, Some(&ctx), |program, _| {
        for (idx, column) in columns.iter() {
            let GeneratedType::Virtual { expr, .. } = column.generated_type() else {
                continue;
            };
            let target_reg = dml_ctx.to_column_reg(idx);
            translate_expr(program, None, expr, target_reg, resolver)?;
            emit_generated_value_conversion(program, target_reg, column, resolver)?;
            if !dml_ctx.holds_encoded_values() && table.is_strict && !column.is_array() {
                emit_column_encode(program, resolver, column, target_reg, &table.name, None)?;
                emit_user_facing_column_value(
                    program, target_reg, target_reg, column, true, resolver,
                )?;
            }
        }
        Ok(())
    })?;
    if dml_ctx.holds_encoded_values() && table.is_strict {
        for (idx, column) in columns.iter() {
            if column.is_virtual_generated() {
                let reg = dml_ctx.to_column_reg(idx);
                emit_column_encode(program, resolver, column, reg, &table.name, None)?;
            }
        }
    }
    Ok(())
}

/// Emit bytecode to compute a single virtual generated column expression.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_gencol_expr_from_registers(
    program: &mut ProgramBuilder,
    expr: &ast::Expr,
    target_reg: usize,
    registers_start: usize,
    columns: &[crate::schema::Column],
    resolver: &Resolver,
    rowid_reg: usize,
    layout: &ColumnLayout,
    table: &Arc<BTreeTable>,
) -> Result<()> {
    let ctx = SelfTableContext::ForDML {
        dml_ctx: DmlColumnContext::layout(columns, registers_start, rowid_reg, layout.clone()),
        table: Arc::clone(table),
        uses_custom_type_operators: false,
    };
    resolver.with_self_table_context(program, Some(&ctx), |program, _| {
        translate_expr(program, None, expr, target_reg, resolver)?;
        Ok(())
    })?;

    Ok(())
}
