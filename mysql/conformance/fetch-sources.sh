#!/usr/bin/env bash
# Fetch the pinned spec sources listed in corpus/sources.lock.toml.
#
#   mysql/conformance/fetch-sources.sh [source-id...]
#
# Each source is checked out at exactly its pinned revision (shallow, sparse
# to the listed paths) into $MYSQL_SPEC_SOURCES/<id> (default
# mysql/conformance/.sources, ignored by git). Existing checkouts at the right
# revision are reused. Sources are data: nothing in them is executed.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
LOCK=${MYSQL_SPEC_LOCK:-$HERE/corpus/sources.lock.toml}
DEST=${MYSQL_SPEC_SOURCES:-$HERE/.sources}

mapfile -t ENTRIES < <(python3 -I - "$LOCK" "$@" <<'PY'
import sys, tomllib
lock = tomllib.load(open(sys.argv[1], "rb"))
wanted = set(sys.argv[2:])
known = {source["id"] for source in lock["source"]}
unknown = wanted - known
if unknown:
    sys.exit(f"unknown source ids: {sorted(unknown)}; known: {sorted(known)}")
for source in lock["source"]:
    if wanted and source["id"] not in wanted:
        continue
    print("\t".join([source["id"], source["repository"], source["revision"], " ".join(source.get("paths", []))]))
PY
)

mkdir -p "$DEST"
for entry in "${ENTRIES[@]}"; do
    IFS=$'\t' read -r id repository revision paths <<<"$entry"
    target="$DEST/$id"
    if [ -d "$target/.git" ] && [ "$(git -C "$target" rev-parse HEAD 2>/dev/null)" = "$revision" ]; then
        echo "$id: $revision (cached in $target)"
        continue
    fi
    rm -rf "$target"
    git init --quiet "$target"
    git -C "$target" remote add origin "$repository"
    if [ -n "$paths" ]; then
        # Anchor every listed path at the repository root.
        patterns=()
        for path in $paths; do
            patterns+=("/${path#/}")
        done
        git -C "$target" sparse-checkout set --no-cone "${patterns[@]}"
    fi
    git -C "$target" fetch --quiet --depth 1 --filter=blob:none origin "$revision"
    git -C "$target" -c advice.detachedHead=false checkout --quiet FETCH_HEAD
    actual=$(git -C "$target" rev-parse HEAD)
    if [ "$actual" != "$revision" ]; then
        echo "$id: fetched $actual, expected $revision" >&2
        exit 1
    fi
    echo "$id: $revision -> $target"
done
