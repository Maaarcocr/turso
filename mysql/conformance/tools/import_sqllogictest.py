#!/usr/bin/env python3
"""Import SQLite sqllogictest scripts as MySQL spec candidates.

Reads the pinned sqllogictest checkout (see mysql/conformance/corpus/sources.lock.toml),
keeps only records that the reference harness would run for the `mysql` engine,
and writes candidate JSONL in the schema of mysql/conformance/SPECS.md.

Expected results in the scripts are SQLite's and are never copied: the recorder
captures real MySQL results. The scripts only contribute SQL, setup order, the
sort mode, and (for sampling) the size of the result they produced in SQLite.

Statement records become setup. Each `query` record (and each runnable
`statement error` record) becomes a candidate whose setup is every runnable
statement before it in the same file, in order. Setup statements that fail in
MySQL are dropped by the recorder (`"setup_errors": "drop"`): MySQL statements
are atomic, so a failed statement has no effect and dropping it is equivalent.

Only scripts are read; nothing from the checkout is executed.
"""

import argparse
import array
import collections
import concurrent.futures
import dataclasses
import importlib.util
import json
import pathlib
import re
import sys

ENGINE = "mysql"
SOURCE_NAME = "sqllogictest"
DEFAULT_REPO = "https://github.com/gregrahn/sqllogictest.git"
DEFAULT_REVISION = "c93ac416f05635ff5cc490009add936d9753d01d"
LICENSE = "CC0-1.0"
# MySQL's default exhaustive join-order search (optimizer_search_depth=62)
# takes from seconds to hours for FROM lists of more than about 20 tables
# (select5.test joins up to 64). Those queries are skipped; lists over MySQL's
# 61-table limit fail at once (error 1116) and are kept.
WIDE_JOIN_MIN = 21
MYSQL_MAX_JOIN_TABLES = 61
HASHED = re.compile(r"^(\d+) values hashing to [0-9a-f]{32}$")


def load_spec_lib():
    path = pathlib.Path(__file__).resolve().with_name("spec_lib.py")
    spec = importlib.util.spec_from_file_location("spec_lib", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


LIB = load_spec_lib()


@dataclasses.dataclass
class Record:
    kind: str
    line: int
    tokens: list
    conditions: list
    sql: str = ""
    results: list = dataclasses.field(default_factory=list)


def script_lines(text):
    """Model of sqllogictest.c `nextLine`: strip one trailing CR, blank all-space
    lines, and skip lines starting with `#` everywhere."""
    for number, raw in enumerate(text.split("\n"), start=1):
        line = raw[:-1] if raw.endswith("\r") else raw
        if line.strip() == "":
            line = ""
        if line.startswith("#"):
            continue
        yield number, line


def parse_records(text):
    """Split a script into records exactly like sqllogictest.c's main loop."""
    records = []
    lines = list(script_lines(text))
    index = 0
    while index < len(lines):
        while index < len(lines) and lines[index][1] == "":
            index += 1
        if index >= len(lines):
            break
        block = []
        while index < len(lines) and lines[index][1] != "":
            block.append(lines[index])
            index += 1
        conditions = []
        position = 0
        while position < len(block):
            tokens = block[position][1].split()
            if tokens and tokens[0] in ("skipif", "onlyif"):
                conditions.append((tokens[0], tokens[1] if len(tokens) > 1 else ""))
                position += 1
            else:
                break
        if position >= len(block):
            records.append(Record("conditions-only", block[-1][0], [], conditions))
            continue
        number, header = block[position]
        tokens = header.split()
        body = [line for _, line in block[position + 1:]]
        record = Record(tokens[0], number, tokens, conditions)
        if record.kind == "statement":
            record.sql = "\n".join(body)
        elif record.kind == "query":
            if "----" in body:
                separator = body.index("----")
                record.sql = "\n".join(body[:separator])
                record.results = body[separator + 1:]
            else:
                record.sql = "\n".join(body)
        records.append(record)
    return records


def runs_on(record, engine=ENGINE):
    for kind, db in record.conditions:
        match = db.lower() == engine
        if kind == "skipif" and match:
            return False
        if kind == "onlyif" and not match:
            return False
    return True


def result_rows_estimate(record):
    """Rows SQLite produced (values / columns); used only for sampling."""
    columns = max(1, len(record.tokens[1])) if len(record.tokens) > 1 else 1
    if record.results:
        match = HASHED.match(record.results[0])
        if match:
            return int(match.group(1)) // columns
    return len(record.results) // columns


def scan_records(text, stats):
    """Yield runnable records for the mysql engine in order, updating stats.

    Yields ("statement", line, sql, mode) and ("query", line, sql, ordered,
    rows_estimate). Stops at a `halt` that applies to mysql.
    """
    for record in parse_records(text):
        if record.kind == "conditions-only":
            stats["malformed-records"] += 1
            continue
        if record.kind in ("statement", "query"):
            stats[f"{record.kind}-records"] += 1
        if not runs_on(record):
            if record.kind in ("statement", "query"):
                stats[f"{record.kind}-skipped-for-mysql"] += 1
            continue
        if record.kind == "halt":
            stats["halted-files"] += 1
            return
        if record.kind == "hash-threshold":
            continue
        if record.kind == "statement":
            mode = record.tokens[1] if len(record.tokens) > 1 else ""
            if mode not in ("ok", "error"):
                stats["statement-bad-mode"] += 1
                continue
            stats[f"statement-{mode}-runnable"] += 1
            yield ("statement", record.line, record.sql.strip(), mode)
            continue
        if record.kind == "query":
            if len(record.tokens) < 2 or not re.fullmatch(r"[TIR]+", record.tokens[1]):
                stats["query-bad-types"] += 1
                continue
            sort = record.tokens[2] if len(record.tokens) > 2 else "nosort"
            sql = record.sql.strip()
            has_order_by = re.search(r"\bORDER\s+BY\b", sql, re.I) is not None
            if sort == "nosort" and not has_order_by:
                stats["query-nosort-without-order-by"] += 1
            stats["query-runnable"] += 1
            yield ("query", record.line, sql, sort == "nosort" and has_order_by,
                   result_rows_estimate(record))
            continue
        stats[f"unknown-record:{record.kind}"] += 1


def read_script(path):
    return path.read_bytes().decode("utf-8", errors="replace")


def scan_metadata(job):
    """Pass 1: compact per-file metadata, eligible queries in stable hash order."""
    path, root, max_rows = job
    relpath = path.relative_to(root).as_posix()
    group = group_name(relpath)
    stats = collections.Counter()
    statements = 0
    eligible = []
    for item in scan_records(read_script(path), stats):
        if item[0] == "statement":
            if item[3] == "error":
                eligible.append((LIB.stable_fraction(f"{group}:L{item[1]}"), item[1], statements))
            statements += 1
            continue
        _, line, sql, _ordered, rows = item
        if rows > max_rows:
            stats["excluded-too-large-in-source"] += 1
            continue
        if WIDE_JOIN_MIN <= LIB.from_width(sql) <= MYSQL_MAX_JOIN_TABLES:
            stats["excluded-wide-join"] += 1
            continue
        eligible.append((LIB.stable_fraction(f"{group}:L{line}"), line, statements))
    eligible.sort()
    lines = array.array("I", (line for _, line, _ in eligible))
    setup_lens = array.array("I", (setup for _, _, setup in eligible))
    return relpath, stats, statements, lines, setup_lens


def scan_selected(job):
    """Pass 2: statements and the SQL of the selected lines of one file."""
    path, root, wanted = job
    wanted = set(wanted)
    statements = []
    queries = []
    for item in scan_records(read_script(path), collections.Counter()):
        if item[0] == "statement":
            _, line, sql, mode = item
            if mode == "error" and line in wanted:
                queries.append({"line": line, "sql": sql, "setup_len": len(statements),
                                "ordered": True, "kind": "statement-error"})
            statements.append(sql)
            continue
        _, line, sql, ordered, _rows = item
        if line in wanted:
            queries.append({"line": line, "sql": sql, "setup_len": len(statements),
                            "ordered": ordered})
    return path.relative_to(root).as_posix(), statements, queries


def group_name(relpath):
    return f"{SOURCE_NAME}/{relpath[:-5] if relpath.endswith('.test') else relpath}"


def family(relpath):
    parts = relpath.split("/")
    if len(parts) == 1:
        return "select" if parts[0].startswith("select") else parts[0]
    if parts[0] in ("index", "random"):
        return f"{parts[0]}/{parts[1]}"
    return parts[0]


def make_candidate(relpath, statements, query, base_id, setup_from, setup_features, args):
    group = group_name(relpath)
    candidate_id = f"{group}:L{query['line']}"
    level, features = LIB.classify(statements[: query["setup_len"]], [query["sql"]], setup_features)
    record = {
        "id": candidate_id,
        "source": {
            "repo": args.repo,
            "revision": args.revision,
            "path": f"test/{relpath}",
            "line": query["line"],
            "license": LICENSE,
        },
        "group": group,
        "setup": statements[setup_from: query["setup_len"]],
        "sql": [query["sql"]],
        "ordered": query["ordered"],
        "level": level,
        "features": features,
        "setup_errors": "drop",
    }
    if base_id is not None:
        record["setup_base"] = base_id
    return record


def select_lines(metadata, args, report):
    """Deterministic sample within the test and replay-cost budgets.

    Each file keeps the same fraction of its eligible queries (at least
    --min-per-file), taken in stable-hash order so that raising the target only
    adds candidates. Per file, tests * average setup length stays within
    --file-setup-budget and tests stay within --max-per-file so that a file fits
    in one shard. The fraction is the largest whose totals stay within --target
    and --setup-budget.
    """
    files = []
    for relpath, _stats, _statements, lines, setup_lens in metadata:
        prefix = array.array("Q", [0])
        for value in setup_lens:
            prefix.append(prefix[-1] + value)
        count = len(lines)
        cap = min(args.max_per_file, count)
        if count and prefix[-1]:
            cap = min(cap, int(args.file_setup_budget // (prefix[-1] / count)))
        files.append((relpath, lines, prefix, count, cap))

    full_families = set(filter(None, args.full_families.split(",")))

    def plan(fraction):
        tests = cost = 0
        counts = {}
        for relpath, _lines, prefix, count, cap in files:
            if args.all or family(relpath) in full_families:
                keep = count if args.all else cap
            else:
                keep = min(cap, max(round(count * fraction), min(args.min_per_file, count)))
            counts[relpath] = keep
            tests += keep
            cost += prefix[keep]
        return counts, tests, cost

    if args.all:
        counts, _, _ = plan(1.0)
        fraction = 1.0
    else:
        low, high = 0.0, 1.0
        counts, tests, cost = plan(0.0)
        if tests > args.target or cost > args.setup_budget:
            raise SystemExit("per-file minimums alone exceed --target or --setup-budget")
        full = plan(1.0)
        if full[1] <= args.target and full[2] <= args.setup_budget:
            counts, low = full[0], 1.0
        else:
            for _ in range(50):
                middle = (low + high) / 2
                attempt = plan(middle)
                if attempt[1] <= args.target and attempt[2] <= args.setup_budget:
                    counts, low = attempt[0], middle
                else:
                    high = middle
        fraction = low
    report["sample-fraction-ppm"] = round(fraction * 1_000_000)
    return {relpath: sorted(lines[: counts[relpath]]) for relpath, lines, _, _, _ in files}


class SerialPool:
    """In-process stand-in for a process pool (--jobs 1)."""

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False

    def map(self, function, items, chunksize=1):
        return map(function, items)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--source", required=True, type=pathlib.Path,
                        help="sqllogictest checkout (the directory containing test/)")
    parser.add_argument("--output", type=pathlib.Path,
                        help="candidate JSONL to write")
    parser.add_argument("--report", type=pathlib.Path, help="write extraction counts as JSON")
    parser.add_argument("--repo", default=DEFAULT_REPO)
    parser.add_argument("--revision", default=DEFAULT_REVISION)
    parser.add_argument("--target", type=int, default=150_000,
                        help="maximum candidates across all files (default 150000)")
    parser.add_argument("--setup-budget", type=int, default=30_000_000,
                        help="maximum sum over candidates of setup statements (default 30M)")
    parser.add_argument("--file-setup-budget", type=int, default=250_000,
                        help="maximum setup statements replayed per file (default 250000)")
    parser.add_argument("--min-per-file", type=int, default=100,
                        help="keep at least this many candidates per file when budgets allow")
    parser.add_argument("--full-families", default="evidence,select",
                        help="families kept up to their per-file caps (comma separated)")
    parser.add_argument("--max-per-file", type=int, default=1000,
                        help="maximum candidates per file, so a file fits one shard")
    parser.add_argument("--max-rows", type=int, default=100,
                        help="skip queries whose SQLite result exceeded this many rows")
    parser.add_argument("--all", action="store_true",
                        help="emit every eligible candidate (local use; ignores budgets)")
    parser.add_argument("--stats", action="store_true", help="print per-family structure and exit")
    parser.add_argument("--jobs", type=int, default=2, help="parallel scanner processes (default 2)")
    args = parser.parse_args(argv)

    root = args.source / "test"
    paths = sorted(root.rglob("*.test"), key=lambda p: p.relative_to(root).as_posix())
    by_relpath = {path.relative_to(root).as_posix(): path for path in paths}

    report = collections.Counter()
    report["files"] = len(paths)
    with (concurrent.futures.ProcessPoolExecutor(max_workers=args.jobs) if args.jobs > 1
          else SerialPool()) as pool:
        metadata = list(pool.map(scan_metadata, [(p, root, args.max_rows) for p in paths], chunksize=4))
        for _relpath, stats, _statements, _lines, _setup_lens in metadata:
            report.update(stats)

        if args.stats:
            families = collections.defaultdict(collections.Counter)
            for relpath, _stats, statements, lines, setup_lens in metadata:
                fam = families[family(relpath)]
                fam["files"] += 1
                fam["statements"] += statements
                fam["eligible"] += len(lines)
                fam["max-setup"] = max(fam["max-setup"], statements)
                fam["setup-cost-all"] += sum(setup_lens)
                fam["segments"] += len(set(setup_lens))
            keys = ["files", "statements", "eligible", "max-setup", "setup-cost-all", "segments"]
            print("family".ljust(24), " ".join(k.rjust(16) for k in keys))
            for name in sorted(families):
                print(name.ljust(24), " ".join(str(families[name][k]).rjust(16) for k in keys))
            print(json.dumps(dict(sorted(report.items())), indent=1))
            return 0

        selected = select_lines(metadata, args, report)
        jobs = [(by_relpath[rel], root, list(lines)) for rel, lines in sorted(selected.items()) if lines]
        scanned = pool.map(scan_selected, jobs, chunksize=4)

        out = open(args.output, "w", encoding="utf-8") if args.output else sys.stdout
        emitted = collections.Counter()
        setup_cost = 0
        try:
            for relpath, statements, queries in scanned:
                base_id = None
                setup_from = 0
                setup_features = set()
                for query in queries:
                    if query.get("kind") != "statement-error" and not LIB.references_tables(query["sql"]):
                        # A query that reads no table cannot observe the file's
                        # setup (sqllogictest setups only create, fill and drop
                        # tables, indexes and views), so it needs none.
                        candidate = make_candidate(relpath, [], dict(query, setup_len=0), None, 0,
                                                   set(), args)
                        report["candidates-without-setup"] += 1
                    else:
                        for statement in statements[setup_from: query["setup_len"]]:
                            setup_features |= LIB.setup_statement_features(statement)
                        candidate = make_candidate(relpath, statements, query, base_id, setup_from,
                                                   setup_features, args)
                        setup_cost += query["setup_len"]
                        base_id = candidate["id"]
                        setup_from = query["setup_len"]
                    problems = LIB.validate_candidate(candidate)
                    if problems:
                        raise SystemExit(f"{candidate['id']}: invalid candidate: {problems}")
                    out.write(LIB.dumps_jsonl(candidate) + "\n")
                    emitted[family(relpath)] += 1
        finally:
            if out is not sys.stdout:
                out.close()
    report["candidates"] = sum(emitted.values())
    report["setup-statements-replayed"] = setup_cost
    summary = {
        "source": SOURCE_NAME,
        "repo": args.repo,
        "revision": args.revision,
        "license": LICENSE,
        "mode": "all" if args.all else "sample",
        "target": args.target,
        "setup_budget": args.setup_budget,
        "file_setup_budget": args.file_setup_budget,
        "max_per_file": args.max_per_file,
        "min_per_file": args.min_per_file,
        "full_families": args.full_families,
        "max_rows": args.max_rows,
        "counts": dict(sorted(report.items())),
        "candidates_by_family": dict(sorted(emitted.items())),
    }
    if args.report:
        args.report.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
    print(json.dumps(summary, indent=2, sort_keys=True), file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
