#!/usr/bin/env python3
"""Train the linear ranker on bench feature dumps and write rank_weights.rs.

Input: one or more JSON-lines dumps written by
    donsetch search-bench --set all --replay ARTIFACT... --features-out DUMP
one line per row and run, the candidate pool in the fusion's order:
    {"id","set","shape","query_shape","query","candidates":[{"url","tier","label","x":[22 floats]}]}
`shape` is the bench's label for the row; `query_shape` is the product's own
reading of the query (identifier, code, news, paper, howto, shopping, entity,
web), the one the ranker sees at run time.
Labels: 2 = a gold page, 1 = same host as a gold page, 0 = neither.

The split is the bench's own. Training reads only `set: gold` rows; the L2
strength is chosen by 5-fold cross-validation over them, split by row id so
every run of one query lands in one fold. The same out-of-fold predictions
decide which query shapes the model ranks: a shape where it does not beat the
fusion there keeps the fusion's order. One linear model serves every shape, and
a weight that helps `code` can cost `web`; the fusion is the measured floor.

The gate reads only `set: held` rows, which are never trained on: the weights
are written only when the shipped ranker's held-out MRR beats the fusion's on
the same candidates **and** no shape's held-out MRR falls (the bench's shapes
and the query shapes both). Otherwise nothing is written and the feature set is
what gets fixed, never the gate.

Ordering everywhere is the product's: tier first (a resolver's record leads),
then the score, ties kept in the fusion's order.

Method: pairwise logistic regression (RankNet) on standardised feature
differences, pairs only inside one row and one tier, each row weighted equally,
full-batch projected gradient descent with a fixed seed; the weights of the
evidence features are held non-negative (NONNEGATIVE). Needs numpy.
"""
import argparse
import collections
import hashlib
import json
import sys

import numpy as np

FEATURES = 22
NAMES = [
    "vote", "bm25_title", "bm25_snippet", "bm25_url", "bm25_host", "host_named",
    "families", "best_rank", "kind_record", "kind_publisher", "kind_community",
    "kind_aggregator", "fresh", "anchor", "ident", "rm3", "ngram", "snippet_q",
    "depth", "off_shape", "coverage", "vertical_only",
]
L2_GRID = [1e-3, 1e-2, 1e-1, 1.0]
# Evidence that a candidate answers the query can only help it. Several of these
# overlap (the vote already carries family agreement and rank), and an
# unconstrained fit pays for the overlap with a negative weight — agreement
# made a page *worse*, measured on `postgresql 18 released`. The kinds, the
# freshness, snippet quality, depth and off-shape carry no such prior and stay
# free.
NONNEGATIVE = {
    "vote", "bm25_title", "bm25_snippet", "bm25_url", "bm25_host", "host_named",
    "families", "best_rank", "anchor", "ident", "rm3", "ngram", "coverage",
}


def load(paths):
    rows = []
    for path in paths:
        with open(path, encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                row = json.loads(line)
                cands = row.get("candidates") or []
                if len(cands) < 2:
                    continue
                if any(len(c["x"]) != FEATURES for c in cands):
                    sys.exit(f"train: {path}: a vector is not {FEATURES} wide; re-dump with this build")
                row["X"] = np.array([c["x"] for c in cands], dtype=float)
                row["y"] = np.array([c["label"] for c in cands])
                row["tier"] = np.array([c.get("tier", 1) for c in cands])
                row["qshape"] = row.get("query_shape", "web")
                rows.append(row)
    return rows


def first_gold(row, scores):
    """1-based rank of the first label-2 candidate under (tier, -score), ties in
    dump order; 0 when the pool holds none."""
    order = sorted(range(len(scores)), key=lambda i: (row["tier"][i], -scores[i], i))
    for rank, i in enumerate(order, 1):
        if row["y"][i] == 2:
            return rank
    return 0


def metrics(rows, scorer):
    """MRR and hit@1 over rows whose pool holds a gold page."""
    rr, hit1, n = 0.0, 0, 0
    for row in rows:
        if not (row["y"] == 2).any():
            continue
        rank = first_gold(row, scorer(row))
        n += 1
        if rank:
            rr += 1.0 / rank
            hit1 += rank == 1
    return (rr / n if n else 0.0), (hit1 / n if n else 0.0), n


def fusion(row):
    # The dump is in the fusion's order: earlier is better.
    return -np.arange(len(row["y"]), dtype=float)


def pairs(rows, mean, std):
    """Standardised differences (better minus worse) and a weight per pair, so
    each row counts once however many candidates it had."""
    diffs, weights = [], []
    for row in rows:
        Z = (row["X"] - mean) / std
        y, tier = row["y"], row["tier"]
        idx = [(i, j) for i in range(len(y)) for j in range(len(y))
               if y[i] > y[j] and tier[i] == tier[j]]
        if not idx:
            continue
        for i, j in idx:
            diffs.append(Z[i] - Z[j])
            weights.append(1.0 / len(idx))
    if not diffs:
        return np.zeros((0, FEATURES)), np.zeros(0)
    return np.array(diffs), np.array(weights)


def fit(rows, mean, std, l2, steps=3000, lr=0.5):
    D, wt = pairs(rows, mean, std)
    w = np.zeros(FEATURES)
    if len(D) == 0:
        return w
    wt = wt / wt.sum()
    floor = np.array([0.0 if n in NONNEGATIVE else -np.inf for n in NAMES])
    for _ in range(steps):
        z = D @ w
        # d/dw of sum wt*log(1+exp(-z)) = -sum wt*sigmoid(-z)*D
        g = -(D * (wt * (1.0 / (1.0 + np.exp(z))))[:, None]).sum(axis=0) + 2 * l2 * w
        w = np.maximum(w - lr * g, floor)
    return w


def scorer_for(w, mean, std):
    return lambda row: ((row["X"] - mean) / std) @ w


def by_shape(rows, scorer, key="shape"):
    groups = collections.defaultdict(list)
    for row in rows:
        groups[row[key]].append(row)
    return {s: metrics(rs, scorer) for s, rs in sorted(groups.items())}


def gated(model, shapes):
    """The ranker as shipped: the model on the shapes it ranks, else the fusion."""
    return lambda row: model(row) if row["qshape"] in shapes else fusion(row)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--features", required=True, nargs="+")
    ap.add_argument("--out", default="src/search/rank_weights.rs")
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()
    np.random.seed(args.seed)

    rows = load(args.features)
    gold = [r for r in rows if r["set"] == "gold"]
    held = [r for r in rows if r["set"] == "held"]
    if len(gold) < 20 or len(held) < 20:
        print(f"train: gold {len(gold)} / held {len(held)} rows is not a corpus (20 each)", file=sys.stderr)
        return 2
    digest = hashlib.sha256()
    for path in sorted(args.features):
        digest.update(open(path, "rb").read())
    digest = digest.hexdigest()[:16]

    allX = np.vstack([r["X"] for r in gold])
    mean, std = allX.mean(axis=0), allX.std(axis=0)
    std[std < 1e-9] = 1.0

    # Model selection on gold rows only, folds by row id.
    ids = sorted({r["id"] for r in gold})
    rng = np.random.RandomState(args.seed)
    rng.shuffle(ids)
    fold_of = {rid: k % 5 for k, rid in enumerate(ids)}
    best = None
    for l2 in L2_GRID:
        oof = {}
        for k in range(5):
            train = [r for r in gold if fold_of[r["id"]] != k]
            w = fit(train, mean, std, l2)
            for i, r in enumerate(gold):
                if fold_of[r["id"]] == k:
                    oof[i] = scorer_for(w, mean, std)(r)
        for i, r in enumerate(gold):
            r["oof"] = oof[i]
        held_out = lambda row: row["oof"]
        cv_model = by_shape(gold, held_out, "qshape")
        cv_fusion = by_shape(gold, fusion, "qshape")
        shapes = sorted(s for s in cv_model if cv_model[s][0] > cv_fusion[s][0] + 1e-9)
        cv = metrics(gold, gated(held_out, shapes))[0]
        print(f"  l2={l2:<6g} gold 5-fold MRR {cv:.4f}  ranks {', '.join(shapes) or 'nothing'}")
        if best is None or cv > best[1]:
            best = (l2, cv, shapes, cv_model, cv_fusion)
    l2, _, shapes, cv_model, cv_fusion = best
    print("  gold by query shape  fusion MRR   model MRR (5-fold)   rows")
    for s, (fm, _, c) in cv_fusion.items():
        print(f"    {s:<12} {fm:>14.4f} {cv_model[s][0]:>11.4f} {c:>13}" + ("  ranks" if s in shapes else ""))
    if not shapes:
        print("train: the model beats the fusion on no query shape: nothing written")
        return 1
    w = fit(gold, mean, std, l2)
    model = gated(scorer_for(w, mean, std), shapes)

    base_mrr, base_h1, n = metrics(held, fusion)
    model_mrr, model_h1, _ = metrics(held, model)
    gold_base, _, _ = metrics(gold, fusion)
    print(f"train: gold {len(gold)} rows, held {len(held)} rows ({n} with a gold page in the pool), dumps {digest}, l2 {l2:g}")
    print(f"  gold   (fit, 5-fold) MRR {best[1]:.4f}   fusion {gold_base:.4f}")
    print(f"  held   fusion  MRR {base_mrr:.4f}  hit@1 {base_h1:.4f}")
    print(f"  held   model   MRR {model_mrr:.4f}  hit@1 {model_h1:.4f}")
    fell = []
    for key, title in (("shape", "held by bench shape "), ("qshape", "held by query shape ")):
        shapes_base = by_shape(held, fusion, key)
        shapes_model = by_shape(held, model, key)
        print(f"  {title} fusion MRR   model MRR   rows")
        for shape, (bm, _, c) in shapes_base.items():
            mm = shapes_model[shape][0]
            mark = "  FELL" if mm < bm - 1e-9 else ""
            if mark:
                fell.append(shape)
            print(f"    {shape:<12} {bm:>14.4f} {mm:>11.4f} {c:>6}{mark}")
    print("  weights:", ", ".join(f"{NAMES[j]}={w[j]:+.3f}" for j in range(FEATURES)))
    if model_mrr <= base_mrr:
        print("train: the model does not beat the fusion on held-out rows: nothing written")
        return 1
    if fell:
        print(f"train: held-out MRR fell on {', '.join(fell)}: nothing written")
        return 1
    if args.dry_run:
        return 0
    with open(args.out, encoding="utf-8") as fh:
        text = fh.read()
    begin = text.index("// ── BEGIN GENERATED ──")
    end = text.index("// ── END GENERATED ──")
    fmt = lambda v: ", ".join(f"{x:.6f}" for x in v)
    block = (
        "// ── BEGIN GENERATED ──\n"
        f"/// Trained on bench feature dumps `{digest}` (seed {args.seed}, l2 {l2:g}, {len(gold)} gold rows).\n"
        f"/// Held-out MRR {model_mrr:.4f} against the fusion's {base_mrr:.4f} over {n} rows; no shape fell.\n"
        "pub const WEIGHTS: Weights = Weights {\n"
        f"    mean: [{fmt(mean)}],\n"
        f"    std: [{fmt(std)}],\n"
        f"    w: [{fmt(w)}],\n"
        f"    shapes: &[{', '.join(chr(34) + s + chr(34) for s in shapes)}],\n"
        "};\n"
        "/// The dumps the weights were trained on.\n"
        f'pub const TRAINED_ON: &str = "{digest}";\n'
    )
    with open(args.out, "w", encoding="utf-8") as fh:
        fh.write(text[:begin] + block + text[end:])
    print(f"train: wrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
