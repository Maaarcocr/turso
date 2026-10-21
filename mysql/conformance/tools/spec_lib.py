"""Shared helpers for the recorded MySQL spec corpus.

This module is loaded by path (see ``load_spec_lib``) so the tools keep working
under ``python3 -I``, which does not put the script directory on ``sys.path``.

It contains:

* a faithful model of how ``testing/sqltest`` reads blocks and expectations,
  used to escape SQL and to prove that every recorded result round-trips;
* the candidate schema validator shared by importers and the recorder;
* the feature/level classifier that orders specs from simplest to hardest.
"""

import hashlib
import json
import re

# ---------------------------------------------------------------------------
# sqltest block model (testing/sqltest/src/parser/lexer.rs and mod.rs)
# ---------------------------------------------------------------------------

# Rust's `char::is_whitespace` (Unicode White_Space). `str.trim()` in the sqltest
# parser strips exactly these characters, which differ from Python's
# `str.isspace` (Python also treats U+001C..U+001F as whitespace).
RUST_WHITESPACE = frozenset(
    "\t\n\x0b\x0c\r \x85\xa0        "
    "        　"
)


def rust_trim(value):
    start = 0
    end = len(value)
    while start < end and value[start] in RUST_WHITESPACE:
        start += 1
    while end > start and value[end - 1] in RUST_WHITESPACE:
        end -= 1
    return value[start:end]


def rust_lines(value):
    """Rust `str::lines`: split on `\\n`, drop one trailing `\\r` per line."""
    if value == "":
        return []
    parts = value.split("\n")
    if parts[-1] == "":
        parts.pop()
    return [part[:-1] if part.endswith("\r") else part for part in parts]


def escape_block(text):
    """Escape text so the sqltest lexer's block extraction returns it unchanged.

    The lexer treats `\\{`, `\\}` and `\\\\` as escapes and counts unescaped braces.
    Escaping every brace keeps blocks balanced; a backslash only needs escaping
    when the character after it would otherwise form an escape sequence.
    """
    out = []
    for index, char in enumerate(text):
        if char in "{}":
            out.append("\\" + char)
        elif char == "\\":
            following = text[index + 1] if index + 1 < len(text) else ""
            out.append("\\\\" if following in ("\\", "{", "}") else "\\")
        else:
            out.append(char)
    return "".join(out)


class BlockError(ValueError):
    pass


_BLOCK_SPECIAL = re.compile(r"[\\{}]")


def extract_block(source, start):
    """Model of lexer.rs `extract_block_content`.

    `start` is the index just after the opening `{`. Returns the unescaped
    content and the index after the closing `}`. The lexer drops one newline
    right after `{` and one right before the closing `}`; `\\{`, `\\}` and
    `\\\\` are escapes, and unescaped braces nest.
    """
    depth = 1
    content = []
    index = start
    if source.startswith("\n", index):
        index += 1
    length = len(source)
    while True:
        match = _BLOCK_SPECIAL.search(source, index)
        if match is None:
            raise BlockError("unterminated block")
        position = match.start()
        content.append(source[index:position])
        char = source[position]
        if char == "\\":
            following = source[position + 1] if position + 1 < length else ""
            if following in ("{", "}", "\\"):
                content.append(following)
                index = position + 2
            else:
                content.append(char)
                index = position + 1
            continue
        if char == "{":
            depth += 1
        else:
            depth -= 1
            if depth == 0:
                text = "".join(content)
                if text.endswith("\n"):
                    text = text[:-1]
                return text, position + 1
        content.append(char)
        index = position + 1


def expand_macros_would_change(content):
    """sqltest expands `{{...}}` macros after unescaping; we never emit them."""
    return "{{" in content


def parse_exact_lines(content):
    return [rust_trim(line) for line in content.split("\n")]


def parse_unordered_lines(content):
    return [line for line in (rust_trim(x) for x in rust_lines(rust_trim(content))) if line != ""]


def expected_rows(lines):
    """Model of comparison/mod.rs `parse_expected_rows`."""
    return [[""] if line == "" else line.split("|") for line in lines]


def format_rows(rows):
    return "\n".join("|".join(row) for row in rows)


def render_block_lines(lines, indent="    "):
    """Render expectation lines as block content (one indented line each)."""
    return "".join(f"{indent}{escape_block(line)}\n" for line in lines)


def render_raw_block_lines(lines):
    """Render expectation lines for an `expect raw` block (no indentation)."""
    return "".join(f"{escape_block(line)}\n" for line in lines)


def expectation_round_trips(rows, ordered, raw=False):
    """Return None when `rows` survive a write/parse/compare cycle, else a reason.

    `rows` are rendered text values (NULL already rendered as ``NULL``). The
    check renders the expect block exactly like the recorder, re-reads it with
    the sqltest lexer/parser model, and applies the runner's comparator to the
    original MySQL rows. `raw` checks an `expect raw` block, which keeps
    whitespace but compares in order, so it suits ordered or single-row
    results.
    """
    for row in rows:
        for value in row:
            if "|" in value:
                return "value contains '|'"
            if "\n" in value or "\r" in value:
                return "value contains a line break"
            if any(ord(c) < 0x20 and c != "\t" or ord(c) == 0x7F for c in value):
                return "value contains a control character"
    lines = ["|".join(row) for row in rows]
    rendered = render_raw_block_lines(lines) if raw else render_block_lines(lines)
    try:
        content, _ = extract_block("\n" + rendered + "}", 0)
    except BlockError:
        return "expect block does not terminate"
    if expand_macros_would_change(content):
        return "expect block contains '{{'"
    if raw:
        if not (ordered or len(rows) <= 1):
            return "raw blocks compare in order"
        parsed = expected_rows(content.split("\n")) if rows else []
        if format_rows(parsed) != format_rows(rows):
            return "row text is altered by sqltest trimming"
    elif ordered:
        parsed = expected_rows(parse_exact_lines(content))
        if format_rows(parsed) != format_rows(rows):
            return "row text is altered by sqltest trimming"
    else:
        parsed = expected_rows(parse_unordered_lines(content))
        actual_set = {"|".join(row) for row in rows}
        expected_set = {"|".join(row) for row in parsed}
        if actual_set != expected_set:
            return "row text is altered by sqltest trimming"
    return None


def sql_block_text(statements, indent=""):
    """Render statements as a block body; each ends with `;` on its own terms.

    `indent` is inserted before every statement after the first (the block's
    first line is indented by the caller). Lines inside a statement are kept
    verbatim, so string literals spanning lines are unchanged.
    """
    out = []
    for statement in statements:
        last_line = statement.rsplit("\n", 1)[-1]
        if statement.endswith(";"):
            out.append(statement)
        elif "--" in last_line or "#" in last_line:
            # A trailing line comment would swallow the terminator.
            out.append(statement + "\n;")
        else:
            out.append(statement + ";")
    return ("\n" + indent).join(out)


_LEXEME = re.compile(r"['\"`#;]|--|/\*")


def mysql_statements(text):
    """Split MySQL text at top-level semicolons, like mysql/parser/split.rs.

    Semicolons inside '...', "..." (backslash and doubled-quote escapes),
    `...` identifiers and `#`, `-- ` and `/* */` comments do not split; every
    other semicolon ends a statement, as with the mysql client's default
    delimiter (compound-statement bodies are not supported). Returns the
    trimmed non-empty pieces without their semicolons.
    """
    pieces = []
    start = index = 0
    length = len(text)
    while True:
        match = _LEXEME.search(text, index)
        if match is None:
            break
        index = match.start()
        token = match.group(0)
        if token in ("'", '"', "`"):
            index += 1
            while index < length:
                char = text[index]
                if char == "\\" and token != "`":
                    index += 2
                    continue
                if char == token:
                    if index + 1 < length and text[index + 1] == token:
                        index += 2
                        continue
                    break
                index += 1
            index += 1
        elif token == "#" or (token == "--" and (index + 2 >= length or text[index + 2] in " \t\n\r\x0b\x0c")):
            newline = text.find("\n", index)
            index = length if newline == -1 else newline + 1
        elif token == "--":
            index += 2
        elif token == "/*":
            end = text.find("*/", index + 2)
            index = length if end == -1 else end + 2
        else:
            pieces.append(text[start:index])
            index += 1
            start = index
    pieces.append(text[start:])
    return [rust_trim(piece) for piece in pieces if rust_trim(piece)]


def statement_representable(statement):
    """Return None when a statement can be written to and split back from a block."""
    if statement != rust_trim(statement):
        return "statement has surrounding whitespace"
    if statement == "":
        return "empty statement"
    if len(mysql_statements(statement)) != 1:
        return "statement has a top-level ';' (the replay splits compound statements)"
    if "{{" in statement:
        return "statement contains '{{'"
    if "\r" in statement:
        return "statement contains a carriage return"
    if "\x00" in statement:
        return "statement contains NUL"
    return None


def split_block_statements(content):
    """Inverse of `sql_block_text` for recorder-emitted blocks."""
    return mysql_statements(content)


# ---------------------------------------------------------------------------
# Minimal .sqltest reader for recorder-emitted files (used by --verify/tests)
# ---------------------------------------------------------------------------

_DIRECTIVE = re.compile(r"[ \t]*(@[A-Za-z-]+|setup|test|expect)\b[ \t]*([^\n{]*)")


def read_sqltest(text):
    """Parse the subset of the DSL the recorder emits.

    Returns {"setups": {name: content}, "tests": [ {name, setups, sql,
    expect_kind, expect_lines, comments} ]}. Raises ValueError on anything
    outside that subset.
    """
    setups = {}
    tests = []
    pending_setups = []
    pending_comments = []
    index = 0
    length = len(text)
    while index < length:
        line_end = text.find("\n", index)
        if line_end == -1:
            line_end = length
        line = text[index:line_end]
        stripped = line.strip()
        if stripped == "":
            index = line_end + 1
            continue
        if stripped.startswith("#"):
            pending_comments.append(stripped[1:].strip())
            index = line_end + 1
            continue
        if stripped.startswith("@database"):
            index = line_end + 1
            continue
        if stripped.startswith("@setup "):
            pending_setups.append(stripped.split()[1])
            index = line_end + 1
            continue
        if stripped.startswith("setup ") or stripped.startswith("test "):
            keyword, name = stripped.split()[:2]
            brace = text.find("{", index)
            if brace == -1 or brace > line_end:
                raise ValueError(f"missing block after {keyword} {name}")
            content, index = extract_block(text, brace + 1)
            if keyword == "setup":
                setups[name] = rust_trim(content)
                pending_comments = []
                continue
            # expect block
            match = re.compile(r"\s*expect(?:[ \t]+(error|unordered|raw|pattern))?[ \t]*\{").match(text, index)
            if not match:
                raise ValueError(f"test {name} has no expect block")
            body, index = extract_block(text, match.end())
            kind = match.group(1) or "exact"
            if kind == "exact":
                lines = parse_exact_lines(body)
            elif kind == "unordered":
                lines = parse_unordered_lines(body)
            elif kind == "raw":
                lines = body.split("\n")
            elif kind == "error":
                lines = [rust_trim(body)] if rust_trim(body) else []
            else:
                raise ValueError(f"unsupported expect kind {kind}")
            tests.append({
                "name": name,
                "setups": pending_setups,
                "sql": rust_trim(content),
                "expect_kind": kind,
                "expect_lines": lines,
                "comments": pending_comments,
            })
            pending_setups = []
            pending_comments = []
            continue
        raise ValueError(f"unexpected line: {line!r}")
    return {"setups": setups, "tests": tests}


# ---------------------------------------------------------------------------
# Candidate schema
# ---------------------------------------------------------------------------

CANDIDATE_KEYS = {
    "id", "source", "group", "setup", "setup_base", "sql", "ordered", "level",
    "features", "setup_errors", "notes",
}
SOURCE_KEYS = {"repo", "revision", "path", "line", "license"}


def validate_candidate(record):
    """Return a list of schema problems (empty when valid)."""
    problems = []
    unknown = set(record) - CANDIDATE_KEYS
    if unknown:
        problems.append(f"unknown keys {sorted(unknown)}")
    for key in ("id", "group"):
        if not isinstance(record.get(key), str) or not record.get(key):
            problems.append(f"{key} must be a non-empty string")
    source = record.get("source")
    if not isinstance(source, dict) or not SOURCE_KEYS <= set(source):
        problems.append("source must contain repo, revision, path, line, license")
    for key in ("setup", "sql"):
        value = record.get(key)
        if not isinstance(value, list) or not all(isinstance(x, str) for x in value):
            problems.append(f"{key} must be a list of strings")
    if isinstance(record.get("sql"), list) and not record["sql"]:
        problems.append("sql must not be empty")
    if not isinstance(record.get("ordered"), bool):
        problems.append("ordered must be a boolean")
    level = record.get("level")
    if not isinstance(level, int) or isinstance(level, bool) or not 0 <= level <= 1000:
        problems.append("level must be an integer in 0..=1000")
    features = record.get("features")
    if not isinstance(features, list) or not all(isinstance(x, str) for x in features):
        problems.append("features must be a list of strings")
    if record.get("setup_errors", "fail") not in ("fail", "drop"):
        problems.append("setup_errors must be 'fail' or 'drop'")
    if "setup_base" in record and not isinstance(record["setup_base"], str):
        problems.append("setup_base must be a candidate id")
    if isinstance(record.get("id"), str) and isinstance(record.get("group"), str):
        if record["id"].split("/", 1)[0] != record["group"].split("/", 1)[0]:
            problems.append("id and group must share the source prefix")
    return problems


def stable_fraction(text):
    """Deterministic value in [0, 1) derived from text (for sampling)."""
    digest = hashlib.sha256(text.encode("utf-8")).digest()
    return int.from_bytes(digest[:8], "big") / 2**64


def dumps_jsonl(record):
    return json.dumps(record, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


# ---------------------------------------------------------------------------
# Feature and level classification
# ---------------------------------------------------------------------------

# Base levels follow mysql/conformance/curriculum.toml priorities: the level of
# a spec is the level of its hardest prerequisite feature plus a small
# complexity bonus, so simpler specs inside a band sort first.
FEATURE_LEVELS = {
    "select-literal": 100,
    "arithmetic": 110,
    "comparison": 120,
    "logical": 125,
    "null-handling": 130,
    "string-literal": 105,
    "function": 150,
    "case": 160,
    "cast": 170,
    "between": 140,
    "in-list": 145,
    "create-table": 200,
    "primary-key": 210,
    "insert": 250,
    "insert-select": 280,
    "table-scan": 300,
    "where": 350,
    "float-column": 360,
    "order-by": 400,
    "limit": 410,
    "distinct": 420,
    "constraint": 450,
    "create-index": 460,
    "join": 500,
    "outer-join": 520,
    "aggregate": 550,
    "group-by": 560,
    "having": 570,
    "update": 600,
    "delete": 600,
    "drop": 610,
    "transaction": 650,
    "subquery": 700,
    "exists": 710,
    "set-operation": 720,
    "view": 740,
    "cte": 780,
    "window": 800,
    "upsert": 820,
    "json": 850,
    "temporal": 860,
    "collation": 870,
    "session": 900,
    "metadata": 910,
}

_AGGREGATES = re.compile(r"\b(COUNT|SUM|AVG|MIN|MAX|GROUP_CONCAT|TOTAL|STDDEV|VARIANCE|BIT_AND|BIT_OR|BIT_XOR)\s*\(", re.I)
_FUNCTION = re.compile(r"\b(?!IN\b|EXISTS\b|VALUES\b|AS\b|ON\b|AND\b|OR\b|NOT\b|USING\b|OVER\b|FROM\b|SELECT\b|WHERE\b|CASE\b|WHEN\b|THEN\b|ELSE\b|CAST\b|INTO\b|TABLE\b|KEY\b|INDEX\b|IF\b|JOIN\b|BETWEEN\b)[A-Za-z_][A-Za-z0-9_]*\s*\(", re.I)
_STRING = re.compile(r"'(?:[^'\\]|\\.|'')*'|\"(?:[^\"\\]|\\.|\"\")*\"")
_TEMPORAL = re.compile(r"\b(DATE|TIME|DATETIME|TIMESTAMP|YEAR|NOW|CURDATE|CURTIME|INTERVAL|DATE_ADD|DATE_SUB|STR_TO_DATE|DATE_FORMAT)\b", re.I)
_TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*|\d+(?:\.\d+)?|'[^']*'|\S")


# Asterisks and dashes that are not arithmetic: comments, `SELECT *`, `t.*`,
# `COUNT(*)`.
_ARITHMETIC_NOISE = re.compile(
    r"--[^\n]*|/\*.*?\*/|\(\s*\*\s*\)|\.\s*\*|"
    r"\bSELECT\s+(?:ALL\s+|DISTINCT\s+)?\*|,\s*\*(?=\s*(?:,|FROM\b))",
    re.I | re.S,
)


def _strip_strings(sql):
    return _STRING.sub("''", sql)


def _words(sql):
    return " " + " ".join(re.sub(r"[(),;]", " \\g<0> ", sql.upper()).split()) + " "


_FROM_END = re.compile(r"\b(WHERE|GROUP|ORDER|HAVING|LIMIT|UNION|EXCEPT|INTERSECT|WINDOW)\b", re.I)


def from_width(sql):
    """Number of tables in the first FROM clause (comma list plus JOINs)."""
    text = _strip_strings(sql)
    match = re.search(r"\bFROM\b", text, re.I)
    if not match:
        return 0
    depth = 0
    width = 1
    rest = text[match.end():]
    index = 0
    while index < len(rest):
        char = rest[index]
        if char == "(":
            depth += 1
        elif char == ")":
            if depth == 0:
                break
            depth -= 1
        elif depth == 0:
            if char == ",":
                width += 1
            elif char.isalpha() and (index == 0 or not (rest[index - 1].isalnum() or rest[index - 1] == "_")):
                word = re.match(r"[A-Za-z_]+", rest[index:]).group(0)
                if _FROM_END.fullmatch(word):
                    break
                if word.upper() == "JOIN":
                    width += 1
                index += len(word)
                continue
        index += 1
    return width


def first_keyword(sql):
    value = sql.lstrip()
    while True:
        if value.startswith("--") or value.startswith("#"):
            value = value.partition("\n")[2].lstrip()
        elif value.startswith("/*") and "*/" in value:
            value = value.partition("*/")[2].lstrip()
        else:
            break
    match = re.match(r"\(*\s*([A-Za-z_]+)", value)
    return match.group(1).upper() if match else "UNKNOWN"


def statement_features(sql):
    """Feature tags for one statement."""
    text = _strip_strings(sql)
    words = _words(text)
    first = first_keyword(sql)
    features = set()
    if _STRING.search(sql):
        features.add("string-literal")
    if first in ("SELECT", "WITH", "VALUES", "TABLE"):
        if " FROM " not in words:
            features.add("select-literal")
        else:
            features.add("table-scan")
    elif first == "CREATE":
        if re.search(r"\bCREATE\s+(UNIQUE\s+)?INDEX\b", text, re.I):
            features.add("create-index")
        elif re.search(r"\bCREATE\s+(OR\s+REPLACE\s+)?(ALGORITHM\s*=\s*\w+\s+)?VIEW\b", text, re.I):
            features.add("view")
        else:
            features.add("create-table")
            if re.search(r"\b(FLOAT|REAL|DOUBLE)\b", text, re.I):
                features.add("float-column")
            if re.search(r"\bPRIMARY\s+KEY\b", text, re.I):
                features.add("primary-key")
            if re.search(r"\b(UNIQUE|CHECK|FOREIGN\s+KEY|NOT\s+NULL|DEFAULT)\b", text, re.I):
                features.add("constraint")
    elif first in ("INSERT", "REPLACE"):
        features.add("insert")
        if " SELECT " in words:
            features.add("insert-select")
        if " ON DUPLICATE KEY " in words or first == "REPLACE":
            features.add("upsert")
    elif first == "UPDATE":
        features.add("update")
    elif first == "DELETE":
        features.add("delete")
    elif first in ("DROP", "ALTER", "TRUNCATE", "RENAME"):
        features.add("drop")
    elif first in ("BEGIN", "START", "COMMIT", "ROLLBACK", "SAVEPOINT", "RELEASE"):
        features.add("transaction")
    elif first in ("SET", "USE"):
        features.add("session")
    elif first in ("SHOW", "DESCRIBE", "DESC", "EXPLAIN"):
        features.add("metadata")
    else:
        features.add(f"statement:{first.lower()}")
    if re.search(r"[-+*/%]|\bDIV\b|\bMOD\b", _ARITHMETIC_NOISE.sub(" ", text)):
        features.add("arithmetic")
    if re.search(r"<>|!=|<=|>=|[<>=]", text):
        features.add("comparison")
    if re.search(r"\b(AND|OR|NOT|XOR)\b", text, re.I):
        features.add("logical")
    if re.search(r"\bIS\s+(NOT\s+)?NULL\b|\bNULL\b|\b(COALESCE|IFNULL|NULLIF)\s*\(", text, re.I):
        features.add("null-handling")
    if re.search(r"\bCASE\b", text, re.I):
        features.add("case")
    if re.search(r"\bCAST\s*\(|\bCONVERT\s*\(", text, re.I):
        features.add("cast")
    if re.search(r"\bBETWEEN\b", text, re.I):
        features.add("between")
    if re.search(r"\bIN\s*\(\s*(?!SELECT\b)", text, re.I):
        features.add("in-list")
    if " WHERE " in words:
        features.add("where")
    if " ORDER BY " in words:
        features.add("order-by")
    if " LIMIT " in words:
        features.add("limit")
    if " DISTINCT " in words:
        features.add("distinct")
    if _AGGREGATES.search(text):
        features.add("aggregate")
    if " GROUP BY " in words:
        features.add("group-by")
    if " HAVING " in words:
        features.add("having")
    if " JOIN " in words or re.search(r"\bFROM\s+[^()]*?,", text, re.I | re.S):
        features.add("join")
    if re.search(r"\b(LEFT|RIGHT|FULL)\s+(OUTER\s+)?JOIN\b", text, re.I):
        features.add("outer-join")
    if re.search(r"\(\s*SELECT\b", text, re.I) and first != "INSERT":
        features.add("subquery")
    if re.search(r"\bEXISTS\s*\(", text, re.I):
        features.add("exists")
    if re.search(r"\b(UNION|INTERSECT|EXCEPT)\b", text, re.I):
        features.add("set-operation")
    if first == "WITH":
        features.add("cte")
    if re.search(r"\bOVER\s*\(|\bOVER\s+\w+|\bWINDOW\s+\w+\s+AS\b", text, re.I):
        features.add("window")
    if re.search(r"\bJSON", text, re.I):
        features.add("json")
    if _TEMPORAL.search(text):
        features.add("temporal")
    if re.search(r"\bCOLLATE\b|\bCHARACTER\s+SET\b|\bCHARSET\b", text, re.I):
        features.add("collation")
    if _FUNCTION.search(text):
        features.add("function")
    return features


def setup_statement_features(statement):
    """Features a setup statement contributes as a prerequisite of a spec."""
    return {f for f in statement_features(statement) if f in _SETUP_FEATURES}


def classify(setup, sql, setup_features=None):
    """Return (level, sorted features) for a candidate.

    `setup_features` may carry the precomputed union of
    `setup_statement_features` over `setup` (importers with long shared setups
    compute it incrementally).
    """
    if setup_features is None:
        setup_features = set()
        for statement in setup:
            setup_features |= setup_statement_features(statement)
    features = set(setup_features)
    tokens = 0
    for statement in sql:
        features |= statement_features(statement)
        tokens += len(_TOKEN.findall(_strip_strings(statement)))
    base = max((FEATURE_LEVELS.get(f, 950) for f in features), default=100)
    bonus = min(49, tokens // 8 + max(0, len(setup).bit_length() - 3))
    return min(1000, base + bonus), sorted(features)


# Features of setup statements that are prerequisites of the spec itself.
_SETUP_FEATURES = {
    "create-table", "insert", "insert-select", "float-column", "constraint", "primary-key",
    "create-index", "update", "delete", "drop", "view", "transaction", "upsert",
    "session", "json", "temporal", "collation",
}


# ---------------------------------------------------------------------------
# Statement side-effect classification (recorder scheduling)
# ---------------------------------------------------------------------------

_READ_ONLY_FIRST = {"SELECT", "WITH", "VALUES", "TABLE", "SHOW", "DESCRIBE", "DESC", "EXPLAIN"}
# Constructs whose result depends on the previous statement on the session or
# that change session state; such statements always run in a fresh session.
_STATEFUL = re.compile(
    r"@|\bINTO\b|\bFOR\s+(UPDATE|SHARE)\b|\bLOCK\b|\bLAST_INSERT_ID\b|\bROW_COUNT\b|"
    r"\bFOUND_ROWS\b|\bSQL_CALC_FOUND_ROWS\b|\bGET_LOCK\b|\bRELEASE_(ALL_)?LOCKS?\b|"
    r"\bIS_(FREE|USED)_LOCK\b|\bSLEEP\b|\bBENCHMARK\b|\bWARNINGS\b|\bERRORS\b|"
    r"\bSTATUS\b|\bPROFILES?\b|\bANALYZE\b|\bCONNECTION_ID\b|\bNEXTVAL\b|\bSETVAL\b",
    re.I,
)
# Constructs that observe or change server-global state (other databases,
# users, global variables); these run serially on a quiet server.
_GLOBAL = re.compile(
    r"\b(DATABASE|DATABASES|SCHEMA|SCHEMAS|USER|USERS|GRANT|REVOKE|ROLE|GLOBAL|PERSIST|"
    r"PERSIST_ONLY|INSTALL|UNINSTALL|PLUGIN|SERVER|TABLESPACE|LOGFILE|BINLOG|MASTER|"
    r"REPLICA|SLAVE|INFORMATION_SCHEMA|PERFORMANCE_SCHEMA|PROCESSLIST|FLUSH|RESET|"
    r"KILL|SHUTDOWN|EVENT|PRIVILEGES)\b|\bmysql\s*\.|\bsys\s*\.",
    re.I,
)


def references_tables(statement):
    """True unless the statement provably reads no table (no FROM/TABLE clause)."""
    words = _words(_strip_strings(statement))
    return any(marker in words for marker in (" FROM ", " TABLE ", " JOIN ", " INTO ")) \
        or first_keyword(statement) not in ("SELECT", "VALUES")


_MUTATIONS = [
    (re.compile(r"^\s*SET\b.*(\bGLOBAL\b|\bPERSIST(_ONLY)?\b|@@GLOBAL\.|@@PERSIST(_ONLY)?\.)", re.I | re.S),
     "sets a global variable"),
    (re.compile(r"^\s*SET\s+PASSWORD\b|^\s*(ALTER|RENAME)\s+USER\b", re.I), "changes a user account"),
    (re.compile(r"^\s*(GRANT|REVOKE|DROP\s+USER|DROP\s+ROLE|SET\s+(DEFAULT\s+)?ROLE)\b.*\broot\b", re.I | re.S),
     "changes the root account"),
    (re.compile(r"^\s*(INSTALL|UNINSTALL)\b|\bSONAME\b", re.I), "installs server code"),
    (re.compile(r"\bTABLESPACE\b|\bLOGFILE\s+GROUP\b|^\s*(CREATE|ALTER|DROP)\s+SERVER\b", re.I),
     "manages server storage or servers"),
    (re.compile(r"^\s*(RESET|PURGE|SHUTDOWN|RESTART|KILL|CLONE|XA)\b|^\s*(LOCK|UNLOCK|ALTER)\s+INSTANCE\b", re.I),
     "administers the server"),
    (re.compile(r"^\s*(CHANGE\s+(MASTER|REPLICATION)|START\s+(SLAVE|REPLICA|GROUP_REPLICATION)|"
                r"STOP\s+(SLAVE|REPLICA|GROUP_REPLICATION))\b", re.I), "controls replication"),
]


def server_mutation(statement):
    """Why a statement must never run on the shared recording server, or None.

    These change server-global state that dropping the test databases and
    users cannot undo (global variables, plugins, replication, the root
    account), or leave state behind a session (XA transactions).
    """
    text = _strip_strings(statement)
    text = re.sub(r"^(\s*(--[^\n]*\n|#[^\n]*\n|/\*.*?\*/))*", "", text, flags=re.S)
    for pattern, reason in _MUTATIONS:
        if pattern.search(text):
            return reason
    return None


_DIAGNOSTICS = re.compile(
    r"^\s*SHOW\s+(COUNT\s*\(\s*\*\s*\)\s+)?(WARNINGS|ERRORS)\b|^\s*GET\s+(CURRENT\s+|STACKED\s+)?DIAGNOSTICS\b|"
    r"@@(SESSION\.)?(WARNING_COUNT|ERROR_COUNT)\b|\bROW_COUNT\s*\(|\bFOUND_ROWS\s*\(",
    re.I,
)
_STATISTICS = re.compile(
    r"^\s*SHOW\s+(EXTENDED\s+)?(INDEX|INDEXES|KEYS|TABLE\s+STATUS)\b|"
    r"\bINFORMATION_SCHEMA\s*\.\s*`?(STATISTICS|TABLES|PARTITIONS|INNODB_\w+)\b|"
    r"\b(CARDINALITY|TABLE_ROWS|DATA_LENGTH|INDEX_LENGTH|DATA_FREE|AVG_ROW_LENGTH|UPDATE_TIME|CHECK_TIME)\b",
    re.I,
)


def reads_diagnostics(statement):
    """True when the result depends on the previous statement's diagnostics."""
    return bool(_DIAGNOSTICS.search(_strip_strings(statement)))


def reads_storage_statistics(statement):
    """True when the result includes statistics InnoDB updates asynchronously."""
    return bool(_STATISTICS.search(_strip_strings(statement)))


def is_read_only(statement):
    text = _strip_strings(statement)
    return first_keyword(statement) in _READ_ONLY_FIRST and not _STATEFUL.search(text)


def touches_global_state(statement):
    return first_keyword(statement) == "USE" or bool(_GLOBAL.search(_strip_strings(statement)))
