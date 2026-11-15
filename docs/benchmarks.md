# Measurements

Recorded results, their conditions, and what they establish. [Back to the README](../README.md).

## Defended-site fetch corpus

Recorded 15 November 2026 from a home IP. The corpus has 28 defended targets.

| Protocol | Content | Median | p95 |
|---|---:|---:|---:|
| Cold process · up to 3 attempts · 2 rounds | 49/56 | 1.18 s | 2.87 s |
| Paired curl · same cold measurement window | 21/56 | — | — |
| Learned state · no page cache · 3 s deadline · 3 rounds | 69/84 | 0.98 s | 3.03 s |

The cold and learned rows use different protocols. Their medians are not a controlled measure of learning's speedup. The learned run has no paired curl measurement.

Both runs used the identity/interface/challenge routes; no row used `transport: program` or `replay`. These numbers do not establish the content lane's success rate.

### What failed

| Cold run · 7 non-content outcomes | Count |
|---|---:|
| Shell | 4 |
| Human-interaction wall | 2 |
| Network | 1 |

The learned run's 15 non-content outcomes were six shells, five interaction walls, two incomplete documents, one program wall, and one network failure. TikTok and Instagram supplied the six shells. AliExpress and WSJ were incomplete. Product Hunt supplied the program/network failures.

These outcomes stay in the denominator. A wall or shell is not scored as content.

### Evidence

[Aggregate receipts](benchmarks/fetch-2026-11-15.json) retain the original summary values and SHA256 hashes of the local source artifacts:

- `m6-hard-cold.json`
- `m6-hard-learned.json`

The full run files and private corpus are not distributed with the repository. The receipts are summaries of those saved measurements, not a new run or a public reproduction package. Egress, site changes, prior state, timeouts, and corpus composition all affect results.

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

The compiled ranker records a held-out MRR of 0.8467 versus fusion's 0.8070 over 400 saved rows in [rank_weights.rs](../src/search/rank_weights.rs). This is a ranker validation on saved candidates, not live end-to-end search accuracy.

The v4 Firecrawl comparison predates the fetch-engine replacement. Cloud/local peer comparisons without a recorded run remain unmeasured. They are not evidence that the current release is faster or more accurate than those products.
