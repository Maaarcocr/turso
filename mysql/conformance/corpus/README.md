# Mined MySQL specification corpus

This directory contains the schema, pinned-source manifest, and import recipes for a quantity-first MySQL implementation curriculum. Generated official MySQL MTR cases are deliberately ignored by Git pending licensing review.

## Current seed

The repository includes 5,000 Apache-2.0 cases mined from the pinned go-mysql-server `enginetest` fixtures:

- 67 Go fixture files scanned
- 13,130 static query assertions extracted
- 5,004 static setup statements extracted
- 13,519 unique candidates after deduplication
- 5,000 cases selected across eleven rough implementation levels
- 4,494 positive syntax expectations and 506 source error cases awaiting oracle classification

The initial local MTR seed adds another 5,000 cases from pinned MySQL 8.4.10 sources:

- 1,457 mysqltest files parsed
- 104,903 SQL commands found
- 100,719 static SQL commands
- 4,151 commands requiring mysqltest variable evaluation
- 59,634 unique static cases after exact SQL/expectation deduplication
- 4,923 positive syntax expectations and 77 negative syntax expectations selected

MTR output lives under `corpus/local/mysql-mtr/` and is not redistributed. Its `manifest.json` records extraction counts and selection parameters. The tracked go-mysql-server shards live under `corpus/go-mysql-server/` with source attribution in every record.

These first cases are syntax requirements with source intent and rough feature classification. They do not yet claim executable result-value conformance. Oracle recording will progressively promote the structured setup/query fixtures to behavioral scenarios.

## Reproduce or expand the corpus

Regenerate the tracked Apache-licensed corpus, or set `GO_MYSQL_SERVER_LIMIT=0` to emit all 13,519 currently extractable cases:

```bash
CARGO=/path/to/cargo ./mysql/conformance/mine-go-mysql-server.sh
```

`GO_MYSQL_SERVER_SOURCE`, `GO_MYSQL_SERVER_OUTPUT`, and `GO_MYSQL_SERVER_SKIP_REPORT` override its checkout, output, and reporting behavior.

The MTR command fetches the exact sparse MySQL source checkout, emits 5,000 local cases, and reports current parser progress:

```bash
CARGO=/path/to/cargo ./mysql/conformance/mine-mtr.sh
```

Use an existing checkout or mine every unique static case:

```bash
MYSQL_MTR_SOURCE=/tmp/mysql-src \
MYSQL_MTR_LIMIT=0 \
CARGO=/path/to/cargo \
  ./mysql/conformance/mine-mtr.sh
```

Useful overrides:

- `MYSQL_MTR_OUTPUT=/path` changes the generated output directory.
- `MYSQL_MTR_LIMIT=N` changes the balanced sample size; `0` emits all cases.
- `MYSQL_MTR_SKIP_REPORT=1` performs extraction without compiling the Turso parser reporter.

The importer fails if any mysqltest file cannot be parsed, if the checkout does not match the pinned commit, or if a generated shard cannot be serialized.

## Measure implementation progress

Run the parser reporter independently:

```bash
cargo run -p turso_mysql_parser --example corpus -- \
  mysql/conformance/corpus/local/mysql-mtr
```

Limit the report to an early generated level or write machine-readable output:

```bash
cargo run -p turso_mysql_parser --example corpus -- \
  mysql/conformance/corpus/local/mysql-mtr \
  --through 04-query \
  --json-report /tmp/mysql-parser-progress.json
```

The normal report exits successfully while showing the earliest implementation frontier. Add `--strict` when a selected corpus is expected to pass completely. Parser panics always fail the run.

## Corpus format

Each JSONL record follows [`schema-v1.json`](schema-v1.json) and includes:

- Stable source-attributed ID
- Repository revision, path, and line
- Resolved static SQL text
- Positive, negative, or unknown syntax expectation
- Original MTR error expectation
- Connection and conditional context
- Approximate curriculum level, features, and complexity

Generated levels are intentionally rough. They exist to spread the initial sample across statement families; they can be refined without changing stable case IDs.
