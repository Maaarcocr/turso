#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
CARGO_BIN=${CARGO:-cargo}

cd "$ROOT"
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-1}
export CARGO_PROFILE_TEST_DEBUG=${CARGO_PROFILE_TEST_DEBUG:-0}
export CARGO_PROFILE_DEV_DEBUG=${CARGO_PROFILE_DEV_DEBUG:-0}
export CARGO_INCREMENTAL=${CARGO_INCREMENTAL:-0}

"$CARGO_BIN" fmt --all -- --check
"$CARGO_BIN" fmt --manifest-path mysql/conformance/tools/mtr-miner/Cargo.toml --all -- --check
python3 -m unittest mysql/conformance/tools/test_mining.py
python3 -m unittest mysql/conformance/tools/test_specs.py
python3 mysql/conformance/tools/validate_corpus.py mysql/conformance/corpus/go-mysql-server
"$CARGO_BIN" test -p turso_mysql_parser -p turso_mysql -p tursomysql
"$CARGO_BIN" test --manifest-path mysql/conformance/tools/mtr-miner/Cargo.toml
"$CARGO_BIN" clippy -p turso_mysql_parser -p turso_mysql -p tursomysql --all-targets --no-deps -- --deny warnings
"$CARGO_BIN" clippy --manifest-path mysql/conformance/tools/mtr-miner/Cargo.toml --all-targets -- --deny warnings

OUTPUT=$("$CARGO_BIN" run --quiet -p tursomysql -- "SELECT 'hello from mysql' AS greeting")
test "$OUTPUT" = "hello from mysql"
