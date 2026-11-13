# fetch layer

One Rust binary that fetches the web: carried client identity, carried state, a page cache, the
site's own interface in front of its walled document, and an in-process V8 engine that runs a
challenge page's own scripts when a wall demands it. No browser, no layout, no rasteriser.

The crate lives in `crates/fetch/`, a workspace member: build and test it from the repo root
with `-p donsetch-fetch` (`cargo test -p donsetch-fetch`), so it shares the lockfile and target
cache with the product. `hard-corpus.json`, `fixtures/` and `runs/` are private working
material (git-ignored); this README is tracked.

```sh
# build (vendored wreq/btls; V8 arrives as a prebuilt static library via deno_core)
# from the repo root; the artifacts land in the shared ./target
cargo build -p donsetch-fetch --release

# one fetch: portfolio selection, pacing, state spent and earned
V5_PERSONA=~/Projects/donsetch-evidence/research/persona/chrome-local.json \
  ./target/release/donsetch-fetch fetch https://example.com/

# identity matrix: every portfolio identity against every target, one request each
./target/release/donsetch-fetch probe \
  --targets ~/Projects/donsetch-evidence/research/corpus.json --out matrix.json

# interface equivalence: the ids the page links to must be the ids we return
python3 runs/verify_datapaths.py

# cold measurement paired with curl on the same window
./target/release/donsetch-fetch bench \
  --targets ~/Projects/donsetch-evidence/research/corpus.json --runs 3 --curl --out bench.json

# the same corpus through one egress lane: the egress class is the variable
source ~/.config/secrets.env
./target/release/donsetch-fetch bench --targets hard-corpus.json --runs 2 --curl \
  --proxy "http://$V5_PROXY_USER:$V5_PROXY_PASS@198.23.243.226:6361" --out runs/a6-dc.json

# steady state: discovery allowed, state carried between rounds (--no-cache measures learning
# alone; without it the page cache answers too)
V5_STATE=/tmp/donsetch-fetch/state.json ./target/release/donsetch-fetch bench \
  --targets ~/Projects/donsetch-evidence/research/corpus.json --runs 3 --persist --discover --curl

# a repeat fetch is answered by the page cache: fresh, revalidated (304) or changed
./target/release/donsetch-fetch fetch https://pypi.org/ [--revalidate] [--no-cache]

# a recognised challenge (AWS WAF's goku shape) mints in-process on its own;
# V5_ENGINE_LOG=1 prints the host surfaces and cookies the mint needed
V5_ENGINE_LOG=1 ./target/release/donsetch-fetch fetch https://www.imdb.com/ --identity chrome-local

./target/release/donsetch-fetch identities [--headers]   # the portfolio and what each sends

# identity scorecard: every browser identity's live wire (JA4, peetprint, Akamai H2, UA)
# against the committed baseline; --record rewrites it deliberately
./target/release/donsetch-fetch scorecard [--record] [--json]
./target/release/donsetch-fetch state [show|reset]       # scores, priors, jars (reset clears pages)
./target/release/donsetch-fetch cache clear              # forget cached pages, keep what was learned
```

## What is in here

| File | Role |
|---|---|
| `src/lib.rs` | The library: `Session::fetch(&Request) -> Outcome`, `Send + Sync`, blocking (async callers use `spawn_blocking`); pooled workers fold into one learned state |
| `src/identity.rs` | The portfolio: coherent clients, one profile each; the built-in local identity (`personas/chrome-local.json`) |
| `src/http.rs` | One request on the wire, with the identity's headers and cookies |
| `src/classify.rs` | Nine outcome classes, vendor-and-ask driven |
| `src/contract.rs` | Content contracts (the frozen corpus's own predicates) |
| `src/state.rs` | Scores, vendor priors, routes, cache trust, pacing clocks, cookie jars with bindings; bounded, merged into the file under a lock; the per-OS cache path |
| `src/cache.rs` | The page cache: what may be stored, served fresh, revalidated or audited |
| `src/fetch.rs` | The encounter (`Worker`): pace, spend, classify, follow the vendor, run the engine, ask the interface, remember |
| `src/challenge.rs` | Native challenge contracts: Akamai's `bm-verify` hand-off, SiteGround's and NCBI's proof of work; hands page programs to the engine |
| `src/datapath.rs` | Interface contracts: which host, which request, which items count |
| `src/engine.rs` | The challenge engine: V8 in-process, one HTTP op on the fetcher's identity, the mint, the watchdog |
| `src/engine_host.js` | The bounded host layer: small DOM (including a page's own forms and a JS challenge's form navigation), script loader, browser surfaces, WebAssembly budget, no rendering |
| `src/webcrypto.rs` | WebCrypto on BoringSSL: digests, HMAC, AES, PBKDF2, HKDF; anything else is refused |
| `src/envelope.rs` | The engine's limits: wall clock, heap, typed-array and WebAssembly memory, op count, URL/redirect/size guard, one engine at a time, resident-memory backstop on Linux, Windows and macOS |
| `src/testkit.rs` | A local HTTP server for offline tests, including hostile replies (endless, trickled, silent, redirecting) |
| `src/engine_tests.rs`, `src/lifecycle_tests.rs` | Offline tests: hostile pages against the envelope and host fidelity; a fake AWS WAF origin with a controllable clock against the token lifecycle |
| `src/fixture_tests.rs`, `fixtures/aws-waf/` | The three AWS WAF shapes (goku, legacy inline, interstitial) run from captured bundles against a local verifier |
| `src/session_tests.rs` | The library seam: threads, `spawn_blocking`, proxy, body capture, vendor escalation, route memory, state location |
| `src/cache_tests.rs`, `src/redirect_tests.rs`, `src/revalidation_tests.rs` | The page cache against a local site that changes, lies, walls and redirects under it |
| `src/main.rs` | CLI: `fetch`, `probe`, `bench`, `identities`, `scorecard`, `state`, `cache` |

## The mechanism

**A wall is a classification problem before it is anything else.** A site decides before content
is served on *who is asking* (TLS/HTTP shape, header order, client hints, egress class, pacing)
and *what they already have* (cookies, clearance, session). Both are carried, never emulated. A
fetch costs one round trip and one jar read. Five things are earned out of band and amortised:
the identity profile, session state, request contracts, the egress class, and challenge tokens —
cost per fetch is curl's cost (47–68 ms on a trivial origin, curl 46 ms).

### Identity

Eight **coherent** clients, each carrying one real profile's TLS fingerprint, HTTP/2 settings,
header set and order, and client hints: `plain`, `chrome` (156/Linux), `chrome-local` (the
reference browser's own 156 identity, with its script-visible facts), `chrome-win`,
`chrome-android` (a measured Chrome 149 Android client), `firefox` (151), `safari` (26.4/macOS),
`okhttp` (5/Android app). Nothing is patched per field; a profile is chosen, not assembled, and
each is diffed against the reference browser it claims (JA4, peetprint, Akamai H2, header set
and order). A ninth identity labelled `curl` was removed: it was the native stack with a curl
user-agent, a claim no detector would believe.

All Chrome identities share JA4 `t13d1517h2_8daaf6152771_cb7bf5808d99` and 17 headers in the
same order; JA3 varies per connection as Chrome's does. Chromium 156/157 send TLS extension
4832, which no `wreq-util` profile sends — hence `chrome-android` is declared as 149 rather
than relabelled, and replacing it is a reference-capture task, not a code change.

One-shot product reads present the host's own class — `chrome-local` on Linux, `chrome-win` on
Windows, `safari` on macOS (`src/fetch/core.rs`): a Linux UA leaving a Windows box contradicts
the client hints and is the incoherence this rule forbids.

**Selection is per origin, measured:** stored winner → this origin's scored identities → for a
host with no scores, a sibling host on its site → the vendor prior over `state::VENDOR_SEED` →
the global success prior → untried identities → identities that only failed here. A wall that
names its vendor moves that vendor's known passers to the front of what is left. Sticky: a
proven identity keeps the origin until it fails.

**Pacing is identity:** 900 ms minimum interval per origin, 250 ms between attempts inside one;
every wait is cancel-aware and fits the deadline; a `Retry-After` past one hour is ignored. The
first request always goes out, a later one only with 1.2 s left. Velocity is part of identity —
three fast requests to one origin is what escalated a challenge in testing, not the identity.

### State

Cookie jar per (host, identity) with expiry; scores per (origin, identity) that halve after 32
attempts so a changed site is believed; vendor priors; the route that answered (a 429 keeps it,
any other failure drops it); cache trust per origin. Cookies are never shared across hosts or
identities, and clearance is never banked across fingerprints.

One compact JSON file, at most 4,096 origins (LRU out with their cookies and credentials),
written once per fetch by merging into what is on disk under a lock file; the reload is skipped
when the file's stamp (length, mtime, inode) is the one this process wrote; write is temp file
plus rename, retried while Windows holds the target. `DONSETCH_CACHE_DIR` / `[paths] cache_dir`
move it.

**Credential lifecycle:** a minted cookie set is a `Credential` bound to (host, identity) with
its declared lifetime and a learned TTL — the age at which the origin accepted it
(`accepted_age`) and refused one it had accepted (`refused_age`). Renewed at 0.9× the learned
TTL before the next spend, and once more when the origin refuses it (202/401/403) — one re-mint
per encounter, then a clean re-ask. A host may mint 3 times per 600 s; a failed mint backs the
host off 600 s, doubling to 6 h.

State acquisition with a browser is removed (`warm`, `acquire`): no browser earns state or
content for this path, and a fetch spends only its own responses or a credential the engine
minted from the site's own challenge.

### The page cache

`cache.rs`, RFC 9111 for a private client, made strict. Only verified content (a 200, at most
8 MiB) is stored, keyed by identity, egress and URL, in `pages/` beside the state (256 MiB, LRU
eviction). A repeat read sends Chrome's conditional request (`If-None-Match`/`If-Modified-Since`
after `Accept-Language`, no `Cache-Control`) and the server's 304 is the proof.

A declared lifetime (`max-age`, `Expires`, capped at 60 s) is served without asking only after
the last read inside one was checked unchanged, alternating check and serve; a page that changes
inside its lifetime makes its origin ask on every read for 7 days. Validators are audited: an
unproven origin's copy is fetched whole after 2 revalidations, a proven one's after 4 or an hour;
unchanged validators over changed text distrust the origin for 7 days. No heuristic freshness,
nothing stored under `no-store` or `Vary: *`, `Vary: Cookie` honoured, a fresh copy served only
to the cookie it was confirmed under. Outcomes say `fresh`, `revalidated` or `changed`.

### The data path — ask the interface the app asks

A wall protects a document; the site's own client calls an interface. Those are open, fast, and
already tolerate the client's own request shape.

| Origin | Interface | Returns | Measured |
|---|---|---|---|
| Reddit | the page's own Atom feed (`<path>/.rss`) | posts with permalinks and timestamps | 25 items, 1.1–1.7 s, `x-ratelimit-reset` obeyed |
| Stack Overflow | `api.stackexchange.com/2.3/questions` | questions with score, answers, tags, links | 30 items, 0.43 s |
| 4chan | `a.4cdn.org/boards.json`, `/<board>/1.json` | board index, threads | 77 boards, 0.60 s |
| Twitch | `gql.twitch.tv/gql`, public web client id | live streams with viewers and channels | 20 streams, 0.71 s |
| NYT | `rss.nytimes.com/services/xml/rss/nyt/<section>.xml` | the stories the front page leads with | 14 items, 0.70 s |
| WSJ | `feeds.a.dj.com/rss/RSSWorldNews.xml` | world-news items | 20 items, 1.3 s |
| crates.io | `/api/v1/summary` | totals plus new, most-downloaded and just-updated crates | paced to its published 1 req/s |

The request is the app's own: `sec-fetch-dest: empty`, `sec-fetch-mode: cors`,
`accept: application/json`, the page as referer, the same identity that would fetch the
document. Verification is equivalence with the rendered page, never a rewritten predicate
(`runs/datapath-equivalence.txt`: Stack Overflow 4/5 question ids, 4chan 5/5 boards, Twitch 2/5
live channels, Reddit item ↔ its permalink's page).

Seven interface contracts live in `datapath.rs`; each declares hosts, the request it makes, a
parser that yields real items, a minimum item count and an honest URL shape. A path marked
`preferred` is asked *before* the document because its answer is the whole of what the document
renders partially. Published limits are state: a 429 records `x-ratelimit-reset` as a cooldown,
so the next fetch returns in ~150 ms naming the pace owed. An interface's own answer is taken at
its word — Stack Exchange's `error_name: not_found` is a missing item, not a wall
(`datapath::absent`).

### Native challenge contracts

Some walls ask for arithmetic, not a browser. `challenge.rs` answers them as the challenged
identity, inside the deadline, and hands anything needing a page program to the engine.

- **Akamai Bot Manager**, no-JS path: the response carries `ak_bmsc` and a meta refresh to
  `?bm-verify=<token>`; following it after the declared wait (bounded at 6 s), keeping cookies,
  is what a browser with scripting disabled does. Measured: Amazon content in both rounds
  (≈1.5 s first contact, then cookie-carried).
- **SiteGround `sg-captcha`**: a meta-refresh stub leads to `/.well-known/sgcaptcha/` carrying
  `sgchallenge` (`<bits>:<nonce>`) and `sgsubmit_url`; find a counter whose SHA-1 over
  `challenge ‖ counter` starts with `bits` zero bits, navigate to
  `sgsubmit_url&sol=<base64>&s=<ms>:<hashes>` once per encounter, refusing above 24 bits.
  Witness: an istacon.org article, fresh state, 202 then 200 (74 KB) after 116,858 hashes in
  2.9 s.
- **NCBI `/_pow/solve`** (PubMed serves it with HTTP 203): SHA-256 over
  `<challengeId>:<nonce>` with `bits` zero bits, posted as JSON to `/_pow/solve`; 12 bits asked,
  at most 22 answered.

### The engine — the site's own program, in-process

`engine.rs` embeds V8, runs the challenge page the fetcher already fetched, fetches the scripts
that page names with the same identity, executes them in document order with the page's own
inline bootstrap, and returns the credential the site itself issued. No browser, no imported
cookie, no DOM beyond the challenge's needs, no layout, no raster.

- **AWS WAF, all three shapes** (measured cold, no state): IMDb (goku) content at 3.6–4.6 s,
  Booking (legacy inline) at 2.7–3.0 s, Trustpilot (interstitial) at 1.8–2.3 s, on `chrome`,
  `chrome-local` and `chrome-win`. The 690 KB bundle compiles in ~9 ms; the mint is
  network-bound.
- **Reddit's gate** — the page's own form, run and spent: the page's inline program computes one
  field (the token it wrote, doubled) and submits a GET form; the engine parses the form's
  fields, runs the program, and hands the fetcher the navigation it chose. Content measured on
  front page, subreddit and thread (0.8–1.4 s cold, ~500 KB). A thread without its slug answers
  with a `location.replace` shell that is followed once; a 429 is paced, never minted on.
- **Cloudflare's managed challenge is run and stops at the host boundary: declared.** The engine
  mints `__cf_bm` and executes the 616 KB orchestrator; the program then needs
  `performance.getEntries()` of its own script load, a live DOM tree and an iframe realm for
  Turnstile — the DOM growth that is drift, for a vendor `okhttp` already answers. No
  `cf_clearance` is claimed; the engine gets a Cloudflare page only on the encounter's last
  allowed attempt, so identity is spent first.
- **A credential the server issued but the origin refuses is a failed mint.** The server's
  verdict rides inside the token, so only the spend tells; the engine reports the wall and never
  claims success unverified.
- **The host never invents page configuration.** Trustpilot: a host-defined
  `window.awsWafCookieDomainList` made the SDK report `domain: "trustpilot.com"` instead of the
  page host, and every token was refused whatever the fingerprint. Found by relaying Chrome's
  SDK traffic and swapping one field at a time.

**The host surface grows only for a measured challenge**, and the engine logs every surface it
needs. Measured so far: `Blob`, `FileReader`, `FormData` with a multipart boundary, WebCrypto
(real, on the BoringSSL the TLS stack links — SHA, HMAC, AES-GCM/CBC/CTR/KW, PBKDF2, HKDF;
anything else is `NotSupportedError`), Worker and Blob URLs, classic-script declaration
hoisting, cookie `Domain` semantics (RFC 6265), a WHATWG `URL` with live `searchParams`,
render-blocking stylesheets requested before the scripts after them, `crypto.getRandomValues`,
a Chrome-resolution clock, an identity view (`navigator`, client hints, WebGL strings, screen)
taken from the identity's own headers, and — for Reddit's gate — `document.forms` with declared
fields, live `elements.namedItem`, and `submit()`/`requestSubmit()` producing the GET navigation
a browser would make. A method the host does not run is logged, never guessed at.

### The envelope

Each mint runs inside limits enforced outside the page (`envelope.rs`); a breach ends the mint
with a named cause, never a hang.

| Limit | Default | Mechanism |
|---|---|---|
| Wall clock | 10 s (`V5_ENGINE_WALL_MS`) | watchdog thread, `terminate_execution`; includes blocked time |
| JS heap | 256 MB | near-heap-limit callback terminates V8 |
| Typed arrays / `ArrayBuffer` | 128 MB live | custom allocator refuses past the cap |
| WebAssembly memory | 128 MB total | V8 per-memory page flag plus host charges; resident-memory ceiling as backstop (`VmRSS`, working set, `task_info`) |
| Ops | 96 per mint | counted in Rust |
| Network | http(s), public addresses, 5 redirects, 1 MB request, 8 MB per response, 32 MB total | one guard for URL, resolver and redirect policy; the page's own host is exempt from the private-address rule |
| Concurrency | one engine per process at a time | process-wide slot |

The guards live in the Rust op bodies, not the JS wrapper, and `Deno` is deleted before any page
script runs. Async `WebAssembly.compile/instantiate` on raw bytes never settle here (V8 delivers
them as platform tasks and the host drains microtasks only); no measured challenge needs them.

### Encounter lifecycle

0. **Cache.** A stored copy is served only as the page cache allows: earned freshness, else a
   conditional request whose 304 is the answer.
1. **Pace.** Wait out the origin's minimum interval.
2. **Spend.** One request as the best identity for this origin, with its own cookies only.
3. **Classify.** Content, or a class with its vendor and ask.
4. **Content:** record the win, the vendor prior and the cookie state. **Otherwise follow the
   vendor:** a named wall reorders the identities left to try. A human wall costs one identity
   past the class that saw it and a second one ends the encounter; a second identity shown the
   same text as the first with no wall named ends it too (the site is not reacting to the
   client). A remembered origin gets one or two attempts, not the first-contact search.
5. **Answer a native challenge.** If the page carries a hand-off we have a contract for, do
   exactly what it asks and re-classify.
6. **Ask the interface.** If the document is still walled, a shell or incomplete and the origin
   has a data path for *this* URL, ask it as the same identity, obeying its published limits.
   Items that reach the minimum are content with `transport: data`. An interface that is only
   asking for time (paced, cooling, 429) has not answered and the encounter falls through.
7. **Run the site's program.** Which program and when: AWS WAF decides by client class, so a
   plain client is re-asked as `chrome-local` and that page is executed; Cloudflare's page goes
   to the engine on the last allowed attempt; Reddit's gate is run as the client that received
   it. What the mint produces is kept and spent by the same identity, and a 429 is paced.
8. **Remember.** Scores, wall class, next identity, route, cookies and minted credentials
   persist, so the next encounter starts at step 2 with better information.

### The census, by class

Two taxonomies: the **six wall classes** (state, state renewal, request contract, identity or
egress, human interaction, the page's own program) and the **nine outcome classes** reported.

| Outcome | Wall class | Meaning | Ours? |
|---|---|---|---|
| `content` | covered by state we hold | the contract passed | done |
| `wall_state` | state renewal | clearance or session missing or stale; a login form is the commonest shape | declared — never a failure; sessions deferred, never credentials |
| `wall_contract` | request contract | the page carries a challenge payload a native contract can answer | yes — contract |
| `wall_identity` | another identity or egress | refused with no page of its own, or a short page titled as a refusal even when served 200 (Akamai's "Access Denied") | yes — identity/egress |
| `wall_interaction` | human interaction | slider, press-and-hold, captcha, Turnstile widget | identity only — the vendor's known passers are tried; otherwise declared |
| `wall_program` | the page's own program | managed JS challenge, WAF stub (`202`/`204`, `x-amzn-waf-action`), or a page whose inline program fills and submits a challenge form | yes — engine, then interface |
| `shell` | page's program, unanswered | 2xx, program-driven application with no content in the document | yes — interface |
| `incomplete` | — | 2xx, contract failed, no wall, no shell | no — declared |
| `network` / `http_error` | — | no response, or an HTTP error with no wall markers. A failure before any client is shown ends the encounter on its first attempt; one after the client spoke earns one identity of another wire class (a silent server's on a 4 s clock), never a third | diagnose |

Content contracts are the frozen corpus's own predicates: title, minimum text, required phrases,
selector counts — data, nothing relaxes them. With no manifest contract the generic one is 200
characters of visible text, and a page of at most 1,500 that asks for a human, or whose title or
first heading is a refusal or interstitial, is not content. Vendors name themselves in headers
(`x-amzn-waf-action`, `x-datadome`, `cf-mitigated`, `x-px`) and classification reads those before
any body. A hard-corpus predicate is derived from what the reference browser shows and validated
against a copy fetched by an independent plain client; targets where the browser itself receives
a stub are left out, because a contract only the fetcher under test could satisfy would be
fitting.

### Targets, as they stand

| Target | Vendor | Ask | State |
|---|---|---|---|
| Etsy, Yelp | DataDome | captcha | no identity passes |
| Bloomberg, Zillow | PerimeterX | press and hold | desktop Chrome class passes intermittently; Zillow passes all but `chrome` |
| Glassdoor, Product Hunt, Indeed, npm, Medium | Cloudflare | managed challenge | content by identity (`okhttp`; Indeed `chrome-win`) |
| TikTok | — (shell) | signed `item_list` API | declared: every identity gets the same shell |
| AliExpress, Instagram | — | stub / login shell for `plain` | the Chrome class passes |
| crates.io | — (Ember shell) | `/api/v1/summary` | data path, paced 1 req/s |
| PubMed (NCBI) | NCBI | `/_pow/solve`, HTTP 203 | answered natively |
| SiteGround sites | SiteGround | `sg-captcha`, SHA-1, 20–22 bits | answered natively |
| Stack Exchange | Cloudflare | managed challenge | questions and tags read through the public API |
| OpenReview | Turnstile (own page) | interactive | declared |
| Federal Register | reCAPTCHA | captcha | declared; its public API is the open route |
| WSJ | DataDome-class 401 | interaction | declared; its RSS is frozen since Jan 2025 |

### Measured negatives — do not rebuild these

- **Interface discovery** (read the shell, ask the endpoints its scripts name). The walls we lose
  against serve stubs: 2 KB (IMDb), 5 KB (crates), 7 KB (Booking), 12 KB (Bloomberg), 33 KB
  (npm) — no inline state, no framework payload, no endpoint literals. Where real shells arrive
  (TikTok 460 KB, AliExpress 477 KB) their own bundles return **zero** endpoint paths: modern
  bundles assemble paths at runtime. Of seven shells, none carried a Next/Nuxt/Gatsby/Astro
  build id. The walls are decided before a document exists, by who is asking and from where. A
  discovery step may read one document and a handful of its own assets; needing the whole bundle
  graph is a refusal, not an optimisation.
- **Arrival realism** (warm-up of the origin, referrer, `sec-fetch-site: same-origin`): **no
  class changed** on 16 targets, no regressions, one extra request paid. Off by default
  (`--arrival` enables it).

### Trust, bodies, dependencies

**TLS trust:** the bundled Mozilla roots plus the OS's own store (Windows ROOT, the macOS
keychain, the Linux bundle, `SSL_CERT_FILE`/`SSL_CERT_DIR`, and the product's
`[tls] cert_file`/`cert_dir` through `add_trust_pem`; a bundle that does not parse is left out
and named), read once per process, as Chrome trusts the OS. It does not touch the ClientHello.

**Bodies:** every read is incremental and capped — 64 MiB for documents, the envelope's for the
engine; non-UTF-8 bodies keep their bytes. **Rendering:** not here; a page that needs rendering
is classified and reported.

The dependency rule holds: `wreq` (HTTP/TLS), `wreq-util` (profiles), `tokio`,
`futures-util` (capped streaming reads), `serde`, `url`, `scraper` (HTML parsing for contracts,
not a browser), V8, `btls` (WebCrypto on the BoringSSL the TLS stack links), `getrandom`, and
`webpki-root-certs` with `rustls-native-certs` — no browser, no layout, no raster.

## Environment

| Variable | Meaning |
|---|---|
| `V5_STATE` | state file path (default `donsetch/v5/state.json` under the OS cache directory: `%LOCALAPPDATA%`, `~/Library/Caches`, `$XDG_CACHE_HOME` or `~/.cache`; empty disables persistence); cached pages live in `pages/` beside it |
| `RUSTY_V8_ARCHIVE` | a local prebuilt V8 static library, for offline builds |
| `SSL_CERT_FILE`, `SSL_CERT_DIR` | extra trust roots, added to the bundled set and the OS store |
| `V5_PERSONA` | captured identity JSON overriding the built-in `personas/chrome-local.json` |
| `V5_ENGINE_LOG` | `1` prints the challenge engine's host surfaces, cookies and errors |
| `V5_ENGINE_DUMP_DIR` | a directory: every request the challenge page makes, headers and body, written out so a program's failure is read instead of guessed at (inspection only) |
| `V5_ENGINE_WALL_MS` | the engine's wall-clock cap per mint (default 10000); the other limits are `envelope::Limits` defaults |

Tests are offline: `cargo test --release --offline`. A few tests are `#[ignore]`d by default —
the AWS WAF fixture tests need the private captures (`--include-ignored`) and one is a
measurement. Engine tests take a process-wide turn, so timing assertions are not contaminated by
parallel tests. Measured runs land in `runs/` (bench JSON, the identity matrix, datapath
equivalence).

Where the suite runs: linux-x64 and linux-aarch64 in CI, both with the product binary starting
(PDFium links with the toolchain's `rust-lld`; see the root README, "LLD"). The product's macOS
and Windows jobs exercise this crate as a library through their own suites; a native `cargo
test` on those two happens on a contributor's machine at release, never in CI.
