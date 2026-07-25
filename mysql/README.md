# Turso MySQL frontend

This directory contains the MySQL parser, Turso frontend, CLI, and conformance configuration.

The bootstrap supports literal and anonymous `?` parameter `SELECT` projections and lowers the parsed MySQL AST into Turso's AST before VDBE execution. Embedded callers can bind `NULL`, `i64`, and UTF-8 text to each 1-based slot with `Statement::bind_at`; callers must bind every slot because MySQL missing-argument diagnostic parity is not implemented. Numbered and named placeholders, parameter expressions, CLI binding, and MySQL wire behavior remain unsupported.

```bash
cargo run -p tursomysql
cargo run -p tursomysql -- "SELECT 'hello from mysql' AS greeting"
```

The frozen compatibility oracle is documented in [`TARGET.md`](TARGET.md). Current support is tracked in [`COMPAT.md`](COMPAT.md), and the complete owned test workflow is documented in [`conformance/README.md`](conformance/README.md).

```bash
CARGO=/path/to/cargo ./mysql/test.sh
```
