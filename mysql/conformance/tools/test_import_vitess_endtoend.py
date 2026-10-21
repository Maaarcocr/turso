import collections
import pathlib
import sys
import tempfile
import textwrap
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import file_replay  # noqa: E402
import import_vitess_endtoend as endtoend  # noqa: E402
import importer_common as common  # noqa: E402

MAIN_TEST = """package union

var (
	keyspaceName  = "ks_union"
	uKeyspaceName = "uks"
)
"""

SCHEMA = """create table t1 (id bigint, col bigint, primary key (id)) Engine = InnoDB;
create table t2 (id bigint, primary key (id)) Engine = InnoDB;
"""

TEST_GO = """package union

func start(t *testing.T) (utils.MySQLCompare, func()) {
	mcmp, err := utils.NewMySQLCompare(t, vtParams, mysqlParams)
	return mcmp, func() {}
}

func seed(mcmp *utils.MySQLCompare) {
	mcmp.Exec("insert into t1(id, col) values (3, 30)")
}

func TestUnion(t *testing.T) {
	mcmp, closer := start(t)
	defer closer()

	mcmp.Exec("insert into t1(id, col) values (1, 10), (2, 20)")
	utils.Exec(t, mcmp.VtConn, "set workload = olap")
	mcmp.AssertMatchesNoOrder("select id from t1 union select id from ks_union.t1", `[[INT64(1)]]`)
	queries := []string{
		"select col from t1 order by col",
		"select count(*) from t1",
	}
	for _, query := range queries {
		mcmp.Exec(query)
	}
	for _, agg := range []string{"min", "max"} {
		mcmp.Exec(fmt.Sprintf("select %s(col) from t1", agg))
	}
	for i := range 3 {
		mcmp.Exec(fmt.Sprintf("insert into t2(id) values (%d)", i))
	}
	mcmp.Run("nested", func(mcmp *utils.MySQLCompare) {
		mcmp.AssertMatches("select * from t2", `[]`)
	})
	seed(mcmp)
	mcmp.ExecVitessAndMySQLDifferentQueries("select 1 from dual where @@workload = 'olap'", "select 1 from dual")
	mcmp.ExecAllowAndCompareError("select * from uks.u", utils.CompareOptions{})
	mcmp.AssertContainsError("select nope from t1", "column")
}
"""


class EndToEndTests(unittest.TestCase):
    def test_mcmp_calls_are_replayed_per_test(self):
        with tempfile.TemporaryDirectory() as directory:
            source = pathlib.Path(directory)
            package = source / endtoend.QUERIES / "union"
            package.mkdir(parents=True)
            (package / "main_test.go").write_text(MAIN_TEST)
            (package / "schema.sql").write_text(SCHEMA)
            (package / "union_test.go").write_text(TEST_GO)
            report = common.Report("test", {})
            dispositions = collections.Counter()
            candidates = endtoend.mcmp_candidates(source, report, file_replay.Budget(), dispositions)
        by_sql = {candidate["sql"][0]: candidate for candidate in candidates}
        t1 = ["create table t1 (id bigint, col bigint, primary key (id)) Engine = InnoDB"]
        seeded = t1 + ["insert into t1(id, col) values (1, 10), (2, 20)"]
        self.assertEqual(by_sql["insert into t1(id, col) values (1, 10), (2, 20)"]["setup"], t1)
        union = by_sql["select id from t1 union select id from t1"]
        self.assertEqual(union["setup"], seeded)
        self.assertFalse(union["ordered"])
        self.assertTrue(by_sql["select col from t1 order by col"]["ordered"])
        self.assertIn("select count(*) from t1", by_sql)
        self.assertIn("select min(col) from t1", by_sql)
        self.assertIn("select max(col) from t1", by_sql)
        # The insert loop could not be unrolled, so t2 is not used afterwards.
        self.assertNotIn("select * from t2", by_sql)
        # Package helpers that receive mcmp are followed.
        self.assertEqual(by_sql["insert into t1(id, col) values (3, 30)"]["setup"], seeded)
        self.assertIn("select 1 from dual", by_sql)
        self.assertNotIn("select * from uks.u", by_sql)
        self.assertEqual(
            by_sql["select nope from t1"]["setup"], seeded + ["insert into t1(id, col) values (3, 30)"]
        )
        self.assertNotIn("set workload = olap", " ".join(by_sql))
        self.assertEqual(report.filtered["dynamic SQL argument"], 1)
        self.assertEqual(report.filtered["cross-keyspace reference"], 1)
        self.assertEqual(
            by_sql["select col from t1 order by col"]["group"],
            "vitess-endtoend/go/test/endtoend/vtgate/queries/union/union_test/TestUnion",
        )

    def test_tester_scripts(self):
        with tempfile.TemporaryDirectory() as directory:
            source = pathlib.Path(directory)
            script = source / endtoend.TESTER / "join" / "join.test"
            script.parent.mkdir(parents=True)
            script.write_text(textwrap.dedent("""\
                create table t (a int);
                insert into t values (1);
                --skip not supported by vitess
                select a from t order by a;
                --error infinite recursion
                with recursive c as (select 1 as n union all select n+1 from c) select * from c;
                """))
            report = common.Report("test", {})
            candidates, _dispositions = endtoend.tester_candidates(source, report, file_replay.Budget())
        self.assertEqual(
            [(c["id"], c["sql"][0]) for c in candidates],
            [
                ("vitess-endtoend/vitess_tester/join/join:L4", "select a from t order by a"),
                (
                    "vitess-endtoend/vitess_tester/join/join:L6",
                    "with recursive c as (select 1 as n union all select n+1 from c) select * from c",
                ),
            ],
        )
        self.assertEqual(candidates[0]["setup"], ["create table t (a int)", "insert into t values (1)"])


if __name__ == "__main__":
    unittest.main()
