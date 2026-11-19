# Measurements

Recorded results, their conditions, and what they establish. [Back to the README](../README.md).

## Defended-site fetch corpus

Recorded 18 November 2026 from a home IP on the 5.5.6 build. The corpus has 28 defended targets.

| Protocol | Content | Standing | Median | p95 |
|---|---:|---:|---:|---:|
| Cold process · up to 3 attempts (`--discover`) · 2 rounds | 46/56 | 23/28 | 1.16 s | 3.01 s |
| Paired curl · same cold measurement window | 20/56 | — | — | — |
| Learned state · no page cache · 3 s deadline · 3 rounds | 71/84 | 24/28 | 0.76 s | 2.69 s |

Standing counts each target once, by its last round; the content column counts every round. A first-round discovery miss that later rounds solved is not a wall.

The cold and learned rows use different protocols. Their medians are not a controlled measure of learning's speedup. The learned run has no paired curl measurement.

The runs used the identity/interface/challenge routes. The content lane ran on the app-shell rows (TikTok, Instagram) and returned no content there; these numbers are not a claim about the content lane's success rate. The 5.5.7 lane surfaces (worker, navigation timing, the reload cookie jar) postdate this corpus; the five-site witness below records what those two rows now do.

### What failed

| Cold run · 10 non-content outcomes | Count |
|---|---:|
| Shell | 4 |
| Human-interaction wall | 4 |
| Program wall | 2 |

The learned run's 13 non-content outcomes were six shells, four identity refusals, and three interaction walls. TikTok and Instagram supplied the six shells, Indeed three refusals and LinkedIn one, WSJ the three interaction walls. Standing (last round per target): TikTok and Instagram shells, Indeed identity, WSJ interaction — 24/28 content.

These outcomes stay in the denominator. A wall or shell is not scored as content.

### Evidence

[Aggregate receipts](benchmarks/fetch-2026-11-18.json) retain the original summary values and SHA256 hashes of the local source artifacts:

- `m7-hard-cold.json`
- `m7-hard-learned.json`

The full run files and private corpus are not distributed with the repository. The receipts are summaries of those saved measurements, not a new run or a public reproduction package. Egress, site changes, prior state, timeouts, and corpus composition all affect results.

## Five-site wall witness (5.5.7)

Recorded 18 November 2026 from the same home IP with the cold product binary (`dist-thin`), two rounds per site, a fresh state directory per round. These are the five sites a 5.5.6 reader reproduced as unchanged; the run records what the caller now gets.

| Site | Rounds | Result | Content | Wall clock |
|---|---|---|---|---|
| TikTok profile | 2 | content 2/2 | 213 chars (`97M Followers`, `469.2M Likes`) | 11.1–11.4 s |
| LinkedIn company | 5 | content 5/5 | 6,251 chars (`34,759,394 followers`) | 1.4–1.9 s |
| Google SERP | 2 | incomplete 2/2 | — | 0.5 s |
| Instagram profile | 2 | incomplete 2/2 | — | 1.0 s |
| Best Buy search | 2 | incomplete 2/2 | — | 1.4–2.1 s |

The incomplete sites are honest refusals: Google serves a JS-required notice and Best Buy a country chooser, both of which 5.5.6 returned as `content_ok` with the notice's own text. Instagram returns the real profile on the runs where its app renders it (506 chars, `686M Followers`) and the locale picker otherwise; the picker is now refused instead of served. Captures: `~/Projects/donsetch-evidence/five-2026-11-18/`. Conditions: no browser, no proxy, cold state per round, `--deadline-ms` 20 s (60 s for TikTok); TinyFish's recorded times on the same pages were 25.4 s for TikTok and 11.3 s for LinkedIn.

## Separate live-web field test

The original field report recorded about 90 manual reads across 75+ public sites on 15 November 2026. It used an ordinary home connection geolocating in Nepal and **already-learned state**, including 48 previously seen origins. It was a field observation, not the cold corpus above.

| Observation | Recorded result |
|---|---|
| Scored reads | 85 |
| Content | 60 · about 71% |
| Refused | 15, including one transport refusal |
| Shell or degraded | 7 |
| Size or timeout | 3 |
| Additional login gates | 4, reported separately as `wall.auth` |
| Median of 54 timed reads | 1.1 s · p90 3.8 s |
| One eight-URL batch | 6/8 content · 36.6 KB extracted · 5.1 s |

Observed content included LinkedIn job postings, Walmart listings, Amazon search results, Yahoo Finance quotes, Twitch streams, Reddit records, and publisher articles. The routes differed: some documents answered directly; others used the site's public interface.

Failures included Cloudflare, DataDome, Kasada, generic challenges, HTTP/2 refusals, thin shells, and obfuscated fields. Repeated Reddit reads degraded into long timeouts. Heavy pages also hit body or deadline limits. No browser was launched for page reads.

The sample was manually selected and not independently blinded. Its success rate must not be combined with the corpus score or used as a guarantee for a particular site.

## Search and competitor comparisons

No cold search-bench artifact is recorded for the current build, so this page makes no current search-accuracy claim.

In my [16 November comparison with paid Firecrawl Cloud](firecrawl-comparison.md), the agent preferred DonSeTch for fetching and focused crawling, and Firecrawl for search relevance. It compared 12 fetch targets, 9 search queries, 4 full crawls and a map, plus an unpaired DonSeTch warm diff. The write-up shows the faster calls, more useful crawl selection and smaller reading views, alongside specific defects, the environment, tools and settings. It's a hand-picked MCP field test with uncontrolled caches; the results describe that session.

The compiled ranker records a held-out MRR of 0.8467 versus fusion's 0.8070 over 400 saved rows in [rank_weights.rs](../src/search/rank_weights.rs). This is a ranker validation on saved candidates, not live end-to-end search accuracy.

The v4 Firecrawl comparison predates the fetch-engine replacement. Cloud/local peer comparisons without a recorded run remain unmeasured. They are not evidence that the current release is faster or more accurate than those products.
