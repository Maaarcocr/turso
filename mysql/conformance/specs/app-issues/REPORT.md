# app-issues

Specs from the GitHub issues filed on Maaarcocr/turso after running an
application test suite against turso_mysql. Each issue's reproduction
becomes one shard, with one test per statement whose MySQL answer the
issue reports; the statements before it that MySQL ran are its setups.
The expected values come from the issues (MySQL 8.4.8), not from
tools/record_mysql.py. Regenerate with tools/import_app_issues.py.

| Issue | Title | Tests | Notes |
|---|---|---|---|
| [#1](https://github.com/Maaarcocr/turso/issues/1) | Optimizer hints MAX_EXECUTION_TIME and SET_VAR make the whole SELECT fail | 3 |  |
| [#2](https://github.com/Maaarcocr/turso/issues/2) | Index hints (USE INDEX, FORCE INDEX, IGNORE INDEX) make the statement fail | 4 |  |
| [#3](https://github.com/Maaarcocr/turso/issues/3) | SHOW statements are refused (SHOW SESSION VARIABLES, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE) | 4 | `SHOW CREATE TABLE t1` is left out: its value has line breaks |
| [#4](https://github.com/Maaarcocr/turso/issues/4) | Reading system variables (@@sql_mode) and the common driver session SET are refused | 4 |  |
| [#5](https://github.com/Maaarcocr/turso/issues/5) | utf8mb4_general_ci and utf8mb4_unicode_ci are refused (tables, COLLATE, SET NAMES) | 5 |  |
| [#6](https://github.com/Maaarcocr/turso/issues/6) | Tables and columns with CHARACTER SET utf8 (utf8mb3) are refused | 2 |  |
| [#7](https://github.com/Maaarcocr/turso/issues/7) | User variables (SET @v = ..., SELECT @v) are refused, so mysqldump output does not load | 4 |  |
| [#8](https://github.com/Maaarcocr/turso/issues/8) | INVISIBLE columns are refused | 5 |  |
| [#9](https://github.com/Maaarcocr/turso/issues/9) | Setting the AUTO_INCREMENT counter is refused (table option AUTO_INCREMENT=N and ALTER TABLE ... AUTO_INCREMENT = N) | 5 |  |
| [#10](https://github.com/Maaarcocr/turso/issues/10) | AUTO_INCREMENT on a column that is not the whole primary key is refused | 4 |  |
| [#11](https://github.com/Maaarcocr/turso/issues/11) | DEFAULT CURRENT_TIMESTAMP(6) (fractional seconds) is refused | 4 |  |
| [#12](https://github.com/Maaarcocr/turso/issues/12) | Key length check rejects keys MySQL accepts: every integer or temporal key part counts as 8 bytes | 3 |  |
| [#13](https://github.com/Maaarcocr/turso/issues/13) | INSERT IGNORE is refused | 2 |  |
| [#14](https://github.com/Maaarcocr/turso/issues/14) | Transaction and locking statements are refused (BEGIN, START TRANSACTION, COMMIT, SAVEPOINT, isolation level, FOR UPDATE, LOCK TABLES) | 15 |  |
| [#15](https://github.com/Maaarcocr/turso/issues/15) | information_schema is missing (TABLES, COLUMNS, STATISTICS) | 3 |  |
| [#16](https://github.com/Maaarcocr/turso/issues/16) | DATABASE(), VERSION(), CONNECTION_ID(), LAST_INSERT_ID(), ROW_COUNT(), FOUND_ROWS() are missing | 7 |  |
| [#17](https://github.com/Maaarcocr/turso/issues/17) | CAST(x AS CHAR CHARACTER SET ascii) is a parse error | 3 |  |
| [#18](https://github.com/Maaarcocr/turso/issues/18) | Named locks (GET_LOCK, RELEASE_LOCK, RELEASE_ALL_LOCKS, IS_FREE_LOCK) are missing | 4 |  |
| [#19](https://github.com/Maaarcocr/turso/issues/19) | JSON_ARRAYAGG and JSON_OBJECTAGG are missing | 2 |  |
| [#20](https://github.com/Maaarcocr/turso/issues/20) | GROUP BY of a primary key refuses other columns of the same table (MySQL accepts them by functional dependence) | 3 |  |
| [#21](https://github.com/Maaarcocr/turso/issues/21) | Binary literals with bytes that are not UTF-8 cannot be stored in a BLOB (X'..' and _binary '..') | 4 |  |
| [#22](https://github.com/Maaarcocr/turso/issues/22) | A WHERE with more than about 100 OR terms fails with "expressions nested deeper than 100 levels" | 2 |  |
| [#23](https://github.com/Maaarcocr/turso/issues/23) | Row order without ORDER BY differs from InnoDB (covering secondary index and joins) | 3 |  |
| [#24](https://github.com/Maaarcocr/turso/issues/24) | DATETIME/DATE values are stored as written: no normalization, no fraction padding, invalid dates accepted | 5 |  |
| [#25](https://github.com/Maaarcocr/turso/issues/25) | SET time_zone to anything but UTC is refused, and TIMESTAMP is not converted | 5 |  |
| [#26](https://github.com/Maaarcocr/turso/issues/26) | A JSON column returns the text as inserted; MySQL returns its normalized form | 3 |  |
| [#27](https://github.com/Maaarcocr/turso/issues/27) | Arithmetic on a BIGINT UNSIGNED column above 2^63-1 returns a double | 1 |  |
| [#28](https://github.com/Maaarcocr/turso/issues/28) | ORDER BY an ENUM column sorts by text; MySQL sorts by member position | 2 |  |
| [#29](https://github.com/Maaarcocr/turso/issues/29) | STORED generated columns are refused | 4 |  |
| [#30](https://github.com/Maaarcocr/turso/issues/30) | TRUNCATE, multi-table DELETE and UPDATE ... ORDER BY ... LIMIT are refused | 5 |  |
| [#31](https://github.com/Maaarcocr/turso/issues/31) | SQL PREPARE / EXECUTE / DEALLOCATE PREPARE are refused | 3 |  |
| [#32](https://github.com/Maaarcocr/turso/issues/32) | GROUP_CONCAT(... ORDER BY ...) is refused | 1 |  |
| [#33](https://github.com/Maaarcocr/turso/issues/33) | No MySQL wire-protocol server: standard MySQL clients and drivers cannot connect | 0 | the wire protocol has no SQL statements to replay |
