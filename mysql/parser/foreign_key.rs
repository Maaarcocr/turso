use crate::stored_table::{ColumnDefinition, Family, TableDefinition};
use crate::{Environment, MysqlParserError};
use std::collections::HashSet;
use turso_parser::ast;

/// Checks the foreign keys of `CREATE TABLE table body` against the tables
/// they reference, as MySQL 8.4 does when it creates them, since Turso only
/// checks them when rows are written:
///
/// - the referenced table must exist (1824), unless `FOREIGN_KEY_CHECKS` is
///   0, and have the referenced columns (3734);
/// - the referenced columns must be its primary key or a unique key, in
///   order (6125, `restrict_fk_on_non_standard_key`);
/// - `TEXT`, `BLOB` and `JSON` columns cannot be keys (1170, 3152);
/// - string columns must have the same collation, and integer columns the
///   same size and sign (3780);
/// - a `SET NULL` action needs nullable columns (1830);
/// - constraint names are unique in the database (1826).
///
/// MySQL accepts keys between `DECIMAL` columns of different scales or
/// sizes and between temporal columns of different types or fractional
/// seconds, which then match no value, and between `CHAR` and `VARCHAR`
/// columns, whose values may match unlike Turso's when they end in spaces;
/// so does Turso, checking writes to the table instead (see
/// `check_writes`). Refused: other columns of different types, which MySQL
/// accepts or rejects by rules not modeled here. With `FOREIGN_KEY_CHECKS` 0, every failed
/// check refuses instead of failing.
pub(crate) fn check_foreign_keys(
    table: &ast::Name,
    body: &ast::CreateTableBody,
    environment: &dyn Environment,
) -> Result<(), MysqlParserError> {
    let ast::CreateTableBody::ColumnsAndConstraints {
        columns,
        constraints,
        ..
    } = body
    else {
        return Ok(());
    };
    let foreign_keys = constraints
        .iter()
        .filter_map(|constraint| match &constraint.constraint {
            ast::TableConstraint::ForeignKey {
                columns: child_columns,
                clause,
                ..
            } => Some((constraint.name.as_ref(), child_columns, clause)),
            _ => None,
        })
        .collect::<Vec<_>>();
    if foreign_keys.is_empty() {
        return Ok(());
    }
    let checks = environment.foreign_key_checks();
    let fail = |message: String| {
        if checks {
            MysqlParserError::Rejected(message)
        } else {
            MysqlParserError::Unsupported(format!(
                "a foreign key MySQL may reject with FOREIGN_KEY_CHECKS = 0 ({message})"
            ))
        }
    };
    let existing_names = environment
        .foreign_key_names()?
        .into_iter()
        .map(|name| name.to_lowercase())
        .collect::<HashSet<_>>();
    let mut names = HashSet::new();
    let mut generated = 0;
    let own = TableDefinition::from_body(columns, constraints);
    for (name, child_columns, clause) in foreign_keys {
        let constraint = match name {
            Some(name) => {
                let lowered = name.as_str().to_lowercase();
                if !names.insert(lowered.clone()) || existing_names.contains(&lowered) {
                    return Err(fail(format!(
                        "Duplicate foreign key constraint name '{}'",
                        name.as_str()
                    )));
                }
                if lowered.contains("_ibfk_") {
                    return Err(MysqlParserError::Unsupported(format!(
                        "foreign key name `{}`, which may be the name MySQL generates for another key, is not lowered yet",
                        name.as_str()
                    )));
                }
                name.as_str().to_string()
            }
            None => {
                generated += 1;
                format!("{}_ibfk_{generated}", table.as_str())
            }
        };
        let child_names = child_columns
            .iter()
            .map(|column| column.col_name.as_str().to_lowercase())
            .collect::<Vec<_>>();
        if child_names.iter().collect::<HashSet<_>>().len() != child_names.len() {
            return Err(MysqlParserError::Unsupported(format!(
                "foreign key `{constraint}` that names a column twice is not lowered yet"
            )));
        }
        let parent_name = clause.tbl_name.as_str();
        let parent = if parent_name.eq_ignore_ascii_case(table.as_str()) {
            own.clone()
        } else {
            match environment.table_sql(parent_name)? {
                Some(sql) => TableDefinition::parse(&sql)?,
                None if checks => {
                    return Err(MysqlParserError::Rejected(format!(
                        "Failed to open the referenced table '{parent_name}'"
                    )))
                }
                None => continue,
            }
        };
        let parent_names = clause
            .columns
            .iter()
            .map(|column| column.col_name.as_str().to_lowercase())
            .collect::<Vec<_>>();
        let mut parent_keys = parent.unique_keys.clone();
        if !parent_name.eq_ignore_ascii_case(table.as_str()) {
            parent_keys.extend(environment.unique_keys_of(parent_name)?);
        }
        for column in &parent_names {
            if parent.column(column).is_none() {
                return Err(fail(format!(
                    "Failed to add the foreign key constraint. Missing column '{column}' for constraint '{constraint}' in the referenced table '{parent_name}'"
                )));
            }
        }
        let key_matches = |key: &Vec<String>| {
            key.len() == parent_names.len()
                && key
                    .iter()
                    .zip(&parent_names)
                    .all(|(key, column)| key.eq_ignore_ascii_case(column))
        };
        if !parent_keys.iter().any(key_matches) {
            return Err(fail(format!(
                "Failed to add the foreign key constraint. Missing unique key for constraint '{constraint}' in the referenced table '{parent_name}'"
            )));
        }
        let set_null = clause.args.iter().any(|arg| {
            matches!(
                arg,
                ast::RefArg::OnDelete(ast::RefAct::SetNull)
                    | ast::RefArg::OnUpdate(ast::RefAct::SetNull)
            )
        });
        for (child_name, parent_column) in child_names.iter().zip(&parent_names) {
            let child = own.column(child_name).ok_or_else(|| {
                MysqlParserError::Rejected(format!(
                    "Key column '{child_name}' doesn't exist in table"
                ))
            })?;
            let parent = parent
                .column(parent_column)
                .expect("the referenced columns were checked to exist");
            if set_null && child.not_null {
                return Err(fail(format!(
                    "Column '{child_name}' cannot be NOT NULL: needed in a foreign key constraint '{constraint}' SET NULL"
                )));
            }
            for column in [child, parent] {
                match column.family() {
                    Family::Blob => {
                        return Err(fail(format!(
                            "BLOB/TEXT column '{}' used in key specification without a key length",
                            column.name
                        )))
                    }
                    Family::Json => {
                        return Err(fail(format!(
                            "JSON column '{}' supports indexing only via generated columns on those expressions",
                            column.name
                        )))
                    }
                    Family::String | Family::Integer | Family::Other => {}
                }
            }
            let incompatible = || {
                fail(format!(
                    "Referencing column '{child_name}' and referenced column '{parent_column}' in foreign key constraint '{constraint}' are incompatible."
                ))
            };
            match (child.family(), parent.family()) {
                (Family::String, Family::String) => {
                    if child.collation != parent.collation {
                        return Err(incompatible());
                    }
                }
                _ if child.type_text == parent.type_text => {}
                _ if stored_match(child, parent) != StoredMatch::Unknown => {}
                (Family::Integer, Family::Integer)
                    if !child.type_text.contains("ZEROFILL")
                        && !parent.type_text.contains("ZEROFILL") =>
                {
                    return Err(incompatible());
                }
                _ => {
                    return Err(MysqlParserError::Unsupported(format!(
                        "foreign key `{constraint}` between a {} and a {} column, which MySQL compares as stored, is not lowered yet",
                        child.type_text, parent.type_text
                    )))
                }
            }
        }
    }
    Ok(())
}

/// How MySQL matches the values of a referencing column with those of the
/// referenced one, comparing their stored bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoredMatch {
    /// Equal values are stored alike, so they match as Turso matches them.
    Values,
    /// The types store every value differently, so no value matches.
    Never,
    /// A `CHAR` and a `VARCHAR` column of one collation: values that do not
    /// end in a space match as Turso matches them.
    UnlessTrailingSpaces,
    /// `ENUM` or `SET` columns of different members, which MySQL matches by
    /// their positions: a member declared at the same position by both
    /// matches as Turso matches it.
    ByPosition,
    /// Not modeled.
    Unknown,
}

fn stored_match(child: &ColumnDefinition, parent: &ColumnDefinition) -> StoredMatch {
    if child.type_text == parent.type_text {
        return StoredMatch::Values;
    }
    if child.family() == Family::String && parent.family() == Family::String {
        return if child.base_type() == parent.base_type() {
            StoredMatch::Values
        } else {
            StoredMatch::UnlessTrailingSpaces
        };
    }
    if let (Some(_), Some(_)) = (&child.members, &parent.members) {
        return if child.base_type() == parent.base_type() {
            StoredMatch::ByPosition
        } else {
            StoredMatch::Unknown
        };
    }
    let is_binary =
        |column: &ColumnDefinition| matches!(column.base_type(), "BINARY" | "VARBINARY");
    if is_binary(child) && is_binary(parent) {
        return StoredMatch::Values;
    }
    if let (Some(child), Some(parent)) = (decimal_storage(child), decimal_storage(parent)) {
        return if child == parent {
            StoredMatch::Values
        } else {
            StoredMatch::Never
        };
    }
    match (temporal_storage(child), temporal_storage(parent)) {
        (Some((child_base, child_bytes)), Some((parent_base, parent_bytes)))
            if child_base != parent_base || child_bytes != parent_bytes =>
        {
            StoredMatch::Never
        }
        _ => StoredMatch::Unknown,
    }
}

/// The scale and the bytes of the integer part of a `DECIMAL(p, s)`, which
/// set how MySQL stores its values.
fn decimal_storage(column: &ColumnDefinition) -> Option<(u32, u32)> {
    let parameters = column.type_text.strip_prefix("DECIMAL")?;
    let (precision, scale): (u32, u32) = match parameters {
        "" => (10, 0),
        _ => {
            let parameters = parameters.strip_prefix('(')?.strip_suffix(')')?;
            match parameters.split_once(',') {
                Some((precision, scale)) => (precision.parse().ok()?, scale.parse().ok()?),
                None => (parameters.parse().ok()?, 0),
            }
        }
    };
    const LEFTOVER_BYTES: [u32; 9] = [0, 1, 1, 2, 2, 3, 3, 4, 4];
    let digits = precision.checked_sub(scale)?;
    Some((
        scale,
        digits / 9 * 4 + LEFTOVER_BYTES[(digits % 9) as usize],
    ))
}

/// The type and the bytes of the fractional seconds of a `DATETIME`,
/// `TIMESTAMP` or `TIME` column.
fn temporal_storage(column: &ColumnDefinition) -> Option<(&str, u32)> {
    let base = column.base_type();
    if !matches!(base, "DATETIME" | "TIMESTAMP" | "TIME") {
        return None;
    }
    let precision: u32 = match column.type_text.strip_prefix(base)? {
        "" => 0,
        parameters => parameters
            .strip_prefix('(')?
            .strip_suffix(')')?
            .parse()
            .ok()?,
    };
    Some((base, precision.div_ceil(2)))
}

/// Checks a write to a table whose foreign key MySQL matches unlike Turso
/// (see `StoredMatch`). MySQL fails a row whose key is not NULL when the key
/// matches no value (1452), so an `INSERT` of such literal rows fails as in
/// MySQL; a write whose key values are NULL or match alike runs, and other
/// writes of the key are refused.
pub(crate) fn check_writes(
    lowered: &ast::Stmt,
    environment: &dyn Environment,
) -> Result<(), MysqlParserError> {
    let table = match lowered {
        ast::Stmt::Insert { tbl_name, .. } => tbl_name.name.as_str(),
        ast::Stmt::Update(update) => update.tbl_name.name.as_str(),
        _ => return Ok(()),
    };
    if !environment.foreign_key_checks() {
        return Ok(());
    }
    let keys = environment.foreign_keys_of(table)?;
    if keys.is_empty() {
        return Ok(());
    }
    let Some(child) = environment.table_sql(table)? else {
        return Ok(());
    };
    let child = TableDefinition::parse(&child)?;
    for key in keys {
        let Some(parent) = environment.table_sql(&key.parent_table)? else {
            continue;
        };
        let parent = TableDefinition::parse(&parent)?;
        let mut columns = Vec::with_capacity(key.child_columns.len());
        for (child_name, parent_name) in key.child_columns.iter().zip(&key.parent_columns) {
            let (Some(child_column), Some(parent_column)) =
                (child.column(child_name), parent.column(parent_name))
            else {
                return Err(MysqlParserError::Unsupported(format!(
                    "a write to `{table}`, whose foreign key names a column its table lacks, is not lowered yet"
                )));
            };
            columns.push((
                child_column,
                parent_column,
                stored_match(child_column, parent_column),
            ));
        }
        if columns
            .iter()
            .all(|(_, _, matched)| matches!(matched, StoredMatch::Values | StoredMatch::Unknown))
        {
            continue;
        }
        let refused = || {
            MysqlParserError::Unsupported(format!(
                "a write to `{table}` of the key of a foreign key MySQL matches as stored ({}) is not lowered yet",
                columns
                    .iter()
                    .map(|(column, _, _)| column.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        };
        match lowered {
            ast::Stmt::Insert {
                columns: listed,
                body,
                ..
            } => {
                let rows: Vec<Vec<Option<&ast::Expr>>> = match body {
                    ast::InsertBody::DefaultValues => vec![vec![None; columns.len()]],
                    ast::InsertBody::Select(select, None) => {
                        let ast::OneSelect::Values(rows) = &select.body.select else {
                            return Err(refused());
                        };
                        if !select.body.compounds.is_empty() {
                            return Err(refused());
                        }
                        if listed.is_empty() && child.columns.iter().any(|column| column.generated)
                        {
                            return Err(refused());
                        }
                        let width = if listed.is_empty() {
                            child.columns.len()
                        } else {
                            listed.len()
                        };
                        if rows.iter().any(|row| row.len() != width) {
                            return Err(refused());
                        }
                        let positions: Vec<Option<usize>> = columns
                            .iter()
                            .map(|(column, _, _)| {
                                if listed.is_empty() {
                                    child.columns.iter().position(|other| {
                                        other.name.eq_ignore_ascii_case(&column.name)
                                    })
                                } else {
                                    listed.iter().position(|other| {
                                        other.as_str().eq_ignore_ascii_case(&column.name)
                                    })
                                }
                            })
                            .collect();
                        rows.iter()
                            .map(|row| {
                                positions
                                    .iter()
                                    .map(|position| position.map(|index| row[index].as_ref()))
                                    .collect()
                            })
                            .collect()
                    }
                    ast::InsertBody::Select(_, Some(_)) => return Err(refused()),
                };
                for row in rows {
                    let row: Vec<Option<&ast::Expr>> = row
                        .iter()
                        .zip(&columns)
                        .map(|(value, (column, _, _))| value.or(column.default.as_ref()))
                        .collect();
                    let values: Vec<KeyValue> =
                        row.iter().map(|value| KeyValue::of(*value)).collect();
                    if values.contains(&KeyValue::Null) {
                        continue;
                    }
                    if values.contains(&KeyValue::Unknown) {
                        return Err(refused());
                    }
                    if columns
                        .iter()
                        .any(|(_, _, matched)| *matched == StoredMatch::Never)
                    {
                        return Err(MysqlParserError::Rejected(format!(
                            "Cannot add or update a child row: a foreign key constraint fails (`{table}`)"
                        )));
                    }
                    if !row
                        .iter()
                        .zip(&values)
                        .zip(&columns)
                        .all(|((value, kind), column)| matches_alike(*value, *kind, column))
                    {
                        return Err(refused());
                    }
                }
            }
            ast::Stmt::Update(update) => {
                let assigned: Vec<Option<&ast::Expr>> = columns
                    .iter()
                    .map(|(column, _, _)| {
                        update.sets.iter().find_map(|set| {
                            set.col_names
                                .iter()
                                .any(|name| name.as_str().eq_ignore_ascii_case(&column.name))
                                .then_some(set.expr.as_ref())
                        })
                    })
                    .collect();
                if assigned.iter().all(Option::is_none) {
                    continue;
                }
                let values: Vec<KeyValue> = assigned
                    .iter()
                    .map(|value| value.map_or(KeyValue::Unknown, |value| KeyValue::of(Some(value))))
                    .collect();
                if values.contains(&KeyValue::Null) {
                    continue;
                }
                let alike =
                    assigned
                        .iter()
                        .zip(&values)
                        .zip(&columns)
                        .all(|((value, kind), column)| {
                            column.2 != StoredMatch::Never && matches_alike(*value, *kind, column)
                        });
                if !alike {
                    return Err(refused());
                }
            }
            _ => unreachable!("only INSERT and UPDATE write rows"),
        }
    }
    Ok(())
}

/// Whether a non-NULL literal key value matches the referenced values in
/// MySQL as it does in Turso.
fn matches_alike(
    value: Option<&ast::Expr>,
    kind: KeyValue,
    (child, parent, matched): &(&ColumnDefinition, &ColumnDefinition, StoredMatch),
) -> bool {
    match matched {
        StoredMatch::Values | StoredMatch::Unknown | StoredMatch::Never => true,
        StoredMatch::UnlessTrailingSpaces => kind == KeyValue::Plain,
        StoredMatch::ByPosition => {
            let (Some(child_members), Some(parent_members)) = (&child.members, &parent.members)
            else {
                return false;
            };
            let same_at = |position: usize| {
                child_members
                    .get(position)
                    .is_some_and(|member| parent_members.get(position) == Some(member))
            };
            let position_of = |text: &str| {
                child_members
                    .iter()
                    .position(|member| member.eq_ignore_ascii_case(text))
            };
            match value {
                Some(ast::Expr::Literal(ast::Literal::String(text))) => {
                    let Some(text) = crate::stored_table::string_text(text) else {
                        return false;
                    };
                    if child.base_type() == "ENUM" {
                        position_of(&text).is_some_and(same_at)
                    } else {
                        text.is_empty()
                            || text
                                .split(',')
                                .all(|member| position_of(member).is_some_and(same_at))
                    }
                }
                Some(ast::Expr::Literal(ast::Literal::Numeric(number)))
                    if child.base_type() == "ENUM" =>
                {
                    number
                        .parse::<usize>()
                        .ok()
                        .and_then(|number| number.checked_sub(1))
                        .is_some_and(same_at)
                }
                _ => false,
            }
        }
    }
}

/// What lowering knows of a value written to a column of a foreign key.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyValue {
    Null,
    /// A literal that does not end in a space.
    Plain,
    /// A string literal that ends in a space.
    TrailingSpaces,
    Unknown,
}

impl KeyValue {
    /// `None` is a column the statement leaves out without a default.
    fn of(value: Option<&ast::Expr>) -> Self {
        match value {
            None | Some(ast::Expr::Literal(ast::Literal::Null)) => Self::Null,
            Some(ast::Expr::Literal(ast::Literal::String(text))) => {
                if text.trim_end_matches('\'').ends_with(' ') {
                    Self::TrailingSpaces
                } else {
                    Self::Plain
                }
            }
            Some(ast::Expr::Literal(ast::Literal::Numeric(_) | ast::Literal::Blob(_))) => {
                Self::Plain
            }
            Some(ast::Expr::Unary(ast::UnaryOperator::Negative, operand))
                if matches!(
                    operand.as_ref(),
                    ast::Expr::Literal(ast::Literal::Numeric(_))
                ) =>
            {
                Self::Plain
            }
            Some(_) => Self::Unknown,
        }
    }
}
