<div align="center">

# DonSeTch

**The web, for AI agents.**

<div align="center">
<a href="https://trendshift.io/repositories/163922?utm_source=trendshift-badge&amp;utm_medium=badge&amp;utm_campaign=badge-trendshift-163922" target="_blank" rel="noopener noreferrer"><img src="https://trendshift.io/api/badge/trendshift/repositories/163922/daily?language=Rust" alt="dondai44423%2Fdonsetch | Trendshift" width="250" height="55"/></a>
</div>

[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/G5Y624N5RE)

[![Rust](https://img.shields.io/badge/Rust-edition%202024-ce422b?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![MCP](https://img.shields.io/badge/MCP-server-7c3aed?logo=modelcontextprotocol&logoColor=white)](https://modelcontextprotocol.io)
[![License](https://img.shields.io/badge/license-AGPL%203.0-2563eb)](LICENSE)
[![Tests](https://img.shields.io/badge/tests-751%20passed-00d4aa)](#)
[![npm](https://img.shields.io/npm/v/donsetch?color=cb3837&logo=npm)](https://www.npmjs.com/package/donsetch)
[![npm downloads](https://img.shields.io/npm/dm/donsetch?color=cb3837&logo=npm&label=downloads)](https://www.npmjs.com/package/donsetch)
[![GitHub stars](https://img.shields.io/github/stars/dondai44423/donsetch?style=flat&logo=github&color=e3b341)](https://github.com/dondai44423/donsetch/stargazers)

[Install](#-install) · [Quickstart](#-quickstart) · [The 3 tools](#-the-3-tools) · [Chrome TLS](#-chrome-tls-not-chrome-like) · [Solve & Bounce](#-solve-and-bounce) · [Search](#-keyless-search) · [PDF](#-pdf--ocr) · [Benchmark](#-wrb-web-research-benchmark) · [Comparison](#-comparison) · [Gotchas](#-gotchas) · [Limits](#-honest-limits)

</div>

---

<img src="assets/herobanner.png" alt="DonSeTch, the web, for AI agents" width="100%">

>
> **🤝 Use Bright Data? Support DonSeTch.** Get your proxy, SERP, or
> Web Unlocker plan through this partner link and part of it comes
> back to keep DonSeTch free and actively developed. You pay nothing
> extra:
>
> 💰 **https://get.brightdata.com/ivqwoicrrlbr**
>
> Bright Data is woven into the tool itself: the `bd` SERP provider,
> the Web Unlocker tier-3 bypass, and the `unlocker` key type plug
> straight into a Bright Data account.

DonSeTch gives any AI agent full web research from a single local process.
Three tools, zero API keys, zero accounts. Rust, one binary. Every layer
built from scratch: no reqwest, no hyper, no Playwright, no Selenium.

Works with every MCP client (Claude Code, Cursor, OpenCode, Pi, Hermes)
and as a standalone CLI.

## ✨ What makes it different

| | What it does |
|---|---|
| 🛡️ **Real Chrome TLS** | Drives Chrome's own BoringSSL natively. Your ClientHello IS Chrome's ClientHello. The fingerprint is emergent from the real engine, not a faked table that rots. |
| ⏱️ **Temporal stealth** | TLS session resumption, conditional revalidation (304), persistent cookies, connection pooling. The loudest remaining bot tell, and nobody else fakes it. |
| 👻 **Solve-and-bounce** | Browser solves the challenge, hands cookies back to tier 1, goes to sleep. The browser almost never fetches content. |
| 🧠 **Self-improving fetch** | Learns from every fetch. Cookie lifetimes learned adaptively. Warm starts skip the browser. Converges to optimal routing per domain. |
| 🔑 **Keyless search** | 10+ backends in parallel, fused by cross-engine consensus + local semantic reranking. No API keys. $0 forever. BYOK optional. |
| 📄 **Pixel-fusion PDF** | Glyphs + rendered pixels from the same stream, fused deterministically. Per-region trust audit. Scanned PDFs auto-OCR'd. |
| 🧬 **Built from scratch** | Own HTTP/2 (HPACK, flow control), own extraction engine, own PDF parser, own search aggregator, own crawl engine. |
| 🪶 **~3.5k tokens** | Three tools, ~3.5k tokens total in the MCP context. Every token earns its place. |

## 🆕 v3, the agent-first upgrade

Context is the agent's budget. v3 saves it aggressively:

| | What it does |
|---|---|
| 🔗 **Reference handles** | Links render as `[text](L12)`, search results as `S1…Sn`, and `fetch S3` just works. URLs cost 80 tokens, handles cost 3. Raw URLs stay in `structuredContent`. |
| 🧾 **Probe mode** | `must_contain` verifies a claim against the fully fetched page but returns MATCH/NO-MATCH + up to 3 excerpts (~60 tokens instead of 4k). |
| ♻️ **Resurrection fetch** | Dead link? `archive=auto` serves the nearest Wayback snapshot, honestly labeled with its age. |
| 🕵️ **Anti-cloak check** | On decoy-prone domains, tier-1 responses are equivalence-checked against a headless render. `decoy suspected` is stamped, never silently passed as content. |
| 📌 **Page memory** | Every fetch is fingerprinted. Re-fetches report `changed` with section-level diffs; `since_last=true` collapses a re-check to one line (~30 tokens). |
| 🧠 **Domain intelligence** | Reddit, npm/PyPI/crates.io/Go/RubyGems, GitHub, Stack Overflow, Wikipedia, docs sites get restructured from each site's own keyless surfaces, labeled `via=adapter:…`, kill-switchable. |
| ⏱️ **The clock** | `deadline_ms` everywhere, real MCP cancellation, progress notifications, ms cost footer. Nothing can silently hang. |
| 🧵 **Article stitching** | `stitch=true` walks `rel=next` into ONE call with part markers. |
| ⚡ **Warm handoff** | Search pre-fetches top results; the next `fetch S1` serves from cache in ~3ms. |
| 🧯 **Crash-only daemon** | `donsetch mcp --supervised`: a panic is a blip, the daemon restarts and the session survives. |
| 🧾 **Stable error codes** | `wall.challenge`, `guard.ssrf`, `deadline.hit`, `archive.stale`… branch on codes, not prose. |

## 🎬 Demo

<div align="center">

<video src="https://github.com/user-attachments/assets/32bc0899-87bf-417b-8ca8-c0a4a51ee167" controls muted width="640"></video>

</div>

*(30-second walkthrough: search, bot-wall bypass, crawl)*

<div align="center">

<video src="https://github.com/user-attachments/assets/f164b31e-96ef-4294-b2dd-6777642098dc" controls muted width="640"></video>

</div>

*(Pi agent session: live research with DonSeTch as a native extension)*

## 📦 Install

**npm (recommended, any platform):**

```bash
npm install -g donsetch
```

Downloads the prebuilt binary for your platform from GitHub Releases
with SHA256 verification. No build tools needed. Linux prebuilts run
on glibc >= 2.35 (Ubuntu 22.04 LTS and newer; the bundled ONNX lib
keeps its own 2.27 floor, so OCR/rerank work on every one of those).

**Homebrew (macOS/Linux):**

```bash
brew tap dondai44423/donsetch && brew install donsetch
```

**Pi agent (native extension):**

```bash
pi install npm:donsetch
```

Registers the 3 tools as native pi tools, spawns the binary at session
start, self-updates with `pi update --extensions`.

<details>
<summary><b>Build from source</b></summary>

| Dependency | Why | Linux | macOS | Windows |
|---|---|---|---|---|
| **Rust** | toolchain | `rustup` | `rustup` | `rustup` |
| **Go** | BoringSSL build | `apt install golang-go` | `brew install go` | `winget install GoLang.Go` |
| **NASM** | BoringSSL asm | `apt install nasm` | `brew install nasm` | `choco install nasm` |
| **CMake** | BoringSSL build | `apt install cmake` | `brew install cmake` | `winget install cmake` |
| **Clang** | bindgen | `apt install clang libclang-dev` | bundled | `choco install llvm` |
| **LLD** | PDFium link (aarch64) | `apt install lld` | not needed | not needed |

```bash
git clone https://github.com/dondai44423/donsetch.git
cd donsetch
cargo build --release --features ocr,rerank,http
```

On Ubuntu 22.04 (or any distro with bfd 2.38): build with lld
explicitly. bfd cannot parse the `.crel` relocations rustc 1.86+
emits for aarch64, and the default link dies with "unknown
architecture" errors:

```bash
sudo apt-get install -y cmake build-essential pkg-config libclang-dev clang lld nasm golang-go
RUSTFLAGS="-C link-arg=-fuse-ld=lld" cargo build --release
```

Same recipe works for the release-binaries-for-22.04 case: any
prebuilt from v3.4.5+ is built on the Ubuntu 22.04 baseline and
runs there directly.

First build compiles BoringSSL (~2 min), cached after. Chromium is
optional (tier 2 escalation); DonSeTch auto-discovers system Chromium,
Playwright's cached builds, or Edge on Windows.

Feature matrix: `default = []` = fetch/search/crawl/PDF. `ocr,rerank`
pulls in ONNX Runtime, `http` enables the HTTP MCP transport. The npm
prebuilt ships all three on linux-x64, macOS-arm64, Windows-x64;
linux-arm64 and macOS-x64 are core-only (ONNX has no working prebuilt
for those targets). Linux ARM64 has two honest limits: no OCR/rerank
(the ONNX aarch64 prebuilt deadlocks at load) and fragile PDF (a
loader hang in some paths, tracked in CI).

</details>

### Selectable Ghost browser backend

DonSeTch supports the original Chromium browser and CloakBrowser. The original
Chromium backend remains the default behavior: headful on Xvfb/off-screen when
available, with `--headless=new` only when no display is available.

Set `DONSETCH_BROWSER_BACKEND` explicitly when choosing the runtime:

```bash
# Original Chromium backend (preserves the shipped behavior)
DONSETCH_BROWSER_BACKEND=chromium donsetch doctor

# Original Chromium binary, forced into headless mode
DONSETCH_BROWSER_BACKEND=headless donsetch doctor --deep

# CloakBrowser
DONSETCH_BROWSER_BACKEND=cloakbrowser \
  CLOAKBROWSER_BINARY_PATH=/path/to/chrome donsetch doctor --deep
```

Accepted aliases are `original` for `chromium` and `original-headless` for
`headless`. `auto` is also accepted: it uses a configured local CloakBrowser
path when present, otherwise the original Chromium backend. CloakBrowser
downloads remain disabled unless explicitly enabled.

For a local CloakBrowser build, `CLOAKBROWSER_BINARY_PATH` is used without any
network access. Public downloads are opt-in only:

```bash
DONSETCH_BROWSER_BACKEND=cloakbrowser \
  DONSETCH_CLOAK_AUTO_DOWNLOAD=1 donsetch doctor --deep
```

The installer downloads the platform archive from CloakBrowser's public GitHub
release, verifies its detached Ed25519-signed `SHA256SUMS`, binds the manifest
to the requested Chromium version, checks the archive SHA-256, rejects unsafe
archive paths, and caches the executable below DonSeTch's cache directory.
`CLOAKBROWSER_VERSION` pins a full numeric version. CloakBrowser binaries are
not bundled in DonSeTch releases or Docker images.

**Verify the install:**

```bash
donsetch doctor          # 14 checks, ~1 second
donsetch doctor --deep   # adds the live browser probe
donsetch doctor --fix    # repairs mechanical problems automatically
```

`doctor --json` emits a machine-readable report; it also detects your
MCP client and prints ready-to-paste registration blocks.

## Quickstart

Two ways to use it:

**1. MCP server (for any agent).** Register and go:

```json
{
  "mcpServers": {
    "donsetch": {
      "command": "donsetch",
      "args": ["mcp", "--supervised"]
    }
  }
}
```

Or via `npx` without a global install:
`"command": "npx", "args": ["donsetch", "mcp"]`.

HTTP transport (optional): `donsetch mcp --http --port 8765`, clients
connect to `http://localhost:8765/mcp`. Sessions, cancellation,
`/health`, token auth via `DONSETCH_HTTP_TOKEN`, per-request timeout,
all documented in `donsetch mcp --help`.

**2. CLI (for humans and scripts).** Same engine as MCP, thin adapter:

```bash
donsetch fetch https://example.com --focus "pricing"
donsetch search "rust async patterns" --intent code
donsetch crawl https://docs.python.org --mode map --topic asyncio
```

## 🔀 HTTP Proxy

Optional proxies for search and crawl only. The fetch system always
uses the main home IP: proxied fetches burn the Stealth guarantee and
the fetch tier gets no config path for them by design.

```bash
donsetch proxy add <url>            # rotate-able proxy entry
DONSEEK_PROXIES="url1,url2"         # env form, comma separated
donsetch proxy list                 # list + masked creds
donsetch proxy check                # live connectivity test
donsetch proxy remove/clear         # manage
```

## 🐳 Docker

```bash
docker build -t donsetch-mcp .
docker compose up -d                 # loopback-only by default
```

Multi-stage build, non-root user, optional Chrome, resource limits in
the compose file. An opt-in `http` compose profile serves the HTTP
transport with a healthcheck; `docker compose stop` gives in-flight
tier-2 fetches a 45s grace period.

## 💻 CLI

The CLI is a thin adapter over the same engine the MCP server uses:

| Command | What it does |
|---|---|
| `donsetch fetch <url>` | Fetch as clean markdown (flags: `--focus`, `--max-chars`, `--json`) |
| `donsetch search <query>` | Search, keyless + BYOK (`--intent`, `--max-results`) |
| `donsetch crawl <url>` | Crawl (`--mode map\|full\|content`, `--topic`) |
| `donsetch mcp` | MCP server (stdio or `--http`) |
| `donsetch doctor` | Health check + auto-fix (`--deep`, `--json`, `--fix`) |
| `donsetch keys` | BYOK provider management (`add`, `list`, `default`, `export`) |
| `donsetch proxy` | Proxy management (`add`, `list`, `check`, `clear`) |
| `donsetch status` | Version, keys, proxies, cache, health overview |
| `donsetch update` / `rollback` | Self-update from GitHub Releases, revert |
| `donsetch tools` | Tool schemas as JSON (same as MCP `tools/list`) |

## 🎯 The 3 tools

| Tool | What it does |
|---|---|
| 🌐 **`web_fetch`** | Any URL as clean markdown. HTTP first, escalates to headless browser on bot walls. PDFs with OCR + per-page confidence, `focus`/`toc`/`section`, pagination, `actions` for in-page control, `must_contain` probes, `archive` resurrection. |
| 🔎 **`web_search`** | Keyless multi-engine search: 10+ backends, consensus + semantic reranking, query-aware official-source placement. Returns ranked URLs + snippets. |
| 🕷️ **`web_crawl`** | Best-first same-domain crawl. Sitemap + frontier, `focus` ranking, elastic pacing, resume tokens, honest stop reasons. |

Tool schemas: `donsetch tools`. Every tool returns structured errors
with stable codes + `next_action`; every content result carries
`content_ok`, `quality`, and the escalation trace.

## 🖱️ Browser actions, page control inside fetch

`web_fetch` takes an `actions` array executed in the real browser
before extraction:

```json
{
  "url": "https://duckduckgo.com",
  "actions": [
    { "do": "type", "selector": "input[name=q]", "text": "rust async tokio" },
    { "do": "press", "key": "Enter" },
    { "do": "wait_text", "text": "tokio" }
  ],
  "focus": "tokio"
}
```

Steps: `wait`, `wait_selector`, `wait_text`, `click`, `hover`, `type`
(human-cadence), `press`, `scroll`. Up to 16 steps, deterministic
waits, per-step results in `structuredContent.actions`. Search flows,
form submits, load-more buttons: one call, no separate browser tool.

## 🛡️ Chrome TLS, not Chrome-like

Everyone else patches a foreign TLS stack to resemble Chrome and ships
hardcoded fingerprint tables that rot. DonSeTch drives Chrome's own
BoringSSL with Chrome's native behaviors on: GREASE, extension
permutation, ECH-GREASE, ALPS, SCT, OCSP, cert-compression. The
ClientHello is generated by the same machinery Chrome uses.

<details>
<summary><b>Verified against live Chromium at tls.peet.ws</b></summary>

| Signal | Match |
|---|---|
| **JA4** | cipher hash identical to Chrome |
| **Akamai h2 fingerprint** | exact match |
| **h2 header order** | sec-ch-ua → sec-ch-ua-mobile → sec-ch-ua-platform, exact |
| **Extension set** | identical (contents differ only in random key material) |

</details>

### Own HTTP/2 stack

Off-the-shelf h2 doesn't expose pseudo-header order, the exact SETTINGS
set, WINDOW_UPDATE values, or HPACK indexing strategy, all
fingerprintable. So DonSeTch has its own: HPACK (all 257 Huffman
symbols + 61 static entries, verified), frame engine (SETTINGS, DATA,
WINDOW_UPDATE, PING, GOAWAY, RST_STREAM, CONTINUATION), flow control
with replenishment, TLS 1.3 session resumption, connection pool.

The h2 preface is asserted byte-identical to Chromium in CI:
detectability regressions are build failures.

### Temporal stealth

| Mechanism | Why it matters |
|---|---|
| TLS session resumption | Scrapers never resume. Chrome always does. |
| h2 connection pooling | Fresh connection per request = bot signal. |
| Conditional revalidation | 304 → serve from cache. Browsers do this. |
| Happy Eyeballs | IPv6/IPv4 race with 250ms stagger, like Chrome. |
| Persistent cookie jar | No cookie memory = bot. |

## 👻 Solve-and-bounce

> **The browser almost never fetches content.** It exists for exactly
> two things HTTP can't do: pass JS challenges and execute JS-rendered
> pages. Its output is cookies (handed to tier 1, which fetches at full
> speed) or rendered HTML (handed to the extraction engine).

| Step | What happens | Speed |
|---|---|---|
| 1. Tier 1 | Fast stealth HTTP | ~100-300ms |
| 2. Wall detected | Cloudflare / DataDome / PerimeterX / Akamai | - |
| 3. Ghost solves | Headless browser clears the challenge, harvests cookies | ~2-6s |
| 4. Bounce | Cookies to tier 1, refetch at full speed, browser sleeps | ~100-300ms |
| 5. After | Tier 1 with warm cookies, browser stays asleep | ~100-300ms |

Raw CDP launch without automation flags: `navigator.webdriver` is
natively false. No JS injection ever, no spoofed patches. Real window,
real GPU, real locale.

<details>
<summary><b>Process lifecycle, the RAM-smart part</b></summary>

The ghost process is SIGSTOP'd (frozen) after 20s idle, reaped after
10 min frozen:

| State | RAM | CPU | Wake time |
|---|---|---|---|
| Active | full | real | - |
| Frozen | mapped but cold | 0 | ~50ms |
| Reaped | freed | 0 | ~1-2s relaunch (profile keeps warmth) |

Crash-transparent: thaw finds a dead browser, silently relaunches.
Persistent profile keeps cookie/clearance state across restarts.

</details>

## 🧠 Self-improving fetch (experimental)

Every fetch is an action AND an observation. Pure deterministic
state, no ML. The loop converges; the more you use it, the less it
escalates.

| Visit | Route | What happens |
|---|---|---|
| 1 (unknown) | `Cold` | tier 1 → walled → solve → store cookies |
| 2 (fresh) | `Warm` | tier 1 with cookies, browser asleep |
| N (expired) | `SkipToSolve` | straight to ghost, no doomed round-trip |
| M (24h later) | `RecheckCold` | wall may be gone, try tier 1 cold |

Cookie lifetime converges: `observed_lifetime = min(previous,
now - last_solved)`. Only clearance cookies persist (cf_clearance,
datadome, _abck); tracking cookies are filtered out.
Disable with `DONSEEK_NO_DISK_STATE=1`. Experimental: edge cases exist.

<div align="center">

<img src="assets/owlsearch.png" alt="DonSeTch Search">

</div>

## 🔎 Keyless search

No API key, no account. 10+ keyless backends in parallel on your
machine, merged, deduped, ranked.

- **Backends**: Brave, Bing, DuckDuckGo, Mojeek, Yandex, Startpage
  + keyless verticals (GitHub, Wikipedia, HN, Semantic Scholar, arXiv,
  StackExchange, MDN, Google News).
- **Semantic reranking**: local ONNX cross-encoder
  (`ms-marco-MiniLM-L-6-v2`, 23MB) reads query + title + snippet
  through full attention. 60/40 blend with RRF + BM25 + consensus.
- **Consensus**: a URL several independent indexes return gets a
  boost; every result carries `score`, `consensus`, `engines`.
- **Entity coverage penalty**: anchor entities ("B-tree", version
  numbers, years) checked against results. Wrong entity → 0.3x.
- **Honest reporting**: `weak=true` means low consensus; per-engine
  status is always visible. No fake "no results".

Keyless quality (110 questions, 11 niches, no keys): **95.5%**
answer-in-snippet vs Tavily's published 93.3% LLM-graded. Reproduce:
`python3 bench/search_quality.py --verbose`. Full methodology, per-niche
breakdown, and caveats live in the repo and in
`bench/search_quality.py`.

### BYOK (Bring Your Own Keys)

Paid providers add rate limits and premium sources. Optional, never
required:

- Stack keys per provider; DonSeTch rotates and pools them. Two Exa
  keys = one 3,000-credit pool.
- Automatic fallback to keyless when a provider errors or runs dry.
- Per-key rate-limit cooldown and depletion tracking.
- Portable store: `donsetch keys export/import`.

```bash
donsetch keys add tavily tvly-...       # Tavily
donsetch keys add exa sk-exa-...        # Exa (stackable)
donsetch keys add serper ...            # Serper.dev
donsetch keys add serpapi ...           # SerpApi
donsetch keys add bravesearch ...       # Brave Search API
donsetch keys add tinyfish sk-...       # TinyFish (free tier)
donsetch keys add parallel nKil3...     # Parallel AI (fast mode)
donsetch keys add bd 576d013c...        # Bright Data SERP
donsetch keys add unlocker <key>[::zone]  # Bright Data Web Unlocker
donsetch keys default local             # dispatch order: keyless first
```

> Bright Data SERP, Web Unlocker, and their proxy/data products are
> available at [get.brightdata.com](https://get.brightdata.com/ivqwoicrrlbr)
> (affiliate link).

<div align="center">

<img src="assets/byok-keys.png" alt="DonSeTch keys list" width="640">

</div>

---

<div align="center">

<img src="assets/fetch.png" alt="DonSeTch Fetch">

</div>

## 🌐 Fetch

Plain HTTP first (~100-300ms). Wall or JS shell → auto-escalate to
ghost, solve, bounce cookies back, refetch at full speed.

**DonSift extraction engine**: HTML bytes in, agent-native markdown
out. Typed blocks (Heading/Para/List/Table/Code/Quote/Media) with
heading breadcrumbs.

- **`focus`**: BM25-relevant blocks only. Cuts context 80%+ on long
  pages. 12-language BM25 (CJK unigrams + bigrams, stopword lists,
  stemming, accent folding).
- **`toc` + `section`**: outline first, then target one section. Two
  cheap calls instead of one expensive one.
- **Token-war policies**: links stripped by default (~30%), link-farm
  lists dropped, wiki junk dropped, duplicates suppressed.
- **Content classification**: `Article` / `Listing` / `Forum` /
  `Docs` / `Table` / `Page`, quality score 0-1, agent-trust signals
  inline (focus-miss, JS-shell warning, empty-content note).

**Tier 3 bypass** (opt-in): when ghost itself hits a hard wall, fetch
falls back to Bright Data Web Unlocker if a key is configured
(`donsetch keys add unlocker <key>[::zone]`). The unlocker solves
server-side, captchas included, and returns rendered HTML into the
normal pipeline. Advanced users only; DonSeTch works identically
without it. (Bright Data sign-up link above is an affiliate link.)

**Solve-cache**: every successful unlock is cached locally (URL-hash
keyed, sliding 6h TTL, 200 entries, parallel fetches share one paid
call). Same page again inside the TTL = served from cache at zero cost.
Env knobs: `DONSETCH_BYPASS=0` (off), `DONSETCH_BYPASS_MAX_DAILY`
(default 50), `DONSETCH_BYPASS_TIMEOUT_SECS`, `DONSETCH_BYPASS_RENDER`,
`DONSETCH_BYPASS_CACHE_TTL_SECS`, `DONSETCH_BYPASS_CACHE_MAX_ENTRIES`,
`DONSETCH_BYPASS_CACHE=0`.

<details>
<summary><b>Anti-bot benchmark</b></summary>

| Site | Protection | Status |
|---|---|---|
| Cloudflare-protected sites | interstitial | ✅ 200 OK |
| DataDome sites | DataDome | ✅ 200 OK |
| Stack Overflow / Medium | Cloudflare | ✅ 200 OK |
| Reddit | bot detection | ✅ 200 OK |
| Interactive captchas | hCaptcha/reCAPTCHA/Turnstile | ⛔ honest block without a key (unlocker key: ✅) |

</details>

<div align="center">

<img src="assets/crawl.png" alt="DonSeTch Crawl">

</div>

## 🕷️ Crawl

Same-domain, best-first. Two phases: sitemap discovery (cheap URL
inventory), then Governor-paced frontier walk with extraction per page.

- **Modes**: `full` (map + content), `map` (URL inventory only),
  `content` (BFS from seed, no sitemap).
- **Focus-ranked frontier**: `focus="query"` ranks pages by BM25
  relevance, crawls only matches.
- **Adaptive pacing**: the Governor paces per (host, lane).
  429/503 → exponential backoff. Dwell-time variance proportional to
  page size breaks metronome fingerprints.
- **Resume tokens**: stopped crawls resume with one call; valid 30
  min, survive restarts.
- **Near-dup detection**: title + first 200 chars hashed.
- **Honest stop reasons**: `FrontierEmpty` / `MaxPages` /
  `CharBudget` / `DepthLimit` / `Deadline` / `ThrottledOut`.

## 📄 PDF + OCR

PDFs are detected by Content-Type or `%PDF` magic and parsed with a
custom PDFium FFI. No external PDF library, no Python subprocess.

> **Glyphs and rendered pixels come from the same content stream, so
> they are already aligned.** Pixels tell the truth about structure,
> glyphs tell the truth about text. The fusion is deterministic, no
> hallucination.

| Innovation | What it does |
|---|---|
| Pixel-fusion rule extraction | Tables/borders detected on the rendered bitmap. A rule line is a fact, not a hypothesis. |
| Span detection by ink continuity | A cell spans a separator iff the separator has no ink under it. Deterministic colspan/rowspan. |
| Trust audit + arbitration | Glyph stream is authoritative unless zero-glyphs+pixels (scan) or ≥30% PUA garbage; corrupt regions get OCR'd even when neighbors read fine. |
| Orientation canonicalization | Vertical/rotated text = one pipeline, coordinate frames rotated. |
| Confidence honesty | Verbatim glyphs or OCR with per-line confidence; `[uncertain: …]` markers below threshold. |
| Forms as data | AcroForm widgets → name/type/value triples. |

Tier B (lazy): OCR via PP-OCR cascade (En → Zh → Deva) for scans and
broken ToUnicode pages.

<details>
<summary><b>PDF battle test results</b></summary>

40-document battle corpus, zero garbage output, 6-14x faster than
Python alternatives, 120/120 fuzz clean.

| Document type | Result |
|---|---|
| Academic papers | ✅ clean text, math symbols recovered |
| Scanned documents | ✅ OCR'd, confidence-scored |
| Tax forms | ✅ forms as data |
| Multi-column layouts | ✅ reading order preserved |
| Encrypted / corrupt PDFs | ⛔ honest flag with reason |
| Nepali UDHR (broken ToUnicode) | ✅ 10,542 chars at 86% confidence (pymupdf: 28) |

</details>

## 🏗️ Built from scratch

Every layer in Rust. No dependency on existing OSS web tooling.

| Component | What | Where |
|---|---|---|
| 🛡️ **DonShadow** | Tier 1 stealth HTTP, BoringSSL TLS, own h1+h2, temporal stealth, cookie jar | `src/fetch/` `src/transport/` |
| 👻 **DonGhost** | Tier 2 ghost browser, CDP (no Runtime/Console/Debugger), solve-and-bounce, SIGSTOP lifecycle | `src/ghost/` |
| 📝 **DonSift** | HTML→markdown, block model, 12-language BM25 focus, token-war policies | `src/extract/` |
| 🔎 **DonSeek** | Keyless multi-engine search, RRF + BM25 + consensus + semantic reranking | `src/search/` |
| 🕷️ **DonTread** | Crawl engine, sitemap, focus frontier, Governor pacing, resume tokens | `src/crawl/` |
| 📄 **DonSheet** | PDF extraction, PDFium FFI, pixel-truth fusion, OCR cascade, forms | `src/pdf/` |
| 🔌 **MCP daemon** | stdio + HTTP servers, JSON-RPC 2.0, 3 tools, crash-only supervisor | `src/mcp/` |

 **727 tests. Zero clippy warnings.**
`cargo clippy --all-targets --features ocr,rerank -- -Dwarnings` is the law.

## 🔬 WRB: Web Research Benchmark

Benchmarked with [WRB](https://github.com/dondai44423/wrb), a
tool-level benchmark: no LLM, pure string matching, any web tool can
run it by implementing a thin runner adapter. 48 fetch URLs, 55 search
queries, 5 crawl targets.

| Metric | Fetch | Search | Crawl |
|---|---|---|---|
| Success | **95.8%** content retrieval | **96.4%** precision | 74.3% relevant pages |
| Stealth (tier-weighted) | 93.3% | - | - |
| Speed | 772ms median | 1,356ms median | 67 pages / 5 targets |
| Token efficiency | 1,105/page | 802/query | - |
| False positives | **0** | - | - |

Metrics nobody else measures: **honesty** (0 false success claims on
bot walls), tier-weighted stealth (Cloudflare counts more than
Wikipedia), token efficiency at the tool level.

Run it: `git clone https://github.com/dondai44423/wrb && python3 lib/wrb.py donsetch --verbose`

## 📊 Comparison

| | **DonSeTch** | Hound | Crawl4AI | Jina Reader | Firecrawl |
|---|---|---|---|---|---|
| **Language** | Rust | Python | Python | Python (API) | TypeScript |
| **TLS fingerprint** | Real Chrome (BoringSSL) | curl-impersonate | requests | their servers | their servers |
| **Own HTTP/2** | ✅ | ❌ | ❌ | ❌ | ❌ |
| **Temporal stealth** | ✅ | ❌ | ❌ | ❌ | ❌ |
| **Tier 2 strategy** | Solve-and-bounce | browser-fetches-all | n/a | n/a | n/a |
| **Self-improving fetch** | ✅ | ❌ | ❌ | ❌ | ❌ |
| **Web search** | ✅ (keyless 10+) | ✅ | ❌ | ✅ | ❌ |
| **Semantic reranking** | ✅ (local ONNX) | ✅ | ❌ | ❌ | ❌ |
| **Crawl** | ✅ (resume tokens) | ✅ | ✅ | ❌ | ✅ (cloud) |
| **PDF → markdown** | ✅ (pixel-fusion) | ✅ | partial | ✅ | ✅ (cloud) |
| **Scanned-PDF OCR** | ✅ (PP-OCR) | ✅ | ❌ | ❌ | ✅ (paid) |
| **Query focus** | ✅ (12-language BM25) | ✅ | ✅ | ❌ | ❌ |
| **CLI** | ✅ | ❌ | ✅ | ❌ | ❌ |
| **Runs locally** | ✅ | ✅ | ✅ | ❌ | self-host |
| **MCP server** | ✅ | ✅ | community | ✅ | build it |
| **Token cost (tools)** | ~3.5K | ~2.7K | varies | n/a | varies |
| **License** | AGPL v3 | MIT | Apache 2.0 | proprietary | MIT |

## 🆚 DonSeTch vs Firecrawl (live head-to-head)

Both CLIs run live on identical real-world tasks. Firecrawl = paid
cloud API; DonSeTch = free, local, keyless.

### Fetch / Scrape

| URL | Firecrawl (paid cloud) | DonSeTch (free local) |
|---|---|---|
| Reddit | ❌ "we do not support this site" | ✅ real feed content |
| Stack Overflow (Cloudflare) | ✅ 4.7s, verbose | ✅ 7s, clean Q&A (847 tokens) |
| Wikipedia | 267KB | 16KB (**16x smaller**) |
| arXiv PDF | 32.6s, 71KB | 1.4s, 16KB (**22x faster, 4.4x smaller**) |

On Wikipedia DonSeTch returns 16x fewer tokens; on PDFs, 22x faster.

### Search

| | Firecrawl | DonSeTch |
|---|---|---|
| Speed | 1-2s | 5-7s |
| Result style | full scraped articles inline | ranked snippets |
| Code specificity | good | matched or beat (exact GitHub issues) |
| Academic | arXiv + NeurIPS + Wikipedia | ar5iv + arXiv + Wikipedia + NeurIPS |
| Token cost per query | high (5 full articles) | low (snippets, fetch what you need) |

Search is close. Firecrawl is faster and leans mainstream authority;
DonSeTch leans technical specificity and costs far fewer tokens.

### Crawl

| | Firecrawl | DonSeTch |
|---|---|---|
| Speed (fastapi docs, topic DI) | 47.6s | 18.7s (**2.5x faster**) |
| Focus filter | none (firehose) | `--topic` ranks and filters |
| On-miss behavior | dumped verbose unrelated content | failed fast, small, honest (quality 0.22-0.30) |
| No-sitemap discovery | ✅ `map` | needs `--mode content` |

For agent workloads, fetch and crawl matter most: fewer tokens,
faster PDFs, honest failure behavior. Firecrawl's genuine strengths:
search speed, mainstream source authority, no-sitemap discovery.

## ⚠️ Gotchas

| Surprise | Why |
|---|---|
| First build ~2 min | BoringSSL compiles from source. Cached after. |
| Go is a build dependency | BoringSSL's build system is Go-based. |
| OCR/rerank not in default build | ONNX Runtime is heavy and optional: `--features ocr,rerank`. npm prebuilt ships them on linux-x64, macOS-arm64, Windows-x64. |
| First OCR/search downloads models | ~24MB reranker, ~37MB OCR, on first use, cached forever. Pre-seed offline boxes by copying the `ocr`/`rerank` cache dirs. |
| Captchas need Bright Data Web Unlocker | hCaptcha, reCAPTCHA and Turnstile cannot be solved locally, by design. With an unlocker key (`donsetch keys add unlocker <key>[::zone]`), fetch falls back to Bright Data Web Unlocker and captcha-walled pages come through rendered. No key = a clear honest error, never a hang. (Bright Data sign-up link is an affiliate link.) |
| robots.txt ON for crawl | `respect_robots=true` for crawl; `fetch` doesn't check. |
| Search rate-limits without a proxy | Keyless search hits engines from your IP. Set `DONSEEK_PROXIES` for heavy use. |
| Rerank in a CPU-limited container | Auto-clamped to cgroup parallelism on Linux. `DONSEEK_RERANK_THREADS` to override. |
| Windows needs DirectML.dll | In-box since Windows 10 1903. Only trimmed Server Core/Nano images need the NuGet copy beside the binary. |
| Not built for mass scraping | Agentic research, not bulk extraction. |

## 🧱 Honest limits

| It can NOT | Why |
|---|---|
| Sites requiring login | Page rendering, not authenticated sessions. |
| ML-DSA post-quantum signatures | BoringSSL 5.1 lacks them; lands when BoringSSL has it. |
| Search with all engines down | Error with per-engine status. Honest, never fake. |

## 🤝 Contributing

PRs welcome. See [CONTRIBUTING.md](CONTRIBUTING.md). Run
`cargo clippy --all-targets --features ocr,rerank -- -Dwarnings` and
`cargo test --features ocr,rerank` before submitting. AGPL v3: all
contributions under the same license.

## 📄 License

Copyright (c) 2026 Bishesh Bhandari. AGPL-3.0, see [LICENSE](LICENSE).

---

<div align="center">

### If DonSeTch saves you time, ⭐ the repo

[![Stars](https://img.shields.io/github/stars/dondai44423/donsetch?color=ff9f43&style=flat-square)](https://github.com/dondai44423/donsetch)

**AGPL v3** · [Changelog](CHANGELOG.md) · [Issues](https://github.com/dondai44423/donsetch/issues) · [Releases](https://github.com/dondai44423/donsetch/releases)

</div>