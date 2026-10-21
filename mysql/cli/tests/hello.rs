use std::process::{Command, Output};

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tursomysql"))
        .args(arguments)
        .output()
        .expect("tursomysql must start")
}

fn assert_success(arguments: &[&str], expected_stdout: &str) {
    let output = run(arguments);
    assert!(
        output.status.success(),
        "tursomysql failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected_stdout);
    assert!(output.stderr.is_empty());
}

fn assert_failure(sql: &str, expected_error: &str) {
    let output = run(&[sql]);
    assert!(
        !output.status.success(),
        "query unexpectedly succeeded: {sql}"
    );
    assert!(output.stdout.is_empty(), "failed query wrote rows: {sql}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_error),
        "SQL: {sql}\nstderr: {stderr}"
    );
}

#[test]
fn default_command_executes_hello_world() {
    assert_success(&[], "hello world\n");
}

#[test]
fn command_executes_owned_literal_cases() {
    let cases = [
        (
            "SELECT 'hello from mysql' AS greeting",
            "hello from mysql\n",
        ),
        ("SELECT 42", "42\n"),
        ("SELECT TRUE, FALSE, NULL", "1\t0\tNULL\n"),
        ("SELECT 'I''m fine'", "I'm fine\n"),
        ("SELECT 'Grüße 世界 🦀'", "Grüße 世界 🦀\n"),
        ("SELECT 1, 'two', NULL", "1\ttwo\tNULL\n"),
    ];
    for (sql, expected) in cases {
        assert_success(&[sql], expected);
    }
}

#[test]
fn command_joins_separate_shell_arguments_into_sql() {
    assert_success(
        &["SELECT", "'joined arguments'", "AS", "greeting"],
        "joined arguments\n",
    );
}

#[test]
fn command_executes_basic_create_table() {
    assert_success(
        &["CREATE TABLE users(id INTEGER PRIMARY KEY, name VARCHAR(100))"],
        "",
    );
}

#[test]
fn command_reports_unsupported_syntax() {
    assert_failure(
        "SELECT ?",
        "tursomysql CLI does not support binding parameters",
    );
}
