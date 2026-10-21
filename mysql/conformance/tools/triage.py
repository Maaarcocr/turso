#!/usr/bin/env python3
"""Bucket failing MySQL specs by the feature that blocks them.

Reads a replay report (the merged `sqltest --output json` that
`make run-specs` writes) and the spec files it refers to, and assigns every
failing spec exactly one primary blocker:

- `function: NAME`      a MySQL function Turso does not implement
- `syntax: CONSTRUCT`   a MySQL construct the frontend does not lower and the
                        SQLite fallback cannot parse (DIV, _charset'..', ...)
- `ddl: CONSTRUCT`      the same for CREATE/ALTER TABLE details (ENGINE=, ...)
- `statement: KIND`     a statement kind that is not supported (SHOW, SET, ...)
- `wrong result: KIND`  the statement runs but returns different values
- `missing error: N`    MySQL raises error N, Turso succeeds
- `engine: MESSAGE`     any other Turso error, normalized

A spec can need several features; the primary blocker is the first one that
stops it. Fixing a blocker can reveal the next one, so counts are an upper
bound on what a fix unlocks, but they rank the work well.

Usage:
    tools/triage.py REPORT.json [--markdown OUT.md] [--json OUT.json]
"""

import argparse
import collections
import json
import math
import pathlib
import re
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import spec_lib  # noqa: E402

# Ordered detectors for MySQL constructs the SQLite fallback cannot parse or
# treats differently. The first match on the failing statement names the
# blocker, so put specific constructs before general ones.
_S = r"(?:'(?:[^'\\]|\\.|'')*')"  # a MySQL single-quoted string
CONSTRUCTS = [
    # DDL details
    ("ddl", "generated column", r"\b(GENERATED\s+ALWAYS\s+)?AS\s*\([^;]{0,400}?\)\s*(STORED|VIRTUAL)\b"),
    ("ddl", "ENGINE= table option", r"\bENGINE\s*=?\s*\w+"),
    ("ddl", "AUTO_INCREMENT", r"\bAUTO_INCREMENT\b"),
    ("ddl", "UNSIGNED/ZEROFILL", r"\b(UNSIGNED|ZEROFILL)\b"),
    ("ddl", "CHARACTER SET/CHARSET", r"\b(CHARACTER\s+SET|CHARSET)\b"),
    ("ddl", "ENUM/SET column type", r"\b(ENUM|SET)\s*\("),
    ("ddl", "ON UPDATE CURRENT_TIMESTAMP", r"\bON\s+UPDATE\s+CURRENT_TIMESTAMP"),
    ("ddl", "column COMMENT", r"\bCOMMENT\s+'"),
    ("ddl", "inline KEY/INDEX in CREATE TABLE", r"^\s*CREATE\s+(TEMPORARY\s+)?TABLE\b.*[(,]\s*(UNIQUE\s+|FULLTEXT\s+|SPATIAL\s+)?(KEY|INDEX)\b"),
    ("ddl", "FULLTEXT/SPATIAL index", r"\b(FULLTEXT|SPATIAL)\b"),
    ("ddl", "PARTITION BY", r"\bPARTITION\s+BY\b"),
    ("ddl", "index prefix length", r"\b(KEY|INDEX)\b[^;]*\(\s*`?\w+`?\s*\(\d+\)"),
    ("ddl", "ALTER TABLE MODIFY/CHANGE/ALTER COLUMN", r"^\s*ALTER\s+TABLE\b.*\b(MODIFY|CHANGE|ALTER)\s+(COLUMN\s+)?`?\w"),
    ("ddl", "ALTER TABLE ADD/DROP INDEX/KEY/CONSTRAINT", r"^\s*ALTER\s+TABLE\b.*\b(ADD|DROP)\s+(UNIQUE\s+)?(INDEX|KEY|CONSTRAINT|PRIMARY|FOREIGN)\b"),
    ("ddl", "column position FIRST/AFTER", r"^\s*ALTER\s+TABLE\b.*\b(FIRST|AFTER\s+`?\w+`?)\s*(,|;|$)"),
    ("ddl", "integer display width", r"\b(TINYINT|SMALLINT|MEDIUMINT|INT|INTEGER|BIGINT)\s*\(\d+\)"),
    ("ddl", "CREATE DATABASE/SCHEMA", r"^\s*(CREATE|DROP)\s+(DATABASE|SCHEMA)\b"),
    ("ddl", "CREATE PROCEDURE/FUNCTION/TRIGGER/EVENT", r"^\s*CREATE\s+(DEFINER\s*=\s*\S+\s+)?(PROCEDURE|FUNCTION|TRIGGER|EVENT)\b"),
    # statements
    ("statement", "SHOW", r"^\s*SHOW\b"),
    ("statement", "DESCRIBE/DESC/EXPLAIN", r"^\s*(DESCRIBE|DESC|EXPLAIN)\b"),
    ("statement", "SET variable", r"^\s*SET\b"),
    ("statement", "USE database", r"^\s*USE\b"),
    ("statement", "LOCK/UNLOCK TABLES", r"^\s*(LOCK|UNLOCK)\s+TABLES?\b"),
    ("statement", "PREPARE/EXECUTE/DEALLOCATE", r"^\s*(PREPARE|EXECUTE|DEALLOCATE)\b"),
    ("statement", "CALL", r"^\s*CALL\b"),
    ("statement", "TABLE t", r"^\s*TABLE\s+\w"),
    ("statement", "VALUES ROW(...)", r"\bVALUES\s+ROW\s*\("),
    ("statement", "INSERT IGNORE", r"^\s*INSERT\s+(LOW_PRIORITY\s+|DELAYED\s+|HIGH_PRIORITY\s+)?IGNORE\b"),
    ("statement", "ON DUPLICATE KEY UPDATE", r"\bON\s+DUPLICATE\s+KEY\s+UPDATE\b"),
    ("statement", "INSERT ... SET", r"^\s*INSERT\s+(INTO\s+)?`?\w+`?\s+SET\b"),
    ("statement", "multi-table UPDATE/DELETE", r"^\s*(UPDATE\s+\w+\s*(,|\bJOIN\b)|DELETE\s+\w+(\s*,\s*\w+)*\s+FROM\b)"),
    ("statement", "UPDATE/DELETE ... ORDER BY/LIMIT", r"^\s*(UPDATE|DELETE)\b.*\b(ORDER\s+BY|LIMIT)\b"),
    ("statement", "SELECT ... INTO", r"^\s*SELECT\b.*\bINTO\s+(@|OUTFILE|DUMPFILE)"),
    ("statement", "FOR UPDATE/SHARE locking read", r"\b(FOR\s+(UPDATE|SHARE)|LOCK\s+IN\s+SHARE\s+MODE)\b"),
    # literals
    ("syntax", "charset introducer (_utf8mb4'..', _binary ..)", r"(?<![\w$])_(binary|utf8mb4|utf8mb3|utf8|latin1|ascii|utf16|utf32|ucs2|gbk|big5|cp1251|euckr|sjis|ujis|koi8r|hebrew|tis620)\b"),
    ("syntax", "hex literal X'..'/0x..", r"(?i)(\bX'[0-9a-f]*'|\b0x[0-9a-f]+\b)"),
    ("syntax", "bit literal b'..'/0b..", r"(?i)(\bB'[01]*'|\b0b[01]+\b)"),
    ("syntax", "N'..' national string", r"(?<![\w$])N'"),
    ("syntax", "backslash escape in string", r"'(?:[^'\\]|'')*\\."),
    ("syntax", "typed literal DATE/TIME/TIMESTAMP '..'", r"\b(DATE|TIME|TIMESTAMP)\s*'"),
    ("syntax", "user variable @x / :=", r"(?<![\w@])@(?!@)\w|:="),
    ("syntax", "system variable @@x", r"@@\w"),
    # operators
    ("syntax", "DIV operator", r"\bDIV\b"),
    ("syntax", "MOD operator", r"\bMOD\b(?!\s*\()"),
    ("syntax", "XOR operator", r"\bXOR\b"),
    ("syntax", "&& operator", r"&&"),
    ("syntax", "! operator", r"!(?!=)"),
    ("syntax", "^ operator", r"\^"),
    ("syntax", "<=> operator", r"<=>"),
    ("syntax", "RLIKE/REGEXP operator", r"\b(RLIKE|REGEXP)\b"),
    ("syntax", "SOUNDS LIKE", r"\bSOUNDS\s+LIKE\b"),
    ("syntax", "MEMBER OF", r"\bMEMBER\s+OF\b"),
    ("syntax", "MATCH ... AGAINST", r"\bMATCH\s*\([^;]{0,200}?\)\s*AGAINST\b"),
    ("syntax", "INTERVAL expression", r"\bINTERVAL\b"),
    ("syntax", "BINARY operator", r"\bBINARY\s+(?!\()[\w'@(]"),
    ("syntax", "CONVERT(.. USING ..)/CONVERT(x, type)", r"\bCONVERT\s*\("),
    ("syntax", "CAST(.. AS type) MySQL target", r"\bCAST\s*\(.*\bAS\s+(SIGNED|UNSIGNED|CHAR|BINARY|JSON|YEAR|DATETIME|DATE|TIME|DECIMAL|DOUBLE|FLOAT|NCHAR)\b"),
    ("syntax", "COLLATE", r"\bCOLLATE\b"),
    ("syntax", "JSON ->/->> operator", r"->"),
    ("syntax", "GROUP BY ... WITH ROLLUP", r"\bWITH\s+ROLLUP\b"),
    ("syntax", "GROUP_CONCAT(.. SEPARATOR/ORDER BY)", r"\bGROUP_CONCAT\s*\(.*\b(SEPARATOR|ORDER\s+BY)\b"),
    ("syntax", "index hints USE/FORCE/IGNORE INDEX", r"\b(USE|FORCE|IGNORE)\s+(INDEX|KEY)\b"),
    ("syntax", "STRAIGHT_JOIN", r"\bSTRAIGHT_JOIN\b"),
    ("syntax", "SELECT modifiers (SQL_*, HIGH_PRIORITY, DISTINCTROW)", r"\b(SQL_\w+|HIGH_PRIORITY|DISTINCTROW)\b"),
    ("syntax", "LATERAL / JSON_TABLE", r"\b(LATERAL|JSON_TABLE)\b"),
    ("syntax", "optimizer hint comment", r"/\*\+"),
    ("syntax", "double-quoted string", r'"[^"]*"'),
    ("syntax", "|| as OR", r"\|\|"),
]
CONSTRUCTS = [(kind, name, re.compile(pattern, re.I | re.S)) for kind, name, pattern in CONSTRUCTS]

_STRIP_STRINGS = re.compile(r"'(?:[^'\\]|\\.|'')*'")


def detect_construct(statement):
    """Return (kind, name) of the first MySQL construct found, or None."""
    for kind, name, pattern in CONSTRUCTS:
        # Match operators and keywords outside string literals, but literal
        # forms need the strings.
        text = statement if kind != "syntax" or name in _NEEDS_STRINGS else _STRIP_STRINGS.sub("''", statement)
        if pattern.search(text):
            return kind, name
    return None


_NEEDS_STRINGS = {
    "charset introducer (_utf8mb4'..', _binary ..)",
    "hex literal X'..'/0x..",
    "bit literal b'..'/0b..",
    "N'..' national string",
    "backslash escape in string",
    "typed literal DATE/TIME/TIMESTAMP '..'",
    "double-quoted string",
}

_NUMBER = re.compile(r"^[-+]?(\d+\.?\d*|\.\d+)([eE][-+]?\d+)?$")


def _num(value):
    if _NUMBER.match(value):
        try:
            return float(value)
        except ValueError:
            return None
    return None


def _diff_lines(reason):
    expected, actual = [], []
    for line in reason.split("\n"):
        if line.startswith("--- ") or line.startswith("+++ "):
            continue
        if line.startswith("  - "):
            expected.append(line[4:])
        elif line.startswith("  + "):
            actual.append(line[4:])
        elif line.startswith("-"):
            expected.append(line[1:])
        elif line.startswith("+"):
            actual.append(line[1:])
    return expected, actual


def classify_values(expected, actual, sql):
    """Name the kind of a wrong result from the first differing values."""
    if not expected or not actual:
        return "row count"
    if len(expected) != len(actual):
        return "row count"
    for e_row, a_row in zip(expected, actual):
        e_cells, a_cells = e_row.split("|"), a_row.split("|")
        if len(e_cells) != len(a_cells):
            return "column count"
        for e, a in zip(e_cells, a_cells):
            if e == a:
                continue
            return classify_cell(e, a, sql)
    return "row order or duplicates"


def classify_cell(e, a, sql):
    if e == "NULL" or a == "NULL":
        return "NULL vs value"
    en, an = _num(e), _num(a)
    if en is not None and an is not None:
        if en == an:
            if "e" in e.lower() or "e" in a.lower():
                return "DOUBLE formatting"
            if "." in e or "." in a:
                return "DECIMAL scale/format (e.g. 0.5000)"
            return "number formatting"
        if "." in e and "." not in a and "/" in sql:
            return "division returns DECIMAL"
        if an != 0 and abs(en - an) <= 1e-6 * max(abs(en), abs(an)):
            return "floating-point precision"
        if math.trunc(en) == an or math.floor(en) == an or math.ceil(en) == an:
            return "integer vs fractional result"
        return "numeric value"
    if en is not None or an is not None:
        return "string vs number"
    if e.lower() == a.lower():
        return "case (collation/upper/lower)"
    for text in (e, a):
        if text[:1] in "[{":
            try:
                ej, aj = json.loads(e), json.loads(a)
            except ValueError:
                break
            return "JSON formatting" if ej == aj else "JSON value"
    if re.match(r"^\d{4}-\d{2}-\d{2}|^-?\d+:\d{2}:\d{2}", e):
        return "temporal value"
    if e.strip() == a.strip():
        return "whitespace"
    return "string value"


_MSG_NUM = re.compile(r"\b\d+\b")
_MSG_QUOTED = re.compile(r"'[^']*'|\"[^\"]*\"|`[^`]*`")


def normalize_message(message):
    return _MSG_NUM.sub("N", _MSG_QUOTED.sub("'…'", message)).strip()[:90]


def mysql_error(test):
    for comment in test["comments"]:
        if comment.startswith("mysql-error:"):
            parts = comment[len("mysql-error:"):].strip().split(" ", 2)
            errno = parts[0] if parts else "?"
            message = parts[2] if len(parts) > 2 else ""
            return errno, message
    return None


_FUNC_MISSING = re.compile(r"no such function: (\w+)", re.I)
_SETUP_FAILED = re.compile(r"^setup '([^']+)' failed: (?:failed to execute SQL: )?(.*)$", re.S)
_EXPECTED_SUCCESS = re.compile(r"^expected success but got error: (.*)$", re.S)
_NEAR = re.compile(r'near "(.*?)": syntax error', re.S)


def failing_statement(statements, message):
    """Pick the statement in a setup block that most likely failed."""
    near = _NEAR.search(message)
    if near and near.group(1):
        for statement in statements:
            if near.group(1) in statement:
                return statement
    for statement in statements[:200]:
        if detect_construct(statement[:2000]):
            return statement
    return statements[0] if statements else ""


def classify_error(message, statement):
    message = message.strip()
    first = message.split("\n")[0]
    m = _FUNC_MISSING.search(first)
    if m:
        name = m.group(1).upper()
        # Typed literals and introducers surface as missing columns, not
        # functions, so only genuine calls get here.
        return f"function: {name}"
    m = re.search(r"unknown table option: (\w+)", first, re.I)
    if m:
        return f"ddl: table option {m.group(1).upper()}="
    m = re.search(r"no such collation sequence: (\w+)", first)
    if m:
        return f"collation: {m.group(1)}"
    if "Generated columns require" in first:
        return "ddl: generated column"
    if re.search(r"bad JSON path", first):
        return "json: MySQL JSON path syntax ($**, [*], ...)"
    m = re.search(r"(\w+) function called with not exactly (\d+) arguments", first)
    if m:
        return f"function: {m.group(1).upper()} with another arity"
    construct = detect_construct(statement[:4000]) if statement else None
    syntaxish = (
        "syntax error" in first
        or "unrecognized token" in first
        or "bad number" in first
        or "expected = sign" in first
        or "Array features" in first
        or "no such column" in first
    )
    if construct and syntaxish:
        kind, name = construct
        return f"{kind}: {name}"
    if construct and re.search(r"no such table|already exists", first) is None:
        kind, name = construct
        return f"{kind}: {name}"
    near = _NEAR.search(first)
    if near:
        token = near.group(1)
        if re.fullmatch(r"[A-Za-z_]+", token):
            return f"syntax: near keyword {token.upper()}"
        return "syntax: other"
    return "engine: " + normalize_message(first)


class SpecIndex:
    def __init__(self, root):
        self.root = root
        self.cache = {}

    def lookup(self, path, name):
        if path not in self.cache:
            file = pathlib.Path(path)
            if not file.is_absolute():
                file = self.root / file
            try:
                data = spec_lib.read_sqltest(file.read_text())
            except (OSError, ValueError):
                data = {"setups": {}, "tests": []}
            self.cache[path] = (data["setups"], {t["name"]: t for t in data["tests"]})
        setups, tests = self.cache[path]
        return setups, tests.get(name)


def source_of(path):
    parts = pathlib.PurePath(path).parts
    if "specs" in parts:
        i = parts.index("specs")
        rest = parts[i + 1:]
        if rest and rest[0] == "local" and len(rest) > 1:
            return rest[1]
        return rest[0] if rest else "?"
    return parts[0] if parts else "?"


def triage(report, root):
    index = SpecIndex(root)
    buckets = collections.defaultdict(list)
    totals = collections.Counter()
    passing = collections.Counter()
    for file in report["files"]:
        path = file["path"]
        source = source_of(path)
        for result in file["results"]:
            totals[source] += 1
            if result["outcome"] == "passed":
                passing[source] += 1
                continue
            reason = result.get("reason", "") or ""
            setups, test = index.lookup(path, result["name"])
            sql = test["sql"] if test else ""
            bucket = classify_failure(result, reason, setups, test, sql)
            buckets[bucket].append((source, path, result["name"], sql, reason))
    return buckets, totals, passing


_SETUP_CACHE = {}


def classify_failure(result, reason, setups, test, sql):
    first = reason.split("\n")[0]
    if "memory allocation" in reason:
        return "engine: unbounded allocation (REPEAT/LPAD/RPAD length)"
    if "timed out" in first or "timeout" in first.lower():
        return "engine: timeout"
    if "crash" in first.lower() or "panicked" in reason:
        return "engine: crash"
    m = _SETUP_FAILED.match(first)
    if m:
        name, message = m.group(1), m.group(2)
        key = (id(setups), name, message)
        if key not in _SETUP_CACHE:
            statements = spec_lib.split_block_statements(setups.get(name, ""))
            statement = failing_statement(statements, message)
            _SETUP_CACHE[key] = classify_error(message, statement)
        return _SETUP_CACHE[key]
    m = _EXPECTED_SUCCESS.match(first)
    if m:
        message = m.group(1)
        statements = spec_lib.split_block_statements(sql)
        statement = failing_statement(statements, message) if len(statements) > 1 else sql
        return classify_error(message, statement)
    if first.startswith("expected error but query succeeded"):
        error = mysql_error(test) if test else None
        if error:
            errno, message = error
            return f"missing error: {errno} {normalize_message(message)}"
        return "missing error: ?"
    if first.startswith("--- expected") or first.startswith("Missing rows") or first.startswith("Extra rows"):
        expected, actual = _diff_lines(reason)
        return "wrong result: " + classify_values(expected, actual, sql)
    if first.startswith("expected error"):
        return "wrong error"
    return "engine: " + normalize_message(first)


def family(path, sql):
    """A finer label for wrong results: the Vitess function family, or the
    first function the statement calls."""
    stem = pathlib.PurePath(path).stem
    if "/vitess/evalengine/" in path:
        return re.sub(r"-\d+$", "", stem)
    text = _STRIP_STRINGS.sub("''", sql).upper()
    for m in _CALL.finditer(text):
        if m.group(1) not in _NOT_FUNCTIONS:
            return m.group(1)
    return "(no function)"


_CALL = re.compile(r"\b([A-Z_][A-Z0-9_]*)\s*\(")
_NOT_FUNCTIONS = {
    "SELECT", "IN", "VALUES", "EXISTS", "AS", "FROM", "WHERE", "AND", "OR",
    "NOT", "ON", "INTO", "TABLE", "USING", "JOIN", "BY", "OVER", "KEY", "INDEX",
}


def render_markdown(buckets, totals, passing, top):
    lines = []
    total = sum(totals.values())
    passed = sum(passing.values())
    failed = total - passed
    lines.append("# MySQL spec failures by blocking feature")
    lines.append("")
    lines.append("Generated by `mysql/conformance/tools/triage.py`; do not edit.")
    lines.append("")
    lines.append(f"{passed:,} of {total:,} specs pass; {failed:,} fail. Every failing spec is")
    lines.append("counted once, under the first feature that blocks it. Fixing a blocker may")
    lines.append("reveal the next one, so a count is an upper bound on what a fix unlocks.")
    lines.append("")
    lines.append("| source | passing | total |")
    lines.append("|---|---:|---:|")
    for source in sorted(totals):
        lines.append(f"| {source} | {passing[source]:,} | {totals[source]:,} |")
    lines.append("")

    by_category = collections.Counter()
    for bucket, items in buckets.items():
        by_category[bucket.split(":")[0]] += len(items)
    lines.append("## By category")
    lines.append("")
    lines.append("| category | failing specs |")
    lines.append("|---|---:|")
    for category, count in by_category.most_common():
        lines.append(f"| {category} | {count:,} |")
    lines.append("")

    lines.append(f"## Top {top} blockers")
    lines.append("")
    lines.append("| # | blocker | specs | sources | example |")
    lines.append("|---:|---|---:|---|---|")
    ranked = sorted(buckets.items(), key=lambda kv: (-len(kv[1]), kv[0]))
    for rank, (bucket, items) in enumerate(ranked[:top], 1):
        sources = collections.Counter(item[0] for item in items)
        source_text = ", ".join(f"{s} {n:,}" for s, n in sources.most_common(3))
        example = items[0][3].replace("\n", " ").replace("|", "\\|")
        if len(example) > 90:
            example = example[:87] + "..."
        lines.append(f"| {rank} | {bucket} | {len(items):,} | {source_text} | `{example}` |")
    lines.append("")

    for title, prefix in (("Missing functions", "function: "), ("Wrong results", "wrong result: "), ("Missing MySQL errors", "missing error: ")):
        group = [(b, items) for b, items in ranked if b.startswith(prefix)]
        if not group:
            continue
        count = sum(len(items) for _, items in group)
        lines.append(f"## {title} ({count:,} specs)")
        lines.append("")
        if prefix == "wrong result: ":
            lines.append("| kind | specs | top families (Vitess function family or first function) |")
            lines.append("|---|---:|---|")
            for bucket, items in group:
                families = collections.Counter(family(item[1], item[3]) for item in items)
                text = ", ".join(f"{f} {n:,}" for f, n in families.most_common(6))
                lines.append(f"| {bucket[len(prefix):]} | {len(items):,} | {text} |")
        else:
            names = [f"{b[len(prefix):]} ({len(items):,})" for b, items in group]
            lines.append(", ".join(names[:150]) + ("" if len(names) <= 150 else f", … {len(names) - 150} more"))
        lines.append("")
    return "\n".join(lines) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("report", type=pathlib.Path, help="merged sqltest JSON report")
    parser.add_argument("--root", type=pathlib.Path, default=pathlib.Path(__file__).resolve().parent.parent,
                        help="directory spec paths in the report are relative to (default: mysql/conformance)")
    parser.add_argument("--markdown", type=pathlib.Path, help="write the summary here")
    parser.add_argument("--json", type=pathlib.Path, help="write {blocker: [spec ids]} here")
    parser.add_argument("--top", type=int, default=60)
    args = parser.parse_args(argv)

    report = json.loads(args.report.read_text())
    buckets, totals, passing = triage(report, args.root)
    markdown = render_markdown(buckets, totals, passing, args.top)
    if args.markdown:
        args.markdown.write_text(markdown)
    else:
        sys.stdout.write(markdown)
    if args.json:
        data = {bucket: sorted(f"{item[1]}::{item[2]}" for item in items)
                for bucket, items in sorted(buckets.items(), key=lambda kv: (-len(kv[1]), kv[0]))}
        args.json.write_text(json.dumps(data, indent=0, sort_keys=False) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
