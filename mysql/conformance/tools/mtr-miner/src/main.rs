use mtparser::ast::{InterpolatedText, Span, Statement};
use mtparser::parser::{parse, ParserConfig};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::env;
use std::error::Error;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 1;
const DEFAULT_LIMIT: usize = 5_000;
const DEFAULT_SHARD_SIZE: usize = 1_000;

#[derive(Debug)]
struct Options {
    input: PathBuf,
    output: PathBuf,
    revision: String,
    path_prefix: String,
    limit: usize,
    shard_size: usize,
    include_dynamic: bool,
}

#[derive(Clone, Debug, Serialize)]
struct SpecCase {
    schema_version: u32,
    id: String,
    source: Source,
    mode: &'static str,
    sql: String,
    expectation: Expectation,
    context: Context,
    curriculum: Curriculum,
}

#[derive(Clone, Debug, Serialize)]
struct Source {
    corpus: &'static str,
    repository: &'static str,
    revision: String,
    path: String,
    line: u32,
    command: String,
    license: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct Expectation {
    syntax: &'static str,
    source_outcome: &'static str,
    mysql_errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct Context {
    connection: String,
    conditional: bool,
    dynamic: bool,
    row_order: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct Curriculum {
    level: &'static str,
    features: Vec<String>,
    complexity: usize,
}

#[derive(Default)]
struct ExtractState {
    connection: String,
    pending_errors: Vec<String>,
    sorted_result: bool,
    conditional_depth: usize,
}

#[derive(Default, Serialize)]
struct ExtractionStats {
    files: usize,
    parsed_files: usize,
    statements: usize,
    literal_statements: usize,
    dynamic_statements: usize,
    empty_statements: usize,
    duplicates: usize,
    selected: usize,
    truncated: usize,
    by_level: BTreeMap<String, usize>,
    by_source_outcome: BTreeMap<String, usize>,
    by_syntax_expectation: BTreeMap<String, usize>,
}

#[derive(Serialize)]
struct Manifest<'a> {
    schema_version: u32,
    generator: &'static str,
    source_repository: &'static str,
    source_revision: &'a str,
    source_root: String,
    redistribution: &'static str,
    selection: Selection,
    stats: &'a ExtractionStats,
}

#[derive(Serialize)]
struct Selection {
    strategy: &'static str,
    limit: usize,
    shard_size: usize,
    includes_dynamic_sql: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("mtr-miner: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let options = parse_options()?;
    let mut paths = Vec::new();
    collect_test_files(&options.input, &mut paths)?;
    paths.sort();

    let mut stats = ExtractionStats {
        files: paths.len(),
        ..ExtractionStats::default()
    };
    let mut candidates = Vec::new();
    for path in paths {
        let bytes = fs::read(&path)?;
        let text = String::from_utf8_lossy(&bytes);
        let statements = parse(&text, ParserConfig::default()).map_err(|error| {
            format!(
                "failed to parse mysqltest source {}: {error}",
                path.display()
            )
        })?;
        stats.parsed_files += 1;
        let relative_path = path.strip_prefix(&options.input).unwrap_or(&path);
        let source_path = join_source_path(&options.path_prefix, relative_path);
        let mut state = ExtractState {
            connection: "default".to_owned(),
            ..ExtractState::default()
        };
        extract_statements(
            &statements,
            &source_path,
            &options.revision,
            &options,
            &mut state,
            &mut candidates,
            &mut stats,
        );
    }

    let mut seen = HashSet::new();
    candidates.retain(|case| {
        let key = format!(
            "{}\u{0}{}\u{0}{}",
            case.sql, case.expectation.syntax, case.expectation.source_outcome
        );
        if seen.insert(key) {
            true
        } else {
            stats.duplicates += 1;
            false
        }
    });

    let available = candidates.len();
    let selected = select_balanced(candidates, options.limit);
    stats.selected = selected.len();
    stats.truncated = available.saturating_sub(stats.selected);
    for case in &selected {
        *stats
            .by_level
            .entry(case.curriculum.level.to_owned())
            .or_default() += 1;
        *stats
            .by_source_outcome
            .entry(case.expectation.source_outcome.to_owned())
            .or_default() += 1;
        *stats
            .by_syntax_expectation
            .entry(case.expectation.syntax.to_owned())
            .or_default() += 1;
    }

    write_corpus(&options, &selected, &stats)?;
    println!(
        "extracted {} cases from {} mysqltest files into {} shards at {}",
        selected.len(),
        stats.parsed_files,
        selected.len().div_ceil(options.shard_size),
        options.output.display()
    );
    println!(
        "literal={} dynamic={} duplicates={} available={} selected={}",
        stats.literal_statements,
        stats.dynamic_statements,
        stats.duplicates,
        available,
        stats.selected
    );
    Ok(())
}

fn parse_options() -> Result<Options, Box<dyn Error>> {
    let mut args = env::args().skip(1);
    let mut input = None;
    let mut output = None;
    let mut revision = None;
    let mut path_prefix = "mysql-test/t".to_owned();
    let mut limit = DEFAULT_LIMIT;
    let mut shard_size = DEFAULT_SHARD_SIZE;
    let mut include_dynamic = false;

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--input" => input = Some(PathBuf::from(required_value(&mut args, "--input")?)),
            "--output" => output = Some(PathBuf::from(required_value(&mut args, "--output")?)),
            "--revision" => revision = Some(required_value(&mut args, "--revision")?),
            "--path-prefix" => path_prefix = required_value(&mut args, "--path-prefix")?,
            "--limit" => limit = required_value(&mut args, "--limit")?.parse()?,
            "--shard-size" => shard_size = required_value(&mut args, "--shard-size")?.parse()?,
            "--include-dynamic" => include_dynamic = true,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }
    if shard_size == 0 {
        return Err("--shard-size must be greater than zero".into());
    }
    Ok(Options {
        input: input.ok_or("--input is required")?,
        output: output.ok_or("--output is required")?,
        revision: revision.ok_or("--revision is required")?,
        path_prefix,
        limit,
        shard_size,
        include_dynamic,
    })
}

fn required_value(
    args: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, Box<dyn Error>> {
    args.next()
        .ok_or_else(|| format!("{option} requires a value").into())
}

fn print_help() {
    println!(
        "mysql-mtr-spec-miner \\\n  --input PATH \\\n  --output PATH \\\n  --revision COMMIT \\\n  [--path-prefix mysql-test/t] \\\n  [--limit 5000] \\\n  [--shard-size 1000] \\\n  [--include-dynamic]\n\nUse --limit 0 to emit every eligible case."
    );
}

fn collect_test_files(path: &Path, output: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    if path.is_file() {
        if path.extension().and_then(|value| value.to_str()) == Some("test") {
            output.push(path.to_owned());
        }
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        if child.is_dir() {
            collect_test_files(&child, output)?;
        } else if child.extension().and_then(|value| value.to_str()) == Some("test") {
            output.push(child);
        }
    }
    Ok(())
}

fn extract_statements(
    statements: &[Statement],
    source_path: &str,
    revision: &str,
    options: &Options,
    state: &mut ExtractState,
    output: &mut Vec<SpecCase>,
    stats: &mut ExtractionStats,
) {
    for statement in statements {
        match statement {
            Statement::Error(command) => {
                state.pending_errors = command
                    .error_codes
                    .iter()
                    .map(InterpolatedText::to_raw_string)
                    .collect();
            }
            Statement::SortedResult(_) | Statement::PartiallySortedResult(_) => {
                state.sorted_result = true;
            }
            Statement::Connection(command) => {
                state.connection = command.name.to_raw_string();
            }
            Statement::If(block) => {
                let mut nested_state = ExtractState {
                    connection: state.connection.clone(),
                    conditional_depth: state.conditional_depth + 1,
                    ..ExtractState::default()
                };
                extract_statements(
                    &block.body,
                    source_path,
                    revision,
                    options,
                    &mut nested_state,
                    output,
                    stats,
                );
            }
            Statement::While(block) => {
                let mut nested_state = ExtractState {
                    connection: state.connection.clone(),
                    conditional_depth: state.conditional_depth + 1,
                    ..ExtractState::default()
                };
                extract_statements(
                    &block.body,
                    source_path,
                    revision,
                    options,
                    &mut nested_state,
                    output,
                    stats,
                );
            }
            Statement::Sql(command) => emit_sql(
                &command.sql,
                command.span,
                "sql",
                source_path,
                revision,
                options,
                state,
                output,
                stats,
            ),
            Statement::Query(command) => emit_text(
                &command.sql,
                command.span,
                "query",
                source_path,
                revision,
                options,
                state,
                output,
                stats,
            ),
            Statement::Eval(command) => emit_text(
                &command.sql,
                command.span,
                "eval",
                source_path,
                revision,
                options,
                state,
                output,
                stats,
            ),
            Statement::Send(command) => emit_text(
                &command.sql,
                command.span,
                "send",
                source_path,
                revision,
                options,
                state,
                output,
                stats,
            ),
            Statement::SendEval(command) => emit_text(
                &command.sql,
                command.span,
                "send_eval",
                source_path,
                revision,
                options,
                state,
                output,
                stats,
            ),
            Statement::QueryVertical(command) => emit_text(
                &command.sql,
                command.span,
                "query_vertical",
                source_path,
                revision,
                options,
                state,
                output,
                stats,
            ),
            Statement::EvalP(command) => emit_text(
                &command.sql,
                command.span,
                "evalp",
                source_path,
                revision,
                options,
                state,
                output,
                stats,
            ),
            Statement::PsPrepare(command) => emit_text(
                &command.sql,
                command.span,
                "ps_prepare",
                source_path,
                revision,
                options,
                state,
                output,
                stats,
            ),
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_text(
    text: &InterpolatedText,
    span: Span,
    command: &str,
    source_path: &str,
    revision: &str,
    options: &Options,
    state: &mut ExtractState,
    output: &mut Vec<SpecCase>,
    stats: &mut ExtractionStats,
) {
    emit_sql_with_dynamic(
        &text.to_raw_string(),
        !text.is_literal(),
        span,
        command,
        source_path,
        revision,
        options,
        state,
        output,
        stats,
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_sql(
    sql: &str,
    span: Span,
    command: &str,
    source_path: &str,
    revision: &str,
    options: &Options,
    state: &mut ExtractState,
    output: &mut Vec<SpecCase>,
    stats: &mut ExtractionStats,
) {
    emit_sql_with_dynamic(
        sql,
        sql.contains('$'),
        span,
        command,
        source_path,
        revision,
        options,
        state,
        output,
        stats,
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_sql_with_dynamic(
    sql: &str,
    dynamic: bool,
    span: Span,
    command: &str,
    source_path: &str,
    revision: &str,
    options: &Options,
    state: &mut ExtractState,
    output: &mut Vec<SpecCase>,
    stats: &mut ExtractionStats,
) {
    stats.statements += 1;
    let sql = sql.trim();
    if sql.is_empty() {
        stats.empty_statements += 1;
        consume_one_shot_state(state);
        return;
    }
    if dynamic {
        stats.dynamic_statements += 1;
        if !options.include_dynamic {
            consume_one_shot_state(state);
            return;
        }
    } else {
        stats.literal_statements += 1;
    }

    let errors = std::mem::take(&mut state.pending_errors);
    let row_order = if std::mem::take(&mut state.sorted_result) {
        "sorted"
    } else if has_top_level_order_by(sql) {
        "exact"
    } else {
        "bag"
    };
    let (syntax, source_outcome) = classify_expectation(&errors);
    let (level, features) = classify_features(sql);
    let fingerprint = fnv1a64(format!("{source_path}\0{}\0{sql}", span.line).as_bytes());
    let id = format!(
        "mysql-mtr:{}:L{}:{fingerprint:016x}",
        source_path.trim_end_matches(".test"),
        span.line
    );
    let complexity = 1 + features.len() + sql.len().div_ceil(200);
    output.push(SpecCase {
        schema_version: SCHEMA_VERSION,
        id,
        source: Source {
            corpus: "mysql-mtr",
            repository: "https://github.com/mysql/mysql-server.git",
            revision: revision.to_owned(),
            path: source_path.to_owned(),
            line: span.line,
            command: command.to_owned(),
            license: "GPL-2.0-only",
        },
        mode: "syntax",
        sql: sql.to_owned(),
        expectation: Expectation {
            syntax,
            source_outcome,
            mysql_errors: errors,
        },
        context: Context {
            connection: state.connection.clone(),
            conditional: state.conditional_depth > 0,
            dynamic,
            row_order,
        },
        curriculum: Curriculum {
            level,
            features,
            complexity,
        },
    });
}

fn consume_one_shot_state(state: &mut ExtractState) {
    state.pending_errors.clear();
    state.sorted_result = false;
}

fn classify_expectation(errors: &[String]) -> (&'static str, &'static str) {
    if errors.is_empty() {
        return ("accept", "success");
    }
    let normalized: Vec<String> = errors
        .iter()
        .map(|error| error.trim().to_ascii_uppercase())
        .collect();
    if normalized.iter().any(|error| error == "0") {
        return ("unknown", "success-or-error");
    }
    if normalized.iter().any(|error| {
        matches!(
            error.as_str(),
            "1064" | "1149" | "ER_PARSE_ERROR" | "ER_SYNTAX_ERROR" | "ER_EMPTY_QUERY"
        )
    }) {
        ("reject", "parse-error")
    } else {
        ("accept", "execution-error")
    }
}

fn classify_features(sql: &str) -> (&'static str, Vec<String>) {
    let upper = sql.to_ascii_uppercase();
    let first = first_keyword(&upper);
    let normalized = upper.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut features = vec![format!("statement:{}", first.to_ascii_lowercase())];
    let feature_markers = [
        (" JOIN ", "join"),
        (" WHERE ", "where"),
        (" GROUP BY ", "group-by"),
        (" HAVING ", "having"),
        (" ORDER BY ", "order-by"),
        (" LIMIT ", "limit"),
        (" WITH ", "cte"),
        (" UNION ", "set-operation"),
        (" WINDOW ", "window"),
        (" OVER ", "window"),
        (" JSON", "json"),
        (" COLLATE ", "collation"),
        (" CHARACTER SET ", "charset"),
        (" AUTO_INCREMENT", "auto-increment"),
        (" GENERATED ", "generated-column"),
        (" FOREIGN KEY ", "foreign-key"),
        (" ON DUPLICATE KEY ", "upsert"),
        (" PREPARE ", "prepared-statement"),
        (" TRANSACTION", "transaction"),
    ];
    let padded = format!(" {normalized} ");
    for (marker, feature) in feature_markers {
        if padded.contains(marker) && !features.iter().any(|value| value == feature) {
            features.push(feature.to_owned());
        }
    }
    features.sort();

    let level = match first.as_str() {
        "SELECT" if !padded.contains(" FROM ") => "01-literal-select",
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" => "02-schema",
        "INSERT" | "REPLACE" | "LOAD" => "03-insert",
        "SELECT" if padded.contains(" JOIN ") => "07-joins",
        "SELECT" if padded.contains(" GROUP BY ") || padded.contains(" OVER ") => {
            "08-aggregation-window"
        }
        "SELECT" => "04-query",
        "UPDATE" | "DELETE" => "05-mutations",
        "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" => "06-transactions",
        "SET" | "USE" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" => "09-session-metadata",
        "GRANT" | "REVOKE" => "10-security",
        _ => "11-advanced",
    };
    (level, features)
}

fn first_keyword(sql: &str) -> String {
    let mut rest = sql.trim_start();
    loop {
        if rest.starts_with("--") || rest.starts_with('#') {
            rest = rest
                .split_once('\n')
                .map(|(_, tail)| tail.trim_start())
                .unwrap_or("");
        } else if rest.starts_with("/*") {
            rest = rest
                .split_once("*/")
                .map(|(_, tail)| tail.trim_start())
                .unwrap_or("");
        } else {
            break;
        }
    }
    rest.trim_start_matches('(')
        .trim_start()
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .find(|token| !token.is_empty())
        .unwrap_or("unknown")
        .to_owned()
}

fn has_top_level_order_by(sql: &str) -> bool {
    let upper = sql.to_ascii_uppercase();
    let normalized = upper.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(" {normalized} ").contains(" ORDER BY ")
}

fn select_balanced(cases: Vec<SpecCase>, limit: usize) -> Vec<SpecCase> {
    let target = if limit == 0 { usize::MAX } else { limit };
    let mut groups: BTreeMap<&'static str, VecDeque<SpecCase>> = BTreeMap::new();
    for case in cases {
        groups
            .entry(case.curriculum.level)
            .or_default()
            .push_back(case);
    }
    let levels: Vec<&'static str> = groups.keys().copied().collect();
    let mut selected = Vec::new();
    while selected.len() < target {
        let mut made_progress = false;
        for level in &levels {
            if selected.len() == target {
                break;
            }
            if let Some(case) = groups.get_mut(level).and_then(VecDeque::pop_front) {
                selected.push(case);
                made_progress = true;
            }
        }
        if !made_progress {
            break;
        }
    }
    selected
}

fn write_corpus(
    options: &Options,
    cases: &[SpecCase],
    stats: &ExtractionStats,
) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(&options.output)?;
    for entry in fs::read_dir(&options.output)? {
        let path = entry?.path();
        let generated_file = path.extension().and_then(|value| value.to_str()) == Some("jsonl")
            || path.file_name().and_then(|value| value.to_str()) == Some("manifest.json");
        if generated_file {
            fs::remove_file(path)?;
        }
    }

    for (index, shard) in cases.chunks(options.shard_size).enumerate() {
        let path = options.output.join(format!("cases-{index:05}.jsonl"));
        let mut writer = BufWriter::new(File::create(path)?);
        for case in shard {
            serde_json::to_writer(&mut writer, case)?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
    }

    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        generator: env!("CARGO_PKG_NAME"),
        source_repository: "https://github.com/mysql/mysql-server.git",
        source_revision: &options.revision,
        source_root: options.input.display().to_string(),
        redistribution: "Generated MySQL MTR cases are local-only pending license review.",
        selection: Selection {
            strategy: "round-robin-by-generated-level",
            limit: options.limit,
            shard_size: options.shard_size,
            includes_dynamic_sql: options.include_dynamic,
        },
        stats,
    };
    let manifest_path = options.output.join("manifest.json");
    let mut writer = BufWriter::new(File::create(manifest_path)?);
    serde_json::to_writer_pretty(&mut writer, &manifest)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn join_source_path(prefix: &str, relative: &Path) -> String {
    let relative = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    format!("{}/{relative}", prefix.trim_end_matches('/'))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_source_expectations() {
        assert_eq!(classify_expectation(&[]), ("accept", "success"));
        assert_eq!(
            classify_expectation(&["ER_PARSE_ERROR".to_owned()]),
            ("reject", "parse-error")
        );
        assert_eq!(
            classify_expectation(&["ER_DUP_ENTRY".to_owned()]),
            ("accept", "execution-error")
        );
        assert_eq!(
            classify_expectation(&["0".to_owned(), "ER_LOCK_WAIT_TIMEOUT".to_owned()]),
            ("unknown", "success-or-error")
        );
    }

    #[test]
    fn generates_rough_curriculum_features() {
        let (level, features) = classify_features(
            "SELECT u.id FROM users u JOIN teams t ON t.id = u.team_id ORDER BY u.id",
        );
        assert_eq!(level, "07-joins");
        assert!(features.contains(&"join".to_owned()));
        assert!(features.contains(&"order-by".to_owned()));
    }

    #[test]
    fn skips_leading_comments_when_classifying_statements() {
        assert_eq!(first_keyword("/* fixture */\nSELECT 1"), "SELECT");
        assert_eq!(
            first_keyword("# fixture\nINSERT INTO t VALUES (1)"),
            "INSERT"
        );
    }

    #[test]
    fn uses_stable_fingerprints() {
        assert_eq!(fnv1a64(b"mysql"), 0xdd6b19df3bd150bd);
    }
}
