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
5. Clippy for all MySQL targets with warnings denied
6. A direct CLI hello-world smoke test

The differential tier compares owned literal cases against the pinned MySQL 8.4.10 Docker oracle:

```bash
CARGO=/path/to/cargo ./mysql/conformance/differential.py
```

This tier requires Docker and may pull the pinned image on its first run. It checks both engines against explicit expected output so matching bugs cannot pass by agreement.

The compatibility target is frozen in `target.toml`. Feature prerequisites and stage-by-stage status are tracked in `curriculum.toml`.

## Mined implementation curriculum

The quantity-first corpus miner and source pins live under [`corpus/`](corpus/README.md). Five thousand Apache-licensed go-mysql-server cases are checked in and can be regenerated with:

```bash
CARGO=/path/to/cargo ./mysql/conformance/mine-go-mysql-server.sh
```

A second recipe extracts a balanced 5,000-case syntax curriculum from the official MySQL 8.4.10 MTR suite and reports the earliest parser gaps:

```bash
CARGO=/path/to/cargo ./mysql/conformance/mine-mtr.sh
```

Set either miner's `*_LIMIT=0` variable to extract every eligible static case. Generated MTR SQL stays local pending licensing review.

Unsupported owned cases are assertions, not skips. When a feature is implemented, move its cases to successful execution coverage and add MySQL differential cases where values or diagnostics are observable. Never weaken or delete a rejection case merely because it starts succeeding.
