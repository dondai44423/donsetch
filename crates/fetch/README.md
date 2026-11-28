# `donsetch-fetch` — the fetch layer

Workspace crate (`crates/fetch/`) owning wire identity, per-origin state, strict page caching, native challenge contracts, public site interfaces, embedded V8 (challenge minting + data-plane content execution), and mined HTTP request replay. No browser, CSS cascade, layout, or rasteriser enters this path.

## CLI & verification

Build and test from the repository root so artifacts share `./target` and `Cargo.lock`:

```sh
cargo build -p donsetch-fetch --release

# Single fetch (optional --revalidate, --no-cache, --render, --identity <name>)
./target/release/donsetch-fetch fetch https://example.com/
V5_ENGINE_LOG=1 ./target/release/donsetch-fetch fetch https://www.imdb.com/ --identity chrome-local

# Identity × target probe matrix & cold benchmark paired with curl
./target/release/donsetch-fetch probe --targets targets.json --out matrix.json
./target/release/donsetch-fetch bench --targets targets.json --runs 2 --curl --out bench.json

# Portfolio inspection, wire scorecard, learned state & page cache
./target/release/donsetch-fetch identities [--headers]
./target/release/donsetch-fetch scorecard [--record] [--json]
./target/release/donsetch-fetch state [show|reset]
./target/release/donsetch-fetch cache clear

# Offline unit & contract suite (CI runs this on Linux x86_64)
sh scripts/budget.sh cargo nextest run --cargo-profile fast --locked -p donsetch-fetch --offline
```

## Module map

| File | Owns |
|---|---|
| `src/lib.rs`, `src/main.rs` | `Session::fetch(&Request) -> Outcome` (`Send + Sync`, blocking; async callers use `spawn_blocking`) and CLI subcommands |
| `src/identity.rs` | Eight coherent client profiles + built-in `personas/chrome-local.json` |
| `src/http.rs`, `src/ca_blocklist.rs` | Wire transport (`wreq`/BoringSSL), header order, cookies, compromised-CA blocklist |
| `src/classify.rs`, `src/contract.rs` | Vendor-first classification into nine outcome classes; content predicates |
| `src/hydrate.rs`, `src/synthesize.rs` | Structural hydration-deficit shell gate, zero-JS inline state materializer, and wire-transcript DOM fusion |
| `src/state.rs` | Origin scores, vendor priors, sticky routes, pacing clocks, `(host, identity)` cookie jars, credentials, mined contracts (`<= 4,096` origins LRU, file-locked atomic merge) |
| `src/splash.rs` | Market-splash contracts: the query pair a store's own chooser links carry, so the first request is the one its "stay on this market" link makes |
| `src/cache.rs` | Strict RFC 9111 private cache (`<= 8 MiB`/entry, `256 MiB` LRU, validator audits) |
| `src/fetch.rs` | `Worker` encounter state machine (steps 0–9 below) |
| `src/challenge.rs` | Native non-JS challenges: Akamai `bm-verify`, SiteGround SHA-1 PoW, NCBI SHA-256 PoW |
| `src/datapath.rs` | Seven public site interface contracts (Reddit, Stack Overflow, 4chan, Twitch, NYT, WSJ, crates.io) |
| `src/mine.rs` | Request-contract mining (`{{n}}` URL/affix templates) and structural HTTP replay (`Collection`, `EntityJson`, `HtmlFragment`) |
| `src/engine.rs`, `src/engine_host.js` | Embedded V8 isolate, watchdog, native ES module loader, multiplexed async subresource I/O, challenge host, and data-plane content DOM |
| `src/webcrypto.rs`, `src/envelope.rs` | BoringSSL WebCrypto ops; hard wall-clock, heap, Wasm, RSS, op-count, and network guards |
| `src/surface_probe.html`, `src/surface_reference.json` | Real Chromium 157 headed surface capture enforced by `surface_tests.rs` |
| `src/testkit.rs`, `src/*_tests.rs` | Offline hostile-origin server and unit/contract suites |

## Core mechanisms

### 1 · Coherent identities & pacing

Eight measured client profiles carry matching TLS (JA4/peetprint), HTTP/2 frames, header sets/order, and client hints: `plain`, `chrome` (156/Linux), `chrome-local` (captured 156/Linux + script surfaces), `chrome-win` (156/Windows), `chrome-android` (149/Android — kept at 149 because `wreq-util` lacks Chromium 156+ TLS extension 4832), `firefox` (151), `safari` (26.4/macOS), and `okhttp` (5/Android). An unseen origin is met as the client this machine itself is (`chrome-local` on Linux, `chrome-win` on Windows, `safari` on macOS) — the first request is the one a bot wall judges, and a client that names itself `donsetch/5.0` is offered only to an origin that has earned it. Measured on forty vendor-heavy sites (fresh state, `dist-thin`, 2026-11-27): first contact as the local browser plus keeping the cookies a response hands back moved content from 25/40 to 30/40 with p50 1893 → 1865 ms.

- **Host-coherent one-shot reads:** product reads default to the host OS class (`chrome-local` Linux, `chrome-win` Windows, `safari` macOS) so UA and client hints never contradict.
- **Per-origin selection:** sticky winner → origin score → sibling-host score → vendor prior (`state::VENDOR_SEED`) → global prior → untried → prior failures. Named vendor walls promote known passers for that vendor.
- **Pacing:** 900 ms minimum per-origin interval, 250 ms between intra-encounter attempts, `Retry-After` above 1 h ignored, and `>= 1.2 s` remaining deadline required before starting a retry.

### 2 · State & credential lifecycle

- **Isolation:** cookies, clearances, and scores bind strictly to `(host, identity)` and never cross hosts or fingerprints. Scores halve every 32 attempts; HTTP 429 preserves a sticky route while other failures drop it.
- **Persistence:** `<cache>/donsetch/v5/state.json` (max 4,096 origins LRU), merged under a lock file via temp-file + rename, skipping disk reload when `(len, mtime, inode)` matches our last write.
- **Minted credentials:** renew proactively at `0.9 × learned TTL` (`accepted_age` / `refused_age`), re-mint at most once per encounter on `202`/`401`/`403`, cap at 3 mints per 600 s per host, and back off 600 s → 6 h on failed mints.

### 3 · Strict page cache (`cache.rs`)

Stores verified HTTP 200 content (`<= 8 MiB`) under `pages/` (`256 MiB` LRU) keyed by `(identity, egress, URL)`. Repeat reads send Chrome-ordered `If-None-Match`/`If-Modified-Since` (no `Cache-Control`).

- **Earned freshness:** `max-age`/`Expires` (capped at 60 s) serves without a request only after a prior read inside the window validated unchanged, alternating fresh serve and network check.
- **Validator audits:** forces full re-fetch after 2 (`unproven`) or 4 (`proven`, or 1 h) 304s; origins that mutate body text under unchanged validators or inside `max-age` lose trust for 7 days. Honors `no-store`, `Vary: *`, and `Vary: Cookie`. Reports `fresh`, `revalidated`, or `changed`.

### 4 · Public site interfaces (`datapath.rs`)

Requests use the app's own fetch metadata (`sec-fetch-dest: empty`, `sec-fetch-mode: cors`, `accept: application/json`, page referer, same identity) and require item equivalence with the rendered page:

| Origin | Interface | Yield & rate contract |
|---|---|---|
| Reddit | `<path>/.rss` Atom feed | Posts, permalinks, timestamps; obeys `x-ratelimit-reset` |
| Stack Overflow | `api.stackexchange.com/2.3/questions` | Questions, answers, scores, tags (`not_found` → `datapath::absent`) |
| 4chan | `a.4cdn.org/boards.json`, `/<board>/1.json` | Board index and thread items |
| Twitch | `gql.twitch.tv/gql` (public client ID) | Live streams, channels, viewer counts |
| NYT / WSJ | Section & world-news RSS feeds | Front-page / world-news lead items |
| crates.io | `/api/v1/summary` | Summary lists, paced to 1 req/s |

### 5 · Native challenges (`challenge.rs`)

Answered without V8 inside the encounter deadline:
- **Akamai Bot Manager (no-JS):** honors `ak_bmsc` + meta-refresh `?bm-verify=<token>` after the declared wait (capped at 6 s).
- **SiteGround `sg-captcha`:** solves SHA-1 leading-zero PoW (`<= 24` bits) over `<bits>:<nonce>` and submits `sgsubmit_url`.
- **NCBI `/_pow/solve`:** solves HTTP 203 SHA-256 PoW (`<= 22` bits) and POSTs JSON solution.

### 6 · Embedded V8: challenge mint & content lane (`engine.rs`, `engine_host.js`)

One V8 isolate mode for challenge credentials, a second data-plane DOM mode for JS shell applications (`transport: "program"`). A single pooled HTTP/2 prefetch runtime fetches script/modulepreload assets concurrently during V8 bootstrap (`op_prefetch_wait`) reusing the parsed DOM and the same identity wire; static pages with no `<script>` tag skip V8 completely, and stalled shells with zero text growth or `2xx` data progress exit early instead of burning the full ceiling.

- **Challenge mints:** passes all three AWS WAF shapes (`goku`, legacy inline, interstitial) and Reddit's inline token-form GET gate. Cloudflare managed challenges mint `__cf_bm` on the final allowed attempt and stop honestly at Turnstile/iframe boundaries (answered via `okhttp`/`chrome-win` identity instead; no `cf_clearance` claimed). Akamai Bot Manager's behavioral challenge (`sec-if-cpt`) mints the sensor cookie set (`_abck`, `bm_sz`, `bm_s`, `bm_so` — measured 1.5 s) when the challenge page is what was served; DataDome's interstitial (`var dd=…` + `ct.captcha-delivery.com/i.js`) is run as the class that received it. Never invent vendor globals on `window` (e.g. `awsWafCookieDomainList`).
- **Measured host surfaces:** BoringSSL WebCrypto (`SHA`, `HMAC`, `AES-GCM/CBC/CTR/KW`, `PBKDF2`, `HKDF`), `Blob`, `FileReader`, multipart `FormData`, Worker/Blob URLs, RFC 6265 `document.cookie`, WHATWG `URL`/`URLSearchParams`, Chrome-resolution clock, `document.forms` GET submission, html5ever `innerHTML`/`outerHTML`/`<template>`, host selector matching, `DOMParser`/`NodeIterator`/`TreeWalker`/`customElements`/`CSSStyleSheet`/`MessageChannel`, SVG geometry stubs, batched observers (reporting ratio `1` so lazy loaders fire), persona `matchMedia`/screen/window geometry, inline `px` boxes with `0` positions (`elementFromPoint` = `null`), and native V8 ES modules (`import`, dynamic `import()`, `import.meta.resolve`, import maps, `<base href>`, speculative child pre-warming). Unimplemented methods log on touch and are never guessed; `surface_tests` enforces parity against `surface_reference.json`.
- **Hydration gate, Stage-0 inline state & Dual-Plane fusion (`hydrate.rs`, `synthesize.rs`):** `assess_html_shell` separates `<main>`/`<article>` content from frame chrome (`<nav>`, `<header>`, `<footer>`, `<aside>`) and detects unhydrated framework payloads (such as null Next.js `pageProps`) with zero false triggers when no application script exists. Before booting V8, `materialize_inline_state` extracts structured collections, normalized Apollo/Relay caches, or rich entities embedded directly in HTML (`window.__appData`, `data-page`, `data-props`, `<script type="application/json">`): rows wrapped in one object key (`{"node": …}`, `{"attributes": …}`) are unwrapped, Relay's `require…__bbox.result.data.<entity>.<connection>.edges` depth is walked, a caption object is the row's text, and a collection of bare labels is refused as navigation. After V8 runs, `fuse_dom_and_transcript` combines graphical/collapsed DOMs with verified `2xx` JSON, OpenAPI, dependency-graph, and HTML-fragment wire responses; a payload is fused only when its rows carry a sentence, a description or a link, and on a query URL only a *collection* is eligible — preferring the request whose own URL carries the query.
- **Mined request contracts (`mine.rs`):** when a content-lane run passes the contract, `mine.rs` learns `Collection`, `EntityJson`, or `HtmlFragment` endpoints and stores a `{{n}}`-templated request contract (including middle-segment and filename-affix placeholders) per `(host, identity, URL shape)` (`<= 8`/origin). Subsequent encounters replay it as plain HTTP before V8 (`transport: "replay"`), checking structure + `min_items`, and drop/re-mine on HTTP error or schema drift.

### 7 · Engine envelope (`envelope.rs`)

| Resource | Mint limit | Content-lane limit | Enforcement |
|---|---|---|---|
| Wall clock | 10 s (`V5_ENGINE_WALL_MS`), clamped to the encounter deadline with a 5 s measured-mint floor | 5 s default (`V5_CONTENT_WALL_MS`), raised to an 8 s ceiling when the encounter has the time; clamped to deadline | Watchdog `terminate_execution` |
| Memory | 256 MB JS heap · 128 MB typed arrays · 128 MB Wasm | Same | V8 heap callback, custom allocator, Wasm page cap + OS RSS backstop |
| Execution | 96 ops | 256 ops (`V5_CONTENT_OPS`) | Rust op counter + wall-clock pump bound |
| Network | `http(s)` public IPs, `<= 5` redirects, 1 MB req, 8 MB/resp, 32 MB total | Same guards, 16 MB/resp, 64 MB total | Enforced in Rust ops; `Deno` global deleted before page code |
| Concurrency | 1 active V8 engine per process | Same | Process-wide permit |

## Encounter lifecycle & outcome classes

0. **Cache:** serve earned fresh copy or conditional `304`.
1. **Pace:** honor per-origin interval.
2. **Spend:** send request with best identity and its isolated cookie jar; a host with a market-splash contract (`splash.rs`) is asked with its own "stay on this market" pair on the first request, so its geolocated chooser is never what comes back.
3. **Classify:** inspect vendor headers (`x-amzn-waf-action`, `x-datadome`, `cf-mitigated`, `x-px`) before body; check content contract (`>= 200` visible chars generic floor **and** `>= 200` content-bearing chars — body text outside `nav`/`header`/`footer`/`aside`, with `alt`/`aria-label` labels counted, because a navigation frame passes any total-text floor), rejecting `<= 1,500`-char human-check/refusal stubs, gate pages and outage notices. A human widget counts only where it is mounted, and a vendor sensor that loads on ordinary pages (Kasada's `kpsdk`, Cloudflare's passive detections) is not a wall on a 2xx application page.
4. **Route by verdict:** on `content`, persist win + cookies; on named wall, reorder remaining identities (2 human walls or 2 identical unlabelled bodies end identity search). A 404 that carries a search query is retried once at the origin's own search route (`/search?…`).
5. **Native challenge:** execute `challenge.rs` contract if matched, then re-classify.
6. **Public interface:** query `datapath.rs` if defined for the URL (`transport: "data"`).
7. **Mined replay:** execute stored `mine.rs` HTTP template before JS (`transport: "replay"`).
8. **V8 program:** run the site's own program — challenge mint (AWS WAF upgrades `plain` → `chrome-local`; Cloudflare on the last allowed attempt; Akamai `sec-if-cpt`, DataDome interstitial and Reddit's token form as the class that received them) or the content lane once (`shell` on the receiving identity; `incomplete` after the last identity). A shell served to a plain client is retried as the browser class first (a shell is not the same shell to every client), and the lane's ceiling is 12 s when the caller's deadline allows it, 8 s otherwise. Cookies a response hands back are kept whether or not the session persists, and a wall that changed the cookie header is answered once more by the same identity with what it was given, before any other identity and before any program.
9. **Remember:** merge scores, sticky route, cookies, credentials, and mined contracts to disk.

| Outcome | Wall class | Action / ownership |
|---|---|---|
| `content` | Held state | Contract passed — done |
| `wall_state` | State renewal | Re-mint stale clearance; **login gates are declared, never signed into** |
| `wall_contract` | Request contract | Answered natively in `challenge.rs` |
| `wall_identity` | Identity / egress | Rotate coherent client or egress (includes HTTP 200 "Access Denied" stubs) |
| `wall_interaction` | Human interaction | Try vendor's known-passing identities once; otherwise **declare** (no captcha solvers) |
| `wall_program` | Page program | Run V8 challenge mint, then interface |
| `shell` | Unanswered app shell | Run V8 content lane once as receiving identity |
| `incomplete` | Contract miss (2xx) | Run V8 content lane once after final identity |
| `network` / `http_error` | Transport / origin | Pre-client errors stop immediately; post-client errors try at most one alternate wire class |

## Target notes, measured negatives & environment

- **Current target standing:** AWS WAF (IMDb, Booking, Trustpilot), Akamai no-JS (Amazon), SiteGround, NCBI PubMed, Reddit, AliExpress/Instagram (`chrome*`), Cloudflare via `okhttp`/`chrome-win` (Product Hunt, Glassdoor, Indeed, npm, Medium), and `HTTP 200` client-rendered SPAs (React/Next, Angular, Vue/Nuxt, Svelte, Vite, Mastodon, Ashby, Swagger, Registry/Docs SPAs) deliver content. Measured 2026-11-27 (cold, fresh state, `dist-thin`): Instagram's profile reads from the app's own Relay payload in 2.3 s with no V8; Nike's launch calendar and Best Buy's search grid (via its own `intl=nosplash` market pair) deliver content; Pinterest renders its shell but the extracted text is still navigation; Target's product API answers `435` to the lane and its own apology page is declared, not served; Sephora's stale `/shop` route is repaired to `/search` and then stops at Akamai's behavioral challenge (whose `_abck` the engine mints — the retry still meets the sensor). DataDome captchas (Etsy, Yelp, WSJ), active `HTTP 403` Cloudflare challenge / Turnstile / reCAPTCHA gates (OpenReview, Federal Register), PerimeterX press-and-hold (intermittent on Bloomberg), and TikTok's edge-gated shell API remain open/declared identity or interaction walls. Home Depot refuses this egress for a real Chrome as well (Akamai IP reputation): an egress wall, not a client one.
- **Do not rebuild:** static bundle endpoint scraping (defended stubs carry zero endpoints; modern SPA bundles assemble URLs at runtime — use V8 content-lane mining instead) and default origin warm-up/`--arrival` (0 class wins across 16 targets for +1 request).
- **TLS, bodies & deps:** Mozilla roots + OS trust store + `SSL_CERT_FILE`/`SSL_CERT_DIR` + `[tls]` config (never altering ClientHello); streaming reads capped at 64 MiB preserving raw non-UTF-8 bytes; crates restricted to `wreq`/`wreq-util`, `btls`, V8, `tokio`, `futures-util`, `scraper`, `serde`, `url`, `getrandom`, `webpki-root-certs`, `rustls-native-certs`. For LLD linker notes, see [setup.md](../../docs/setup.md#build-from-source).

| Variable | Purpose |
|---|---|
| `V5_STATE` | State JSON path (default `<os-cache>/donsetch/v5/state.json`; empty disables disk persistence) |
| `V5_PERSONA` | Override built-in `personas/chrome-local.json` |
| `V5_ENGINE_LOG` / `V5_ENGINE_DUMP_DIR` | Print touched V8 surfaces/cookies (`1`) or dump engine HTTP transcripts for inspection |
| `V5_ENGINE_WALL_MS` / `V5_CONTENT_WALL_MS` / `V5_CONTENT_OPS` | Override mint timeout (`10000`), content-lane timeout (`5000`), or content op cap (`256`) |
| `RUSTY_V8_ARCHIVE` / `SSL_CERT_FILE` / `SSL_CERT_DIR` | Offline prebuilt V8 static archive; extra PEM trust roots |
