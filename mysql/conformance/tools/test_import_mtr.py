"""Unit tests for the MySQL mysql-test (MTR) importer."""

import pathlib
import sys
import tempfile
import textwrap
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import file_replay  # noqa: E402
import import_mtr  # noqa: E402
import importer_common as common  # noqa: E402


def items(text):
    return [(i.kind, i.name, i.text, i.line) for i in import_mtr.read_script(textwrap.dedent(text))]


class Tree:
    """A temporary mysql-test directory."""

    def __init__(self, files):
        self.directory = tempfile.TemporaryDirectory()
        self.base = pathlib.Path(self.directory.name) / "mysql-test"
        for relative, text in files.items():
            path = self.base / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(textwrap.dedent(text))

    def statements(self, relative="t/a.test"):
        report = common.Report("test", {})
        profile = import_mtr.MtrProfile()
        statements = profile.read_statements(self.base / relative, relative, report)
        return statements, report, profile

    def candidates(self, relative="t/a.test"):
        report = common.Report("test", {})
        importer = file_replay.FileImporter(import_mtr.MtrProfile(), file_replay.Budget(), report)
        return importer.import_file(self.base / relative, relative), report

    def close(self):
        self.directory.cleanup()


class ReaderTests(unittest.TestCase):
    def test_commands_statements_and_blocks(self):
        self.assertEqual(items("""\
            # a comment
            --echo # also output only
            -- this is a comment, not a command
            --error ER_DUP_ENTRY
            INSERT INTO t1
              VALUES (1); SELECT 2;
            eval SELECT $a,
              $b;
            let $x = 1;
            if ($x)
            {
              SELECT 3;
            } else {
              SELECT 4;
            }
            while ($x) {
              dec $x;
            }
            --end
            """), [
            ("command", "echo", "# also output only", 2),
            ("command", "error", "ER_DUP_ENTRY", 4),
            ("sql", "", "INSERT INTO t1\n  VALUES (1)", 5),
            ("sql", "", "SELECT 2", 6),
            ("command", "eval", "SELECT $a,\n  $b", 7),
            ("command", "let", "$x = 1", 9),
            ("if", "", "$x", 10),
            ("sql", "", "SELECT 3", 12),
            ("end", "", "", 13),
            ("else", "", "", 13),
            ("sql", "", "SELECT 4", 14),
            ("end", "", "", 15),
            ("while", "", "$x", 16),
            ("command", "dec", "$x", 17),
            ("end", "", "", 18),
            ("end", "", "", 19),
        ])

    def test_delimiters_and_heredocs(self):
        self.assertEqual(items("""\
            DELIMITER |;
            CREATE PROCEDURE p() BEGIN SELECT 1; END|
            delimiter ;|
            --delimiter //
            SELECT 2//
            --delimiter ;
            --write_file $MYSQLTEST_VARDIR/x.txt
            SELECT 'not sql';
            EOF
            perl END_OF_PERL;
              print "SELECT 3;";
            END_OF_PERL
            SELECT 4;
            """), [
            ("sql", "", "CREATE PROCEDURE p() BEGIN SELECT 1; END", 2),
            ("sql", "", "SELECT 2", 5),
            ("command", "write_file", "$MYSQLTEST_VARDIR/x.txt", 7),
            ("command", "perl", "END_OF_PERL", 10),
            ("sql", "", "SELECT 4", 13),
        ])

    def test_long_statements_scan_linearly(self):
        rows = ",\n".join(f"  ({n}, 'a;b')" for n in range(20000))
        [(kind, _, text, line)] = items(f"INSERT INTO t VALUES\n{rows};\n")
        self.assertEqual((kind, line), ("sql", 1))
        self.assertTrue(text.endswith("(19999, 'a;b')"))

    def test_unbalanced_blocks_are_rejected(self):
        with self.assertRaises(import_mtr.FileExcluded):
            import_mtr.build_blocks(import_mtr.read_script("if ($a) {\nSELECT 1;\n"))
        with self.assertRaises(import_mtr.FileExcluded):
            import_mtr.build_blocks(import_mtr.read_script("}\n"))


class EvaluationTests(unittest.TestCase):
    def test_expand_follows_mysqltest_escapes(self):
        variables = {"a": "1", "name": "t1"}
        self.assertEqual(import_mtr.expand("SELECT $a FROM $name", variables), ("SELECT 1 FROM t1", True))
        self.assertEqual(import_mtr.expand("SELECT '\\$a', \"\\\\\"", variables), ("SELECT '$a', \"\\\"", True))
        # Unset variables read as "" (mysqltest), MTR's environment is unknown.
        self.assertEqual(import_mtr.expand("SELECT '$missing'", variables), ("SELECT ''", True))
        self.assertEqual(import_mtr.expand("SELECT $MYSQLTEST_VARDIR", variables), ("SELECT $MYSQLTEST_VARDIR", False))
        self.assertEqual(import_mtr.expand("SELECT $BIG_TEST", import_mtr.ENVIRONMENT), ("SELECT 1", True))
        self.assertEqual(import_mtr.expand("SELECT 5$", variables), ("SELECT 5$", True))

    def test_conditions(self):
        variables = {"a": "0", "b": "x", "n": "3"}
        evaluate = import_mtr.evaluate_condition
        self.assertFalse(evaluate("$a", variables))
        self.assertTrue(evaluate("!$a", variables))
        self.assertTrue(evaluate("$b", variables))
        self.assertTrue(evaluate("$b == x", variables))
        self.assertTrue(evaluate("$n > 2", variables))
        self.assertFalse(evaluate("$n <= 2", variables))
        self.assertIsNone(evaluate("$b < 2", variables))
        self.assertFalse(evaluate("$unset", variables))
        self.assertIsNone(evaluate("$MYSQL_TEST_DIR", variables))
        self.assertIsNone(evaluate("$c", {"c": None}))
        self.assertIsNone(evaluate("`SELECT 1`", variables))


class InterpreterTests(unittest.TestCase):
    def test_static_execution(self):
        tree = Tree({
            "t/a.test": """\
                --source include/have_innodb.inc
                let $engine = InnoDB;
                eval CREATE TABLE t1 (a INT) ENGINE=$engine;
                let $i = 2;
                while ($i)
                {
                  eval INSERT INTO t1 VALUES ($i);
                  dec $i;
                }
                --sorted_result
                SELECT a FROM t1;
                if (`SELECT COUNT(*) FROM t1`)
                {
                  INSERT INTO t1 VALUES (9);
                }
                if ($i)
                {
                  SELECT 'not run';
                }
                --source include/body.inc
                --disable_testcase BUG#1
                SELECT 'disabled';
                --enable_testcase
                --error ER_NO_SUCH_TABLE
                SELECT * FROM missing;
                eval SELECT $UNKNOWN_DIR;
                --replace_column 1 #
                let $v = `SELECT 5`;
                SELECT 'masked';
                let $w = `INSERT INTO t1 VALUES (6)`;
                eval SELECT '$unset';
                exit;
                SELECT 'after exit';
                """,
            "include/have_innodb.inc": "--skip never inlined\n",
            "include/body.inc": "SELECT COUNT(*) FROM t1;\nSELECT 2;\n",
        })
        try:
            statements, report, _ = tree.statements()
        finally:
            tree.close()
        got = [(s.sql, s.line, s.ordinal_on_line, s.skip, s.expect_error, s.sorted_hint, s.masked) for s in statements]
        self.assertEqual(got, [
            ("CREATE TABLE t1 (a INT) ENGINE=InnoDB", 3, 1, "", False, False, ""),
            ("INSERT INTO t1 VALUES (2)", 5, 1, "", False, False, ""),
            ("INSERT INTO t1 VALUES (1)", 5, 2, "", False, False, ""),
            ("SELECT a FROM t1", 11, 1, "", False, True, ""),
            ("INSERT INTO t1 VALUES (9)", 14, 1, "undecidable condition", False, False, ""),
            ("SELECT COUNT(*) FROM t1", 20, 1, "", False, False, ""),
            ("SELECT 2", 20, 2, "", False, False, ""),
            ("SELECT * FROM missing", 25, 1, "", True, False, ""),
            ("SELECT $UNKNOWN_DIR", 26, 1, "unresolved variable", False, False, ""),
            ("SELECT 5", 28, 1, "", False, False, ""),
            ("SELECT 'masked'", 29, 1, "", False, False, "--replace_column"),
            ("INSERT INTO t1 VALUES (6)", 30, 1, "query in a mysqltest variable", False, False, ""),
            ("SELECT ''", 31, 1, "", False, False, ""),
        ])
        self.assertEqual(report.counts["requirement includes met by the oracle"], 1)
        self.assertEqual(report.counts["includes inlined"], 1)

    def test_file_exclusions(self):
        cases = {
            "connect (con1,localhost,root,,);\n": "multiple connections",
            "--connection con1\n": "multiple connections",
            "--exec echo hi\n": "runs shell commands",
            "--source include/have_debug.inc\n": "needs a debug build",
            "--source include/rpl/init_source_replica.inc\n": "replication or binary log",
            "--source include/restart_mysqld.inc\n": "restarts the server",
            "--sleep 1\n": "sleeps",
            "--skip Needs something\n": "skipped by the test itself",
            "if (`SELECT 1`) {\n  --skip Needs something\n}\n": "runtime requirement check in the test",
            "--source include/have_feature.inc\n": "runtime requirement check in an include",
            "--source $file\n": "sources a file named by an unresolved variable",
            "--source suite/innodb/include/x.inc\n": "sources a file outside the selected suites",
            "--source include/nope.inc\n": "sources a missing file",
            "if ($a) {\n": "unbalanced mysqltest blocks",
            "let $i = 30000;\nwhile ($i) {\n  dec $i;\n}\n": "loops over 20000 iterations",
        }
        for text, reason in cases.items():
            with self.subTest(text=text):
                tree = Tree({"t/a.test": text, "include/have_feature.inc":
                             "if (`SELECT COUNT(*) = 0 FROM x`) {\n  --skip Needs feature\n}\n",
                             "suite/innodb/include/x.inc": "SELECT 1;\n"})
                try:
                    statements, report, _ = tree.statements()
                finally:
                    tree.close()
                self.assertIsNone(statements)
                self.assertEqual(report.filtered, {f"excluded file ({reason})": 1})
                self.assertEqual(report.counts["files excluded"], 1)

    def test_sleeps_under_undecidable_conditions_and_harmless_options_are_kept(self):
        tree = Tree({
            "t/a.test": "if (`SELECT 1`) {\n  --sleep 1\n}\nSELECT 1;\n",
            "t/a-master.opt": "--skip-log-bin --secure-file-priv=$MYSQLTEST_VARDIR\n",
            "t/b.test": "SELECT 1;\n",
            "t/b-master.opt": "--character-set-server=latin1\n",
        })
        try:
            statements, _, _ = tree.statements("t/a.test")
            self.assertEqual([s.sql for s in statements], ["SELECT 1"])
            statements, report, _ = tree.statements("t/b.test")
            self.assertIsNone(statements)
            self.assertEqual(report.filtered, {"excluded file (server options)": 1})
        finally:
            tree.close()


class ProfileTests(unittest.TestCase):
    def test_suite_selection(self):
        profile = import_mtr.MtrProfile()
        self.assertEqual(profile.exclusion("t/select.test"), "")
        self.assertEqual(profile.exclusion("suite/gis/t/x.test"), "")
        self.assertEqual(profile.exclusion("suite/engines/funcs/t/x.test"), "")
        self.assertIsNone(profile.exclusion("suite/rpl/t/x.test"))
        self.assertIsNone(profile.exclusion("r/x.test"))
        self.assertEqual(profile.exclusion("include/x.test"), "not a test directory")
        self.assertTrue(import_mtr.fetched_path("suite/gis/include/x.inc"))
        self.assertFalse(import_mtr.fetched_path("suite/innodb/include/x.inc"))
        self.assertEqual(profile.report_directory("suite/gis/t/x.test"), "suite/gis")
        self.assertEqual(profile.report_directory("t/x.test"), "t")

    def test_lock_fetches_exactly_the_imported_paths(self):
        import tomllib

        lock = tomllib.loads((pathlib.Path(__file__).resolve().parents[1] / "corpus/sources.lock.toml").read_text())
        [entry] = [e for e in lock["source"] if "mysql-mtr" in e.get("spec_sources", [])]
        self.assertEqual(entry["paths"], import_mtr.sparse_paths())
        self.assertEqual(entry["revision"], import_mtr.MtrProfile.revision)
        self.assertEqual(entry["license"], import_mtr.MtrProfile.license)
        self.assertFalse(entry["redistribute_generated_cases"])

    def test_candidates_from_a_test_file(self):
        tree = Tree({
            "t/a.test": """\
                CREATE TABLE t1 (a INT);
                let $i = 2;
                while ($i) {
                  eval INSERT INTO test.t1 VALUES ($i);
                  dec $i;
                }
                if (`SELECT 1`) {
                  CREATE TABLE t2 (b INT);
                }
                SELECT a FROM t1 ORDER BY a;
                SELECT b FROM t2;
                --source include/q.inc
                """,
            "include/q.inc": "SELECT COUNT(*) FROM t1;\nSELECT 7;\n",
        })
        try:
            candidates, report = tree.candidates()
        finally:
            tree.close()
        by_id = {c["id"]: c for c in candidates}
        self.assertEqual(sorted(by_id), ["mysql-mtr/t/a:L10", "mysql-mtr/t/a:L12", "mysql-mtr/t/a:L12.2"])
        first = by_id["mysql-mtr/t/a:L10"]
        self.assertEqual(first["setup"], ["CREATE TABLE t1 (a INT)", "INSERT INTO t1 VALUES (2)",
                                          "INSERT INTO t1 VALUES (1)"])
        self.assertEqual(first["source"], {"repo": import_mtr.MtrProfile.repo, "revision": import_mtr.MtrProfile.revision,
                                           "path": "mysql-test/t/a.test", "line": 10, "license": "GPL-2.0-only"})
        self.assertEqual(first["group"], "mysql-mtr/t/a.test")
        self.assertEqual(by_id["mysql-mtr/t/a:L12.2"]["setup"], [])
        self.assertEqual(
            report.filtered["depends on an object modified by a statement that is not statically replayable"], 1)


if __name__ == "__main__":
    unittest.main()
