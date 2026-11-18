# Contributing to DonSeTch

DonSeTch is licensed under AGPL-3.0-only; all contributions are accepted under that license. Coding agents and their operators should also read [AGENTS.md](AGENTS.md).

## Prerequisites & build shapes

Requires **Rust 1.98+** (pinned in `rust-toolchain.toml`), **Go 1.22+**, **NASM**, **LLVM/Clang (`libclang`)**, and **CMake** — see [setup.md](docs/setup.md#build-from-source) for per-OS install commands.

```bash
just bin              # edit-loop binary: target/fast/donsetch (seconds warm)
just bin-dist         # shipped binary shape: target/dist-thin/donsetch (opt-level 3, panic=abort)
cargo build --release # local thin-LTO release build
```

Two Cargo profiles power the workflow: `fast` (workspace opt-level 0, deps opt-level 1, no debuginfo, 16 codegen units — shared by local loops and CI test lanes) and `dist-thin` (what CI ships and nightly heavy tests run against). Avoid bare `cargo test` or repeated `cargo build --release` during iteration: they compile separate target graphs and break process-per-test isolation.

## Development workflow

Climb only as far up the ladder as your change needs:

```bash
just check              # type-check workspace, no optional features (~1 s warm)
just t crawl::frontier  # scoped nextest run, no optional features (~2 s warm)
just check-all          # type-check all targets with ocr,http
just tf crawl::frontier # scoped nextest run with ocr,http
just test               # full default suite with ocr,http (~35 s warm)
just all                # pre-push gate: guard + fmt-check + check + lockgate + lint + budget-static
```

- **Feature split:** `just check` and `just t` omit `ocr,http` (386 vs 572 crates) for fast fetch/search/crawl iteration; `just all` compiles and lints both feature sets before push.
- **Scoped fetch-crate tests:** `sh scripts/budget.sh cargo nextest run --cargo-profile fast --locked -p donsetch-fetch`.
- **Resource wrapper & caching:** `Justfile` routes builds through `scripts/budget.sh` (sizing parallel jobs to `MemAvailable`, forwarding job caps to CMake/Make/NASM, and auto-enabling `sccache` / `rust-sccache` when installed) and `scripts/guard.sh` (keeping `target/` under 55 GB). Override with `BG_JOBS=<n>` or `BG_CORES=<set>`.
- **Test budgets (`tests/budget.json`):** `.config/nextest.toml` excludes heavy PDF, soak, and clock tests from `just test` (run them via `just th <name>` or `just heavy`; test release-opt behavior via `just tdist <expr>`). `tests/budget.json` caps single-test duration, total CPU, and fixture size — keep new tests specific and under ~2 s, or place corpus/clock tests in the heavy set. Inspect with `just budget`; tighten ceilings with `just ratchet`.

### CI & releases (`.github/workflows/ci.yml`)

Branch protection requires one check: **`gate`**.

| Trigger | Pipeline work |
|---|---|
| Docs-only push or PR | Skips compile/test; `gate` passes immediately |
| Code push or PR | `lint` (fmt, cargo-deny, static budget, clippy with and without `ocr,http`) + Linux x86_64 test suite (~5 min warm) |
| Platform-touching push or PR (`mcp`, `profile`, `pdf`, `browser`, `cli`, `paths`, `onnx`, `crates/fetch/`, `build.rs`, `Cargo.*`, `npm/`, `.github/`) | Above + macOS arm64 and Windows x86_64 platform suites |
| Release commit or nightly | All three OS suites + all five release binaries built and checked by `scripts/gates.sh` |

Newer `master` pushes cancel unfinished non-release runs. Trigger platform/binary checks manually on a branch with `gh workflow run ci.yml --ref <branch> -f platforms=true -f build=true`. Keep GitHub Actions cache usage within the 10 GB pool (`gh cache list`).

- **Windows & cross-checking:** native Windows CI (`msvc`) is authoritative because embedded V8 prevents a full Linux→Windows cross-check; `Justfile` retains `just win-check-core` / `just win-check` (`mingw-w64`) for partial `#[cfg(windows)]` checks. On Windows, run `just` inside Git Bash or wrap `just` in PowerShell `$PROFILE` (`function just { just.exe --shell "C:\Program Files\Git\bin\sh.exe" @args }`) — never add Git's `usr\bin` to global `PATH`.
- **Shipping (maintainer):** `just ship <version>` verifies `CHANGELOG.md`, bumps `Cargo.toml`, `Cargo.lock`, and `npm/package.json`, and pushes `chore(release): <version>`. CI gates all platforms and binaries before tagging, publishing the GitHub Release, and dispatching `npm-publish.yml` (`just runs` checks status; `gh run rerun <id> --failed` or `gh workflow run npm-publish.yml -f tag=v<version>` retries).

## What belongs in DonSeTch

**Welcome:**
- Reproducible bug fixes paired with a regression test.
- Keyless search sources and fixes to existing optional BYOK providers (without weakening keyless search).
- Measured fetch improvements inside **identity, state, request contract, or egress** backed by a live witness ([AGENTS.md](AGENTS.md#the-fetch-path-has-a-shape)).
- PDF edge cases (corrupt encodings, complex tables, forms) and measured performance wins.

**Out of scope:**
- New built-in BYOK search providers (use the [executable search plugin interface](docs/tools.md#search-plugins) instead).
- Features requiring API keys/accounts on the default path, Python/Node runtime dependencies, interactive captcha solvers, login/session vaults, or bulk-scraping features.

## Pull requests & issues

1. Branch from `master` (`feat/…`, `fix/…`, `docs/…`) and use Conventional Commits (`feat(search): …`, `fix: …`, `docs: …`, `!` for breaking changes).
2. Place unit tests in inline `#[cfg(test)]` modules and integration tests under `tests/it/` (registered in `tests/it/main.rs` so they share one test binary — never add new top-level `tests/*.rs` binaries).
3. Run scoped tests + `just all` for code changes (docs-only PRs need no build/test run) and ensure CI `gate` is green.

When filing issues, include `donsetch --version`, the target URL or query, and the `[meta]` JSON receipt (`verdict` for walls; `engines` and `weak` for search).

## Maintainers & CODEOWNERS

| Role | GitHub | Scope |
|---|---|---|
| Maintainer | `@dondai44423` | Repository-wide |
| Co-maintainer | `@Mart-Bogdan` | Search, fetch, GitHub workflows |
| Co-maintainer | `@mnaza` | Search/BYOK, PDF, transport, cookies |
| Subsystem reviewer | `@adaaaaaaaaaaaaaaaaaaaaaa` | MCP surface, `src/spec.rs`, `tests/` |
| Subsystem reviewer | `@imonlinux` | `Dockerfile`, `docker-compose.yml`, HTTP MCP |
