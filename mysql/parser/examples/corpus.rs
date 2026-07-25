use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use turso_mysql_parser::parse;

#[derive(Deserialize)]
struct Case {
    schema_version: u32,
    id: String,
    sql: String,
    expectation: Expectation,
    curriculum: Curriculum,
}

#[derive(Deserialize)]
struct Expectation {
    syntax: String,
}

#[derive(Deserialize)]
struct Curriculum {
    level: String,
    complexity: usize,
}

#[derive(Default, Serialize)]
struct LevelStats {
    total: usize,
    passed: usize,
    expected_accept: usize,
    accepted: usize,
    expected_reject: usize,
    rejected: usize,
    unknown: usize,
    panics: usize,
}

#[derive(Serialize)]
struct Gap {
    id: String,
    level: String,
    complexity: usize,
    expected: String,
    sql: String,
    observed: String,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    cases: usize,
    passed: usize,
    pass_percent: f64,
    panics: usize,
    by_level: BTreeMap<String, LevelStats>,
    frontier: Vec<Gap>,
}

struct Options {
    inputs: Vec<PathBuf>,
    through: Option<String>,
    json_report: Option<PathBuf>,
    strict: bool,
    frontier_size: usize,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("mysql parser corpus: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let options = parse_options()?;
    let mut files = Vec::new();
    for input in &options.inputs {
        collect_jsonl_files(input, &mut files)?;
    }
    files.sort();
    if files.is_empty() {
        return Err("no JSONL corpus shards found".into());
    }

    let mut cases = Vec::new();
    for path in files {
        let reader = BufReader::new(File::open(&path)?);
        for (index, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let case: Case = serde_json::from_str(&line).map_err(|error| {
                format!("invalid case at {}:{}: {error}", path.display(), index + 1)
            })?;
            if case.schema_version != 1 {
                return Err(format!(
                    "unsupported schema version {} in {}",
                    case.schema_version, case.id
                )
                .into());
            }
            if options
                .through
                .as_ref()
                .is_some_and(|through| case.curriculum.level.as_str() > through.as_str())
            {
                continue;
            }
            cases.push(case);
        }
    }
    cases.sort_by(|left, right| {
        left.curriculum
            .level
            .cmp(&right.curriculum.level)
            .then(left.curriculum.complexity.cmp(&right.curriculum.complexity))
            .then(left.id.cmp(&right.id))
    });

    let mut by_level: BTreeMap<String, LevelStats> = BTreeMap::new();
    let mut gaps = Vec::new();
    for case in cases {
        let level = by_level.entry(case.curriculum.level.clone()).or_default();
        level.total += 1;
        match case.expectation.syntax.as_str() {
            "accept" => level.expected_accept += 1,
            "reject" => level.expected_reject += 1,
            "unknown" => level.unknown += 1,
            expectation => {
                return Err(
                    format!("unknown syntax expectation {expectation:?} in {}", case.id).into(),
                )
            }
        }
        let result = std::panic::catch_unwind(|| parse(&case.sql));
        match (case.expectation.syntax.as_str(), result) {
            ("accept", Ok(Ok(_))) => {
                level.accepted += 1;
                level.passed += 1;
            }
            ("accept", Ok(Err(error))) => gaps.push(gap(&case, error.to_string())),
            ("reject", Ok(Err(_))) => {
                level.rejected += 1;
                level.passed += 1;
            }
            ("reject", Ok(Ok(_))) => gaps.push(gap(&case, "accepted".to_owned())),
            ("unknown", Ok(_)) => {}
            (_, Err(payload)) => {
                level.panics += 1;
                gaps.push(gap(&case, panic_message(payload)));
            }
            _ => unreachable!("syntax expectations were validated before parsing"),
        }
    }

    let case_count: usize = by_level.values().map(|stats| stats.total).sum();
    let passed: usize = by_level.values().map(|stats| stats.passed).sum();
    let panics: usize = by_level.values().map(|stats| stats.panics).sum();
    let graded = by_level
        .values()
        .map(|stats| stats.expected_accept + stats.expected_reject)
        .sum::<usize>();
    let pass_percent = if graded == 0 {
        0.0
    } else {
        100.0 * passed as f64 / graded as f64
    };
    gaps.sort_by(|left, right| {
        left.level
            .cmp(&right.level)
            .then(left.complexity.cmp(&right.complexity))
            .then(left.id.cmp(&right.id))
    });
    gaps.truncate(options.frontier_size);

    println!("MySQL syntax corpus: {passed}/{graded} passing ({pass_percent:.2}%)");
    for (level, stats) in &by_level {
        let graded = stats.expected_accept + stats.expected_reject;
        println!(
            "  {level}: {}/{} passing; accepted={}/{} rejected={}/{} unknown={} panics={}",
            stats.passed,
            graded,
            stats.accepted,
            stats.expected_accept,
            stats.rejected,
            stats.expected_reject,
            stats.unknown,
            stats.panics
        );
    }
    if !gaps.is_empty() {
        println!("\nImplementation frontier:");
        for item in &gaps {
            println!(
                "  {} [{} complexity={}]: expected {}, observed {}\n    {}",
                item.id,
                item.level,
                item.complexity,
                item.expected,
                one_line(&item.observed, 180),
                one_line(&item.sql, 220)
            );
        }
    }

    let report = Report {
        schema_version: 1,
        cases: case_count,
        passed,
        pass_percent,
        panics,
        by_level,
        frontier: gaps,
    };
    if let Some(path) = &options.json_report {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut writer = BufWriter::new(File::create(path)?);
        serde_json::to_writer_pretty(&mut writer, &report)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }

    if panics > 0 {
        return Err(format!("parser panicked on {panics} corpus cases").into());
    }
    if options.strict && passed < graded {
        return Err(format!("{} graded corpus cases are not passing", graded - passed).into());
    }
    Ok(())
}

fn gap(case: &Case, observed: String) -> Gap {
    Gap {
        id: case.id.clone(),
        level: case.curriculum.level.clone(),
        complexity: case.curriculum.complexity,
        expected: case.expectation.syntax.clone(),
        sql: case.sql.clone(),
        observed,
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        format!("panic: {message}")
    } else if let Some(message) = payload.downcast_ref::<String>() {
        format!("panic: {message}")
    } else {
        "panic with non-string payload".to_owned()
    }
}

fn parse_options() -> Result<Options, Box<dyn Error>> {
    let mut arguments = env::args().skip(1);
    let mut inputs = Vec::new();
    let mut through = None;
    let mut json_report = None;
    let mut strict = false;
    let mut frontier_size = 20;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--through" => through = Some(required_value(&mut arguments, "--through")?),
            "--json-report" => {
                json_report = Some(PathBuf::from(required_value(
                    &mut arguments,
                    "--json-report",
                )?))
            }
            "--frontier" => {
                frontier_size = required_value(&mut arguments, "--frontier")?.parse()?;
            }
            "--strict" => strict = true,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown argument: {value}").into())
            }
            path => inputs.push(PathBuf::from(path)),
        }
    }
    if inputs.is_empty() {
        return Err("provide at least one corpus file or directory".into());
    }
    Ok(Options {
        inputs,
        through,
        json_report,
        strict,
        frontier_size,
    })
}

fn required_value(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, Box<dyn Error>> {
    arguments
        .next()
        .ok_or_else(|| format!("{option} requires a value").into())
}

fn print_help() {
    println!(
        "cargo run -p turso_mysql_parser --example corpus -- CORPUS... \\\n  [--through LEVEL] [--json-report PATH] [--frontier 20] [--strict]"
    );
}

fn collect_jsonl_files(path: &Path, output: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    if path.is_file() {
        if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            output.push(path.to_owned());
        }
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let child = entry?.path();
        if child.is_dir() {
            collect_jsonl_files(&child, output)?;
        } else if child.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            output.push(child);
        }
    }
    Ok(())
}

fn one_line(value: &str, limit: usize) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect()
}
