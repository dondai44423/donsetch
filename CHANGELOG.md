# Changelog

All notable changes to DonSeTch are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Snippets are 200 chars, cut on a word boundary.** 120 ended mid-word almost every time — `sequence transduc`, `All You Nee`, `the attention ` on a live query. The cost was not lost context but a wasted `web_fetch` to learn what the snippet nearly said. The cut trims back rather than extending, so output stays bounded by the budget; if the character past the window is whitespace the window already ends cleanly and is kept whole; backing off is abandoned when it would cost more than a fifth of the budget, since a long URL or an unbroken CJK run would otherwise strip the snippet to nothing. The ellipsis is appended only when text was actually dropped, so it never promises content that does not exist. Trailing marks that join clauses (`, ; : 、 ， 「 《 【`) are removed before it; sentence terminators (`. ! ? 。！？`, shared codepoints across Chinese and Japanese) stay, because a cut landing after one means the snippet ended on a complete sentence. (#40, @Mart-Bogdan)
- **Each result names the engines behind it**, with its blended score: `engines: bing, ddg · score: 0.83`. Which indexes agreed is what separates two equally plausible results — independent engines converging usually means canonical, a lone vertical hit often means tangential — and it was previously visible only in `structuredContent`. Names rather than a count: `consensus` there is `sources.len()`, which double-counts an engine that returned the URL at two ranks, while ranking counts index families. Deduped names are the honest version, and say *which* source. (#40, @Mart-Bogdan)

### Fixed

- **Whitespace in titles and snippets is collapsed at merge.** HTML-scraped engines normalized already; JSON-sourced hits did not — MDN summaries, BYOK provider snippets (Exa returns raw page text) and GitHub descriptions arrived with embedded newlines, breaking the three-space indent of the markdown list. Normalizing once in `rank::merge`, the single point every source flows through, also fixes the "longest snippet wins" and "shortest clean title wins" comparisons, which previously ranked on whitespace count: a newline-padded short snippet could beat a genuinely longer one. (#40, @Mart-Bogdan)
- **Clippy only ever linted Linux, and only lib and bins.** The ~32 `#[cfg(windows)]` sites — all of `ghost/proc.rs` — were never compiled by the lint pass, and neither were the 50 `#[cfg(test)]` modules or `tests/*.rs`, which the default lib+bins pass skips. Clippy now runs on Windows as well, with `--all-targets`. macOS stays out deliberately: its only exclusive site is one `target_os = "macos"` block, everything else being `unix` (shared with Linux) or `not(linux_like)` (shared with Windows), so Linux+Windows already covers it.
- **`rust-toolchain.toml` pins the local toolchain to 1.98**, matching the CI/release pin. v3.0.0 pinned the CI side to end local-vs-CI clippy drift, but nothing pinned the contributor's side, so a local clippy of a different version reports a different lint set — findings that CI does not have, and misses that it does.

## [3.1.0] - 2026-08-23

The focus release: the `focus` parameter rebuilt from flat BM25 block scoring to hierarchical section-aware scoring. Plus a homebrew tap URL fix.

### Changed

- **Section Gravity focus**: the `focus` parameter was rebuilt. The previous flat BM25 scoring treated every block in isolation: a heading match did not pull in its section, a body match did not pull in its heading, blocks were orphaned. Four mechanisms now replace it:
  - **Section Gravity**: a heading match pulls in its entire section. The heading defines the topic; all content under it is relevant.
  - **Inverse Gravity**: a body match pulls in its section heading. The agent needs the heading for context, never an orphaned block.
  - **Breadcrumb Expansion**: for each kept block, all parent heading blocks from its path are added. Structural context is never lost.
  - **Code Block Fission**: large code blocks (>2000 chars) are split into sub-blocks at logical boundaries (JSON top-level keys, blank-line sections) before scoring. A 38k JSON schema becomes scorable sub-blocks instead of one monolithic document.
  - The body-only match threshold is now `>0` (any keyword appearance) instead of `max*0.15`. Never cut relevant info: noise costs tokens, cut info is unrecoverable.
  - Fixed: focus on small pages no longer gets overridden by the raw-text fallback when the short content is intentional (the agent asked for a filtered slice, not a shell).

### Fixed

- Homebrew tap URLs included the version number in the asset filename (e.g. `donsetch-v3.0.0-darwin-arm64.tar.gz`) but the release workflow names assets without it (`donsetch-darwin-arm64.tar.gz`). This caused a 404 on `brew install donsetch`. (#38)

## [3.0.0] - 2026-08-23

The context-warfare release: six milestones — reference handles, budgets, probe and structure-first reading (M1); deadlines, real cancellation and ms-precision costs (M2); page fingerprints, deltas, Wayback resurrection and anti-cloak (M3); keyless domain adapters for reddit/npm/PyPI/crates/Go/RubyGems/GitHub/StackExchange/Wikipedia/docs frameworks (M4); search→fetch warm handoff, stitching and Chrome-parity TLS (M5); stable error codes, CI token/memory gates, a crash-only supervisor and the pi-agent v3 extension (M6). Plus a community fix for a Windows tier-1 boot hang (#36, @problaems).

The context-warfare milestone (v3 M1): every tool now respects the agent's context window as the scarce resource it is.

### Added

- **Reference handles (`L1`, `S1`)**: fetched-page links render as `[text](L12)` instead of raw URLs, and search results list `S1`-`Sn` instead of 80-token URLs. `fetch` accepts a handle anywhere it accepts a URL (`fetch S3` = result 3 of your last search). Handles are stable per URL (L) or per search position (S), persisted at `~/.cache/donsetch/handles.json` with a 24h TTL and 2048-entry cap. Raw URLs remain in `structuredContent` for citation.
- **Batch fetch with global token budget**: `url` now accepts an array (up to 12) — one parallel call instead of N round-trips. `budget_tokens` shares one output budget across all results, allocated by size (small pages stay whole, big ones slice with a resume note). Composed output carries per-URL status; only all-failed is an error.
- **Probe mode (`must_contain`)**: verification questions ("does the changelog mention CVE-2026-XXXX?") resolve the page fully but collapse the output to MATCH/NO-MATCH plus up to three short context excerpts (~60 tokens instead of 4k). Case-insensitive substring or `/regex/`.
- **TOC section IDs + sizes**: `toc=true` now renders `- [s3] Heading . 1.2k` — a stable per-section ID and content-size label. `section="s3"` targets by ID (heading-name matching still works). Read structure and cost before reading content.
- **Dropped-content manifest**: when `focus` removes blocks, the output gains one accounting line (`dropped by focus: 256 blocks (~12.1k words) — History, Early years, ...`). Omission is audited, never silent.
- **On-demand image OCR (`image_text=true`)**: fetches and OCRs the page's content images (up to 4, 5MB each, SSRF-guarded) and appends an `image text` section — infographics, comics and screenshot-locked pages become readable (the OCR engine ships with `--features ocr` builds; core builds say so honestly).
- Fuzz targets (`fuzz/`): `extract`, `charset`, `paginate`, `sitemap`, `feed` — the five panic-surface parsers, wired as CI smoke jobs with crash-artifact upload. The crate grew a library target (`src/lib.rs`) to support this; the binary is unchanged behavior.
- Supply-chain gate: `deny.toml` + cargo-deny CI job (advisories, licenses, bans, sources).
- `bench/tokens.py`: token-efficiency bench asserting the invariants (focus >=40% savings, probe <=400 chars, no raw-URL leaks past handle rewriting).

### Changed

- **Main-content scoring**: link density now discounts punctuation/paragraph mass too (a sidebar of link lists could outrank the real article on punctuation inside link labels), image `alt` text counts as content text, and structural region IDs (`footer`, `bottom`, `sidebar`, `nav`, ...) are excluded from main-content candidacy at any size. xkcd scoped to its sidebar before this; it now scopes to the comic.
- Media (`<img>`) elements are always segmented (cheap) and dropped at render time unless `media=true` — the image list must exist even when media lines are not rendered, so on-demand OCR works on any page.

### Fixed

- Comic/gallery pages (text-thin, image-rich) lost their content images when extraction fell back to raw text — fallbacks now carry the scoped image list through.
- CI and release workflows pin rustc 1.98 (was floating `stable`), ending local-vs-CI clippy drift.
### Added (M2 — the clock)

- **Deadline contracts (`deadline_ms`)**: fetch (single and batch, per-URL) and search accept a hard time budget (500ms–600s). On expiry: honest `deadline` error with a next_action that names the usual eater (browser escalation) — never a silent hang.
- **Real MCP cancellation**: `notifications/cancelled` now aborts in-flight work. Fetch/search drop via select (all persistent state was already written atomically); the crawl stops its workers gracefully through the existing stop-flag and persists its resume token — partial progress is never lost. Cancelled requests get no response, per spec.
- **Progress notifications**: requests carrying `_meta.progressToken` get `notifications/progress` beats — per-page during crawls ("12 pages, 34 queued", throttled to 2s) and per-URL during batch fetches.
- **Cost footer**: every fetch result's `[meta]` line and structuredContent carries `ms` — the agent sees what latency cost.
- Crawl stop reason `Cancelled` with its own next_action ("resume with the token above").

### Added (M3 — trust & memory)

- **Page fingerprints + change verdicts**: every completed fetch is fingerprinted (sha256 of the normalized full markdown, first 12 hex) and recorded in a persistent page history (`~/.cache/donsetch/page-history.json`, capped: 64KB text per URL, 4MB total, 512 URLs). The next fetch of the same URL stamps its verdict in `[meta]` — `changed (minor|changed|rewritten)` with an ago-seconds label — so a re-read after a hot edit is an informed decision, not a guess.
- **`since_last=true`**: collapses the fetch output to the verdict. Unchanged pages become one line ("unchanged since last fetch (300s ago) — fingerprint …"); changed pages return a section-level delta report (headings added/removed/changed, capped at 8) plus "refetch without since_last for full content". Re-watching a page costs ~30 tokens instead of 4k.
- **Archive resurrection (`archive=auto|only|off`)**: on a dead link (404/410/gone), `auto` transparently checks the Wayback Machine and, if a snapshot exists, returns it stamped `ARCHIVED COPY of <url> — snapshot <date> (<age> old)` with an honest age warning when the snapshot is stale. `only` goes to the archive directly; `off` preserves the raw error. Dead links stop being dead ends.
- **Anti-cloak equivalence check**: on domains known to serve decoy content to plain-HTTP clients (the wall registry), DonShadow's response is cross-checked against a headless render — text-similarity below the threshold appends a `decoy suspected` warning instead of confidently returning cloaked junk.
- **Freshness truth**: `structuredContent.server_modified` surfaces the server's own `Last-Modified` on successful fetches — cache-lie detection for the agent ("the page says 2024, the server says 2019").
- **Loud engine degradation**: search results from degraded engines carry a `*degraded: 3/5 engines ok (duckduckgo: timeout)*` line — silent quality collapse is visible in-band.
- **Delta crawl (`since_last=true`)**: crawl skips pages whose recorded fingerprint is still fresh (24h window), reporting each as `unchanged (since_last)` in the skipped list — re-crawling a site after an edit returns just what moved.

### Added (M4 — domain intelligence)

A keyless adapter registry for the sites agents actually hit. Fetch-level rewrites route page URLs to the site's own public JSON APIs (one plain-HTTP request for structured truth — often skipping the wall entirely); extract-level adapters restructure HTML the generic pipeline mangles. Every result is honestly labeled `via=adapter:…` in `[meta]` and structuredContent; any adapter miss falls back to the generic path, and an adapter failure (rate limit, login wall, non-JSON 200) transparently retries the ORIGINAL url through the full pipeline. Kill switch: `DONSETCH_NO_ADAPTERS=1`.

- **Reddit `.json`**: threads and subreddit listings fetched from the site's keyless JSON endpoints and rendered as comment trees with scores, ages, OP/sticky/NSFW flags and collapsed-reply counts; nested replies indented. Replaces the HTML scrape when available (an IP under Reddit's logged-out limit still gets the old.reddit/generic/ghost cascade).
- **Package registries**: npm, PyPI, crates.io, Go module proxy and RubyGems page URLs (e.g. `npmjs.com/package/react`) resolve to their JSON APIs and render one unified package card — description, current version, publish/update dates, license, repo, download counts, dependencies, deprecation/`DEPRECATED` warnings, yanked markers, and a recent-versions list that prefers stable releases over canaries. Version-specific URLs fetch the version manifest (crates.io version pages carry the dependency tree).
- **GitHub**: issue/PR lists, individual issues/PRs, releases and commits restructured from the server-rendered DOM (both the current React markup via stable `data-testid` hooks and the legacy markup). Issue lists: title, number, open/closed, author, date, labels. Issue threads: state, author, date, full body — plus an honest note that comments stream via JS (re-fetch with `tier=2` to read the discussion). No auth, no API rate jail.
- **Stack Exchange**: question + answers as a QA tree with per-post scores (from `data-score`), accepted-answer ✓ marking, asker/answerer authorship and asked-dates.
- **Wikipedia infoboxes**: the summary table (born/died/founded/license/versions…) becomes a clean `field | value` table at the top of the output, with the full article body (headings, paragraphs, data tables, lists) below — navbox/infobox duplication and citation markers stripped.
- **Docs frameworks**: mkdocs / Docusaurus / Sphinx / Antora sites (detected via generator meta or framework markers) prepend a compact `Site outline` built from the nav — the site map with cheap L-handle links — before the page content. Version-switcher noise filtered.
- `donsetch dev extract --url <url> --input <file>`: run the extraction pipeline on a saved HTML file against a URL (adapter development, fixture capture). `DONSETCH_ADAPTER_DUMP=<dir>` captures every body the adapters inspect.

### Added (M5 — speed & stealth)

- **Search→fetch warm handoff**: search enrichment already fetches the top results — that content is now cached (bounded: 10 bodies, 1.5MB each, 10min TTL) and the subsequent `web_fetch` of a result serves it instantly. `structuredContent.prewarmed_by_search: true`, tier reads `prewarmed` (the search→fetch second hop measured at ~3ms). One-shot: a second fetch goes to the wire for freshness; extraction, thin→ghost escalation and page history run unchanged on the cached body.
- **Route hints on search results**: domains the self-improving store knows need the browser are annotated in the results (`⚠ needs browser (~+6s)`) — the agent can pick a faster source or budget time before spending the fetch.
- **Article stitching (`stitch=true`)**: multi-page articles with rel=next pagination are walked (up to 6 parts, 48k budget, same-host only) and returned as ONE article with `*(part N)*` markers — an 8-part spread costs one call, not eight. `structuredContent.stitched` reports the part count.
- **h2 fingerprint parity gate**: DonShadow's h2 preface (SETTINGS values+order, connection WINDOW_UPDATE, pseudo-header order, no PRIORITY frames) is now asserted byte-identical to the Chromium capture in a CI test — any future divergence is a red build, not a silent detectability regression.
- **Locale-coherent Accept-Language**: the header now follows the target's locale (host TLD map + percent-encoded script in the path) — an en-US header on a .ru page gets the English stub on some sites and is a mild incoherence signal; localized sites now serve their real content. Default remains Chrome's en-US.

### Fixed

- **Daemon-abort panic in jsdata blob discovery (fuzzer find, CI fuzz gate)**: a known-global assignment (`__NUXT__ = `) matching at the very end of a page whose preceding byte was invalid UTF-8 (decoded to a 3-byte replacement char) advanced the scan cursor past the string / mid-character — `html[from..]` panicked. The cursor now floors to the next char boundary, clamped to the string length. Found by the new CI fuzz gate on its first green-config run; regression-tested with the crash input.
- **Windows tier-1 boot hang in the browser version probe (#36, @problaems)**: startup spawned a real browser (`--version --headless=new`) with no timeout to learn its version — on Chrome 129 the spawn hangs (crash-looping GPU/network services) and blocks every command at boot, leaving an orphaned process tree. The probe now reads the version from the browser's own registry key (`HKCU\Software\<Browser>\BLBeacon\version` — zero spawns, honours `DONGHOST_CHROME` families incl. Thorium/Edge) and hard-caps any spawned fallback at 3s with a whole-tree kill. Review follow-ups: non-Windows build stub, child cleanup on an early-out path, unit tests for the version parser.

### Added (M6 — foundation)

- **Stable error codes**: every error on all three tools carries a machine-readable `code` (`guard.ssrf`, `deadline.hit`, `network.dns`, `wall.challenge`, `wall.paywall`, `content.binary`, `crawl.resume`, `archive.stale`, `cloak.suspected`, …) alongside the prose and `next_action` — agents branch on codes, not string matching.
- **Token-efficiency CI gate**: the live claims (focus ≥40% savings, toc ≤5%, probe ≤2% of page, link rendering) are now asserted offline against saved real-page corpora on every build (`tests/token_invariants.rs`).
- **Memory soak gate**: 200 full-pipeline extractions + 10k handle churn + 800 page-history records with RSS growth asserted bounded (`tests/soak.rs`) — a creeping daemon is a build failure, not a surprise.
- **Crash-only supervisor**: `donsetch mcp --supervised` proxies stdio over a supervised child daemon — a panic-abort (or a SIGKILL) restarts the daemon (500ms backoff, 5-crash give-up), held requests are replayed, idle deaths are caught within 500ms, and the MCP session survives. Live-verified: SIGKILL mid-session, all requests answered after restart.
- **Homebrew tap**: `brew tap dondai44423/donsetch && brew install donsetch` (formula staged, published with the release).
- **Release workflow hardening**: release builds are `--locked` (deps can't drift mid-release); every platform binary must *report the tagged version* before packaging — a missed `Cargo.toml` bump fails the release job, not the user's `--version`; GitHub release notes are generated from `CHANGELOG.md` (curated) with commit-log notes appended, not the bare commit log.
- **pi agent extension v3**: tools now run under the crash-only supervisor (`mcp --supervised` — a SIGKILLed daemon no longer kills the pi session); pi's Esc/cancel forwards real MCP cancellation so server-side fetch/crawl work actually stops; tool cards surface v3 stable error codes (`[deadline.hit] …`) and `stitched ×N` pagination. Tool definitions are discovered live from the binary, so `pi update --extensions` picks up all of v3 with no extension-side pinning.

### Decision

- **HTTP/3: not in 3.0.0** (timeboxed spike concluded — see design notes): h3 fingerprinting is not yet a vendor signal, h2 fallback is first-class everywhere, and a second transport stack (quiche + duplicate BoringSSL) pre-3.0 trades proven reliability for an unmeasured signal. The bar to ship post-3.0 is documented.


## [2.5.0] - 2026-08-22

The polish & reliability release: one daemon-crashing charset bug fixed (#35), four panic-abort paths closed, one infinite hang capped, the error contract extended to every tool, and installation/upgrades hardened across platforms.

### Fixed

- **ghost-dom double-decoded browser text as GB18030 mojibake (#35)**: the headless-browser tier reads UTF-8 text from the live DOM via CDP — the browser already decoded the page. But the rendered DOM keeps the page's original `<meta charset=gb18030>` declaration, so the charset sniffer honored it and "decoded" the already-UTF-8 bytes a second time (末日乐园 → 鏈棩涔愐涯 on 69shuba). Browser-provided text is now pinned as UTF-8 (`GHOST_TEXT_CT`) at every extraction site (fetch ghost paths, actions, render cache, crawl ghost escalation). Raw HTTP bytes keep full detection — the v2.3.8 GBK/Big5/Shift-JIS fixes are untouched.

- **Daemon-abort panics (release builds run `panic=abort` — each of these was a one-request kill)**:
  - `js_unescape`: a literal backslash before a multi-byte UTF-8 character (hostile or sloppy page in a Next.js flight frame) advanced the cursor mid-character; the next string slice panicked. Copy the full character instead.
  - Pagination: unclamped `max_chars`/`offset` tool args wrapped `start + max_chars` below `start` (integer overflow) → slice panic. Now saturating arithmetic plus server-side clamps (`max_chars` 200..=1 MiB, `offset` ≤ 1e9).
  - Pagination resume: the 500-byte block-boundary search window could split a multi-byte character on CJK pages → slice panic. Window end is floored to a char boundary.
  - Ghost debug HTML dump could slice a multi-byte character at byte 1200.

- **Infinite hang**: `Cdp::connect` — the only unguarded network primitive in the ghost stack — could hang a tool call forever if the browser accepted TCP but stalled the WebSocket handshake. 10-second cap.

- **Unclamped action waits**: a `wait` step with `ms: 3600000` stalled the tool call for an hour with no cancellation path. Per-step waits cap at 30s, selector/text polls at 60s.

- **Crawl resume via CLI**: `donsetch crawl "" --resume <token>` errored with "url must be http(s)" before reaching the resume loader (the MCP path accepted it, the CLI didn't). Empty-URL resume-only invocation now works.

### Changed

- **Windows browser discovery** now probes Microsoft Edge install directories (often the only CDP-capable browser on a stock Windows box — its directory is never on PATH), per-user Chromium, and the Playwright cache. Ghost escalation, browser actions, and `doctor` work on default Windows installs.

- **macOS Intel (darwin-x64) supported end-to-end**: prebuilt binaries now build in CI (native `macos-15-intel` runner), `npm install` accepts the platform, and self-update maps it correctly. Core build (no OCR/rerank — `ort-sys` ships no prebuilt ONNX Runtime for Intel macOS; same trade-off as Linux ARM64).

- **npm install.js hardened**: musl (Alpine) systems are detected up front with an honest "glibc-linked binary will not run" error instead of a deferred cryptic spawn failure; `tar` presence is checked on Windows before downloading; stale/truncated leftover binaries (< 1 MiB) are re-fetched instead of shadowing a fresh install; extraction is verified before chmod.

- **Error contract extended to every tool**: `web_crawl` and `web_search` failures now return structured errors with escalation trace + `next_action` (crawl failures classified permanent vs transient — bad seed/expired token no longer masquerade as retryable); crawl ghost-escalation failures surface their reason (launch error, captcha, timeouts) in `skipped[]` instead of vanishing; SSRF / binary-content / extraction-failure errors carry `next_action`; zero-result searches suggest the available levers.

- **CLI exit codes honest**: `update`, `doctor`, and `rollback` exit 1 on failure (scripts gate on `$?`); bulk-fetch JSON mode no longer collapses walled/transient failures to the permanent exit code; signal exit code matches the received signal.

- **Search meta reports rerank state**: a silently-degraded cross-encoder (feature off / model failed to load) is now visible in `structuredContent.rerank` instead of stderr-only.

### Security / Reliability

- **Sitemap decompression bomb capped**: gzip sitemaps decompress through the same 64 MiB cap as every other path — a malicious `.xml.gz` could previously OOM the daemon via unbounded allocation.
- **HPACK hostile index 0**: `checked_sub` instead of unsigned wrap (protocol-violation byte from a hostile server).
- **MCP stdout write failures** now log and shut down instead of silently serving into a broken pipe while the client waits forever.
- **Update flow**: backup-copy failure warns before the atomic swap (rollback would otherwise be silently impossible); cookie-vault persist failure logs instead of silently dropping warm clearance state.
- **Key masking** (`donsetch keys list`) is char-boundary-safe for keys containing multi-byte characters.
- `fetch` validates URL parse up front — an unparseable URL can no longer flow through the pipeline with an empty host, poisoning domain profiles.
- `/tmp` literals replaced with `std::env::temp_dir()` (ghost screenshots, search debug dumps) — Windows-safe.
- `doctor`'s browser-timeout remedy is platform-appropriate (no `pkill`/`/tmp` advice on Windows/macOS).

## [2.4.1] - 2026-08-20

### Fixed

- **Cyrillic search results mangled (#28)**: search engine result pages were decoded with `String::from_utf8_lossy`, which produces replacement characters for non-UTF-8 encodings. A page in Windows-1251 (Cyrillic) showed question marks instead of text. Search now uses the full charset detection pipeline (`charset::decode`) that handles Content-Type, BOM, meta charset, and statistical detection.

- **Cached search results ignore max-results (#29)**: `rank::merge` trimmed results to `max_results` before caching. A first search with max=2 cached only 2 results; a later search with max=10 got the stale 2 from cache. Merge now always produces 12 results (the cache ceiling), the response trims to `max_results`, and the cache stores the full 12.

- **pi-extension.ts broke on [meta] block**: the pi extension read `content[0].text` which is now the `[meta]` block, not page content. Fixed to join all content blocks and skip `[meta]`-prefixed ones.

- **Japanese legacy encoding detection (Shift-JIS, EUC-JP)**: same tofu problem as Chinese GBK/Big5. Pages with no charset declaration in Shift-JIS or EUC-JP fell back to UTF-8 lossy, producing replacement characters. Statistical detection now covers Shift-JIS (detected by kana presence in decode) and EUC-JP (detected in the ambiguous 0xA1-0xFE range by kana in EUC-JP decode vs Hangul in EUC-KR decode).

### Changed

- Bump boring 5.1.0 -> 5.2.0, boring-sys 5.1.0 -> 5.2.0, tokio-boring 5.0.0 -> 5.2.0, futures-util 0.3.33 -> 0.3.34, actions/download-artifact v4 -> v8.

## [2.4.0] - 2026-08-20

### Fixed

- **Crawl fails on PDF with 3-second timeout (#26)**: PR #23 added a 3-second `spawn_blocking` timeout for PDF extraction in crawl to isolate ARM64 PDFium hangs. But 3 seconds is far too short for real PDFs: a 28 MB archive.org PDF takes ~70 seconds to process. The timeout is now 300 seconds (5 minutes), covering large PDFs while still preventing infinite hangs. `fetch` was never affected (it has no timeout on PDF extraction).

- **Claude Code and VSCode ignore text content when structuredContent is present (#27)**: some MCP clients (Claude Code, VSCode) show only one form of response, either text content blocks or structuredContent, and structuredContent takes precedence. When both are present, the text content (actual page markdown) is dropped, and the agent sees only metadata. Fix: all MCP responses now prepend a compact `[meta]` JSON text block containing essential fields (url, tier, verdict, content_ok, thin, next_offset, tokens_est, lang, title, pdf_pages) before the content. Clients that only show text now see both metadata and content. Clients that show both see slight redundancy (meta block + structuredContent), which is acceptable. Search results keep structuredContent-only (the user confirmed structured is more useful there). Error responses now include `next_action` in the text content for the same reason.

- **CLI output broken by [meta] block**: the CLI tool only extracted `content[0].text`, which became the `[meta]` block. Fixed to iterate all content blocks and skip `[meta]`-prefixed ones.

## [2.3.9] - 2026-08-20

### Fixed

- **`max_chars` ignored on PDF fetch (#25)**: the markdown output was correctly paginated, but the MCP `structuredContent` included the full `pdf.per_page` array with one entry per page. A 1032-page PDF produced 60K of per-page JSON alone, blowing past the MCP response limit even with `max_chars=400`. The `per_page` array is now capped at 50 entries; a summary (total pages, OCR pages, mean confidence) is always included, and `per_page_capped` signals when the detail was truncated.

## [2.3.8] - 2026-08-20

### Fixed

- **Chinese/CJK text shows tofu boxes and garbled encoding (#24)**: three bugs in charset detection caused Chinese (and Korean) text to decode incorrectly:
  1. **Content-Type charset was case-sensitive**: HTTP headers are case-insensitive, but `charset=` was matched case-sensitively. `Content-Type: text/html; Charset=GBK` fell through to the meta sniff, and if the page had no `<meta charset>`, the fallback was UTF-8 lossy, producing U+FFFD tofu for every CJK byte pair. Now case-insensitive.
  2. **Quoted charset values were dropped**: `charset="utf-8"` (with quotes) produced an empty label because the quote character was used as a split delimiter before the value was extracted. Now handles double and single quotes.
  3. **No statistical fallback for undeclared CJK encodings**: pages with no charset in Content-Type, no BOM, and no `<meta charset>` fell back to `String::from_utf8_lossy`, which turns GBK/Big5/EUC-KR bytes into replacement characters. Added byte-pattern analysis that distinguishes GBK, Big5, and EUC-KR by their lead/trail byte ranges, with a decode-and-compare fallback for ambiguous cases (all bytes in 0xA1-0xFE). The meta charset scan window also grew from 2 KB to 4 KB.

- **CJK Unicode ranges incomplete**: `char_script()` only recognized CJK Unified Ideographs (U+4E00-U+9FFF), Extension A (U+3400-U+4DBF), and Extension B (U+20000-U+2A6DF). Now also covers Extensions C-F, Compatibility Ideographs, Compatibility Supplement, Radicals Supplement, Kangxi Radicals, and CJK Strokes.

## [2.3.7] - 2026-08-19

### Fixed

- **Windows: debug builds die with `STATUS_STACK_OVERFLOW` (#18)**: the main thread's stack comes from the PE header, 1MB by default, against Linux's 8MB. DonSeTch runs its whole future tree there via tokio's `block_on`, and `fetch_tool`'s frame does not fit unoptimized: `cargo build` produced a binary that aborted in `__chkstk` before the function body ran. Release fit only because optimization shrank the frame. `build.rs` now requests 8MB (`/STACK` on MSVC, `-Wl,--stack` on MinGW), so the ceiling no longer depends on the build profile.

- **HTTP 304 (cached re-read) reported as `Blocked` at status 200 (#20)**: re-reading the same URL in one long-lived process (the MCP server) failed with `verdict: Blocked, status: 200`, even though the page was fine and the first read of it succeeded. A re-read asks the server "has this changed?", and an unchanged page answers HTTP 304 Not Modified with an empty body. Wall detection has no rule for 304, so that empty response scored as `Blocked`; the cached body, status and headers were then merged back in over it, but the verdict was left behind. The verdict is now re-scored over the merged body, as the fresh-cache path already did. This hit every read after the first, permanently, for any page served with an ETag but no `Cache-Control` (S3/CloudFront, nginx defaults). The CLI was never affected, since its cache lives and dies with each run.

- **Basic auth and proxy auth headers were corrupted by a base64 bug (#15)**: the encoder placed its `=` padding at the start of the final group instead of the end, so `user:passwd` encoded as `dXNlcjpwYXNz==QA` rather than `dXNlcjpwYXNzd2Q=`. Only credentials whose byte length was an exact multiple of 3 came out valid; everything else was rejected by the server. Covered by RFC 4648 test vectors.

## [2.3.6] - 2026-08-19

### Added

- **HTTP proxy support**: standard `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` environment variables are now respected by all tier-1 fetches and tier-2 Ghost browser launches. Follows the curl/wget convention: `HTTPS_PROXY` for https URLs, `HTTP_PROXY` for http URLs, `ALL_PROXY` as fallback, `NO_PROXY` for host-based bypass (exact match, suffix match with leading dot, and `*` wildcard). SOCKS5 proxies via `ALL_PROXY=socks5://host:port` are supported. The Ghost browser (Chrome) receives `--proxy-server` so tier-2 traffic also routes through the proxy.

- **Linux ARM64 (aarch64) prebuilt binaries**: GitHub Actions release workflow now builds `donsetch-linux-arm64.tar.gz` on `ubuntu-24.04-arm` (native ARM64), and the npm `install.js` postinstall script recognizes the `linux-arm64` platform (`process.platform=linux` + `process.arch=arm64`). `npm install -g donsetch` now works on aarch64 Linux. CI also runs the full test suite on `ubuntu-24.04-arm`.

### Fixed

- **HTTP basic auth dropped from URL userinfo (#15)**: the HTTP client discarded the `user:pass@` component when normalizing URLs, so every tier-1 request to a basic-auth URL went out unauthenticated. Credentials are now carried as an `Authorization: Basic` header, matching browser behavior. Also fixes the tier-2 regression where the ghost-retry with ghost cookies re-hit the auth wall and discarded already-rendered content.

- **macOS: visible, unresponsive Chrome window after tier-2 fetch (#14)**: on macOS, the Ghost browser was frozen with SIGSTOP after use, leaving a visible, unresponsive Chrome window on the desktop for up to 10 minutes. macOS now kills the browser on `GhostGuard::drop` (same fix as Windows in #11). The version probe (`probe_installed_major` and `check_chrome` in doctor) now passes `--headless=new` + temp `--user-data-dir` on macOS to avoid opening a visible window during version detection.

- **Termux (Android) build fails at 3 points (#16)**: (1) `boring-sys` panics on Android targets without `ANDROID_NDK_HOME`. Documented workaround: `export ANDROID_NDK_HOME=$PREFIX` before building. (2) `build.rs` panicked on `target_os = "android"` with no PDFium source. Android now uses bblanchon's shared library (`libpdfium.so`) instead of kognitos' glibc-targeted static archive (`libpdfium.a`), linked as `dylib=pdfium` with `c++_shared` and `log`. (3) `known_chrome_paths()` was `#[cfg(target_os = "linux")]` only, so Android failed to compile. Introduced `linux_like` cfg flag (emitted by `build.rs` for both `linux` and `android` targets) to share all Linux code paths with Android.

- **Linux headless fallback**: when no Xvfb and no DISPLAY are available on Linux (WSL, headless server, container), Ghost now falls back to `--headless=new` mode instead of silently failing.

## [2.3.5] - 2026-08-19

### Fixed

- **Windows: orphaned Chrome processes after every fetch (#11)**: `AssignProcessToJobObject` requires both `PROCESS_SET_QUOTA` **and** `PROCESS_TERMINATE` on the process handle, but only the former was requested. The call failed with `ERROR_ACCESS_DENIED`, leaving the Job Object empty, so `KILL_ON_JOB_CLOSE` had nothing to kill when the handle dropped, and the whole browser tree outlived donsetch. Because the orphans inherit donsetch's stdout, any pipeline calling donsetch would also block until they were killed by hand, which looked like donsetch itself hanging.

- **Silent Job Object assignment failure**: the failure branch was empty, so this degraded silently. It now warns unconditionally and names the consequence, matching the existing convention for failure-with-fallback messages.

## [2.3.4] - 2026-08-19

### Added

- **Termux (Android) support**: first-class native support for Termux. DonSeTch auto-detects Termux via `$PREFIX` env var, finds Chromium at `$PREFIX/bin/chromium-browser`, skips Xvfb (uses `--headless=new` mode since Android has no X11 by default), and the doctor reports correctly. Build: `pkg install rust clang make pkg-config go lld && cargo build --release`.

### Fixed

- **Linux headless fallback**: when no Xvfb and no DISPLAY are available on Linux (WSS, headless server, container), Ghost now falls back to `--headless=new` mode instead of silently failing. Previously, the browser would try to connect to a non-existent display and crash.

- **build.rs Android target**: LLD auto-detection, PDFium target pair mapping, and target triple now all handle `target_os = "android"` correctly. Android uses the same Linux ELF static archives for PDFium.

## [2.3.3] - 2026-08-19

### Fixed

- **Windows: Chrome window popping up during search/fetch (#10)**: `probe_installed_major()` ran `chrome.exe --version` without `--headless`, which opens a visible GUI window on Windows (and may pop the profile picker since no `--user-data-dir` is passed). The probe now passes `--headless=new` plus a temp `--user-data-dir` on Windows so Chrome prints the version and exits silently. Result is cached in a `OnceLock` so the probe runs at most once per process. Same fix applied to `check_chrome()` in doctor.

- **Windows: Chrome not auto-closed after tier-2 fetch (#11)**: the Ghost browser was frozen (not killed) after use, leaving a visible, unresponsive Chrome window in the taskbar. On Windows, the browser is now killed immediately when the GhostGuard drops. The Proc's Drop closes the Job Object handle, triggering `KILL_ON_JOB_CLOSE` which kills the whole browser tree. The warm-browser optimization is sacrificed on Windows for a clean user experience.

- **WSL: Xvfb fails to start (#12)**: `/tmp/.X11-unix/` directory may not exist under WSL and minimal container setups, preventing Xvfb from creating the X11 socket. The directory is now created with `create_dir_all` before starting Xvfb. Startup timeout increased from 5s to 10s for slower environments. Error messages updated to be distro-agnostic (`apt install xvfb` alongside `pacman`).

## [2.3.2] - 2026-08-18

### Fixed

- **Linux ARM64: default build now works out of the box**: `ocr` and `rerank` are no longer default features. ONNX Runtime's C++ global constructors (protobuf `InitProtobufDefaultsSlow`) deadlock at startup on aarch64 Linux before `main()` is reached, making the full-feature binary hang indefinitely. The default build (fetch, search, crawl, PDF) works standalone. CI and release builds explicitly enable both features with `--features ocr,rerank`.

- **Linux ARM64: LLD auto-detection**: GNU ld on aarch64 rejects LLVM-produced PDFium static archives (reports "architecture: UNKNOWN!"). `build.rs` now auto-detects `ld.lld` and injects `-fuse-ld=lld` when available. No manual `RUSTFLAGS` needed. Warns if LLD is missing on aarch64.

- **Snap Chromium resolution**: `/snap/bin/chromium` is a symlink to `/usr/bin/snap` and doesn't reliably pass CDP flags through Snap confinement. Ghost now resolves Snap wrappers to the real Chromium binary inside the snap mount (`/snap/chromium/current/usr/lib/chromium-browser/chrome`).

- **Doctor: accurate feature reporting**: OCR and rerank checks now report "not compiled" when the binary was built without those features, instead of showing "not cached".

- **OCR/rerank init timeout safety**: ONNX Runtime initialization (both OCR and reranker) now runs in a separate thread with a 30s timeout. If ONNX's C++ constructors deadlock, the tool degrades gracefully instead of hanging forever. Reranking falls back to RRF+BM25; OCR falls back to the glyph stream.

- **Build-time aarch64 + ONNX warning**: when `ocr` or `rerank` features are explicitly enabled on aarch64 Linux, the build script emits a warning about potential startup deadlocks.

## [2.3.1] - 2026-08-17

### Fixed

- **Crawl: auto-scope drift on multi-tenant hosts** — seeding a crawl at `docs.rs/tokio` (single-segment path) returned `None` from `auto_scope`, causing the crawler to explore the entire `docs.rs` sitemap instead of staying within `/tokio/`. Fixed: single-segment paths now scope to `/{segment}/*`. Before: 383 off-topic pages fetched (async-blocking-bridger, asm_block, etc.). After: 5 pages, all within `/tokio/`.

- **Crawl: focus filter false positives from compound terms** — `focus_match` tokenized `spawn_blocking` into `spawn` + `block`, then matched `block` against unrelated paths like `/ant-libp2p-allow-block-list/`. Fixed: compound terms (containing `_` or `-`) are matched as full substrings OR require ALL fragments to match. `spawn_blocking` must appear as `spawn_blocking` in the path, or both `spawn` AND `block` must be present.

- **Fetch: content density threshold too high** — lowered from 50KB to 20KB raw and 5000 to 3000 chars extracted. Sites like artstation (91KB raw, 866 chars, 0.9% density) now correctly escalate to tier 2. Sites like bilibili (24KB raw, 1476 chars, 6% density) are not flagged.

- **Fetch: ghost settle time increased to 4s** — 3s was not enough for some SPAs (crates.io occasionally settled at 8KB before hydration). 4s gives SvelteKit/React enough time to download, parse, and execute JS bundles.

- **Doctor: TLS fingerprint false warning** — `tls.peet.ws` being unreachable showed a warning in `donsetch doctor`. Changed to Pass: the TLS stack is active (used for every fetch); the external fingerprint service being down is not a DonSeTch issue.

- **Tests: crawl_cycles_terminate with root seed** — test was seeded at `/a` which auto-scoped to `/a/*`, preventing the root page from being fetched. Fixed: seed at `/` (root) so auto-scope returns `None` and all paths are in scope.

## [2.3.0] - 2026-08-17

### Fixed

- **False positive ContentOk on SPA shells** — pages that server-render their layout (navigation, sidebar, footer) but client-render the main content produced enough boilerplate text (> 800 chars) to pass the thin check. The tool returned this boilerplate as content without escalating to tier 2. Added content density check: if raw HTML is > 50KB and extracted text is < 5% of raw with < 5000 chars, the page is classified as a JS shell and triggers tier-2 escalation. Measured false positives: artstation (0.9% density), all caught. Real pages: 15-40%+ density, never triggered.

- **Ghost (tier 2) settles too early on SPA shells** — the ghost_fetch content-quality oracle settled after 2 stability polls (~400ms), before SPAs had time to hydrate and render their content. A stable 8KB DOM at 400ms is a SvelteKit/React shell, not a complete page. Added a minimum settle time of 3 seconds for DOMs < 50KB, giving SPAs time to download, parse, and execute their JS bundles. Large DOMs (>= 50KB) settle fast as before. Fixed: crates.io (SvelteKit, was 8KB shell, now 47KB full render), users.rust-lang.org (Discourse, was intermittent 30KB shell, now consistent 397KB full render).

- **Pi extension TUI: truncateToWidth ANSI leak** — pi-tui's `truncateToWidth` function injects `\x1b[0m` RESET codes around the ellipsis even when the input is plain text. These RESET codes broke pi's green/red tool-call overlay mid-line, causing text to fall outside the highlight. Replaced all `truncateToWidth` calls with a local `truncate()` function that adds zero ANSI codes.

## [2.2.4] - 2026-08-17

### Fixed

- **Pi extension TUI: visual glitch fixed** — stripped ALL ANSI color codes from renderCall and renderResult. Plain text only. Pi wraps tool calls in its own green (success) / red (failure) highlight; our ANSI RESET codes were breaking pi's overlay mid-line, causing text to fall outside the highlight and show with the TUI background color.

## [2.2.3] - 2026-08-17

### Fixed

- **Pi extension TUI: removed all green/red ANSI from renderResult** — pi handles success (green) and failure (red) coloring itself. Our own green/red codes bled into pi's highlight causing a visual glitch. renderResult now outputs only amber (tool name) and dim (metadata).

## [2.2.2] - 2026-08-17

### Changed

- **Pi extension TUI: provider + cache display** — search results now show the provider (`via local`, `via exa`, `via tavily`), so the agent and user can see which engine was used. Fetch results show `via cache` when warm cookies were used (not a fresh fetch) and `via ghost` when the browser escalated.
- **Pi extension TUI: success/fail coloring fix** — removed all green and red ANSI codes from renderResult. Pi's TUI already wraps successful tool calls in green and failures in red; our own green/red codes bled into pi's highlight causing a visual glitch. renderResult now outputs only amber (tool name) and dim (metadata) — pi handles the success/fail coloring.

## [2.2.1] - 2026-08-17

### Changed

- **Crawl auto-scope** — when `include_paths` is empty, the crawl now auto-derives a path scope from the seed URL's path. `docs.rs/tokio/latest/tokio/` stays within `/tokio/latest/tokio/*`; `github.com/tokio-rs/tokio/wiki` stays within `/tokio-rs/tokio/*`. Multi-tenant sites (docs.rs, github.com) and multi-section sites (stripe.com, nextjs.org) no longer escape the seed's section. The user no longer needs to manually set `include_paths` for the common case.
- **Focus filtering on all link discovery paths** — when a `focus` query is set, links with zero focus-token matches are now filtered from BFS outlinks, pagination `<link rel="next">`, RSS/Atom feed entries, and sitemap frontier seeding. Previously only the sitemap map display was focus-filtered; all discovered links were enqueued regardless of relevance. The filter uses a smart soft/hard approach: if the current page has any matching links, non-matching links are hard-filtered (only relevant pages crawled). If no links match (e.g., a homepage linking to a tutorial that links to the target content), non-matching links are soft-filtered (enqueued at low priority) to enable multi-hop discovery.
- **Junk path filtering** — common non-content paths (`/login*`, `/signin*`, `/signup*`, `/register*`, `/auth*`, `/oauth*`, `/account*`, `/settings*`, `/cart*`, `/checkout*`, `/favicon*`) are now excluded by default, merged with user-specified `exclude_paths`.
- **Faster crawl pacing** — base inter-request delay reduced from 300ms to 200ms; skim dwell cap reduced from 300ms to 100ms. Roughly 2x faster crawls with zero observed throttling on test sites.

### Fixed

- **Sitemap focus filter bug** — the sitemap filter used `score <= 0.0` which incorrectly filtered deep but relevant pages (depth_prior made the total score negative even with a focus token match). Replaced with `focus_match()` which checks for any token match regardless of depth.
- **Sitemap seeding not focus-filtered** — sitemap entries were seeded into the frontier without focus filtering (only the map display was filtered). Now all sitemap-seeded entries pass the focus gate.

### Added

- **`next_action` in crawl output** — when the crawl returns 0 pages or stops early, the structured output now includes a `next_action` field with actionable guidance: "use mode=content", "try broader include_paths", "the site blocked the crawler", "resume={token} to continue", etc.

## [2.2.0] - 2026-08-17

### Fixed

**Reliability: the self-improving fetch loop actually self-improves now.** Four compounding bugs made ghost-solved domains re-need the ghost forever and occasionally served bot-wall pages as content:

- **Fake solves** — the tier-2 oracle settled on modern Cloudflare interstitials ("Performing security verification", ~344 visible chars of vendor boilerplate) and recorded them as solved, then replay-served the wall page as `ContentOk`. New interstitial detection layer (title/H1 boilerplate + near-empty-DOM-with-challenge-markers shapes) runs before the visible-text override in `detect_dom_smart` and `detect`. The ghost now waits for real clears.
- **Learning was gated off on re-solves** — a `skip-to-solve` re-fetch (cookies past their TTL) never called `record_solved` because `learn` required a fresh tier-1 challenge, so expired domains went ghost-first forever. Learning now fires on every wall-driven escalation. Live-verified: solve once → next fetch rides warm tier 1 in ~0.4s.
- **State poisoning** — ANY non-content verdict (404, 429, paywall, auth wall) marked domains `needs_tier2`, forcing a 20s ghost launch on every later fetch of that domain. Only real `Challenge` verdicts set the flag now; terminal verdicts move counters only. One-time migration un-poisons existing profiles that never recorded a solve (144 → 15 in the dev state file).
- **Warm-stale over-learning** — a single walled warm fetch (often transient challenge rotation) cleared the cookie vault and clamped `observed_lifetime` to as low as 1 second (the live stackoverflow case), killing warm routing permanently. Two consecutive failures are now required, and the learned lifetime is floored at 120s.
- **`replay_ok` gating** — warm routing now requires the post-solve tier-1 retry to have VERIFIED that these cookies actually work on tier 1 (some vendors bind clearance to the browser fingerprint; replay is impossible there). Unverifiable cookies never earn a doomed warm roundtrip again.
- **Ghost 404 laundering** — on skip-to-solve routes the ghost happily rendered 404 pages (browsers do) and the pipeline served them as `ContentOk`. The post-solve tier-1 retry is now the oracle of record for terminal verdicts (404/paywall/auth): dead URLs return honest errors.
- **Version coherence** — tier 1 claimed Chrome 150 headers while the ghost ran the installed Chromium 151 (client hints advertise the real version even under `--user-agent`). The installed browser's major version is now probed at startup and both tiers advertise the same coherent identity — clearance cookies bind to it.

**DonSift content fidelity** (the agent-reported gaps):

- **Math is no longer destroyed.** `<math>` elements are recovered as LaTeX: MediaWiki `alttext` first (with the `{\displaystyle}` wrapper stripped), then `<annotation encoding="application/x-tex">`, then a compact MathML serialization (`W_{Q}^{T}`, `(QK^{T})/(sqrt(d_{k}))`, matrices as `(a, b; c, d)`). Hidden-math exception: `display:none`/`aria-hidden` wrappers around `<math>` (the a11y twin of rendered formula images — MediaWiki, MathJax, KaTeX shape) are extracted instead of skipped. Live-verified on the attention-paper Wikipedia page: every formula and matrix variable renders. `<sup>`/`<sub>` content is preserved as `^{...}`/`_{...}` (only citation markers like `[1]` are dropped).
- **Discussion threads are no longer lossy.** Hacker News gets a dedicated extractor (threads AND the 2026 comment-permalink layout): full comment text (was: table cells truncated at 120 chars / entire subtrees dropped), authors, ages, reply depth via indentation, story header with points. Generic fix for other forums: layout/prose tables (any cell ≥300 chars, single-column tables, `role="presentation"`) are walked as containers instead of rendered as pipe tables; `class="comment"` is no longer treated as boilerplate (it silently removed whole comment sections from scoring).
- **Feeds render as feeds, not raw XML.** RSS 2.0 / Atom / JSON Feed → structured markdown: channel header, items with linked titles, dates, HTML-stripped summaries (was: 25KB CDATA blob). Handles lying Content-Types (`text/xml`, `text/plain`) by payload sniffing, and the HTML-parser traps (`<link>` void-element mangling, CDATA leakage) via preprocessing.
- **Thin-hole closed** — a 27KB page extracting 250 chars over 3+ boilerplate blocks was classified non-thin (how challenge pages leaked through). Any page over 5KB yielding <800 chars is thin now.
- **HTML served as `text/plain`** is parsed as HTML instead of passing through as angle-bracket soup.
- **`tokens_est` is honest** — dedicated extractors reported full-document token counts instead of the returned slice's.

**Fetch and escalation:**

- **`/pdf/` path convention honored everywhere** — `arxiv.org/pdf/1706.03762` previously skipped PDF early-detection (only `.pdf` suffix counted), escalating to a 23s ghost roundtrip; now routed straight to DonSheet (0.7s, tier 1).
- **Walls never enter the revalidation cache** — a challenge interstitial carrying an ETag was re-served fresh as content on later fetches; fresh-cache hits also get honest verdicts now instead of hardcoded `ContentOk`.
- **Warm cookies are no longer killed by extraction gaps** — a warm `ContentOk` that extracts thin is only treated as a shell when the body is big with almost no visible text (real shell evidence); rich-visible-text pages with thin extraction keep their valid cookies.
- **Turnstile clicks retry** — the checkbox iframe renders late and repositions; the old one-shot click usually fired before it attached. Up to 3 attempts, re-finding geometry each time. (Interactive captchas remain an honest dead end by design.)
- **Section slices no longer trigger ghost escalation** — a small `section=` result on a huge page computed as "thin" (shell) and escalated to the browser, which returned the FULL page instead of the requested section. A matched section is intentionally small; shell detection is skipped for it.
- **Math brace fidelity** — the `\displaystyle` wrapper strip removed exactly one closing brace per formula (`W_{Q}` stayed intact; the previous `trim_end_matches` ate inner braces).
- **HN threads honor `focus`** — relevant comments surface on 700-comment threads (with the standard no-match notice); previously the dedicated extractor ignored the query and returned the first N comments.
- **Legacy lifetime de-poisoning** — pre-fix `observed_lifetime` values below the 120s floor are dropped at load AND on each new solve; stackoverflow (clamped to 1s by the old bug) rides warm tier 1 again.

### Added

- **Crawl explains its pace** — when a site's robots.txt declares `Crawl-delay` and it's honored, the crawl output says so (`robots crawl-delay: 30s between requests (site-declared; pass respect_robots=false to override)`) plus `crawl_delay` in structuredContent. A slow crawl is no longer a mystery.
- **Feed extraction surface** — feed URLs return `content_kind: Listing` with item counts in `blocks_total`/`blocks_shown`.

## [2.1.2] - 2026-08-16

### Added

- **Pi agent TUI rendering** — custom `renderCall` and `renderResult` for all 3 tools in the pi extension. Tool calls show a clean amber icon + tool name + key arg (URL or query). Results show a compact status line (✓/✗ glyph, tool name, metadata) plus a one-line preview. No more raw content dumps in the TUI — the LLM still gets full content, the user sees a clean summary card. Amber theme matching DonSeTch's identity (#ffb200).

## [2.1.1] - 2026-08-16

### Added

- **Pi agent support** — `pi install npm:donsetch` now works natively. The npm package ships a pi extension that spawns the donsetch MCP binary at session start, discovers tools dynamically via `tools/list`, and registers them as native pi tools. Zero configuration, zero maintenance — tool definitions are fetched from the binary, so they stay in sync automatically. If the binary is missing (e.g. npm blocked postinstall), the extension auto-downloads it from GitHub Releases.
- **Tool-def token optimization** — cut 203 tokens of duplicated/redundant text from MCP tool descriptions (2,566 → 2,363 tokens, measured with tiktoken/GPT-4o). No quality loss — all behavior guidance preserved.

## [2.1.0] - 2026-08-16

### Added

- **`donsetch status`** — one-glance overview: version + update check, search config (providers, keys, default mode), proxies count, cache size, and health hint. No probes, no browser launch — fast. The "I just installed it, what's the state?" command.
- **`donsetch help <command>`** — route to any command's help: `donsetch help keys`, `donsetch help proxy`, `donsetch help fetch`, etc. Falls back to top-level help for unknown commands.
- **`donsetch keys default local`** — set the local keyless search engine as the default search method, even when BYOK provider keys are configured. When local is the default, the local 5-engine search is tried first and BYOK keys are only used as fallback if local search fails. This lets users test or use the local engine without removing their keys. `donsetch keys default <provider>` switches back to BYOK-first mode.
- **`donsetch keys export [path|-]`** — export all BYOK keys and config to a file (with 0600 permissions) or stdout (with `-`). Useful for backup, transfer between machines, or dotfiles repos.
- **`donsetch keys import <path>`** — import a config from a file previously exported by `keys export`. Replaces the current config entirely. Validates structure (provider names, key states, default) before saving.
- **`donsetch keys clear`** — remove all keys and reset to a clean state. The nuclear option for starting fresh.

### Fixed

- **Proxy missing from top-level help** — `proxy` command was not listed in `donsetch --help`, making it undiscoverable. Now shown in the MANAGEMENT section alongside `keys`, `doctor`, `update`, etc.
- **`proxy remove` now accepts numeric indices** — `proxy list` displays proxies as `1, 2, 3, ...` but `proxy remove` only accepted `host:port` or full URLs. Now `donsetch proxy remove 1` works. Handles multiple indices (`remove 1 3 5`) with correct order-of-operations (collects all first, removes in reverse to avoid index shifting). Backward compatible with `host:port` and full URL arguments.

## [2.0.0] - 2026-08-16

The v2 quality jump — a direct response to the 50-case
DonSeTch-vs-Hound comparison. Search top-1 decisiveness, browser
actions inside fetch, honest telemetry on every result, crawl
elastic pacing, and a browser path that's boring to install.

### Added

- **Browser actions in `web_fetch`** — page control inside fetch: `actions=[{...}]` runs click / type / press / scroll / hover / wait steps in the headless browser BEFORE extraction. Deterministic waits (`wait_selector`, `wait_text`), element addressing by CSS selector or visible text, human-cadence typing (log-normal key gaps, think-pauses), trusted CDP input events with bezier mouse paths. Up to 16 steps, validated before any browser time is spent. After the script, the normal extraction pipeline runs (focus/section/toc apply to the interacted page). Per-step results in `structuredContent.actions`; the first failing step aborts honestly with everything that succeeded. Form submits, search flows, load-more, lazy-load scrolls — one call, no separate browser tool.
- **Authority-aware search ranking** — the decisive top-placement layer. v1 had top-5 recall (23/25) but weak top-1 placement (6/25 vs hound's 13/25); v2 measures **29/30 top-1, 30/30 top-3** on the 30-query regression suite (`bench/regression.py`). Query-aware official-domain registry (~130 tech entries), title entity-term coverage with exact-phrase bonus, docs-seeking amplification, paper-repository authority for research queries, and news freshness ranking (the `published` field was dead data in v1 — it ranks now).
- **Escalation trace** — every fetch result (success AND error) carries `structuredContent.escalation`: the ordered steps actually taken (route decision → HTTP fetch → browser launch → ghost render → cookie retry → fallbacks) with per-step latency. A 3-second fetch is no longer opaque.
- **Structured error contract** — errors now carry `structuredContent {url, status, verdict, next_action, escalation}`. `next_action` is a one-line instruction derived from the failure kind (retry with tier=2, wait 30-60s, needs credentials, use an interactive browser). The CLI JSON envelope surfaces it too.
- **New success fields** — `content_ok` (true content, not a JS shell), `quality` (0-1 content trust, previously computed but never surfaced), `lang`.
- **PDF per-page stats** — `structuredContent.pdf = {pages, per_page: [{page, chars, ocr, confidence}]}`: per-page extraction confidence (glyph trust for text pages, OCR mean confidence for scanned pages), page boundaries preserved where block merging deliberately flows text across pages.
- **Doctor browser proof** — doctor now checks Xvfb (with :99 reuse detection), performs a REAL browser launch through the exact tier-2 code path with the fingerprint selftest (webdriver=false verified, 40s bound), verifies ghost-state.json permissions (auto-tightens to 0600), and reports the rerank model cache. 13 checks total (was 9). All new paths are platform-neutral (macOS/Windows report Xvfb as not-needed and use off-screen headful).
- **Search regression suite** — `bench/regression.py`: 30 queries with canonical domains defined upfront, measuring hit@1/3/5. The report's bar (official/primary in top-3 for ≥80% of tech-doc queries) passes at 100%.

### Fixed

- **arXiv PDF false "blocked"** (from the 50-case report): wall detection marker-scanned PDF bytes as lossy text — a Cloudflare-fronted paper containing "attention required" plus a cf-ray header produced a Blocked verdict at HTTP 200. Binary bodies (PDFs, images, archives) are now exempt from HTML marker scanning on 2xx; bot walls speak HTML. Non-2xx still classifies normally.
- **Cloudflare "Enable JavaScript and cookies to continue" shells** (report: "do not call a response successful when it only contains…") are now Challenge, never success.
- **Crawl latency** (report: 6.29s median vs 0.45s): v1 slept ~2.7s/page (700ms pace + up to 2s anti-metronome dwell) plus serial sitemap probes. v2 elastic pacing: 300ms base pace, skim-model dwell (≤300ms), sitemap candidates probed in one parallel wave on miss, reactive escalation ladder unchanged (throttle/latency signals still back off aggressively). 5-page docs crawl now ~3.5s wall including extraction.
- **Domain-profile poisoning from browser fetches**: cookie write-back in the actions path no longer marks never-walled domains as needs_tier2 (the v1.1 reddit-poisoning bug class, caught in live testing).
- Actions on PDF-shaped URLs (`.pdf` suffix or `/pdf/` path segment) are rejected up front with a clear message instead of burning a browser launch on Chrome's PDF-viewer JS shell.

### Changed

- Search enrichment now prefetches the top 5 results (was 3) — parallel with a 4s cap each, so real page titles/descriptions feed the final ordering at no wall-clock cost.
- Crawl sitemap child-index recursion is wave-parallel (bounds of 8) instead of serial.

## [1.2.0] - 2026-08-16

Security hardening — full audit by GLM 5.3 found 8 live-proven
vulnerabilities. All patched, PoC-verified against the release binary.

### Security

- **SSRF: DNS pinning** — hostnames resolving to private/loopback addresses are now blocked at the transport layer (post-resolution IP check, TOCTOU-safe). Previously only literal IPs were checked, so `127-0-0-1.nip.io` or any rebinding DNS reached loopback and cloud metadata endpoints. Escape hatch: `DONSETCH_ALLOW_PRIVATE_EGRESS=1`.
- **SSRF: redirect re-check** — every redirect hop is now checked with the SSRF guard before following. Previously the guard ran once on the initial URL; a public URL redirecting into a private network bypassed it.
- **SSRF: crawl guard** — `web_crawl` now checks the seed URL with the SSRF guard (same as `web_fetch`). Previously crawl had no guard at all.
- **Decompression bomb** — all decompression codecs (br/gzip/deflate/zstd) and identity bodies are now capped at 64 MiB. A 500 KB gzip body expanding to 512 MB previously caused unbounded memory growth; now returns a clean error.
- **h2 memory DoS** — three amplifiers fixed in the custom HTTP/2 stack: CONTINUATION flood capped at 256 KiB header blocks, frame size cap reduced from 16 MiB to 1 MiB, HPACK dynamic-table size updates rejected above 64 KiB (Chrome's advertised max). Response bodies capped at 64 MiB.
- **Cookie tossing** — `Domain=` attribute now validated per RFC 6265 §5.3.6: accepted only when it equals the request host or is a parent suffix. Previously any origin could pin cookies on any victim domain.
- **Expired cookie replay** — `header_for` and `snapshot_for` now filter expired cookies; `purge_expired()` runs after every store. Previously expired cookies were replayed indefinitely.
- **CRLF request splitting** — h2 header values with CR/LF/NUL are now rejected at decode time (RFC 9113 §8.2.2). The cookie jar rejects control characters at store time. Outgoing headers are validated in both `fetch_once_via` and `h1::get` before any wire write. Previously a crafted h2 `set-cookie` with embedded CRLF could inject arbitrary headers into later h1 requests.

### Fixed

- h1 response bodies now capped (content-length, chunked, read-to-close) — a lying Content-Length or an endless chunked stream previously caused unbounded allocation. Chunk-size arithmetic overflow also capped.
- `ghost-state.json` and BYOK key tmp files now created with 0600 permissions before content is written. Previously the tmp file was 0644 until the atomic rename, leaving harvested cookies and API keys world-readable on crash.
- IPv4-mapped IPv6 addresses (`::ffff:127.0.0.1`) now detected as their v4 self in the SSRF guard. Previously they bypassed all v6 rules.
- IPv6 literals in brackets (`[::1]`) now correctly parsed by the SSRF guard. Previously brackets prevented the IP parser from running.
- Cookie path-match now follows RFC 6265 §5.1.4: a `/foo` cookie no longer matches `/foobar`.

### Changed

- npm installer uses `execFileSync` instead of `execSync` (no shell, no string interpolation), caps redirects at 5 hops, and refuses http:// downgrade redirects.
- 404 tests (was 401).
- Added more bugs to fix later.

## [1.1.1] - 2026-08-15

Hybrid semantic focus filter + tool definition updates.

### Added

- Hybrid BM25 + cross-encoder semantic focus filter for `web_fetch`. The `focus` parameter now uses keyword matching (BM25) as the base, then if the cross-encoder model is already cached (from search reranking), runs a second pass and adds semantically relevant blocks that BM25 missed. Catches blocks where the query uses different vocabulary than the page (e.g. query "how gradients flow through layers" matches "backpropagation" and "chain rule"). No model download is triggered during fetch — only uses the model if already cached.
- `cross_encoder_scores` and `is_model_cached` exposed from the rerank module for reuse by the focus filter.

### Changed

- `focus` parameter description strengthened to drive agent adoption: explains the 50-80% token reduction, hybrid matching, concrete example, and ends with a directive to always set focus when you know what you're looking for.
- `web_fetch` tool description updated with a prominent "Token efficiency — use focus" section.
- `web_crawl` `focus` (topic) param and description updated similarly.
- 401 tests (was 395).

## [1.1.0] - 2026-08-15

Stability, storage, and cross-platform fixes.

### Added

- `donsetch version` update check: fetches releases.atom feed and shows whether up to date.
- `DONSEEK_NO_DISK_STATE` env var: disable disk persistence for self-improving fetch.
- `donsetch doctor` now shows per-component cache breakdown.

### Fixed

- Reddit URLs no longer escalate to ghost browser (old.reddit.com is SSR). Prevents ghost-state poisoning.
- Stale Xvfb socket detection: verifies actual connectivity instead of file existence.
- Windows freeze/thaw now suspends the entire Chrome process tree via Job Object enumeration.
- Atom feed version parsing uses `<id>` tag instead of `<title>` (release titles can contain extra text).
- Disk storage: only clearance cookies persisted (tracking cookies filtered out). Render cache capped at 20 entries / 200KB max. Chrome disk cache disabled. One-time migration on load.

### Changed

- Self-improving fetch marked as experimental in README.
- Dependencies: sha2 0.11, brotli 8, tokio-tungstenite 0.30, GitHub Actions v7.
- 395 tests.

## [1.0.0] - 2026-08-15

First stable release. Feature-complete MCP server + CLI for web fetch, search, and crawl.

### Added

- **CLI**: full command-line interface — `fetch`, `search`, `crawl` with same engine as MCP.
  - `--json` for machine-readable output, `-q` for quiet mode, `--tier` for manual escalation control.
  - `keys` subcommand: manage BYOK search provider keys (`add`, `remove`, `list`, `default`, `reset`).
  - `doctor`: 9-check health diagnostics with auto-fix.
  - `update`: self-update from GitHub Releases (no API rate limits).
  - `rollback`: revert to previous version.
  - `version`: version + build info.
  - `tools`: print tool schemas as JSON (same as MCP `tools/list`).

- **BYOK search providers**: external search providers (TinyFish, Tavily, Serper, Exa) bypass the local engine entirely. Key stacking, rotation, rate-limit cooldown (60s auto-recovery), credit-depletion detection, local fallback. Config: `~/.cache/donsetch/byok-keys.json`.

- **Query-entity coverage penalty**: anchor entities (hyphenated compounds like "B-tree") and specifiers (version numbers, years) checked against results. Wrong entity = 0.3× score penalty. Fixes BM25 splitting "B-tree" → "b" + "tree" where "binary tree" matches. Universal — no-op for queries without entities.

- **Crawl v2**: transient retry (max 2), canonical URL resolution, pagination (`<link rel="next">`), RSS/Atom feed discovery, `<base href>` resolution, binary content-type guard, referer + sec-fetch-site chaining, parent metadata, score-sorted output, sitemap `<priority>` + `<lastmod>`, ghost escalation (capped 3/crawl). Seed URL always in scope.

- **Xvfb socket-file polling**: replaced `xdpyinfo` dependency with `/tmp/.X11-unix/X99` socket polling for Xvfb readiness. Fixes ghost browser launch failure on systems without `xorg-xdpyinfo`.

- **npm package**: `npm install -g donsetch` downloads platform-correct binary from GitHub Releases at install time (SHA256-verified).

- **Release workflow**: tag-triggered, 3-platform build (Linux x86_64, macOS arm64, Windows x86_64), binary verification, packaging (tar.gz + SHA256), GitHub release.

### Changed

- README rewritten for v1.0.0: removed BETA warnings, added two-usage-modes section (MCP + CLI), updated test counts, cleaned stale info.
- Rust edition 2024 (let-chains support).
- Test count: 388 (was 249 at 0.5.0).

### Fixed

- TinyFish BYOK adapter: GET (not POST), root path `/` (not `/search`), query params (not JSON body). Old endpoint returned 404 (Next.js catch-all), misclassified as rate-limited.
- Crawl seed scope: `--include`/`--exclude` apply to discovered links only, not the seed entry point.
- Flaky PDF test under parallel execution: non-PDF body + PDF content-type instead of fake `%PDF-1.4` body (avoids PDFium race).
- Xvfb readiness check: `xdpyinfo` dependency removed, socket-file polling added.

## [0.5.0] - 2026-08-07

Initial public beta. Feature-complete MCP server for web fetch, search, and crawl.

### Added

- **Fetch** (`fetch`): two-tier stealth HTTP fetch with auto-escalation to headless browser.
  - Custom BoringSSL TLS stack (real Chrome ClientHello, `mlkem` post-quantum key exchange).
  - Own HTTP/1.1 + HTTP/2 transport (HPACK, flow control, connection pooling). No `reqwest`, no `hyper`.
  - Self-improving fetch loop: persistent domain intelligence, adaptive cookie lifetimes, warm-start after solve.
  - Bot wall detection: Cloudflare, DataDome, PerimeterX, Akamai, generic interstitials.
  - DonSift extraction engine: block model, BM25 focus, heading breadcrumbs, token-war policies.
  - `toc` / `section` / `focus` / `selector` / `offset` / `links` / `media` params.
  - PDF detection and parsing (PDFium FFI, OCR, tables, forms).
  - Non-HTML passthrough (JSON, XML, text).
  - Content classification: Article / Listing / Forum / Docs / Table / Page.

- **Search** (`search`): keyless multi-engine web search.
  - 10+ backends in parallel: Brave, Bing, DuckDuckGo, Mojeek + keyless verticals (GitHub, Wikipedia, HN, Scholar, arXiv, StackExchange, MDN, Google News).
  - Cross-engine consensus ranking (weighted RRF + BM25 + domain priors + diversity cap).
  - Semantic reranking: local ONNX cross-encoder (`ms-marco-MiniLM-L-6-v2`, 23MB, Apache-2.0). 60/40 blend with RRF+BM25+consensus. Graceful no-op if model unavailable.
  - Intent detection: auto / web / code / paper / news / entity. Routes to appropriate verticals.
  - Adaptive egress governor: fan-out width shrinks under stress, engine trust EWMA, chronic-failure quarantine (3 strikes, 10-min bench), single-flight deduplication.
  - Persistent disk cache with intent + recency-aware TTL.
  - Honest reporting: `weak` flag, per-engine status, never a fake "no results".

- **Crawl** (`crawl`): best-first same-domain crawl.
  - Three modes: `full` (sitemap map + content), `map` (URL inventory only), `content` (BFS from seed).
  - Focus-ranked frontier: BM25 relevance scoring, crawl only matching pages.
  - Adaptive pacing: Governor with per-(host, lane) backoff. Success → steady, 429/503 → exponential, error → cooldown.
  - Resume tokens: continue stopped crawls across calls. Disk-backed, 30-min TTL.
  - Near-dup detection: title + content hash signature.
  - Path scoping: `include_paths` / `exclude_paths`, `same_host`, `respect_robots`.
  - Honest stop reasons: FrontierEmpty, MaxPages, CharBudget, DepthLimit, Deadline, ThrottledOut.

- **PDF engine** (DonSheet): custom PDFium FFI, three-engine fusion.
  - PDFium text extraction + pixel-truth OCR (PP-OCR via ONNX Runtime) + form field extraction.
  - OCR arbitration cascade: English → Chinese → Devanagari.
  - Tables as markdown, multi-column reading order, orientation canonicalization, BiDi text.
  - Forms as data: AcroForm field names + values as structured table.
  - Honest flags: encrypted, scanned, vertical, corrupt.
  - 40-doc battle corpus tested, 120/120 fuzz clean.

- **MCP daemon**: stdio server, JSON-RPC 2.0, MCP protocol 2024-11-05+.
  - 3 tools, ~1.8K tokens at `tools/list`.
  - Dense, LLM-optimized tool definitions with full response format documentation.

- **CI**: 3-platform matrix (Linux, macOS, Windows), clippy (`-Dwarnings`), fmt check.
- **License**: AGPL v3.

### Known limitations

- Interactive captchas (hCaptcha, reCAPTCHA, Turnstile checkbox) are not solved — no solving service by design.
- ML-DSA post-quantum signatures not yet supported (BoringSSL 5.1.0 lacks them).
- `outerWidth/Height` in headless: protocol-level override only.
- Windows/macOS PDF subsystem compiled but CI verification pending.
