# DonSeTch

**The web, for AI agents.** Fetch, search, crawl and screenshot from one local binary. Zero API keys, zero accounts.

[![Release](https://img.shields.io/github/v/release/dondai44423/donsetch?color=00d4aa&style=flat-square)](https://github.com/dondai44423/donsetch/releases)
[![CI](https://img.shields.io/github/actions/workflow/status/dondai44423/donsetch/ci.yml?label=CI&style=flat-square)](https://github.com/dondai44423/donsetch/actions/workflows/ci.yml)
[![npm downloads](https://img.shields.io/npm/dm/donsetch?color=cb3837&style=flat-square&label=downloads)](https://www.npmjs.com/package/donsetch)
[![GitHub stars](https://img.shields.io/github/stars/dondai44423/donsetch?style=flat-square&color=e3b341)](https://github.com/dondai44423/donsetch/stargazers)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-00d4aa?style=flat-square)](https://github.com/dondai44423/donsetch/blob/master/LICENSE)

![DonSeTch, the web, for AI agents](https://raw.githubusercontent.com/dondai44423/donsetch/master/assets/herobanner.png)

DonSeTch gives any AI agent full web research from a single local process: four tools, one Rust binary, no API keys, no accounts, nothing to configure. The transport is built from scratch (no hyper, no Playwright, no Selenium), which is why the fetch tier is fast *and* stealthy and the tool schema fits in ~2.4k tokens.

Works with every MCP client (Claude Code, Cursor, OpenCode, Pi, Hermes) and as a standalone CLI.

## Why it's different

| | What it does |
|---|---|
| **Real Chrome TLS** | Drives Chrome's own BoringSSL natively: your ClientHello IS Chrome's, ML-DSA signature algorithms included. Emergent from the real engine, not a faked table that rots. |
| **Temporal stealth** | TLS session resumption, 304 revalidation, persistent cookies, connection pooling, TCP Fast Open. The loudest remaining bot tell, and nobody else fakes it. |
| **Solve-and-bounce** | The browser solves the challenge and hands cookies to tier 1, which then fetches at full speed. The browser almost never fetches content. |
| **Keyless search** | 10+ backends in parallel, fused by cross-engine consensus plus local semantic reranking. No keys, $0 forever. BYOK is optional. |
| **Pixel-fusion PDF** | Glyphs and rendered pixels come from the same stream and are fused deterministically, with a per-region trust audit. Scanned PDFs auto-OCR. |
| **Token control** | Links render as `[text](L12)` and results as `S1…Sn`, so `fetch S3` costs 3 tokens instead of 80. `focus`, `toc`, `section`, `must_contain` and `since_last` cut a page to what the agent actually needs. |
| **Self-improving fetch** | Cookie lifetimes adapt, walls that beat a real browser twice go into cooldown, searches pre-solve known walls. All local state, receipts in `status` and `doctor --improve`. |
| **One command to verify** | `donsetch doctor` sweeps config, search health, egress, TLS, browser, DNS, secret permissions, and fixes what is mechanically fixable. |

## Install

```bash
npm install -g donsetch
```

Downloads the prebuilt binary for your platform from [GitHub Releases](https://github.com/dondai44423/donsetch/releases) with SHA256 verification. No build tools needed.

| Platform | Asset | OCR + rerank |
|---|---|---|
| Linux x86_64 (glibc >= 2.35) | `donsetch-linux-x64.tar.gz` | yes |
| Linux ARM64 | `donsetch-linux-arm64.tar.gz` | no, and PDF is fragile (ONNX has no working aarch64 prebuilt) |
| macOS Apple Silicon | `donsetch-darwin-arm64.tar.gz` | yes |
| macOS Intel | `donsetch-darwin-x64.tar.gz` | no (ONNX has no working x64 prebuilt) |
| Windows x86_64 | `donsetch-win32-x64.tar.gz` | yes |
| Windows ARM64 | same x64 asset, under emulation | yes |

**Verify the install:** `donsetch doctor` (fast local sweep), `doctor --deep` (adds live browser and egress probes), `doctor --fix` (repairs mechanical problems), `doctor --json` (machine-readable, also prints ready-to-paste MCP registration blocks for your client).

**If something goes wrong:**

- **pnpm or bun:** the postinstall needs approval. `pnpm approve-builds` (or the bun equivalent), then reinstall. If scripts were blocked, `npx donsetch` runs the self-healing shim.
- **`--ignore-scripts`:** postinstall is intentionally skipped. Run `node node_modules/donsetch/install.js`, or use `npx donsetch` to download the binary when network access is available.
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

If your client shows only half of each result (tool metadata but no page text, or text but no citable URLs), it is dropping one of the two MCP result surfaces. `DONSETCH_MCP_TEXT_ONLY=1 donsetch mcp` forces the `[meta]` fold that fixes it for every client.

### 2. As a CLI (for humans and scripts)

```bash
donsetch fetch https://example.com --focus "pricing"
donsetch search "rust async patterns" --intent code
donsetch crawl https://docs.python.org --mode map --topic asyncio
donsetch screenshot https://example.com --out page.png
donsetch doctor
donsetch update
```

## The 4 tools

| Tool | What it does |
|---|---|
| `web_fetch` | Any URL as clean markdown. HTTP first, escalates to a headless browser on bot walls. PDFs with OCR and per-page confidence, `focus` / `toc` / `section`, pagination, `actions` for in-page control, `must_contain` probes, `archive` resurrection. |
| `web_search` | Keyless multi-engine search: 10+ backends, consensus plus semantic reranking, query-aware official-source placement. Ranked URLs and snippets, never a scraped article dump. |
| `web_crawl` | Best-first same-domain crawl. Sitemap plus frontier, `focus` ranking, elastic pacing, resume tokens, honest stop reasons. |
| `web_screenshot` | Rendered PNG of any URL through the same browser tier, with the usual URL safety guards. |

Every failure is structured: a stable `code` (`wall.challenge`, `guard.ssrf`, `deadline.hit`, `network.dns`…), an `errorKind` (`permanent`, `transient`, `walled`) and a `next_action`, so agents branch on codes instead of parsing prose.

## Highlights

- **Search without keys.** Six keyless engines across four independent index families plus eight official verticals (GitHub, Wikipedia, HN, Semantic Scholar, arXiv, StackExchange, MDN, Google News), merged by consensus and re-ranked locally by an ONNX cross-encoder. 95.5% answer-in-snippet over 110 questions across 11 niches with no keys at all.
- **PDFs done properly.** A custom PDFium FFI, no Python subprocess. Tables and borders come from the rendered bitmap, text from the glyph stream, and the trust audit flags exactly the regions that need OCR.
- **Crawl with manners.** Per-host adaptive pacing that honors `Retry-After` and robots `Crawl-delay`, cross-process politeness so two crawls do not double a site's rate, near-duplicate detection, resume tokens that survive restarts, and honest stop reasons.
- **Fetch that answers honestly.** `content_ok`, `thin`, `changed` with section diffs, `archive.stale` with the snapshot's age, `decoy suspected` instead of silently passing a cloaked page. No fake success.
- **Login to walled sites.** `donsetch login x.com` opens YOUR browser, you sign in, and the session cookies land in the 0600 vault that tier-1 fetches and tier-2 renders already replay. Credentials never enter DonSeTch.
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
