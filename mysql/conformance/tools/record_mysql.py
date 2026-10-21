#!/usr/bin/env python3
"""Record MySQL 8.4.10 behavior for spec candidates as .sqltest shards.

Reads candidate JSONL (mysql/conformance/SPECS.md), runs every candidate against
the pinned MySQL oracle twice in independent databases, and writes
`specs/<source>/` shards, `manifest.jsonl` and `REPORT.md`.

Semantics are "fresh database and session, run the setup, run the sql" per
candidate. For speed, candidates of one group whose setups are prefixes of one
statement sequence and whose sql is read-only share one database that advances
through the setup; everything else runs isolated.

`--verify` re-runs the checked-in shards of a source against MySQL and reports
every test whose recorded expectation no longer matches.

Reusable entry points (no MySQL needed once outcomes exist):
`load_candidates` builds per-group setup tries from candidate JSONL;
`classify_group` turns two cached runs into per-candidate outcomes;
`render_expectation` turns a recorded outcome into its expect block;
`write_specs` writes shards and manifests from (plans, outcomes);
`tally_outcomes` and `write_report` produce REPORT.md.
"""

import argparse
import collections
import concurrent.futures
import dataclasses
import gzip
import hashlib
import importlib.util
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import time
import tomllib
import urllib.parse
import uuid

try:
    import pymysql
except ImportError:  # unit tests of the pure functions do not need a driver
    pymysql = None

# Bump when a change alters recorded results, so cached group runs are redone.
RECORDER_VERSION = 2

ROOT = pathlib.Path(__file__).resolve().parents[3]
CONFORMANCE = ROOT / "mysql/conformance"
TARGET = tomllib.loads((CONFORMANCE / "target.toml").read_text())

DEFAULT_MAX_ROWS = 100
DEFAULT_MAX_EXPECT_BYTES = 16 * 1024
MAX_TESTS_PER_SHARD = 1000
LOCK = CONFORMANCE / "corpus/sources.lock.toml"

# The two determinism runs freeze the session clock at different instants
# (different year, month, day, time and fraction), so results that depend on
# NOW(), CURRENT_TIMESTAMP defaults and the like differ and are classified
# nondeterministic instead of being recorded.
RUN_TIMESTAMPS = {"a": "1700000000.250000", "b": "1913137217.750000"}

# Errors that an imported case most likely produces because the importer could
# not reproduce its context (a dialect it was not written for, or setup that
# was not extracted). Recording them would make missing setup look like
# expected behavior, so they are quarantined instead of recorded.
QUARANTINED_ERRNOS = {
    1064: "syntax error",
    1146: "table does not exist",
    1049: "unknown database",
    1054: "unknown column",
    1051: "unknown table",
}
# Client-side errors: the oracle did not produce a result.
TIMEOUT = "timeout"
CLIENT_ERRNOS = {2006, 2013, 2014, 2055}

# Server options that only change durability and therefore no SQL-visible
# behavior. They make recording much faster (autocommit fsyncs dominate).
PERFORMANCE_OPTIONS = [
    "--innodb-flush-log-at-trx-commit=2",
    "--sync-binlog=0",
]


def load_spec_lib():
    path = pathlib.Path(__file__).resolve().with_name("spec_lib.py")
    spec = importlib.util.spec_from_file_location("spec_lib", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


LIB = load_spec_lib()


def log(message):
    print(f"[record {time.strftime('%H:%M:%S')}] {message}", file=sys.stderr, flush=True)


# ---------------------------------------------------------------------------
# MySQL server and sessions
# ---------------------------------------------------------------------------

@dataclasses.dataclass(frozen=True)
class Dsn:
    host: str
    port: int
    user: str
    password: str

    @staticmethod
    def parse(text):
        parsed = urllib.parse.urlparse(text)
        if parsed.scheme != "mysql":
            raise SystemExit("--dsn must look like mysql://user[:password]@host[:port]")
        return Dsn(parsed.hostname or "127.0.0.1", parsed.port or 3306,
                   urllib.parse.unquote(parsed.username or "root"),
                   urllib.parse.unquote(parsed.password or ""))

    def __str__(self):
        return f"mysql://{self.user}@{self.host}:{self.port}"


def connect(dsn, database=None, read_timeout=None):
    return pymysql.connect(
        host=dsn.host,
        port=dsn.port,
        user=dsn.user,
        password=dsn.password,
        database=database,
        charset=TARGET["character_set_server"],
        collation=TARGET["collation_server"],
        autocommit=True,
        use_unicode=False,
        conv={},
        connect_timeout=30,
        read_timeout=read_timeout,
        write_timeout=read_timeout,
    )


def scalar(conn, sql):
    with conn.cursor() as cursor:
        cursor.execute(sql)
        value = cursor.fetchone()[0]
    return value.decode() if isinstance(value, bytes) else value


EXPECTED_SESSION = {
    "@@collation_connection": TARGET["collation_server"],
    "@@character_set_client": TARGET["character_set_server"],
    "@@character_set_results": TARGET["character_set_server"],
    "@@autocommit": "1",
    "@@sql_mode": TARGET["sql_mode"],
    "@@time_zone": TARGET["time_zone"],
}


def check_profile(dsn):
    """Refuse to record against a server that does not match target.toml."""
    conn = connect(dsn)
    try:
        server = {
            "version": scalar(conn, "SELECT @@version"),
            "sql_mode": scalar(conn, "SELECT @@GLOBAL.sql_mode"),
            "character_set_server": scalar(conn, "SELECT @@character_set_server"),
            "collation_server": scalar(conn, "SELECT @@collation_server"),
            "time_zone": scalar(conn, "SELECT @@GLOBAL.time_zone"),
            "lower_case_table_names": int(scalar(conn, "SELECT @@lower_case_table_names")),
        }
        problems = [
            f"{key}: server has {server[key]!r}, target.toml wants {TARGET[key]!r}"
            for key in ("sql_mode", "character_set_server", "collation_server", "time_zone",
                        "lower_case_table_names")
            if server[key] != TARGET[key]
        ]
        if server["version"] != TARGET["version"]:
            problems.append(f"version: server is {server['version']}, target is {TARGET['version']}")
        check_session(conn, problems)
    finally:
        conn.close()
    if problems:
        raise SystemExit("MySQL server does not match target.toml:\n  " + "\n  ".join(problems))
    return server


def check_session(conn, problems):
    """Append a problem for every session variable that differs from the profile."""
    with conn.cursor() as cursor:
        cursor.execute("SELECT " + ", ".join(EXPECTED_SESSION))
        actual = [value.decode() for value in cursor.fetchone()]
    for (variable, expected), value in zip(EXPECTED_SESSION.items(), actual):
        if value != str(expected):
            problems.append(f"session {variable} is {value!r}, expected {expected!r}")
    return problems


class Container:
    """The pinned MySQL image, started exactly like differential.py."""

    def __init__(self, keep):
        self.name = f"turso-mysql-recorder-{uuid.uuid4().hex[:10]}"
        self.keep = keep

    def start(self):
        options = [
            f"--sql-mode={TARGET['sql_mode']}",
            f"--character-set-server={TARGET['character_set_server']}",
            f"--collation-server={TARGET['collation_server']}",
            f"--default-time-zone={TARGET['time_zone']}",
            f"--lower-case-table-names={TARGET['lower_case_table_names']}",
            *PERFORMANCE_OPTIONS,
        ]
        subprocess.run(
            ["docker", "run", "--detach", "--rm", "--name", self.name,
             "--env", "MYSQL_ALLOW_EMPTY_PASSWORD=yes", TARGET["docker_image"], *options],
            check=True, capture_output=True, text=True,
        )
        address = subprocess.run(
            ["docker", "inspect", "--format", "{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}",
             self.name],
            check=True, capture_output=True, text=True,
        ).stdout.split()[0]
        dsn = Dsn(address, 3306, "root", "")
        deadline = time.monotonic() + 240
        while True:
            # The entrypoint first runs a temporary server with networking
            # disabled, so only a TCP connection proves the real server is up.
            try:
                connect(dsn).close()
                return dsn
            except pymysql.err.OperationalError:
                if time.monotonic() > deadline:
                    logs = subprocess.run(["docker", "logs", self.name], capture_output=True, text=True)
                    raise RuntimeError(f"MySQL did not become ready\n{logs.stdout}\n{logs.stderr}")
                time.sleep(2)

    def stop(self):
        if self.keep:
            log(f"leaving container {self.name} running")
            return
        subprocess.run(["docker", "rm", "--force", self.name], capture_output=True)


# ---------------------------------------------------------------------------
# Candidates and group plans
# ---------------------------------------------------------------------------

@dataclasses.dataclass
class Candidate:
    record: dict
    node: int
    lane: str  # chain | isolated | exclusive
    index: int = 0

    @property
    def id(self):
        return self.record["id"]


@dataclasses.dataclass
class GroupPlan:
    """A trie of setup statements with candidates attached to nodes."""

    name: str
    parent: list = dataclasses.field(default_factory=list)
    statement: list = dataclasses.field(default_factory=list)
    children: dict = dataclasses.field(default_factory=dict)
    candidates: list = dataclasses.field(default_factory=list)
    node_problem: list = dataclasses.field(default_factory=list)
    node_forbidden: list = dataclasses.field(default_factory=list)

    def child(self, parent, statement):
        key = (parent, statement)
        node = self.children.get(key)
        if node is None:
            node = len(self.parent)
            self.children[key] = node
            self.parent.append(parent)
            self.statement.append(statement)
            inherited = self.node_problem[parent] if parent >= 0 else None
            self.node_problem.append(inherited or LIB.statement_representable(statement))
            inherited = self.node_forbidden[parent] if parent >= 0 else None
            self.node_forbidden.append(inherited or LIB.server_mutation(statement))
        return node

    def forbidden_at(self, node):
        return self.node_forbidden[node]

    def path(self, node):
        nodes = []
        while node >= 0:
            nodes.append(node)
            node = self.parent[node]
        nodes.reverse()
        return nodes

    def runnable(self):
        """The part of the plan the workers execute (no records, no indexes)."""
        return {
            "name": self.name,
            "parent": self.parent,
            "statement": self.statement,
            "candidates": [
                {"node": c.node, "lane": c.lane, "sql": c.record["sql"]}
                for c in self.candidates
            ],
        }

    def digest(self, max_rows):
        body = json.dumps(
            [RECORDER_VERSION, TARGET["docker_image"], max_rows, self.parent, self.statement,
             [[c.id, c.node, c.lane, c.record["sql"], c.record["ordered"],
               c.record.get("setup_errors", "fail")] for c in self.candidates]],
            ensure_ascii=False, separators=(",", ":"),
        )
        return hashlib.sha256(body.encode()).hexdigest()


def candidate_lane(setup_statements, sql):
    statements = list(setup_statements) + list(sql)
    if any(LIB.touches_global_state(s) for s in statements):
        return "exclusive"
    if all(LIB.is_read_only(s) for s in sql):
        return "chain"
    return "isolated"


def load_candidates(paths, buckets, details):
    """Group candidates; resolve `setup_base` chains into a trie per group."""
    plans = {}
    seen_ids = {}
    for path in paths:
        with open(path, encoding="utf-8") as handle:
            for number, line in enumerate(handle, start=1):
                if not line.strip():
                    continue
                record = json.loads(line)
                problems = LIB.validate_candidate(record)
                if problems:
                    buckets["invalid"] += 1
                    details["invalid"].append(f"{path}:{number}: {'; '.join(problems)}")
                    continue
                if record["id"] in seen_ids:
                    buckets["duplicate"] += 1
                    continue
                record["setup"] = [LIB.rust_trim(statement) for statement in record["setup"]]
                record["sql"] = [LIB.rust_trim(statement) for statement in record["sql"]]
                group = record["group"]
                plan = plans.get(group)
                if plan is None:
                    plan = plans[group] = GroupPlan(group)
                    plan.content = set()
                    plan.lineage = {}
                base = record.get("setup_base")
                if base is not None:
                    if base not in plan.lineage:
                        buckets["invalid"] += 1
                        details["invalid"].append(f"{record['id']}: setup_base {base} is not an earlier candidate of its group")
                        continue
                    node, has_global = plan.lineage[base]
                else:
                    node, has_global = -1, False
                for statement in record["setup"]:
                    node = plan.child(node, statement)
                    has_global = has_global or LIB.touches_global_state(statement)
                plan.lineage[record["id"]] = (node, has_global)
                seen_ids[record["id"]] = True
                key = (node, tuple(record["sql"]), record["ordered"])
                if key in plan.content:
                    buckets["duplicate"] += 1
                    continue
                plan.content.add(key)
                forbidden = next(filter(None, (LIB.server_mutation(s) for s in record["sql"])), None)
                forbidden = forbidden or (plan.forbidden_at(node) if node >= 0 else None)
                if forbidden:
                    buckets["unsupported-environment"] += 1
                    details["unsupported-environment"].append(forbidden)
                    continue
                if has_global:
                    lane = "exclusive"
                else:
                    lane = candidate_lane([], record["sql"])
                plan.candidates.append(Candidate(record, node, lane))
    for plan in plans.values():
        del plan.content
        del plan.lineage
        for index, candidate in enumerate(plan.candidates):
            candidate.index = index
    return plans


# ---------------------------------------------------------------------------
# Execution (runs in worker processes)
# ---------------------------------------------------------------------------

def render_value(value):
    if value is None:
        return "NULL"
    return value.decode("utf-8", errors="replace")


class Session:
    """One fresh database plus one fresh connection."""

    counter = 0

    def __init__(self, runner):
        Session.counter += 1
        self.runner = runner
        self.database = f"rec_{runner.label}_{os.getpid()}_{Session.counter}"
        runner.admin_execute(f"CREATE DATABASE `{self.database}`")
        self.conn = connect(runner.dsn, self.database, runner.timeout)
        problems = check_session(self.conn, [])
        if problems:
            raise RuntimeError("; ".join(problems))
        with self.conn.cursor() as cursor:
            cursor.execute(f"SET timestamp = {RUN_TIMESTAMPS[runner.label]}")
        self.alive = True

    def execute(self, sql, max_rows):
        """Run one statement. Returns ("rows", rows) or ("error", errno, sqlstate, message)."""
        try:
            with self.conn.cursor() as cursor:
                cursor.execute(sql)
                if cursor.description is None:
                    return ("rows", [])
                rows = cursor.fetchall()
                if len(rows) > max_rows:
                    return ("too-large", len(rows))
                return ("rows", [[render_value(v) for v in row] for row in rows])
        except pymysql.err.MySQLError as error:
            errno = error.args[0] if error.args else 0
            message = str(error.args[1]) if len(error.args) > 1 else str(error)
            if errno in CLIENT_ERRNOS or not isinstance(errno, int) or errno < 1000:
                self.alive = False
                return (TIMEOUT, errno, message)
            message = message.replace(self.database, "test")
            return ("error", errno, getattr(error, "sqlstate", None) or "", message)

    def close(self):
        try:
            if self.alive:
                self.conn.close()
            else:
                self.runner.kill(self.conn)
        except Exception:
            pass
        self.runner.admin_execute(f"DROP DATABASE IF EXISTS `{self.database}`")


class Runner:
    def __init__(self, dsn, label, timeout, max_rows):
        self.dsn = dsn
        self.label = label
        self.timeout = timeout
        self.max_rows = max_rows
        self.admin = None

    def admin_execute(self, sql):
        for attempt in range(3):
            try:
                if self.admin is None:
                    self.admin = connect(self.dsn, read_timeout=120)
                with self.admin.cursor() as cursor:
                    cursor.execute(sql)
                return
            except pymysql.err.OperationalError:
                self.admin = None
                if attempt == 2:
                    raise
                time.sleep(1)

    def kill(self, conn):
        thread = conn.thread_id()
        try:
            self.admin_execute(f"KILL {int(thread)}")
        except pymysql.err.MySQLError:
            pass

    def run_plan(self, plan, lanes):
        """Execute the given lanes of a plan; returns JSON-serializable results.

        node_errors: {node: [errno, sqlstate, message]} for setup statements
        that failed. results: {candidate index: result}.
        """
        parent = plan["parent"]
        statements = plan["statement"]
        node_errors = {}
        results = {}
        chain = collections.defaultdict(list)
        others = []
        for index, candidate in enumerate(plan["candidates"]):
            if candidate["lane"] not in lanes:
                continue
            if candidate["lane"] == "chain":
                chain[candidate["node"]].append(index)
            else:
                others.append(index)

        def run_statement(session, node):
            outcome = session.execute(statements[node], self.max_rows)
            if outcome[0] == "error":
                node_errors.setdefault(node, list(outcome[1:]))
            elif outcome[0] == TIMEOUT:
                node_errors.setdefault(node, [0, "", f"client error: {outcome[2]}"])
            return session.alive

        def run_sql(session, index):
            rows = []
            for sql in plan["candidates"][index]["sql"]:
                outcome = session.execute(sql, self.max_rows)
                if outcome[0] != "rows":
                    return list(outcome)
                rows.extend(outcome[1])
                if len(rows) > self.max_rows:
                    return ["too-large", len(rows)]
            return ["rows", rows]

        # Chain lane: depth-first walk of the trie. A node that does not
        # continue the current database (a sibling branch, or after a lost
        # connection) gets a fresh database that replays its prefix.
        state = {"session": None, "at": None}

        def reset_to(target):
            if state["session"] is not None:
                state["session"].close()
            session = Session(self)
            state["session"], state["at"] = session, -1
            for ancestor in path_of(parent, target):
                if not run_statement(session, ancestor):
                    session.close()
                    state["session"] = None
                    return False
                state["at"] = ancestor
            return True

        needed = set()
        for node in chain:
            while node >= 0 and node not in needed:
                needed.add(node)
                node = parent[node]
        kids = collections.defaultdict(list)
        for node in sorted(needed):
            kids[parent[node]].append(node)
        dead = set()
        stack = [-1]
        while stack:
            node = stack.pop()
            stack.extend(reversed(kids.get(node, [])))
            if node >= 0 and parent[node] in dead:
                dead.add(node)
                continue
            if node >= 0:
                session = state["session"]
                if session is None or not session.alive or state["at"] != parent[node]:
                    if not reset_to(parent[node]):
                        dead.add(node)
                        continue
                if not run_statement(state["session"], node):
                    state["session"].close()
                    state["session"] = None
                    dead.add(node)
                    continue
                state["at"] = node
            for index in chain.get(node, []):
                session = state["session"]
                if session is None or not session.alive or state["at"] != node:
                    if not reset_to(node):
                        break
                results[index] = run_sql(state["session"], index)
        if state["session"] is not None:
            state["session"].close()
        for node, indexes in chain.items():
            for index in indexes:
                results.setdefault(index, [TIMEOUT, 0, "setup statement lost the connection"])

        # Isolated and exclusive lanes: fresh database and session each.
        for index in others:
            node = plan["candidates"][index]["node"]
            session = Session(self)
            try:
                ok = True
                for ancestor in path_of(parent, node):
                    if not run_statement(session, ancestor):
                        ok = False
                        break
                results[index] = run_sql(session, index) if ok else [TIMEOUT, 0, "setup statement failed"]
            finally:
                session.close()
            if plan["candidates"][index]["lane"] == "exclusive":
                self.cleanup_global()
        return {"node_errors": {str(k): v for k, v in node_errors.items()},
                "results": {str(k): v for k, v in results.items()}}

    def cleanup_global(self):
        """After an exclusive candidate: drop databases and users it created."""
        conn = connect(self.dsn)
        try:
            with conn.cursor() as cursor:
                cursor.execute("SHOW DATABASES")
                for (name,) in cursor.fetchall():
                    name = name.decode()
                    if name not in self.baseline_databases and not name.startswith("rec_"):
                        cursor.execute(f"DROP DATABASE `{name}`")
                cursor.execute("SELECT user, host FROM mysql.user")
                for user, host in cursor.fetchall():
                    if (user, host) not in self.baseline_users:
                        cursor.execute("DROP USER %s@%s", (user.decode(), host.decode()))
        finally:
            conn.close()


def path_of(parent, node):
    nodes = []
    while node is not None and node >= 0:
        nodes.append(node)
        node = parent[node]
    nodes.reverse()
    return nodes


_WORKER = {}


def worker_init(dsn, timeout, max_rows):
    _WORKER["dsn"] = dsn
    _WORKER["timeout"] = timeout
    _WORKER["max_rows"] = max_rows


def worker_run(job):
    plan, label, lanes = job
    runner = Runner(_WORKER["dsn"], label, _WORKER["timeout"], _WORKER["max_rows"])
    started = time.monotonic()
    try:
        result = runner.run_plan(plan, lanes)
    except Exception as error:  # one broken group must not abort the whole run
        return plan["name"], label, {"failed": f"{type(error).__name__}: {error}",
                                     "node_errors": {}, "results": {}}
    result["seconds"] = round(time.monotonic() - started, 3)
    return plan["name"], label, result


# ---------------------------------------------------------------------------
# Classification of recorded results
# ---------------------------------------------------------------------------

def same_result(left, right, ordered):
    if left[0] != right[0]:
        return False
    if left[0] == "rows":
        if ordered:
            return left[1] == right[1]
        return sorted(left[1]) == sorted(right[1])
    return left == right


def classify_group(plan, runs, max_expect_bytes):
    """Merge runs A and B into a per-candidate outcome.

    Returns {index: outcome}; an outcome is a dict with "bucket" and, for
    emitted tests, the expectation and the dropped setup nodes.
    """
    errors_a = {int(k): v for k, v in runs["a"]["node_errors"].items()}
    errors_b = {int(k): v for k, v in runs["b"]["node_errors"].items()}
    outcomes = {}
    for candidate in plan.candidates:
        index = candidate.index
        record = candidate.record
        key = str(index)
        path = plan.path(candidate.node)
        problem = next(filter(None, (LIB.statement_representable(s) for s in record["sql"])), None)
        result_a = runs["a"]["results"].get(key)
        result_b = runs["b"]["results"].get(key)
        if result_a is None or result_b is None:
            outcomes[index] = {"bucket": "not-run"}
            continue
        failed_a = [n for n in path if n in errors_a]
        failed_b = [n for n in path if n in errors_b]
        if candidate.node >= 0 and plan.node_problem[candidate.node] and not problem:
            # Only statements that are emitted must be representable; a
            # dropped setup statement is left out of the shard.
            dropped = set(failed_a)
            problem = next(filter(None, (LIB.statement_representable(plan.statement[n])
                                         for n in path if n not in dropped)), None)
        if [errors_a[n] for n in failed_a] != [errors_b[n] for n in failed_b] or failed_a != failed_b:
            outcomes[index] = {"bucket": "nondeterministic", "detail": "setup errors differ"}
            continue
        if failed_a and failed_a[-1] == candidate.node and LIB.reads_diagnostics(record["sql"][0]):
            # The diagnostics area belongs to the dropped statement, which the
            # replay never runs.
            outcomes[index] = {"bucket": "setup-failed",
                               "detail": "diagnostics read after a dropped setup statement"}
            continue
        if any(LIB.reads_storage_statistics(s) for s in record["sql"]):
            outcomes[index] = {"bucket": "nondeterministic",
                               "detail": "reads asynchronously updated storage statistics"}
            continue
        if failed_a and record.get("setup_errors", "fail") == "fail":
            first = errors_a[failed_a[0]]
            outcomes[index] = {"bucket": "setup-failed", "detail": f"{first[0]} {first[2]}"}
            continue
        if any(errors_a[n][0] == 0 for n in failed_a):
            outcomes[index] = {"bucket": TIMEOUT, "detail": "setup statement timed out"}
            continue
        if result_a[0] == TIMEOUT or result_b[0] == TIMEOUT:
            outcomes[index] = {"bucket": TIMEOUT, "detail": str(result_a[1:] if result_a[0] == TIMEOUT else result_b[1:])}
            continue
        if result_a[0] == "too-large" or result_b[0] == "too-large":
            outcomes[index] = {"bucket": "too-large", "detail": "row limit"}
            continue
        if not same_result(result_a, result_b, record["ordered"]):
            outcomes[index] = {"bucket": "nondeterministic", "detail": "results differ"}
            continue
        if result_a[0] == "error" and result_a[1] in QUARANTINED_ERRNOS:
            outcomes[index] = {"bucket": "error-quarantined", "detail": str(result_a[1])}
            continue
        if problem:
            outcomes[index] = {"bucket": "unrepresentable", "detail": problem}
            continue
        if result_a[0] == "rows":
            rows = result_a[1]
            raw = False
            reason = LIB.expectation_round_trips(rows, record["ordered"])
            if reason and (record["ordered"] or len(rows) <= 1):
                # `expect raw` keeps leading and trailing whitespace.
                raw = LIB.expectation_round_trips(rows, record["ordered"], raw=True) is None
                reason = None if raw else reason
            if reason:
                outcomes[index] = {"bucket": "unrepresentable", "detail": reason}
                continue
            lines = ["|".join(row) for row in rows]
            if not record["ordered"]:
                # MySQL's row order is not stable for unordered results.
                lines.sort()
            if sum(len(line) + 5 for line in lines) > max_expect_bytes:
                outcomes[index] = {"bucket": "too-large", "detail": "expect bytes"}
                continue
            outcomes[index] = {"bucket": "recorded", "kind": "rows", "lines": lines,
                               "dropped": failed_a, "raw": raw}
        else:
            errno, sqlstate, message = result_a[1], result_a[2], result_a[3]
            outcomes[index] = {"bucket": "recorded", "kind": "error",
                               "error": [errno, sqlstate, " ".join(message.split())],
                               "dropped": failed_a}
    return outcomes


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------

def slug(text):
    value = re.sub(r"[^a-z0-9_-]+", "-", text.lower()).strip("-")
    if not value or not re.match(r"[a-z_]", value):
        value = "t-" + value
    return value


def source_of(group):
    return group.split("/", 1)[0]


def group_relpath(group):
    rest = group.split("/", 1)[1] if "/" in group else group
    parts = [re.sub(r"[^A-Za-z0-9_.-]+", "_", part).strip(".") or "_" for part in rest.split("/")]
    return "/".join(parts)


def natural_key(candidate):
    source = candidate.record["source"]
    line = source.get("line")
    # Numbers inside ids sort numerically (`:9` before `:10`).
    id_key = [(0, int(part), "") if part.isdigit() else (1, 0, part)
              for part in re.split(r"(\d+)", candidate.id) if part]
    return (str(source.get("path", "")), line if isinstance(line, int) else 0, id_key)


def build_shards(plan, outcomes):
    """Split the emitted tests of a group into shards of rendered text."""
    emitted = [c for c in plan.candidates if outcomes[c.index]["bucket"] == "recorded"]
    emitted.sort(key=natural_key)
    shards = []
    relpath = group_relpath(plan.name)
    for start in range(0, len(emitted), MAX_TESTS_PER_SHARD):
        part = emitted[start:start + MAX_TESTS_PER_SHARD]
        number = start // MAX_TESTS_PER_SHARD + 1
        path = f"{relpath}.sqltest" if number == 1 else f"{relpath}-{number}.sqltest"
        shards.append((path, part))
    return shards


def test_name(candidate, plan):
    """The id after its group (`<group>:L12` -> `l12`), else after its last ':'."""
    remainder = candidate.id
    if remainder.startswith(plan.name):
        remainder = remainder[len(plan.name):]
    elif ":" in remainder:
        remainder = remainder.rsplit(":", 1)[1]
    return slug(remainder) if slug(remainder) != "t-" else slug(candidate.id)


def render_expectation(outcome, ordered):
    """Turn a recorded outcome into (comment lines, expect block text).

    `outcome` is a "recorded" outcome of `classify_group`: {"kind": "rows",
    "lines": [...], "raw": bool} or {"kind": "error", "error": [errno,
    sqlstate, message]}. The comment lines go before the test's decorators.
    """
    comments = []
    if outcome["kind"] == "error":
        errno, sqlstate, message = outcome["error"]
        comments.append(f"# mysql-error: {errno} {sqlstate} {message}")
        opening = "expect error {"
    elif outcome.get("raw"):
        opening = "expect raw {"
    elif ordered:
        opening = "expect {"
    else:
        opening = "expect unordered {"
    lines = [opening]
    if outcome["kind"] == "rows" and outcome["lines"]:
        render = LIB.render_raw_block_lines if outcome.get("raw") else LIB.render_block_lines
        lines.append(render(outcome["lines"])[:-1])
    lines.append("}")
    return comments, "\n".join(lines)


def render_shard(plan, tests, outcomes, header):
    """Render one shard. Returns (text, [(candidate, name)])."""
    # Cut points: nodes with tests, plus branch points between them.
    live = collections.Counter()
    test_nodes = set()
    for candidate in tests:
        if candidate.node >= 0:
            test_nodes.add(candidate.node)
    for node in test_nodes:
        for step in plan.path(node):
            live[step] += 0
    children = collections.Counter(plan.parent[node] for node in live)
    cuts = {node for node in live if node in test_nodes or children[node] > 1}
    dropped = set()
    for candidate in tests:
        dropped.update(outcomes[candidate.index]["dropped"])

    segment_names = {}
    segment_bodies = []

    def segments_for(node):
        chain = []
        start_segment = []
        for step in plan.path(node):
            if step not in dropped:
                start_segment.append(step)
            if step in cuts:
                if start_segment:
                    key = tuple(start_segment)
                    if key not in segment_names:
                        segment_names[key] = f"s{len(segment_names) + 1}"
                        segment_bodies.append((segment_names[key], [plan.statement[n] for n in key]))
                    chain.append(segment_names[key])
                start_segment = []
        return chain

    names = {}
    used = set()
    blocks = []
    for candidate in tests:
        name = test_name(candidate, plan)
        base = name
        counter = 2
        while name in used:
            name = f"{base}-{counter}"
            counter += 1
        used.add(name)
        names[candidate.index] = name
        outcome = outcomes[candidate.index]
        lines = []
        record = candidate.record
        if header["multi_path"]:
            source = record["source"]
            lines.append(f"# source: {source['path']}:{source['line']}")
        comments, expect = render_expectation(outcome, record["ordered"])
        lines.extend(comments)
        for segment in segments_for(candidate.node) if candidate.node >= 0 else []:
            lines.append(f"@setup {segment}")
        body = LIB.escape_block(LIB.sql_block_text(record["sql"], "    "))
        lines.append(f"test {name} {{")
        lines.append(f"    {body}")
        lines.append("}")
        lines.append(expect)
        blocks.append("\n".join(lines))

    out = [
        "# Recorded from MySQL by mysql/conformance/tools/record_mysql.py. Do not edit;",
        "# re-record instead (see mysql/conformance/SPECS.md).",
        f"# oracle: MySQL {TARGET['version']} ({TARGET['docker_image'].split('@')[0]}), default profile from target.toml",
        f"# source: {header['repo']} @ {header['revision']}",
    ]
    out.extend(f"#   {path}" for path in header["paths"])
    out.append(f"# license: {header['license']}")
    out.append("@database :memory:")
    out.append("")
    for name, statements in segment_bodies:
        out.append(f"setup {name} {{")
        out.append("    " + LIB.escape_block(LIB.sql_block_text(statements, "    ")))
        out.append("}")
        out.append("")
    for block in blocks:
        out.append(block)
        out.append("")
    return "\n".join(out), [(c, names[c.index]) for c in tests]


def group_header(plan, tests):
    sources = [c.record["source"] for c in tests]
    paths = sorted({s["path"] for s in sources})
    repos = sorted({s["repo"] for s in sources})
    revisions = sorted({s["revision"] for s in sources})
    licenses = sorted({s["license"] for s in sources})
    return {
        "repo": ", ".join(repos),
        "revision": ", ".join(revisions),
        "license": ", ".join(licenses),
        "paths": paths if len(paths) <= 5 else paths[:5] + [f"... and {len(paths) - 5} more"],
        "multi_path": len(paths) > 1,
    }


def local_only(source, plans, args):
    """Whether a source's recordings must stay in the ignored specs/local/.

    Sources are committed only when their entry in corpus/sources.lock.toml
    (matched through `spec_sources`) allows redistributing generated cases and
    no candidate carries a GPL license.
    """
    if args.local:
        return True, "--local"
    if any("GPL" in c.record["source"]["license"].upper() for plan in plans for c in plan.candidates):
        return True, "GPL-licensed candidates"
    lock = tomllib.loads(LOCK.read_text()) if LOCK.exists() else {"source": []}
    for entry in lock["source"]:
        if source in entry.get("spec_sources", []):
            if entry.get("redistribute_generated_cases", False):
                return False, ""
            return True, f"{LOCK.name} entry {entry['id']} does not allow redistribution"
    return True, f"no {LOCK.name} entry lists {source!r} in spec_sources"


# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------

BUCKET_ORDER = [
    "recorded", "nondeterministic", "unrepresentable", "too-large", "setup-failed",
    "error-quarantined", TIMEOUT, "unsupported-environment", "duplicate", "invalid", "not-run",
]


def write_report(path, source, stats, importer_report, server):
    lines = [f"# {source} recorded specs", ""]
    lines.append("Generated by `mysql/conformance/tools/record_mysql.py`; do not edit.")
    lines.append("")
    lines.append("## Provenance")
    lines.append("")
    for key in ("repo", "revision", "license"):
        lines.append(f"- {key}: {', '.join(sorted(stats['provenance'][key]))}")
    lines.append(f"- oracle: MySQL {server['version']} `{TARGET['docker_image']}`")
    lines.append(f"- profile: sql_mode `{server['sql_mode']}`, {server['character_set_server']}/"
                 f"{server['collation_server']}, time zone `{server['time_zone']}`, "
                 f"lower_case_table_names={server['lower_case_table_names']}")
    lines.append(f"- recorder version: {RECORDER_VERSION}; row limit {stats['max_rows']}; "
                 f"expect limit {stats['max_expect_bytes']} bytes")
    lines.append("")
    if isinstance(importer_report, str):
        lines.append("## Extraction")
        lines.append("")
        body = [line for line in importer_report.strip().splitlines() if not line.startswith("# ")]
        while body and not body[0].strip():
            body.pop(0)
        lines.extend("#" + line if line.startswith("#") else line for line in body)
        lines.append("")
    elif importer_report:
        lines.append("## Extraction")
        lines.append("")
        lines.append("| count | value |")
        lines.append("|---|---:|")
        for key, value in sorted(importer_report.get("counts", importer_report).items()):
            if isinstance(value, (int, float)):
                lines.append(f"| {key} | {value} |")
        for key in ("mode", "target", "setup_budget", "file_setup_budget", "min_per_file",
                    "max_per_file", "max_rows", "full_families"):
            if key in importer_report:
                lines.append(f"| importer {key} | {importer_report[key]} |")
        lines.append("")
    lines.append("## Recording buckets")
    lines.append("")
    lines.append("| bucket | candidates |")
    lines.append("|---|---:|")
    buckets = stats["buckets"]
    for bucket in BUCKET_ORDER + sorted(set(buckets) - set(BUCKET_ORDER)):
        if buckets.get(bucket):
            lines.append(f"| {bucket} | {buckets[bucket]} |")
    lines.append(f"| **total** | {sum(buckets.values())} |")
    lines.append("")
    lines.append(f"Recorded tests: {stats['tests']} ({stats['tests_rows']} result sets, "
                 f"{stats['tests_error']} errors) in {stats['shards']} shards. "
                 f"Distinct setup statements dropped because they fail in MySQL: "
                 f"{stats['dropped_statements']}.")
    lines.append("")
    for title, key in (("Recorded errors by errno", "recorded_errnos"),
                       ("Quarantined errors by errno", "quarantined_errnos"),
                       ("Dropped setup statements by errno", "dropped_errnos"),
                       ("Unrepresentable reasons", "unrepresentable"),
                       ("Setup failures", "setup_failed"),
                       ("Unsupported environment (never run)", "unsupported")):
        counter = stats[key]
        if not counter:
            continue
        lines.append(f"### {title}")
        lines.append("")
        lines.append("| value | count |")
        lines.append("|---|---:|")
        for value, count in sorted(counter.items(), key=lambda item: (-item[1], str(item[0])))[:25]:
            lines.append(f"| {value} | {count} |")
        lines.append("")
    lines.append("## Recorded tests by level")
    lines.append("")
    lines.append("| levels | tests |")
    lines.append("|---|---:|")
    for band, count in sorted(stats["levels"].items()):
        lines.append(f"| {band}–{band + 99} | {count} |")
    lines.append("")
    lines.append("## Recorded tests by feature")
    lines.append("")
    lines.append("Features include those the setup needs; see `tools/spec_lib.py`.")
    lines.append("")
    lines.append("| feature | tests |")
    lines.append("|---|---:|")
    for feature, count in sorted(stats["features"].items()):
        lines.append(f"| {feature} | {count} |")
    lines.append("")
    lines.append("## Recorded tests by directory")
    lines.append("")
    lines.append("| directory | tests |")
    lines.append("|---|---:|")
    for directory, count in sorted(stats["directories"].items()):
        lines.append(f"| {directory} | {count} |")
    lines.append("")
    path.write_text("\n".join(lines))


# ---------------------------------------------------------------------------
# Orchestration
# ---------------------------------------------------------------------------

def cache_path(cache_dir, plan, max_rows):
    return cache_dir / f"{plan.digest(max_rows)}.json.gz"


def run_all(plans, dsn, args, cache_dir):
    """Run every plan twice (labels a and b), reusing cached group runs."""
    runs = collections.defaultdict(dict)
    jobs = []
    exclusive = []
    for plan in plans:
        cached = cache_path(cache_dir, plan, args.max_rows) if cache_dir else None
        if cached and cached.exists():
            with gzip.open(cached, "rt", encoding="utf-8") as handle:
                runs[plan.name] = json.load(handle)
            continue
        runnable = plan.runnable()
        for label in ("a", "b"):
            jobs.append((runnable, label, ("chain", "isolated")))
        if any(c.lane == "exclusive" for c in plan.candidates):
            exclusive.append(runnable)
    if jobs:
        log(f"running {len(jobs) // 2} groups ({len(plans) - len(jobs) // 2} cached) on {args.jobs} connections")
    by_name = {plan.name: plan for plan in plans}
    pending = collections.Counter()
    for runnable, _label, _lanes in jobs:
        pending[runnable["name"]] += 1
    started = time.monotonic()
    done = 0
    # Longest setups first so that the slowest groups do not form a tail; the
    # two runs of a group use different workers and databases.
    order = sorted(jobs, key=lambda job: (-len(job[0]["statement"]) - len(job[0]["candidates"]),
                                          job[0]["name"], job[1]))
    with concurrent.futures.ProcessPoolExecutor(
        max_workers=args.jobs, initializer=worker_init, initargs=(dsn, args.timeout, args.max_rows)
    ) as pool:
        futures = [pool.submit(worker_run, job) for job in order]
        for future in concurrent.futures.as_completed(futures):
            name, label, result = future.result()
            runs[name][label] = result
            pending[name] -= 1
            done += 1
            if "failed" in result:
                log(f"group {name} run {label} failed: {result['failed']}")
            elif (pending[name] == 0 and "failed" not in runs[name].get("a" if label == "b" else "b", {})
                  and not any(c.lane == "exclusive" for c in by_name[name].candidates)):
                save_cache(cache_dir, by_name[name], runs[name], args.max_rows)
            if done % 50 == 0 or done == len(order):
                elapsed = time.monotonic() - started
                log(f"{done}/{len(order)} group runs in {elapsed:.0f}s")
    if exclusive:
        log(f"running {len(exclusive)} groups with server-global statements serially")
        worker_init(dsn, args.timeout, args.max_rows)
        for runnable in exclusive:
            for label in ("a", "b"):
                runner = Runner(dsn, label, args.timeout, args.max_rows)
                runner.baseline_databases, runner.baseline_users = server_baseline(dsn)
                try:
                    result = runner.run_plan(runnable, ("exclusive",))
                except Exception as error:
                    log(f"group {runnable['name']} exclusive run {label} failed: {error}")
                    runs[runnable["name"]][label]["failed"] = str(error)
                    continue
                merged = runs[runnable["name"]][label]
                merged["node_errors"].update(result["node_errors"])
                merged["results"].update(result["results"])
            if not any("failed" in runs[runnable["name"]][label] for label in ("a", "b")):
                save_cache(cache_dir, by_name[runnable["name"]], runs[runnable["name"]], args.max_rows)
    return runs


def server_baseline(dsn):
    conn = connect(dsn)
    try:
        with conn.cursor() as cursor:
            cursor.execute("SHOW DATABASES")
            databases = {row[0].decode() for row in cursor.fetchall()}
            cursor.execute("SELECT user, host FROM mysql.user")
            users = {(u, h) for u, h in cursor.fetchall()}
    finally:
        conn.close()
    return databases, users


def save_cache(cache_dir, plan, runs, max_rows):
    if not cache_dir:
        return
    path = cache_path(cache_dir, plan, max_rows)
    tmp = path.with_suffix(".tmp")
    with gzip.open(tmp, "wt", encoding="utf-8") as handle:
        json.dump(runs, handle, separators=(",", ":"))
    os.replace(tmp, path)


def check_files(paths, args):
    if args.no_check:
        log("skipping sqltest check (--no-check)")
        return
    binary = args.sqltest_bin
    if binary is None:
        for candidate in (os.environ.get("SQLTEST_BIN"), ROOT / "target/debug/sqltest",
                          ROOT / "target/release/sqltest"):
            if candidate and pathlib.Path(candidate).exists():
                binary = str(candidate)
                break
    if binary is None:
        raise SystemExit("no sqltest binary: pass --sqltest-bin, set SQLTEST_BIN, or --no-check")
    failures = []
    paths = [str(p) for p in paths]
    for start in range(0, len(paths), 200):
        chunk = paths[start:start + 200]
        result = subprocess.run([binary, "check", *chunk], capture_output=True, text=True)
        if result.returncode != 0:
            failures.append(result.stdout[-4000:] + result.stderr[-4000:])
    if failures:
        raise SystemExit("sqltest check rejected emitted files:\n" + "\n".join(failures))
    log(f"sqltest check accepted {len(paths)} files")


def new_stats(buckets, details, max_rows, max_expect_bytes):
    """Counters that `tally_outcomes` and `write_specs` fill for REPORT.md."""
    return {
        "buckets": buckets,
        "provenance": collections.defaultdict(set),
        "recorded_errnos": collections.Counter(),
        "quarantined_errnos": collections.Counter(),
        "dropped_errnos": collections.Counter(),
        "unrepresentable": collections.Counter(),
        "setup_failed": collections.Counter(),
        "unsupported": collections.Counter(details["unsupported-environment"]),
        "levels": collections.Counter(),
        "features": collections.Counter(),
        "directories": collections.Counter(),
        "tests": 0, "tests_rows": 0, "tests_error": 0, "shards": 0, "dropped_statements": 0,
        "max_rows": max_rows, "max_expect_bytes": max_expect_bytes,
    }


def tally_outcomes(plan, outcomes, runs, stats):
    """Count one group's buckets, quarantined errnos and dropped statements."""
    buckets = stats["buckets"]
    for candidate in plan.candidates:
        for key in ("repo", "revision", "license"):
            stats["provenance"][key].add(candidate.record["source"][key])
    node_errors = {int(k): v for k, v in runs["a"]["node_errors"].items()}
    dropped_nodes = set()
    for candidate in plan.candidates:
        outcome = outcomes[candidate.index]
        buckets[outcome["bucket"]] += 1
        if outcome["bucket"] == "error-quarantined":
            errno = int(outcome["detail"])
            stats["quarantined_errnos"][f"{errno} {QUARANTINED_ERRNOS[errno]}"] += 1
        elif outcome["bucket"] == "unrepresentable":
            stats["unrepresentable"][outcome["detail"]] += 1
        elif outcome["bucket"] == "setup-failed":
            stats["setup_failed"][outcome["detail"][:80]] += 1
        elif outcome["bucket"] == "recorded":
            dropped_nodes.update(outcome["dropped"])
    for node in dropped_nodes:
        stats["dropped_errnos"][node_errors[node][0]] += 1
    stats["dropped_statements"] += len(dropped_nodes)


def write_specs(out_dir, plans, outcomes_by_group, stats):
    """Write the shards and per-directory manifests of one source.

    `plans` are `GroupPlan`s (setup trie plus candidates, see
    `load_candidates`), `outcomes_by_group` maps a group name to
    {candidate index: outcome} as returned by `classify_group`; only
    "recorded" outcomes are written. Returns the written shard paths.
    """
    manifests = collections.defaultdict(list)
    written = []
    for plan in plans:
        outcomes = outcomes_by_group[plan.name]
        for shard_path, tests in build_shards(plan, outcomes):
            header = group_header(plan, tests)
            text, named = render_shard(plan, tests, outcomes, header)
            target = out_dir / shard_path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(text, encoding="utf-8")
            written.append(target)
            stats["shards"] += 1
            directory = str(pathlib.PurePosixPath(shard_path).parent)
            paths = sorted({c.record["source"]["path"] for c, _ in named})
            entry = {
                "shard": shard_path,
                "group": plan.name,
                "repo": header["repo"],
                "revision": header["revision"],
                "license": header["license"],
                "paths": paths,
                "tests": [],
            }
            manifests[directory].append(entry)
            for candidate, name in named:
                outcome = outcomes[candidate.index]
                record = candidate.record
                stats["tests"] += 1
                stats[f"tests_{outcome['kind']}"] += 1
                if outcome["kind"] == "error":
                    stats["recorded_errnos"][outcome["error"][0]] += 1
                stats["levels"][record["level"] // 100 * 100] += 1
                stats["features"].update(record["features"])
                stats["directories"][directory] += 1
                entry["tests"].append([name, record["id"], record["level"],
                                       paths.index(record["source"]["path"]), record["source"]["line"]])
    # One manifest per shard directory with one record per shard keeps the
    # files small: per test only [name, id, level, path index, line].
    for directory, entries in sorted(manifests.items()):
        with open(out_dir / directory / "manifest.jsonl", "w", encoding="utf-8") as handle:
            for entry in entries:
                handle.write(LIB.dumps_jsonl(entry) + "\n")
    return written


def record(args):
    buckets = collections.Counter()
    details = collections.defaultdict(list)
    plans = load_candidates(args.candidates, buckets, details)
    if details["invalid"]:
        for line in details["invalid"][:20]:
            log(f"invalid: {line}")
    sources = sorted({source_of(name) for name in plans})
    if args.source:
        plans = {name: plan for name, plan in plans.items() if source_of(name) == args.source}
        sources = [args.source]
    if len(sources) != 1:
        raise SystemExit(f"candidates span several sources {sources}; pass --source")
    source = sources[0]
    plan_list = [plans[name] for name in sorted(plans)]
    log(f"{sum(len(p.candidates) for p in plan_list)} candidates in {len(plan_list)} groups for {source}")
    lanes = collections.Counter(c.lane for p in plan_list for c in p.candidates)
    log(f"lanes: {dict(lanes)}")

    container = None
    if args.dsn:
        dsn = Dsn.parse(args.dsn)
    else:
        container = Container(args.keep_container)
        log(f"starting {TARGET['docker_image']} as {container.name}")
        dsn = container.start()
        log(f"MySQL ready at {dsn}")
    started = time.monotonic()
    try:
        server = check_profile(dsn)
        cache_dir = None if args.no_cache else args.cache / source
        if cache_dir:
            cache_dir.mkdir(parents=True, exist_ok=True)
        runs = run_all(plan_list, dsn, args, cache_dir)
    finally:
        if container:
            container.stop()
    elapsed = time.monotonic() - started

    local, why = local_only(source, plan_list, args)
    if local:
        log(f"{source} is recorded into specs/local/ (not committed): {why}")
    out_dir = args.out_dir / ("local" if local else "") / source
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)

    stats = new_stats(buckets, details, args.max_rows, args.max_expect_bytes)
    outcomes_by_group = {}
    for plan in plan_list:
        outcomes_by_group[plan.name] = classify_group(plan, runs[plan.name], args.max_expect_bytes)
        tally_outcomes(plan, outcomes_by_group[plan.name], runs[plan.name], stats)
    written = write_specs(out_dir, plan_list, outcomes_by_group, stats)
    importer_report = None
    if args.import_report:
        text = args.import_report.read_text()
        importer_report = text if args.import_report.suffix == ".md" else json.loads(text)
    write_report(out_dir / "REPORT.md", source, stats, importer_report, server)
    check_files(written, args)
    log(f"recorded {stats['tests']} tests into {stats['shards']} shards under {out_dir} "
        f"(oracle time {elapsed:.0f}s)")
    log("buckets: " + ", ".join(f"{k}={v}" for k, v in sorted(buckets.items())))
    if args.invalid_fails and buckets["invalid"]:
        return 1
    return 0


# ---------------------------------------------------------------------------
# Verify
# ---------------------------------------------------------------------------

def verify(args):
    source_dir = args.out_dir / args.source if args.source else None
    if source_dir is None or not source_dir.is_dir():
        raise SystemExit("--verify needs --source naming a directory under --out-dir")
    files = sorted(source_dir.rglob("*.sqltest"))
    plans = []
    expectations = {}
    for path in files:
        parsed = LIB.read_sqltest(path.read_text(encoding="utf-8"))
        name = f"{args.source}/{path.relative_to(source_dir).as_posix()}"
        plan = GroupPlan(name)
        for test in parsed["tests"]:
            node = -1
            for setup in test["setups"]:
                for statement in LIB.split_block_statements(parsed["setups"][setup]):
                    node = plan.child(node, statement)
            sql = LIB.split_block_statements(test["sql"])
            ordered = test["expect_kind"] != "unordered"
            record = {"id": f"{name}::{test['name']}", "sql": sql, "ordered": ordered,
                      "setup_errors": "fail", "source": {"path": str(path), "line": 0}}
            lane = candidate_lane([plan.statement[n] for n in plan.path(node)], sql)
            plan.candidates.append(Candidate(record, node, lane, len(plan.candidates)))
            expectations[(name, len(plan.candidates) - 1)] = test
        plans.append(plan)
    container = None
    if args.dsn:
        dsn = Dsn.parse(args.dsn)
    else:
        container = Container(args.keep_container)
        dsn = container.start()
    try:
        server = check_profile(dsn)
        log(f"verifying {len(expectations)} tests in {len(files)} files against MySQL {server['version']}")
        runs = run_all(plans, dsn, args, None)
    finally:
        if container:
            container.stop()
    drift = []
    for plan in plans:
        outcomes = classify_group(plan, runs[plan.name], 1 << 30)
        for candidate in plan.candidates:
            test = expectations[(plan.name, candidate.index)]
            outcome = outcomes[candidate.index]
            if outcome["bucket"] != "recorded" or outcome["dropped"]:
                drift.append((candidate.id, f"now {outcome['bucket']}: {outcome.get('detail', '')}"))
                continue
            if test["expect_kind"] == "error":
                recorded = next((c for c in test["comments"] if c.startswith("mysql-error:")), "")
                now = f"mysql-error: {outcome['error'][0]} {outcome['error'][1]}" if outcome["kind"] == "error" else "rows"
                if outcome["kind"] != "error" or not recorded.startswith(now):
                    drift.append((candidate.id, f"recorded {recorded!r}, now {now!r}"))
                continue
            if outcome["kind"] != "rows":
                drift.append((candidate.id, f"recorded rows, now error {outcome['error']}"))
                continue
            expected = LIB.expected_rows(test["expect_lines"])
            actual = [line.split("|") if line else [""] for line in outcome["lines"]]
            if test["expect_kind"] == "unordered":
                same = sorted("|".join(r) for r in expected) == sorted("|".join(r) for r in actual)
            else:
                same = LIB.format_rows(expected) == LIB.format_rows(actual)
            if not same:
                drift.append((candidate.id, "rows differ"))
    for test_id, reason in drift[:200]:
        print(f"DRIFT {test_id}: {reason}")
    print(f"{len(expectations) - len(drift)}/{len(expectations)} recorded specs still match MySQL")
    return 1 if drift else 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("candidates", nargs="*", type=pathlib.Path, help="candidate JSONL files")
    parser.add_argument("--source", help="source name to record or verify (default: the only one)")
    parser.add_argument("--out-dir", type=pathlib.Path, default=CONFORMANCE / "specs")
    parser.add_argument("--dsn", help="reuse a running server, e.g. mysql://root@172.17.0.2:3306")
    parser.add_argument("--keep-container", action="store_true", help="leave the started container running")
    parser.add_argument("--jobs", type=int, default=3, help="parallel MySQL connections (default 3)")
    parser.add_argument("--timeout", type=int, default=60, help="per-statement timeout in seconds")
    parser.add_argument("--max-rows", type=int, default=DEFAULT_MAX_ROWS)
    parser.add_argument("--max-expect-bytes", type=int, default=DEFAULT_MAX_EXPECT_BYTES)
    parser.add_argument("--cache", type=pathlib.Path, default=CONFORMANCE / ".record-cache",
                        help="per-group run cache for resumable recording")
    parser.add_argument("--no-cache", action="store_true")
    parser.add_argument("--import-report", type=pathlib.Path, help="importer JSON report to include in REPORT.md")
    parser.add_argument("--sqltest-bin", help="sqltest binary used to check emitted files")
    parser.add_argument("--no-check", action="store_true", help="do not run sqltest check")
    parser.add_argument("--invalid-fails", action="store_true", help="exit nonzero if any candidate is invalid")
    parser.add_argument("--verify", action="store_true", help="re-run checked-in specs and report drift")
    parser.add_argument("--local", action="store_true",
                        help="record into specs/local/ (ignored by git) regardless of the lock file")
    args = parser.parse_args(argv)
    if pymysql is None:
        raise SystemExit("record_mysql.py needs PyMySQL (pip install pymysql)")
    if args.verify:
        return verify(args)
    if not args.candidates:
        parser.error("candidate JSONL files are required")
    return record(args)


if __name__ == "__main__":
    raise SystemExit(main())
