import json
import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import go_source  # noqa: E402
import importer_common as common  # noqa: E402


class SqlLexingTests(unittest.TestCase):
    def test_split_respects_quotes_comments_and_custom_delimiters(self):
        text = "select 'a;b', `c;d` -- e;f\n; select \"g;h\" /* i;j */; select 3"
        self.assertEqual(
            [sql for sql, _offset in common.split_statements(text)],
            ["select 'a;b', `c;d` -- e;f", 'select "g;h" /* i;j */', "select 3"],
        )
        procedure = "create procedure p() begin select 1; end$$ select 2$$"
        self.assertEqual(
            [sql for sql, _offset in common.split_statements(procedure, "$$")],
            ["create procedure p() begin select 1; end", "select 2"],
        )

    def test_mysql_comment_rules(self):
        # "--" starts a comment only when followed by whitespace.
        self.assertEqual(common.strip_comments("select 1--1"), "select 1--1")
        self.assertEqual(common.strip_comments("select 1 -- one\n"), "select 1")
        self.assertEqual(common.strip_comments("select /*+ BKA(t) */ 1 /* x */"), "select /*+ BKA(t) */ 1")

    def test_top_level_order_by(self):
        self.assertTrue(common.has_top_level_order_by("SELECT a FROM t ORDER BY a"))
        self.assertFalse(common.has_top_level_order_by("SELECT * FROM (SELECT a FROM t ORDER BY a) d"))
        self.assertTrue(common.has_top_level_order_by("SELECT a FROM t UNION SELECT b FROM u ORDER BY 1"))
        self.assertFalse(common.has_top_level_order_by("SELECT a FROM t ORDER BY a UNION SELECT b FROM u"))
        self.assertFalse(common.has_top_level_order_by("SELECT 'order by'"))

    def test_first_keyword_skips_parentheses_and_comments(self):
        self.assertEqual(common.first_keyword("/* c */ (SELECT 1) UNION (SELECT 2)"), "select")
        self.assertEqual(common.first_keyword("  insert into t values (1)"), "insert")


class ClassificationTests(unittest.TestCase):
    def test_levels_follow_the_curriculum_order(self):
        literal, _ = common.classify(["SELECT 1"])
        arithmetic, _ = common.classify(["SELECT 1 + 2"])
        string, _ = common.classify(["SELECT CONCAT('a', 'b')"])
        temporal, _ = common.classify(["SELECT DATE_ADD(DATE '2020-01-01', INTERVAL 1 DAY)"])
        join, _ = common.classify(["SELECT * FROM a JOIN b ON a.x = b.x"])
        window, _ = common.classify(["SELECT ROW_NUMBER() OVER (ORDER BY a) FROM t"])
        self.assertLess(literal, arithmetic)
        self.assertLess(arithmetic, string)
        self.assertLess(string, temporal)
        self.assertLess(temporal, join)
        self.assertLess(join, window)
        self.assertTrue(all(0 <= level <= 1000 for level in (literal, window)))

    def test_features_include_setup_requirements(self):
        level, features = common.classify(["SELECT a FROM t"], ["CREATE TABLE t(a JSON)"])
        self.assertIn("type:json", features)
        self.assertIn("from", features)
        self.assertGreaterEqual(level, common.FEATURE_LEVELS["type:json"])

    def test_wildcards_are_not_arithmetic(self):
        _, features = common.classify(["SELECT COUNT(*), t.* FROM t"])
        self.assertNotIn("operator:arithmetic", features)
        self.assertIn("aggregate", features)


class CandidateOutputTests(unittest.TestCase):
    def test_candidate_shape_and_shards(self):
        candidate = common.make_candidate(
            candidate_id="src/file:L1",
            repo="https://example.invalid/repo.git",
            revision="abc",
            path="t/file.test",
            line=1,
            license="Apache-2.0",
            group="src/file",
            setup=["CREATE TABLE t(a INT) "],
            sql=[" SELECT a FROM t ORDER BY a"],
            ordered=True,
            setup_errors="drop",
        )
        self.assertEqual(
            sorted(candidate),
            ["features", "group", "id", "level", "ordered", "setup", "setup_errors", "source", "sql"],
        )
        self.assertEqual(candidate["setup"], ["CREATE TABLE t(a INT)"])
        self.assertEqual(candidate["sql"], ["SELECT a FROM t ORDER BY a"])
        self.assertEqual(
            sorted(candidate["source"]), ["license", "line", "path", "repo", "revision"]
        )
        with tempfile.TemporaryDirectory() as directory:
            second = dict(candidate, id="src/file:L2")
            common.write_candidates(directory, "src", [candidate, second], shard_size=1)
            shards = sorted(pathlib.Path(directory).glob("*.jsonl"))
            self.assertEqual([path.name for path in shards], ["src-00000.jsonl", "src-00001.jsonl"])
            self.assertEqual(json.loads(shards[1].read_text())["id"], "src/file:L2")
            with self.assertRaises(ValueError):
                common.write_candidates(directory, "src", [candidate, candidate])

    def test_report_renders_counts_and_filters(self):
        report = common.Report("Example import", {"repository": "r"})
        report.count("files scanned", 2)
        report.filter("explain", 3)
        report.filter("explain")
        text = report.render()
        self.assertIn("| files scanned | 2 |", text)
        self.assertIn("| explain | 4 |", text)
        self.assertIn("| **total** | 4 |", text)


class GoStringTests(unittest.TestCase):
    def test_byte_escapes_decode_as_utf8(self):
        # The reviewer's case: three byte escapes form U+FFFD, not three code points.
        self.assertEqual(go_source.decode_string(r'"\xef\xbf\xbd"'), "\ufffd")
        self.assertEqual(go_source.decode_string(r'"\303\251"'), "\u00e9")
        self.assertEqual(go_source.decode_string(r'"\u00e9\U0001F600\t\"\\"'), "\u00e9\U0001F600\t\"\\")
        self.assertEqual(go_source.decode_string("`raw \\x41\r\n`"), "raw \\x41\n")
        with self.assertRaises(UnicodeDecodeError):
            go_source.decode_string(r'"\xff"')
        self.assertEqual(go_source.decode_string_bytes(r'"\xff"'), b"\xff")


class GoParserTests(unittest.TestCase):
    SOURCE = '''
package example

// Query: "not this"
var Tests = []queries.ScriptTest{
    {
        Name: "basic",
        SetUpScript: []string{
            "CREATE " + "TABLE t(a INT)",
            `INSERT INTO t VALUES ('}')`,
        },
        Assertions: []ScriptTestAssertion{
            {Query: "SELECT a FROM t", Expected: []sql.Row{{1}}},
            {Query: fmt.Sprintf("SELECT %d", 1)},
        },
        Skip: true,
    },
}

var (
    grouped = []string{"x", "y"}
    other   = map[string]int{"a": 1}
)

func Build() {
    local := []string{"p", "q"}
    local = append(local, "r", rest...)
}
'''

    def test_composite_literals_and_fields(self):
        parser = go_source.Parser(self.SOURCE)
        declarations = dict(parser.declarations(0, None))
        tests = declarations["Tests"]
        self.assertEqual(tests.type, "[]queries.ScriptTest")
        (script,) = tests.elements()
        self.assertEqual(script.field("Name").value, "basic")
        self.assertEqual([node.value for node in script.field("SetUpScript").elements()], ["CREATE TABLE t(a INT)", "INSERT INTO t VALUES ('}')"])
        first, second = script.field("Assertions").elements()
        self.assertEqual(first.field("Query").value, "SELECT a FROM t")
        self.assertEqual(first.field("Query").line, 13)
        self.assertEqual(second.field("Query").kind, "expr")
        self.assertEqual(script.field("Skip").text, "true")
        self.assertEqual([node.value for node in declarations["grouped"].elements()], ["x", "y"])
        self.assertEqual(declarations["other"].kind, "composite")

    def test_function_statements_and_call_arguments(self):
        parser = go_source.Parser(self.SOURCE)
        open_index, close_index, line = go_source.function_spans(parser)["Build"]
        self.assertEqual(line, 25)
        statements = list(parser.statements(open_index + 1, close_index))
        self.assertEqual([(name, op) for name, op, _node in statements], [("local", ":="), ("local", "=")])
        arguments = parser.call_arguments(statements[1][2])
        self.assertEqual([argument.text for argument in arguments], ["local", '"r"', "rest..."])

    def test_semicolons_are_inserted_at_line_ends(self):
        tokens = go_source.tokenize("x := 1\ny := f(\n  2,\n)\n")
        self.assertEqual([t.text for t in tokens], ["x", ":=", "1", ";", "y", ":=", "f", "(", "2", ",", ")", ";"])


if __name__ == "__main__":
    unittest.main()
