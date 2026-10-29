#!/usr/bin/env python3
"""Write specs/app-issues/ from the GitHub issues of Maaarcocr/turso: one
shard per issue, one test per statement of the issue's reproduction that MySQL
answered in the issue's "Expected" section.

    gh issue list --repo Maaarcocr/turso --state all --limit 200 \
        --json number,title,body > issues.json
    python3 -I tools/import_app_issues.py issues.json specs/app-issues
"""
import json
import re
import sys
from pathlib import Path

ISSUES = Path(sys.argv[1])
OUT = Path(sys.argv[2])
REPO = "https://github.com/Maaarcocr/turso"
LEVEL = 100
# Issues whose later statements read session state that earlier queries set
# (LAST_INSERT_ID, ROW_COUNT, FOUND_ROWS, named locks): every earlier
# statement, queries included, runs before each test.
STATEFUL = {16, 18}
# Issues with no SQL to replay.
SKIPPED = {33: "the wire protocol has no SQL statements to replay"}
# Statements whose MySQL output cannot round-trip through the .sqltest format.
UNREPRESENTABLE = {"SHOW CREATE TABLE t1": "its value has line breaks"}


def section(body, title):
    start = body.index(f"## {title}")
    rest = body[start:]
    block = re.search(r"```[a-z]*\n(.*?)```", rest, re.S)
    return block.group(1)


def statements(sql):
    out, current = [], []
    for line in sql.splitlines():
        if not line.strip():
            continue
        current.append(line)
        if line.rstrip().endswith(";"):
            out.append("\n".join(current).rstrip()[:-1].rstrip())
            current = []
    assert not current, current
    return out


def expected(text):
    results, statement, lines = {}, None, []

    def flush():
        if statement is None:
            return
        kind, rows, error = "rows", [], None
        for line in lines:
            if line.startswith("  warning:"):
                continue
            if line.startswith("OK, "):
                kind = "ok"
            elif line.startswith("ERROR "):
                kind, error = "error", line
            else:
                rows.append(line)
        assert kind == "rows" or not rows, (statement, lines)
        results[statement] = (kind, rows, error)

    for line in text.splitlines():
        if line.startswith("mysql> "):
            flush()
            statement, lines = line[len("mysql> "):].rstrip().rstrip(";").rstrip(), []
        else:
            lines.append(line)
    flush()
    return results


def escaped(text):
    return text.replace("\\", "\\\\").replace("{", "\\{").replace("}", "\\}")


def main():
    issues = sorted(json.loads(ISSUES.read_text()), key=lambda issue: issue["number"])
    OUT.mkdir(parents=True, exist_ok=True)
    manifest, report = [], []
    for issue in issues:
        number, title, body = issue["number"], issue["title"], issue["body"]
        url = f"{REPO}/issues/{number}"
        if number in SKIPPED:
            report.append((number, title, 0, SKIPPED[number]))
            continue
        sql = statements(section(body, "Reproduce"))
        answers = expected(section(body, "Expected"))
        flats = [" ".join(s.split("\n")) for s in sql]
        for key in list(answers):
            cut = re.fullmatch(r"(.*) \.\.\. \(\d+ characters\)", key, re.S)
            if cut:
                [full] = [flat for flat in flats if flat.startswith(cut.group(1))]
                answers[full] = answers.pop(key)
        unknown = set(answers) - set(flats)
        assert not unknown, (number, unknown)
        shard = f"issue-{number:02}.sqltest"
        lines = [
            f"# Specs from {url}: {title}",
            "# Expected values are MySQL 8.4.8's (Percona Server 8.4.8-8, default 8.4",
            "# profile, time_zone '+00:00') as reported in the issue; they were not",
            "# recorded by tools/record_mysql.py. One test per statement MySQL answered;",
            "# every earlier statement that MySQL ran without error is a setup.",
            "@database :memory:",
            "",
        ]
        setups, tests, notes = [], [], []
        for index, statement in enumerate(sql, 1):
            flat = " ".join(statement.split("\n"))
            answer = answers.get(flat)
            if flat in UNREPRESENTABLE:
                notes.append(f"`{flat}` is left out: {UNREPRESENTABLE[flat]}")
                answer = ("skipped", [], None)
            if answer is not None and answer[0] != "skipped":
                kind, rows, error = answer
                name = f"s{index}"
                block = []
                if error:
                    block.append(f"# mysql-error: {error[len('ERROR '):]}")
                block += [f"@setup {setup}" for setup in setups]
                block.append(f"test {name} {{")
                block.append(f"    {escaped(statement)};")
                block.append("}")
                if kind == "error":
                    block.append("expect error {")
                else:
                    block.append("expect {")
                    block += [f"    {escaped(row)}" for row in rows]
                block.append("}")
                tests.append("\n".join(block))
                manifest_test = [name, f"app-issues/issue-{number:02}:S{index}", LEVEL, 0, index]
                manifest.append((shard, url, manifest_test))
            ran = answer is None or answer[0] == "ok" or (
                number in STATEFUL and answer[0] == "rows")
            if ran:
                setup = f"p{index}"
                lines.append(f"setup {setup} {{\n    {escaped(statement)};\n}}\n")
                setups.append(setup)
        lines.append("\n\n".join(tests))
        (OUT / shard).write_text("\n".join(lines) + "\n")
        report.append((number, title, len(tests), "; ".join(notes)))
    shards = {}
    for shard, url, test in manifest:
        shards.setdefault(shard, (url, []))[1].append(test)
    with open(OUT / "manifest.jsonl", "w") as handle:
        for shard, (url, tests) in sorted(shards.items()):
            handle.write(json.dumps({
                "group": f"app-issues/{shard.removesuffix('.sqltest')}",
                "license": "MIT",
                "paths": [url],
                "repo": REPO,
                "revision": "",
                "shard": shard,
                "tests": tests,
            }, sort_keys=True, separators=(",", ":")) + "\n")
    with open(OUT / "REPORT.md", "w") as handle:
        handle.write("# app-issues\n\n")
        handle.write(
            "Specs from the GitHub issues filed on Maaarcocr/turso after running an\n"
            "application test suite against turso_mysql. Each issue's reproduction\n"
            "becomes one shard, with one test per statement whose MySQL answer the\n"
            "issue reports; the statements before it that MySQL ran are its setups.\n"
            "The expected values come from the issues (MySQL 8.4.8), not from\n"
            "tools/record_mysql.py. Regenerate with tools/import_app_issues.py.\n\n")
        handle.write("| Issue | Title | Tests | Notes |\n|---|---|---|---|\n")
        for number, title, count, note in report:
            handle.write(f"| [#{number}]({REPO}/issues/{number}) | {title.replace('|', '/')} | {count} | {note} |\n")
    print(sum(len(tests) for _, tests in shards.values()), "tests in", len(shards), "shards")


main()
