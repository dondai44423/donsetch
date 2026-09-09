# Native Google search

Google participates in the default keyless search through DonShadow HTTP.
No browser, paid API, CSE dependency or interactive CAPTCHA solver is required
for this lane. The unofficial mobile endpoint can change or stop working;
local successes do not establish long-term availability or browser-equivalent results.

## Configuration and lifecycle

`DONSETCH_GOOGLE_PROFILE` selects the initial profile (default `6230-03.15`).
Valid IDs, in rotation order: `6230-03.15`, `6230-05.50`, `6230-04.44`, `6230i-03.80`,
`6280-03.60`, `7610-5.0509.0`, `7610-7.0642.0`.
Set it before startup; unknown/empty values fail only the Google lane.

- Preference is **in memory, per egress**; restart resets it. There are no
  per-profile cooldowns or suspensions: CAPTCHA advances A → B → … → A.
- A valid parsed success retains the profile. The common thin-merge retry wave
  decides whether to make one extra attempt and chooses its egress normally.
  On an untouched egress, a CAPTCHA retry selects the profile after the failed
  one. An evolved egress cursor takes precedence over old retry evidence,
  including after wraparound. Other failures do not advance the cursor.
- Generation checks prevent stale outcomes undoing a new preference;
  success on the unchanged profile does not invalidate other in-flight outcomes.
- Pacing admission is FIFO and cancellation-safe, with no future-slot debt.
  Admission and I/O share an eight-second budget for initial engine attempts;
  all retries retain the common three-second budget. Synchronous
  parsing is not preemptible. Retry timeouts remain visible in reports.
- All Google failures, including CAPTCHA, follow normal engine/egress health,
  quarantine and direct-fallback rules. A circular cursor cannot override them.
  `google_http_v1` isolates native health from legacy browser records.
  Browser health stays `google_ghost`; both count as one Google ranking family.
- Reports optionally expose the attempted `profile`. Cached IDs are historical
  evidence, never restored profile preferences. Old cache records remain readable.
  Native success suppresses the automatic browser cascade unless explicitly forced.

## Code ownership

`engines/google_wml.rs` owns URLs, parsing and the process-local profile selector.
The searcher owns the adapter state; the generic egress pool knows no profiles.
`egress.rs` owns pacing/egress health, `tasks.rs` bounds one attempt, and
`search/mod.rs` owns retry scheduling. The existing fetcher supplies request-local,
cookie-less headers, URL guards, TLS validation and body limits.
Unavailable egress skips only that engine. DDG endpoint aliases share the pool's
health key for selection and reporting, not the broader ranking index family.
No Chrome persona is replaced with a Nokia identity; this is header compatibility,
not Nokia TLS emulation. CLI/MCP still share their dispatcher and ranking pipeline.

## Verification

Offline tests cover parsing, header isolation, circular profile advancement,
stale outcomes, cancellation, deadlines and legacy report loading. Explicit live
probe (one to five public queries; uses the configured initial identity, not the
full search scheduler):

```sh
cargo run --profile ci --example google_wml_probe -- 'rust programming language'
donsetch search 'rust programming language' --json
```

Seven profiles returned results in a limited local sample; some were checked on
only one query. Live CAPTCHA recovery and sustained/cross-region reliability
remain unverified. Full CI is required before merge.

Protocol references, not runtime dependencies:
[SearXNG](https://github.com/searxng/searxng/blob/master/searx/engines/google.py),
[DDGS](https://github.com/deedy5/ddgs/blob/main/ddgs/engines/google.py).
Additional profile strings were located in
[WAP-Browser-data](https://github.com/bevelgacom/WAP-Browser-data) and
[Wordlist-Collection](https://github.com/gurkylee/Wordlist-Collection/blob/main/user_agents/software_name/nokia_browser.txt);
a listed User-Agent is not proof of an authenticated physical device.
