use crate::stored_table::{Family, TableDefinition};
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
/// Refused: other columns of different types, which MySQL accepts or
/// rejects by rules not modeled here (it compares keys as stored, so
/// `DECIMAL(4,1)` never matches `DECIMAL(4,2)`). With `FOREIGN_KEY_CHECKS` 0, every failed
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
                    if child.base_type() != parent.base_type() {
                        return Err(MysqlParserError::Unsupported(format!(
                            "foreign key `{constraint}` between a {} and a {} column is not lowered yet",
                            child.base_type(),
                            parent.base_type()
                        )));
                    }
                }
                _ if child.type_text == parent.type_text => {}
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
