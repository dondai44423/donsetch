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
donsetch crawl https://docs.python.org/3/ --mode full --topic asyncio \
  --max-pages 10 --deadline 60
donsetch crawl --resume 'TOKEN_FROM_PREVIOUS_RESULT'
```

| Mode | Work |
|---|---|
| `full` · default | Discover sitemap inventory, then fetch content. |
| `map` | Return inventory; without a sitemap, use seed-page links. |
| `content` | Walk from the seed without sitemap discovery. |

MCP `focus` is CLI `--topic`. It scores link text and URL paths before fetch, then focuses extracted passages. This is lexical matching: a link sharing no query token is not enqueued.

| Budget | Default / range |
|---|---|
| `max_pages` | 10 / up to 200 |
| `map_limit` | 120 / 1–50,000 inventory URLs |
| `max_depth` | 2 / 0 means seed only |
| `max_total_chars` · CLI `--max-chars` | 60,000 / 1–500,000 |
| `per_page_max` | 8,000 / 1–40,000 |
| `deadline_s` · CLI `--deadline` | 120 / 1–600 seconds |

Character caps apply to page Markdown, not inventory and header metadata. Cut pages carry `next_offset` and `content_complete: false`; continue with fetch. `include_paths` / `exclude_paths` (CLI `--include` / `--exclude`) accept path globs.

Crawl respects robots.txt by default, shares per-host pacing across processes, and honors published waits. It starts on the direct lane; [pool routing](proxies.md) can spread subsequent work. `since_last` suppresses unchanged sources after rechecking. `dataset` returns versioned JSON Lines with per-page focus receipts.

An output cap, failed sitemap, failed page, policy denial, or depth limit is not site-wide completeness. Inventory discovery has safety bounds of 100,000 URLs and 32 sitemap reads. Resume tokens last 30 minutes and survive process restarts; pass `resume` alone.

Stop reasons include `FrontierEmpty`, `MaxPages`, `CharBudget`, `DepthLimit`, `Deadline`, `ThrottledOut`, `Cancelled`, and `InventoryLimit`. Map mode can finish as `InventoryComplete`. Always inspect the completeness receipt too.

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
