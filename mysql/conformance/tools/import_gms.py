#!/usr/bin/env python3
"""Import go-mysql-server engine tests as structured MySQL spec candidates.

go-mysql-server (Apache-2.0) keeps its engine tests as Go composite literals in
``enginetest/queries``. Unlike the syntax-only miner (``mine_go_mysql_server.py``)
this importer keeps their structure:

* ``ScriptTest``-like arrays (``SetUpScript`` plus ``Assertions``/``Queries``)
  are replayed in order; each assertion becomes a candidate whose setup is the
  dependency slice of the fixture, the script's setup and earlier assertions.
* Shared-fixture arrays (``QueryTest``, ``WriteQueryTest``, ``QueryErrorTest``,
  ...) run against the ``enginetest/scriptgen/setup`` fixtures that the Go test
  driver installs before ranging over the array (``harness.Setup(...)`` in
  ``enginetest/*.go``); every test is independent.
* ``QueryPlanTest`` arrays are ignored (plans are engine-specific).

Expected results in the Go source are ignored. ``Skip: true`` tests, scripts for
other dialects and tests that need bound parameters are not imported. Go source
files are tokenized as text; nothing is compiled or executed.
"""

import argparse
import collections
import dataclasses
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import file_replay  # noqa: E402
import go_source  # noqa: E402
import importer_common as common  # noqa: E402

SOURCE_NAME = "go-mysql-server"
REPO = "https://github.com/dolthub/go-mysql-server.git"
REVISION = "478bed778dd282aa13aa48ebd75b0549ebe75698"
LICENSE = "Apache-2.0"
QUERIES = "enginetest/queries"
FIXTURES = "enginetest/scriptgen/setup/setup_data.sg.go"
GROUPS = "enginetest/scriptgen/setup/helper.go"
DEFAULT_FIXTURE = ["MydbData"]

SCRIPT_TYPES = {"ScriptTest", "CharsetCollationWireTest", "TypeWireTest"}
QUERY_TYPES = {"QueryTest", "QueryErrorTest", "GenericErrorQueryTest", "RegexTest"}
WRITE_TYPES = {"WriteQueryTest"}
ASSERTION_TYPES = {"ScriptTestAssertion"}
IGNORED_TYPES = {
    "QueryPlanTest": "query plans are engine-specific",
    "TransactionTest": "multi-client transaction scripts",
    "UserPrivilegeTest": "users and privileges",
    "QuickPrivilegeTest": "users and privileges",
    "ServerAuthenticationTest": "users and privileges",
    "ParallelismTest": "engine parallelism",
    "VectorFunctionTestCase": "vector functions",
}

GMS_FILTERS = [
    (r"\bdolt\w*|\bas\s+of\b", "dolt/versioned query"),
    (r"@@\w*(gms|dolt)\w*", "go-mysql-server system variable"),
]


class GmsProfile(file_replay.Profile):
    name = SOURCE_NAME
    title = "go-mysql-server enginetest"
    repo = REPO
    revision = REVISION
    license = LICENSE
    root = ""
    filters = GMS_FILTERS
    notes = ()


@dataclasses.dataclass
class Test:
    """One test after extraction from Go, before replay."""

    array: str
    path: str
    kind: str  # script or fixture
    fixture: list
    statements: list  # (sql, line, role) with role setup|assertion|setup-only|write|select
    new_session_before: set = dataclasses.field(default_factory=set)


def decode(node):
    """String value of a Go node, or None when it is not a static UTF-8 string."""
    if node is None or node.kind != "string":
        return None
    return node.value


def is_true(node):
    return node is not None and node.kind == "ident" and node.text == "true"


def element_type(type_text):
    text = type_text.strip()
    if text.startswith("[]"):
        text = text[2:]
    return text.rsplit(".", 1)[-1].lstrip("*")


class FixtureLibrary:
    def __init__(self, root):
        parser = go_source.Parser((root / FIXTURES).read_text(encoding="utf-8"))
        self.fixtures = {}
        for name, node in parser.declarations():
            if node.kind == "composite" and element_type(node.type) == "SetupScript":
                statements = []
                for script in node.elements():
                    for element in script.elements():
                        value = decode(element)
                        if value is None:
                            raise ValueError(f"dynamic fixture statement in {name}")
                        statements.append(value)
                self.fixtures[name] = statements
        parser = go_source.Parser((root / GROUPS).read_text(encoding="utf-8"))
        self.groups = {}
        for name, node in parser.declarations():
            if node.kind == "composite":
                self.groups[name] = [element.text.rsplit(".", 1)[-1] for element in node.elements()]

    def expand(self, name):
        if name in self.groups:
            return list(self.groups[name])
        if name in self.fixtures:
            return [name]
        raise KeyError(name)

    def statements(self, items):
        out = []
        for item in items:
            if isinstance(item, tuple):
                out.extend(item[1])
            else:
                out.extend(self.fixtures[item])
        return out


def driver_fixtures(root, library, report):
    """Map ``queries.<Array>`` to the fixtures installed by the Go test driver."""
    mapping = {}
    sources = sorted((root / "enginetest").glob("*.go"))
    functions = []
    for path in sources:
        parser = go_source.Parser(path.read_text(encoding="utf-8"))
        for name, span in go_source.function_spans(parser).items():
            functions.append((("Prepared" in name), path.name, name, parser, span))
    for _prepared, _file, _name, parser, (open_index, close_index, _line) in sorted(functions, key=lambda f: f[:3]):
        local = {}
        for variable, _op, node in parser.statements(open_index + 1, close_index):
            value = _fixture_items(node, local, library, parser)
            if value is not None:
                local[variable] = value
        current = None
        tokens = parser.tokens
        for index in range(open_index, close_index):
            token = tokens[index]
            if (
                token.kind == "ident"
                and token.text == "Setup"
                and index > 0
                and tokens[index - 1].text == "."
                and parser.is_op(index + 1, "(")
            ):
                arguments = parser.call_arguments(go_source.Node("expr", None, token.line, token_index=index))
                items = []
                for argument in arguments:
                    value = _fixture_items(argument, local, library, parser)
                    if value is None:
                        report.count("driver Setup arguments not understood")
                        items = None
                        break
                    items += value
                current = items
            elif (
                token.kind == "ident"
                and token.text == "queries"
                and parser.is_op(index + 1, ".")
                and index + 2 < len(tokens)
                and tokens[index + 2].kind == "ident"
            ):
                array = tokens[index + 2].text
                if array not in mapping and current is not None:
                    mapping[array] = current
    return mapping


def _fixture_items(node, local, library, parser):
    text = node.text.removesuffix("...").strip()
    if text.startswith("setup."):
        try:
            return library.expand(text.removeprefix("setup."))
        except KeyError:
            return None
    if text in local:
        return list(local[text])
    if node.kind == "composite" and element_type(node.type) == "SetupScript":
        statements = []
        for script in node.elements():
            for element in script.elements():
                value = decode(element)
                if value is None:
                    return None
                statements.append(value)
        return [("literal", statements)]
    if node.kind == "expr" and node.text.startswith("append("):
        items = []
        for argument in parser.call_arguments(node):
            value = _fixture_items(argument, local, library, parser)
            if value is None:
                return None
            items += value
        return items
    return None


class Extractor:
    def __init__(self, root, library, mapping, report):
        self.root = root
        self.library = library
        self.mapping = mapping
        self.report = report

    def arrays(self):
        for path in sorted((self.root / QUERIES).glob("*.go")):
            if path.name.endswith("_test.go"):
                continue
            relative = path.relative_to(self.root).as_posix()
            parser = go_source.Parser(path.read_text(encoding="utf-8"))
            for name, node in parser.declarations():
                if node.kind != "composite" or not node.type.startswith("[]"):
                    continue
                kind = element_type(node.type)
                yield relative, name, kind, node, parser

    def tests(self):
        report = self.report
        for relative, name, kind, node, parser in self.arrays():
            if kind in IGNORED_TYPES:
                report.filter(f"ignored array type {kind} ({IGNORED_TYPES[kind]})", len(node.elements()))
                continue
            fixture = self.mapping.get(name)
            if kind in SCRIPT_TYPES:
                for element in node.elements():
                    test = self.script(relative, name, element, fixture or DEFAULT_FIXTURE)
                    if test:
                        yield test
            elif kind in ASSERTION_TYPES:
                test = self.script_from_assertions(relative, name, node.elements(), [], fixture or DEFAULT_FIXTURE)
                if test:
                    yield test
            elif kind in QUERY_TYPES or kind in WRITE_TYPES:
                if fixture is None:
                    report.count("query arrays without a driver fixture (default fixture used)")
                for element in node.elements():
                    test = self.fixture_test(relative, name, kind, element, fixture or DEFAULT_FIXTURE)
                    if test:
                        yield test
            elif name == "CollationCoercionTests":
                yield from self.collation_coercion(relative, name, node, parser)
            elif name == "ConvertTests":
                yield from self.convert_tests(relative, name, node, fixture or DEFAULT_FIXTURE)
            elif kind == "string" or name == "CollationCoercionSetup":
                continue
            else:
                report.filter(f"unsupported array type {kind}", len(node.elements()))

    def script(self, relative, array, element, fixture):
        report = self.report
        if is_true(element.field("Skip")):
            report.filter("script skipped by source (Skip: true)")
            return None
        dialect = decode(element.field("Dialect"))
        if dialect and dialect.lower() != "mysql":
            report.filter(f"script for another dialect ({dialect})")
            return None
        setup = []
        setup_node = element.field("SetUpScript")
        if setup_node is not None:
            for statement in setup_node.elements():
                value = decode(statement)
                if value is None:
                    report.filter("script with a dynamic setup statement")
                    return None
                setup.append((value, statement.line))
        assertions = element.field("Assertions") or element.field("Queries")
        items = assertions.elements() if assertions is not None else []
        if element.field("Query") is not None:
            items = [element]
        return self.script_from_assertions(relative, array, items, setup, fixture)

    def script_from_assertions(self, relative, array, items, setup, fixture):
        report = self.report
        statements = [(sql, line, "setup") for sql, line in setup]
        new_session = set()
        for item in items:
            if item.kind == "string":
                if item.value is None:
                    report.filter("assertion with a non-UTF-8 query")
                    continue
                statements.append((item.value, item.line, "assertion"))
                continue
            query = item.field("Query")
            value = decode(query)
            if value is None:
                report.filter("assertion with a dynamic query")
                continue
            if is_true(item.field("Skip")):
                report.filter("assertion skipped by source (Skip: true)")
                continue
            if item.field("Bindings") is not None:
                report.filter("assertion needs bound parameters")
                continue
            if is_true(item.field("NewSession")):
                new_session.add(len(statements))
            role = "setup-only" if is_true(item.field("SkipResultsCheck")) else "assertion"
            statements.append((value, query.line, role))
        if not any(role == "assertion" for _sql, _line, role in statements):
            return None
        return Test(array, relative, "script", fixture, statements, new_session)

    def fixture_test(self, relative, array, kind, element, fixture):
        report = self.report
        if is_true(element.field("Skip")):
            report.filter("test skipped by source (Skip: true)")
            return None
        if element.field("Bindings") is not None:
            report.filter("test needs bound parameters")
            return None
        if kind in WRITE_TYPES:
            write, select = element.field("WriteQuery"), element.field("SelectQuery")
            if decode(write) is None or (select is not None and decode(select) is None):
                report.filter("test with a dynamic query")
                return None
            statements = [(decode(write), write.line, "write")]
            if select is not None:
                statements.append((decode(select), select.line, "select"))
            return Test(array, relative, "fixture", fixture, statements)
        query = element.field("Query")
        if decode(query) is None:
            report.filter("test with a dynamic query")
            return None
        return Test(array, relative, "fixture", fixture, [(decode(query), query.line, "assertion")])

    def convert_tests(self, relative, array, node, fixture):
        """Port of TestConvert in enginetest/enginetests.go."""
        for element in node.elements():
            parts = [decode(element.field(field)) for field in ("Field", "Op", "Operand")]
            if None in parts:
                self.report.filter("test with a dynamic query")
                continue
            query = "select count(*) from typestable where %s %s %s" % tuple(parts)
            yield Test(array, relative, "fixture", fixture, [(query, element.line, "assertion")])

    def collation_coercion(self, relative, array, node, parser):
        """Port of TestCollationCoercion in enginetest/engine_only_test.go."""
        setup = None
        for name, value in parser.declarations():
            if name == "CollationCoercionSetup":
                setup = [(decode(element), element.line) for element in value.elements()]
        for element in node.elements():
            parameters = decode(element.field("Parameters"))
            if parameters is None:
                self.report.filter("test with a dynamic query")
                continue
            line = element.field("Parameters").line
            statements = [(sql, setup_line, "setup") for sql, setup_line in setup]
            statements.append((f"SELECT COERCIBILITY({parameters}) FROM temp_tbl LIMIT 1;", line, "assertion"))
            statements.append((f"SELECT COLLATION({parameters}) FROM temp_tbl LIMIT 1;", line, "assertion"))
            yield Test(array, relative, "script", DEFAULT_FIXTURE, statements)


class Importer:
    def __init__(self, library, report, budget):
        self.library = library
        self.report = report
        self.budget = budget
        self.profile = GmsProfile()
        self.preparer = file_replay.StatementPreparer(self.profile, report)
        self.dispositions = collections.Counter()
        self.fixture_cache = {}
        self.ids = collections.Counter()

    def fixture_statements(self, fixture):
        return [
            file_replay.Statement(sql=sql.strip().rstrip(";").strip(), line=0)
            for sql in self.library.statements(fixture)
        ]

    def candidates(self, test):
        if test.kind == "script":
            return self.script_candidates(test)
        return self.fixture_candidates(test)

    def script_candidates(self, test):
        statements = self.fixture_statements(test.fixture)
        fixture_count = len(statements)
        roles = {}
        for sql, line, role in test.statements:
            statement = file_replay.Statement(sql=sql.strip().rstrip(";").strip(), line=line)
            roles[len(statements)] = role
            statements.append(statement)
        self.preparer.prepare(statements, "")
        replayer = file_replay.Replayer(self.report, self.dispositions)
        out = []
        for position, statement in enumerate(statements):
            if position - fixture_count in test.new_session_before:
                replayer.new_session()
            if not replayer.process(statement):
                if roles.get(position) == "assertion" and statement.kind in ("use", "noop", "dropdb"):
                    self.report.filter("assertion is a database statement (virtualized)")
                continue
            if roles.get(position) == "assertion":
                candidate = self.candidate(test, statement, [statement], replayer)
                if candidate:
                    out.append(candidate)
        return out

    def fixture_candidates(self, test):
        key = tuple(item if isinstance(item, str) else ("literal", tuple(item[1])) for item in test.fixture)
        cached = self.fixture_cache.get(key)
        if cached is None:
            statements = self.fixture_statements(test.fixture)
            context = self.preparer.prepare(statements, "")
            replayer = file_replay.Replayer(self.report, self.dispositions)
            for statement in statements:
                replayer.process(statement)
            cached = (len(statements), context, replayer)
            self.fixture_cache[key] = cached
        size, context, replayer = cached
        extra = [
            file_replay.Statement(sql=sql.strip().rstrip(";").strip(), line=line) for sql, line, _role in test.statements
        ]
        # Resolve the test statements in the fixture's final context; each test is
        # independent, so nothing is added to the shared replay state.
        self.preparer.prepare(extra, context=context, start_index=size)
        anchors = []
        for statement in extra:
            if not replayer.process(statement, persist=False):
                self.report.filter("test is a database statement (virtualized)")
                return []
            anchors.append(statement)
        return [c for c in [self.candidate(test, anchors[0], anchors, replayer)] if c]

    def candidate(self, test, anchor, anchors, replayer):
        report = self.report
        for statement in anchors:
            if statement.filtered:
                report.filter(statement.filtered)
                return None
        setup, taint = replayer.slice(anchors)
        if taint:
            report.filter(f"depends on an object {taint.split(' (')[0]}")
            return None
        if len(setup) > self.budget.max_setup:
            report.filter("setup longer than the per-candidate cap")
            return None
        stem = test.path.removesuffix(".go")
        base = f"{SOURCE_NAME}/{stem}:L{anchor.line}"
        self.ids[base] += 1
        candidate_id = base if self.ids[base] == 1 else f"{base}.{self.ids[base]}"
        last = anchors[-1]
        return common.make_candidate(
            candidate_id=candidate_id,
            repo=REPO,
            revision=REVISION,
            path=test.path,
            line=anchor.line,
            license=LICENSE,
            group=f"{SOURCE_NAME}/{stem}/{test.array}",
            setup=[s.sql for s in setup],
            sql=[s.sql for s in anchors],
            ordered=file_replay.candidate_ordered(last),
            setup_errors="drop",
        )


def run(source, output, budget, write=True):
    source = pathlib.Path(source)
    revision = common.check_revision(source, REVISION)
    report = common.Report(
        "go-mysql-server enginetest import",
        {
            "repository": REPO,
            "revision": revision,
            "path": QUERIES,
            "license": LICENSE,
            "budget": f"max {budget.max_setup} setup statements per candidate",
        },
    )
    library = FixtureLibrary(source)
    mapping = driver_fixtures(source, library, report)
    extractor = Extractor(source, library, mapping, report)
    importer = Importer(library, report, budget)
    candidates = []
    seen = set()
    per_array = collections.Counter()
    tests = 0
    for test in extractor.tests():
        tests += 1
        for candidate in importer.candidates(test):
            key = common.stable_hash("\x1f".join(candidate["setup"]) + "\x1e" + "\x1f".join(candidate["sql"]))
            if key in seen:
                report.filter("duplicate of an earlier candidate")
                continue
            seen.add(key)
            candidates.append(candidate)
            per_array[f"{test.path.rsplit('/', 1)[-1]}:{test.array}"] += 1
    report.counts = collections.Counter(
        {
            "fixtures": len(library.fixtures),
            "arrays mapped to driver fixtures": len(mapping),
            "tests extracted": tests,
            **report.counts,
        }
    )
    report.count("candidates emitted", len(candidates))
    report.count("setup statements across candidates (replay budget)", sum(len(c["setup"]) for c in candidates))
    report.table(
        "Non-query statement dispositions",
        ["disposition", "statements"],
        sorted(importer.dispositions.items(), key=lambda item: (-item[1], item[0])),
    )
    report.table("Candidates by array", ["array", "candidates"], sorted(per_array.items()))
    report.table("Levels", ["level", "candidates"], common.level_histogram(candidates))
    report.note(
        "Script assertions are replayed in order: each candidate's setup is the dependency slice of the "
        "fixture, the script's SetUpScript and the earlier assertions (writes included). Assertions with "
        "SkipResultsCheck only contribute setup. NewSession assertions reset session state."
    )
    report.note(
        "Shared-fixture tests use the fixtures that enginetest/*.go installs with harness.Setup before "
        "ranging over the array (first non-Prepared driver); unmapped arrays use the mydb fixture. "
        "WriteQueryTest candidates run [WriteQuery, SelectQuery] as the tested statements."
    )
    report.note("Expected results, errors and warnings in the Go source are ignored.")
    if write:
        common.write_candidates(output, SOURCE_NAME, candidates)
        text = report.write(output)
    else:
        text = report.render()
    return candidates, report, text


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--source", required=True, type=pathlib.Path, help="go-mysql-server checkout")
    parser.add_argument(
        "--output",
        type=pathlib.Path,
        default=pathlib.Path(__file__).resolve().parents[1] / "candidates" / SOURCE_NAME,
    )
    parser.add_argument("--max-setup", type=int, default=200, help="setup statements per candidate")
    args = parser.parse_args(argv)
    budget = file_replay.Budget(max_setup=args.max_setup)
    candidates, _report, text = run(args.source, args.output, budget)
    print(text)
    print(f"wrote {len(candidates):,} candidates to {args.output}")


if __name__ == "__main__":
    main()
