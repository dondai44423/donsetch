![DonSeTch — The web. On your terms.](https://raw.githubusercontent.com/dondai44423/donsetch/master/assets/readme/masthead.webp)

# DonSeTch

**The web tools your agent needs.** Search, fetch, crawl, and screenshot from one local Rust binary. No required API keys or accounts.

[![Release](https://img.shields.io/github/v/release/dondai44423/donsetch?style=flat-square&labelColor=171719&color=e52b31)](https://github.com/dondai44423/donsetch/releases)
[![CI](https://img.shields.io/github/actions/workflow/status/dondai44423/donsetch/ci.yml?branch=master&style=flat-square&labelColor=171719&color=e52b31&label=build)](https://github.com/dondai44423/donsetch/actions/workflows/ci.yml)
[![Stars](https://img.shields.io/github/stars/dondai44423/donsetch?style=flat-square&labelColor=171719&color=e52b31)](https://github.com/dondai44423/donsetch/stargazers)

[![Sponsor DonSeTch](https://img.shields.io/badge/Sponsor%20DonSeTch-e52b31?style=for-the-badge&logo=githubsponsors&logoColor=white)](#sponsorship)

[Full README](https://github.com/dondai44423/donsetch#readme) · [Setup](https://github.com/dondai44423/donsetch/blob/master/docs/setup.md) · [Tools](https://github.com/dondai44423/donsetch/blob/master/docs/tools.md) · [Proxies](https://github.com/dondai44423/donsetch/blob/master/docs/proxies.md) · **[Sponsor](#sponsorship)**

## Install

```bash
npm install -g donsetch
donsetch doctor
```

The first `donsetch` command downloads a SHA256-verified release binary. There are no install scripts to approve, including with npm, pnpm, or bun. No Rust build tools needed. Linux, macOS, and Windows are supported; [the setup guide](https://github.com/dondai44423/donsetch/blob/master/docs/setup.md) lists platform requirements and OCR availability.

For Pi, run `pi install npm:donsetch`; the extension prepares the binary when the session starts. A read-only global package uses your user cache automatically. Windows releases include the Visual C++ runtime beside the binary; no administrator runtime setup is required.

## Connect your agent

Add this to your MCP client's server configuration, then restart the client:

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

`donsetch doctor --mcp` prints absolute-path registration examples. The stdio supervisor restarts the daemon after a crash. HTTP transport is available with `donsetch mcp --http --port 8765` on supported builds.

## Four tools

| Tool | What it returns |
|---|---|
| `web_fetch` | Pages, PDFs, and site records as Markdown, or just the passages you need. Batches up to 12 URLs. |
| `web_search` | Keyless ranked sources and evidence snippets; optional paid providers. |
| `web_crawl` | A site model, previewable plan, or bounded crawl with an evidence digest, `pN` handles, and resumable job. |
| `web_screenshot` | An inline PNG through an installed Chromium-family browser. |

**Page reads launch no browser.** Eight coherent identities, carried clearance, native interfaces, and embedded V8 handle fetching. A successful application run can reveal a data request that later visits replay over HTTP. Search uses independent-index agreement and local ranking, with no model download. Screenshots use a separate browser path.

Choose `must_contain` for phrase checks, `focus` for relevant passages, or `toc` then `section` for one part of a long page. These views work on PDFs too. Login gates, bot walls, and incomplete sources remain explicit outcomes.

**DonSeTch vs. paid Firecrawl Cloud:** in my agent-run field test, DonSeTch came out ahead on fetching and focused crawling—faster reads, better topic selection, and much smaller responses with handles to read more. Firecrawl had better search relevance. I chose cloud after internal self-hosted testing favored DonSeTch; those earlier runs aren't published. [Full results, specific issues, environment and settings](https://github.com/dondai44423/donsetch/blob/master/docs/firecrawl-comparison.md).

## Use the terminal

```bash
donsetch search "rust async cancellation" --intent code
donsetch fetch https://docs.python.org/3/library/asyncio-task.html --focus "cancel a task"
donsetch crawl https://docs.python.org/3/ --mode map --topic asyncio
donsetch screenshot https://example.com --out page.png
```

CLI and MCP share the same engine. All four tool schemas fit in about 2,000 tokens.

## Optional proxies and keys

```bash
donsetch proxy add 'socks5://user:pass@proxy.example:1080'
donsetch proxy check
donsetch proxy fetch on
```

Restart MCP after changing the pool. Search uses configured pool lanes; crawl starts direct before spreading across lanes; fetch uses the pool only when enabled. Environment proxies are a separate route setting. [Proxy setup and routing details](https://github.com/dondai44423/donsetch/blob/master/docs/proxies.md).

Paid search providers and Bright Data Web Unlocker are optional. [Provider/plugin guide](https://github.com/dondai44423/donsetch/blob/master/docs/tools.md#optional-search-providers) · [Bright Data zones and keys](https://github.com/dondai44423/donsetch/blob/master/docs/brightdata.md).

## Need help?

Run `donsetch doctor --deep` for live network/provider checks and `doctor --fix` for mechanical repairs. If a global npm install fails with `EACCES`, try `npx -y donsetch doctor` or use a user-owned npm prefix. `DONSETCH_BINARY` selects an existing binary for both CLI and Pi; `DONSETCH_RELEASES_BASE` selects a release mirror.

[Install troubleshooting](https://github.com/dondai44423/donsetch/blob/master/docs/setup.md#troubleshooting) · [Issues](https://github.com/dondai44423/donsetch/issues) · [Changelog](https://github.com/dondai44423/donsetch/blob/master/CHANGELOG.md)

## Support the project

Useful in your toolkit? [Star DonSeTch](https://github.com/dondai44423/donsetch). Contributions and reproducible bug reports are welcome.

[![Support on Ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/G5Y624N5RE)

### Sponsorship

| Tier | Monthly price | What you get |
|---|---:|---|
| **Silver** | **$25/month** | A text link to your site in the GitHub README's sponsorship section. |
| **Gold** | **$79/month** | A large banner linked to your site, with priority placement. A native integration may be considered only when your product fits DonSeTch; scope and fit must be agreed first. |

Email [bhandaribishesh879@gmail.com](mailto:bhandaribishesh879@gmail.com) with your product URL, chosen tier, and number of months. For Gold, include a banner if available and describe any integration idea. We'll confirm placement and terms, then I'll share PayPal payment details. Email is the reliable route while GitHub Sponsors payouts aren't available in my region.

Prepay any number of months to lock the rate for that whole term: six months of Silver is **$150**; six months of Gold is **$474**. Later price changes do not affect an already prepaid term. One-time support is welcome at any amount through Ko-fi above.

[Full sponsorship details](https://github.com/dondai44423/donsetch#sponsorship) · [AGPL-3.0-only](https://github.com/dondai44423/donsetch/blob/master/LICENSE) · Built by Dondai and contributors.
