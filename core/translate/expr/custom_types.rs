use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_typed_operator(
    program: &mut ProgramBuilder,
    referenced_tables: Option<&TableReferences>,
    e1: &ast::Expr,
    op: &ast::Operator,
    e2: &ast::Expr,
    resolved: &ResolvedOperator,
    target_register: usize,
    resolver: &Resolver,
    emit_mode: BinaryEmitMode,
) -> Result<()> {
    let Some(func_name) = &resolved.function else {
        let lhs = converted_operand_expr(
            program,
            referenced_tables,
            e1,
            &resolved.conversions[0],
            resolver,
        )?;
        let rhs = converted_operand_expr(
            program,
            referenced_tables,
            e2,
            &resolved.conversions[1],
            resolver,
        )?;
        binary_expr_shared(
            program,
            referenced_tables,
            &lhs,
            &rhs,
            op,
            target_register,
            resolver,
            emit_mode,
        )?;
        return Ok(());
    };
    let lhs = emit_converted_operand(
        program,
        referenced_tables,
        e1,
        &resolved.conversions[0],
        resolver,
    )?;
    let rhs = emit_converted_operand(
        program,
        referenced_tables,
        e2,
        &resolved.conversions[1],
        resolver,
    )?;
    emit_operator_function(
        program,
        op,
        func_name,
        resolved,
        lhs,
        rhs,
        target_register,
        resolver,
    )?;
    if let BinaryEmitMode::Condition(condition_metadata) = emit_mode {
        emit_cond_jump(program, condition_metadata, target_register);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_typed_comparison(
    program: &mut ProgramBuilder,
    referenced_tables: Option<&TableReferences>,
    op: &ast::Operator,
    resolved: &ResolvedOperator,
    operands: [(usize, Option<&ast::Expr>); 2],
    dest: usize,
    resolver: &Resolver,
) -> Result<()> {
    let lhs = emit_value_conversion(program, operands[0].0, &resolved.conversions[0], resolver)?;
    let rhs = emit_value_conversion(program, operands[1].0, &resolved.conversions[1], resolver)?;
    if let Some(func_name) = &resolved.function {
        return emit_operator_function(program, op, func_name, resolved, lhs, rhs, dest, resolver);
    }
    let operand_expr = |i: usize, reg: usize| match operands[i].1 {
        Some(expr) if resolved.conversions[i].is_none() => expr.clone(),
        _ => ast::Expr::Register(reg),
    };
    let lhs_expr = operand_expr(0, lhs);
    let rhs_expr = operand_expr(1, rhs);
    let collation = comparison_collation(&lhs_expr, &rhs_expr, referenced_tables, Some(resolver))?;
    program.set_collation(collation.map(|collation| (collation, false)));
    emit_binary_insn(
        program,
        op,
        lhs,
        rhs,
        dest,
        &lhs_expr,
        &rhs_expr,
        referenced_tables,
        Some(resolver),
    )?;
    program.reset_collation();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_operator_function(
    program: &mut ProgramBuilder,
    op: &ast::Operator,
    func_name: &str,
    resolved: &ResolvedOperator,
    lhs: usize,
    rhs: usize,
    dest: usize,
    resolver: &Resolver,
) -> Result<()> {
    let func = resolver
        .resolve_function(func_name, 2)?
        .ok_or_else(|| LimboError::ParseError(format!("no such function: {func_name}")))?;
    if !matches!(op, ast::Operator::Is | ast::Operator::IsNot) {
        emit_function_call(program, func, resolved, lhs, rhs, dest);
        return Ok(());
    }
    let done = program.allocate_label();
    let lhs_not_null = program.allocate_label();
    program.emit_insn(Insn::NotNull {
        reg: lhs,
        target_pc: lhs_not_null,
    });
    program.emit_insn(Insn::Integer { value: 1, dest });
    program.emit_insn(Insn::IsNull {
        reg: rhs,
        target_pc: done,
    });
    program.emit_insn(Insn::Integer { value: 0, dest });
    program.emit_insn(Insn::Goto { target_pc: done });
    program.preassign_label_to_next_insn(lhs_not_null);
    program.emit_insn(Insn::Integer { value: 0, dest });
    program.emit_insn(Insn::IsNull {
        reg: rhs,
        target_pc: done,
    });
    emit_function_call(program, func, resolved, lhs, rhs, dest);
    program.preassign_label_to_next_insn(done);
    if matches!(op, ast::Operator::IsNot) {
        program.emit_insn(Insn::Not { reg: dest, dest });
    }
    Ok(())
}

fn emit_function_call(
    program: &mut ProgramBuilder,
    func: Func,
    resolved: &ResolvedOperator,
    lhs: usize,
    rhs: usize,
    dest: usize,
) {
    let (first, second) = if resolved.swap_args {
        (rhs, lhs)
    } else {
        (lhs, rhs)
    };
    let args = program.alloc_registers(2);
    program.emit_insn(Insn::Copy {
        src_reg: first,
        dst_reg: args,
        extra_amount: 0,
    });
    program.emit_insn(Insn::Copy {
        src_reg: second,
        dst_reg: args + 1,
        extra_amount: 0,
    });
    program.emit_insn(Insn::Function {
        constant_mask: 0,
        start_reg: args,
        dest,
        func: FuncCtx { func, arg_count: 2 },
    });
    if resolved.negate {
        program.emit_insn(Insn::Not { reg: dest, dest });
    }
}

fn emit_converted_operand(
    program: &mut ProgramBuilder,
    referenced_tables: Option<&TableReferences>,
    expr: &ast::Expr,
    conversion: &Conversion,
    resolver: &Resolver,
) -> Result<usize> {
    let reg = program.alloc_register();
    translate_expr(program, referenced_tables, expr, reg, resolver)?;
    emit_value_conversion(program, reg, conversion, resolver)
}

fn converted_operand_expr(
    program: &mut ProgramBuilder,
    referenced_tables: Option<&TableReferences>,
    expr: &ast::Expr,
    conversion: &Conversion,
    resolver: &Resolver,
) -> Result<ast::Expr> {
    if matches!(conversion, Conversion::None) {
        return Ok(expr.clone());
    }
    let reg = emit_converted_operand(program, referenced_tables, expr, conversion, resolver)?;
    Ok(ast::Expr::Register(reg))
}

pub(super) fn emit_type_function_call(
    program: &mut ProgramBuilder,
    referenced_tables: Option<&TableReferences>,
    resolved: &ResolvedFunction,
    args: &[&ast::Expr],
    target_register: usize,
    resolver: &Resolver,
) -> Result<usize> {
    let func = resolver
        .resolve_function(&resolved.function, args.len())?
        .ok_or_else(|| {
            LimboError::ParseError(format!("no such function: {}", resolved.function))
        })?;
    let start_reg = program.alloc_registers(args.len());
    for (i, arg) in args.iter().enumerate() {
        translate_expr(program, referenced_tables, arg, start_reg + i, resolver)?;
        emit_value_conversion_in_place(program, start_reg + i, &resolved.conversions[i], resolver)?;
    }
    program.emit_insn(Insn::Function {
        constant_mask: 0,
        start_reg,
        dest: target_register,
        func: FuncCtx {
            func,
            arg_count: args.len(),
        },
    });
    Ok(target_register)
}

pub(super) struct TypedInTargets {
    pub(super) found: BranchOffset,
    pub(super) not_found: BranchOffset,
    pub(super) unknown: BranchOffset,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_typed_in_subquery_probe(
    program: &mut ProgramBuilder,
    referenced_tables: Option<&TableReferences>,
    cursor_id: CursorID,
    lhs: (usize, &ast::Expr),
    resolved: &ResolvedOperator,
    targets: TypedInTargets,
    resolver: &Resolver,
) -> Result<()> {
    let saw_unknown = program.alloc_register();
    program.emit_insn(Insn::Integer {
        value: 0,
        dest: saw_unknown,
    });
    let no_match = program.allocate_label();
    program.emit_insn(Insn::Rewind {
        cursor_id,
        pc_if_empty: no_match,
    });
    let loop_start = program.allocate_label();
    program.preassign_label_to_next_insn(loop_start);
    let value_reg = program.alloc_register();
    program.emit_insn(Insn::Column {
        cursor_id,
        column: 0,
        dest: value_reg,
        default: None,
    });
    let equal_reg = program.alloc_register();
    emit_typed_comparison(
        program,
        referenced_tables,
        &ast::Operator::Equals,
        resolved,
        [(lhs.0, Some(lhs.1)), (value_reg, None)],
        equal_reg,
        resolver,
    )?;
    program.emit_insn(Insn::If {
        reg: equal_reg,
        target_pc: targets.found,
        jump_if_null: false,
    });
    let next = program.allocate_label();
    program.emit_insn(Insn::IfNot {
        reg: equal_reg,
        target_pc: next,
        jump_if_null: false,
    });
    program.emit_insn(Insn::Integer {
        value: 1,
        dest: saw_unknown,
    });
    program.preassign_label_to_next_insn(next);
    program.emit_insn(Insn::Next {
        cursor_id,
        pc_if_next: loop_start,
        fullscan: false,
        is_index: false,
    });
    program.preassign_label_to_next_insn(no_match);
    program.emit_insn(Insn::If {
        reg: saw_unknown,
        target_pc: targets.unknown,
        jump_if_null: false,
    });
    program.emit_insn(Insn::Goto {
        target_pc: targets.not_found,
    });
    Ok(())
}

pub(super) fn translate_typed_nullif(
    program: &mut ProgramBuilder,
    referenced_tables: Option<&TableReferences>,
    args: &[Box<ast::Expr>],
    target_register: usize,
    resolver: &Resolver,
) -> Result<bool> {
    let env = TypeEnv::from_resolver(resolver);
    let Some(resolved) = resolve_operator(
        &ast::Operator::Equals,
        &args[0],
        &args[1],
        referenced_tables,
        &env,
    )?
    else {
        return Ok(false);
    };
    let first_reg = program.alloc_register();
    let second_reg = program.alloc_register();
    translate_expr(program, referenced_tables, &args[0], first_reg, resolver)?;
    translate_expr(program, referenced_tables, &args[1], second_reg, resolver)?;
    let equal_reg = program.alloc_register();
    emit_typed_comparison(
        program,
        referenced_tables,
        &ast::Operator::Equals,
        &resolved,
        [(first_reg, Some(&args[0])), (second_reg, Some(&args[1]))],
        equal_reg,
        resolver,
    )?;
    program.emit_insn(Insn::Copy {
        src_reg: first_reg,
        dst_reg: target_register,
        extra_amount: 0,
    });
    let done = program.allocate_label();
    program.emit_insn(Insn::IfNot {
        reg: equal_reg,
        target_pc: done,
        jump_if_null: true,
    });
    program.emit_insn(Insn::Null {
        dest: target_register,
        dest_end: None,
    });
    program.preassign_label_to_next_insn(done);
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn translate_typed_min_max(
    program: &mut ProgramBuilder,
    referenced_tables: Option<&TableReferences>,
    args: &[Box<ast::Expr>],
    is_max: bool,
    func_ctx: FuncCtx,
    target_register: usize,
    resolver: &Resolver,
) -> Result<bool> {
    let env = TypeEnv::from_resolver(resolver);
    let arg_exprs: Vec<&ast::Expr> = args.iter().map(|arg| arg.as_ref()).collect();
    let Some((common, conversions)) = common_type_conversions(&arg_exprs, referenced_tables, &env)
    else {
        return Ok(false);
    };
    let common = ExprType::Named(common);
    let less = resolve_typed_operator(
        &ast::Operator::Less,
        [(&common, None), (&common, None)],
        &env,
    )?;
    let start_reg = program.alloc_registers(args.len());
    for (i, arg) in args.iter().enumerate() {
        translate_expr(program, referenced_tables, arg, start_reg + i, resolver)?;
        emit_value_conversion_in_place(program, start_reg + i, &conversions[i], resolver)?;
    }
    let Some(less) = less.filter(|less| less.function.is_some()) else {
        program.emit_insn(Insn::Function {
            constant_mask: 0,
            start_reg,
            dest: target_register,
            func: func_ctx,
        });
        return Ok(true);
    };
    let null_result = program.allocate_label();
    let done = program.allocate_label();
    for i in 0..args.len() {
        program.emit_insn(Insn::IsNull {
            reg: start_reg + i,
            target_pc: null_result,
        });
    }
    program.emit_insn(Insn::Copy {
        src_reg: start_reg,
        dst_reg: target_register,
        extra_amount: 0,
    });
    let less_reg = program.alloc_register();
    for candidate in start_reg + 1..start_reg + args.len() {
        let operands = if is_max {
            [(target_register, None), (candidate, None)]
        } else {
            [(candidate, None), (target_register, None)]
        };
        emit_typed_comparison(
            program,
            referenced_tables,
            &ast::Operator::Less,
            &less,
            operands,
            less_reg,
            resolver,
        )?;
        let keep_current = program.allocate_label();
        program.emit_insn(Insn::IfNot {
            reg: less_reg,
            target_pc: keep_current,
            jump_if_null: true,
        });
        program.emit_insn(Insn::Copy {
            src_reg: candidate,
            dst_reg: target_register,
            extra_amount: 0,
        });
        program.preassign_label_to_next_insn(keep_current);
    }
    program.emit_insn(Insn::Goto { target_pc: done });
    program.preassign_label_to_next_insn(null_result);
    program.emit_insn(Insn::Null {
        dest: target_register,
        dest_end: None,
    });
    program.preassign_label_to_next_insn(done);
    Ok(true)
}

/// Evaluate an expression-index expression in a DML context (INSERT/UPDATE/UPSERT).
///
/// Shared logic: decode custom-type column registers into temps (so the
/// expression sees user-facing values), build a `SelfTableContext::ForDML`,
/// and translate the expression.
///
/// The caller must:
/// 1. Clone the expression from `idx_col.expr`
/// 2. Build the initial `column_regs` mapping (before decode)
///
/// The expression is resolved via `resolve_gencol_expr_columns` and custom-type
/// columns are decoded in-place in `column_regs`.
///
/// `uses_custom_type_operators` must be false when `expr` is the expression of a
/// virtual generated column, see `SelfTableContext::ForDML`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_dml_expr_index_value(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    mut expr: ast::Expr,
    columns: &[Column],
    column_regs: &mut [usize],
    table: &Arc<BTreeTable>,
    dest_reg: usize,
    uses_custom_type_operators: bool,
) -> Result<()> {
    crate::schema::resolve_gencol_expr_columns(&mut expr, columns)?;

    let is_strict = table.is_strict;
    for (i, col) in columns.iter().enumerate() {
        if col.is_rowid_alias() {
            continue;
        }
        if let Ok(Some(resolved)) = resolver.schema().column_resolved_type(col, is_strict) {
            if resolved.chain.iter().any(|td| td.decode().is_some()) {
                let src_reg = column_regs[i];
                let tmp = program.alloc_register();
                emit_user_facing_column_value(program, src_reg, tmp, col, is_strict, resolver)?;
                column_regs[i] = tmp;
            }
        }
    }

    let pairs = columns.iter().zip(column_regs.iter().copied());
    let ctx = SelfTableContext::ForDML {
        dml_ctx: DmlColumnContext::from_column_reg_mapping(pairs),
        table: Arc::clone(table),
        uses_custom_type_operators,
    };
    resolver.with_self_table_context(program, Some(&ctx), |program, _| {
        translate_expr(program, None, &expr, dest_reg, resolver)?;
        Ok(())
    })?;
    Ok(())
}

/// Emit bytecode that transforms a stored column value into its user-facing
/// representation.
///
/// For regular columns this is a simple copy (or no-op when source == dest).
/// For custom type columns with a DECODE function the decode expression is
/// applied, converting the internal storage form back to the value the user
/// expects to see.
///
/// Every code path that surfaces a stored column value to the user — SELECT,
/// RETURNING, trigger OLD/NEW — should go through this function so decode
/// logic lives in one place.
pub(crate) fn emit_user_facing_column_value(
    program: &mut ProgramBuilder,
    source_reg: usize,
    dest_reg: usize,
    column: &Column,
    is_strict: bool,
    resolver: &Resolver,
) -> Result<()> {
    if source_reg != dest_reg {
        program.emit_insn(Insn::Copy {
            src_reg: source_reg,
            dst_reg: dest_reg,
            extra_amount: 0,
        });
    }
    // Array columns: pass through raw record blob. ArrayDecode is emitted
    // at display time (ResultRow) so that functions/subscripts see raw blobs.
    if column.is_array() {
        return Ok(());
    }
    if let Ok(Some(resolved)) = resolver.schema().column_resolved_type(column, is_strict) {
        if resolved.chain.iter().all(|td| td.decode().is_none()) {
            return Ok(());
        }
        let skip_label = program.allocate_label();
        program.emit_insn(Insn::IsNull {
            reg: dest_reg,
            target_pc: skip_label,
        });

        // Apply decode in reverse order (parent/ancestor first, then child)
        for td in resolved.chain.iter().rev() {
            if let Some(decode_expr) = td.decode() {
                emit_type_expr(
                    program,
                    decode_expr,
                    dest_reg,
                    dest_reg,
                    column,
                    td,
                    resolver,
                )?;
            }
        }

        program.preassign_label_to_next_insn(skip_label);
    }
    Ok(())
}

/// Emit domain constraint checks for CAST(expr AS domain).
/// Validates NOT NULL and CHECK constraints from the domain type chain.
pub(super) fn emit_domain_cast_constraints(
    program: &mut ProgramBuilder,
    chain: &[crate::sync::Arc<TypeDef>],
    reg: usize,
    resolver: &Resolver,
) -> Result<()> {
    use crate::error::{SQLITE_CONSTRAINT_CHECK, SQLITE_CONSTRAINT_NOTNULL};

    let any_not_null = chain.iter().any(|td| td.not_null);

    if any_not_null {
        program.emit_insn(Insn::HaltIfNull {
            target_reg: reg,
            err_code: SQLITE_CONSTRAINT_NOTNULL,
            description: format!(
                "domain {} does not allow null values",
                chain.first().map(|td| td.name.as_str()).unwrap_or("?")
            ),
            on_error: None,
        });
    }

    for td in chain {
        for (i, dc) in td.domain_checks.iter().enumerate() {
            let constraint_name = dc
                .name
                .clone()
                .unwrap_or_else(|| format!("{}_{}", td.name, i));

            // Bind `value` → reg, translate check expr, verify truthy
            let outer_value = program
                .id_register_overrides
                .insert("value".to_string(), reg);

            let expr_result_reg = program.alloc_register();
            let translated = translate_expr(program, None, &dc.check, expr_result_reg, resolver);

            match outer_value {
                Some(outer_reg) => program
                    .id_register_overrides
                    .insert("value".to_string(), outer_reg),
                None => program.id_register_overrides.remove("value"),
            };
            translated?;

            let passed_label = program.allocate_label();

            // NULL result passes CHECK constraints (SQLite semantics)
            program.emit_insn(Insn::IsNull {
                reg: expr_result_reg,
                target_pc: passed_label,
            });

            program.emit_insn(Insn::If {
                reg: expr_result_reg,
                target_pc: passed_label,
                jump_if_null: false,
            });

            program.emit_insn(Insn::Halt {
                err_code: SQLITE_CONSTRAINT_CHECK,
                description: format!(
                    "value for domain {} violates check constraint \"{}\"",
                    td.name, constraint_name
                ),
                on_error: None,
                description_reg: None,
            });

            program.preassign_label_to_next_insn(passed_label);
        }
    }
    Ok(())
}

/// Emit bytecode for a custom type encode/decode expression.
/// Sets up `value` to reference `value_reg`, and type parameter overrides
/// from `column.ty_params` matched against `type_def.params`.
/// The expression result is written to `dest_reg`.
pub(crate) fn emit_type_expr(
    program: &mut ProgramBuilder,
    expr: &ast::Expr,
    value_reg: usize,
    dest_reg: usize,
    column: &Column,
    type_def: &TypeDef,
    resolver: &Resolver,
) -> Result<usize> {
    let outer_overrides = program.id_register_overrides.clone();
    program
        .id_register_overrides
        .insert("value".to_string(), value_reg);

    // Set up type parameter overrides. Capture the result so we can
    // clean up overrides even if param translation fails.
    let param_result: Result<()> = (|| {
        // Skip `value` param (already handled above); match remaining params
        // against the user-provided ty_params by position.
        if type_def.takes_label_list {
            let labels = column
                .ty_params
                .iter()
                .map(|label| label.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let reg = program.alloc_register();
            program.emit_insn(Insn::String8 {
                value: labels,
                dest: reg,
            });
            for param in type_def.user_params() {
                program
                    .id_register_overrides
                    .insert(param.name.clone(), reg);
            }
            return Ok(());
        }
        let user_params: Vec<_> = type_def.user_params().collect();
        for (i, param) in user_params.iter().enumerate() {
            let param_expr = column
                .ty_params
                .get(i)
                .map(|param_expr| param_expr.as_ref())
                .or(param.default.as_deref());
            if let Some(param_expr) = param_expr {
                let reg = program.alloc_register();
                translate_expr(program, None, param_expr, reg, resolver)?;
                program
                    .id_register_overrides
                    .insert(param.name.clone(), reg);
            }
        }
        Ok(())
    })();

    // Translate body expression only if param setup succeeded
    let result = param_result.and_then(|()| {
        // Translate the expression, disabling constant optimization since
        // the `value` placeholder refers to a register that changes per row.
        translate_expr_no_constant_opt(
            program,
            None,
            expr,
            dest_reg,
            resolver,
            NoConstantOptReason::RegisterReuse,
        )
    });

    program.id_register_overrides = outer_overrides;

    result
}

/// Decode custom type columns for AFTER trigger NEW registers.
///
/// For each column with a custom type decode expression, copies the encoded register
/// to a new register and emits the decode expression. NULL values are skipped.
/// Returns a Vec of registers: one per column (decoded or original) plus the rowid at the end.
pub(crate) fn emit_trigger_decode_registers(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    columns: &[Column],
    source_regs: &dyn Fn(usize) -> usize,
    rowid_reg: usize,
    is_strict: bool,
) -> Result<Vec<usize>> {
    columns
        .iter()
        .enumerate()
        .map(|(i, col)| -> Result<usize> {
            if let Ok(Some(resolved)) = resolver.schema().column_resolved_type(col, is_strict) {
                if resolved.chain.iter().any(|td| td.decode().is_some()) {
                    let decoded_reg = program.alloc_register();
                    emit_user_facing_column_value(
                        program,
                        source_regs(i),
                        decoded_reg,
                        col,
                        is_strict,
                        resolver,
                    )?;
                    return Ok(decoded_reg);
                }
            }
            Ok(source_regs(i))
        })
        .chain(std::iter::once(Ok(rowid_reg)))
        .collect::<Result<Vec<usize>>>()
}
