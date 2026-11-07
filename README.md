<div align="center">

# DonSeTch

**The web, for AI agents.**
**v5 beta:** a new fetch engine. No browser in the fetch path: a wall is answered by the right client identity, the site's own challenge program run in-process, or the site's own public interface. [What changed](CHANGELOG.md).
<div align="center">
<a href="https://trendshift.io/repositories/163922?utm_source=trendshift-badge&amp;utm_medium=badge&amp;utm_campaign=badge-trendshift-163922" target="_blank" rel="noopener noreferrer"><img src="https://trendshift.io/api/badge/trendshift/repositories/163922/daily?language=Rust" alt="dondai44423%2Fdonsetch | Trendshift" width="250" height="55"/></a>
</div>

[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/G5Y624N5RE)

[![Become a sponsor](https://img.shields.io/badge/sponsor-become%20a%20sponsor-ea4aaa?logo=githubsponsors&logoColor=white)](#-sponsor-this-project)
[![Rust](https://img.shields.io/badge/Rust-edition%202024-ce422b?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![MCP](https://img.shields.io/badge/MCP-server-7c3aed?logo=modelcontextprotocol&logoColor=white)](https://modelcontextprotocol.io)
[![License](https://img.shields.io/badge/license-AGPL%203.0-2563eb)](LICENSE)
[![Tests](https://img.shields.io/badge/tests-1%2C900%2B%20passing-00d4aa)](.github/workflows/ci.yml)
[![npm](https://img.shields.io/badge/npm-donsetch-cb3837?logo=npm)](https://www.npmjs.com/package/donsetch)
[![npm downloads](https://img.shields.io/npm/dm/donsetch?color=cb3837&logo=npm&label=downloads)](https://www.npmjs.com/package/donsetch)
[![GitHub stars](https://img.shields.io/github/stars/dondai44423/donsetch?style=flat&logo=github&color=e3b341)](https://github.com/dondai44423/donsetch/stargazers)

[Why](#-why-its-different) · [Demo](#-demo) · [Sponsors](#-our-sponsors) · [Install](#-install) · [Quickstart](#-quickstart) · [The 4 tools](#-the-4-tools) · [Fetch](#-fetch) · [Search](#-keyless-search) · [PDF](#-pdf--ocr) · [Stealth](#-stealth-real-clients-not-chrome-like) · [Compare](#-how-it-compares) · [Limits](#-gotchas--honest-limits) · [CLI](#-cli)

</div>

---

<img src="assets/herobanner.png" alt="DonSeTch, the web, for AI agents" width="100%">

DonSeTch gives any AI agent full web research from a single local process: fetch, search, crawl, screenshot. Four tools, zero API keys, zero accounts, Rust, one binary. Fetching launches no browser (no Playwright, no Selenium, no headless Chrome): pages are read as a real client would read them, at curl's cost, and the tool surface stays small enough to fit an agent's context.

Works with every MCP client (Claude Code, Cursor, OpenCode, Pi, Hermes) and as a standalone CLI.

> **🤝 Use Bright Data? Support DonSeTch.** Their proxy, SERP and Web Unlocker
> products come through this partner link and part of it comes back to keep
> DonSeTch free. You pay nothing extra:
> **https://get.brightdata.com/ivqwoicrrlbr**
> Bright Data plugs into the tool itself: the `bd` SERP provider, the Web
> Unlocker (opt-in) and the `unlocker` key type.
> [Bright Data setup guide](docs/brightdata.md): zones, API token, HTML/JSON and troubleshooting.

## ✨ Why it's different

| | What it does |
|---|---|
| 🛡️ **Real clients, whole** | Eight client identities: Chrome (Linux, Windows, Android), Firefox, Safari, an Android app (OkHttp) and a plain client. Each carries one real profile's TLS, HTTP/2 settings, header order and client hints as a whole. The Chrome identities match real Chrome's JA4 and Akamai HTTP/2 fingerprint exactly. Chosen per site, never patched per field. |
| ⚙️ **Challenges, in one process** | When a wall needs the page's own program, it runs in an embedded V8 against a small, bounded host, on the same identity, and the credential it mints is spent by that identity. AWS WAF passes cold in 2-5 s. No browser, no layout, no rendering. |
| 🧠 **Self-improving fetch** | Remembers which identity each site accepts, what each bot-wall vendor lets through, which route answered, and how far each site's cache headers can be trusted. Measured: first-try content on a hard corpus went from 13/28 to 26/28 once learned, with 40% fewer requests. |
| 🎯 **Read only what you need** | `must_contain` answers "is X on this page?" per term with counts, sections and excerpts, in a few hundred characters. `focus` returns the passages that answer a question, best first. `toc` then `section` reads one part. Every view searches the whole page, PDFs and site adapters included. |
| 🧾 **Honest outcomes** | Every read says what it is: content, or a wall with its vendor and what it asks for (a login, a human, a program). A probe on a partly read page says `unverified`, never a false "not found". |
| 🔑 **Keyless search** | 10+ backends in parallel, fused by cross-engine consensus plus local semantic reranking. No keys, $0 forever. BYOK optional. |
| 📄 **Pixel-fusion PDF** | Glyphs and rendered pixels come from the same content stream and are fused deterministically, with a per-region trust audit. Scanned PDFs auto-OCR. |
| 🧬 **Built from scratch** | Own fetch engine, extraction engine, PDF parser, search aggregator and crawl engine. |
| 🪶 **~2k tool schema** | All four tools: 2,042 tokens (`o200k_base`, compact `tools/list` JSON). |
| 🩺 **`doctor`** | One command sweeps config, search health, egress, TLS, browser, DNS, captive portal, secret-store permissions, and fixes what is mechanically fixable. |

## 🎬 Demo

<div align="center">

<video src="https://github.com/user-attachments/assets/32bc0899-87bf-417b-8ca8-c0a4a51ee167" controls muted width="640"></video>

</div>

*(30-second walkthrough: search, bot-wall bypass, crawl)*

<div align="center">

<video src="https://github.com/user-attachments/assets/f164b31e-96ef-4294-b2dd-6777642098dc" controls muted width="640"></video>

</div>

*(Pi agent session: live research with DonSeTch as a native extension)*

## 💛 Our Sponsors

<div align="center">

<a href="https://proxy-seller.com/"><img src="assets/sponsors/proxy-seller-banner.png" alt="Proxy-Seller: private residential, mobile, ISP and datacenter proxies with HTTP(S) and SOCKS5 support. Promo code DON15 for 15% off." width="100%"></a>

**[Proxy-Seller](https://proxy-seller.com/)**: private residential, mobile (4G/5G/LTE), ISP, and datacenter IPv4/IPv6 proxies with HTTP(S) and SOCKS5 support, 47M+ residential IPs in 220+ locations, precise country/city/ISP targeting, 99.7% uptime, and 24/7 support. Promo code **DON15** takes 15% off.

**Want your name or logo here?** [Sponsorship tiers](#-sponsor-this-project).

</div>

## 🤝 Sponsor this project

DonSeTch is free and open source, and stays that way. Sponsorship pays for the time it takes to keep shipping.

| Tier | Price | What you get |
|---|---|---|
| 🥉 Bronze | $10/mo | Name + link in Our Sponsors |
| 🥈 Silver | $25/mo | Small logo + link in Our Sponsors |
| 🥇 Gold | $49/mo | Large logo + link, pinned at the top of Our Sponsors |

If your product is part of this space (proxy platforms, search infrastructure, BYO providers, anything a DonSeTch user would plug in), Gold goes one step further: fit natively inside DonSeTch and you get the placement plus an official integration shipped in the binary itself.

**How to sponsor.** Email **bhandaribishesh879@gmail.com**, or use the ♥ **Sponsor** button on this repo. GitHub Sponsors payouts aren't available in my region yet, so email is the reliable route: write me, I'll share my PayPal, and we go through the steps. One-time sponsorships are welcome at any amount.

**Prepay to lock your rate.** You can pay for any number of months in one go at today's price, and that rate is fixed for the whole term, so 6 months of Gold at today's $49/mo is $294, and a later increase (even to $150/mo) never touches it. Price changes only ever apply to sponsorships started after them.

## 📦 Install

**npm, any platform (recommended):**

```bash
npm install -g donsetch
```

Downloads the prebuilt binary for your platform from GitHub Releases with SHA256 verification. No build tools needed.

**Homebrew (macOS/Linux):**

```bash
brew tap dondai44423/donsetch && brew install donsetch
```

**Pi agent (native extension):**

```bash
pi install npm:donsetch
```

Registers the tools as native pi tools, spawns the binary at session start, self-updates with `pi update --extensions`.

**DeepSeek Harness (`dsh`, first-class plugin):**

```bash
dsh plugin --profile web add github:dondai44423/donsetch-dsh
```

One line and every dsh agent gets fetch, search and crawl as native `donsetch_*` tools, no `mcp__` names and no manual MCP config: the plugin downloads the verified binary for your platform, registers the tools in-process, auto-updates with DonSeTch releases, and picks up `donsetch keys add` changes live. See the [donsetch-dsh repo](https://github.com/dondai44423/donsetch-dsh) for the config reference.

Homebrew and dsh auto-track published releases.

**Verify the install:**

```bash
donsetch doctor          # grouped health checks, including a browser launch for screenshots
donsetch doctor --deep   # adds live egress, provider and captive-portal probes
donsetch doctor --fix    # repairs mechanical problems automatically
donsetch doctor --json   # appends machine-readable check results
donsetch doctor --mcp    # detects clients and prints absolute-path registration blocks
```

<details>
<summary><b>Install notes and troubleshooting</b></summary>

- **Linux prebuilts:** glibc >= 2.35 (Ubuntu 22.04 LTS and newer). The bundled ONNX lib keeps its own 2.27 floor, so OCR and rerank work on all of those.
- **pnpm or bun:** approve the build script (`pnpm approve-builds`, or the bun equivalent), then reinstall. If scripts were blocked, `npx donsetch` invokes the self-healing shim.
- **`--ignore-scripts`:** postinstall is intentionally skipped. Run `node node_modules/donsetch/install.js`, or `npx donsetch` to download the binary when network is available.
- **System-wide npm installs:** a root-owned npm prefix (`/usr/local/lib/node_modules`) cannot receive the binary when you run as a normal user. donsetch falls back to an existing binary on `PATH` or in `~/.cargo/bin`; `DONSETCH_BINARY=/path/to/donsetch` points at one explicitly. Keep the package copy with `npm config set prefix ~/.npm-global && npm install -g donsetch`.
- **Proxy:** set `HTTPS_PROXY` (or `https_proxy`, `HTTP_PROXY`, `http_proxy`) to an HTTP CONNECT proxy.
- **Release mirror:** set `DONSETCH_RELEASES_BASE` to a mirror holding `<tag>/<asset>` paths.
- **Windows:** the installer needs `tar`, included since Windows 10 1803.
- **Windows ARM64:** the x64 build runs under emulation, no native asset needed.
- **musl/Alpine:** published Linux binaries are glibc. Build from source on musl.
- **First OCR/search run downloads models** (~24MB reranker, ~37MB OCR), cached forever. Pre-seed offline boxes by copying the `ocr`/`rerank` cache dirs.

</details>

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

The challenge engine embeds V8 through `deno_core`. The build downloads a prebuilt V8 static library for your target; on an offline box, point `RUSTY_V8_ARCHIVE` at a downloaded copy.

```bash
git clone https://github.com/dondai44423/donsetch.git
cd donsetch
cargo build --release --features ocr,rerank,http
```

First build compiles BoringSSL and links V8 (a few minutes), cached after. Chromium is optional and never fetches pages: it serves `web_screenshot` and `donsetch login`. DonSeTch auto-discovers system Chromium, Playwright's cached builds, or Edge on Windows.

On Ubuntu 22.04 and anything with bfd 2.38, link with lld: bfd cannot parse the `.crel` relocations rustc 1.86+ emits for aarch64, and the default link dies with "unknown architecture".

```bash
sudo apt-get install -y cmake build-essential pkg-config libclang-dev clang lld nasm golang-go
RUSTFLAGS="-C link-arg=-fuse-ld=lld" cargo build --release
```

The same recipe covers the prebuilt baseline: every asset from v3.4.5+ is built on Ubuntu 22.04 and runs there directly.

**Feature set:** default is `[]` (fetch, search, crawl, PDF). `ocr,rerank` pulls in ONNX Runtime, `http` enables the HTTP MCP transport. npm prebuilts ship all three on linux-x64, macOS-arm64, macOS-x64 and Windows-x64. On macOS, OCR and rerank need macOS 13.4 or newer (the minimum of the Microsoft-signed ONNX dylib that ships beside the binary). **Linux ARM64 is not in the 5.0 beta:** the v5 core's binary does not start on aarch64 yet (the core's own suite passes there; the product's link does not), so the beta ships no arm64 asset — 4.10.5 remains the installable release on that target.

</details>

<details>
<summary><b>Selectable browser backend</b></summary>

The browser serves screenshots and `donsetch login`, never page reads. Chromium is the default: headful on Xvfb/off-screen when a display exists, `--headless=new` only when none does. CloakBrowser is used only when explicitly selected, a bare `CLOAKBROWSER_BINARY_PATH` never switches it.

```bash
DONSETCH_BROWSER_BACKEND=chromium donsetch doctor             # default backend
DONSETCH_BROWSER_BACKEND=headless donsetch doctor --deep      # forced headless
DONSETCH_BROWSER_BACKEND=cloakbrowser \
  CLOAKBROWSER_BINARY_PATH=/path/to/chrome donsetch doctor --deep
```

You do not have to install CloakBrowser yourself: with auto-download on, DonSeTch fetches the pinned release from GitHub the first time the cloak backend is needed (a screenshot, a login or `doctor`), unpacks it into its cache and reuses it from then on.

```toml
[browser]
backend = "cloak"
cloak_auto_download = true
```

The same from the environment: `DONSETCH_BROWSER__BACKEND=cloak DONSETCH_BROWSER__CLOAK_AUTO_DOWNLOAD=true donsetch doctor`.

Aliases: `original` for `chromium`, `original-headless` for `headless`. Public CloakBrowser downloads are opt-in (`DONSETCH_CLOAK_AUTO_DOWNLOAD=1`) and verified: Ed25519-signed `SHA256SUMS`, manifest bound to the requested Chromium version, archive SHA-256 checked, unsafe paths rejected. `CLOAKBROWSER_VERSION` pins a version. CloakBrowser binaries are never bundled in releases or images.

</details>

## 🚀 Quickstart

**MCP server (for any agent).** Register and go:

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

`--supervised` is the crash-only daemon: a panic is a blip, the daemon restarts itself, the session survives. Without a global install, use `"command": "npx", "args": ["donsetch", "mcp"]`.

HTTP transport instead of stdio: `donsetch mcp --http --port 8765`, clients connect to `http://localhost:8765/mcp`. Sessions, cancellation, `/health`, token auth via `DONSETCH_HTTP_TOKEN` and per-request timeouts are documented in `donsetch mcp --help`. A request that carries a browser `Origin` is refused unless the page is on loopback or CORS mode (`DONSETCH_HTTP_CORS`, which requires the token) is on, and a server bound to loopback answers only to loopback `Host` names on its own port, so a web page cannot drive it and a DNS-rebound name cannot read from it.

**CLI (for humans and scripts).** Same engine, thin adapter:

```bash
donsetch fetch https://example.com/pricing --must-contain "free tier" --must-contain '/\$\d+/'
donsetch fetch https://docs.python.org/3/library/asyncio-task.html --focus "cancel a task"
donsetch search "rust async patterns" --intent code
donsetch crawl https://docs.python.org --mode map --topic asyncio
```

<details>
<summary><b>MCP results and client bridges</b></summary>

By default, every client receives evidence and actionable state together in
`content`: a leading `[meta] {JSON}` line followed by source markdown. This
also works for unknown wrappers and OpenCode's generic `cli` handshake.
`structuredContent` is omitted so a bridge cannot select metadata and drop
the document. PNG captures remain image content blocks; diagnostics stay in
`_meta`. The same contract applies over stdio and HTTP.

Raw MCP returns a result object with a `content` array. Some agent bridges
flatten its text blocks into a string. Handle both at the integration boundary:

```js
const text = typeof result === "string"
  ? result
  : result.content.filter(block => block.type === "text")
      .map(block => block.text).join("\n\n");
const end = text.indexOf("\n");
if (!text.startsWith("[meta] ") || end < 0) throw new Error("Missing result state");
const state = JSON.parse(text.slice(7, end));
const markdown = text.slice(end + 1).trimStart();
// Inspect state.ok, content_ok, next_offset, resume or next_action.
```

For integrations that deliberately consume both surfaces, `[mcp] text_only =
false` restores the split result for unrecognized clients. Known clients that
drop one surface retain compatibility folding. From the environment, set
`DONSETCH_MCP__TEXT_ONLY=false`. The default requires no client-specific
configuration. `donsetch status --json` reports the configured mode and its
source under `status.mcp`; `doctor` and human `status` show the same posture.
The mode is the default for an unknown client: recognized clients can still
require compatibility folding after their MCP handshake.

</details>

## 🎯 The 4 tools

| Tool | What it does |
|---|---|
| 🌐 **`web_fetch`** | Any URL as clean markdown, or only the part that answers: `must_contain` probes, `focus` passages, `toc` → `section`. Walls answered in-process, never by a browser. PDFs with OCR and per-page confidence, site adapters, page memory, batches of up to 12. |
| 🔎 **`web_search`** | Keyless multi-engine search: 10+ backends, consensus plus semantic reranking, query-aware official-source placement. Ranked URLs and snippets, never a scraped article dump. |
| 🕷️ **`web_crawl`** | Best-first same-domain crawl. Sitemap plus frontier, `focus` ranking, elastic pacing, resume tokens, honest stop reasons. |
| 📸 **`web_screenshot`** | Rendered PNG of any URL through DonSeTch's browser. URL goes through the usual safety guards. CLI twin: `donsetch screenshot URL [--out PATH]`. |

Tool schemas: `donsetch tools` (same JSON as MCP `tools/list`). Every failure is structured: a stable `code` (`wall.challenge`, `guard.ssrf`, `deadline.hit`, `network.dns`…), an `errorKind` (`permanent`, `transient`, `walled`), and a `next_action` line, so agents branch on codes instead of parsing prose. The model surface carries evidence and the state that changes the next action; transport telemetry (fetch, quality, escalation trace, timings, engines) stays under `_meta`, for example `_meta["com.donsetch/fetch-debug"]`.

## 🌐 Fetch

One request, as a real client, at curl's cost: about half a second median on defended corpora, 47 ms on a trivial origin (curl: 46 ms). No browser is launched to read a page. When a site walls the request, DonSeTch answers the wall the way the site asks, and when nothing passes it says which wall it met instead of returning a guess.

**How a read goes:**

1. **Cache.** A stored copy is served only as far as it has been earned: a repeat read sends the conditional request Chrome sends, and the server's 304 is the proof. A page is served from its declared lifetime (capped at 60 s) only after the last check found it unchanged; a site whose validators lie is distrusted for a week.
2. **Spend.** One request as the identity this site accepted before, with that identity's own cookies, paced per site.
3. **Classify.** Content, or a wall named by its vendor (Cloudflare, AWS WAF, DataDome, PerimeterX, Akamai) and its ask: a different client, the page's program, a human, or a login. Vendors are read from their own headers before any body.
4. **Answer the wall.** Move to the identities that vendor has let through; follow a native hand-off (Akamai's `bm-verify`); run the page's challenge program in-process ([below](#-challenges-in-one-process)); or ask the site's own public interface (Reddit's feed, the Stack Exchange API, crates.io's API and others), which comes back as `transport: data`.
5. **Remember.** The identity that answered, the vendor's verdict, the route and the cookies persist, so the next read of that site starts in the right place ([self-improving fetch](#-self-improving-fetch)).

Measured from a home IP, cold, on a hard corpus of 28 defended sites × 2 rounds: **42/56 content through DonSeTch, 22/56 for curl.**

### Read only what you need

Every view is computed over the whole extracted page (up to 8 MiB), whatever the source: HTML, PDF, a site adapter, a feed or plain text. Only the view is returned.

| Option | What comes back |
|---|---|
| `must_contain` | Is it on the page? One term or up to 8: a phrase (case, spacing, quote and dash style and markdown emphasis ignored) or a `/regex/`. Per term: `found ×count`, the sections it occurs in and short excerpts, or `not found` with a near-miss hint ("its words all appear in §s4, not as one phrase"). Verdict `match`, `partial`, `no_match`, or `unverified` when part of the page was not read, so absence is never claimed without proof. With `section`, it searches only that part. |
| `focus` | The passages that answer a question, best first, each under its section path (`§s21 · Syntax and features › Ownership and references › Lifetimes`): paragraphs, list items, table rows with their header row, code. Names the query words the page does not contain. A miss returns the outline, never the whole page. 12-language BM25: CJK unigrams and bigrams, stopwords, stemming, accent folding. |
| `toc` | The heading outline with `sN` ids and sizes, no body. |
| `section` | One part by `sN` id or heading text, subsections included. A miss returns the outline. |
| `max_chars`, `offset` | Length cap (16,000 by default; the batch total for a URL array, 48,000 by default). A cut-off response carries `next_offset`; a paginated article names its next part as `next_page`. |
| `since_last` | Re-check against your previous read: one `unchanged` line, or only the changed sections. |
| `links` | Keep links as compact `[text](L-handle)` references; pass a handle as `url` to follow it. Off by default (~30% fewer tokens). |
| `image_text` | OCR the first four content images into an `image text` section that every view searches. |
| `deadline_ms` | Hard time limit: a deadline error on expiry, never a hang. Real MCP cancellation and progress notifications. |

```text
$ donsetch fetch https://doc.rust-lang.org/std/vec/struct.Vec.html --must-contain with_capacity --must-contain amortized
# Vec in std::vec - Rust
https://doc.rust-lang.org/std/vec/struct.Vec.html

must_contain: MATCH 2/2
+ "with_capacity" ×27 in §s3, §s6, §s7, §s12, §s14, §s15, +17 more
  …]); // The following is equivalent, but potentially slower: let mut vec = Vec::with_capacity(5); vec.resize(5, 0); assert_eq!(vec, [0, 0, 0, 0, 0]); ``` For more informati…
  …reallocate, which can be slow. For this reason, it is recommended to use `Vec::with_capacity` whenever possible to specify how big the vector is expected to get. ## §Guara…
+ "amortized" ×4 in §s7, §s37, §s41, §s607
  …nstant growth factor. Whatever strategy is used will of course guarantee *O*(1) amortized `push`. It is guaranteed, in order to respect the intentions of the programmer…
  …2]; vec.push(3); assert_eq!(vec, [1, 2, 3]); ``` ##### §Time complexity Takes amortized *O*(1) time. If the vector’s length would exceed its capacity after the push, *…
```

**A reading ladder** (approximate costs): search, then `must_contain` for a yes/no with evidence (100-250 tokens), `focus` for the answer (a few hundred to a few thousand), or `toc` (a few hundred) then `section` for a complete read of one part. The page itself is the last resort. Contradictory options (`must_contain` with `focus`, `toc` with `focus`, `since_last` with a view) are refused before anything is fetched, with a `fetch.args` error that says why.

**Extraction (DonSift)**: HTML bytes in, agent-native markdown out. Typed blocks with heading breadcrumbs; link farms, wiki junk and duplicates dropped; links off unless asked for. Pages are classified `Article` / `Listing` / `Forum` / `Docs` / `Table` / `Page` with a 0-1 quality score.

- **Domain adapters**: Reddit, npm/PyPI/crates.io/Go/RubyGems, GitHub, Stack Overflow, Wikipedia and docs sites are restructured from each site's own keyless surfaces. Labeled `via=adapter:…`, kill-switchable. Every view applies to them.
- **Page memory**: every read is fingerprinted, so a re-read reports `changed` with section-level diffs, and `since_last` collapses a re-check to one line (~30 tokens).
- **API/JSON endpoints**: `.json` URLs and JSON responses come back as structured data.
- **Batches**: pass up to 12 URLs; they are read in parallel with the same view, and successful siblings stay available beside individual error rows.

`read_status` names what was returned: `content` (the whole page), `partial` (cut off, or a known subset), `probe` (a `must_contain` answer), `focus`, `section`, `outline`, `unchanged`, or `thin` (a suspected empty shell). Failures use `walled`, `notfound` or `error`. `content_complete` says whether you have the whole page. `next_offset` is a character offset into the rendered markdown, not a page number.

**Walls are answers, not errors to retry.** A wall comes back fast with its kind: `wall.challenge` (naming the vendor), `wall.captcha`, `wall.auth` (a login: fetch carries no session), `wall.paywall`, `wall.blocked`, `wall.empty_shell`, with a `next_action` line. Classified failures return normally with `ok:false`; protocol errors remain JSON-RPC errors.

**Time and freshness (observed).** Median read 0.5-0.6 s on defended corpora; an AWS WAF first contact 2-5 s, then cookie-carried; guards are instant. Dead URLs return the live failure directly; a readable URL path may supply a `suggested_query` for locating a moved copy. `changed` and `since_last` compare stored history: they are not a live revalidation.

Stack Overflow question reads use the public question API with answers and comments; an empty, failed or incompatible API response falls back to the website, and service-declared backoff is honored. Fetch keeps its direct-IP default even with a configured proxy pool; search and crawl keep their pool routing.

`web_screenshot` returns the inline PNG and a private local `path` containing the same bytes; remote clients should use the inline image because the path belongs to the server host.

**Optional unlocker (opt-in).** When a local fetch hits a hard wall, it falls back to Bright Data Web Unlocker once, if a key is configured (`donsetch keys add unlocker "<token>::<Web Unlocker zone>"`). It returns HTML into the normal extraction pipeline; a challenge or empty shell remains a failed fetch. `donsetch doctor --deep` checks token access, zone name and product type for free. Successful responses are cached locally (URL and render-mode keyed, sliding 6h TTL, 200 entries). The daily cap counts every API attempt, including retries, and a timed-out paid request is not replayed automatically. Settings live in `[bypass]`: `donsetch config show`. See the [setup guide](docs/brightdata.md). DonSeTch works without it.

<details>
<summary><b>Walls, measured (home IP, 2026-11)</b></summary>

| Protection | Sites | Result |
|---|---|---|
| AWS WAF, all three shapes | IMDb, Booking, Trustpilot | ✅ the challenge program runs in-process, cold 2-5 s, then cookie-carried |
| Akamai Bot Manager | Amazon | ✅ native `bm-verify` hand-off, ~1.5 s first contact |
| Cloudflare managed challenge | Glassdoor, Indeed, npm, Medium, Product Hunt | ✅ content through the identity Cloudflare lets through; the challenge program itself runs but does not pass yet |
| App shells and walled documents | Reddit, Stack Overflow, crates.io, 4chan, Twitch, NYT, WSJ | ✅ the site's own feed or API (`transport: data`) |
| PerimeterX press-and-hold | Bloomberg, Zillow | ⚠️ passes intermittently by identity, otherwise declared |
| DataDome captcha | Etsy, Yelp | ⛔ declared: it asks a human |
| Interactive captchas | hCaptcha / reCAPTCHA / Turnstile | ⛔ declared without a key (with an unlocker key: ✅) |
| Login walls | x.com, gated docs | ⛔ declared as `wall.auth`: fetch carries no session yet |

From a datacenter IP (what a VPS user has) the same corpus answers about half as often: the egress class decides many walls, which is why `donsetch proxy` exists.

</details>

<div align="center">

<img src="assets/fetch.png" alt="DonSeTch Fetch">

</div>

## 🕷️ Crawl

Same-domain, best-first. Two phases: sitemap discovery (cheap URL inventory), then a Governor-paced frontier walk with extraction per page.

- **Modes**: `full` (map + content), `map` (URL inventory only), `content` (BFS from the seed, no sitemap).
- **Focus-ranked frontier**: `focus="query"` ranks pages by BM25 relevance over link text and URL path, and crawls only matches. No semantic matching before fetch: a link sharing no token with the query is never enqueued.
- **Adaptive pacing**: the Governor paces per (host, lane). 429/503 back off on host signals, host-declared waits (`Retry-After`, robots `Crawl-delay`) are honored in full, and everything self-inferred caps at ~7s. Zero artificial dwell on the fetch path: stealth through truth, never time.
- **Crawl-shape**: frontier pops get a seeded reader-like jitter so repeated crawls never replay one identical, score-eager order into server logs. Ordering only, payloads untouched. Kill switch: `DONSETCH_NO_CRAWL_SHAPE=1`.
- **Resume tokens**: a stopped crawl resumes in one call, valid 30 minutes, survives restarts.
- **Near-duplicate detection**: title plus first 200 chars, hashed.
- **Budgets**: `per_page_max` and `max_total_chars` honor positive character limits down to 1, including concurrent workers. `deadline_s` accepts 1-600 seconds and bounds discovery, page reads, browser handoffs and extraction. A sliced page carries `next_offset`; use `web_fetch` with that offset to continue it.
- **Inventory**: `map_limit` controls returned URLs (default 120, up to 50,000). `inventory.complete=false` discloses output caps, unavailable sitemaps and discovery safety bounds (100,000 URLs, 32 sitemap reads). A map derived from seed-page links cannot establish site-wide completeness. Narrow `focus`/`include_paths` for large sites.
- **Change tracking**: `since_last=true` rechecks sources, then suppresses unchanged pages using per-URL fingerprints from earlier fetches or crawls. Pagination does not prevent recording a full-source fingerprint; partial output never becomes full baseline text.
- **Focus receipts**: page state records whether passages were omitted and the block counts. Dataset JSON Lines use `dataset_version:2`, adding the per-row focus receipt.
- **Honest stop reasons**: `FrontierEmpty`, `MaxPages`, `CharBudget`, `DepthLimit`, `Deadline`, `ThrottledOut`, `Cancelled`, `InventoryLimit`. A drained frontier is complete only when the requested evidence was obtained without failures or excluded pages.
- **Cross-process politeness**: one host-pace store shared by every crawler on the box, so two runs against one site do not double the rate.

Crawl caps apply to each page's rendered markdown and the aggregate page
content. Headers and inventory metadata are separate. A capped page carries
`next_offset` and `content_complete: false`; it never becomes a full-content
history baseline. Full crawl output summarizes the unfetched inventory;
structured `map` retains the URLs. Map mode reports `mode: "map"`, `discovered`,
and `InventoryComplete` when finished, without an empty content-page array.
Map is sitemap-backed when the site has one; without a sitemap it falls
back to the seed page's links.
Continue a stopped crawl using `resume` alone; a new crawl requires `url`.

<div align="center">

<img src="assets/crawl.png" alt="DonSeTch Crawl">

</div>

## 🔎 Keyless search

No API key, no account. Six keyless engines across four independent index families, plus eight official verticals, run in parallel on your machine, then merge, dedupe and rank.

- **Backends**: Bing family (Bing, DuckDuckGo, Yahoo), Brave, Mojeek, Google, plus keyless verticals (GitHub, Wikipedia, HN, Semantic Scholar, arXiv, StackExchange, MDN, Google News).
- **Native Google, browser-free**: HTTP through the legacy mobile endpoint on the existing Rust transport, with seven experimentally verified Nokia profiles (`6230-03.15` by default). A profile that succeeds stays preferred per egress, CAPTCHA advances the cursor circularly, rate limits do not. Retry, pacing and quarantine use the same policy as every other engine. Availability depends on Google and your network; see [configuration and limits](docs/google-wml.md).
- **Semantic reranking**: a local ONNX cross-encoder (`ms-marco-MiniLM-L-6-v2`, 23MB) reads query + title + snippet through full attention, blended 60/40 with RRF + BM25 + consensus. A post-enrichment top-up re-scores close calls using the destination page's real title and description instead of SERP fragments.
- **Consensus**: a URL several independent indexes return gets a boost. Every result carries `score`, `consensus` (independent index families) and `engines`, and the compact surface shows `· N sources`.
- **Learned engine health**: per-engine trust EWMAs and chronic-failure quarantine survive restarts, so dead engines get benched instead of burning fan-out slots on every query.
- **Entity coverage**: anchor entities (B-tree, version numbers, years) are checked against results. Wrong entity, 0.3x.
- **Honest reporting**: `weak=true` means low consensus, per-engine status is always visible, and there is no fake "no results".
- **Warm handoff**: search pre-fetches the top results, so the next MCP fetch serves from cache in ~3ms.
- **Query compiler**: `site:`, `filetype:` and `intitle:` are sent only to engines that honor them, and SERP instant answers are extracted with their source URLs.

Keyless quality, 110 questions across 11 niches with no keys: **95.5%** answer-in-snippet against Tavily's published 93.3% (LLM-graded). Reproduce with `python3 bench/search_quality.py --verbose`; methodology and per-niche numbers live in the script. Search-quality caching for the bench lives in `~/.cache/donsetch/bench-search/`, delete it when comparing binaries.

Search reports `requested_results`, `returned_results`, and `underfilled`
when engines cannot supply the requested count. `weak` considers lexical
query coverage as well as engine agreement; it is a retrieval warning, not a
fact-check. `source_type` identifies video, community, paper, reference,
publisher, official, institutional, and general web sources from known hosts.
It does not certify a page's claims or assign trust to lookalike domains.

### BYOK, optional and never required

Paid providers add rate limits and premium sources on top of the keyless chain.

```bash
donsetch keys add tavily tvly-...       # Tavily
donsetch keys add exa sk-exa-...        # Exa (stackable)
donsetch keys add serper ...            # Serper.dev
donsetch keys add serpapi ...           # SerpApi
donsetch keys add serpbase sb-...       # SerpBase Google SERP (100 free searches)
donsetch keys add bravesearch ...       # Brave Search API
donsetch keys add tinyfish sk-...       # TinyFish (free tier)
donsetch keys add parallel nKil3...     # Parallel AI (fast mode)
donsetch keys add bd "<token>::<SERP zone>"           # Bright Data SERP
donsetch keys add unlocker "<token>::<Unlocker zone>" # Bright Data Web Unlocker
donsetch keys default local             # dispatch order: keyless first
```

- Stack keys per provider: two Exa keys are one 3,000-credit pool. `donsetch keys export/import` moves the store.
- Automatic fallback to keyless when a provider errors or runs dry, per-key rate-limit cooldown and depletion tracking.
- The native adapter list is deliberately short, only the biggest services get one. Everything else belongs in the plugin system below, wired by you, running in the same chain.
- Bright Data SERP, Web Unlocker and the proxy/data products are available at [get.brightdata.com](https://get.brightdata.com/ivqwoicrrlbr) (affiliate link).

<div align="center">

<img src="assets/byok-keys.png" alt="DonSeTch keys list" width="640">

</div>

### Search plugins: any provider, no release needed

Not natively supported is not the same as unsupported. Register any executable that answers a small stdin/stdout JSON contract and DonSeTch treats it like any other provider: default chain, fallback, attribution. Any language works.

```bash
donsetch keys add plugin searxng --cmd 'python3 ~/searxng-adapter.py' --test
```

Request on stdin, response on stdout:

```json
{"format":1,"query":"rust async","max_results":8,"intent":"web","deadline_ms":30000}
{"format":1,"results":[{"title":"...","url":"https://...","snippet":"...","score":0.9}]}
```

Errors: exit non-zero with a message on stderr, or answer `{"format":1,"error":"...","retryable":true,"error_kind":"rate_limited"}`. `error_kind` is optional and says what actually went wrong: `invalid_key`, `credit_depleted`, `rate_limited`, `server_error`, `network_error`. Report one of the first three and the plugin is parked the way a native key in that state is parked, rate-limited for a 60s cooldown then retried, invalid or depleted until you re-register it with `keys add plugin`, and `keys list` shows which. Leave the field out and nothing changes: `retryable` alone still only picks between retrying this search elsewhere and reporting the failure. Constraints that keep it reliable: hard timeout (default 30s, `--timeout` to change), 8 MiB stdout cap, direct exec with no shell, killed on cancellation, and a malformed response degrades gracefully down the fallback chain. Keys belong in the adapter's own environment, never in DonSeTch config.

<div align="center">

<img src="assets/owlsearch.png" alt="DonSeTch Search">

</div>

## 📄 PDF + OCR

PDFs are detected by Content-Type or `%PDF` magic and parsed through a custom PDFium FFI. No external PDF library, no Python subprocess.

> **Glyphs and rendered pixels come from the same content stream, so they are already aligned.** Pixels tell the truth about structure, glyphs tell the truth about text, and the fusion is deterministic: no hallucination.

| Mechanism | What it does |
|---|---|
| Pixel-fusion rule extraction | Tables and borders detected on the rendered bitmap. A rule line is a fact, not a hypothesis. |
| Span detection by ink continuity | A cell spans a separator iff the separator has no ink under it. Deterministic colspan/rowspan. |
| Trust audit + arbitration | The glyph stream is authoritative unless there are zero glyphs plus pixels (a scan) or 30%+ PUA garbage. Corrupt regions get OCR'd even when neighbors read fine. |
| Orientation canonicalization | Vertical and rotated text run one pipeline, with coordinate frames rotated. |
| Confidence honesty | Verbatim glyphs, or OCR with per-line confidence and `[uncertain: …]` markers below threshold. |
| Forms as data | AcroForm widgets become name/type/value triples. |

Tier B, lazy: OCR through the PP-OCR cascade (En → Zh → Deva) for scans and broken ToUnicode pages.

<details>
<summary><b>Battle test results</b></summary>

40-document corpus, zero garbage output, 6-14x faster than the Python alternatives, 120/120 fuzz clean.

| Document type | Result |
|---|---|
| Academic papers | ✅ clean text, math symbols recovered |
| Scanned documents | ✅ OCR'd, confidence-scored |
| Tax forms | ✅ forms as data |
| Multi-column layouts | ✅ reading order preserved |
| Encrypted or corrupt PDFs | ⛔ honest flag with the reason |
| Nepali UDHR (broken ToUnicode) | ✅ 10,542 chars at 86% confidence (pymupdf: 28) |

</details>

## 🛡️ Stealth: real clients, not Chrome-like

A site decides before it serves content on two things: **who is asking** (TLS and HTTP/2 shape, header set and order, client hints, pacing) and **what they already hold** (cookies, clearance). DonSeTch carries both. A fingerprint is never assembled field by field: each identity is one real client's profile, carried whole, and the identity is chosen per site.

| Identity | Profile |
|---|---|
| `plain` | A plain HTTP client: the cheapest first ask |
| `chrome-local` | Chrome 156 on Linux, captured from a real browser |
| `chrome` | Chrome 149 on Linux |
| `chrome-win` | Chrome on Windows |
| `chrome-android` | Chrome on Android |
| `firefox` | Firefox 151 |
| `safari` | Safari 26.4 on macOS |
| `okhttp` | OkHttp 5, the client inside Android apps |

<details>
<summary><b>Verified against the real browsers</b></summary>

Each identity is fingerprinted and diffed against the browser it claims to be. The Chrome identities reproduce real Chrome exactly: JA4 `t13d1517h2_8daaf6152771_cb7bf5808d99`, the same Akamai HTTP/2 fingerprint, the same 17 headers in the same order. Chrome permutes its ClientHello extension order per connection, and so do they, so JA3 varies as the real browser's does. An identity that does not match is removed, not relabelled: a `curl` identity was dropped because it was the native stack with a curl user-agent, a claim no detector would believe.

</details>

**What is not in the bytes:**

| Behavior | Why it matters |
|---|---|
| Cookies per site and identity | A jar per (host, identity), with expiry. Cookies never cross hosts or identities, and a clearance is never spent by a different client class: a fingerprint mismatch burns the request and looks like a wall. |
| Conditional revalidation | A repeat read sends `If-None-Match` / `If-Modified-Since` in Chrome's order. A 304 is what a returning browser gets. |
| Pacing | At least 900 ms between reads of one site and 250 ms between attempts. A burst is a signature even when every request is perfect. |
| Page-load realism | After an HTML read on a site that has walled before, a bounded background burst of the page's own subresources with the page as referer, as a browser loads them (`[fetch] shadow_fetch`). |
| The OS's trust store | The bundled Mozilla roots plus the OS store and `SSL_CERT_FILE` / `SSL_CERT_DIR`, as Chrome trusts the OS: an enterprise inspection root installed the OS's way works. The ClientHello does not change. |

## ⚡ Challenges, in one process

> **No browser fetches content, and no browser earns state for a fetch.** A wall that needs the page's own program gets it run inside the fetch process, by an embedded V8, against a small bounded host. The credential the site issues is spent by the identity whose request minted it.

| Step | What happens | Cost |
|---|---|---|
| 1. Read | The identity's request comes back as a challenge page, named by its vendor | one request |
| 2. Run | V8 runs the page's scripts in document order with its own inline bootstrap, fetching each script as the same identity. The host is a minimal DOM, the browser surfaces those programs read, and WebCrypto on BoringSSL. Nothing is laid out or rendered. | AWS WAF's 690 KB bundle compiles in ~9 ms; the mint is network-bound |
| 3. Spend | The same identity sends the credential and re-classifies the answer. A credential the origin refuses is a failed mint, reported as the wall it is. | one request |
| 4. Reuse | The credential is kept for its learned lifetime and re-minted once when refused; mints are capped and backed off per site. | none |

Measured cold from a home IP, no state: all three AWS WAF shapes (IMDb, Booking, Trustpilot) reach content in 2-5 s, then ride their cookie. Cloudflare's managed challenge is run and does not pass yet, so identity is spent first: the identity Cloudflare lets through answers every Cloudflare site in the corpus.

Hostile pages end bounded and named: an endless loop stops at the 10 s wall clock, a heap bomb in 0.15 s, a 1 GB buffer is refused, a WebAssembly loop stops in 18 ms. Memory is checked on Linux, Windows and macOS; one engine runs at a time, and its memory exists only while a mint runs.

## 🧠 Self-improving fetch

Every read is an action and an observation. Deterministic state, no ML, all local: one compact file under the cache directory (`<cache>/v5/state.json`), with cached pages beside it.

| It learns | So that |
|---|---|
| Which identity each site accepts | The next read starts there. It stays until it fails; scores halve after 32 attempts, so a site that changed is believed. |
| What each bot-wall vendor lets through | Learned across sites over a measured seed: a new site behind a known vendor starts with the identities that vendor passes. |
| The route that answered | A site whose public interface answered is asked there first. |
| Published limits | A 429 with `x-ratelimit-reset` becomes a cooldown: the next read answers in ~150 ms naming the wait owed, instead of getting blocked. |
| Minted credentials | Reused for their learned lifetime, re-minted once when refused. |
| How far each site's cache headers can be trusted | Validators are audited by occasional full reads; a site whose validators lie is re-read in full for 7 days, and a page that changes inside its declared lifetime is always re-checked. |

Measured with persisted state on a hard corpus of 28 defended sites: first-try content **13/28 cold → 26/28 learned**, requests per round 48 → 29, median 1.1 s → 0.63-0.85 s. On a friendly corpus of 17 sites: 10/17 → 15-16/17.

The state is bounded (4,096 sites, least recently used go first with their cookies and credentials) and written by merging into the file under a lock, so processes sharing it keep each other's learning. Search engine trust, domain quality priors and crawl pacing ladders learn the same way. Receipts in `donsetch status` and `donsetch doctor --improve`; `DONSEEK_NO_DISK_STATE=1` keeps all of it in memory.

## 🏗️ Built from scratch

Every layer in Rust.

| Component | What | Where |
|---|---|---|
| 🛡️ **v5 core** | The fetch engine: client identities, learned state, page cache, wall classification, the site's own interfaces, the in-process V8 challenge engine | `v5core/` |
| 📝 **DonSift** | HTML → markdown, block model, 12-language BM25, the page views (`must_contain`, `focus`, `toc`, `section`) | `src/extract/` |
| 🔎 **DonSeek** | Keyless multi-engine search, RRF + BM25 + consensus + semantic reranking | `src/search/` |
| 🕷️ **DonTread** | Crawl engine, sitemap, focus frontier, Governor pacing, resume tokens | `src/crawl/` |
| 📄 **DonSheet** | PDF extraction, PDFium FFI, pixel-truth fusion, OCR cascade, forms | `src/pdf/` |
| 📸 **Browser** | Screenshots and `donsetch login` only; never a page read | `src/browser/` |
| 🔌 **MCP daemon** | stdio + HTTP servers, JSON-RPC 2.0, four tools, crash-only supervisor | `src/mcp/` |

1,900+ tests. Zero clippy warnings: `cargo clippy --all-targets --features ocr,rerank -- -Dwarnings` is the law, and CI runs the full matrix on Linux, macOS and Windows.

## ⚙️ Configuration

Every runtime knob lives in one typed config (`src/config.rs`), layered, later wins:

1. **Compiled defaults.** A bare `donsetch mcp` stays the law: zero config needed.
2. **Legacy env vars** (the pre-v4 names, `DONSETCH_NO_CRAWL_SHAPE` say). Honored exactly as before, reported as deprecated by `doctor`.
3. **`donsetch.toml`** at `<config-dir>/donsetch/donsetch.toml`, or anywhere via `DONSETCH_CONFIG=/path/file.toml`. Unknown keys and bad values are hard errors naming the file. `DONSETCH_NO_CONFIG_FILE=1` skips the file layer (setting both is an error).
4. **New env names**: `DONSETCH_<SECTION>__<KEY>`, so `DONSETCH_FETCH__PDF_MAX_MB=25`.

Sections: `transport`, `mcp`, `paths`, `state`, `proxy`, `tls`, `persona`, `cli`, `fetch`, `bypass`, `search`, `browser`, `debug`, `rules`.

```toml
[fetch]
shadow_fetch = "never"  # or "auto" / "always": the page-load burst
pdf_max_mb = 100
dns_cache_ttl_secs = 30 # seconds a resolved name is reused in-process, 0 disables

[bypass]
max_daily = 100
```

Booleans take `true`/`false`, enums take their listed spellings.

- `donsetch config show` prints every knob with its value **and its origin** (default, legacy, file, env).
- `donsetch config show --markdown` prints the full reference table, one row per knob.
- `donsetch config show --legacy` maps every old env name to its config key.
- `donsetch doctor` warns about the legacy vars active in your shell, mapped to their keys.

### Local rules: deny a site

Some sites treat any automated traffic as hostile, however well it is disguised. The block lands on your IP, sometimes on your whole network, outlasts the session that caused it, and also locks you out in your own browser. A note in the agent's instructions file ("never fetch site X") helps only if the model reads it and obeys it every time, and one forgotten turn is enough to get banned. [Pick the right tool](#-pick-the-right-tool) covers which task shapes draw these blocks. Local rules enforce your answer inside DonSeTch, whatever the model decides.

Per-host rules in `donsetch.toml` can refuse a URL before any request, handing the agent your own instructions in place of the page. DonSeTch ships none; the table is empty until you write one.

```toml
[rules.url."example.org"]        # the host and every subdomain, http and https
action  = "deny"
reason  = "ip_ban"               # error code policy.denied.ip_ban
message = "do not fetch this site with curl, scripts or another HTTP client: it bans our IP for automated access. Find file elsewhere, or ask the human operator to download it"
```

`web_fetch`, `web_crawl` and `web_screenshot` then refuse that site, redirects and cached copies included, and the agent reads `message` as its `Next action:`, so lead with what to do instead, and say why when the reason rules out workarounds. The most specific matching rule wins in full: a narrower allow rule also cancels a broader deny. `donsetch rules test <url>` shows which rule a URL gets and why, offline.

Rules are a guardrail, not a security boundary: no MCP argument can switch them off, but an agent with a shell can run `donsetch fetch --ignore-rules`, set `DONSETCH_RULES__MODE=off`, or edit `donsetch.toml`. Pattern grammar, load errors, precedence, what the agent sees and the known limits: [local rules](docs/rules.md).

## 💻 CLI

Thin adapter over the same engine the MCP server uses.

| Command | What it does |
|---|---|
| `donsetch fetch <url>` | Fetch as clean markdown (`--must-contain`, `--focus`, `--toc`, `--section`, `--max-chars`, `--since-last`, `--json`) |
| `donsetch search <query>` | Search, keyless + BYOK (`--intent`, `--max-results`) |
| `donsetch crawl <url>` | Crawl (`--mode map\|full\|content`, `--topic`, `--max-pages`) |
| `donsetch screenshot <url>` | Render to PNG (`--out`) |
| `donsetch mcp` | MCP server (stdio, or `--http --port N`) |
| `donsetch doctor` | Health check and auto-fix (`--deep`, `--json`, `--fix`) |
| `donsetch status` | Version, keys, proxies, cache, health overview (`--json` for scripts and agents) |
| `donsetch keys` | BYOK providers and plugins (`add`, `list`, `default`, `export`) |
| `donsetch proxy` | Proxy management (`add`, `list`, `check`, `test`, `import`, `export`, `remove`, `clear`, `fetch on\|off`, `crawl on\|off`) |
| `donsetch login` | Store a signed-in session in the private vault (`--list`, `--status`, `--logout`, `--import`); fetch does not send it yet |
| `donsetch config` | `show`, `--markdown`, `--legacy` |
| `donsetch rules test <url>` | Explain what the local rules do with a URL, offline |
| `donsetch tools` | Tool schemas as JSON, same as MCP `tools/list` |
| `donsetch update` / `rollback` | Self-update from GitHub Releases, and revert |

Every management command validates its flags: an unknown one is refused with the nearest known match and exits 1. A usage error is never a silent run. The `proxy` help and its empty-pool state carry one labelled sponsor line (Proxy-Seller); `DONSETCH_NO_SPONSOR=1` removes it, and it never appears in `--json`, `proxy export`, or any MCP result or tool definition.

## 🔀 Proxies

Adding a proxy is one command. Nothing else is required.

```bash
donsetch proxy add socks5://user:pass@1.2.3.4:1080   # validated, probed, then saved
donsetch proxy list                                  # what is configured (credentials masked)
donsetch proxy check                                 # live probe: latency + real exit IP
```

- **Formats**: `http://host:port` (HTTP CONNECT, credentials optional as `user:pass@`), `socks5://host:port` and its `socks5h://` alias (SOCKS5 sends the target as a name, so the proxy resolves DNS and your resolver never sees the host). A bare `host:port` is HTTP CONNECT. Anything else is refused with the reason.
- **Many at once**: `donsetch proxy add url1 url2 url3`, or a whole file with `donsetch proxy import proxies.txt` (one URL per line, `#` comments) — `donsetch proxy export` writes that file back out. `--no-check` skips the probe when you are adding hundreds.
- **Try before you add**: `donsetch proxy test <url>` probes one proxy without saving it.
- **Where they live**: `donsetch proxy add` appends `<cache-dir>/proxies.txt` (mode 0600, credentials included). `DONSEEK_PROXIES="url1,url2"` in the environment and `[proxy] pool = ["url1", "url2"]` in `donsetch.toml` are the other two doors; all three merge, and an environment/config entry replaces a file entry with the same `host:port`. The pool is read at startup, so restart the MCP server to pick up a change.
- **Removing**: `donsetch proxy remove 1` (the index `proxy list` prints, or `host:port`) and `donsetch proxy clear`.
- **Checking is a tunnel, not a TCP handshake**: the probe connects through the proxy to `api.ipify.org` and prints the exit IP, so a proxy that accepts the connection and then cannot carry traffic is reported dead. `donsetch status` shows the count, `donsetch doctor` shows every lane and its health.

### What uses the pool, and what does not

A proxy you configure is a proxy that sees your traffic, so nothing uses one by accident.

| Tool | Default | Off switch |
|---|---|---|
| `web_search` | always rotates the pool: one lane per engine, sticky per host, so a fan-out search does not hit nine engines from one IP | — |
| `web_crawl` | each host starts on the direct lane; its later requests spread across the pool lanes as their pacing allows, and a host that blocks or throttles direct moves off it | `donsetch proxy crawl off` |
| `web_fetch` | your own IP | `donsetch proxy fetch on` |

`donsetch proxy fetch on` makes `web_fetch` sticky to a pool lane per host, rotating on 429/407/dead/timeout. A one-shot read does not need a shared exit, which is why fetch is opt-in while search and crawl are not.

Crawl is where the first request decides things: a host that bans on contact has already seen your own IP before a lane can take over. When a site must never see your address at all, keep it off entirely with a **[local rule](#local-rules-deny-a-site)** rather than leaning on the pool.

### Riding your own proxy network

Set the environment and every fetch follows the curl convention:

- `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` (any case) route fetches through that proxy, and `NO_PROXY` exempts hosts (suffix match, `*`, `host:port`, CIDR). `DONSETCH_NO_ENV_PROXY=1` disables the convention entirely. `[proxy] http` / `https` / `all` in `donsetch.toml` do the same thing without environment variables.
- A configured proxy is a promise: if it cannot be dialled the fetch fails and says so, instead of quietly leaving on your own IP. Same for a pool lane: a dead lane is benched, not replaced by a direct read of a host it was carrying.
- In an intercepting network (a TLS-terminating middlebox), trust its root the OS's way, or with `SSL_CERT_FILE` / `SSL_CERT_DIR` as with curl: both are added to the bundled roots. The handshake stays the identity's own. `donsetch doctor` reports the whole posture.
- The challenge engine leaves through the same proxy as the read it is answering.

## 🔐 Logged-in sessions

**In v5, `web_fetch` never signs in and never sends a stored session.** A page behind a login (x.com, gated docs, internal tools) comes back as a declared `wall.auth`, not as content, and no cookie a browser harvested is ever spent by a fetch. Session support for fetch is planned as its own feature.

`donsetch login` still stores sessions in the private vault, without ever seeing your credentials, ready for that feature:

```bash
donsetch login x.com          # opens YOUR browser, you sign in, press Enter
donsetch login --list         # stored sessions (names and counts only)
donsetch login --status x.com # one domain, in detail
donsetch login --logout x.com # forget a domain
donsetch login --import cookies.txt x.com   # servers and CI, Netscape format
```

- Credentials never enter DonSeTch. It opens a real Chromium on your display in a dedicated profile, never the automation one. You type into the browser. No keystroke capture, no screenshots, no CDP attach until you press Enter.
- Afterwards, cookies selected by your domain or explicit import are stored regardless of whether their names look like authentication tokens. Known analytics and clearance cookies, expired records and invalid values are excluded. Names and paths retain their identity, including opaque persistent names such as `c_user`, `xs` and `li_at`. Cookie values stay in the private vault; `Saved`/`Imported` lists names only. Captures from older browsers cannot overwrite the replacement session.
- A named-domain post-login probe checks reachability and reports login/access redirects as unverified. A public home-page HTTP 200 cannot prove authentication, so it never earns a verified checkmark. Multi-site saves avoid one probe per domain.
- Failed writes and captures return an error instead of reporting success. EOF and Ctrl+C save nothing to the vault; `--fresh` waits for the exclusive profile lock before wiping it. The dedicated browser profile may retain its own cookies until cleared with `--fresh`.
- The registry (`auth-state.json`) stores metadata only: names, counts, expiries, probe verdicts. Never values.
- Multi-site: run bare `donsetch login`, sign into as many tabs as you like, press Enter once. New cookies and rotated values are captured, including opaque persistent cookies.

## 🐳 Docker

```bash
docker build -t donsetch-mcp .
docker compose up -d                 # loopback-only by default
```

Multi-stage build, non-root user, optional Chrome, resource limits in the compose file. An opt-in `http` profile serves the HTTP transport with a healthcheck, and `docker compose stop` gives in-flight browser work a 45s grace period.

## 🧭 Pick the right tool

DonSeTch is a **rapid-fire research tool**: search, read, verify. A search, a fetch or two, a docs page, one PDF. Its speed is the point, and that speed is its stealth for one-shot reads.

It is NOT built for tasks where an agent "works through" a defended site the way a person would:

- **Bulk document harvesting**: discovering and downloading many PDFs from one repository in a single run.
- **Long sessions against one site**: page after page, at machine speed, same IP, no human pauses.
- **Mass extraction**: mirroring a file library, collecting a dataset, systematic downloads.

DonSeTch will *probably* work on those, and nothing stops it. But every request fires hundreds of times faster than a human, and a defended site reads that pattern itself as a bot, not just the fingerprint. The realistic risk is an IP-level block, sometimes on your whole network mid-run. When that happens it is the task shape, not a fetch-layer failure: the same page fetched once, as a research read, is fine.

For that other shape of work, use **[Bladebro](https://github.com/dondai44423/bladebro)**: a real browser doing what a human does, page by page, download by download, at a human's pace. Slower than DonSeTch by design, because over a long session against a defended site, looking human beats being fast.

**Rule of thumb: one-shot research = DonSeTch. Working a defended site like a person to collect things = Bladebro.**

## ⚠️ Gotchas & honest limits

| Surprise | Why |
|---|---|
| First build takes a few minutes | BoringSSL compiles from source and V8 is downloaded and linked, cached after. Go is a build dependency too: BoringSSL's build system is Go-based. |
| v5 is a beta | The fetch engine is new. Its suite runs on Linux and on Windows (the core suite runs there on every push); the macOS builds compile from the same code and are verified on real machines at release. Report anything that regressed from v4. |
| A login wall is an answer | `web_fetch` carries no session in v5: a signed-in page returns `wall.auth`. |
| OCR and rerank are not in the default build | ONNX Runtime is heavy and optional: `--features ocr,rerank`. Prebuilts ship them on linux-x64, macOS-arm64, macOS-x64 and Windows-x64. |
| First OCR/rerank use downloads models | ~24MB reranker, ~37MB OCR, cached forever. |
| Captchas need an unlocker key | hCaptcha, reCAPTCHA, Turnstile and press-and-hold walls ask a human; they are not solved locally, by design. With `donsetch keys add unlocker <key>[::zone]` they come through rendered; without one you get a clear honest error, never a hang. |
| robots.txt is ON for crawl | `respect_robots=true` for crawl. `fetch` does not check robots. |
| Keyless search rate-limits without a proxy | It hits engines from your IP. Set `DONSEEK_PROXIES` for heavy use. |
| Rerank in a CPU-limited container | Auto-clamped to cgroup parallelism on Linux, `DONSEEK_RERANK_THREADS` to override. |

| It cannot | Why |
|---|---|
| Solve interactive captchas locally | hCaptcha, reCAPTCHA, Turnstile: an honest dead end, no solving service by design. |
| Render a page | Nothing is laid out or painted in the fetch path. A page whose content exists only after rendering, with no interface behind it, is reported as a shell. |
| Search with every engine down | An error with per-engine status. Honest, never fake. |
| Replace a human working a defended site | See "Pick the right tool" above. |

## 🆚 How it compares

The honest summary: nothing else in this space combines browser-free challenge handling, real client identities, keyless search, PDF pixel-fusion and local-only operation in one binary.

| | **DonSeTch** | Hound | Crawl4AI | Jina Reader | Firecrawl |
|---|---|---|---|---|---|
| Language | **Rust, one binary** | Python | Python | Python (API) | TypeScript |
| TLS fingerprint | **8 whole client profiles, chosen per site** | curl-impersonate | requests | their servers | their servers |
| JS challenges | **site's own program in embedded V8, no browser** | browser fetches all | browser fetches all | their servers | their servers |
| Search | **keyless, 10+ engines, local rerank** | keyed | no | yes | no |
| Crawl | **yes, resume tokens** | yes | yes | no | cloud only |
| PDF + OCR | **pixel-fusion + PP-OCR** | yes | partial | yes | cloud, paid |
| Self-improving routing | **yes, local state** | no | no | no | no |
| Runs locally, no account | **yes** | yes | yes | no | self-host or paid |
| MCP server | **first-class** | yes | community | yes | build it |
| Tool schema | **~2.0k tokens** | ~2.7k | varies | n/a | varies |
| License | AGPL v3 | MIT | Apache 2.0 | proprietary | MIT |

**Against Firecrawl, head to head, same tasks, live** (measured with v4; v5 numbers are coming). Firecrawl is a paid cloud API, DonSeTch is free, local and keyless:

- **Fetch**: Wikipedia comes back 16x smaller (16KB against 267KB). An arXiv PDF is 22x faster and 4.4x smaller (1.4s/16KB against 32.6s/71KB). Reddit: Firecrawl refuses the site, DonSeTch returns the real feed.
- **Crawl**: 2.5x faster on the same docs target (18.7s against 47.6s), with `--topic` ranking the frontier instead of firehosing, and an honest small failure when the topic has no match instead of verbose unrelated content.
- **Search**: Firecrawl is genuinely faster (1-2s against 5-7s) and leans mainstream authority. DonSeTch leans technical specificity, matched or beat it on exact GitHub issues, and costs far fewer tokens per query.

## 🤝 Contributing

PRs welcome, see [CONTRIBUTING.md](CONTRIBUTING.md). Before submitting: `just check`, `just t <scope>` for the area you touched, and `just lint` (clippy with `-Dwarnings`). CI runs the full matrix on three platforms. AGPL v3: all contributions land under the same license.

## 📄 License

Copyright (c) 2026 Bishesh Bhandari. AGPL-3.0, see [LICENSE](LICENSE).

---

<div align="center">

### If DonSeTch saves you time, ⭐ the repo

[![Stars](https://img.shields.io/github/stars/dondai44423/donsetch?color=ff9f43&style=flat-square)](https://github.com/dondai44423/donsetch)

**AGPL v3** · [Changelog](CHANGELOG.md) · [Issues](https://github.com/dondai44423/donsetch/issues) · [Releases](https://github.com/dondai44423/donsetch/releases)

</div>
