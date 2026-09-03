# Changelog

All notable changes to DonSeTch are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **`Proxy` and BYOK `KeyEntry` derived a plaintext `Debug`:** neither
  type has a live call site that formats it with `{:?}` today, but
  nothing in the type system stopped one from being added later and
  silently leaking a proxy password or a BYOK API key into a log or
  error message. Both now redact the secret field behind a hand-
  written `Debug` impl; `ProviderConfig`/`ByokConfig`'s derived
  `Debug` picks up the redaction automatically through the nested
  `KeyEntry`.

## [3.5.2] - 2026-09-03

### Added

- **DeepSeek Harness (dsh) native plugin:** first-class dsh support
  in a separate repo, [donsetch-dsh](https://github.com/dondai44423/donsetch-dsh).
  One install line (`dsh plugin --profile web add github:dondai44423/donsetch-dsh`)
  gives every dsh agent the full web suite as native `donsetch_*`
  tools: in-process registration on the harness registry (permissions,
  timeouts and cancellation apply like any native tool), platform
  binary auto-download with SHA256 verification against the release
  sidecar, auto-updates tracking DonSeTch releases, live pickup of
  `donsetch keys add` config changes from the terminal, call/result
  cards in the Web workbench, and a `donsetch_status` self-diagnostic
  tool. Keyless engines work out of the box: no API key required.

### Added

- **BYOK search plugins:** platforms without a native adapter can
  now be hooked through any user-registered executable that
  answers a tiny stdin/stdout JSON contract (format 1:
  `{query, max_results, intent, deadline_ms}` in,
  `{results:[{title,url,snippet?,score?}], degraded?}` out).
  Register with `donsetch keys add plugin <name> --cmd '...'
  [--timeout N] [--test]`; the plugin then joins the same default
  provider / fallback chain as natively supported keys, with
  attribution, dedup, and rerank handoff unchanged. Any language
  works. Reliability is enforced on our side: direct exec (never
  a shell, argv tokenized once at registration), hard per-plugin
  timeout with SIGKILL + kill-on-drop (MCP cancellation can never
  orphan a child), 8 MiB stdout / 64 KiB stderr caps, overflow
  kill, concurrent stderr draining (no pipe deadlocks), one
  attempt per call with honest errors, graceful fallback. Names
  are validated against native providers, keyless engine ids and
  "local". New doctor check reports registration state and warns
  on missing program files; `keys list` renders the plugin
  section; `keys default` accepts plugin names. Native support
  for big/keyed providers keeps coming - plugins are the bridge
  for everything else. (README BYOK plugins section documents the
  contract.)

### Fixed

- **`DONSETCH_HTTP_CORS` without `DONSETCH_HTTP_TOKEN` was a silent
  drive-by footgun:** CORS and bearer auth on the opt-in HTTP MCP
  transport are independently optional env vars, so enabling
  permissive CORS (any origin) without also setting a token left
  the server wide open: any webpage in a local browser could POST
  arbitrary MCP tool calls (fetch/crawl/search, including the
  `actions` browser-automation surface) with no authentication, the
  classic "localhost server + permissive CORS" drive-by pattern.
  The HTTP transport now refuses to start with that combination
  instead of silently accepting it, with an error pointing at
  `DONSETCH_HTTP_TOKEN`.

## [3.5.1] - 2026-09-03

### Added

- **Bright Data connection UX and diagnostics:** `donsetch keys add
  unlocker|brightdata` now validates the key and zone shape at add
  time and prints the active zone; `donsetch doctor --deep` adds a
  FREE live zone probe (the route_ips endpoint validates token +
  zone before the first paid call) and the default doctor shows
  the daily-cap usage, solve-cache state and kill-switch state for
  the solver and the SERP key state.

- **SerpBase as a BYOK search provider (PR #109, gefsikatsinelou):**
  Google SERP via serpbase.dev with the X-API-Key auth their docs
  specify, business-status envelope handling (1001 unauthorized
  marks the key dead so rotation moves on), organic-result mapping
  with position-derived relevance, and the same error
  classification as the other providers. No dependency additions;
  nothing changes when the key is absent. Closes #94.

### Fixed

- **Bright Data solver errors were bare status codes; the jar was
  behind the current API contract:** every failure class the paid
  tier can produce is now typed (api/network/config/solve/internal)
  with a recovery hint attached to the fetch escalation trace.
  `parse_response` reads the CURRENT contract (x-brd-status-code /
  x-brd-error-code / x-brd-error response headers, legacy JSON
  fields still accepted), zone-not-found is classified as a fixable
  config problem instead of a generic API error, 403 policy blocks
  no longer mark a healthy key dead, transient solve classes named
  retry-friendly by the docs get one automatic retry (failures are
  never billed twice), and the solve timeout default now sits
  inside Bright Data's documented 30-150s unlock window. Solve-cache
  v2 stores bodies byte-exact (v1's lossy UTF-8 round trip could
  corrupt binary bodies on a cache hit), the parallel-gate map is
  pruned so a long-lived daemon does not leak one mutex per URL,
  and stale daily counters are cleaned up. BD SERP queries now
  percent-encode UTF-8 correctly (non-ASCII queries were mangled
  before). Proven end to end by a six-test live suite over a fake
  Bright Data API (tests/bypass_live.rs).

- **pi-extension: startup banner corrupted the viewport:** the
  `[donsetch] N tools registered: ...` line printed on every
  `session_start` via a raw `process.stderr.write`, which bypasses
  pi's TUI paint cycle. Under parallel background agents each
  process's banner interleaved with the others on the same screen,
  producing garbled repeated lines above the status bar. Now gated
  behind `DONSETCH_DEBUG`/`DEBUG`, matching the existing gate on the
  daemon's forwarded stderr diagnostics (issue #95) so a normal
  session prints nothing.

- **Secure cookies could replay over plain HTTP (security):** the
  tier-1 jar dropped the `Secure` attribute at both ingresses:
  bare `Secure` tokens from `Set-Cookie` never matched the
  key=value-only attribute parser, and the tier-2 harvest import
  read the real flag from the browser record but discarded it.
  A cookie set `Secure` on an HTTPS visit was replayed on later
  plain-`http://` requests to the same host, exposing the session
  to any passive network observer. Reported privately by mnaza
  via the GitHub security advisory flow (own draft advisory, will
  be published with the patched release). The jar now: stores the
  `Secure`, `HttpOnly` and `SameSite` attributes including bare
  tokens; rejects a Secure cookie arriving over plain HTTP;
  refuses to attach Secure cookies to non-HTTPS requests (both
  the fetch loop and the CDP-harvest import path); enforces the
  `__Secure-`/`__Host-` prefix rules and `SameSite=None requires
  Secure`; and round-trips the real flags through the snapshot
  handoff. Proven by a live socket regression test that fails on
  the old code (a setup server's Secure cookie got replayed in
  cleartext) and passes now, plus unit coverage for every
  ingress and gate.

- **Windows rooted-without-drive archive paths passed the cloak
  extraction guard (PR #98, mnaza):** `safe_member` used
  `is_absolute()`, which Windows defines as root plus drive
  prefix: an archive entry like `/tmp/...` or `\tmp\...` has a
  root but no prefix, reported not-absolute, and could escape the
  extraction root when joined. The guard now uses `has_root()`
  (identical semantics on Unix), with the regression case pinned
  in the traversal test. The screenshot path resolver got the same
  `has_root()` treatment so its first line of defense matches
  reality instead of relying on the canonical-frame check further
  down.
- **PathBuf import now gated by x86_64 flag (PR #104):** this import
  was used only on x86_64 and caused clippy errors on other architectures.
- **Bing result cards could expose attribution links as results (PR #99):**
  grouped selectors followed document order and could choose the breadcrumb
  anchor before the actual heading. Bing parsing now prefers the `h2` result
  link while retaining the existing fallback selectors.
- **Requested table-of-contents output could be replaced by page text
  (PR #101):** content-rescue paths treated a compact outline as a failed
  extraction on long pages. A completed TOC projection now returns directly
  instead of falling through to body-oriented rescue.
- **Short PDFs could be mislabeled as HTML application shells (PR #102):**
  downstream shell heuristics could mark valid, compact PDF output as thin
  and suggest browser rendering. Documents already identified through PDF
  page metadata now bypass HTML-only shell classification.
- **Tracking parameters split identical search results during URL
  deduplication (PR #103):** normalized URL keys retained analytics
  identifiers such as `utm_*`, `gclid`, `fbclid`, and `msclkid`, so the same
  page could appear as distinct candidates. Deduplication now drops a
  conservative set of known tracking keys while preserving meaningful query
  parameters, ordering, and encoding. (changelog: document tracking-parameter deduplication)
- **Yahoo result titles included breadcrumb and URL text (PR #100):**
  result cards can wrap their breadcrumb and heading in one outer link, so
  reading the whole anchor produced noisy titles. Yahoo parsing now extracts
  the dedicated heading first and keeps the outer text as a fallback.
- **PDF bookmark titles carried two trailing NUL characters:**
  `FPDF_GetMetaText`/`FPDFBookmark_GetTitle` report their length in bytes
  (including the UTF-16 NUL terminator), but the decode call was treating
  that count as UTF-16 units, a mismatch that read past the real string
  into the buffer's zero-initialized slack. `get_meta`'s output was
  unaffected (it already strips all `'\0'` chars), but every extracted
  outline/bookmark title picked up two invisible trailing NULs. Both call
  sites now convert bytes to units first, matching the sibling
  `field_string` helper in `forms.rs`, which already did this correctly.
- **A stray HTTP/2 RST_STREAM could abort the wrong request on a
  reused connection:** every other per-stream frame type (HEADERS,
  CONTINUATION, DATA) in the h2 read loop is scoped to the current
  stream id, but RST_STREAM matched any stream id. On a pooled,
  reused connection, a late RST_STREAM for an already-finished prior
  stream would abort a completely unrelated new request in flight.
  Also: the RST_STREAM sent to refuse a PUSH_PROMISE (a spec
  violation, since we advertise `ENABLE_PUSH=0`) carried the wrong
  payload: the current request's own stream id instead of a 4-byte
  HTTP/2 error code (RFC 7540 §6.4); it now sends `REFUSED_STREAM`.


## [3.5.0] - 2026-09-01

### Added

- **Session vault: logins survive daemon restarts, crashes, and a
  `kill -9`.** Every tier-2 run now harvests login/session cookies
  (after actions/solve fetches, and again at reap time) into
  `ghost-state.json`: junk-filtered, deduped, capped, atomic write
  like the rest of the state file. Every browser launch replants
  them before the first navigation, batch CDP call with a
  per-cookie fallback for older builds. A session established today
  is still there next week, even if the daemon died hard in
  between. Live-verified three ways: cookie + Local Storage across
  separate processes; a full freeze -> reap -> relaunch cycle in
  one daemon; and with Chromium's Cookies DB file deleted outright
  while the session still came back from the vault.

### Fixed

- **Xvfb startup race and misleading diagnostics (issue #95):**
  concurrent sessions racing for the shared display could kill each
  other's Xvfb via the stale-cleanup pkill, degrade to headful
  off-screen, and blame the package manager. Startup is now
  serialized through a create_new gate: one coordinator, everyone
  else reuses the winner's display. A present binary that exits
  early surfaces its own last stderr line, never an install hint;
  the reporter's fake-Xvfb repro is a regression test. Stale gates
  self-heal after 30s. `DONSETCH_XVFB_DISPLAY` overrides the display
  number for multi-daemon hosts.
- **Windows profile lock stealable from a live daemon (PR #97,
  mnaza):** the lockfile mtime was set once at creation, so every
  lock older than 10 minutes looked stale and could be deleted out
  from under a still-running daemon: a second process would then
  claim the same profile. The lock now heartbeats every 120s, and
  the stale window has three heartbeats of margin; the heartbeat is
  aborted before removal in Drop.
- **pi extension stderr noise:** raw MCP stderr forwarding put every
  non-fatal daemon warning into pi's TUI as a popup. Only crash or
  fatal lines surface now; `DONSETCH_DEBUG`/`DEBUG` restores full
  forwarding.

### Changed

- **Browser version reporting is the real build:** version probing
  is cached (one spawn per binary per process, was one per ghost
  launch), and the full dotted build now flows through resolution
  into `doctor`, `status`, and backend descriptions instead of a
  padded major.
- **CloakBrowser launches keep the stealth that matters:** the
  extension/plugin and default-app disabling flags are dropped for
  the cloak backend only, so its C++-level plugin enumeration
  patches stay effective; stock Chromium keeps the hardened flags.


- **prebuilts refused to start on Ubuntu 22.04 LTS (issue #93):**
  the release legs built on a glibc 2.39 runner, so both npm and
  GitHub release binaries demanded `GLIBC_2.39` and every 22.04
  host died at first launch. Linux legs now build on
  ubuntu-22.04 (glibc 2.35 baseline), pinned to lld (bfd 2.38
  chokes on rustc 1.98's `.crel` relocations), and a new hard CI
  gate objdumps the built bytes and fails the release if any
  symbol exceeds 2.35: a regressed glibc leak now dies in CI,
  not on a user's VM. README carries the verified build recipe.
- **Session vault discipline:** replay and reap-harvest ride ONLY
  the shared profile, so a temp-profile divergence run can never
  borrow or overwrite the canonical session (a vendor that binds
  sessions to fingerprints would see one login on two profiles);
  tier 1 boots with the vault at daemon start, so a JS-less domain
  gets an authenticated plain-HTTP fetch on the first request
  after a restart; plain renders harvest too, not just solve and
  actions.
- **Cookie harvests could stall a finished fetch:** solve and
  actions harvested with the 20s generic CDP timeout, so a wedged
  browser added a 20s tail to a completed response. All harvest
  sites now carry explicit 3-5s bounds and degrade to no-vault
  instead of stalling.
- **Crawl renders kept their cookies to themselves:** a login set
  during a crawl's JS-render now lands in the session vault and
  the tier-1 jar like every other tier-2 flow.
- **Windows daemon collision on the shared profile:** two
  daemons fought Chromium's singleton and the loser died without a
  DevTools line. A create_new profile lockfile now mirrors the
  unix flock: the loser diverges to a temp profile, and a stale
  lock left by a dead daemon recovers by age (10 min).
- **Windows profile lockfile could be stolen out from under a live
  daemon:** the lockfile's mtime was never refreshed after
  creation. Windows kills the Ghost on every guard drop (unlike
  Linux/Xvfb's warm-freeze path), so the lock is normally held for
  one call's duration : but a single long `actions` script (up to
  16 steps, each wait capped at 60s) can legitimately outlast the
  10-minute staleness window while the Ghost is nowhere near dead.
  A second daemon starting mid-call would see the un-refreshed
  mtime, mistake the still-live holder for abandoned, and steal the
  profile : the exact collision the lock exists to prevent. The
  lockfile's mtime now gets refreshed every 2 minutes for as long
  as a Ghost holds it, opened with FILE_SHARE_DELETE so the
  refresh can never block cleanup on exit.
- **CloakBrowser archive extraction didn't reject rooted paths on
  Windows:** `safe_member()`'s traversal guard used `is_absolute()`,
  which requires a drive prefix on Windows: a path like
  `/tmp/chrome` has a root but no prefix, so `is_absolute()` is
  false there even though joining it onto the extraction dir still
  escapes it (Windows path-join semantics replace everything past
  the prefix for any rooted push). Practical impact is narrow: the
  archive's hash is checked against a manifest that is itself
  Ed25519-signed and verified against a public key pinned in this
  binary, before extraction ever starts: exploiting this needs a
  compromise of that signing key or release process, not just an
  untrusted archive. Switched the
  guard to `has_root()`, which `is_absolute()` is itself defined as
  on Unix (no behavior change there) and is the correct, broader
  check on Windows.
- **Browser fingerprint noise:** the ghost no longer runs Chrome
  default apps or extensions, killing the surprise-component
  detection class (enumerable extensions, default-app traffic)
  without touching the browser surface sites actually check.
- **Ghost reap used to discard the session's newest cookies (and
  could eat a login).** The reap SIGKILLed the process group with
  no shutdown handshake, so the cookie checkpoint Chromium only
  makes on clean exit never hit disk. Reap now thaws, harvests the
  session vault, sends `Browser.close`, waits a bounded 6s for the
  clean exit, and only then falls back to the hard kill. Same CDP
  path on all three platforms. A profile's Cookies DB that had
  sat untouched since 2026-08-30 now checkpoints on every exit.
- **selftest pages littered the persistent browser profile** when
  a daemon died mid-check; they now live in the system temp dir.
- **Seven std mutex lock sites still panicked the daemon if the
  lock was poisoned** (panic = abort build): converted to the same
  poison-safe pattern as the earlier sweep.
- **`focus_match` compound-term check crossed word boundaries:** the
  crawl frontier's hard focus gate matched a compound query term
  (e.g. `auto-complete`) against any URL/anchor text containing it
  as a raw substring, so `auto-completed` (a different word once
  stemming strips `-ed`) falsely counted as a match. The full-form
  check is now a contiguous token-subsequence match instead of a
  string `contains`, closing the same word-boundary gap the
  existing fragment-based check (added in v2.3.1) already guarded
  against.

## [3.4.4] - 2026-08-31

### Added

- **MCP `instructions` at initialize:** the handshake now carries a
  short server blurb (one line per tool, generated from the spec
  table) so deferred-loading MCP clients can tell the agent these
  tools exist before their schemas load. Kept small, gated at 150
  est. tokens by a token invariant, with a golden fixture pinning
  the whole initialize result.
- **SerpApi BYOK provider:** `donsetch keys add serpapi <key>` wires
  up [SerpApi](https://serpapi.com) as a Google-SERP BYOK backend,
  alongside the existing Serper.dev provider. Routes by intent like
  the other providers: `google_scholar` engine for paper queries,
  `tbm=nws` for news.
- **Brave Search API BYOK provider:** `donsetch keys add bravesearch <key>`
  wires up the official, keyed
  [Brave Search API](https://api.search.brave.com) : distinct from
  the existing keyless `brave` SERP scraper. Uses a dedicated news
  endpoint for news-intent queries.
- **Playwright-managed Chromium discovery (issue #84):** the browser
  probe now finds every Playwright layout (chrome-linux64,
  chrome-win64, chrome-mac-arm64 plus the legacy dirs), honors
  `PLAYWRIGHT_BROWSERS_PATH` and `XDG_CACHE_HOME`, and does it via
  one shared helper on all three platforms. The headless-shell
  registry stays excluded on purpose (strictly weaker CDP target).
- **Selectable browser backend:** `DONSETCH_BROWSER_BACKEND` now supports
  `chromium` for the original shipped behavior, `headless` to force the
  original Chromium binary into `--headless=new`, and `cloakbrowser` for the
  CloakBrowser backend. `auto` (the default) keeps the shipped Chromium
  behavior; CloakBrowser runs only after explicit backend selection, and
  downloads stay opt-in via `DONSETCH_CLOAK_AUTO_DOWNLOAD=1`.
- **Explicit CloakBrowser backend:** DonGhost resolves Chromium versus
  CloakBrowser explicitly, accepts `CLOAKBROWSER_BINARY_PATH` without network
  access, and supports opt-in (`DONSETCH_CLOAK_AUTO_DOWNLOAD=1`) public binary
  installation with signed-manifest, version, checksum, and archive-path
  verification. Source/path/version and deep fingerprint status are visible in
  `doctor` and `status`; CloakBrowser payloads remain outside releases and
  Docker images.

### Fixed

- **frontier focus scoring now has real IDF (issue #86):** the
  crawl frontier scored every query-token hit with flat weights,
  so ubiquitous site-furniture tokens (/docs, /api) buried precise
  matches. The map phase now builds an Okapi IDF table from the
  site's own sitemap inventory and the frontier scores with it:
  distinctive tokens multiply their hits, common ones shrink. No
  inventory (BFS mode, resume) keeps the exact pre-IDF flat
  weights as a separate tested path.
- **focus tool descriptions now match the code:** web_fetch focus
  is BM25 keyword matching with the cross-encoder pass only on
  pages of 80 blocks or fewer AND only when the rerank model is
  already cached (never a mid-fetch download); web_crawl focus is
  BM25-lite link-text/URL-path scoring, no semantic matching
  before fetch, hard no-shared-token gate at enqueue.
- **macOS build broken by the Playwright-discovery change above:**
  `known_chrome_paths()` on macOS referenced an undefined `paths`
  variable (`E0425`) : the hardcoded-app-bundle list's `.collect()`
  was never bound to a `let`, so the build failed on every macOS
  target. Also un-broke `playwright_discovers_chrome_linux64_layout`,
  which wasn't OS-gated and failed on Windows/macOS CI runners since
  it asserts against the Linux-only `chrome-linux64` layout.
- **Xvfb install hint printed on macOS/Windows every session
  (issue #81):** the "install xvfb" advice belongs to Linux-family
  systems only; headful off-screen Chrome is the native mode on
  macOS and Windows and the hint was pure noise on every daemon
  start. The hint is a platform-gated pure function now, with a
  regression test that runs on the Windows/macOS CI legs.
- **fake-ip TUNs no longer trip the SSRF guard (issue #83):**
  networks where a DNS rewriter maps every hostname into
  198.18.0.0/15 (mihomo/Clash/Surge) saw every fetch blocked as a
  false positive. The guard is now two-tiered: URL literals stay
  strict, the DNS-resolved tier exempts the IETF-reserved
  benchmarking block only, and every real private range stays
  blocked. `DONSETCH_ALLOW_PRIVATE_EGRESS=1` now works end to end
  (it was dead at the guard layer). Transport pinning agrees with
  the guard so no layer re-blocks what another allowed.

## [3.4.3] - 2026-08-30

### Fixed

- **must_contain probe regressions (issue #80):** regex probes with
  a trailing flag like `/needle/i` were treated as literals and
  returned a false NO MATCH; `must_contain` on non-HTML passthrough
  bodies (text/plain, json, xml) silently returned the full document
  instead of the probe; and `section=` was silently ignored on
  adapter pages (both the extract fixture layer and the fetch-level
  URL rewrite now defer to the generic pipeline when a section is
  requested).
- **Semantic reranking no longer starves async workers (PR #77):**
  with the rerank feature on, concurrent searches ran synchronous
  ONNX inference directly on Tokio workers while other workers
  parked on the shared session mutex, starving timers and I/O.
  Ranking now runs on the blocking pool (rerank builds only; the
  inline path is unchanged otherwise). Measured with 8 concurrent
  jobs on 2 CPUs: mean max executor stall 573.5ms to 3.5ms, no
  latency regression, identical result digests.
- **Same stall fixed on the fetch side:** focus extraction ran the
  cross-encoder inline on the async worker. Scores now flow through
  `block_in_place` on multi-thread runtimes (inline otherwise, since
  `block_in_place` panics on current-thread runtimes), with a
  single-worker timer regression test that fails on the old code.

### Added

- **Parallel query variants for `web_search` (PR #79):** a search
  call can now carry up to two explicit `query_variants` alongside
  the base query. All run concurrently under one shared deadline,
  each keeps DonSeTch's existing ranking and returns as a clearly
  separated result set, one global S-handle table covers every
  result, and partial failures keep the successful searches.
  DonSeTch never invents variants: the calling agent supplies
  alternative formulations, the tool only fan-outs. Single-query
  behavior, envelope, and cache keys are byte-for-byte unchanged.

## [3.4.2] - 2026-08-29

### Fixed

- **Fetch guard starvation (issue #76):** a single broadcast `Lagged`
  during an event burst killed the CDP fetch-guard loop permanently,
  every later `Fetch.requestPaused` went unanswered, and Chrome
  wedged the whole tier-2 session into CDP timeouts. The guard now
  survives `Lagged` (it has already resynced) and exits only on
  `Closed`; a regression test reproduces the exact overflow shape.
- **OCR/rerank dead on Ubuntu 20.04/22.04 and any glibc < 2.38:** the
  shipped `libonnxruntime.so` was relinked from pyke's static archive
  and required GLIBC_2.38 plus six `__isoc23_*` symbols, so it could
  not even load on most long-term-support distros. Linux now ships
  the official Microsoft prebuilt, which requires only GLIBC_2.27
  (Ubuntu 18.04+) and has no isoc23 imports; it is also a third
  smaller (22MB vs 33MB). No LD_PRELOAD shims, nothing for users to do.
- **ONNX loader hangs can no longer hang the server:** ort's init path
  can deadlock inside the dynamic loader instead of returning an
  error (pykeio/ort #579, #560). Init now runs on a dedicated thread
  with a 15s bound; a hang fails fast with a clear message, poisons
  a flag so retries cannot stack leaking threads, and fetch/search/
  crawl/PDF stay fully working.
- **Mutex poisoning no longer kills the daemon:** with
  `panic = "abort"`, any panicking worker thread poisoned a std
  mutex and the next locker aborted the whole process. All std
  mutex/rwlock unwraps now recover from poisoning (116 sites).

### Added

- **PDF on Linux ARM64:** the v3.4.2 native-arm64 CI experiment proved
  the current pdfium-static pin fixed the old
  `FPDF_LoadMemDocument64` hang (the full pdf:: suite passes on
  native aarch64), so the arm64 prebuilt now ships PDF alongside
  fetch/search/crawl.
- **Doctor v2:** `--deep` runs the live browser probe (default fast
  mode skips it); `--json` emits one machine-readable document at
  the tail; `--fix` repairs the mechanical problems (cache dirs,
  stale ghost state, corrupt models); MCP client detection prints
  ready-to-paste registration blocks (Claude Desktop, OpenCode,
  Hermes, .mcp.json).
- **Bounded CI per-test timeouts:** a hung native-code test now fails
  the job in 30s instead of wedging it until the workflow timeout.

### Changed

- README: em-dash-free, corrected platform matrix, doctor docs.
- Issue #76: closed with a precise root-cause writeup.

## [3.4.1] - 2026-08-29

### Fixed

- **First-ever model download aborted the process:** OCR and rerank models download lazily on first use, but the download used `reqwest::blocking` on the calling thread : which on first use is a tokio runtime thread (the async search path for rerank, async fetch paths for OCR). `reqwest::blocking` panics there by design, and release builds carry `panic = "abort"`, so a fresh install's first search or first scanned PDF killed the daemon instead of fetching the model. Invisible on any machine with a warm model cache, which is why it survived. Downloads now run on a dedicated plain thread joined by the caller; timeouts and verification are unchanged. (PR #72, @Mart-Bogdan)

- **OCR and rerank silently dead on Windows and macOS arm64 since 3.3.0:** 3.3.0 intended to confine `ort`'s `load-dynamic` to Linux and keep macOS/Windows on static linking, but declared `ort` in the shared `[dependencies]` table *and* in a `cfg(not(target_os = "linux"))` table. Cargo unions features across every target section whose cfg matches rather than choosing one, so `load-dynamic` reached macOS and Windows too, where it wins over static linking. It also implies `ort-sys/disable-linking`, whose build script returns before downloading anything and before `copy-dylibs` runs : so the binaries shipped with ONNX Runtime neither linked in nor present beside them, and no build-time error. Nothing failed loudly at runtime either: OCR reported scanned pages as `no text layer and OCR did not recover them` instead of reading them, and search dropped to RRF+BM25 after a 30s reranker init timeout, memoized for the process lifetime. Visible in the shipped artifacts : `donsetch-win32-x64`'s binary fell from 35.6MB to 16.3MB and `donsetch-darwin-arm64` from 14.2MB to 8.3MB, the missing ~19MB and ~6MB being the ONNX static archive. Fixed by declaring `ort` only in two mutually exclusive target sections, never in the shared one. Linux keeps `load-dynamic` and its AVX gate unchanged. Verified on Windows 11 and Windows 10 22H2: OCR restored to 98% mean confidence on the issue #26 PDF, reranker initializes, cold-start search back to ~3s. (PR #68, @Mart-Bogdan)

- **Loud failure for `--http` without the feature.** In a binary built without the `http` cargo feature (the linux-arm64 and macOS-x64 prebuilts, and any plain `cargo build`), `donsetch mcp --http` and `DONSETCH_TRANSPORT=http` silently fell through to stdio : a client configured for HTTP would hang waiting on a listener that never came up. Both paths now exit immediately with an error naming the missing cargo feature and how to get a binary that includes it. (PR #71, @imonlinux; issue #67)

- **Docs: HTTP transport interface.** The README documented flags and env vars that do not exist (`--bind`, `--token`, `DONSETCH_HTTP_BIND`, `DONSETCH_HTTP_CORS=*`). Replaced with the real interface (`--host`/`--port`, `DONSETCH_HTTP_HOST`/`_PORT`/`_TOKEN`/`_TIMEOUT_SECS`, `DONSETCH_HTTP_CORS=1`), a build-requirement note (`http` is an optional cargo feature; the linux-arm64/macOS-x64 prebuilts are core-only), corrected feature-flag notes, and a Gotchas row for requesting `--http` when the build lacks the feature. (PR #69, @imonlinux; issue #66)

- **Markdown escaping of literal emphasis characters in extracted text.** A literal `*` at the start of italic text collided with the emphasis markers and corrupted the output structure (issue #74). Text nodes now escape flanking `*`, `_`, and backticks so prose like `* These figures ...` round-trips exactly.

### Changed

- **`src/onnx.rs` module docs rewritten:** a per-target map and the rule that `ort` must stay in per-target tables now lead; below them, reference sections on how ONNX is acquired and linked on each platform, a postmortem of how the 3.3.0 feature leak stayed silent, and why Windows links `DirectML.dll` without ever calling it. (PR #68, @Mart-Bogdan)

- **README gotchas:** documented that Windows needs `DirectML.dll` present at startup (in-box since Windows 10 1903, version irrelevant, `0xC0000135` and no output when missing : and never harvest a copy from another machine's `System32`, which fails just as silently with `0xC0000142`), and that OCR/rerank models are downloaded on first use rather than bundled, with their cache locations and how to pre-seed an offline machine. (PR #68, @Mart-Bogdan)

- **Dev/test loop redesign:** new `ci` cargo profile (release optimizations, no fat LTO, `panic = "abort"` inherited) makes the 111-binary test suite link in seconds instead of minutes; switched to cargo-nextest (parallel, fail-fast locally, full failure set in CI); added `Justfile` recipes (`just check`/`test`/`lint`/`all`/`bin`/`smoke`) that collapse the previous multi-command grind into a ~4-second warm pre-push gate. Warm local iteration after a code edit: ~1m45s full gate, ~57s binary (was 8-15 min). The shipped binary keeps full fat LTO via `--release` at release time.

- **Payload gates that make dead-payload releases structurally impossible (the v3.3.0 leak class):** the `onnx_payload_probe` unit test initializes the ONNX environment and runs inside every features-enabled CI suite (fails if the dynamic load or static link cannot commit; this test is red against v3.3.0/v3.4.0 Windows/macOS binaries); `donsetch doctor`'s ONNX check on static targets is now a real `commit()` probe instead of a cfg constant; `load_and_init` surfaces `commit()` failures on both paths; release builds gate-check per-platform binary size floors (missing static ONNX collapses the binary), require `libonnxruntime.so` beside the Linux binary, and require the doctor probe to report the expected ONNX state before a release draft is created. CI also adds a Windows compile-gate for the release feature set (oc
,rerank,http without the 305MB MSVC link).

### Added

- **HTTP transport in the Docker image.** The image now builds `--features ocr,rerank,http` (matching the linux-x64/macOS-arm64/Windows-x64 release binaries), `EXPOSE`s 8765, and the bundled compose file gains an opt-in `http` profile: `docker compose --profile http up -d donsetch-http` serves MCP at `http://localhost:8765/mcp` with a listener-based healthcheck. The profiled service carries its own `build:` block (it builds the image if it isn't there yet) and `restart: unless-stopped` (Docker-level crash recovery : the HTTP transport has no in-process supervisor). The port is published on `127.0.0.1` by default so an unset `DONSETCH_HTTP_TOKEN` never exposes unauthenticated MCP to the LAN. The stdio service is unchanged. (PR #70, @imonlinux)

- **`--links`/`--media` flags for `dev extract`:** expose `include_links` and `include_media` on the local-file extraction command, matching the `links`/`media` parameters of the `fetch` tool. Both default off (token savers), so link/media rendering issues could not be reproduced offline before. The usage line now also documents the existing `--url` flag. (PR #75, @Mart-Bogdan)

## [3.4.0] - 2026-08-28

### Added

- **Tier 3 bypass fetch (Bright Data Web Unlocker):** when ghost hits a hard wall it cannot solve (interactive captcha, DataDome, Cloudflare challenge), `fetch` hands the URL to Bright Data's Web Unlocker API, which solves the wall server-side and returns rendered HTML into the normal extraction pipeline. Strictly opt-in for advanced users: `donsetch keys add unlocker <key>[::zone]` (alias `wu`). No key = behavior identical to previous releases. Only successful unlocks are billed. Guardrails: daily cap (`DONSETCH_BYPASS_MAX_DAILY`, default 50), hard timeout (`DONSETCH_BYPASS_TIMEOUT_SECS`, default 90), explicit off switch (`DONSETCH_BYPASS=0`), optional JS render (`DONSETCH_BYPASS_RENDER=1`). 12 unit tests.
- **Solve-cache:** every successful unlock is stored locally keyed by URL hash (sliding TTL, default 6h via `DONSETCH_BYPASS_CACHE_TTL_SECS`). Fetching the same page twice inside the TTL costs nothing: the second fetch is served from cache (`tier 3-cached`) and does not consume the daily cap. Hot URLs stay alive, cold ones expire, oldest-entry pruning caps the cache at 200 entries (`DONSETCH_BYPASS_CACHE_MAX_ENTRIES`). Parallel fetches of the same URL coalesce into a single paid call via an in-flight gate. Cache off via `DONSETCH_BYPASS_CACHE=0`.
- **Doctor check #15:** reports whether an unlocker key is configured.

### Fixed

- Bypass HTTP client no longer negotiates gzip/deflate: Bright Data returns JSON-wrapped HTML and reqwest auto-decompression failed on large responses (756KB pages), truncating the body.

## [3.3.0] - 2026-08-28

### Added

- **MCP streamable HTTP transport:** `donsetch mcp --http` starts an HTTP server alongside the default stdio transport. Endpoints: `POST /mcp` (JSON-RPC), `GET /mcp` (SSE stream), `DELETE /mcp` (end session), `GET /health`. Session management with 16-char random IDs, 30min idle GC, 1024 max sessions. Optional bearer auth via `DONSETCH_HTTP_TOKEN` (constant-time comparison). CORS off by default, enable with `DONSETCH_HTTP_CORS`. Per-request timeout via `DONSETCH_HTTP_TIMEOUT_SECS` (default 300). 8 new tests. (PR #58, @imonlinux)
- **Docker image and compose service:** multi-stage build (`rust:slim` builder to `debian:trixie-slim` runtime on the same glibc generation, arch-aware Go install for BoringSSL) with all features enabled, `--locked` to `Cargo.lock`, non-root runtime user, optional `INSTALL_CHROME=true` build arg for tier 2 escalation. `docker-compose.yml` runs stdio out of the box with a persistent cache volume, 2GB memory ceiling, init for zombie reaping, and a 45s stop grace period. (PR #59, @imonlinux)
- **Container-aware reranker threads:** on Linux, the ONNX intra-op thread pool is automatically clamped when cgroup v1/v2 quota or CPU affinity exposes less effective parallelism than the host's physical core count. `DONSEEK_RERANK_THREADS` env var for explicit cross-platform override. Unconstrained hosts preserve ONNX native default. 6 new tests. (PR #60, @Bnbig)

### Fixed

- **`<br>` tags flattened to spaces (issue #61):** `<br>` elements in HTML were converted to spaces instead of line breaks in the markdown output. Fixed using a NUL sentinel that survives the whitespace collapse pass, then replaced with newlines. 3 new tests.
- **AVX dynamic loading simplified to Linux-only:** macOS and Windows now use static linking (`ort download-binaries`) since building shared libraries from the static archive had unsolvable duplicate symbol issues on macOS (ld64 lacks `--allow-multiple-definition`) and Windows (MSVC linker complications). Linux x86_64 continues to use dynamic loading (dlopen after AVX check) to avoid SIGILL on non-AVX CPUs. The `cc` build-dependency was removed.

### Changed

- **lzma-rust2 0.11 to 0.15:** API rename `LZMA2Reader` to `Lzma2Reader`.
- **actions/upload-artifact v4 to v7** in CI workflows.
- **CODEOWNERS:** @Mart-Bogdan expanded to `src/ghost/`, `src/fetch/`, `.github/`.

## [3.2.5] - 2026-08-28

The AVX fix release. ONNX Runtime is now dynamically loaded at runtime instead of statically linked, fixing SIGILL crashes on non-AVX CPUs (pre-2011 Intel, QEMU default, Docker VMs without AVX passthrough).

### Fixed

- **SIGILL on non-AVX CPUs (issue #57):** ONNX Runtime's prebuilt static archive contained unguarded AVX instructions (`vxorps xmm0,xmm0,xmm0`) in C++ global constructors that ran before `main()`. Statically linking it caused SIGILL at process start on any CPU without AVX. Fixed by switching ONNX from static linking (`download-binaries`) to dynamic loading (`load-dynamic`). The base binary is now SSE2-safe (0 ONNX-linked AVX instructions). A shared library (`libonnxruntime.so`/`.dylib`/`.dll`) is built from the prebuilt static archive at compile time and dlopen'd at runtime after an AVX check. Non-AVX CPUs get a working binary (minus OCR/rerank) instead of a crash. Verified on QEMU qemu64 (SSE2-only, no AVX): `--version`, `doctor`, `fetch` all pass. On AVX hosts: OCR and rerank work via dlopen'd ONNX.

### Added

- **AVX detection with disk cache:** `src/cpu.rs` checks AVX support via CPUID with persistent caching. AVX=yes is cached permanently (never re-checked). AVX=no is re-checked each run (in case of CPU upgrade). Cache file: `~/.cache/donsetch/avx.json`.
- **ONNX runtime init:** `src/onnx.rs` manages dlopen loading of the ONNX shared library. `ensure_loaded()` is called before any OCR/rerank operation. On non-AVX CPUs, returns an error and OCR/rerank falls back gracefully (glyph stream for PDFs, RRF+BM25 for search).
- **Doctor ONNX check:** `donsetch doctor` now reports ONNX Runtime status: AVX detected + shared library present, or CPU lacks AVX with a warning.
- **QEMU non-AVX CI verification:** The release workflow now installs QEMU and runs `donsetch --version` under `qemu-x86_64 -cpu qemu64` (SSE2-only) to verify the binary doesn't SIGILL on non-AVX CPUs. This catches regressions where AVX instructions leak into the base binary.
- **ONNX shared library in release tarball:** Release tarballs for linux-x64, darwin-arm64, and win32-x64 now include the ONNX shared library alongside the binary.

### Changed

- **ort crate features:** Changed from `download-binaries` + `copy-dylibs` to `load-dynamic` + `api-24`. ONNX is no longer statically linked. The `ort-sys` build script returns early (`disable-linking`), and donsetch's `build.rs` downloads and builds the shared library instead.
- **build.rs:** Added ONNX tarball download (pyke CDN), SHA256 verification, custom LZMA2 decompression (`lzma-rust2`), tar extraction, and shared library building (`cc -shared -z noexecstack` on Linux, `cc -dynamiclib` on macOS, `link /DLL` on Windows).
- **OCR/rerank gate:** `src/pdf/ocr.rs` and `src/search/rerank.rs` now call `crate::onnx::ensure_loaded()` before initializing ONNX engines. If ONNX is unavailable (no AVX or missing shared lib), OCR falls back to the glyph stream and rerank falls back to RRF+BM25.

## [3.2.4] - 2026-08-26

The security and hardening release: unpredictable handle IDs (GHSA-g279-2v66-j8g2), centralized SSRF guard with DNS resolution, CDP Fetch interception, cookie PSL validation, Chrome sandbox opt-in, screenshot path validation, PDFium fail-closed hashes, QEMU x86-64 SIGILL fix, strict LLM provider schema compatibility, musl detection fix, bounded CDP waits for Debian 12, plus Parallel AI and Bright Data SERP as BYOK providers.

### Added

- **Parallel AI BYOK provider:** POST `https://api.parallel.ai/v1/search` with `mode: "fast"`, `x-api-key` auth, objective + search_queries input, excerpts joined to snippet. Live-tested: 7 results, ~1.1s, all on-topic.
- **Bright Data SERP BYOK provider:** POST `https://api.brightdata.com/request` with Bearer token auth, zone parsing (`key::zone` or default `serp_api1`), `brd_json=1` for parsed Google organic results. 30s timeout to accommodate variable API latency. `bd` alias: `donsetch keys add bd ...` works in all key commands (add, remove, reset, default).
- BYOK provider list updated in CLI help, README, and tool spec: Tavily, Exa, Serper, TinyFish, Parallel AI, Bright Data.

### Security

- **Unpredictable handle IDs (GHSA-g279-2v66-j8g2):** Reference handles (`S{id}`/`L{id}`) were sequential and fully predictable (`S1`-`S12`, `L1`-`L2048`), enabling cross-session information disclosure via indirect prompt injection. A page could instruct an agent to fetch handles it never received, resolving to URLs bound by a different session, project, or MCP client on the same host. Fixed: handle IDs are now random 8-char base62 tokens generated from SHA-256 of (nanosecond timestamp + PID + atomic counter + ASLR stack address). The output space is 62^8 approx 2.18x10^14, making enumeration infeasible. A page that was never given a handle cannot name one. Reported by @Mart-Bogdan.
- **Search handles no longer persisted:** Search handles (`S{id}`) are now in-memory only and never written to `handles.json`. They die with the process. This eliminates cross-session leakage through the on-disk table. L-handles (`L{id}`) remain persisted (useful across restarts) but with unguessable random IDs.
- **Versioned persistence format:** `handles.json` now carries a `version` field (currently 2). Old-format files (no version field) fail to deserialize and are silently discarded. Old-format sequential handles that somehow survive the format change are filtered out during load.
- **`DONSETCH_URL_HANDLES=off` disable switch:** When set, both emission (search results show raw URLs, links keep hrefs) and resolution (`is_handle` returns false, `resolve_fetch_url` refuses handles) are disabled. The on-disk table outlives the switch but cannot be addressed while it is off.
- **Search handle rebind correctness bug fixed:** `set_search_results` no longer overwrites positions 1..n. A new search mints new random handles, so earlier ones keep resolving to what they always meant. No more silent repointing of `S3`.
- **LRU eviction uses monotonic counter:** Eviction ordering now uses a per-entry `seq` field (monotonic counter) instead of wall-clock seconds, which could collide for rapid inserts.
- **Centralized SSRF guard with DNS resolution (#52, @amitamit10):** URL validation is now centralized in `fetch::guards`. `validate_url_basic` (sync) checks scheme, credentials, and literal IP ranges. `ensure_url_safe` (async) adds DNS resolution and rejects hostnames that resolve to private/loopback addresses. Extended `is_private_ip` to cover multicast, documentation, benchmarking, reserved, 6to4 relay, and IPv4-mapped/compatible IPv6. Redirect targets are re-validated per hop. The crawl seed, fetch entry, and browser navigation all pass through this gate.
- **CDP Fetch request interception (#52, @amitamit10):** The ghost browser now intercepts every network request at the CDP `Fetch.requestPaused` layer. Each paused request is validated via `ensure_url_safe` before `Fetch.continueRequest` or `Fetch.failRequest` (`BlockedByClient`). Post-action navigation is re-checked. Defense-in-depth alongside the pre-navigation guard.
- **Cookie domain validation with Public Suffix List (#52, @amitamit10):** Cookie `Domain` attributes are validated against the Public Suffix List (via `psl` crate). Public suffixes (`com`, `co.uk`) are rejected, preventing cookie tossing. Domain and host normalization rejects control characters, empty labels, oversized labels, leading/trailing hyphens, and non-DNS characters. Dot-boundary matching prevents `evil-example.com` from matching `example.com`.
- **Chrome sandbox opt-in (#52, @amitamit10):** `--no-sandbox` and `--disable-setuid-sandbox` are no longer passed by default. The sandbox stays enabled unless `DONGHOST_NO_SANDBOX=1` is explicitly set (with a loud warning). The escape hatch exists for containers without user-namespace support.
- **Screenshot path validation (#52, @amitamit10):** Screenshot output paths are constrained to `cache_dir()/screenshots`. `resolve_screenshot_path` rejects `..` traversal, symlinks escaping the root, absolute paths outside the screenshots dir, and NUL bytes. Ghost debug DOM dumps also go to `cache_dir()/ghost-debug` instead of `temp_dir()`.
- **PDFium fail-closed hash verification (#52, @amitamit10):** `build.rs` now requires a pinned SHA256 for each PDFium platform asset. Missing or mismatched hashes fail the build instead of silently proceeding.

### Fixed

- **SIGILL on QEMU-emulated x86_64 (issue #51):** Prebuilt x86_64 binaries used CPU instructions (AVX2/AVX-512) from the GitHub Actions runner that QEMU/KVM userspace emulation does not support, causing immediate `Illegal instruction` (exit 132) on every command. Fixed: the release workflow now sets `RUSTFLAGS=-C target-cpu=x86-64` for x86_64 targets, compiling for the baseline x86-64 ISA (SSE2 only). Reported by @James-Butler2026.
- **web_fetch schema type array rejected by strict LLM providers (issue #55):** The `url` parameter used `type: ["string", "array"]` which is valid JSON Schema but rejected by OpenAI, GitHub Copilot, Google Gemini, and other providers that enforce strict function-calling validation, causing HTTP 400 `invalid_request_body` on every request. Fixed: the schema now uses `anyOf: [{"type":"string"},{"type":"array","items":{"type":"string"}}]` instead. Reported by @zos474.
- **musl detection false positive on Fedora (issue #53):** The ELF interpreter check in `npm/install.js` had a double-close bug: after finding PT_INTERP and closing the file descriptor, the `if (!isMusl) fs.closeSync(fd)` line tried to close it again, throwing EBADF. The catch block then fell back to the existence check for `/lib/ld-musl-x86_64.so.1`, which is present on Fedora systems that have the `musl` package installed. Fixed: the fd is now closed exactly once, and the fallback to the existence check only runs when the ELF interpreter could not be read at all. Reported by @yagaltd.
- **Bounded session CDP waits for Debian 12 Chromium 151 (PR #54, @Brandon168):** On Debian 12 containers with Chromium 151, session-scoped CDP responses (`Page.navigate`, `DOM.getDocument`, `DOM.getOuterHTML`) stall for ~26s during first-navigation settle, then flush together. The unbounded 20s default timeout turned this into a hard "no content" failure on every tier-2 fetch. Fixed: `Cdp::call_with_timeout` allows per-call timeout bounds. `outer_html` uses 3s/5s bounds per call (a bounded miss costs one poll iteration, not the whole render window). A warmup navigation to `https://example.com/` at launch (capped at 35s, tolerated on failure) absorbs the settle cost once per launch instead of on every fetch. The `Page.navigate` call in `navigate()` is capped at 8s, with the URL poll loop absorbing any residue.

## [3.2.3] - 2026-08-26

### Fixed

- **Links swallowed in nested formatting (issue #49):** `<em>`/`<strong>`/`<a>` inline rendering used `plain()`, which flattened nested children to bare text. `<em>A <strong><a>B</a></strong> C</em>` became `A B C`, dropping the bold and the link. Fixed: `a`, `strong`, and `em` now render children recursively, so nested formatting survives (`*A **[B](url)** C*`). Regression test added with all four issue cases.
- **tier-2 ghost navigation hang on Chrome 151/152 (issue #48):** Chrome for Testing 151/152 (observed on macOS arm64) has a bug where `Page.navigate`'s CDP response never dispatches even though the URL advances and the navigation commits. DonSeTch waited on that response and hit a 20s timeout on every tier-2 fetch. Fixed: navigate now dispatches and polls `current_url()` (browser-level `Target.getTargetInfo`, routed separately and still returns the advancing URL) until the target leaves `about:blank`. Works on both healthy and buggy Chrome.

## [3.2.2] - 2026-08-25

Process-leak hotfix: orphaned Chrome, profile collision, and a
fuzz-found byte-boundary panic in the JSON extractor.

### Fixed

- **Ghost browser leak (issue #43):** Chrome processes survived after the tool call returned on macOS because `Ghost` had no `Drop` impl and tokio's `Child` does not kill on drop. Added `impl Drop for Ghost` that calls `kill_group()` synchronously: the safety net that fires on every code path that drops a Ghost without an explicit `kill().await` (macOS `GhostGuard::Drop`, panic, CLI exit). Also set `kill_on_drop(true)` on the tokio Child as belt-and-suspenders.
- **Profile collision (issue #43):** concurrent donsetch processes launched Chrome against the same fixed `ghost-profile` dir, colliding on `SingletonLock` and surfacing a user-visible error dialog. Added an `flock`-based profile lock: if another process holds the lock, the caller falls back to a throwaway temp profile (no collision, no cookie warmth, the job still runs). The lock lives for the Ghost's lifetime. `SingletonLock` files are now only removed when we hold the lock.
- **`donsetch stop` command:** kills orphaned Chrome instances using the ghost profile and cleans up stale lock files + temp profiles. Use after a crash or when Chrome from a previous session is still resident.
- **Xvfb process leak:** `Xvfb` had no `Drop` impl, so it leaked if the GhostManager was dropped without calling `shutdown()` (panic, crash). Added `impl Drop for Xvfb` that calls `start_kill()` synchronously.
- **Fuzz crash in `find_blobs` (extract):** advancing `from` by the JSON value's string length instead of its end position in the source HTML could land mid-character on invalid UTF-8 (U+FFFD), panicking with "byte index is not a char boundary". Fixed: `extract_js_value` now returns the consumed byte position; `from` advances past the closing bracket (always ASCII, always a char boundary). Regression test added with the exact fuzz input.

## [3.2.1] - 2026-08-23

Hotfix: pi extension crashed with `write EPIPE` on Windows WSL when
the MCP server died mid-request. The extension wrote to the child's
stdin with no `'error'` listener; Node escalated the EPIPE to an
uncaughtException and killed the whole pi process.

### Fixed

- **pi-extension: stream `'error'` handlers on stdin/stdout/stderr**
  reject in-flight requests and drop the dead server instead of
  crashing pi with an uncaughtException. `sendNotification` writes
  are also guarded. Verified: loads the real extension, ran a real
  MCP round-trip, SIGKILLed the server mid-session, and survived the
  kill+rewrite window without crashing.

## [3.2.0] - 2026-08-23

The search-legibility release: signals the merge already computed now reach the text channel, where every client can read them.

### Changed

- **Snippets are 200 chars, cut on a word boundary.** 120 ended mid-word almost every time : `sequence transduc`, `All You Nee`, `the attention ` on a live query. The cost was not lost context but a wasted `web_fetch` to learn what the snippet nearly said. The cut trims back rather than extending, so output stays bounded by the budget; if the character past the window is whitespace the window already ends cleanly and is kept whole; backing off is abandoned when it would cost more than a fifth of the budget, since a long URL or an unbroken CJK run would otherwise strip the snippet to nothing. The ellipsis is appended only when text was actually dropped, so it never promises content that does not exist. Trailing marks that join clauses (`, ; : 、 ， 「 《 【`) are removed before it; sentence terminators (`. ! ? 。！？`, shared codepoints across Chinese and Japanese) stay, because a cut landing after one means the snippet ended on a complete sentence. (#40, @Mart-Bogdan)
- **Each result names the engines behind it**, with its blended score: `engines: bing, ddg · score: 0.83`. Which indexes agreed is what separates two equally plausible results : independent engines converging usually means canonical, a lone vertical hit often means tangential : and it was previously visible only in `structuredContent`. Names rather than a count: `consensus` there is `sources.len()`, which double-counts an engine that returned the URL at two ranks, while ranking counts index families. Deduped names are the honest version, and say *which* source. (#40, @Mart-Bogdan)

### Fixed

- **Whitespace in titles and snippets is collapsed at merge.** HTML-scraped engines normalized already; JSON-sourced hits did not : MDN summaries, BYOK provider snippets (Exa returns raw page text) and GitHub descriptions arrived with embedded newlines, breaking the three-space indent of the markdown list. Normalizing once in `rank::merge`, the single point every source flows through, also fixes the "longest snippet wins" and "shortest clean title wins" comparisons, which previously ranked on whitespace count: a newline-padded short snippet could beat a genuinely longer one. (#40, @Mart-Bogdan)
- **Clippy only ever linted Linux, and only lib and bins.** The ~32 `#[cfg(windows)]` sites : all of `ghost/proc.rs` : were never compiled by the lint pass, and neither were the 50 `#[cfg(test)]` modules or `tests/*.rs`, which the default lib+bins pass skips. Clippy now runs on Windows as well, with `--all-targets`. macOS stays out deliberately: its only exclusive site is one `target_os = "macos"` block, everything else being `unix` (shared with Linux) or `not(linux_like)` (shared with Windows), so Linux+Windows already covers it.
- **`rust-toolchain.toml` pins the local toolchain to 1.98**, matching the CI/release pin. v3.0.0 pinned the CI side to end local-vs-CI clippy drift, but nothing pinned the contributor's side, so a local clippy of a different version reports a different lint set : findings that CI does not have, and misses that it does.

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

The context-warfare release: six milestones : reference handles, budgets, probe and structure-first reading (M1); deadlines, real cancellation and ms-precision costs (M2); page fingerprints, deltas, Wayback resurrection and anti-cloak (M3); keyless domain adapters for reddit/npm/PyPI/crates/Go/RubyGems/GitHub/StackExchange/Wikipedia/docs frameworks (M4); search→fetch warm handoff, stitching and Chrome-parity TLS (M5); stable error codes, CI token/memory gates, a crash-only supervisor and the pi-agent v3 extension (M6). Plus a community fix for a Windows tier-1 boot hang (#36, @problaems).

The context-warfare milestone (v3 M1): every tool now respects the agent's context window as the scarce resource it is.

### Added

- **Reference handles (`L1`, `S1`)**: fetched-page links render as `[text](L12)` instead of raw URLs, and search results list `S1`-`Sn` instead of 80-token URLs. `fetch` accepts a handle anywhere it accepts a URL (`fetch S3` = result 3 of your last search). Handles are stable per URL (L) or per search position (S), persisted at `~/.cache/donsetch/handles.json` with a 24h TTL and 2048-entry cap. Raw URLs remain in `structuredContent` for citation.
- **Batch fetch with global token budget**: `url` now accepts an array (up to 12) : one parallel call instead of N round-trips. `budget_tokens` shares one output budget across all results, allocated by size (small pages stay whole, big ones slice with a resume note). Composed output carries per-URL status; only all-failed is an error.
- **Probe mode (`must_contain`)**: verification questions ("does the changelog mention CVE-2026-XXXX?") resolve the page fully but collapse the output to MATCH/NO-MATCH plus up to three short context excerpts (~60 tokens instead of 4k). Case-insensitive substring or `/regex/`.
- **TOC section IDs + sizes**: `toc=true` now renders `- [s3] Heading . 1.2k` : a stable per-section ID and content-size label. `section="s3"` targets by ID (heading-name matching still works). Read structure and cost before reading content.
- **Dropped-content manifest**: when `focus` removes blocks, the output gains one accounting line (`dropped by focus: 256 blocks (~12.1k words) : History, Early years, ...`). Omission is audited, never silent.
- **On-demand image OCR (`image_text=true`)**: fetches and OCRs the page's content images (up to 4, 5MB each, SSRF-guarded) and appends an `image text` section : infographics, comics and screenshot-locked pages become readable (the OCR engine ships with `--features ocr` builds; core builds say so honestly).
- Fuzz targets (`fuzz/`): `extract`, `charset`, `paginate`, `sitemap`, `feed` : the five panic-surface parsers, wired as CI smoke jobs with crash-artifact upload. The crate grew a library target (`src/lib.rs`) to support this; the binary is unchanged behavior.
- Supply-chain gate: `deny.toml` + cargo-deny CI job (advisories, licenses, bans, sources).
- `bench/tokens.py`: token-efficiency bench asserting the invariants (focus >=40% savings, probe <=400 chars, no raw-URL leaks past handle rewriting).

### Changed

- **Main-content scoring**: link density now discounts punctuation/paragraph mass too (a sidebar of link lists could outrank the real article on punctuation inside link labels), image `alt` text counts as content text, and structural region IDs (`footer`, `bottom`, `sidebar`, `nav`, ...) are excluded from main-content candidacy at any size. xkcd scoped to its sidebar before this; it now scopes to the comic.
- Media (`<img>`) elements are always segmented (cheap) and dropped at render time unless `media=true` : the image list must exist even when media lines are not rendered, so on-demand OCR works on any page.

### Fixed

- Comic/gallery pages (text-thin, image-rich) lost their content images when extraction fell back to raw text : fallbacks now carry the scoped image list through.
- CI and release workflows pin rustc 1.98 (was floating `stable`), ending local-vs-CI clippy drift.
### Added (M2 : the clock)

- **Deadline contracts (`deadline_ms`)**: fetch (single and batch, per-URL) and search accept a hard time budget (500ms-600s). On expiry: honest `deadline` error with a next_action that names the usual eater (browser escalation) : never a silent hang.
- **Real MCP cancellation**: `notifications/cancelled` now aborts in-flight work. Fetch/search drop via select (all persistent state was already written atomically); the crawl stops its workers gracefully through the existing stop-flag and persists its resume token : partial progress is never lost. Cancelled requests get no response, per spec.
- **Progress notifications**: requests carrying `_meta.progressToken` get `notifications/progress` beats : per-page during crawls ("12 pages, 34 queued", throttled to 2s) and per-URL during batch fetches.
- **Cost footer**: every fetch result's `[meta]` line and structuredContent carries `ms` : the agent sees what latency cost.
- Crawl stop reason `Cancelled` with its own next_action ("resume with the token above").

### Added (M3 : trust & memory)

- **Page fingerprints + change verdicts**: every completed fetch is fingerprinted (sha256 of the normalized full markdown, first 12 hex) and recorded in a persistent page history (`~/.cache/donsetch/page-history.json`, capped: 64KB text per URL, 4MB total, 512 URLs). The next fetch of the same URL stamps its verdict in `[meta]` : `changed (minor|changed|rewritten)` with an ago-seconds label : so a re-read after a hot edit is an informed decision, not a guess.
- **`since_last=true`**: collapses the fetch output to the verdict. Unchanged pages become one line ("unchanged since last fetch (300s ago) : fingerprint …"); changed pages return a section-level delta report (headings added/removed/changed, capped at 8) plus "refetch without since_last for full content". Re-watching a page costs ~30 tokens instead of 4k.
- **Archive resurrection (`archive=auto|only|off`)**: on a dead link (404/410/gone), `auto` transparently checks the Wayback Machine and, if a snapshot exists, returns it stamped `ARCHIVED COPY of <url> : snapshot <date> (<age> old)` with an honest age warning when the snapshot is stale. `only` goes to the archive directly; `off` preserves the raw error. Dead links stop being dead ends.
- **Anti-cloak equivalence check**: on domains known to serve decoy content to plain-HTTP clients (the wall registry), DonShadow's response is cross-checked against a headless render : text-similarity below the threshold appends a `decoy suspected` warning instead of confidently returning cloaked junk.
- **Freshness truth**: `structuredContent.server_modified` surfaces the server's own `Last-Modified` on successful fetches : cache-lie detection for the agent ("the page says 2024, the server says 2019").
- **Loud engine degradation**: search results from degraded engines carry a `*degraded: 3/5 engines ok (duckduckgo: timeout)*` line : silent quality collapse is visible in-band.
- **Delta crawl (`since_last=true`)**: crawl skips pages whose recorded fingerprint is still fresh (24h window), reporting each as `unchanged (since_last)` in the skipped list : re-crawling a site after an edit returns just what moved.

### Added (M4 : domain intelligence)

A keyless adapter registry for the sites agents actually hit. Fetch-level rewrites route page URLs to the site's own public JSON APIs (one plain-HTTP request for structured truth : often skipping the wall entirely); extract-level adapters restructure HTML the generic pipeline mangles. Every result is honestly labeled `via=adapter:…` in `[meta]` and structuredContent; any adapter miss falls back to the generic path, and an adapter failure (rate limit, login wall, non-JSON 200) transparently retries the ORIGINAL url through the full pipeline. Kill switch: `DONSETCH_NO_ADAPTERS=1`.

- **Reddit `.json`**: threads and subreddit listings fetched from the site's keyless JSON endpoints and rendered as comment trees with scores, ages, OP/sticky/NSFW flags and collapsed-reply counts; nested replies indented. Replaces the HTML scrape when available (an IP under Reddit's logged-out limit still gets the old.reddit/generic/ghost cascade).
- **Package registries**: npm, PyPI, crates.io, Go module proxy and RubyGems page URLs (e.g. `npmjs.com/package/react`) resolve to their JSON APIs and render one unified package card : description, current version, publish/update dates, license, repo, download counts, dependencies, deprecation/`DEPRECATED` warnings, yanked markers, and a recent-versions list that prefers stable releases over canaries. Version-specific URLs fetch the version manifest (crates.io version pages carry the dependency tree).
- **GitHub**: issue/PR lists, individual issues/PRs, releases and commits restructured from the server-rendered DOM (both the current React markup via stable `data-testid` hooks and the legacy markup). Issue lists: title, number, open/closed, author, date, labels. Issue threads: state, author, date, full body : plus an honest note that comments stream via JS (re-fetch with `tier=2` to read the discussion). No auth, no API rate jail.
- **Stack Exchange**: question + answers as a QA tree with per-post scores (from `data-score`), accepted-answer ✓ marking, asker/answerer authorship and asked-dates.
- **Wikipedia infoboxes**: the summary table (born/died/founded/license/versions…) becomes a clean `field | value` table at the top of the output, with the full article body (headings, paragraphs, data tables, lists) below : navbox/infobox duplication and citation markers stripped.
- **Docs frameworks**: mkdocs / Docusaurus / Sphinx / Antora sites (detected via generator meta or framework markers) prepend a compact `Site outline` built from the nav : the site map with cheap L-handle links : before the page content. Version-switcher noise filtered.
- `donsetch dev extract --url <url> --input <file>`: run the extraction pipeline on a saved HTML file against a URL (adapter development, fixture capture). `DONSETCH_ADAPTER_DUMP=<dir>` captures every body the adapters inspect.

### Added (M5 : speed & stealth)

- **Search→fetch warm handoff**: search enrichment already fetches the top results : that content is now cached (bounded: 10 bodies, 1.5MB each, 10min TTL) and the subsequent `web_fetch` of a result serves it instantly. `structuredContent.prewarmed_by_search: true`, tier reads `prewarmed` (the search→fetch second hop measured at ~3ms). One-shot: a second fetch goes to the wire for freshness; extraction, thin→ghost escalation and page history run unchanged on the cached body.
- **Route hints on search results**: domains the self-improving store knows need the browser are annotated in the results (`⚠ needs browser (~+6s)`) : the agent can pick a faster source or budget time before spending the fetch.
- **Article stitching (`stitch=true`)**: multi-page articles with rel=next pagination are walked (up to 6 parts, 48k budget, same-host only) and returned as ONE article with `*(part N)*` markers : an 8-part spread costs one call, not eight. `structuredContent.stitched` reports the part count.
- **h2 fingerprint parity gate**: DonShadow's h2 preface (SETTINGS values+order, connection WINDOW_UPDATE, pseudo-header order, no PRIORITY frames) is now asserted byte-identical to the Chromium capture in a CI test : any future divergence is a red build, not a silent detectability regression.
- **Locale-coherent Accept-Language**: the header now follows the target's locale (host TLD map + percent-encoded script in the path) : an en-US header on a .ru page gets the English stub on some sites and is a mild incoherence signal; localized sites now serve their real content. Default remains Chrome's en-US.

### Fixed

- **Daemon-abort panic in jsdata blob discovery (fuzzer find, CI fuzz gate)**: a known-global assignment (`__NUXT__ = `) matching at the very end of a page whose preceding byte was invalid UTF-8 (decoded to a 3-byte replacement char) advanced the scan cursor past the string / mid-character : `html[from..]` panicked. The cursor now floors to the next char boundary, clamped to the string length. Found by the new CI fuzz gate on its first green-config run; regression-tested with the crash input.
- **Windows tier-1 boot hang in the browser version probe (#36, @problaems)**: startup spawned a real browser (`--version --headless=new`) with no timeout to learn its version : on Chrome 129 the spawn hangs (crash-looping GPU/network services) and blocks every command at boot, leaving an orphaned process tree. The probe now reads the version from the browser's own registry key (`HKCU\Software\<Browser>\BLBeacon\version` : zero spawns, honours `DONGHOST_CHROME` families incl. Thorium/Edge) and hard-caps any spawned fallback at 3s with a whole-tree kill. Review follow-ups: non-Windows build stub, child cleanup on an early-out path, unit tests for the version parser.

### Added (M6 : foundation)

- **Stable error codes**: every error on all three tools carries a machine-readable `code` (`guard.ssrf`, `deadline.hit`, `network.dns`, `wall.challenge`, `wall.paywall`, `content.binary`, `crawl.resume`, `archive.stale`, `cloak.suspected`, …) alongside the prose and `next_action` : agents branch on codes, not string matching.
- **Token-efficiency CI gate**: the live claims (focus ≥40% savings, toc ≤5%, probe ≤2% of page, link rendering) are now asserted offline against saved real-page corpora on every build (`tests/token_invariants.rs`).
- **Memory soak gate**: 200 full-pipeline extractions + 10k handle churn + 800 page-history records with RSS growth asserted bounded (`tests/soak.rs`) : a creeping daemon is a build failure, not a surprise.
- **Crash-only supervisor**: `donsetch mcp --supervised` proxies stdio over a supervised child daemon : a panic-abort (or a SIGKILL) restarts the daemon (500ms backoff, 5-crash give-up), held requests are replayed, idle deaths are caught within 500ms, and the MCP session survives. Live-verified: SIGKILL mid-session, all requests answered after restart.
- **Homebrew tap**: `brew tap dondai44423/donsetch && brew install donsetch` (formula staged, published with the release).
- **Release workflow hardening**: release builds are `--locked` (deps can't drift mid-release); every platform binary must *report the tagged version* before packaging : a missed `Cargo.toml` bump fails the release job, not the user's `--version`; GitHub release notes are generated from `CHANGELOG.md` (curated) with commit-log notes appended, not the bare commit log.
- **pi agent extension v3**: tools now run under the crash-only supervisor (`mcp --supervised` : a SIGKILLed daemon no longer kills the pi session); pi's Esc/cancel forwards real MCP cancellation so server-side fetch/crawl work actually stops; tool cards surface v3 stable error codes (`[deadline.hit] …`) and `stitched ×N` pagination. Tool definitions are discovered live from the binary, so `pi update --extensions` picks up all of v3 with no extension-side pinning.

### Decision

- **HTTP/3: not in 3.0.0** (timeboxed spike concluded : see design notes): h3 fingerprinting is not yet a vendor signal, h2 fallback is first-class everywhere, and a second transport stack (quiche + duplicate BoringSSL) pre-3.0 trades proven reliability for an unmeasured signal. The bar to ship post-3.0 is documented.


## [2.5.0] - 2026-08-22

The polish & reliability release: one daemon-crashing charset bug fixed (#35), four panic-abort paths closed, one infinite hang capped, the error contract extended to every tool, and installation/upgrades hardened across platforms.

### Fixed

- **ghost-dom double-decoded browser text as GB18030 mojibake (#35)**: the headless-browser tier reads UTF-8 text from the live DOM via CDP : the browser already decoded the page. But the rendered DOM keeps the page's original `<meta charset=gb18030>` declaration, so the charset sniffer honored it and "decoded" the already-UTF-8 bytes a second time (末日乐园 → 鏈棩涔愐涯 on 69shuba). Browser-provided text is now pinned as UTF-8 (`GHOST_TEXT_CT`) at every extraction site (fetch ghost paths, actions, render cache, crawl ghost escalation). Raw HTTP bytes keep full detection : the v2.3.8 GBK/Big5/Shift-JIS fixes are untouched.

- **Daemon-abort panics (release builds run `panic=abort` : each of these was a one-request kill)**:
  - `js_unescape`: a literal backslash before a multi-byte UTF-8 character (hostile or sloppy page in a Next.js flight frame) advanced the cursor mid-character; the next string slice panicked. Copy the full character instead.
  - Pagination: unclamped `max_chars`/`offset` tool args wrapped `start + max_chars` below `start` (integer overflow) → slice panic. Now saturating arithmetic plus server-side clamps (`max_chars` 200..=1 MiB, `offset` ≤ 1e9).
  - Pagination resume: the 500-byte block-boundary search window could split a multi-byte character on CJK pages → slice panic. Window end is floored to a char boundary.
  - Ghost debug HTML dump could slice a multi-byte character at byte 1200.

- **Infinite hang**: `Cdp::connect` : the only unguarded network primitive in the ghost stack : could hang a tool call forever if the browser accepted TCP but stalled the WebSocket handshake. 10-second cap.

- **Unclamped action waits**: a `wait` step with `ms: 3600000` stalled the tool call for an hour with no cancellation path. Per-step waits cap at 30s, selector/text polls at 60s.

- **Crawl resume via CLI**: `donsetch crawl "" --resume <token>` errored with "url must be http(s)" before reaching the resume loader (the MCP path accepted it, the CLI didn't). Empty-URL resume-only invocation now works.

### Changed

- **Windows browser discovery** now probes Microsoft Edge install directories (often the only CDP-capable browser on a stock Windows box : its directory is never on PATH), per-user Chromium, and the Playwright cache. Ghost escalation, browser actions, and `doctor` work on default Windows installs.

- **macOS Intel (darwin-x64) supported end-to-end**: prebuilt binaries now build in CI (native `macos-15-intel` runner), `npm install` accepts the platform, and self-update maps it correctly. Core build (no OCR/rerank : `ort-sys` ships no prebuilt ONNX Runtime for Intel macOS; same trade-off as Linux ARM64).

- **npm install.js hardened**: musl (Alpine) systems are detected up front with an honest "glibc-linked binary will not run" error instead of a deferred cryptic spawn failure; `tar` presence is checked on Windows before downloading; stale/truncated leftover binaries (< 1 MiB) are re-fetched instead of shadowing a fresh install; extraction is verified before chmod.

- **Error contract extended to every tool**: `web_crawl` and `web_search` failures now return structured errors with escalation trace + `next_action` (crawl failures classified permanent vs transient : bad seed/expired token no longer masquerade as retryable); crawl ghost-escalation failures surface their reason (launch error, captcha, timeouts) in `skipped[]` instead of vanishing; SSRF / binary-content / extraction-failure errors carry `next_action`; zero-result searches suggest the available levers.

- **CLI exit codes honest**: `update`, `doctor`, and `rollback` exit 1 on failure (scripts gate on `$?`); bulk-fetch JSON mode no longer collapses walled/transient failures to the permanent exit code; signal exit code matches the received signal.

- **Search meta reports rerank state**: a silently-degraded cross-encoder (feature off / model failed to load) is now visible in `structuredContent.rerank` instead of stderr-only.

### Security / Reliability

- **Sitemap decompression bomb capped**: gzip sitemaps decompress through the same 64 MiB cap as every other path : a malicious `.xml.gz` could previously OOM the daemon via unbounded allocation.
- **HPACK hostile index 0**: `checked_sub` instead of unsigned wrap (protocol-violation byte from a hostile server).
- **MCP stdout write failures** now log and shut down instead of silently serving into a broken pipe while the client waits forever.
- **Update flow**: backup-copy failure warns before the atomic swap (rollback would otherwise be silently impossible); cookie-vault persist failure logs instead of silently dropping warm clearance state.
- **Key masking** (`donsetch keys list`) is char-boundary-safe for keys containing multi-byte characters.
- `fetch` validates URL parse up front : an unparseable URL can no longer flow through the pipeline with an empty host, poisoning domain profiles.
- `/tmp` literals replaced with `std::env::temp_dir()` (ghost screenshots, search debug dumps) : Windows-safe.
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

- **Crawl: auto-scope drift on multi-tenant hosts** : seeding a crawl at `docs.rs/tokio` (single-segment path) returned `None` from `auto_scope`, causing the crawler to explore the entire `docs.rs` sitemap instead of staying within `/tokio/`. Fixed: single-segment paths now scope to `/{segment}/*`. Before: 383 off-topic pages fetched (async-blocking-bridger, asm_block, etc.). After: 5 pages, all within `/tokio/`.

- **Crawl: focus filter false positives from compound terms** : `focus_match` tokenized `spawn_blocking` into `spawn` + `block`, then matched `block` against unrelated paths like `/ant-libp2p-allow-block-list/`. Fixed: compound terms (containing `_` or `-`) are matched as full substrings OR require ALL fragments to match. `spawn_blocking` must appear as `spawn_blocking` in the path, or both `spawn` AND `block` must be present.

- **Fetch: content density threshold too high** : lowered from 50KB to 20KB raw and 5000 to 3000 chars extracted. Sites like artstation (91KB raw, 866 chars, 0.9% density) now correctly escalate to tier 2. Sites like bilibili (24KB raw, 1476 chars, 6% density) are not flagged.

- **Fetch: ghost settle time increased to 4s** : 3s was not enough for some SPAs (crates.io occasionally settled at 8KB before hydration). 4s gives SvelteKit/React enough time to download, parse, and execute JS bundles.

- **Doctor: TLS fingerprint false warning** : `tls.peet.ws` being unreachable showed a warning in `donsetch doctor`. Changed to Pass: the TLS stack is active (used for every fetch); the external fingerprint service being down is not a DonSeTch issue.

- **Tests: crawl_cycles_terminate with root seed** : test was seeded at `/a` which auto-scoped to `/a/*`, preventing the root page from being fetched. Fixed: seed at `/` (root) so auto-scope returns `None` and all paths are in scope.

## [2.3.0] - 2026-08-17

### Fixed

- **False positive ContentOk on SPA shells** : pages that server-render their layout (navigation, sidebar, footer) but client-render the main content produced enough boilerplate text (> 800 chars) to pass the thin check. The tool returned this boilerplate as content without escalating to tier 2. Added content density check: if raw HTML is > 50KB and extracted text is < 5% of raw with < 5000 chars, the page is classified as a JS shell and triggers tier-2 escalation. Measured false positives: artstation (0.9% density), all caught. Real pages: 15-40%+ density, never triggered.

- **Ghost (tier 2) settles too early on SPA shells** : the ghost_fetch content-quality oracle settled after 2 stability polls (~400ms), before SPAs had time to hydrate and render their content. A stable 8KB DOM at 400ms is a SvelteKit/React shell, not a complete page. Added a minimum settle time of 3 seconds for DOMs < 50KB, giving SPAs time to download, parse, and execute their JS bundles. Large DOMs (>= 50KB) settle fast as before. Fixed: crates.io (SvelteKit, was 8KB shell, now 47KB full render), users.rust-lang.org (Discourse, was intermittent 30KB shell, now consistent 397KB full render).

- **Pi extension TUI: truncateToWidth ANSI leak** : pi-tui's `truncateToWidth` function injects `\x1b[0m` RESET codes around the ellipsis even when the input is plain text. These RESET codes broke pi's green/red tool-call overlay mid-line, causing text to fall outside the highlight. Replaced all `truncateToWidth` calls with a local `truncate()` function that adds zero ANSI codes.

## [2.2.4] - 2026-08-17

### Fixed

- **Pi extension TUI: visual glitch fixed** : stripped ALL ANSI color codes from renderCall and renderResult. Plain text only. Pi wraps tool calls in its own green (success) / red (failure) highlight; our ANSI RESET codes were breaking pi's overlay mid-line, causing text to fall outside the highlight and show with the TUI background color.

## [2.2.3] - 2026-08-17

### Fixed

- **Pi extension TUI: removed all green/red ANSI from renderResult** : pi handles success (green) and failure (red) coloring itself. Our own green/red codes bled into pi's highlight causing a visual glitch. renderResult now outputs only amber (tool name) and dim (metadata).

## [2.2.2] - 2026-08-17

### Changed

- **Pi extension TUI: provider + cache display** : search results now show the provider (`via local`, `via exa`, `via tavily`), so the agent and user can see which engine was used. Fetch results show `via cache` when warm cookies were used (not a fresh fetch) and `via ghost` when the browser escalated.
- **Pi extension TUI: success/fail coloring fix** : removed all green and red ANSI codes from renderResult. Pi's TUI already wraps successful tool calls in green and failures in red; our own green/red codes bled into pi's highlight causing a visual glitch. renderResult now outputs only amber (tool name) and dim (metadata) : pi handles the success/fail coloring.

## [2.2.1] - 2026-08-17

### Changed

- **Crawl auto-scope** : when `include_paths` is empty, the crawl now auto-derives a path scope from the seed URL's path. `docs.rs/tokio/latest/tokio/` stays within `/tokio/latest/tokio/*`; `github.com/tokio-rs/tokio/wiki` stays within `/tokio-rs/tokio/*`. Multi-tenant sites (docs.rs, github.com) and multi-section sites (stripe.com, nextjs.org) no longer escape the seed's section. The user no longer needs to manually set `include_paths` for the common case.
- **Focus filtering on all link discovery paths** : when a `focus` query is set, links with zero focus-token matches are now filtered from BFS outlinks, pagination `<link rel="next">`, RSS/Atom feed entries, and sitemap frontier seeding. Previously only the sitemap map display was focus-filtered; all discovered links were enqueued regardless of relevance. The filter uses a smart soft/hard approach: if the current page has any matching links, non-matching links are hard-filtered (only relevant pages crawled). If no links match (e.g., a homepage linking to a tutorial that links to the target content), non-matching links are soft-filtered (enqueued at low priority) to enable multi-hop discovery.
- **Junk path filtering** : common non-content paths (`/login*`, `/signin*`, `/signup*`, `/register*`, `/auth*`, `/oauth*`, `/account*`, `/settings*`, `/cart*`, `/checkout*`, `/favicon*`) are now excluded by default, merged with user-specified `exclude_paths`.
- **Faster crawl pacing** : base inter-request delay reduced from 300ms to 200ms; skim dwell cap reduced from 300ms to 100ms. Roughly 2x faster crawls with zero observed throttling on test sites.

### Fixed

- **Sitemap focus filter bug** : the sitemap filter used `score <= 0.0` which incorrectly filtered deep but relevant pages (depth_prior made the total score negative even with a focus token match). Replaced with `focus_match()` which checks for any token match regardless of depth.
- **Sitemap seeding not focus-filtered** : sitemap entries were seeded into the frontier without focus filtering (only the map display was filtered). Now all sitemap-seeded entries pass the focus gate.

### Added

- **`next_action` in crawl output** : when the crawl returns 0 pages or stops early, the structured output now includes a `next_action` field with actionable guidance: "use mode=content", "try broader include_paths", "the site blocked the crawler", "resume={token} to continue", etc.

## [2.2.0] - 2026-08-17

### Fixed

**Reliability: the self-improving fetch loop actually self-improves now.** Four compounding bugs made ghost-solved domains re-need the ghost forever and occasionally served bot-wall pages as content:

- **Fake solves** : the tier-2 oracle settled on modern Cloudflare interstitials ("Performing security verification", ~344 visible chars of vendor boilerplate) and recorded them as solved, then replay-served the wall page as `ContentOk`. New interstitial detection layer (title/H1 boilerplate + near-empty-DOM-with-challenge-markers shapes) runs before the visible-text override in `detect_dom_smart` and `detect`. The ghost now waits for real clears.
- **Learning was gated off on re-solves** : a `skip-to-solve` re-fetch (cookies past their TTL) never called `record_solved` because `learn` required a fresh tier-1 challenge, so expired domains went ghost-first forever. Learning now fires on every wall-driven escalation. Live-verified: solve once → next fetch rides warm tier 1 in ~0.4s.
- **State poisoning** : ANY non-content verdict (404, 429, paywall, auth wall) marked domains `needs_tier2`, forcing a 20s ghost launch on every later fetch of that domain. Only real `Challenge` verdicts set the flag now; terminal verdicts move counters only. One-time migration un-poisons existing profiles that never recorded a solve (144 → 15 in the dev state file).
- **Warm-stale over-learning** : a single walled warm fetch (often transient challenge rotation) cleared the cookie vault and clamped `observed_lifetime` to as low as 1 second (the live stackoverflow case), killing warm routing permanently. Two consecutive failures are now required, and the learned lifetime is floored at 120s.
- **`replay_ok` gating** : warm routing now requires the post-solve tier-1 retry to have VERIFIED that these cookies actually work on tier 1 (some vendors bind clearance to the browser fingerprint; replay is impossible there). Unverifiable cookies never earn a doomed warm roundtrip again.
- **Ghost 404 laundering** : on skip-to-solve routes the ghost happily rendered 404 pages (browsers do) and the pipeline served them as `ContentOk`. The post-solve tier-1 retry is now the oracle of record for terminal verdicts (404/paywall/auth): dead URLs return honest errors.
- **Version coherence** : tier 1 claimed Chrome 150 headers while the ghost ran the installed Chromium 151 (client hints advertise the real version even under `--user-agent`). The installed browser's major version is now probed at startup and both tiers advertise the same coherent identity : clearance cookies bind to it.

**DonSift content fidelity** (the agent-reported gaps):

- **Math is no longer destroyed.** `<math>` elements are recovered as LaTeX: MediaWiki `alttext` first (with the `{\displaystyle}` wrapper stripped), then `<annotation encoding="application/x-tex">`, then a compact MathML serialization (`W_{Q}^{T}`, `(QK^{T})/(sqrt(d_{k}))`, matrices as `(a, b; c, d)`). Hidden-math exception: `display:none`/`aria-hidden` wrappers around `<math>` (the a11y twin of rendered formula images : MediaWiki, MathJax, KaTeX shape) are extracted instead of skipped. Live-verified on the attention-paper Wikipedia page: every formula and matrix variable renders. `<sup>`/`<sub>` content is preserved as `^{...}`/`_{...}` (only citation markers like `[1]` are dropped).
- **Discussion threads are no longer lossy.** Hacker News gets a dedicated extractor (threads AND the 2026 comment-permalink layout): full comment text (was: table cells truncated at 120 chars / entire subtrees dropped), authors, ages, reply depth via indentation, story header with points. Generic fix for other forums: layout/prose tables (any cell ≥300 chars, single-column tables, `role="presentation"`) are walked as containers instead of rendered as pipe tables; `class="comment"` is no longer treated as boilerplate (it silently removed whole comment sections from scoring).
- **Feeds render as feeds, not raw XML.** RSS 2.0 / Atom / JSON Feed → structured markdown: channel header, items with linked titles, dates, HTML-stripped summaries (was: 25KB CDATA blob). Handles lying Content-Types (`text/xml`, `text/plain`) by payload sniffing, and the HTML-parser traps (`<link>` void-element mangling, CDATA leakage) via preprocessing.
- **Thin-hole closed** : a 27KB page extracting 250 chars over 3+ boilerplate blocks was classified non-thin (how challenge pages leaked through). Any page over 5KB yielding <800 chars is thin now.
- **HTML served as `text/plain`** is parsed as HTML instead of passing through as angle-bracket soup.
- **`tokens_est` is honest** : dedicated extractors reported full-document token counts instead of the returned slice's.

**Fetch and escalation:**

- **`/pdf/` path convention honored everywhere** : `arxiv.org/pdf/1706.03762` previously skipped PDF early-detection (only `.pdf` suffix counted), escalating to a 23s ghost roundtrip; now routed straight to DonSheet (0.7s, tier 1).
- **Walls never enter the revalidation cache** : a challenge interstitial carrying an ETag was re-served fresh as content on later fetches; fresh-cache hits also get honest verdicts now instead of hardcoded `ContentOk`.
- **Warm cookies are no longer killed by extraction gaps** : a warm `ContentOk` that extracts thin is only treated as a shell when the body is big with almost no visible text (real shell evidence); rich-visible-text pages with thin extraction keep their valid cookies.
- **Turnstile clicks retry** : the checkbox iframe renders late and repositions; the old one-shot click usually fired before it attached. Up to 3 attempts, re-finding geometry each time. (Interactive captchas remain an honest dead end by design.)
- **Section slices no longer trigger ghost escalation** : a small `section=` result on a huge page computed as "thin" (shell) and escalated to the browser, which returned the FULL page instead of the requested section. A matched section is intentionally small; shell detection is skipped for it.
- **Math brace fidelity** : the `\displaystyle` wrapper strip removed exactly one closing brace per formula (`W_{Q}` stayed intact; the previous `trim_end_matches` ate inner braces).
- **HN threads honor `focus`** : relevant comments surface on 700-comment threads (with the standard no-match notice); previously the dedicated extractor ignored the query and returned the first N comments.
- **Legacy lifetime de-poisoning** : pre-fix `observed_lifetime` values below the 120s floor are dropped at load AND on each new solve; stackoverflow (clamped to 1s by the old bug) rides warm tier 1 again.

### Added

- **Crawl explains its pace** : when a site's robots.txt declares `Crawl-delay` and it's honored, the crawl output says so (`robots crawl-delay: 30s between requests (site-declared; pass respect_robots=false to override)`) plus `crawl_delay` in structuredContent. A slow crawl is no longer a mystery.
- **Feed extraction surface** : feed URLs return `content_kind: Listing` with item counts in `blocks_total`/`blocks_shown`.

## [2.1.2] - 2026-08-16

### Added

- **Pi agent TUI rendering** : custom `renderCall` and `renderResult` for all 3 tools in the pi extension. Tool calls show a clean amber icon + tool name + key arg (URL or query). Results show a compact status line (✓/✗ glyph, tool name, metadata) plus a one-line preview. No more raw content dumps in the TUI : the LLM still gets full content, the user sees a clean summary card. Amber theme matching DonSeTch's identity (#ffb200).

## [2.1.1] - 2026-08-16

### Added

- **Pi agent support** : `pi install npm:donsetch` now works natively. The npm package ships a pi extension that spawns the donsetch MCP binary at session start, discovers tools dynamically via `tools/list`, and registers them as native pi tools. Zero configuration, zero maintenance : tool definitions are fetched from the binary, so they stay in sync automatically. If the binary is missing (e.g. npm blocked postinstall), the extension auto-downloads it from GitHub Releases.
- **Tool-def token optimization** : cut 203 tokens of duplicated/redundant text from MCP tool descriptions (2,566 → 2,363 tokens, measured with tiktoken/GPT-4o). No quality loss : all behavior guidance preserved.

## [2.1.0] - 2026-08-16

### Added

- **`donsetch status`** : one-glance overview: version + update check, search config (providers, keys, default mode), proxies count, cache size, and health hint. No probes, no browser launch : fast. The "I just installed it, what's the state?" command.
- **`donsetch help <command>`** : route to any command's help: `donsetch help keys`, `donsetch help proxy`, `donsetch help fetch`, etc. Falls back to top-level help for unknown commands.
- **`donsetch keys default local`** : set the local keyless search engine as the default search method, even when BYOK provider keys are configured. When local is the default, the local 5-engine search is tried first and BYOK keys are only used as fallback if local search fails. This lets users test or use the local engine without removing their keys. `donsetch keys default <provider>` switches back to BYOK-first mode.
- **`donsetch keys export [path|-]`** : export all BYOK keys and config to a file (with 0600 permissions) or stdout (with `-`). Useful for backup, transfer between machines, or dotfiles repos.
- **`donsetch keys import <path>`** : import a config from a file previously exported by `keys export`. Replaces the current config entirely. Validates structure (provider names, key states, default) before saving.
- **`donsetch keys clear`** : remove all keys and reset to a clean state. The nuclear option for starting fresh.

### Fixed

- **Proxy missing from top-level help** : `proxy` command was not listed in `donsetch --help`, making it undiscoverable. Now shown in the MANAGEMENT section alongside `keys`, `doctor`, `update`, etc.
- **`proxy remove` now accepts numeric indices** : `proxy list` displays proxies as `1, 2, 3, ...` but `proxy remove` only accepted `host:port` or full URLs. Now `donsetch proxy remove 1` works. Handles multiple indices (`remove 1 3 5`) with correct order-of-operations (collects all first, removes in reverse to avoid index shifting). Backward compatible with `host:port` and full URL arguments.

## [2.0.0] - 2026-08-16

The v2 quality jump : a direct response to the 50-case
DonSeTch-vs-Hound comparison. Search top-1 decisiveness, browser
actions inside fetch, honest telemetry on every result, crawl
elastic pacing, and a browser path that's boring to install.

### Added

- **Browser actions in `web_fetch`** : page control inside fetch: `actions=[{...}]` runs click / type / press / scroll / hover / wait steps in the headless browser BEFORE extraction. Deterministic waits (`wait_selector`, `wait_text`), element addressing by CSS selector or visible text, human-cadence typing (log-normal key gaps, think-pauses), trusted CDP input events with bezier mouse paths. Up to 16 steps, validated before any browser time is spent. After the script, the normal extraction pipeline runs (focus/section/toc apply to the interacted page). Per-step results in `structuredContent.actions`; the first failing step aborts honestly with everything that succeeded. Form submits, search flows, load-more, lazy-load scrolls : one call, no separate browser tool.
- **Authority-aware search ranking** : the decisive top-placement layer. v1 had top-5 recall (23/25) but weak top-1 placement (6/25 vs hound's 13/25); v2 measures **29/30 top-1, 30/30 top-3** on the 30-query regression suite (`bench/regression.py`). Query-aware official-domain registry (~130 tech entries), title entity-term coverage with exact-phrase bonus, docs-seeking amplification, paper-repository authority for research queries, and news freshness ranking (the `published` field was dead data in v1 : it ranks now).
- **Escalation trace** : every fetch result (success AND error) carries `structuredContent.escalation`: the ordered steps actually taken (route decision → HTTP fetch → browser launch → ghost render → cookie retry → fallbacks) with per-step latency. A 3-second fetch is no longer opaque.
- **Structured error contract** : errors now carry `structuredContent {url, status, verdict, next_action, escalation}`. `next_action` is a one-line instruction derived from the failure kind (retry with tier=2, wait 30-60s, needs credentials, use an interactive browser). The CLI JSON envelope surfaces it too.
- **New success fields** : `content_ok` (true content, not a JS shell), `quality` (0-1 content trust, previously computed but never surfaced), `lang`.
- **PDF per-page stats** : `structuredContent.pdf = {pages, per_page: [{page, chars, ocr, confidence}]}`: per-page extraction confidence (glyph trust for text pages, OCR mean confidence for scanned pages), page boundaries preserved where block merging deliberately flows text across pages.
- **Doctor browser proof** : doctor now checks Xvfb (with :99 reuse detection), performs a REAL browser launch through the exact tier-2 code path with the fingerprint selftest (webdriver=false verified, 40s bound), verifies ghost-state.json permissions (auto-tightens to 0600), and reports the rerank model cache. 13 checks total (was 9). All new paths are platform-neutral (macOS/Windows report Xvfb as not-needed and use off-screen headful).
- **Search regression suite** : `bench/regression.py`: 30 queries with canonical domains defined upfront, measuring hit@1/3/5. The report's bar (official/primary in top-3 for ≥80% of tech-doc queries) passes at 100%.

### Fixed

- **arXiv PDF false "blocked"** (from the 50-case report): wall detection marker-scanned PDF bytes as lossy text : a Cloudflare-fronted paper containing "attention required" plus a cf-ray header produced a Blocked verdict at HTTP 200. Binary bodies (PDFs, images, archives) are now exempt from HTML marker scanning on 2xx; bot walls speak HTML. Non-2xx still classifies normally.
- **Cloudflare "Enable JavaScript and cookies to continue" shells** (report: "do not call a response successful when it only contains…") are now Challenge, never success.
- **Crawl latency** (report: 6.29s median vs 0.45s): v1 slept ~2.7s/page (700ms pace + up to 2s anti-metronome dwell) plus serial sitemap probes. v2 elastic pacing: 300ms base pace, skim-model dwell (≤300ms), sitemap candidates probed in one parallel wave on miss, reactive escalation ladder unchanged (throttle/latency signals still back off aggressively). 5-page docs crawl now ~3.5s wall including extraction.
- **Domain-profile poisoning from browser fetches**: cookie write-back in the actions path no longer marks never-walled domains as needs_tier2 (the v1.1 reddit-poisoning bug class, caught in live testing).
- Actions on PDF-shaped URLs (`.pdf` suffix or `/pdf/` path segment) are rejected up front with a clear message instead of burning a browser launch on Chrome's PDF-viewer JS shell.

### Changed

- Search enrichment now prefetches the top 5 results (was 3) : parallel with a 4s cap each, so real page titles/descriptions feed the final ordering at no wall-clock cost.
- Crawl sitemap child-index recursion is wave-parallel (bounds of 8) instead of serial.

## [1.2.0] - 2026-08-16

Security hardening : full audit by GLM 5.3 found 8 live-proven
vulnerabilities. All patched, PoC-verified against the release binary.

### Security

- **SSRF: DNS pinning** : hostnames resolving to private/loopback addresses are now blocked at the transport layer (post-resolution IP check, TOCTOU-safe). Previously only literal IPs were checked, so `127-0-0-1.nip.io` or any rebinding DNS reached loopback and cloud metadata endpoints. Escape hatch: `DONSETCH_ALLOW_PRIVATE_EGRESS=1`.
- **SSRF: redirect re-check** : every redirect hop is now checked with the SSRF guard before following. Previously the guard ran once on the initial URL; a public URL redirecting into a private network bypassed it.
- **SSRF: crawl guard** : `web_crawl` now checks the seed URL with the SSRF guard (same as `web_fetch`). Previously crawl had no guard at all.
- **Decompression bomb** : all decompression codecs (br/gzip/deflate/zstd) and identity bodies are now capped at 64 MiB. A 500 KB gzip body expanding to 512 MB previously caused unbounded memory growth; now returns a clean error.
- **h2 memory DoS** : three amplifiers fixed in the custom HTTP/2 stack: CONTINUATION flood capped at 256 KiB header blocks, frame size cap reduced from 16 MiB to 1 MiB, HPACK dynamic-table size updates rejected above 64 KiB (Chrome's advertised max). Response bodies capped at 64 MiB.
- **Cookie tossing** : `Domain=` attribute now validated per RFC 6265 §5.3.6: accepted only when it equals the request host or is a parent suffix. Previously any origin could pin cookies on any victim domain.
- **Expired cookie replay** : `header_for` and `snapshot_for` now filter expired cookies; `purge_expired()` runs after every store. Previously expired cookies were replayed indefinitely.
- **CRLF request splitting** : h2 header values with CR/LF/NUL are now rejected at decode time (RFC 9113 §8.2.2). The cookie jar rejects control characters at store time. Outgoing headers are validated in both `fetch_once_via` and `h1::get` before any wire write. Previously a crafted h2 `set-cookie` with embedded CRLF could inject arbitrary headers into later h1 requests.

### Fixed

- h1 response bodies now capped (content-length, chunked, read-to-close) : a lying Content-Length or an endless chunked stream previously caused unbounded allocation. Chunk-size arithmetic overflow also capped.
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

- Hybrid BM25 + cross-encoder semantic focus filter for `web_fetch`. The `focus` parameter now uses keyword matching (BM25) as the base, then if the cross-encoder model is already cached (from search reranking), runs a second pass and adds semantically relevant blocks that BM25 missed. Catches blocks where the query uses different vocabulary than the page (e.g. query "how gradients flow through layers" matches "backpropagation" and "chain rule"). No model download is triggered during fetch : only uses the model if already cached.
- `cross_encoder_scores` and `is_model_cached` exposed from the rerank module for reuse by the focus filter.

### Changed

- `focus` parameter description strengthened to drive agent adoption: explains the 50-80% token reduction, hybrid matching, concrete example, and ends with a directive to always set focus when you know what you're looking for.
- `web_fetch` tool description updated with a prominent "Token efficiency : use focus" section.
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

- **CLI**: full command-line interface : `fetch`, `search`, `crawl` with same engine as MCP.
  - `--json` for machine-readable output, `-q` for quiet mode, `--tier` for manual escalation control.
  - `keys` subcommand: manage BYOK search provider keys (`add`, `remove`, `list`, `default`, `reset`).
  - `doctor`: 9-check health diagnostics with auto-fix.
  - `update`: self-update from GitHub Releases (no API rate limits).
  - `rollback`: revert to previous version.
  - `version`: version + build info.
  - `tools`: print tool schemas as JSON (same as MCP `tools/list`).

- **BYOK search providers**: external search providers (TinyFish, Tavily, Serper, Exa) bypass the local engine entirely. Key stacking, rotation, rate-limit cooldown (60s auto-recovery), credit-depletion detection, local fallback. Config: `~/.cache/donsetch/byok-keys.json`.

- **Query-entity coverage penalty**: anchor entities (hyphenated compounds like "B-tree") and specifiers (version numbers, years) checked against results. Wrong entity = 0.3× score penalty. Fixes BM25 splitting "B-tree" → "b" + "tree" where "binary tree" matches. Universal : no-op for queries without entities.

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

- Interactive captchas (hCaptcha, reCAPTCHA, Turnstile checkbox) are not solved : no solving service by design.
- ML-DSA post-quantum signatures not yet supported (BoringSSL 5.1.0 lacks them).
- `outerWidth/Height` in headless: protocol-level override only.
- Windows/macOS PDF subsystem compiled but CI verification pending.
