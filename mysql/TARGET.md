# MySQL compatibility target

The compatibility oracle is frozen to MySQL 8.4.10, from the MySQL 8.4 LTS series.

| Property | Pinned value |
|---|---|
| Source tag | `mysql-8.4.10` |
| Source commit | `6adc159923b7b6abbe649949551ec25264c2daf9` |
| Container | `mysql:8.4.10@sha256:8dbcf531a03aade657e181b9cf2f1d1803ce621a1d55610cb44cb531ab7d7db6` |
| SQL mode | `ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION` |
| Character set | `utf8mb4` |
| Collation | `utf8mb4_0900_ai_ci` |
| Time zone | UTC (`+00:00`) |
| `lower_case_table_names` | `0` |

The machine-readable copy is [`conformance/target.toml`](conformance/target.toml). Tests must use the pinned image digest rather than the floating `8.4` tag.

The initial target is SQL and application compatibility. InnoDB file compatibility, replication, binlogs, plugins, and MariaDB behavior are outside the bootstrap scope.
