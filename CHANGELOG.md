# Changelog

All notable changes to DonSeTch are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- A unit test no longer reads the developer's own `donsetch.toml`
  (#305, the config side of #299): the file layer under test is empty
  unless `DONSETCH_CONFIG` names a file, so a local `[state]
  no_disk_state = true` can no longer flip the governor's persistence,
  or any other key a test's behaviour, from outside the test.

## [4.3.4] - 2026-09-25

### Fixed

- `donsetch.exe` crashed at start, with no output, on CPUs without AVX
  (#277: a first-generation Core i7). ONNX Runtime was linked
  statically, and its global constructors run AVX instructions before
  `main`, so the crash hit every command. Windows now loads ONNX
  Runtime the way Linux does: at runtime, from Microsoft's
  `onnxruntime.dll` (CPU-only build, pinned by version and sha256 in
  `build.rs`) shipped beside the exe; the hard `DirectML.dll` import
  (a static-link leftover that stopped the exe from starting on Server
  Core and Windows 10 before 1903) is gone, and the self-updater
  carries `onnxruntime.dll` across updates the same way as
  `pdfium.dll`. (mnaza, #298)
- OCR and rerank were disabled on any x86-64 CPU without AVX (#277: a
  Celeron J1900) by a CPUID gate written for pyke's static ONNX
  archive, whose constructors do need AVX. The runtime the dlopen
  targets ship is Microsoft's own build, which selects its kernels at
  runtime: it loads and OCRs a scanned PDF on an SSE4.2-only CPU
  (verified under QEMU; Windows under Intel SDE). The gate is gone;
  `doctor` names the CPU class and proves the library loads, and the
  release workflow checks the runtime on a non-AVX CPU. (mnaza, #300)
- The test suite no longer reads or writes the user's real cache
  directory: a crawl test's governor loaded
  `~/.cache/donsetch/crawl-governor.json`, saved its mock hosts
  there, and under nextest's process-per-test the governor tests read
  each other's throttle state back, failing on a loaded box depending
  on which process wrote last. A unit test without
  `DONSETCH_CACHE_DIR` gets a temp root of its own
  (`$TMP/donsetch-test/<id>/cache`, swept at exit on every platform),
  and the daemon-boot integration test stops handing its child the
  real cache. (mnaza, #299)
- The BYOK search degraded line names the reason when no key could
  even be tried: with every key invalid, depleted or cooling down (and
  every plugin striking) the status read `(byok: )` with no word in
  it. It reads `no usable key (all invalid or depleted or cooling
  down)` now, and `compact_failure` never returns an empty status.
  (mnaza, #301)
- A fetch no longer falls through to the real address while an
  unburned lane exists. Probation after one block seats its lane again
  (it scored 0, and the seat check `score > best_score` with
  best_score 0 never let it through, so a one-lane pool went direct
  after a single 429); a burned pair rests its cooldown instead of
  being re-seated on every fetch of that host (re-seating voided the
  cooldown and held a rate-limiting lane under constant requests); a
  live lane bound to another host's persona serves before direct
  (exclusivity is preferred, not absolute, and a shared exit is less
  identifying than the real address); and a `direct` answer from the
  pool no longer mutes `HTTPS_PROXY`. (mnaza, #302)
- The docs-outline adapter walks a wrapper div's leaf divs in linear
  time again: asking every candidate's every ancestor whether a block
  lives below it re-scanned the wrapper's subtree once per leaf,
  quadratic in the leaf count (20 000 leaf divs, a 240 KB page, ran
  for minutes). The set of elements with a block descendant is built
  once, in one pass. (mnaza, #303)
- The docs-outline nesting check no longer walks every ancestor of
  every candidate: "nested" is now one pre-order walk's own
  bookkeeping, descending only through wrapper divs and non-candidate
  containers and skipping the subtree of anything it emits. 16 000
  leaf divs under 4 000 wrapper divs went from 24.4 s (over the test
  bound) to 1.1 s on the fast profile (#304).

## [4.3.3] - 2026-09-24

### Changed

- The README sponsors section is two sections now: "Our Sponsors"
  holds the placements, starting with Fluxion AI (Gold, banner
  pinned at the top), and "Sponsor this project" carries the
  tiers and the ask.
- CI keeps the compiled tree warm across dependency bumps: `target/`
  has its own cache keyed on the target, the build kind, the
  compiler and a version-insensitive dependency hash, a restore
  falls back to the previous entry, a red run saves an unswept
  `-partial-` entry the next run resumes from, and a
  fingerprint-based sweep before each save drops only what the run
  did not use. Pull requests and tags restore but never save, so
  release builds stop filling the 10 GB pool with entries they can
  never read (Mart-Bogdan, #297).

### Fixed

- pkg.go.dev URLs that pin a version (`/module@v1.2.3`) map to the
  Go proxy's pinned `.info` endpoint now. The `@` used to ride along
  into the `@latest` rewrite, which the proxy read as part of the
  module name: every pinned page cost a 404, a fallback and a second
  fetch of the page itself (1.9s of detour on a live probe; the
  pinned card answers in one request, 247ms).
- crates.io `/crates/<name>/versions` renders the versions card
  (newest release, dates, yanked flags, total count) instead of
  dumping the endpoint's raw JSON: a live page went from 16 KB of
  JSON to a 500-char card.
- Wikipedia articles whose titles contain a colon (`Star Trek: The
  Original Series`) get the infobox treatment again. The old blanket
  colon check read any colon as a namespace prefix and skipped the
  adapter; namespace pages (File:, Talk:, Special:, ...) still skip.
  Live: the same page lost 4k chars of spilled table markup and
  gained the clean field list.
- Stack Exchange host matching is label-aware now: `meta.*`
  subdomains and `mathoverflow.net` are covered, and look-alikes
  (`notstackoverflow.com`) are not.
- The reddit extractors (JSON adapter and HTML) no longer claim
  look-alike domains: `reddit.com` or `*.reddit.com`, nothing else.
- GitHub releases and commits pages fire the adapter again. The
  commits URL arm takes the ref segment (`/commits/master`, the
  canonical link), the releases selector follows the 2025+
  server-rendered markup, and the commits renderer reads the React
  rows (data-testid hooks, sha from `data-commit-link`, dates from
  the per-group titles, which are the only server-rendered dates).
  Live: a 30-commit page came back as a titled list with authors,
  dates and short shas, a releases page as `## vX.Y.Z : date` plus
  its notes. The legacy markup stays as fallback.
- Adapter JSON endpoints no longer dump raw JSON when focus, toc or
  section is set: the card is served instead. The generic cut
  machinery runs on HTML blocks, a JSON payload has none, and the
  old bail fell through to the raw-JSON passthrough, silently
  dropping the cut (a `crates.io/crates/serde --focus downloads`
  fetch returned 16 KB of JSON). `must_contain` still runs as a
  probe.

- Reddit renders one clean card for every URL shape, on both
  paths. The shreddit SSR adapter is new: threads with the full
  comment tree (scores, ages, nesting, hidden-reply counts), the
  post body, listings with the server-rendered posts and an
  honest feed-cut note, subreddit about pages with rules, wiki
  pages with the whole document and its revision stamp, all
  through the same formatting helpers the `.json` adapter uses.
  Where the `.json` endpoint refuses, the page path now lands in
  the same cards instead of generic chrome: a live thread went
  from 1546 chars of navigation text to a 6437-char card with 25
  of 39 comments.
- Reddit's session init is a first-class step now, not an accident
  of a failed `.json` hop: a reddit page that refuses at tier 1
  (the humanity interstitial, the JS shell) gets one legacy-host
  navigation and one retry, on tier 1 and auto alike. A live wiki
  page (which has no `.json` at all) went from a bot wall to the
  full document on a cold jar.
- Reddit URL coverage: user pages (`/user/<name>` -> profile card,
  `/comments` and `/submitted` -> activity cards), subreddit about
  pages (`/about`, `/about/rules`), and the legacy hosts:
  `old.reddit.com`/`np.reddit.com` content URLs rewrite onto www
  (they serve a login wall to anonymous clients) and fallback
  retries ride the content host. Wiki pages and share links (`/s/`)
  stay pages (no JSON shape); `redd.it` shortlinks resolve through
  the redirect into the thread card.
- A repeated block was replaced by a marker whenever the marker came
  out one character shorter, and the marker named nothing. Sites
  that wrap every LINE in its own element (lyrics, verse,
  subtitles, transcripts) hand the extractor blocks barely longer
  than the marker, so a repeated stanza came back as a run of
  anonymous `*[repeated block omitted]*` lines: on one such page 13
  markers stood in for 12 of the 32 lines. A marker must now be two
  thirds of the block's length or less, so those lines print
  verbatim (32 of 32), and a marker that does stand in names its
  source: `*[repeated block omitted, same as block 16: "..."]*`,
  with the ordinal identifying the copy and the quoted opening what
  a human reads. Blocks big enough for collapsing to pay are
  unaffected (Mart-Bogdan, #294).
- The parent-death signal a browser or Xvfb child arms in
  `pre_exec` covers a parent that dies after the child started; a
  parent that died between the fork and the `prctl` left a child
  that never got it. The child now verifies it is still its
  parent's child after arming and fails the spawn instead of
  leaking an orphan, and the killed-parent path has a test (mnaza,
  #295).
- The egress pool no longer retires a proxy lane for a certificate
  problem at the origin. Any TLS failure read as a dead lane, so
  fetching a self-signed, expired or hostname-mismatched site
  through a pool lane benched that lane globally for 10 minutes:
  two or three such probes degraded the whole pool to direct.
  Certificate-verify failures now leave lane health alone and move
  the HOST off the lane (pair probation + rotation), so a lane that
  intercepts TLS cannot pin a host; egress-flavored TLS failures
  (handshake reset or cut short) still bench.
- A proxy dial can no longer burn over a minute per attempt: the
  per-step 12s timeouts stacked (a SOCKS5 handshake is up to nine
  steps), so a lane that accepted TCP and stalled mid-handshake
  stalled every fetch, and the Chrome relay re-paid it per browser
  request before its strike cache armed. One 30s budget now bounds
  the whole dial+handshake.
- NO_PROXY entries that combine brackets and a port ("[::1]:8443")
  match again: the port strip skipped any host containing ':', so
  the bracketed IPv6 + port form matched nothing at all.
- An empty proxy env var counts as unset: HTTPS_PROXY="" used to
  short-circuit the fallback chain, sending https traffic direct
  and starving the lowercase variant and ALL_PROXY.
- Proxy credentials with an empty user name (":token@host:port")
  are no longer dropped: the CONNECT header, the raw-hop
  Proxy-Authorization and the save round trip carry them, matching
  the ghost tier's user-or-pass definition of an authenticated lane.
  SOCKS5 offers username/password negotiation only when both fields
  are present (RFC 1929 wants ULEN and PLEN in 1..255); otherwise it
  offers no-auth alone.
- IPv6 literal targets ride proxy lanes correctly: a bare literal
  (the shape the Chrome relay forwards from ATYP 0x04) is bracketed
  for the CONNECT line and Host header, and SOCKS5 sends IP
  literals with their native address type instead of as an
  unresolvable domain name.
- Doc fixes: from_env_for's resolution-order comment described an
  order the code never had (config slot, then config all, then the
  env chain: exactly what doctor reports), and the relay's drop
  comment overstated what aborting the accept loop cuts.

## [4.3.2] - 2026-09-23

### Fixed

- Reddit threads returned a 257-char JS shell instead of the post
  once their `.json` endpoint refused: the retry re-fetched the page
  with no reddit session, and reddit answers that with a shell, so
  the fetch looked thin and escalated or failed. A refused `.json`
  hop no longer marks the domain walled, and the fallback makes one
  navigation through the legacy host first, which restores the
  reddit.com session cookies, then retries the caller's URL: the
  real SSR page comes back (1333-1551 chars with the post body on a
  live thread; listings too). When the `.json` endpoint answers,
  the path is unchanged (#291).
- Repeated content no longer disappears silently. The
  exact-duplicate rule ate every repeat after the first (a song's
  refrain, a repeated clause) and a section dropped as
  near-identical left no trace. Both rules now leave a marker in
  place (`*[repeated block omitted]*`, `*[repeated section "X"
  omitted]*`) and `structuredContent.omitted_repeats` counts them.
  A paragraph that opens its own section is never dedupe-eligible,
  section comparison is order-sensitive (token 3-grams), and a
  section whose body carries a table only collapses on an exact
  repeat: there the digits are the content (#292).
- The docs-outline adapter no longer hijacks ordinary pages. It
  detected Docusaurus by a bare `a.menu__link` class, which any BEM
  menu can wear, and its own renderer had no `div` in its whitelist,
  so a page whose content sits in divs came back as nav chrome with
  the content erased, reported as success. Detection is the
  framework's `div#__docusaurus` app root now (present on real
  Docusaurus sites, absent from the false positive), and div-based
  prose survives the adapter's renderer (#293).

## [4.3.1] - 2026-09-23

### Fixed
- A BYOK provider failure fell back to local search with no visible
  signal: the result line read `provider local`, indistinguishable
  from a run with no provider configured, and the only record of
  the failure was behind `DONSEEK_DEBUG`. The failure now rides the
  `degraded:` field the local engine failures already use
  (`degraded: byok brightdata parse error at HTTP 200, ...`), and
  the same entry appears in `_meta.engines` (#285).
- Every HTTP provider client read its response body with
  `unwrap_or_default()`, so a body-read failure arrived as an empty
  string, and its parse error dropped the HTTP status it already
  held: an empty 200, a redirect and a malformed payload all read
  "parse error: EOF while parsing a value". One shared
  `parse_provider_json` now keeps the transport error, names an
  empty body, and reports a parse failure with its status and byte
  count, across all nine clients (#286).
- A refused reddit `.json` rewrite no longer ends the fetch. Every
  `www.reddit.com` thread died on a single 403 with a two-step trail,
  because a challenge on an adapter endpoint matched an arm that does
  nothing, and adapter endpoints never route to the browser. Any
  refusal on an adapter rewrite (a challenge included) now retries
  the page the caller asked for through the generic ladder, and the
  adapter hop's trail is folded in front of the retry's so the
  escalation reads as one ladder (`domain-profile` → `http-fetch`
  403 → `adapter fallback` → `browser-launch` → solve pass →
  `http-retry-with-ghost-cookies`). (#287)
- Extraction no longer ships invisible nodes as content: an inline
  `<style>` inside a price heading, a `<script>` inside a spec-table
  cell and a comment header's `SML.load(...)` call all reached the
  output through the raw text iterator behind headings, table cells,
  list terms and the adapters. Invisible tags (script, style,
  noscript, template, svg, canvas, iframe, object, embed) contribute
  no text anywhere now, ad slots (reddit's `shreddit-ad-post`, the
  common ad-wrapper classes) are skipped before extraction, a
  serialized JSON blob sitting in a text node is dropped instead of
  rendered as a paragraph, and a section that repeats its
  predecessor (same heading, same body modulo numbers, like a
  protection plan repeated per variant) renders once. On the issue's
  own page shape the leaked output drops from 1543 to 715 bytes with
  every real block kept. (#288)
- A re-fetch of a changed page no longer prepends the full section
  delta unless `since_last` asked for it. The unasked note is one
  line now (`*[changed since last fetch (rewritten) :
  since_last=true returns just the delta]*`) and the delta stays in
  `structuredContent.changed_sections`, where it already lived. On a
  product page the old header was 1955 characters, 34% of the
  answer, repeating every extracted fragment. `since_last` behavior
  is unchanged, the collapse-to-delta included. (#289)
- A response carrying an unrecognised `Content-Encoding` token (S3's
  classic `Content-Encoding: UTF-8` on a plain body) is passed
  through as identity instead of failing the fetch permanently, and
  the escalation ladder stays alive behind it. `x-gzip` and
  `x-deflate` decode as their canonical names, a recognised codec
  that genuinely fails to decode still errors loudly, and the size
  cap applies to the pass-through exactly as to identity. The
  284-page guideline PDF from the report now fetches at tier 1.
  (#290)
- The invisible-text rule now covers every remaining DOM collector:
  search result titles and snippets, crawl link labels, feed
  summaries, `jsdata`'s mined strings, the dedicated extractors and
  the adapters all skip script and style subtrees now (the same
  raw-text leak class as #288, found while hardening it).
- `jsdata`'s embedded-HTML stripper never worked: it selected `body`
  on a FRAGMENT tree, and a fragment has none, so every mined string
  carrying markup was silently emptied instead of stripped and the
  item vanished from SPA renders. It walks the fragment root now, so
  `<b>Title</b>` yields `Title` instead of nothing.

## [4.3.0] - 2026-09-22

### Changed
- A tier-2 render no longer waits a fixed four seconds on pages that
  cannot hydrate. The SPA hydration guard floored every DOM under 50 KB
  at four seconds before the content oracle could settle, and a document
  with no `<script` at all cannot grow its DOM after load, so the floor
  there was pure latency. Scriptless small pages settle on the content
  oracle alone now; every page that can hydrate late keeps the guard
  unchanged (a scripted page under `--tier 2` measures the same as
  before). `example.com --tier 2`: 4.77s to 0.95s median in a warm
  daemon, 5.58s to 2.02s through the CLI, where the cold browser launch
  is the rest.
- The browser display starts on first use instead of at pipeline boot.
  Constructing the fetch pipeline started or adopted Xvfb and ran
  `which Xvfb`, `which xdpyinfo` and an `xdpyinfo` connect before the
  first fetch, so a tier-1-only CLI run paid three child processes and
  an X11 connect for a browser it would never launch. The display now
  initializes on the first browser acquire; tier-1-only runs spawn
  nothing.
- A fetch lane for a new host is chosen by measured RTT among clean
  lanes. With a proxy pool configured, every equally-clean lane scored
  the same and the first lane in `proxies.txt` won for every new host,
  so a fast lane could never displace a slow one. The lowest measured
  RTT wins now; file order remains the tie-break while no measurements
  exist.
- The `chromium --version` probe is cached across processes, keyed by
  the binary's identity (path, mtime, size). The probe spawned the
  browser once per process and the CLI is one process per fetch, so
  every CLI fetch paid it. An updated browser re-probes because its
  identity changed. Receipt: two consecutive CLI fetches on one cache
  directory, the second spawns zero browsers.

### Fixed
- A small page with no `<script>` but an inline load-time handler
  (`<body onload="…">`, `<img onerror="…">`) can still build itself
  after load; the scriptless fast path skipped the tier-2 settle
  floor for it. Load-time handlers count as script again; click
  handlers, which need a user, do not.
- The ghost lane's `Xvfb` child was never waited on. An Xvfb that
  died while donsetch lived sat as a `<defunct>` zombie for the rest
  of the process's life, and a hard parent death (SIGKILL from the
  OOM killer, a Ctrl-C'd CLI) left the display detached at PPID 1
  forever; a three-day-old orphan showed on a reporter's box. One
  reaper task owns the child now (it reaps within a tick and is the
  only thing that kills it), `PR_SET_PDEATHSIG` makes the kernel
  kill the display with its parent for any death, and a display
  that never came up is killed on the failure path instead of
  leaked (#280).
- `donsetch mcp --supervised` replayed raw byte history after a
  child died, so the replacement re-ran every request the dead
  child had already answered: a long session replayed ~50 finished
  fetch batches (minutes of CPU, gigabytes of RSS) and the client
  logged one "unknown message ID" per duplicate response. The
  replay window is request-level now: every line the client sends
  with an id is held until its response passes through the stdout
  forwarder (or the client cancels it), and only unanswered
  requests replay (#281).
- "The extraction produced text" was the whole success test, so a
  client-rendered shell's navigation and login chrome (240 chars of
  nav, title, footer and "Continue with Email") and an unsolved
  challenge interstitial (323 chars of vendor prose) shipped as
  `content_ok`. Two positive tests gate the success path now:
  chrome-only extraction (small, prompt-laden, prose-poor) fails as
  `wall.empty_shell`, and an interstitial's own words fail as
  `wall.challenge_unsolved`. Both are `walled`, so the escalation
  ladder (ghost render, a configured unlocker) engages instead of
  the agent trusting boilerplate; the ghost settle oracle also
  refuses to settle on an interstitial any more, so a challenge
  that clears seconds later is waited out rather than shipped
  (#282).
- Both reddit adapters retargeted fetches to `old.reddit.com`,
  which serves a login wall to anonymous clients on HTML and
  `.json` alike: every reddit fetch paid a dead hop (and on a
  walled host already recorded by the detector, the wasted hop
  escalated to a billed unlocker call), while a caller who
  supplied the working `www.reddit.com/....json` URL was detoured
  through the wall first. The `.json` path rewrite stays and keeps
  the caller's host; the old.reddit retarget is gone (#283).
- `donsetch keys add unlocker` put the Web Unlocker in the search
  provider list: every search burned one guaranteed-failing
  dispatch (`unknown provider: unlocker`), the add claimed "BYOK
  search is now active : local search is bypassed" for a key that
  cannot serve a search, and the unlocker held the search-default
  slot. Fetch-side providers are tagged now: the unlocker stays out
  of the search chain, out of default selection and out of the
  search notes, while `keys list` still shows the key with its
  fetch-side role; a store already carrying `default=unlocker`
  heals on the next key add (#284).
- `robots.txt` rules with `*` or a trailing `$` (`Disallow: /*.pdf$`,
  `Disallow: /*?`, `Disallow: /private*/`) were matched as literal
  prefixes, so they matched nothing and a crawl with `respect_robots`
  fetched what the site had disallowed. RFC 9309 §2.2.3 makes both
  required; rules are matched that way now, longest rule still wins
  and `Allow` still wins a tie. The matcher is iterative and
  polynomial on a hostile rule (mnaza, #271).
- A news result whose `pubDate` carried an absurd year (the RSS date
  token is copied as is) overflowed the freshness arithmetic: a panic
  under overflow checks, a wrapped garbage age and a wrong freshness
  multiplier in release. A year outside 1..=9999 is not a date now and
  ranks neutral, the same gate the cookie jar got for `Expires`
  (mnaza, #272).
- A search result's title from a native provider or a SERP page was
  taken at any length; the plugin adapter cut its titles at 512
  characters in 4.2.9 and the other sources did not, so one result
  could carry a multi-MiB title into the tool text and the structured
  content. Every source passes the merge once; titles are cut there
  (mnaza, #272).
- A crawl scope pattern with several `*` (`/*/docs/*/api/*`) matched
  against a link the page chose could hold the crawl worker for hours:
  the glob matcher tried every split at every `*`, exponential on a
  mismatch, and one 8 KiB href with the right shape was enough. The
  matcher is iterative and polynomial now; every pattern that matched
  before still matches, and only that (mnaza, #273).
- The Bright Data unlocker's answer was read with no size cap, where
  every other transport stops at 64 MiB: the target page comes back
  inside a JSON string, then was decoded and base64-encoded for the
  cache, three copies of whatever size the page chose. It is read in
  chunks and refused past the same cap now (mnaza, #274).
- A malformed proxy entry's error echoed the whole entry, password
  included, into a fetch error that can surface in a tool result; the
  Debug form was already redacted. The messages name the expected
  shape (mnaza, #274).
- `web_fetch` of a PDF parsed it on the async runtime's worker thread:
  pdfium rasterizes and lays out every page synchronously, so for the
  whole parse that worker did nothing else, the call's own
  `deadline_ms` could not fire (the future never yielded inside it),
  and other tool calls scheduled there waited. The crawl already ran
  PDFs on the blocking pool under a five-minute budget; both paths now
  share that helper, the ghost retry and the wayback snapshot included
  (mnaza, #275).
- A PDF was read to its last page whatever the count. The size gate
  admits a document of hundreds of thousands of near-empty pages, and
  each page costs a rasterization, so that was hours of work for a
  note saying the pages were blank. The first 500 pages are read; the
  true page count is reported and a note says how many were read
  (mnaza, #275).
- A page nested thousands of elements deep held the extractor for
  minutes, and hours on a larger body: the HTML parser scans its
  stack of open elements on every tag, as the specification writes
  it, so the parse is quadratic in nesting depth (8 000 nested
  `<div>` 0.4 s, 32 000 6.6 s) and nothing capped the depth (browsers
  stop at 512).
  A linear pre-parse scan now refuses a body nested deeper than 4096
  levels as not a document, counting what the parser's stack keeps
  (void, raw-text and sibling-closed elements are left out; the
  self-closing slash is ignored, as the HTML parser ignores it), on
  the HTML path and the feed path (mnaza, #276).
- Tables nested in tables were extracted in time quadratic in the
  nesting, and a nested table's rows were merged into the outer
  table: the row and cell scans were descendant selects. They read
  the table's own rows (directly or through `thead`/`tbody`/`tfoot`)
  and the row's own cells now; 1 000 nested tables take a fraction of
  a second and a nested table stays inside its cell (mnaza, #276).
- A page repeating a known JS-data marker (`window.__NEXT_DATA__ = `)
  with no value behind it rescanned the rest of the document once per
  marker; 512 KiB of them was minutes. The scan has a budget of a few
  document lengths (mnaza, #276).
- The old-reddit renderer recursed without a depth cap and its list
  renderer used descendant selects, so a post body nested a few
  thousand levels deep overflowed the stack (an abort) after a
  quadratic render. Both carry the same caps as the main extractor
  (mnaza, #276).

## [4.2.9] - 2026-09-20

### Fixed
- With `proxy.fetch_rotate` on, a fetch of a host that does not resolve
  benched the proxy lane it was riding for ten minutes, on disk. The
  4.2.5 lane-health sweep read the new `Dns`/`DnsTimeout` variants as
  the lane's own name failing, but those come from the SSRF guard,
  which resolves the origin before any lane dials; a lane whose own
  name fails still arrives as an `Io` from the proxy connect. After as
  many dead hosts as there are lanes (typos, dead domains, or pages
  that redirect to one) every lane was benched, `pick_fetch` fell
  through to `direct`, and the fetch left on the real address with
  rotation configured. The origin's name now leaves lane health alone
  (mnaza, #265).
- A search plugin registered with an empty command (a hand-edited
  `plugins.json`; the CLI refuses one) panicked the daemon on every
  `web_search`. 4.2.5 guarded the doctor against that entry; the search
  path now returns the same registration error instead of indexing an
  empty argv.
- A plugin that wrote past the 8 MiB stdout cap and kept running sat
  until its `timeout_ms` and came back as a timeout. Nobody reads the
  pipe past the cap, so the plugin blocked on it; it is killed as soon
  as the cap is crossed, as the contract said, and the error names the
  cap.
- A plugin result's `title` and `url` were taken verbatim while the
  snippet was capped at 8 KiB. A title is now cut at 512 characters and
  a result with a url over 4 KiB is dropped, counted with the other
  dropped entries (mnaza, #266).
- A URL with an IPv6 literal host (`http://[2606:4700::1111]/`, or
  `http://[::1]:8799/` under the private-egress hatch) failed with a
  DNS error: the host string keeps its brackets and the resolver does
  not know `[::1]`. A literal is now answered without a lookup, the
  same address set a lookup would give, and is judged by the same
  filter.
- The SSRF guard judges the IPv4 address embedded in a NAT64 address
  (`64:ff9b::/96`, `64:ff9b:1::/48`) or a 6to4 address (`2002::/16`),
  as it already did for the `::ffff:` mapped form. On an IPv6-only
  network the translator that turns `64:ff9b::a9fe:a9fe` into a packet
  for 169.254.169.254 sits inside the network, so the v6 form reached
  what the v4 rule refuses; a literal, a resolved AAAA answer and the
  connect-time filter all go through this one predicate (mnaza, #267).
- A sitemap element whose text held many `&` without a `;` behind them
  (a `<loc>` of ampersands; 64 MiB of them fits a small `.xml.gz`)
  held the crawl's worker for hours: the entity decoder searched the
  whole remainder for the `;` on every `&` and only then asked
  whether it was within the ten-character entity window. The search
  is bounded to the window now, and the decoder is linear.
- A `<loc>` or `<lastmod>` of any length was kept as a sitemap entry,
  and a text sitemap line likewise; one 64 MiB entry per file across
  the 32 files a discovery may read was 2 GiB of strings carried by
  the map, the frontier, the focus IDF table and the resume token.
  sitemaps.org caps a `<loc>` at 2048 characters; longer ones are not
  entries, and a `<lastmod>` past 64 characters is dropped from an
  otherwise kept entry (mnaza, #268).
- A crawl ignored a relative `<base href>` (`<base href="/app/">`, the
  common form): the base was parsed as an absolute URL, failed, and
  every link on the page resolved against the page instead, so the
  pages the site actually links to were never fetched. The base now
  resolves against the document URL, as a browser does, and a base
  that is not http(s) is ignored (mnaza, #269).
- The revalidation cache capped each body at 8 MiB and the entry count
  at 512, but not what they add up to: a daemon whose agent fetched a
  few hundred large pages carrying an ETag or a fresh window held up
  to 4 GiB of them in memory for as long as it ran. Resident bodies
  are now budgeted at 64 MiB, evicting the oldest entries first, the
  same order the entry cap uses (mnaza, #270).

## [4.2.8] - 2026-09-20

### Changed
- The macos-x86_64 lane runs the smoke suite instead of nothing, so no lane
  in the matrix is test-free any more. It was the slowest runner and the only
  one running no binaries: its full-suite run died at the step timeout
  mid-compile, and ten separate integration-test links were the cost that
  made it unaffordable. With the integration tests consolidated into one
  binary that link is a tenth of what it was, so the lane runs the same
  smoke set macos-arm64 runs (process spawn and restart policy, paths,
  profile detection, extraction) and macOS-exclusive code gets a second
  architecture's worth of exercise.

### Fixed
- The Turnstile click aims at the widget now, and it survives the widget
  rendering late. Three defects in that path, each one measured on a live
  widget (Turnstile's interactive test sitekey `3x00000000000000000000FF`,
  which renders a real checkbox on a local page):
  - In `solve`, a click that could not be aimed still spent the pass's one
    click. The widget script is async and the first poll has no
    `cf-turnstile-response` input at all, so that click went to a
    hardcoded point and the pass then stopped trying before the thing it
    was aiming at existed. An unaimed click no longer spends an attempt;
    the fixed-point guess is still allowed once, as a last resort.
  - The lookup refused any element with a zero-width box. A real widget
    box does collapse to zero width once Cloudflare has taken the
    container over (measured `x=32 y=372 w=0 h=68`) while the widget
    still renders at that box's left edge, so the refusal sent the click
    to a fixed point on the page. The same width also fed the 22px
    checkbox inset (`min(22, w/2)`), which collapsed to a 0px inset on
    that box, aiming at the widget's border. The lookup now walks the
    real widget structure, requires only a height, and keeps the inset.
  - A headless launch laid the page out at 0x0. The layout viewport was
    pinned only for an unknown platform or a non-default persona
    viewport, so a headless browser on Linux got no metrics override and
    a launch with no window laid the page out at zero size: measured
    `window.innerWidth`/`innerHeight` 0x0 and
    `document.elementFromPoint(x, y)` returning null at EVERY point, with
    nothing able to land on the checkbox. Extraction never showed it (the
    DOM is still there and the wall oracle reads it), which is why only
    clicking was broken. A headless launch pins the layout to the persona
    viewport now; a headful launch is untouched, since its window really
    is that size.
- `deadline_ms` reaches the browser passes. Every pass was fixed at 20s
  (25s on the actions path) and ignored the caller's budget, so a call with
  a short deadline was cut off mid-pass by the clock that wraps the call: it
  answered `deadline.hit` with no wall verdict, even where the pass had
  already seen the wall, and the browser kept working on a pass nobody was
  waiting for. A pass is bounded by what is left of the budget now, floored
  at 3s and capped at the old default, so no pass can get longer than before
  and a caller who sets no deadline still gets the fixed pass. One honest
  limit: a deadline hit still reports no trail, because the escalation
  built so far is discarded with the cancelled pass, and that needs its own
  change.

## [4.2.7] - 2026-09-20

### Changed
- The integration tests build as one binary instead of ten. Every file
  directly under `tests/` was its own crate, so each one linked the whole
  library and dependency tree again and re-generated the library's generic
  code it used; they are now modules of `tests/it/`, sharing one link. At
  the `ci` profile's opt-level 3 the integration tests' combined unit time
  drops from 81.5s to 10.1s on a local build. On a machine with spare cores
  the wall time does not move (those units were never on the critical
  path); the gain is where cores are scarce, about 5% on the linux-x86_64
  lane and about 9% on a warm Windows lane. nextest still runs every test
  in its own process, so the isolation the `DONSETCH_CACHE_DIR` tests rely
  on is unchanged. A new integration test goes in `tests/it/` with a `mod`
  line in `tests/it/main.rs`: a new top-level `tests/*.rs` file would
  quietly become a separate binary again (Mart-Bogdan).

### Fixed
- The `[meta]` block no longer runs into the document's first line. In
  text-only mode the fold prepends `[meta] {...}` as its own content block,
  but a separate block carries no boundary to the model: most harnesses
  concatenate the blocks, and Claude Code joins adjacent text blocks with no
  delimiter at all, so `...example.com/"}# Example Domain` arrived glued and
  the heading stopped being markdown. The meta line ends with a blank line
  now. Two newlines rather than one, because a single `\n` only separates
  content that can interrupt a paragraph in CommonMark (an ATX heading, a
  fence, a thematic break), and a page opening with a plain paragraph or a
  table row would lazily continue the `[meta]` line instead, which is the
  same failure with none of the visibility. Every tool emits exactly one
  text block and the fold is the only place a second one is prepended, so
  one change covers `web_fetch`, `web_search`, `web_crawl` and
  `web_screenshot`. Harmless where a client does keep the blocks apart: a
  trailing blank line is trimmed on render, and OpenCode already joins text
  blocks with `\n\n` of its own, so there it doubles a separator that was
  going to be there anyway and still reads as one paragraph break
  (Mart-Bogdan).
- The MCP server no longer dies on a stack overflow in a debug build. Tokio
  gives runtime worker threads 2 MiB of stack and the MCP server runs every
  tool call on one, while the fetch path needs more than that unoptimized:
  measured on the `fast` profile, 2 MiB and 2.5 MiB both overflowed and
  2.75 MiB was the first size that passed. A stack overflow is an abort with
  no error envelope, so `donsetch mcp` killed the whole session on the first
  fetch while the same fetch through the CLI was fine, because the CLI runs
  on the 8 MiB main thread. Workers now get 8 MiB, the same as the main
  thread. The released binary was never affected (the same path needs under
  256 KiB there), so this is a development-loop fix, and it carries an
  integration test that drives the real binary over stdio and goes red if
  the worker stack is left at the default.
- The handle-shape test no longer reads the process-wide config. It asserted
  `is_handle(...)` unconditionally, and `is_handle` returns false when
  `mcp.url_handles` is off, so on any machine where handles are disabled in
  the real config the test failed for a reason that had nothing to do with
  the code. The shape rules are asserted against `is_valid_handle_id` now,
  and the gate is asserted against the knob's own value, so it holds either
  way (Mart-Bogdan, #263).

## [4.2.6] - 2026-09-19

### Added
- Search plugins can report why they failed. The format-1 error envelope
  takes an optional `error_kind` (`invalid_key`, `credit_depleted`,
  `rate_limited`, `server_error`, `network_error`), classified into the same
  key states a native adapter derives from an HTTP status, so a plugin
  whose API key was revoked is parked instead of being spawned again on
  every search for the rest of the day. `keys list` shows the state,
  `doctor` warns about it, and re-registering the plugin is the recovery.
  An envelope without `error_kind` records no state, so every existing
  plugin keeps its behaviour. The parked cases also keep the plugin's own
  words: `keys add plugin --test` used to print a bare "invalid key" at
  exactly the moment a user is debugging credentials (googio).

### Changed
- The ghost no longer injects a script into the page before its own scripts
  run. It used to define `navigator.languages`, fill in `window.chrome` and
  `window.chrome.runtime`, and replace an empty `navigator.plugins`, on the
  theory that some launches leave those gaps. Measured against real Chrome
  on both backends (headful Xvfb and `--headless=new`), the gaps are not
  there, and every patch that fired moved the page further from a real one:
  real Chrome has no own properties on `navigator` at all, `chrome.runtime`
  is absent rather than present-and-empty, and `plugins` is a real 5-entry
  PluginArray with a working `item`. The README's "no JS injection" line is
  true now rather than aspirational, and it says what the page gets.
- `_meta.engines` carries one lane per provider the search tried, in walk
  order, instead of only the one that answered: each lane has its own wall
  time and a status (`empty`, `rate_limited`, `error`, `ok`). A caller can
  no longer read "Tavily answered" as "Tavily is the default" when Serper
  was asked first and came back empty and a monitor paged a provider change
  that never happened (#253).

### Fixed
- One entry the key-file schema rejects no longer discards the whole store.
  `byok-keys.json` was parsed as a unit and ANY error returned an empty
  config with "corrupt key file (...), ignoring", so a single hand-edited
  `"state": "dead"` cost every provider at once and the keyless answer that
  followed read as "donsetch has no provider fallback". The decode is per
  entry now: a rejected key is dropped and named on stderr, an empty key is
  dropped for the same reason the write path rejects it, and every other
  key keeps working. A file that is not JSON at all still degrades to
  empty, because nothing in it can be trusted.
- The Turnstile click never aimed at the widget: the lookup selector was
  invalid CSS (`iframe[src*=challenges.cloudflare]` carries an unquoted
  dot), and since it is one selector list, that single bad arm made
  `querySelector` throw for all three arms and the `.ok()` chain swallowed
  it, so every click went to a hardcoded point with nothing in the log.
  Verified on a real browser: the unquoted form throws SyntaxError while
  the quoted form matches, and `iframe[src*=turnstile]` alone is valid,
  which is what makes the dot the culprit. The values are quoted, the
  container of the hidden `cf-turnstile-response` input is the fallback
  (Cloudflare keeps the real iframe in a closed shadow root), and a failed
  lookup logs its reason (Mart-Bogdan, #257).
- A walled fetch reported `"verdict": "ContentOk"` beside
  `"code": "wall.captcha"`, because the success path's default reached the
  failure envelope. A failure never claims content now: a verdict the run
  actually earned is kept, and otherwise the failure names itself,
  `Blocked` for a wall and `Unknown` for anything else.
- A failed second solve pass left no trace step, so the escalation listed
  one `ghost-render` while the log showed two attempts. The failing pass
  records `solve-pass2` with "still walled" and its own ms.
- `[ghost] Xvfb started` was printed for a display that was reused rather
  than started.
- `cargo run` printed "ignoring DONSETCH_FEATURES: expected
  DONSETCH_<SECTION>__<KEY>" for each of build.rs' cargo:rustc-env values.
  They are reserved names now, not misnamed settings.
- The BYOK key store's CLI said "dead" for a key state the file does not
  accept. `keys list`'s legend printed one ✗ labelled "dead" for two states,
  its no-usable-key warning said "all keys are dead", `keys --help` said
  `reset` "fixes rate-limited/dead keys", and the store module's own doc
  called the states dead, while the serialized states are exactly `active`,
  `rate_limited`, `credit_depleted` and `invalid`, and anything else makes
  the loader warn "corrupt key file" and run with no keys at all. An
  operator who wrote `"state": "dead"` by hand, because that was the word
  the program taught, silently lost every key and read the keyless answer
  as "no provider fallback". Every human-facing string now names the state
  the file names. Strings only; no behaviour change. Reported by
  daniel-plescia.
- A short page that loads Cloudflare Turnstile's public embed
  (`challenges.cloudflare.com/turnstile/v0/api.js`) without a form was
  read as a challenge interstitial, so a browser fetch that had already
  passed the challenge never settled and ended `walled` after both
  solve passes. scrapingcourse.com's Cloudflare test page, solved in
  about 5 s on Windows, came back walled after 42 s; it returns the page
  in about 10 s now. The embed host alone is no longer an interstitial
  marker: it counts only with a `cf-turnstile` widget on the page, so a
  bare Turnstile shell still reads as a challenge. Two golden fixtures
  pin both sides: the solved page and the live interstitial, its
  per-visit tokens replaced (Mart-Bogdan).

## [4.2.5] - 2026-09-19

### Changed
- The npm package page carries a real README instead of the stub: the fourth
  tool (`web_screenshot`), the feature list, the full platform matrix with
  OCR/rerank availability per platform, the install troubleshooting notes,
  the sponsor tiers, and links to the docs. `package.json` gained a
  `funding` field, so npm shows a sponsor link on the package page, and
  search keywords that describe what the package actually is.
- The repository README was rewritten: 1090 lines to 700, sponsors moved
  from dead last to the fourth section with a nav link from the top, the
  v3 and v4 "what's new" sections folded into the feature sections they
  describe, the WRB results replaced by a pointer to the WRB repo, and the
  comparison table cut from 18 rows to 12 plus a short Firecrawl head to
  head. Two stale numbers fixed: the tests badge said 1270 while the body
  said 727.

### Fixed
- A crawl seeded on a page at the host root (`/p0.html`, `/index.php`)
  returned the seed alone, or nothing, with `complete: true`. The
  auto-scope rule that keeps `/tokio` on docs.rs inside `/tokio/*` was
  applied to the file name too, so the scope became `/p0.html/*` and
  every sibling link was filtered out before it reached the frontier.
  Separately, the quality gate skipped a low-quality page before
  harvesting its outlinks, and a hub page (a link list with almost no
  prose) is the lowest-quality page on a site and the one a crawl is
  seeded from. A root-level file now scopes to the host, and a skipped
  page still feeds the frontier (mnaza, #250, fixes #249).
- A resolver that could not answer (EAI_AGAIN: resolv.conf unreachable,
  SERVFAIL, a VPN flap) was reported as `network.dns` with
  `errorKind: permanent` and a do-not-retry action, the mirror of #248:
  the outage read as a dead name. It is transient now, while a name that
  does not exist stays permanent. Search enrichment no longer demotes a
  live result when the resolver times out during the prefetch, and the
  ghost probe's failure class and the search task status label kept their
  signals (mnaza, #251).
- Egress lane health learned the same variants: a proxy lane whose own
  name stops resolving is marked dead again, and a resolver timeout is
  counted as a timeout (the message reads "dns timeout", which the old
  `contains("timed out")` check never matched).

## [4.2.4] - 2026-09-19

### Added
- An in-process DNS cache. The host resolver is a network round trip on a
  box without a local caching daemon (public resolvers, no nscd or
  systemd-resolved), measured at ~55ms per lookup here, and one fetch
  asked for the same name at least twice: the SSRF guard, then the
  connect, plus once per redirect hop and once per page of a crawl. A
  4-page crawl of one host sent 12 DNS query batches; it sends 1 now.
  Positive answers only, cached for `fetch.dns_cache_ttl_secs` (default
  30, 0 disables). A failure or a timeout is never cached, so a resolver
  blip cannot block a host for the TTL, and the cache holds addresses
  rather than decisions: the guard and the connect still filter every
  address they dial, so a name that starts answering with a private
  address inside the TTL is still refused. The h3 lane now resolves
  through the same cache instead of running its own blocking lookup on a
  worker thread.

### Fixed
- The h3 lane built a fresh `chrome_150` browser profile per request
  instead of using the identity the fetch presents everywhere else, so
  its QUIC and TLS configuration could drift from the caller's profile.
  It now takes the caller's profile.
- A URL whose host does not resolve is a DNS failure, not a policy block
  (Mart-Bogdan, #248). The SSRF guard's DNS messages ended in "fail-closed
  SSRF guard", and the classifier matched that phrase before it matched
  "dns", so a typo, a dead domain or a resolver problem came back as
  `guard.ssrf` with `errorKind: permanent`: an agent branching on the code
  concluded the target was deliberately forbidden. A resolver TIMEOUT got
  the same permanent verdict even though a retry can work. `FetchError`
  now carries `Dns`, `DnsTimeout` and `Ssrf` variants, the tool error
  carries the code its variant declares, and the classifier reads that
  code ahead of the prose, so the mapping cannot drift with wording
  again. A resolver timeout is `transient` now, a private/loopback address
  is still `guard.ssrf`, and the post-action navigation guard reports the
  real failure instead of a hardcoded private/loopback verdict.

## [4.2.3] - 2026-09-19

### Changed
- The development loop and CI were rebuilt around what actually costs
  time. Local recipes now run a `fast` cargo profile (debug codegen for
  this crate, deps at opt-level 1, no debuginfo): an edit rebuilds in
  seconds instead of the 8-12 minutes a release-shaped whole-crate
  recompile cost. `just check` is the seconds-level signal, `just t` is
  the touched scope, `just tci`/`just bin-ci` keep release-profile parity
  on demand, and `just heavy` runs the tests that measure time and memory
  (soak, corpus, landmarks, live probes). `just all` no longer runs the
  suite: CI is the full gate and now runs it in parallel across the
  matrix instead of serially under the push hook.
- CI runs the full suite where a native warm toolchain makes it cheap
  (both Linux arches) and a platform-specific smoke set plus
  `clippy --all-targets` on the platforms whose cost is the link, not the
  test count. The heavy set and all five fuzz targets run nightly; pull
  requests fuzz one target. The pre-push hook checks both feature sets on
  the fast profile, which is the same net for seconds instead of minutes.
- `sccache` is used automatically when installed (`rust-sccache`), for
  rustc and for the C/C++ compilers, so recompiles across profiles,
  feature sets and branches become cache hits.

### Fixed
- The rerank model cache follows `DONSETCH_CACHE_DIR` and `[paths] cache_dir`
  like every other cache (Mart-Bogdan, #243). It read `dirs::cache_dir()`
  directly, so an override moved every cache except the model: a container or
  a side-by-side install read and wrote the real user cache while `doctor`,
  which checks the overridden root, reported the model as missing. The model
  download publishes atomically now: it wrote straight to the final path, so
  a concurrent first-use download could expose a half-written model, the load
  failed, and reranking was silently skipped.
- The page-history fingerprint describes the page, not the request
  (Mart-Bogdan, #246). It was taken over the rendered, focus-filtered slice
  with the request's own notices prepended, so two reads of one unchanged URL
  with different reading parameters looked like a content change: an agent
  alternating a focused and a full read got `changed` on every call, and one
  call's parameters became the baseline for the next.
- The profile label names the Chrome the build actually presents
  (Mart-Bogdan, #247). `--version`, the scorecard label and every recorded
  stealth baseline said `chrome-150` while the wire UA followed the detected
  browser, and the undetected-browser fallback was presented as a fact about
  the host instead of as a fallback.

## [4.2.2] - 2026-09-19

### Fixed
- The tier-1 persona fetch path (MCP `web_fetch`, and the CLI `fetch` that
  shares it) resolves `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` instead of
  dialing direct, and re-resolves it at every redirect hop the way the main
  fetch lane already did. Behind an egress that only allows outbound
  traffic through a forward proxy, every tool call failed with "Network is
  unreachable" while `donsetch doctor` reported egress healthy: two call
  chains into the same dialer, and only one of them read the ambient proxy.
  Search engine hops, the search prewarm fetches of result URLs and the
  render prefetch assets follow the same convention now. A direct dial on
  the prewarm lane did not just fail: it scored the result
  `QualityObs::Dead`, so live results were demoted to dead links because
  of the egress rather than the page. Reported by theangrykangaroo.
- The Xvfb readiness probe accepts a display that exists only in the
  abstract socket namespace. Xvfb binds its socket name both as a file
  under `/tmp/.X11-unix` and as a Linux abstract socket, but the probe
  looked only at the file. Under WSL that directory is a read-only mount,
  so the file can never appear: every launch spent the full 10s readiness
  budget waiting on it, then reported a healthy display as dead and fell
  back to the detectable `--headless=new` instead of the headful mode the
  virtual display exists for. The file is still probed first, so a stale
  socket is still read as dead. On WSL, tier-2 captures go from 19.1s to
  7.3s and run headful as intended.
- The mid-write daemon-restart test is causal instead of timed: the client
  waits for the child to close its own stdin before writing, so it no
  longer races `sh` startup on a loaded macOS runner. That race is what
  turned CI red on a documentation-only pull request (#239), as a failed
  spawn assert before and a 30s hang after.
- `donsetch doctor` probes the tool lane too. Its network check rode the
  generic fetch lane only, which is a different call chain into the same
  dialer than the tier-1 persona path that `web_fetch` and the CLI `fetch`
  ride, so a fault in that one chain was reported as healthy egress: the
  report above came in as a suspected deployment problem because of it.
  The check now reports `generic and tool lanes`, and says which lane
  failed when they disagree.
- Build hygiene, both found by review (Mart-Bogdan, #240, #241): the
  `guard` recipe's size test never evaluated (`$$(...)` is Makefile
  escaping, not just, so sh saw `<pid>(du -sm target | cut -f1)` and the
  test was silently false, which is why the bloat prune had never fired;
  it prunes past 25G now), and the `fuzz` recipe kept a hardcoded path to
  the budget wrapper instead of riding the `budget` variable like every
  other recipe.
- The CI test step's 30-minute cap was also the cold-build budget: a PR
  branch starts with an empty dependency cache, and on the macos-x86_64
  Intel runner a cold build plus the suite ran past 30 minutes and was
  killed mid-compile, reddening a lane with no test failure behind it.
  Raised to 60, which is a build-phase backstop only: a hung test is
  still caught by nextest's own per-test timeout.

## [4.2.1] - 2026-09-19

### Fixed
- `web_fetch` with a `selector` that matches nothing now returns the default
  rendering plus a notice instead of a different, worse one presented as
  success. The empty match set went on to the rescue paths, so the caller
  got a navigation-first page (`content_kind: Page`, quality 0.30 on the
  reported sites) with `content_ok: true` and no signal at all that the
  constraint had never been applied. It now behaves like `focus` and
  `section` already did: the constraint is reported as not applied, the
  content is what the same fetch returns without a selector, and the first
  line of it says so. Reported by daniel-plescia (#238).

## [4.2.0] - 2026-09-19

The reliability wave. Nothing new to learn: the same four tools, with the
sharp edges taken off the fetch, search and crawl paths, the clients, and
the build itself.

### Fixed
- Nested `Content-Encoding` headers are capped at 8 layers. A hostile
  server could answer with a deeply nested gzip body plus a matching
  header, and peeling those layers is recursive: the layer count chose
  the client's stack depth and spent up to 64 MiB of decompression per
  layer, bounded only by the response's own header length. Real
  responses carry one layer, rarely two.
- The HTTP/2 header decoder starts its dynamic table at the 65536 bytes
  we advertise in `SETTINGS_HEADER_TABLE_SIZE`, not at 4096. A server
  that keeps the default and never sends a dynamic table size update is
  entitled to index everything it inserted, so entries we had already
  evicted came back as `hpack: bad index N` and failed an otherwise
  valid response.
- `donsetch <command> --help` prints help for every command. The
  management commands used to run instead: `doctor --help` started a
  full health check, `update --help` checked the network, `rollback
  --help` touched the install, `mcp --help` opened a daemon on stdin,
  and `status --help` printed the status. `donsetch help stop` had no
  page at all.
- The ghost relay's upstream strike cache drops expired rows instead of
  holding every host it ever failed to dial for the life of the
  process, and its mutex no longer panics if it was poisoned.
- The crawl pace store gives every writer its own temp file and treats
  only temps older than a minute as abandoned. The store exists because
  separate processes crawl the same host, and a temp named after the
  host alone was shared by all of them: two concurrent stamps could
  interleave in one file and publish a row that no longer parses, and
  either side's prune could delete the other's in-flight temp before it
  was renamed, losing that host's floor.
- The pi extension sizes its client timeout from the call's own budget
  (`deadline_s`, `deadline_ms`) instead of a flat 120s. A crawl at its
  own default 120s deadline was racing the client timeout, and any
  larger legal deadline was unreachable through pi. `web_screenshot`
  also gets its own icon instead of the generic diamond.

- A server-controlled `Set-Cookie: ...; Expires=` header no longer reaches
  unbounded i64 arithmetic in the cookie date parser: the year and clock
  fields are gated per RFC 6265 5.1.1 before any math, so an absurd year
  or time makes the attribute fail to parse and the cookie stays a session
  cookie, instead of a panic under overflow-checks or a silently wrapped
  garbage expiry in release. (mnaza, #230)
- The last-resort raw-text fallback walker caps its recursion depth at
  300, matching the two primary HTML walkers: a deeply nested, text-poor
  page that reaches the fallback could overflow the tokio worker's
  2 MiB stack and abort the process on one request. (mnaza, #231)

### Changed
- MCP tool descriptions: the six parameters that inherited verbose CLI
  wording (`tier`, `section`, `shot`, and `deadline_ms` on both fetch
  and search, `intent`) now carry agent-facing strings. Every documented
  fact is preserved. Measured: `tools/list` goes from 9742 to 9591
  compact bytes (2436 to 2398 estimated tokens). The rest of the surface
  is structural JSON (enums, `anyOf`, `maxItems`, the nested actions
  object), not prose, so there is nothing left to trim without dropping
  a machine-readable constraint.

## [4.1.2] - 2026-09-17

The install-hardening wave: smaller npm installs, recoverable blocked
postinstall scripts, bounded offline behavior, and release plumbing that
keeps every distribution channel on the same binary.

### Fixed
- npm CLI installs no longer pull the Pi coding-agent peer dependency
  tree; the package remains dependency-free while Pi continues to provide
  its host runtime modules.
- The npm shim self-heals after pnpm/bun approval blocks or
  `--ignore-scripts`, with package-manager-neutral recovery guidance.
- The npm installer now supports HTTPS proxies, CONNECT tunneling,
  timeouts, retries, release mirrors, version stamps, and the
  `DONSETCH_INSTALL_TAG` testing seam.
- Musl detection no longer misclassifies glibc hosts with a secondary musl
  loader (#45); true musl systems receive a source-build command.
- Windows ARM64 uses the Windows x64 asset under emulation, and the Rust
  updater selects the same asset.
- Doctor MCP JSON escapes Windows paths and reports the `donsetch` npm shim
  (with `npx donsetch` as an alternative) instead of an internal path.
- `--version` skips update checks in CI or when requested and bounds the
  online check to three seconds.
- The Pi extension uses the active Node executable, allows long installs,
  and gives `donsetch doctor` guidance when initialization times out.
- Release publication now gates npm on all binary and checksum assets and
  copies the repository LICENSE into the package.

### Changed
- Build metadata environment names now consistently use the `DONSETCH_*`
  prefix; legacy user configuration variables retain their historical
  names.
- Homebrew and dsh downstream repositories can auto-track published
  releases through the npm release workflow.

## [4.1.1] - 2026-09-16

The bug-hunt wave: a real regression report turned into a universal
fix, plus two reliability fixes found hunting edge cases around it.

### Fixed
- Bare-`<br>` layouts keep their paragraph breaks on every path.
  `loose_text` is the shared DOM-to-markdown site that tier 1, the
  tier-2 ghost browser, the site adapters, and the focus/section
  excerpt renderers all funnel through, and it used to trim the
  `<br>` line-break sentinel away: a page built as bare text plus
  `<br>` tags (like the live-reported novel chapter with the
  `<br />` + CR + `<br />` + CRLF sequence) collapsed to one
  space-separated blob. The `<br>` now emits its sentinel in place
  and the shared post-processing turns it into real newlines, so a
  br/whitespace/br run yields a blank line between paragraphs,
  matching browser rendering. Covered by fixtures for the exact
  reported byte sequence, the plain `<br>` form, the single-br case,
  and the downstream focus renderer.
- The last-resort fallback extractor now tells a line break from a
  paragraph break the same way: one `<br>` breaks the line inside
  the paragraph, two in a row break the paragraph. It used to flush
  a paragraph at every `<br>`, splitting prose at every line break,
  and added a space after each break.
- The browser relay's per-host strike cache expires after ten
  minutes and clears on a successful connect. Three dial failures
  used to fast-reject a host for the whole process lifetime, even
  after the network healed and even after a successful connection.
- Docker aarch64 builds link again: the image now installs lld so
  the linker-fallback logic that hardens release builds applies
  inside the container too (aarch64 rustc output needs lld).

## [4.1.0] - 2026-09-14

The live-verification wave: every subsystem re-tested against the
real binary on real sites, and the bugs the unit suites could not
see are dead.

### Fixed
- Delta recrawls see the live page. The crawl fetch rode the
  revalidation cache, so a fresh-window entry was served back
  without dialing the origin and every recrawl compared the last
  crawl's own body, reporting zero changes forever. The crawl fetch
  path now bypasses the cache. Covered by an e2e test: the plain
  path serves the stale body with zero dials, the crawl path dials
  and sees the change.
- A SOCKS5 relay that refuses a host remembers the refusal and
  fails fast. Repeat-offender hosts ate a dial timeout per request
  (the tiktok ladder went 40s to 21s).
- Tier 1 follows redirects through the identity wrappers. The
  persona/class/UA lanes had no redirect loop and reported
  "blocked: returned HTTP 301" on sites that simply 301 to their
  real URL. One shared bounded redirect driver now.
- Chrome runs with stdin closed: a suspended SIGTTIN page could
  hang the ghost render.
- Wall detection classifies 200-class walls (the reddit nonce form,
  the amazon gate, "please wait" interstitials, the instagram
  AuthWall). A challenge page that arrives with HTTP 200 is not
  content.
- The last-resort DOM fallback is trusted: header/footer/nav/aside
  blocks are skipped, ghost text is accepted at 40+ chars
  non-login-only, and a final shell gate sits in front of the tool
  result. Facebook now gets the honest "the site renders an app
  shell without real content" instead of a success-looking blob.
- The search enrich pass is bounded. One slow top page could stall
  every cold search (2.0 to 4.4s observed). The enrich batch now
  runs under a 700ms deadline; stragglers drop to the SERP snippet.
  Per-stage timings (enrich/filters/topup) ride the search meta so
  a cold-path regression names its phase. Cold search is steady at
  2.0 to 2.3s.
- The no-rerank build compiles again: the rerank-gated topup_ms
  binding was consumed unconditionally, which killed the fuzz smoke
  and windows jobs with E0425.
- Stealth baseline re-captured for the current chromium 151.0.x:
  the client-hello extension ordering moved; user agent and every
  behavioral layer are unchanged.
- Dataset crawl stderr summaries report real page and char counts
  instead of "0 pages" while rows streamed on stdout.

## [4.0.0] - 2026-09-13

The agent web stack, rebuilt end to end. Same four tools, same
zero-config `donsetch mcp` default. Faster, stealthier, more
reliable, and honest about what it knows.

### Removed
- `web_answer` (the evidence pack) and `web_memory` (the local vector
  store), by my call. A fixed search-then-read pipeline and a
  memory-of-everything store strip the agent's control over the loop
  (which link, which budget) and spend tokens on pages the agent
  would not have picked. The agent owns search + fetch; DonSeTch just
  makes each of those calls god-tier. The search prefetch that the
  pack spawned is kept (web_fetch consumes it: a speed win with zero
  new surface).

### Fixed
- Persona locale is fail-closed sanitized before it can reach Chrome
  `--lang`, CDP `navigator.languages` injection, or an
  Accept-Language header. A corrupt on-disk persona or hostile
  `persona.locale` / LANG value can no longer break out of the JS
  array or inject header bytes; invalid values become `en-US`.
- A warm ghost whose viewport/locale no longer matches the incoming
  persona (quarantine re-mint) now relaunches instead of silently
  claiming the old identity. `web_screenshot` uses the same persona
  wire as tier-1.
- Cross-process host-pace rows are compared by host on read (FNV
  collision no longer shares a floor) and stale `.tmp` files from
  failed atomic writes are pruned.
- PDF glyph walk no longer panics when the first glyph's font has no
  PDFium-reportable name (Type3, missing BaseFont, over-long name).
  The mono-font check now bounds-checks the family index like the
  sibling dingbat flag; a crafted PDF can no longer abort the process
  (release `panic = "abort"`) on fetch. (#221)
- Search syndication dedup keeps punctuation in the title key, so
  `C++ Tutorial` and `C# Tutorial` (and dotted versions like
  `Rust 1.75`) no longer collapse into one result and silently drop
  the other. Only titles that are identical after case/whitespace
  normalization still dedup. (#222)
- `web_screenshot` omitted `full_page` now defaults to viewport on
  both CLI and MCP (MCP used to silently mean full-page).
- Fetch CLI long-help cites `--actions`, not the dead
  `--browser-actions`.

### Changed
- `donsetch config show` redacts bearer tokens and credential-bearing proxy
  values, including proxy pools, while still showing whether each field is
  configured and where its value came from.
- Browser version probes now apply one deadline to process exit and bounded
  stdout capture, terminate inherited-pipe descendants on Unix and Windows,
  and cap captured output at 64 KiB.
- Every runtime env read now flows through the typed config. The old
  env names keep their exact historical trigger semantics and still
  work in 4.0, but they are deprecated; `donsetch doctor` lists the
  ones your shell still sets and maps each to its config key. The
  hard cut lands in a later minor once the migration is done.

### Added
- **Egress fabric (v4 A):** search, crawl, fetch, and ghost share one
  process-wide proxy pool with durable lane health. Burned and dead
  lines survive restarts in `egress-health.json` (kill:
  `DONSETCH_NO_EGRESS_PERSIST`). When a pool is configured, `web_fetch`
  sticks to one exit per host and rotates on 429 / 407 / connect-dead /
  timeout; the lane stays pinned for the whole redirect chain (never
  mid-200-session). Kill: `DONSETCH_NO_FETCH_ROTATE`. Personas get an
  exclusive lane bind: a burned or foreign-persona exit is never
  reused for a new mint, and quarantine frees the lane. Per-lane RTT
  EWMA feeds search pacing and surfaces in `donsetch doctor --deep`
  as one line per lane (ok / slow / burned / dead / auth) with a named
  fix. Daemon preflight still benches dead proxies before the first
  query; crawl skips benched lanes and reports outcomes into the same
  health world.
- **Learning engine v2 (v4 B, local only):** per-(engine, intent)
  search trust EWMAs drive roster order (`search-trust.json` v2; empty
  intent maps seed every intent from engine-global history so a
  restart never re-learns a walled engine from zero). Domain profiles
  stamp which egress class their cookies were learned on; a Warm
  clearance vault is refused when the live lane is the other class
  (home-IP cookies never ride a proxy exit). Crawl host ladders
  (429 storms, robots delays) persist to `crawl-governor.json`
  (cap ~2k hosts, 7d TTL). Enrich/prefetch success density feeds a
  capped domain quality prior (`search-quality.json`, ±0.08 after
  ≥3 samples; kill `DONSETCH_NO_QUALITY_PRIOR`; walls and timeouts
  never count). Agent-outcome feedback (must_contain miss / thin /
  SoftNotFound) soft-demotes `class|host` in `outcome-feedback.json`
  (±0.05 after ≥2 misses; **default off** until the 24h soak proves
  it; enable with `DONSETCH_OUTCOME_FEEDBACK=1`; never extra fetches).
  `donsetch status` has an **improve** line
  (warm-hits, walled, cooldowns, flaky, low-trust/quarantined,
  quality hosts, outcome demotes) and `donsetch doctor --improve`
  explains the loop in ~10 lines with live local receipts and kill
  switches. No MCP tool, no telemetry, no cross-machine sharing.
  A 24h soak battery still gates any public improve claim.
- **Search v3:** adaptive early-return cancels stragglers once ≥3
  independent index families already agree on a top-3 URL (kill
  `DONSETCH_NO_SEARCH_EARLY`). Mojeek `empty-parse` (blocked as HTTP
  200) burns engine trust harder so it leaves the default width
  instead of occupying a slot every query. Compact snippets trim to
  180 chars. Query compiler splits `site:` / `filetype:` / `intitle:`
  out of the free text and only sends each operator to engines that
  honor it (DDG lite strips them so BM25 never ranks the literal
  token); post-merge filters enforce `site:` and `intitle:`, and
  `filetype:` keeps matching URLs plus extensionless download
  handlers (kill `DONSETCH_NO_QUERY_COMPILE`). `site:github.com`,
  `site:stackoverflow.com`, and friends also join the fan-out as
  that site's own API. A byte-derived SERP instant layer lifts
  featured snippets / instant answers / knowledge panels out of the
  organic list into their own slot with the source URL always
  present (never invents text; not cached; kill
  `DONSETCH_NO_SERP_INSTANT`). Code verticals are query-gated and
  stress-aware: StackExchange only on Q&A/error language, MDN only
  on web-platform tokens, HN only on release/announce; under pool
  stress HN drops first.
- **Doctor ultra (v4 D):** broader local coverage, same fast default.
  New checks: config posture (`NO_CONFIG_FILE` + `DONSETCH_CONFIG`
  conflict, missing explicit file, layer report), search health
  (engine trust, quarantine, quality/outcome receipts, BYOK key
  state counts, C kill-switch flags), clearance stores (routes.json
  counts, handles, cookie vault), crawl stores (governor host
  ladders, page-history size warning past 5MB), DNS resolution
  (independent of HTTP, names AAAA presence), and a captive-portal
  probe under `--deep` (`generate_204` must stay 204). State
  permissions now cover every secret-bearing store
  (`ghost-state.json`, `routes.json`, `byok-keys.json`) and
  auto-tighten to 0600. All new checks carry unit tests; live
  `doctor` on a real box is green with honest warnings.
- **ML-DSA ClientHello parity (v4 E1):** Chrome 151 pads
  `signature_algorithms` with ML-DSA 44/65/87 (IANA 0x0904/05/06).
  boring 5.2.0 finally names and verifies those codes, so the
  ChromeTrue wire advertises them (`mldsa44:mldsa65:mldsa87:` at
  the front). InterceptionSafe never sends them (a corporate MITM
  re-terminates without ML-DSA certs). Kill:
  `tls.mldsa_sigalgs=false` / `DONSETCH_NO_MLDSA_SIGALGS`.
- **Persona wire divergence (v4 E2):** per-domain personas now
  actually drive the wire. Tier-1 Accept-Language is persona-locale
  first (TLD/script kept as a lower-q preference so localized
  content still lands, without the classic en-US browser speaking
  perfect ru-RU JA4H tell). The ghost browser launches with the
  persona's viewport and `--lang`, and CDP `navigator.languages`
  matches. Quarantined personas fall back to the default wire.
- **Crawl polish (v4 F):** dataset JSONL rows carry
  `dataset_version: 1` in structuredContent; resume-token TTL is
  `fetch.resume_ttl_secs` (default 7200, clamped 300..86400) instead
  of a hardcoded 2h. Cross-process host politeness: crawls share a
  per-host last-stamp file under `<cache>/host-pace/` so a daemon and
  a CLI crawl of the same host respect one floor gap (kill:
  `DONSETCH_NO_HOST_PACE_FILE`; corrupt file = ignore). Canonical/
  binary skip and transient-vs-permanent retry were already on
  master; reconfirmed with tests.
- `just win-check`: type-checks the crate for `x86_64-pc-windows-gnu`
  from Linux (clippy, no linkage), both `--no-default-features` and
  the full feature set, so `#[cfg(windows)]` breakage from a
  Linux-only change surfaces before the push instead of in Windows
  CI. Needs mingw-w64; see CONTRIBUTING.md.
- One typed runtime config (`src/config.rs`, issue #193): every knob
  lives in a layered struct, defaults < `donsetch.toml`
  (`DONSETCH_CONFIG` or `<config-dir>/donsetch/donsetch.toml`,
  skippable via `DONSETCH_NO_CONFIG_FILE=1`) < `DONSETCH_<SECTION>__<KEY>`
  env names. Values validate loudly at load; unknown TOML keys name
  the offending file. `donsetch config show` prints each knob with
  its value and origin (`--markdown` for the reference table,
  `--legacy` for the old-name map); `donsetch doctor` warns about
  deprecated env names active in the shell.
- Resurrection reaches transport-dead URLs: a TLS handshake against
  a parked domain, a dead DNS name and a refused port now gate
  `archive=auto` resurrection the same way 404/410 do, while
  timeouts, resets and protocol errors stay excluded (a snapshot
  must never launder an unknown, and a reset can be an IP-level
  block that an archived copy would paper over). The gate reads a
  new `structuredContent.fetch_error` transport class recorded on
  every status-0 fetch error.
- Wayback snapshot serving survives wayback's own redirect
  interstitials: a thin capture is re-checked against wayback stub
  markers, and a stub chain-hops through up to 4 `<meta
  http-equiv="refresh">` targets when (and only when) wayback
  rewrote the target (`web.archive.org/web/<14-digit-ts>/...`); a
  live-web refresh target is never followed, because the URL we
  are resurrecting may still be dead, moved, or hostile.
- `web_screenshot` MCP tool: a rendered PNG of a page through the
  existing tier-2 browser (url, full_page, wait_ms). The capture is
  in-process only; the MCP result carries an image content block and
  a text note. Verified live against a real render (PNG header,
  image/png). The CLI ships the same render now that it was wired up:
  `donsetch screenshot URL [--full-page|--wait-ms N]` prints the
  receipt and, with the CLI-only `--out PATH`, writes the PNG with
  honest failure paths (bad path or missing path = stderr + exit 1).
- h3 lane hardening (mnaza, PR #169): every transport now exits
  through one point, so decompression and wall detection also apply
  to HTTP/3 responses (a challenge served over h3 no longer reports
  as clean content); alt-svc routes honor the server's own `ma=`
  lifetime instead of a constant; the HTTP/3 body reader stops at
  the shared 64 MiB cap before allocating; `routes.json` carries
  serialized TLS session material and lands owner-only (0600).
- Experimental HTTP/3 lane (opt-in via `DONSETCH_H3=1`) on a quiche
  0.29.3 fork that shares one BoringSSL build with tier-1. An h3
  route is learned from `alt-svc` response headers (same-origin
  only, proxies exempt) and persisted in the cache dir under
  `routes.json` together with the serialized QUIC TLS session. The
  lane stays off by default: on repeat visits the v1 one-shot
  connection shape measures slower than our reused h2 pool, and
  `DONSETCH_NO_H3` kills the whole path regardless. The h3 Client
  Hello reuses the same Chrome-true TLS builder as h1/h2.
  Transport parity verified live against curl --http3 on a
  1.4 MB page (852 ms vs 825 ms) and 0-RTT resumption arms and
  is accepted on repeat visits; the default stays on our reused
  h2 pool, which still measurably wins the same-or-faster gate.

- Local web memory for agents (`web_memory` tool + `donsetch memory`
  query): a bounded on-device index of pages this machine fetched.
  all-MiniLM-L6-v2 quantized runs locally on the same onnxruntime
  the search reranker uses; the model + tokenizer download once,
  sha256-pinned, into the cache dir and nothing ever leaves the box
  after that. Fetches, crawl pages and search snippets ingest
  automatically; `DONSETCH_NO_WEB_MEMORY` disables ingest and search;
  `DONSETCH_WEB_MEMORY_CAP` bounds the index (default 4000 rows,
  oldest-first eviction). Unavailable on builds without the rerank
  feature: the release builds all carry it.

- Adapter registry v1 (`donsetch adapters`): the named rewrite and
  extract adapters are now a registry, and operators can add more
  fetch-level rewrites as pure-data JSON plugins in
  `cache_dir()/adapters/` (one file per rule: hosts, optional path
  prefix, an https target template with one `{path}` placeholder;
  no executables or scripts, the fetcher's egress guards still
  apply). Bad files are skipped with a receipt and never break
  fetching; `DONSETCH_NO_ADAPTERS=1` stays the master kill switch.


- Native keyless Google search via the legacy mobile endpoint, using
  DonShadow without a browser or paid API. Seven selectable Nokia
  profiles (`DONSETCH_GOOGLE_PROFILE`, default `6230-03.15`), observed
  working in local tests; availability is not guaranteed. An explicit
  CAPTCHA permits one next-profile attempt in the existing thin-merge
  retry wave; successful profiles remain preferred per egress in
  memory, with circular advancement on CAPTCHA and no profile cooldowns.
  Profile selection resets on restart. HTTP 429 and other walls do
  not rotate. Native and
  browser Google share one ranking family but keep separate health.
- Route memory: fetches learn route health per tier (EWMA latency by
  route, failure classes, LRU-bounded store). Tier 1 rides healthy
  routes only; persistent failures quarantine and a background probe
  re-heals them when the block lifts; a replaced route never re-enters
  while still marked failed. Kill switches + `donsetch status` line.
- Persona store: long-lived per-domain identity records (fingerprint
  class, headers, session state) ride inside ghost pages across runs,
  with a coherence checker and a `donsetch status` line.
- Stealth scorecard: `doctor --stealth` grades the tier-1 battery
  (TLS/H2/header classes) with conflict codes; `--parity` diffs the
  live fingerprint against real local Chromium (JA4/JA3/H2/vector
  agreement). Weekly `stealth.yml` CI alarm on profile drift.
- Tier-1 request realism: navigation-class header sets per request
  class (document/rpc/embed), subresource shadow-fetching that
  reproduces real page-load traffic (assets behind the primary fetch),
  and tier-1 cookie persistence replaying returning-device sessions
  across runs. Each subsystem carries a kill switch.
- Prewarm: search hands a warm tier-1 pipe to the first-result fetch
  that follows it (hot-path time drops from roughly 550 ms wire
  build-up to about 25 ms served from RAM). Served/wire split is
  visible in `donsetch status`.
- `web_answer`: evidence-pack answer tool for chat-first MCP clients
  (query + token budget, up to a handful of pages, ranked citation
  graph, one-line answer). Kill switch `DONSETCH_NO_ANSWER_TOOL`.
- Crawl dataset mode: `--json` renders one JSON object per page
  (url/title/kind/markdown/chars/fetched_at/lastmod/parent), sorted by
  URL, deduped; skipped pages carry reasons. Best for machine-ready
  exports; plain text stays the default.
- Crawl delta recrawl: `--since-last` re-fetches pages and compares
  content fingerprints against crawl history; unchanged pages refresh
  history but drop out of results and budgets with skip reason
  "unchanged since last crawl". Replaces the old fixed 24 h window
  that silently missed quiet mutations.
- Crawl-shape: seeded reader-like frontier ordering (head-window
  jitter over the top ranks, topology preserved), so repeated crawls
  of one site stop replaying an identical mechanical order to access
  logs. Kill switch `DONSETCH_NO_CRAWL_SHAPE`.
- Adapters reach the crawl fetch path: the same rewrite pass
  `web_fetch` uses rides inside crawl fetches; the canonical URL stays
  the dedup/history key.
- Ghost browser pool: up to 16 warm browser slots keyed by persona
  identity and host affinity; a repeat hit on the same host lands on
  the browser that already carries that site's session state, and a
  persona switch inside a slot relaunches instead of inheriting
  another identity's fingerprint state. `donsetch status` shows the
  warm-serve receipt; `DONSETCH_GHOST_POOL_SLOTS` sizes the pool
  (default 3), `DONSETCH_NO_GHOST_POOL` reverts to the old
  single-slot path.
- MCP compat folding applies to `web_search` too: structured-content-
  only clients receive raw result URLs through the folded metadata
  block instead of losing them; the model-facing contract text now
  states it. (#165)
- Clarify compact search labels: counts describe search-index families
  that returned a URL, not independent sources corroborating its
  claims. The weak results message now refers to cross-index
  agreement. Ranking is unchanged. (#166)

### Changed

- Crawl pacing cleanup: the vestigial 0-100 ms skim dwell is deleted;
  self-inferred waits cap at 7 s while host-declared waits (Retry-
  After) stay uncapped and honored. All pacing lives in the crawl
  governor, pressure-adaptive.
- Dependencies: tokenizers 0.23.2, encoding_rs 0.8.40, brotli 9.0.0,
  zstd 0.14.0, psl 2.1.231, actions/checkout 4 -> 7.

### Fixed

- Security review batch (#216-#219):
  - `donsetch doctor` Bright Data zone probe hit `/zone/route_ips` twice
    (a doubled path that 404'd every configured zone). The ship path now
    passes the API root and builds the URL through one shared helper.
  - h3/QUIC dial now applies the same connect-time SSRF filter as h1/h2:
    a DNS rebind to a private/loopback address between the request-time
    check and the dial is refused. `DONSETCH_ALLOW_PRIVATE_EGRESS` still
    works as the explicit escape hatch.
  - BYOK provider error bodies are capped at 600 chars before they reach
    the agent, stderr, or the debug log (context DoS). serpapi additionally
    scrubs the URL-borne `api_key` from a reflected error body (key leak).
  - Crawl resume tokens that are not plain ASCII alphanumeric are refused
    before they index the filesystem, closing a path-traversal read-then-
    delete on agent-supplied `resume`.
- Browser backend aliases now share the typed enum as their single registry
  and normalize casing and surrounding whitespace across config sources.
  Unknown effective legacy values fail closed instead of silently becoming
  `auto`; a valid higher-precedence modern setting still overrides them.
- The ONNX reranker now consumes the validated `search.rerank_threads` value
  directly instead of leaking and reparsing a string, and reports configured
  values as coming from the layered config. `0 = auto` behavior is unchanged.
- Master hardening wave (overnight full-tree audit, every finding
  reproduced before fixing):
- h1: a connection that closes before Content-Length is satisfied now
  fails the transport instead of returning the partial body as
  success. The old reader scored truncated pages clean and stored
  them in the revalidation cache as Fresh, so every later fetch of
  that URL served the truncated page forever. The fix caught a lying
  Content-Length in my own egress-proxy test rig live (it claimed 12
  bytes and sent 10).
- h3: a QUIC connection that closes or drains mid-body is an error,
  same truncated-success class. The route drops and h1/h2 answer.
- Cookies: a hostile `Set-Cookie` with a multibyte month token in
  `Expires=` panicked the date parser, and with panic=abort in
  release that is a one-request remote kill of the daemon. Date
  tokens now parse by bytes. Cookie path matching uses the URI path
  only (a path-scoped cookie used to stop attaching on query URLs,
  against RFC 6265 and every browser). The jar enforces the RFC 6265
  section 6.1 bounds (4096-byte cookies, 100 per domain, 3000 total,
  4 KiB / 50-pair Cookie header), so a Set-Cookie flood can neither
  grow the jar for process lifetime nor emit megabyte headers.
- Ghost session vault: a live daemon's state save no longer erases
  fresh harvests or resurrects logged-out sessions. Vault writers
  (the logout clear, the session store, the tier-1 sync) stamp a
  vault epoch, and every save adopts whichever side wrote last.
- Crawl resume-only (empty url + resume token) consumed the token
  twice, always failed with "resume token expired or unknown", and
  destroyed the saved state in the process. One take, and the
  resume-only flow works.
- Search hot paths no longer initialize the reranker: the first
  `rerank::active()` on the search path blocked a tokio worker on
  the whole init chain, up to a 120-second model download. Report
  stamps now read a non-initializing check; ranking still
  initializes inside its own spawn_blocking.
- h2: a bodyless response whose header block arrives fragmented
  (END_STREAM on HEADERS, END_HEADERS on CONTINUATION) no longer
  hangs to the 30s timeout; a missing or unparseable `:status` is a
  transport error instead of a success with status 0; an
  unfragmented header frame honors the same 256 KiB block cap as
  continuations; response header names are lowercased like h1 so a
  nonconforming peer's mixed-case `content-encoding` cannot skip
  decompression and its `alt-svc` cannot skip h3 discovery.
- TLS: brotli certificate decompression is bounded by a take() cap;
  a hostile compress_certificate stream used to decompress to its
  full (potentially gigabyte) end before the declared-length check
  fired.
- Wall detection: bare prose mentions of PerimeterX, Imperva,
  Incapsula, Sucuri and Wordfence on healthy 200 pages no longer
  score Challenge (each false positive burned a warm retry, route
  memory and a paid bypass call). The challenge-specific markers and
  error-status co-signals still fire, pinned by tests.
- Revalidation cache: `Cache-Control: no-cache` is honored (stored,
  never served fresh); entries are keyed by cookie lane, so a
  jar-less search lane can never be served a logged-in fetch's body
  or the reverse; a 304 whose entry was evicted mid-flight fails
  honestly instead of scoring an empty body as Blocked.
- Crawl: a redirect out of the seed's host, scope, or robots rules
  is skipped instead of landing in results, dataset and history
  under its final URL. Frontier normalization re-encodes query
  strings (distinct URLs with encoded `&`/`=` collided in the
  seen-set and one of them silently never fetched). A hostile
  `<priority>NaN</priority>` can no longer poison a resume token.
  Too-deep items are skipped with a reason instead of aborting the
  whole crawl and discarding the frontier. A host's declared
  Crawl-delay is honored in full above the 7s self-inferred cap (a
  host declaring Crawl-delay: 30 actually sees 30s pacing), stored
  per host so concurrent crawls stop cross-polluting each other's
  pacing. The governor prunes idle per-(host, lane) clocks instead
  of growing them forever.
- MCP supervisor: a failed replay write followed by new client data
  no longer drops the held bytes; the held request folds into the
  new replay window.
- MCP tools: the fetch batch honors cancellation (a cancelled
  12-URL batch no longer runs every escalation to completion);
  web_screenshot honors deadline and cancellation with a 60s
  ceiling; an all-failed fetch batch classifies permanent vs
  transient like the search batch instead of always claiming
  "safe to retry"; a hostile multibyte wayback timestamp no longer
  panics a byte slice; oversized stdin request lines are bounded
  and answered with a parse error instead of growing the reader
  without bound.
- Search: SERP bodies cap at 3 MiB before the DOM parse (one broken
  proxy answering 200 with tens of MB stalled the whole fan-out
  past its deadline). BYOK results carry single-flight, so two
  concurrent identical queries bill the metered provider once, and
  the byok cache stores the provider's full top-12 (a first search
  at max=2 no longer serves later max=10 calls a 2-row slice as
  cached). Provider error bodies cap at 600 chars. `+` survives
  cache-key normalization, so "rust vs c++ performance" and "rust
  vs c performance" stop sharing one cache entry. The search cache
  serializes to disk outside its lock. The ddg_html retry lane
  reads its learned trust. Short API keys are never printed whole
  by `keys list`.
- Adapters: the JSON fast path honors focus/toc/must_contain/section
  (must_contain on an adapter-shaped URL used to hand the agent the
  full document against its own contract). PyPI normalization
  follows PEP 503 separator runs. The plugin loader opens the file,
  stats the handle and bounds the read: a swapped FIFO could hang
  the first rewrite and a swapped file bypass the size cap.
- h3 lane: DNS resolution runs off the reactor with the same 10s
  bound as the h1/h2 path; the platform trust store parses once per
  process instead of per request; the profile's accept-encoding
  rides the h3 request (bodies arrive compressed and the wire
  matches Chrome); a server-declared alt-svc ma= is capped at 30
  days; TLS session warm detection uses the same key the session
  store writes, so TFO actually arms on non-default ports and
  proxied lanes.
- Hygiene: handles.json is written owner-only (interned link URLs
  can carry query-string credentials); finished shadow-burst
  handles are reaped in daemons; timed-out CDP calls release their
  pending slot (a wedged tab used to leak one sender per call); a
  Page.navigate failure surfaces its real cause instead of a
  generic 20s timeout; egress dead-proxy and auth-fail reports use
  poison-recovery locks; config file-layer errors stop labeling
  themselves "invalid env value"; a non-UTF-8 modern env value
  warns instead of vanishing; `DONSETCH_CONFIG=""` is treated as
  unset; `donsetch help` no longer routes the removed answer/memory
  commands; the search help lists the real intent verticals.

- HTTP bearer configuration now fails closed: modern TOML/env tokens reject
  whitespace and non-visible bytes without echoing the secret, while legacy
  text tokens retain their exact historical value instead of being trimmed
  into disabled authentication. CORS/auth validation also runs before daemon
  work.
- `donsetch config show --markdown` now emits one complete Markdown table per
  config section, so section labels no longer turn the following knob rows
  into plain paragraphs.
- Modern `DONSETCH_<SECTION>__<KEY>` variables now fail loudly when a
  recognized value is not UTF-8 or when distinct names normalize to the same
  config key. Diagnostics identify the variable names without exposing their
  values, and collision handling no longer depends on environment iteration
  order.
- Typed config validation now rejects an out-of-policy TOML value with
  file attribution even when a valid higher-precedence env value would
  otherwise hide it; legacy out-of-range values remain warning-only.
- Browser path overrides now preserve source presence across the typed config:
  legacy empty values keep their historical override semantics, while modern
  empty values reset to discovery or ambient defaults. Explicit Chromium,
  CloakBrowser and Playwright paths remain literal.

- Legacy route-memory and bypass-cache controls compose in their original
  order under the typed config: the route-memory kill switch beats read-only,
  while a valid legacy bypass TTL applied after `DONSETCH_BYPASS_CACHE=0`
  re-enables the cache (and TTL zero disables it). TOML and modern env
  overrides retain their higher precedence.

- The supervisor's replay now survives a crash loop: the bytes
  replayed into a replacement were cleared from the unacked
  history, so a second silent death dropped them (reported by
  mnaza in #202, discriminating two-death test included). The
  live unacked history is also bounded to the same 1 MiB replay
  window on every append (keep the tail), not only at death time.

- The browser probe timeout now kills the whole process group, not
  just the parent: a wedged browser whose descendant held the stdout
  pipe used to hang `donsetch doctor` forever. The probe now fails
  at the timeout, bounded, on every platform.


- The MCP supervisor no longer loses a request buffered into a
  child that dies before consuming it. The write succeeds while
  the child is alive, so no EPIPE ever fires, and the death only
  surfaces on the next idle poll: the supervisor now replays the
  child's whole unacked history (bounded to 1 MiB, duplicated
  delivery preferred over a lost request). Caught by the macOS CI
  run timing the crash between a successful write and the idle
  poll.
- Crawl resume tokens survive concurrent crawlers. The store was
  one shared JSON map saved with load-modify-save, so two
  processes issuing tokens at the same time (the daemon plus a
  CLI run, parallel test processes) wrote stale copies over each
  other and fresh tokens read back as expired or unknown. Tokens
  are now one immutable file each under the cache dir; the legacy
  single-file store migrates on first touch, retires only after
  every entry lands, and the consume-on-resume semantics are
  unchanged. Caught by the Windows CI run while it beat on the
  shared store in parallel.
- `donsetch doctor --deep` no longer reports a valid Bright Data
  dynamic-IP unlocker (Web Access API zones and friends) as broken:
  the free zone probe costs nothing, and where the zone has no
  static route pool Bright Data answers 403 "Static routes not
  found". That answer no longer reads as a failed check: the
  probe skips with an honest, zero-credit note instead (reported
  by tripflex on #200). A 401 always stays a failure.
- Resurrection no longer claims "never archived" on shaky ground:
  the availability API is lossy and scheme-strict (a capture
  recorded under `http://` is invisible to an `https://` query), so
  an empty answer now falls through to the complete CDX index
  (scheme-canonical, `filter=statuscode:200`, last 5 captures)
  before `archive=only` reports anything, an unreachable archive
  answers `transient` with its own message instead of a false
  `permanent`, and a found-but-unusable snapshot names itself and
  its stage (`structuredContent.archive_stage`) instead of
  collapsing into the live error.
- `site:` queries no longer leak off-domain results through BYOK
  providers (issue #190): both BYOK exits (provider-first and the
  local-first fallback) sweep results through the same post-merge
  domain filter the local engine uses; goto/redirect proxies drop
  (fail closed), and the warm body prewarm runs on the rows that
  survive the filter.
- `donsetch login --logout DOMAIN` now also wipes the rendered-DOM
  cache of the logged-out domain (Mart-Bogdan, PR #188): a page
  fetched behind the session was a fourth persisted copy of the
  session, served back with no network hop for up to five minutes
  after logout. Unrelated domains' renders stay.
- Repeat BYOK searches now ride the same TTL'd disk cache as keyless
  results instead of re-billing the provider (issue #195): same query
  + intent replays from cache under a separate byok namespace, capped
  at 500 entries like the local path (mnaza, #197).
- The web-memory index persist stages to a PID+sequence-unique
  file per write (PR #191): overlapping persists (a crawl fires
  one per 256-row chunk and one on completion) can no longer tear
  `index.json` through a shared tmp inode and parse-fail the next
  recall to an empty index.
- Subresource shadow-fetching no longer aborts the daemon on a page
  containing `İ`, `K` or `Ω`. The scanner searched a `to_lowercase()`
  copy of the document for tag offsets and then sliced the original
  string at them, but `to_lowercase` is not byte-length preserving, so
  one such character shifted every later offset: assets silently
  dropped where the shift landed on ASCII, a mid-codepoint slice panic
  where it did not : and the release profile's `panic = "abort"` turns
  that into a daemon kill. Folding is `to_ascii_lowercase` now, which
  is byte-length and char-boundary preserving, and is also what the
  HTML standard specifies for tag and attribute names. `attr()` had
  the same latent pattern and is fixed with it.
- MCP text-only fold now covers OpenCode v1 (tested on 1.18.3):\
  unlike Claude Code / VS Code, OpenCode renders the `content` array
  and drops `structuredContent` entirely, so agents previously lost
  every compact state field (URL handles, `next_offset`, verdicts,
  error codes). Handshake detection now matches `opencode` and folds
  the state into the leading `[meta]` text block, same as the other
  text-only clients.
- A corrupt local embedding model no longer wedges every subsequent
  fetch. `ensure_file` recursed into itself with identical arguments
  when the file on disk failed its SHA/size pin, never removing it, so
  the same bad bytes were read forever : a stack overflow, or an
  infinite loop if the recursion was optimized into a tail call. The
  bad file is removed and the existing atomic download path takes
  over; a concurrent remover is tolerated with a receipt, any other
  removal failure is honest and immediate.
- Default-feature builds compile again (Mart-Bogdan, PR #174): the
  web-memory status receipt tested the rerank feature with a runtime
  `cfg!` so no-rerank builds failed the compile on three symbols
  behind the feature gate.
- `donsetch login --logout` now wipes the tier-1 echo of the vault
  from ghost-state.json, not only the registries the sessions
  replays through on the next start (issue #173).
- `routes.json` was rewritten on every alt-svc sighting, could grow
  without bound, and had no switch: re-vouches with nothing
  materially new (under a 60 s grace) are skipped before the disk
  write, expired rows never survive a persist, the row count caps at
  512, and `DONSETCH_NO_ALT_SVC` shuts the bookkeeping off (issue
  #175).
- Web memory re-embedded and rewrote its whole index once per row,
  putting up to one 20MB+ write on the answer path per hit: batch
  ingest embeds in one pass and persists once, the crawl path chunks
  at 256 rows, and ingestion runs on the blocking pool so a recall
  never holds a response past its deadline (issue #178).
- CI runs now concurrency-cancel per pull request only, never on
  master (Mart-Bogdan, PR #181).
- The alt-svc kill-switch test no longer excludes Windows
  (Mart-Bogdan, PR #189): the env-var mutations in it are not
  Windows-specific, so the guard was dead weight.

- Xvfb reuse gate now demands a bounded real-protocol answer
  (xdpyinfo within 2s) before handing a display to the pool, so a
  SIGKILLed Xvfb's tombstone socket can no longer wedge every
  tier-2 escalation behind an honest "devtools ws timeout".
- Ghost pool spill fix (mnaza): a same-persona pool previously
  funneled every distinct host into slot 0, since "any warm
  same-persona slot" outranked free slots and the daemon always runs
  one persona. Slot claims (persona key + host) are now stamped and
  visible to concurrent selectors at pick time under the meta lock,
  before the seconds-long browser launch, so a second in-flight host
  claims a free slot and a same-host job joins the in-flight claim
  and warm-serves it. An exhausted pool reuses our coldest own
  browser before evicting a stranger's. Their contribution lives as
  its own rebased commit on master (af13034).
- Search pacing uses cancellation-safe per-engine/egress admission;
  waiting is included in attempt deadlines and cancelled waiters
  leave no future-slot debt. Google HTTP health is isolated from
  legacy browser health; Google follows common failure and quarantine rules.
- Search retries retain completed peers when another retry times out
  and report retry timeouts explicitly, with the attempted Google
  profile when available. At most one retry per engine per search. Ordinary
  retries, including Google, keep a three-second budget including pacing.
- **#164 audit wave (S1-S6 in the search/fetch stack):**
  S1: version matching is boundary-aware; "5.2" no longer matches
  "15.2" or "5.20" but still matches "5.2.1" and "v5.2".
  S2: empty vertical results report no-results instead of success;
  engine OK counts and retry/cache gates were inflated.
  S3: plugin-hit truncation before URL dedup dropped, so oversize
  plugin output can no longer shrink the final unique count.
  S4: cache entries keep engine reports; cache hits return real
  engine evidence (old 4-tuple caches still load).
  S5: single-URL fetch with `budget_tokens` runs under
  `run_with_budget`: `deadline_ms` and MCP cancellation apply, and
  the budget bounds the page like batch mode.
  S6: BYOK plugin error envelopes cap at 600 chars, matching the
  stderr trim.
- Storage guard: bounded target-dir growth and pinned the cargo
  profile on every nextest run.
- Ghost-state counters now merge against the state on disk at every
  save, so a late save with a stale in-memory snapshot can no longer
  rewind lifetime counters (caught live: the pool warm-serve receipt).

## [3.6.7] - 2026-09-07

### Fixed

**Audit wave (2026-09-07): every finding from the post-3.6.6 refactor
audit, verified live before fixing, discriminating tests on the fixes.**

- **S1 (security):** the certificate-decompression callback reserved
  the SERVER-declared uncompressed length upfront; a hostile origin
  could declare 4 GiB and trigger the reservation before any byte
  arrived. Refused above 16 MiB (far past any real chain),
  discriminator-tested with the callback directly.
- **B2:** conditional revalidation headers (If-None-Match/If-Modified-
  Since) minted for the original URL rode every redirect hop; a
  colliding ETag on the target produced a false 304. Fixed + proven
  with an E2E rig: pre-fix the second hop carries the stale validator
  and the caller merges the wrong cached body; post-fix the redirect
  target answers a real 200. Reverting the fix fails the test.
- **B6:** the cookie export (`snapshot_for`) hard-coded `path: "/"`,
  widening path-scoped cookies on the export/import cycle. The real
  path is carried now (regression test).
- **B1:** the search single-flight follower read the cache mutex with
  a poison-panic; one transient panic mid-lock would outage every
  search in the daemon. Poison-safe like every other access.
- **E15 (curl parity):** the env proxy is re-evaluated per redirect
  hop against NO_PROXY; a redirect to a NO_PROXY-covered host dials
  direct instead of riding the proxy for the rest of the chain.
  Discriminator test: post-fix the second hop arrives origin-form at
  the direct server, pre-fix it arrives absolute-form at the proxy.
- **E4 (RFC 9112 6.3):** differing Content-Length values on one
  response are rejected as invalid (request-smuggling class); the
  same value repeated stays tolerated (HTTP/1.0 proxy reality).
- **E14:** the cookie `Expires=` date form is parsed now (IMF-fixdate
  + RFC 850 + asctime, RFC 6265 5.1.1 tolerance); date-expired
  cookies no longer live as session cookies or leak into the vault
  export. Max-Age keeps precedence. Canonical-vector tested.
- **E13:** an unparseable cookie Max-Age is IGNORED (RFC 6265 5.2.2:
  session cookie), not turned into a 1-second cookie.
- **E12:** `Vary: *` responses are never stored in the revalidation
  cache (Chrome parity; a stored variant would serve stale forever).
- **E11:** revalidation-cache eviction is FIFO by insert order, not
  an arbitrary HashMap victim.
- **E10:** layered `Content-Encoding` (`gzip, br`) peels both layers
  instead of a hard fetch error.
- **E1 (curl 7.86 parity):** NO_PROXY understands bracketed IPv6 and
  bare IPv6 literals, CIDR networks (`192.168.0.0/16`) and
  `host:port` entries.
- **E5:** unsupported proxy schemes (`socks4://`, `https://` upstream
  proxies...) are rejected at parse with a clear message instead of
  parsing as HTTP and dying at dial time with a confusing error.
- **E9:** the query-cache recency window for year mentions is
  generated from the clock ([current-2, current+1]); the hardcoded
  2024-2027 list would have made every 2028 query cache as evergreen.
- **E7:** the query-cache key embeds a stable u8 intent code instead
  of the Intent Debug string (renaming a variant used to remap or
  orphan old cache entries).
- **E16:** the BMP magic check now requires the declared file-size
  field to be plausible against the body length; a plain-text
  document starting with "BM" no longer classifies as binary.
- **E19:** the raw-text fallback thresholds are named shared consts.
- **L1:** the SSL_CERT_FILE/SSL_CERT_DIR bundle is cached keyed on
  (path, size, mtime); was re-read and re-parsed on every connector
  build (every fetch) in interception networks.
- **L7:** the async DNS-aware SSRF gate runs exactly once per
  request (inside the request path); the outer gate kept only the
  synchronous literal checks for cache-fresh returns, and the
  redirect hop's duplicate pre-gate is gone. Was 2x resolver RTT
  per fetch and per hop.
- **L2:** h1's naive `windows().position()` scans replaced by
  memmem (sublinear); the old loop was quadratic on slow-drip
  header responses.
- **L4:** the engine-health disk save is debounced by a dirty flag;
  was a clone + serialize + write on every uncached search.
- **L9:** the SPA-shape detector counts `aria-busy` markers with an
  ASCII-case-insensitive scan instead of lowercasing the whole
  document.
- **Q1:** unparseable proxy lines are counted and surfaced in
  `donsetch status` ("N invalid line(s) ignored") instead of being
  dropped silently.
- **Q3/Q4:** the wall-classification verdict is scored inside the
  response-finalizer (one site of truth instead of every caller
  re-detecting), and the egress label chain is a mapping function.

### Added

- **Structured-content compat for Claude Code / VS Code (issue #27,
  thanks Mart-Bogdan + maykura):** those harnesses show the model only
  `structuredContent` and drop the `content` array, so agents saw
  fetch/crawl metadata but never the page markdown. DonSeTch now
  detects them at the MCP handshake (`clientInfo.name`, matched
  case-insensitively against a known list; Claude Code's object-shaped
  `version` is tolerated) and, for those sessions, merges the two
  surfaces: the full structured state folds into a compact leading
  `[meta]` text block, the document stays a clean markdown text block,
  and `structuredContent` is omitted. web_search is exempt (its
  structuredContent is the richer surface). Every other client keeps
  the unchanged token-optimal split. Manual override:
  `DONSETCH_MCP_TEXT_ONLY=1` forces the compat shape for any client,
  which is also the escape hatch for newly discovered broken hosts.
  Compat mode is per-session on the HTTP transport; error results
  carry their stable code + escalation trace through the same
  `[meta]` fold. Verified end to end over both stdio and streamable
  HTTP against the real binary (default shape, compat shape, error
  path, search exemption, per-session isolation, env override).



### Fixed

- **The supervisor crash-recovery outlived the 3.6.6 SIGPIPE
  restoration** (mnaza, #163): pinning SIG_DFL process-wide killed the
  `mcp --supervised` parent exactly when a crashed child's stdin write
  returned EPIPE (the signal it exists to survive), and bypassed the
  stdio transport's graceful broken-pipe shutdown and the BYOK
  plugin's tolerant child-write path. CLI commands keep the quiet
  exit-141 pipe convention; `mcp` and the supervisor pin SIG_IGN for
  themselves. Discriminating test: on 3.6.6 the test process dies by
  signal 13, with the fix it passes.

## [3.6.6] - 2026-09-06

### Fixed

- **Crawl robots.txt `Crawl-delay: inf` aborted the crawl worker:**
  `Duration::from_secs_f64` panics on infinite or overflowing values,
  so a host declaring an absurd delay killed the lane. Delay is now
  clamped (finite + capped 60s) at every setter and re-clamped in the
  pacing math. Credit: mnaza (#155).
- **Sitemap `<loc>` URLs were used with XML entities intact:**
  `&`, numeric/char codes (`&#x27;`, `&#39;`) and CDATA wrappers
  stayed raw in every fetched URL, breaking query-string pages. Own
  minimal entity decoder with bounds; CDATA stripped inside `<loc>`.
  Credit: mnaza (#156).
- **MathML table-of-parts shapes were invisible in extraction:**
  layout tags (mtable/mtd/msup etc.) were absent from the recursion
  guard's tag list. Credit: mnaza (#157).
- **HTTP/1.1 1xx interim responses returned as the final response:**
  100 Continue/103 Early Hints from CDNs got mistaken for the real
  status, handing callers a hint block with an unframed body. Interim
  blocks skipped (a 1xx-class counter caps the stream; unexpected
  101 upgrades refused). Credit: mnaza (#158).
- **RSS/Atom close tags were matched case-sensitively** while the
  open already wasn't: `<LINK>` left a row half-parsed. Close scans
  are case-insensitive ASCII now (the open already were). Credit:
  mnaza (#159).
- **`tls.egress` was dead code:** the TLS classifier's two hints share
  the substring `SSL_CERT_FILE`, and the egress arm ran behind the
  cert-trust arm, so every intercepted-transport error landed on the
  cert guidance. The classifier's own opening sentences now drive the
  split, and the test composes both through the real function so the
  pair can't drift. Credit: mnaza (#160).
- **Plaintext HTTP via an authenticating proxy sent no
  Proxy-Authorization, and SOCKS5 tunnels carried absolute-form
  request lines to the target server** instead of origin-form. Both
  fixed with one shared `proxy_authorization()` builder + correct
  in-tunnel form. Credit: mnaza (#161).
- **One non-UTF-8 line in Chrome's stderr failed the whole ghost
  launch.** stderr is scanned lossy now (byte-level scan for wall
  markers), only the real failure paths depend on exact text.
  Credit: mnaza (#162).
- **`donsetch --help | head` printed a panic backtrace after head
  closed the pipe** (Rust masks SIGPIPE; the write's EPIPE panics).
  The default disposition is restored at start: silent exit 141 like
  rg/curl. The MCP daemon inherits it: a broken transport pipe means
  the client is gone, so an instant exit beats a stack dump.
- **The 3 tool subcommands leaked the agent-facing MCP description
  into `--help`** (LLM-voice paragraphs). Each now carries a short
  human description of its own: same facts, terminal voice.

## [3.6.5] - 2026-09-06

### Added

- **Fetch now survives TLS-intercepting egress networks (issue #154,
  thanks maykura):** cloud sandboxes and corporate networks often
  force all traffic through an HTTP proxy that re-terminates TLS with
  its own CA. Three fixes close the whole class: (1) `SSL_CERT_FILE` /
  `SSL_CERT_DIR` bundles are loaded into the trust store on every
  connector build (PEM bundles, DER files, directory scans), the one
  thing that makes re-signed certificates verifiable; (2) requests
  routed through an HTTP CONNECT proxy switch to an
  interception-safe handshake (no GREASE/permute/ECH/ALPS/cert
  compression/OCSP/SCT), because the middlebox's second TLS stack is
  what resets exotic ClientHellos, and stealth is moot behind a MITM
  anyway (SOCKS5 keeps the Chrome-true profile, TLS rides
  end-to-end); (3) the env-proxy convention is documented with a
  kill switch: HTTPS_PROXY/HTTP_PROXY/ALL_PROXY + NO_PROXY are
  honored, DONSETCH_NO_ENV_PROXY=1 disables them, and plaintext
  http:// through an HTTP proxy now uses raw dial + absolute-form
  request targets (the previous CONNECT-then-relative-form path was
  broken).
- Handshake failures now classify instead of dumping raw
  MidHandshakeSslStream debug fields: egress resets and cert
  verification failures each get a one-line message naming the exact
  fix (export the proxy, export the CA bundle), with matching
  `tls.egress` / `tls.verify` error codes and operator-level
  next_action text.
- `donsetch doctor` adds a "Fetch egress" check: resolved env proxy,
  kill-switch state, system + environment trust-store counts, and
  SSL_CERT_FILE loadability. The network check, when it fails while
  proxy env vars are set, now says the interception fix instead of
  "check your connection".
- E2E MITM test battery (tests/egress_proxy.rs): in-process HTTP
  CONNECT proxy with a re-signing TLS server proves trusted-CA
  success, untrusted-CA honest failure, and absolute-form plaintext,
  on the real Fetcher.

### Fixed

- **Crawl link/feed extraction could panic or drop URLs on pages
  with case-folding characters:** the extractors lowercase the whole
  document to fold tag names, then index the ORIGINAL at offsets
  measured on the copy. Unicode folding is not length-stable ('İ' U+0130
  = 2 bytes but lowercases to "i̇" = 3), so links after the first
  folding char were sliced mid-character (a str-slice panic: daemon
  abort in release) or one byte late (wrong span: RSS/Atom feed URLs
  silently dropped). Canonical/base/feed extraction now scans the
  original bytes with ASCII case-insensitive matching (tag and
  attribute names are ASCII, and ASCII folding is length-stable),
  which also removes the per-page whole-document lowercase
  allocation. Two discriminating tests: pre-fix one panicked and
  one dropped both feed URLs; post-fix both correct.

- **h1 chunked reader no longer allocates unboundedly:** a server
  that never sends the chunk terminator (or keeps the size line
  growing) drove an unbounded buffer in the response path. Chunk
  size lines and trailer sections are now capped. Credit: mnaza
  (#144).
- **MathML serialization could overflow the stack:** deeply nested
  XML structure recursed per node in extraction; a pathological page
  overflowed the stack and aborted the daemon. Iterative fallback
  on deep nesting instead. Credit: mnaza (#145).
- **Markdown links with `)` in the URL were cut off:**
  `[Mercury (planet)](https://en.wikipedia.org/wiki/Mercury_(planet))`
  truncated at the first `)`. A balanced-close scan now finds the
  real end of the destination. Credit: mnaza (#146).
- **Search intent matching fired on substrings:** the word
  "software" matched the `war` news marker (and similar accidents),
  silently downgrading engines and freshness weights. Intent
  markers now match on whole tokens. Credit: mnaza (#147).
- **A single non-UTF-8 byte on the MCP stdin killed the daemon:** the
  line loop exited on the parse error as if the client had closed.
  A malformed line now receives a -32700 parse-error response and
  the session continues. Credit: mnaza (#148).
- **`<pre>` code blocks closed their own fence:** content showing a
  nested ``` block (e.g. a Markdown syntax example) broke the
  outer fence and spilled the rest of the page into the code block.
  Fences now extend to at least the longest run of backticks in the
  content. Credit: mnaza (#149).
- **Docs-infobox adapter duplicated nested list items:** a nested
  `<li>` matched both the outer list's selector and its own,
  appearing twice and restarting ordered-list numbering. Nested
  matches are skipped and lists render through the shared nested
  list helper instead. Credit: mnaza (#150).
- **String JSON-RPC ids could never be cancelled:** cancellation
  keyed on an `i64`, while the JSON-RPC spec allows string ids
  (UUIDs etc.), so requests from such clients ignored `notifications/
  cancelled` forever. Cancellation keys now cover both id shapes,
  with `7` and `"7"` kept distinct. Credit: mnaza (#151).
- **Supervised mode dropped in-flight responses on client EOF:** a
  one-shot client that closed stdin right after the request got
  nothing back, because the supervisor exited on EOF and the exit
  killed the stdout forwarder mid-response (and skipped the
  daemon's own graceful shutdown). EOF now waits for the daemon to
  drain and answer, with a bounded timeout, forwarding all output;
  and the rapid-restart counter forgives old crashes. Credit: mnaza
  (#153).

## [3.6.4] - 2026-09-05

### Fixed

- **News freshness treated future-dated items as stale:** a negative
  days-since count fell through every freshness arm to the stale 0.85
  weight, and the freshness test used hardcoded dates that would rot.
  A future date is a skewed clock and deserves the freshest weight;
  the test now walks the tier boundaries relative to today.
  Credit: mnaza (#143).
- **Char-boundary slice panics (panic=abort means daemon death):** the JSON-LD metadata search, the `\u` escape decoder, PDF date parsing, `doctor`'s key masking, and the Xvfb stderr-diagnostic path all sliced/truncated at raw byte positions, which panics inside a multibyte character on any non-ASCII input. All cuts now pull back onto `floor_char_boundary` or go char-based. Credit: mnaza (#133, #141).
- **BYOK transport errors leaked the request URL, SerpApi keys included:** a failed DNS/refused/TLS call rendered the full `reqwest` error, which for SerpApi embeds the key in the query string, into the model-visible `last_error`, CLI stderr and debug log. All ten providers now go through one `from_transport` mapping that uses `without_url()`, with a test proving the key is gone. Credit: mnaza (#134).
- **`keys export`/`proxy export` wrote credentials world-readable:** files were created at the umask default (0644) and tightened only afterwards, with the chmod failure silently ignored. New `write_private` opens owner-only (0600) from the moment the file exists and re-tightens an existing file. Credit: mnaza (#139).
- **Self-update left a stale `libonnxruntime.so` beside new binaries:** `-u` swapped only the binary, so Linux self-updaters kept the old runtime (e.g. one needing GLIBC_2.38) and OCR/rerank stayed dead while `doctor`'s presence check said fine. The update now stages, backs up and atomically swaps sibling runtime libs, and `--rollback` pairs the previous binary with its previous lib. Credit: mnaza (#136).
- **`--rollback` could destroy the previous version:** the current binary was copied over `.bak` before the final atomic rename, so a rename failure (sticky-bit dir, immutable file) discarded the only copy of the version being rolled back to; the Windows path also returned exit 0 when its copy failed. The swap now stages everything first and keeps `.bak` untouched until the rename has succeeded. Credit: mnaza (#137).
- **`must_contain`/regex probe excerpts missed the match on non-ASCII pages:** the substring path searched a separately lowercased copy (offsets drift when case folding changes byte lengths) and both paths fed byte offsets into a char-indexed window, so on any page with non-ASCII text before the hit the excerpt landed past it. Everything now goes through one case-insensitive regex with a byte-window literal fallback. Credit: mnaza (#140).
- **Data tables lost their row labels:** `<th scope="row">` cells were collected per-row from a `<td>`-only select, so every label vanished and the remaining cells shifted one column left. One select over `th, td` in document order keeps the column alignment. Credit: mnaza (#135).
- **Proxy passwords containing `@` broke parsing:** auth split at the first `@`, so `p@ss` became user `alice`, host `p`. The address cannot contain `@`, so splitting at the last one is the only correct point. Credit: mnaza (#138).
- **Ghost escalation budget could wrap to `usize::MAX` under concurrent workers:** a load-then-decrement race let two workers both pass the budget check, wrapping the counter and deleting the crawl's cost ceiling. `fetch_update` makes the decrement atomic. Credit: mnaza (#142).

## [3.6.3] - 2026-09-05

### Added

- **Windows binaries carry a version resource:** Explorer, Task Manager,
  UAC prompts and `Get-Command` showed no publisher, description or
  version. `build.rs` now stamps `VERSIONINFO` derived entirely from
  `Cargo.toml`: `FileDescription` the display name, `Comments` the
  package description, `LegalCopyright` the license. The numeric
  `FILEVERSION` carries `MAJOR.MINOR.PATCH`, the string field the full
  version, and a version suffix marks the build unofficial (`rc`/`beta`
  set `VS_FF_PRERELEASE`, anything else `VS_FF_PRIVATEBUILD`). Best
  effort: a missing `rc.exe` warns instead of failing the build.
  `CompanyName` is left as a disabled hook : there is no publisher to
  claim.
- **`serverInfo.title` in the MCP handshake:** MCP separates the
  programmatic identifier from the display label (`title`, optional
  since 2025-06-18), so clients that prefer it now show `DonSeTch`
  instead of the `donsetch` package id. Older clients ignore the field.

### Changed

- **One source for the product name:** hardcoded in 16 places across the
  CLI, it now lives in `src/display_name.rs` (`donsetch::DISPLAY_NAME`),
  read by the CLI titles, the MCP title and build.rs alike : the last by
  `include!`, since a build script cannot `use` items from the crate it
  builds.
- **`donsetch help` header now reads `DonSeTch`,** not the lowercase
  `donsetch`: the invocation name is already on the `USAGE:` line below
  it. Every `USAGE:`/`Usage:` line still shows the command exactly as
  typed.

### Fixed

- **False-like private-egress values no longer disable SSRF guards:**
  `DONSETCH_ALLOW_PRIVATE_EGRESS` previously enabled private egress from
  presence alone, so `false`, `0`, an empty string, or an unknown value
  bypassed the URL, DNS, and socket checks. One shared fail-closed parser now
  accepts only `1`, `true`, or `on` (case-insensitive, with surrounding
  whitespace ignored); malformed and non-Unicode values remain disabled.
- **Engine trust EWMA was eroded by infra failures:** the quarantine
  gate excluded `dead-proxy`/`auth-fail`/`no-results` but the trust
  EWMA only excluded `no-results`, so dead egresses and BYOK key
  problems quietly dinged engine trust (the ranking weight) for
  failures the engine had nothing to do with. One shared predicate
  (`is_engine_fault`) now drives both, with a table test. Credit:
  mnaza (#124).
- **Ranking `topup()` could leave the result vector non-sorted past
  its depth-8 prefix**: the score nudge could push `results[7]`
  below `results[8]`'s untouched score (a near-tied pair straddling
  the top-up depth boundary), and callers taking more than
  `depth` results (merge keeps 12; top-up runs at depth 8) got a
  slice whose rank order contradicted its score order at that
  boundary. `topup` now re-sorts the whole vector (a provided
  testable `apply_topup_scores` helper), with a discriminating
  boundary test. Credit: mnaza (#125).
- **Search (S-)handles were the one unbounded in-memory store in a
  daemon meant to run forever:** every tool search call minted up
  to 12 fresh S-handles into the handle table and nothing ever
  evicted them (every sibling structure is bounded: L-handles
  2048 LRU, search cache 500, prewarms 10, HTTP sessions 1024,
  crawl governor 1024). FIFO cap of 2048, oldest-minted evicted
  first, with a discriminating bound test.
- **Search validation failures reported the wrong error kind**:
  `search_error` declared `errorKind: "transient"` (with an engine
  escalation trace) for `validate_query` rejections that never
  contacted an engine, contradicting the function's own caller
  comment and the retry taxonomy, and mislabeling the CLI exit
  code for a non-retryable input error. `SearchFailure` now carries
  the kind (permanent for bad input, transient for exhausted
  engines/providers), the permanent path drops the false escalation
  trace, and the batch path composes kinds (any transient variant =
  retryable batch). Four tests. Credit: mnaza (#126).

## [3.6.2] - 2026-09-05

### Added

- **Ghost SERP cascade lane:** when the plain-HTTP fan-out AND its retry
  wave leave the merge thin (<3 working lanes or <15 hits), one browser
  render through the shared ghost hook fetches Google's 2026 JS-shell
  SERP (plain HTTP gets 0 result anchors; the browser render parses with
  the existing layered parser). Joins the merge as the independent
  `google` index family (5 keyless families instead of 4). Costs zero
  when healthy (never fires), trust/quarantine shared with the `google`
  base, honest `google_ghost` engine reports. Kill switches:
  `DONSEEK_NO_GHOST_LANES`, `DONSEEK_FORCE_GHOST_LANE` (dev bench).
- **Persisted engine health:** trust EWMAs + failure streaks live in
  `search-trust.json`, so a benched engine stays benched across daemon
  crashes instead of re-paying three failure lanes after every restart.
- **Corroboration on the model surface:** compact search lines carry the
  independent-index-family count per result (`· 3 sources`), the same
  math the ranking consensus uses.
- **Ranking top-up on page truth:** after enrichment swaps in real page
  titles/descriptions for the top slice, the cross-encoder gets a
  bounded ±0.1 nudge on close calls (`DONSEEK_NO_TOPUP` = A/B switch).
  Bench A/B: head-10 corpus MRR 0.75 → 0.80 with the top-up.

### Fixed

- **`donsetch login` post-login probe aborted the CLI process:** the
  probe and the CDP endpoint poll built `reqwest::blocking` clients
  inside the tokio runtime context, which is a documented reqwest
  panic (and with `panic = "abort"`, a hard process abort). Reproduced
  on master: `probe_domain` in a tokio test kills the process. Fixed
  by running the probe on a dedicated thread and converting the
  endpoint poll to an async client. Both paths regression-tested
  under the runtime. Credit: mnaza (#122).
- **Linux warm-connect (TFO) path shipped three real bugs in 3.6.x:**
  (1) `sockaddr_of` returned a pointer to a local inside its own match
  arm: dangling-pointer UB, benign only by stack-layout luck (the
  shipped path "worked" by accident of stack reuse);
  (2) `from_ne_bytes(...).to_be()` reversed the IPv4 octets on every
  little-endian target, so warm TFO connects dialed octet-reversed
  addresses (203.0.113.7 → 7.113.0.203), always failed, and silently
  fell back to a plain connect: warm TCP Fast Open never actually
  worked on Linux in 3.6.x, and the fix makes warm connects genuinely
  faster (fewer round trips on repeat-navigation origins);
  (3) the raw `libc::connect` on a TFO-requesting socket is a real
  blocking syscall that ran inline on the executor thread: measured
  against loopback by the author at ~135s vs ~220µs for a plain
  connect, silently defeating the outer connect timeout. Fixed with
  `spawn_blocking` + honest fallback to a plain connect. Structs now
  returned by value, execute-test covers the executor-freedom pattern
  with a stated caveat. Credit: mnaza (#123).
- **search --json lost titles/snippets for machine consumers** (compact
  contracts, v3.6.0): the model surface (structuredContent) stays
  compact, but the client-only `com.donsetch/search-debug` namespace now
  carries the full per-result machine view (title/url/snippet/score/)
  and the CLI `--json` re-materializes it into `meta` for pipelines.
  The in-repo search bench had silently gone 0/30 because of the loss:
  found live, restored, and the bench now measures real recall
  (snippet accuracy 96.7% over the 30-question corpus, MRR 0.80).
- **`consensus` double-counted same-engine ranks** in the JSON meta: a
  Yahoo URL at two ranks read as consensus 2 for one opinion. Now counts
  index families (same math as ranking), matching its own field name.
- **Enrichment demoted slow-alive pages as dead links:** transport
  timeouts no longer halve a result's score; only refused/DNS-dead/
  4xx/5xx answers do (slow ≠ dead).
- **Single-flight stampede:** two identical searches at different
  max_results each paid a full fan-out; the flight now keys on
  query+intent since the leader publishes the full top-12 anyway.
- **Oversized/empty queries** now fail fast with a clean message +
  next_action on BOTH paths (BYOK providers included), instead of
  burning a fan-out: `donsetch search "<862-char query>"` no longer
  reaches exa/engine endpoints.
- **Search-health single write per search:** trust snapshots now save
  once per completed search (not per engine outcome).

### Changed

- **Google News snippets** now carry `Publisher · date` instead of a bare
  RFC-822 date string (a date alone says nothing about the story).
- **search --json engines list** dedups engine names (an engine surfacing
  a URL at two ranks is one opinion, same as the markdown surface).
- **Bench harness:** `bench/search_quality.py` results cache note + the
  search invocation verified against the current CLI arg surface.

## [3.6.1] - 2026-09-05

### Changed

- **Compact MCP contracts:** model-facing tool schemas now keep lifecycle
  guidance on the tool and field-specific rules on each parameter, while
  preserving the full CLI help. Search, fetch, crawl, and batch responses
  render evidence once, retain actionable routing and recovery state, and
  move bounded diagnostics to client-only `_meta`. The three existing tools,
  acquisition, ranking, explicit query grouping, and fallback behavior are
  unchanged. tools/list schema tokens dropped from ~3.5k to ~2.0k (measured).
  Credit: adaaaaaaaaaaaaaaaaaaaaa (#120). CLI stats footer updated in the
  same change set to read moved telemetry from `_meta`.
- **CLI multi-fetch markers:** `donsetch fetch url1 url2 ...` prints an
  explicit `### [n] URL` boundary before each result, matching the MCP
  batch layout; large batches are parseable again.
- **README honesty:** token and envelope claims updated to the compact
  contracts (2.0k tools/list, model-state vs client-telemetry division).

### Fixed

- **`chrome_h2_probe` non-Linux CI:** gate the Linux-only imports,
  ALPN callback, and frame-decoding helpers together with the probe entry
  point, so `cargo clippy --all-targets -- -Dwarnings` does not compile them
  as unused code on Windows or macOS.
  Credit: adaaaaaaaaaaaaaaaaaaaaa (#121).
- **pi extension fetch badge:** the via-cache/via-ghost label reads the
  tier from the compact-contract debug payload, so the badge survives the
  new envelopes (and still falls back to the old surface).

## [3.6.0] - 2026-09-04

### Added

- **Self-improvement engine upgraded: fail-fast wall memory + background
  pre-solve.** The domain profile now remembers when a wall survives REAL
  browser solves (not just tier-1 challenges): two consecutive
  wall-persisted passes put the domain into a solve-cooldown with
  exponential backoff (15 min base, doubling, 2 h cap). Inside the
  cooldown a fetch answers honestly in milliseconds instead of burning
  a 20-40 s browser cycle per attempt. The memory heals itself: a
  successful solve or a tier-1 cold success clears it. Searches now
  opportunistically pre-solve the top result's domain in the
  background while the agent reads results (bounded: one in flight,
  top result only), so the follow-up fetch lands warm. Wall failures
  in the ghost path feed the same memory, and warm routing still
  requires a verified tier-1 replay.

### Added

- **Stealth floor rebuilt (v3.6 line, part 1): wire-truth tier 1,
  capture-driven, no guesses.** A dev rig now captures the REAL
  browser's HTTP/2 byte stream (examples/chrome_h2_probe.rs: real
  Chromium against a raw-boring acceptor) and parity tests are
  generated from those captures, never hand-edited. From the first
  capture batch: request HEADERS now carry Chromium's PRIORITY flag
  + 5-byte priority block (exclusive=1, dep=0, weight=255), the
  `priority: u=0, i` header moved to h2-only exactly where Chrome
  puts it, the cookie header now sits after sec-fetch-dest /
  before accept-encoding, and sec-ch-ua now reflects the ACTUAL
  host browser: greased brand `"Not=A?Brand";v="99"`, Chromium
  first, and the `Google Chrome` brand only when the host binary
  really is branded Chrome (distro Chromium gets the two-brand
  list it actually sends). Warmer connects on Linux now request
  TCP Fast Open (TCP_FASTOPEN_CONNECT) for repeat-navigation
  origins, matching Chrome's behavior, non-fatal everywhere.
- **Wall classification + escalation floor.** Cloudflare's
  explicit `cf-mitigated: challenge` header now classifies a 403
  as a challenge (glassdoor-class pages whose markers sit 100KB+
  deep; error-status bodies get a widened 256KB marker window).
  Ghost solves get a solve-grade second pass (warm re-render
  when the first render settles on an invisible wall), and big
  SPA DOMs no longer settle on 80 visible chars of shell: they
  need 800+ visible chars or a scroll kick, so product-page
  style hydration is waited out instead of snapshot-mid-load.
- **Dev rig: chrome_h2_probe** (examples/, dev-only, never
  ships): byte-exact h2 capture of the installed Chromium,
  incl. HPACK decode via our own decoder.


- **`donsetch login`: authenticated sessions for gated sites.**
  `donsetch login x.com` opens a real browser on your display (a
  dedicated profile, never the automation one): you sign in yourself,
  press Enter, and donsetch harvests the session cookies into the
  existing 0600 vault. Tier-1 fetches and tier-2 renders of that
  domain replay the login automatically, no daemon restart needed,
  and the per-call vault resync makes logouts (and seconds-old
  logins) take effect live. Credentials never enter the process:
  no keystroke capture, no screenshots, no CDP attach before
  Enter, and `auth-state.json` stores masked metadata only (names,
  counts, expiries, probe verdicts, never values). Ships with
  `--list`, `--status`, `--logout`, `--import` (Netscape
  cookies.txt, the server/CI path), multi-site mode (bare `login`),
  a post-login wall probe (redirect-to-/login, 401/403), a doctor
  check, hostile-input hardening (userinfo URL rejection, IDNA,
  loopback ports), and a 12-test integration battery including a
  live local gate-server E2E of login → gated fetch → logout →
  re-gated fetch.

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
