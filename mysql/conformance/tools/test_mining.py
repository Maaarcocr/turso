import importlib.util
import pathlib
import tempfile
import unittest

MODULE_PATH = pathlib.Path(__file__).with_name("mine_go_mysql_server.py")
SPEC = importlib.util.spec_from_file_location("mine_go_mysql_server", MODULE_PATH)
MINER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MINER)


class GoMysqlServerMinerTests(unittest.TestCase):
    def test_extracts_static_queries_setup_and_source_intent(self):
        source = r'''
package queries

var Tests = []ScriptTest{
    {
        Name: "basic",
        SetUpScript: []string{
            "CREATE " + "TABLE users(id INT)",
            `INSERT INTO users VALUES (1)`,
        },
        Assertions: []ScriptTestAssertion{
            {Query: "SELECT id FROM users ORDER BY id"},
            {Query: "SELECT broken", ExpectedErr: sql.ErrTableNotFound},
            {Query: "SELECT skipped", Skip: true},
            {Query: dynamicQuery},
        },
    },
}
'''
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            path = root / "enginetest/queries/example.go"
            path.parent.mkdir(parents=True)
            path.write_text(source)
            cases, stats = MINER.extract_file(path, root, "revision")

        self.assertEqual(
            [case["sql"] for case in cases],
            [
                "CREATE TABLE users(id INT)",
                "INSERT INTO users VALUES (1)",
                "SELECT id FROM users ORDER BY id",
                "SELECT broken",
            ],
        )
        self.assertEqual(cases[2]["expectation"]["syntax"], "accept")
        self.assertEqual(cases[2]["context"]["row_order"], "exact")
        self.assertEqual(cases[3]["expectation"]["syntax"], "unknown")
        self.assertEqual(stats["skipped_by_source"], 1)
        self.assertEqual(stats["dynamic_query"], 1)

    def test_tokenizer_ignores_comments_and_preserves_raw_strings(self):
        source = '''
package queries
// Query: "not SQL"
var Test = QueryTest{
    Query: `SELECT '{not a Go brace}'`,
    /* Query: "also not SQL" */
}
'''
        tokens, pairs = MINER.tokenize_go(source)
        strings = [token.value for token in tokens if token.kind == "string"]
        self.assertEqual(strings, ["SELECT '{not a Go brace}'"])
        self.assertEqual(len(pairs), 1)

    def test_classifier_handles_multiline_clauses(self):
        level, features = MINER.classify("SELECT *\nFROM a\nJOIN b ON a.id = b.id")
        self.assertEqual(level, "07-joins")
        self.assertIn("join", features)


if __name__ == "__main__":
    unittest.main()
