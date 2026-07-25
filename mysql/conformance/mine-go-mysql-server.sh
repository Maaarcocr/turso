#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
CARGO_BIN=${CARGO:-cargo}
SOURCE=${GO_MYSQL_SERVER_SOURCE:-/tmp/turso-mysql-spec-sources/go-mysql-server}
OUTPUT=${GO_MYSQL_SERVER_OUTPUT:-$ROOT/mysql/conformance/corpus/go-mysql-server}
LIMIT=${GO_MYSQL_SERVER_LIMIT:-5000}
REVISION=478bed778dd282aa13aa48ebd75b0549ebe75698

export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-1}
export CARGO_PROFILE_DEV_DEBUG=${CARGO_PROFILE_DEV_DEBUG:-0}
export CARGO_INCREMENTAL=${CARGO_INCREMENTAL:-0}

if [[ ! -d "$SOURCE/.git" ]]; then
    mkdir -p "$(dirname "$SOURCE")"
    git clone --filter=blob:none --no-checkout \
        https://github.com/dolthub/go-mysql-server.git "$SOURCE"
fi

if ! git -C "$SOURCE" cat-file -e "$REVISION^{commit}"; then
    git -C "$SOURCE" fetch origin "$REVISION"
fi

git -C "$SOURCE" checkout "$REVISION" -- \
    LICENSE \
    enginetest/queries \
    enginetest/scriptgen/setup/scripts \
    enginetest/sqllogictest/testdata

python3 "$ROOT/mysql/conformance/tools/mine_go_mysql_server.py" \
    --input "$SOURCE" \
    --output "$OUTPUT" \
    --revision "$REVISION" \
    --limit "$LIMIT" \
    --shard-size 1000

python3 "$ROOT/mysql/conformance/tools/validate_corpus.py" "$OUTPUT"

if [[ ${GO_MYSQL_SERVER_SKIP_REPORT:-0} != 1 ]]; then
    cd "$ROOT"
    "$CARGO_BIN" run --quiet -p turso_mysql_parser --example corpus -- \
        "$OUTPUT" \
        --json-report "${GO_MYSQL_SERVER_REPORT:-/tmp/go-mysql-server-parser-progress.json}"
fi
