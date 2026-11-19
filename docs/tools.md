# Tool guide

Use the same four tools over MCP or the CLI. [Back to the README](../README.md).

Run `donsetch tools` for live MCP JSON schemas or `donsetch <command> --help` for CLI flags; [src/spec.rs](../src/spec.rs) generates both.

## Fetch

```bash
donsetch fetch https://example.com
donsetch fetch https://docs.python.org/3/library/asyncio-task.html --focus "cancel a task"
donsetch fetch https://doc.rust-lang.org/book/ --toc
donsetch fetch https://doc.rust-lang.org/book/ --section s3
donsetch fetch https://example.com/pricing --must-contain "free tier" --must-contain '/\$\d+/'
```

Returns clean Markdown (or a requested slice) across HTML, PDFs, feeds, JSON, plain text, and site adapters (Reddit, GitHub, Stack Exchange, YouTube, Wikipedia, docs, package registries — marked `via=adapter:…`).

| MCP argument | CLI flag | Behavior |
|---|---|---|
| `url` | Positional URL(s) | 1 to 12 URLs; batch calls apply the same view to each URL |
| `must_contain` | `--must-contain` (repeatable) | Up to 8 phrases or `/regex/imsx` patterns; normalizes case, spacing, quotes, emphasis, and numeric amounts (`$1.5bn` matches `US$1,500 million`). Verdicts: `match`, `partial`, `no_match`, `unverified` |
| `focus` | `--focus` | Lexical ranking over paragraphs, lists, table rows (with headers), and code; falls back to outline on miss |
| `toc` | `--toc` | Heading tree with `sN` IDs and section sizes |
| `section` | `--section` | Read one `sN` ID or heading title (including subsections); combinable with `must_contain` or `focus` |
| `max_chars` | `--max-chars` | Output cap: default 16,000 single URL / 48,000 batch total |
| `offset` | `--offset` | Resume from returned `next_offset` (character offset in rendered Markdown; keep the same view) |
| `links` | `--links` / `--links=handles` | `urls` (`true`) keeps full URLs; `handles` emits session-local `L-` handles; default keeps anchor text only |
| `since_last` | `--since-last` | Diff against the previous stored read (standalone view; returns full page on first read) |
| `image_text` | `--image-text` | OCR up to 4 content images (`ocr` build required) |
| `deadline_ms` | `--deadline-ms` | Hard call budget: 500–600,000 ms (per URL in batches) |

- **Mutually exclusive views:** `must_contain`, `focus`, `toc`, and `since_last` cannot combine with one another (`section` narrows `must_contain`, `focus`, or full reads; `offset` paginates full/section reads).
- **PDFs:** detected by `Content-Type` or `%PDF` magic bytes; fuses PDFium glyphs + pixels for layout, tables, and AcroForms, running PP-OCR on scanned/corrupt regions with explicit uncertainty markers.

## Search

```bash
donsetch search "rust async cancellation" --intent code --max-results 5
donsetch search "site:postgresql.org vacuum analyze"
donsetch search "attention is all you need" --intent paper --read 1
donsetch search "中華人民共和国 人口" --lang ja
```

| MCP argument | CLI flag | Behavior |
|---|---|---|
| `intent` | `--intent` | `auto` (default), `web`, `code`, `paper`, `news`, `entity` |
| `lang` | `--lang` | Explicit BCP-47 tag (`uk`, `ne`, `pt-BR`); only a region subtag sets a country market, and explicit `lang` uses local sources even when a paid provider is default |
| `max_results` | `--max-results` | Default 7, max 12 |
| `read` | `--read` | Fetch and extract up to 3 top pages inside the deadline (`page:` snippets) |
| `query_variants` | `--query-variant` | Up to 2 caller-written alternatives, returning a distinct set per variant |
| `deadline_ms` | `--deadline-ms` | Hard budget: 500–600,000 ms |

Supports `site:`, `filetype:`, `intitle:`, `"…"`, `-term`, exact identifiers, and versions. Script never implies language: without `lang`, Latin queries default to English (`en`, no country) and non-Latin queries send no language/country.

- **Answer metadata:** `confidence` (`high` | `medium` | `low` — retrieval agreement/coverage, not fact verification), `degraded`, `requested_results` / `returned_results` / `underfilled`, `source_type`, and `cache_age_ms` (only `high`-confidence merged sets are cached).
- **Diagnostics:** `donsetch search --sources`, `donsetch search doctor`, `donsetch search --explain "query"`.

### Parallel MCP (keyless, opt-in)

Set this in `donsetch.toml` and restart an existing MCP process. Then use the same
`donsetch search "rust async cancellation"` command or the `web_search` tool.

```toml
[search]
parallel_mcp = true
```

You can also set `DONSETCH_SEARCH__PARALLEL_MCP=true`. When enabled, CLI and
agent searches without an explicit `lang` send their queries and intent-derived
objectives to `https://mcp.parallel.ai/mcp`. No account or API key is needed.
This source is rate limited and uses either a direct connection or the
configured HTTP proxy. It reports its own errors. An explicit `lang` uses the
existing language-aware search path, and `read` and follow-up `web_fetch` use
DonSeTch's fetch path. Set `parallel_mcp = false` to restore the usual provider
order, including any configured API keys.

### Optional search providers with API keys

| Provider | Key name | Provider | Key name |
|---|---|---|---|
| Tavily | `tavily` | Brave Search API | `bravesearch` |
| Exa | `exa` | TinyFish | `tinyfish` |
| Serper.dev | `serper` | Parallel | `parallel` |
| SerpApi | `serpapi` | Bright Data SERP | `bd` / `brightdata` |
| SerpBase | `serpbase` | | |

Manage keys with `donsetch keys add <provider> <key>` (stackable per provider), `keys default local|<provider>`, `keys list`, `keys reset <provider>`, and `keys export|import`, then restart MCP. Paid providers run their own native filters and fall back down the chain (then local) on errors or exhausted credits; Bright Data's fetch-only `unlocker` zone is configured separately ([brightdata.md](brightdata.md)).

### Search plugins

Register any executable as a search provider (`donsetch keys add plugin searxng --cmd 'python3 /path/adapter.py' --test`). DonSeTch spawns it directly without a shell (default 30 s `--timeout`, 8 MiB stdout cap), pipes one JSON request to `stdin`, and reads one JSON response from `stdout`:

```json
{"format":1,"query":"rust async","max_results":8,"intent":"web","deadline_ms":30000}
{"format":1,"results":[{"title":"Example","url":"https://example.com","snippet":"Source text","score":0.9}]}
{"format":1,"error":"Rate limit reached","retryable":true,"error_kind":"rate_limited"}
```

Optional `error_kind` values: `invalid_key`, `credit_depleted` (parked until re-registered), `rate_limited` (60 s rest), `server_error`, `network_error`.

## Crawl

```bash
donsetch crawl https://docs.python.org/3/ --mode map --topic asyncio
donsetch crawl plan https://docs.python.org/3/ --topic asyncio --max-pages 10
donsetch crawl explain https://docs.python.org/3/ --topic asyncio --max-pages 10
donsetch crawl https://docs.python.org/3/ --topic asyncio --max-pages 10 --deadline 60
donsetch crawl get p1 --job j0123456789abcdef --section s2
donsetch crawl resume j0123456789abcdef
donsetch crawl diff https://docs.python.org/3/ --refresh
donsetch crawl sites
donsetch crawl forget docs.python.org
```

| Mode / CLI verb | Work |
|---|---|
| `full` (default) / `run` | Plan over the site model, execute one page at a time per host, persist job `<id>`, and return the evidence digest + `pN` handles |
| `map` | Build/read the site model (`llms.txt`, sitemaps, feeds, seed/hub links): site kind, sections, families, URL counts, freshness — zero content-page fetches |
| `plan` / `explain` | Preview planned pages (topics → breadth → family members + reserves) with per-row reasons and cost; `explain` (`explain=true`) adds the scored candidate table |
| `get` | Read stored page `pN` from a job (`--job <id>`, defaults to newest) with zero network I/O; supports `--section` and `--offset` |
| `diff` / `--since-last` | Compare against the host dossier (`--refresh` forces live revalidation), fetch only new/changed pages, and list removed URLs |
| `resume` | Continue unfinished job `<id>` (`crawl resume <id>` or MCP `resume="<id>"`); completed/walled jobs return idempotently with zero network requests |
| `sites` / `forget` | List cached host dossiers or delete one host's dossier (`fetch` cache untouched) |

MCP `focus` (CLI `--topic`) scores URLs and site link/title labels lexically (no embeddings or query rewriting). Every named term in `focus` is planned before breadth; receipts report `n/m topics answered` and name missing terms, falling back to breadth when nothing matches.

| Option | Default / range | Scope |
|---|---|---|
| `max_pages` | 10 / 1–200 | Pages planned (`plan`) and fetched (`full`/`run`) |
| `map_limit` | 120 / 1–50,000 | Flat URL list cap in `map` (model still summarizes full inventory up to 100k URLs / 32 sitemaps) |
| `max_depth` | 2 (`0` = seed only) | Discovery depth |
| `max_total_chars` (`--max-chars`) | 60,000 / 1–500,000 | Total page Markdown envelope |
| `per_page_max` | 8,000 / 1–40,000 | Per-page Markdown cap (`next_offset` marks truncated pages) |
| `deadline_s` (`--deadline`) | 120 / 1–600 s | Wall-clock budget |
| `include_paths` / `exclude_paths` | Auto-scoped to seed section | Path globs (`--include` / `--exclude`) |
| `dataset` (`--dataset`) | `false` | Emit JSONL rows under schema `donsetch.crawl.page` v3 (`handle`, `headings`, `card`, full `markdown`) |

- **Politeness, egress & storage:** honors `robots.txt` by default, shares cross-process host pacing, starts on direct IP before spreading across [proxy pool lanes](proxies.md), and halts a host on the first classified wall (`walled` with class/vendor/status) instead of retrying queued pages. Jobs keep 50 entries / 7 days; dossiers keep 256 hosts / 20k inventory URLs / 4,096 LRU page memories.
- **Stop reasons:** `map` finishes with `InventoryComplete`, `InventoryLimit`, `FrontierEmpty`, or `Deadline`; `plan` reports `covered`, `budget` (`MaxPages`), or `empty`; `run`/`full` reports `covered`, `budget`, `empty`, `deadline`, `walled`, `cancelled`, or `throttled`.

## Screenshot

```bash
donsetch screenshot https://example.com --out page.png
donsetch screenshot https://example.com --full-page --wait-ms 1500
```

Launches an [installed Chromium-family browser](setup.md#browser-for-screenshots) with an isolated throwaway profile (default settle `600 ms`, `wait_ms` up to `5,000 ms`, default deadline `60 s`), checking document requests against SSRF and local rules. MCP returns the inline PNG block plus server-local path.

## Result contract

Default MCP responses lead with `[meta] {JSON}` followed by Markdown (`--json` prints the full envelope on CLI; `-q` silences stderr stats). Classified failures return normal tool results with `ok: false`; protocol errors stay JSON-RPC errors.

| Field | Meaning |
|---|---|
| `ok`, `content_ok`, `content_complete` | Verify before treating text as complete target content |
| `read_status` | `content`, `partial`, `probe`, `focus`, `section`, `outline`, `unchanged`, `thin`, or failure status |
| `next_offset`, `next_page`, `job` / `resume` | Continuation handles |
| `code`, `errorKind`, `next_action` | Stable error code (`wall.auth`, `wall.challenge`, `policy.denied.<reason>`, `guard.ssrf`, `deadline.hit`, `network.dns`), retry class (`permanent`, `transient`, `walled`), and recommended next step |

### Client bridge normalization

```js
const text = typeof result === "string"
  ? result
  : result.content.filter(b => b.type === "text").map(b => b.text).join("\n\n");
const nl = text.indexOf("\n");
if (!text.startsWith("[meta] ") || nl < 0) throw new Error("Missing [meta] line");
const state = JSON.parse(text.slice(7, nl));
const markdown = text.slice(nl + 1).trimStart();
```

Set `[mcp] text_only = false` (`DONSETCH_MCP__TEXT_ONLY=false`) to emit split `structuredContent` for clients that read it natively.
