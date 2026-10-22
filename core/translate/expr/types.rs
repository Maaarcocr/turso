use super::*;
use crate::schema::Schema;
use crate::translate::plan::Plan;

#[derive(Debug, Clone)]
pub(crate) enum ExprType {
    Named(NamedType),
    Dynamic,
}

#[derive(Debug, Clone)]
pub(crate) struct NamedType {
    pub(crate) def: Arc<TypeDef>,
    pub(crate) params: Option<Vec<ast::Expr>>,
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
}

pub(crate) fn expr_type(
    expr: &ast::Expr,
    tables: Option<&TableReferences>,
    env: &TypeEnv,
) -> ExprType {
    if env.schema.type_registry.is_empty() {
        return ExprType::Dynamic;
    }
    match expr {
        ast::Expr::Column { table, column, .. } => column_type(*table, *column, tables, env),
        ast::Expr::Cast { type_name, .. } => cast_type(type_name.as_ref(), env),
        ast::Expr::Parenthesized(exprs) if exprs.len() == 1 => expr_type(&exprs[0], tables, env),
        ast::Expr::Collate(inner, _) => expr_type(inner, tables, env),
        ast::Expr::Case {
            when_then_pairs,
            else_expr,
            ..
        } => common_type(
            when_then_pairs
                .iter()
                .map(|(_, then)| then.as_ref())
                .chain(else_expr.as_deref()),
            tables,
            env,
        ),
        ast::Expr::FunctionCall { name, args, .. } => function_call_type(name, args, tables, env),
        _ => ExprType::Dynamic,
    }
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
            None => ExprType::Dynamic,
        },
        None if table_id.is_self_table() => match &env.self_table {
            Some(table) => match table.columns().get(column) {
                Some(col) => stored_column_type(col, table.is_strict, env),
                None => ExprType::Dynamic,
            },
            None => ExprType::Dynamic,
        },
        None => ExprType::Dynamic,
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
        _ => ExprType::Dynamic,
    }
}

fn subquery_column_type(plan: &Plan, column: usize, env: &TypeEnv) -> ExprType {
    let Plan::Select(select) = plan else {
        return ExprType::Dynamic;
    };
    match select.result_columns.get(column) {
        Some(result_column) => expr_type(&result_column.expr, Some(&select.table_references), env),
        None => ExprType::Dynamic,
    }
}

fn cast_type(type_name: Option<&ast::Type>, env: &TypeEnv) -> ExprType {
    let Some(type_name) = type_name else {
        return ExprType::Dynamic;
    };
    let name = crate::util::dequote(&type_name.name);
    if type_name.array_dimensions > 0 {
        return ExprType::Dynamic;
    }
    let params = cast_type_params(type_name);
    match env.schema.get_type_def_unchecked(&name) {
        Some(def) if is_typed_cast(def, params.len()) => ExprType::Named(NamedType {
            def: Arc::clone(def),
            params: Some(params),
        }),
        _ => ExprType::Dynamic,
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
        ))) => common_type(args.iter().map(|arg| arg.as_ref()), tables, env),
        Ok(Some(crate::function::Func::Scalar(ScalarFunc::Nullif))) => {
            expr_type(&args[0], tables, env)
        }
        _ => ExprType::Dynamic,
    }
}

fn common_type<'e>(
    exprs: impl Iterator<Item = &'e ast::Expr>,
    tables: Option<&TableReferences>,
    env: &TypeEnv,
) -> ExprType {
    let mut common: Option<NamedType> = None;
    for expr in exprs.filter(|expr| !matches!(expr, ast::Expr::Literal(ast::Literal::Null))) {
        let ExprType::Named(named) = expr_type(expr, tables, env) else {
            return ExprType::Dynamic;
        };
        common = match common {
            None => Some(named),
            Some(current) if current.def.name.eq_ignore_ascii_case(&named.def.name) => {
                let params = match (current.params, named.params) {
                    (Some(a), Some(b)) if a == b => Some(a),
                    _ => None,
                };
                Some(NamedType {
                    def: current.def,
                    params,
                })
            }
            Some(_) => return ExprType::Dynamic,
        };
    }
    match common {
        Some(named) => ExprType::Named(named),
        None => ExprType::Dynamic,
    }
}
