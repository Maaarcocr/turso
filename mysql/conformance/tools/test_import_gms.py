import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import file_replay  # noqa: E402
import import_gms as gms  # noqa: E402
import importer_common as common  # noqa: E402

FIXTURES = '''package setup

var MydbData = []SetupScript{{
	`create database if not exists mydb`,
	`use mydb`,
}}

var MytableData = []SetupScript{{
	`create table mytable (i int primary key, s varchar(20))`,
	"insert into mytable values (1, 'a')",
}}

var OtherData = []SetupScript{{
	`create table other (x int)`,
}}
'''

GROUPS = '''package setup

var (
	Mytable = [][]SetupScript{MydbData, MytableData}
)
'''

DRIVER = '''package enginetest

func TestQueries(t *testing.T, harness Harness) {
	harness.Setup(setup.Mytable...)
	for _, tt := range queries.QueryTests {
		TestQuery(t, harness, tt.Query)
	}
}

func TestQueriesPrepared(t *testing.T, harness Harness) {
	harness.Setup(setup.MydbData)
	for _, tt := range queries.QueryTests {
	}
}

func TestWrites(t *testing.T, harness Harness) {
	extra := []setup.SetupScript{{"create table extra (e int)"}}
	all := append(setup.Mytable, extra)
	harness.Setup(all...)
	for _, tt := range queries.WriteTests {
	}
}
'''

QUERIES = r'''package queries

var QueryTests = []QueryTest{
	{
		Query:    "SELECT s FROM mytable ORDER BY i",
		Expected: []sql.Row{{"a"}},
	},
	{
		Query: "SELECT 'skipped'",
		Skip:  true,
	},
	{
		Query: fmt.Sprintf("SELECT %d", 1),
	},
	{
		Query: "SELECT '\xef\xbf\xbd'",
	},
}

var ScriptTests = []ScriptTest{
	{
		Name: "script",
		SetUpScript: []string{
			"create table t (a int)",
			"insert into t values (1)",
		},
		Assertions: []ScriptTestAssertion{
			{
				Query:    "insert into t values (2)",
				Expected: []sql.Row{{types.NewOkResult(1)}},
			},
			{
				Query: "select * from t",
				Skip:  true,
			},
			{
				Query:            "set @x = 5",
				SkipResultsCheck: true,
			},
			{
				Query: "select @x, count(*) from t",
			},
			{
				Query:      "select @x",
				NewSession: true,
			},
			{
				Query:    "select ?",
				Bindings: map[string]sqlparser.Expr{"v1": nil},
			},
		},
	},
	{
		Name:    "postgres only",
		Dialect: "postgres",
		Assertions: []ScriptTestAssertion{
			{Query: "select 1::int"},
		},
	},
}

var WriteTests = []WriteQueryTest{
	{
		WriteQuery:  "INSERT INTO mytable VALUES (2, 'b')",
		SelectQuery: "SELECT * FROM mytable",
	},
}

var PlanTests = []QueryPlanTest{
	{Query: "select 1", ExpectedPlan: "Project"},
}
'''


def make_tree(directory):
    root = pathlib.Path(directory)
    files = {
        gms.FIXTURES: FIXTURES,
        gms.GROUPS: GROUPS,
        "enginetest/enginetests.go": DRIVER,
        "enginetest/queries/queries.go": QUERIES,
    }
    for relative, text in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    return root


class GmsImporterTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = make_tree(self.directory.name)
        self.report = common.Report("test", {})
        self.library = gms.FixtureLibrary(self.root)

    def tearDown(self):
        self.directory.cleanup()

    def test_fixtures_and_driver_mapping(self):
        self.assertEqual(self.library.statements(["MytableData"])[1], "insert into mytable values (1, 'a')")
        mapping = gms.driver_fixtures(self.root, self.library, self.report)
        # The non-prepared driver wins; literal SetupScripts and append() are understood.
        self.assertEqual(mapping["QueryTests"], ["MydbData", "MytableData"])
        self.assertEqual(
            mapping["WriteTests"], ["MydbData", "MytableData", ("literal", ["create table extra (e int)"])]
        )

    def candidates(self):
        mapping = gms.driver_fixtures(self.root, self.library, self.report)
        extractor = gms.Extractor(self.root, self.library, mapping, self.report)
        importer = gms.Importer(self.library, self.report, file_replay.Budget(max_setup=50))
        out = {}
        for test in extractor.tests():
            for candidate in importer.candidates(test):
                out[tuple(candidate["sql"])] = candidate
        return out

    def test_candidates(self):
        candidates = self.candidates()
        query = candidates[("SELECT s FROM mytable ORDER BY i",)]
        self.assertEqual(
            query["setup"], ["create table mytable (i int primary key, s varchar(20))", "insert into mytable values (1, 'a')"]
        )
        self.assertTrue(query["ordered"])
        self.assertEqual(query["id"], "go-mysql-server/enginetest/queries/queries:L5")
        self.assertEqual(query["group"], "go-mysql-server/enginetest/queries/queries/QueryTests")
        self.assertIn(("SELECT '�'",), candidates)

        self.assertEqual(candidates[("insert into t values (2)",)]["setup"], ["create table t (a int)", "insert into t values (1)"])
        self.assertEqual(
            candidates[("select @x, count(*) from t",)]["setup"],
            ["create table t (a int)", "insert into t values (1)", "insert into t values (2)", "set @x = 5"],
        )
        self.assertEqual(candidates[("select @x",)]["setup"], [])
        self.assertNotIn(("set @x = 5",), candidates)

        write = candidates[("INSERT INTO mytable VALUES (2, 'b')", "SELECT * FROM mytable")]
        self.assertEqual(
            write["setup"], ["create table mytable (i int primary key, s varchar(20))", "insert into mytable values (1, 'a')"]
        )
        self.assertEqual(len(candidates), 6)
        filtered = self.report.filtered
        self.assertEqual(filtered["test skipped by source (Skip: true)"], 1)
        self.assertEqual(filtered["test with a dynamic query"], 1)
        self.assertEqual(filtered["assertion skipped by source (Skip: true)"], 1)
        self.assertEqual(filtered["assertion needs bound parameters"], 1)
        self.assertEqual(filtered["script for another dialect (postgres)"], 1)
        self.assertEqual(filtered["ignored array type QueryPlanTest (query plans are engine-specific)"], 1)


if __name__ == "__main__":
    unittest.main()
