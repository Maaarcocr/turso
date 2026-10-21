#!/usr/bin/env python3
import argparse
import collections
import dataclasses
import hashlib
import json
import pathlib
import re
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import go_source  # noqa: E402

SCHEMA_VERSION = 1
REPOSITORY = "https://github.com/dolthub/go-mysql-server.git"
LICENSE = "Apache-2.0"
QUERY_FIELDS = {"Query": "query", "WriteQuery": "write_query", "SelectQuery": "select_query"}


@dataclasses.dataclass
class Token:
    kind: str
    value: str
    start: int
    end: int
    line: int
    parent: int | None = None


def tokenize_go(source):
    tokens = []
    index = 0
    line = 1
    length = len(source)
    while index < length:
        character = source[index]
        if character.isspace():
            if character == "\n":
                line += 1
            index += 1
            continue
        if source.startswith("//", index):
            end = source.find("\n", index + 2)
            index = length if end < 0 else end
            continue
        if source.startswith("/*", index):
            end = source.find("*/", index + 2)
            end = length - 2 if end < 0 else end
            line += source[index : end + 2].count("\n")
            index = end + 2
            continue
        if character == "`":
            start = index
            start_line = line
            end = source.find("`", index + 1)
            if end < 0:
                raise ValueError(f"unterminated raw string at line {line}")
            literal = source[index : end + 1]
            line += literal.count("\n")
            index = end + 1
            tokens.append(Token("string", decode_literal(literal), start, index, start_line))
            continue
        if character == '"':
            start = index
            start_line = line
            index += 1
            while index < length:
                if source[index] == "\\":
                    index += 2
                elif source[index] == '"':
                    index += 1
                    break
                else:
                    if source[index] == "\n":
                        line += 1
                    index += 1
            else:
                raise ValueError(f"unterminated interpreted string at line {start_line}")
            literal = source[start:index]
            try:
                value = decode_literal(literal)
            except ValueError as error:
                raise ValueError(f"unsupported Go string at line {start_line}: {error}") from error
            tokens.append(Token("string", value, start, index, start_line))
            continue
        if character == "'":
            start_line = line
            index += 1
            while index < length:
                if source[index] == "\\":
                    index += 2
                elif source[index] == "'":
                    index += 1
                    break
                else:
                    index += 1
            else:
                raise ValueError(f"unterminated rune at line {start_line}")
            continue
        if character.isalpha() or character == "_":
            start = index
            while index < length and (source[index].isalnum() or source[index] == "_"):
                index += 1
            tokens.append(Token("ident", source[start:index], start, index, line))
            continue
        if character.isdigit():
            start = index
            while index < length and (source[index].isalnum() or source[index] in "._"):
                index += 1
            tokens.append(Token("number", source[start:index], start, index, line))
            continue
        tokens.append(Token("punct", character, index, index + 1, line))
        index += 1

    stack = []
    pairs = {}
    for token_index, token in enumerate(tokens):
        token.parent = stack[-1] if stack else None
        if token.kind != "punct":
            continue
        if token.value == "{":
            stack.append(token_index)
        elif token.value == "}":
            if not stack:
                raise ValueError(f"unmatched closing brace at line {token.line}")
            opening = stack.pop()
            pairs[opening] = token_index
    if stack:
        raise ValueError(f"unmatched opening brace at line {tokens[stack[-1]].line}")
    return tokens, pairs


def decode_literal(literal):
    """Decode a Go string literal; ``None`` when its bytes are not valid UTF-8.

    Go strings are byte sequences, so ``\\xNN`` and octal escapes are bytes that
    must be decoded together as UTF-8 (``"\\xef\\xbf\\xbd"`` is U+FFFD).
    """
    try:
        return go_source.decode_string_bytes(literal).decode("utf-8")
    except UnicodeDecodeError:
        return None


def parse_string_expression(tokens, start, parent):
    if start >= len(tokens) or tokens[start].kind != "string":
        return None
    pieces = [tokens[start].value]
    line = tokens[start].line
    index = start + 1
    while index + 1 < len(tokens) and tokens[index].value == "+":
        if tokens[index].parent != parent or tokens[index + 1].kind != "string":
            return None
        pieces.append(tokens[index + 1].value)
        index += 2
    if index < len(tokens) and tokens[index].parent == parent and tokens[index].value not in {",", "}"}:
        return None
    if any(piece is None for piece in pieces):
        return None, line, index
    return "".join(pieces), line, index


def direct_field(tokens, pairs, parent, name, source):
    if parent is None or parent not in pairs:
        return None
    index = parent + 1
    end = pairs[parent]
    while index + 1 < end:
        token = tokens[index]
        if token.parent == parent and token.kind == "ident" and token.value == name and tokens[index + 1].value == ":":
            value_start = index + 2
            value_end = value_start
            while value_end < end:
                candidate = tokens[value_end]
                if candidate.parent == parent and candidate.value in {",", "}"}:
                    break
                value_end += 1
            if value_start == value_end:
                return ""
            return source[tokens[value_start].start : tokens[value_end - 1].end].strip()
        index += 1
    return None


def direct_string_field(tokens, pairs, parent, name):
    if parent is None or parent not in pairs:
        return None
    index = parent + 1
    end = pairs[parent]
    while index + 2 < end:
        if (
            tokens[index].parent == parent
            and tokens[index].kind == "ident"
            and tokens[index].value == name
            and tokens[index + 1].value == ":"
        ):
            parsed = parse_string_expression(tokens, index + 2, parent)
            return parsed[0] if parsed else None
        index += 1
    return None


def extract_file(path, root, revision):
    source = path.read_text(encoding="utf-8")
    tokens, pairs = tokenize_go(source)
    cases = []
    stats = collections.Counter()

    for index, token in enumerate(tokens):
        if token.kind != "ident" or index + 2 >= len(tokens) or tokens[index + 1].value != ":":
            continue
        if token.value in QUERY_FIELDS:
            parsed = parse_string_expression(tokens, index + 2, token.parent)
            if not parsed:
                stats["dynamic_query"] += 1
                continue
            sql, line, _ = parsed
            if sql is None:
                stats["non_utf8_query"] += 1
                continue
            if not sql.strip():
                stats["empty"] += 1
                continue
            skip = direct_field(tokens, pairs, token.parent, "Skip", source)
            if skip == "true":
                stats["skipped_by_source"] += 1
                continue
            expected_error = direct_field(tokens, pairs, token.parent, "ExpectedErr", source)
            expected_error_text = direct_string_field(tokens, pairs, token.parent, "ExpectedErrStr")
            source_error = (expected_error not in {None, "", "nil"}) or bool(expected_error_text)
            cases.append(
                make_case(
                    sql=sql,
                    path=path.relative_to(root).as_posix(),
                    line=line,
                    command=QUERY_FIELDS[token.value],
                    revision=revision,
                    source_error=source_error,
                )
            )
            stats["query"] += 1
        elif token.value == "SetUpScript":
            start = index + 2
            opening = next(
                (
                    candidate
                    for candidate in range(start, min(len(tokens), start + 8))
                    if tokens[candidate].value == "{"
                ),
                None,
            )
            if opening is None or opening not in pairs:
                stats["dynamic_setup"] += 1
                continue
            end = pairs[opening]
            cursor = opening + 1
            while cursor < end:
                if tokens[cursor].parent == opening and tokens[cursor].kind == "string":
                    parsed = parse_string_expression(tokens, cursor, opening)
                    if parsed:
                        sql, line, cursor = parsed
                        if sql is None:
                            stats["non_utf8_setup"] += 1
                        elif sql.strip():
                            cases.append(
                                make_case(
                                    sql=sql,
                                    path=path.relative_to(root).as_posix(),
                                    line=line,
                                    command="setup",
                                    revision=revision,
                                    source_error=False,
                                )
                            )
                            stats["setup"] += 1
                        continue
                cursor += 1
    return cases, stats


def make_case(sql, path, line, command, revision, source_error):
    sql = sql.strip()
    level, features = classify(sql)
    syntax = "unknown" if source_error else "accept"
    fingerprint = hashlib.sha256(f"{path}\0{line}\0{command}\0{sql}".encode()).hexdigest()[:16]
    return {
        "schema_version": SCHEMA_VERSION,
        "id": f"go-mysql-server:{path.removesuffix('.go')}:L{line}:{fingerprint}",
        "source": {
            "corpus": "go-mysql-server-enginetest",
            "repository": REPOSITORY,
            "revision": revision,
            "path": path,
            "line": line,
            "command": command,
            "license": LICENSE,
        },
        "mode": "syntax",
        "sql": sql,
        "expectation": {
            "syntax": syntax,
            "source_outcome": "execution-error" if source_error else "success",
            "mysql_errors": [],
        },
        "context": {
            "connection": "default",
            "conditional": False,
            "dynamic": False,
            "row_order": "exact" if contains_marker(sql, "ORDER BY") else "bag",
        },
        "curriculum": {
            "level": level,
            "features": features,
            "complexity": 1 + len(features) + (len(sql) + 199) // 200,
        },
    }


def normalized_sql(sql):
    return " ".join(sql.upper().split())


def contains_marker(sql, marker):
    return f" {marker} " in f" {normalized_sql(sql)} "


def first_keyword(sql):
    value = sql.lstrip()
    while True:
        if value.startswith("--") or value.startswith("#"):
            value = value.partition("\n")[2].lstrip()
        elif value.startswith("/*") and "*/" in value:
            value = value.partition("*/")[2].lstrip()
        else:
            break
    match = re.match(r"\(?\s*([A-Za-z_]+)", value)
    return match.group(1).upper() if match else "UNKNOWN"


def classify(sql):
    first = first_keyword(sql)
    markers = {
        "JOIN": "join",
        "WHERE": "where",
        "GROUP BY": "group-by",
        "HAVING": "having",
        "ORDER BY": "order-by",
        "LIMIT": "limit",
        "UNION": "set-operation",
        "WINDOW": "window",
        "OVER": "window",
        "JSON": "json",
        "COLLATE": "collation",
        "CHARACTER SET": "charset",
        "AUTO_INCREMENT": "auto-increment",
        "GENERATED": "generated-column",
        "FOREIGN KEY": "foreign-key",
        "ON DUPLICATE KEY": "upsert",
        "PREPARE": "prepared-statement",
        "TRANSACTION": "transaction",
    }
    features = {f"statement:{first.lower()}"}
    for marker, feature in markers.items():
        if contains_marker(sql, marker):
            features.add(feature)
    if first == "SELECT" and not contains_marker(sql, "FROM"):
        level = "01-literal-select"
    elif first in {"CREATE", "ALTER", "DROP", "TRUNCATE", "RENAME"}:
        level = "02-schema"
    elif first in {"INSERT", "REPLACE", "LOAD"}:
        level = "03-insert"
    elif first == "SELECT" and contains_marker(sql, "JOIN"):
        level = "07-joins"
    elif first == "SELECT" and (contains_marker(sql, "GROUP BY") or contains_marker(sql, "OVER")):
        level = "08-aggregation-window"
    elif first == "SELECT":
        level = "04-query"
    elif first in {"UPDATE", "DELETE"}:
        level = "05-mutations"
    elif first in {"BEGIN", "START", "COMMIT", "ROLLBACK", "SAVEPOINT"}:
        level = "06-transactions"
    elif first in {"SET", "USE", "SHOW", "DESCRIBE", "DESC", "EXPLAIN"}:
        level = "09-session-metadata"
    elif first in {"GRANT", "REVOKE"}:
        level = "10-security"
    else:
        level = "11-advanced"
    return level, sorted(features)


def select_balanced(cases, limit):
    groups = collections.defaultdict(collections.deque)
    for case in cases:
        groups[case["curriculum"]["level"]].append(case)
    selected = []
    target = len(cases) if limit == 0 else min(limit, len(cases))
    levels = sorted(groups)
    while len(selected) < target:
        progressed = False
        for level in levels:
            if len(selected) == target:
                break
            if groups[level]:
                selected.append(groups[level].popleft())
                progressed = True
        if not progressed:
            break
    return selected


def write_corpus(output, cases, args, stats, available, duplicates):
    output.mkdir(parents=True, exist_ok=True)
    for old in output.glob("cases-*.jsonl"):
        old.unlink()
    for index in range(0, len(cases), args.shard_size):
        path = output / f"cases-{index // args.shard_size:05}.jsonl"
        with path.open("w", encoding="utf-8") as handle:
            for case in cases[index : index + args.shard_size]:
                handle.write(json.dumps(case, ensure_ascii=False, separators=(",", ":")) + "\n")

    level_counts = collections.Counter(case["curriculum"]["level"] for case in cases)
    expectation_counts = collections.Counter(case["expectation"]["syntax"] for case in cases)
    manifest = {
        "schema_version": SCHEMA_VERSION,
        "generator": pathlib.Path(__file__).name,
        "source_repository": REPOSITORY,
        "source_revision": args.revision,
        "source_root": "enginetest",
        "license": LICENSE,
        "selection": {
            "strategy": "round-robin-by-generated-level",
            "limit": args.limit,
            "shard_size": args.shard_size,
        },
        "stats": {
            **dict(sorted(stats.items())),
            "duplicates": duplicates,
            "available": available,
            "selected": len(cases),
            "by_level": dict(sorted(level_counts.items())),
            "by_syntax_expectation": dict(sorted(expectation_counts.items())),
        },
    }
    (output / "manifest.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )


def main():
    parser = argparse.ArgumentParser(description="Mine static SQL fixtures from go-mysql-server enginetest")
    parser.add_argument("--input", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--limit", type=int, default=5000)
    parser.add_argument("--shard-size", type=int, default=1000)
    args = parser.parse_args()
    if args.limit < 0:
        parser.error("--limit must be zero or greater")
    if args.shard_size <= 0:
        parser.error("--shard-size must be greater than zero")

    root = args.input.resolve()
    if not root.is_dir():
        parser.error(f"input directory does not exist: {root}")
    cases = []
    stats = collections.Counter()
    files = sorted(root.rglob("*.go"))
    for path in files:
        extracted, file_stats = extract_file(path, root, args.revision)
        cases.extend(extracted)
        stats.update(file_stats)
    stats["files"] = len(files)

    deduplicated = {}
    for case in cases:
        key = (case["sql"], case["expectation"]["syntax"], case["source"]["command"])
        deduplicated.setdefault(key, case)
    duplicates = len(cases) - len(deduplicated)
    unique = list(deduplicated.values())
    selected = select_balanced(unique, args.limit)
    write_corpus(args.output, selected, args, stats, len(unique), duplicates)
    print(
        f"extracted {len(selected)} cases from {len(files)} Go files; "
        f"available={len(unique)} duplicates={duplicates} output={args.output}"
    )


if __name__ == "__main__":
    main()
