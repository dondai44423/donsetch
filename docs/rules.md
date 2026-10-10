# Local rules

Local rules are a table of per-host rules you write in `donsetch.toml`. A
rule can refuse a URL before any request is made, handing the agent your own
instructions instead of the page, or pin the fetch tier for a host. DonSeTch
ships no rules: the table is empty until you write one.

The typical use is a site that bans your IP when an agent works through it
at machine speed. ResearchGate is the case that started this feature
([#152](https://github.com/dondai44423/donsetch/issues/152)). A deny rule 
stops `web_fetch`, `web_crawl` and `web_screenshot` from touching the site 
and tells the agent what to do instead: for example, ask the user to 
download the file manually.

**Rules are a guardrail, not a security boundary.** They stop a model that
works through the MCP tools from fetching a denied URL, whether by accident
or on its own judgment; the MCP tools offer no argument that switches them
off. They do not stop an agent that also has a shell, as Claude Code, Codex
and OpenCode do: such an agent can run `donsetch fetch --ignore-rules`, set
`DONSETCH_RULES__MODE=off`, or edit `donsetch.toml` itself.

How rules are built, and what is planned next, is in
[the architecture notes](rules-architecture.md).

## A first rule

Rules live under `[rules.url."<pattern>"]`, keyed by a host pattern:

```toml
[rules.url."example.org"]
action  = "deny"
reason  = "ip_ban"
message = "do not fetch this site with curl, scripts or another HTTP client: it bans our IP for automated access. Use BladeBrowser (a real browser at human pace), or ask the human operator to download the file"
```

That rule covers `example.org` and every subdomain, over http and https,
on every port and path. An agent that asks for
`https://www.example.org/publication/123` gets, with no request sent:

```
blocked by a local DonSeTch rule `example.org` (walled: try another source)

Next action: do not fetch this site with curl, scripts or another HTTP client: it bans our IP for automated access. Use BladeBrowser (a real browser at human pace), or ask the human operator to download the file
```

A rule that only pins the tier:

```toml
[rules.url."example.gov"]
tier = "2"               # go straight to the browser for this host
```

A rule kept in the file but switched off:

```toml
[rules.url."slow-site.com"]
enabled = false
action  = "deny"
message = "ask the human operator for this page"
```

The whole section, with the two mechanism-wide settings at their defaults:

```toml
[rules]
mode = "enforce"                  # "enforce" | "off"
crawl_denied_urls_per_rule = 20   # URLs listed per rule in a crawl's denied section, 0-200

[rules.url."example.org"]
action  = "deny"
kind    = "walled"
reason  = "ip_ban"
message = "do not fetch this site with curl, scripts or another HTTP client: it bans our IP for automated access. Use BladeBrowser (a real browser at human pace), or ask the human operator to download the file"
```

Run `donsetch rules test <url>` after editing to see which rule a URL gets
(see [Checking your rules](#checking-your-rules)).

## Rule fields

Every field is optional, except that a `deny` rule needs a `message`.

| field | values | meaning |
| --- | --- | --- |
| `enabled` | `true` (default), `false` | `false` makes the rule inert, as if it were not written |
| `action` | `"allow"` (default), `"deny"` | `deny` refuses the URL; `allow` lets it through |
| `message` | string | for `deny`: your guidance, passed to the agent verbatim as its next action; required and non-blank |
| `kind` | `"walled"` (default), `"permanent"` | for `deny`: `walled` means "get it elsewhere", `permanent` means "do not pursue it"; sets the agent's `errorKind` and the CLI exit code |
| `reason` | `[a-z0-9_]+` | for `deny`: a subcode; the error code becomes `policy.denied.<reason>` |
| `tier` | `"auto"`, `"1"`, `"2"` | the tier this host is fetched with, as if the call had passed `tier` |
| `tier_enforce` | `false` (default), `true` | `true` makes `tier` win over a tier the call passed explicitly |

Unknown fields are a load error, and so is an `action`, `kind` or `tier`
value outside the list. `message`, `kind` and `reason` are read only on a
`deny` rule; on an `allow` rule they are ignored rather than rejected.

The error code always has three segments: `policy.denied.ip_ban`, or
`policy.denied.unspecified` when the rule sets no `reason`. Scripts should
match the prefix `policy.denied` or read the third segment, never compare the
whole code, which changes the moment someone adds a `reason`.

## Writing the message

The agent receives `message` as `Next action:`, so say what to do instead,
and say why when the reason rules out workarounds: "do not fetch this site
with curl, scripts or another HTTP client: it bans our IP for automated
access. Use BladeBrowser (a real browser at human pace), or ask the human
operator to download the file." Without the reason, an agent told only "use
BladeBrowser" may decide that `curl` is just another way to get the page. An
explanation alone ("this site bans our IP") reads badly after `Next action:`
and leaves the agent to guess the alternative. DonSeTch adds no advice of its own to a denial: only you know
whether the way out is another browser, a mirror, a DOI resolver, an
institutional proxy or a human.

## Patterns

Rules match the host and, optionally, the scheme. Nothing else: no paths, no
query, no port.

| key | matches |
| --- | --- |
| `example.org` | `example.org`, `www.example.org` and every other subdomain; http and https |
| `.example.org` | `example.org` only |
| `https://example.org` | as the first row, https only |
| `https://.example.org` | `example.org` only, https only |

- A bare host covers its subdomains on label boundaries: `example.com` matches
  `sub.www.example.com` but not `notexample.com`.
- A leading `.` means that host only. It is the only prefix.
- The scheme form takes `http` or `https`.
- An IP literal (`127.0.0.1`, `[2001:db8::1]`) matches that address only.
  IPv6 goes in brackets. An IPv4 rule also matches the address written as
  IPv4-mapped IPv6 (`[::ffff:127.0.0.1]`), so that spelling cannot get past
  it.
- A dotless name such as `localhost` is a valid key.
- Every port and every path match. A deny on `example.org` cannot be
  evaded with `example.org:8443`.

On the URL side the host is compared as the URL parser gives it, lowercase
and in punycode, with every trailing dot removed, so
`https://Example.ORG./x` still meets a deny on `example.org`.

The keys look like Chrome's URLBlocklist entries and like extension match
patterns (`@match` in userscripts), but they do not behave like either in
every respect:

| | DonSeTch rules | Chrome URLBlocklist | Chrome match patterns (`@match`) |
| --- | --- | --- | --- |
| bare host `example.com` | host and subdomains | same | exact host only |
| `*.example.com` | load error | not in the format | host and subdomains |
| `*`, `http://*`, `https://*` | load error | allowed (block all) | `<all_urls>` matches everything |
| port | load error; every port matches | allowed | allowed, optional |
| path, query | load error | path prefix, query tokens | path required, `*` glob |
| schemes | `http`, `https` | standard and custom schemes | `http`, `https`, `file` and others |

Two of these differences are deliberate and worth knowing before you copy an
entry over:

- **A bare host includes its subdomains, also in the scheme form.**
  `https://example.com` covers `https://www.example.com`, unlike `@match`.
  With `@match` semantics, `example.org` would leave
  `www.example.org` open, and you would find out after the ban.
- **There is no catch-all rule.** DonSeTch's own traffic passes the same
  check: the keyless search engines, the search result prefetch and doctor's
  checks. "Deny everything, allow a few hosts" would also switch off keyless
  search, so a rule must name the hosts it is for. A policy such as "no plain
  HTTP anywhere" cannot be written.

To cover the subdomains but not the apex, pair a tree rule with an exact
allow, which wins at the same host:

```toml
[rules.url."example.com"]
action  = "deny"
message = "ask the human operator for pages on example.com"

[rules.url.".example.com"]
action = "allow"         # the apex itself stays open
```

### Keys must be in normal form

A key is matched exactly as written, so it must already be in the one
spelling DonSeTch accepts: a lowercase ASCII host (punycode for an
internationalized name), no trailing dot, and an IP literal in canonical form
(IPv4 as a dotted quad, IPv6 in brackets with lowercase compressed hex).
Accepting other spellings would let `Example.ORG` and `example.org`
sit side by side as two rules, and an intended override would silently become
a second rule.

A bad key is a load error: DonSeTch refuses to start, and every command,
`doctor` and `config show` included, prints the error and exits 1. This is
deliberate. Skipping a bad rule with a warning would mean running without a
protection you believe is in place. Keys are validated even when `mode` is
`"off"` or `--ignore-rules` is given.

Each message names the key and says what to write instead. A file error is
printed as `donsetch: invalid config file <path>: ` followed by the message.

| key | message |
| --- | --- |
| `example.com:8080` | `ports are not supported in rule patterns.`<br>`A pattern matches every port : write "example.com".` |
| `example.com/docs` | `paths are not supported in v1 rule patterns.`<br>`A pattern matches every path : write "example.com".` |
| `example.com?a=1` | `queries are not supported in rule patterns.`<br>`A pattern matches every query : write "example.com".` |
| `example.com#top` | `fragments are not supported in rule patterns.`<br>`A pattern matches every fragment : write "example.com".` |
| `*.example.com` | `"*." is not supported in rule patterns.`<br>`"example.com" already covers example.com and all its subdomains; write ".example.com" for that host only.` |
| `*`, `http://*`, `https://*` | `a rule for every host is not supported.`<br>`Name the hosts the rule is for, e.g. "example.com".` |
| `*://example.com` | `"*://" is not supported in rule patterns.`<br>`A pattern without a scheme already covers http and https : write "example.com".` |
| `exa*mple.com` | `"*" is not supported in rule patterns.`<br>`Write the host itself, e.g. "example.com": it already covers all its subdomains.` |
| `ftp://example.com` | `only "http" and "https" schemes are supported in rule patterns.`<br>`Write "https://example.com" for one scheme, or "example.com" for both.` |
| `HTTPS://example.com` | `schemes must be written in lowercase in rule patterns.`<br>`Write "https://example.com".` |
| `Example.ORG` | `hosts must be written in lowercase in rule patterns.`<br>`Write "example.org".` |
| `bücher.de` | `non-ASCII hosts are not supported in rule patterns.`<br>`Write the punycode form: "xn--bcher-kva.de".` |
| `example.com.` | `a trailing dot is not supported in rule patterns.`<br>`A pattern matches the host with or without one : write "example.com".` |
| `a..b.com` | `empty labels ("..") are not supported in rule patterns.`<br>`Write "a.b.com".` |
| `..example.com` | `a pattern takes at most one leading ".".`<br>`Write ".example.com" for that host only, or "example.com" for it and its subdomains.` |
| `user@example.com` | `userinfo is not supported in rule patterns.`<br>`Write "example.com".` |
| `0x7f.1` | `IP literals must be in canonical form in rule patterns.`<br>`Write "127.0.0.1".` |
| `2001:db8::1` | `IPv6 literals must be in brackets in rule patterns.`<br>`Write "[2001:db8::1]".` |
| `.127.0.0.1` | `IP literals take no "." prefix: an address has no subdomains.`<br>`Write "127.0.0.1".` |
| `[::ffff:127.0.0.1]` | `IPv4-mapped IPv6 literals are not supported in rule patterns.`<br>`Write the IPv4 address "127.0.0.1": it matches both spellings.` |
| `glob:…`, `re:…` | `"glob:" patterns are reserved and not yet supported.` (or `"re:"`)<br>`Write a host, e.g. "example.com": rules match the host and the scheme only.` |

Every message in the table starts with `config error in
[rules.url."<key>"]: `, for example:

```
config error in [rules.url."example.com:8080"]: ports are not supported in rule patterns.
A pattern matches every port : write "example.com".
```

The other load errors:

- A `deny` rule with no `message`, or a blank one:
  ```
  config error in [rules.url."example.com"]: a "deny" rule needs a non-blank message: it is the guidance the agent receives in place of the page.
  Add e.g. message = "ask the human operator to download the file".
  ```
- A `reason` outside `[a-z0-9_]+`, with a suggested spelling:
  ```
  config error in [rules.url."example.com"]: reason "IP-Ban" may contain only lowercase letters, digits and "_".
  Write e.g. reason = "ip_ban"; it yields the code "policy.denied.ip_ban".
  ```
- Two keys that compile to the same pattern:
  ```
  config error: [rules.url."<key A>"] and [rules.url."<key B>"] are the same pattern.
  Keep one. To override a rule from another layer, use its exact key.
  ```
  The normal-form rules already leave one spelling per pattern, so this is a
  backstop.
- `crawl_denied_urls_per_rule` above 200:
  `rules.crawl_denied_urls_per_rule must be 0..=200`.
- A rule set through the environment (see [Switching rules
  off](#switching-rules-off)).

These checks run on disabled rules too: a rule with `enabled = false` and
`action = "deny"` still needs a valid key and a non-blank `message`.

## Which rule wins

**The single most specific matching rule applies, in full.** Rules are not
merged with each other. Candidates rank by:

1. host specificity: a host with more labels beats one with fewer
   (`www.example.com` beats `example.com`); at the same host, the exact form
   (`.example.com`) beats the tree (`example.com`);
2. scheme: a rule for one scheme (`https://…`) beats a rule for both;
3. the key string, as a final tie-break that two valid keys never reach.

A rule with `enabled = false` drops out of the ranking, and the most specific
remaining rule wins.

### A narrow tier rule cancels a broad deny

A narrower rule replaces a broader one entirely, `action` included, and
`action` defaults to `allow`. So a narrow rule written only to set `tier`
also allows, and under a broader deny it cancels the deny for everything it
covers:

```toml
[rules.url."example.org"]
action  = "deny"
message = "do not fetch this site with curl, scripts or another HTTP client: it bans our IP for automated access. Use BladeBrowser (a real browser at human pace), or ask the human operator to download the file"

[rules.url."www.example.org"]
tier = "2"    # wins for www. as the longer host: action is "allow", the deny is gone
```

Here `https://example.org/…` is denied, and `https://www.example.org/…`
is fetched with the browser. To keep a host denied, a narrower rule must
restate the deny itself (`action = "deny"` plus a `message`), at which point
its `tier` is moot, since a denied URL is never fetched. `donsetch rules test
<url>` shows the winner and every rule it beat, which is how to spot this.

### Layers

Today there is one config file, so this matters only when several layers
appear: the merge between layers happens per field, before rules are ranked.
A higher layer that sets only `tier` on a key keeps the lower layer's
`action` and `message`. There is no way to delete a key from a lower layer;
`enabled = false` neutralizes it. A higher layer that turns a `deny` into an
`allow` still carries the lower layer's `message`, `kind` and `reason`, which
is why those fields are ignored on an `allow` rule rather than rejected.

## Switching rules off

Rules are on by default. Three switches turn them off, all with the same
effect: the command runs as if the table were empty, tier pins included.

- `[rules] mode = "off"` in `donsetch.toml`.
- `DONSETCH_RULES__MODE=off` in the environment, for example in one MCP
  server registration.
- `--ignore-rules` on `donsetch fetch`, `donsetch crawl` and
  `donsetch screenshot`, for that one command. `donsetch search` does not
  take it.

`--ignore-rules` exists only on the CLI. It is not a tool argument: an agent
that sends `ignore_rules` over MCP gets an invalid-argument error
(`fetch.invalid`, `crawl.invalid` or `screenshot.invalid`) naming the unknown
parameter, and no rule is bypassed.

A forgotten `DONSETCH_RULES__MODE=off` switches off a protection silently, so
the off state is shown in three places: `donsetch config show` lists
`rules.mode` with its origin, `donsetch doctor` warns, and `donsetch rules
test` says so before anything else.

The environment reaches only the two scalars, `DONSETCH_RULES__MODE` and
`DONSETCH_RULES__CRAWL_DENIED_URLS_PER_RULE`. Rules themselves come from the
file only. Any other name under `DONSETCH_RULES`, including a bare
`DONSETCH_RULES`, fails the load instead of being ignored:

```
DONSETCH_RULES__URL__EXAMPLE: rules cannot be set through the environment.

Put them in donsetch.toml as [rules.url."<pattern>"].
```

## What a deny blocks

A deny means "this content is not returned", not only "this host is not
contacted". The check runs before anything else in a call, so a denied URL
mints no persona, picks no proxy lane and reads no cache:

- the live fetch, on every tier;
- a page the search already prefetched and parked for the follow-up fetch;
- the browser render cache;
- the paid unlocker's local cache.

The same check guards every request DonSeTch sends, so it also covers:

- **redirects**: a redirect from an allowed URL into a denied host is not
  followed, and `web_fetch` returns the rule's error;
- **adapter rewrites**: when an adapter moves the request to another host
  (`npmjs.com` to `registry.npmjs.org`, `old.reddit.com` to `www.reddit.com`),
  a deny on the target host applies;
- **the browser**: navigations and the HTTP requests of the page itself
  (see [Known limits](#known-limits) for the connections this misses);
- **DonSeTch's own traffic**: the keyless search engines, the search result
  prefetch and pre-solve, the background route prober, and crawl `robots.txt`
  and sitemap requests. The BYOK search providers are the exception: their API
  calls do not pass the check.

`web_search` itself is never refused, and in v1 rules never change search
ranking. A denied result is not prefetched, keeps its place and its score,
and teaches the learned host quality nothing; its title and snippet stay as
the engine returned them, since they may be how the agent finds a mirror.

## What the agent sees

### web_fetch and web_screenshot

A denied URL is a normal tool result marked `ok: false`, not a protocol
error. The text is one line naming the rule, with the kind as a short phrase,
followed by your message:

```
blocked by a local DonSeTch rule `example.org` (walled: try another source)

Next action: do not fetch this site with curl, scripts or another HTTP client: it bans our IP for automated access. Use BladeBrowser (a real browser at human pace), or ask the human operator to download the file
```

A `permanent` rule reads `(permanent: do not pursue)` instead.

The structured part carries `code` (`policy.denied.<reason>`), `errorKind`
(the rule's `kind`), `url`, `rule` (the key), `next_action` (your message,
verbatim), `ok: false` and the usual `read_status`, `content_ok` and
`content_complete`. With the default `mcp.text_only = true`, the MCP server
folds the whole structured part into a leading `[meta]` text block, so the
model reads `code`, `rule` and `next_action` even when its client ignores
structured content. `mcp.text_only = false` only hands that choice back to the
client handshake: clients DonSeTch knows to drop structured content still get
the folded shape.

In a batch, a denied URL's row carries `code`, `errorKind`, `rule` and
`next_action`. When every URL in a batch fails, the batch carries the code
its URLs agree on, and the `errorKind` they agree on; mixed kinds read
`transient`.

### web_crawl

**A denied seed fails the crawl** with the same policy error as `web_fetch`,
before any request. That includes a seed whose adapter rewrite is denied, and
a resumed crawl whose stored seed was denied since: the resume token is left
unused, so it still works once you change the rule. A seed that redirects
into a denied host, in a crawl that has nothing else to fetch, fails with the
same policy error, plus `requested_url` and `landing_url`.

**A denied URL found during the crawl is skipped, not fatal.** It is never
fetched and never takes a queue slot. Only links the crawl would otherwise
follow are reported: a link already outside the crawl's scope (another host
while `same_host` is on, or a path outside the seed's path scope) is dropped
by that scope check first, so it appears neither in the denied section nor in
its count. A seed under `/wiki/` that links to a denied host's front page
therefore reports nothing. In the markdown modes the crawl shows a counter
under the seed:

```
Denied by local DonSeTch rules: 19 URLs, listed at the end.
```

and a trailing section with one group per rule, its kind phrase, your message
and up to `crawl_denied_urls_per_rule` URLs:

```
## Denied by local DonSeTch rules (not fetched; web_fetch would refuse them too)

walled: try another source
Next action: do not fetch this site with curl, scripts or another HTTP client: it bans our IP for automated access. Use BladeBrowser (a real browser at human pace), or ask the human operator to download the file
- https://www.example.org/publication/1
- https://www.example.org/publication/2
…and 14 more

permanent: do not pursue
Next action: this mirror is gone, do not look for it
3 URLs
```

With `crawl_denied_urls_per_rule = 0` each group shows its count and no URLs.
In `[meta]` the markdown modes keep only the total,
`"denied_by_local_rules":{"count":19}`, since the document already lists the
groups. In dataset mode the JSON Lines rows carry no denials, so `[meta]`
keeps the groups with their `errorKind`, `next_action`, `count` and capped
`urls`. The CLI prints one stderr line for a crawl that denied anything, in
every output mode and even with `-q`:

```
[crawl] 16 URLs denied by local DonSeTch rules (1 rule)
```

Neither the text nor `[meta]` names the rule key; the structured part, for
clients that read it with `mcp.text_only = false`, carries it as `rule` in
each group. To find which rule denied a listed URL, run `donsetch rules test`
on it.

A crawl that denied anything is not reported `complete`, since the pages
behind the denied URLs were not crawled. When every URL a crawl reached was
refused by a rule, its next action says so instead of suggesting a retry.

### CLI exit codes

The rule's `kind` picks the exit code of `donsetch fetch`, `crawl` and
`screenshot`: `walled` exits 3, `permanent` exits 1. Rules add no exit code
of their own. A deny never exits 2 (transient), because a rule is not a
condition that retrying fixes.

In a `--json` multi-URL fetch the batch exit code is the most severe member's,
and walled is the least severe failure, so a batch mixing a denied URL with a
transient failure exits 2. The `code` of each row, not the exit code, tells a
script that a rule fired.

## Tier pins

A rule's `tier` acts exactly as if the caller had passed `tier` itself. It
replaces the call's argument before anything reads it, so a rule's `"2"` and
an explicit `tier=2` never behave differently.

| rule | the call passes | the fetch uses |
| --- | --- | --- |
| no `tier` | anything | what the call asked for |
| `tier = "2"` | nothing, or `auto` | 2 |
| `tier = "2"` | `1` | 1: the explicit argument wins |
| `tier = "2"`, `tier_enforce = true` | `1` | 2: the rule wins |
| `tier = "1"`, `tier_enforce = true` | `2` | 1: the rule wins |
| `tier = "auto"`, `tier_enforce = true` | `1` or `2` | auto: the rule wins |
| `tier = "auto"` | anything | what the call asked for |

Nearly every call arrives as `auto`, so an advisory rule (the default) applies
in full; `tier_enforce = true` matters only against an agent that explicitly
asked for the other tier. No tool argument can lift a `deny`, with or without
`tier_enforce`.

`tier = "auto"` with `tier_enforce = true` makes a host ignore the tier an
agent asks for: DonSeTch's own escalation decides, as if the call had passed
nothing. Without `tier_enforce`, `"auto"` changes nothing today. It is
accepted so that, once there is more than one config file layer, a higher
layer can cancel a lower layer's pin: a key cannot be removed from a lower
layer, only overridden.

The pin is chosen once per URL, on the URL the caller asked for. In a batch
each URL gets its own pin. An adapter's fallback to another URL on the same
site keeps the pin chosen for the original URL.

Things to know before pinning a host:

- **PDF-shaped URLs and JSON endpoints still go over HTTP first.** A path
  ending in `.pdf`, with a `pdf` segment or ending in `/pdf`, and a path ending
  in `.json`, are fetched over plain HTTP first whatever the tier, rule or
  `tier_enforce` included, because a browser cannot extract either. A JSON
  response never escalates to the browser.
- **Tier `"2"` turns off the walled-PDF recovery.** Under `auto`, a PDF that
  answers with a bot wall is recovered by solving the wall in the browser and
  retrying over HTTP with its cookies. Tier `"2"`, from a rule or from the
  call, turns that retry off, so a walled PDF comes back as Chrome's PDF
  viewer instead of the document. A URL that serves a PDF without looking
  like one gets no HTTP route at all under tier `"2"`. `donsetch rules test`
  warns about PDF-shaped URLs under a tier-`"2"` rule.
- **Tier `"2"` skips adapters.** On a host with an adapter (reddit, PyPI, npm
  and the others) the URL is not rewritten to the adapter's API and goes
  straight to the browser.
- **Tier `"1"` never escalates.** A host pinned to tier `"1"` fails with a
  walled error on a bot wall, even one DonSeTch detected, rather than opening
  the browser or the unlocker. With `tier_enforce = true` that is "never use
  the browser on this host". A tier `"1"` pin, advisory or enforced, also
  keeps the browser solve that `web_search` starts in the background for a
  walled top result off that host.
- **The result does not say a rule chose the tier.** `tier_used` and the CLI
  stats report the effective tier, and a tier learned from earlier walls
  looks the same. `donsetch rules test <url>` tells them apart.

## Checking your rules

### `donsetch rules test <url>`

Explains, offline, what your rules do with one URL: every matching rule, most
specific first, the one that wins, and the winner's fields with the config
layer each came from. Nothing is fetched. With the two example.org rules from
[A narrow tier rule cancels a broad deny](#a-narrow-tier-rule-cancels-a-broad-deny)
plus `reason = "ip_ban"`:

```
$ donsetch rules test https://www.example.org/publication/123_Example
URL   https://www.example.org/publication/123_Example
host  www.example.org

Matching rules, most specific first:
  * www.example.org  wins
    example.org      beaten by a more specific rule

Winning rule: [rules.url."www.example.org"]
  action        allow                    (default)
  tier          2                        (file:/home/me/.config/donsetch/donsetch.toml)
  tier_enforce  false                    (default)
Effect: allowed; tier "2" applies unless the call passes an explicit tier.
```

```
$ donsetch rules test https://example.org/profile/Someone
URL   https://example.org/profile/Someone
host  example.org

Matching rules, most specific first:
  * example.org  wins

Winning rule: [rules.url."example.org"]
  action        deny                     (file:/home/me/.config/donsetch/donsetch.toml)
  kind          walled                   (default)
  reason        ip_ban                   (file:/home/me/.config/donsetch/donsetch.toml)
  message       do not fetch this site with curl, scripts or another HTTP client: it bans our IP for automated access. Use BladeBrowser (a real browser at human pace), or ask the human operator to download the file (file:/home/me/.config/donsetch/donsetch.toml)
Effect: denied before any request: code policy.denied.ip_ban, errorKind walled (walled: try another source), CLI exit 3.
```

A disabled match is listed as `disabled (enabled = false)`. A URL no rule
matches prints `No rule matches this URL: it is fetched as if no rule
existed.` When rules are off, the first line is `Rules are OFF (mode = "off",
from <origin>): nothing below is enforced.`, and the matches are still
explained below it. Under a tier-`"2"` winner on a PDF-shaped URL the report
ends with:

```
warning: tier "2" turns off the cookie retry that recovers walled PDFs; this URL may come back as Chrome's PDF viewer, not the document
```

The URL must be absolute, with its scheme.

### `donsetch doctor`

The **Local rules** row, in the "Configuration & state" group, reports the
rule count and the mode with the layer the mode came from:

```
2 rules (1 disabled) · mode=enforce (default)
```

It warns when `mode` is `off` while at least one rule is enabled, adding
`: no rule is enforced`. A broken rule never reaches doctor: it fails the
config load first, and that error is the report.

### `donsetch config show`

Lists `rules.mode` and `rules.crawl_denied_urls_per_rule` with their values
and origins, like every other knob. The rule table itself is not listed; use
`rules test`.

## Known limits

- **Rules match whole hosts.** A rule for one path, or for one repository on a
  shared host, cannot be written in v1; such fetches proceed as before.
- **The paid unlocker can bypass a rule through a redirect.** When an allowed
  URL comes back walled and the Web Unlocker fetches it, the unlocker follows
  redirects on its own side and DonSeTch never learns where it landed. If the
  allowed URL redirects into a denied host, that host's content comes back.
  It is fetched from the unlocker provider's network, never from your IP, so
  this is harmless for an IP-ban rule but not for a rule meant to keep content
  out.
- **A denied CDN can hide a sitemap.** If a site serves its sitemap from a
  host you deny, a CDN most plausibly, that sitemap is not loaded and the
  crawl says nothing about it: discovery is quietly smaller and the crawl
  still reports a normal finish. In map mode the inventory then falls back to the seed page's links
  and one rendered read of the seed, so the map comes back smaller rather than
  empty. If that finds nothing either, the map is not reported complete and
  its skip reason reads "no sitemap found at common locations and the seed
  page exposed no usable links", which is misleading when a sitemap existed
  and was denied.
- **An allowed site whose `robots.txt` redirects into a denied host** reads
  as robots unreachable, which counts as disallow-all: with robots respected,
  that whole site is skipped for the rest of the crawl and its pages are
  counted only as filtered out.
- **A map inventory is not filtered by rules.** `mode=map` fetches none of the
  URLs it lists, so it lists denied URLs too, unmarked. Fetching one with
  `web_fetch` returns the rule's error and your message.
- **A host reached only by redirect is a skipped page, not a denial.** When a
  crawled page redirects into a denied host, the page was requested before the
  rule could apply to the hop. It is recorded as a skipped page with the
  reason ``policy.denied.<reason>: blocked by a local DonSeTch rule `<key>` ``,
  not in the denied section or its count. Skipped pages appear only in the
  crawl's debug data for the client (`_meta`), not in its text or `[meta]`;
  the crawl is then reported not `complete`.
- **The browser reports some refusals as browser errors.** `policy.denied`
  appears only when the URL the browser was sent to is itself denied, or the
  page lands on a denied URL. A redirect the browser follows into a denied
  host is blocked, and surfaces as a browser navigation error; in a crawl the
  skip reason reads `ghost escalation failed: …`. A navigation the page starts
  on its own after loading (a script or meta refresh) is blocked the same way
  and surfaces as a failed page. A subresource from a denied host (an image, a
  script) is dropped silently from any page the browser renders.
- **The browser check sees only the page's own HTTP requests.** It intercepts
  the requests Chrome pauses for the page through the DevTools `Fetch`
  domain. A WebSocket and a preconnect hint are not checked, so a page on an
  allowed host can still make the browser open a connection to a denied host;
  a DNS prefetch hint only looks up the host's name. Requests made inside a
  cross-site iframe or by a worker the page starts are probably not checked
  either. This list is read from the code and from how Chrome's DevTools
  protocol works, not from a test with a real browser.
- **Changing rules does not rewrite a resumable crawl.** After you remove or
  narrow a rule, a resumed crawl neither fetches the URLs the earlier run
  denied nor stops listing them: they stay in the crawl's seen set and in its
  denied section. Start a fresh crawl instead. The other way round, URLs
  queued before you added a rule are still refused when the resumed crawl
  reaches them, but they show up as skipped pages rather than in the denied
  section.
- **A deny on a host DonSeTch uses narrows DonSeTch.** Denying a search
  engine's host switches that engine off; the engine report shows
  `invalid-config: blocked by a local DonSeTch rule`, and the engine's health
  record is not penalized. For example `lite.duckduckgo.com` switches off the
  `ddg` and `ddg_lite` engines while `ddg_html` remains.
- **Dataset mode keeps denials out of the rows.** A crawl in dataset mode
  reports them only in the structured part (and `[meta]`); the CLI's JSON
  Lines output gets only the stderr summary line.
