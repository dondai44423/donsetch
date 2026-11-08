# fetch core

The fetch path the [mechanism](../design/mechanism.md) describes: carried identity,
carried state, the site's own interface in front of its walled document, and an
in-process V8 engine that runs a challenge page's own scripts when a wall demands
it. One Rust binary, no browser, no layout, no rasteriser.

The crate is the repo-root `core/`. Run every command below from there.
`hard-corpus.json`, `fixtures/` and `runs/` are private working material
(git-ignored); this README is tracked.

```sh
# build (vendored wreq/btls; V8 arrives as a prebuilt static library via deno_core)
cargo build --release

# one fetch: portfolio selection, pacing, state spent and earned
V5_PERSONA=~/Projects/donsetch-evidence/research/persona/chrome-local.json \
  ./target/release/donsetch-core fetch https://example.com/

# identity matrix: every portfolio identity against every target, one request each
./target/release/donsetch-core probe \
  --targets ~/Projects/donsetch-evidence/research/corpus.json --out matrix.json

# interface equivalence: the ids the page links to must be the ids we return
python3 runs/verify_datapaths.py

# cold measurement paired with curl on the same window
./target/release/donsetch-core bench \
  --targets ~/Projects/donsetch-evidence/research/corpus.json --runs 3 --curl --out bench.json

# the same corpus through one egress lane: the egress class is the variable
source ~/.config/secrets.env
./target/release/donsetch-core bench --targets hard-corpus.json --runs 2 --curl \
  --proxy "http://$V5_PROXY_USER:$V5_PROXY_PASS@198.23.243.226:6361" --out runs/a6-dc.json

# steady state: discovery allowed, state carried between rounds (--no-cache
# measures learning alone; without it the page cache answers too)
V5_STATE=/tmp/core/state.json ./target/release/donsetch-core bench \
  --targets ~/Projects/donsetch-evidence/research/corpus.json --runs 3 --persist --discover --curl

# a repeat fetch is answered by the page cache: fresh, revalidated (304) or changed
./target/release/donsetch-core fetch https://pypi.org/ [--revalidate] [--no-cache]

# a recognised challenge (AWS WAF's goku shape) mints in-process on its own;
# V5_ENGINE_LOG=1 prints the host surfaces and cookies the mint needed
V5_ENGINE_LOG=1 ./target/release/donsetch-core fetch https://www.imdb.com/ --identity chrome-local

./target/release/donsetch-core identities [--headers]   # the portfolio and what each sends
./target/release/donsetch-core state [show|reset]       # scores, priors, jars (reset clears pages too)
./target/release/donsetch-core cache clear              # forget cached pages, keep what was learned
```

## What is in here

| File | Role |
|---|---|
| `src/lib.rs` | The library: `Session::fetch(&Request) -> Outcome`, `Send + Sync`, pooled workers folding into one learned state |
| `src/identity.rs` | The portfolio: coherent clients, one profile each; the built-in local identity (`personas/chrome-local.json`) |
| `src/http.rs` | One request on the wire, with the identity's headers and cookies |
| `src/classify.rs` | Nine outcome classes, vendor-and-ask driven |
| `src/contract.rs` | Content contracts (the frozen corpus's own predicates) |
| `src/state.rs` | Scores, vendor priors, routes, cache trust, pacing clocks, cookie jars with bindings; bounded, merged into the file under a lock; the per-OS cache path |
| `src/cache.rs` | The page cache: what may be stored, served fresh, revalidated or audited |
| `src/fetch.rs` | The encounter (`Worker`): pace, spend, classify, move to the identities a named vendor has let through, run the engine, ask the interface, remember |
| `src/challenge.rs` | Native challenge contracts: Akamai's `bm-verify` hand-off, SiteGround's and NCBI's proof of work; hands page programs to the engine |
| `src/datapath.rs` | Interface contracts: which host, which request, which items count |
| `src/engine.rs` | The challenge engine: V8 in-process, one HTTP op on the fetcher's identity, the mint, the watchdog |
| `src/engine_host.js` | The bounded host layer: small DOM (including a page's own forms and a JS challenge's form navigation), script loader, browser surfaces, WebAssembly budget, no rendering |
| `src/webcrypto.rs` | WebCrypto on BoringSSL: digests, HMAC, AES, PBKDF2, HKDF; anything else is refused |
| `src/envelope.rs` | The engine's limits: wall clock, heap, typed-array and WebAssembly memory, op count, URL/redirect/size guard, one engine at a time, resident-memory backstop on Linux, Windows and macOS |
| `src/testkit.rs` | A local HTTP server for offline tests, including hostile replies (endless, trickled, silent, redirecting) |
| `src/engine_tests.rs`, `src/lifecycle_tests.rs` | Offline tests: hostile pages against the envelope and host fidelity; a fake AWS WAF origin with a controllable clock against the token lifecycle |
| `src/fixture_tests.rs`, `fixtures/aws-waf/` | The three AWS WAF shapes (goku, legacy inline, interstitial) run from captured bundles against a local verifier |
| `src/session_tests.rs` | The library seam: threads, `spawn_blocking`, proxy, body capture, vendor escalation, route memory, state location |
| `src/cache_tests.rs`, `src/redirect_tests.rs`, `src/revalidation_tests.rs` | The page cache against a local site that changes, lies, walls and redirects under it |
| `src/main.rs` | CLI: `fetch`, `probe`, `bench`, `identities`, `state`, `cache` |

## Rules

The rules this code follows are [project.md](../project.md) `#RULES` and the
[mechanism](../design/mechanism.md) guards; the engine's limits and credential lifecycle are in
the design's engine sections.

## Environment

| Variable | Meaning |
|---|---|
| `V5_STATE` | state file path (default `donsetch/v5/state.json` under the OS cache directory: `%LOCALAPPDATA%`, `~/Library/Caches`, `$XDG_CACHE_HOME` or `~/.cache`; empty disables persistence); cached pages live in `pages/` beside it |
| `RUSTY_V8_ARCHIVE` | a local prebuilt V8 static library, for offline builds |
| `SSL_CERT_FILE`, `SSL_CERT_DIR` | extra trust roots, added to the bundled set and the OS store |
| `V5_PERSONA` | captured identity JSON overriding the built-in `personas/chrome-local.json` |
| `V5_ENGINE_LOG` | `1` prints the challenge engine's host surfaces, cookies and errors |
| `V5_ENGINE_DUMP_DIR` | a directory: every request the challenge page makes, headers and body, written out so a program's failure is read instead of guessed at (inspection only) |
| `V5_ENGINE_WALL_MS` | the engine's wall-clock cap per mint (default 10000); the other limits are `envelope::Limits` defaults |

Tests are offline: `cargo test --release --offline`. Four tests are `#[ignore]`d by
default — the three AWS WAF fixture tests need the private captures
(`--include-ignored`) and one is a measurement — and the counts are in
[status.md](../status.md). Engine tests take a process-wide turn, so timing
assertions are not contaminated by parallel tests. Measured runs land in `runs/`
(bench JSON, the identity matrix, datapath equivalence).

Where the suite runs: linux-x64 and linux-aarch64 in CI, both with the product
binary starting (PDFium links with the toolchain's `rust-lld`; see the root README,
"LLD"). The product's macOS and Windows jobs exercise this crate as a library
through their own suites; a native `cargo test` on those two happens on a
contributor's machine at release, never in CI.
