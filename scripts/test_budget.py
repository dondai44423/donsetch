#!/usr/bin/env python3
"""Test-suite budget guard.

The suite is a tool, not a landfill. This fails when it grows or slows without
someone deliberately raising the budget, and it names the two honest fixes:
make the test cheaper, or move it to the heavy lane.

Usage:
  python3 scripts/test_budget.py --static           # fixtures + budget sanity (fast, no build)
  python3 scripts/test_budget.py --json RUN.json    # a nextest libtest-json-plus run
  python3 scripts/test_budget.py --json RUN.json --ratchet
      # tighten tests/budget.json to what RUN.json measured (plus a little
      # headroom), after a green full run. Ceilings only ever move on purpose:
      # raising one is a reviewed edit to budget.json, lowering one is this.

Three things keep the suite from growing by accident:
  * ceilings (count, per-suite count, single-test time, total CPU, fixtures)
    that fail the run when crossed;
  * a slack warning when a ceiling sits far above what the suite really uses,
    because a loose ceiling is room for growth nobody decided on;
  * the slowest tests printed on every run, so creep is visible before it
    crosses anything.
"""

import argparse
import json
import math
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


# How far a ceiling may sit above the measured suite before the guard says so.
SLACK_TESTS = 60
# Headroom a ratchet leaves: new tests are allowed, a landfill is not.
RATCHET_TESTS = 25
RATCHET_SUITE = 15
# CI runners are slower than the desktop the numbers are usually taken on.
RATCHET_CPU_FACTOR = 2.5
LAST_RUN: dict = {}


def ratchet(budget: dict) -> None:
    """Rewrite budget.json ceilings to the last measured run plus headroom."""
    if not LAST_RUN:
        sys.exit("test_budget: --ratchet needs --json RUN.json")
    counts = LAST_RUN["counts"]
    new = dict(budget)
    new["max_total"] = LAST_RUN["total"] + RATCHET_TESTS
    new["max_sum_s"] = int(math.ceil(LAST_RUN["sum_s"] * RATCHET_CPU_FACTOR / 10.0) * 10)
    new["suites"] = {g: c + RATCHET_SUITE for g, c in sorted(counts.items())}
    BUDGET.write_text(json.dumps(new, indent=2) + "\n")
    print(
        f"ratcheted: max_total {new['max_total']}, max_sum_s {new['max_sum_s']}, "
        f"{len(new['suites'])} suite ceilings"
    )


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
    global LAST_RUN
    LAST_RUN = {"counts": dict(counts), "total": total, "sum_s": total_sum}
    if total_sum > budget["max_sum_s"]:
        errors.append(
            f"test CPU total {total_sum:.0f}s, over the {budget['max_sum_s']}s budget. "
            "The suite is getting heavier somewhere; find the slowest entries above."
        )

    print(
        f"test budget: {total} tests (ceiling {max_total}), {total_sum:.0f}s CPU "
        f"(ceiling {budget['max_sum_s']}s), slowest {max((t for t, _ in times), default=0):.1f}s"
    )
    for group in sorted(counts):
        print(f"  {counts[group]:5d}  {group}  (ceiling {budget.get('suites', {}).get(group, '-')})")
    print("slowest five:")
    for seconds, name in sorted(times, reverse=True)[:5]:
        print(f"  {seconds:6.1f}s  {name.split('$', 1)[-1]}")
    if max_total - total > SLACK_TESTS:
        print(
            f"note: max_total {max_total} is {max_total - total} above the {total} measured; "
            "a loose ceiling is room to grow unnoticed. Tighten it: "
            "python3 scripts/test_budget.py --json RUN.json --ratchet"
        )
    return errors


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--static", action="store_true", help="only fixtures and budget file")
    ap.add_argument("--json", type=pathlib.Path, help="nextest libtest-json-plus output")
    ap.add_argument("--ratchet", action="store_true", help="tighten budget.json to the measured run")
    args = ap.parse_args()
    budget = load_budget()
    errors = check_static(budget)
    if args.json:
        errors += check_run(budget, args.json)
    elif not args.static:
        ap.error("pass --static or --json RUN.json")
    if args.ratchet and not errors:
        ratchet(budget)
    if errors:
        print("\ntest budget failed:", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
