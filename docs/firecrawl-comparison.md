# DonSeTch vs. paid Firecrawl Cloud

A hands-on MCP comparison from 16 November 2026: 12 fetch targets, 9 search queries, 4 focused crawls, a map comparison and a warm diff, with a subsequent Reddit/Alexandria follow-up below. [Back to the README](../README.md#donsetch-vs-firecrawl-cloud).

**DonSeTch came out ahead on fetching and focused crawling in this session. Firecrawl had better search relevance.** The practical difference was how much useful work the agent got from each call: DonSeTch usually answered faster, kept more site furniture out, and selected pages closer to the crawl's topic. Its reading views and digest handles let the agent choose when to spend context on a full page.

I asked an AI agent to use both tools and read their outputs. This page brings together the results, environment, tools and settings from that session. I used an agent to help prepare this write-up too.

## Why I compared against the paid cloud version

**Every Firecrawl result below came from the paid, hosted cloud service, accessed with an API key.** Firecrawl also has [free, open-source code](https://github.com/firecrawl/firecrawl) that you can [self-host](https://docs.firecrawl.dev/contributing/self-host). That is a closer match to DonSeTch's free, open-source deployment model.

I tried self-hosted Firecrawl in earlier internal testing. In those runs, DonSeTch was far ahead, so I didn't publish those results. I wanted the comparison tables to show a closer contest and chose Firecrawl's paid cloud infrastructure instead. Those earlier runs and their settings aren't included here; that assessment is from my internal testing, and the published numbers below cover only the cloud session.

The cloud account's subscription tier and dollar bill weren't recorded. The responses included Firecrawl credits and `proxyUsed: basic`; no stealth-tier tuning was tested.

## Fetch: faster reads and cleaner extraction

In the original core-tool test, both returned content on **11 of 12 targets**. DonSeTch read Reddit where `firecrawl_scrape` reported it unsupported; Firecrawl read the Cloudflare-protected G2 page where DonSeTch returned a wall. **The later Alexandria check succeeded on Reddit**; that corrects the broader impression that Firecrawl couldn't access it.

On the **ten targets both read**, DonSeTch was faster on **nine**, with recorded medians of **827.5 ms vs 1,996.5 ms**—about 2.4× faster for those calls. IMDb was the exception: Firecrawl took 2.16 s against DonSeTch's 5.69 s.

The agent also preferred DonSeTch's extraction: Python code kept its line breaks on Pydantic, the arXiv PDF's tables appeared as Markdown tables, and GitHub/Stack Overflow reads contained less interface text. Firecrawl's Pydantic output joined code lines; its PDF tables appeared as flattened text. Those are observations on these pages, rather than claims about every page either tool handles.

| Target | DonSeTch ms | Firecrawl core scrape ms | Observed result |
|---|---:|---:|---|
| MDN Fetch API | **449** | 1,473 | DonSeTch returned a clean excerpt with continuation; Firecrawl included navigation in 22,126 chars. |
| Pydantic models | **711** | 1,833 | DonSeTch kept Python code intact; Firecrawl joined code lines, observed twice on this URL. |
| Reddit LocalLLaMA weekly top | **1,195** | 1,124, unsupported | DonSeTch returned 24 items with followable handles. The core scrape call failed; [Alexandria later returned Reddit data](#reddit-follow-up-alexandria-works). |
| Stack Overflow question 59073556 | **522** | 2,957 | DonSeTch returned the question and answers with intact code; Firecrawl also included voting/save/advertising text. |
| G2 Slack reviews | 889, wall | **1,936** | Firecrawl returned content; DonSeTch reported a Cloudflare 403. |
| Wikipedia: Transformer architecture | **944** | 3,940 | DonSeTch returned the requested 1,500-char window; Firecrawl returned 208,959 chars. |
| Wikipedia: deep-learning software comparison | **1,521** | 6,541 | Both preserved Markdown tables; responses were 16,071 vs 66,178 chars. |
| arXiv: Attention Is All You Need | **1,307** | 1,837 | DonSeTch preserved table structure; Firecrawl flattened tables to lines. |
| CPython issue 101581 | **1,866** | 3,848 | DonSeTch returned issue content without account banners and repeated UI controls. |
| IMDb: The Shawshank Redemption | 5,687 | **2,156** | Both returned content; Firecrawl was faster. |
| Next.js installation docs | **517** | 1,819 | DonSeTch omitted the copy/AI interface text seen in Firecrawl's output. |
| GitHub CPython API JSON | **634** | 1,619 | DonSeTch returned parseable JSON; Firecrawl's Markdown response put JSON inside a code fence. |

### The agent chooses how much to read

DonSeTch's bounded output is an agent-control feature. The caller can ask for a short read, use `toc` and `section`, find passages with `focus`, or continue with `offset`. It can request more text when the task needs it. A short first response avoids spending context on a whole article before the agent knows which part matters.

The test used DonSeTch `max_chars` values from 500 to 16,000 against Firecrawl's full Markdown/main-content responses. That is the workflow whose latency was measured. It shows the benefit of selective reading for these tasks; it doesn't measure the cost of reading every byte through both tools. Partial reads stay identified as partial, with continuation available. [Reading controls →](tools.md#fetch)

An initial Firecrawl JSON-format request also failed because the caller omitted required `jsonOptions`; its subsequent Markdown response is the timed JSON row above.

### Reddit follow-up: Alexandria works

A Firecrawl team member pointed out that I'd missed Reddit access through [Alexandria](https://alexandria.firecrawl.dev/). I tried it, and it worked. My fault: I'd tested the core tools after adding an API key and stopped there. The scrape error itself also pointed toward Alexandria.

**My updated access/stealth verdict for these checks: Firecrawl + Alexandria has the edge**, covering Reddit as well as Firecrawl's earlier G2 success. **DonSeTch still had the simpler, faster Reddit reading workflow.** Here's what my agent reported for the follow-up, beside the original DonSeTch read:

| Getting Reddit content | DonSeTch | Firecrawl Alexandria |
|---|---|---|
| Calls to first content | **1** `web_fetch` call | **6**: two catalog searches, list `/library`, find the Reddit capability, read its contract, then `alexandria_library_read` |
| Tool time | **1.195 s** | **~6–8 s** through discovery; **3.77 s** for the data read alone |
| Returned data | 24 items as ranked Markdown, scores/authors/comment counts, followable handles and `next_offset` | 17 posts under `data.posts`, with typed fields and cursor pagination |
| Discovery/context overhead | No extra catalog or provider-contract lookup | One catalog response was reported at **~15k tokens**, plus contract/schema inspection |
| Pacing encountered | No provider quota; origin pacing still applies | The agent encountered an **11 requests/minute** limit and one quota stall during exploration |

The broader exploration reached roughly ten calls: repeated Reddit reads while locating the nested rows, a Wikipedia title/redirect mismatch, and a broad library search that timed out. Those aren't all failures to fetch Reddit. The two runs also returned different item counts; this follow-up didn't establish identical weekly-top ordering or item coverage.

So yes, Alexandria got through. For a one-off page read, the extra discovery was a hassle compared with DonSeTch's one call. Discovery can be reused, and Alexandria's typed fields (`ups`, `upvote_ratio`, `created_utc`, `link_flair_text`) and cursor pagination are useful for dataset work. These are the agent's reported timings and encountered limits, not a new cold benchmark or a universal quota claim; the original core-tool medians above stay separate.

## Crawl: more useful pages with less work

**This was DonSeTch's clearest task-level advantage.** Its focused selection found the material the agent wanted while Firecrawl sometimes spent the page budget on navigation, feeds or the wrong part of the docs.

- **HTTPX:** DonSeTch read the async and timeout pages in two page fetches. Firecrawl read six pages and still missed `/async/`.
- **uv:** DonSeTch selected five pages around Python-version management. Firecrawl read eight pages; the agent found none addressing the requested Python-version topic.
- **Simon Willison:** the agent judged all six DonSeTch pages on-topic. Firecrawl also read six pages, but its homepage and feed alone contributed 170,653 characters of the response.
- **Python asyncio:** both got six asyncio pages. Firecrawl was 819 ms faster after the tester supplied an `includePaths` pattern; DonSeTch selected from the caller's topic.

| Site / task | Pages: DonSeTch / FC | Time: DonSeTch / FC | Returned chars: digest / full bodies |
|---|---|---|---|
| HTTPX: async + timeouts | **2** / 6 | **5.13 s** / 16.37 s | **6,112** / 38,317 |
| uv: Python-version management | **5** / 8 | **9.56 s** / 22.83 s | **10,555** / 73,553 |
| Simon Willison: LLM tooling/agents | 6 / 6 | **9.67 s** / 24.37 s | **10,628** / 230,318 |
| Python library: asyncio | 6 / 6 | 9.43 s / **8.61 s** | **13,662** / 169,254 |

Across these runs, DonSeTch used **19 page fetches instead of 26**, about **27% fewer**, with better topic selection in the agent's assessment. Fetching extra off-topic pages wouldn't improve the answer. The recorded median task time was **9.49 s vs 19.60 s**, about **2.1× faster**, with DonSeTch faster on three runs out of four. Firecrawl reported **26 crawl credits**; DonSeTch's keyless engine incurred no search/scraping-provider fee.

These are comparisons of the requested research tasks. The page sets differed because selection is part of what a crawler does; the numbers aren't a measure of per-page throughput.

### Compact evidence first, full pages on demand

Firecrawl returned **6–22× as many characters** across the four runs. DonSeTch's digest gave the agent a site brief, headings, relevant passages and stable `pN` handles. That makes the first response much easier to work through, especially when the alternative includes a homepage and an entire feed.

The agent can expand a stored page with `crawl get pN --job <job>` without a new network read, then choose a section or continuation. Keeping the full page available behind a handle is part of the design: the agent decides which details deserve its context. The tester didn't exercise `get` in this session; this behavior is documented in the [crawl tool guide](tools.md#crawl).

Response sizes were measured in characters; model-token bills weren't measured. A digest and a full page serve different reading needs, and a caller who wants everything can expand the stored pages.

**One actual defect:** the agent observed code flattened into a single line in the HTTPX and uv crawl digests. Separate DonSeTch fetches kept code intact. That formatting issue remains reported here; compact output shouldn't damage a code example. This documentation update doesn't fix it.

### Map and repeat reads

| Task | DonSeTch | Firecrawl Cloud |
|---|---|---|
| uv map | **3.86 s**, 84 canonical URLs | 12.49 s, 126 links including anchor fragments |
| Warm Simon Willison diff | **346 ms**, no unchanged-page re-fetch reported | No counterpart tested |
| Starlette crawl | DNS failure explained; no pages | Completed with zero pages |

The map shows quicker, cleaner URL discovery in this session; 84 vs 126 isn't a site-coverage score because fragments and canonicalization differ. The warm diff demonstrates DonSeTch's repeat-read workflow, with no Firecrawl comparison claimed. Starlette failed DNS resolution in the test environment and is excluded from the four valid full crawls.

## Search: DonSeTch was faster; Firecrawl ranked better

**DonSeTch kinda got its ass beat on search relevance.** The agent preferred Firecrawl on **six queries**, DonSeTch on **one**, and called **two ties**. DonSeTch was faster on all **eight queries with timings for both**, with medians of **1.71 s vs 3.19 s**. That made search quick, but the exact-error, factual and product queries exposed quality gaps.

| Query | DonSeTch / FC ms | Agent's assessment |
|---|---|---|
| `Nvidia quarterly earnings November 2026 revenue results` | **1,816** / 3,652 | Firecrawl preferred: primary NVIDIA sources vs DonSeTch's aggregators. |
| `python asyncio TaskGroup cancel remaining tasks when one task raises exception` | **1,800** / 5,925 | Firecrawl preferred: a more relevant list; DonSeTch included the Python homepage and downloads page. |
| `docker build "failed to solve" "did not complete successfully" apt-get install exit code 100` | **1,828** / 6,306 | Firecrawl preferred: 6/6 direct hits vs 3/6 useful DonSeTch hits in the agent's judgment. |
| `cargo workspace dependency inherited "workspace = true" Cargo.toml documentation` | Missing / 2,659 | Tie: both found the Cargo Book; DonSeTch had the more explanatory snippet. |
| `Python 3.15 release date and what is new` | **1,617** / 2,114 | Slight DonSeTch preference for the answer-bearing snippet. |
| `Zahnarzt Notdienst Berlin Mitte Samstag` | **877** / 2,795 | Slight Firecrawl preference for official bodies ranked first; DonSeTch used `lang: de`. |
| `who is the current CEO of Intel` | **959** / 2,818 | Firecrawl preferred. DonSeTch returned the name the agent judged correct at #1, but a conflicting, stale predecessor claim at #2. |
| `Sony WH-1000XM6 best price buy` | **1,821** / 3,023 | Firecrawl preferred: product/manufacturer pages vs DonSeTch's category page at #1. |
| `induction heads mechanistic interpretability transformer circuits` | **1,241** / 3,350 | Tie overall; Firecrawl put the primary source first, DonSeTch second. |

The stale CEO snippet is a specific freshness problem. DonSeTch's retrieval confidence describes source agreement and evidence, rather than checking each fact. Neither tool had a hard failure on the nine rated queries. The agent's usefulness judgments weren't independently scored, and this session didn't independently verify every fact, earnings period, release date or price in the returned snippets.

### You can choose a search provider

You can use a search API while keeping DonSeTch's local fetching, extraction and crawling. For example, [Tavily offers 1,000 free API credits per month without a credit card](https://docs.tavily.com/documentation/api-credits), checked 16 November 2026; credit use depends on search depth.

```bash
donsetch keys add tavily 'YOUR_KEY'
donsetch keys default tavily
# Restart MCP after changing keys or the default.
```

That selects the provider for search. BYOK wasn't part of this test, so the comparison contains no score for that setup. An explicit search `lang` uses the local language-aware path. `donsetch keys default local` switches back to keyless-first search. [All providers and fallback behavior →](tools.md#optional-search-providers)

## How the test was run

| Item | Recorded setup |
|---|---|
| Date | 16 November 2026, observed server timestamps around 06:30 UTC |
| Agent / platform | OpenCode, `opencode/deepseek-v4.1-flash-free`, Linux |
| Tools | DonSeTch `web_fetch`, `web_search`, `web_crawl`; Firecrawl `firecrawl_scrape`, `firecrawl_search`, `firecrawl_crawl`, `firecrawl_map` |
| Calls | Individual MCP calls through the agent's JavaScript `execute` tool; generally sequential pairs, with some early parallel probes disclosed |
| Timing | `Date.now()` around awaited calls, including MCP transport, queueing and output delivery |
| Fetch settings | DonSeTch caller-selected `max_chars`; Firecrawl Markdown with `onlyMainContent: true` |
| Crawl settings | DonSeTch caller-supplied focus and page budget; Firecrawl page limit, `maxDiscoveryDepth: 2`, Markdown/main-content scrape options; Python asyncio also used `includePaths: ["/3/library/asyncio.*"]` |
| Search | DonSeTch keyless; intents supplied on some queries; Firecrawl hosted search |
| Judgment | The testing agent read outputs and assessed usefulness; no automated scoring loop |
| Network | Requests went through the MCP servers; egress details weren't recorded |
| Browser automation | No separate browser-driver calls; DonSeTch screenshots weren't used |

This is my own hand-picked field test, asking how a local, free tool compares with paid cloud infrastructure. Caches weren't controlled; the agent reported first-run crawl timings but couldn't establish that they were cold. Server versions and complete configurations weren't pinned, and some long outputs were sliced for inspection. The tables summarize the recorded timings and the agent's content inspection. I haven't rerun the comparisons for this write-up.

The measurements describe this session's calls and workflows. They aren't a repeated-run benchmark or a complete comparison of either product. Firecrawl's agent, monitoring, valid schema extraction and stealth-tier tuning weren't tested; neither were DonSeTch screenshots, OCR, search `read`/variants or crawl `plan`/`get`. The self-hosted assessment above comes from my separate internal testing. Dollar bills, local compute costs, model-token costs and an uptime rate weren't measured.

### Counting and interpretation

Fetch success means content was returned, including identified partial reads; a wall doesn't count as content. The fetch timing comparison uses the ten targets both tools read. The search timing comparison uses the same eight queries, with DonSeTch's missing Cargo-query timer left out of both sides. The crawl comparison contains four paired full runs; map, warm diff and the invalid DNS target stay separate.

DonSeTch's focus is lexical. IMDb's longer response time doesn't identify which fetch route answered; DonSeTch's fetch path launches no browser renderer. The table-heavy Wikipedia page preserved Markdown tables, while content beyond the inspected window of the Transformer article wasn't verified. The diff's recorded `FrontierEmpty` differs from the current v6 stop vocabulary; with no pinned server version, that discrepancy remains unresolved.

Unrated exploratory calls included a poor DonSeTch Hacker News search, an error for query `x`, and unusual URLs in a Firecrawl example.com map. They aren't included in the rated totals; they weren't systematically paired or investigated as security findings.
