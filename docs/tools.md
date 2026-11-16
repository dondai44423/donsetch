# Tool guide

Use the same four tools through MCP or the CLI. [Back to the README](../README.md).

`donsetch tools` prints the current MCP schemas. `donsetch <command> --help` is the CLI reference. The spec in [src/spec.rs](../src/spec.rs) generates both.

## Fetch

```bash
donsetch fetch https://example.com
donsetch fetch https://docs.python.org/3/library/asyncio-task.html --focus "cancel a task"
donsetch fetch https://doc.rust-lang.org/book/ --toc
donsetch fetch https://doc.rust-lang.org/book/ --section s3
```

Fetch returns clean Markdown, or a selected view. HTML, PDFs, feeds, plain text, JSON, and supported site adapters share the reading views.

| MCP argument | CLI | Meaning |
|---|---|---|
| `url` | Positional URL(s) | One URL or up to 12; each gets the same view. |
| `must_contain` | `--must-contain` | One phrase/regex or up to eight; repeat the CLI flag. |
| `focus` | `--focus` | Passages matching a question or terms, best first. |
| `toc` | `--toc` | Heading outline with `sN` IDs and sizes. |
| `section` | `--section` | An `sN` ID or heading text, including subsections. |
| `max_chars` | `--max-chars` | Default 16,000 for one page; 48,000 total for a batch. |
| `offset` | `--offset` | Continue at a returned `next_offset`, with the same URL and view. |
| `links` | `--links` / `--links=handles` | Keep real URLs or compact link handles. Default: link text only. |
| `since_last` | `--since-last` | Changes against the stored previous read. With no baseline, returns the page. |
| `image_text` | `--image-text` | OCR the first four content images into a searchable section. Requires OCR. |
| `deadline_ms` | `--deadline-ms` | Hard call limit, 500–600,000 ms, per URL in a batch. |

### Presence checks and focused reads

```bash
donsetch fetch https://example.com/pricing \
  --must-contain "free tier" --must-contain '/\$\d+/'
```

Phrase checks ignore case, spacing, typographic quotes/dashes, and Markdown emphasis. Regexes support `imsx` flags. Amounts match by value: `$1.5bn` can find `US$1,500 million`; `3%` can find `3 percent`.

Each term reports a count, section IDs, and excerpts, or a near-miss hint when the words appear separately. Verdicts are `match`, `partial`, `no_match`, or `unverified`. A partial source cannot prove absence.

`focus` ranks paragraphs, list items, table rows with their headers, and code. A miss returns the outline. `section` narrows a probe or focused read to one part. Conflicting views are rejected before the fetch:

- `must_contain` cannot combine with `focus`, `toc`, `offset`, or `since_last`.
- `toc` and `focus` are separate views.
- `since_last` runs without view arguments.

### Pagination, links, and history

`next_offset` is a character position in the rendered Markdown, not a PDF page number. A multipage article may instead name `next_page`. Keep the same view when continuing.

Use `links: true` or `"urls"` for citation addresses. `"handles"` returns short `L-` handles that the same MCP server can resolve as fetch URLs. Search `S-` handles are session-local too; copy actual URLs between separate CLI calls.

History fingerprints and `since_last` describe changes relative to an earlier read. Fetch freshness is governed separately by cache revalidation; a history comparison alone is not proof of a fresh origin response.

### Site records and PDFs

Adapters reshape Reddit, GitHub, Stack Exchange, YouTube, Wikipedia, docs, and package registries using their own public surfaces or page data. Output is attributed as `via=adapter:…`. A thread/article must be answered as that item; a front-page feed does not stand in for it.

PDFs are recognized by content type or `%PDF` bytes. Glyphs, pixel structure, reading order, tables, and AcroForms enter the same reading pipeline. Optional PP-OCR handles scans and corrupt text regions. OCR includes confidence and uncertainty markers. Encrypted or unreadable PDFs return the reason.

## Search

```bash
donsetch search "rust async cancellation" --intent code --max-results 5
donsetch search "site:postgresql.org vacuum analyze"
donsetch search "attention is all you need" --intent paper --read 1
donsetch search "中華人民共和国 人口" --lang ja
```

| Option | Meaning |
|---|---|
| `intent` / `--intent` | `auto` (default), `web`, `code`, `paper`, `news`, or `entity`. |
| `lang` / `--lang` | Explicit BCP-47 language, e.g. `uk`, `ne`, `pt-BR`. Only a region subtag selects a market. |
| `max_results` / `--max-results` | Default 7, maximum 12. |
| `read` / `--read` | Read up to three top pages inside the call's deadline; their passages are marked `page:`. |
| `query_variants` / `--query-variant` | Up to two caller-written alternatives, with a separate result set per query. |
| `deadline_ms` / `--deadline-ms` | Hard time budget, 500–600,000 ms. |

Search understands `site:`, `filetype:`, `intitle:`, quoted phrases, and `-term` exclusions. It preserves code identifiers and exact versions.

Language is never inferred from script. Latin-script queries default to English without a country; non-Latin queries send no language unless `lang` is set. An explicit `lang` uses local language-aware sources and bypasses configured paid search providers for that call.

Keyless routing can ask registries for a named package, first-party records for papers or entities, and dated news sources for a news query or time window. A configured paid provider uses its own filters rather than joining the native fan-out.

### Reading the answer

| Field | Meaning |
|---|---|
| `confidence` | Retrieval confidence from independent sources, agreement, and query evidence; not a fact-check of the pages. |
| `degraded` | Thin independence or evidence. |
| `requested_results`, `returned_results`, `underfilled` | Whether the requested list size was obtained. |
| `source_type` | Source category, such as paper, community, reference, publisher, or official. Not a trust certificate. |
| `cache_age_ms` | Age when a merged answer comes from cache. |

The source census distinguishes answers, refusals, timeouts, off-query sets, and lanes not asked. Only high-confidence merged results are cached. A failed page read retains its search snippet.

Inspect sources with `donsetch search --sources`, run canaries with `donsetch search doctor`, and inspect ranking with `donsetch search --explain "your query"`.

### Optional search providers

| Provider | Key command name |
|---|---|
| Tavily | `tavily` |
| Exa | `exa` |
| Serper.dev | `serper` |
| SerpApi | `serpapi` |
| SerpBase | `serpbase` |
| Brave Search API | `bravesearch` |
| TinyFish | `tinyfish` |
| Parallel | `parallel` |
| Bright Data SERP | `bd` or `brightdata` |

`donsetch keys add <provider> <key>` registers a key. Multiple keys can be stacked per provider. `keys default local` prefers keyless search; provider errors or exhausted credits can fall back to the remaining chain. Rate limits rest the affected key. `keys list` shows the state; `keys export` / `import` move the store. Restart MCP after changes.

Bright Data's fetch-only `unlocker` key is separate from search; see [its setup guide](brightdata.md).

### Search plugins

Register an executable as an optional provider:

```bash
donsetch keys add plugin searxng --cmd 'python3 /path/to/searxng-adapter.py' --test
```

DonSeTch executes it without a shell, writes one JSON request to stdin, and reads its JSON answer from stdout:

```json
{"format":1,"query":"rust async","max_results":8,"intent":"web","deadline_ms":30000}
```

```json
{"format":1,"results":[{"title":"Example","url":"https://example.com","snippet":"Source text","score":0.9}]}
```

For an error, exit nonzero with stderr or return:

```json
{"format":1,"error":"Rate limit reached","retryable":true,"error_kind":"rate_limited"}
```

Optional `error_kind`: `invalid_key`, `credit_depleted`, `rate_limited`, `server_error`, or `network_error`. Invalid/depleted plugins stay parked until re-registration; rate-limited plugins rest 60 seconds. Without it, `retryable` controls fallback. The default hard timeout is 30 seconds (`--timeout` changes it); stdout is capped at 8 MiB. Cancellation kills the process. Keep service keys in the adapter's environment.

## Crawl

```bash
donsetch crawl https://docs.python.org/3/ --mode map --topic asyncio
donsetch crawl plan https://docs.python.org/3/ --topic asyncio --max-pages 10
donsetch crawl explain https://docs.python.org/3/ --topic asyncio --max-pages 10
donsetch crawl run https://docs.python.org/3/ --topic asyncio --max-pages 10
donsetch crawl get p1 --job j0123456789abcdef
donsetch crawl diff https://docs.python.org/3/ --refresh
donsetch crawl sites
donsetch crawl forget docs.python.org
donsetch crawl resume j0123456789abcdef
donsetch crawl https://docs.python.org/3/ --mode full --topic asyncio \
  --max-pages 10 --deadline 60
donsetch crawl --resume 'TOKEN_FROM_PREVIOUS_RESULT'
```

| Mode | Work |
|---|---|
| `full` · default | Plan the site, then execute the plan; returns the digest and job. |
| `map` | Return the site model: sections, families, URL counts and freshness; no page fetches. |
| `plan` | Return the pages to fetch first, with reasons, coverage, cost and reserves; no page fetches. CLI verb `crawl plan`. |
| `run` | Spend the plan through the executor: one page at a time, durable job, declared wall; returns the digest and pN handles. CLI verb `crawl run`. |
| `get` | Read one `pN` page from a run job with no network. Supports `--section` and `--offset`. |
| `diff` | Revalidate the inventory and fetch only changed/new pages; report removed URLs. |
| `sites` | List the bounded host dossiers and their page-memory counts. |
| `forget` | Remove one host's dossier; the fetch cache is untouched. |

MCP `focus` is CLI `--topic`. It ranks the inventory and plan lexically by link text and URL paths, then keeps focus passages in the digest. This is lexical matching: no embeddings, query rewriting or semantic guess. A focus that matches nothing says so and falls back to breadth.

`map` reads the site's own inventory (`llms.txt`, sitemaps, feeds, then the seed's links) and answers with the model: site kind, sections, families, URL counts, freshness and the inventory requests it spent. It never reads a content page; the seed page is read only when the site publishes no inventory. `map_limit` caps the flat URL list; the model still covers the whole inventory and says when the list is capped.

`plan` spends no page: it answers every focus topic first (one page per named term the inventory
can answer), then covers sections and families, then deepens into a family's members, keeping a
reserve list and stating each row's reason, section, cost and request class. A focus filters the
plan to matching URLs; if nothing matches it says so and falls back to breadth. `explain` adds a
bounded candidate table (`explain=true` in MCP). A focused plan uses the full inventory; a
no-focus plan can use the warm dossier. `max_pages` is the plan budget.

`full` is `plan` then `run`: it spends the plan through the v5 fetch engine, one page at a time
per host, the plan budget plus reserves as the host request budget, a wall declaration for the
first classified challenge/block/auth/paywall, and a durable job under `cache/crawl/jobs/`.
Every completed page is persisted before the next request; `resume=<job>` (CLI `crawl resume`)
returns the same pages and does not re-fetch them. A complete job is idempotent. A wall is not
retried: switch egress or use a real browser driver. The response is the digest: a site brief,
page cards and stable `pN` handles; repeated source headers/footers are removed, headings and
focus passages are kept, and cards grow with the page budget (a 10-page crawl returns ten full
cards, not a fixed brief). The result carries `job` and `resume` (the same id) and names the
focus topics it answered, the pages it planned against the inventory, and any unanswered term.
`crawl get pN --job <job>` reads the full stored page with no network;
`--section`/`--offset` project it like a fetch. Dataset mode emits schema
`donsetch.crawl.page` v3 with `handle`, `headings`, `card` and the full `markdown` per row.

| Budget | Default / range |
|---|---|
| `max_pages` | 10 / up to 200; plan mode: pages to plan; run mode: pages to fetch |
| `explain` | Plan mode: include the candidate table |
| `map_limit` | 120 / 1–50,000 URLs in the flat map list |
| `max_depth` | 2 / 0 means seed only |
| `max_total_chars` · CLI `--max-chars` | 60,000 / 1–500,000 |
| `per_page_max` | 8,000 / 1–40,000 |
| `deadline_s` · CLI `--deadline` | 120 / 1–600 seconds |

Character caps apply to page Markdown, not inventory and header metadata. Cut pages carry `next_offset` and `content_complete: false`; continue with fetch or `crawl get --offset`. Run dataset rows are schema `donsetch.crawl.page` v3. `include_paths` / `exclude_paths` (CLI `--include` / `--exclude`) accept path globs.

Crawl respects robots.txt by default, shares per-host pacing across processes, and honors published waits. It starts on the direct lane; [pool routing](proxies.md) can spread subsequent work. `since_last` on a run is delta mode: unchanged pages cost no fetch, moved/new pages are fetched, and removed URLs are reported. `dataset` returns versioned JSON Lines.

An output cap, failed sitemap, failed page, policy denial, wall or deadline is not site-wide completeness. Inventory discovery has safety bounds of 100,000 URLs and 32 sitemap reads. Run jobs last 7 days (50-job LRU) and are resumed with the `job` id; pass `crawl resume <job>` (or `resume` alone in MCP). `crawl diff` uses the warm dossier (inventory hash, source, lastmod trust, stable page list, per-URL fingerprint/lastmod) and `--refresh` revalidates now; the dossier keeps at most 256 hosts, 20,000 inventory URLs and 4,096 page memories per host, evicting the least recently used page memory. `crawl sites` lists dossiers; `crawl forget <host>` removes one without touching the fetch cache.

Crawl receipts name their own stop: map/plan `covered`, `budget` or `empty`; run `covered`, `budget`, `empty`, `deadline`, `walled`, `cancelled` or `throttled`. A focus receipt also reports `n/m topics answered` and names an unanswered term; a focus that matched nothing says so. Map mode can finish as `InventoryComplete`. Plan mode reports its own `covered`, `budget`, or `empty` and maps budget to `MaxPages` in the crawl receipt; run mode reports `covered`, `budget`, `empty`, `deadline`, `walled`, `cancelled`, or `throttled`, and a wall carries class/vendor/status. Always inspect the completeness receipt too.

## Screenshot

```bash
donsetch screenshot https://example.com --out page.png
donsetch screenshot https://example.com --full-page --wait-ms 1500
```

Requires an [installed browser](setup.md#browser-for-screenshots). Default settle time is 600 ms; `wait_ms` allows up to 5,000 ms. The default deadline is 60 seconds. The URL and intercepted document requests pass safety checks.

MCP returns the inline PNG plus a server-local path. Remote clients should use the image block. It is a throwaway profile: no signed-in session and no state supplied to fetch.

## Result contract

Default MCP text starts with `[meta] {JSON}`, then the source Markdown. PNGs remain image blocks. Timing, engine, and escalation diagnostics live under `_meta`. A classified failure is a normal tool result with `ok: false`; malformed protocol requests remain JSON-RPC errors.

| Field | How to use it |
|---|---|
| `ok`, `content_ok` | Check before treating the response as the requested content. |
| `read_status` | `content`, `partial`, `probe`, `focus`, `section`, `outline`, `unchanged`, `thin`, or a failure status. |
| `content_complete` | Whether the returned read is complete. |
| `next_offset`, `next_page`, `resume` | Continue a read or crawl. |
| `code` | Stable cause, e.g. `wall.auth`, `wall.challenge`, `guard.ssrf`, `deadline.hit`, `network.dns`. |
| `errorKind` | `permanent`, `transient`, or `walled`. |
| `next_action` | The action the state calls for. |

CLI `--json` prints the full envelope. `-q` suppresses routine stderr statistics. Unknown arguments are refused.

### Client bridges

Some MCP bridges return the raw result object; others flatten text blocks. Normalize once at your integration boundary:

```js
const text = typeof result === "string"
  ? result
  : result.content.filter(block => block.type === "text")
      .map(block => block.text).join("\n\n");
const end = text.indexOf("\n");
if (!text.startsWith("[meta] ") || end < 0) throw new Error("Missing result state");
const state = JSON.parse(text.slice(7, end));
const markdown = text.slice(end + 1).trimStart();
```

`[mcp] text_only = false` (or `DONSETCH_MCP__TEXT_ONLY=false`) restores split `structuredContent` for unrecognized clients. Known clients that drop one surface keep compatibility folding. `status --json` reports the configured mode and its origin.
