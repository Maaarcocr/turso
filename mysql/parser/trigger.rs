use crate::dialect::MySql;
use crate::names::translate_ddl_name;
use crate::{Environment, MysqlParserError};
use sqlparser::ast::{
    ConditionalStatements, CreateTrigger, DropTrigger, ObjectName, ObjectNamePart, Set, Statement,
    TriggerEvent, TriggerObject, TriggerObjectKind, TriggerPeriod,
};
use sqlparser::dialect::{Dialect, SQLiteDialect};
use sqlparser::tokenizer::{Token, Tokenizer, Word};
use std::collections::HashSet;
use turso_parser::ast;

/// `CREATE TRIGGER name {BEFORE|AFTER} {INSERT|UPDATE|DELETE} ON table FOR
/// EACH ROW statement`, or a `BEGIN ... END` of statements, each an
/// `INSERT`, `REPLACE`, `UPDATE` or `DELETE` lowered as outside a trigger.
///
/// Refused: a second trigger with the timing and event of one the table has,
/// since MySQL fires triggers in the order they were created and Turso in the
/// opposite order; and, in a `BEFORE` trigger, `NEW` of a generated column or
/// (on `INSERT`) of the auto-increment key, which Turso has not computed yet
/// (MySQL has 0 for the key). `SET NEW.column`, other statements, `FOLLOWS`
/// and `PRECEDES` are not lowered.
pub(crate) fn translate_create_trigger(
    create: &CreateTrigger,
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
    let CreateTrigger {
        or_alter,
        temporary,
        or_replace,
        is_constraint,
        name,
        period,
        period_before_table: _,
        events,
        table_name,
        referenced_table_name,
        referencing,
        trigger_object,
        condition,
        exec_body,
        statements_as,
        statements,
        characteristics,
    } = create;
    let unsupported =
        || MysqlParserError::Unsupported(format!("CREATE TRIGGER `{create}` is not MySQL syntax"));
    if *or_alter
        || *temporary
        || *or_replace
        || *is_constraint
        || referenced_table_name.is_some()
        || !referencing.is_empty()
        || condition.is_some()
        || exec_body.is_some()
        || *statements_as
        || characteristics.is_some()
        || *trigger_object != Some(TriggerObjectKind::ForEach(TriggerObject::Row))
    {
        return Err(unsupported());
    }
    let time = match period {
        Some(TriggerPeriod::Before) => ast::TriggerTime::Before,
        Some(TriggerPeriod::After) => ast::TriggerTime::After,
        _ => return Err(unsupported()),
    };
    let event = match events.as_slice() {
        [TriggerEvent::Insert] => Event::Insert,
        [TriggerEvent::Update(columns)] if columns.is_empty() => Event::Update,
        [TriggerEvent::Delete] => Event::Delete,
        _ => return Err(unsupported()),
    };
    let body = match statements {
        Some(ConditionalStatements::Sequence { statements }) => statements,
        Some(ConditionalStatements::BeginEnd(block)) => &block.statements,
        _ => return Err(unsupported()),
    };
    if body.is_empty() {
        return Err(MysqlParserError::Unsupported(
            "a trigger without statements is not lowered yet".to_string(),
        ));
    }
    let text = create.to_string();
    let references = row_references(&text);
    if event == Event::Insert && references.iter().any(|(row, _)| *row == Row::Old) {
        return Err(MysqlParserError::Rejected(
            "There is no OLD row in on INSERT trigger".to_string(),
        ));
    }
    if event == Event::Delete && references.iter().any(|(row, _)| *row == Row::New) {
        return Err(MysqlParserError::Rejected(
            "There is no NEW row in on DELETE trigger".to_string(),
        ));
    }
    for statement in body {
        let Statement::Set(set) = statement else {
            continue;
        };
        for target in set_targets(set) {
            match row_reference(target) {
                Some(Row::Old) => {
                    return Err(MysqlParserError::Rejected(
                        "Updating of OLD row is not allowed in trigger".to_string(),
                    ))
                }
                Some(Row::New) if time == ast::TriggerTime::After => {
                    return Err(MysqlParserError::Rejected(
                        "Updating of NEW row is not allowed in after trigger".to_string(),
                    ))
                }
                _ => {}
            }
        }
    }
    let trigger_name = translate_ddl_name(name)?;
    let table = translate_ddl_name(table_name)?;
    for existing in environment.triggers_on(table.as_str())? {
        let existing = StoredTrigger::parse(&existing)?;
        if existing.time == time && existing.event == event {
            return Err(MysqlParserError::Unsupported(format!(
                "a second {} {} trigger on `{}`, which MySQL fires after `{}` and Turso before it, is not lowered yet",
                time_keyword(time),
                event.keyword(),
                table.as_str(),
                existing.name
            )));
        }
    }
    if time == ast::TriggerTime::Before {
        let Some(columns) = environment.columns_of(table.as_str())? else {
            return Err(MysqlParserError::Rejected(format!(
                "Table '{}' doesn't exist",
                table.as_str()
            )));
        };
        for column in references
            .iter()
            .filter(|(row, _)| *row == Row::New)
            .map(|(_, column)| column)
        {
            let Some(facts) = columns
                .iter()
                .find(|facts| facts.name.eq_ignore_ascii_case(column))
            else {
                continue;
            };
            if facts.generated || (facts.auto_increment && event == Event::Insert) {
                return Err(MysqlParserError::Unsupported(format!(
                    "NEW.{column} in a BEFORE {} trigger, whose value MySQL computes before the trigger and Turso after it, is not lowered yet",
                    event.keyword()
                )));
            }
        }
    }
    let commands = body
        .iter()
        .map(|statement| trigger_command(statement, environment))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ast::Stmt::CreateTrigger {
        temporary: false,
        if_not_exists: false,
        trigger_name: ast::QualifiedName::single(trigger_name),
        time: Some(time),
        event: event.into_ast(),
        tbl_name: ast::QualifiedName::single(table),
        for_each_row: true,
        when_clause: None,
        commands,
    })
}

/// `DROP TRIGGER [IF EXISTS] name`.
pub(crate) fn translate_drop_trigger(drop: &DropTrigger) -> Result<ast::Stmt, MysqlParserError> {
    let DropTrigger {
        if_exists,
        trigger_name,
        table_name: None,
        option: None,
    } = drop
    else {
        return Err(MysqlParserError::Unsupported(format!(
            "`{drop}` is not MySQL syntax"
        )));
    };
    Ok(ast::Stmt::DropTrigger {
        if_exists: *if_exists,
        trigger_name: ast::QualifiedName::single(translate_ddl_name(trigger_name)?),
    })
}

/// Refuses `lowered`, the lowering of the `INSERT`, `REPLACE`, `UPDATE` or
/// `DELETE` `statement`, when the triggers it fires (also through the
/// statements of those triggers) would run differently from MySQL's:
///
/// - a trigger that writes a table that the statement, or a trigger that
///   fired it (directly or through other triggers), uses: MySQL fails
///   (1442), Turso writes it;
/// - a trigger fired by a foreign key action: MySQL does not fire it;
/// - a trigger of a table written with `REPLACE`, `IGNORE` or `ON DUPLICATE
///   KEY UPDATE`, whose triggers are not verified to fire as MySQL's.
///
/// Tables count as used when their name appears in the statement, so a
/// column with a table's name refuses statements that MySQL runs.
pub(crate) fn check_fired_triggers(
    statement: &Statement,
    lowered: &ast::Stmt,
    environment: &dyn Environment,
) -> Result<(), MysqlParserError> {
    if !environment.has_triggers() {
        return Ok(());
    }
    let Some(write) = Write::of_statement(lowered) else {
        return Ok(());
    };
    let mut pending = vec![(write, words(&statement.to_string(), &MySql::default()))];
    let mut cascades = HashSet::new();
    // A trigger's words have the name of its table, so a chain of triggers
    // that comes back to a table is refused before it repeats.
    while let Some((write, used)) = pending.pop() {
        for &event in &write.events {
            if write.cascaded && !cascades.insert((write.table.clone(), event)) {
                continue;
            }
            let triggers = environment
                .triggers_on(&write.table)?
                .iter()
                .map(|sql| StoredTrigger::parse(sql))
                .collect::<Result<Vec<_>, _>>()?;
            for trigger in triggers
                .into_iter()
                .filter(|trigger| trigger.event == event)
            {
                if write.cascaded {
                    return Err(MysqlParserError::Unsupported(format!(
                        "a foreign key action that changes `{}`, whose trigger `{}` MySQL does not fire and Turso does, is not lowered yet",
                        write.table, trigger.name
                    )));
                }
                if !write.plain {
                    return Err(MysqlParserError::Unsupported(format!(
                        "REPLACE, IGNORE or ON DUPLICATE KEY UPDATE on `{}`, which has trigger `{}`, is not lowered yet",
                        write.table, trigger.name
                    )));
                }
                for body_write in &trigger.writes {
                    if used.contains(&body_write.table) {
                        return Err(MysqlParserError::Unsupported(format!(
                            "trigger `{}`, which writes `{}` that the statement uses (MySQL fails with 1442), is not lowered yet",
                            trigger.name, body_write.table
                        )));
                    }
                }
                let mut used = used.clone();
                used.extend(trigger.words);
                pending.extend(
                    trigger
                        .writes
                        .into_iter()
                        .map(|body_write| (body_write, used.clone())),
                );
            }
            for foreign_key in environment.foreign_keys_to(&write.table)? {
                let action = match event {
                    Event::Insert => continue,
                    Event::Update => foreign_key.on_update,
                    Event::Delete => foreign_key.on_delete,
                };
                let child_event = match action {
                    ast::RefAct::Restrict | ast::RefAct::NoAction => continue,
                    ast::RefAct::Cascade if event == Event::Delete => Event::Delete,
                    ast::RefAct::Cascade | ast::RefAct::SetNull | ast::RefAct::SetDefault => {
                        Event::Update
                    }
                };
                pending.push((
                    Write {
                        table: foreign_key.table.to_lowercase(),
                        events: vec![child_event],
                        plain: true,
                        cascaded: true,
                    },
                    used.clone(),
                ));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Event {
    Insert,
    Update,
    Delete,
}

impl Event {
    fn from_ast(event: &ast::TriggerEvent) -> Self {
        match event {
            ast::TriggerEvent::Insert => Self::Insert,
            ast::TriggerEvent::Update | ast::TriggerEvent::UpdateOf(_) => Self::Update,
            ast::TriggerEvent::Delete => Self::Delete,
        }
    }

    fn into_ast(self) -> ast::TriggerEvent {
        match self {
            Self::Insert => ast::TriggerEvent::Insert,
            Self::Update => ast::TriggerEvent::Update,
            Self::Delete => ast::TriggerEvent::Delete,
        }
    }

    fn keyword(self) -> &'static str {
        match self {
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        }
    }
}

fn time_keyword(time: ast::TriggerTime) -> &'static str {
    match time {
        ast::TriggerTime::Before => "BEFORE",
        ast::TriggerTime::After => "AFTER",
        ast::TriggerTime::InsteadOf => "INSTEAD OF",
    }
}

/// A write of a statement to `table` (in lower case), which fires the
/// triggers of `events`. `plain` is false for `REPLACE`, `IGNORE` and
/// `ON DUPLICATE KEY UPDATE`; `cascaded` is true for a foreign key action.
struct Write {
    table: String,
    events: Vec<Event>,
    plain: bool,
    cascaded: bool,
}

impl Write {
    fn new(table: &ast::Name, events: Vec<Event>, plain: bool) -> Self {
        Self {
            table: table.as_str().to_lowercase(),
            events,
            plain,
            cascaded: false,
        }
    }

    fn of_insert(
        table: &ast::Name,
        or_conflict: Option<ast::ResolveType>,
        upsert: Option<&ast::Upsert>,
    ) -> Self {
        let mut events = vec![Event::Insert];
        if upsert.is_some() {
            events.push(Event::Update);
        }
        if or_conflict.is_some() {
            events.extend([Event::Update, Event::Delete]);
        }
        Self::new(table, events, or_conflict.is_none() && upsert.is_none())
    }

    fn of_statement(statement: &ast::Stmt) -> Option<Self> {
        Some(match statement {
            ast::Stmt::Insert {
                or_conflict,
                tbl_name,
                body,
                ..
            } => {
                let upsert = match body {
                    ast::InsertBody::Select(_, upsert) => upsert.as_deref(),
                    ast::InsertBody::DefaultValues => None,
                };
                Self::of_insert(&tbl_name.name, *or_conflict, upsert)
            }
            ast::Stmt::Update(update) => Self::new(
                &update.tbl_name.name,
                vec![Event::Update],
                update.or_conflict.is_none(),
            ),
            ast::Stmt::Delete { tbl_name, .. } => {
                Self::new(&tbl_name.name, vec![Event::Delete], true)
            }
            _ => return None,
        })
    }

    fn of_command(command: &ast::TriggerCmd) -> Option<Self> {
        Some(match command {
            ast::TriggerCmd::Insert {
                or_conflict,
                tbl_name,
                upsert,
                ..
            } => Self::of_insert(tbl_name, *or_conflict, upsert.as_deref()),
            ast::TriggerCmd::Update {
                or_conflict,
                tbl_name,
                ..
            } => Self::new(tbl_name, vec![Event::Update], or_conflict.is_none()),
            ast::TriggerCmd::Delete { tbl_name, .. } => {
                Self::new(tbl_name, vec![Event::Delete], true)
            }
            ast::TriggerCmd::Select(_) => return None,
        })
    }
}

/// A trigger of the schema, from its stored `CREATE TRIGGER`.
struct StoredTrigger {
    name: String,
    time: ast::TriggerTime,
    event: Event,
    writes: Vec<Write>,
    /// The words of its statement, in lower case: the names of every table
    /// it uses, and others.
    words: HashSet<String>,
}

impl StoredTrigger {
    fn parse(sql: &str) -> Result<Self, MysqlParserError> {
        let corrupt = || MysqlParserError::Rejected(format!("stored trigger `{sql}` is not valid"));
        let mut parser = turso_parser::parser::Parser::new(sql.as_bytes());
        let Ok(Some(ast::Cmd::Stmt(ast::Stmt::CreateTrigger {
            trigger_name,
            time,
            event,
            commands,
            ..
        }))) = parser.next_cmd()
        else {
            return Err(corrupt());
        };
        Ok(Self {
            name: trigger_name.name.as_str().to_string(),
            time: time.unwrap_or(ast::TriggerTime::Before),
            event: Event::from_ast(&event),
            writes: commands.iter().filter_map(Write::of_command).collect(),
            words: words(sql, &SQLiteDialect {}),
        })
    }
}

/// The words of `sql` (keywords and identifiers, quoted or not), in lower
/// case.
fn words(sql: &str, dialect: &dyn Dialect) -> HashSet<String> {
    Tokenizer::new(dialect, sql)
        .tokenize()
        .map(|tokens| {
            tokens
                .into_iter()
                .filter_map(|token| match token {
                    Token::Word(word) => Some(word.value.to_lowercase()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    New,
    Old,
}

fn row_of(word: &Word) -> Option<Row> {
    if word.quote_style.is_some() {
        return None;
    }
    if word.value.eq_ignore_ascii_case("new") {
        Some(Row::New)
    } else if word.value.eq_ignore_ascii_case("old") {
        Some(Row::Old)
    } else {
        None
    }
}

/// The `NEW.column` and `OLD.column` of `sql`, a MySQL `CREATE TRIGGER`.
fn row_references(sql: &str) -> Vec<(Row, String)> {
    let dialect = MySql::default();
    let Ok(tokens) = Tokenizer::new(&dialect, sql).tokenize() else {
        return Vec::new();
    };
    let tokens = tokens
        .into_iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    tokens
        .windows(3)
        .filter_map(|window| match window {
            [Token::Word(row), Token::Period, Token::Word(column)] => {
                row_of(row).map(|row| (row, column.value.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The variables a `SET` assigns.
fn set_targets(set: &Set) -> Vec<&ObjectName> {
    match set {
        Set::SingleAssignment { variable, .. } => vec![variable],
        Set::ParenthesizedAssignments { variables, .. } => variables.iter().collect(),
        Set::MultipleAssignments { assignments } => assignments
            .iter()
            .map(|assignment| &assignment.name)
            .collect(),
        _ => Vec::new(),
    }
}

/// The row of `NEW.column` or `OLD.column`.
fn row_reference(name: &ObjectName) -> Option<Row> {
    match name.0.as_slice() {
        [ObjectNamePart::Identifier(row), ObjectNamePart::Identifier(_)]
            if row.quote_style.is_none() =>
        {
            if row.value.eq_ignore_ascii_case("new") {
                Some(Row::New)
            } else if row.value.eq_ignore_ascii_case("old") {
                Some(Row::Old)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// A statement of a trigger body, lowered as outside a trigger.
fn trigger_command(
    statement: &Statement,
    environment: &dyn Environment,
) -> Result<ast::TriggerCmd, MysqlParserError> {
    let lowered = match statement {
        Statement::Insert(insert) => crate::insert::translate_insert(insert, environment)?,
        Statement::Update(update) => crate::dml::translate_update(update, environment)?,
        Statement::Delete(delete) => crate::dml::translate_delete(delete)?,
        Statement::Query(_) => {
            return Err(MysqlParserError::Rejected(
                "Not allowed to return a result set from a trigger".to_string(),
            ))
        }
        _ => {
            return Err(MysqlParserError::Unsupported(format!(
                "statement `{statement}` in a trigger is not lowered yet"
            )))
        }
    };
    let unavailable = |lowered: &ast::Stmt| {
        MysqlParserError::Unsupported(format!(
            "statement `{statement}` in a trigger, lowered to `{lowered}`, is not available"
        ))
    };
    match lowered {
        ast::Stmt::Insert {
            with: None,
            or_conflict,
            tbl_name:
                ast::QualifiedName {
                    namespace: None,
                    name: tbl_name,
                    alias: None,
                },
            columns,
            body: ast::InsertBody::Select(select, upsert),
            returning,
        } if returning.is_empty() => Ok(ast::TriggerCmd::Insert {
            or_conflict,
            tbl_name,
            col_names: columns,
            select,
            upsert,
            returning,
        }),
        ast::Stmt::Update(ast::Update {
            with: None,
            or_conflict,
            tbl_name:
                ast::QualifiedName {
                    namespace: None,
                    name: tbl_name,
                    alias: None,
                },
            indexed: None,
            sets,
            from,
            where_clause,
            returning,
        }) if returning.is_empty() => Ok(ast::TriggerCmd::Update {
            or_conflict,
            tbl_name,
            sets,
            from,
            where_clause,
        }),
        ast::Stmt::Delete {
            with: None,
            tbl_name:
                ast::QualifiedName {
                    namespace: None,
                    name: tbl_name,
                    alias: None,
                },
            indexed: None,
            where_clause,
            returning,
        } if returning.is_empty() => Ok(ast::TriggerCmd::Delete {
            tbl_name,
            where_clause,
        }),
        other => Err(unavailable(&other)),
    }
}
