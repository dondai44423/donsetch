# DonSeTch dev loop — one profile, everything fast.
#
# Everything below runs on the `ci` cargo profile (release opts,
# no fat LTO): test links take seconds, the binary behaves like
# release (`panic = "abort"` inherited), and all artifacts share
# one graph in target/ci. Fat LTO runs once, at ship time, via a
# plain `cargo build --release` (see status.md Workflow section).
#
#   just check    compile-check the full feature set (~10s warm)
#   just test     full test suite, full features, fail-fast
#   just lint     clippy -Dwarnings on the full feature set
#   just all      the pre-push gate: fmt + lint + test
#   just bin      build target/ci/donsetch (for live smokes)
#   just smoke    bin + doctor + fetch/search/bypass smoke
#   just fuzz extract    30s fuzz burst on one target
#   just clean-bloat     drop profiles the loop never uses + fuzz cache

# Cargo never GCs stale artifacts: debug/release/fuzz caches grow
# without bound across dep bumps (110G caught; ~99G was bloat). This
# clears everything except the warm `ci` loop profile.
clean-bloat:
	rm -rf target/debug target/release fuzz/target

# Hard storage guard: the target dir never gets to blow past 25G.
# One du + compare (~2s), prune-through only when bloat exists. The
# second GIB recheck uses 30G so the gate pays no extra rebuild.
guard:
	@if [ -d target ] && [ "$$(du -sm target | cut -f1)" -gt 25000 ]; then \
		echo "guard: pruning bloat profiles (target > 25G)"; \
		rm -rf target/debug target/release fuzz/target; \
	fi

# Instant size report: what each shell of target/ costs.
space:
	@du -shx target 2>/dev/null
	@du -shx target/*/ 2>/dev/null | sort -rh | head -8

# Pre-push gate: everything CI will flag.
all: guard fmt-check lint test

# Pre-tag gate: `all` + the Cargo.lock gate (catches a version bump
# with a stale lock in seconds, the failure that used to cost a full
# release round-trip) + the tag-time payload gates mirrored against
# the ci-profile binary. Fat LTO is NOT built locally: the release
# workflow's own gates are the authoritative payload check, paying
# for the fat-LTO build twice bought nothing.
preflight: all lockgate ci-gates

# Full fat-LTO + gates, for when the release workflow itself changed
# and the payload gates must be proven locally first.
preflight-full: all gates

# Manifest/lock coherence: the bump-invalidates-lock failure must die
# here in seconds, never in CI.
lockgate:
    cargo check --locked --profile ci --all-targets --features ocr,rerank,http

# The tag-time gates (linux-x64 mirror of release.yml), against the
# fast binary: sizes/version/dylib presence/ONNX probe/QEMU all hold
# on the ci profile as well, so the slow fat-LTO pass is CI's job.
ci-gates: bin
    @sh scripts/gates.sh linux-x64 target/ci

# The tag-time gates (linux-x64 mirror of release.yml).
gates:
    cargo build --release --features ocr,rerank,http
    @sh scripts/gates.sh linux-x64 target/release

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

# Clippy on the full feature set; --profile ci reuses the test
# artifact graph instead of compiling a dev one.
lint:
    cargo clippy --profile ci --all-targets --features ocr,rerank,http -- -Dwarnings

# Windows cross-check from Linux: type-checks every cfg(windows)
# path with the full feature set — the exact breakage a Linux-only
# change ships to Windows CI. No linkage, so no MSVC/mingw runtime
# is exercised; three env crumbs make the deps graph cross-buildable:
#   ASM_NASM        BoringSSL links crypto against Threads::Threads;
#                   CMake 3.28 leaks -pthread into the NASM command
#                   line, and nasm reads it as -p thread (pre-include
#                   file "thread"). The wrapper strips the flag.
#   BINDGEN_…       libclang parsing the mingw headers needs GCC's
#                   private include dir (mm_malloc.h lives there).
#   ORT_SKIP_DOWNLOAD  pyke ships no ONNX prebuilts for windows-gnu;
#                   ort-sys then defers its error to link time, which
#                   a check never reaches. Windows CI proper uses the
#                   msvc prebuilts, so this gap is cross-check-only.
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
    cargo clippy --target x86_64-pc-windows-gnu --all-targets --features ocr,rerank,http -- -Dwarnings

# The no-features half of the matrix: a feature-gated `use` can
# satisfy a cfg(windows) path that the core build then lacks, so
# full-feature green does not imply core green. Cheaper than the
# full half (no ort download step), so it is the one to run first.
win-check-core: _win-check-prereqs
    ASM_NASM="{{justfile_directory()}}/scripts/nasm-no-pthread.sh" \
    BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu="-I$(x86_64-w64-mingw32-gcc -print-file-name=include) -D__CLANG_MAX_ALIGN_T_DEFINED" \
    cargo clippy --target x86_64-pc-windows-gnu --no-default-features --all-targets -- -Dwarnings

# Full suite, full feature set, fail-fast. The cargo profile is
# pinned via CLI: nextest 0.9.x ignores the config-level key, and an
# unpinned run compiles the debug graph (the 110G/21G recidivism).
test:
    cargo nextest run --cargo-profile ci --features ocr,rerank,http

# Scoped test run with the SAME pin: `just t crawl::frontier`
# is the only local way to run a subset without growing debug.
t expression:
    cargo nextest run --cargo-profile ci --features ocr,rerank,http -E 'test({{expression}})' 

# The binary for live smoke runs (fast profile, real behavior).
bin:
    cargo build --profile ci --features ocr,rerank,http

# Compile-only full-feature check, fastest structural signal.
check:
    cargo check --profile ci --all-targets --features ocr,rerank,http

# Live smoke: payload, normal site, walled site, search.
smoke: bin
    target/ci/donsetch doctor 2>&1 | rg -i 'ONNX|Status' | head -4
    target/ci/donsetch fetch https://en.wikipedia.org/wiki/Markdown --json 2>/dev/null | head -c 120
    echo
    target/ci/donsetch search "linux kernel" --json 2>/dev/null | head -c 120
    echo

# 30-second fuzz burst on one target: just fuzz extract
fuzz target:
    cd fuzz && cargo fuzz run {{target}} -s none -- -max_total_time=30