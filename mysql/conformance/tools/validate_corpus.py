#!/usr/bin/env python3
import argparse
import collections
import json
import pathlib
import sys

LEVELS = {
    "01-literal-select",
    "02-schema",
    "03-insert",
    "04-query",
    "05-mutations",
    "06-transactions",
    "07-joins",
    "08-aggregation-window",
    "09-session-metadata",
    "10-security",
    "11-advanced",
}
SYNTAX_EXPECTATIONS = {"accept", "reject", "unknown"}
SOURCE_OUTCOMES = {"success", "execution-error", "parse-error", "success-or-error", "unknown"}
ROW_ORDERS = {"exact", "bag", "sorted"}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def validate_case(case, location):
    require(isinstance(case, dict), f"{location}: case must be an object")
    required = {"schema_version", "id", "source", "mode", "sql", "expectation", "context", "curriculum"}
    require(required <= case.keys(), f"{location}: missing keys {sorted(required - case.keys())}")
    require(case["schema_version"] == 1, f"{location}: unsupported schema version")
    require(isinstance(case["id"], str) and case["id"], f"{location}: invalid id")
    require(case["mode"] == "syntax", f"{location}: unsupported mode")
    require(isinstance(case["sql"], str) and case["sql"].strip(), f"{location}: empty SQL")

    source = case["source"]
    source_keys = {"corpus", "repository", "revision", "path", "line", "command", "license"}
    require(isinstance(source, dict) and source_keys <= source.keys(), f"{location}: invalid source")
    require(isinstance(source["line"], int) and source["line"] >= 1, f"{location}: invalid source line")
    for key in source_keys - {"line"}:
        require(isinstance(source[key], str) and source[key], f"{location}: invalid source.{key}")

    expectation = case["expectation"]
    require(expectation.get("syntax") in SYNTAX_EXPECTATIONS, f"{location}: invalid syntax expectation")
    require(expectation.get("source_outcome") in SOURCE_OUTCOMES, f"{location}: invalid source outcome")
    require(
        isinstance(expectation.get("mysql_errors"), list)
        and all(isinstance(error, str) for error in expectation["mysql_errors"]),
        f"{location}: invalid mysql_errors",
    )

    context = case["context"]
    require(context.get("row_order") in ROW_ORDERS, f"{location}: invalid row order")
    require(isinstance(context.get("connection"), str), f"{location}: invalid connection")
    require(isinstance(context.get("conditional"), bool), f"{location}: invalid conditional flag")
    require(isinstance(context.get("dynamic"), bool), f"{location}: invalid dynamic flag")

    curriculum = case["curriculum"]
    require(curriculum.get("level") in LEVELS, f"{location}: invalid curriculum level")
    require(
        isinstance(curriculum.get("features"), list)
        and curriculum["features"]
        and all(isinstance(feature, str) and feature for feature in curriculum["features"]),
        f"{location}: invalid features",
    )
    require(
        isinstance(curriculum.get("complexity"), int) and curriculum["complexity"] >= 1,
        f"{location}: invalid complexity",
    )


def collect_shards(path):
    if path.is_file():
        return [path] if path.suffix == ".jsonl" else []
    return sorted(path.rglob("*.jsonl"))


def validate_path(path, all_ids):
    shards = collect_shards(path)
    require(shards, f"{path}: no JSONL shards found")
    count = 0
    expectations = collections.Counter()
    levels = collections.Counter()
    for shard in shards:
        with shard.open(encoding="utf-8") as handle:
            for line_number, line in enumerate(handle, 1):
                if not line.strip():
                    continue
                location = f"{shard}:{line_number}"
                try:
                    case = json.loads(line)
                except json.JSONDecodeError as error:
                    raise ValueError(f"{location}: invalid JSON: {error}") from error
                validate_case(case, location)
                require(case["id"] not in all_ids, f"{location}: duplicate id {case['id']}")
                all_ids.add(case["id"])
                count += 1
                expectations[case["expectation"]["syntax"]] += 1
                levels[case["curriculum"]["level"]] += 1

    manifest_path = path / "manifest.json" if path.is_dir() else None
    if manifest_path and manifest_path.exists():
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        selected = manifest.get("stats", {}).get("selected")
        require(selected == count, f"{manifest_path}: selected={selected}, shards contain {count}")
    print(f"{path}: {count} valid cases; expectations={dict(sorted(expectations.items()))}")
    print(f"{path}: levels={dict(sorted(levels.items()))}")
    return count


def main():
    parser = argparse.ArgumentParser(description="Validate mined MySQL corpus JSONL shards")
    parser.add_argument("paths", nargs="+", type=pathlib.Path)
    args = parser.parse_args()
    all_ids = set()
    try:
        total = sum(validate_path(path, all_ids) for path in args.paths)
    except (OSError, ValueError) as error:
        print(f"corpus validation failed: {error}", file=sys.stderr)
        return 1
    print(f"total: {total} valid cases with unique IDs")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
