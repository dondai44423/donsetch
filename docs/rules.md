# Local rules

Per-host policy rules in `donsetch.toml` that refuse matching URLs before any network request and hand the agent your explicit instructions instead. DonSeTch ships with an empty rule table.

**Guardrail, not a sandbox:** MCP tools expose no flag to bypass rules, so an MCP-only model cannot override them. An agent with shell access can still run `donsetch fetch --ignore-rules`, set `DONSETCH_RULES__MODE=off`, or edit `donsetch.toml`. Maintainer internals live in [rules-architecture.md](rules-architecture.md).

## Quick start

```toml
[rules]
mode = "enforce"                  # "enforce" (default) | "off"
crawl_denied_urls_per_rule = 20   # 0..=200 URLs listed per rule in crawl denial summaries

[rules.url."example.org"]
enabled = true                    # true (default) | false
action  = "deny"                  # "allow" (default) | "deny"
kind    = "walled"                # "walled" (default, exit 3) | "permanent" (exit 1)
reason  = "ip_ban"                # [a-z0-9_]+ -> code "policy.denied.ip_ban" (default "unspecified")
message = "do not fetch this site with curl or scripts: it bans our IP. Use BladeBrowser at human pace or ask the operator to download the file"
```

- **Required fields:** only `message` is required (and must be non-blank) when `action = "deny"`. On `action = "allow"`, `message`, `kind`, and `reason` are ignored so layer overrides stay clean. Unknown fields or invalid values fail config loading across all commands.
- **Writing `message`:** delivered verbatim after `Next action:` — state both *what to do instead* and *why* (so an agent with shell access does not fall back to `curl`).
- **Matching error codes:** scripts should match prefix `policy.denied.` or parse segment 3 (`ip_ban` / `unspecified`), never hardcode `policy.denied.unspecified`.

## Patterns & normal form

Rules match **host** and optional **scheme (`http` / `https`)** across all ports and paths:

| Key | Matches |
|---|---|
| `example.org` | `example.org` and every subdomain (`www.example.org`, `a.b.example.org`), `http` and `https` |
| `.example.org` | Exact host `example.org` only, `http` and `https` |
| `https://example.org` | `example.org` and subdomains, `https` only |
| `https://.example.org` | Exact host `example.org` only, `https` only |
| `127.0.0.1` / `[2001:db8::1]` | Exact IP literal (IPv4 rules also match IPv4-mapped IPv6 `[::ffff:127.0.0.1]`) |

Keys must be written in canonical normal form so two spellings can never silently create duplicate rules:
- Lowercase ASCII host (punycode `xn--…` for IDNs), no trailing dot, no `..`, no userinfo, no port, no path/query/fragment, no wildcards (`*`, `*.example.com`, `*://`), and no catch-all rule (DonSeTch's own search/doctor requests pass the same guard). `glob:` and `re:` prefixes are reserved load errors.
- IPv4 in dotted-quad form; IPv6 in `[…]` compressed lowercase hex without leading `.` or IPv4-mapped `::ffff:`.
- Invalid keys fail startup immediately — including on disabled rules (`enabled = false`) or when `mode = "off"` — printing the exact canonical spelling to use.

## Precedence

**Winner-takes-all** (no cross-rule field merging), ranked by:
1. **Host specificity:** more labels beat fewer (`www.example.com` > `example.com`); at the same host, exact (`.example.com`) beats tree (`example.com`).
2. **Scheme:** single-scheme (`https://…`) beats scheme-less.

`enabled = false` removes a rule from ranking.

### A narrow allow rule cancels a broad deny

Because the winning rule applies in full and `action` defaults to `"allow"`, `[rules.url."www.example.org"]` (`action = "allow"`) overrides `[rules.url."example.org"]` (`action = "deny"`) for `www.example.org`. Conversely, to deny subdomains while keeping the apex open, pair `[rules.url."example.com"]` (`deny`) with `[rules.url.".example.com"]` (`allow`). Verify precedence offline with `donsetch rules test <url>`.

## Switching rules off

Three switches disable enforcement (syntax validation still runs):
- `[rules] mode = "off"` in `donsetch.toml`.
- `DONSETCH_RULES__MODE=off` in the environment.
- `--ignore-rules` on CLI `donsetch fetch`, `crawl`, and `screenshot` (`search` does not accept it, and MCP rejects `ignore_rules` as an invalid argument).

Only `DONSETCH_RULES__MODE` and `DONSETCH_RULES__CRAWL_DENIED_URLS_PER_RULE` are valid in the environment; setting `[rules.url.*]` via env vars is a hard load error. When `mode = "off"`, `config show`, `doctor`, and `rules test` surface it explicitly.

## What a deny blocks & what the agent receives

The guard runs before caches (including search prewarm and Unlocker cache), persona selection, proxy selection, adapter rewrites (`npmjs.com` → `registry.npmjs.org`), every redirect hop, screenshot browser navigations/page HTTP fetches, and DonSeTch's own keyless search/robots/sitemap traffic (optional BYOK provider API calls do not pass host rules).

- **`web_fetch` & `web_screenshot`:** returns `ok: false` with `code: "policy.denied.<reason>"`, `errorKind` (`walled` → CLI exit 3; `permanent` → CLI exit 1), `rule`, and `next_action` set to your `message`:
  ```text
  excluded by the operator's local DonSeTch rules `example.org` (another source may have this content)

  Next action: <your message>
  ```
- **`web_search`:** never refused and ranking is unchanged; hits on denied hosts keep their snippets, skip prefetching, and do not penalize engine health (denying a search engine's own host marks that engine `invalid-config`).
- **`web_crawl`:** a denied seed (or denied seed rewrite/redirect with nothing else to fetch) fails upfront with the same `policy.denied.<reason>` tool error, leaving any resumable job untouched. During execution, any planned URL, adapter rewrite, or redirect refused by a rule is skipped without retries as `policy.denied.<reason>` (marking the crawl incomplete); when `DeniedLog` groups are populated, markdown output includes a header count + trailing `## Excluded by the operator's local DonSeTch rules` section (up to `crawl_denied_urls_per_rule` URLs) and `[meta]` records `denied_by_local_rules`.

## Checking your rules

- **`donsetch rules test <url>`:** offline trace of all matching rules (most specific first), winning fields with config-layer origins, and exact effect/exit code (`url` must include scheme).
- **`donsetch doctor`:** reports active/disabled rule counts and warns if `mode = "off"` while rules exist.
- **`donsetch config show`:** shows `rules.mode` and `rules.crawl_denied_urls_per_rule` with their origins.

## Known limits

- **Host + scheme granularity only:** v1 has no path, query, or port patterns.
- **Unlocker server-side redirects:** Bright Data Web Unlocker resolves redirects on its own network; if an allowed URL redirects there into a denied host, your IP never contacts it, though its body can return.
- **Sitemaps, `robots.txt` & `map`:** denying a CDN hosting a site's sitemap silently shrinks discovery; a `robots.txt` redirecting into a denied host counts as unreachable (`disallow-all`); `mode=map` lists inventory URLs without fetching or marking denied entries.
- **Screenshot browser scope:** CDP `Fetch` interception blocks main navigations, redirects, page subresources (silently failing subresources inside the page), and dedicated workers, but does not see WebSockets, preconnect/DNS-prefetch hints, cross-site iframes, or shared/service workers.
- **Resumed crawl jobs:** editing rules mid-job does not rewrite already-stored job history; start a fresh crawl after changing rules.
