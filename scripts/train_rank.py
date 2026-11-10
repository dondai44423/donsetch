#!/usr/bin/env python3
"""Train the linear ranker on a bench feature dump and write rank_weights.rs.

The gate is this script's own: the model is scored on rows it never trained on,
against the order the fusion actually produced (the dump order). A model that
does not beat that order on held-out rows writes nothing, and the ranker stays on
the fusion.

Input: one JSON object per row (`search-bench --features-out`):
    {"id","set","shape","query","candidates":[{"url","label","x":[18 floats]}]}
Labels: 2 = the row's gold page, 1 = same host as a gold page, 0 = neither.

Method: pairwise logistic regression (RankNet) on the difference of standardised
feature vectors, L2 = 1e-3, fixed seed, 5-fold split by row — the held-out rows
are never trained on.
"""
import argparse
import hashlib
import json
import random
import sys

FEATURES = 18
NAMES = [
    "bm25_title", "bm25_snippet", "bm25_url", "families", "best_rank", "shape_trust",
    "kind_record", "kind_publisher", "kind_aggregator", "fresh", "anchor", "ident",
    "rm3", "ngram", "snippet_q", "depth", "shape_match", "bias",
]


def load(path):
    rows = []
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            row = json.loads(line)
            if len(row.get("candidates") or []) >= 2:
                rows.append(row)
    return rows


def rank_of_first_relevant(order_labels):
    """1-based rank of the first label-2 candidate; 0 when the row has none."""
    for i, label in enumerate(order_labels):
        if label == 2:
            return i + 1
    return 0


def mrr(rows, scorer):
    total, counted = 0.0, 0
    for row in rows:
        cands = row["candidates"]
        if not any(c["label"] == 2 for c in cands):
            continue
        order = sorted(cands, key=scorer, reverse=True)
        rank = rank_of_first_relevant([c["label"] for c in order])
        total += 1.0 / rank if rank else 0.0
        counted += 1
    return total / counted if counted else 0.0


def baseline_mrr(rows):
    """The fusion's own order: the dump is written in the order it produced."""
    total, counted = 0.0, 0
    for row in rows:
        labels = [c["label"] for c in row["candidates"]]
        if 2 not in labels:
            continue
        rank = rank_of_first_relevant(labels)
        total += 1.0 / rank if rank else 0.0
        counted += 1
    return total / counted if counted else 0.0


def standardise(rows):
    mean = [0.0] * FEATURES
    count = 0
    for row in rows:
        for cand in row["candidates"]:
            for j, v in enumerate(cand["x"]):
                mean[j] += v
            count += 1
    if count:
        mean = [m / count for m in mean]
    var = [0.0] * FEATURES
    for row in rows:
        for cand in row["candidates"]:
            for j, v in enumerate(cand["x"]):
                var[j] += (v - mean[j]) ** 2
    std = [(v / count) ** 0.5 if count else 1.0 for v in var]
    return mean, [s if s > 1e-9 else 1.0 for s in std]


def pairs(row, mean, std):
    """Every (better, worse) pair inside one row, scores standardised."""
    out = []
    cands = row["candidates"]
    for i in range(len(cands)):
        for j in range(len(cands)):
            if cands[i]["label"] <= cands[j]["label"]:
                continue
            a = [(cands[i]["x"][k] - mean[k]) / std[k] for k in range(FEATURES)]
            b = [(cands[j]["x"][k] - mean[k]) / std[k] for k in range(FEATURES)]
            out.append((a, b))
    return out


def train_pairwise(training_rows, mean, std, epochs=500, lr=0.05, l2=1e-3, seed=7):
    rng = random.Random(seed)
    data = []
    for row in training_rows:
        data.extend(pairs(row, mean, std))
    if not data:
        return [0.0] * FEATURES
    w = [0.0] * FEATURES
    for _ in range(epochs):
        rng.shuffle(data)
        for a, b in data:
            diff = [a[k] - b[k] for k in range(FEATURES)]
            z = sum(w[k] * diff[k] for k in range(FEATURES))
            # logistic loss on the pair: target 1, so p = sigmoid(z)
            p = 1.0 / (1.0 + pow(2.718281828459045, -z))
            g = (1.0 - p)
            for k in range(FEATURES):
                w[k] += lr * (g * diff[k] - l2 * w[k])
    return w


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--features", required=True)
    ap.add_argument("--out", default="src/search/rank_weights.rs")
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    rows = load(args.features)
    if len(rows) < 20:
        print(f"train: {len(rows)} rows is not a corpus (need at least 20)", file=sys.stderr)
        return 2
    digest = hashlib.sha256(open(args.features, "rb").read()).hexdigest()[:16]
    mean, std = standardise(rows)
    base = baseline_mrr(rows)
    folds = 5
    rng = random.Random(args.seed)
    shuffled = rows[:]
    rng.shuffle(shuffled)
    held_out_pred, held_out_rows = 0.0, 0
    for k in range(folds):
        test = shuffled[k::folds]
        train = [r for r in shuffled if r not in test]
        w = train_pairwise(train, mean, std, seed=args.seed)
        scorer = lambda c, w=w: sum(
            w[j] * ((c["x"][j] - mean[j]) / std[j]) for j in range(FEATURES)
        )
        held_out_pred += mrr(test, scorer) * len(test)
        held_out_rows += len(test)
    model_mrr = held_out_pred / held_out_rows if held_out_rows else 0.0
    print(f"train: rows={len(rows)} corpus={digest}")
    print(f"  baseline (fusion order) held-out MRR {base:.4f}")
    print(f"  model    (linear ranker) held-out MRR {model_mrr:.4f}")
    if model_mrr <= base:
        print("train: the model does not beat the fusion on held-out rows — nothing written")
        return 1
    w = train_pairwise(rows, mean, std, seed=args.seed)
    weight_line = ", ".join(f"{v:.6f}" for v in w)
    print("  weights:", ", ".join(f"{NAMES[j]}={w[j]:+.3f}" for j in range(FEATURES)))
    if args.dry_run:
        return 0
    with open(args.out, encoding="utf-8") as fh:
        text = fh.read()
    begin = text.index("// ── BEGIN GENERATED ──")
    end = text.index("// ── END GENERATED ──")
    block = (
        "// ── BEGIN GENERATED ──\n"
        f"/// Trained on bench feature dump `{digest}` (seed {args.seed}, {len(rows)} rows).\n"
        f"/// Held-out MRR {model_mrr:.4f} against the fusion's {base:.4f}.\n"
        "pub const WEIGHTS: Weights = Weights {\n"
        f"    mean: [{', '.join(f'{v:.6f}' for v in mean)}],\n"
        f"    std: [{', '.join(f'{v:.6f}' for v in std)}],\n"
        f"    w: [{weight_line}],\n"
        "};\n"
        f'pub const TRAINED_ON: &str = "{digest}";\n'
    )
    with open(args.out, "w", encoding="utf-8") as fh:
        fh.write(text[:begin] + block + text[end:])
    print(f"train: wrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
