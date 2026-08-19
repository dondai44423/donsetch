# Changelog

All notable changes to DonSeTch are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.3.5] - 2026-08-19

### Fixed

- **Windows: orphaned Chrome processes after every fetch (#11)**: `AssignProcessToJobObject` requires both `PROCESS_SET_QUOTA` **and** `PROCESS_TERMINATE` on the process handle, but only the former was requested. The call failed with `ERROR_ACCESS_DENIED`, leaving the Job Object empty — so `KILL_ON_JOB_CLOSE` had nothing to kill when the handle dropped, and the whole browser tree outlived donsetch. Because the orphans inherit donsetch's stdout, any pipeline calling donsetch would also block until they were killed by hand, which looked like donsetch itself hanging.

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
