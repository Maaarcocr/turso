#!/usr/bin/env python3
import os
import pathlib
import subprocess
import sys
import time
import tomllib
import uuid

ROOT = pathlib.Path(__file__).resolve().parents[2]
TARGET = tomllib.loads((ROOT / "mysql/conformance/target.toml").read_text())
CASES = [
    ("zero", "SELECT 0", "0"),
    ("integer", "SELECT 42", "42"),
    ("maximum signed integer", "SELECT 9223372036854775807", "9223372036854775807"),
    ("leading zeroes", "SELECT 00042", "42"),
    ("booleans and null", "SELECT TRUE, FALSE, NULL", "1\t0\tNULL"),
    ("empty string", "SELECT ''", ""),
    ("text", "SELECT 'hello world'", "hello world"),
    ("escaped quote", "SELECT 'I''m fine'", "I'm fine"),
    ("double quoted text", 'SELECT "double quoted"', "double quoted"),
    ("utf8mb4", "SELECT 'Grüße 世界 🦀'", "Grüße 世界 🦀"),
    ("mixed projection", "SELECT 1, 'two', NULL, TRUE", "1\ttwo\tNULL\t1"),
    ("aliases", "SELECT 'Ada' AS name", "Ada"),
    ("comments", "/* mysql */ SELECT 8 /* tail */", "8"),
]


def run(command, **kwargs):
    return subprocess.run(command, check=True, text=True, capture_output=True, **kwargs)


def normalize(output):
    return output.replace("\r\n", "\n").rstrip("\n")


def wait_for_mysql(container):
    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        probe = subprocess.run(
            ["docker", "exec", container, "mysqladmin", "ping", "-uroot", "--silent"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if probe.returncode == 0:
            return
        time.sleep(2)
    logs = subprocess.run(["docker", "logs", container], text=True, capture_output=True)
    raise RuntimeError(f"MySQL did not become ready\n{logs.stdout}\n{logs.stderr}")


def main():
    if subprocess.run(["docker", "version"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode != 0:
        raise RuntimeError("Docker is required for the MySQL differential suite")

    cargo = os.environ.get("CARGO", "cargo")
    build_env = {
        **os.environ,
        "CARGO_BUILD_JOBS": os.environ.get("CARGO_BUILD_JOBS", "1"),
        "CARGO_PROFILE_DEV_DEBUG": os.environ.get("CARGO_PROFILE_DEV_DEBUG", "0"),
        "CARGO_INCREMENTAL": os.environ.get("CARGO_INCREMENTAL", "0"),
    }
    subprocess.run([cargo, "build", "-p", "tursomysql"], cwd=ROOT, env=build_env, check=True)
    turso_cli = ROOT / "target/debug/tursomysql"

    container = f"turso-mysql-oracle-{uuid.uuid4().hex[:10]}"
    mysql_options = [
        f"--sql-mode={TARGET['sql_mode']}",
        f"--character-set-server={TARGET['character_set_server']}",
        f"--collation-server={TARGET['collation_server']}",
        f"--default-time-zone={TARGET['time_zone']}",
        f"--lower-case-table-names={TARGET['lower_case_table_names']}",
    ]
    try:
        run([
            "docker",
            "run",
            "--detach",
            "--rm",
            "--name",
            container,
            "--env",
            "MYSQL_ALLOW_EMPTY_PASSWORD=yes",
            TARGET["docker_image"],
            *mysql_options,
        ])
        wait_for_mysql(container)

        failures = []
        for name, sql, expected in CASES:
            mysql = normalize(run([
                "docker",
                "exec",
                container,
                "mysql",
                "--batch",
                "--skip-column-names",
                "--raw",
                "--default-character-set=utf8mb4",
                "-uroot",
                "--execute",
                sql,
            ]).stdout)
            turso = normalize(run([str(turso_cli), sql], cwd=ROOT).stdout)
            if mysql != expected or turso != expected:
                failures.append((name, sql, expected, mysql, turso))
                print(f"FAIL {name}")
            else:
                print(f"PASS {name}")

        if failures:
            for name, sql, expected, mysql, turso in failures:
                print(f"\n{name}\nSQL: {sql}\nexpected: {expected!r}\nmysql: {mysql!r}\nturso: {turso!r}", file=sys.stderr)
            return 1
        print(f"\n{len(CASES)} differential cases passed against MySQL {TARGET['version']}")
        return 0
    finally:
        subprocess.run(["docker", "rm", "--force", container], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == "__main__":
    raise SystemExit(main())
