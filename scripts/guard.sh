#!/bin/sh
# Storage guard: target/ stays under a hard ceiling, automatically.
#
# Bloat changes shape; a guard written as a fixed list of paths rots.
# The 2026-09-08 guard deleted only the retired debug/release profiles,
# both of which then vanished from the tree, so it went green on every
# push while fast/incremental regrew to 48G and 15 forgotten review
# worktrees reached 42G: target/ hit 133G with the "guard" silently
# deleting nothing.
#
# This reclaims by KIND, every time the ceiling is crossed:
#   - incremental caches under any profile (regrow freely; cost only
#     time on the next build),
#   - the retired profiles (debug / release / fuzz / windows-gnu),
#   - review worktrees under target/wt* that are CLEAN. Dirty ones are
#     kept and named; a clean checkout's only payload is ignored
#     build/vendor cache, and its branch ref always lives in the main
#     repo, so nothing that exists nowhere else is ever dropped.
# Never touched: fast/ci deps + build (the warm loop ladder), the
# sccache cache (budget.sh self-caps it at 10G), wave receipts.
#
# Callers:
#   just guard / `just all` / .githooks/pre-push    ceiling mode
#   just clean-bloat                                --force, acts always
#   GUARD_CAP_MB=<n> overrides the ceiling (tests)
set -eu
cd "$(git rev-parse --show-toplevel)"
[ -d target ] || exit 0

cap=${GUARD_CAP_MB:-80000}
force=0
if [ "${1:-}" = "--force" ]; then
    force=1
fi

sm=$(du -sm target | cut -f1)
if [ "$force" -eq 0 ] && [ "$sm" -le "$cap" ]; then
    exit 0
fi

echo "guard: target ${sm}M (ceiling ${cap}M): reclaiming"

rm -rf target/debug target/release fuzz/target target/x86_64-pc-windows-gnu
rm -rf target/fast/incremental target/ci/incremental

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

sm=$(du -sm target | cut -f1)
if [ "$sm" -gt "$cap" ]; then
    echo "guard: target still ${sm}M, over the ${cap}M ceiling: inspect with: just space"
    exit 4
fi
echo "guard: target now ${sm}M"
