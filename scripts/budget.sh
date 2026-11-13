#!/bin/sh
# The resource budget wrapper. Every build/test/fuzz command in this repo
# goes through it (Justfile recipes, .githooks/pre-push, scripts/gates.sh).
#
# status.md mechanic 5: this box is Dondai's live desktop (8 threads /
# 31 GB), not a build farm. The ceiling is all the cores AND a bounded
# share of RAM, because a `-j` flag bounds neither:
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
# spawn, they share that set), niced and ionice-idle so the live session
# wins every contention, and the job-count variables are exported so nested
# build systems stay in the same budget.
#
# MEMORY is the half that actually lags a desktop. Measured 2026-09-19:
# three concurrent rustc held ~2.4 GB and pushed the box to 3.6 GB of
# swap with the browser running at 10 fps. A memory-shaped box does not
# stall, it swaps. So the job count is also bounded by what RAM is
# available RIGHT NOW: one job per ~700 MB, after reserving 1 GB for
# the session and the OS.
#
#   BG_JOBS=<n>    job count for cargo/cmake/make (default: the min of
#                  all cores and the RAM budget)
#   BG_CORES=<set> cores to pin to, taskset syntax (default: every core)
#                  (give it 0-5 when you want the desktop kept quiet)
#
# Both are escapes for a known-idle box, not the default.
set -eu

# DISK citizenship, same choke point as the CPU cap below: every build, test
# and fuzz command in this repo comes through this script, so this is where
# the tree's disk growth is bounded. Cargo never garbage-collects target/:
# incremental variants accrete one per dep-graph hash change and the dead
# ones stay forever (measured 2026-10-30: 499 variants, 42G, 370 of them >3
# days old). guard.sh owns the policy and is age-gated, so it can never
# delete the variant a build is using right now; ~20ms, and a failure here
# never fails the build.
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
if [ -f "$root/scripts/guard.sh" ]; then
    sh "$root/scripts/guard.sh" --prune-stale || true
fi

nproc_now=$(nproc 2>/dev/null || echo 4)
# Every thread: 32 GB no longer needs the 7.6 GB-era half-core cap, and
# profile.codegen-units still caps each rustc's own LLVM fan-out. `nice` and
# `ionice` put the live session first, so an interactive task preempts the
# build instead of waiting behind it.
core_jobs=$nproc_now
[ "$core_jobs" -lt 1 ] && core_jobs=1

# MemAvailable is the kernel's own estimate of what we can take without
# pushing the session into swap; read it rather than trusting total RAM.
avail_mb=$(awk '/^MemAvailable:/{printf "%d", $2/1024}' /proc/meminfo 2>/dev/null || echo 0)
case "$avail_mb" in
    ''|*[!0-9]*) avail_mb=0 ;;
esac
mem_jobs=0
# ~700 MB per rustc with codegen-units=16 and opt-level 1 deps (measured on
# this tree). The old 1.2 GB estimate was sized for 256-unit codegen, where
# one crate could fan out into every core and several GB; that shape is gone.
if [ "$avail_mb" -gt 1000 ]; then
    mem_jobs=$(( (avail_mb - 1000) / 700 ))
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

# sccache, when installed: the same source compiled for another profile,
# feature set or branch becomes a cache hit instead of work. rustc goes
# through RUSTC_WRAPPER; the C/C++ compilers go through CC/CXX, which is
# where the real money is (BoringSSL's C++ build is the single most
# expensive thing in this graph, and every profile flip used to re-pay
# it). The cache lives under target/ so it travels with the artifacts.
# Incremental units are not cached (by design) : this buys the deps.
# GitHub outage escape hatch: when releases/download 504s, build.sh's pinned
# prebuilts fall back to the API asset URL on their own, but the v8 crate has no
# fallback of its own. RELEASE_MIRROR=http://127.0.0.1:18777/v8 (from
# scripts/release-mirror.py) points every v8 build at a local mirror of the same
# pinned archive. Unset in normal use.
if [ -n "${RELEASE_MIRROR:-}" ]; then
    export RUSTY_V8_MIRROR="$RELEASE_MIRROR"
fi
if command -v sccache >/dev/null 2>&1; then
    export RUSTC_WRAPPER="${RUSTC_WRAPPER:-sccache}"
    export CC="${CC:-sccache cc}"
    export CXX="${CXX:-sccache c++}"
    export SCCACHE_DIR="${SCCACHE_DIR:-$PWD/target/sccache}"
    export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-10G}"
    # Cross targets keep their own C compiler: the generic CC above is a
    # host compiler, and a host toolchain compiles the target's generated
    # config macros (e.g. oniguruma's `gid_t int` from a mingw-flavored
    # config.h) against host headers, which stops compiling the moment
    # the sccache hits that used to mask it go cold. The value must be a
    # plain compiler name: boringssl's cmake build hands it straight to
    # CMAKE_C_COMPILER and cannot take a wrapper, so the cross toolchain
    # itself is not sccached here.
    if command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
        export CC_x86_64_pc_windows_gnu="${CC_x86_64_pc_windows_gnu:-x86_64-w64-mingw32-gcc}"
    fi
    if command -v x86_64-w64-mingw32-g++ >/dev/null 2>&1; then
        export CXX_x86_64_pc_windows_gnu="${CXX_x86_64_pc_windows_gnu:-x86_64-w64-mingw32-g++}"
    fi
fi

exec nice -n 19 ionice -c3 taskset -c "$cores" "$@"
