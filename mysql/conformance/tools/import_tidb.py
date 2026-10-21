#!/usr/bin/env python3
"""Import TiDB integration tests (``tests/integrationtest/t``) as MySQL spec candidates.

TiDB (Apache-2.0) runs mysqltest-format scripts against TiDB. Each script runs
in its own database named after the file (``executor/insert.test`` uses
``executor__insert``); qualifiers naming that database are removed so the
script replays in the recorder's fresh database. TiDB's expected results are
ignored; only ``--sorted_result`` is used as an ordering hint.
"""

import argparse
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import file_replay  # noqa: E402
import importer_common as common  # noqa: E402


class TidbProfile(file_replay.Profile):
    name = "tidb"
    title = "TiDB integrationtest"
    repo = "https://github.com/pingcap/tidb.git"
    revision = "13544e9b1472f5c69c05e1ed607adcb3c151d53e"
    license = "Apache-2.0"
    root = "tests/integrationtest/t"
    patterns = ("*.test",)
    parser_class = file_replay.MysqltestParser
    filters = file_replay.TIDB_FILTERS
    excluded_dirs = {
        "privilege": "users and privileges",
        "bindinfo": "TiDB plan bindings",
        "statistics": "TiDB statistics",
        "infoschema": "information_schema contents",
        "sessionctx": "session state",
        "sessiontxn": "transaction isolation across sessions",
        "globalindex": "TiDB global indexes",
    }
    excluded_files = {}
    notes = (
        "Each file starts in its own database (path with '/' replaced by '__'). Objects are tracked per "
        "database; `USE` switches the namespace, qualifiers naming the current database are removed, and "
        "statements referencing another database's objects are filtered, so every replayed slice runs in "
        "one database.",
        "Statements in non-default mysqltest connections are skipped; objects they modify are not used "
        "by later candidates.",
        "Statements with --replace_regex/--replace_column or inside --disable_result_log are not "
        "emitted as candidates because TiDB masks or ignores their output.",
        "TiDB's CLUSTERED/NONCLUSTERED primary key keyword is removed from CREATE/ALTER TABLE "
        "(InnoDB primary keys are always clustered); no other SQL is rewritten.",
    )

    rewrite_note = "removed TiDB CLUSTERED/NONCLUSTERED primary key option"

    def own_database(self, relative):
        return relative.removesuffix(".test").replace("/", "__").lower()

    def rewrite(self, sql):
        """Drop TiDB's CLUSTERED/NONCLUSTERED keyword, which MySQL rejects.

        InnoDB primary keys are always clustered, so removing the keyword keeps
        the statement's meaning for MySQL.
        """
        if common.first_keyword(sql) not in ("create", "alter"):
            return sql
        pieces = []
        changed = False
        for token in common.tokenize(sql):
            if token.kind == "ident" and token.text.lower() in ("clustered", "nonclustered"):
                changed = True
                continue
            pieces.append(token.text)
        return "".join(pieces) if changed else sql


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--source", required=True, type=pathlib.Path, help="TiDB checkout")
    parser.add_argument(
        "--output",
        type=pathlib.Path,
        default=pathlib.Path(__file__).resolve().parents[1] / "candidates" / "tidb",
    )
    file_replay.add_budget_arguments(parser)
    args = parser.parse_args(argv)
    candidates, _report, text = file_replay.run_profile(
        TidbProfile(), args.source, args.output, file_replay.budget_from(args)
    )
    print(text)
    print(f"wrote {len(candidates):,} candidates to {args.output}")


if __name__ == "__main__":
    main()
