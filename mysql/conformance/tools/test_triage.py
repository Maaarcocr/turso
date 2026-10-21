import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import triage  # noqa: E402


class DetectConstructTest(unittest.TestCase):
    def test_names_mysql_constructs(self):
        cases = {
            "SELECT 7 DIV 2": "DIV operator",
            "SELECT _utf8mb4'abc'": "charset introducer (_utf8mb4'..', _binary ..)",
            "SELECT DATE '2020-01-01'": "typed literal DATE/TIME/TIMESTAMP '..'",
            "SELECT X'40'": "hex literal X'..'/0x..",
            "SELECT 0b101": "bit literal b'..'/0b..",
            "SELECT 1 <=> NULL": "<=> operator",
            "CREATE TABLE t (a INT) ENGINE=InnoDB": "ENGINE= table option",
            "SHOW TABLES": "SHOW",
        }
        for sql, name in cases.items():
            self.assertEqual(triage.detect_construct(sql)[1], name, sql)

    def test_ignores_operators_inside_strings(self):
        self.assertIsNone(triage.detect_construct("SELECT 'a DIV b'"))


class ClassifyTest(unittest.TestCase):
    def test_missing_function(self):
        self.assertEqual(
            triage.classify_error("Parse error: no such function: CONV", "SELECT CONV('a',16,2)"),
            "function: CONV",
        )

    def test_syntax_error_names_the_construct(self):
        self.assertEqual(
            triage.classify_error('near "-": syntax error', "SELECT 15 DIV - 71"),
            "syntax: DIV operator",
        )

    def test_wrong_result_kinds(self):
        self.assertEqual(triage.classify_cell("0.5000", "0.5", "SELECT 1/2"), "DECIMAL scale/format (e.g. 0.5000)")
        self.assertEqual(triage.classify_cell("NULL", "1", ""), "NULL vs value")
        self.assertEqual(triage.classify_cell("ABC", "abc", ""), "case (collation/upper/lower)")
        self.assertEqual(triage.classify_cell("[1, 2]", "[1,2]", ""), "JSON formatting")
        self.assertEqual(triage.classify_values(["1", "2"], ["1"], ""), "row count")

    def test_missing_error_uses_recorded_errno(self):
        test = {"comments": ["mysql-error: 1690 22003 BIGINT value is out of range in '(1 + 2)'"]}
        result = {"outcome": "failed"}
        bucket = triage.classify_failure(result, "expected error but query succeeded", {}, test, "SELECT 1")
        self.assertEqual(bucket, "missing error: 1690 BIGINT value is out of range in '…'")

    def test_setup_failure_is_attributed_to_the_failing_statement(self):
        setups = {"s1": "CREATE TABLE t (a INT);\nCREATE TABLE u (a INT) ENGINE=InnoDB;"}
        reason = "setup 's1' failed: failed to execute SQL: unknown table option: ENGINE"
        bucket = triage.classify_failure({"outcome": "error"}, reason, setups, None, "SELECT 1")
        self.assertEqual(bucket, "ddl: table option ENGINE=")

    def test_family_skips_keywords(self):
        self.assertEqual(triage.family("specs/x/a.sqltest", "SELECT x FROM t WHERE a IN (SELECT LOWER(b) FROM u)"), "LOWER")
        self.assertEqual(triage.family("specs/vitess/evalengine/FnRound-2.sqltest", ""), "FnRound")


if __name__ == "__main__":
    unittest.main()
