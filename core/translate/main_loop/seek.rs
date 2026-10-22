use super::*;
use crate::translate::expr::{
    column_named_type, emit_type_expr, emit_type_key, emit_user_facing_column_value,
    emit_value_conversion, emit_value_conversion_in_place, expr_type, resolve_typed_operator,
    type_key, Conversion, DynamicClass, ExprType, NamedType, TypeEnv,
};
use crate::translate::plan::BitSet;
use crate::vdbe::insn::NullMatchingMask;
use turso_parser::ast;
use turso_parser::ast::NullsOrder;

fn index_seek_affinities(seek_def: &SeekDef, seek_key: &SeekKey) -> String {
    // Apply the constraint's resolved comparison affinity to the seek key,
    // not the indexed column's affinity.
    seek_def
        .iter(seek_key)
        .zip(seek_def.iter_affinity(seek_key))
        .map(|(key_component, aff)| match key_component {
            SeekKeyComponent::Expr(expr) if aff.expr_needs_no_affinity_change(expr) => {
                affinity::SQLITE_AFF_BLOB
            }
            _ => aff.aff_mask(),
        })
        .collect()
}

/// Seek-based loop setup.
///
/// A seek loop has a real two-phase contract:
/// 1. Emit and position using the start bound.
/// 2. Emit the termination bound and anchor `loop_start`.
pub(super) struct SeekEmitter<'a, 'plan> {
    program: &'a mut ProgramBuilder,
    tables: &'a TableReferences,
    seek_def: &'a SeekDef,
    t_ctx: &'a mut TranslateCtx<'plan>,
    seek_cursor_id: usize,
    start_reg: usize,
    loop_end: BranchOffset,
    seek_index: Option<&'a Arc<Index>>,
    is_index: bool,
}

impl<'a, 'plan> SeekEmitter<'a, 'plan> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        program: &'a mut ProgramBuilder,
        tables: &'a TableReferences,
        seek_def: &'a SeekDef,
        t_ctx: &'a mut TranslateCtx<'plan>,
        seek_cursor_id: usize,
        start_reg: usize,
        loop_end: BranchOffset,
        seek_index: Option<&'a Arc<Index>>,
    ) -> Self {
        Self {
            program,
            tables,
            seek_def,
            t_ctx,
            seek_cursor_id,
            start_reg,
            loop_end,
            seek_index,
            is_index: seek_index.is_some(),
        }
    }

    /// Emit the start bound and position the cursor at the first candidate row.
    fn emit_start_bound(&mut self, use_bloom_filter: bool) -> Result<()> {
        if self.seek_def.prefix.is_empty()
            && matches!(self.seek_def.start.last_component, SeekKeyComponent::None)
        {
            match self.seek_def.iter_dir {
                IterationDirection::Forwards => {
                    if self.seek_index.is_some_and(|index| {
                        index.columns[0].effective_nulls_order() == NullsOrder::First
                    }) {
                        self.program.emit_null(self.start_reg, None);
                        self.program.emit_insn(Insn::SeekGT {
                            is_index: self.is_index,
                            cursor_id: self.seek_cursor_id,
                            start_reg: self.start_reg,
                            num_regs: 1,
                            target_pc: self.loop_end,
                        });
                    } else {
                        self.program.emit_insn(Insn::Rewind {
                            cursor_id: self.seek_cursor_id,
                            pc_if_empty: self.loop_end,
                        });
                    }
                }
                IterationDirection::Backwards => {
                    if self.seek_index.is_some_and(|index| {
                        index.columns[0].effective_nulls_order() == NullsOrder::Last
                    }) {
                        self.program.emit_null(self.start_reg, None);
                        self.program.emit_insn(Insn::SeekLT {
                            is_index: self.is_index,
                            cursor_id: self.seek_cursor_id,
                            start_reg: self.start_reg,
                            num_regs: 1,
                            target_pc: self.loop_end,
                        });
                    } else {
                        self.program.emit_insn(Insn::Last {
                            cursor_id: self.seek_cursor_id,
                            pc_if_empty: self.loop_end,
                        });
                    }
                }
            }
            return Ok(());
        }

        for (i, key) in self.seek_def.iter(&self.seek_def.start).enumerate() {
            let reg = self.start_reg + i;
            match key {
                SeekKeyComponent::Expr(expr) => {
                    translate_expr_no_constant_opt(
                        self.program,
                        Some(self.tables),
                        expr,
                        reg,
                        &self.t_ctx.resolver,
                        NoConstantOptReason::RegisterReuse,
                    )?;
                    // A NULL key can never satisfy `=`, so the loop is done as
                    // soon as one shows up. `IS` matches NULL instead: keep the
                    // NULL in the seek register and let the index comparison
                    // find the rows whose key component is NULL.
                    if !expr.is_nonnull(self.tables)
                        && !self.seek_def.is_null_matching_key_component(i)
                    {
                        self.program.emit_insn(Insn::IsNull {
                            reg,
                            target_pc: self.loop_end,
                        });
                    }
                }
                SeekKeyComponent::Null => self.program.emit_null(reg, None),
                SeekKeyComponent::None => {
                    unreachable!("None component is not possible in iterator")
                }
            }
        }
        let num_regs = self.seek_def.size(&self.seek_def.start);
        // Which key components match NULL rather than comparing with `=`; the
        // seek and the bloom-filter probe keep their "NULL key cannot match"
        // shortcut for the rest.
        let mut null_matching_bits = BitSet::default();
        for i in 0..num_regs {
            if self.seek_def.is_null_matching_key_component(i) {
                null_matching_bits.set(i)?;
            }
        }
        let null_matching_mask = NullMatchingMask::from(null_matching_bits);

        if let Some(idx) = self.seek_index {
            let key_exprs: Vec<Option<&ast::Expr>> = self
                .seek_def
                .iter(&self.seek_def.start)
                .map(|key| match key {
                    SeekKeyComponent::Expr(expr) => Some(expr),
                    SeekKeyComponent::Null | SeekKeyComponent::None => None,
                })
                .collect();
            emit_typed_seek_keys(
                self.program,
                self.tables,
                idx,
                self.start_reg,
                &key_exprs,
                0,
                self.seek_def.prefix.len(),
                self.loop_end,
                &self.t_ctx.resolver,
            )?;
            let affinities = index_seek_affinities(self.seek_def, &self.seek_def.start);
            if affinities.chars().any(|c| c != affinity::SQLITE_AFF_BLOB) {
                self.program.emit_insn(Insn::Affinity {
                    start_reg: self.start_reg,
                    count: std::num::NonZeroUsize::new(num_regs).unwrap(),
                    affinities,
                });
            }
            if use_bloom_filter {
                turso_assert!(
                    idx.ephemeral,
                    "bloom filter can only be used with ephemeral indexes"
                );
                // The probe treats a NULL key as "definitely absent", which
                // would skip rows whose key IS NULL. `emit_autoindex` never
                // builds a filter for a NULL-matching seek, so probing one
                // here means the build and probe decisions have diverged.
                turso_assert!(
                    null_matching_mask.is_empty(),
                    "a NULL-matching seek must not probe a bloom filter"
                );
                self.program.emit_insn(Insn::Filter {
                    cursor_id: self.seek_cursor_id,
                    key_reg: self.start_reg,
                    num_keys: num_regs,
                    target_pc: self.loop_end,
                });
            }
        }

        match self.seek_def.start.op {
            SeekOp::GE { eq_only } => self.program.emit_insn(Insn::SeekGE {
                is_index: self.is_index,
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: self.loop_end,
                eq_only,
                null_matching_mask,
            }),
            SeekOp::GT => self.program.emit_insn(Insn::SeekGT {
                is_index: self.is_index,
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: self.loop_end,
            }),
            SeekOp::LE { eq_only } => self.program.emit_insn(Insn::SeekLE {
                is_index: self.is_index,
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: self.loop_end,
                eq_only,
                null_matching_mask,
            }),
            SeekOp::LT => self.program.emit_insn(Insn::SeekLT {
                is_index: self.is_index,
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: self.loop_end,
            }),
        };

        Ok(())
    }

    /// Emit the end bound check and anchor the loop-start label.
    fn emit_termination(&mut self, loop_start: BranchOffset) -> Result<()> {
        if self.seek_def.prefix.is_empty()
            && matches!(self.seek_def.end.last_component, SeekKeyComponent::None)
        {
            self.program.preassign_label_to_next_insn(loop_start);
            match self.seek_def.iter_dir {
                IterationDirection::Forwards => {
                    if self.seek_index.is_some_and(|index| {
                        index.columns[0].effective_nulls_order() == NullsOrder::Last
                    }) {
                        self.program.emit_null(self.start_reg, None);
                        self.program.emit_insn(Insn::IdxGE {
                            cursor_id: self.seek_cursor_id,
                            start_reg: self.start_reg,
                            num_regs: 1,
                            target_pc: self.loop_end,
                        });
                    }
                }
                IterationDirection::Backwards => {
                    if self.seek_index.is_some_and(|index| {
                        index.columns[0].effective_nulls_order() == NullsOrder::First
                    }) {
                        self.program.emit_null(self.start_reg, None);
                        self.program.emit_insn(Insn::IdxLE {
                            cursor_id: self.seek_cursor_id,
                            start_reg: self.start_reg,
                            num_regs: 1,
                            target_pc: self.loop_end,
                        });
                    }
                }
            }
            return Ok(());
        }

        let num_regs = self.seek_def.size(&self.seek_def.end);
        let last_reg = self.start_reg + self.seek_def.prefix.len();
        match &self.seek_def.end.last_component {
            SeekKeyComponent::Expr(expr) => {
                translate_expr_no_constant_opt(
                    self.program,
                    Some(self.tables),
                    expr,
                    last_reg,
                    &self.t_ctx.resolver,
                    NoConstantOptReason::RegisterReuse,
                )?;
                if let Some(idx) = self.seek_index {
                    emit_typed_seek_keys(
                        self.program,
                        self.tables,
                        idx,
                        last_reg,
                        &[Some(expr)],
                        self.seek_def.prefix.len(),
                        0,
                        self.loop_end,
                        &self.t_ctx.resolver,
                    )?;
                    let affinities = index_seek_affinities(self.seek_def, &self.seek_def.end);
                    if affinities.chars().any(|c| c != affinity::SQLITE_AFF_BLOB) {
                        self.program.emit_insn(Insn::Affinity {
                            start_reg: self.start_reg,
                            count: std::num::NonZeroUsize::new(num_regs).unwrap(),
                            affinities,
                        });
                    }
                }
                if !expr.is_nonnull(self.tables) {
                    self.program.emit_insn(Insn::IsNull {
                        reg: last_reg,
                        target_pc: self.loop_end,
                    });
                }
            }
            SeekKeyComponent::Null => self.program.emit_null(last_reg, None),
            SeekKeyComponent::None => {}
        }

        self.program.preassign_label_to_next_insn(loop_start);
        let mut rowid_reg = None;
        let mut affinity = None;
        if !self.is_index {
            rowid_reg = Some(self.program.alloc_register());
            self.program.emit_insn(Insn::RowId {
                cursor_id: self.seek_cursor_id,
                dest: rowid_reg.unwrap(),
            });

            affinity = if let Some(table_ref) = self
                .tables
                .joined_tables()
                .iter()
                .find(|t| t.columns().iter().any(|c| c.is_rowid_alias()))
            {
                if let Some(rowid_col_idx) =
                    table_ref.columns().iter().position(|c| c.is_rowid_alias())
                {
                    Some(table_ref.columns()[rowid_col_idx].affinity())
                } else {
                    Some(Affinity::Numeric)
                }
            } else {
                Some(Affinity::Numeric)
            };
        }

        match (self.is_index, self.seek_def.end.op) {
            (true, SeekOp::GE { .. }) => self.program.emit_insn(Insn::IdxGE {
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: self.loop_end,
            }),
            (true, SeekOp::GT) => self.program.emit_insn(Insn::IdxGT {
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: self.loop_end,
            }),
            (true, SeekOp::LE { .. }) => self.program.emit_insn(Insn::IdxLE {
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: self.loop_end,
            }),
            (true, SeekOp::LT) => self.program.emit_insn(Insn::IdxLT {
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: self.loop_end,
            }),
            (false, SeekOp::GE { .. }) => self.program.emit_insn(Insn::Ge {
                lhs: rowid_reg.unwrap(),
                rhs: self.start_reg,
                target_pc: self.loop_end,
                flags: CmpInsFlags::default()
                    .jump_if_null()
                    .with_affinity(affinity.unwrap()),
                collation: self.program.curr_collation(),
            }),
            (false, SeekOp::GT) => self.program.emit_insn(Insn::Gt {
                lhs: rowid_reg.unwrap(),
                rhs: self.start_reg,
                target_pc: self.loop_end,
                flags: CmpInsFlags::default()
                    .jump_if_null()
                    .with_affinity(affinity.unwrap()),
                collation: self.program.curr_collation(),
            }),
            (false, SeekOp::LE { .. }) => self.program.emit_insn(Insn::Le {
                lhs: rowid_reg.unwrap(),
                rhs: self.start_reg,
                target_pc: self.loop_end,
                flags: CmpInsFlags::default()
                    .jump_if_null()
                    .with_affinity(affinity.unwrap()),
                collation: self.program.curr_collation(),
            }),
            (false, SeekOp::LT) => self.program.emit_insn(Insn::Lt {
                lhs: rowid_reg.unwrap(),
                rhs: self.start_reg,
                target_pc: self.loop_end,
                flags: CmpInsFlags::default()
                    .jump_if_null()
                    .with_affinity(affinity.unwrap()),
                collation: self.program.curr_collation(),
            }),
        }
        Ok(())
    }

    pub(super) fn emit(mut self, loop_start: BranchOffset, use_bloom_filter: bool) -> Result<()> {
        self.emit_start_bound(use_bloom_filter)?;
        self.emit_termination(loop_start)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_typed_seek_keys(
    program: &mut ProgramBuilder,
    tables: &TableReferences,
    seek_index: &Arc<Index>,
    start_reg: usize,
    key_exprs: &[Option<&ast::Expr>],
    idx_col_offset: usize,
    first_range_bound: usize,
    no_match: BranchOffset,
    resolver: &Resolver<'_>,
) -> crate::Result<()> {
    let Some(table) = tables
        .find_table_by_identifier(&seek_index.table_name)
        .or_else(|| tables.find_table_by_table_name(&seek_index.table_name))
    else {
        return Ok(());
    };
    let env = TypeEnv::from_resolver(resolver);
    for (i, key_expr) in key_exprs.iter().enumerate() {
        let Some(idx_col) = seek_index.columns.get(idx_col_offset + i) else {
            break;
        };
        if idx_col.expr.is_some() {
            continue;
        }
        let Some(column) = table.columns().get(idx_col.pos_in_table) else {
            continue;
        };
        let Some(column_type) = column_named_type(column, table.is_strict(), resolver.schema())
        else {
            continue;
        };
        if i >= first_range_bound {
            emit_typed_range_bound(
                program,
                tables,
                start_reg + i,
                column_type,
                *key_expr,
                &env,
                resolver,
            )?;
            continue;
        }
        emit_typed_seek_key(
            program,
            tables,
            start_reg + i,
            column,
            column_type,
            *key_expr,
            no_match,
            &env,
            resolver,
        )?;
    }
    Ok(())
}

fn emit_typed_range_bound(
    program: &mut ProgramBuilder,
    tables: &TableReferences,
    key_reg: usize,
    column_type: NamedType,
    key_expr: Option<&ast::Expr>,
    env: &TypeEnv,
    resolver: &Resolver<'_>,
) -> crate::Result<()> {
    let key_type = match key_expr {
        Some(expr) => expr_type(expr, Some(tables), env),
        None => ExprType::Dynamic(DynamicClass::Unknown),
    };
    let key = type_key(&column_type, resolver.schema())?;
    let conversion = resolve_typed_operator(
        &ast::Operator::Less,
        [(&ExprType::Named(column_type), None), (&key_type, key_expr)],
        env,
    )?
    .map(|resolved| resolved.conversions[1].clone())
    .unwrap_or(Conversion::None);
    emit_value_conversion_in_place(program, key_reg, &conversion, resolver)?;
    if let Some(key) = key {
        emit_type_key(program, key_reg, &key, resolver)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_typed_seek_key(
    program: &mut ProgramBuilder,
    tables: &TableReferences,
    key_reg: usize,
    column: &crate::schema::Column,
    column_type: NamedType,
    key_expr: Option<&ast::Expr>,
    no_match: BranchOffset,
    env: &TypeEnv,
    resolver: &Resolver<'_>,
) -> crate::Result<()> {
    let key_type = match key_expr {
        Some(expr) => expr_type(expr, Some(tables), env),
        None => ExprType::Dynamic(DynamicClass::Unknown),
    };
    let column_type = ExprType::Named(column_type);
    let conversion = resolve_typed_operator(
        &ast::Operator::Equals,
        [(&column_type, None), (&key_type, key_expr)],
        env,
    )?
    .map(|resolved| resolved.conversions[1].clone())
    .unwrap_or(Conversion::None);
    let converted_reg = emit_value_conversion(program, key_reg, &conversion, resolver)?;
    let Some(resolved_type) = resolver.schema().column_resolved_type(column, true)? else {
        return Err(crate::LimboError::InternalError(format!(
            "column type {} is not registered",
            column.ty_str
        )));
    };
    if resolved_type.chain.iter().all(|def| def.encode().is_none()) {
        if converted_reg != key_reg {
            program.emit_insn(Insn::Copy {
                src_reg: converted_reg,
                dst_reg: key_reg,
                extra_amount: 0,
            });
        }
        return Ok(());
    }
    let value_reg = program.alloc_register();
    program.emit_insn(Insn::Copy {
        src_reg: converted_reg,
        dst_reg: value_reg,
        extra_amount: 0,
    });
    if converted_reg != key_reg {
        program.emit_insn(Insn::Copy {
            src_reg: converted_reg,
            dst_reg: key_reg,
            extra_amount: 0,
        });
    }
    let done = program.allocate_label();
    let mismatch = program.allocate_label();
    program.emit_insn(Insn::IsNull {
        reg: key_reg,
        target_pc: done,
    });
    program.emit_insn(Insn::ErrorTarget {
        target_pc: mismatch,
    });
    let previous_raise_target = program.raise_target.replace(mismatch);
    let encoded = resolved_type
        .chain
        .iter()
        .try_for_each(|def| match def.encode() {
            Some(encode) => {
                emit_type_expr(program, encode, key_reg, key_reg, column, def, resolver).map(|_| ())
            }
            None => Ok(()),
        });
    program.raise_target = previous_raise_target;
    encoded?;
    program.emit_insn(Insn::ClearErrorTarget);
    let decoded_reg = program.alloc_register();
    emit_user_facing_column_value(program, key_reg, decoded_reg, column, true, resolver)?;
    emit_jump_if_type_equal(
        program,
        &column_type,
        decoded_reg,
        value_reg,
        done,
        env,
        resolver,
    )?;
    program.preassign_label_to_next_insn(mismatch);
    program.emit_insn(Insn::ClearErrorTarget);
    program.emit_insn(Insn::Goto {
        target_pc: no_match,
    });
    program.preassign_label_to_next_insn(done);
    Ok(())
}

fn emit_jump_if_type_equal(
    program: &mut ProgramBuilder,
    value_type: &ExprType,
    lhs: usize,
    rhs: usize,
    target_pc: BranchOffset,
    env: &TypeEnv,
    resolver: &Resolver<'_>,
) -> crate::Result<()> {
    let equality = resolve_typed_operator(
        &ast::Operator::Equals,
        [(value_type, None), (value_type, None)],
        env,
    )?;
    let Some(func_name) = equality.and_then(|resolved| resolved.function) else {
        program.emit_insn(Insn::Eq {
            lhs,
            rhs,
            target_pc,
            flags: CmpInsFlags::default(),
            collation: None,
        });
        return Ok(());
    };
    let func = resolver
        .resolve_function(&func_name, 2)?
        .ok_or_else(|| crate::LimboError::ParseError(format!("no such function: {func_name}")))?;
    let args = program.alloc_registers(2);
    program.emit_insn(Insn::Copy {
        src_reg: lhs,
        dst_reg: args,
        extra_amount: 0,
    });
    program.emit_insn(Insn::Copy {
        src_reg: rhs,
        dst_reg: args + 1,
        extra_amount: 0,
    });
    let result = program.alloc_register();
    program.emit_insn(Insn::Function {
        constant_mask: 0,
        start_reg: args,
        dest: result,
        func: crate::function::FuncCtx { func, arg_count: 2 },
    });
    program.emit_insn(Insn::If {
        reg: result,
        target_pc,
        jump_if_null: false,
    });
    Ok(())
}
