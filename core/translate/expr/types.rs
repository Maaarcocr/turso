use super::*;
use crate::schema::{Schema, TypeDefKind};
use crate::translate::plan::Plan;
use crate::types::ValueType;

#[derive(Debug, Clone)]
pub(crate) enum ExprType {
    Named(NamedType),
    Dynamic(DynamicClass),
}

#[derive(Debug, Clone)]
pub(crate) struct NamedType {
    pub(crate) def: Arc<TypeDef>,
    pub(crate) params: Option<Vec<ast::Expr>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DynamicClass {
    Integer,
    Real,
    Text,
    Blob,
    Unknown,
}

pub(crate) struct TypeEnv<'a> {
    schema: &'a Schema,
    self_table: Option<Arc<BTreeTable>>,
}

impl<'a> TypeEnv<'a> {
    pub(crate) fn new(schema: &'a Schema) -> Self {
        Self {
            schema,
            self_table: None,
        }
    }

    pub(crate) fn from_resolver(resolver: &'a Resolver) -> Self {
        Self {
            schema: resolver.schema(),
            self_table: resolver.self_table_with_custom_type_operators(),
        }
    }
}

impl NamedType {
    fn is(&self, def: &TypeDef) -> bool {
        self.def.name.eq_ignore_ascii_case(&def.name)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedOperator {
    pub(crate) function: Option<String>,
    pub(crate) swap_args: bool,
    pub(crate) negate: bool,
    pub(crate) conversions: [Conversion; 2],
    pub(crate) own_operator_of: Option<Arc<TypeDef>>,
    pub(crate) result: ExprType,
}

#[derive(Debug, Clone)]
pub(crate) enum Conversion {
    None,
    IntoType(NamedType),
    IntoClass(DynamicClass),
}

impl Conversion {
    pub(crate) fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

pub(crate) fn expr_type(
    expr: &ast::Expr,
    tables: Option<&TableReferences>,
    env: &TypeEnv,
) -> ExprType {
    if env.schema.type_registry.is_empty() {
        return ExprType::Dynamic(DynamicClass::Unknown);
    }
    match expr {
        ast::Expr::Column { table, column, .. } => column_type(*table, *column, tables, env),
        ast::Expr::Cast { type_name, .. } => cast_type(type_name.as_ref(), env),
        ast::Expr::Parenthesized(exprs) if exprs.len() == 1 => expr_type(&exprs[0], tables, env),
        ast::Expr::Collate(inner, _) => expr_type(inner, tables, env),
        ast::Expr::Literal(literal) => ExprType::Dynamic(literal_class(literal)),
        ast::Expr::RowId { .. } => ExprType::Dynamic(DynamicClass::Integer),
        ast::Expr::Binary(lhs, op, rhs) => binary_type(lhs, op, rhs, tables, env),
        ast::Expr::Case {
            when_then_pairs,
            else_expr,
            ..
        } => common_type(
            &case_branches(when_then_pairs, else_expr.as_deref()),
            tables,
            env,
        ),
        ast::Expr::FunctionCall { name, args, .. } => function_call_type(name, args, tables, env),
        _ => ExprType::Dynamic(DynamicClass::Unknown),
    }
}

pub(crate) fn resolve_operator(
    op: &ast::Operator,
    lhs: &ast::Expr,
    rhs: &ast::Expr,
    tables: Option<&TableReferences>,
    env: &TypeEnv,
) -> Result<Option<ResolvedOperator>> {
    if env.schema.type_registry.is_empty() {
        return Ok(None);
    }
    let Some(op) = resolvable_operator(op, lhs, rhs) else {
        return Ok(None);
    };
    let lhs_type = expr_type(lhs, tables, env);
    let rhs_type = expr_type(rhs, tables, env);
    resolve_typed_operator(op, [(&lhs_type, Some(lhs)), (&rhs_type, Some(rhs))], env)
}

fn resolvable_operator<'o>(
    op: &'o ast::Operator,
    lhs: &ast::Expr,
    rhs: &ast::Expr,
) -> Option<&'o ast::Operator> {
    match op {
        ast::Operator::Is | ast::Operator::IsNot => {
            (!is_null_literal(lhs) && !is_null_literal(rhs)).then_some(&ast::Operator::Equals)
        }
        _ => operator_symbol(op).map(|_| op),
    }
}

pub(crate) fn hash_join_can_compare_values(
    lhs: &ast::Expr,
    rhs: &ast::Expr,
    tables: Option<&TableReferences>,
    env: &TypeEnv,
) -> bool {
    match resolve_operator(&ast::Operator::Equals, lhs, rhs, tables, env) {
        Ok(None) => true,
        Ok(Some(resolved)) => {
            resolved.function.is_none() && resolved.conversions.iter().all(Conversion::is_none)
        }
        Err(_) => false,
    }
}

pub(crate) fn common_type_conversions(
    exprs: &[&ast::Expr],
    tables: Option<&TableReferences>,
    env: &TypeEnv,
) -> Option<(NamedType, Vec<Conversion>)> {
    if env.schema.type_registry.is_empty() {
        return None;
    }
    let types: Vec<Option<ExprType>> = exprs
        .iter()
        .map(|expr| (!is_null_literal(expr)).then(|| expr_type(expr, tables, env)))
        .collect();
    let mut common: Option<NamedType> = None;
    for named in types.iter().flatten().filter_map(|ty| match ty {
        ExprType::Named(named) => Some(named),
        ExprType::Dynamic(_) => None,
    }) {
        common = match common {
            None => Some(named.clone()),
            Some(current) if current.is(&named.def) => {
                let params = match (current.params, &named.params) {
                    (Some(a), Some(b)) if a == *b => Some(a),
                    _ => None,
                };
                Some(NamedType {
                    def: current.def,
                    params,
                })
            }
            Some(_) => return None,
        };
    }
    let mut common = common?;
    let mut conversions = Vec::with_capacity(types.len());
    for (expr, ty) in exprs.iter().zip(&types) {
        let conversion = match ty {
            None | Some(ExprType::Named(_)) => Conversion::None,
            Some(ExprType::Dynamic(class)) => {
                let converts = match class {
                    DynamicClass::Unknown => matches!(expr, ast::Expr::Variable(_)),
                    _ => declares_cast(
                        &common.def,
                        ast::TypeCastDirection::From,
                        class_name(*class),
                    ),
                };
                if !converts {
                    return None;
                }
                conversion_into(&common.def, Some(&common))?
            }
        };
        conversions.push(conversion);
    }
    if conversions.iter().any(|conversion| !conversion.is_none()) {
        common.params = None;
    }
    Some((common, conversions))
}

pub(crate) fn resolve_typed_operator(
    op: &ast::Operator,
    operands: [(&ExprType, Option<&ast::Expr>); 2],
    env: &TypeEnv,
) -> Result<Option<ResolvedOperator>> {
    let Some(symbol) = operator_symbol(op) else {
        return Ok(None);
    };
    let owners = named_owners(&operands);
    if owners.is_empty() {
        return Ok(None);
    }
    let mut candidates = Vec::new();
    for owner in &owners {
        for signature in signatures_for(owner, op, symbol, env) {
            if let Some(candidate) = fit_signature(&signature, &operands) {
                candidates.push(candidate);
            }
        }
    }
    if let Some(best) = choose_candidate(candidates, &operands)? {
        return Ok(Some(best));
    }
    Ok(cast_to_class_fallback(op, &operands))
}

pub(crate) fn emit_value_conversion(
    program: &mut ProgramBuilder,
    source_reg: usize,
    conversion: &Conversion,
    resolver: &Resolver,
) -> Result<usize> {
    match conversion {
        Conversion::None => Ok(source_reg),
        Conversion::IntoClass(class) => {
            let dest = program.alloc_register();
            program.emit_insn(Insn::Copy {
                src_reg: source_reg,
                dst_reg: dest,
                extra_amount: 0,
            });
            program.emit_insn(Insn::Cast {
                reg: dest,
                affinity: class_affinity(*class),
            });
            Ok(dest)
        }
        Conversion::IntoType(named) => {
            let dest = program.alloc_register();
            emit_conversion_into_type(program, source_reg, dest, named, resolver)?;
            Ok(dest)
        }
    }
}

pub(crate) fn emit_value_conversion_in_place(
    program: &mut ProgramBuilder,
    reg: usize,
    conversion: &Conversion,
    resolver: &Resolver,
) -> Result<()> {
    match conversion {
        Conversion::None => Ok(()),
        Conversion::IntoClass(class) => {
            program.emit_insn(Insn::Cast {
                reg,
                affinity: class_affinity(*class),
            });
            Ok(())
        }
        Conversion::IntoType(named) => {
            emit_conversion_into_type(program, reg, reg, named, resolver)
        }
    }
}

pub(crate) fn branch_conversions(
    branches: &[&ast::Expr],
    tables: Option<&TableReferences>,
    resolver: &Resolver,
) -> Option<Vec<Conversion>> {
    let env = TypeEnv::from_resolver(resolver);
    let (_, conversions) = common_type_conversions(branches, tables, &env)?;
    conversions
        .iter()
        .any(|conversion| !conversion.is_none())
        .then_some(conversions)
}

pub(crate) fn emit_conversion_into_type(
    program: &mut ProgramBuilder,
    source_reg: usize,
    dest_reg: usize,
    named: &NamedType,
    resolver: &Resolver,
) -> Result<()> {
    if source_reg != dest_reg {
        program.emit_insn(Insn::Copy {
            src_reg: source_reg,
            dst_reg: dest_reg,
            extra_amount: 0,
        });
    }
    let done = program.allocate_label();
    program.emit_insn(Insn::IsNull {
        reg: dest_reg,
        target_pc: done,
    });
    let column = conversion_column(named);
    let input_type = named.def.value_input_type().to_uppercase();
    if let Some(expected) = strict_value_type(&input_type) {
        let input_ok = program.allocate_label();
        program.emit_column_affinity(dest_reg, Affinity::affinity(&input_type));
        program.emit_insn(Insn::IsType {
            reg: dest_reg,
            target_pc: input_ok,
            value_type: expected,
        });
        program.emit_insn(Insn::Halt {
            err_code: crate::error::SQLITE_CONSTRAINT,
            description: format!("invalid input for type {}", named.def.name),
            on_error: None,
            description_reg: None,
        });
        program.preassign_label_to_next_insn(input_ok);
    }
    let Some(resolved) = resolver.schema().resolve_type_unchecked(&named.def.name)? else {
        return Err(LimboError::InternalError(format!(
            "type {} is not registered",
            named.def.name
        )));
    };
    for def in &resolved.chain {
        if let Some(encode) = def.encode() {
            emit_type_expr(program, encode, dest_reg, dest_reg, &column, def, resolver)?;
        }
    }
    for def in resolved.chain.iter().rev() {
        if let Some(decode) = def.decode() {
            emit_type_expr(program, decode, dest_reg, dest_reg, &column, def, resolver)?;
        }
    }
    program.preassign_label_to_next_insn(done);
    Ok(())
}

pub(crate) fn conversion_column(named: &NamedType) -> Column {
    let mut column = Column::new(
        None,
        named.def.name.clone(),
        None,
        None,
        Type::Null,
        None,
        ColDef::default(),
    );
    column.ty_params = named
        .params
        .iter()
        .flatten()
        .map(|param| Box::new(param.clone()))
        .collect();
    column
}

fn column_type(
    table_id: ast::TableInternalId,
    column: usize,
    tables: Option<&TableReferences>,
    env: &TypeEnv,
) -> ExprType {
    let found = tables.and_then(|tables| tables.find_table_by_internal_id(table_id));
    match found {
        Some((_, Table::FromClauseSubquery(subquery))) => {
            subquery_column_type(&subquery.plan, column, env)
        }
        Some((_, table)) => match table.get_column_at(column) {
            Some(col) => stored_column_type(col, table.is_strict(), env),
            None => ExprType::Dynamic(DynamicClass::Unknown),
        },
        None if table_id.is_self_table() => match &env.self_table {
            Some(table) => match table.columns().get(column) {
                Some(col) => stored_column_type(col, table.is_strict, env),
                None => ExprType::Dynamic(DynamicClass::Unknown),
            },
            None => ExprType::Dynamic(DynamicClass::Unknown),
        },
        None => ExprType::Dynamic(DynamicClass::Unknown),
    }
}

fn stored_column_type(column: &Column, is_strict: bool, env: &TypeEnv) -> ExprType {
    match env.schema.get_type_def(&column.ty_str, is_strict) {
        Some(def) if !column.is_array() => ExprType::Named(NamedType {
            def: Arc::clone(def),
            params: Some(
                column
                    .ty_params
                    .iter()
                    .map(|param| (**param).clone())
                    .collect(),
            ),
        }),
        _ => ExprType::Dynamic(affinity_class(column.affinity())),
    }
}

fn subquery_column_type(plan: &Plan, column: usize, env: &TypeEnv) -> ExprType {
    let Plan::Select(select) = plan else {
        return ExprType::Dynamic(DynamicClass::Unknown);
    };
    match select.result_columns.get(column) {
        Some(result_column) => expr_type(&result_column.expr, Some(&select.table_references), env),
        None => ExprType::Dynamic(DynamicClass::Unknown),
    }
}

fn cast_type(type_name: Option<&ast::Type>, env: &TypeEnv) -> ExprType {
    let Some(type_name) = type_name else {
        return ExprType::Dynamic(DynamicClass::Unknown);
    };
    let name = crate::util::dequote(&type_name.name);
    if type_name.array_dimensions > 0 {
        return ExprType::Dynamic(affinity_class(Affinity::of_written_type(&name)));
    }
    let params = cast_type_params(type_name);
    match env.schema.get_type_def_unchecked(&name) {
        Some(def) if is_typed_cast(def, params.len()) => ExprType::Named(NamedType {
            def: Arc::clone(def),
            params: Some(params),
        }),
        _ => ExprType::Dynamic(affinity_class(Affinity::of_written_type(&name))),
    }
}

fn cast_type_params(type_name: &ast::Type) -> Vec<ast::Expr> {
    match &type_name.size {
        Some(ast::TypeSize::MaxSize(size)) => vec![(**size).clone()],
        Some(ast::TypeSize::TypeSize(precision, scale)) => {
            vec![(**precision).clone(), (**scale).clone()]
        }
        None => Vec::new(),
    }
}

fn is_typed_cast(def: &TypeDef, param_count: usize) -> bool {
    if def.is_domain {
        return true;
    }
    let user_param_count = def.user_params().count();
    user_param_count == 0 || param_count == user_param_count
}

fn binary_type(
    lhs: &ast::Expr,
    op: &ast::Operator,
    rhs: &ast::Expr,
    tables: Option<&TableReferences>,
    env: &TypeEnv,
) -> ExprType {
    let Some(resolvable) = resolvable_operator(op, lhs, rhs) else {
        return untyped_operator_type(op);
    };
    let lhs_type = expr_type(lhs, tables, env);
    let rhs_type = expr_type(rhs, tables, env);
    if let Ok(Some(resolved)) = resolve_typed_operator(
        resolvable,
        [(&lhs_type, Some(lhs)), (&rhs_type, Some(rhs))],
        env,
    ) {
        return resolved.result;
    }
    match op {
        ast::Operator::Add
        | ast::Operator::Subtract
        | ast::Operator::Multiply
        | ast::Operator::Divide
        | ast::Operator::Modulus => {
            let class_of = |operand_type: ExprType| match operand_type {
                ExprType::Dynamic(class) => class,
                ExprType::Named(_) => DynamicClass::Unknown,
            };
            ExprType::Dynamic(arithmetic_class(class_of(lhs_type), class_of(rhs_type)))
        }
        _ => untyped_operator_type(op),
    }
}

fn untyped_operator_type(op: &ast::Operator) -> ExprType {
    match op {
        ast::Operator::Concat => ExprType::Dynamic(DynamicClass::Text),
        _ if op.is_comparison()
            || matches!(
                op,
                ast::Operator::And
                    | ast::Operator::Or
                    | ast::Operator::Is
                    | ast::Operator::IsNot
                    | ast::Operator::BitwiseAnd
                    | ast::Operator::BitwiseOr
                    | ast::Operator::LeftShift
                    | ast::Operator::RightShift
            ) =>
        {
            ExprType::Dynamic(DynamicClass::Integer)
        }
        _ => ExprType::Dynamic(DynamicClass::Unknown),
    }
}

fn arithmetic_class(lhs: DynamicClass, rhs: DynamicClass) -> DynamicClass {
    match (lhs, rhs) {
        (DynamicClass::Integer, DynamicClass::Integer) => DynamicClass::Integer,
        (DynamicClass::Real, DynamicClass::Integer | DynamicClass::Real)
        | (DynamicClass::Integer, DynamicClass::Real) => DynamicClass::Real,
        _ => DynamicClass::Unknown,
    }
}

fn function_call_type(
    name: &ast::Name,
    args: &[Box<ast::Expr>],
    tables: Option<&TableReferences>,
    env: &TypeEnv,
) -> ExprType {
    use crate::function::ScalarFunc;
    match crate::function::Func::resolve_function(name.as_str(), args.len()) {
        Ok(Some(crate::function::Func::Scalar(
            ScalarFunc::Coalesce | ScalarFunc::IfNull | ScalarFunc::Max | ScalarFunc::Min,
        ))) => {
            let args: Vec<&ast::Expr> = args.iter().map(|arg| arg.as_ref()).collect();
            common_type(&args, tables, env)
        }
        Ok(Some(crate::function::Func::Scalar(ScalarFunc::Iif))) => {
            common_type(&iif_branches(args), tables, env)
        }
        Ok(Some(crate::function::Func::Scalar(ScalarFunc::Nullif))) => {
            expr_type(&args[0], tables, env)
        }
        _ => ExprType::Dynamic(DynamicClass::Unknown),
    }
}

pub(crate) fn case_branches<'e>(
    when_then_pairs: &'e [(Box<ast::Expr>, Box<ast::Expr>)],
    else_expr: Option<&'e ast::Expr>,
) -> Vec<&'e ast::Expr> {
    when_then_pairs
        .iter()
        .map(|(_, then)| then.as_ref())
        .chain(else_expr)
        .collect()
}

pub(crate) fn iif_branches(args: &[Box<ast::Expr>]) -> Vec<&ast::Expr> {
    let mut branches: Vec<&ast::Expr> = args.chunks_exact(2).map(|pair| pair[1].as_ref()).collect();
    if args.len() % 2 != 0 {
        branches.extend(args.last().map(|arg| arg.as_ref()));
    }
    branches
}

fn common_type(exprs: &[&ast::Expr], tables: Option<&TableReferences>, env: &TypeEnv) -> ExprType {
    match common_type_conversions(exprs, tables, env) {
        Some((named, _)) => ExprType::Named(named),
        None => ExprType::Dynamic(DynamicClass::Unknown),
    }
}

fn is_null_literal(expr: &ast::Expr) -> bool {
    matches!(expr, ast::Expr::Literal(ast::Literal::Null))
}

fn literal_class(literal: &ast::Literal) -> DynamicClass {
    match literal {
        ast::Literal::String(_) | ast::Literal::Null => DynamicClass::Unknown,
        ast::Literal::Blob(_) => DynamicClass::Blob,
        _ => {
            match crate::translate::alter::literal_default_value(literal).map(|v| v.value_type()) {
                Ok(ValueType::Integer) => DynamicClass::Integer,
                Ok(ValueType::Float) => DynamicClass::Real,
                Ok(ValueType::Text) => DynamicClass::Text,
                _ => DynamicClass::Unknown,
            }
        }
    }
}

fn affinity_class(affinity: Affinity) -> DynamicClass {
    match affinity {
        Affinity::Integer => DynamicClass::Integer,
        Affinity::Real | Affinity::Numeric => DynamicClass::Real,
        Affinity::Text => DynamicClass::Text,
        Affinity::Blob | Affinity::None => DynamicClass::Blob,
    }
}

fn class_affinity(class: DynamicClass) -> Affinity {
    match class {
        DynamicClass::Integer => Affinity::Integer,
        DynamicClass::Real => Affinity::Real,
        DynamicClass::Text => Affinity::Text,
        DynamicClass::Blob | DynamicClass::Unknown => Affinity::Blob,
    }
}

fn strict_value_type(declared: &str) -> Option<ValueType> {
    match declared {
        "INTEGER" | "INT" => Some(ValueType::Integer),
        "REAL" => Some(ValueType::Float),
        "TEXT" => Some(ValueType::Text),
        "BLOB" => Some(ValueType::Blob),
        _ => None,
    }
}

fn operator_symbol(op: &ast::Operator) -> Option<&'static str> {
    match op {
        ast::Operator::Add => Some("+"),
        ast::Operator::Subtract => Some("-"),
        ast::Operator::Multiply => Some("*"),
        ast::Operator::Divide => Some("/"),
        ast::Operator::Modulus => Some("%"),
        ast::Operator::Less => Some("<"),
        ast::Operator::LessEquals => Some("<="),
        ast::Operator::Greater => Some(">"),
        ast::Operator::GreaterEquals => Some(">="),
        ast::Operator::Equals => Some("="),
        ast::Operator::NotEquals => Some("!="),
        _ => None,
    }
}

fn named_owners(operands: &[(&ExprType, Option<&ast::Expr>); 2]) -> Vec<Arc<TypeDef>> {
    let mut owners: Vec<Arc<TypeDef>> = Vec::new();
    for (operand_type, _) in operands {
        if let ExprType::Named(named) = operand_type {
            if !owners.iter().any(|owner| named.is(owner)) {
                owners.push(Arc::clone(&named.def));
            }
        }
    }
    owners
}

#[derive(Debug, Clone)]
enum Slot {
    Named(Arc<TypeDef>),
    Class(DynamicClass),
    Any,
}

struct Signature {
    owner: Arc<TypeDef>,
    slots: [Slot; 2],
    result: Option<Slot>,
    function: Option<String>,
    swap_args: bool,
    negate: bool,
    is_comparison: bool,
    legacy: bool,
}

fn signatures_for(
    owner: &Arc<TypeDef>,
    op: &ast::Operator,
    symbol: &str,
    env: &TypeEnv,
) -> Vec<Signature> {
    let direct = declared_signatures(owner, symbol, false, false, env);
    if !direct.is_empty() {
        return direct;
    }
    let derived = match op {
        ast::Operator::Greater => declared_signatures(owner, "<", true, false, env),
        ast::Operator::GreaterEquals => declared_signatures(owner, "<", false, true, env),
        ast::Operator::LessEquals => declared_signatures(owner, "<", true, true, env),
        ast::Operator::NotEquals => declared_signatures(owner, "=", false, true, env),
        _ => Vec::new(),
    };
    if derived.is_empty()
        && matches!(op, ast::Operator::Equals | ast::Operator::NotEquals)
        && matches!(owner.kind, TypeDefKind::Custom { .. })
        && !owner.is_domain
    {
        return vec![implicit_equality(owner)];
    }
    derived
}

fn implicit_equality(owner: &Arc<TypeDef>) -> Signature {
    Signature {
        owner: Arc::clone(owner),
        slots: [
            Slot::Named(Arc::clone(owner)),
            Slot::Named(Arc::clone(owner)),
        ],
        result: None,
        function: None,
        swap_args: false,
        negate: false,
        is_comparison: true,
        legacy: true,
    }
}

fn declared_signatures(
    owner: &Arc<TypeDef>,
    symbol: &str,
    swap_args: bool,
    negate: bool,
    env: &TypeEnv,
) -> Vec<Signature> {
    let is_arithmetic = matches!(symbol, "+" | "-" | "*" | "/" | "%");
    let is_comparison = matches!(symbol, "<" | "=");
    let mut signatures = Vec::new();
    for declared in owner.operators().iter().filter(|o| o.op == symbol) {
        let this = || Slot::Named(Arc::clone(owner));
        let signature = |slots: [Slot; 2], result: Option<Slot>, legacy: bool| Signature {
            owner: Arc::clone(owner),
            slots,
            result,
            function: declared.func_name.clone(),
            swap_args,
            negate,
            is_comparison,
            legacy,
        };
        match &declared.operands {
            None => {
                signatures.push(signature([this(), this()], None, true));
                if is_arithmetic && owner.decode_returns_value_unchanged() {
                    signatures.push(signature([this(), Slot::Any], None, true));
                    signatures.push(signature([Slot::Any, this()], None, true));
                }
            }
            Some(operands) if operands.len() == 2 => {
                let (Some(left), Some(right)) =
                    (slot_named(&operands[0], env), slot_named(&operands[1], env))
                else {
                    continue;
                };
                let result = match &declared.returns {
                    Some(returns) => match slot_named(returns, env) {
                        Some(slot) => Some(slot),
                        None => continue,
                    },
                    None => None,
                };
                signatures.push(signature([left, right], result, false));
            }
            Some(_) => {}
        }
    }
    signatures
}

fn slot_named(name: &str, env: &TypeEnv) -> Option<Slot> {
    match name.to_ascii_lowercase().as_str() {
        "any" => Some(Slot::Any),
        "integer" => Some(Slot::Class(DynamicClass::Integer)),
        "real" => Some(Slot::Class(DynamicClass::Real)),
        "text" => Some(Slot::Class(DynamicClass::Text)),
        "blob" => Some(Slot::Class(DynamicClass::Blob)),
        _ => env
            .schema
            .get_type_def_unchecked(name)
            .map(|def| Slot::Named(Arc::clone(def))),
    }
}

enum Fit {
    Exact,
    Any,
    Convert(Conversion),
}

struct Candidate {
    resolved: ResolvedOperator,
    exact: usize,
    any: usize,
    unknown_takes_other_type: bool,
}

fn fit_signature(
    signature: &Signature,
    operands: &[(&ExprType, Option<&ast::Expr>); 2],
) -> Option<Candidate> {
    let other_named = |i: usize| match operands[1 - i].0 {
        ExprType::Named(named) => Some(named),
        ExprType::Dynamic(_) => None,
    };
    let fit = |i: usize| {
        fit_operand(
            operands[i].0,
            operands[i].1,
            &signature.slots[i],
            signature.legacy,
            other_named(i),
        )
    };
    let fits = [fit(0)?, fit(1)?];
    let exact = fits.iter().filter(|fit| matches!(fit, Fit::Exact)).count();
    let any = fits.iter().filter(|fit| matches!(fit, Fit::Any)).count();
    let unknown_takes_other_type = operands.iter().enumerate().any(|(i, (operand, _))| {
        matches!(operand, ExprType::Dynamic(DynamicClass::Unknown))
            && matches!(
                (&signature.slots[i], operands[1 - i].0),
                (Slot::Named(slot), ExprType::Named(other)) if other.is(slot)
            )
    });
    let [left, right] = fits.map(|fit| match fit {
        Fit::Convert(conversion) => conversion,
        Fit::Exact | Fit::Any => Conversion::None,
    });
    let own_operator_of = match &signature.slots {
        [Slot::Named(a), Slot::Named(b)]
            if a.name.eq_ignore_ascii_case(&signature.owner.name)
                && b.name.eq_ignore_ascii_case(&signature.owner.name) =>
        {
            Some(Arc::clone(&signature.owner))
        }
        _ => None,
    };
    Some(Candidate {
        resolved: ResolvedOperator {
            function: signature.function.clone(),
            swap_args: signature.swap_args,
            negate: signature.negate,
            conversions: [left, right],
            own_operator_of,
            result: signature_result(signature),
        },
        exact,
        any,
        unknown_takes_other_type,
    })
}

fn signature_result(signature: &Signature) -> ExprType {
    match &signature.result {
        Some(Slot::Named(def)) => ExprType::Named(NamedType {
            def: Arc::clone(def),
            params: None,
        }),
        Some(Slot::Class(class)) => ExprType::Dynamic(*class),
        Some(Slot::Any) => ExprType::Dynamic(DynamicClass::Unknown),
        None if signature.is_comparison => ExprType::Dynamic(DynamicClass::Integer),
        None => ExprType::Dynamic(DynamicClass::Unknown),
    }
}

fn fit_operand(
    operand: &ExprType,
    operand_expr: Option<&ast::Expr>,
    slot: &Slot,
    legacy: bool,
    other: Option<&NamedType>,
) -> Option<Fit> {
    match slot {
        Slot::Any => Some(Fit::Any),
        Slot::Class(class) => match operand {
            ExprType::Dynamic(operand_class) if operand_class == class => Some(Fit::Exact),
            ExprType::Dynamic(DynamicClass::Unknown) => Some(Fit::Any),
            ExprType::Named(named) if declares_cast_to_class(&named.def, *class) => {
                Some(Fit::Convert(Conversion::IntoClass(*class)))
            }
            _ => None,
        },
        Slot::Named(target) => match operand {
            ExprType::Named(named) if named.is(target) => Some(Fit::Exact),
            ExprType::Named(named)
                if declares_cast(target, ast::TypeCastDirection::From, &named.def.name)
                    || declares_cast(&named.def, ast::TypeCastDirection::To, &target.name) =>
            {
                conversion_into(target, other).map(Fit::Convert)
            }
            ExprType::Dynamic(DynamicClass::Unknown) => {
                conversion_into(target, other).map(Fit::Convert)
            }
            ExprType::Dynamic(class)
                if declares_cast(target, ast::TypeCastDirection::From, class_name(*class)) =>
            {
                conversion_into(target, other).map(Fit::Convert)
            }
            ExprType::Dynamic(_) if legacy && legacy_literal_converts(operand_expr, target) => {
                conversion_into(target, other).map(Fit::Convert)
            }
            ExprType::Named(_) | ExprType::Dynamic(_) => None,
        },
    }
}

fn conversion_into(target: &Arc<TypeDef>, other: Option<&NamedType>) -> Option<Conversion> {
    if target.required_user_param_count() == 0 {
        return Some(Conversion::IntoType(NamedType {
            def: Arc::clone(target),
            params: Some(Vec::new()),
        }));
    }
    let other = other.filter(|other| other.is(target))?;
    let params = other.params.clone()?;
    Some(Conversion::IntoType(NamedType {
        def: Arc::clone(target),
        params: Some(params),
    }))
}

fn legacy_literal_converts(operand_expr: Option<&ast::Expr>, target: &TypeDef) -> bool {
    let Some(ast::Expr::Literal(literal)) = operand_expr else {
        return false;
    };
    let literal_type = match literal {
        ast::Literal::Numeric(s) if s.contains(['.', 'e', 'E']) => "real",
        ast::Literal::Numeric(_) | ast::Literal::True | ast::Literal::False => "integer",
        ast::Literal::String(_) => "text",
        ast::Literal::Blob(_) => "blob",
        _ => return false,
    };
    let input = target.value_input_type();
    input.eq_ignore_ascii_case("any") || input.eq_ignore_ascii_case(literal_type)
}

fn declares_cast(def: &TypeDef, direction: ast::TypeCastDirection, name: &str) -> bool {
    def.casts().iter().any(|cast| {
        cast.direction == direction && cast.types.iter().any(|ty| ty.eq_ignore_ascii_case(name))
    })
}

fn declares_cast_to_class(def: &TypeDef, class: DynamicClass) -> bool {
    declares_cast(def, ast::TypeCastDirection::To, class_name(class))
}

fn class_name(class: DynamicClass) -> &'static str {
    match class {
        DynamicClass::Integer => "integer",
        DynamicClass::Real => "real",
        DynamicClass::Text => "text",
        DynamicClass::Blob => "blob",
        DynamicClass::Unknown => "any",
    }
}

fn choose_candidate(
    candidates: Vec<Candidate>,
    operands: &[(&ExprType, Option<&ast::Expr>); 2],
) -> Result<Option<ResolvedOperator>> {
    let rank = |c: &Candidate| (c.exact, c.any, c.unknown_takes_other_type);
    let Some(best_rank) = candidates.iter().map(rank).max() else {
        return Ok(None);
    };
    let mut best = candidates.into_iter().filter(|c| rank(c) == best_rank);
    let chosen = best.next().expect("the best rank comes from a candidate");
    if best.next().is_some() {
        let describe = |operand: &ExprType| match operand {
            ExprType::Named(named) => named.def.name.clone(),
            ExprType::Dynamic(class) => class_name(*class).to_string(),
        };
        crate::bail_parse_error!(
            "operator is not unique: {} and {}",
            describe(operands[0].0),
            describe(operands[1].0)
        );
    }
    Ok(Some(chosen.resolved))
}

fn cast_to_class_fallback(
    op: &ast::Operator,
    operands: &[(&ExprType, Option<&ast::Expr>); 2],
) -> Option<ResolvedOperator> {
    let mut conversions = [Conversion::None, Conversion::None];
    let mut classes = [DynamicClass::Unknown, DynamicClass::Unknown];
    for (i, (operand, _)) in operands.iter().enumerate() {
        match operand {
            ExprType::Named(named) => {
                let class = [
                    DynamicClass::Integer,
                    DynamicClass::Real,
                    DynamicClass::Text,
                ]
                .into_iter()
                .find(|class| declares_cast_to_class(&named.def, *class))?;
                conversions[i] = Conversion::IntoClass(class);
                classes[i] = class;
            }
            ExprType::Dynamic(class) => classes[i] = *class,
        }
    }
    let result = if op.is_comparison() {
        DynamicClass::Integer
    } else {
        arithmetic_class(classes[0], classes[1])
    };
    Some(ResolvedOperator {
        function: None,
        swap_args: false,
        negate: false,
        conversions,
        own_operator_of: None,
        result: ExprType::Dynamic(result),
    })
}
