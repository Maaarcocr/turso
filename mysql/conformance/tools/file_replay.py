"""Shared file-replay importer for mysqltest-style SQL test suites.

TiDB ``tests/integrationtest`` and MatrixOne ``test/distributed/cases`` are SQL
scripts replayed against a server. This module turns every row-returning
statement of a script into a candidate whose setup is the earlier state-changing
statements of the same script, so the recorder can capture MySQL's answer in a
fresh database. Expected results from the source are never used; only its
ordering hints (``--sorted_result``, ``-- @sortkey``) are kept.

To keep replay cheap, the setup of each candidate is a *dependency slice* of
the script: the earlier statements that touch the objects the candidate reads,
transitively, restricted to the generation of each object (``DROP TABLE t``
followed by ``CREATE TABLE t`` starts a new generation, so earlier statements
about the old ``t`` are not replayed). User variables and prepared statements
are sliced the same way; the ``SET`` of a system variable is replayed when it
is active for a replayed statement, and transaction control is replayed around
replayed statements. Each database is a separate namespace, so a slice never
mixes databases and can run in the recorder's single fresh database. Setup
statements that fail in MySQL are dropped by the recorder
(``setup_errors: drop``) because vendor-specific statements are mixed into
these scripts.

Statements that are vendor-specific, depend on server state, or are
nondeterministic are filtered with counted reasons; candidates whose slice
depends on an object created or modified by a filtered statement, or by
another session, are dropped rather than replayed against a different state.

Source files are read as text only; nothing in a source tree is executed.
"""

import bisect
import collections
import dataclasses
import pathlib
import re

import importer_common as common

# ---------------------------------------------------------------------------
# Parsed statements
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class Statement:
    sql: str
    line: int
    expect_error: bool = False
    sorted_hint: bool = False
    masked: str = ""  # the source masks or does not check this statement's output
    skip: str = ""  # statement runs outside the replayed session/database
    filtered: str = ""  # vendor-specific or server-state statement
    kind: str = ""  # query, warnings, setup, session, use, noop, dropdb
    db: str = ""
    index: int = 0
    mentions: frozenset = frozenset()
    needs: tuple = ()
    ordinal_on_line: int = 1


@dataclasses.dataclass
class Budget:
    max_setup: int = 120
    max_candidates_per_file: int = 1000
    max_setup_per_file: int = 20_000


# ---------------------------------------------------------------------------
# Parsers for the two script dialects
# ---------------------------------------------------------------------------


def _find_delimiter(text, delimiter):
    """Index of the first complete-statement delimiter, or -1."""
    position = 0
    length = len(text)
    while position < length:
        if text.startswith(delimiter, position):
            return position
        match = common.TOKEN_RE.match(text, position)
        if match is None:
            return -1  # unterminated quote or comment: statement continues
        if match.lastgroup in ("string", "qident", "comment", "hint", "exec"):
            position = match.end()
            continue
        found = text.find(delimiter, position + 1, match.end() + len(delimiter) - 1)
        position = found if found >= 0 else match.end()
    return -1


class ScriptParser:
    """Accumulates SQL lines into statements; subclasses handle directives."""

    def __init__(self, report):
        self.report = report
        self.statements = []
        self.delimiter = ";"
        self.pending = {}
        self.region = ""  # non-empty while statements run in another session
        self.buffer = ""
        self.buffer_line = 0

    def directive(self, stripped, line):
        """Handle a non-SQL line at a statement boundary; True if consumed."""
        return False

    def parse(self, text):
        lines = text.split("\n")
        for number, raw in enumerate(lines, 1):
            line = raw.rstrip("\r")
            if not self.buffer.strip():
                self.buffer = ""
                stripped = line.strip()
                if not stripped:
                    continue
                if self.directive(stripped, number):
                    continue
                self.buffer_line = number
            self.buffer += line + "\n"
            self._drain()
        if self.buffer.strip():
            self.report.count("statements without a final delimiter")
            self._emit(self.buffer.strip(), self.buffer_line)
            self.buffer = ""
        return self.statements

    def _drain(self):
        while True:
            end = _find_delimiter(self.buffer, self.delimiter)
            if end < 0:
                return
            piece = self.buffer[:end]
            rest = self.buffer[end + len(self.delimiter) :]
            leading = len(piece) - len(piece.lstrip())
            line = self.buffer_line + piece.count("\n", 0, leading)
            if piece.strip():
                self._emit(piece.strip(), line)
            self.buffer_line += piece.count("\n") + self.delimiter.count("\n")
            self.buffer = rest if rest.strip() else ""
            if self.buffer:
                skipped = len(rest) - len(rest.lstrip())
                self.buffer_line += rest.count("\n", 0, skipped)
                self.buffer = rest.lstrip()

    def _emit(self, sql, line):
        statement = Statement(sql=sql, line=line)
        statement.expect_error = bool(self.pending.pop("error", False))
        statement.sorted_hint = bool(self.pending.pop("sorted", False))
        statement.masked = self.pending.pop("masked", "")
        statement.skip = self.region
        if self.statements and self.statements[-1].line == line:
            statement.ordinal_on_line = self.statements[-1].ordinal_on_line + 1
        self.statements.append(statement)


MYSQLTEST_COMMANDS = re.compile(
    r"^(connect|connection|disconnect|sleep|real_sleep|let|eval|echo|source|if|while|end|inc|dec|exec|"
    r"send|reap|send_eval|query_vertical|query_horizontal|vertical_results|horizontal_results|"
    r"remove_file|write_file|append_file|cat_file|copy_file|perl|die|exit|skip|require|"
    r"character_set|disable_\w+|enable_\w+|replace_\w+|sorted_result|error|result_format|"
    r"save_master_pos|sync_with_master|wait_for_slave_to_stop|change_user|shutdown_server|"
    r"lowercase_result|query_get_value|expr|assert)\b",
    re.I,
)


class MysqltestParser(ScriptParser):
    """mysqltest ``.test`` files as used by TiDB's integration tests."""

    def __init__(self, report):
        super().__init__(report)
        self.connection = "default"
        self.result_log = True

    def directive(self, stripped, line):
        if stripped.startswith("#"):
            return True
        if stripped.startswith("--"):
            self._command(stripped[2:].strip().rstrip(";").strip())
            return True
        lowered = stripped.lower()
        if lowered.startswith("delimiter"):
            parts = stripped.split(None, 1)
            value = parts[1].strip() if len(parts) > 1 else ";"
            if value != self.delimiter and value.endswith(self.delimiter):
                value = value[: -len(self.delimiter)]
            self.delimiter = value or ";"
            self.report.count("delimiter changes")
            return True
        if lowered in ("}", "};") or MYSQLTEST_COMMANDS.match(lowered):
            self._command(stripped.rstrip(";").strip())
            return True
        return False

    def _command(self, command):
        match = re.match(r"([A-Za-z_]+)\s*(.*)$", command, re.S)
        if not match:
            self.report.count("mysqltest command (other)")
            return
        name = match.group(1).lower()
        argument = match.group(2).strip().rstrip(";").strip()
        if name == "connect":
            connection = argument.lstrip("(").split(",", 1)[0].strip()
            self.connection = connection
            self.region = f"connection {connection}"
            self.report.count("mysqltest connect")
        elif name == "connection":
            self.connection = argument
            self.region = "" if argument == "default" else f"connection {argument}"
        elif name == "disconnect":
            if argument == self.connection:
                self.connection = ""
                self.region = "connection (disconnected)"
        elif name == "error":
            self.pending["error"] = True
        elif name == "sorted_result":
            self.pending["sorted"] = True
        elif name.startswith("replace_") or name in ("query_vertical", "lowercase_result"):
            self.pending["masked"] = f"--{name}"
        elif name == "disable_result_log":
            self.result_log = False
        elif name == "enable_result_log":
            self.result_log = True
        else:
            self.report.count(f"mysqltest command {name}")

    def _emit(self, sql, line):
        if not self.result_log and "masked" not in self.pending:
            self.pending["masked"] = "--disable_result_log"
        super()._emit(sql, line)


class MoTesterParser(ScriptParser):
    """MatrixOne mo-tester ``.sql``/``.test`` files with ``-- @`` directives."""

    def __init__(self, report):
        super().__init__(report)
        self.bvt_block = False

    def directive(self, stripped, line):
        lowered = stripped.lower()
        if stripped.startswith("-- @") or stripped.startswith("--@"):
            self._directive(stripped.split("@", 1)[1].strip())
            return True
        if stripped.startswith("--") or stripped.startswith("#"):
            return True
        if lowered.startswith("delimiter"):
            parts = stripped.split(None, 1)
            value = parts[1].strip() if len(parts) > 1 else ";"
            self.delimiter = value or ";"
            self.report.count("delimiter changes")
            return True
        return False

    def _directive(self, text):
        lowered = text.lower()
        match = re.match(r"[a-z_]+", lowered)
        name = match.group() if match else lowered
        if name == "session":
            if lowered.startswith("session:"):
                self.region = "mo session " + text.split(":", 1)[1].split("&", 1)[0].rstrip("{")
            else:
                self.region = ""
        elif name == "bvt":
            self.bvt_block = ":issue#" in lowered
            if self.bvt_block:
                self.report.count("mo @bvt:issue blocks")
        elif name in ("ignore", "regex", "wait", "wait_expect", "metacmp", "meta_cmp", "binary_value", "floating"):
            self.pending["masked"] = f"@{name}"
        elif name == "sortkey":
            self.pending["sorted"] = True
        elif name == "pattern":
            self.pending["error"] = True
        elif name == "skip":
            self.report.count("mo @skip markers (statements kept)")
        else:
            self.report.count(f"mo directive @{name}")

    def _emit(self, sql, line):
        if self.bvt_block:
            self.report.count("statements inside mo @bvt:issue blocks (kept)")
        super()._emit(sql, line)


# ---------------------------------------------------------------------------
# Statement analysis
# ---------------------------------------------------------------------------

# Statement.skip prefixes of statements that run in another session.
SESSION_REGIONS = ("connection", "mo session")

SESSION_KEYWORDS = {"set", "begin", "commit", "rollback", "savepoint", "release", "prepare", "deallocate"}
QUERY_FIRST = {"select", "with", "table", "values", "execute"}
SHOW_ALLOWED = re.compile(
    r"^\s*show\s+(full\s+)?(create\s+(table|view)|columns|fields|index|indexes|keys|tables)\b", re.I
)
WARNINGS = re.compile(r"^\s*show\s+(count\s*\(\s*\*\s*\)\s+)?(warnings|errors)\b", re.I)
DESCRIBE_TABLE = re.compile(r"^\s*(desc|describe|explain)\s+[`\w$.]+\s*(?:[`\w$]+)?\s*$", re.I)

COMMON_FILTERS = [
    (r"^\s*(explain|trace)\b|^\s*desc(ribe)?\s+(select|insert|update|delete|replace|with|format|analyze|table|\()", "explain"),
    (r"^\s*(create|alter|drop|rename)\s+(user|role)\b|^\s*(grant|revoke)\b|^\s*set\s+(password|role|default\s+role)\b", "users and privileges"),
    (r"^\s*(flush|kill|shutdown|restart|purge|reset|install|uninstall|change\s+(master|replication))\b", "server administration"),
    (r"^\s*set\s+(@@)?global\b|^\s*set\s+@@global\.|^\s*set\s+(@@)?persist(_only)?\b|,\s*(@@)?global\s+\w", "global variables"),
    (r"\binformation_schema\b|\bperformance_schema\b|\bmysql\s*\.\s*`?\w|\bsys\s*\.\s*`?\w", "system schema"),
    (r"\b(sleep|get_lock|release_lock|is_free_lock|is_used_lock|release_all_locks|benchmark)\s*\(", "timing or locking function"),
    (
        r"@@(session\.|local\.)?(hostname|version\w*|port|socket|pid_file|datadir|basedir|server_id|server_uuid|"
        r"tmpdir|plugin_dir|log_\w+|general_log\w*|slow_query\w*|innodb_\w+|max_connections|thread_id|"
        r"pseudo_thread_id|timestamp|identity|last_insert_id|warning_count|error_count|rand_seed\d|"
        r"insert_id|hostname|license|protocol_version|character_sets_dir|lc_messages_dir|secure_file_priv)\b",
        "server-specific system variable",
    ),
    (r"\binto\s+(outfile|dumpfile)\b|^\s*load\s+(data|xml)\b|\bload_file\s*\(|\blocal\s+infile\b", "file system"),
    (r"^\s*(lock|unlock)\s+tables?\b|^\s*(lock|unlock)\s+instance\b", "table locks"),
    (r"^\s*(xa|handler)\b", "xa or handler"),
    (r"^\s*(analyze|optimize|repair|check|checksum)\s+(no_write_to_binlog\s+|local\s+)?tables?\b", "table maintenance"),
    (
        r"^\s*show\s+(?!(full\s+)?(create\s+(table|view)|columns|fields|index|indexes|keys|tables|warnings|errors|count))",
        "server-state show",
    ),
    (r"\bshow\s+(full\s+)?tables\s+(from|in)\b", "server-state show"),
    (r"^\s*(create|drop|alter)\s+(database|schema)\b", "other database"),
    (r"^\s*(create|alter|drop)\s+(event|server|tablespace|logfile|spatial\s+reference|resource\s+group)\b", "server object"),
]

TIDB_FILTERS = [
    (r"\btidb\w*|_tidb_rowid\b", "tidb-specific identifier"),
    (r"^\s*plan\s+replayer\b|^\s*admin\b|^\s*split\b|\bsplit\s+(table|region)|^\s*(flashback|recover)\b", "tidb administration"),
    (r"\bbatch\s+(on|limit)\b", "tidb non-transactional dml"),
    (r"\bbindings?\b", "tidb plan binding"),
    (r"\bplacement\b|\bresource[_ ]group\b|\bttl\b|\bttl_\w+", "tidb placement/resource/ttl"),
    (r"\bauto_random\b|\bshard_row_id_bits\b|\bpre_split_regions\b|\bauto_id_cache\b|\bshard_index\b", "tidb table option"),
    (r"\b(non)?clustered\b", "tidb clustered index option"),
    (r"^\s*alter\s+table\s+\S+\s+(no)?cache\b", "tidb cached table"),
    (r"\bstats_\w+|^\s*(load|lock|unlock|drop|refresh)\s+stats\b", "tidb statistics"),
    (r"^\s*(backup|restore|import\s+into|calibrate|cancel|query\s+watch|refresh|distribute|traffic)\b", "tidb administration"),
    (r"^\s*set\s+config\b|\b(begin|start\s+transaction)\s+(pessimistic|optimistic)\b", "tidb session option"),
    (r"^\s*(create|alter|drop)\s+sequence\b|\b(nextval|lastval|setval)\s*\(", "sequence"),
    (r"\btiflash\b|\btikv\b", "tidb tiflash/tikv"),
    (r"\bvector\b|\bvec_\w+\s*\(|\bvecf32\b", "tidb vector"),
    (r"\bglobal\s+(index|binding)\b|\bglobal\s*,\s*\w|\)\s*global\b", "tidb global index"),
    (
        r"\b(approx_count_distinct|approx_percentile|json_memberof|sm3|uuid_timestamp|uuid_version|uuid_v[1-8]|"
        r"translate|encode|decode|vitess_hash|password)\s*\(|\bilike\b",
        "tidb-only function",
    ),
    (r"^\s*(set|select)\b.*@@(session\.)?(sql_select_limit|max_execution_time|innodb_lock_wait_timeout)\b", "session limit"),
    (r"\bas\s+of\s+timestamp\b|\bstale\b|\bread\s+only\s+as\s+of\b", "tidb stale read"),
    (r"^\s*(create|drop|alter)\s+(placement|resource)\b", "tidb placement/resource/ttl"),
    (r"\bpartition\s+by\s+\w+.*\bglobal\b", "tidb global index"),
]

MATRIXONE_FILTERS = [
    (
        r"^\s*(create|drop|alter|show|restore|pause|resume)\s+(or\s+replace\s+)?(if\s+(not\s+)?exists\s+)?"
        r"(snapshots?|pitr|publications?|subscriptions?|stages?|accounts?|tenants?|cdc|connectors?|sources?|"
        r"dynamic\s+table|daemon\s+task|external\s+table|branch|data\s+branch)\b"
        r"|^\s*create\s+(table|database)\s+\S+\s+clone\b|\bdata\s+branch\b|\{\s*(snapshot|mo_ts)\s*=",
        "matrixone administration object",
    ),
    (r"\b(vecf32|vecf64|ivfflat|hnsw|datalink|bitmap_\w+|l2_distance\w*|cosine_\w+|inner_product|l1_norm|l2_norm|normalize_l2|vector_dims|subvector|summation)\b", "matrixone vector/datalink"),
    (r"\bmo_\w+|\bmo_catalog\b|\bsystem_metrics\b|\bsystem\s*\.\s*\w|\bmo_debug\b|\benable_fault_injection\b|\bfault_point\b", "matrixone system object"),
    (r"\bcluster\s+by\b|\bsample\s*\(|\binterval\s*\(\s*\w+\s*,\s*\d+\s*,|\bsliding\s*\(|\bfill\s*\(", "matrixone query extension"),
    (
        r"\b(serial|serial_full|serial_extract|starts_with|ends_with|startswith|endswith|hash_value|prefix_eq|"
        r"prefix_in|prefix_between|split_part|to_upper|to_lower|date_trunc|unix_timestamp_ms|purge_log|"
        r"fulltext_match|trim_right|trim_left|to_date|to_char|mo_ctl|bit_cast|enable_fault_injection|"
        r"disable_fault_injection|add_fault_point|remove_fault_point|in_range|median|unnest|jq|try_jq|sinh|cosh|"
        r"tanh|asinh|acosh|atanh|approx_percentile|approx_count_distinct|uuid_v[1-8]|uuid_extract_timestamp|"
        r"s2_\w+|h3_\w+|hll_\w+|sql_tvf_\w+|generate_series|percentile_disc|percentile_cont|empty|encode|"
        r"decode|llm_\w+|vecf16|vecbf16|vecint8|vecuint8|cube|rollup|try_moplugin|moplugin)\s*\(|\bminus\b",
        "matrixone-only function",
    ),
    (r"^\s*(create|alter|drop)\s+(sequence|function\s+\w+\s*\(.*\)\s*returns\s+\w+\s+language\s+python|stage|source|connector|task)\b|\b(nextval|lastval|setval)\s*\(", "matrixone sequence/udf/stream"),
    (r"\bsave_query_result\b|\bresult_scan\s*\(|\bmeta_scan\s*\(|\blast_query_id\s*\(", "matrixone query result cache"),
    (r"\bstatement_info\b|\bstatement_id\b|\bstatement_query_type\b|\bsql_source_type\b", "matrixone statement metadata"),
    (r"\bgit\b|\bkafka\b|\bs3option\b|\bjsonline\b|\bparquet\b|\bcsv\b\s*\(", "matrixone external data"),
    (r"^\s*(set|select)\b.*\b(enable_privilege_cache|experimental_\w+|save_query_result|query_result_\w+|transaction_operator_open_log|keep_user_target_list_in_result)\b", "matrixone session variable"),
    (r"^\s*(prepare|execute|deallocate)\b.*\busing\b.*\bvector\b", "matrixone vector/datalink"),
    (r"^\s*(begin|start\s+transaction)\s+(pessimistic|optimistic)\b", "matrixone session option"),
    (r"^\s*create\s+.*\bpartition\s+by\s+\w+.*\bpartitions?\s+\d+\b.*\bcluster\b", "matrixone query extension"),
]

# Checked for every statement except CREATE/ALTER, where DEFAULT CURRENT_TIMESTAMP
# and ON UPDATE CURRENT_TIMESTAMP only describe the schema.
NONDETERMINISTIC = re.compile(
    r"\b(now|sysdate|curdate|curtime|utc_date|utc_time|utc_timestamp|uuid|uuid_short|connection_id|"
    r"database|schema|user|current_user|session_user|system_user|version|found_rows|row_count|"
    r"current_role|icu_version|ps_current_thread_id|unix_timestamp)\s*\(\s*\)|\brand\s*\(\s*\)|"
    r"\b(current_date|current_time|current_timestamp|localtime|localtimestamp|current_user)\b(?!\s*\()",
    re.I,
)

SYSTEM_SCHEMAS = {"information_schema", "mysql", "performance_schema", "sys", "mo_catalog", "metrics_schema", "system", "system_metrics", "mo_task", "mo_debug"}


def compile_filters(filters):
    return [(re.compile(pattern, re.I | re.S), reason) for pattern, reason in filters]


def mask_strings(sql):
    """SQL without comments and with every string literal replaced by ``''``."""
    pieces = []
    for token in common.tokenize(sql):
        if token.kind == "comment":
            pieces.append(" ")
        elif token.kind == "string":
            pieces.append("''")
        else:
            pieces.append(token.text)
    return "".join(pieces).strip()


# Filtered statements with these reasons do not change data as MySQL would see
# it (storage hints, statistics, plans), so they do not invalidate later reads.
NO_DATA_EFFECT = {
    "explain",
    "table maintenance",
    "server-state show",
    "tidb cached table",
    "tidb placement/resource/ttl",
    "tidb table option",
    "tidb tiflash/tikv",
    "tidb statistics",
    "tidb administration",
    "tidb plan binding",
}


def filter_reason(statement_text, filters):
    lowered = statement_text
    for pattern, reason in filters:
        if pattern.search(lowered):
            return reason
    return ""


def is_select_into_variable(tokens):
    depth = 0
    for index, token in enumerate(tokens):
        if token.text == "(":
            depth += 1
        elif token.text == ")":
            depth -= 1
        elif depth == 0 and token.kind == "ident" and token.text.lower() == "into":
            following = tokens[index + 1] if index + 1 < len(tokens) else None
            if following is not None and following.kind == "var":
                return True
    return False


def statement_kind(sql, tokens):
    first = common.first_keyword(sql)
    if WARNINGS.match(sql):
        return "warnings"
    if first in QUERY_FIRST or (not first and tokens and tokens[0].text == "("):
        if first == "select" and is_select_into_variable(tokens):
            return "session"
        if first in ("with", "select", "table", "values", "") or first == "execute":
            return "query"
    if first == "show":
        return "query" if SHOW_ALLOWED.match(sql) else "setup"
    if first in ("desc", "describe", "explain") and DESCRIBE_TABLE.match(sql):
        return "query"
    if first == "set" and re.match(r"^\s*set\s+(session\s+)?transaction\b", sql, re.I):
        return "session"
    if first in SESSION_KEYWORDS:
        return "session"
    if first == "start" and re.match(r"^\s*start\s+transaction\b", sql, re.I):
        return "session"
    if first == "use":
        return "use"
    return "setup"


_CREATE_OBJECT = re.compile(
    r"^\s*create\s+(?:or\s+replace\s+)?(?:temporary\s+)?(?:algorithm\s*=\s*\w+\s+)?(?:definer\s*=\s*\S+\s+)?"
    r"(?:sql\s+security\s+\w+\s+)?(?:table|view|trigger|procedure|function|event)\s+(?:if\s+not\s+exists\s+)?"
    r"((?:`[^`]+`|[\w$]+)(?:\s*\.\s*(?:`[^`]+`|[\w$]+))?)",
    re.I,
)
_DROP_OBJECT = re.compile(
    r"^\s*drop\s+(?:temporary\s+)?(?:table|tables|view|trigger|procedure|function|event)\s+(?:if\s+exists\s+)?(.*)$",
    re.I | re.S,
)
_RENAME_TABLE = re.compile(r"^\s*rename\s+tables?\s+(.*)$", re.I | re.S)
_ALTER_RENAME = re.compile(
    r"^\s*alter\s+table\s+((?:`[^`]+`|[\w$]+))\b.*?\brename\s+(?:to\s+|as\s+)?(?!column\b|index\b|key\b)((?:`[^`]+`|[\w$]+))",
    re.I | re.S,
)


_LIVE_OBJECT = re.compile(
    r"^\s*create\s+(?:or\s+replace\s+)?(?:algorithm\s*=\s*\w+\s+)?(?:definer\s*=\s*\S+\s+)?"
    r"(?:sql\s+security\s+\w+\s+)?(view|trigger|procedure|function)\b",
    re.I,
)
_TRIGGER_TABLE = re.compile(r"\b(?:before|after)\s+(?:insert|update|delete)\s+on\s+((?:`[^`]+`|[\w$]+)(?:\.(?:`[^`]+`|[\w$]+))?)", re.I)
TRANSACTION = re.compile(r"^\s*(begin|start\s+transaction|commit|rollback|savepoint|release\s+savepoint)\b", re.I)
MODIFYING = {"insert", "replace", "update", "delete", "alter", "truncate", "create", "rename", "load", "call"}


_DATABASE_NAME = re.compile(
    r"^\s*(?:(create|drop)\s+(?:database|schema)|use)\s+(?:if\s+(?:not\s+)?exists\s+)?(`[^`]+`|[\w$]+)\s*$",
    re.I,
)


def _key(database, name):
    return f"{database}\x00{name}"


def _name(text):
    """Unqualified, unquoted, lowercase object name from ``db.name`` text."""
    parts = re.findall(r"`((?:[^`]|``)*)`|([^`.\s]+)", text.strip())
    if not parts:
        return text.strip().lower()
    quoted, bare = parts[-1]
    return (quoted.replace("``", "`") if quoted or not bare else bare).lower()


def _qualifier(text):
    """Lowercase database qualifier of ``db.name`` text, or None."""
    parts = re.findall(r"`((?:[^`]|``)*)`|([^`.\s]+)", text.strip())
    if len(parts) < 2:
        return None
    quoted, bare = parts[-2]
    return (quoted.replace("``", "`") if quoted or not bare else bare).lower()


def _object(op, text):
    return (op, _qualifier(text), _name(text))


def object_effects(sql):
    """Return ``(op, database_or_None, name)`` effects of a statement on named objects."""
    match = _CREATE_OBJECT.match(sql)
    if match:
        return [_object("create", match.group(1))]
    match = _DROP_OBJECT.match(sql)
    if match:
        body = re.split(r"\b(restrict|cascade)\b", match.group(1), flags=re.I)[0]
        return [_object("drop", piece) for piece in body.split(",") if piece.strip()]
    match = _RENAME_TABLE.match(sql)
    if match:
        effects = []
        for pair in match.group(1).split(","):
            parts = re.split(r"\s+to\s+", pair.strip(), flags=re.I)
            if len(parts) == 2:
                effects.append(_object("drop", parts[0]))
                effects.append(_object("create", parts[1]))
        return effects
    match = _ALTER_RENAME.match(sql)
    if match:
        return [_object("drop", match.group(1)), _object("create", match.group(2))]
    return []


_NAME = r"((?:`[^`]+`|[\w$]+)(?:\s*\.\s*(?:`[^`]+`|[\w$]+))?)"
_TARGETS = [
    re.compile(r"^\s*(?:insert|replace)\s+(?:(?:low_priority|delayed|high_priority|ignore)\s+)*(?:into\s+)?" + _NAME, re.I),
    re.compile(r"^\s*update\s+(?:(?:low_priority|ignore)\s+)*" + _NAME, re.I),
    re.compile(r"^\s*delete\s+.*?\bfrom\s+" + _NAME, re.I | re.S),
    re.compile(r"^\s*alter\s+(?:(?:online|ignore)\s+)?table\s+" + _NAME, re.I),
    re.compile(r"^\s*truncate\s+(?:table\s+)?" + _NAME, re.I),
    re.compile(r"\binto\s+table\s+" + _NAME, re.I),
]


def modified_targets(sql):
    """``(database_or_None, name)`` of the table a DML/ALTER statement modifies, if known."""
    for pattern in _TARGETS:
        match = pattern.search(sql)
        if match:
            return [(_qualifier(match.group(1)), _name(match.group(1)))]
    return []


def identifier_words(tokens):
    words = set()
    for token in tokens:
        if token.kind == "ident":
            words.add(token.text.lower())
        elif token.kind == "qident":
            words.add(token.text[1:-1].replace("``", "`").lower())
    return words


def strip_qualifier(sql, database):
    """Remove ``database.`` qualifiers for the replayed database."""
    if not database:
        return sql
    tokens = list(common.tokenize(sql))
    out = []
    index = 0
    target = database.lower()
    while index < len(tokens):
        token = tokens[index]
        name = None
        if token.kind == "ident":
            name = token.text.lower()
        elif token.kind == "qident":
            name = token.text[1:-1].lower()
        if (
            name == target
            and index + 2 < len(tokens)
            and tokens[index + 1].text == "."
            and tokens[index + 2].kind in ("ident", "qident")
            and (index == 0 or tokens[index - 1].text != ".")
        ):
            index += 2
            continue
        out.append(token.text)
        index += 1
    return "".join(out)


def qualified_databases(tokens):
    """Names used as ``name.something`` qualifiers."""
    names = set()
    for index in range(len(tokens) - 2):
        token = tokens[index]
        if token.kind in ("ident", "qident") and tokens[index + 1].text == "." and tokens[index + 2].kind in ("ident", "qident", "op"):
            if index > 0 and tokens[index - 1].text == ".":
                continue
            name = token.text.lower() if token.kind == "ident" else token.text[1:-1].lower()
            names.add(name)
    return names


# ---------------------------------------------------------------------------
# File processing
# ---------------------------------------------------------------------------


class Profile:
    """Source-specific configuration for the replay importer."""

    name = ""
    repo = ""
    revision = ""
    license = ""
    root = ""
    patterns = ()
    excluded_dirs = {}
    excluded_files = {}
    filters = ()
    parser_class = MysqltestParser

    rewrite_note = ""
    # Objects created by skipped statements (another session, or statements a
    # static reading cannot place) are tracked, so reads of them are tainted.
    track_skipped_objects = False

    def own_database(self, relative):
        return None

    def rewrite(self, sql):
        """Source-specific syntax normalization that does not change MySQL semantics."""
        return sql

    def finish_report(self, report):
        """Add source-specific tables to the import report."""

    def report_directory(self, relative):
        """The directory a file's candidates are counted under in the import report."""
        return relative.split("/", 1)[0] if "/" in relative else "."

    def exclusion(self, relative):
        """Why a file (path relative to ``root``) is not imported, ``""``, or None to ignore it uncounted."""
        top = relative.split("/", 1)[0] if "/" in relative else "."
        excluded = self.excluded_dirs.get(top) or self.excluded_files.get(relative)
        if not excluded:
            for prefix, reason in self.excluded_dirs.items():
                if relative.startswith(prefix + "/"):
                    return reason
        return excluded or ""

    def read_statements(self, path, relative, report):
        """Parse one file into statements; ``None`` when the file turns out to be excluded."""
        data = path.read_bytes()
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            # Statements with invalid bytes keep surrogates and are filtered later.
            text = data.decode("utf-8", errors="surrogateescape")
            report.count("files with invalid UTF-8")
        return self.parser_class(report).parse(text)

    def group(self, relative):
        return f"{self.name}/{relative}"

    def candidate_id(self, relative, statement):
        stem = relative.rsplit(".", 1)[0]
        suffix = f".{statement.ordinal_on_line}" if statement.ordinal_on_line > 1 else ""
        return f"{self.name}/{stem}:L{statement.line}{suffix}"


@dataclasses.dataclass
class PrepareContext:
    current: str
    databases: set
    known: dict


class StatementPreparer:
    """Classifies statements and resolves database namespaces and object mentions."""

    def __init__(self, profile, report):
        self.profile = profile
        self.report = report
        self.filters = compile_filters(COMMON_FILTERS) + compile_filters(profile.filters)

    def prepare(self, statements, initial_db="", context=None, start_index=0):
        """Resolve statements in script order; returns the context after the last one.

        ``context`` continues from an earlier ``prepare`` call (for example a
        shared fixture) without re-reading its statements.
        """
        for index, statement in enumerate(statements):
            statement.index = start_index + index
        # Databases named by the script. Each database is a separate namespace; a
        # candidate replays only statements of the database it runs in, so the
        # replay can flatten everything into the recorder's single database.
        if context is None:
            databases = {initial_db} if initial_db else set()
            current = initial_db
            known = collections.defaultdict(set)
        else:
            databases = set(context.databases)
            current = context.current
            known = collections.defaultdict(set, {db: set(names) for db, names in context.known.items()})
        for statement in statements:
            match = _DATABASE_NAME.match(statement.sql)
            if match:
                databases.add(match.group(2).strip("`").lower())
        for statement in statements:
            self._resolve(statement, current, databases)
            if statement.kind == "use" and not statement.skip:
                current = statement.db
        for statement in statements:
            if statement.kind == "setup" and (not statement.skip or self.profile.track_skipped_objects):
                for _op, qualifier, name in object_effects(statement.sql):
                    known[qualifier or statement.db].add(name)
        for statement in statements:
            if statement.kind in ("use", "noop", "dropdb"):
                continue
            tokens = common.significant_tokens(statement.sql)
            words = identifier_words(tokens)
            if common.first_keyword(statement.sql) == "prepare":
                for token in tokens:
                    if token.kind == "string":
                        words |= identifier_words(common.significant_tokens(token.text[1:-1]))
            statement.mentions = frozenset(_key(statement.db, name) for name in words & known[statement.db])
        return PrepareContext(current, databases, known)

    def _resolve(self, statement, current, databases):
        """Classify a statement and resolve it against the current database."""
        statement.db = current
        sql = statement.sql
        tokens = common.significant_tokens(sql)
        statement.kind = statement_kind(sql, tokens)
        if statement.kind == "use":
            match = re.match(r"^\s*use\s+(`[^`]+`|[\w$]+)", sql, re.I)
            statement.db = match.group(1).strip("`").lower() if match else current
            return
        match = _DATABASE_NAME.match(sql)
        if match and match.group(1):
            statement.db = match.group(2).strip("`").lower()
            statement.kind = "dropdb" if match.group(1).lower() == "drop" else "noop"
            return
        stripped_sql = strip_qualifier(sql, current)
        if stripped_sql != sql:
            self.report.count("statements with the current database qualifier removed")
            statement.sql = stripped_sql
            tokens = common.significant_tokens(stripped_sql)
        rewritten = self.profile.rewrite(statement.sql)
        if rewritten != statement.sql:
            self.report.count(f"statements rewritten ({self.profile.rewrite_note})")
            statement.sql = rewritten
            tokens = common.significant_tokens(rewritten)
        # Filters look at SQL outside comments and string literals.
        masked = mask_strings(statement.sql)
        reason = filter_reason(masked, self.filters)
        if not reason and common.first_keyword(statement.sql) not in ("create", "alter") and NONDETERMINISTIC.search(masked):
            reason = "nondeterministic or session-dependent function"
        if not reason and current in SYSTEM_SCHEMAS:
            reason = "system schema"
        if not reason:
            qualifiers = qualified_databases(tokens)
            if qualifiers & SYSTEM_SCHEMAS:
                reason = "system schema"
            elif qualifiers & (databases - {current}):
                reason = "cross-database reference"
        if not reason and not common.is_utf8_text(statement.sql):
            reason = "invalid UTF-8"
        statement.filtered = reason


class Replayer:
    """Tracks object generations of a script and computes setup slices.

    A DROP followed by a CREATE of the same name starts a new generation, so later
    candidates do not replay the old object's history. User variables and prepared
    statements are objects too: every assignment or PREPARE starts a new
    generation. The SET of a system variable is replayed when it is active for a
    replayed statement. Transaction control is replayed around replayed
    statements only.
    """

    def __init__(self, report, dispositions, uses_autocommit=False, track_skipped_objects=False):
        self.report = report
        self.track_skipped_objects = track_skipped_objects
        self.dispositions = dispositions
        self.alive = {}
        self.generation = collections.Counter()
        self.members = collections.defaultdict(list)
        self.member_indexes = collections.defaultdict(list)  # parallel to members, increasing
        self.ended_by = {}
        self.tainted = {}
        self.live_dependencies = collections.defaultdict(set)
        self.system_sets = collections.defaultdict(list)
        self.transaction_statements = []
        self.uses_autocommit = uses_autocommit

    def current(self, name):
        return (name, self.generation[name])

    def new_session(self):
        """Forget session state: user variables, prepared statements, SETs, transactions."""
        for name in list(self.alive):
            if name.startswith(("\x01", "\x02")) and self.alive[name]:
                self.ended_by[self.current(name)] = None
                self.alive[name] = False
                self.generation[name] += 1
        self.system_sets.clear()
        self.transaction_statements.clear()

    def process(self, statement, persist=True):
        """Analyze one statement in script order.

        With ``persist`` false the statement is analyzed against the current
        state without changing it (used for independent tests over a shared
        fixture). Returns False when the statement is outside the replay.
        """
        kind = statement.kind
        alive = self.alive
        if kind in ("use", "noop", "dropdb"):
            self.dispositions["database statement (virtualized)"] += 1
        if kind in ("use", "noop"):
            return False
        if kind == "dropdb":
            if statement.skip or not persist:
                return False
            prefix = _key(statement.db, "")
            for name in [name for name, is_alive in alive.items() if is_alive and name.startswith(prefix)]:
                self.ended_by[self.current(name)] = None
                alive[name] = False
            for name in [name for name in self.live_dependencies if name.startswith(prefix)]:
                del self.live_dependencies[name]
            return False
        tokens = common.significant_tokens(statement.sql)
        session = session_analysis(statement.sql, tokens, kind)
        effects = (
            [(op, _key(qualifier or statement.db, name)) for op, qualifier, name in object_effects(statement.sql)]
            if kind == "setup"
            else []
        )
        mentions = set(statement.mentions) | {_user_variable(name) for name in session.reads}
        if session.execute:
            mentions.add(_prepared(session.execute))
        # Views, routines, triggers and prepared statements read other objects
        # whenever they are used.
        frontier = list(mentions)
        while frontier:
            name = frontier.pop()
            for dependency in self.live_dependencies.get(name, ()):
                if dependency not in mentions:
                    mentions.add(dependency)
                    frontier.append(dependency)
        if statement.skip:
            if statement.skip.startswith(SESSION_REGIONS):
                label = f"runs in another session ({statement.skip.rsplit(' ', 1)[0]})"
                taint = "modified outside the replayed session"
            else:
                # Statements a static reading cannot place (an undecidable
                # mysqltest condition, unresolved variables).
                label = f"not statically replayable ({statement.skip})"
                taint = "modified by a statement that is not statically replayable"
            if kind in ("query", "warnings"):
                self.report.filter(label)
            else:
                self.dispositions[f"skipped: {label}"] += 1
                if persist:
                    for name in mentions:
                        self.tainted.setdefault(self.current(name), taint)
                    if self.track_skipped_objects:
                        # An object such a statement may create exists with a
                        # tainted generation, so later reads are dropped.
                        for op, name in effects:
                            if op == "create" and not alive.get(name, False):
                                self.generation[name] += 1
                                alive[name] = True
                            self.tainted.setdefault(self.current(name), taint)
            return False
        if not persist:
            # Only reads are analyzed; definitions would change shared state.
            statement.needs = tuple(self.current(name) for name in sorted(mentions) if alive.get(name, False))
            return True
        created = {name for op, name in effects if op == "create"}
        dropped = {name for op, name in effects if op == "drop"}
        # A name only refers to an object that exists when the statement runs;
        # otherwise it is a column or alias that happens to share the name.
        mentions = {name for name in mentions if alive.get(name, False)}
        needs = []
        for name in sorted(mentions | created | dropped):
            if name in created and not alive.get(name, False):
                self.generation[name] += 1
                alive[name] = True
                if statement.filtered:
                    self.tainted.setdefault(self.current(name), f"created by a filtered statement ({statement.filtered})")
            needs.append(self.current(name))
        defined = [_user_variable(name) for name in session.user_targets]
        if session.prepare:
            defined.append(_prepared(session.prepare))
        for name in defined:
            self.generation[name] += 1
            alive[name] = True
            if statement.filtered:
                self.tainted.setdefault(self.current(name), f"created by a filtered statement ({statement.filtered})")
            needs.append(self.current(name))
            if session.prepare and name == _prepared(session.prepare):
                self.live_dependencies[name] = set(statement.mentions)
        if session.deallocate:
            name = _prepared(session.deallocate)
            needs.append(self.current(name))
            dropped.add(name)
        statement.needs = tuple(needs)
        if created and not statement.filtered:
            live = _LIVE_OBJECT.match(statement.sql)
            if live:
                for name in created:
                    self.live_dependencies[name] = set(mentions) - {name}
                trigger_table = _TRIGGER_TABLE.search(statement.sql)
                if live.group(1).lower() == "trigger" and trigger_table:
                    self.live_dependencies[_key(statement.db, _name(trigger_table.group(1)))] |= set(mentions)
        if kind not in ("query", "warnings"):
            self.dispositions[f"filtered: {statement.filtered}" if statement.filtered else f"replayable {kind}"] += 1
        if statement.filtered:
            if (
                statement.filtered not in NO_DATA_EFFECT
                and kind not in ("query", "warnings")
                and common.first_keyword(statement.sql) in MODIFYING
            ):
                targets = modified_targets(statement.sql)
                if effects:
                    keys = [self.current(name) for _op, name in effects]
                elif targets:
                    keys = [self.current(_key(qualifier or statement.db, name)) for qualifier, name in targets]
                else:
                    keys = needs
                for key in keys:
                    self.tainted.setdefault(key, f"modified by a filtered statement ({statement.filtered})")
        elif kind in ("setup", "session"):
            for key in needs:
                self.members[key].append(statement)
                self.member_indexes[key].append(statement.index)
            for variable in session.system_targets:
                self.system_sets[variable].append(statement)
            if TRANSACTION.match(statement.sql):
                self.transaction_statements.append(statement)
        for name in dropped:
            if alive.get(name, False):
                self.ended_by[self.current(name)] = statement
                alive[name] = False
                self.live_dependencies.pop(name, None)
        return True

    def slice(self, anchors):
        """Return ``(setup_statements, taint_reason)`` replaying everything ``anchors`` need."""
        included = {}
        needed_keys = set()
        anchor_indexes = {anchor.index for anchor in anchors}
        # Every member of a key before position processed[key] is included,
        # so only the members after it need a look (keeps slices linear).
        processed = {}

        def close(work):
            while work:
                statement = work.pop()
                for key in statement.needs:
                    if key in self.tainted:
                        return self.tainted[key]
                    needed_keys.add(key)
                    start = processed.get(key, 0)
                    end = bisect.bisect_left(self.member_indexes.get(key, ()), statement.index)
                    if end <= start:
                        continue
                    for member in self.members[key][start:end]:
                        if member.index not in included:
                            included[member.index] = member
                            work.append(member)
                    processed[key] = end
            return ""

        taint = close(list(anchors))
        while not taint:
            # The SET of a system variable that is active when a replayed
            # statement runs is replayed as well.
            added = []
            for statement in list(anchors) + list(included.values()):
                if statement.index not in anchor_indexes and statement.kind == "session":
                    continue  # a SET does not depend on earlier SETs
                for sets in self.system_sets.values():
                    position = bisect.bisect_left([s.index for s in sets], statement.index) - 1
                    if position >= 0:
                        active = sets[position]
                        if active.index not in included and active.index not in anchor_indexes:
                            included[active.index] = active
                            added.append(active)
            if not added:
                break
            taint = close(added)
        if taint:
            return [], taint
        last = max(anchor_indexes)
        # When several generations of an object are replayed, replay the drops
        # that separate them so the later CREATE succeeds.
        by_name = collections.defaultdict(list)
        for name, gen in needed_keys:
            by_name[name].append(gen)
        for name, gens in by_name.items():
            for gen in sorted(gens)[:-1]:
                drop = self.ended_by.get((name, gen))
                if drop is not None and drop.index < last and not drop.filtered:
                    included.setdefault(drop.index, drop)
        for index in anchor_indexes:
            included.pop(index, None)
        add_transactions(included, min(anchor_indexes), self.transaction_statements, self.uses_autocommit)
        return [included[index] for index in sorted(included)], ""


def add_transactions(included, anchor_index, transaction_statements, uses_autocommit):
    """Replay transaction control around replayed statements only."""
    earlier = [statement for statement in transaction_statements if statement.index < anchor_index]
    if uses_autocommit:
        for statement in earlier:
            included[statement.index] = statement
        return
    spans = []
    span = None
    for statement in earlier:
        keyword = common.first_keyword(statement.sql)
        if keyword in ("begin", "start"):
            if span:
                spans.append((span, statement.index))
            span = [statement]
        elif span is not None:
            span.append(statement)
            if keyword in ("commit", "rollback") and not re.search(r"\bto\b", statement.sql, re.I):
                spans.append((span, statement.index))
                span = None
    if span:
        spans.append((span, anchor_index + 1))
    indexes = sorted(included)
    for span, end in spans:
        start = span[0].index
        if any(start < index < end for index in indexes) or start < anchor_index < end:
            for statement in span:
                included[statement.index] = statement


def candidate_ordered(statement):
    first = common.first_keyword(statement.sql)
    if statement.kind == "warnings" or first in ("show", "desc", "describe", "explain"):
        return True
    return common.has_top_level_order_by(statement.sql) and not statement.sorted_hint


class FileImporter:
    def __init__(self, profile, budget, report):
        self.profile = profile
        self.budget = budget
        self.report = report
        self.preparer = StatementPreparer(profile, report)
        self.setup_hist = collections.Counter()
        self.dispositions = collections.Counter()

    def import_file(self, path, relative):
        report = self.report
        statements = self.profile.read_statements(path, relative, report)
        if statements is None:
            return []
        report.count("statements parsed", len(statements))
        self.preparer.prepare(statements, self.profile.own_database(relative) or "")
        return self._candidates(statements, relative)

    def _candidates(self, statements, relative):
        report = self.report
        uses_autocommit = any(
            statement.kind == "session" and re.search(r"\bautocommit\b", statement.sql, re.I) for statement in statements
        )
        replayer = Replayer(report, self.dispositions, uses_autocommit, self.profile.track_skipped_objects)
        candidates = []
        file_setup = 0
        for statement in statements:
            if not replayer.process(statement):
                continue
            kind = statement.kind
            if not (kind in ("query", "warnings") or (kind == "setup" and statement.expect_error and not statement.filtered)):
                continue
            candidate = self._candidate(statement, statements, replayer, relative)
            if candidate is None:
                continue
            if len(candidates) >= self.budget.max_candidates_per_file:
                report.filter("per-file candidate cap")
                continue
            if file_setup + len(candidate["setup"]) > self.budget.max_setup_per_file:
                report.filter("per-file setup budget")
                continue
            file_setup += len(candidate["setup"])
            candidates.append(candidate)
        return candidates

    def _candidate(self, statement, statements, replayer, relative):
        report = self.report
        if statement.filtered:
            report.filter(statement.filtered)
            return None
        if statement.masked:
            report.filter(f"source masks or skips the output ({statement.masked})")
            return None
        sql = [statement.sql]
        anchor = statement
        if statement.kind == "warnings":
            previous = statements[statement.index - 1] if statement.index else None
            if (
                previous is None
                or previous.skip
                or previous.filtered
                or previous.masked
                or previous.kind not in ("query", "setup", "session")
            ):
                report.filter("warnings check without an eligible preceding statement")
                return None
            sql = [previous.sql, statement.sql]
            anchor = previous
        setup, taint = replayer.slice([anchor])
        if taint:
            report.filter(f"depends on an object {taint.split(' (')[0]}")
            return None
        if len(setup) > self.budget.max_setup:
            report.filter("setup longer than the per-candidate cap")
            return None
        self.setup_hist[min(len(setup) // 10 * 10, 200)] += 1
        if statement.kind == "setup":
            report.count("statement candidates (source expects an error)")
        return common.make_candidate(
            candidate_id=self.profile.candidate_id(relative, statement),
            repo=self.profile.repo,
            revision=self.profile.revision,
            path=f"{self.profile.root}/{relative}",
            line=statement.line,
            license=self.profile.license,
            group=self.profile.group(relative),
            setup=[s.sql for s in setup],
            sql=sql,
            ordered=candidate_ordered(statement),
            setup_errors="drop",
        )


@dataclasses.dataclass
class SessionEffects:
    reads: set
    user_targets: list
    system_targets: list
    prepare: str = ""
    deallocate: str = ""
    execute: str = ""


def _variable_name(text):
    name = text.lstrip("@")
    if name[:1] in "`'\"" and len(name) >= 2:
        name = name[1:-1]
    return name.lower()


def _user_variable(name):
    return "\x01@" + name


def _prepared(name):
    return "\x02" + name


def _split_top_level(tokens, separator=","):
    parts = [[]]
    depth = 0
    for token in tokens:
        if token.text == "(":
            depth += 1
        elif token.text == ")":
            depth -= 1
        if depth == 0 and token.text == separator:
            parts.append([])
            continue
        parts[-1].append(token)
    return [part for part in parts if part]


def session_analysis(sql, tokens, kind):
    """User variables read/assigned, system variables set and prepared statements."""
    effects = SessionEffects(reads=set(), user_targets=[], system_targets=[])
    user_tokens = [token for token in tokens if token.kind == "var" and not token.text.startswith("@@")]
    first = common.first_keyword(sql)
    lowered = [token.text.lower() for token in tokens]
    if kind == "session" and first == "set":
        body = tokens[lowered.index("set") + 1 :]
        if body and body[0].kind == "ident" and body[0].text.lower() == "transaction":
            effects.system_targets.append("transaction")
            return effects
        for part in _split_top_level(body):
            head = part[0]
            if head.kind == "var" and not head.text.startswith("@@") and len(part) > 1 and part[1].text in ("=", ":="):
                effects.user_targets.append(_variable_name(head.text))
                effects.reads |= {_variable_name(t.text) for t in part[2:] if t.kind == "var" and not t.text.startswith("@@")}
                continue
            if head.kind == "var":
                name = head.text.lstrip("@").lower()
                for prefix in ("session.", "local."):
                    name = name.removeprefix(prefix)
            elif head.kind == "ident" and head.text.lower() in ("session", "local") and len(part) > 1:
                name = part[1].text.lower()
            elif head.kind == "ident" and head.text.lower() in ("names", "character", "charset"):
                name = "names"
            else:
                name = head.text.strip("`").lower()
            effects.system_targets.append(name)
            effects.reads |= {_variable_name(t.text) for t in part[1:] if t.kind == "var" and not t.text.startswith("@@")}
        return effects
    if first == "prepare" and len(tokens) > 1:
        effects.prepare = tokens[lowered.index("prepare") + 1].text.strip("`").lower()
        effects.reads = {_variable_name(t.text) for t in user_tokens}
        return effects
    if first in ("deallocate", "drop") and "prepare" in lowered:
        position = lowered.index("prepare") + 1
        if position < len(tokens):
            effects.deallocate = tokens[position].text.strip("`").lower()
        return effects
    if first == "execute" and len(tokens) > 1:
        effects.execute = tokens[lowered.index("execute") + 1].text.strip("`").lower()
    if kind == "session" and first in ("select", "with"):
        # SELECT ... INTO @a, @b assigns user variables.
        depth = 0
        into = None
        for index, token in enumerate(tokens):
            if token.text == "(":
                depth += 1
            elif token.text == ")":
                depth -= 1
            elif depth == 0 and lowered[index] == "into":
                into = index
        if into is not None:
            targets = []
            for token in tokens[into + 1 :]:
                if token.kind == "var":
                    targets.append(token)
                elif token.text != ",":
                    break
            effects.user_targets = [_variable_name(t.text) for t in targets]
            effects.reads = {_variable_name(t.text) for t in user_tokens if t not in targets}
            return effects
    if kind == "session" and TRANSACTION.match(sql):
        return effects
    effects.reads = {_variable_name(t.text) for t in user_tokens}
    return effects


def run_profile(profile, source, output, budget, limit_files=None, write=True):
    source = pathlib.Path(source)
    revision = common.check_revision(source, profile.revision)
    root = source / profile.root
    report = common.Report(
        f"{profile.title} import",
        {
            "repository": profile.repo,
            "revision": revision,
            "path": profile.root,
            "license": profile.license,
            "budget": (
                f"max {budget.max_setup} setup statements per candidate, "
                f"max {budget.max_candidates_per_file} candidates and "
                f"{budget.max_setup_per_file:,} setup statements per file"
            ),
        },
    )
    importer = FileImporter(profile, budget, report)
    files = sorted(path for pattern in profile.patterns for path in root.rglob(pattern))
    candidates = []
    per_dir = collections.Counter()
    seen = set()
    scanned = 0
    for path in files:
        relative = path.relative_to(root).as_posix()
        top = profile.report_directory(relative)
        excluded = profile.exclusion(relative)
        if excluded is None:
            continue  # outside the paths the profile imports
        if excluded:
            report.filter(f"excluded file ({excluded})")
            report.count("files excluded")
            continue
        scanned += 1
        if limit_files is not None and scanned > limit_files:
            break
        for candidate in importer.import_file(path, relative):
            key = common.stable_hash("\x1f".join(candidate["setup"]) + "\x1e" + "\x1f".join(candidate["sql"]))
            if key in seen:
                report.filter("duplicate of an earlier candidate")
                continue
            seen.add(key)
            candidates.append(candidate)
            per_dir[top] += 1
    report.counts = collections.Counter({"files scanned": scanned, **report.counts})
    report.count("candidates emitted", len(candidates))
    setup_total = sum(len(candidate["setup"]) for candidate in candidates)
    report.count("setup statements across candidates (replay budget)", setup_total)
    report.table(
        "Non-query statement dispositions",
        ["disposition", "statements"],
        sorted(importer.dispositions.items(), key=lambda item: (-item[1], item[0])),
    )
    report.table("Candidates by directory", ["directory", "candidates"], sorted(per_dir.items()))
    report.table(
        "Setup length distribution",
        ["setup statements", "candidates"],
        [(f"{bucket}-{bucket + 9}" if bucket < 200 else "200+", count) for bucket, count in sorted(importer.setup_hist.items())],
    )
    report.table("Levels", ["level", "candidates"], common.level_histogram(candidates))
    profile.finish_report(report)
    for note in profile.notes:
        report.note(note)
    report.note(
        "Setup is a dependency slice of the earlier statements of the same file: statements touching the "
        "objects a candidate reads, transitively, within the current generation of each object, plus all "
        "session statements (SET, transaction control, PREPARE). Every candidate uses setup_errors=drop."
    )
    report.note(
        "Queries are SELECT/WITH/TABLE/VALUES/EXECUTE, SHOW CREATE TABLE|VIEW, SHOW COLUMNS|INDEX|TABLES and "
        "DESCRIBE <table>. SHOW WARNINGS is emitted together with the statement it inspects. "
        "Non-query statements become candidates only when the source expects them to fail."
    )
    if write:
        common.write_candidates(output, profile.name, candidates)
        text = report.write(output)
    else:
        text = report.render()
    return candidates, report, text


def add_budget_arguments(parser):
    defaults = Budget()
    parser.add_argument("--max-setup", type=int, default=defaults.max_setup, help="setup statements per candidate")
    parser.add_argument(
        "--max-candidates-per-file", type=int, default=defaults.max_candidates_per_file, help="candidates per file"
    )
    parser.add_argument(
        "--max-setup-per-file", type=int, default=defaults.max_setup_per_file, help="total setup statements per file"
    )


def budget_from(args):
    return Budget(
        max_setup=args.max_setup,
        max_candidates_per_file=args.max_candidates_per_file,
        max_setup_per_file=args.max_setup_per_file,
    )
