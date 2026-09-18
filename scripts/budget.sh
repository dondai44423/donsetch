#!/bin/sh
# The resource budget wrapper. Every build/test/fuzz command in this repo
# goes through it (Justfile recipes, .githooks/pre-push, scripts/gates.sh).
#
# status.md mechanic 5: this box is Dondai's live desktop (16 threads /
# 7.6 GB), not a build farm. The ceiling is a quarter of the cores AND a
# bounded share of RAM, because a `-j` flag bounds neither:
#
#   * rustc runs its own codegen threads (the ci profile asks for 256
#     codegen units), so one rustc process can use every core;
#   * build scripts spawn their own trees: boring-sys drives cmake ->
#     make -j$(nproc) -> 16 cc1plus at once, cargo-fuzz adds another whole
#     target graph, onnxruntime does the same;
#   * nextest runs N test processes, each of which can fork more.
#
# So the cap is MECHANICAL, not aspirational: the whole command tree is
# pinned to a core subset with taskset (however many threads the tools
# spawn, they share a quarter of the machine), niced and ionice-idle so
# the live session wins every contention, and the job-count variables are
# exported so nested build systems stay in the same budget.
#
# MEMORY is the half that actually lags a desktop. Measured 2026-09-19:
# three concurrent rustc held ~2.4 GB and pushed the box to 3.6 GB of
# swap with the browser running at 10 fps. A memory-shaped box does not
# stall, it swaps. So the job count is also bounded by what RAM is
# available RIGHT NOW: one job per ~1.2 GB, after reserving 1.5 GB for
# the session and the OS.
#
#   BG_JOBS=<n>    job count for cargo/cmake/make (default: the min of
#                  the core quarter and the RAM budget)
#   BG_CORES=<set> cores to pin to, taskset syntax (default: first quarter)
#
# Both are escapes for a known-idle box, not the default.
set -eu

nproc_now=$(nproc 2>/dev/null || echo 4)
core_jobs=$(( (nproc_now + 3) / 4 ))

# MemAvailable is the kernel's own estimate of what we can take without
# pushing the session into swap; read it rather than trusting total RAM.
avail_mb=$(awk '/^MemAvailable:/{printf "%d", $2/1024}' /proc/meminfo 2>/dev/null || echo 0)
case "$avail_mb" in
    ''|*[!0-9]*) avail_mb=0 ;;
esac
mem_jobs=0
if [ "$avail_mb" -gt 1500 ]; then
    mem_jobs=$(( (avail_mb - 1500) / 1200 ))
fi
[ "$mem_jobs" -lt 1 ] && mem_jobs=1

default_jobs=$core_jobs
[ "$mem_jobs" -lt "$core_jobs" ] && default_jobs=$mem_jobs

cores="${BG_CORES:-0-$(( core_jobs - 1 ))}"
jobs="${BG_JOBS:-$default_jobs}"

export CARGO_BUILD_JOBS="$jobs"
export CMAKE_BUILD_PARALLEL_LEVEL="$jobs"
export MAKEFLAGS="-j$jobs"
export NUM_JOBS="$jobs"
# nasm/cmake read this one too when a build script hardcodes its own -j.
export CARGO_MAKEFLAGS="-j$jobs"

exec nice -n 19 ionice -c3 taskset -c "$cores" "$@"
