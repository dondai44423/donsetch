#!/usr/bin/env python3
"""Compare two search-bench artifacts row by row and shape by shape.

This is the release gate's arithmetic: "no shape regresses" has to be computed
from two runs of the same corpus, not remembered. Both artifacts come from
`donsetch search-bench --out`, which records every row's hit, answer-key rank,
junk count and latency.

    python3 bench/compare.py before.json after.json [--json out.json]

Exit code 0 when no shape regresses and nothing violated a must_not; 1 when a
shape regresses on hit@3; 2 when the artifacts cannot be compared.
A regression is reported, never silently tolerated: the point of a "before" file
is to lose against.
"""
import json
import sys
from collections import defaultdict, Counter

# The gate's own clause: a shape may not lose more than this on hit@3.
REGRESSION = 0.02


def load(path):
    with open(path) as f:
        doc = json.load(f)
    rows = doc.get("queries") or []
    if not rows:
        raise SystemExit(f"{path}: no rows (not a search-bench artifact?)")
    meta = {
        "version": doc.get("version"),
        "mode": doc.get("mode"),
        "runs": doc.get("runs"),
        "started": doc.get("run_started_unix") or [],
        "rows": len(rows),
        "hash": doc.get("queries_sha256"),
    }
    occurrences = Counter()
    indexed = {}
    for row in rows:
        occurrences[row["id"]] += 1
        indexed[f"{row['id']}#{occurrences[row['id']]}"] = row
    return meta, indexed


def rate(rows, pred, of=None):
    n = len(rows) if of is None else of
    return (sum(1 for r in rows if pred(r)) / n) if n else 0.0


def mrr(rows):
    vals = [1.0 / r["hit"] for r in rows if r.get("hit")]
    return sum(vals) / len(rows) if rows else 0.0


def answer_rows(rows):
    return [r for r in rows if r.get("expect", "answer") == "answer"]


def shape_stats(rows):
    ans = answer_rows(rows)
    return {
        "n": len(ans),
        "hit1": rate(ans, lambda r: r.get("hit") and r["hit"] <= 1),
        "hit3": rate(ans, lambda r: r.get("hit") and r["hit"] <= 3),
        "hit5": rate(ans, lambda r: r.get("hit") and r["hit"] <= 5),
        "mrr": mrr(ans),
        "key_coverage": rate(ans, lambda r: r.get("key_at") and r["key_at"] <= 5),
        "zero": rate(ans, lambda r: not r.get("urls")),
        "junk5": rate(rows, lambda r: r.get("junk5", 0) > 0),
        "p50": percentile([r.get("ms", 0) for r in rows], 0.5),
        "p95": percentile([r.get("ms", 0) for r in rows], 0.95),
    }


def percentile(values, p):
    if not values:
        return 0
    v = sorted(values)
    return v[min(int(len(v) * p), len(v) - 1)]


def main(argv):
    if len(argv) < 3:
        raise SystemExit(__doc__)
    before_meta, before = load(argv[1])
    after_meta, after = load(argv[2])
    if before_meta['hash'] != after_meta['hash'] or not before_meta['hash']:
        raise SystemExit("artifacts have different or absent corpus hashes")
    out_path = None
    if "--json" in argv:
        out_path = argv[argv.index("--json") + 1]

    common = sorted(set(before) & set(after))
    only_before = sorted(set(before) - set(after))
    only_after = sorted(set(after) - set(before))

    by_shape_before, by_shape_after = defaultdict(list), defaultdict(list)
    for i in common:
        by_shape_before[before[i]["shape"]].append(before[i])
        by_shape_after[after[i]["shape"]].append(after[i])

    print(f"before: {argv[1]}  v{before_meta['version']} {before_meta['mode']} "
          f"runs={before_meta['runs']} rows={before_meta['rows']}")
    print(f"after:  {argv[2]}  v{after_meta['version']} {after_meta['mode']} "
          f"runs={after_meta['runs']} rows={after_meta['rows']}")
    if only_before or only_after:
        print(f"!! rows only in before: {only_before}")
        print(f"!! rows only in after: {only_after}")
        return 2

    head = (f"{'shape':<12}{'n':>4}  {'hit@1':>13} {'hit@3':>13} "
            f"{'MRR':>13} {'key coverage':>13} {'zero':>13} {'junk5':>11}")
    print()
    print(head)
    print(f"{'':<12}{'':>4}  {'before→after':>13} {'before→after':>13} "
          f"{'':>13} {'':>13} {'':>13} {'':>11}")

    regressed, violations = [], []
    summary = {}
    for shape in sorted(set(by_shape_before) | set(by_shape_after)):
        b = shape_stats(by_shape_before.get(shape, []))
        a = shape_stats(by_shape_after.get(shape, []))
        summary[shape] = {"before": b, "after": a}
        delta3 = a["hit3"] - b["hit3"]
        flag = ""
        if b["n"] and delta3 < -REGRESSION:
            flag = "  !! REGRESSED"
            regressed.append((shape, b["hit3"], a["hit3"]))
        print(f"{shape:<12}{a['n']:>4}  "
              f"{b['hit1']:>5.2f}→{a['hit1']:<5.2f} "
              f"{b['hit3']:>5.2f}→{a['hit3']:<5.2f} "
              f"{b['mrr']:>5.3f}→{a['mrr']:<5.3f} "
              f"{b['key_coverage']:>5.2f}→{a['key_coverage']:<5.2f} "
              f"{b['zero']:>5.2f}→{a['zero']:<5.2f} "
              f"{b['junk5']:>4.2f}→{a['junk5']:<4.2f}{flag}")

    for i in common:
        for r in (before[i], after[i]):
            violations.extend(r.get("violations") or [])
    gained = [i for i in common if not before[i].get("hit") and after[i].get("hit")]
    lost = [i for i in common if before[i].get("hit") and not after[i].get("hit")]
    filled = [i for i in common if before[i].get("urls") is not None
              and not before[i]["urls"] and after[i].get("urls")]
    emptied = [i for i in common if before[i].get("urls") and not after[i].get("urls")]
    print()
    print(f"rows compared {len(common)}  gained a gold hit {len(gained)}  "
          f"lost one {len(lost)}  zero→filled {len(filled)}  filled→zero {len(emptied)}")
    if gained:
        print(f"  gained: {gained}")
    if lost:
        print(f"  lost:   {lost}")
    if filled:
        print(f"  zero→filled: {filled}")
    if emptied:
        print(f"  filled→zero: {emptied}")
    if violations:
        print(f"!! must_not violations: {sorted(set(violations))}")

    if out_path:
        doc = {"before": argv[1], "after": argv[2], "shapes": summary,
               "gained": gained, "lost": lost, "filled": filled, "emptied": emptied,
               "violations": sorted(set(violations)),
               "regressed": [{"shape": s, "before": b, "after": a} for s, b, a in regressed]}
        with open(out_path, "w") as f:
            json.dump(doc, f, indent=2)
        print(f"wrote {out_path}")

    if regressed or violations:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
