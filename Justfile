# DonSeTch dev loop.
#
# THE LADDER (each rung is a different question, and they cost
# different amounts; climb only as far as the question needs):
#
#   1. just check      types + cfg on the full feature set   (~seconds, warm)
#   2. just t <expr>   the touched scope, fast profile        (~seconds-min)
#   3. CI              the full matrix, the only full gate    (async)
#
# `just all` is the pre-push net: fmt + check + lockgate + lint. It does
# NOT run the suite: CI already does, in parallel, on 5 platforms, and
# paying for a local full run just serializes what is already running.
#
# LOCAL vs CI PROFILE (the reason the loop is fast): every local recipe
# runs the `fast` profile (debug codegen for this crate, deps at
# opt-level 1, no debuginfo). A source edit then rebuilds in seconds
# instead of the 8-12 minutes an opt-level-3 whole-crate recompile cost
# under the RAM-bounded job cap. The `ci` profile (release opts,
# panic=abort, the shape the binary ships) stays for CI, for
# `just tci`/`just bin-ci` when release-profile behavior is the
# question, and for the heavy suite.
#
#   just check        compile-check, full feature set, fast profile
#   just t <expr>     scoped tests, fast profile (`just t crawl::frontier`)
#   just test         the whole suite, fast profile, minus the heavy set
#   just heavy        the heavy set (soak / corpus / landmarks) on ci
#   just tci <expr>   scoped tests on the ci profile (release parity)
#   just lint         clippy -Dwarnings, full feature set
#   just all          pre-push net: guard + fmt-check + check + lockgate + lint
#   just bin          target/fast/donsetch (fast live smokes)
#   just bin-ci       target/ci/donsetch  (release parity + the gates)
#   just smoke        bin + doctor + fetch + search
#   just loop-report  measure this ladder on this box, right now
#   just fuzz extract 30s fuzz burst on one target
#   just clean-bloat  reclaim all that regrows (incremental, worktrees)
#
# A hung test is caught by nextest's own per-test slow-timeout; a long
# COLD build is caught by the CI step timeout, which has to clear the
# slowest cold build on the slowest runner (see .github/workflows/ci.yml).
#
# CPU + MEMORY citizenship: this box is the operator's desktop (16
# threads, 7.6 GB RAM, a live session next to every compile). Every
# cargo/build/fuzz command below runs through scripts/budget.sh, which
# pins the whole process TREE to a quarter of the cores with taskset,
# exports the job caps into nested build systems (cmake, make, nasm), and
# bounds the job count by what RAM is available right now. A `-j` flag
# caps neither: rustc spawns its own codegen threads, boring-sys spawns
# its own make -j$(nproc), and a memory-shaped box swaps instead of
# stalling.
#   BG_JOBS=<n>     job count (default: min of the core quarter and the
#                   RAM budget, one job per ~1.2 GB after reserving 1.5 GB)
#   BG_CORES=<set>  taskset core set (default: the first quarter)
# With sccache installed (rust-sccache), budget.sh wires RUSTC_WRAPPER
# and the C/C++ compilers through it, so recompiles across profiles and
# branches are cache hits instead of work.
# Absolute, so a recipe that cd's first (fuzz) can still use it.
budget := "sh '" + justfile_directory() / "scripts/budget.sh" + "'"
feat := "--features ocr,rerank,http"

# Storage guard: one implementation, three entry points (scripts/guard.sh).
# Cargo never GCs stale artifacts; how the bloat actually regrew, and why
# the previous fixed-profile-list guard went green while doing nothing, is
# in the script header. Ceiling 80G; the floor under it is legitimate warm
# artifacts (fast+ci deps, sccache's own 10G cap, receipts).
guard:
	@sh scripts/guard.sh

# Unconditional reclaim: whatever the guard would drop, now. Use when
# space is the question rather than the loop.
clean-bloat:
	@sh scripts/guard.sh --force

# Instant size report: what each shell of target/ costs.
space:
	@du -shx target 2>/dev/null
	@du -shx target/*/ 2>/dev/null | sort -rh | head -8

# Pre-push net. Deliberately NOT the suite: CI is the full gate and runs
# it on 5 platforms in parallel; a local full run only serializes it.
all: guard fmt-check check lockgate lint

# Pre-tag gate: `all` + the payload gates mirrored against the ci-profile
# binary. Fat LTO is not built locally: the release workflow's own gates
# are the authoritative payload check.
preflight: all ci-gates

# Full fat-LTO + gates, for when the release workflow itself changed and
# the payload gates must be proven locally first.
preflight-full: all gates

# Manifest/lock coherence plus the ci-profile (release-shaped,
# panic=abort) compile: the second half of the structural gate, and the
# one that catches anything gated on the release profile.
lockgate:
    {{budget}} cargo check --locked --profile ci --all-targets {{feat}}

# The tag-time gates (linux-x64 mirror of release.yml), against the
# ci-profile binary: sizes/version/dylib presence/ONNX probe/QEMU all
# hold there too, so the slow fat-LTO pass is CI's job.
ci-gates: bin-ci
    @sh scripts/gates.sh linux-x64 target/ci

# The tag-time gates (linux-x64 mirror of release.yml).
gates:
    {{budget}} cargo build --release {{feat}}
    @sh scripts/gates.sh linux-x64 target/release

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

# Clippy on the full feature set, fast profile: lints do not depend on
# the profile, and this reuses the loop's artifact graph instead of
# compiling a second one.
lint:
    {{budget}} cargo clippy --profile fast --all-targets {{feat}} -- -Dwarnings

# Windows cross-check from Linux: type-checks every cfg(windows) path
# with the full feature set, the exact breakage a Linux-only change
# ships to Windows CI. No linkage, so no MSVC/mingw runtime is
# exercised; three env crumbs make the deps graph cross-buildable:
#   ASM_NASM        BoringSSL links crypto against Threads::Threads;
#                   CMake 3.28 leaks -pthread into the NASM command
#                   line, and nasm reads it as -p thread (pre-include
#                   file "thread"). The wrapper strips the flag.
#   BINDGEN_…       libclang parsing the mingw headers needs GCC's
#                   private include dir (mm_malloc.h lives there).
#   ORT_SKIP_DOWNLOAD  pyke ships no ONNX prebuilts for windows-gnu;
#                   ort-sys then defers its error to link time, which
#                   a check never reaches.
# Prereqs (Debian/Ubuntu): mingw-w64 nasm cmake libclang-dev
# pkg-config, plus `rustup target add x86_64-pc-windows-gnu`.
win-check: win-check-core win-check-full

_win-check-prereqs:
    @command -v x86_64-w64-mingw32-gcc >/dev/null || { echo "win-check: missing cross toolchain — sudo apt install mingw-w64 nasm cmake libclang-dev pkg-config && rustup target add x86_64-pc-windows-gnu"; exit 1; }
    @rustup target list --installed --toolchain "$(rustup show active-toolchain | head -1 | cut -d ' ' -f1)" | grep -q '^x86_64-pc-windows-gnu$' || { echo "win-check: missing rustup target — rustup target add x86_64-pc-windows-gnu"; exit 1; }

win-check-full: _win-check-prereqs
    ASM_NASM="{{justfile_directory()}}/scripts/nasm-no-pthread.sh" \
    BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu="-I$(x86_64-w64-mingw32-gcc -print-file-name=include) -D__CLANG_MAX_ALIGN_T_DEFINED" \
    ORT_SKIP_DOWNLOAD=1 \
    {{budget}} cargo clippy --target x86_64-pc-windows-gnu --all-targets {{feat}} -- -Dwarnings

# The no-features half of the matrix: a feature-gated `use` can satisfy a
# cfg(windows) path that the core build then lacks, so full-feature green
# does not imply core green.
win-check-core: _win-check-prereqs
    ASM_NASM="{{justfile_directory()}}/scripts/nasm-no-pthread.sh" \
    BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu="-I$(x86_64-w64-mingw32-gcc -print-file-name=include) -D__CLANG_MAX_ALIGN_T_DEFINED" \
    {{budget}} cargo clippy --target x86_64-pc-windows-gnu --no-default-features --all-targets -- -Dwarnings

# Heavy by nature: soak holds the process for minutes, corpus and
# landmark runs parse whole fixture trees, the live ones need the
# network. They are worth running, they are just not worth running on
# every edit; CI runs them in the heavy lane.
heavy_expr := "test(soak) | test(corpus) | test(landmark)"

# The whole suite minus the heavy set, fast profile. CI runs the rest of
# the matrix; this is for the times a local full run is genuinely wanted.
test:
    {{budget}} cargo nextest run --cargo-profile fast {{feat}} -E 'not ({{heavy_expr}})' --workspace

# Scoped run: `just t crawl::frontier` runs only what matches.
t expression:
    {{budget}} cargo nextest run --cargo-profile fast {{feat}} -E 'test({{expression}})'

# Explicit native qualification: only the requested ignored test scope, using
# fresh owned browser profiles. Never part of an ordinary local test run.
t-native expression:
    {{budget}} cargo nextest run --cargo-profile fast {{feat}} --run-ignored only -E 'test({{expression}})'

# Scoped run on the ci profile (release opts, panic=abort): use when the
# question is release-profile behavior, not "does it pass".
tci expression:
    {{budget}} cargo nextest run --cargo-profile ci {{feat}} -E 'test({{expression}})'

# The heavy set on the ci profile: soak, corpus, landmarks, live probes.
heavy:
    {{budget}} cargo nextest run --cargo-profile ci {{feat}} -E '{{heavy_expr}}' --no-fail-fast

# The binary for live smokes (fast profile: seconds, real behavior).
bin:
    {{budget}} cargo build --profile fast {{feat}}

# The release-shaped binary for parity smokes and the payload gates.
bin-ci:
    {{budget}} cargo build --profile ci {{feat}}

# Compile-only full-feature check: the whole-crate structural signal.
# Fast when the check graph is warm; ~2.5 min when only the test graph is
# (check and test build different artifacts for the deps).
check:
    {{budget}} cargo check --profile fast --all-targets {{feat}}

# What the ladder actually costs on THIS box, right now: warm check,
# warm scoped test, and the artifact shells. Run it after changing
# anything in this file.
loop-report:
    #!/usr/bin/env bash
    set -u
    echo "== artifacts"
    du -shx target/*/ 2>/dev/null | sort -rh | head -5
    echo "== just check (warm)"
    s=$(date +%s); just check >/dev/null 2>&1; echo "   $(( $(date +%s) - s ))s"
    echo "== just t mcp::supervisor (warm)"
    s=$(date +%s); just t mcp::supervisor >/dev/null 2>&1; echo "   $(( $(date +%s) - s ))s"
    echo "== sccache"
    sccache --show-stats 2>/dev/null | grep -E 'cache hits|compile requests|Cache size' || echo "   (sccache not installed: rust-sccache in the Void repos)"

# Live smoke: payload, normal site, walled site, search.
smoke: bin
    target/fast/donsetch doctor 2>&1 | rg -i 'ONNX|Status' | head -4
    target/fast/donsetch fetch https://en.wikipedia.org/wiki/Markdown --json 2>/dev/null | head -c 120
    echo
    target/fast/donsetch search "linux kernel" --json 2>/dev/null | head -c 120
    echo

# 30-second fuzz burst on one target: just fuzz extract
# Fuzzing is the one thing that can escape a job cap entirely (the
# libFuzzer process plus a second cargo target graph), so it rides the
# same taskset-bounded budget wrapper as everything else.
fuzz target:
    cd fuzz && {{budget}} cargo fuzz run {{target}} -s none -- -max_total_time=30

# Release everything in one command. The CHANGELOG [version] section
# must already exist; the recipe bumps both manifests + the lock,
# validates, commits, pushes, and tags. The tag triggers CI + the
# Release build in parallel; publish the draft when the watcher
# reports green (gh run watch does it; nothing waits serially).
ship version:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "$(git rev-parse --show-toplevel)"
    if [ -n "$(git status --porcelain)" ]; then
        echo "ship: dirty tree, commit or stash first"; exit 1
    fi
    grep -q "^## \[{{version}}\] - " CHANGELOG.md || {
        echo "ship: CHANGELOG.md has no [{{version}}] section yet"; exit 1
    }
    sed -i 's/^version = ".*"/version = "{{version}}"/' Cargo.toml
    sed -i 's/^  "version": ".*",/  "version": "{{version}}",/' npm/package.json
    cargo update -p donsetch --precise {{version}} >/dev/null
    grep -q 'version = "{{version}}"' Cargo.lock || {
        echo "ship: Cargo.lock did not take the bump"; exit 1
    }
    git add Cargo.toml Cargo.lock npm/package.json CHANGELOG.md
    git commit -q -m "chore(release): {{version}}"
    git push -q origin master
    git tag "v{{version}}"
    git push -q origin "v{{version}}"
    # Match the run by its branch (the tag), not by "newest": the run for
    # this tag may not exist yet at this instant, and `.[0]` then answered
    # with the PREVIOUS release's run id (v4.2.1 printed v4.2.0's).
    run=""
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        run=$(gh run list --workflow=Release --limit 10 --json databaseId,headBranch \
            --jq "[.[]|select(.headBranch==\"v{{version}}\")][0].databaseId // empty")
        [ -n "$run" ] && break
        sleep 3
    done
    echo "ship: v{{version}} pushed. Release build run ${run:-unknown} is underway."
    echo "      watcher: just watch ${run:-<run-id>}"
    echo "      publish when green: gh release edit v{{version}} --draft=false"

# Follow one GitHub Actions run to completion in the foreground.
# This replaces every sleep loop and polling loop: one command, it
# exits with the run's verdict.
watch run-id:
    gh run watch {{run-id}} --exit-status --interval 30
