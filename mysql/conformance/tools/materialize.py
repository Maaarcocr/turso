#!/usr/bin/env python3
"""Rebuild local MySQL specs from committed results and a pinned source checkout.

Sources whose SQL must not be committed (GPL or license-unclear test suites,
`redistribute_generated_cases = false` in corpus/sources.lock.toml) are
recorded with `record_mysql.py --results-only`, which commits only MySQL's
results under `results/<source>/`: per candidate its id, a SHA-256 of the exact
setup and sql, the ordering flag and the recorded outcome, never SQL text.

For each source this tool

1. checks out the pinned revision with fetch-sources.sh into an ignored cache
   (or uses `--checkout`), reading it as data only;
2. re-runs the source's deterministic importer (the command stored in
   `results/<source>/meta.json`) under `python3 -I` into `candidates/<source>/`;
3. joins the candidates with the committed results by id, using a result only
   when its SHA-256 matches the regenerated candidate (otherwise the candidate
   is counted as `stale-result` and skipped; candidates without a result are
   `no-result`);
4. writes `.sqltest` shards, manifests and REPORT.md into the ignored
   `specs/local/<source>/` with the recorder's own `write_specs` and
   `write_report`, so the output is byte-identical to a direct recording.

No MySQL server is needed. Usage:

    python3 -I mysql/conformance/tools/materialize.py [source ...]
"""

import argparse
import collections
import importlib.util
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tomllib


def load_recorder():
    path = pathlib.Path(__file__).resolve().with_name("record_mysql.py")
    spec = importlib.util.spec_from_file_location("record_mysql", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


REC = load_recorder()
CONFORMANCE = REC.CONFORMANCE
FETCH = CONFORMANCE / "fetch-sources.sh"


def log(message):
    print(f"[materialize] {message}", file=sys.stderr, flush=True)


def read_meta(results_dir):
    path = results_dir / "meta.json"
    if not path.exists():
        raise SystemExit(f"{path} does not exist; record the source with record_mysql.py --results-only")
    meta = json.loads(path.read_text(encoding="utf-8"))
    if meta.get("format") != REC.RESULTS_FORMAT:
        raise SystemExit(f"{path}: results format {meta.get('format')}, this tool reads {REC.RESULTS_FORMAT}")
    return meta


def checkout_source(meta, args):
    """Return a checkout of the pinned revision (fetching it when needed)."""
    lock = tomllib.loads(REC.LOCK.read_text())
    entry = next((e for e in lock["source"] if e["id"] == meta["lock_id"]), None)
    if entry is None:
        raise SystemExit(f"{REC.LOCK.name} has no entry {meta['lock_id']}")
    if entry["revision"] != meta["revision"]:
        raise SystemExit(f"{meta['source']}: results were recorded at {meta['revision']}, but {REC.LOCK.name} "
                         f"pins {entry['revision']}; re-record the source")
    if args.checkout:
        checkout = args.checkout
    else:
        env = dict(os.environ, MYSQL_SPEC_SOURCES=str(args.sources_dir))
        subprocess.run([str(FETCH), meta["lock_id"]], check=True, env=env)
        checkout = args.sources_dir / meta["lock_id"]
    revision = subprocess.run(["git", "-C", str(checkout), "rev-parse", "HEAD"], check=True,
                              capture_output=True, text=True).stdout.strip()
    if revision != meta["revision"]:
        raise SystemExit(f"{checkout} is at {revision}, results need {meta['revision']}")
    return checkout


def run_importer(meta, checkout, candidates_dir):
    """Re-run the recorded importer command; returns the candidate files."""
    command = [part.format(checkout=checkout, candidates=candidates_dir) for part in meta["importer"]]
    script = (CONFORMANCE / command[0]).resolve()
    if CONFORMANCE.resolve() / "tools" != script.parent:
        raise SystemExit(f"importer {command[0]} is not a tool in mysql/conformance/tools/")
    if candidates_dir.exists():
        shutil.rmtree(candidates_dir)
    log(f"{meta['source']}: importing {checkout} with {command[0]}")
    result = subprocess.run([sys.executable, "-I", str(script), *command[1:]], capture_output=True, text=True)
    if result.returncode != 0:
        raise SystemExit(f"importer failed:\n{result.stdout[-4000:]}\n{result.stderr[-4000:]}")
    return sorted(candidates_dir.glob("*.jsonl"))


def join(plans, results):
    """Turn committed results into per-group outcomes, verifying every SHA-256.

    Returns ({group: {index: outcome}}, {group: fake runs for tally_outcomes},
    counters).
    """
    outcomes_by_group = {}
    runs_by_group = {}
    counts = collections.Counter()
    used = set()
    for plan in plans:
        outcomes = {}
        node_errors = {}
        for candidate in plan.candidates:
            entry = results.get(candidate.id)
            if entry is None:
                outcomes[candidate.index] = {"bucket": "no-result"}
                counts["no-result"] += 1
                continue
            used.add(candidate.id)
            if entry["sha256"] != REC.plan_digest(plan, candidate) or entry["ordered"] != candidate.record["ordered"]:
                outcomes[candidate.index] = {"bucket": "stale-result"}
                counts["stale-result"] += 1
                continue
            outcome, errors = REC.outcome_from_result(plan, candidate, entry)
            for node, errno in errors.items():
                if node_errors.setdefault(node, errno) != errno:
                    raise SystemExit(f"{candidate.id}: setup statement {node} has conflicting errnos")
            outcomes[candidate.index] = outcome
            counts["joined"] += 1
        outcomes_by_group[plan.name] = outcomes
        runs_by_group[plan.name] = {"a": {"node_errors": {str(node): [errno, "", ""]
                                                          for node, errno in node_errors.items()}}}
    counts["orphaned"] = len(set(results) - used)
    return outcomes_by_group, runs_by_group, counts


def materialize(source, args):
    results_dir = args.results_dir / source
    meta = read_meta(results_dir)
    if meta["source"] != source:
        raise SystemExit(f"{results_dir}/meta.json is for {meta['source']}")
    if meta["recorder_version"] != REC.RECORDER_VERSION:
        log(f"{source}: results come from recorder version {meta['recorder_version']}, "
            f"this recorder is {REC.RECORDER_VERSION}; consider re-recording")
    candidates_dir = args.candidates / source
    if args.reuse_candidates:
        files = sorted(candidates_dir.glob("*.jsonl"))
        if not files:
            raise SystemExit(f"--reuse-candidates: no candidates in {candidates_dir}")
    else:
        files = run_importer(meta, checkout_source(meta, args), candidates_dir)
    digest = REC.candidates_digest(files)
    same_candidates = digest == meta["candidates_sha256"]
    if not same_candidates:
        log(f"{source}: regenerated candidates differ from the recorded import "
            f"(sha256 {digest[:12]} vs {meta['candidates_sha256'][:12]}); every result is still verified per candidate")

    buckets = collections.Counter()
    details = collections.defaultdict(list)
    plans = REC.load_candidates(files, buckets, details)
    plan_list = [plans[name] for name in sorted(plans) if REC.source_of(name) == source]
    results = REC.read_results(results_dir)
    outcomes_by_group, runs_by_group, counts = join(plan_list, results)

    out_dir = args.out_dir / source
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)
    stats = REC.new_stats(buckets, details, meta["max_rows"], meta["max_expect_bytes"])
    for plan in plan_list:
        REC.tally_outcomes(plan, outcomes_by_group[plan.name], runs_by_group[plan.name], stats)
    written = REC.write_specs(out_dir, plan_list, outcomes_by_group, stats)
    importer_report = None
    if meta["import_report"]:
        report_path = candidates_dir / "IMPORT_REPORT.md"
        importer_report = report_path.read_text() if report_path.exists() else ""
    REC.write_report(out_dir / "REPORT.md", source, stats, importer_report, meta["server"])
    if args.check:
        REC.check_files(written, argparse.Namespace(no_check=False, sqltest_bin=args.sqltest_bin))
    same_report = (out_dir / "REPORT.md").read_bytes() == (results_dir / "REPORT.md").read_bytes()
    log(f"{source}: {len(files)} candidate files, {sum(len(p.candidates) for p in plan_list)} candidates; "
        f"results joined {counts['joined']}, stale {counts['stale-result']}, missing {counts['no-result']}, "
        f"orphaned {counts['orphaned']}")
    log(f"{source}: wrote {stats['tests']} tests in {stats['shards']} shards to {out_dir}; REPORT.md "
        + ("matches the committed one" if same_report else "differs from the committed one"))
    summary = {"source": source, "tests": stats["tests"], "shards": stats["shards"], "same_report": same_report,
               "same_candidates": same_candidates}
    for key in ("joined", "stale-result", "no-result", "orphaned"):
        summary[key] = counts[key]
    return summary


def available_sources(results_root):
    return sorted(path.parent.name for path in results_root.glob("*/meta.json"))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("names", nargs="*", metavar="source",
                        help="sources to materialize (default: every results/<source>/)")
    parser.add_argument("--results-dir", type=pathlib.Path, default=REC.RESULTS)
    parser.add_argument("--out-dir", type=pathlib.Path, default=CONFORMANCE / "specs/local")
    parser.add_argument("--candidates", type=pathlib.Path, default=CONFORMANCE / "candidates")
    parser.add_argument("--sources-dir", type=pathlib.Path,
                        default=pathlib.Path(os.environ.get("MYSQL_SPEC_SOURCES", CONFORMANCE / ".sources")),
                        help="fetch-sources.sh checkout cache (default .sources/)")
    parser.add_argument("--checkout", type=pathlib.Path,
                        help="use this checkout of the pinned revision instead of fetching (one source only)")
    parser.add_argument("--reuse-candidates", action="store_true",
                        help="skip fetching and importing; use candidates/<source>/ as it is")
    parser.add_argument("--check", action="store_true", help="validate the shards with `sqltest check`")
    parser.add_argument("--sqltest-bin", help="sqltest binary for --check")
    parser.add_argument("--strict", action="store_true",
                        help="exit nonzero unless every candidate has a matching result and REPORT.md matches")
    args = parser.parse_args(argv)
    names = args.names or available_sources(args.results_dir)
    if not names:
        parser.error(f"no results in {args.results_dir}")
    if args.checkout and len(names) != 1:
        parser.error("--checkout needs exactly one source")
    summaries = [materialize(source, args) for source in names]
    failed = False
    for summary in summaries:
        if summary["stale-result"] or summary["no-result"] or not summary["same_report"]:
            failed = True
        print(f"{summary['source']}: {summary['tests']} specs; {summary['joined']} results used, "
              f"{summary['stale-result']} stale, {summary['no-result']} missing, {summary['orphaned']} orphaned; "
              f"candidates {'identical' if summary['same_candidates'] else 'changed'}, "
              f"REPORT.md {'identical' if summary['same_report'] else 'changed'}")
    return 1 if args.strict and failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
