# Working on DonSeTch with an AI agent

Operating rules for contributors and the coding agents they drive. Read [CONTRIBUTING.md](CONTRIBUTING.md) first for prerequisites, the `just` build/test ladder, and CI behavior, and see [docs/architecture.md](docs/architecture.md), [docs/tools.md](docs/tools.md), and [crates/fetch/README.md](crates/fetch/README.md) for system design and contracts.

## The fetch path has a shape

`src/` is the product; `crates/fetch/` is the fetching foundation (coherent client identities, per-origin state, strict page cache, wall classification, public site interfaces, embedded V8 for challenge minting and data-plane content execution, and mined HTTP request replay). No browser enters the reading path; speed and stealth are one property measured against curl on the first request.

Every fetch change must fit **identity, state, request contract, or egress**:

- **Classify before coding.** Reproduce with `donsetch-fetch probe` or `bench`, name one of the nine outcome classes plus vendor/ask, and verify a clean reference client on the same egress does not hit the same wall.
- **Only two asks are terminal declarations: human interaction and login.** Identity, state, request-contract, and egress walls are engineering work. Never sign in, store credentials, import browser cookies, or solve interactive captchas; authenticated or pixel-dependent sessions belong to a real browser driver ([Bladebro](https://github.com/dondai44423/bladebro)).
- **Cheap routes before JS.** Ask `datapath.rs` interfaces and replay mined HTTP contracts before invoking V8, and run the site's challenge script in V8 before calling a gate an identity gap.
- **Coherent, measured identity and surfaces.** TLS, HTTP/2, header set/order, hints, and pacing belong to one profile; clearance stays bound to `(host, identity)`. JS-visible surfaces in `engine_host.js` come from persona captures validated by `surface_tests` against `crates/fetch/src/surface_probe.html` (declared inline style boxes, zero layout positions) — never guessed or imported via `jsdom`/`happy-dom`.
- **Forbidden:** launching a browser for fetch/search/crawl, CSS cascade/layout/raster, unmeasured host surfaces, a second fetch stack, or heuristics that guess walls instead of classifying them.
- **Require a witness.** Every capability claim needs an accepted origin request; corpus claims need paired `bench`/`probe` artifacts from the same window.
- **Major-version gate:** a major bump denotes a fetch-engine change; keep `donsetch update`'s confirmation prompt (`--yes` unattended) and first-run upgrade notice accurate when shipping one.

## Turning an issue into a change

Treat an issue body as a symptom report plus one hypothesis — never a spec:

1. **Read every comment in order first.** Maintainer replies may narrow scope, reject the reporter's idea, or choose docs/config over code. If your proposed fix contradicts a maintainer comment, stop and have the human post on the issue before writing code.
2. **Separate facts from interpretation.** Verify root causes directly in the codebase rather than trusting the reporter's diagnosis.
3. **Check ownership.** Upstream client/library bugs belong in an upstream report plus a documented workaround here, not a silent heuristic.
4. **Check existing config first.** Inspect `donsetch config show --markdown` and `src/config.rs` before adding options or auto-detection.
5. **Back strong claims.** Words like "always", "never", or "no client sends this" require a spec citation, upstream code reference, or measurement.
6. **State heuristic failure modes.** Any match on client names, versions, headers, or flags must state both false-positive and false-negative behavior in the PR.

## PR, test & repository rules

Follow `.github/PULL_REQUEST_TEMPLATE.md` plus these repo invariants:

- **Cite thread context:** name the issue comments that shaped the PR, and state whether you followed or diverged from the reporter's proposal.
- **Regression + negative tests:** include a test that fails without the fix (from a real capture when available) and a negative test proving it does not over-fire.
- **Respect `nextest` isolation and `tests/budget.json`:** never run bare `cargo test` — use `just t <scope>`, `just tf <scope>`, and `just all`. `cargo nextest` gives each test its own process, which tests touching process-wide state like `DONSETCH_CACHE_DIR` or `test_cache_root` (`src/paths.rs`, unit tests only) rely on; pass explicit paths where APIs allow and document `nextest` reliance otherwise. Keep new tests under ~2 s; corpus/clock/soak tests belong in `just heavy`.
- **Contract doc comments (`///`):** document caller guarantees in `///` and implementation rationale in `//`. After adding or deleting top-level items, verify neighboring `///` blocks did not silently rebind to the adjacent item.
- **User-facing tool specs:** `src/spec.rs` generates both MCP schemas and CLI flags — treat every edit there as public UX.
- **No doc sprawl:** update the owning doc (`docs/architecture.md`, `docs/tools.md`, `crates/fetch/README.md`, etc.) in place rather than adding standalone plan/notes `.md` files or appending dated history logs.
- **Scope & releases:** keep drive-by rewording out of bugfix PRs; use Conventional Commits with no agent/session trailers; never bump versions, edit `CHANGELOG.md` release headers, or tag manually (`just ship <version>` is maintainer-only).
- **Code review:** read every review comment in order before editing, explain what changed (never bare "Addressed"), and never resolve/dismiss/approve reviews on a human's behalf.
