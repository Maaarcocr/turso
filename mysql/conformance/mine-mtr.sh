#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
CARGO_BIN=${CARGO:-cargo}
SOURCE=${MYSQL_MTR_SOURCE:-/tmp/turso-mysql-spec-sources/mysql-server}
OUTPUT=${MYSQL_MTR_OUTPUT:-$ROOT/mysql/conformance/corpus/local/mysql-mtr}
LIMIT=${MYSQL_MTR_LIMIT:-5000}
REVISION=6adc159923b7b6abbe649949551ec25264c2daf9
TAG=mysql-8.4.10

export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-1}
export CARGO_PROFILE_DEV_DEBUG=${CARGO_PROFILE_DEV_DEBUG:-0}
export CARGO_INCREMENTAL=${CARGO_INCREMENTAL:-0}

if [[ ! -d "$SOURCE/.git" ]]; then
    mkdir -p "$(dirname "$SOURCE")"
    git clone --filter=blob:none --depth 1 --branch "$TAG" --sparse \
        https://github.com/mysql/mysql-server.git "$SOURCE"
    git -C "$SOURCE" sparse-checkout set mysql-test/t mysql-test/r
fi

ACTUAL_REVISION=$(git -C "$SOURCE" rev-parse HEAD)
if [[ "$ACTUAL_REVISION" != "$REVISION" ]]; then
    echo "expected MySQL source revision $REVISION, found $ACTUAL_REVISION at $SOURCE" >&2
    exit 1
fi

"$CARGO_BIN" run --quiet \
    --manifest-path "$ROOT/mysql/conformance/tools/mtr-miner/Cargo.toml" -- \
    --input "$SOURCE/mysql-test/t" \
    --output "$OUTPUT" \
    --revision "$REVISION" \
    --path-prefix mysql-test/t \
    --limit "$LIMIT" \
    --shard-size 1000

if [[ ${MYSQL_MTR_SKIP_REPORT:-0} != 1 ]]; then
    cd "$ROOT"
    "$CARGO_BIN" run --quiet -p turso_mysql_parser --example corpus -- \
        "$OUTPUT" \
        --json-report "$OUTPUT/parser-report.json"
fi
