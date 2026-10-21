#!/usr/bin/env python3
"""Import MySQL's own mysql-test (MTR) suite as MySQL spec candidates.

The MySQL 8.4.10 ``mysql-test`` scripts are GPL-2.0-only, so their SQL is never
committed: only MySQL's recorded results are (``results/mysql-mtr/``), and
``tools/materialize.py`` re-runs this importer on the pinned checkout to rebuild
the specs locally (see SPECS.md). MySQL's own ``.result`` files are not used.

The scripts are mysqltest programs. This importer reads them statically, the
way mysqltest would run them against the default server: it follows
``--source`` includes, ``let``/``eval`` variables with statically known values,
``if``/``while`` blocks whose conditions it can decide, ``--error``,
``--sorted_result`` and the output-masking commands. Statements it cannot place
(an undecidable condition, an unresolved variable, a query in a variable) are
kept as *not statically replayable*: candidates never depend on objects they
touch. Whole files are excluded, with counted reasons, when they need what a
fresh database on the shared oracle cannot provide: other connections, shell
or perl commands, sleeps, server restarts, debug builds, replication, plugins
or components, server option files, or a runtime requirement check
(``--skip`` under an undecidable condition) that the pinned profile is not
known to satisfy. Candidates are then the file_replay dependency slices
(``tools/file_replay.py``). Source files are read as text only.
"""

import argparse
import bisect
import collections
import dataclasses
import pathlib
import re
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import file_replay  # noqa: E402
import importer_common as common  # noqa: E402

# Suites of mysql-test/suite/ whose tests are SQL-level; only these, t/ and
# include/ are fetched and imported. The others test replication, NDB,
# plugins and components, storage-engine internals, server variables, system
# schemas, client programs or need special server setups.
SUITES = {
    "collations": "collations",
    "engines": "storage-engine independent SQL (funcs, iuds)",
    "funcs_1": "functions, views, triggers, stored procedures",
    "funcs_2": "character sets",
    "gcol": "generated columns",
    "gis": "spatial functions",
    "innodb_fts": "full-text search",
    "innodb_gis": "spatial indexes",
    "json": "JSON",
    "parts": "partitioning",
}
# mysqltest commands (mysqltest.cc command_names). A line or delimited command
# whose first word is one of these is a mysqltest command, not SQL.
COMMANDS = frozenset(
    """
    append_file assert cat_file change_user character_set chmod connect connection copy_file
    copy_files_wildcard dec delimiter diff_files die dirty_close disable_abort_on_error
    disable_async_client disable_connect_log disable_info disable_metadata disable_ps_protocol
    disable_query_log disable_reconnect disable_result_log disable_session_track_info
    disable_testcase disable_warnings disconnect echo enable_abort_on_error enable_async_client
    enable_connect_log enable_info enable_metadata enable_ps_protocol enable_query_log
    enable_reconnect enable_result_log enable_session_track_info enable_testcase enable_warnings
    end end_timer error eval exec exec_in_background execw exit expr file_exists force-cpdir
    force-rmdir horizontal_results if inc let list_files list_files_append_file
    list_files_write_file lowercase_result mkdir move_file output partially_sorted_result perl
    ping query query_attributes query_horizontal query_vertical real_sleep reap
    remove_file remove_files_wildcard replace_column replace_numeric_round replace_regex
    replace_result require reset_connection result_format rmdir save_master_pos send send_eval
    send_quit send_shutdown shutdown_server skip skip_if_hypergraph sleep sorted_result source
    start_timer sync_slave_with_master sync_with_master system vertical_results
    wait_for_slave_to_stop while write_file
    """.split()
)
HEREDOC_COMMANDS = {"perl", "write_file", "append_file"}

# Commands that make a file unusable wherever they appear (even in a block
# whose condition cannot be decided).
EXCLUDING_COMMANDS = {
    "connect": "multiple connections",
    "change_user": "changes the session user",
    "reset_connection": "resets the session",
    "exec": "runs shell commands",
    "execw": "runs shell commands",
    "exec_in_background": "runs shell commands",
    "system": "runs shell commands",
    "perl": "runs perl",
    "shutdown_server": "restarts the server",
    "send_shutdown": "restarts the server",
    "send_quit": "closes connections",
    "dirty_close": "closes connections",
    "save_master_pos": "replication",
    "sync_with_master": "replication",
    "sync_slave_with_master": "replication",
    "wait_for_slave_to_stop": "replication",
}

# Includes (paths relative to mysql-test/) that make a file unusable.
EXCLUDING_INCLUDES = [
    (r"debug", "needs a debug build"),
    (r"(^|/)rpl/|master-slave|replica|replication|gtid|binlog|have_log_bin", "replication or binary log"),
    (r"ndb", "NDB cluster"),
    (r"plugin|component|keyring|have_udf|have_example|have_mecab|xplugin|mysqlx", "plugins or components"),
    (r"restart|shutdown|start_mysqld|kill_|crash|reconnect|wait_until_(dis)?connected", "restarts the server"),
    (r"have_hypergraph|valgrind_only|have_valgrind|have_asan|have_ubsan|have_tsan|have_lowercase[12]|"
     r"have_case_insensitive|have_innodb_(4|8|32|64)k|have_ssl|have_openssl|have_profiling|have_32bit|"
     r"have_big5|have_cp932|have_eucjpms|have_gb2312|have_gbk|have_sjis|have_ujis|have_mysql_upgrade|"
     r"have_util_|have_grep|have_perl|have_symlink|have_no_symlink|have_.*_ssl|have_tlsv",
     "needs a different build or server configuration"),
    (r"wait_condition|wait_for_|show_rpl", "waits for asynchronous server state"),
]

# Includes that only check a requirement the pinned oracle meets (or only set
# output masks); they are not inlined. Any other `--skip` under an undecidable
# condition excludes its file.
IGNORED_INCLUDES = {
    "include/big_test.inc",
    "include/count_sessions.inc",
    "include/elide_costs.inc",
    "include/have_64bit.inc",
    "include/have_archive.inc",
    "include/have_blackhole.inc",
    "include/have_case_sensitive_file_system.inc",
    "include/have_innodb.inc",
    "include/have_innodb_16k.inc",
    "include/have_innodb_max_16k.inc",
    "include/have_innodb_min_8k.inc",
    "include/have_little_endian.inc",
    "include/have_lowercase0.inc",
    "include/have_myisam.inc",
    "include/have_nodebug.inc",
    "include/have_partition.inc",
    "include/no_protocol.inc",
    "include/no_ps_protocol.inc",
    "include/no_valgrind_without_big.inc",
    "include/not_asan.inc",
    "include/not_asan_windows.inc",
    "include/not_hypergraph.inc",
    "include/not_parallel.inc",
    "include/not_sparc_debug.inc",
    "include/not_threadpool.inc",
    "include/not_tsan.inc",
    "include/not_ubsan.inc",
    "include/not_valgrind.inc",
    "include/not_windows.inc",
    "include/wait_until_count_sessions.inc",
}

# mysqltest environment of a default MTR run, as far as conditions use it.
ENVIRONMENT = {"BIG_TEST": "1", "VALGRIND_TEST": "0", "HYPERGRAPH_TEST": "0"}

# Server option files whose options all leave SQL-visible behavior of a single
# session unchanged are allowed; any other option excludes the test.
HARMLESS_OPTIONS = re.compile(
    r"^--(loose[-_])?(skip[-_])?(log[-_]bin\S*|log[-_]replica[-_]updates|slow[-_]query[-_]log\S*|"
    r"general[-_]log\S*|log[-_]output\S*|long[-_]query[-_]time\S*|log[-_]queries[-_]not[-_]using[-_]indexes|"
    r"secure[-_]file[-_]priv\S*|local[-_]infile\S*|replica[-_]preserve[-_]commit[-_]order|"
    r"innodb[-_]flush[-_]log[-_]at[-_]trx[-_]commit\S*|sync[-_]binlog\S*)$",
    re.I,
)

# Variables mysqltest sets after every statement; never known statically.
RUNTIME_VARIABLES = {"mysql_errno", "mysql_errname"}

MAX_INCLUDE_DEPTH = 16
MAX_LOOP_ITERATIONS = 20_000
# Larger values make a variable unknown; more SQL text excludes the file
# (scripts that build ever longer statements in loops).
MAX_VARIABLE_LENGTH = 1 << 20
MAX_FILE_SQL = 64 << 20


class FileExcluded(Exception):
    """A file cannot be replayed; ``detail`` (an include path) is reported separately."""

    def __init__(self, reason, detail=""):
        super().__init__(reason)
        self.reason = reason
        self.detail = detail


@dataclasses.dataclass
class Item:
    kind: str  # sql | command | if | while | else | end
    line: int
    name: str = ""
    text: str = ""


@dataclasses.dataclass
class Block:
    kind: str  # if | while
    line: int
    condition: str
    body: list = dataclasses.field(default_factory=list)
    otherwise: list = dataclasses.field(default_factory=list)


# ---------------------------------------------------------------------------
# Reading mysqltest scripts
# ---------------------------------------------------------------------------


def _split_word(text):
    match = re.match(r"([A-Za-z_][A-Za-z0-9_-]*)(.*)$", text, re.S)
    if not match:
        return "", text
    return match.group(1).lower(), match.group(2)


def _split_condition(text):
    """``if (cond) {`` -> (cond, rest after the closing parenthesis)."""
    start = text.index("(")
    depth = 0
    quote = None
    for index in range(start, len(text)):
        char = text[index]
        if quote:
            if char == quote:
                quote = None
            continue
        if char in "`'\"":
            quote = char
        elif char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth == 0:
                return text[start + 1 : index].strip(), text[index + 1 :].strip()
    return text[start + 1 :].strip(), ""


_SPACE = re.compile(r"\s*")
_ELSE = re.compile(r"[ \t]*else\b[ \t]*\{?", re.I)
_QUOTE_END = {quote: re.compile(f"[{quote}\\\\]") for quote in "'\"`"}


def _scan(text, position, delimiter, pattern):
    """mysqltest ``read_line`` from ``position``: (end, resume, brace).

    The command ends at the delimiter outside '...', "..." and `...` (a
    backslash escapes inside them), or at ``{`` for ``if``/``while``/``else``.
    Comments are not special: mysqltest itself ends a statement at a
    delimiter inside ``/* */``.
    """
    index = position
    length = len(text)
    while True:
        match = pattern.search(text, index)
        if match is None:
            return length, length, False
        found = match.start()
        token = match.group()
        if token == delimiter:
            return found, found + len(delimiter), False
        if token == "{":
            head = text[position:found].lower()
            if re.match(r"(if|while)\s*\(|else\b", head):
                return found, found + 1, True
            index = found + 1
            continue
        quote_end = _QUOTE_END[token]
        index = found + 1
        while True:
            closing = quote_end.search(text, index)
            if closing is None:
                return length, length, False
            if closing.group() == "\\":
                index = closing.end() + 1
                continue
            index = closing.end()
            break


def read_script(text):
    """Split mysqltest text into items the way mysqltest's ``read_line`` does.

    At the start of a command, ``#`` and ``-`` lines are comments unless they
    are ``--command`` lines (which end at the line end); ``}`` (optionally
    followed by ``else {``) closes a block. Anything else is a command or a
    statement that ends at the current delimiter outside quotes, or at ``{``
    for ``if``/``while``. Heredoc bodies of ``perl``, ``write_file`` and
    ``append_file`` are skipped.
    """
    items = []
    length = len(text)
    line_starts = [0] + [match.end() for match in re.finditer("\n", text)]

    def line_of(position):
        return bisect.bisect_right(line_starts, position)

    def line_end(position):
        found = text.find("\n", position)
        return length if found < 0 else found

    def skip_heredoc(position, name, argument):
        parts = argument.strip().rstrip(";").split()
        if name == "perl":
            terminator = parts[0] if parts else "EOF"
        else:
            terminator = parts[1] if len(parts) > 1 else "EOF"
        position = line_end(position) + 1
        while position < length:
            end = line_end(position)
            if text[position:end].strip() == terminator:
                return end + 1
            position = end + 1
        return length

    delimiter = ";"
    patterns = {}
    position = 0
    while True:
        position = _SPACE.match(text, position).end()
        if position >= length:
            break
        char = text[position]
        line = line_of(position)
        if char in "#-":
            end = line_end(position)
            content = text[position:end].strip()
            position = end
            if not content.startswith("--"):
                continue
            name, argument = _split_word(content[2:].strip())
            if name not in COMMANDS:
                continue  # a comment
            if name == "delimiter":
                delimiter = argument.strip() or ";"
            elif name == "end":
                items.append(Item("end", line))
            else:
                items.append(Item("command", line, name, argument.strip()))
                if name in HEREDOC_COMMANDS:
                    position = skip_heredoc(position, name, argument)
            continue
        if char == "}":
            items.append(Item("end", line))
            position += 1
            match = _ELSE.match(text, position)
            if match:
                items.append(Item("else", line))
                position = match.end()
            continue
        if char == "{":
            position += 1
            continue
        if delimiter not in patterns:
            patterns[delimiter] = re.compile(f"{re.escape(delimiter)}|['\"`{{]")
        end, resume, brace = _scan(text, position, delimiter, patterns[delimiter])
        piece = text[position:end].strip()
        position = resume
        name, argument = _split_word(piece)
        if brace or name in ("if", "while", "else"):
            if name == "else":
                items.append(Item("else", line))
            elif "(" in piece:
                condition, _rest = _split_condition(piece)
                items.append(Item(name, line, text=condition))
            continue
        if name == "delimiter":
            delimiter = argument.strip() or ";"
            continue
        if not piece:
            continue
        items.append(_classify(piece, line))
        if name in HEREDOC_COMMANDS:
            position = skip_heredoc(position - 1, name, argument)
    return items


def _classify(text, line):
    name, argument = _split_word(text)
    if name == "end":
        return Item("end", line)
    if name in COMMANDS:
        return Item("command", line, name, argument.strip())
    return Item("sql", line, text=text)


def build_blocks(items):
    """Nest items into ``Block``s; raises FileExcluded when unbalanced."""
    root = []
    stack = [root]
    for item in items:
        if item.kind in ("if", "while"):
            block = Block(item.kind, item.line, item.text)
            stack[-1].append(block)
            stack.append(block.body)
        elif item.kind == "else":
            previous = stack[-1][-1] if stack[-1] else None
            if not isinstance(previous, Block) or previous.kind != "if":
                raise FileExcluded("unbalanced mysqltest blocks")
            stack.append(previous.otherwise)
        elif item.kind == "end":
            if len(stack) == 1:
                raise FileExcluded("unbalanced mysqltest blocks")
            stack.pop()
        else:
            stack[-1].append(item)
    if len(stack) != 1:
        raise FileExcluded("unbalanced mysqltest blocks")
    return root


# ---------------------------------------------------------------------------
# Static evaluation of mysqltest variables and conditions
# ---------------------------------------------------------------------------

_VARIABLE = re.compile(r"[A-Za-z0-9_]+")
_ASSIGNS = re.compile(r"\binto\b|:=|\bfor\s+(update|share)\b|\block\s+in\b", re.I)
_INTEGER = re.compile(r"[+-]?\d+")


def lookup(variables, name):
    """A mysqltest variable's value, or None when it is not known statically.

    Like mysqltest, a variable that was never set reads as the environment
    variable of that name or as ""; MTR's environment (all upper case, such as
    $MYSQLTEST_VARDIR) is unknown except for ENVIRONMENT.
    """
    if name in variables:
        return variables[name]
    if name.isupper() or name in RUNTIME_VARIABLES:
        return None
    return ""


def expand(text, variables):
    """mysqltest ``do_eval``: returns (text, known). Unknown variables leave ``$name``."""
    out = []
    known = True
    escaped = False
    index = 0
    length = len(text)
    while index < length:
        char = text[index]
        if char == "$" and not escaped:
            match = _VARIABLE.match(text, index + 1)
            if match:
                value = lookup(variables, match.group())
                if value is None:
                    known = False
                    out.append(text[index : match.end()])
                else:
                    out.append(value)
                index = match.end()
                continue
            out.append(char)
        elif char == "\\" and not escaped and index + 1 < length and text[index + 1] in "\\$\"":
            escaped = True
            index += 1
            continue
        else:
            out.append(char)
        escaped = False
        index += 1
    return "".join(out), known


def truth(value):
    value = value.strip()
    if _INTEGER.fullmatch(value):
        return int(value) != 0
    return value != ""


def evaluate_condition(condition, variables):
    """True/False for a decidable ``if``/``while`` condition, else None."""
    text = condition.strip()
    if not text or "`" in text:
        return None
    negate = text.startswith("!")
    if negate:
        text = text[1:].strip()
    match = re.match(r"(.*?)\s*(==|!=|<=|>=|<|>)\s*(.*)$", text, re.S)
    if match:
        if negate:
            return None
        left, known_left = expand(match.group(1), variables)
        right, known_right = expand(match.group(3), variables)
        if not (known_left and known_right):
            return None
        left, right, operator = left.strip(), right.strip(), match.group(2)
        if operator == "==":
            return left == right
        if operator == "!=":
            return left != right
        if not (_INTEGER.fullmatch(left) and _INTEGER.fullmatch(right)):
            return None
        left, right = int(left), int(right)
        return {"<": left < right, "<=": left <= right, ">": left > right, ">=": left >= right}[operator]
    value, known = expand(text, variables)
    if not known:
        return None
    return truth(value) != negate


# ---------------------------------------------------------------------------
# The static interpreter
# ---------------------------------------------------------------------------


def sparse_paths():
    """The mysql-test paths corpus/sources.lock.toml fetches for this importer."""
    return ["mysql-test/t", "mysql-test/include"] + [f"mysql-test/suite/{suite}" for suite in sorted(SUITES)]


def fetched_path(relative):
    """Whether a path relative to mysql-test/ is inside sparse_paths()."""
    return any(f"mysql-test/{relative}".startswith(prefix + "/") for prefix in sparse_paths())


def include_policy(relative):
    """("exclude", reason), ("ignore", None) or ("inline", None) for an include path."""
    if relative in IGNORED_INCLUDES:
        return "ignore", None
    for pattern, reason in EXCLUDING_INCLUDES:
        if re.search(pattern, relative, re.I):
            return "exclude", reason
    return "inline", None


class Interpreter:
    """Runs one test file statically and collects its statements."""

    def __init__(self, base, report, cache):
        self.base = base.resolve()
        self.report = report
        self.cache = cache
        self.variables = dict(ENVIRONMENT)
        self.statements = []
        self.pending = {}
        self.result_log = True
        self.testcase = True
        self.ordinals = collections.Counter()
        self.iterations = 0
        self.sql_length = 0
        self.stopped = False
        self.stack = []

    def load(self, path):
        key = str(path)
        if key not in self.cache:
            data = path.read_bytes()
            try:
                text = data.decode("utf-8")
            except UnicodeDecodeError:
                text = data.decode("utf-8", errors="surrogateescape")
            try:
                self.cache[key] = build_blocks(read_script(text))
            except FileExcluded as error:
                self.cache[key] = error
        tree = self.cache[key]
        if isinstance(tree, FileExcluded):
            raise FileExcluded(tree.reason, self.relative(path))
        return tree

    def relative(self, path):
        return path.relative_to(self.base).as_posix()

    def run(self, path):
        path = path.resolve()
        self.stack.append(path)
        self.execute(self.load(path), None, None)
        self.stack.pop()
        return self.statements

    def execute(self, nodes, dynamic, anchor):
        for node in nodes:
            if self.stopped:
                return
            if isinstance(node, Block):
                self.block(node, dynamic, anchor)
            elif node.kind == "sql":
                self.emit(node.text, node.line, dynamic, anchor)
            else:
                self.command(node, dynamic, anchor)

    def block(self, block, dynamic, anchor):
        value = evaluate_condition(block.condition, self.variables)
        if block.kind == "if":
            if value is None or dynamic:
                region = dynamic or "undecidable condition"
                if value is not False:
                    self.execute(block.body, region, anchor)
                if value is not True:
                    self.execute(block.otherwise, region, anchor)
            else:
                self.execute(block.body if value else block.otherwise, None, anchor)
            return
        loop_anchor = anchor or block.line
        if dynamic or value is None:
            if value is not False:
                self.execute(block.body, dynamic or "undecidable loop condition", loop_anchor)
            return
        while value:
            self.iterations += 1
            if self.iterations > MAX_LOOP_ITERATIONS:
                raise FileExcluded(f"loops over {MAX_LOOP_ITERATIONS} iterations")
            self.execute(block.body, None, loop_anchor)
            if self.stopped:
                return
            value = evaluate_condition(block.condition, self.variables)
            if value is None:
                self.execute(block.body, "undecidable loop condition", loop_anchor)
                return

    def emit(self, sql, line, dynamic, anchor):
        pending, self.pending = self.pending, {}
        if not self.testcase:
            return
        self.sql_length += len(sql)
        if self.sql_length > MAX_FILE_SQL:
            raise FileExcluded(f"expands to more than {MAX_FILE_SQL >> 20} MiB of SQL")
        statement = file_replay.Statement(sql=sql, line=anchor or line)
        statement.expect_error = bool(pending.get("error"))
        statement.sorted_hint = bool(pending.get("sorted"))
        statement.masked = pending.get("masked", "" if self.result_log else "--disable_result_log")
        statement.skip = dynamic or ""
        self.ordinals[statement.line] += 1
        statement.ordinal_on_line = self.ordinals[statement.line]
        self.statements.append(statement)

    def command(self, item, dynamic, anchor):
        name, argument = item.name, item.text
        if name in EXCLUDING_COMMANDS:
            raise FileExcluded(EXCLUDING_COMMANDS[name])
        if name == "let":
            self.let(argument, item.line, dynamic, anchor)
        elif name in ("inc", "dec"):
            match = re.match(r"\$(\w+)", argument.strip())
            if match:
                value = lookup(self.variables, match.group(1))
                if dynamic or value is None or not _INTEGER.fullmatch(value.strip()):
                    self.variables[match.group(1)] = None
                else:
                    self.variables[match.group(1)] = str(int(value) + (1 if name == "inc" else -1))
        elif name == "expr":
            self.expr(argument, dynamic)
        elif name in ("eval", "send_eval"):
            text, known = expand(argument, self.variables)
            if not known:
                self.report.count("mysqltest statements with unresolved variables")
            self.emit(text, item.line, dynamic or (None if known else "unresolved variable"), anchor)
        elif name in ("query", "query_vertical", "query_horizontal", "send"):
            # Without other connections, `send` + `reap` runs the statement
            # synchronously; a bare `send` applies to the next statement.
            if argument:
                self.emit(argument, item.line, dynamic, anchor)
        elif name == "reap":
            if self.pending.pop("error", False) and self.statements:
                self.statements[-1].expect_error = True
        elif name == "source":
            self.source(argument, item.line, dynamic, anchor)
        elif name == "error":
            self.pending["error"] = True
        elif name in ("sorted_result", "partially_sorted_result"):
            self.pending["sorted"] = True
        elif name.startswith("replace_") or name == "lowercase_result":
            self.pending["masked"] = f"--{name}"
        elif name == "disable_result_log":
            self.result_log = False
        elif name == "enable_result_log":
            self.result_log = True
        elif name == "disable_testcase":
            if not dynamic:
                self.testcase = False
        elif name == "enable_testcase":
            self.testcase = True
        elif name == "connection":
            if argument.strip().strip("`") != "default":
                raise FileExcluded("multiple connections")
        elif name in ("sleep", "real_sleep"):
            if not dynamic:
                raise FileExcluded("sleeps")
        elif name == "skip":
            if not dynamic:
                raise FileExcluded("skipped by the test itself")
            if len(self.stack) == 1:
                raise FileExcluded("runtime requirement check in the test")
            raise FileExcluded("runtime requirement check in an include", self.relative(self.stack[-1]))
        elif name == "die":
            if not dynamic:
                raise FileExcluded("dies")
        elif name == "exit":
            if not dynamic:
                self.stopped = True
        else:
            self.report.count(f"mysqltest command {name}")

    def let(self, argument, line, dynamic, anchor):
        match = re.match(r"\$?([A-Za-z0-9_]+)\s*=(.*)$", argument, re.S)
        if not match:
            return
        name, value = match.group(1), match.group(2).strip()
        if value.startswith("`"):
            # mysqltest runs the query and keeps its value; a read-only query
            # is an ordinary statement, any other cannot be placed statically.
            inner, known = expand(value.strip("`"), self.variables)
            inner = inner.strip()
            if inner:
                read_only = common.first_keyword(inner) in ("select", "show", "with", "values", "table") and not \
                    _ASSIGNS.search(file_replay.mask_strings(inner))
                region = None if known and read_only else "query in a mysqltest variable"
                # --error applies to this query; result masks and sorting to
                # the next displayed result.
                kept = {key: self.pending.pop(key) for key in ("sorted", "masked") if key in self.pending}
                self.emit(inner, line, dynamic or region, anchor)
                self.pending.update(kept)
            self.variables[name] = None
            return
        if value.lower().startswith("query_get_value("):
            self.variables[name] = None
            return
        text, known = expand(value, self.variables)
        self.variables[name] = text if known and not dynamic and len(text) <= MAX_VARIABLE_LENGTH else None

    def expr(self, argument, dynamic):
        match = re.match(r"\$(\w+)\s*=\s*(.*)$", argument, re.S)
        if not match:
            return
        name = match.group(1)
        text, known = expand(match.group(2), self.variables)
        parts = re.fullmatch(r"\s*([+-]?\d+)\s*([-+*/%])\s*([+-]?\d+)\s*", text) if known else None
        if dynamic or not parts:
            self.variables[name] = None
            return
        left, operator, right = int(parts.group(1)), parts.group(2), int(parts.group(3))
        if operator in "/%" and right == 0:
            self.variables[name] = None
            return
        value = {"+": left + right, "-": left - right, "*": left * right,
                 "/": int(left / right), "%": left % right if left >= 0 else -(-left % right)}[operator]
        self.variables[name] = str(value)

    def source(self, argument, line, dynamic, anchor):
        # The file name is the first word; a `#` comment may follow it.
        name, known = expand(argument.split("#", 1)[0].strip().rstrip(";").strip(), self.variables)
        name = name.split()[0] if name.split() else ""
        if not known or not name:
            raise FileExcluded("sources a file named by an unresolved variable")
        # mysqltest looks next to the current file first, then in mysql-test/.
        path = None
        for candidate in (self.stack[-1].parent / name, self.base / name):
            try:
                resolved = candidate.resolve()
                resolved.relative_to(self.base)
            except (OSError, ValueError):
                continue
            if resolved.is_file():
                path = resolved
                break
        relative = path.relative_to(self.base).as_posix() if path else name
        policy, reason = include_policy(relative)
        if policy == "inline" and path is not None and not fetched_path(relative):
            # Only paths fetch-sources.sh checks out, so a sparse checkout
            # imports exactly what a full one does.
            raise FileExcluded("sources a file outside the selected suites", relative)
        if path is None and policy == "inline":
            raise FileExcluded("sources a missing file", name)
        if policy == "exclude":
            raise FileExcluded(reason, relative)
        if policy == "ignore":
            self.report.count("requirement includes met by the oracle")
            return
        if len(self.stack) > MAX_INCLUDE_DEPTH or path in self.stack:
            raise FileExcluded("recursive includes")
        self.report.count("includes inlined")
        self.stack.append(path)
        try:
            self.execute(self.load(path), dynamic, anchor or line)
        finally:
            self.stack.pop()


# ---------------------------------------------------------------------------
# Profile
# ---------------------------------------------------------------------------


class MtrProfile(file_replay.Profile):
    name = "mysql-mtr"
    title = "MySQL mysql-test (MTR)"
    repo = "https://github.com/mysql/mysql-server.git"
    revision = "6adc159923b7b6abbe649949551ec25264c2daf9"
    license = "GPL-2.0-only"
    root = "mysql-test"
    patterns = ("*.test",)
    filters = ()
    track_skipped_objects = True
    notes = (
        "Files are mysqltest programs read statically (see tools/import_mtr.py): `--source` includes are "
        "inlined, `let`/`eval`/`inc`/`dec`/`expr` with known values are evaluated, and `if`/`while` blocks run "
        "when their conditions are decidable (loops are unrolled). Statements in undecidable blocks, with "
        "unresolved variables or in mysqltest variable queries are not replayed, and candidates never depend "
        "on objects they touch.",
        "Candidate ids use the line of the top-level command (`--source`, `while`) plus an ordinal for "
        "statements it produces; provenance names the test file.",
        "Statements under --replace_*, --lowercase_result or --disable_result_log are not emitted as "
        "candidates because MTR masks their output.",
        "Each test runs in database `test`; its qualifier is removed.",
        "Only mysql-test/t and the suites " + ", ".join(sorted(SUITES)) + " are imported; other suites test "
        "replication, NDB, plugins and components, storage-engine internals, server variables or system "
        "schemas.",
    )

    def __init__(self):
        self.include_cache = {}
        self.exclusion_details = collections.Counter()

    def own_database(self, relative):
        return "test"

    def report_directory(self, relative):
        parts = relative.split("/")
        return "/".join(parts[:2]) if parts[0] == "suite" else parts[0]

    def exclusion(self, relative):
        if not fetched_path(relative):
            return None  # not fetched by fetch-sources.sh: ignored, not counted
        parts = relative.split("/")
        if (parts[0] == "t" and len(parts) == 2) or (parts[0] == "suite" and "t" in parts[2:-1]):
            return ""
        return "not a test directory"

    def read_statements(self, path, relative, report):
        reason = option_file_problem(path)
        if not reason:
            base = path.parents[len(relative.split("/")) - 1]
            try:
                return Interpreter(base, report, self.include_cache).run(path)
            except FileExcluded as error:
                reason = error.reason
                if error.detail:
                    self.exclusion_details[f"{error.reason}: {error.detail}"] += 1
        report.filter(f"excluded file ({reason})")
        report.count("files excluded")
        return None

    def finish_report(self, report):
        report.table(
            "Files excluded by includes (top 40)",
            ["reason: include", "files"],
            sorted(self.exclusion_details.items(), key=lambda item: (-item[1], item[0]))[:40],
        )


def option_file_problem(path):
    """Why a test's server option or setup files make it unusable, or ``""``."""
    stem = path.name.removesuffix(".test")
    for suffix in ("-master.sh", "-slave.opt", "-slave.sh", ".cnf", "-master.cnf"):
        if (path.parent / f"{stem}{suffix}").exists():
            return "server setup files"
    options = []
    for name in (f"{stem}-master.opt", f"{stem}.opt", "suite.opt"):
        option_path = path.parent / name
        if option_path.exists():
            options += option_path.read_text(errors="replace").split()
    for option in options:
        if not HARMLESS_OPTIONS.match(option):
            return "server options"
    return ""


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--source", required=True, type=pathlib.Path, help="mysql-server checkout")
    parser.add_argument(
        "--output",
        type=pathlib.Path,
        default=pathlib.Path(__file__).resolve().parents[1] / "candidates" / "mysql-mtr",
    )
    file_replay.add_budget_arguments(parser)
    args = parser.parse_args(argv)
    candidates, _report, text = file_replay.run_profile(
        MtrProfile(), args.source, args.output, file_replay.budget_from(args)
    )
    print(text)
    print(f"wrote {len(candidates):,} candidates to {args.output}")


if __name__ == "__main__":
    main()
