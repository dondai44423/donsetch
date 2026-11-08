#!/bin/sh
# Storage guard: the build tree this repo owns stays under a hard ceiling,
# automatically.
#
# Cargo never garbage-collects target/. Measured here 2026-10-30, when the
# tree was 71G: target/fast/incremental held 42G across 499 variant
# directories — one variant per dep-graph hash change (a dep bump, a feature
# flip, a toolchain bump) and nothing ever removed the dead ones; 370 of them
# (>3 days) were 34G. That recency-blind pile, not the deps, is what walked
# this tree 20G -> 40G -> 80G -> 120G.
#
# A ceiling alone cannot hold that line, because this guard only ran on
# `just guard`, `just all` and pre-push: a session that builds and never
# pushes sails past it, and the old 80000M ceiling sat above where the pile
# actually reached, so the guard reported green while the tree ate the disk.
# Two parts now:
#
#   1. --prune-stale, called by scripts/budget.sh — the one place every
#      build, test and fuzz command in this repo passes through — deletes
#      incremental variants untouched for GUARD_KEEP_DAYS (default 3). The
#      variant the loop is using was written seconds ago, so the hot set can
#      never be the casualty; ~20ms; never fails a build. This is the part
#      that makes the tree stop growing.
#   2. ceiling mode (`just guard`, `just all`, pre-push): measure every
#      target dir this tree owns, reclaim by KIND, and refuse the push if
#      it is still over.
#
# Reclaim by kind, in escalation order, and only when over the ceiling:
#   - stale incremental variants (the age policy above), repo-wide,
#   - the retired profiles (debug / release / fuzz / windows-gnu),
#   - review worktrees under target/wt* that are CLEAN. Dirty ones are kept
#     and named; a clean checkout's only payload is ignored build/vendor
#     cache and its branch ref always lives in the main repo, so nothing
#     that exists nowhere else is ever dropped,
#   - still over: all incremental caches, hot set included.
# Never touched: fast/ci deps + build (the warm loop ladder), the sccache
# cache (budget.sh self-caps it at 10G), wave receipts.
#
# Ceiling 55000M. The floor under it is the warm artifact set measured after
# the 2026-11-08 profile split: fast ~22G (deps at opt-level 1), ci ~7G,
# core/target ~7.6G, sccache <=10G (its own cap), a 3-day incremental window
# ~8G. ~45G is what a healthy tree costs; 55G leaves room for one `dist`
# release-verification tree (~10G). A crossing means rot, not the ladder.
#
# Callers:
#   scripts/budget.sh (every build/test/fuzz)       --prune-stale
#   just guard / `just all` / .githooks/pre-push    ceiling mode
#   just clean-bloat                                --force, acts always
#   just space                                      --report, measures only
#   GUARD_CAP_MB=<n> / GUARD_KEEP_DAYS=<n> override the ceiling and the age
set -eu
cd "$(git rev-parse --show-toplevel)"

cap=${GUARD_CAP_MB:-55000}
keep_days=${GUARD_KEEP_DAYS:-3}
mode=ceiling
case "${1:-}" in
    --force) mode=force ;;
    --prune-stale) mode=prune ;;
    --report) mode=report ;;
    '') ;;
    *) echo "guard: unknown argument: $1" >&2 ; exit 2 ;;
esac

# Every target dir this tree owns. the core keeps a target of its own (its
# standalone builds and tests); it
# counts toward the ceiling so its cache cannot hide outside it. Review
# worktrees under target/wt* are already inside `target`.
target_dirs() {
    for t in target core/target fuzz/target; do
        if [ -d "$t" ]; then
            printf '%s\n' "$t"
        fi
    done
}

# The incremental caches, wherever they are. -prune stops the walk from
# descending into the very directories it is looking for.
incremental_dirs() {
    find $(target_dirs) -type d -name incremental -prune 2>/dev/null || true
}

size_mb() {
    du -sm $(target_dirs) 2>/dev/null | awk '{s+=$1} END {print s+0}'
}

# Age, not size: the variant in use was modified seconds ago and can never
# be a candidate, which is also what makes this safe to run next to a build.
prune_stale() {
    pruned=0
    for base in $(incremental_dirs); do
        before=$(find "$base" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)
        find "$base" -mindepth 1 -maxdepth 1 -type d -mtime +"$keep_days" -exec rm -rf {} + 2>/dev/null || true
        after=$(find "$base" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)
        pruned=$(( pruned + before - after ))
    done
    if [ "$pruned" -gt 0 ]; then
        echo "guard: pruned $pruned incremental variant(s) untouched for ${keep_days}d"
    fi
}

report() {
    printf 'guard: build tree %sM, ceiling %sM\n' "$(size_mb)" "$cap"
    for base in $(incremental_dirs); do
        n=$(find "$base" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)
        mb=$(du -sm "$base" 2>/dev/null | cut -f1)
        stale=$(find "$base" -mindepth 1 -maxdepth 1 -type d -mtime +"$keep_days" 2>/dev/null | wc -l)
        printf 'guard:   %s: %s variants, %sM, %s stale (>%sd)\n' "$base" "$n" "${mb:-0}" "$stale" "$keep_days"
    done
}

case "$mode" in
    prune)
        prune_stale
        exit 0
        ;;
    report)
        report
        exit 0
        ;;
esac

sm=$(size_mb)
if [ "$mode" = ceiling ] && [ "$sm" -le "$cap" ]; then
    exit 0
fi

echo "guard: build tree ${sm}M (ceiling ${cap}M): reclaiming"

prune_stale
rm -rf target/debug target/release target/dist fuzz/target target/x86_64-pc-windows-gnu

git worktree prune
for wt in $(git worktree list --porcelain | awk -v p="$PWD/target/wt" '/^worktree / && index($2, p) == 1 { print $2 }'); do
    [ -d "$wt" ] || continue
    st=$(git -C "$wt" status --porcelain 2>/dev/null) || {
        echo "guard: kept $wt (status failed)"
        continue
    }
    if [ -n "$st" ]; then
        echo "guard: kept DIRTY worktree $wt"
        continue
    fi
    if git worktree remove --force "$wt" 2>/dev/null; then
        echo "guard: removed clean worktree $wt"
    else
        echo "guard: could not remove $wt"
    fi
done

# Last resort, and the whole point of `just clean-bloat`: incremental caches
# regrow, everything else in the ladder has to be re-earned.
if [ "$mode" = force ] || [ "$(size_mb)" -gt "$cap" ]; then
    for base in $(incremental_dirs); do
        rm -rf "$base"
    done
fi

sm=$(size_mb)
if [ "$sm" -gt "$cap" ]; then
    echo "guard: build tree still ${sm}M, over the ${cap}M ceiling: inspect with: just space"
    exit 4
fi
echo "guard: build tree now ${sm}M"
