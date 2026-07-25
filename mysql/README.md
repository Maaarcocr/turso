# Turso MySQL frontend

This directory contains the MySQL parser, Turso frontend, CLI, and conformance configuration.

The bootstrap supports literal-only `SELECT` statements and lowers the parsed MySQL AST into Turso's AST before VDBE execution.

```bash
cargo run -p tursomysql
cargo run -p tursomysql -- "SELECT 'hello from mysql' AS greeting"
```

The frozen compatibility oracle is documented in [`TARGET.md`](TARGET.md). Current support is tracked in [`COMPAT.md`](COMPAT.md), and the complete owned test workflow is documented in [`conformance/README.md`](conformance/README.md).

```bash
CARGO=/path/to/cargo ./mysql/test.sh
```
