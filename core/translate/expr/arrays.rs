use super::*;
use crate::vdbe::insn::ArrayEncodeData;

/// Maximum number of array elements supported in the per-element transform loop.
/// Limited by the fixed register block allocated at compile time.
const MAX_ARRAY_LOOP_ELEMENTS: usize = 1024;

/// Emit a per-element transform loop on an array blob in `reg`.
/// Extracts each element, applies `transform_expr` via emit_type_expr,
/// stores results into contiguous registers, then rebuilds the blob with
/// MakeArrayDynamic. O(N) instead of O(N²) ArraySetElement per iteration.
pub(super) fn emit_array_element_loop(
    program: &mut ProgramBuilder,
    reg: usize,
    transform_expr: &ast::Expr,
    col: &Column,
    type_def: &TypeDef,
    resolver: &Resolver,
) -> Result<()> {
    let reg_len = program.alloc_register();
    let reg_idx = program.alloc_register();
    let reg_elem = program.alloc_register();
    // Reserve a contiguous block for transformed elements.
    // At runtime, we only use registers[elem_base..elem_base+len].
    let elem_base = program.alloc_registers(MAX_ARRAY_LOOP_ELEMENTS);

    program.emit_insn(Insn::ArrayLength { reg, dest: reg_len });

    // Guard: halt if the array exceeds the register block size.
    let max_reg = program.alloc_register();
    program.emit_insn(Insn::Integer {
        value: MAX_ARRAY_LOOP_ELEMENTS as i64,
        dest: max_reg,
    });
    let ok_label = program.allocate_label();
    program.emit_insn(Insn::Le {
        lhs: reg_len,
        rhs: max_reg,
        target_pc: ok_label,
        flags: CmpInsFlags::default(),
        collation: None,
    });
    program.emit_insn(Insn::Halt {
        err_code: SQLITE_CONSTRAINT,
        description: format!(
            "array exceeds maximum element count for custom type transform ({MAX_ARRAY_LOOP_ELEMENTS})"
        ),
        on_error: None,
        description_reg: None,
    });
    program.preassign_label_to_next_insn(ok_label);
    // reg_idx is the 1-based array index for ArrayElement (PG convention)
    program.emit_insn(Insn::Integer {
        value: 1,
        dest: reg_idx,
    });
    // reg_offset is the 0-based offset for RegCopyOffset into the register block
    let reg_offset = program.alloc_register();
    program.emit_insn(Insn::Integer {
        value: 0,
        dest: reg_offset,
    });

    let loop_start = program.allocate_label();
    let loop_end_label = program.allocate_label();
    program.preassign_label_to_next_insn(loop_start);

    program.emit_insn(Insn::Gt {
        lhs: reg_idx,
        rhs: reg_len,
        target_pc: loop_end_label,
        flags: CmpInsFlags::default(),
        collation: None,
    });

    // Extract element from record blob (1-based index)
    program.emit_insn(Insn::ArrayElement {
        array_reg: reg,
        index_reg: reg_idx,
        dest: reg_elem,
    });

    // Apply per-element transform expression
    emit_type_expr(
        program,
        transform_expr,
        reg_elem,
        reg_elem,
        col,
        type_def,
        resolver,
    )?;

    // Store transformed element into contiguous register block at 0-based offset
    program.emit_insn(Insn::RegCopyOffset {
        src: reg_elem,
        base: elem_base,
        offset_reg: reg_offset,
    });

    program.emit_insn(Insn::AddImm {
        register: reg_idx,
        value: 1,
    });
    program.emit_insn(Insn::AddImm {
        register: reg_offset,
        value: 1,
    });
    program.emit_insn(Insn::Goto {
        target_pc: loop_start,
    });

    program.preassign_label_to_next_insn(loop_end_label);

    // Rebuild the array blob from the contiguous register block in one pass
    program.emit_insn(Insn::MakeArrayDynamic {
        start_reg: elem_base,
        count_reg: reg_len,
        dest: reg,
    });

    Ok(())
}

/// Emit bytecode to encode an array value: parse JSON text input, validate/coerce
/// elements, and serialize to a native record-format BLOB.
/// For custom element types with encode expressions, a per-element bytecode loop
/// normalizes input to blob, applies encode per element, then rebuilds the blob.
pub(super) fn emit_array_encode(
    program: &mut ProgramBuilder,
    reg: usize,
    col: &Column,
    resolver: &Resolver,
    table_name: &str,
) -> Result<()> {
    if let Some(type_def) = resolver.schema().column_type_def(col, true) {
        if let Some(encode_expr) = type_def.encode() {
            // Normalize input (text or blob) to blob with ANY affinity first
            program.emit_insn(Insn::ArrayEncode {
                data: Box::new(ArrayEncodeData {
                    reg,
                    element_affinity: Affinity::Blob,
                    element_type: "ANY".into(),
                    table_name: table_name.into(),
                    col_name: col.name.as_deref().unwrap_or("").into(),
                }),
            });

            emit_array_element_loop(program, reg, encode_expr, col, &type_def, resolver)?;
        }
    }

    // ArrayEncode: parse JSON text → validate/coerce → serialize to record blob.
    // For multi-dimensional arrays (e.g. INTEGER[][]), the outer array's elements
    // are themselves arrays (blobs), so we use ANY/Blob for validation.
    // Only 1-dimensional arrays validate elements against the declared base type.
    let is_any = col.ty_str.eq_ignore_ascii_case("ANY");
    let is_multidim = col.array_dimensions() > 1;
    let col_name = col.name.as_deref().unwrap_or("");
    let element_affinity = if is_any || is_multidim {
        Affinity::Blob
    } else {
        Affinity::affinity(&col.ty_str)
    };
    let element_type = if is_any || is_multidim {
        "ANY".into()
    } else {
        col.ty_str.to_uppercase().into()
    };
    program.emit_insn(Insn::ArrayEncode {
        data: Box::new(ArrayEncodeData {
            reg,
            element_affinity,
            element_type,
            table_name: table_name.into(),
            col_name: col_name.into(),
        }),
    });
    Ok(())
}

/// Emit bytecode to decode an array value: convert record-format BLOB to JSON text.
/// For base element types, this is a single ArrayDecode instruction.
/// For custom element types with decode expressions, a per-element loop
/// extracts elements via ArrayElement, applies decode, then rebuilds the blob.
pub(crate) fn emit_array_decode(
    program: &mut ProgramBuilder,
    reg: usize,
    col: &Column,
    resolver: &Resolver,
) -> Result<()> {
    if let Some(type_def) = resolver.schema().column_type_def(col, true) {
        if let Some(decode_expr) = type_def.decode() {
            emit_array_element_loop(program, reg, decode_expr, col, &type_def, resolver)?;
        }
    }

    // Convert record blob to JSON text for display
    program.emit_insn(Insn::ArrayDecode { reg });
    Ok(())
}

pub(crate) fn emit_key_conversion(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    table: &BTreeTable,
    key_reg: usize,
) -> Result<()> {
    let Some(column) = marked_key_column(table, resolver) else {
        return Ok(());
    };
    if column.notnull() {
        program.emit_insn(Insn::HaltIfNull {
            target_reg: key_reg,
            err_code: crate::error::SQLITE_CONSTRAINT_NOTNULL,
            description: key_description(table, column),
            on_error: None,
        });
    }
    emit_key_assign(program, resolver, table, key_reg, None)?;
    emit_key_encode(program, resolver, table, key_reg, None)
}

pub(crate) fn emit_key_assign(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    table: &BTreeTable,
    key_reg: usize,
    row_number_reg: Option<usize>,
) -> Result<()> {
    let Some(column) = marked_key_column(table, resolver) else {
        return Ok(());
    };
    emit_custom_type_assign_columns(
        program,
        resolver,
        std::slice::from_ref(column),
        key_reg,
        None,
        &ColumnLayout::Identity { column_count: 1 },
        row_number_reg,
    )
}

pub(crate) fn emit_key_encode(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    table: &BTreeTable,
    key_reg: usize,
    row_number_reg: Option<usize>,
) -> Result<()> {
    let Some(column) = marked_key_column(table, resolver) else {
        return Ok(());
    };
    emit_custom_type_encode_columns(
        program,
        resolver,
        std::slice::from_ref(column),
        key_reg,
        None,
        &table.name,
        &ColumnLayout::Identity { column_count: 1 },
        row_number_reg,
    )?;
    let out_of_range = program.allocate_label();
    let done = program.allocate_label();
    program.emit_insn(Insn::IsType {
        reg: key_reg,
        target_pc: out_of_range,
        value_type: crate::types::ValueType::Blob,
    });
    program.emit_insn(Insn::Goto { target_pc: done });
    program.preassign_label_to_next_insn(out_of_range);
    emit_key_error(
        program,
        column,
        row_number_reg,
        "out of range",
        format!("key out of range for {}", key_description(table, column)),
    );
    program.preassign_label_to_next_insn(done);
    Ok(())
}

pub(crate) fn emit_generated_key_range_check(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    table: &BTreeTable,
    key_reg: usize,
    row_number_reg: Option<usize>,
) -> Result<()> {
    let Some(column) = marked_key_column(table, resolver) else {
        return Ok(());
    };
    let largest = resolver
        .schema()
        .largest_key_of_type(&column.ty_str)
        .ok_or_else(|| {
            LimboError::InternalError(format!(
                "the key {} has no integer type",
                key_description(table, column)
            ))
        })?;
    if largest == i64::MAX {
        return Ok(());
    }
    let largest_reg = program.alloc_register();
    program.emit_insn(Insn::Integer {
        value: largest,
        dest: largest_reg,
    });
    let in_range = program.allocate_label();
    program.emit_insn(Insn::Le {
        lhs: key_reg,
        rhs: largest_reg,
        target_pc: in_range,
        flags: CmpInsFlags::default(),
        collation: None,
    });
    emit_key_error(
        program,
        column,
        row_number_reg,
        "no more keys",
        format!("no more keys for {}", key_description(table, column)),
    );
    program.preassign_label_to_next_insn(in_range);
    Ok(())
}

fn marked_key_column<'a>(table: &'a BTreeTable, resolver: &Resolver) -> Option<&'a Column> {
    if !resolver.schema().uses_name_table(table) {
        return None;
    }
    table.get_rowid_alias_column().map(|(_, column)| column)
}

fn key_description(table: &BTreeTable, column: &Column) -> String {
    format!(
        "{}.{}",
        table.name,
        column.name.as_deref().unwrap_or_default()
    )
}

fn emit_key_error(
    program: &mut ProgramBuilder,
    column: &Column,
    row_number_reg: Option<usize>,
    kind: &str,
    message: String,
) {
    if let (Some(row_reg), Some(name)) = (row_number_reg, column.name.as_deref()) {
        program.emit_insn(Insn::TypeConversionContext {
            column: Some(std::sync::Arc::from(name)),
            row_reg,
        });
    }
    let args = program.alloc_registers(2);
    program.emit_insn(Insn::String8 {
        value: kind.to_string(),
        dest: args,
    });
    program.emit_insn(Insn::String8 {
        value: message,
        dest: args + 1,
    });
    let unused = program.alloc_register();
    program.emit_insn(Insn::Function {
        constant_mask: 0,
        start_reg: args,
        dest: unused,
        func: crate::function::FuncCtx {
            func: crate::function::Func::Scalar(crate::function::ScalarFunc::Storage(
                crate::dialect::StorageFunc::ConversionError,
            )),
            arg_count: 2,
        },
    });
}

pub(crate) fn emit_custom_type_assign_columns(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    columns: &[Column],
    start_reg: usize,
    only_columns: Option<&ColumnMask>,
    layout: &ColumnLayout,
    row_number_reg: Option<usize>,
) -> Result<()> {
    let mut named_a_column = false;
    for (i, col) in columns.iter().enumerate() {
        if only_columns.is_some_and(|filter| !filter.get(i)) || col.is_virtual_generated() {
            continue;
        }
        let Ok(Some(resolved)) = resolver.schema().column_resolved_type(col, true) else {
            continue;
        };
        let Some((def, assign)) = resolved
            .chain
            .iter()
            .find_map(|def| def.assign().map(|assign| (def, assign)))
        else {
            continue;
        };
        named_a_column |= emit_conversion_context(program, col, row_number_reg);
        let reg = layout.to_register(start_reg, i);
        let skip_null = program.allocate_label();
        program.emit_insn(Insn::IsNull {
            reg,
            target_pc: skip_null,
        });
        emit_type_expr(program, assign, reg, reg, col, def, resolver)?;
        program.preassign_label_to_next_insn(skip_null);
    }
    if named_a_column {
        emit_conversion_context_end(program, row_number_reg);
    }
    Ok(())
}

/// Emit encode expressions for columns with custom types in a contiguous register range.
/// Used by INSERT, UPDATE, and UPSERT paths to encode values before TypeCheck.
///
/// If `only_columns` is `Some`, only encode columns whose index is in the set.
/// This is needed for UPDATE/UPSERT where non-SET columns are already encoded
/// (read from disk), and re-encoding them would corrupt data.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_custom_type_encode_columns(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    columns: &[Column],
    start_reg: usize,
    only_columns: Option<&ColumnMask>,
    table_name: &str,
    layout: &ColumnLayout,
    row_number_reg: Option<usize>,
) -> Result<()> {
    let mut named_a_column = false;
    for (i, col) in columns.iter().enumerate() {
        if let Some(filter) = only_columns {
            if !filter.get(i) {
                continue;
            }
        }

        let reg = layout.to_register(start_reg, i);

        // Handle array columns: encode input (text or blob) -> record blob for storage
        if col.is_array() {
            let skip_label = program.allocate_label();
            program.emit_insn(Insn::IsNull {
                reg,
                target_pc: skip_label,
            });
            emit_array_encode(program, reg, col, resolver, table_name)?;
            program.preassign_label_to_next_insn(skip_label);
            continue;
        }

        if col.ty_str.is_empty() {
            continue;
        }
        let Ok(Some(resolved)) = resolver.schema().column_resolved_type(col, true) else {
            continue;
        };

        // Check if any type in the chain has not_null — if so, don't skip NULLs
        let any_not_null = resolved.chain.iter().any(|td| td.not_null);

        let skip_label = if !any_not_null {
            // Skip NULL values: jump over encode if NULL
            let label = program.allocate_label();
            program.emit_insn(Insn::IsNull {
                reg,
                target_pc: label,
            });
            Some(label)
        } else {
            None
        };

        if resolved.chain.iter().any(|td| td.encode().is_some()) {
            named_a_column |= emit_conversion_context(program, col, row_number_reg);
        }

        // Apply encode for each type in the chain (child first, then parent)
        for td in &resolved.chain {
            if let Some(encode_expr) = td.encode() {
                emit_type_expr(program, encode_expr, reg, reg, col, td, resolver)?;
            }
        }

        if let Some(label) = skip_label {
            program.preassign_label_to_next_insn(label);
        }
    }
    if named_a_column {
        emit_conversion_context_end(program, row_number_reg);
    }
    Ok(())
}

fn emit_conversion_context(
    program: &mut ProgramBuilder,
    column: &Column,
    row_number_reg: Option<usize>,
) -> bool {
    let (Some(row_reg), Some(name)) = (row_number_reg, column.name.as_deref()) else {
        return false;
    };
    program.emit_insn(Insn::TypeConversionContext {
        column: Some(std::sync::Arc::from(name)),
        row_reg,
    });
    true
}

fn emit_conversion_context_end(program: &mut ProgramBuilder, row_number_reg: Option<usize>) {
    if let Some(row_reg) = row_number_reg {
        program.emit_insn(Insn::TypeConversionContext {
            column: None,
            row_reg,
        });
    }
}

pub(crate) fn emit_row_number_counter(program: &mut ProgramBuilder) -> usize {
    let reg = program.alloc_register();
    program.emit_insn(Insn::Integer {
        value: 0,
        dest: reg,
    });
    program.mark_last_insn_constant();
    program.emit_insn(Insn::AddImm {
        register: reg,
        value: 1,
    });
    reg
}

/// Emit decode expressions for columns with custom types in a contiguous register range.
/// Used by the UPSERT path to decode values that were read from disk (encoded) so that
/// WHERE/SET expressions in DO UPDATE see user-facing values.
///
/// If `only_columns` is `Some`, only decode columns whose index is in the set.
pub(crate) fn emit_custom_type_decode_columns(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    columns: &[Column],
    start_reg: usize,
    only_columns: Option<&ColumnMask>,
    layout: &ColumnLayout,
) -> Result<()> {
    for (i, col) in columns.iter().enumerate() {
        if let Some(filter) = only_columns {
            if !filter.get(i) {
                continue;
            }
        }

        let reg = layout.to_register(start_reg, i);

        // Handle array columns: decode record blob -> JSON text for display
        if col.is_array() {
            let skip_label = program.allocate_label();
            program.emit_insn(Insn::IsNull {
                reg,
                target_pc: skip_label,
            });
            emit_array_decode(program, reg, col, resolver)?;
            program.preassign_label_to_next_insn(skip_label);
            continue;
        }

        if col.ty_str.is_empty() {
            continue;
        }
        let Ok(Some(resolved)) = resolver.schema().column_resolved_type(col, true) else {
            continue;
        };

        // Skip NULL values: jump over decode if NULL
        let skip_label = program.allocate_label();
        program.emit_insn(Insn::IsNull {
            reg,
            target_pc: skip_label,
        });

        // Apply decode in reverse order (parent/ancestor first, then child)
        for td in resolved.chain.iter().rev() {
            if let Some(decode_expr) = td.decode() {
                emit_type_expr(program, decode_expr, reg, reg, col, td, resolver)?;
            }
        }

        program.preassign_label_to_next_insn(skip_label);
    }
    Ok(())
}
