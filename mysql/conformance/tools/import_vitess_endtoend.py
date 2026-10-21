#!/usr/bin/env python3
"""Import Vitess end-to-end MySQL comparison tests as spec candidates.

Vitess (Apache-2.0) runs part of its end-to-end suite against both vtgate and
a plain MySQL server and compares the answers:

* ``go/test/endtoend/vtgate/queries/<package>`` tests call ``mcmp.Exec``,
  ``mcmp.AssertMatches`` and friends on a ``utils.MySQLCompare``; each package
  has a ``schema.sql`` loaded into MySQL first.
* ``go/test/endtoend/vtgate/vitess_tester/**.test`` are mysqltest-format
  scripts run by vitess-tester against both engines.

Each test function (or script) is replayed in order with the shared
file-replay engine: writes become setup, reads and statements expected to
fail become candidates. Only calls that run on MySQL are imported (``mcmp.*``,
not ``utils.Exec(t, mcmp.VtConn, ...)``). Test bodies are read as text: string
arguments, ``for _, x := range <[]string>`` loops and ``fmt.Sprintf`` over
known values are evaluated; anything else is counted as dynamic. Writes that
cannot be evaluated, or that sit in loops that cannot be unrolled, make the
written table unusable for later candidates. Vitess results are ignored.
"""

import argparse
import collections
import pathlib
import re
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import file_replay  # noqa: E402
import go_source  # noqa: E402
import import_vitess  # noqa: E402
import importer_common as common  # noqa: E402

SOURCE_NAME = "vitess-endtoend"
REPO = import_vitess.REPO
REVISION = import_vitess.REVISION
LICENSE = import_vitess.LICENSE
QUERIES = "go/test/endtoend/vtgate/queries"
TESTER = "go/test/endtoend/vtgate/vitess_tester"

# mcmp methods whose first SQL argument runs on MySQL, and how to treat it.
STATEMENT_METHODS = {
    "Exec": "statement",
    "ExecAndIgnore": "statement",
    "ExecWithColumnCompare": "statement",
    "ExecNoCompare": "statement",
    "ExecAllowError": "statement",
    "AssertMatches": "statement",
    "AssertMatchesNoCompare": "statement",
    "AssertMatchesAnyNoCompare": "statement",
    "AssertMatchesNoOrder": "unordered",
    "AssertMatchesNoOrderInclColumnNames": "unordered",
    "AssertIsEmpty": "statement",
    "AssertFoundRowsValue": "statement",
    "ExecAllowAndCompareError": "error",
    "AssertContainsError": "error",
}
MULTI_METHODS = {"ExecMulti", "ExecMultiAllowError"}
IGNORED_METHODS = {"Run", "Close", "AsT", "VExplain"}

VITESS_FILTERS = [
    (r"\bvitess_\w+|\bvindex\b|\bvexplain\b|\bvtexplain\b|\bworkload\b|\bvgtid\w*|\bvschema\b", "vitess-specific syntax"),
    (r"@(primary|replica|rdonly)\b|\bnext\s+\d+\s+values\b|\bddl_strategy\b|\bmigration_context\b", "vitess-specific syntax"),
]


class VitessProfile(file_replay.Profile):
    name = SOURCE_NAME
    title = "Vitess end-to-end MySQL comparisons"
    repo = REPO
    revision = REVISION
    license = LICENSE
    root = TESTER
    patterns = ("*.test",)
    parser_class = file_replay.MysqltestParser
    filters = VITESS_FILTERS
    notes = ()

    def group(self, relative):
        return f"{self.name}/vitess_tester/{relative}"

    def candidate_id(self, relative, statement):
        return super().candidate_id("vitess_tester/" + relative, statement)


class Unknown(Exception):
    """An expression the evaluator cannot resolve statically."""


class BodyWalker:
    """Collects the SQL a Go test function sends through ``mcmp``."""

    def __init__(self, parser, functions, report):
        self.parser = parser
        self.functions = functions
        self.report = report
        self.calls = []  # (sql, line, mode, uncertain)

    def walk(self, start, end, env, in_loop=False, depth=0):
        parser = self.parser
        tokens = parser.tokens
        index = start
        while index < end:
            token = tokens[index]
            if token.kind == "ident" and token.text == "for":
                index = self._loop(index, end, env, in_loop, depth)
                continue
            if (
                token.kind == "ident"
                and parser.is_op(index + 1, ":=")
                and (index == 0 or parser.is_op(index - 1, ";") or parser.is_op(index - 1, "{"))
            ):
                node, after = parser.parse_value(index + 2, (";",))
                try:
                    env[token.text] = self.evaluate(node, env)
                except Unknown:
                    env.pop(token.text, None)
                index += 2
                continue
            if (
                token.kind == "ident"
                and token.text == "mcmp"
                and parser.is_op(index + 1, ".")
                and index + 3 < len(tokens)
                and parser.is_op(index + 3, "(")
            ):
                self._call(tokens[index + 2].text, index + 2, env, in_loop)
                index += 3
                continue
            if (
                token.kind == "ident"
                and token.text in self.functions
                and parser.is_op(index + 1, "(")
                and depth < 3
                and not (index > 0 and tokens[index - 1].text in ("func", "."))
            ):
                arguments = parser.call_arguments(go_source.Node("expr", None, token.line, token_index=index))
                if any(argument.text == "mcmp" or argument.text.startswith("&mcmp") for argument in arguments):
                    open_index, close_index, _line = self.functions[token.text]
                    self.walk(open_index + 1, close_index, {}, in_loop, depth + 1)
            index += 1

    def _loop(self, index, end, env, in_loop, depth):
        parser = self.parser
        tokens = parser.tokens
        body = self._loop_body(index)
        if body is None:
            return index + 1
        close = parser.pairs[body]
        header = tokens[index + 1 : body]
        texts = [token.text for token in header]
        values = None
        if "range" in texts:
            position = texts.index("range")
            names = [text for text in texts[:position] if text not in (",", ":=", "=")]
            expression = header[position + 1 :]
            if expression and len(names) == 2 and names[0] == "_":
                if len(expression) == 1 and expression[0].kind == "ident":
                    values = env.get(expression[0].text)
                else:
                    node, _after = parser.parse_value(index + 1 + position + 1, ("{",))
                    try:
                        values = self.evaluate(node, env)
                    except Unknown:
                        values = None
                if not isinstance(values, list):
                    values = None
        if values is None:
            self.walk(body + 1, close, dict(env), True, depth)
        else:
            for value in values:
                self.walk(body + 1, close, dict(env, **{names[1]: value}), in_loop, depth)
        return close + 1

    def _loop_body(self, index):
        """Index of the ``{`` opening a for statement's body."""
        parser = self.parser
        tokens = parser.tokens
        cursor = index + 1
        while cursor < len(tokens):
            token = tokens[cursor]
            if token.kind == "op" and token.text in ("(", "["):
                cursor = parser.pairs[cursor] + 1
                continue
            if token.kind == "op" and token.text == "{":
                previous = tokens[cursor - 1]
                before = tokens[cursor - 2] if cursor >= 2 else None
                # []T{...} and map[K]V{...} in a range expression are literals.
                if previous.kind == "ident" and before is not None and before.text in ("]",):
                    cursor = parser.pairs[cursor] + 1
                    continue
                return cursor
            cursor += 1
        return None

    def _call(self, method, method_index, env, in_loop):
        parser = self.parser
        report = self.report
        if method in IGNORED_METHODS:
            return
        arguments = parser.call_arguments(go_source.Node("expr", None, 0, token_index=method_index))
        line = parser.tokens[method_index].line
        if method == "ExecVitessAndMySQLDifferentQueries":
            arguments = arguments[1:]
            mode = "statement"
        elif method in MULTI_METHODS:
            mode = "multi"
        elif method in STATEMENT_METHODS:
            mode = STATEMENT_METHODS[method]
        else:
            report.count(f"mcmp method not imported: {method}")
            return
        if not arguments:
            return
        try:
            sql = self.evaluate(arguments[0], env)
            if not isinstance(sql, str):
                raise Unknown()
        except Unknown:
            report.filter("dynamic SQL argument")
            self.calls.append((None, line, mode, self._dynamic_target(arguments[0], env)))
            return
        statements = [s for s, _offset in common.split_statements(sql)] if mode == "multi" else [sql]
        for statement in statements:
            self.calls.append((statement, line, "statement" if mode == "multi" else mode, in_loop))

    def _dynamic_target(self, node, env):
        """Best-effort SQL text of a dynamic argument, used to find written tables."""
        text = node.text
        match = re.match(r"fmt\.Sprintf\(\s*(\"(?:[^\"\\]|\\.)*\"|`[^`]*`)", text)
        if match:
            return go_source.decode_string(match.group(1))
        return ""

    def evaluate(self, node, env):
        if node.kind == "string":
            if node.value is None:
                raise Unknown()
            return node.value
        if node.kind == "ident":
            if node.text in env:
                return env[node.text]
            raise Unknown()
        if node.kind == "composite" and node.type == "[]string":
            return [self.evaluate(element, env) for element in node.elements()]
        if node.kind == "expr":
            text = node.text
            if text.startswith("fmt.Sprintf("):
                arguments = self.parser.call_arguments(node)
                values = [self.evaluate(argument, env) for argument in arguments]
                if not all(isinstance(value, (str, int)) for value in values):
                    raise Unknown()
                return import_vitess.S(*values)
            # String concatenation: a + "b" + c
            parts = self._split_plus(node)
            if len(parts) > 1:
                values = [self.evaluate(part, env) for part in parts]
                if all(isinstance(value, str) for value in values):
                    return "".join(values)
            if re.fullmatch(r"-?\d+", text):
                return int(text)
        raise Unknown()

    def _split_plus(self, node):
        parser = self.parser
        tokens = parser.tokens
        start = node.token_index
        end = parser.skip_to_stop(start, (",", ")", ";", "}"))
        parts = []
        index = start
        while index < end:
            value, after = parser.parse_value(index, ("+", ",", ")", ";", "}"))
            parts.append(value)
            index = after + 1 if after < end and tokens[after].text == "+" else after
            if after >= end or tokens[after].text != "+":
                break
        return parts


def keyspaces(package):
    """Keyspace names declared in a package's main_test.go (first is the MySQL database)."""
    names = []
    path = package / "main_test.go"
    if not path.exists():
        return names
    for match in re.finditer(r"(\w*[Kk]eyspace\w*)\s*=\s*\"([\w-]+)\"", path.read_text(encoding="utf-8")):
        names.append(match.group(2).lower())
    return names


def mcmp_candidates(source, report, budget, dispositions):
    profile = VitessProfile()
    profile.filters = VITESS_FILTERS
    preparer = file_replay.StatementPreparer(profile, report)
    candidates = []
    root = source / QUERIES
    for package in sorted(path for path in root.iterdir() if path.is_dir()):
        schema = package / "schema.sql"
        tests = sorted(package.glob("*_test.go"))
        if not schema.exists() or not tests:
            continue
        names = keyspaces(package)
        own = names[0] if names else ""
        fixture = [sql for sql, _offset in common.split_statements(schema.read_text(encoding="utf-8"))]
        report.count("packages")
        for path in tests:
            relative = path.relative_to(source).as_posix()
            parser = go_source.Parser(path.read_text(encoding="utf-8"))
            functions = go_source.function_spans(parser)
            for name, (open_index, close_index, _line) in functions.items():
                if not name.startswith("Test"):
                    continue
                walker = BodyWalker(parser, functions, report)
                walker.walk(open_index + 1, close_index, {})
                if not walker.calls:
                    continue
                report.count("test functions with MySQL comparisons")
                candidates += replay_test(
                    relative, name, fixture, walker.calls, own, names, preparer, report, budget, dispositions
                )
    return candidates


def replay_test(relative, test_name, fixture, calls, own, keyspace_names, preparer, report, budget, dispositions):
    statements = [file_replay.Statement(sql=sql.strip(), line=0) for sql in fixture]
    roles = {}
    for sql, line, mode, uncertain in calls:
        if sql is None:
            # Dynamic SQL: if it writes, the written table is unusable later.
            statement = file_replay.Statement(sql=uncertain or "/* dynamic */ select 1", line=line)
            roles[len(statements)] = ("dynamic", mode)
        else:
            statement = file_replay.Statement(sql=sql.strip().rstrip(";").strip(), line=line)
            statement.expect_error = mode == "error"
            roles[len(statements)] = ("loop" if uncertain else "call", mode)
        statements.append(statement)
    preparer.prepare(statements, own)
    databases = set(keyspace_names) - {own}
    replayer = file_replay.Replayer(report, dispositions)
    out = []
    seen_ids = collections.Counter()
    for position, statement in enumerate(statements):
        role = roles.get(position)
        if role and (role[0] in ("dynamic", "loop")) and statement.kind in ("setup", "session"):
            # Not replayable as written: mark what it modifies as unusable.
            statement.filtered = statement.filtered or (
                "dynamic SQL" if role[0] == "dynamic" else "write repeated by a loop"
            )
        if statement.kind not in ("use", "noop", "dropdb") and not statement.filtered:
            qualifiers = file_replay.qualified_databases(common.significant_tokens(statement.sql))
            if qualifiers & databases:
                statement.filtered = "cross-keyspace reference"
        if not replayer.process(statement):
            continue
        if role is None or role[0] == "dynamic":
            continue
        if statement.filtered:
            report.filter(statement.filtered)
            continue
        if statement.kind == "session" and not statement.expect_error:
            continue
        setup, taint = replayer.slice([statement])
        if taint:
            report.filter(f"depends on an object {taint.split(' (')[0]}")
            continue
        if len(setup) > budget.max_setup:
            report.filter("setup longer than the per-candidate cap")
            continue
        stem = relative.removesuffix(".go")
        base = f"{SOURCE_NAME}/{stem}:L{statement.line}"
        seen_ids[base] += 1
        ordered = role[1] != "unordered" and file_replay.candidate_ordered(statement)
        out.append(
            common.make_candidate(
                candidate_id=base if seen_ids[base] == 1 else f"{base}.{seen_ids[base]}",
                repo=REPO,
                revision=REVISION,
                path=relative,
                line=statement.line,
                license=LICENSE,
                group=f"{SOURCE_NAME}/{stem}/{test_name}",
                setup=[s.sql for s in setup],
                sql=[statement.sql],
                ordered=ordered,
                setup_errors="drop",
            )
        )
    return out


def tester_candidates(source, report, budget):
    importer = file_replay.FileImporter(VitessProfile(), budget, report)
    root = source / TESTER
    candidates = []
    for path in sorted(root.rglob("*.test")):
        relative = path.relative_to(root).as_posix()
        for candidate in importer.import_file(path, relative):
            candidates.append(candidate)
        report.count("vitess-tester files")
    return candidates, importer.dispositions


def run(source, output, budget, write=True):
    source = pathlib.Path(source)
    revision = common.check_revision(source, REVISION)
    report = common.Report(
        "Vitess end-to-end MySQL comparison import",
        {
            "repository": REPO,
            "revision": revision,
            "paths": f"{QUERIES}, {TESTER}",
            "license": LICENSE,
            "budget": f"max {budget.max_setup} setup statements per candidate",
        },
    )
    tester, dispositions = tester_candidates(source, report, budget)
    mcmp = mcmp_candidates(source, report, budget, dispositions)
    candidates = []
    seen = set()
    for candidate in tester + mcmp:
        key = common.stable_hash("\x1f".join(candidate["setup"]) + "\x1e" + "\x1f".join(candidate["sql"]))
        if key in seen:
            report.filter("duplicate of an earlier candidate")
            continue
        seen.add(key)
        candidates.append(candidate)
    report.count("candidates emitted", len(candidates))
    report.count("setup statements across candidates (replay budget)", sum(len(c["setup"]) for c in candidates))
    report.table(
        "Non-query statement dispositions",
        ["disposition", "statements"],
        sorted(dispositions.items(), key=lambda item: (-item[1], item[0])),
    )
    report.table("Levels", ["level", "candidates"], common.level_histogram(candidates))
    report.note(
        "vitess-tester scripts use the shared mysqltest replay; `--skip` (skipped by Vitess only) "
        "statements are kept because MySQL runs them."
    )
    report.note(
        "mcmp tests: each Test function is replayed after its package's schema.sql; writes are setup "
        "and also candidates, reads and expected errors are candidates. Qualifiers naming the package "
        "keyspace are removed; other keyspaces are filtered."
    )
    if write:
        common.write_candidates(output, SOURCE_NAME, candidates)
        text = report.write(output)
    else:
        text = report.render()
    return candidates, report, text


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--source", required=True, type=pathlib.Path, help="Vitess checkout")
    parser.add_argument(
        "--output",
        type=pathlib.Path,
        default=pathlib.Path(__file__).resolve().parents[1] / "candidates" / SOURCE_NAME,
    )
    parser.add_argument("--max-setup", type=int, default=120, help="setup statements per candidate")
    args = parser.parse_args(argv)
    candidates, _report, text = run(args.source, args.output, file_replay.Budget(max_setup=args.max_setup))
    print(text)
    print(f"wrote {len(candidates):,} candidates to {args.output}")


if __name__ == "__main__":
    main()
