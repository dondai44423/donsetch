# Architecture

How DonSeTch's four tools share one local Rust process. [Back to the README](../README.md).

## Workspace & components

The workspace has two crates: the product at the root (`src/`) and its fetching foundation in `crates/fetch/`. CLI commands and MCP tools run the same engine; [src/spec.rs](../src/spec.rs) generates both MCP schemas and CLI flags.

| Component | Owns | Source |
|---|---|---|
| Fetch layer | Coherent client identities, carried state, strict page cache, wall classification, site interfaces, embedded V8, mined HTTP replay | [crates/fetch](../crates/fetch/README.md) |
| DonSift | HTML extraction, typed content blocks, section breadcrumbs, reading views, link handles | [src/extract](../src/extract/) |
| DonSeek | Query compilation, keyless source planning, family RRF, BM25+/RM3/LTR ranking, source ledger | [src/search](../src/search/) |
| DonTread | Site model (`map`), topic-first planner (`plan`), v5 executor & durable jobs (`run`/`resume`), digest & `pN` handles (`get`), dossier delta (`diff`) | [src/crawl](../src/crawl/) |
| DonSheet | PDFium glyph + pixel fusion, reading order, tables, AcroForms, optional PP-OCR | [src/pdf](../src/pdf/) |
| Browser | Throwaway Chromium screenshots and `doctor --stealth --parity` captures | [src/browser](../src/browser/) |
| MCP | Stdio (with crash supervisor) and Streamable HTTP transports, client compatibility folding | [src/mcp](../src/mcp/) |

## Fetch: carry the client, classify the answer

![Simplified fetch routes and separate screenshot path](../assets/readme/fetch-route.svg)

An encounter checks the page cache, sends one paced request using a coherent identity and its isolated cookie jar, and classifies the response before trusting HTTP 200. When a document is walled or an empty shell, the verdict selects the cheapest honest route: another client profile, a native challenge hand-off, the site's public data interface (only when it answers the requested URL), a replayed HTTP request contract, or embedded V8.

### Eight coherent clients

`plain`, `chrome-local` (captured Chrome 156 / Linux + script-visible surfaces), `chrome` (156 / Linux), `chrome-win` (156 / Windows), `chrome-android` (149 / Android), `firefox` (151), `safari` (26.4 / macOS), and `okhttp` (5 / Android).

Each profile bundles TLS, HTTP/2 settings, header set/order, and client hints. Winners stay sticky per origin until failure; cookies and minted clearances never cross hosts or identities.

### Embedded V8: challenge mint & content lane

| Mode | Job | Output |
|---|---|---|
| Challenge | Runs a challenge page's scripts on the same wire identity | Minted credential or form navigation to spend and re-classify |
| Content | Runs an app shell's scripts against a data-plane DOM (skipped if the HTML already passes) | Assembled HTML verified against the content contract |

No browser process, CSS cascade, layout, or raster runs here. When a content run passes, `mine.rs` extracts the underlying JSON request whose records appear in the assembled page and saves a URL-templated contract per `(host, identity, URL shape)`. Subsequent visits replay that HTTP request before touching V8, dropping and re-mining on schema drift or refusal.

Outside page code, `envelope.rs` enforces hard limits: 256 MB JS heap, 128 MB typed-array store, 128 MB WebAssembly memory, 96 ops, one active V8 isolate per process, 10 s mint / 5 s content wall ceilings, and full URL/DNS/redirect/size guards.

### State & caching

- **Fetch state (`<cache>/donsetch/v5/state.json`):** up to 4,096 LRU origins holding identity scores, vendor priors, sticky routes, pacing timestamps, cookies, credentials, mined contracts, and cache trust, merged under a file lock (`DONSEEK_NO_DISK_STATE=1` keeps state in memory; [setup guide](setup.md#configuration)).
- **Page cache (`pages/`):** verified HTTP 200 responses keyed by `(identity, egress, URL)`. Repeat reads revalidate conditionally; unvalidated `max-age` reuse must be earned via unchanged checks, caps at 60 s, and distrusts lying origins for 7 days.
- **Product history:** tracks page fingerprints separately for `since_last` diffs. No login credentials or imported browser sessions are ever stored.

## Search: rank verified source answers

1. **Understand:** compile identifiers, query shape, operators, date windows, and explicit BCP-47 `lang`.
2. **Plan:** spend a 6-request budget across package registries, first-party APIs, dated feeds, and general indexes.
3. **Admit:** veto excluded `-term` hits, off-scope URLs, redirect wrappers, SERPs, and near-duplicates.
4. **Merge:** fuse across measured independent index families (`bing`/`ddg`/`yahoo` count as one family).
5. **Rank:** score with field BM25+, anchored RM3 expansion, and an offline-gated 22-feature linear ranker (active on `code`, `howto`, and `shopping`; fusion elsewhere). Search loads no ONNX runtime and downloads no model.
6. **Report:** attach source census, evidence snippets, completeness, and retrieval confidence; cache merged sets only at `high` confidence.

Google's default adapter (`google-cse`) uses the public search-element bootstrap + JSONP contract over native HTTP (1-hour version/OS/route-keyed token cache; 2 requests cold, 1 warm; quota responses rest the lane without rotating identity or egress; `google_wml.rs` stays offline for historical fixtures).

## Crawl: model, plan, execute, digest

1. **Model (`map`):** builds a deterministic site model (`llms.txt` APIs/links → sitemaps → feeds → seed/hub links) with structural templates, hub-depth sections, `(section, template)` families, site-supplied link labels, and a cached host dossier (`<cache>/crawl/dossier/<host>.json`, v4) — without fetching content pages.
2. **Plan (`plan`):** allocates `max_pages` across named `focus` topics first, then section/family breadth, then family members plus reserves, recording each row's reason and token estimate.
3. **Execute (`run` / `full`):** fetches one page at a time per host through the shared v5 `Fetcher` and `Governor`, persisting every completed page to `<cache>/crawl/jobs/<id>.json` for idempotent `resume`, and tripping a host circuit breaker on the first classified wall instead of retrying queued pages.
4. **Digest & delta (`get`, `diff`):** returns a site brief, boilerplate-stripped cards, and `pN` handles expandable offline via `crawl get pN` (or JSONL `donsetch.crawl.page` v3 via `--dataset`). `crawl diff` combines the dossier's `ChangeContract` and per-URL fingerprints to fetch only changed/new pages and report removed URLs.

## Extraction, PDF & screenshots

- **DonSift** converts HTML into typed blocks with section breadcrumbs, strips navigation furniture, and projects `must_contain`, `focus`, `toc`, `section`, or `since_last` views.
- **DonSheet** fuses PDFium glyphs with rendered page pixels to recover reading order, bordered tables, and forms, invoking lazy PP-OCR (`ocr` feature) only on scanned or corrupt regions.
- **Screenshots** launch an isolated Chromium browser to return PNGs and never share state with fetch, search, or crawl.

See [tools.md](tools.md) for caller contracts and [crates/fetch/README.md](../crates/fetch/README.md) for fetch internals.
