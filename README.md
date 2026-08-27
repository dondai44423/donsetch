<div align="center">

<img src="assets/herobanner.png" width="640" alt="DonSeTch — The web, for AI agents">

**$0. No keys, no accounts. Built from scratch in Rust.**

Fetch · search · crawl · bypass bot walls · read PDFs (even scanned) · semantic reranking

One binary · Chrome's own TLS · headless browser escalation · zero Python

Works with **Claude Code**, **Cursor**, **OpenCode**, **Pi**, **Windsurf**, and any MCP client.

</div>

<br>

<div align="center">

[![npm](https://img.shields.io/npm/v/donsetch?color=cb3837&logo=npm)](https://www.npmjs.com/package/donsetch)
[![npm downloads](https://img.shields.io/npm/dm/donsetch?color=cb3837&logo=npm&label=downloads)](https://www.npmjs.com/package/donsetch)
[![GitHub stars](https://img.shields.io/github/stars/dondai44423/donsetch?style=flat&logo=github&color=e3b341)](https://github.com/dondai44423/donsetch/stargazers)
[![CI](https://img.shields.io/github/actions/workflow/status/dondai44423/donsetch/ci.yml?branch=master&logo=github&label=CI)](https://github.com/dondai44423/donsetch/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-edition%202024-ce422b?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![MCP](https://img.shields.io/badge/MCP-server-7c3aed?logo=modelcontextprotocol&logoColor=white)](https://modelcontextprotocol.io)
[![License](https://img.shields.io/badge/license-AGPL%203.0-2563eb)](LICENSE)
[![Tests](https://img.shields.io/badge/tests-674%20passed-00d4aa)](#)

</div>

<br>

```bash
npm install -g donsetch
```

<div align="center">

[Install](#-install) · [Two ways to use it](#-two-ways-to-use-it) · [The 3 tools](#-the-3-tools) · [Chrome TLS](#-chrome-tls-not-chrome-like) · [Solve & Bounce](#-solve-and-bounce) · [Search](#-keyless-search) · [PDF](#-pdf--ocr) · [Benchmark](#-wrb-web-research-benchmark) · [Comparison](#-comparison) · [Gotchas](#-gotchas) · [Limits](#-honest-limits)

### 🔥 [DonSeTch vs Firecrawl — live head-to-head](#-donsetch-vs-firecrawl-live-head-to-head)

</div>

---

DonSeTch gives any AI agent full web research from a single local process. Three tools, zero API keys, zero accounts. Built in Rust — one binary, no Python, no Playwright, no Selenium, no `reqwest`, no `hyper`. Every layer built from scratch.

Works with every MCP client: Claude Code, Cursor, OpenCode, Pi, anything that speaks MCP. Also works as a standalone CLI.

## ✨ What makes it different

| | What it does |
|---|---|
| 🛡️ **Real Chrome TLS** | Drives Chrome's own BoringSSL natively. Your ClientHello IS Chrome's ClientHello — fingerprint is emergent from the real engine, not a faked table that rots. |
| ⏱️ **Temporal stealth** | TLS session resumption, conditional revalidation (304), persistent cookies, connection pooling. The loudest remaining bot tell — and nobody else fakes it. |
| 👻 **Solve-and-bounce** | Browser solves the challenge, hands cookies to tier 1, goes to sleep. Tier 1 fetches at full speed. The browser almost never fetches content. |
| 🧠 **Self-improving fetch** | Learns from every fetch. Cookie lifetimes learned adaptively. Warm starts skip the browser entirely. Converges to optimal routing per domain. |
| 🔎 **Semantic reranking** | Local ONNX cross-encoder reads query + result through full attention. Pushes out generic articles that keyword-match but aren't about the topic. |
| 🔑 **Keyless search** | 10+ backends in parallel, fused by cross-engine consensus. No API keys, no accounts, no billing. $0 forever. BYOK optional. |
| 📄 **Pixel-fusion PDF** | Glyphs + rendered pixels from the same stream, fused deterministically. No hallucination. Per-region trust audit. Scanned PDFs auto-OCR'd. |
| 🧬 **Built from scratch** | Own HTTP/2 (HPACK, flow control), own extraction engine, own PDF parser, own search aggregator, own crawl engine. Zero dependency on existing OSS web tooling. |
| 🪶 **~3.5k tokens** | Three tools, ~3.5k tokens total in the MCP context. No bloat, no redundancy, every token earns its place. |

---

## 🆕 v3 — the agent-first upgrade

Four things no free (or paid) competitor has, plus a stack of agent-first mechanics:

| | What it does |
|---|---|
| 🔗 **Reference handles** | Fetched-page links render as `[text](L12)`, search results as `S1…Sn` — and `fetch S3` just works. URLs cost 80 tokens a piece; handles cost 3. Raw URLs stay in `structuredContent` for citation. |
| 🧾 **Probe mode** | `must_contain: "CVE-2026-1234"` verifies a claim against the FULLY-fetched page but returns MATCH/NO-MATCH + ≤3 excerpts (~60 tokens instead of 4k). Verification questions stop paying reading prices. |
| ♻️ **Resurrection fetch** | Dead link? `archive=auto` transparently serves the nearest Wayback snapshot, labeled `ARCHIVED COPY — 2021-04-03 (5 years old)`. Dead ends become honest answers. |
| 🕵️ **Anti-cloak check** | On domains known to serve decoys, tier-1 responses are equivalence-checked against a headless render — `decoy suspected` is stamped, never silently passed off as content. |
| 📌 **Page memory** | Every fetch is fingerprinted; re-fetches report `changed (minor/changed/rewritten)` with section-level diffs. `since_last=true` collapses a re-check to one line (~30 tokens). Delta crawls skip unchanged pages. |
| 🧠 **Domain intelligence** | Reddit `.json`, npm/PyPI/crates.io/Go/RubyGems, GitHub issues/releases, Stack Overflow QA trees, Wikipedia infoboxes, docs-site outlines — restructured from each site's own keyless endpoints/DOM, honestly labeled `via=adapter:…`, kill-switchable, always falling back to the generic pipeline. |
| ⏱️ **The clock** | `deadline_ms` on fetch/search, real MCP cancellation, progress notifications per crawl page, and an `ms` cost footer on every result. No operation can silently hang. |
| 🧵 **Article stitching** | `stitch=true` walks `rel=next` and returns an 8-part article as ONE call with part markers. |
| ⚡ **Warm handoff** | Search enrichment pre-fetches the top results; your next `fetch S1` serves from that cache — the search→fetch second hop runs in ~3ms. |
| 🛡️ **h2 parity, gated** | The HTTP/2 preface (SETTINGS, window update, header order) is asserted byte-identical to Chromium in CI. Detectability regressions are build failures. |
| 🧯 **Crash-only daemon** | `donsetch mcp --supervised` — a panic is a blip: the daemon restarts, state reloads, the session survives (SIGKILL-verified). |
| 🧾 **Error codes** | Every error carries a stable machine code (`wall.challenge`, `guard.ssrf`, `deadline.hit`, `archive.stale`, …) — branch on codes, not prose. |

---

## 🎬 Demo

<div align="center">

<video src="https://github.com/user-attachments/assets/32bc0899-87bf-417b-8ca8-c0a4a51ee167" controls muted width="640"></video>

</div>

*(30-second walkthrough: search, fetch with bot-wall bypass, and crawl)*

<div align="center">

<video src="https://github.com/user-attachments/assets/f164b31e-96ef-4294-b2dd-6777642098dc" controls muted width="640"></video>

</div>

*(Pi agent session: live search, fetch, and crawl with DonSeTch as a native extension)*

---

## 📦 Install

### Option 1 — npm (recommended)

```bash
npm install -g donsetch
```

Downloads the prebuilt binary for your platform from GitHub Releases with SHA256 verification. No build tools needed.

| Platform | Binary |
|---|---|
| Linux x86_64 | `donsetch-linux-x64.tar.gz` |
| Linux arm64 | `donsetch-linux-arm64.tar.gz` |
| macOS arm64 | `donsetch-darwin-arm64.tar.gz` |
| Windows x86_64 | `donsetch-win32-x64.tar.gz` |
| Termux (Android) | Build from source (see build notes) |

### Option 2 — Homebrew (macOS / Linux)

```bash
brew tap dondai44423/donsetch
brew install donsetch
```

Installs the same official release binaries.

### Option 3 — Pi agent (native extension)

```bash
pi install npm:donsetch
```

Installs DonSeTch as a native pi extension. The donsetch MCP binary spawns at session start, discovers its 3 tools dynamically, and registers them as native pi tools — no MCP adapter, no proxy, no config. Tools stay in sync with the binary automatically (zero maintenance). If the binary is missing, the extension auto-downloads it from GitHub Releases.

Update with `pi update --extensions` — both the binary and extension update together.

### Option 4 — Build from source

| Dependency | Why | Linux | macOS | Windows |
|---|---|---|---|---|
| **Rust** | Build toolchain | `rustup` | `rustup` | `rustup` |
| **Go** | BoringSSL build | `pacman -S go` / `apt install golang-go` | `brew install go` | `winget install GoLang.Go` |
| **NASM** | BoringSSL assembly | `pacman -S nasm` / `apt install nasm` | `brew install nasm` | `choco install nasm` |
| **CMake** | BoringSSL build | `pacman -S cmake` / `apt install cmake` | `brew install cmake` | `winget install cmake` |
| **Clang** | bindgen (boring-sys) | `apt install clang libclang-dev` | *(bundled on macOS)* | `choco install llvm` |
| **LLD** | PDFium link (aarch64) | `apt install lld` *(aarch64 only)* | — | — |

```bash
git clone https://github.com/dondai44423/donsetch.git
cd donsetch
cargo build --release
```

Binary lands at `target/release/donsetch`. First build takes ~2 min (compiling BoringSSL). Subsequent builds are cached.

<details>
<summary><b>Build notes</b></summary>

- **BoringSSL** is vendored and built from source via `boring-sys`. First build compiles it (~2 min), then cached.
- **PDFium** is downloaded as a static library by `build.rs` — no manual setup.
- **ONNX Runtime** is downloaded at build time by `oar-ocr` (OCR) and `ort` (reranker) when those features are enabled.
- **Models** (OCR + reranker) download on first use to `~/.cache/donsetch/`, not bundled in the binary.
- **Feature flags**: `default = []` — the core tool (fetch, search, crawl, PDF) works standalone. Build with `--features ocr,rerank` for OCR + semantic reranking (pulls in ONNX Runtime). The prebuilt npm binary ships with both features enabled.
- **Chromium** (optional): needed for tier 2 browser escalation on bot-walled sites. Linux: `pacman -S chromium` / `apt install chromium-browser`. macOS: `brew install chromium`. Windows: Edge works. **Playwright**: if you already have `npx playwright install`, DonSeTch auto-discovers `~/.cache/ms-playwright/chromium-*/chrome-linux/chrome` — no manual `DONGHOST_CHROME` needed. **Ubuntu Snap**: set `DONGHOST_CHROME=/snap/chromium/current/usr/lib/chromium-browser/chrome` — the `/snap/bin/chromium` wrapper doesn't reliably pass CDP flags.
- **Linux Xvfb**: for headful Chrome on Linux, `xorg-server-xvfb` is needed (`apt install xvfb`). DonSeTch starts Xvfb automatically on `:99`. If your distro uses a regional Ubuntu Ports mirror that is down, fix it: `sudo sed -i 's|http://.*\.clouds\.ports\.ubuntu\.com|http://ports.ubuntu.com|' /etc/apt/sources.list.d/ubuntu.sources && sudo apt-get update`.
- **Linux ARM64 (aarch64)**: the default build (no features) works out of the box. Requires `lld` + `clang libclang-dev` for PDFium + boring-sys: `apt install lld clang libclang-dev` (fix mirror first if needed, see above). If you enable `--features ocr,rerank`, ONNX Runtime's C++ global constructors may deadlock at startup on aarch64.
- **AppArmor / sandbox** (Ubuntu 23.10+): unprivileged user namespaces are disabled by default, so Chromium fails with `No usable sandbox!`. DonSeTch now passes `--no-sandbox --disable-setuid-sandbox` automatically — no manual fix needed.
- **Termux (Android)**: `pkg install rust clang make pkg-config go lld && cargo build --release`. Chromium: `pkg install x11-repo && pkg install chromium`. DonSeTch auto-detects Termux and uses headless mode (no Xvfb needed). **boring-sys NDK workaround**: boring-sys's build script panics on Android targets without `ANDROID_NDK_HOME`. Run `export ANDROID_NDK_HOME=$PREFIX` before `cargo build` to satisfy the check (Termux IS the native Android environment, its toolchain lives in `$PREFIX`). PDFium uses bblanchon's Android shared library (`libpdfium.so`), not the glibc-targeted static archive.

</details>

### Option 5 — Docker

For isolated deployment, a consistent runtime environment, and easy updates:

```bash
git clone https://github.com/dondai44423/donsetch.git
cd donsetch
docker compose build
```

**Optional build choice — Chromium.** The default image excludes
Chromium; tier 2 browser escalation only activates when a browser is
configured. To bake Chromium in (~+350MB) for tier 2 bot-wall bypass:

```bash
docker build --build-arg INSTALL_CHROME=true -t donsetch-mcp .
```

Then point the server at it at runtime with
`-e DONGHOST_CHROME=/usr/bin/chromium` (the bundled compose file ships
this as a commented entry).

**stdio (default transport).** MCP clients launch the server as a
subprocess:

```bash
docker run -i --rm --init donsetch-mcp donsetch mcp --supervised
```

stdio MCP client config (Docker flavor):

```json
{
  "mcpServers": {
    "donsetch": {
      "command": "docker",
      "args": ["run", "-i", "--rm", "--init", "donsetch-mcp", "donsetch", "mcp", "--supervised"]
    }
  }
}
```

OpenCode uses a stricter MCP schema — a `type` discriminator, a single
`command` array, and an explicit `enabled` — in
`~/.config/opencode/opencode.json`:

```json
{
  "mcp": {
    "donsetch": {
      "type": "local",
      "command": ["docker", "run", "-i", "--rm", "--init", "donsetch-mcp", "donsetch", "mcp", "--supervised"],
      "enabled": true,
      "timeout": 120000
    }
  }
}
```

`timeout` is optional but recommended: OpenCode defaults MCP requests
to 5 seconds, and tier 1 fetches regularly run longer.

Or via the bundled compose service (adds the persistent cache volume
and resource limits):

```bash
docker compose run --rm donsetch
```

**Docker Compose options.** The bundled `docker-compose.yml`:

- A cache volume persisting fetch/search state across restarts.
- A 2GB memory ceiling (OCR + reranking peak around 1–2GB under heavy
  crawls) and an init process to reap zombies.
- A 45-second stop grace period so in-flight tier-2 fetches finish on
  `docker compose stop`.

**Architecture notes.** Multi-stage build (`rust:slim` →
`debian:trixie-slim`, kept on the same glibc generation — ort-sys's
prebuilt ONNX Runtime needs glibc 2.38+ symbols on both sides), all
features enabled, PDFium acquired at build time by the repo's own
`build.rs` (sha256-verified), Go installed per-target-arch for
BoringSSL's build system (amd64 and arm64). Single ~36MB binary in a
minimal runtime image — no Python, no Playwright.

---

## 🔀 HTTP Proxy

DonSeTch respects standard proxy environment variables, following the curl/wget convention:

```bash
# All HTTPS traffic through a proxy
export HTTPS_PROXY=http://proxy.example.com:8080

# All HTTP traffic through a proxy
export HTTP_PROXY=http://proxy.example.com:8080

# Both HTTP and HTTPS (fallback when scheme-specific var is absent)
export ALL_PROXY=socks5://proxy.example.com:1080

# Bypass proxy for specific hosts (comma-separated, suffix match)
export NO_PROXY=localhost,127.0.0.1,.internal.example.com

# Bypass proxy for everything
export NO_PROXY=*
```

Both HTTP CONNECT and SOCKS5 proxies are supported. Credentials in the URL (`http://user:pass@host:port`) are honored. The Ghost browser (tier 2) also routes through the proxy via Chrome's `--proxy-server` flag.

---

## 🎯 Two ways to use it

DonSeTch is **one binary, two interfaces**. Same engine, same output, same reliability.

### MCP Server (for AI agents)

Point your AI agent at the binary. No arguments, no API keys, no environment variables.

```json
{
  "mcpServers": {
    "donsetch": { "command": "donsetch", "args": ["mcp"] }
  }
}
```

Or use `npx` without global install:

```json
{
  "mcpServers": {
    "donsetch": { "command": "npx", "args": ["donsetch", "mcp"] }
  }
}
```

Works with Claude Code, Cursor, OpenCode, Pi, Windsurf, and any client that speaks MCP. Three tools: `web_fetch`, `web_search`, `web_crawl`.

### CLI (for humans and scripts)

```bash
donsetch fetch https://example.com
donsetch search "rust async patterns"
donsetch crawl https://docs.example.com --topic "api reference"
```

Same engine, same output — the CLI is a thin adapter over the same `call_tool` function the MCP server uses. Every feature available to the agent is available on the command line.

```bash
donsetch fetch https://example.com --focus "pricing" --max-chars 2000
donsetch search "GLM 5.2" --intent paper --max-results 5 --json
donsetch crawl https://docs.python.org --mode map --topic "asyncio"
donsetch keys add tinyfish sk-tinyfish-...
donsetch doctor
donsetch update
```

<details>
<summary><b>All CLI commands</b></summary>

| Command | What it does |
|---|---|
| `fetch <url>` | Fetch a URL as clean markdown. Auto bot-wall bypass, PDF, JS render. |
| `search <query>` | Web search — 10+ keyless engines merged + reranked, or your API keys. |
| `crawl <url>` | Crawl a site into markdown. Sitemap-aware, focus-ranked, resumable. |
| `mcp` | Start MCP server (JSON-RPC on stdio). |
| `keys` | Manage BYOK search provider keys (`add`, `remove`, `list`, `default`, `reset`, `export`, `import`, `clear`). |
| `proxy` | Manage proxy configuration (`add`, `remove`, `list`, `check`, `clear`, `test`, `import`, `export`). |
| `status` | Quick status overview — version, keys, proxies, cache, health. |
| `doctor` | Health check with auto-fix. |
| `update` | Self-update from GitHub Releases. |
| `rollback` | Revert to previous version. |
| `version` | Show version and build info. |
| `tools` | Print tool schemas as JSON (same as MCP `tools/list`). |

</details>

---

## 🎯 The 3 tools

| Tool | One-liner |
|------|-----------|
| 🌐 **`web_fetch`** | Fetch any URL as clean markdown. HTTP first, escalates to headless browser if blocked. PDFs with OCR + per-page confidence, `focus` for token savings, `toc`/`section`, pagination, `actions` for in-page browser control (click/type/press/scroll/wait). |
| 🔎 **`web_search`** | Keyless multi-engine web search. 10+ backends in parallel, consensus + semantic reranking, query-aware official-source placement. Returns URLs + snippets, not content. |
| 🕷️ **`web_crawl`** | Best-first same-domain crawl. Sitemap + frontier, elastic pacing, resume tokens. `focus` for budget management. |

---

## 🖱️ Browser actions — page control inside fetch (v2)

`web_fetch` accepts an `actions` array executed in the real headless browser **before** extraction:

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

Steps: `wait`, `wait_selector`, `wait_text` (deterministic waits — no blind sleeps), `click` (by CSS selector or visible text), `hover`, `type` (human-cadence keystrokes), `press`, `scroll`. Up to 16 steps. After the script runs, the normal extraction pipeline works on the final DOM — `focus`, `section`, `toc` all apply to the interacted-with page. Per-step results come back in `structuredContent.actions`; the first failing step aborts honestly with everything that succeeded, so you fix one step and re-run. Form submits, search flows, load-more buttons, lazy-load scrolls — one call, no separate browser tool.

---

## 🛡️ Chrome TLS, not Chrome-like

Everyone in the impersonation game (curl-impersonate, rquest, wreq) patches a foreign TLS stack to *resemble* Chrome and ships hardcoded fingerprint tables that rot as browsers evolve.

DonSeTch does something different in kind:

> **We drive Chrome's own TLS library (BoringSSL) with its native Chrome behaviors switched on** — GREASE, extension permutation, ECH-GREASE, ALPS, SCT, OCSP, brotli cert-compression. The ClientHello is *generated by the same machinery* that generates Chrome's.

When Chrome's TLS posture shifts, we update a data table, not patch a C library. **The fingerprint isn't a table we fake. It's emergent from the real engine.**

<details>
<summary><b>Verified against live Chromium 150 at tls.peet.ws</b></summary>

| Signal | Match |
|---|---|
| **JA4** | cipher hash identical to Chrome 150 |
| **Akamai h2 fingerprint** | `1:65536;2:0;4:6291456;6:262144\|15663105\|0\|m,a,s,p` — exact match |
| **h2 header order** | sec-ch-ua → sec-ch-ua-mobile → sec-ch-ua-platform → ... — exact match |
| **Extension set** | identical (contents differ only in random GREASE/key material, like real Chrome) |

</details>

### Own HTTP/2 stack (because off-the-shelf leaks)

Off-the-shelf h2 (hyper's `h2` crate) doesn't expose pseudo-header order, exact SETTINGS set, WINDOW_UPDATE values, or HPACK indexing strategy — all fingerprintable (Akamai h2 fingerprint). So we wrote our own:

- Own HPACK (RFC 7541 — all 257 Huffman symbols + 61 static entries verified)
- Own frame engine (SETTINGS, HEADERS, DATA, WINDOW_UPDATE, PING, GOAWAY, RST_STREAM, CONTINUATION)
- Own flow control with WINDOW_UPDATE replenishment
- Own connection pool with TLS 1.3 session resumption

**No `reqwest`. No `hyper`. No `isahc`.** Every byte on the wire is ours.

### Temporal stealth (the tell nobody else fakes)

Everyone fakes the *handshake*. But a bot wall's second look is **temporal**: what does the client do *over time*?

| Mechanism | What it does | Why it matters |
|---|---|---|
| TLS session resumption | Per-origin session-ticket cache. Resumed handshakes are abbreviated. | Scrapers never resume. Chrome always does. |
| h2 connection pool | Connections kept alive, reused across fetches. | Fresh connection every time = bot signal. |
| Conditional revalidation | Sends `If-None-Match` / `If-Modified-Since` on refetch. `304` → serve cached body. | Scrapers never send conditional headers. Browsers always do. |
| Happy Eyeballs | Races IPv6 vs IPv4 with 250ms stagger. | Chrome does exactly this. Fixes dead-IP 10s timeouts. |
| Persistent cookie jar | Cookies survive across calls like a real browser profile. | A client with no cookie memory is a bot. |

**More stealth AND faster.** The rare quadrant.

---

## 👻 Solve-and-bounce

The entire industry does tier 2 wrong: **use a 600MB browser for everything.** Every fetch pays browser startup, browser RAM, browser CPU. And their stealth is *subtractive*: launch with automation flags, then inject JS patches to hide the damage. Every patch is a detectable lie.

DonSeTch inverts both.

> **The browser almost never fetches content.** It exists to do exactly two things HTTP can't: pass JS challenges and execute JS-rendered pages. Its output is *cookies* (handed to tier 1, which fetches at full speed) or *rendered HTML* (handed to the extraction engine).

| Step | What happens | Speed |
|---|---|---|
| 1. Tier 1 fetch | Fast stealth HTTP (BoringSSL TLS) | ~100-300ms |
| 2. Wall detected | Cloudflare / DataDome / PerimeterX / Akamai | — |
| 3. Ghost solves | Headless browser navigates, waits for challenge to clear, harvests clearance cookies | ~2-6s |
| 4. **Bounce** | Cookies handed to tier 1. Tier 1 re-fetches at full speed. Browser goes to sleep. | ~100-300ms |
| 5. Subsequent fetches | Tier 1 with warm cookies. Browser stays asleep. | ~100-300ms |

### Nothing is patched because nothing is broken

Raw CDP launch without `--enable-automation` means `navigator.webdriver` is *natively* false. No JS injection ever. No Runtime, Console, or Debugger domains. Environmental truthfulness instead of spoofing: real window, real GPU, real locale, consistent story.

<details>
<summary><b>Process lifecycle — the RAM-smart part</b></summary>

The ghost process is **SIGSTOP'd** (frozen, not killed) after 20s of idleness:

| State | RAM | CPU | Wake time |
|---|---|---|---|
| Active | full | real | — |
| Frozen (SIGSTOP) | mapped but cold | 0 | ~50ms (SIGCONT) |
| Reaped (>10 min frozen) | freed | 0 | ~1-2s (relaunch, profile keeps warmth) |

Crash-transparent: thaw finds a dead browser → silent relaunch. The agent never sees the lifecycle.

Persistent profile dir `~/.cache/donsetch/ghost-profile`: aged cookie/history state makes challenges EASIER (real users have history) and `cf_clearance` survives daemon restarts.

</details>

---

## 🧠 Self-improving fetch (experimental)

> **Experimental.** Works reliably for most domains, but edge cases exist (stale profiles, cookie race conditions). Disk persistence can be disabled with `DONSEEK_NO_DISK_STATE=1`.

> Every fetch is both an action AND an observation. The system learns from each outcome and routes the next fetch more efficiently. **The more you use DonSeTch, the less it escalates to tier 2.**

No ML, no prediction models. Pure deterministic observation + state → better routing. The loop converges, it doesn't guess.

| Visit | Route decision | What happens |
|---|---|---|
| Visit 1 (unknown) | `Cold` | Tier 1 → walled → ghost solves → cookies stored |
| Visit 2 (cookies fresh) | `Warm` | Tier 1 with injected cookies → success. Ghost stays asleep. |
| Visit N (cookies expired) | `SkipToSolve` | Skip the doomed tier-1 round-trip, go straight to ghost |
| Visit M (24h since cold check) | `RecheckCold` | Try tier 1 cold — the wall may have been removed |

When warm cookies go stale, the system learns the real lifetime: `observed_lifetime = min(previous, now - last_solved)`. Over multiple cycles, it converges to the real cookie lifetime for each domain.

**Only clearance cookies are persisted** (cf_clearance, datadome, _abck, etc.) — tracking cookies are filtered out to keep the state file compact.

**Disable disk persistence** with `DONSEEK_NO_DISK_STATE=1` — keeps in-memory state for the session but skips writing to `~/.cache/donsetch/ghost-state.json`. On by default.

---

<div align="center">

<img src="assets/owlsearch.png" alt="DonSeTch Search">

</div>

## 🔎 Keyless search

No API key, no account, no third-party service. 10+ keyless backends in parallel on your machine, merged, deduped, ranked.

- **10+ independent backends**: Brave, Bing, DuckDuckGo, Mojeek, Yandex, Startpage + keyless verticals (GitHub, Wikipedia, HN, Semantic Scholar, arXiv, StackExchange, MDN, Google News).
- **Semantic reranking**: local ONNX cross-encoder (`ms-marco-MiniLM-L-6-v2`, 23MB, Apache-2.0) reads query + title + snippet through full transformer attention. 60/40 blend with RRF + BM25 + consensus. ~5ms/pair on CPU.
- **Cross-engine consensus**: a URL returned by several independent indexes gets a consensus boost. Every result carries `score`, `consensus` count, and `engines` list.
- **Entity coverage penalty**: anchor entities (hyphenated compounds like "B-tree") and specifiers (version numbers, years) checked against results. Wrong entity → 0.3× score penalty. Fixes BM25 splitting "B-tree" → "b" + "tree" where "binary tree" matches.
- **Honest reporting**: `weak=true` means low consensus. Per-engine status always visible. Never a fake "no results" that's actually a rate limit.

### Benchmark: keyless search quality

110 questions across 11 niches (science, history, technology, geography, sports, entertainment, health, business, niche/obscure, programming, arts). Keyless backends only, no API keys, 10 free rotating residential proxies.

| | **DonSeTch keyless** | **Tavily** (published) |
|---|---|---|
| **Accuracy** | **95.5%** (105/110) | 93.3% |
| **Cost** | $0 (no API keys) | paid API |

8 of 11 niches scored 100%. The keyless engine is not a weak fallback, it's the primary path and it competes with paid APIs on quality. BYOK exists for rate limits at scale, not because keyless can't compete.

<details><summary>Per-niche breakdown</summary>

| Niche | Questions | Correct | Accuracy |
|---|---|---|---|
| Science & Nature | 10 | 10 | 100% |
| History | 10 | 10 | 100% |
| Technology | 10 | 9 | 90% |
| Geography | 10 | 10 | 100% |
| Sports | 10 | 10 | 100% |
| Entertainment | 10 | 10 | 100% |
| Health & Medicine | 10 | 10 | 100% |
| Business & Finance | 10 | 10 | 100% |
| Niche & Obscure | 10 | 9 | 90% |
| Programming & Dev | 10 | 8 | 80% |
| Arts & Literature | 10 | 9 | 90% |

</details>

<details><summary>Methodology and caveats</summary>

**Our metric**: answer-in-snippet. Does the expected answer text appear in the top-5 search result titles or snippets? This is a necessary condition for any LLM to answer correctly from retrieved docs, so it's a lower bound on end-to-end accuracy.

**Tavily's metric**: end-to-end LLM accuracy. GPT-4.1 reads the retrieved documents, answers using only them (no parametric knowledge), then OpenAI's correctness prompt grades it CORRECT / INCORRECT / NOT_ATTEMPTED. Tavily also reports hallucination and not-attempted rates.

These are different bars. Our snippet-recall test is easier: finding "Canberra" in a snippet doesn't mean an LLM would correctly answer "What is the capital of Australia?" if the snippet is on a page about cricket. A full apples-to-apples comparison would require running the same LLM grading step.

Our benchmark is also smaller (110 hand-curated questions vs SimpleQA's 4,326) and uses straightforward factual queries rather than SimpleQA's deliberately hard, adversarial set. So 95.5% snippet-recall vs 93.3% LLM-graded is not a direct comparison. What it does show: the keyless engine returns the right information for the vast majority of queries, at zero cost.

</details>

Reproduce: `python3 bench/search_quality.py --verbose`

> **Take these numbers with a grain of salt.** This is a small benchmark (110 questions) with an easier metric (answer-in-snippet, not LLM-graded). Tavily ran 4,326 questions through GPT-4.1 with OpenAI's grader. Running the same scale of benchmark on DonSeTch requires reliable rotating proxies (free ones die fast) and an LLM API key for grading, both of which are the hard part. The keyless search is still genuinely good at finding the right information. Anyone is free to run a deeper, more realistic benchmark and report their own numbers. The script is in the repo.

### BYOK (Bring Your Own Keys) — Pro Search, No Vendor Lock-in

The local engine is powerful, but paid providers give you higher rate limits, premium data sources, and managed infrastructure. DonSeTch makes BYOK a first-class feature, not an afterthought:

- **Multi-provider, multi-key stacking** — add as many keys as you want, even for the same provider. Two Exa keys with 1,500 credits each? You now have 3,000 credits in a single pool. DonSeTch rotates across them automatically — when one hits its rate limit or runs dry, it falls through to the next. No manual switching.
- **Smart fallback** — if a provider exhausts all its keys or errors out, DonSeTch falls back to the local keyless engine. You never get a dead search. Set the order yourself: BYOK-first or local-first.
- **Key rotation** — every key tracks its own rate-limit cooldown and credit-depletion state. A throttled key is skipped until it recovers; a depleted key is retired. All automatic.
- **Portable key store** — export, import, transfer, or wipe your keys. Move between machines in one command.

<div align="center">

<img src="assets/byok-keys.png" alt="DonSeTch keys list with configured providers" width="640">

</div>

```bash
# Add keys — stack as many as you want per provider
donsetch keys add exa sk-exa-...         # 1,500 credits
donsetch keys add exa sk-exa-...         # another 1,500 -> 3,000 total
donsetch keys add tavily tvly-...
donsetch keys add serper ...
donsetch keys add tinyfish sk-tinyfish-...
donsetch keys add parallel nKil3...      # Parallel AI (fast mode)
donsetch keys add bd 576d013c...        # Bright Data SERP (bd = alias)

# See what's configured (keys are masked)
donsetch keys list

# Set dispatch order — which engine tries first?
donsetch keys default local       # local keyless first, BYOK fallback
donsetch keys default exa          # BYOK first, local fallback

# Back up, transfer, or reset
donsetch keys export ~/donsetch-keys.json
donsetch keys import ~/donsetch-keys.json
donsetch keys clear
```

Providers: **TinyFish** (free tier), **Tavily**, **Serper.dev**, **Exa**, **Parallel AI** (fast mode), **Bright Data** SERP (`bd` alias).

---

<div align="center">

<img src="assets/fetch.png" alt="DonSeTch Fetch">

</div>

## 🌐 Fetch

`fetch` tries plain HTTP first (~100-300ms). If the site serves a bot wall or a JS shell, it auto-escalates to the ghost browser, solves the challenge, bounces cookies back to tier 1, and re-fetches at full speed.

- **DonSift extraction engine**: HTML bytes in, agent-native markdown out. Block model: typed blocks (Heading/Para/List/Table/Code/Quote/Media) with heading breadcrumbs.
- **`focus`** — BM25-relevant blocks only. Cuts context 80%+ on long pages. 12-language BM25: CJK character unigrams + bigrams, stopword lists, light stemming, accent folding.
- **`toc` + `section`** — heading outline first, then target one section. Two cheap calls instead of one expensive one.
- **Pagination** — `next_offset` in the response. Call again with `offset=that value`.
- **Token-war policies** — links stripped by default (~30% savings), link-farm lists dropped, bare-link lines dropped, wiki `[edit]` junk dropped, cross-block duplicate suppression.
- **Content classification**: `Article` / `Listing` / `Forum` / `Docs` / `Table` / `Page`.
- **Quality score** (0.0-1.0): content density, metadata, structure, language, text volume.
- **Agent-trust signals inline** — focus-miss notice, section-miss notice, JS-shell warning, empty-content note — all in the content, not metadata.

<details>
<summary><b>Anti-bot benchmark</b></summary>

| Site | Protection | Status |
|---|---|---|
| Cloudflare-protected sites | Cloudflare interstitial | ✅ 200 OK |
| DataDome sites | DataDome | ✅ 200 OK |
| Stack Overflow | Cloudflare | ✅ 200 OK |
| Medium | Cloudflare | ✅ 200 OK |
| Reddit | bot detection | ✅ 200 OK |
| Hacker News | None (baseline) | ✅ 200 OK |
| Interactive captcha sites | hCaptcha / reCAPTCHA / Turnstile | ⛔ Honest block |

</details>

---

<div align="center">

<img src="assets/crawl.png" alt="DonSeTch Crawl">

</div>

## 🕷️ Crawl

`crawl` walks same-domain links in best-first order. Two phases: sitemap discovery (cheap URL inventory in one fetch), then Governor-paced frontier walk with extraction per page.

- **Three modes**: `full` (default) = sitemap map + content. `map` = URL inventory only (very cheap). `content` = skip sitemap, BFS from seed.
- **Focus-ranked frontier**: `focus="query"` ranks pages by BM25 relevance and crawls only matching ones.
- **Adaptive pacing**: the Governor paces per (host, lane). Success → steady. 429/503 → exponential. Error → cooldown. Dwell-time variance proportional to page size breaks metronome fingerprinting.
- **Resume tokens**: stopped crawls return a resume token. Call again with `resume=token` to continue. Valid 30 min, survives restarts.
- **Near-dup detection**: title + first 200 normalized chars → hash. Duplicates skipped.
- **Honest stop reasons**: `FrontierEmpty` (done), `MaxPages`/`CharBudget`/`DepthLimit`/`Deadline` (use resume), `ThrottledOut` (wait and resume).
- **Seed always in scope**: `--include`/`--exclude` apply to discovered links only, not the seed entry point.

---

## 📄 PDF + OCR

DonSeTch detects PDFs (by Content-Type or `%PDF` magic bytes) and parses them to structured markdown using a custom PDFium FFI. No external PDF library, no Python subprocess.

> **DonSeTch fuses both modalities, deterministically.** PDFium gives us both a document's glyph stream (exact text + positions) AND rendered pixels (exact visual ground truth). Both come from the same content stream, so they're already aligned.

**Pixels tell the truth about structure. Glyphs tell the truth about text. The fusion is exact because both render from one stream. No guessing, no hallucination.**

| Innovation | What it does |
|---|---|
| Pixel-fusion rule extraction | Tables/borders/separators detected on the rendered bitmap via morphological opening. A rule line is a fact, not a hypothesis. |
| Span detection by ink continuity | A cell spans a separator iff the separator has NO ink under the cell's row band. Deterministic colspan/rowspan. |
| Trust audit + per-region arbitration | Glyph stream authoritative UNLESS: zero glyphs + pixels (scan), or region ≥30% PUA/garbage. Corrupt regions get OCR'd from pixels even when neighbors read fine. |
| Orientation canonicalization | Vertical/rotated text = same document rotated. Rotate coordinate frames, run ONE pipeline. |
| Confidence honesty | Verbatim glyphs or OCR with per-line confidence. `[uncertain: ...]` markers below threshold. Neural extractors cannot offer this. |
| Forms as data | AcroForm widgets → name/type/value triples. |

| Tier | What | When |
|---|---|---|
| Tier A (always) | Glyph stream + pixel-fusion layout engine | Every page |
| Tier B (lazy) | OCR for pages/regions with no trustworthy text | Scans, broken ToUnicode |

<details>
<summary><b>PDF battle test results</b></summary>

40-document battle corpus, zero garbage output, 6-14x faster than Python alternatives. 120/120 fuzz clean.

| Document type | Result |
|---|---|
| Academic papers | ✅ Clean text — math symbols recovered, not CID garbage |
| Scanned documents | ✅ OCR'd — PP-OCR cascade (En → Zh → Deva), confidence-scored |
| Tax forms (W-9) | ✅ Forms as data — field names + values as table |
| Multi-column layouts | ✅ Reading order preserved — column detection + merge |
| Encrypted PDFs | ⛔ Honest flag — `encrypted: password required` |
| Corrupt PDFs | ⛔ Honest flag — `corrupt: parse failed at offset N` |
| Nepali UDHR (broken ToUnicode) | ✅ 10,542 usable Nepali chars at 86% confidence (pymupdf: 28) |

</details>

---

## 🏗️ Built from scratch

Every layer built in Rust. No dependency on existing OSS web tooling.

| Component | What it does | Key files |
|---|---|---|
| 🛡️ **DonShadow** | Tier 1 stealth HTTP — BoringSSL TLS, own HTTP/1.1 + HTTP/2, temporal stealth, cookie jar | `src/fetch/`, `src/transport/` |
| 👻 **DonGhost** | Tier 2 ghost browser — CDP (no Runtime/Console/Debugger), solve-and-bounce, SIGSTOP lifecycle | `src/ghost/` |
| 📝 **DonSift** | HTML-to-markdown — block model, 12-language BM25 focus, token-war policies | `src/extract/` |
| 🔎 **DonSeek** | Keyless multi-engine search — weighted RRF + BM25 + consensus + semantic reranking | `src/search/` |
| 🕷️ **DonTread** | Crawl engine — sitemap, focus-ranked frontier, Governor pacing, resume tokens | `src/crawl/` |
| 📄 **DonSheet** | PDF extraction — PDFium FFI, pixel-truth fusion, OCR arbitration cascade, forms | `src/pdf/` |
| 🔌 **MCP daemon** | stdio server — JSON-RPC 2.0, 3 tools | `src/mcp/` |

**637 tests. Zero clippy warnings.** `cargo clippy --all-targets --features ocr,rerank -- -Dwarnings` is the law.

---

## 🔬 WRB: Web Research Benchmark

DonSeTch was benchmarked with [WRB](https://github.com/dondai44423/wrb), a tool-level benchmark that tests fetch, search, and crawl operations directly. No LLM required, pure string matching. Any web tool can run it by implementing a thin runner adapter.

**48 fetch URLs across 3 difficulty tiers, 55 search queries across 11 niches, 5 crawl targets.**

### Fetch

| Metric | Result |
|---|---|
| Content retrieval | **95.8%** (46/48 URLs returned real content) |
| Tier 1 (no anti-bot) | 100% (19/19) |
| Tier 2 (mild protection) | 100% (16/16) |
| Tier 3 (aggressive anti-bot) | 84.6% (11/13) |
| Stealth score (tier-weighted) | 93.3% |
| Speed (median) | 772ms |
| Speed (P90) | 5,200ms |
| Token efficiency | 1,105 tokens/page |
| False positives | **0** (never claimed success on a bot wall) |

Tier 3 covers Cloudflare, Akamai, Datadome, and PerimeterX protected sites. The 2 misses (Etsy/Cloudflare, Target/Akamai) were honest failures: empty content, no false success claim.

### Search

| Metric | Result |
|---|---|
| Precision (answer found) | **96.4%** (53/55) |
| Recall (expected domain in top 5) | 81.8% (45/55) |
| Coverage | 100% (every query returned results) |
| Speed (median) | 1,356ms |
| Token efficiency | 802 tokens/query |

### Crawl

| Metric | Result |
|---|---|
| Precision (relevant pages) | 74.3% |
| Pages crawled | 67 across 5 targets |
| Coverage | Being refined (known-relevant URL sets updated) |

<details><summary>How WRB works</summary>

WRB tests the tool layer, not the agent layer. No LLM, no reasoning, no multi-step planning. Each fetch URL has a reference probe string that must appear in the returned content. Each search query has expected domains and answer snippets. Each crawl target has known-relevant URLs.

Metrics no other benchmark measures:
- **Honesty**: does the tool claim success when it actually got a bot wall? WRB tracks false positives. DonSeTch scored 0.
- **Tier-weighted stealth**: getting past Cloudflare counts more than getting past Wikipedia. Easy sites don't inflate the score.
- **Token efficiency at the tool level**: context window is the bottleneck for agents. Less tokens per result = more room for reasoning.

Run it yourself: `git clone https://github.com/dondai44423/wrb && python3 lib/wrb.py donsetch --verbose`

</details>

---

## 📊 Comparison

| | **DonSeTch** | Hound | Crawl4AI | Jina Reader | Firecrawl |
|---|---|---|---|---|---|
| **Language** | Rust | Python | Python | Python (API) | TypeScript |
| **TLS fingerprint** | Real Chrome (BoringSSL) | curl-impersonate | requests | their servers | their servers |
| **HTTP/2 stack** | Own (HPACK, flow control) | primp | requests | their servers | their servers |
| **Temporal stealth** | ✅ | ❌ | ❌ | ❌ | ❌ |
| **Tier 2 strategy** | Solve-and-bounce | Browser fetches all | n/a | n/a | n/a |
| **Self-improving** | ✅ | ❌ | ❌ | ❌ | ❌ |
| **Web search** | ✅ (keyless, 10+) | ✅ (keyless, 10) | ❌ | ✅ | ❌ |
| **Semantic reranking** | ✅ (local ONNX) | ✅ | ❌ | ❌ | ❌ |
| **Deep crawl** | ✅ (resume tokens) | ✅ | ✅ | ❌ | ✅ (cloud) |
| **PDF → markdown** | ✅ (pixel-fusion) | ✅ (pdfplumber) | partial | ✅ | ✅ (cloud) |
| **Scanned-PDF OCR** | ✅ (PP-OCR) | ✅ (rapidocr) | ❌ | ❌ | ✅ (paid) |
| **Query focus** | ✅ (12-language BM25) | ✅ | ✅ | ❌ | ❌ |
| **CLI** | ✅ | ❌ | ✅ | ❌ | ❌ |
| **Runs locally** | ✅ | ✅ | ✅ | ❌ | self-host |
| **MCP server** | ✅ | ✅ | community | ✅ | build it |
| **Token cost** | ~3.5K (3 tools) | ~2.7K (6 tools) | varies | n/a | varies |
| **License** | AGPL v3 | MIT | Apache 2.0 | proprietary | MIT |

---

## 🆚 DonSeTch vs wigolo

Both are local-first web tools for AI agents. Both are keyless and open source. The similarities end there.

| | **DonSeTch** | **wigolo** |
|---|---|---|
| **Install** | `npm install -g donsetch` — done | `npx wigolo init` — downloads ~1.5 GB (ML models + browser), then config wizard |
| **Startup time** | 0.3s | ~13s cold start (ML model + browser + module initialization) |
| **Search latency** | 6.1s avg | 23.5s avg (fetches page content during search for ML reranking) |
| **Search engines** | 5 keyless + intent verticals (GitHub, HN, Scholar, StackExchange, MDN) — 5-8 effective per query | 18 adapters total; 4-5 in default keyless mode (Bing, DDG, Wikipedia, Marginalia, Mojeek) |
| **Bot-wall bypass** | Auto-escalation with Chrome-true TLS (BoringSSL) — bypasses Cloudflare on StackOverflow, Amazon, BBC, Guardian | TLS impersonation + stealth browser mode exist, but in testing: blocked on StackOverflow, timed out on Reddit (120s) |
| **Crawl** | Semantic topic filter, sitemap discovery, per-page quality scoring, resume tokens | URL pattern filtering (regex), sitemap strategy, but no semantic topic ranking, no quality scores, no resume |
| **Token efficiency** | 5.7 KB per search, `focus` cuts fetch tokens 50-80% | 11.4 KB per search, 40% is scoring metadata |
| **Dependencies** | 0 npm packages. Single 36 MB Rust binary | 28 npm packages, Playwright browser, ML models, SQLite cache (~1.5 GB total per wigolo docs) |
| **Setup steps** | 1 (`npm install -g donsetch`) | 1-2 (`npx wigolo init` downloads ~1.5 GB, then optional agent wiring + LLM env vars) |
| **Config** | Zero. Works immediately | Init wizard, env vars for LLM provider, optional hybrid search config |
| **Pi agent** | `pi install npm:donsetch` — native extension, zero config | Not available |
| **Fetch success rate** | 87.5% (14/16 tested sites) | 67% (10/15 tested sites) |
| **Cold-start failure points** | Binary, search engines | ML model, Playwright, SQLite, 28-package dependency tree |

DonSeTch is install and use. wigolo is install, download ~1.5 GB of models and browsers, configure, then wait ~13s on every cold start.

---

## 🆚 DonSeTch vs Firecrawl (live head-to-head)

Both CLIs run live, head-to-head, on identical real-world tasks. Firecrawl is the paid cloud API (not the self-host OSS version). DonSeTch is free, local, keyless.

### Fetch / Scrape

| URL | Firecrawl (paid cloud) | DonSeTch (free local) |
|---|---|---|
| LinkedIn | ❌ "we do not support this site" | ✅ real job listings |
| Reddit | ❌ "we do not support this site" | ✅ real feed content |
| Stack Overflow (Cloudflare) | ✅ 4.7s, verbose | ✅ 7s, clean Q&A (847 tokens) |
| Wikipedia (Transformer) | 267KB | 16KB (**16x smaller**) |
| arXiv PDF | 32.6s, 71KB | 1.4s, 16KB (**22x faster, 4.4x smaller**) |

Firecrawl explicitly refuses LinkedIn and Reddit. DonSeTch fetches both. On Wikipedia, DonSeTch returns 16x fewer tokens. On PDFs, 22x faster.

### Search

| | Firecrawl | DonSeTch |
|---|---|---|
| Speed | 1-2s | 5-7s |
| Result style | Full scraped article content inline | Clean ranked snippets |
| Code specificity | Good | Matched or beat (found exact GitHub issues) |
| Academic | arXiv + NeurIPS + Wikipedia | ar5iv + arXiv + Wikipedia + NeurIPS |
| News/mainstream | Better (NBC, MKBHD, Reddit discussions) | Niche tech blogs, less mainstream authority |
| Token cost per query | High (full articles for 5 sites) | Low (snippets only, fetch what you need) |

Search quality is close. Firecrawl is faster and leans mainstream authority. DonSeTch leans technical specificity. Firecrawl's "return full articles" model is a token liability for agents: you get 5 sites' full content when you needed one snippet.

### Crawl

| | Firecrawl | DonSeTch |
|---|---|---|
| Crawl speed (fastapi docs, topic: DI) | 47.6s | 18.7s (**2.5x faster**) |
| Found target pages? | No | No |
| Failure behavior | Dumped verbose unrelated content (security, path-params, testing) | Returned little, fast, with honest low quality scores (0.22-0.30) |
| Focus filter | None (firehose by design) | `--topic` ranks and filters by relevance |
| No-sitemap URL discovery | ✅ `map` works well | Needs `--mode content` fallback |

Both missed the dependency-injection pages. The difference is how: DonSeTch failed fast, small, and honestly (quality scores admitted low relevance). Firecrawl spent 2.5x longer and dumped a pile of unrelated content. A tool that fails honestly is better than one that dumps token bloat on a miss.

### Bottom line

| | Search | Fetch | Crawl |
|---|---|---|---|
| **DonSeTch** | Competitive (close) | **Decisive win** | **Win** |
| **Firecrawl** | Slight edge (speed, authority) | Refuses LinkedIn/Reddit; token-bloated | Slower; bloat-on-miss; better no-sitemap discovery |

For agent workloads, fetch and crawl matter most. DonSeTch wins both: it reaches sites the paid tool refuses, returns 16x fewer tokens, processes PDFs 22x faster, and crawls 2.5x faster with honest failure behavior. Firecrawl's genuine strengths are search speed, mainstream source authority, and no-sitemap URL discovery.

---

## ⚠️ Gotchas

| Surprise | Why |
|---|---|
| First build takes ~2 min | BoringSSL is compiled from source. Cached after that. |
| Go is a build dependency | BoringSSL's build system is Go-based. You need Go even though DonSeTch is Rust. |
| OCR/rerank not in default build | ONNX Runtime's C++ global constructors can deadlock on aarch64. Build with `--features ocr,rerank` to enable. The prebuilt npm binary ships with both. |
| Interactive captchas not solved | hCaptcha, reCAPTCHA, Turnstile checkbox = honest dead end. No solving service by design. |
| robots.txt ON by default for crawl | `respect_robots=true` for crawl. `fetch` doesn't check robots. |
| Search rate-limits without a proxy | Keyless search scrapes public engines from your IP. Set `DONSEEK_PROXIES` for heavy use. |
| Not built for mass scraping | DonSeTch is for agentic research, not bulk extraction. |
| Disable disk persistence | `DONSEEK_NO_DISK_STATE=1` skips writing self-improvement data to disk. In-memory state still works for the session. |

---

## 🧱 Honest limits

| What it can NOT do | Why |
|---|---|
| Solve CAPTCHAs | Deliberate. You get a clear error, not a hang. |
| Sites requiring login | Out of scope (page rendering, not authenticated sessions). |
| ML-DSA post-quantum signatures | BoringSSL 5.1.0 lacks them. Will be added when BoringSSL gains it. |
| Search with all engines down | Returns an error with per-engine status. Honest, not fake. |

---

## 🤝 Contributing

PRs welcome. See [CONTRIBUTING.md](CONTRIBUTING.md). Run `cargo clippy --all-targets --features ocr,rerank -- -Dwarnings` and `cargo test --features ocr,rerank` before submitting. AGPL v3: all contributions under the same license.


## 📄 License

Copyright (c) 2026 Bishesh Bhandari. AGPL-3.0 — see [LICENSE](LICENSE).

---

<div align="center">

### If DonSeTch saves you time, ⭐ the repo

[![Stars](https://img.shields.io/github/stars/dondai44423/donsetch?color=ff9f43&style=flat-square)](https://github.com/dondai44423/donsetch)

**AGPL v3** · [Changelog](CHANGELOG.md) · [Issues](https://github.com/dondai44423/donsetch/issues) · [Releases](https://github.com/dondai44423/donsetch/releases)

</div>
