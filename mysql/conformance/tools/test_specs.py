"""Unit tests for the recorded-spec tools (no MySQL server needed)."""

import contextlib
import importlib.util
import io
import json
import pathlib
import tempfile
import unittest

TOOLS = pathlib.Path(__file__).resolve().parent


def load(name):
    spec = importlib.util.spec_from_file_location(name, TOOLS / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


LIB = load("spec_lib")
SLT = load("import_sqllogictest")
REC = load("record_mysql")
SCORE = load("score")


class BlockModelTests(unittest.TestCase):
    def round_trip(self, text):
        rendered = "\n    " + LIB.escape_block(text) + "\n}"
        content, end = LIB.extract_block(rendered, 0)
        self.assertEqual(end, len(rendered))
        return LIB.rust_trim(content)

    def test_escaping_round_trips_braces_and_backslashes(self):
        for text in ["SELECT '{x}'", "SELECT '}'", "a\\{b", "a\\\\b", "x\\", "\\}\\{\\\\", "{{a}}"[1:-1],
                     "SELECT 'it\\'s'", "SELECT '\\n'", "SELECT '}}}{{'"[0:12]]:
            self.assertEqual(self.round_trip(text), text, text)

    def test_unterminated_block_is_reported(self):
        with self.assertRaises(LIB.BlockError):
            LIB.extract_block("abc { def", 0)

    def test_rust_trim_differs_from_python_strip(self):
        self.assertEqual(LIB.rust_trim("\x1fa\x1f"), "\x1fa\x1f")
        self.assertEqual(LIB.rust_trim("　a\t"), "a")

    def test_statements_split_back(self):
        statements = ["CREATE TABLE t(a INT)", "INSERT INTO t VALUES(1) -- tail", "SELECT 'a\nb'",
                      "SELECT 1 # hash", "SELECT 'x;\ny' AS `;`", "SELECT 2;"[:-1]]
        text = LIB.sql_block_text(statements, "    ")
        self.assertEqual(LIB.split_block_statements(text), statements)

    def test_mysql_statement_splitting(self):
        text = ("SELECT 'a;b', \"c\\\";d\", `e;f`, 'g'';h' -- x;y\n; SELECT 2 # z;\n;"
                "/* ; */ SELECT 3;;  -- only a comment\n")
        self.assertEqual(LIB.mysql_statements(text), [
            "SELECT 'a;b', \"c\\\";d\", `e;f`, 'g'';h' -- x;y", "SELECT 2 # z;", "/* ; */ SELECT 3",
            "-- only a comment"])
        self.assertEqual(LIB.mysql_statements("SELECT 1--2; SELECT 3"), ["SELECT 1--2", "SELECT 3"])
        self.assertIsNotNone(LIB.statement_representable(
            "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW BEGIN SET NEW.a = 1; SET NEW.b = 2; END"))
        self.assertIsNone(LIB.statement_representable("SELECT 'x;\ny'"))

    def test_statement_representability(self):
        self.assertIsNone(LIB.statement_representable("SELECT 1"))
        self.assertIsNotNone(LIB.statement_representable("SELECT 1;\nSELECT 2"))
        self.assertIsNotNone(LIB.statement_representable(" SELECT 1"))
        self.assertIsNotNone(LIB.statement_representable("SELECT '{{x}}'"))


class ExpectationTests(unittest.TestCase):
    def test_plain_rows_round_trip(self):
        self.assertIsNone(LIB.expectation_round_trips([["1", "NULL"], ["2", "x y"]], True))
        self.assertIsNone(LIB.expectation_round_trips([], True))
        self.assertIsNone(LIB.expectation_round_trips([], False))

    def test_inner_whitespace_survives_but_edges_do_not(self):
        self.assertIsNone(LIB.expectation_round_trips([["1", " x ", "2"]], True))
        self.assertIsNotNone(LIB.expectation_round_trips([[" x", "2"]], True))
        self.assertIsNotNone(LIB.expectation_round_trips([["2", "x "]], True))

    def test_separators_and_line_breaks_are_unrepresentable(self):
        self.assertIsNotNone(LIB.expectation_round_trips([["a|b"]], True))
        self.assertIsNotNone(LIB.expectation_round_trips([["a\nb"]], True))
        self.assertIsNotNone(LIB.expectation_round_trips([["a\rb"]], False))
        self.assertIsNotNone(LIB.expectation_round_trips([["a\x01"]], True))
        self.assertIsNotNone(LIB.expectation_round_trips([["{{x}}"]], True))

    def test_raw_blocks_keep_edge_whitespace(self):
        rows = [["  a", "b "], ["", "{x}"], ["c\\", ""]]
        self.assertIsNotNone(LIB.expectation_round_trips(rows, True))
        self.assertIsNone(LIB.expectation_round_trips(rows, True, raw=True))
        self.assertIsNone(LIB.expectation_round_trips([[" one row "]], False, raw=True))
        self.assertIsNotNone(LIB.expectation_round_trips([[" a"], ["b"]], False, raw=True))

    def test_empty_single_value_rows(self):
        # Exact blocks keep empty lines; unordered blocks drop them.
        self.assertIsNone(LIB.expectation_round_trips([["1"], [""]], True))
        self.assertIsNotNone(LIB.expectation_round_trips([["1"], [""]], False))

    def test_braces_in_values_round_trip(self):
        self.assertIsNone(LIB.expectation_round_trips([['{"a": [1]}'], ["}"]], True))


class ClassifierTests(unittest.TestCase):
    def test_levels_order_simple_before_complex(self):
        literal, _ = LIB.classify([], ["SELECT 1"])
        scan, _ = LIB.classify(["CREATE TABLE t(a INT)", "INSERT INTO t VALUES(1)"], ["SELECT a FROM t"])
        join, _ = LIB.classify(["CREATE TABLE t(a INT)"], ["SELECT * FROM t AS x JOIN t AS y ON x.a = y.a"])
        sub, _ = LIB.classify(["CREATE TABLE t(a INT)"], ["SELECT a FROM t WHERE a IN (SELECT a FROM t)"])
        self.assertLess(literal, scan)
        self.assertLess(scan, join)
        self.assertLess(join, sub)

    def test_select_star_is_not_arithmetic(self):
        features = LIB.statement_features("SELECT * FROM t WHERE x IN (SELECT COUNT(*) FROM u)")
        self.assertNotIn("arithmetic", features)
        self.assertIn("arithmetic", LIB.statement_features("SELECT - a * 2 FROM t"))

    def test_from_width(self):
        self.assertEqual(LIB.from_width("SELECT 1"), 0)
        self.assertEqual(LIB.from_width("SELECT a FROM t1, t2 , t3 WHERE a IN (SELECT b FROM u, v)"), 3)
        self.assertEqual(LIB.from_width("SELECT * FROM t1 JOIN t2 ON 1 LEFT JOIN t3 ON 1 ORDER BY 1"), 3)
        self.assertEqual(LIB.from_width("SELECT * FROM (SELECT 1 FROM a, b) AS x, t2"), 2)
        self.assertEqual(LIB.from_width("SELECT (SELECT max(b) FROM u, v) FROM t"), 2)

    def test_server_mutations_are_refused(self):
        for statement in ["SET GLOBAL sql_mode = ''", "set @@global.max_connections = 10",
                          "SET SESSION a = 1, PERSIST b = 2", "SET PASSWORD = 'x'",
                          "ALTER USER CURRENT_USER() IDENTIFIED BY 'x'", "GRANT ALL ON *.* TO root@'%'",
                          "INSTALL PLUGIN x SONAME 'x.so'", "CREATE TABLESPACE ts ADD DATAFILE 'a.ibd'",
                          "XA START 'x'", "RESET MASTER", "KILL 12", "STOP REPLICA",
                          "/* c */ SET GLOBAL x = 1"]:
            self.assertIsNotNone(LIB.server_mutation(statement), statement)
        for statement in ["SET sql_mode = ''", "SET SESSION time_zone = '+01:00'", "SELECT @@GLOBAL.sql_mode",
                          "CREATE USER u@'%'", "GRANT SELECT ON db.* TO u", "SELECT 'SET GLOBAL'",
                          "CREATE TABLE t(a INT) TABLESPACE_X = 1"[:24]]:
            self.assertIsNone(LIB.server_mutation(statement), statement)

    def test_side_effect_lanes(self):
        self.assertTrue(LIB.is_read_only("SELECT a FROM t"))
        self.assertFalse(LIB.is_read_only("SELECT LAST_INSERT_ID()"))
        self.assertFalse(LIB.is_read_only("SELECT @x := 1"))
        self.assertFalse(LIB.is_read_only("INSERT INTO t VALUES(1)"))
        self.assertTrue(LIB.touches_global_state("CREATE DATABASE d"))
        self.assertTrue(LIB.touches_global_state("USE d"))
        self.assertFalse(LIB.touches_global_state("SELECT * FROM t USE INDEX (i)"))
        self.assertFalse(LIB.references_tables("SELECT 1 + 2"))
        self.assertTrue(LIB.references_tables("SELECT (SELECT 1 FROM t)"))


class CandidateSchemaTests(unittest.TestCase):
    def candidate(self, **overrides):
        record = {
            "id": "src/g:L1",
            "source": {"repo": "r", "revision": "v", "path": "p", "line": 1, "license": "MIT"},
            "group": "src/g",
            "setup": [],
            "sql": ["SELECT 1"],
            "ordered": True,
            "level": 100,
            "features": ["select-literal"],
        }
        record.update(overrides)
        return record

    def test_valid_candidate(self):
        self.assertEqual(LIB.validate_candidate(self.candidate()), [])
        self.assertEqual(LIB.validate_candidate(self.candidate(setup_errors="drop", setup_base="src/g:L0")), [])

    def test_invalid_candidates(self):
        self.assertTrue(LIB.validate_candidate(self.candidate(sql=[])))
        self.assertTrue(LIB.validate_candidate(self.candidate(level=1001)))
        self.assertTrue(LIB.validate_candidate(self.candidate(setup_errors="ignore")))
        self.assertTrue(LIB.validate_candidate(self.candidate(group="other/g")))
        self.assertTrue(LIB.validate_candidate(self.candidate(extra=1)))


SCRIPT = (
    "# comment\r\n"
    "hash-threshold 8\r\n"
    "\r\n"
    "statement ok\r\n"
    "CREATE TABLE t1(a INTEGER, b INTEGER)\r\n"
    "\r\n"
    "statement ok\r\n"
    "INSERT INTO t1 VALUES(1,2)\r\n"
    "\r\n"
    "query II rowsort label-1\r\n"
    "SELECT a, b\r\n"
    "# inside a record\r\n"
    "  FROM t1\r\n"
    "----\r\n"
    "1\r\n"
    "2\r\n"
    "\r\n"
    "skipif mysql\r\n"
    "query I nosort\r\n"
    "SELECT 1/0\r\n"
    "----\r\n"
    "NULL\r\n"
    "\r\n"
    "onlyif mssql\r\n"
    "statement ok\r\n"
    "CREATE INDEX t1i ON t1(a)\r\n"
    "\r\n"
    "onlyif mysql\r\n"
    "query I nosort\r\n"
    "SELECT a FROM t1 ORDER BY a\r\n"
    "----\r\n"
    "30 values hashing to 0123456789abcdef0123456789abcdef\r\n"
    "\r\n"
    "statement error\r\n"
    "INSERT INTO nope VALUES(1)\r\n"
    "\r\n"
    "query I nosort\r\n"
    "SELECT 7\r\n"
    "----\r\n"
    "7\r\n"
    "\r\n"
    "onlyif mysql\r\n"
    "halt\r\n"
    "\r\n"
    "query I nosort\r\n"
    "SELECT 'after halt'\r\n"
)


class SqllogictestParserTests(unittest.TestCase):
    def test_records_follow_the_reference_harness(self):
        records = SLT.parse_records(SCRIPT)
        kinds = [r.kind for r in records]
        self.assertEqual(kinds, ["hash-threshold", "statement", "statement", "query", "query", "statement",
                                 "query", "statement", "query", "halt", "query"])
        query = records[3]
        self.assertEqual(query.sql, "SELECT a, b\n  FROM t1")
        self.assertEqual(query.results, ["1", "2"])
        self.assertEqual(query.line, 10)
        self.assertFalse(SLT.runs_on(records[4]))
        self.assertFalse(SLT.runs_on(records[5]))
        self.assertTrue(SLT.runs_on(records[6]))
        self.assertEqual(SLT.result_rows_estimate(records[6]), 30)
        self.assertEqual(SLT.result_rows_estimate(query), 1)

    def test_scan_keeps_mysql_records_until_halt(self):
        stats = __import__("collections").Counter()
        items = list(SLT.scan_records(SCRIPT, stats))
        self.assertEqual([item[0] for item in items], ["statement", "statement", "query", "query",
                                                       "statement", "query"])
        self.assertEqual(items[2][3], False)  # rowsort -> unordered
        self.assertEqual(items[3][3], True)  # nosort with ORDER BY -> ordered
        self.assertEqual(items[5][3], False)  # nosort without ORDER BY -> unordered
        self.assertEqual(stats["halted-files"], 1)
        self.assertEqual(stats["query-skipped-for-mysql"], 1)
        self.assertEqual(stats["statement-skipped-for-mysql"], 1)

    def test_candidates_chain_setups(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory) / "test"
            (root / "evidence").mkdir(parents=True)
            (root / "evidence" / "x.test").write_text(SCRIPT)
            output = pathlib.Path(directory) / "out.jsonl"
            with contextlib.redirect_stderr(io.StringIO()):
                SLT.main(["--source", directory, "--output", str(output), "--jobs", "1"])
            records = [json.loads(line) for line in output.read_text().splitlines()]
        ids = [r["id"] for r in records]
        self.assertEqual(ids, ["sqllogictest/evidence/x:L10", "sqllogictest/evidence/x:L29",
                               "sqllogictest/evidence/x:L34", "sqllogictest/evidence/x:L37"])
        first, second, error, constant = records
        self.assertEqual(first["setup"], ["CREATE TABLE t1(a INTEGER, b INTEGER)", "INSERT INTO t1 VALUES(1,2)"])
        self.assertNotIn("setup_base", first)
        self.assertEqual(second["setup_base"], first["id"])
        self.assertEqual(second["setup"], [])
        self.assertEqual(error["sql"], ["INSERT INTO nope VALUES(1)"])
        self.assertEqual(error["setup_base"], second["id"])
        # A query that reads no table needs no setup.
        self.assertEqual(constant["setup"], [])
        self.assertNotIn("setup_base", constant)
        for record in records:
            self.assertEqual(LIB.validate_candidate(record), [])
            self.assertEqual(record["setup_errors"], "drop")
            self.assertEqual(record["source"]["license"], "CC0-1.0")


def write_candidates(directory, records):
    path = pathlib.Path(directory) / "c.jsonl"
    path.write_text("".join(json.dumps(r) + "\n" for r in records))
    return path


def candidate(cid, setup, sql, base=None, ordered=True, policy="drop"):
    record = {
        "id": f"src/g:{cid}",
        "source": {"repo": "r", "revision": "v", "path": "t.test", "line": int(cid[1:]), "license": "MIT"},
        "group": "src/g",
        "setup": setup,
        "sql": sql,
        "ordered": ordered,
        "level": 100,
        "features": [],
        "setup_errors": policy,
    }
    if base:
        record["setup_base"] = f"src/g:{base}"
    return record


class RecorderPlanTests(unittest.TestCase):
    def plan(self, records):
        buckets = __import__("collections").Counter()
        details = __import__("collections").defaultdict(list)
        with tempfile.TemporaryDirectory() as directory:
            plans = REC.load_candidates([write_candidates(directory, records)], buckets, details)
        return plans["src/g"], buckets

    def test_setup_base_chains_build_one_trie_path(self):
        plan, buckets = self.plan([
            candidate("L1", ["CREATE TABLE t(a INT)", "INSERT INTO t VALUES(1)"], ["SELECT a FROM t"]),
            candidate("L2", ["DELETE FROM t"], ["SELECT a FROM t"], base="L1"),
            candidate("L3", [], ["SELECT a FROM t"], base="L2"),
            candidate("L4", ["CREATE TABLE t(a INT)", "INSERT INTO t VALUES(1)"], ["SELECT a FROM t"]),
            candidate("L5", [], ["INSERT INTO t VALUES(2)"], base="L1"),
        ])
        self.assertEqual(plan.statement, ["CREATE TABLE t(a INT)", "INSERT INTO t VALUES(1)", "DELETE FROM t"])
        self.assertEqual([c.id for c in plan.candidates], ["src/g:L1", "src/g:L2", "src/g:L5"])
        self.assertEqual([c.node for c in plan.candidates], [1, 2, 1])
        self.assertEqual(buckets["duplicate"], 2)  # L3 repeats L2, L4 repeats L1
        self.assertEqual([c.lane for c in plan.candidates], ["chain", "chain", "isolated"])

    def runs(self, results_a, results_b=None, errors_a=None, errors_b=None):
        return {
            "a": {"node_errors": errors_a or {}, "results": results_a},
            "b": {"node_errors": errors_b if errors_b is not None else (errors_a or {}),
                  "results": results_b if results_b is not None else results_a},
        }

    def test_classification_buckets(self):
        plan, _ = self.plan([
            candidate("L1", ["CREATE TABLE t(a INT)", "INSERT INTO nope VALUES(1)"], ["SELECT a FROM t"]),
            candidate("L2", [], ["SELECT b FROM t"], base="L1"),
            candidate("L3", [], ["SELECT a FROM t ORDER BY a"], base="L1"),
            candidate("L4", [], ["SELECT 1/0 FROM t"], base="L1", ordered=False),
            candidate("L5", [], ["SELECT CAST(a AS SIGNED) FROM t"], base="L1"),
            candidate("L6", [], ["SELECT 'a|b' FROM t"], base="L1"),
        ])
        errors = {"1": [1146, "42S02", "Table 'test.nope' doesn't exist"]}
        results_a = {
            "0": ["rows", [["1"]]],
            "1": ["error", 1054, "42S22", "Unknown column 'b'"],
            "2": ["rows", [["1"], ["2"]]],
            "3": ["rows", [["NULL"], ["NULL"]]],
            "4": ["error", 1690, "22003", "BIGINT value is out of range"],
            "5": ["rows", [["a|b"]]],
        }
        results_b = dict(results_a, **{"2": ["rows", [["2"], ["1"]]]})
        results_a["3"] = ["rows", [["b"], ["NULL"], ["a"]]]
        results_b["3"] = ["rows", [["a"], ["b"], ["NULL"]]]
        outcomes = REC.classify_group(plan, self.runs(results_a, results_b, errors), 1 << 20)
        self.assertEqual(outcomes[3]["lines"], ["NULL", "a", "b"])
        buckets = [outcomes[i]["bucket"] for i in range(6)]
        self.assertEqual(buckets, ["recorded", "error-quarantined", "nondeterministic", "recorded",
                                   "recorded", "unrepresentable"])
        self.assertEqual(outcomes[0]["dropped"], [1])
        self.assertEqual(outcomes[4]["kind"], "error")

    def test_dropped_statements_need_not_be_representable(self):
        trigger = "CREATE TRIGGER r AFTER INSERT ON t BEGIN SELECT 1; END"
        plan, _ = self.plan([candidate("L1", ["CREATE TABLE t(a INT)", trigger], ["SELECT a FROM t"])])
        dropped = {"1": [1064, "42000", "syntax"]}
        outcomes = REC.classify_group(plan, self.runs({"0": ["rows", []]}, None, dropped), 1 << 20)
        self.assertEqual(outcomes[0]["bucket"], "recorded")
        outcomes = REC.classify_group(plan, self.runs({"0": ["rows", []]}, None, {}), 1 << 20)
        self.assertEqual(outcomes[0]["bucket"], "unrepresentable")

    def test_diagnostics_after_dropped_statement_and_statistics(self):
        plan, _ = self.plan([
            candidate("L1", ["SET lock_warnings = 1"], ["SHOW WARNINGS"]),
            candidate("L2", ["CREATE TABLE t(a INT)"], ["SHOW WARNINGS"]),
            candidate("L3", [], ["SHOW KEYS FROM t"], base="L2"),
        ])
        errors = {"0": [1193, "HY000", "Unknown system variable 'lock_warnings'"]}
        rows = ["rows", [["Error", "1193", "x"]]]
        outcomes = REC.classify_group(plan, self.runs({"0": rows, "1": ["rows", []], "2": ["rows", []]},
                                                      None, errors), 1 << 20)
        self.assertEqual(outcomes[0]["bucket"], "setup-failed")
        self.assertEqual(outcomes[1]["bucket"], "recorded")
        self.assertEqual(outcomes[2]["bucket"], "nondeterministic")
        self.assertTrue(LIB.reads_diagnostics("SELECT @@warning_count"))
        self.assertFalse(LIB.reads_diagnostics("SELECT 'SHOW WARNINGS'"))

    def test_setup_failure_policy(self):
        plan, _ = self.plan([
            candidate("L1", ["CREATE TABLE t(a INT)", "BROKEN"], ["SELECT a FROM t"], policy="fail"),
        ])
        outcomes = REC.classify_group(
            plan, self.runs({"0": ["rows", []]}, None, {"1": [1064, "42000", "syntax"]}), 1 << 20)
        self.assertEqual(outcomes[0]["bucket"], "setup-failed")
        outcomes = REC.classify_group(
            plan, self.runs({"0": ["rows", []]}, None, {"1": [1064, "42000", "syntax"]}, {}), 1 << 20)
        self.assertEqual(outcomes[0]["bucket"], "nondeterministic")

    def test_rendered_shard_reads_back(self):
        plan, _ = self.plan([
            candidate("L1", ["CREATE TABLE t(a INT, b TEXT)", "INSERT INTO t VALUES(1, '{x}')",
                             "INSERT INTO nope VALUES(1)"], ["SELECT a, b FROM t ORDER BY a"]),
            candidate("L2", ["DELETE FROM t WHERE a = 1 -- gone"], ["SELECT a FROM t"], base="L1", ordered=False),
            candidate("L3", [], ["SELECT '\\\\}'"]),
            candidate("L4", [], ["SELECT 1/0 FROM t"], base="L2"),
            candidate("L5", [], ["SELECT SPACE(2)"], ordered=False),
        ])
        errors = {"2": [1146, "42S02", "missing"]}
        results = {
            "0": ["rows", [["1", "{x}"]]],
            "1": ["rows", []],
            "2": ["rows", [["\\}"]]],
            "3": ["error", 1365, "22012", "Division by 0"],
            "4": ["rows", [["  "]]],
        }
        outcomes = REC.classify_group(plan, self.runs(results, None, errors), 1 << 20)
        self.assertTrue(all(o["bucket"] == "recorded" for o in outcomes.values()))
        [(path, tests)] = REC.build_shards(plan, outcomes)
        self.assertEqual(path, "g.sqltest")
        header = REC.group_header(plan, tests)
        text, named = REC.render_shard(plan, tests, outcomes, header)
        self.assertEqual([name for _, name in named], ["l1", "l2", "l3", "l4", "l5"])
        self.assertIn("expect raw {\n  \n}", text)
        parsed = LIB.read_sqltest(text)
        setups = parsed["setups"]
        self.assertEqual(LIB.split_block_statements(setups["s1"]),
                         ["CREATE TABLE t(a INT, b TEXT)", "INSERT INTO t VALUES(1, '{x}')"])
        self.assertEqual(LIB.split_block_statements(setups["s2"]), ["DELETE FROM t WHERE a = 1 -- gone"])
        tests_by_name = {t["name"]: t for t in parsed["tests"]}
        self.assertEqual(tests_by_name["l1"]["setups"], ["s1"])
        self.assertEqual(tests_by_name["l1"]["expect_lines"], ["1|{x}"])
        self.assertEqual(tests_by_name["l2"]["setups"], ["s1", "s2"])
        self.assertEqual(tests_by_name["l2"]["expect_kind"], "unordered")
        self.assertEqual(tests_by_name["l2"]["expect_lines"], [])
        self.assertEqual(tests_by_name["l3"]["setups"], [])
        self.assertEqual(LIB.split_block_statements(tests_by_name["l3"]["sql"]), ["SELECT '\\\\}'"])
        self.assertEqual(tests_by_name["l3"]["expect_lines"], ["\\}"])
        self.assertEqual(tests_by_name["l4"]["expect_kind"], "error")
        self.assertIn("mysql-error: 1365 22012 Division by 0", tests_by_name["l4"]["comments"])
        self.assertEqual(tests_by_name["l4"]["setups"], ["s1", "s2"])
        self.assertEqual(tests_by_name["l5"]["expect_kind"], "raw")
        self.assertEqual(tests_by_name["l5"]["expect_lines"], ["  "])


class RecorderPolicyTests(unittest.TestCase):
    def test_test_names_for_ids_outside_their_group(self):
        plan = REC.GroupPlan("matrixone/dir/case.sql")
        record = candidate("L12", [], ["SELECT 1"])
        record["id"] = "matrixone/dir/case:L12"
        self.assertEqual(REC.test_name(REC.Candidate(record, -1, "chain"), plan), "l12")
        plan = REC.GroupPlan("vitess/evalengine/JSONExtract")
        record["id"] = "vitess/evalengine/JSONExtract:7"
        self.assertEqual(REC.test_name(REC.Candidate(record, -1, "chain"), plan), "t-7")

    def test_server_mutations_are_never_planned(self):
        buckets = __import__("collections").Counter()
        details = __import__("collections").defaultdict(list)
        with tempfile.TemporaryDirectory() as directory:
            plans = REC.load_candidates([write_candidates(directory, [
                candidate("L1", ["CREATE TABLE t(a INT)"], ["SET GLOBAL max_connections = 1"]),
                candidate("L2", ["SET GLOBAL sql_mode = ''"], ["SELECT 1 FROM t"]),
                candidate("L3", ["  CREATE TABLE t(a INT)  "], ["SELECT a FROM t\n"]),
            ])], buckets, details)
        plan = plans["src/g"]
        self.assertEqual(buckets["unsupported-environment"], 2)
        self.assertEqual([c.id for c in plan.candidates], ["src/g:L3"])
        self.assertEqual(plan.candidates[0].record["sql"], ["SELECT a FROM t"])
        self.assertEqual(plan.statement[plan.candidates[0].node], "CREATE TABLE t(a INT)")

    def test_local_only_follows_the_lock(self):
        args = __import__("argparse").Namespace(local=False)
        with tempfile.TemporaryDirectory() as directory:
            lock = pathlib.Path(directory) / "sources.lock.toml"
            lock.write_text('schema_version = 1\n[[source]]\nid = "a"\nspec_sources = ["src"]\n'
                            'redistribute_generated_cases = true\n[[source]]\nid = "b"\n'
                            'spec_sources = ["other"]\nredistribute_generated_cases = false\n')
            saved = REC.LOCK
            REC.LOCK = lock
            try:
                plan = REC.GroupPlan("src/g")
                plan.candidates.append(REC.Candidate(candidate("L1", [], ["SELECT 1"]), -1, "chain"))
                self.assertEqual(REC.local_only("src", [plan], args), (False, ""))
                self.assertTrue(REC.local_only("other", [plan], args)[0])
                self.assertTrue(REC.local_only("unknown", [plan], args)[0])
                self.assertTrue(REC.local_only("src", [plan], __import__("argparse").Namespace(local=True))[0])
                plan.candidates[0].record["source"]["license"] = "GPL-2.0-only"
                self.assertTrue(REC.local_only("src", [plan], args)[0])
            finally:
                REC.LOCK = saved


class ScoreTests(unittest.TestCase):
    def test_highest_cleared_level(self):
        items = [("a", {"level": 100}), ("b", {"level": 100}), ("c", {"level": 150}), ("d", {"level": 200})]
        self.assertEqual(SCORE.highest_cleared(items, {"a", "b", "c"}), 150)
        self.assertEqual(SCORE.highest_cleared(items, {"a", "c", "d"}), None)
        self.assertEqual(SCORE.highest_cleared(items, {"a", "b", "c", "d"}), 200)

    def test_regressions_and_ratchet(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            specs = root / "specs"
            (specs / "src" / "dir").mkdir(parents=True)
            manifest = {"shard": "dir/x.sqltest", "group": "src/x", "repo": "r", "revision": "v",
                        "license": "MIT", "paths": ["x"],
                        "tests": [[f"l{n}", f"src/x:L{n}", 100 + n, 0, n] for n in (1, 2, 3)]}
            (specs / "src" / "dir" / "manifest.jsonl").write_text(json.dumps(manifest) + "\n")
            results = {"files": [{"path": str(specs / "src/dir/x.sqltest"), "results": [
                {"name": "l1", "outcome": "passed"},
                {"name": "l2", "outcome": "failed", "reason": "boom"},
                {"name": "l3", "outcome": "passed"},
            ]}]}
            result_path = root / "r.json"
            result_path.write_text(json.dumps(results))
            baseline = root / "baseline.txt"
            baseline.write_text("src/dir/x.sqltest::l1\n")
            argv = [str(result_path), "--specs", str(specs), "--baseline", str(baseline)]

            def score(extra=()):
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    return SCORE.main(argv + list(extra))

            self.assertEqual(score(), 0)
            self.assertEqual(score(["--update"]), 0)
            self.assertEqual(baseline.read_text(), "src/dir/x.sqltest::l1\nsrc/dir/x.sqltest::l3\n")
            baseline.write_text("src/dir/x.sqltest::l1\nsrc/dir/x.sqltest::l2\n")
            self.assertEqual(score(), 1)
            self.assertEqual(score(["--update"]), 1)
            self.assertEqual(score(["--update", "--allow-shrink"]), 0)
            self.assertEqual(baseline.read_text(), "src/dir/x.sqltest::l1\nsrc/dir/x.sqltest::l3\n")


if __name__ == "__main__":
    unittest.main()
