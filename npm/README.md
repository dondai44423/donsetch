# DonSeTch

**The web, for AI agents.** Fetch, search, crawl and screenshot from one local binary. Zero API keys, zero accounts.

**v5:** a new fetch engine. No browser in the fetch path: a wall is answered by the right client identity, the site's own challenge program run in-process, or the site's own public interface.

[![Release](https://img.shields.io/github/v/release/dondai44423/donsetch?color=00d4aa&style=flat-square)](https://github.com/dondai44423/donsetch/releases)
[![CI](https://img.shields.io/github/actions/workflow/status/dondai44423/donsetch/ci.yml?label=CI&style=flat-square)](https://github.com/dondai44423/donsetch/actions/workflows/ci.yml)
[![npm downloads](https://img.shields.io/npm/dm/donsetch?color=cb3837&style=flat-square&label=downloads)](https://www.npmjs.com/package/donsetch)
[![GitHub stars](https://img.shields.io/github/stars/dondai44423/donsetch?style=flat-square&color=e3b341)](https://github.com/dondai44423/donsetch/stargazers)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-00d4aa?style=flat-square)](https://github.com/dondai44423/donsetch/blob/master/LICENSE)

![DonSeTch, the web, for AI agents](https://raw.githubusercontent.com/dondai44423/donsetch/master/assets/herobanner.png)

DonSeTch gives any AI agent full web research from a single local process: four tools, one Rust binary, no API keys, no accounts, nothing to configure. Fetching launches no browser (no Playwright, no Selenium, no headless Chrome): pages are read as a real client reads them, at curl's cost, and all four tool schemas fit in ~2k tokens.

Works with every MCP client (Claude Code, Cursor, OpenCode, Pi, Hermes) and as a standalone CLI.

## Why it's different

| | What it does |
|---|---|
| **Real clients, whole** | Eight client identities (Chrome on Linux, Windows and Android, Firefox, Safari, an Android app client, a plain client), each carrying one real profile's TLS, HTTP/2, header order and client hints, chosen per site. The Chrome identities match real Chrome's JA4 and HTTP/2 fingerprint exactly. |
| **Challenges, in one process** | A wall that needs the page's own program gets it run in an embedded V8 against a small bounded host, and the credential it mints is spent by the same identity. AWS WAF passes cold in 2-5 s. No browser, no rendering. |
| **Keyless search** | 10+ backends in parallel, fused by cross-engine consensus plus local semantic reranking. No keys, $0 forever. BYOK is optional. |
| **Pixel-fusion PDF** | Glyphs and rendered pixels come from the same stream and are fused deterministically, with a per-region trust audit. Scanned PDFs auto-OCR. |
| **Read only what you need** | `must_contain` answers "is X on this page?" per term with counts, sections and excerpts in a few hundred characters; `focus` returns the passages that answer, best first; `toc` then `section` reads one part. Every view searches the whole page, PDFs and site adapters included. |
| **Self-improving fetch** | Remembers which identity each site accepts, what each bot-wall vendor lets through, which route answered and how far each site's cache headers can be trusted. Hard-corpus first-try content: 13/28 cold, 26/28 learned. All local state, receipts in `status` and `doctor --improve`. |
| **One command to verify** | `donsetch doctor` sweeps config, search health, egress, TLS, browser, DNS, secret permissions, and fixes what is mechanically fixable. |

## Install

```bash
npm install -g donsetch
```

Optional Bright Data setup: [SERP API and Web Unlocker guide](https://github.com/dondai44423/donsetch/blob/master/docs/brightdata.md).
Use separate zones and add the exact dashboard names:

```sh
donsetch keys add brightdata "<token>::<SERP zone>"
donsetch keys add unlocker "<token>::<Web Unlocker zone>"
donsetch doctor --deep
```

Downloads the prebuilt binary for your platform from [GitHub Releases](https://github.com/dondai44423/donsetch/releases) with SHA256 verification. No build tools needed.

| Platform | Asset | OCR + rerank |
|---|---|---|
| Linux x86_64 (glibc >= 2.35) | `donsetch-linux-x64.tar.gz` | yes |
| Linux ARM64 (glibc >= 2.35) | `donsetch-linux-arm64.tar.gz` | no (core only: fetch, search, crawl, PDF; ONNX has no working aarch64 runtime) |
| macOS Apple Silicon (>= 13.4) | `donsetch-darwin-arm64.tar.gz` | yes |
| macOS Intel (>= 13.4) | `donsetch-darwin-x64.tar.gz` | yes |
| Windows x86_64 | `donsetch-win32-x64.tar.gz` | yes |
| Windows ARM64 | same x64 asset, under emulation | yes |

**Verify the install:** `donsetch doctor` (fast local sweep), `doctor --deep` (adds live browser and egress probes), `doctor --fix` (repairs mechanical problems), `doctor --json` (machine-readable, also prints ready-to-paste MCP registration blocks for your client).

**If something goes wrong:**

- **pnpm or bun:** the postinstall needs approval. `pnpm approve-builds` (or the bun equivalent), then reinstall. If scripts were blocked, `npx donsetch` runs the self-healing shim.
- **`--ignore-scripts`:** postinstall is intentionally skipped. Run `node node_modules/donsetch/install.js`, or use `npx donsetch` to download the binary when network access is available.
- **System-wide installs:** if the npm prefix is root-owned (for example `/usr/local/lib/node_modules`) and you run donsetch as a normal user, the package copy cannot be written there. donsetch then uses an existing binary from `PATH` or `~/.cargo/bin` automatically; `DONSETCH_BINARY=/path/to/donsetch` points at one explicitly. To keep the package copy instead: `npm config set prefix ~/.npm-global && npm install -g donsetch`.
- **Proxy:** set `HTTPS_PROXY` (or `https_proxy`, `HTTP_PROXY`, `http_proxy`) to an HTTP CONNECT proxy.
- **Release mirror:** set `DONSETCH_RELEASES_BASE` to a mirror serving `<tag>/<asset>` paths, for example `https://mirror.example/donsetch/releases`.
- **Windows:** the installer needs `tar`, included since Windows 10 1803.
- **musl/Alpine:** published Linux binaries are glibc. Build from source on musl.
- **First OCR or search run:** it downloads the models (~24MB reranker, ~37MB OCR) and caches them forever.

## Quickstart

### 1. As an MCP server (for agents)

```json
{
  "mcpServers": {
    "donsetch": { "command": "donsetch", "args": ["mcp", "--supervised"] }
  }
}
```

`--supervised` is the crash-only daemon: a panic becomes a blip, the daemon restarts, the session survives. Without a global install, use `"command": "npx", "args": ["donsetch", "mcp"]`.

Prefer HTTP over stdio? `donsetch mcp --http --port 8765`, clients connect to `http://localhost:8765/mcp`. Sessions, cancellation, `/health`, token auth (`DONSETCH_HTTP_TOKEN`) and per-request timeouts are documented in `donsetch mcp --help`.

Every client gets the `[meta]` fold by default: compact state and source evidence arrive together in `content`, including for unknown bridges and OpenCode's generic `cli` handshake. Raw MCP returns content blocks; some bridges join them into a string. Parse the leading `[meta]` JSON line, then read the source text. To get the split result instead (`structuredContent` kept) for clients DonSeTch does not recognize, set `[mcp] text_only = false`, or `DONSETCH_MCP__TEXT_ONLY=false` in the environment; details in the [full README](https://github.com/dondai44423/donsetch#readme).

### 2. As a CLI (for humans and scripts)

```bash
donsetch fetch https://example.com/pricing --must-contain "free tier"
donsetch search "rust async patterns" --intent code
donsetch crawl https://docs.python.org --mode map --topic asyncio
donsetch screenshot https://example.com --out page.png
donsetch doctor
donsetch update
```

## The 4 tools

| Tool | What it does |
|---|---|
| `web_fetch` | Any URL as clean markdown, or only the part that answers: `must_contain` probes, `focus` passages, `toc` → `section`. Walls answered in-process, never by a browser. PDFs with OCR and per-page confidence, site adapters, page memory, batches of up to 12. |
| `web_search` | Keyless multi-engine search: 10+ backends, consensus plus semantic reranking, query-aware official-source placement. Ranked URLs and snippets, never a scraped article dump. |
| `web_crawl` | Best-first same-domain crawl. Sitemap plus frontier, `focus` ranking, elastic pacing, resume tokens, honest stop reasons. |
| `web_screenshot` | Rendered PNG of any URL through DonSeTch's browser (used only for screenshots and login), with the usual URL safety guards. |

MCP results default to self-contained text: leading `[meta]` JSON followed by source evidence, including for unknown client bridges. PNGs remain image blocks.

Every failure is structured: a stable `code` (`wall.challenge`, `guard.ssrf`, `deadline.hit`, `network.dns`…), an `errorKind` (`permanent`, `transient`, `walled`) and a `next_action`, so agents branch on codes instead of parsing prose.

## Highlights

- **Search without keys.** Six keyless engines across four independent index families plus eight official verticals (GitHub, Wikipedia, HN, Semantic Scholar, arXiv, StackExchange, MDN, Google News), merged by consensus and re-ranked locally by an ONNX cross-encoder. 95.5% answer-in-snippet over 110 questions across 11 niches with no keys at all.
- **PDFs done properly.** A custom PDFium FFI, no Python subprocess. Tables and borders come from the rendered bitmap, text from the glyph stream, and the trust audit flags exactly the regions that need OCR.
- **Crawl with manners.** Per-host adaptive pacing that honors `Retry-After` and robots `Crawl-delay`, cross-process politeness so two crawls do not double a site's rate, near-duplicate detection, resume tokens that survive restarts, and honest stop reasons.
- **Fetch that answers honestly.** `content_ok`, `thin`, `changed` with section diffs, and `must_contain` says `unverified` instead of a false "not found" when part of a page was unread. No fake success.
- **Walls named, not guessed.** A bot wall names its vendor and what it asks for; a login wall is a declared `wall.auth` (fetch carries no session in v5; session support is planned).
- **Works everywhere.** Linux, macOS, Windows. npm, Homebrew, the Pi agent as a native extension, and the DeepSeek Harness as a first-class plugin.

## Sponsors

DonSeTch is free and open source, and stays that way. Sponsorship pays for the time it takes to keep shipping.

| Tier | Price | What you get |
|---|---|---|
| Bronze | $10/mo | Name + link in the Sponsors section |
| Silver | $25/mo | Small logo + link in the Sponsors section |
| Gold | $49/mo | Large logo + link, pinned at the top of the Sponsors section |

Prepaid monthly, cancel anytime. One-time sponsorships are welcome at any amount.

Pricing goes up as the project grows. It is early now, so a Gold at $49/mo is near-zero investment for any company whose product touches agent web research. If your product is part of this space (proxy platforms, search infrastructure, BYO providers, anything a DonSeTch user would plug in), Gold goes one step further: fit natively inside DonSeTch and you get the placement plus an official integration shipped in the binary itself.

Email **bhandaribishesh879@gmail.com** to become a sponsor.

## Docs

- **Full README, install matrix, configuration reference:** [github.com/dondai44423/donsetch](https://github.com/dondai44423/donsetch)
- **Every config knob with its origin:** `donsetch config show` (or `--markdown` for the whole table)
- **Changelog:** [CHANGELOG.md](https://github.com/dondai44423/donsetch/blob/master/CHANGELOG.md)
- **Issues and requests:** [github.com/dondai44423/donsetch/issues](https://github.com/dondai44423/donsetch/issues)
- **Contributing:** [CONTRIBUTING.md](https://github.com/dondai44423/donsetch/blob/master/CONTRIBUTING.md)
- **Pi agent:** `pi install npm:donsetch` registers the tools as native pi tools and self-updates.
- **DeepSeek Harness:** `dsh plugin --profile web add github:dondai44423/donsetch-dsh`

## License

AGPL-3.0. Copyright (c) 2026 Bishesh Bhandari.

If DonSeTch saves you time, [star the repo](https://github.com/dondai44423/donsetch) or [sponsor it](https://ko-fi.com/G5Y624N5RE).
