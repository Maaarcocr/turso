# go-mysql-server enginetest import

These JSONL shards are generated from the go-mysql-server `enginetest` fixtures at commit `478bed778dd282aa13aa48ebd75b0549ebe75698`.

- Source: <https://github.com/dolthub/go-mysql-server>
- Copyright: DoltHub, Inc. and contributors
- License: Apache-2.0
- Local license copy: [`../../../../licenses/mysql/go-mysql-server-apache-license.md`](../../../../licenses/mysql/go-mysql-server-apache-license.md)

Regenerate the exact 5,000-case sample from the repository root:

```bash
./mysql/conformance/mine-go-mysql-server.sh
```

The import preserves source file and line provenance. Positive cases were successful setup or query fixtures in the source suite; cases marked `unknown` came from assertions expecting an error and require MySQL-oracle classification before becoming positive or negative syntax requirements.
