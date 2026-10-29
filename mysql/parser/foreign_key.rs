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
/// `DECIMAL(4,1)` never matches `DECIMAL(4,2)`), and keys whose columns are a
/// unique key in another order. With `FOREIGN_KEY_CHECKS` 0, every failed
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
            let same_set = parent_keys.iter().any(|key| {
                key.len() == parent_names.len()
                    && key.iter().all(|key| {
                        parent_names
                            .iter()
                            .any(|column| column.eq_ignore_ascii_case(key))
                    })
            });
            if same_set {
                return Err(MysqlParserError::Unsupported(format!(
                    "foreign key `{constraint}` on a unique key of `{parent_name}` in another column order is not lowered yet"
                )));
            }
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    /// `CHAR` and `VARCHAR`, whose lengths may differ.
    String,
    /// Integer types, whose size and sign must be the same.
    Integer,
    Blob,
    Json,
    Other,
}

#[derive(Clone)]
struct ColumnDefinition {
    name: String,
    /// The type as written, in upper case without spaces (`VARCHAR(20)`,
    /// `INTUNSIGNED`), with no display width for integer types and `INT` for
    /// `INTEGER`.
    type_text: String,
    collation: Option<String>,
    not_null: bool,
}

impl ColumnDefinition {
    fn new(definition: &ast::ColumnDefinition) -> Self {
        let written = definition
            .col_type
            .as_ref()
            .map(|col_type| col_type.to_string())
            .unwrap_or_default()
            .to_ascii_uppercase();
        let base = written
            .split(|c: char| c == '(' || c.is_whitespace())
            .next()
            .unwrap_or_default();
        let type_text = if is_integer(base) {
            let mut depth = 0;
            written
                .chars()
                .filter(|c| {
                    match c {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            return false;
                        }
                        _ => {}
                    }
                    depth == 0 && !c.is_whitespace()
                })
                .collect::<String>()
                .replacen("INTEGER", "INT", 1)
        } else {
            written.split_whitespace().collect()
        };
        let mut collation = None;
        let mut not_null = false;
        for constraint in &definition.constraints {
            match &constraint.constraint {
                ast::ColumnConstraint::Collate { collation_name } => {
                    collation = Some(collation_name.as_str().to_string());
                }
                ast::ColumnConstraint::NotNull { nullable, .. } => not_null = !nullable,
                ast::ColumnConstraint::PrimaryKey { .. } => not_null = true,
                _ => {}
            }
        }
        Self {
            name: definition.col_name.as_str().to_string(),
            type_text,
            collation,
            not_null,
        }
    }

    fn base_type(&self) -> &str {
        self.type_text
            .split_once('(')
            .map_or(self.type_text.as_str(), |(base, _)| base)
    }

    fn family(&self) -> Family {
        match self.base_type() {
            "CHAR" | "VARCHAR" => Family::String,
            "TINYTEXT" | "TEXT" | "MEDIUMTEXT" | "LONGTEXT" | "TINYBLOB" | "BLOB"
            | "MEDIUMBLOB" | "LONGBLOB" => Family::Blob,
            "JSON" => Family::Json,
            base if base.starts_with("TINYINT")
                || base.starts_with("SMALLINT")
                || base.starts_with("MEDIUMINT")
                || base.starts_with("INT")
                || base.starts_with("BIGINT") =>
            {
                Family::Integer
            }
            _ => Family::Other,
        }
    }
}

fn is_integer(base: &str) -> bool {
    matches!(
        base,
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" | "BIGINT"
    )
}

/// The columns and unique keys of a `CREATE TABLE`.
#[derive(Clone)]
struct TableDefinition {
    columns: Vec<ColumnDefinition>,
    unique_keys: Vec<Vec<String>>,
}

impl TableDefinition {
    fn parse(sql: &str) -> Result<Self, MysqlParserError> {
        let mut parser = turso_parser::parser::Parser::new(sql.as_bytes());
        match parser.next_cmd() {
            Ok(Some(ast::Cmd::Stmt(ast::Stmt::CreateTable {
                body:
                    ast::CreateTableBody::ColumnsAndConstraints {
                        columns,
                        constraints,
                        ..
                    },
                ..
            }))) => Ok(Self::from_body(&columns, &constraints)),
            _ => Err(MysqlParserError::Unsupported(format!(
                "a foreign key to the table of `{sql}` is not lowered yet"
            ))),
        }
    }

    fn from_body(
        columns: &[ast::ColumnDefinition],
        constraints: &[ast::NamedTableConstraint],
    ) -> Self {
        let mut unique_keys = Vec::new();
        for column in columns {
            if column.constraints.iter().any(|constraint| {
                matches!(
                    constraint.constraint,
                    ast::ColumnConstraint::PrimaryKey { .. } | ast::ColumnConstraint::Unique(_)
                )
            }) {
                unique_keys.push(vec![column.col_name.as_str().to_lowercase()]);
            }
        }
        for constraint in constraints {
            let key_columns = match &constraint.constraint {
                ast::TableConstraint::PrimaryKey { columns, .. }
                | ast::TableConstraint::Unique { columns, .. } => columns,
                _ => continue,
            };
            let key = key_columns
                .iter()
                .map(|column| match column.expr.as_ref() {
                    ast::Expr::Id(name) => Some(name.as_str().to_lowercase()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>();
            if let Some(key) = key {
                unique_keys.push(key);
            }
        }
        Self {
            columns: columns.iter().map(ColumnDefinition::new).collect(),
            unique_keys,
        }
    }

    fn column(&self, name: &str) -> Option<&ColumnDefinition> {
        self.columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(name))
    }
}
