"""Shared helpers for the MySQL spec candidate importers.

Importers turn upstream test suites into candidate JSONL records (see
``mysql/conformance/SPECS.md``). They only extract SQL, setup, ordering hints,
provenance and a rough difficulty level; expected results are recorded later
from the pinned MySQL oracle and are never taken from the upstream source.

Every importer reads untrusted upstream checkouts as plain data: nothing from a
source tree is executed or imported.
"""

import collections
import hashlib
import json
import pathlib
import re
import subprocess

SHARD_SIZE = 5000

# ---------------------------------------------------------------------------
# SQL lexing
# ---------------------------------------------------------------------------

TOKEN_RE = re.compile(
    r"""
    (?P<ws>\s+)
  | (?P<comment>--(?=[\s]|$)[^\n]*|\#[^\n]*|/\*(?![!+]).*?\*/)
  | (?P<hint>/\*\+.*?\*/)
  | (?P<exec>/\*!.*?\*/)
  | (?P<string>'(?:[^'\\]|\\.|'')*'|"(?:[^"\\]|\\.|"")*")
  | (?P<qident>`(?:[^`]|``)*`)
  | (?P<hexlit>[xX]'[0-9A-Fa-f]*'|0[xX][0-9A-Fa-f]+(?![0-9A-Za-z_])|[bB]'[01]*'|0b[01]+(?![0-9A-Za-z_]))
  | (?P<number>(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?)
  | (?P<var>@@?(?:[A-Za-z0-9_.$]+|`(?:[^`]|``)*`|'(?:[^'\\]|\\.)*'|"(?:[^"\\]|\\.)*")?)
  | (?P<ident>[A-Za-z_$\u0080-\U0010ffff][A-Za-z0-9_$\u0080-\U0010ffff]*)
  | (?P<op><=>|->>|->|<<|>>|<=|>=|<>|!=|:=|\|\||&&|[-+*/%=<>!~^&|])
  | (?P<punct>[(),.;?:{}\[\]\\])
    """,
    re.S | re.X,
)

Token = collections.namedtuple("Token", "kind text start")


def tokenize(sql):
    """Yield SQL tokens. Unterminated quotes or comments become one ``error`` token."""
    position = 0
    length = len(sql)
    while position < length:
        match = TOKEN_RE.match(sql, position)
        if match is None:
            yield Token("error", sql[position:], position)
            return
        kind = match.lastgroup
        yield Token(kind, match.group(), position)
        position = match.end()


def significant_tokens(sql):
    """Tokens without whitespace and plain comments (hints and /*! */ are kept)."""
    return [token for token in tokenize(sql) if token.kind not in ("ws", "comment")]


def strip_comments(sql):
    """Remove plain comments, keeping strings, hints and executable comments."""
    pieces = []
    for token in tokenize(sql):
        if token.kind == "comment":
            pieces.append(" ")
        else:
            pieces.append(token.text)
    return "".join(pieces).strip()


def first_keyword(sql):
    for token in significant_tokens(sql):
        if token.text == "(":
            continue
        if token.kind == "ident":
            return token.text.lower()
        if token.kind in ("hint", "exec"):
            continue
        return ""
    return ""


def has_top_level_order_by(sql):
    """True when the outermost statement ends with an ORDER BY clause."""
    depth = 0
    previous = None
    found = False
    for token in significant_tokens(sql):
        text = token.text.lower()
        if token.text == "(":
            depth += 1
        elif token.text == ")":
            depth -= 1
        elif depth == 0 and token.kind == "ident":
            if text == "by" and previous == "order":
                found = True
            elif text in ("union", "intersect", "except"):
                found = False
        if token.kind == "ident":
            previous = text
        else:
            previous = None
    return found


def split_statements(text, delimiter=";"):
    """Split SQL text at ``delimiter`` outside quotes and comments.

    Returns ``(statement, start_offset)`` pairs with surrounding whitespace removed
    and the delimiter dropped. Trailing text without a delimiter is returned too.
    """
    statements = []
    start = 0
    position = 0
    length = len(text)
    while position < length:
        if text.startswith(delimiter, position):
            piece = text[start:position]
            if piece.strip():
                offset = start + (len(piece) - len(piece.lstrip()))
                statements.append((piece.strip(), offset))
            position += len(delimiter)
            start = position
            continue
        match = TOKEN_RE.match(text, position)
        if match is None:
            break
        if match.lastgroup in ("string", "qident", "comment", "hint", "exec"):
            # A delimiter inside quotes or comments does not end the statement.
            position = match.end()
            continue
        # Other tokens may contain or straddle a custom delimiter such as ``$$``.
        found = text.find(delimiter, position + 1, match.end() + len(delimiter) - 1)
        position = found if found >= 0 else match.end()
    piece = text[start:]
    if piece.strip():
        offset = start + (len(piece) - len(piece.lstrip()))
        statements.append((piece.strip(), offset))
    return statements


# ---------------------------------------------------------------------------
# Feature classification and levels
# ---------------------------------------------------------------------------

MATH_FUNCTIONS = set(
    """abs acos asin atan atan2 ceil ceiling conv cos cot crc32 degrees exp floor ln log
    log10 log2 mod pi pow power radians rand round sign sin sqrt tan truncate""".split()
)
STRING_FUNCTIONS = set(
    """ascii bin bit_length char char_length character_length concat concat_ws elt
    export_set field find_in_set format from_base64 hex insert instr lcase left length
    locate lower lpad ltrim make_set mid oct octet_length ord position quote repeat replace
    reverse right rpad rtrim soundex space strcmp substr substring substring_index to_base64
    trim ucase unhex upper weight_string""".split()
)
REGEXP_FUNCTIONS = {"regexp_instr", "regexp_like", "regexp_replace", "regexp_substr"}
TEMPORAL_FUNCTIONS = set(
    """adddate addtime convert_tz curdate current_date current_time current_timestamp
    curtime date date_add date_format date_sub datediff day dayname dayofmonth dayofweek
    dayofyear extract from_days from_unixtime get_format hour last_day localtime
    localtimestamp makedate maketime microsecond minute month monthname now period_add
    period_diff quarter sec_to_time second str_to_date subdate subtime sysdate time
    time_format time_to_sec timediff timestamp timestampadd timestampdiff to_days
    to_seconds unix_timestamp utc_date utc_time utc_timestamp week weekday weekofyear
    year yearweek""".split()
)
AGGREGATE_FUNCTIONS = set(
    """avg bit_and bit_or bit_xor count group_concat json_arrayagg json_objectagg max min
    std stddev stddev_pop stddev_samp sum var_pop var_samp variance any_value""".split()
)
WINDOW_FUNCTIONS = set(
    """cume_dist dense_rank first_value lag last_value lead nth_value ntile percent_rank
    rank row_number""".split()
)
CONTROL_FUNCTIONS = {"if", "ifnull", "nullif", "coalesce", "greatest", "least", "interval", "isnull"}
CHARSET_FUNCTIONS = {"charset", "collation", "coercibility"}
MISC_FUNCTIONS = set(
    """md5 sha sha1 sha2 aes_encrypt aes_decrypt compress uncompress uncompressed_length
    random_bytes uuid uuid_short uuid_to_bin bin_to_uuid is_uuid inet_aton inet_ntoa
    inet6_aton inet6_ntoa is_ipv4 is_ipv4_compat is_ipv4_mapped is_ipv6 bit_count
    name_const sleep benchmark""".split()
)
INFO_FUNCTIONS = set(
    """database schema user current_user session_user system_user version connection_id
    last_insert_id row_count found_rows icu_version current_role""".split()
)
CAST_FUNCTIONS = {"cast", "convert", "binary"}
SPATIAL_PREFIXES = ("st_", "mbr")
SPATIAL_WORDS = {
    "point", "linestring", "polygon", "geometry", "multipoint", "multilinestring",
    "multipolygon", "geometrycollection", "geomcollection", "spatial",
}
CHARSET_INTRODUCERS = re.compile(
    r"_(?:utf8mb4|utf8mb3|utf8|latin1|latin2|binary|ascii|ucs2|utf16|utf16le|utf32|koi8r|"
    r"cp1251|cp1250|cp932|gbk|gb18030|gb2312|big5|sjis|ujis|euckr|tis620|greek|hebrew|"
    r"dec8|hp8|swe7|armscii8|cp850|cp852|cp866|keybcs2|macce|macroman|geostd8|eucjpms|"
    r"latin5|latin7|koi8u|tis620)$",
    re.I,
)
TEMPORAL_TYPES = {"date", "datetime", "timestamp", "time", "year"}
STATEMENT_FEATURES = {
    "select": "select",
    "with": "select",
    "table": "select",
    "values": "select",
    "insert": "insert",
    "replace": "replace",
    "update": "update",
    "delete": "delete",
    "create": "ddl",
    "alter": "alter",
    "drop": "drop",
    "truncate": "truncate",
    "rename": "rename",
    "show": "show",
    "describe": "describe",
    "desc": "describe",
    "explain": "explain",
    "set": "set",
    "begin": "transaction",
    "start": "transaction",
    "commit": "transaction",
    "rollback": "transaction",
    "savepoint": "transaction",
    "release": "transaction",
    "prepare": "prepared-statement",
    "execute": "prepared-statement",
    "deallocate": "prepared-statement",
    "call": "routine",
    "lock": "lock",
    "unlock": "lock",
    "do": "do",
    "use": "use",
    "analyze": "admin",
    "check": "admin",
    "checksum": "admin",
    "optimize": "admin",
    "repair": "admin",
    "load": "load",
    "handler": "handler",
}

# Base level for each feature. A candidate's level is the highest base level of
# its features plus a small bump for breadth. Lower levels are prerequisites for
# higher ones in the rough implementation order of the MySQL curriculum.
FEATURE_LEVELS = {
    "statement:select": 0,
    "literal:integer": 0,
    "literal:string": 5,
    "literal:null": 5,
    "literal:boolean": 10,
    "literal:decimal": 30,
    "literal:float": 40,
    "literal:hex": 60,
    "literal:bit": 60,
    "literal:temporal": 300,
    "operator:arithmetic": 40,
    "operator:comparison": 40,
    "operator:logical": 50,
    "operator:is": 50,
    "operator:null-safe-equal": 60,
    "operator:in": 70,
    "operator:between": 70,
    "operator:bitwise": 90,
    "row-constructor": 110,
    "operator:like": 120,
    "case": 100,
    "function:control": 100,
    "function:math": 150,
    "statement:ddl": 150,
    "statement:drop": 150,
    "create-table": 150,
    "primary-key": 155,
    "from": 160,
    "statement:insert": 160,
    "where": 170,
    "function:string": 180,
    "order-by": 190,
    "limit": 195,
    "variable:user": 200,
    "distinct": 210,
    "cast": 220,
    "function:misc": 250,
    "statement:update": 260,
    "statement:delete": 260,
    "statement:truncate": 270,
    "statement:replace": 280,
    "create-index": 300,
    "charset": 320,
    "collation": 330,
    "type:decimal": 340,
    "type:float": 340,
    "type:binary": 350,
    "type:enum-set": 360,
    "type:temporal": 380,
    "statement:alter": 380,
    "statement:rename": 390,
    "function:temporal": 400,
    "auto-increment": 420,
    "operator:regexp": 450,
    "function:regexp": 450,
    "insert-ignore": 460,
    "insert-select": 465,
    "upsert": 470,
    "aggregate": 480,
    "group-by": 490,
    "check-constraint": 500,
    "having": 510,
    "foreign-key": 520,
    "join": 540,
    "outer-join": 560,
    "set-operation": 570,
    "derived-table": 590,
    "subquery": 600,
    "exists": 610,
    "generated-column": 620,
    "type:json": 600,
    "function:json": 600,
    "operator:json": 610,
    "create-view": 640,
    "cte": 650,
    "statement:transaction": 660,
    "recursive-cte": 690,
    "window": 700,
    "function:info": 700,
    "statement:set": 720,
    "variable:system": 720,
    "statement:describe": 740,
    "statement:show": 750,
    "statement:do": 760,
    "statement:use": 760,
    "statement:prepared-statement": 780,
    "information-schema": 800,
    "statement:lock": 820,
    "lock": 820,
    "statement:admin": 830,
    "statement:routine": 850,
    "routine": 850,
    "trigger": 860,
    "partition": 880,
    "fulltext": 900,
    "spatial": 900,
    "statement:load": 920,
    "statement:handler": 940,
    "statement:explain": 950,
    "statement:other": 900,
}


def _call_names(tokens):
    names = []
    for index, token in enumerate(tokens[:-1]):
        if token.kind == "ident" and tokens[index + 1].text == "(":
            names.append(token.text.lower())
    return names


def features_of(sql):
    """Return the set of feature tags used by one SQL statement."""
    tokens = significant_tokens(sql)
    features = set()
    if not tokens:
        return features
    lowered = [token.text.lower() for token in tokens]
    first = first_keyword(sql)
    features.add("statement:" + STATEMENT_FEATURES.get(first, "other" if first else "select"))

    calls = set(_call_names(tokens))
    for name in calls:
        if name in AGGREGATE_FUNCTIONS:
            features.add("aggregate")
        if name in WINDOW_FUNCTIONS:
            features.add("window")
        if name in MATH_FUNCTIONS:
            features.add("function:math")
        if name in STRING_FUNCTIONS:
            features.add("function:string")
        if name in REGEXP_FUNCTIONS:
            features.add("function:regexp")
        if name in TEMPORAL_FUNCTIONS:
            features.add("function:temporal")
        if name in CONTROL_FUNCTIONS:
            features.add("function:control")
        if name in CHARSET_FUNCTIONS:
            features.add("charset")
        if name in MISC_FUNCTIONS:
            features.add("function:misc")
        if name in INFO_FUNCTIONS:
            features.add("function:info")
        if name in CAST_FUNCTIONS:
            features.add("cast")
        if name.startswith("json_"):
            features.add("function:json")
        if name.startswith(SPATIAL_PREFIXES) or name in SPATIAL_WORDS:
            features.add("spatial")

    depth = 0
    select_count = 0
    for index, token in enumerate(tokens):
        text = lowered[index]
        previous = lowered[index - 1] if index else ""
        following = lowered[index + 1] if index + 1 < len(tokens) else ""
        if token.text == "(":
            depth += 1
            if following == "select" and previous == "from":
                features.add("derived-table")
            continue
        if token.text == ")":
            depth -= 1
            continue
        kind = token.kind
        if kind == "number":
            if "e" in text:
                features.add("literal:float")
            elif "." in text:
                features.add("literal:decimal")
            else:
                features.add("literal:integer")
        elif kind == "string":
            if previous in ("date", "time", "timestamp") and not (index >= 2 and lowered[index - 2] == "("):
                features.add("literal:temporal")
            else:
                features.add("literal:string")
        elif kind == "hexlit":
            features.add("literal:bit" if text.startswith(("b'", "0b")) else "literal:hex")
        elif kind == "var":
            features.add("variable:system" if text.startswith("@@") else "variable:user")
        elif kind == "hint":
            features.add("optimizer-hint")
        elif kind == "op":
            if text == "*" and previous in ("select", ",", ".", "(", "distinct", "all", ""):
                pass  # wildcard projection or COUNT(*)
            elif text in ("+", "-", "*", "/", "%"):
                features.add("operator:arithmetic")
            elif text in ("=", "<", ">", "<=", ">=", "<>", "!="):
                features.add("operator:comparison")
            elif text == "<=>":
                features.add("operator:null-safe-equal")
            elif text in ("&&", "||", "!"):
                features.add("operator:logical")
            elif text in ("&", "|", "^", "<<", ">>", "~"):
                features.add("operator:bitwise")
            elif text in ("->", "->>"):
                features.add("operator:json")
        elif kind in ("ident", "qident"):
            if kind == "qident":
                continue
            if text == "null":
                features.add("literal:null")
            elif text in ("true", "false"):
                features.add("literal:boolean")
            elif text in ("div", "mod") and following != "(":
                features.add("operator:arithmetic")
            elif text in ("and", "or", "xor", "not"):
                features.add("operator:logical")
            elif text == "is":
                features.add("operator:is")
            elif text == "in" and following == "(":
                features.add("operator:in")
            elif text == "between":
                features.add("operator:between")
            elif text == "like":
                features.add("operator:like")
            elif text in ("regexp", "rlike"):
                features.add("operator:regexp")
            elif text == "case":
                features.add("case")
            elif text == "collate":
                features.add("collation")
            elif text in ("charset", "character") or (text == "using" and depth > 0):
                features.add("charset")
            elif CHARSET_INTRODUCERS.match(text):
                features.add("charset")
            elif text == "select":
                select_count += 1
                if depth > 0 and previous == "(":
                    features.add("subquery")
            elif text == "from" and depth == 0:
                features.add("from")
                if following == "(":
                    features.add("derived-table")
            elif text == "where":
                features.add("where")
            elif text == "by" and previous == "group":
                features.add("group-by")
            elif text == "by" and previous == "order" and "over" not in lowered[max(0, index - 6) : index]:
                features.add("order-by")
            elif text == "having":
                features.add("having")
            elif text == "limit":
                features.add("limit")
            elif text == "distinct" or text == "distinctrow":
                features.add("distinct")
            elif text == "join" or text == "straight_join":
                features.add("join")
            elif text in ("left", "right", "full") and following in ("join", "outer"):
                features.add("outer-join")
            elif text in ("union", "intersect", "except"):
                features.add("set-operation")
            elif text == "with" and index == 0 or (text == "with" and previous == "("):
                features.add("cte")
                if following == "recursive":
                    features.add("recursive-cte")
            elif text == "over" and index + 1 < len(tokens) and (following == "(" or tokens[index + 1].kind == "ident"):
                features.add("window")
            elif text == "window" and depth == 0 and first == "select":
                features.add("window")
            elif text == "exists" and following == "(":
                features.add("exists")
            elif text == "json":
                features.add("type:json")
            elif text in ("information_schema", "performance_schema", "sys"):
                features.add("information-schema")
            elif text == "auto_increment":
                features.add("auto-increment")
            elif text == "references" or (text == "key" and previous == "foreign"):
                features.add("foreign-key")
            elif text == "check" and following == "(":
                features.add("check-constraint")
            elif text in ("generated", "stored", "virtual") and first in ("create", "alter"):
                features.add("generated-column")
            elif text == "partition" and first in ("create", "alter", "select", "insert"):
                features.add("partition")
            elif text in ("fulltext", "against"):
                features.add("fulltext")
            elif text in SPATIAL_WORDS and first in ("create", "alter"):
                features.add("spatial")
            elif text in ("decimal", "numeric", "dec") and first in ("create", "alter"):
                features.add("type:decimal")
            elif text in ("float", "double", "real") and first in ("create", "alter"):
                features.add("type:float")
            elif text in ("blob", "binary", "varbinary", "tinyblob", "mediumblob", "longblob", "bit") and first in ("create", "alter"):
                features.add("type:binary")
            elif text in ("enum",) or (text == "set" and following == "(" and first in ("create", "alter")):
                features.add("type:enum-set")
            elif text in TEMPORAL_TYPES and first in ("create", "alter") and following != "(":
                features.add("type:temporal")
            elif text == "duplicate" and following == "key":
                features.add("upsert")
            elif text == "ignore" and previous == "insert":
                features.add("insert-ignore")
            elif text == "trigger":
                features.add("trigger")
            elif text in ("procedure", "function", "event") and first in ("create", "drop", "alter", "call"):
                features.add("routine")
            elif text in ("update", "share") and previous == "for":
                features.add("lock")
            elif text == "key" and previous == "primary":
                features.add("primary-key")
            elif first in ("create", "alter") and (
                text == "index" or (text == "key" and previous in ("unique", ",", "("))
            ):
                features.add("create-index")
            elif text == "interval" and following != "(":
                features.add("function:temporal")
            elif text == "table" and previous == "create":
                features.add("create-table")
            elif text == "view" and first == "create":
                features.add("create-view")
            elif first == "insert" and text == "select":
                features.add("insert-select")
    if first == "insert" and "select" in lowered:
        features.add("insert-select")
    if select_count > 1 and "subquery" not in features and "set-operation" not in features:
        features.add("subquery")
    return features


def level_of(features, length=0):
    if not features:
        return 0
    base = max(FEATURE_LEVELS.get(feature, 300) for feature in features)
    bump = min(30, 3 * (len(features) - 1)) + min(9, length // 120)
    return min(1000, base + bump)


def classify(sql_statements, setup_statements=()):
    """Return ``(level, features)`` for a candidate.

    Features of setup statements are included because a replay must execute the
    setup before the tested statement can pass.
    """
    features = set()
    length = 0
    for statement in list(sql_statements) + list(setup_statements):
        features |= features_of(statement)
    for statement in sql_statements:
        length += len(statement)
    return level_of(features, length), sorted(features)


# ---------------------------------------------------------------------------
# Candidates, output and reports
# ---------------------------------------------------------------------------


def make_candidate(
    *,
    candidate_id,
    repo,
    revision,
    path,
    line,
    license,
    group,
    setup,
    sql,
    ordered,
    setup_errors=None,
):
    setup = [statement.strip() for statement in setup]
    sql = [statement.strip() for statement in sql]
    level, features = classify(sql, setup)
    candidate = {
        "id": candidate_id,
        "source": {
            "repo": repo,
            "revision": revision,
            "path": path,
            "line": line,
            "license": license,
        },
        "group": group,
        "setup": setup,
        "sql": sql,
        "ordered": bool(ordered),
        "level": level,
        "features": features,
    }
    if setup_errors:
        candidate["setup_errors"] = setup_errors
    return candidate


def is_utf8_text(text):
    try:
        text.encode("utf-8")
    except UnicodeEncodeError:
        return False
    return True


def write_candidates(output, source_name, candidates, shard_size=SHARD_SIZE):
    """Write candidates as JSONL shards, replacing earlier shards of this source."""
    output = pathlib.Path(output)
    output.mkdir(parents=True, exist_ok=True)
    for old in output.glob("*.jsonl"):
        old.unlink()
    seen = set()
    shards = 0
    for index in range(0, len(candidates), shard_size):
        path = output / f"{source_name}-{index // shard_size:05}.jsonl"
        with path.open("w", encoding="utf-8") as handle:
            for candidate in candidates[index : index + shard_size]:
                if candidate["id"] in seen:
                    raise ValueError(f"duplicate candidate id {candidate['id']}")
                seen.add(candidate["id"])
                handle.write(json.dumps(candidate, ensure_ascii=False, separators=(",", ":")) + "\n")
        shards += 1
    return shards


class Report:
    """Collects import counters and writes ``IMPORT_REPORT.md``."""

    def __init__(self, title, source):
        self.title = title
        self.source = source
        self.counts = collections.Counter()
        self.filtered = collections.Counter()
        self.notes = []
        self.tables = []

    def count(self, key, amount=1):
        self.counts[key] += amount

    def filter(self, reason, amount=1):
        self.filtered[reason] += amount

    def note(self, text):
        self.notes.append(text)

    def table(self, title, header, rows):
        self.tables.append((title, header, rows))

    def render(self):
        lines = [f"# {self.title}", ""]
        for key, value in self.source.items():
            lines.append(f"- {key}: {value}")
        lines += ["", "## Counts", "", "| metric | count |", "|---|---:|"]
        for key, value in self.counts.items():
            lines.append(f"| {key} | {value:,} |")
        lines += ["", "## Filtered (not emitted)", "", "| reason | count |", "|---|---:|"]
        for key, value in sorted(self.filtered.items(), key=lambda item: (-item[1], item[0])):
            lines.append(f"| {key} | {value:,} |")
        lines.append(f"| **total** | {sum(self.filtered.values()):,} |")
        for title, header, rows in self.tables:
            lines += ["", f"## {title}", "", "| " + " | ".join(header) + " |"]
            lines.append("|" + "|".join("---" if index == 0 else "---:" for index in range(len(header))) + "|")
            for row in rows:
                lines.append("| " + " | ".join(f"{cell:,}" if isinstance(cell, int) else str(cell) for cell in row) + " |")
        if self.notes:
            lines += ["", "## Notes", ""]
            lines += [f"- {note}" for note in self.notes]
        return "\n".join(lines) + "\n"

    def write(self, output):
        output = pathlib.Path(output)
        output.mkdir(parents=True, exist_ok=True)
        text = self.render()
        (output / "IMPORT_REPORT.md").write_text(text, encoding="utf-8")
        return text


def level_histogram(candidates):
    buckets = collections.Counter((candidate["level"] // 100) * 100 for candidate in candidates)
    return [(f"{bucket}-{bucket + 99}", buckets[bucket]) for bucket in sorted(buckets)]


def git_revision(path):
    """Return the checked-out commit of a source tree without running its code."""
    try:
        result = subprocess.run(
            ["git", "-C", str(path), "rev-parse", "HEAD"],
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return None
    return result.stdout.strip()


def check_revision(path, expected, allow_mismatch=False):
    actual = git_revision(path)
    if actual != expected and not allow_mismatch:
        raise SystemExit(
            f"{path}: checked-out revision {actual} does not match pinned revision {expected}"
        )
    return actual or expected


def stable_hash(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()
