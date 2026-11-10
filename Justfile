# DonSeTch dev loop.
#
# THE LADDER (each rung is a different question; climb only as far as the
# question needs):
#
#   1. just check       types + cfg, full feature set        ~1 s warm
#   2. just t <expr>    the touched scope                    ~6 s warm
#   3. just test        the whole default suite              ~35 s warm
#   4. CI               Linux gate on every push, ~5 min; Windows and macOS on
#                       platform-touching pushes, nightly and every release
#
# `just all` is the pre-push net: guard + fmt + check + lockgate + lint + the
# static test budget. It does NOT run the suite: CI already does.
#
# TWO BUILD SHAPES. `fast` is the edit loop AND every CI test lane: this crate
# at opt-level 0, deps at opt-level 1, no debuginfo, codegen-units 16.
# `dist-thin` is the SHIPPED shape (opt-level 3, no LTO) and what the nightly
# heavy suite runs on, because those tests measure time and memory. Nothing
# else: the old `ci` and `dist` profiles are gone (see Cargo.toml).
#
# SHIPPING IS ONE COMMAND. `just ship <version>` bumps the manifests and pushes
# the release commit; the CI pipeline then tests it on five platforms, builds
# and gates the shipped binaries, tags, publishes the GitHub release and npm,
# and stops on the first red. Nothing runs on this machine, nothing waits here.
#
#   just check        compile-check, full feature set, fast profile
#   just t <expr>     scoped tests, fast profile (`just t crawl::frontier`)
#   just th <expr>    scoped heavy test, default-filter lifted
#   just test         the default suite, fast profile
#   just heavy        the heavy set (soak / corpus / PDF corpus / timing)
#   just budget       the suite as the guard sees it, then the ceilings
#   just ratchet      tighten the ceilings to what the suite really uses
#   just tdist <expr> scoped tests on the shipped profile (release opts)
#   just lint         clippy -Dwarnings, full feature set
#   just all          pre-push net: guard + fmt + check + lockgate + lint + budget-static
#   just bin          target/fast/donsetch (fast live smokes)
#   just bin-dist     target/dist-thin/donsetch (the shipped shape)
#   just gates        bin-dist + the payload gates the release pipeline runs
#   just smoke        bin + doctor + fetch + search
#   just bench        the live search bench, each shape cold (bench-smoke: 3 min)
#   just rank-train   retrain the search ranker from saved bench artifacts
#   just ship <ver>   bump + push the release commit; CI does the rest
#   just runs         what CI, the release and npm are doing right now
#   just loop-report  measure this ladder on this box, right now
#   just fuzz extract 30s fuzz burst on one target
#   just clean-bloat  reclaim all that regrows (incremental, worktrees)
#
# A hung test is caught by nextest's own per-test slow-timeout; a long
# COLD build is caught by the CI step timeout.
#
# CPU + MEMORY citizenship: this box is the operator's desktop (16 threads,
# 7.6 GB RAM, a live session next to every compile). Every cargo/build/fuzz
# command below runs through scripts/budget.sh, which pins the whole process
# TREE to half the cores with taskset, exports the job caps into nested build
# systems (cmake, make, nasm), and bounds the job count by MemAvailable right
# now: ~700 MB per rustc at codegen-units 16, after reserving 1 GB.
#   BG_JOBS=<n>     job count (default: min of half the cores and the RAM
#                   budget; 3-4 with a browser open, 8 on an idle box)
#   BG_CORES=<set>  taskset core set (default: the first half)
# With sccache installed (rust-sccache), budget.sh wires RUSTC_WRAPPER and the
# C/C++ compilers through it, so recompiles across profiles and branches are
# cache hits instead of work.
# GitHub release downloads 504 now and then. build.rs already retries the PDFium
# and ONNX prebuilts through the API asset URL; for the v8 archive, start a local
# mirror and export RELEASE_MIRROR (budget.sh maps it to RUSTY_V8_MIRROR):
#   python3 scripts/release-mirror.py --port 18777 --v8 150.4.0 &
#   RELEASE_MIRROR=http://127.0.0.1:18777/v8 just bin
# Absolute, so a recipe that cd's first (fuzz) can still use it.
budget := "sh '" + justfile_directory() / "scripts/budget.sh" + "'"
feat := "--features ocr,http"

# Storage guard: one implementation, four entry points (scripts/guard.sh):
# `just guard`/`just all`/pre-push measure and reclaim at the ceiling,
# `just clean-bloat` forces it, `just space` only measures, and every build
# command in the repo prunes stale incremental variants on the way through
# scripts/budget.sh — that last one is what stops the growth, because a
# session that builds and never pushes never reaches the other three.
# Ceiling 55G; the floor under it is the warm artifact set (fast ~22G,
# ci ~7G, core ~7.6G, sccache <=10G, a 3-day incremental window ~8G).
# How the bloat regrew, and why the previous fixed-profile-list guard went
# green while doing nothing, is in the script header.
guard:
	@sh scripts/guard.sh

# Unconditional reclaim: whatever the guard would drop, now. Use when
# space is the question rather than the loop.
clean-bloat:
	@sh scripts/guard.sh --force

# Instant size report: what each shell of the build tree costs, and how
# much of the incremental caches is already reclaimable.
space:
	@du -shx target fuzz/target 2>/dev/null || true
	@du -shx target/*/ 2>/dev/null | sort -rh | head -8
	@sh scripts/guard.sh --report

# Pre-push net. Deliberately NOT the suite: CI is the full gate and runs
# it on 5 platforms in parallel; a local full run only serializes it.
all: guard fmt-check check lockgate lint budget-static

# Manifest/lock coherence plus the loop-profile compile: one graph, shared with
# `just check`, so this costs seconds when the loop is warm.
lockgate:
    {{budget}} cargo check --locked --profile fast --workspace --all-targets {{feat}}

# The release pipeline's payload gates (size, ONNX payload, version, non-AVX
# CPU) against a locally built shipped-shape binary. Only needed when
# scripts/gates.sh or the release job itself changed: the pipeline runs the
# same script on all five platforms.
gates: bin-dist
    @sh scripts/gates.sh linux-x64 target/dist-thin

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

# Clippy on the full feature set, fast profile: lints do not depend on
# the profile, and this reuses the loop's artifact graph instead of
# compiling a second one.
lint:
    {{budget}} cargo clippy --profile fast --workspace --all-targets {{feat}} -- -Dwarnings

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
    CMAKE_TOOLCHAIN_FILE="{{justfile_directory()}}/scripts/mingw-cross-toolchain.cmake" \
    ORT_SKIP_DOWNLOAD=1 \
    {{budget}} cargo clippy --target x86_64-pc-windows-gnu --all-targets {{feat}} -- -Dwarnings

# The no-features half of the matrix: a feature-gated `use` can satisfy a
# cfg(windows) path that the core build then lacks, so full-feature green
# does not imply core green.
win-check-core: _win-check-prereqs
    ASM_NASM="{{justfile_directory()}}/scripts/nasm-no-pthread.sh" \
    BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu="-I$(x86_64-w64-mingw32-gcc -print-file-name=include) -D__CLANG_MAX_ALIGN_T_DEFINED" \
    CMAKE_TOOLCHAIN_FILE="{{justfile_directory()}}/scripts/mingw-cross-toolchain.cmake" \
    {{budget}} cargo clippy --target x86_64-pc-windows-gnu --no-default-features --all-targets -- -Dwarnings

# Heavy by nature: soak holds the process for minutes, corpus and
# landmark runs parse whole fixture trees, the live ones need the
# network. They are worth running, they are just not worth running on
# every edit; CI runs them in the heavy lane.
# The heavy set: real but expensive, so it never runs per edit. The PDF
# corpus tests and the timing-ceiling test are excluded by nextest's
# default-filter too; this expression is the explicit heavy lane.
heavy_expr := "test(soak) | test(corpus) | test(landmark) | test(pdf::tests::progit_book) | test(pdf::tests::smoke_scanned_detection) | test(pdf::tests::pdf_spec_loads) | test(search::tests::stealth_v3_preflight_batch_obeys_a_global_ceiling)"

# The default suite, fast profile. The heavy set is excluded by nextest's
# default-filter; `just heavy` is the explicit lane for it.
test:
    {{budget}} cargo nextest run --cargo-profile fast {{feat}} --workspace

# Scoped run: `just t crawl::frontier` runs only what matches. The heavy
# default-filter still applies; `just th` is the escape for a heavy test.
t expression:
    {{budget}} cargo nextest run --cargo-profile fast {{feat}} -E 'test({{expression}})'

# Scoped heavy run: `just th pdf::tests::progit_book` (one test, on purpose).
th expression:
    {{budget}} cargo nextest run --cargo-profile fast {{feat}} --ignore-default-filter -E 'test({{expression}})'

# The suite as the budget guard sees it: machine-readable run, then the
# ceilings in tests/budget.json. CI runs the same check on its full lane.
budget:
    #!/usr/bin/env bash
    set -euo pipefail
    NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1 {{budget}} cargo nextest run --cargo-profile fast {{feat}} --workspace --message-format libtest-json-plus > /var/tmp/donsetch-nextest.json
    python3 scripts/test_budget.py --json /var/tmp/donsetch-nextest.json

# After a clean full run, tighten tests/budget.json to what the suite really
# uses. Raising a ceiling is a reviewed edit; lowering one is this.
ratchet: budget
    python3 scripts/test_budget.py --json /var/tmp/donsetch-nextest.json --ratchet

# Fixtures and the budget file only: seconds, no build. Runs in `just all`.
budget-static:
    python3 scripts/test_budget.py --static

# Explicit native qualification: only the requested ignored test scope, using
# fresh owned browser profiles. Never part of an ordinary local test run.
t-native expression:
    {{budget}} cargo nextest run --cargo-profile fast {{feat}} --run-ignored only -E 'test({{expression}})'

# Scoped run on the shipped profile (release opts, panic=abort): use when the
# question is release-profile behavior, not "does it pass".
tdist expression:
    {{budget}} cargo nextest run --cargo-profile dist-thin {{feat}} -E 'test({{expression}})'

# npm wrapper + installer suite (node --test): resolution order, the
# unwritable-target contract (#361), atomic landing, wrapper
# passthrough. Runs in CI as the npm-installer job.
npm-test:
    node --test --test-timeout=90000 npm/tests/*.test.js

# The heavy set on the shipped profile: soak, corpus, landmarks, live probes.
heavy:
    {{budget}} cargo nextest run --cargo-profile dist-thin {{feat}} --ignore-default-filter -E '{{heavy_expr}}' --no-fail-fast

# The binary for live smokes (fast profile: seconds, real behavior).
bin:
    {{budget}} cargo build --profile fast {{feat}}

# The shipped-shape binary for parity smokes and the payload gates.
bin-dist:
    {{budget}} cargo build --profile dist-thin {{feat}}

# Compile-only full-feature check: the whole-crate structural signal.
# Fast when the check graph is warm; ~2.5 min when only the test graph is
# (check and test build different artifacts for the deps).
check:
    {{budget}} cargo check --profile fast --workspace --all-targets {{feat}}

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

# The search bench on the shipped shape: live, cold, every shape from its own
# fresh state (the fair measurement; `--one-ledger` measures the roster's rest
# instead). Pass any `search-bench` flag: `just bench --set held --runs 3
# --gap-min 60 --out bench/results/x.json`. `bench-smoke` is one row per shape
# in ~3 minutes, for iteration; a release never quotes it.
bench *args: bin-dist
    target/dist-thin/donsetch search-bench --set all {{args}}

bench-smoke *args: bin-dist
    target/dist-thin/donsetch search-bench --set all --smoke {{args}}

# Retrain the ranker from saved bench artifacts: their candidate pools are
# replayed through this build, dumped with their features, and trained on the
# gold rows; scripts/train_rank.py writes src/search/rank_weights.rs only when
# held-out MRR beats the fusion and no shape falls.
#   just rank-train bench/results/5.3.0-*.json
rank-train +artifacts: bin
    #!/usr/bin/env bash
    set -euo pipefail
    dump=target/rank-features.jsonl
    rm -f "$dump"
    for a in {{artifacts}}; do
        target/fast/donsetch search-bench --set all --replay "$a" --features-out "$dump" >/dev/null
    done
    python3 scripts/train_rank.py --features "$dump"

# 30-second fuzz burst on one target: just fuzz extract
# Fuzzing is the one thing that can escape a job cap entirely (the
# libFuzzer process plus a second cargo target graph), so it rides the
# same taskset-bounded budget wrapper as everything else.
fuzz target:
    cd fuzz && {{budget}} cargo fuzz run {{target}} -s none -- -max_total_time=30

# Ship a version: bump, commit, push. That is all that runs here.
#
# The pushed commit is what the CI pipeline recognises as a release (its
# Cargo.toml version has no tag yet). The pipeline then runs the suite on
# Linux, Windows and macOS, builds and gates the five shipped binaries in
# parallel with it, and only when every one is green creates the tag and the
# GitHub release and publishes npm. A red run publishes nothing: fix forward
# and push again, the same version is simply tried again. Nothing to wait on
# here and no state to roll back; `just runs` shows how far it got, and a
# single failed job is re-run with `gh run rerun <id> --failed`.
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
    if git ls-remote --exit-code --tags origin "refs/tags/v{{version}}" >/dev/null 2>&1; then
        echo "ship: v{{version}} is already tagged and released"; exit 1
    fi
    sed -i 's/^version = ".*"/version = "{{version}}"/' Cargo.toml
    sed -i 's/^  "version": ".*",/  "version": "{{version}}",/' npm/package.json
    cargo update -p donsetch --precise {{version}} >/dev/null
    grep -q 'version = "{{version}}"' Cargo.lock || {
        echo "ship: Cargo.lock did not take the bump"; exit 1
    }
    git add Cargo.toml Cargo.lock npm/package.json CHANGELOG.md
    # Idempotent: the manifests may already carry this version (a release
    # prepared ahead of its push); an empty commit would abort the ship.
    if ! git diff --cached --quiet; then
        git commit -q -m "chore(release): {{version}}"
    fi
    git push -q origin HEAD
    echo "ship: v{{version}} pushed. CI tests, builds, tags, publishes and runs npm."
    echo "      progress: just runs      watch live: gh run watch"

# One status call: the pipeline's latest runs, the newest release and the npm
# version. No watching, no polling loops.
runs:
    #!/usr/bin/env bash
    set -euo pipefail
    gh run list --limit 6 --json workflowName,status,conclusion,headBranch,createdAt \
      --jq '.[] | [.workflowName, .status, (.conclusion // ""), .headBranch, .createdAt] | @tsv' \
      | awk -F'\t' '{printf "%-14s %-12s %-10s %-12s %s\n", $1, $2, $3, $4, $5}'
    echo
    echo "release: $(gh release view --json tagName,isDraft --jq '"\(.tagName) draft=\(.isDraft)"' 2>/dev/null || echo none)"
    echo "npm:     $(curl -sS https://registry.npmjs.org/donsetch | python3 -c 'import json,sys;print(json.load(sys.stdin)["dist-tags"]["latest"])' 2>/dev/null || echo unknown)"
