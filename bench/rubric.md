# Search relevance rubric

Owns: the blinded review protocol and the meaning of its scores.

Freeze 50 queries and their answer requirements before reading results. Use
held rows, with coverage across shapes. Run both builds in paired time windows,
with identical result and snippet budgets, empty caches and no BYOK.

Export the top five for each query as two anonymous lists, randomly assigned
A/B separately per query. Keep the mapping in a separate private file. The
reviewer must not read it until all scores have been recorded. An automatic
URL/answer-key match is a separate metric, not a blinded relevance review.

Score each result:

- **2:** directly answers the query, meets its constraints, and supplies usable
  evidence in the returned title/snippet.
- **1:** relevant and useful to fetch, but partial or missing the answering fact.
- **0:** unrelated, wrong entity, duplicate, forbidden by an operator, or a wall.

Missing result slots score zero. Weight positions 5, 4, 3, 2, 1, then compare
weighted totals per query. Record a tie when totals are equal. Record a short
reason for each zero and each disagreement between relevance and URL metrics.
After scoring, reveal the mapping and export `{ "blinded": true, "scores":
[{"id": "…", "winner": "new|old|tie", "old": 0, "new": 0, "reason": "…"}] }`.
The new build must win more queries than it loses. A missing review blocks release.

Create the anonymous relevance packet with:

```sh
python3 bench/blind_review.py relevance --old OLD.json --new NEW.json \
  --out bench/results/relevance-blind.json --mapping-out PRIVATE-MAPPING.json
```

The exporter chooses 50 held answer rows across shapes with fixed seed 7. Keep
the mapping unread, fill every result score and reason in the packet, then use
`blind_review.py reveal --scores SCORED.json --mapping PRIVATE-MAPPING.json
--out bench/results/relevance-reviewed.json`. The reveal verifies that the
question and evidence stayed unchanged; missing scores or reasons fail.

Answer accuracy is a separate blinded review. Freeze
`bench/search/live_requirements.json` before scoring. Export the cloud and
local comparisons with `blind_review.py facts --cloud CLOUD.json --local
LOCAL.json --out FACTS.json --mapping-out PRIVATE-FACT-MAPPING.json`. For each
anonymous list, record `extractable: true` only when its returned title and
snippet evidence satisfy the semantic requirement. Verify uncertain facts
against the primary reference named in that requirement; mere topic words or
a promising URL do not supply a factual answer. Navigation requirements can
be met by the authentic named record. Missing evidence and uncertainty score
false. Record a reason for every decision, finish every list before reading
the mapping, and reveal using the same command as above.

The release check binds both reviews to the measured artifact hashes and
binary versions. Relevance totals must be integers from 0 to 30, include reasons,
and agree with their declared winners. The cloud review covers 300 lists; the local review covers
at least 200. URL retrieval and phrase coverage remain separate diagnostics.
