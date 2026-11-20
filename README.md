<p align="center">
  <img src="assets/readme/masthead.webp" alt="DonSeTch — The web. On your terms. A pipe-smoking don against a red and black city skyline." width="100%">
</p>

<h1 align="center">DonSeTch</h1>

<p align="center">
  <strong>The web tools your agent needs.</strong><br>
  Search, fetch, crawl, and screenshot. One local Rust binary. No required API keys or accounts.
</p>

<p align="center">
  <a href="https://github.com/dondai44423/donsetch/releases"><img src="https://img.shields.io/github/v/release/dondai44423/donsetch?style=flat-square&amp;labelColor=171719&amp;color=e52b31&amp;label=release" alt="Latest release"></a>
  <a href="https://github.com/dondai44423/donsetch/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/dondai44423/donsetch/ci.yml?branch=master&amp;style=flat-square&amp;labelColor=171719&amp;color=e52b31&amp;label=build" alt="CI status"></a>
  <a href="https://www.npmjs.com/package/donsetch"><img src="https://img.shields.io/npm/dm/donsetch?style=flat-square&amp;labelColor=171719&amp;color=e52b31&amp;label=npm" alt="Monthly npm downloads"></a>
  <a href="https://github.com/dondai44423/donsetch/stargazers"><img src="https://img.shields.io/github/stars/dondai44423/donsetch?style=flat-square&amp;labelColor=171719&amp;color=e52b31&amp;label=stars" alt="GitHub stars"></a>
</p>

<p align="center">
  <a href="#sponsorship"><img src="https://img.shields.io/badge/Sponsor%20DonSeTch-e52b31?style=for-the-badge&amp;logo=githubsponsors&amp;logoColor=white" alt="Sponsor DonSeTch — see tiers and payment details"></a>
</p>

<p align="center">
  <a href="#see-it-in-action">Demo</a> ·
  <a href="#start-here">Install</a> ·
  <a href="#four-tools-one-process">Tools</a> ·
  <a href="#under-the-hood">Design</a> ·
  <a href="#donsetch-vs-firecrawl-cloud">Comparison</a> ·
  <a href="#proxies-without-the-guesswork">Proxies</a> ·
  <a href="#know-the-limits">Limits</a> ·
  <a href="#documentation">Docs</a> ·
  <strong><a href="#sponsorship">Sponsor</a></strong>
</p>

Find a source, read the useful part, follow a link, check a claim. DonSeTch gives agents that web loop over MCP or from your terminal, with about 2,000 tokens of tool schemas.

**Page reads launch no browser.** Real client profiles, learned routes, and embedded V8 handle the fetch. Chromium is used separately for screenshots. Search needs no model download.

## See it in action

A 54-second demo of an agent searching the web and reading sources with DonSeTch in OpenCode.

https://github.com/user-attachments/assets/8762aaec-8553-4ebb-8a2a-0eb56bc3adf4

## Start here

### 1. Install

```bash
npm install -g donsetch
donsetch doctor
```

The first `donsetch` command downloads a SHA256-verified release binary. No install scripts to approve and no Rust toolchain needed. Linux, macOS, and Windows are supported.

<details>
<summary>Homebrew, Pi, DeepSeek Harness, or build from source</summary>

| Install | Command |
|---|---|
| Homebrew · macOS/Linux | `brew tap dondai44423/donsetch && brew install donsetch` |
| Pi native extension | `pi install npm:donsetch` |
| DeepSeek Harness plugin | `dsh plugin --profile web add github:dondai44423/donsetch-dsh` |

Pi and dsh register native tools. For source builds, platform requirements, Docker, or installer troubleshooting, see the [setup guide](docs/setup.md).

</details>

### 2. Connect your agent

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

Works with Claude Code, Cursor, OpenCode, and other MCP clients. Configuration formats vary; `donsetch doctor --mcp` prints registration examples using the binary's absolute path. `--supervised` restarts the stdio daemon after a crash.

Need an HTTP endpoint? Run `donsetch mcp --http --port 8765` and connect to `http://localhost:8765/mcp`. [HTTP setup and authentication →](docs/setup.md#http-transport)

### 3. Try it

```bash
# Find sources.
donsetch search "rust async cancellation" --intent code

# Read the passages that answer.
donsetch fetch https://docs.python.org/3/library/asyncio-task.html \
  --focus "cancel a task"

# Map a documentation site.
donsetch crawl https://docs.python.org/3/ --mode map --topic asyncio
```

The CLI and MCP tools use the same engine. No account setup sits between install and first use.

**Worth keeping in your toolkit? [Star the repo](https://github.com/dondai44423/donsetch).** It helps other agent builders find it.

## Four tools. One process.

<img src="assets/readme/tools.webp" alt="Four noir illustrations: a retrieved document, a magnifying glass and globe, a connected street map, and a bellows camera." width="100%">

| Tool | Use it for | Useful controls |
|---|---|---|
| `web_fetch` | Pages, PDFs, structured site records, or up to 12 URLs at once | `must_contain`, `focus`, `toc`, `section`, `since_last` |
| `web_search` | Ranked sources and evidence snippets, keyless by default | `intent`, `lang`, `max_results`, `read` |
| `web_crawl` | A site model, a plan, or a bounded crawl with a digest and resumable job | `mode`, `focus`, `max_pages`, `job`, `handle` |
| `web_screenshot` | A rendered PNG through an installed Chromium-family browser | `full_page`, `wait_ms`, `deadline_ms` |

Your agent chooses what to read next. DonSeTch supplies the tools.

### Read less. Keep the evidence.

Every view works over the extracted source, including PDFs and site adapters. You choose what comes back:

| Question | Option | Response |
|---|---|---|
| “Does this page mention it?” | `must_contain` | Counts, sections, and excerpts for up to eight phrases or regexes. |
| “Where is the answer?” | `focus` | Relevant passages, best first, with their section paths. |
| “What's in this document?” | `toc` | Headings and section IDs, without the body. |
| “Read that part.” | `section` | One section and its subsections. |
| “What changed?” | `since_last` | Changes against your stored previous read, or `unchanged`. |
| “Keep going.” | `offset` | Continue at the returned `next_offset`. |

```bash
donsetch fetch https://doc.rust-lang.org/std/vec/struct.Vec.html \
  --must-contain with_capacity --must-contain amortized
```

Amounts match by value: `15M` can find `EUR 15 000 000`. An unread portion makes an absence check `unverified`; it does not become a false “not found.” Use `--links` when you need citation URLs.

[Fetch views, search options, crawl budgets, and result formats →](docs/tools.md)

## Under the hood

<img src="assets/readme/fetch-route.svg" alt="Fetch flow: check cache, make a request with a coherent identity, classify the response, use a native interface or bounded V8 when needed, verify content, and remember successful routes. Mined requests replay over HTTP on later visits. Screenshots use a separate browser path." width="100%">

[Open the route map at full size →](assets/readme/fetch-route.svg)

**The client stays consistent.** TLS, HTTP/2, headers, hints, and cookies belong to one identity. A clearance is spent by the identity that earned it. Successful identities and routes are learned per site.

**JavaScript runs when it has a job.** A challenge can mint clearance in V8. An application shell can run its own program against a data-only DOM. Nothing is laid out or painted. When a successful content run reveals a reusable data request, the fetcher stores that request and replays it over HTTP on later encounters. A broken replay is discarded and re-learned.

**Search ranks evidence.** Sources are selected by query type, merged by measured index independence, and ranked with BM25+, RM3, and a compiled linear model for the query types where it won validation. No LLM, embeddings, or model download.

**PDFs use text and pixels together.** PDFium supplies glyphs and rendered pixels from the same document. DonSeTch uses both to recover reading order, tables, and forms. Scans and corrupt text regions can use optional OCR, with uncertainty marked.

[Architecture, cache behavior, local state, and source map →](docs/architecture.md)

### Measured, with the conditions attached

| Recorded fetch run · 15 Nov 2026 | Content | Median |
|---|---:|---:|
| Cold · 28 defended targets × 2 rounds · up to 3 attempts | **49/56** · curl **21/56** | **1.18 s** |
| Learned state · same 28 targets × 3 rounds · no page cache · 3 s deadline | **69/84** | **0.98 s** |

These are home-IP measurements, not a promise for every site or network. The cold run paired curl in the same window. The learned run is a different protocol, not a controlled speed comparison. Neither run used the new program/replay transport.

[Method, failures, and the separate live-web field test →](docs/benchmarks.md)

### DonSeTch vs. Firecrawl Cloud

**DonSeTch came out ahead on fetching and focused crawling in this test. Firecrawl had better search relevance.** I asked an AI agent to use both MCP servers on real tasks on 16 November 2026, inspect the returned content, and write up what happened.

The opponent was **Firecrawl's paid, hosted cloud service**, accessed with an API key. I tried its [free, open-source self-hosted version](https://github.com/firecrawl/firecrawl) internally first; DonSeTch was far ahead in those runs, so I chose cloud for a closer contest. I didn't publish the self-hosted results. The linked report covers the cloud session only.

| Task | DonSeTch | Firecrawl Cloud | Result in this session |
|---|---|---|---|
| Fetch · 12 targets | **0.83 s** median on the 10 targets both read; faster on **9/10** | **2.00 s** on those same targets | DonSeTch gave faster reads, cleaner code on the tested Pydantic page, and structured PDF tables. Both returned content on **11/12** in the original core-tool test. |
| Focused crawl · 4 runs | **9.49 s** median; **19 pages** selected around the requested topics | **19.60 s**; **26 pages**, including navigation and feeds | DonSeTch found the HTTPX async and uv Python-version material Firecrawl missed, with **7 fewer page fetches**. Firecrawl returned **6–22× as many characters** across the four runs. |
| Search · 9 queries | **1.71 s** median over 8 timed pairs; faster on **8/8** | **3.19 s** over those pairs | The agent preferred Firecrawl's results on **6 queries**, DonSeTch's on **1**, with **2 ties**. |

**Reading less is part of the design.** DonSeTch lets the agent choose an excerpt, section or full read, and expand when needed. Crawl digests give it relevant passages and `pN` handles for the stored pages. In these tasks, that meant less irrelevant reading and better page selection.

**DonSeTch kinda got its ass beat on search relevance.** It surfaced a stale Intel CEO snippet and weaker exact-error/product results. You can use a search API alongside the local reading tools—[Tavily offers a free allowance](https://docs.tavily.com/documentation/api-credits); [setup is below](#optional-keys). BYOK search wasn't tested here.

Specific issues remain: Firecrawl read G2 through a Cloudflare wall and was faster on IMDb; DonSeTch's crawl digest flattened code on two sites. This was a small, hand-picked session with uncontrolled caches and different output views. The comparison doc includes every rated case, the environment, tools and settings.

**Reddit update:** Firecrawl Alexandria returned Reddit data in a follow-up, giving Firecrawl + Alexandria the practical-access edge in these checks. DonSeTch still read it in **one 1.2 s call**, versus **~6–8 s across six discovery/read calls**. [Follow-up and workflow details →](docs/firecrawl-comparison.md#reddit-follow-up-alexandria-works)

[Full comparison: results, examples and methodology →](docs/firecrawl-comparison.md)

## Proxies without the guesswork

**You don't need one to start.** A proxy is useful when an engine rate-limits your IP or a site treats your network differently.

```bash
# Add one proxy. Use your own host and credentials.
donsetch proxy add 'socks5://user:pass@proxy.example:1080'

# Verify the tunnel, exit IP, and latency.
donsetch proxy check
donsetch proxy list
```

Restart the MCP server after changing the pool. The important part is what uses it:

| Tool | Default with a configured pool | Change it |
|---|---|---|
| Search | Uses pool lanes for engine requests | Automatic |
| Crawl | Starts direct; later reads can spread across pool lanes | `donsetch proxy crawl off` disables pool use |
| Fetch | Uses your own IP | `donsetch proxy fetch on` enables the pool |

“Direct” here means outside the pool. A configured `HTTPS_PROXY`, `HTTP_PROXY`, or `ALL_PROXY` can still route that request. A failed configured proxy is reported; it does not silently fall back to your IP.

**Crawl contacts the seed directly before pool lanes take over.** To keep a site entirely out of scope, use a [local deny rule](docs/rules.md).

[HTTP/SOCKS5 formats, imports, credentials, environment proxies, and troubleshooting →](docs/proxies.md)

## Optional keys

The default tools work without keys. Bring your own when you want an additional search provider or a paid fetch fallback.

```bash
donsetch keys add tavily 'YOUR_KEY'
donsetch keys add exa 'YOUR_KEY'
donsetch keys default local           # Keyless first; paid search as fallback.
```

To use a provider for search first, run `donsetch keys default tavily` (or your chosen provider), then restart MCP. Search queries go to that service; fetch and crawl keep their local engine. An explicit search `lang` uses the keyless path instead. The comparison above tested keyless search only.

Native search providers: Tavily, Exa, Serper, SerpApi, SerpBase, Brave Search, TinyFish, Parallel, and Bright Data. Other providers can use the [executable plugin interface](docs/tools.md#search-plugins).

Bright Data uses separate zones for SERP search and Web Unlocker:

```bash
donsetch keys add bd 'YOUR_TOKEN::YOUR_SERP_ZONE'
donsetch keys add unlocker 'YOUR_TOKEN::YOUR_UNLOCKER_ZONE'
```

Unlocker runs after a local wall, only with a configured key. Attempts can cost money and are capped per day. [Zone setup, costs, and troubleshooting →](docs/brightdata.md)

## Know the limits

| Situation | What happens |
|---|---|
| A page requires login | `wall.auth`. No sign-in, imported browser cookies, or stored login credentials. |
| A human challenge remains | A named wall. No local hCaptcha, reCAPTCHA, Turnstile, or press-and-hold solver. An optional Unlocker may help. |
| Content needs layout or canvas | The fetch stays a declared shell if the program cannot produce readable content. Use a screenshot for pixels. |
| A read hits its size or time limit | Partial content or a named failure, with continuation information when available. |
| Search sources refuse the request | Source status and an empty or degraded answer. No invented results. |
| You repeatedly harvest a defended site | Pacing helps; IP bans can still happen. This tool is for research, not bulk collection. |

For signed-in browsing or long, human-paced sessions, use a real browser driver such as [Bladebro](https://github.com/dondai44423/bladebro).

Local processing still makes requests to the sites you ask for. Optional providers and proxies receive the traffic you route through them. [Security policy →](SECURITY.md)

## Configuration and upkeep

```bash
donsetch config show                  # Values and where they came from.
donsetch config show --markdown       # Every supported setting.
donsetch doctor --deep                # Live network and provider checks.
donsetch status                       # Version, keys, proxies, and cache.
donsetch update                       # Update from GitHub Releases.
donsetch rollback                     # Restore the previous version.
```

Settings live in `donsetch.toml`. Precedence: defaults → legacy environment → config file → `DONSETCH_<SECTION>__<KEY>`. Unknown keys and invalid values fail clearly.

[Config paths, browser setup, Docker, HTTP transport, and troubleshooting →](docs/setup.md)

## Documentation

| Guide | What's inside |
|---|---|
| [Setup](docs/setup.md) | Install choices, platform matrix, MCP, HTTP, Docker, config, and repairs. |
| [Tools](docs/tools.md) | Reading views, search, crawling, screenshots, BYOK plugins, and result contracts. |
| [Proxies](docs/proxies.md) | Pool routing, environment proxies, credential formats, and diagnostics. |
| [Architecture](docs/architecture.md) | Fetch, search, extraction, PDF, state, and the code behind them. |
| [Measurements](docs/benchmarks.md) | Benchmark conditions and the live-web field report. |
| [Firecrawl Cloud comparison](docs/firecrawl-comparison.md) | Fetch, crawl and search results, examples, environment, and methodology. |
| [Local rules](docs/rules.md) | Refuse a host before fetching and give the agent your instructions. |
| [Bright Data](docs/brightdata.md) | Optional SERP and Web Unlocker setup. |
| [Contributing](CONTRIBUTING.md) | Development loop, tests, PRs, and releases. |

## Support the project

<p align="center">
  <img src="assets/readme/emblem.webp" alt="The DonSeTch don: top hat, pinstripe suit, pipe, and network smoke." width="128">
  <br>
  <strong>Good tools take work. Help keep this one independent.</strong>
</p>

If DonSeTch earns a place in your agent's toolkit, [give it a star](https://github.com/dondai44423/donsetch), share a useful example, or [report a reproducible bug](https://github.com/dondai44423/donsetch/issues). Code and documentation contributions are welcome.

<p align="center">
  <a href="https://ko-fi.com/G5Y624N5RE"><img src="https://ko-fi.com/img/githubbutton_sm.svg" alt="Support DonSeTch on Ko-fi"></a>
</p>

### Sponsorship

DonSeTch is free and open source. Sponsorship funds maintenance, testing, and releases. Top-of-README placement is capped at 5 active slots so every sponsor stays visible.

| Option | Price | What's included |
|---|---:|---|
| **Sponsor** | **$99/month** | Linked banner and short description near the top of the README. |
| **Integration Partner** | **$599 upfront**, then **$99/month** *(month 4+)* | Everything in Sponsor for your first 3 months, one scoped native BYOK integration (built, tested, `doctor`-checked, and documented with you), plus 1 optional contextual CLI mention where relevant. |

Prepay to lock the current rate against future price rises: **6 months for $495** (1 month free) or **12 months for $990** (2 months free).

#### Policy & how native integrations work

1. **Free scope check first:** email your product, API docs, and proposed integration before paying anything. Scope is agreed in writing first. If the integration is not a fit, you can still take a standard **$99/month** Sponsor slot.
2. **Product boundaries:** every native integration is opt-in BYOK only and plugs into an existing DonSeTch slot (search provider, walled-fetch fallback, or proxy setup). Default keyless behavior never degrades, no sponsor is routed to by default, and no fifth tool is added.
3. **Clean output & editorial independence:** sponsors never appear in MCP tool responses, `--json` output, or tool schemas. Any human-facing CLI mention is clearly labelled `Sponsor:`, dim, and removable with `DONSETCH_NO_SPONSOR=1`. Sponsorship buys visibility and agreed integration work — never biased benchmarks or exclusivity against other BYOK providers.
4. **Delivery & active maintenance:** standard scoped integrations ship within an agreed window (typically 1–2 weeks after payment and sandbox/test credentials are provided); larger custom architectures are quoted separately. Sponsors provide a modest test API key so the integration stays verified across releases while sponsorship is active.
5. **Fair lifecycle:** when a sponsorship ends, README banners and CLI mentions come down at the end of the paid term. Working integrations stay in DonSeTch for users; if an unsponsored integration later breaks due to upstream API changes or becomes a maintenance burden, it may be deprecated or removed after a heads-up.
6. **Billing & refunds:** monthly or prepaid billing via PayPal (GitHub Sponsors payouts are not available in my region yet). The upfront Integration Partner fee is non-refundable once agreed implementation starts.

#### How to sponsor

Email [bhandaribishesh879@gmail.com](mailto:bhandaribishesh879@gmail.com) with your product URL, banner (if you have one), and whether you want a Sponsor slot or an Integration Partner scope check. I'll confirm slot availability and share PayPal details.

One-time support is welcome at any amount through [Ko-fi](https://ko-fi.com/G5Y624N5RE). Using Bright Data? [This partner link](https://get.brightdata.com/ivqwoicrrlbr) supports DonSeTch at no extra cost to you.

---

<p align="center">
  Built by <a href="https://github.com/dondai44423">Dondai</a> and <a href="https://github.com/dondai44423/donsetch/graphs/contributors">contributors</a>.<br>
  <a href="LICENSE">AGPL-3.0-only</a> ·
  <a href="CHANGELOG.md">Changelog</a> ·
  <a href="SECURITY.md">Security</a> ·
  <a href="https://github.com/dondai44423/donsetch/issues">Issues</a>
</p>
