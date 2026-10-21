#!/usr/bin/env python3
"""Replay recorded specs through the tursomysql backend in bounded processes.

Every `.sqltest` file runs in its own `sqltest run --output json` process,
several at a time, so the memory of a replay is bounded by its largest file
rather than by the whole corpus. Each process runs under an address-space
limit: a spec that makes Turso allocate without bound (for example `REPEAT(s,
1073741825)`) then aborts its process at once instead of exhausting the
machine.

A file whose process crashes, or outlives `--process-timeout`, is replayed
again one test per process; a test that crashes alone is reported as an error
with the end of the process's stderr, and every other test of the file keeps
its real outcome. Hanging statements are not crashes: the backend fails a
block that outlives `--timeout` (see testing/sqltest/src/backends/tursomysql.rs).

The per-file reports are merged into one report in the `sqltest --output json`
format, which tools/score.py reads. Files under `specs/local/` are skipped
unless `--include-local` is given.
"""

import argparse
import collections
import importlib.util
import json
import os
import pathlib
import re
import signal
import subprocess
import sys
import tempfile
import time

TOOLS = pathlib.Path(__file__).resolve().parent


def load_spec_lib():
    spec = importlib.util.spec_from_file_location("spec_lib", TOOLS / "spec_lib.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


SPEC_LIB = load_spec_lib()


def collect_files(paths, include_local):
    """Every .sqltest file under `paths`, skipping specs/local/ unless asked."""
    files = []
    for path in paths:
        candidates = sorted(path.rglob("*.sqltest")) if path.is_dir() else [path]
        for file in candidates:
            parents = file.resolve().parents
            local = any(p.name == "local" and p.parent.name == "specs" for p in parents)
            if include_local or not local:
                files.append(file)
    return list(dict.fromkeys(files))


def test_names(path):
    """Names of the tests in a .sqltest file, in file order."""
    text = path.read_text(encoding="utf-8")
    try:
        return [test["name"] for test in SPEC_LIB.read_sqltest(text)["tests"]]
    except ValueError:
        # Not in the recorder's subset of the DSL: fall back to the test headers.
        return re.findall(r"^test\s+(\S+)\s*\{", text, re.MULTILINE)


def describe_status(returncode):
    if returncode < 0:
        try:
            return f"killed by {signal.Signals(-returncode).name}"
        except ValueError:
            return f"killed by signal {-returncode}"
    return f"exit status {returncode}"


class Job:
    """One sqltest process: a whole file, or one test of a file."""

    def __init__(self, path, test=None):
        self.path = path
        self.test = test

    def describe(self):
        return f"{self.path}::{self.test}" if self.test else str(self.path)


class Replay:
    def __init__(self, args):
        self.args = args
        self.results = collections.defaultdict(list)  # path -> [result]
        self.crashes = []
        self.processes = 0

    def command(self, job):
        command = [
            str(self.args.sqltest), "run", str(job.path), "--backend", "tursomysql",
            "--output", "json", "--jobs", "1", "--timeout", str(self.args.timeout),
        ]
        if job.test is not None:
            command += ["--filter", job.test]
        if self.args.memory_mb:
            # `ulimit -v` takes KiB; exec keeps the runner's exit status.
            command = ["sh", "-c", 'ulimit -v "$0" && exec "$@"',
                       str(self.args.memory_mb * 1024)] + command
        return command

    def start(self, job):
        stdout = tempfile.TemporaryFile()
        stderr = tempfile.TemporaryFile()
        process = subprocess.Popen(self.command(job), stdout=stdout, stderr=stderr,
                                   stdin=subprocess.DEVNULL)
        self.processes += 1
        return {"job": job, "process": process, "stdout": stdout, "stderr": stderr,
                "start": time.monotonic(), "killed": False}

    def finish(self, running):
        """Record a finished process. Returns follow-up jobs."""
        job = running["job"]
        returncode = running["process"].returncode
        running["stdout"].seek(0)
        output = running["stdout"].read()
        running["stderr"].seek(0)
        errors = running["stderr"].read().decode("utf-8", "replace")
        running["stdout"].close()
        running["stderr"].close()
        report = None
        unreadable = False
        if returncode in (0, 1, 2) and not running["killed"]:
            # The runner exits 0 or 1 after writing its report, and 2 when it
            # refuses its arguments; anything else is a crash. Output that is
            # not a report (a statement that prints to stdout) is handled like
            # a crash, so the file's other tests still get their outcomes.
            try:
                report = json.loads(output)
            except ValueError:
                if returncode == 2:
                    raise SystemExit(f"sqltest refused {job.describe()}: {errors.strip()}")
                unreadable = True

        if report is not None:
            results = [r for f in report.get("files", []) for r in f.get("results", [])]
            if job.test is None:
                self.results[job.path].extend(results)
                return []
            mine = [r for r in results if r.get("name") == job.test]
            if mine:
                self.results[job.path].extend(mine)
            else:
                self.results[job.path].append({
                    "name": job.test, "outcome": "error", "duration_ms": 0,
                    "reason": "the test was not reported by its own sqltest process"})
            return []

        if running["killed"]:
            status = f"killed after {self.args.process_timeout} s"
        elif unreadable:
            status = f"wrote stdout that is not a JSON report ({output[:80]!r})"
        else:
            status = describe_status(returncode)
        tail = errors.strip()[-1500:]
        self.crashes.append((job.describe(), status, tail))
        print(f"replay: {job.describe()}: sqltest {status}"
              + (f": {tail.splitlines()[-1]}" if tail else ""), file=sys.stderr)
        if job.test is None:
            names = test_names(job.path)
            print(f"replay: rerunning the {len(names)} tests of {job.path} one per process",
                  file=sys.stderr)
            return [Job(job.path, name) for name in names]
        self.results[job.path].append({
            "name": job.test, "outcome": "error",
            "duration_ms": int((time.monotonic() - running["start"]) * 1000),
            "reason": f"sqltest {status} while running this test alone" + (f":\n{tail}" if tail else ""),
        })
        return []

    def run(self, files):
        # Largest files first, so the last processes to finish are short ones.
        pending = collections.deque(Job(path) for path in sorted(files, key=lambda p: -p.stat().st_size))
        running = []
        started = time.monotonic()
        last_progress = started
        files_done = 0
        while pending or running:
            while pending and len(running) < self.args.jobs:
                running.append(self.start(pending.popleft()))
            time.sleep(0.02)
            still_running = []
            for item in running:
                process = item["process"]
                if process.poll() is None:
                    if time.monotonic() - item["start"] > self.args.process_timeout:
                        process.kill()
                        process.wait()
                        item["killed"] = True
                    else:
                        still_running.append(item)
                        continue
                follow_up = self.finish(item)
                # Reruns go first: they finish the file and its outcomes.
                pending.extendleft(reversed(follow_up))
                if item["job"].test is None and not follow_up:
                    files_done += 1
            running = still_running
            now = time.monotonic()
            if now - last_progress >= self.args.progress_interval:
                last_progress = now
                tests = sum(len(r) for r in self.results.values())
                passed = sum(r.get("outcome") == "passed" for rs in self.results.values() for r in rs)
                print(f"replay: {files_done}/{len(files)} files, {tests} tests, {passed} passed, "
                      f"{len(self.crashes)} crashed processes, {now - started:.0f} s", file=sys.stderr)
        return time.monotonic() - started

    def report(self, files, elapsed):
        outcomes = collections.Counter()
        report_files = []
        for path in files:
            results = sorted(self.results.get(path, []), key=lambda r: r.get("name", ""))
            outcomes.update(r.get("outcome", "unknown") for r in results)
            report_files.append({
                "path": str(path),
                "results": results,
                "duration_ms": sum(r.get("duration_ms", 0) for r in results),
            })
        return {
            "files": report_files,
            "summary": {
                "total": sum(outcomes.values()),
                "passed": outcomes["passed"],
                "failed": outcomes["failed"],
                "skipped": outcomes["skipped"],
                "errors": outcomes["error"],
                "duration_ms": int(elapsed * 1000),
                "processes": self.processes,
                "crashed_processes": len(self.crashes),
            },
        }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("paths", nargs="+", type=pathlib.Path, help=".sqltest files or directories")
    parser.add_argument("--sqltest", type=pathlib.Path, required=True, help="sqltest binary")
    parser.add_argument("--output", type=pathlib.Path, required=True, help="merged JSON report")
    parser.add_argument("--jobs", type=int, default=os.cpu_count() or 2,
                        help="sqltest processes at a time (default: CPU count)")
    parser.add_argument("--timeout", type=int, default=30,
                        help="seconds one setup or test block may run (default: 30)")
    parser.add_argument("--memory-mb", type=int, default=2048,
                        help="address-space limit of each process in MiB, 0 for none (default: 2048)")
    parser.add_argument("--process-timeout", type=int, default=3600,
                        help="seconds one process may run before it is killed (default: 3600)")
    parser.add_argument("--include-local", action="store_true", help="also replay specs/local/")
    parser.add_argument("--progress-interval", type=float, default=60.0, help=argparse.SUPPRESS)
    args = parser.parse_args(argv)

    missing = [p for p in args.paths if not p.exists()]
    if missing:
        parser.error(f"no such path: {missing[0]}")
    if not args.sqltest.is_file():
        parser.error(f"no sqltest binary at {args.sqltest}")
    files = collect_files(args.paths, args.include_local)
    if not files:
        parser.error("no .sqltest files to replay")

    replay = Replay(args)
    elapsed = replay.run(files)
    report = replay.report(files, elapsed)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=1) + "\n")
    summary = report["summary"]
    print(f"replay: {summary['total']} tests in {len(files)} files: {summary['passed']} passed, "
          f"{summary['failed']} failed, {summary['errors']} errors, {summary['skipped']} skipped; "
          f"{summary['processes']} processes, {summary['crashed_processes']} crashed; "
          f"{elapsed:.0f} s; wrote {args.output}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
