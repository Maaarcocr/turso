#!/bin/bash
S=/tmp/claude-0/-home-user-turso/d15709bb-7f9a-5758-baa6-00ea81e0de51/scratchpad
G=$S/p3/gates; mkdir -p $G; rm -f $G/summary.txt
source $S/p3/env.sh
cd /home/user/wt/core-types
echo "head $(git rev-parse --short HEAD)" >> $G/summary.txt
flock /tmp/turso-cargo.lock cargo build --bin tursodb > $G/build.txt 2>&1; echo "build tursodb exit $?" >> $G/summary.txt
flock /tmp/turso-cargo.lock cargo build -p tursopg >> $G/build.txt 2>&1; echo "build tursopg exit $?" >> $G/summary.txt
cp target/debug/tursodb $S/bin/tursodb-p3; cp target/debug/tursopg $S/bin/tursopg-p3
flock /tmp/turso-cargo.lock cargo test -p turso_core --lib > $G/core-lib.txt 2>&1; echo "core-lib exit $?" >> $G/summary.txt
flock /tmp/turso-cargo.lock cargo test -p core_tester --test integration_tests > $G/integration.txt 2>&1; echo "integration exit $?" >> $G/summary.txt
flock /tmp/turso-cargo.lock cargo test -p turso_pg_parser -p turso_pg -p turso_pg_tests > $G/pg-rust.txt 2>&1; echo "pg-rust exit $?" >> $G/summary.txt
flock /tmp/turso-cargo.lock cargo clippy -p turso_core -p turso_parser -p core_tester --all-features --all-targets -- --deny=warnings > $G/clippy.txt 2>&1; echo "clippy exit $?" >> $G/summary.txt
flock /tmp/turso-cargo.lock cargo clippy -p turso_pg_parser -p turso_pg -p turso_pg_tests --all-targets --no-deps -- --deny=warnings > $G/clippy-pg.txt 2>&1; echo "clippy-pg exit $?" >> $G/summary.txt
cargo fmt --check > $G/fmt.txt 2>&1; echo "fmt exit $?" >> $G/summary.txt
echo COMPILED >> $G/summary.txt
cd sqlite/conformance
timeout 3000 $S/bin/sqltest-runner run turso-sqltests --backend cli --binary $S/bin/tursodb-p3 > $G/turso-cli.txt 2>&1; echo "turso-cli exit $?" >> $G/summary.txt
cd ../../postgres/conformance
timeout 3000 $S/bin/sqltest-runner run pg-sqltests --timeout 600 --backend tursopg --binary $S/bin/tursopg-p3 > $G/pg.txt 2>&1; echo "pg exit $?" >> $G/summary.txt
if [ "$1" = "full" ]; then
cd ../../sqlite/conformance
EXPLAIN_NEW=$S/bin/tursodb-p3 EXPLAIN_BASE=$S/bin/tursodb-base EXPLAIN_LOG=$G/explain-off.jsonl EXPLAIN_MODE=off \
  timeout 7000 $S/bin/sqltest-runner run $(find sqlite-sqltests -name '*.sqltest' ! -name big.sqltest ! -name big-mvcc.sqltest | sort) --backend cli --binary $S/explain/tursodb-explain-diff --timeout 600 > $G/explain-off-run.txt 2>&1
echo "explain-off exit $?" >> $G/summary.txt
fi
echo DONE >> $G/summary.txt
