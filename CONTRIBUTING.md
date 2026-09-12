# Contributing to DonSeTch

Thanks for your interest in contributing. DonSeTch is AGPL v3 — all contributions must be under the same license.

## Build from source

```bash
git clone https://github.com/dondai44423/donsetch.git
cd donsetch
cargo build --release                     # core build (fetch, search, crawl, PDF)
cargo build --release --features ocr,rerank  # full build (adds OCR + semantic reranking)
```

**Prerequisites**: Rust 1.98+ (pinned via `rust-toolchain.toml`), Go 1.22+, NASM, LLVM/Clang, CMake. See the [README](README.md#install) for platform-specific install commands.

## Development workflow

```bash
cargo test --features ocr,rerank          # 637 tests
cargo clippy --all-targets --features ocr,rerank -- -Dwarnings   # zero warnings enforced
cargo fmt --all -- --check    # formatting check
```

All three must pass before a PR can merge. CI runs the same checks on Linux, macOS, and Windows.

The same tasks are wrapped as [`just`](https://just.systems) recipes — `just test`, `just lint`, `just fmt-check`, `just smoke`.

### Verifying Windows compilation from Linux

Linux-only changes regularly broke Windows CI in `cfg`-gated code that never
compiled locally. `just win-check` type-checks the whole crate for
`x86_64-pc-windows-gnu` without linking — both ends of the feature matrix
(full `ocr,rerank,http` and `--no-default-features`), all targets,
`-Dwarnings` — so those breaks surface before the push. It is not part of
`just all` because it needs a cross toolchain on top of the normal build
prerequisites (NASM, CMake and libclang are already required above). On
Debian/Ubuntu:

```bash
sudo apt install mingw-w64 pkg-config
rustup target add x86_64-pc-windows-gnu
```

On other distros install the equivalents: the x86_64 mingw-w64 cross
toolchain (gcc and g++) and pkg-config. If you skipped the normal
prerequisites, add NASM, CMake and libclang (`nasm cmake libclang-dev` on
Debian/Ubuntu).

Per recipe:

- **`win-check-core`** (`--no-default-features`): `mingw-w64` (cross
  gcc/g++), `nasm` + `cmake` (BoringSSL build), `libclang-dev`
  (bindgen for boring-sys), and the `x86_64-pc-windows-gnu` rustup
  target. Run this one first; it is the cheaper half.
- **`win-check-full`** (`ocr,rerank,http`): all of the above plus
  `pkg-config` — the ort binary downloader builds a host-side
  `openssl-sys`, which needs pkg-config to find the system OpenSSL
  headers (`libssl-dev`, usually already present).
- **`win-check`** runs both.

The first run compiles BoringSSL for the mingw target (a few minutes); warm
runs take seconds. The recipe's comments in the Justfile explain the three
environment workarounds it applies. Windows CI proper stays authoritative —
it builds the msvc target and links — but `win-check` catches the common
case. Type-checking is the ceiling here: an actual `cargo build` for
`x86_64-pc-windows-gnu` fails to link even with `--no-default-features`
(BoringSSL), so do not expect a runnable binary out of the cross setup. The
common case is code under `#[cfg(windows)]` or `#[cfg(unix)]` that no longer compiles
on the other side. Note that rustc reports the first error it hits, so the
message can differ from the Windows CI log for the same break (a missing
import surfaces before a type mismatch on the same line, for instance).

### Windows

The recipes are POSIX shell (`2>/dev/null`, `head -c`, `cd fuzz && …`) and `just` runs them with `sh`,
which PowerShell does not have on `PATH`. Either:

- **Run `just` from a Git Bash prompt.** Works with no setup, wherever Git is installed.
- **Stay in PowerShell** and point `just` at Git's `sh` with a wrapper in your profile.
  `$PROFILE` holds the path — open it with the editor of your choice (`code $PROFILE`, `notepad $PROFILE`),
  creating the file if it does not exist:

  ```powershell
  function just { just.exe --shell "C:\Program Files\Git\bin\sh.exe" @args }
  ```

  Adjust that path if Git is not at the installer default: scoop, winget per-user, and portable installs put `sh.exe` elsewhere.

Do not put Git's `usr\bin` on the global `PATH` as a shortcut — it holds GNU tools whose names collide with Windows ones (`find`, `sort`, `link`), which can confuse toolchains outside this repo. Git Bash exposes them only inside its own session, which is why running `just` there is safe.

## Commit conventions

Conventional Commits:

```
feat: add proxy rotation
fix: handle 429 retry-after
docs: update README
chore: bump deps
feat!: breaking change (the ! marks breaking)
```

Scope optional: `feat(search): add scholar vertical`.

## What we're looking for

- **Bug fixes** with a test that would have caught the bug.
- **New search engines** that are keyless and don't require API keys.
- **BYOK providers** for search: additional cloud/API providers (Tavily, Exa, Serper, TinyFish, etc.) wired through `donsetch keys`. These are opt-in, behind a user-supplied API key, and never required for the core toolset to work. PRs for new providers or requests for specific ones are both welcome.
- **Anti-bot improvements**: new wall vendors, better detection heuristics.
- **PDF extraction** edge cases: corrupt files, weird encodings, exotic tables.
- **Performance**: measured improvements, not micro-optimizations.

## What we're not looking for

- **Core features that hard-require API keys.** The default toolset must work with zero keys, zero accounts. BYOK providers are fine because they're optional: the user opts in by providing their own key. If a feature genuinely needs a key and provides massive value (e.g., a specialized data source), it can be accepted as an optional provider behind BYOK, clearly marked as opt-in. It must never degrade the keyless experience.
- Adding Python or Node.js dependencies. DonSeTch is Rust, built from scratch. That's the point.
- Solving interactive captchas (hCaptcha, reCAPTCHA). That's a hard line, not a TODO.
- Mass-scraping features. DonSeTch is for agentic research, not bulk extraction.

## Architecture overview

DonSeTch is built from scratch — no dependency on existing OSS web tooling:

| Component | What it does | Key files |
|---|---|---|
| DonShadow | Tier 1 stealth HTTP fetch (BoringSSL TLS, own HTTP/1.1 + HTTP/2) | `src/fetch/`, `src/transport/` |
| DonGhost | Tier 2 headless browser (CDP, no Runtime/Console/Debugger) | `src/ghost/` |
| DonSift | HTML-to-markdown extraction engine (block model, BM25 focus) | `src/extract/` |
| DonSeek | Keyless multi-engine search (RRF + BM25 + semantic reranking) | `src/search/` |
| DonTread | Crawl engine (sitemap, frontier, Governor pacing) | `src/crawl/` |
| DonSheet | PDF extraction (PDFium FFI, OCR arbitration, fusion) | `src/pdf/` |
| MCP daemon | stdio server (JSON-RPC 2.0, MCP 2024-11-05+) | `src/mcp/` |

## Pull requests

1. Fork the repo, create a branch (`feat/...`, `fix/...`, `docs/...`).
2. Write tests for your change.
3. Ensure `cargo test --features ocr,rerank`, `cargo clippy --all-targets --features ocr,rerank -- -Dwarnings`, and `cargo fmt --check` all pass.
4. Open a PR with a conventional commit title.
5. CI must be green on all 3 platforms before merge.

## Reporting issues

- **Bugs**: include the URL you tried to fetch/search/crawl, the DonSeTch version (`donsetch --version`), and the structuredContent from the response.
- **Anti-bot failures**: include the site URL and the `verdict` field from the response.
- **Search issues**: include the query, the `engines` report from structuredContent, and whether `weak=true`.

## Reviewers & maintainers

DonSeTch reviewers earn their paths through sustained high-quality
contributions. The two tiers:

- **Co-maintainer** (collaborator): merges once CI is green, keeps the
  project moving outside maintainer hours.
- **Subsystem reviewers**: CODEOWNERS auto-requests their review on
  their owned paths. No merge power, no access grants, just the
  strongest review signal the repo can give.

| Role | Person | Owns |
|---|---|---|
| Maintainer | @dondai44423 | everything |
| Co-maintainer | @Mart-Bogdan | search, ghost, fetch, GitHub config |
| Co-maintainer | @mnaza | PDF, h2 transport, cookies, search BYOK |
| Subsystem reviewer | @adaaaaaaaaaaaaaaaaaaaaaa | MCP surface, tool specs, tests |
| Subsystem reviewer | @imonlinux | Docker image, compose, HTTP transport |

(A personal-account repository has only two access levels: owner and
collaborator. There is no triage tier on this repo type, so reviewer
recognition lives in CODEOWNERS.)

Authors of exceptional one-shot contributions (CloakBrowser,
SerpBase, BYOK adapters, and others) are credited in the changelog
and release notes and are welcome back for a second wave, at which
point subsystem ownership opens up.

## License

By contributing, you agree that your contributions will be licensed under the AGPL v3.
