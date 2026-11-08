#!/usr/bin/env python3
"""Test-suite budget guard.

The suite is a tool, not a landfill. This fails when it grows or slows without
someone deliberately raising the budget, and it names the two honest fixes:
make the test cheaper, or move it to the heavy lane.

Usage:
  python3 scripts/test_budget.py --static           # fixtures + budget sanity (fast, no build)
  python3 scripts/test_budget.py --json RUN.json    # a nextest libtest-json-plus run
"""

import argparse
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
BUDGET = ROOT / "tests" / "budget.json"


def load_budget() -> dict:
    try:
        return json.loads(BUDGET.read_text())
    except FileNotFoundError:
        sys.exit(f"test_budget: missing {BUDGET}")
    except json.JSONDecodeError as e:
        sys.exit(f"test_budget: {BUDGET} is not valid JSON: {e}")


def human_mb(n: int) -> str:
    return f"{n / (1024 * 1024):.1f} MiB"


def check_static(budget: dict) -> list[str]:
    errors = []
    for rel, cap_mb in budget.get("fixtures_mb", {}).items():
        path = ROOT / rel
        if not path.exists():
            continue
        size = sum(p.stat().st_size for p in path.rglob("*") if p.is_file())
        if size > cap_mb * 1024 * 1024:
            errors.append(
                f"{rel} is {human_mb(size)}, over its {cap_mb} MiB budget. "
                "Drop or shrink the fixture, or raise fixtures_mb in tests/budget.json."
            )
    return errors


def group_of(test_name: str) -> str:
    # "donsetch::it$crawl::tests::x" -> "donsetch::it"; "donsetch::bin/donsetch$..." -> that.
    head = test_name.split("$", 1)[0]
    return head or "unknown"


def check_run(budget: dict, path: pathlib.Path) -> list[str]:
    errors = []
    counts: dict[str, int] = {}
    times: list[tuple[float, str]] = []
    for line in path.read_text(errors="replace").splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") != "test" or event.get("event") not in ("ok", "failed"):
            continue
        name = event.get("name", "")
        group = group_of(name)
        counts[group] = counts.get(group, 0) + 1
        if event.get("exec_time") is not None:
            times.append((float(event["exec_time"]), name))

    total = sum(counts.values())
    if total == 0:
        return ["the run JSON contains no completed tests — did the test step write the right file?"]

    max_total = budget["max_total"]
    if total > max_total:
        errors.append(
            f"{total} tests, over the {max_total} budget. Make the addition cheaper, "
            "fold it into an existing contract test, or raise max_total deliberately."
        )

    for group, cap in budget.get("suites", {}).items():
        if counts.get(group, 0) > cap:
            errors.append(
                f"{group}: {counts[group]} tests, over its {cap} budget "
                "(raise suites in tests/budget.json only if the growth is earned)."
            )

    max_single = budget["max_single_test_s"]
    allow = set(budget.get("allow_slow", []))
    for seconds, name in sorted(times, reverse=True):
        if seconds > max_single and name not in allow:
            errors.append(
                f"{seconds:.1f}s  {name} — over the {max_single}s single-test budget. "
                "Make it cheaper, move it to the heavy set (nextest default-filter), "
                "or allow_slow it with a reason."
            )

    total_sum = sum(t for t, _ in times)
    if total_sum > budget["max_sum_s"]:
        errors.append(
            f"test CPU total {total_sum:.0f}s, over the {budget['max_sum_s']}s budget. "
            "The suite is getting heavier somewhere; find the slowest entries above."
        )

    print(
        f"test budget: {total} tests, {total_sum:.0f}s CPU, "
        f"slowest {max((t for t, _ in times), default=0):.1f}s"
    )
    for group in sorted(counts):
        print(f"  {counts[group]:5d}  {group}")
    return errors


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--static", action="store_true", help="only fixtures and budget file")
    ap.add_argument("--json", type=pathlib.Path, help="nextest libtest-json-plus output")
    args = ap.parse_args()
    budget = load_budget()
    errors = check_static(budget)
    if args.json:
        errors += check_run(budget, args.json)
    elif not args.static:
        ap.error("pass --static or --json RUN.json")
    if errors:
        print("\ntest budget failed:", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
