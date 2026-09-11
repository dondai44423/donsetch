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
[![Tests](https://img.shields.io/badge/tests-766%20passed-00d4aa)](#)
[![npm](https://img.shields.io/npm/v/donsetch?color=cb3837&logo=npm)](https://www.npmjs.com/package/donsetch)
[![npm downloads](https://img.shields.io/npm/dm/donsetch?color=cb3837&logo=npm&label=downloads)](https://www.npmjs.com/package/donsetch)
[![GitHub stars](https://img.shields.io/github/stars/dondai44423/donsetch?style=flat&logo=github&color=e3b341)](https://github.com/dondai44423/donsetch/stargazers)

[Install](#-install) · [Quickstart](#-quickstart) · [The 6 tools](#-the-6-tools) · [Chrome TLS](#-chrome-tls-not-chrome-like) · [Solve & Bounce](#-solve-and-bounce) · [Search](#-keyless-search) · [PDF](#-pdf--ocr) · [Benchmark](#-wrb-web-research-benchmark) · [Comparison](#-comparison) · [Gotchas](#-gotchas) · [Limits](#-honest-limits)

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
Three tools, zero API keys, zero accounts. Rust, one binary. The fetch
and crawl transport is built from scratch: no hyper, no Playwright, no
Selenium. (BYOK adapters and the opt-in CloakBrowser installer use
reqwest; the core paths that run on every fetch do not.)

Works with every MCP client (Claude Code, Cursor, OpenCode, Pi, Hermes)
and as a standalone CLI.

> **V4 is coming. And it's not just any upgrade.**

## ✨ What makes it different

| | What it does |
|---|---|
| 🛡️ **Real Chrome TLS** | Drives Chrome's own BoringSSL natively. Your ClientHello IS Chrome's ClientHello. The fingerprint is emergent from the real engine, not a faked table that rots. |
| ⏱️ **Temporal stealth** | TLS session resumption, conditional revalidation (304), persistent cookies, connection pooling. The loudest remaining bot tell, and nobody else fakes it. |
| 👻 **Solve-and-bounce** | Browser solves the challenge, hands cookies back to tier 1, goes to sleep. The browser almost never fetches content. |
| 🧠 **Self-improving fetch** | Learns from every fetch: cookie lifetimes adapt, walls that survive real browsers cool down, searches pre-solve known walls. Converges to optimal routing per domain. |
| 🔑 **Keyless search** | 10+ backends in parallel, fused by cross-engine consensus + local semantic reranking. No API keys. $0 forever. BYOK optional. |
| 📄 **Pixel-fusion PDF** | Glyphs + rendered pixels from the same stream, fused deterministically. Per-region trust audit. Scanned PDFs auto-OCR'd. |
| 🧬 **Built from scratch** | Own HTTP/2 (HPACK, flow control), own extraction engine, own PDF parser, own search aggregator, own crawl engine. |
| 🪶 **~2k tokens** | Three tools, ~2.0k tokens of total schema (tools/list, measured). Every token earns its place. |

## 🧭 Pick the right tool for the job

DonSeTch is a **rapid-fire research tool**: search, read, verify. A
search, a fetch or two, a docs page, one PDF. Its speed is the point,
and that speed is its stealth for one-shot reads.

It is NOT built for tasks where an agent "works through" a defended
site the way a person would. That class includes:

- **Bulk document harvesting**: discovering and downloading many PDFs
  or files from one repository in a single run.
- **Long sessions against one site**: page after page, click by click,
  at machine speed, same IP, no human pauses.
- **Mass extraction**: mirroring a file library, dataset collection,
  systematic downloads.

DonSeTch will *probably work* on those too, and nothing stops it.
But every request fires hundreds of times faster than a human, and
defended sites read that pattern itself as a bot, not just the
fingerprint. The realistic risk is an IP-level block: restricted
access, a captcha wall, or a ban on your whole network mid-run. When
that happens, it is the task shape, not a fetch-layer failure. The
same page fetched once, as a research read, is fine.

For those workflows, use
**[Bladebro](https://github.com/dondai44423/bladebro)**. It is the
next step up, built exactly for that shape of work: a real browser
doing what a human does, page by page, download by download, at a
human's pace. Slower than DonSeTch by design, because over a long
session against a defended site, looking human beats being fast.

**Rule of thumb: one-shot research = DonSeTch. Working a defended
site like a person to collect things = Bladebro.**

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

Registers the agent tools as native pi tools, spawns the binary at session
start, self-updates with `pi update --extensions`.

**DeepSeek Harness (`dsh`, first-class plugin):**

```bash
dsh plugin --profile web add github:dondai44423/donsetch-dsh
```

One line, and every dsh agent gets fetch, search and crawl as native
`donsetch_*` tools (no `mcp__` import names, no manual MCP config): the
plugin downloads the verified DonSeTch binary for your platform,
registers the tools in-process on the harness registry, auto-updates
with DonSeTch releases, and picks up `donsetch keys add` changes live
from the terminal. Keyless engines work out of the box. See the
[donsetch-dsh repo](https://github.com/dondai44423/donsetch-dsh) for
the config reference and update semantics.

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
`headless`. `auto` (the default) always uses the original Chromium backend
with the shipped headful/off-screen behavior. CloakBrowser is used only
after explicit selection via `DONSETCH_BROWSER_BACKEND=cloakbrowser`; a
bare `CLOAKBROWSER_BINARY_PATH` on its own never switches the backend.

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

**One-surface clients (the `[meta]` fold):** if your agent reports tool
metadata but no page text, or gets the page text but can't cite a URL
behind an `S3` handle, can't paginate, and never sees an error code,
the client is rendering only one of the two MCP result surfaces and
dropping the other. DonSeTch's default shape is split on purpose:
`content` carries the document markdown, `structuredContent` carries
compact actionable state (raw URLs behind the handles, `next_offset`,
resume tokens, `content_ok`, `thin`, error codes, `next_action`). MCP
never specified which surface a client shows, so both halves of that
split get dropped in the wild — Claude Code and VS Code keep
`structuredContent` and discard `content`; OpenCode v1 (tested on
1.18.3) keeps `content` and discards `structuredContent`. Either way
the fix is the same fold: a compact leading `[meta]` text block carries
the state, the markdown stays a clean text block behind it, and
`structuredContent` is omitted.

Those three are detected at the handshake by `clientInfo.name`, matched
exactly and case-insensitively against a hardcoded list, so a wrapper
or fork under any other name — `claude-code-proxy`, say — is *not*
detected and silently keeps the split shape. That is what
`DONSETCH_MCP_TEXT_ONLY=1` is for: it forces the fold for every client
regardless of the handshake, so an unlisted host is fixable today
rather than at the next release. Fail-closed, like the other env flags
— only an explicitly true value turns it on; `=false` leaves the
handshake in charge. Every other client keeps the default token-optimal
split shape.

**2. CLI (for humans and scripts).** Same engine as MCP, thin adapter:

```bash
donsetch fetch https://example.com --focus "pricing"
donsetch search "rust async patterns" --intent code
donsetch crawl https://docs.python.org --mode map --topic asyncio
```

## 🔀 HTTP Proxy

**Search and crawl**: configure rotating proxies with `donsetch proxy add`
or `DONSEEK_PROXIES`. Their egress lanes are reported per request
(via= labels on the wire view).

**Fetch**: no proxy by default: direct dials keep the stealth
guarantee intact. But fetch follows the curl/openssl convention when
the environment asks for it:

- `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` (uppercase or lowercase)
  route fetches through that proxy, `NO_PROXY` exempts.
- `DONSETCH_NO_ENV_PROXY=1` disables the convention entirely
  (privacy purists, environment hygiene).
- HTTP CONNECT proxies get an interception-safe handshake: no
  GREASE/ALPS/ECH/compress-cert, because TLS-terminating middleboxes
  (corporate MITM, cloud sandboxes) re-sign with their own stack and
  some reset on exotic ClientHellos. SOCKS5 proxies keep the
  Chrome-true handshake (TLS rides end-to-end).
- `SSL_CERT_FILE` / `SSL_CERT_DIR` are loaded into the trust store:
  in an intercepting network that bundle is the only way re-signed
  certificates verify, exactly like curl.
- `donsetch doctor` reports your egress posture: resolved env proxy,
  kill-switch state, system + environment trust stores, and a live
  network check that names the interception fix when it fails.

A proxy you configured is your egress: an intercepting proxy sees
plaintext by definition, which is the same tradeoff curl and reqwest
make.

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

## 🎯 The 6 tools

| Tool | What it does |
|---|---|
| 🌐 **`web_fetch`** | Any URL as clean markdown. HTTP first, escalates to headless browser on bot walls. PDFs with OCR + per-page confidence, `focus`/`toc`/`section`, pagination, `actions` for in-page control, `must_contain` probes, `archive` resurrection. |
| 🔎 **`web_search`** | Keyless multi-engine search: 10+ backends, consensus + semantic reranking, query-aware official-source placement. Returns ranked URLs + snippets. |
| 🕷️ **`web_crawl`** | Best-first same-domain crawl. Sitemap + frontier, `focus` ranking, elastic pacing, resume tokens, honest stop reasons. |
| 💡 **`web_answer`** | Terminal synthesis over 2-9 fetched sources when enough authority exists; otherwise it answers with the exact gap and no fluff. |
| 🧠 **`web_memory`** | Local-only vector memory the agent writes to as it works (performant: one batch = one embed pass + one disk write, async off the answer path). Search by semantic proximity, cap-bounded, on-device model. |
| 📸 **`web_screenshot`** | Rendered PNG of any URL through the same tier-2 browser. URL goes through the usual safety guards; CLI twin: `donsetch screenshot URL [--out PATH]`. |

Tool schemas: `donsetch tools`. Every tool returns structured errors
with stable codes + `next_action`. v3.6 compact contracts: the model
surface (content + structuredContent) carries only evidence and the
state that can alter the next action, `content_ok`, `thin`, `changed`,
`next_offset`, error codes, while transport telemetry (tier, quality,
escalation trace, timings, engines) stays available to clients under
`_meta` e.g. `_meta["com.donsetch/fetch-debug"]`.

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
<summary><b>Verified against live Chromium, including the installed browser on YOUR machine</b></summary>

A dev rig in the repo captures the real browser's raw stream and
parity tests are generated from those captures, never hand-edited.
New browser version = re-run the rig, diff the expectations.

| Signal | Match |
|---|---|
| **JA4** | cipher hash identical to Chrome |
| **Akamai h2 fingerprint** | exact match |
| **SETTINGS + WINDOW_UPDATE preface** | byte-identical, CI-asserted |
| **Request HEADERS priority** | [E=1 dep=0 weight=255] + PRIORITY flag, exact |
| **h2 header order** | sec-ch-ua → sec-ch-ua-mobile → sec-ch-ua-platform, exact |
| **sec-ch-ua brands** | reflects the installed binary: Chromium first, greased Not=A?Brand v=99, Google Chrome brand only when the binary is branded |
| **Extension set** | identical (contents differ only in random key material) |

</details>

### Own HTTP/2 stack

Off-the-shelf h2 doesn't expose pseudo-header order, the exact SETTINGS
set, WINDOW_UPDATE values, or HPACK indexing strategy, all
fingerprintable. So DonSeTch has its own: HPACK (all 257 Huffman
symbols + 61 static entries, verified), frame engine (SETTINGS, DATA,
WINDOW_UPDATE, PING, GOAWAY, RST_STREAM, CONTINUATION, PRIORITY),
request HEADERS carrying Chromium's priority flag and block,
flow control
with replenishment, TLS 1.3 session resumption, connection pool.

The h2 preface is asserted byte-identical to Chromium in CI:
detectability regressions are build failures.

### Temporal stealth

| Mechanism | Why it matters |
|---|---|
| TLS session resumption | Scrapers never resume. Chrome always does. |
| TCP Fast Open (warm hosts) | Repeat navigations send data on the SYN, like Chrome. Linux. |
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

## 🧠 Self-improving fetch

Every fetch is an action AND an observation. Pure deterministic
state, no ML. The loop converges; the more you use it, the less it
escalates.

| Visit | Route | What happens |
|---|---|---|
| 1 (unknown) | `Cold` | tier 1 → walled → solve → store cookies |
| 2 (fresh) | `Warm` | tier 1 with cookies, browser asleep |
| N (expired) | `SkipToSolve` | straight to ghost, no doomed round-trip |
| M (24h later) | `RecheckCold` | wall may be gone, try tier 1 cold |
| Wall beat the browser twice | `SolveCooldown` | honest sub-ms fail with exponential backoff (15m → 2h cap) instead of a 20-40s browser cycle per attempt |

Cookie lifetime converges: `observed_lifetime = min(previous,
now - last_solved)`. Only clearance cookies persist (cf_clearance,
datadome, _abck); tracking cookies are filtered out.

**Fail-fast wall memory.** A wall that survives TWO real-browser
solves is remembered: the domain enters a solve-cooldown and future
fetches answer honestly in milliseconds until the backoff lapses. A
single failure never gates a domain, and any real solve or tier-1
cold success clears the memory. Your daemon learns what its own
environment cannot do.

**Predictive pre-solve.** Searches look at the top result: when its
domain is a known wall, a bounded background solve starts while the
agent reads results. The follow-up fetch lands warm.

Disable with `DONSEEK_NO_DISK_STATE=1`.

<div align="center">

<img src="assets/owlsearch.png" alt="DonSeTch Search">

</div>

## 🔎 Keyless search

No API key, no account. 6 keyless engines across 4 independent index
families + 8 official verticals run in parallel on your machine, merged,
deduped, ranked.

- **Backends:** Bing-family (Bing, DuckDuckGo, Yahoo), Brave, Mojeek, Google
  + keyless verticals (GitHub, Wikipedia, HN, Semantic Scholar, arXiv,
  StackExchange, MDN, Google News).
- **Native Google:** browser-free HTTP via the legacy mobile endpoint, using
  the existing Rust transport. Seven selectable, experimentally verified Nokia
  profiles; the default is `6230-03.15`. No paid API or CAPTCHA solver.
  Successful profiles remain preferred per egress in memory; CAPTCHA advances
  circularly through the profiles. Retry, pacing and quarantine use the same
  policy as other engines, with no per-profile cooldowns. Selection resets on
  restart. Rate limits do not advance the profile cursor.
  Availability depends on Google and the network; see [configuration and limits](docs/google-wml.md).
- **Ghost SERP cascade lane:** if the plain fan-out and its retry wave
  leave the merge thin (<3 lanes or <15 hits) and native Google failed,
  one headless render can recover Google's desktop SERP. Costs
  nothing when healthy (only fires under underdelivery); reports itself
  honestly as `google_ghost`.
- **Semantic reranking**: local ONNX cross-encoder
  (`ms-marco-MiniLM-L-6-v2`, 23MB) reads query + title + snippet
  through full attention. 60/40 blend with RRF + BM25 + consensus, plus
  a post-enrichment top-up that re-scores close calls using the
  destination page's real title/description instead of SERP fragments.
- **Consensus**: a URL several independent indexes return gets a
  boost; every result carries `score`, `consensus` (independent-index
  families), `engines`. Corroboration also rides the compact model
  surface as `· N sources` per result.
- **Engine health is learned and persisted:** per-engine trust EWMAs
  and chronic-failure quarantine survive daemon restarts; dead engines
  get benched instead of burning fan-out slots every query.
- **Entity coverage penalty**: anchor entities ("B-tree", version
  numbers, years) checked against results. Wrong entity → 0.3x.
- **Honest reporting**: `weak=true` means low consensus; per-engine
  status is always visible. No fake "no results".
- **Bench harness note:** search-quality result caching lives in
  `~/.cache/donsetch/bench-search/`; delete it when comparing binaries.

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
donsetch keys add serpbase sb-...       # SerpBase Google SERP (100 free searches)
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

### BYOK plugins (providers not natively supported yet)

If the platform you have a key for is not natively supported, you can
use a plugin as a workaround in the meantime: register any executable
that answers a tiny stdin/stdout JSON contract, and DonSeTch treats it
like any other search provider (default chain, fallback, attribution).
Any language works: shell, Python, a compiled binary. No code changes
to DonSeTch, no waiting for a release.

```bash
donsetch keys add plugin searxng --cmd 'python3 ~/searxng-adapter.py' --test
```

The adapter reads one JSON document on stdin and prints one on stdout.
Request:

```json
{"format":1,"query":"rust async","max_results":8,"intent":"web","deadline_ms":30000}
```

Response:

```json
{"format":1,"results":[{"title":"...","url":"https://...","snippet":"...","score":0.9}]}
```

Errors: exit non-zero with a message on stderr, or respond with
`{"format":1,"error":"...","retryable":true}`. Constraints that
keep it reliable: hard timeout (default 30s, `--timeout` to change),
8 MiB stdout cap, direct exec (no shell), killed on cancellation, and
a malformed response degrades gracefully to the fallback chain.
Keys belong in the adapter's own environment, never in DonSeTch
config. Native support for the big/keyed providers (Exa, Bright Data,
Tavily, Serper, SerpApi, Brave) keeps coming; plugins are the bridge
for everything else.

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

**Tier 3 bypass** (opt-in): when ghost itself hits a hard wall,
fetch falls back to Bright Data Web Unlocker if a key is configured
(`donsetch keys add unlocker <key>[::zone]`). The unlocker solves
server-side, captchas included, and returns rendered HTML into the
normal pipeline. Failures carry exact guidance (token rejected,
zone not found, balance empty, rate limit, target still walled)
attached to the fetch escalation trace, and `donsetch doctor --deep`
validates the token and zone for free before the first paid call.
Advanced users only; DonSeTch works identically without it.
(Bright Data sign-up link above is an affiliate link.)

**Solve-cache**: every successful unlock is cached locally (URL-hash
keyed, sliding 6h TTL, 200 entries, parallel fetches share one paid
call, bodies stored byte-exact). Same page again inside the TTL =
served from cache at zero cost.
Env knobs: `DONSETCH_BYPASS=0` (off), `DONSETCH_BYPASS_MAX_DAILY`
(default 50), `DONSETCH_BYPASS_TIMEOUT_SECS` (default 120),
`DONSETCH_BYPASS_RENDER`, `DONSETCH_BYPASS_CACHE_TTL_SECS`,
`DONSETCH_BYPASS_CACHE_MAX_ENTRIES`, `DONSETCH_BYPASS_CACHE=0`,
`DONSETCH_UNLOCKER_ZONE` (default zone), `DONSETCH_BYPASS_ENDPOINT`
(test hook). Solver failures carry a concrete fix attached to the
fetch's escalation trace, and `donsetch doctor --deep` probes the
token + zone against Bright Data for free (no spend) first.

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
  429/503 → backoff on host signals; host-declared waits
  (`Retry-After`, robots `Crawl-delay`) honored in full; everything
  self-inferred caps at ~7s. Zero artificial dwell on the fetch path
  (stealth through truth, never time).
- **Crawl-shape**: frontier pops get a seeded reader-like jitter, so
  repeated crawls never replay one identical, score-eager fetch
  order to server logs. Ordering only (every page still fetched,
  payloads untouched). Kill switch: `DONSETCH_NO_CRAWL_SHAPE=1`.
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
| 🔌 **MCP daemon** | stdio + HTTP servers, JSON-RPC 2.0, 6 tools, crash-only supervisor | `src/mcp/` |

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
| Not built for mass scraping | Agentic research, not bulk extraction. See "Pick the right tool" above. |

## 🧱 Honest limits

| It can NOT | Why |
|---|---|
| Interactive captchas (hCaptcha, reCAPTCHA, Turnstile) | Honest dead end, no solving service by design. |
| ML-DSA post-quantum signatures | BoringSSL 5.1 lacks them; lands when BoringSSL has it. |
| Search with all engines down | Error with per-engine status. Honest, never fake. |

## 🔐 Logging into sites (authenticated sessions)

Pages behind a login (x.com, gated docs, internal tools) need a real
session. `donsetch login` gives you one without ever seeing your
credentials:

```bash
donsetch login x.com          # opens YOUR browser, you sign in, press
                              # Enter here. Done.
donsetch login --list         # stored sessions (names/counts only)
donsetch login --status x.com # one domain, in detail
donsetch login --logout x.com # forget a domain
```

How it stays safe:
- **Credentials never enter donsetch.** It opens a real Chromium on
  your display (dedicated profile, never the automation one). You type
  into the browser. No keystroke capture, no screenshots, no CDP
  attach until you press Enter.
- After Enter the resulting cookies are harvested, filtered to
  session-worthy ones, and stored in the same 0600 vault the fetch
  engine already replays: tier-1 fetches AND tier-2 renders of that
  domain carry your login automatically, no daemon restart needed.
  Wall detection (redirect to /login, 401/403) is verified by a
  post-login probe and surfaced in `--list`.
- The registry (`auth-state.json`) stores metadata only: names,
  counts, expiries, probe verdicts. Never values.

Servers and CI (no display):

```bash
donsetch login --import cookies.txt x.com
```

with a Netscape-format cookie export (curl, browser extensions).
Multi-site sessions: run bare `donsetch login`, sign into whatever you
want in as many tabs as you like, press Enter once.

## 🤝 Contributing

PRs welcome. See [CONTRIBUTING.md](CONTRIBUTING.md). Run
`cargo clippy --all-targets --features ocr,rerank -- -Dwarnings` and
`cargo test --features ocr,rerank` before submitting. AGPL v3: all
contributions under the same license.

## 💛 Sponsors

DonSeTch is open source and free to use. If you want to support development, consider sponsoring.

| Tier | Price | What you get |
|---|---|---|
| 🥉 Bronze | $10/mo | Name + link in Sponsors section |
| 🥈 Silver | $25/mo | Small logo + link in Sponsors section |
| 🥇 Gold | $49/mo | Large logo + link, pinned at top of Sponsors section |

One-time sponsorships are also welcome at any amount.

Pricing will increase as the project grows. Right now DonSeTch is early (small but growing), so sponsorship is cheap. A Gold tier at $49/mo is high reward, near zero investment for any company that relies on web research for AI agents. Lock in the current rate before it goes up.

If your product is part of this space (proxy platforms, search infrastructure, BYO providers, anything a DonSeTch user would plug in), Gold goes one step further: if your tool fits natively within DonSeTch, you get the banners and the link plus an official native integration shipped in the binary itself.

Email bhandaribishesh879@gmail.com to become a sponsor.

## 📄 License

Copyright (c) 2026 Bishesh Bhandari. AGPL-3.0, see [LICENSE](LICENSE).

---

<div align="center">

### If DonSeTch saves you time, ⭐ the repo

[![Stars](https://img.shields.io/github/stars/dondai44423/donsetch?color=ff9f43&style=flat-square)](https://github.com/dondai44423/donsetch)

**AGPL v3** · [Changelog](CHANGELOG.md) · [Issues](https://github.com/dondai44423/donsetch/issues) · [Releases](https://github.com/dondai44423/donsetch/releases)

</div>
