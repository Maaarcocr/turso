#!/usr/bin/env python3
"""Score a replay of the recorded MySQL specs against the ratchet baseline.

Reads one or more `sqltest run --output json` results for files under
mysql/conformance/specs/, the per-directory manifest.jsonl files, and
mysql/conformance/baseline.txt (one `<file>::<test-name>` per line, the file
relative to specs/).

* A baseline spec that now fails, errors, is skipped, or is missing from a file
  that was replayed is a regression: the exit status is 1.
* `--update` adds newly passing specs to the baseline. It never removes an
  entry unless `--allow-shrink` is given.
* The report shows pass counts per source, the highest fully cleared level
  (every spec at or below it passes), and the first failures at the lowest
  failing level with their source provenance: the next specs to work on.
"""

import argparse
import collections
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[3]
CONFORMANCE = ROOT / "mysql/conformance"


def spec_relpath(path, specs_root):
    """Map a result file path to a path relative to specs/."""
    candidate = pathlib.Path(path)
    for base in (pathlib.Path.cwd(), CONFORMANCE, ROOT):
        absolute = candidate if candidate.is_absolute() else base / candidate
        try:
            return absolute.resolve().relative_to(specs_root.resolve()).as_posix()
        except ValueError:
            continue
    text = candidate.as_posix()
    marker = "specs/"
    if marker in text:
        return text[text.rindex(marker) + len(marker):]
    return None


def load_results(paths, specs_root):
    """Return ({test id: (outcome, reason)}, {replayed file})."""
    outcomes = {}
    files = set()
    for path in paths:
        data = json.loads(pathlib.Path(path).read_text())
        for file_result in data.get("files", []):
            relpath = spec_relpath(file_result["path"], specs_root)
            if relpath is None:
                continue
            files.add(relpath)
            for result in file_result.get("results", []):
                test_id = f"{relpath}::{result['name']}"
                outcome = result.get("outcome", "unknown")
                previous = outcomes.get(test_id)
                if previous is None or previous[0] == "passed":
                    outcomes[test_id] = (outcome, result.get("reason") or result.get("message") or "")
    return outcomes, files


def source_dir_of(manifest, specs_root):
    parts = manifest.relative_to(specs_root).parts
    if parts[0] == "local" and len(parts) > 2:
        return "/".join(parts[:2])
    return parts[0]


def load_manifests(specs_root):
    specs = {}
    for manifest in sorted(specs_root.rglob("manifest.jsonl")):
        source = source_dir_of(manifest, specs_root)
        with open(manifest, encoding="utf-8") as handle:
            for line in handle:
                if not line.strip():
                    continue
                item = json.loads(line)
                item["source_name"] = source
                specs[f"{source}/{item['test']}"] = item
    return specs


def load_baseline(path):
    if not path.exists():
        return []
    entries = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            entries.append(line)
    return entries


def highest_cleared(items, passed):
    """Largest level L such that every spec with level <= L passes."""
    by_level = collections.defaultdict(list)
    for test_id, item in items:
        by_level[item["level"]].append(test_id)
    cleared = None
    for level in sorted(by_level):
        if all(test_id in passed for test_id in by_level[level]):
            cleared = level
        else:
            break
    return cleared


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("results", nargs="+", type=pathlib.Path, help="sqltest --output json files")
    parser.add_argument("--specs", type=pathlib.Path, default=CONFORMANCE / "specs")
    parser.add_argument("--baseline", type=pathlib.Path, default=CONFORMANCE / "baseline.txt")
    parser.add_argument("--update", action="store_true", help="add newly passing specs to the baseline")
    parser.add_argument("--allow-shrink", action="store_true",
                        help="with --update, also drop baseline entries that no longer pass")
    parser.add_argument("--next", type=int, default=20, help="failures to list at the lowest failing level")
    args = parser.parse_args(argv)

    specs = load_manifests(args.specs)
    outcomes, replayed_files = load_results(args.results, args.specs)
    baseline = load_baseline(args.baseline)
    baseline_set = set(baseline)
    passed = {test_id for test_id, (outcome, _) in outcomes.items() if outcome == "passed"}

    regressions = []
    not_replayed = 0
    for test_id in baseline:
        file = test_id.split("::", 1)[0]
        if file not in replayed_files:
            not_replayed += 1
            continue
        if test_id not in outcomes:
            regressions.append((test_id, "missing from the replay"))
        elif outcomes[test_id][0] != "passed":
            outcome, reason = outcomes[test_id]
            regressions.append((test_id, f"{outcome}: {reason.strip().splitlines()[0] if reason.strip() else ''}"))
    newly_passing = sorted(passed - baseline_set)
    unknown = sorted(test_id for test_id in outcomes if test_id not in specs)

    # Per-source totals over the specs whose files were replayed.
    print("source                          specs   replayed     passed   baseline  cleared-level")
    sources = sorted({item["source_name"] for item in specs.values()})
    lowest_failures = []
    for source in sources:
        items = [(test_id, item) for test_id, item in specs.items() if item["source_name"] == source]
        replayed = [(t, i) for t, i in items if t.split("::", 1)[0] in replayed_files]
        if not replayed:
            print(f"{source:<28} {len(items):>8} {0:>10} {'-':>10} "
                  f"{sum(1 for t, _ in items if t in baseline_set):>10}  -")
            continue
        source_passed = sum(1 for t, _ in replayed if t in passed)
        cleared = highest_cleared(replayed, passed)
        in_baseline = sum(1 for t, _ in items if t in baseline_set)
        print(f"{source:<28} {len(items):>8} {len(replayed):>10} {source_passed:>10} {in_baseline:>10}  "
              f"{'none' if cleared is None else cleared}")
        lowest_failures.extend((i["level"], t, i) for t, i in replayed if t not in passed)

    levels = collections.defaultdict(lambda: [0, 0])
    for test_id, item in specs.items():
        if test_id.split("::", 1)[0] in replayed_files:
            band = item["level"] // 100 * 100
            levels[band][0] += 1
            levels[band][1] += test_id in passed
    if levels:
        print("\nlevels       specs     passed")
        for band in sorted(levels):
            total, ok = levels[band]
            print(f"{band:>4}-{band + 99:<4} {total:>9} {ok:>10}")

    if lowest_failures:
        lowest_failures.sort(key=lambda entry: (entry[0], entry[1]))
        print(f"\nNext specs to work on (lowest failing levels first, {len(lowest_failures)} failing):")
        for level, test_id, item in lowest_failures[: args.next]:
            source = item.get("source", {})
            reason = outcomes.get(test_id, ("not run", ""))[1].strip().splitlines()
            print(f"  [{level}] {test_id}  ({item['id']}; {source.get('path')}:{source.get('line')}, "
                  f"{source.get('license')})")
            if reason:
                print(f"         {reason[0][:160]}")

    print(f"\npassed {len(passed)} of {len(outcomes)} replayed specs; baseline {len(baseline_set)}; "
          f"newly passing {len(newly_passing)}; regressions {len(regressions)}")
    if not_replayed:
        print(f"note: {not_replayed} baseline specs are in files this replay did not include")
    if unknown:
        print(f"note: {len(unknown)} replayed tests are not in any manifest (e.g. {unknown[0]})")
    for test_id, reason in regressions[:50]:
        print(f"REGRESSION {test_id}: {reason}")

    if args.update:
        if regressions and not args.allow_shrink:
            print("refusing to update the baseline while it has regressions (use --allow-shrink to drop them)",
                  file=sys.stderr)
            return 1
        kept = [t for t in baseline if t not in {r[0] for r in regressions}]
        updated = sorted(set(kept) | set(newly_passing))
        header = []
        if args.baseline.exists():
            for line in args.baseline.read_text().splitlines():
                if not line.startswith("#"):
                    break
                header.append(line + "\n")
        args.baseline.write_text("".join(header) + "".join(f"{t}\n" for t in updated))
        print(f"baseline: {len(baseline_set)} -> {len(updated)} entries ({args.baseline})")
        return 0
    return 1 if regressions else 0


if __name__ == "__main__":
    raise SystemExit(main())
