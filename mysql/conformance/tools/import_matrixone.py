#!/usr/bin/env python3
"""Import MatrixOne distributed test cases as MySQL spec candidates.

MatrixOne (Apache-2.0) runs ``test/distributed/cases`` scripts with mo-tester.
Scripts usually create, use and drop one database; that database is the
replayed one and its qualifiers are removed. ``-- @session`` blocks run in other
sessions and are skipped. MatrixOne's expected results are ignored; only
``-- @sortkey`` is used as an ordering hint.
"""

import argparse
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import file_replay  # noqa: E402


class MatrixoneProfile(file_replay.Profile):
    name = "matrixone"
    title = "MatrixOne distributed cases"
    repo = "https://github.com/matrixorigin/matrixone.git"
    revision = "7038c8520e5fc1ba456914b3c389ccedb69e22f7"
    license = "Apache-2.0"
    root = "test/distributed/cases"
    patterns = ("*.sql", "*.test")
    parser_class = file_replay.MoTesterParser
    filters = file_replay.MATRIXONE_FILTERS
    excluded_dirs = {
        "snapshot": "MatrixOne snapshots",
        "pitr": "MatrixOne point-in-time recovery",
        "publication_subscription": "MatrixOne publications",
        "stage": "MatrixOne stages",
        "tenant": "MatrixOne accounts",
        "security": "users and privileges",
        "zz_accesscontrol": "users and privileges",
        "mo_cloud": "MatrixOne cloud features",
        "git4data": "MatrixOne data branches",
        "iceberg": "external tables",
        "dataXtest": "external tools",
        "vector": "MatrixOne vector indexes",
        "array": "MatrixOne vector types",
        "fulltext": "MatrixOne fulltext indexes",
        "fulltext2": "MatrixOne fulltext indexes",
        "load_data": "file system",
        "plugin": "MatrixOne plugins",
        "udf": "MatrixOne Python UDFs",
        "sequence": "sequences",
        "system": "MatrixOne system tables",
        "system_variable": "server variables",
        "log": "MatrixOne logs",
        "task": "MatrixOne tasks",
        "sql_source_type": "MatrixOne statement metadata",
        "statement_query_type": "MatrixOne statement metadata",
        "zz_statement_query_type": "MatrixOne statement metadata",
        "save_query_result": "MatrixOne query result cache",
        "query_result": "MatrixOne query result cache",
        "plan_cache": "query plans",
        "analyze": "query plans",
        "hint": "query plans",
        "benchmark": "shares tables across files and loads data files",
        "disttae": "MatrixOne storage internals",
        "fake_pk": "MatrixOne storage internals",
        "datastream": "external streams",
        "time_window": "MatrixOne time windows",
        "sample": "MatrixOne SAMPLE extension",
        "feature_limit": "MatrixOne accounts",
        "pg_cast": "PostgreSQL syntax",
        "sql_inject": "users and privileges",
        "optimistic": "multi-session transactions",
        "pessimistic_transaction": "multi-session transactions",
        "metadata": "MatrixOne metadata",
        "result_count": "session result counters",
        "util": "timing functions",
    }
    excluded_files = {}
    notes = (
        "Objects are tracked per database; `USE` switches the namespace, qualifiers naming the current "
        "database are removed, statements referencing another database's objects are filtered, and "
        "CREATE/DROP DATABASE are virtualized (a DROP ends every object generation in that database).",
        "Statements in `-- @session` blocks are skipped; objects they modify are not used by later "
        "candidates.",
        "Statements marked `-- @ignore`, `-- @regex`, `-- @wait*` or `-- @metacmp` are not emitted as "
        "candidates because MatrixOne masks or does not compare their output.",
        "Statements in `-- @bvt:issue` blocks are kept: they are skipped by MatrixOne's CI because of "
        "MatrixOne bugs, not because the SQL is invalid for MySQL.",
    )


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--source", required=True, type=pathlib.Path, help="MatrixOne checkout")
    parser.add_argument(
        "--output",
        type=pathlib.Path,
        default=pathlib.Path(__file__).resolve().parents[1] / "candidates" / "matrixone",
    )
    file_replay.add_budget_arguments(parser)
    args = parser.parse_args(argv)
    candidates, _report, text = file_replay.run_profile(
        MatrixoneProfile(), args.source, args.output, file_replay.budget_from(args)
    )
    print(text)
    print(f"wrote {len(candidates):,} candidates to {args.output}")


if __name__ == "__main__":
    main()
