import pathlib
import sys
import tempfile
import textwrap
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import file_replay  # noqa: E402
import import_matrixone  # noqa: E402
import import_tidb  # noqa: E402
import importer_common as common  # noqa: E402


def parse(parser_class, text):
    report = common.Report("test", {})
    return parser_class(report).parse(textwrap.dedent(text)), report


class MysqltestParserTests(unittest.TestCase):
    def test_directives_connections_and_delimiters(self):
        statements, _report = parse(
            file_replay.MysqltestParser,
            """\
            # comment
            create table t (a int);
            --error 1062
            insert into t values (1);
            -- error 1253
            select 1; select 2;
            --sorted_result
            select a
              from t;
            --replace_regex /x/y/
            select 3;
            --disable_result_log
            select 4;
            --enable_result_log
            connect (conn1, localhost, root,,);
            insert into t values (2);
            connection default;
            delimiter //;
            create procedure p() begin select 1; end//
            delimiter ;//
            select 5;
            """,
        )
        rows = [(s.sql.replace("\n", " "), s.line, s.expect_error, s.sorted_hint, s.masked, s.skip, s.ordinal_on_line) for s in statements]
        self.assertEqual(
            rows,
            [
                ("create table t (a int)", 2, False, False, "", "", 1),
                ("insert into t values (1)", 4, True, False, "", "", 1),
                ("select 1", 6, True, False, "", "", 1),
                ("select 2", 6, False, False, "", "", 2),
                ("select a   from t", 8, False, True, "", "", 1),
                ("select 3", 11, False, False, "--replace_regex", "", 1),
                ("select 4", 13, False, False, "--disable_result_log", "", 1),
                ("insert into t values (2)", 16, False, False, "", "connection conn1", 1),
                ("create procedure p() begin select 1; end", 19, False, False, "", "", 1),
                ("select 5", 21, False, False, "", "", 1),
            ],
        )


class MoTesterParserTests(unittest.TestCase):
    def test_mo_directives(self):
        statements, report = parse(
            file_replay.MoTesterParser,
            """\
            create table t (a int);
            -- @sortkey:0
            select a from t order by a;
            -- @ignore:0
            select a from t;
            -- @session:id=1&user=acc:admin&password=111{
            insert into t values (9);
            -- @session}
            -- @pattern
            insert into t values ('x');
            -- @bvt:issue#123
            select 1;
            -- @bvt:issue
            -- plain comment
            select 2;
            """,
        )
        rows = [(s.sql, s.sorted_hint, s.masked, s.skip, s.expect_error) for s in statements]
        self.assertEqual(
            rows,
            [
                ("create table t (a int)", False, "", "", False),
                ("select a from t order by a", True, "", "", False),
                ("select a from t", False, "@ignore", "", False),
                ("insert into t values (9)", False, "", "mo session id=1", False),
                ("insert into t values ('x')", False, "", "", True),
                ("select 1", False, "", "", False),
                ("select 2", False, "", "", False),
            ],
        )
        self.assertEqual(report.counts["statements inside mo @bvt:issue blocks (kept)"], 1)


class TestProfile(file_replay.Profile):
    name = "test"
    title = "Test"
    repo = "https://example.invalid/test.git"
    revision = "r"
    license = "Apache-2.0"
    root = "cases"
    patterns = ("*.test",)
    parser_class = file_replay.MysqltestParser
    filters = file_replay.TIDB_FILTERS
    notes = ()

    def own_database(self, relative):
        return "own"


def run_script(text, profile=None, budget=None):
    profile = profile or TestProfile()
    report = common.Report("test", {})
    importer = file_replay.FileImporter(profile, budget or file_replay.Budget(), report)
    with tempfile.TemporaryDirectory() as directory:
        path = pathlib.Path(directory) / "x.test"
        path.write_text(textwrap.dedent(text))
        candidates = importer.import_file(path, "x.test")
    return {tuple(c["sql"]): c for c in candidates}, report


class ReplaySliceTests(unittest.TestCase):
    def test_generations_variables_and_views(self):
        candidates, report = run_script(
            """\
            drop table if exists t;
            create table t (a int);
            insert into t values (1),(2);
            create table u (b int);
            insert into u values (5);
            select a from t order by a;
            --sorted_result
            select * from t order by a;
            drop table t;
            create table t (a varchar(10));
            insert into t values ('x');
            select * from t;
            set @v = 3;
            set @v = @v + 1;
            select @v + 1;
            set @@sql_mode = '';
            set @@sql_mode = 'STRICT_TRANS_TABLES';
            select 1;
            create view v as select * from u;
            insert into u values (6);
            select * from v;
            select now();
            explain select * from t;
            select tidb_version();
            select 'tidb_x' from t;
            """
        )

        def setup(*sql):
            return candidates[tuple(sql)]["setup"]

        self.assertEqual(setup("select a from t order by a"), ["create table t (a int)", "insert into t values (1),(2)"])
        self.assertTrue(candidates[("select a from t order by a",)]["ordered"])
        self.assertFalse(candidates[("select * from t order by a",)]["ordered"])
        self.assertEqual(setup("select * from t"), ["create table t (a varchar(10))", "insert into t values ('x')"])
        self.assertEqual(setup("select @v + 1"), ["set @v = 3", "set @v = @v + 1"])
        self.assertEqual(setup("select 1"), ["set @@sql_mode = 'STRICT_TRANS_TABLES'"])
        self.assertEqual(
            setup("select * from v"),
            [
                "create table u (b int)",
                "insert into u values (5)",
                "set @@sql_mode = 'STRICT_TRANS_TABLES'",
                "create view v as select * from u",
                "insert into u values (6)",
            ],
        )
        # The SET active when the query runs is replayed too.
        self.assertEqual(
            setup("select 'tidb_x' from t"),
            ["create table t (a varchar(10))", "insert into t values ('x')", "set @@sql_mode = 'STRICT_TRANS_TABLES'"],
        )
        self.assertNotIn(("select now()",), candidates)
        self.assertNotIn(("select tidb_version()",), candidates)
        self.assertEqual(report.filtered["nondeterministic or session-dependent function"], 1)
        self.assertEqual(report.filtered["tidb-specific identifier"], 1)
        candidate = candidates[("select * from t",)]
        self.assertEqual(candidate["id"], "test/x:L12")
        self.assertEqual(candidate["source"]["path"], "cases/x.test")
        self.assertEqual(candidate["setup_errors"], "drop")

    def test_sessions_databases_transactions_and_warnings(self):
        candidates, report = run_script(
            """\
            create table t (a int);
            create table u (b int);
            insert into own.t values (1);
            connect (conn1, localhost, root,,);
            insert into u values (7);
            connection default;
            select * from u;
            select * from t;
            create database d2;
            use d2;
            create table t (z int);
            select z from t;
            select * from own.t;
            use own;
            begin;
            insert into t values (2);
            rollback;
            select a from t;
            insert into t values ('bad');
            show warnings;
            --error 1062
            insert into t values (1);
            """
        )
        self.assertNotIn(("select * from u",), candidates)
        self.assertEqual(report.filtered["depends on an object modified outside the replayed session"], 1)
        self.assertEqual(candidates[("select * from t",)]["setup"], ["create table t (a int)", "insert into t values (1)"])
        self.assertEqual(candidates[("select z from t",)]["setup"], ["create table t (z int)"])
        self.assertNotIn(("select * from own.t",), candidates)
        self.assertEqual(report.filtered["cross-database reference"], 1)
        self.assertEqual(
            candidates[("select a from t",)]["setup"],
            ["create table t (a int)", "insert into t values (1)", "begin", "insert into t values (2)", "rollback"],
        )
        warnings = candidates[("insert into t values ('bad')", "show warnings")]
        self.assertTrue(warnings["ordered"])
        self.assertEqual(warnings["id"], "test/x:L20")
        statement = candidates[("insert into t values (1)",)]
        self.assertIn("insert into t values ('bad')", statement["setup"])
        self.assertEqual(report.counts["statement candidates (source expects an error)"], 1)

    def test_budget_caps(self):
        script = "create table t (a int);\n" + "insert into t values (1);\n" * 5 + "select a from t;\nselect 1;\n"
        candidates, report = run_script(script, budget=file_replay.Budget(max_setup=3))
        self.assertEqual(list(candidates), [("select 1",)])
        self.assertEqual(report.filtered["setup longer than the per-candidate cap"], 1)

    def test_routines_and_triggers_are_live_dependencies(self):
        candidates, _report = run_script(
            """\
            create table t (a int);
            create table log (x int);
            create trigger trg after insert on t for each row insert into log values (new.a);
            insert into t values (1);
            select * from log;
            """
        )
        self.assertEqual(
            candidates[("select * from log",)]["setup"],
            [
                "create table t (a int)",
                "create table log (x int)",
                "create trigger trg after insert on t for each row insert into log values (new.a)",
                "insert into t values (1)",
            ],
        )


class SourceProfileTests(unittest.TestCase):
    def test_tidb_rewrites_clustered_and_names_own_database(self):
        profile = import_tidb.TidbProfile()
        self.assertEqual(profile.own_database("executor/insert.test"), "executor__insert")
        self.assertEqual(
            profile.rewrite("create table t (a int, primary key (a) /*T![clustered_index] CLUSTERED */, b int) "),
            "create table t (a int, primary key (a) /*T![clustered_index] CLUSTERED */, b int) ",
        )
        self.assertEqual(
            profile.rewrite("create table t (a int, primary key (a) NONCLUSTERED)"),
            "create table t (a int, primary key (a) )",
        )
        self.assertEqual(profile.rewrite("select clustered from t"), "select clustered from t")

    def test_matrixone_filters_vendor_syntax(self):
        filters = file_replay.compile_filters(file_replay.COMMON_FILTERS + file_replay.MATRIXONE_FILTERS)
        self.assertEqual(file_replay.filter_reason("create table t (v vecf32(3))", filters), "matrixone vector/datalink")
        self.assertEqual(file_replay.filter_reason("select median(a) from t", filters), "matrixone-only function")
        self.assertEqual(file_replay.filter_reason("select a from t", filters), "")
        masked = file_replay.mask_strings("insert into t values ('mysql.com'), ('x') -- mo_ctl(")
        self.assertEqual(masked, "insert into t values (''), ('')")
        self.assertEqual(file_replay.filter_reason(masked, filters), "")

    def test_matrixone_database_is_virtualized(self):
        profile = import_matrixone.MatrixoneProfile()
        profile.parser_class = file_replay.MoTesterParser
        candidates, report = run_script(
            """\
            drop database if exists db1;
            create database db1;
            use db1;
            create table t (a int);
            insert into db1.t values (1);
            select * from t;
            drop database db1;
            """,
            profile=profile,
        )
        self.assertEqual(candidates[("select * from t",)]["setup"], ["create table t (a int)", "insert into t values (1)"])
        self.assertEqual(report.counts["statements with the current database qualifier removed"], 1)


class ObjectEffectTests(unittest.TestCase):
    def test_object_effects(self):
        self.assertEqual(file_replay.object_effects("create table if not exists `db`.`T1` (a int)"), [("create", "db", "t1")])
        self.assertEqual(file_replay.object_effects("drop table t1, db.t2"), [("drop", None, "t1"), ("drop", "db", "t2")])
        self.assertEqual(
            file_replay.object_effects("rename table a to b"), [("drop", None, "a"), ("create", None, "b")]
        )
        self.assertEqual(file_replay.object_effects("alter table a rename column x to y"), [])
        self.assertEqual(file_replay.modified_targets("insert ignore into db.t select * from u"), [("db", "t")])

    def test_session_analysis(self):
        def analyze(sql, kind):
            return file_replay.session_analysis(sql, common.significant_tokens(sql), kind)

        effects = analyze("set @a = @b + 1, sql_mode = '', @@session.time_zone = '+00:00'", "session")
        self.assertEqual(effects.user_targets, ["a"])
        self.assertEqual(effects.system_targets, ["sql_mode", "time_zone"])
        self.assertEqual(effects.reads, {"b"})
        self.assertEqual(analyze("select max(a) into @m from t", "session").user_targets, ["m"])
        self.assertEqual(analyze("prepare s from 'select 1'", "session").prepare, "s")
        self.assertEqual(analyze("execute s using @x", "query").execute, "s")
        self.assertEqual(analyze("execute s using @x", "query").reads, {"x"})


if __name__ == "__main__":
    unittest.main()
