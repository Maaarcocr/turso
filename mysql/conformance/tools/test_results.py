"""Unit tests for results-only recording and materialization (no MySQL server needed)."""

import argparse
import collections
import contextlib
import importlib.util
import io
import json
import pathlib
import re
import tempfile
import unittest

TOOLS = pathlib.Path(__file__).resolve().parent
CONFORMANCE = TOOLS.parent


def load(name):
    spec = importlib.util.spec_from_file_location(name, TOOLS / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


REC = load("record_mysql")
MAT = load("materialize")
SCORE = load("score")

RESULT_KEYS = {"id", "sha256", "ordered", "bucket", "rows", "raw", "error", "dropped", "detail"}
META_KEYS = {"format", "source", "lock_id", "repository", "revision", "license", "importer", "import_report",
             "candidates_sha256", "candidates", "recorder_version", "oracle", "server", "max_rows",
             "max_expect_bytes"}
BUCKETS = set(REC.BUCKET_ORDER)


def candidate(cid, setup, sql, base=None, ordered=True, policy="drop", group="src/g"):
    record = {
        "id": f"{group}:{cid}",
        "source": {"repo": "r", "revision": "v", "path": "t.test", "line": int(cid[1:]), "license": "MIT"},
        "group": group,
        "setup": setup,
        "sql": sql,
        "ordered": ordered,
        "level": 100,
        "features": ["select"],
        "setup_errors": policy,
    }
    if base:
        record["setup_base"] = f"{group}:{base}"
    return record


def write_jsonl(path, records):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(r) + "\n" for r in records))
    return path


def load_plans(paths):
    buckets = collections.Counter()
    details = collections.defaultdict(list)
    plans = REC.load_candidates(paths, buckets, details)
    return [plans[name] for name in sorted(plans)], buckets, details


RECORDS = [
    candidate("L1", ["CREATE TABLE t(a INT, b TEXT)", "INSERT INTO t VALUES(1, 'x')", "INSERT INTO nope VALUES(1)"],
              ["SELECT a, b FROM t ORDER BY a"]),
    candidate("L2", ["DELETE FROM t WHERE a = 2"], ["SELECT a FROM t"], base="L1", ordered=False),
    candidate("L3", [], ["SELECT SPACE(2)"]),
    candidate("L4", [], ["SELECT 1/0 FROM t"], base="L1"),
    candidate("L5", [], ["SELECT RAND() FROM t"], base="L1"),
    candidate("L6", [], ["SELECT b FROM nope"], base="L1"),
    candidate("L7", [], ["SELECT 'a|b'"]),
    candidate("L8", ["CREATE TABLE u(a INT PRIMARY KEY)", "INSERT INTO u VALUES(1)", "INSERT INTO u VALUES(1)"],
              ["SELECT a FROM u"], policy="fail"),
]
# Two runs as the recorder caches them: setup node errors and per-candidate results.
NODE_ERRORS = {"2": [1146, "42S02", "Table 'test.nope' doesn't exist"],
               "6": [1062, "23000", "Duplicate entry '1' for key 'u.PRIMARY' " + "x" * 90]}
RESULTS_A = {
    "0": ["rows", [["1", "x"]]],
    "1": ["rows", [["1"]]],
    "2": ["rows", [["  "]]],
    "3": ["error", 1365, "22012", "Division by 0"],
    "4": ["rows", [["0.1"]]],
    "5": ["error", 1146, "42S02", "Table 'test.nope' doesn't exist"],
    "6": ["rows", [["a|b"]]],
    "7": ["rows", [["1"]]],
}
RUNS = {"a": {"node_errors": NODE_ERRORS, "results": RESULTS_A},
        "b": {"node_errors": NODE_ERRORS, "results": dict(RESULTS_A, **{"4": ["rows", [["0.7"]]]})}}


class Fixture:
    """Candidates, a direct recording and its results in a temporary tree."""

    def __init__(self, directory, records=RECORDS):
        self.root = pathlib.Path(directory)
        self.candidates = self.root / "candidates"
        self.files = [write_jsonl(self.candidates / "src" / "src-00000.jsonl", records)]
        (self.candidates / "src" / "IMPORT_REPORT.md").write_text("# import\n\n- revision: v\n")
        self.results = self.root / "results"
        self.direct = self.root / "direct"
        plans, buckets, details = load_plans(self.files)
        self.plans = plans
        outcomes = {plan.name: REC.classify_group(plan, RUNS, 1 << 20) for plan in plans}
        withheld = REC.withhold_sql_echoes(plans, outcomes, REC.candidate_statements(self.files))
        stats = REC.new_stats(buckets, details, 100, 1 << 14)
        for plan in plans:
            REC.tally_outcomes(plan, outcomes[plan.name], RUNS, stats)
        out = self.direct / "src"
        out.mkdir(parents=True)
        REC.write_specs(out, plans, outcomes, stats)
        server = {"version": "8.4.10", "sql_mode": "M", "character_set_server": "utf8mb4",
                  "collation_server": "c", "time_zone": "+00:00", "lower_case_table_names": 0}
        report = (self.candidates / "src" / "IMPORT_REPORT.md").read_text()
        REC.write_report(out / "REPORT.md", "src", stats, report, server)
        entry = {"id": "lock-src", "repository": "r", "revision": "v", "license": "MIT",
                 "importers": {"src": ["tools/import_src.py", "--source", "{checkout}", "--output", "{candidates}"]}}
        args = argparse.Namespace(candidates=self.files, max_rows=100, max_expect_bytes=1 << 14)
        meta = REC.results_meta("src", entry, plans, args, server, True)
        REC.write_results(self.results / "src", plans, outcomes, {p.name: RUNS for p in plans}, meta)
        (self.results / "src" / "REPORT.md").write_bytes((out / "REPORT.md").read_bytes())
        self.outcomes = outcomes
        self.withheld = withheld

    def materialize(self, *extra):
        argv = ["src", "--results-dir", str(self.results), "--out-dir", str(self.root / "mat"),
                "--candidates", str(self.candidates), "--reuse-candidates", *extra]
        with contextlib.redirect_stdout(io.StringIO()) as out, contextlib.redirect_stderr(io.StringIO()):
            status = MAT.main(argv)
        return status, out.getvalue()


def tree(directory):
    return {p.relative_to(directory).as_posix(): p.read_bytes() for p in sorted(directory.rglob("*")) if p.is_file()}


class ResultsFormatTests(unittest.TestCase):
    def test_outcomes_round_trip_through_results_records(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Fixture(directory)
            [plan] = fixture.plans
            outcomes = fixture.outcomes[plan.name]
            buckets = [outcomes[c.index]["bucket"] for c in plan.candidates]
            self.assertEqual(buckets, ["recorded", "recorded", "recorded", "recorded", "nondeterministic",
                                       "error-quarantined", "unrepresentable", "setup-failed"])
            entries = REC.read_results(fixture.results / "src")
            self.assertEqual(len(entries), len(plan.candidates))
            node_errors = {}
            for c in plan.candidates:
                entry = json.loads(json.dumps(entries[c.id]))
                outcome, errors = REC.outcome_from_result(plan, c, entry)
                node_errors.update(errors)
                original = outcomes[c.index]
                if original["bucket"] == "recorded":
                    self.assertEqual(outcome, original)
                else:
                    self.assertEqual(outcome["bucket"], original["bucket"])
            # The dropped statement is stored by position in the effective setup.
            first = entries["src/g:L1"]
            self.assertEqual(first["dropped"], [[2, 1146]])
            self.assertEqual(node_errors, {2: 1146})
            self.assertEqual(entries["src/g:L3"], {"id": "src/g:L3", "sha256": entries["src/g:L3"]["sha256"],
                                                   "ordered": True, "bucket": "recorded", "rows": ["  "],
                                                   "raw": True})
            self.assertEqual(entries["src/g:L4"]["error"], [1365, "22012", "Division by 0"])
            self.assertEqual(len(entries["src/g:L8"]["detail"]), 80)
            self.assertEqual(entries["src/g:L6"]["detail"], "1146")

    def test_results_records_carry_no_sql(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Fixture(directory)
            text = "".join(p.read_text() for p in sorted((fixture.results / "src").rglob("*.jsonl")))
            for record in RECORDS:
                for statement in record["setup"] + record["sql"]:
                    self.assertNotIn(statement, text)
            for line in text.split("\n")[:-1]:
                entry = json.loads(line)
                self.assertLessEqual(set(entry), RESULT_KEYS)
                self.assertRegex(entry["sha256"], r"^[0-9a-f]{64}$")
            meta = json.loads((fixture.results / "src" / "meta.json").read_text())
            self.assertEqual(set(meta), META_KEYS)

    def test_digest_covers_setup_sql_order_and_setup_policy(self):
        base = REC.candidate_digest(["CREATE TABLE t(a INT)"], ["SELECT a FROM t"], True, "drop")
        self.assertRegex(base, r"^[0-9a-f]{64}$")
        self.assertNotEqual(base, REC.candidate_digest(["CREATE TABLE t(a INT )"], ["SELECT a FROM t"], True, "drop"))
        self.assertNotEqual(base, REC.candidate_digest(["CREATE TABLE t(a INT)"], ["SELECT a FROM t "], True, "drop"))
        self.assertNotEqual(base, REC.candidate_digest(["CREATE TABLE t(a INT)"], ["SELECT a FROM t"], False, "drop"))
        self.assertNotEqual(base, REC.candidate_digest(["CREATE TABLE t(a INT)"], ["SELECT a FROM t"], True, "fail"))
        self.assertNotEqual(base, REC.candidate_digest([], ["CREATE TABLE t(a INT)", "SELECT a FROM t"], True, "drop"))

    def test_results_echoing_source_sql_are_withheld(self):
        records = [
            candidate("L1", [], ["SELECT INFO FROM information_schema.PROCESSLIST"]),
            candidate("L2", ["CREATE TABLE t(a INT)"], ["SHOW WARNINGS"]),
            candidate("L3", [], ["SELECT 'BEGIN'"]),
            candidate("L4", [], ["SELECT 'cost(select * from t1)'"]),
            candidate("L5", [], ["SELECT 'abc'"]),
            candidate("L6", [], ["select * from t1"]),
        ]
        path = write_jsonl(pathlib.Path(tempfile.mkdtemp()) / "c.jsonl", records)
        plans, _, _ = load_plans([path])
        [plan] = plans

        def rows(*lines):
            return {"bucket": "recorded", "kind": "rows", "lines": list(lines), "dropped": [], "raw": False}

        outcomes = {
            0: rows("SELECT  INFO FROM information_schema.PROCESSLIST"),  # its own statement
            1: rows("Warning|1064|Optimizer hint syntax error near 'TABLE t(a INT)' at line 1"),  # quoted SQL
            2: rows("BEGIN"),  # too short to count
            3: rows("cost(select * from t1)"),  # another candidate's statement inside a string
            4: {"bucket": "recorded", "kind": "error", "error": [1, "x", "near 'abc' at line 1"], "dropped": []},
            5: rows("1"),
        }
        by_group = {plan.name: dict(outcomes)}
        self.assertEqual(REC.withhold_sql_echoes(plans, by_group, REC.candidate_statements([path])), 3)
        self.assertEqual([by_group[plan.name][i]["bucket"] for i in range(6)],
                         ["echoes-sql", "echoes-sql", "recorded", "echoes-sql", "recorded", "recorded"])
        self.assertTrue(REC.quotes_sql("x near 'abcdefghijkl' at line 2"))
        self.assertFalse(REC.quotes_sql("x near 'abcdefghijk' at line 2"))

    def test_find_sql_echoes_scans_every_statement(self):
        entries = [{"id": "a", "rows": ["1|x", "SELECT b FROM t WHERE c = 1|2"]},
                   {"id": "b", "error": [1, "x", "nothing to see here"]},
                   {"id": "c", "detail": "1062 CREATE TABLE u(a INT) said hi"},
                   {"id": "d", "rows": ["BEGIN"]}]
        statements = ["SELECT b FROM t WHERE c = 1", "CREATE TABLE u(a INT)", "BEGIN", "SELECT  x FROM y"]
        self.assertEqual(REC.find_sql_echoes(entries, statements),
                         [("a", "SELECT b FROM t WHERE c = 1"), ("c", "CREATE TABLE u(a INT)")])
        self.assertEqual(REC.find_sql_echoes([{"id": "e", "rows": ["SELECT x FROM y"]}], statements),
                         [("e", "SELECT x FROM y")])
        self.assertEqual(REC.find_sql_echoes([{"id": "f", "rows": ["near 'SELECT b FROM' at line 1"]}], []),
                         [("f", "quoted SQL text")])

    def test_results_only_needs_a_non_redistributable_lock_entry_with_an_importer(self):
        with tempfile.TemporaryDirectory() as directory:
            lock = pathlib.Path(directory) / "sources.lock.toml"
            lock.write_text(
                'schema_version = 1\n'
                '[[source]]\nid = "a"\nrepository = "r"\nrevision = "v"\nlicense = "MIT"\n'
                'spec_sources = ["open"]\nredistribute_generated_cases = true\n'
                '[[source]]\nid = "b"\nrepository = "r"\nrevision = "v"\nlicense = "MIT"\n'
                'spec_sources = ["closed", "noimporter"]\nredistribute_generated_cases = false\n'
                'importers = { closed = ["tools/x.py"] }\n')
            saved = REC.LOCK
            REC.LOCK = lock
            try:
                plans, _, _ = load_plans([write_jsonl(pathlib.Path(directory) / "c.jsonl", RECORDS[:1])])
                self.assertIn("allows redistribution", REC.results_only_entry("open", plans)[1])
                self.assertEqual(REC.results_only_entry("closed", plans), (REC.lock_entry("closed"), None))
                self.assertIn("no importers.noimporter", REC.results_only_entry("noimporter", plans)[1])
                self.assertIn("no sources.lock.toml entry", REC.results_only_entry("unknown", plans)[1])
                plans[0].candidates[0].record["source"]["license"] = "GPL-2.0-only"
                self.assertIsNone(REC.results_only_entry("open", plans)[0])  # still no importer
            finally:
                REC.LOCK = saved


class MaterializeTests(unittest.TestCase):
    def test_materialize_reproduces_the_direct_recording(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Fixture(directory)
            status, output = fixture.materialize("--strict")
            self.assertEqual(status, 0, output)
            self.assertIn("8 results used, 0 stale, 0 missing", output)
            self.assertIn("candidates identical, REPORT.md identical", output)
            self.assertEqual(tree(fixture.root / "mat" / "src"), tree(fixture.direct / "src"))

    def test_stale_and_missing_results_are_skipped(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Fixture(directory)
            changed = [dict(r) for r in RECORDS]
            changed[3] = candidate("L4", [], ["SELECT 2/0 FROM t"], base="L1")  # sql differs from the recording
            changed.append(candidate("L9", [], ["SELECT 9"]))  # never recorded
            write_jsonl(fixture.candidates / "src" / "src-00000.jsonl", changed)
            status, output = fixture.materialize()
            self.assertEqual(status, 0)
            self.assertIn("7 results used, 1 stale, 1 missing, 0 orphaned", output)
            self.assertIn("candidates changed, REPORT.md changed", output)
            self.assertEqual(fixture.materialize("--strict")[0], 1)
            text = (fixture.root / "mat" / "src" / "g.sqltest").read_text()
            self.assertNotIn("test l4 ", text)
            self.assertNotIn("test l9 ", text)
            self.assertIn("test l1 ", text)
            report = (fixture.root / "mat" / "src" / "REPORT.md").read_text()
            self.assertIn("| stale-result | 1 |", report)
            self.assertIn("| no-result | 1 |", report)

    def test_orphaned_results_are_counted(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Fixture(directory)
            write_jsonl(fixture.candidates / "src" / "src-00000.jsonl", RECORDS[:2])
            status, output = fixture.materialize()
            self.assertIn("2 results used, 0 stale, 0 missing, 6 orphaned", output)

    def test_importer_must_be_a_conformance_tool(self):
        meta = {"source": "src", "importer": ["../../evil.py"]}
        with self.assertRaises(SystemExit):
            MAT.run_importer(meta, pathlib.Path("/nonexistent"), pathlib.Path(tempfile.mkdtemp()) / "c")


class CommittedResultsTests(unittest.TestCase):
    """Checks every committed results/<source>/ directory."""

    def sources(self):
        return sorted(path.parent for path in (CONFORMANCE / "results").glob("*/meta.json"))

    def test_committed_results_are_well_formed_and_sql_free(self):
        for directory in self.sources():
            with self.subTest(source=directory.name):
                meta = json.loads((directory / "meta.json").read_text())
                self.assertEqual(set(meta), META_KEYS)
                self.assertEqual(meta["source"], directory.name)
                self.assertTrue((directory / "REPORT.md").exists())
                count = 0
                for path in sorted(directory.rglob("*.jsonl")):
                    for line in path.read_text(encoding="utf-8").split("\n")[:-1]:
                        entry = json.loads(line)
                        count += 1
                        self.assertLessEqual(set(entry), RESULT_KEYS, line[:200])
                        self.assertTrue(entry["id"].startswith(directory.name + "/"), entry["id"])
                        self.assertRegex(entry["sha256"], r"^[0-9a-f]{64}$")
                        self.assertIn(entry["bucket"], BUCKETS)
                        self.assertFalse(any(REC.quotes_sql(text) for text in REC.result_texts(entry)), line[:300])
                self.assertEqual(count, meta["candidates"])

    def test_committed_results_contain_no_candidate_sql(self):
        """Needs the source's candidates (make -C mysql/conformance materialize-local)."""
        checked = 0
        for directory in self.sources():
            meta = json.loads((directory / "meta.json").read_text())
            files = sorted((CONFORMANCE / "candidates" / directory.name).glob("*.jsonl"))
            if not files or REC.candidates_digest(files) != meta["candidates_sha256"]:
                continue
            statements = set()
            for path in files:
                for line in path.read_text(encoding="utf-8").split("\n")[:-1]:
                    record = json.loads(line)
                    statements.update(record["setup"])
                    statements.update(record["sql"])
            entries = REC.read_results(directory).values()
            with self.subTest(source=directory.name):
                self.assertEqual(REC.find_sql_echoes(entries, statements), [])
            checked += 1
        if not checked:
            self.skipTest("no source's candidates are imported; run make -C mysql/conformance materialize-local")


class ScoreLocalTests(unittest.TestCase):
    def test_local_specs_are_scored_only_with_local(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            specs = root / "specs"
            for source in ("open", "local/closed"):
                (specs / source).mkdir(parents=True)
                name = source.rsplit("/", 1)[-1]
                manifest = {"shard": "x.sqltest", "group": f"{name}/x", "repo": "r", "revision": "v",
                            "license": "MIT", "paths": ["x"], "tests": [["l1", f"{name}/x:L1", 100, 0, 1]]}
                (specs / source / "manifest.jsonl").write_text(json.dumps(manifest) + "\n")
            results = {"files": [{"path": str(specs / "open/x.sqltest"), "results": [{"name": "l1", "outcome": "passed"}]},
                                 {"path": str(specs / "local/closed/x.sqltest"),
                                  "results": [{"name": "l1", "outcome": "passed"}]}]}
            report = root / "r.json"
            report.write_text(json.dumps(results))
            baseline = root / "baseline.txt"
            local_baseline = root / "baseline-local.txt"
            local_baseline.write_text("# local\n")

            def score(argv):
                with contextlib.redirect_stdout(io.StringIO()) as out:
                    status = SCORE.main([str(report), *argv])
                return status, out.getvalue()

            status, output = score(["--specs", str(specs), "--baseline", str(baseline), "--update"])
            self.assertEqual(status, 0)
            self.assertEqual(baseline.read_text(), "open/x.sqltest::l1\n")
            self.assertNotIn("closed", output)
            status, output = score(["--local", "--specs", str(specs / "local"), "--baseline", str(local_baseline),
                                    "--update"])
            self.assertEqual(status, 0)
            self.assertEqual(local_baseline.read_text(), "# local\nclosed/x.sqltest::l1\n")


if __name__ == "__main__":
    unittest.main()
