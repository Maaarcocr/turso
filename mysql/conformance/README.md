# MySQL conformance

The default suite is deterministic, network-free, and runs through the real MySQL parser, Turso lowering, planner, VDBE, and CLI boundaries.

```bash
CARGO=/path/to/cargo ./mysql/test.sh
```

`mysql/test.sh` uses one compiler job and disables incremental compilation plus test/dev debug information by default, then runs:

1. Workspace formatting checks
2. Parser acceptance, malformed-input, lowering, and explicit-rejection cases
3. Embedded frontend execution, values, metadata, recovery, and precision guards
4. CLI success and diagnostics tests
5. The owned `.sqltest` specs through the `tursomysql` backend
6. Clippy for all MySQL targets with warnings denied
7. A direct CLI hello-world smoke test

Owned SQL specs live in [`my-sqltests/`](my-sqltests) as `.sqltest` files for the shared runner in `testing/sqltest`. Their expected values were recorded from the pinned MySQL 8.4.10 Docker oracle, so replaying them needs no MySQL server. The `tursomysql` backend runs embedded Turso with `MySqlDialect` in process, splits statements like the `mysql` client, and renders values as MySQL's text protocol does:

```bash
make -C mysql/conformance check run   # syntax check, then the owned specs (must pass)
make -C mysql/conformance run-one FILE=my-sqltests/basics.sqltest
make -C mysql/conformance run-specs   # recorded corpus in specs/, one process per file, JSON report
make -C mysql/conformance score       # compare that report with baseline.txt (the CI ratchet)
```

The recorded corpus and its pipeline are described in [`SPECS.md`](SPECS.md).

The compatibility target is frozen in `target.toml`. Feature prerequisites and stage-by-stage status are tracked in `curriculum.toml`.

## Recorded MySQL specs

`specs/` holds a large corpus of `.sqltest` specs whose expected results were recorded once from MySQL 8.4.10: 302,599 from sqllogictest, Vitess and go-mysql-server, of which 183,657 pass in the first baseline (see the status in [`SPECS.md`](SPECS.md#status)). The pipeline (fetch pinned sources, import candidates, record against the pinned container, replay through `tursomysql`, score against `baseline.txt`) and the file formats are specified in [`SPECS.md`](SPECS.md).

GPL or license-unclear suites (MySQL's own mysql-test, TiDB, MatrixOne) commit only MySQL's results, without SQL, under `results/`. Their specs are rebuilt locally from the pinned sources, without a MySQL server, and scored against `baseline-local.txt` ([`SPECS.md`](SPECS.md#results-only-sources)):

```bash
make -C mysql/conformance materialize-local run-local-specs score-local
```

## Spec candidate importers

Importers turn permissively licensed upstream suites into candidate JSONL for the recorder (schema in [`SPECS.md`](SPECS.md)). They read pinned source checkouts as text only, never use upstream expected results, and write `candidates/<source>/*.jsonl` plus an `IMPORT_REPORT.md` with extraction and filter counts:

```bash
python3 -I mysql/conformance/tools/import_vitess.py --source /path/to/vitess      # 150,000-case sample; --all or --target N
python3 -I mysql/conformance/tools/import_vitess_endtoend.py --source /path/to/vitess
python3 -I mysql/conformance/tools/import_tidb.py --source /path/to/tidb
python3 -I mysql/conformance/tools/import_matrixone.py --source /path/to/matrixone
python3 -I mysql/conformance/tools/import_gms.py --source /path/to/go-mysql-server
python3 -I mysql/conformance/tools/import_mtr.py --source /path/to/mysql-server   # GPL: results-only
```

Revisions are pinned in `corpus/sources.lock.toml`. The script importers replay a dependency slice of each file as setup (`--max-setup`, `--max-candidates-per-file` and `--max-setup-per-file` bound replay cost) and mark it `setup_errors: drop` because vendor statements are mixed into the scripts.

## Mined implementation curriculum

The quantity-first corpus miner and source pins live under [`corpus/`](corpus/README.md). Five thousand Apache-licensed go-mysql-server cases are checked in and can be regenerated with:

```bash
CARGO=/path/to/cargo ./mysql/conformance/mine-go-mysql-server.sh
```

A second recipe extracts a balanced 5,000-case syntax curriculum from the official MySQL 8.4.10 MTR suite and reports the earliest parser gaps:

```bash
CARGO=/path/to/cargo ./mysql/conformance/mine-mtr.sh
```

Set either miner's `*_LIMIT=0` variable to extract every eligible static case. Generated MTR SQL stays local (GPL-2.0-only); only MySQL's results for `tools/import_mtr.py` candidates are committed.

Unsupported owned cases are assertions, not skips. When a feature is implemented, move its cases to successful execution coverage and add `.sqltest` specs recorded from MySQL where values or diagnostics are observable. Never weaken or delete a rejection case merely because it starts succeeding.
