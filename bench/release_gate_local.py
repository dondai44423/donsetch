#!/usr/bin/env python3
"""The 5.3.0 local-only release gate. No network calls.

The full gate (`release_gate.py`) requires the 100-row cloud and local head-to-heads
and their 300/200 blinded fact reviews. The owner scoped those out of 5.3.0: there is
no Exa/TinyFish key and no SearXNG instance on this machine. This gate enforces every
local clause the release can prove and records the peer clauses as **not measured**,
never as passed. Use `release_gate.py` again when the peers exist.

Usage: python3 bench/release_gate_local.py --baseline old.json --candidate new.json
  --rubric scored.json --out gate.json
"""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import statistics

import headtohead
from release_gate import valid_relevance_score

SKIPPED = [
    '100 paired cloud rows: Exa and TinyFish (no keys on this machine)',
    '300 blinded cloud fact reviews (no cloud run)',
    'cloud accuracy/AR and Exa/TinyFish comparisons (no cloud run)',
    'same-machine local comparison and 200 local fact reviews (no SearXNG instance)',
    'paired evidence tokens (no peer evidence)',
]


def rate(data, pred):
    return sum(bool(pred(r)) for r in data) / len(data) if data else 0


def check_artifacts(baseline, candidate, rubric):
    checks = []

    def check(name, passed, detail):
        checks.append({'clause': name, 'passed': bool(passed), 'detail': detail})

    rows = candidate.get('queries', [])
    answers = [r for r in rows if r.get('expect', 'answer') == 'answer']
    held = [r for r in answers if r.get('set') == 'held']
    distinct = {r.get('id') for r in rows}
    hours = {int(t) // 3600 for t in candidate.get('run_started_unix', [])}
    counts = Counter(r.get('id') for r in rows)
    runs = candidate.get('runs', 0)
    check('full cold corpus across three hours',
          runs >= 3 and bool(counts) and all(n == runs for n in counts.values())
          and len(distinct) >= 150 and len(hours) >= 3 and candidate.get('cold') is True
          and candidate.get('pace_ms', 0) >= 2500,
          {'rows': len(distinct), 'hours': len(hours), 'runs': runs})
    same = (baseline.get('queries_sha256') and baseline.get('queries_sha256') == candidate.get('queries_sha256'))
    old_ids = {r.get('id') for r in baseline.get('queries', [])}
    same = same and old_ids == distinct and None not in distinct
    check('same corpus as baseline', same, candidate.get('queries_sha256'))
    split = Counter(r.get('set') for r in {r.get('id'): r for r in rows}.values())
    check('half the corpus is held out', bool(distinct) and split['held'] >= len(distinct) / 2, dict(split))
    check('frozen measured binary', len(candidate.get('binary_sha256', '')) == 64, candidate.get('binary_sha256'))
    check('default search settings; file configuration disabled',
          candidate.get('file_config') == 'disabled' and not candidate.get('search_env_overrides'),
          candidate.get('search_env_overrides'))
    check('cold runs preserve active source refusals',
          candidate.get('refusal_policy') == 'preserve-active-source-refusals-v1',
          candidate.get('refusal_policy'))
    zero = rate(answers, lambda r: not r.get('urls'))
    junk = rate(rows, lambda r: r.get('junk5', 0) > 0)
    hit3 = rate(held, lambda r: r.get('hit') and r['hit'] <= 3)
    check('first-call zero rate <=2%', bool(answers) and zero <= .02, zero)
    check('junk@5 <=5%', bool(rows) and junk <= .05, junk)
    check('held hit@3 >=85%', bool(held) and hit3 >= .85, hit3)
    old = baseline.get('queries', [])
    shape_losses = {}
    for shape in {r.get('shape') for r in held}:
        before = [r for r in old if r.get('shape') == shape and r.get('set') == 'held'
                  and r.get('expect', 'answer') == 'answer']
        after = [r for r in held if r.get('shape') == shape]
        delta = rate(after, lambda r: r.get('hit') and r['hit'] <= 3) - rate(before, lambda r: r.get('hit') and r['hit'] <= 3)
        if not before or delta < -.02 - 1e-9:
            shape_losses[shape] = delta
    check('no held shape loses >2 points', bool(held) and not shape_losses, shape_losses)
    operator = [r for r in rows if r.get('shape') in ('operator', 'adversarial')]
    operator_violations = [r.get('id') for r in operator if r.get('violations')]
    url_purity = [r.get('id') for r in rows if any(r.get(k, 0) for k in ('redirects', 'stale', 'undated', 'foreign'))]
    bad_refusals = [r.get('id') for r in operator if r.get('expect') == 'refuse' and len(r.get('urls', [])) >= 3]
    check('operator contracts and URL purity 100%',
          bool(operator) and not operator_violations and not url_purity and not bad_refusals,
          {'operator_violations': operator_violations, 'url_purity': url_purity, 'false_answers': bad_refusals})
    latencies = sorted(r.get('ms', 10**9) for r in answers)
    p50 = statistics.median(latencies) if latencies else 10**9
    p95 = latencies[min(int(len(latencies) * .95), len(latencies) - 1)] if latencies else 10**9
    requests = [r['requests'] for r in answers if r.get('requests') is not None]
    check('p50 <=1.75s and p95 <=2.5s', p50 <= 1750 and p95 <= 2500, {'p50_ms': p50, 'p95_ms': p95})
    check('median requests <=5',
          candidate.get('request_accounting') == 'http-exchanges-v1'
          and candidate.get('default_pipeline') == 'native-search-and-default-repair-v1'
          and len(requests) == len(answers) and bool(requests) and statistics.median(requests) <= 5,
          statistics.median(requests) if requests else None)
    full_tokens = [headtohead.evidence_tokens(r.get('evidence', []))
                   if r.get('evidence') or not r.get('urls') else None for r in rows]
    check('full 12-result evidence <=1500 measured tokens',
          bool(full_tokens) and candidate.get('max_results') == 12
          and all(isinstance(n, int) and n <= 1500 for n in full_tokens),
          {'max_tokens': max((n for n in full_tokens if n is not None), default=None),
           'unmeasured_rows': sum(n is None for n in full_tokens),
           'tokenizer': 'cl100k_base' if headtohead.tiktoken else None})
    scores = rubric.get('scores', [])
    ids = {s.get('id') for s in scores}
    held_ids = {r.get('id') for r in held}
    check('50 blinded rubric rows',
          len(ids) == len(scores) == 50 and ids <= held_ids and rubric.get('blinded') is True
          and rubric.get('old_binary_sha256') == baseline.get('binary_sha256')
          and rubric.get('new_binary_sha256') == candidate.get('binary_sha256')
          and rubric.get('queries_sha256') == candidate.get('queries_sha256')
          and rubric.get('old_sha256_matches') is True and rubric.get('new_sha256_matches') is True
          and all(valid_relevance_score(s) for s in scores)
          and sum(s['winner'] == 'new' for s in scores) > sum(s['winner'] == 'old' for s in scores),
          dict(Counter(s.get('winner') for s in scores)))
    held_before = [r for r in old if r.get('set') == 'held' and r.get('expect', 'answer') == 'answer']
    gain = hit3 - rate(held_before, lambda r: r.get('hit') and r['hit'] <= 3)
    check('held hit@3 improves on 5.2.1', bool(held_before) and gain > 0, gain)
    return {'scope': 'local-only-5.3.0', 'passed': all(c['passed'] for c in checks),
            'checks': checks, 'skipped': SKIPPED}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('baseline', 'candidate', 'rubric'):
        parser.add_argument('--' + name, required=True)
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    artifacts = []
    missing = []
    for name in ('baseline', 'candidate', 'rubric'):
        path = Path(getattr(args, name))
        try:
            artifacts.append(json.loads(path.read_text()))
        except (OSError, ValueError):
            artifacts.append({})
            missing.append(str(path))
    baseline, candidate, rubric = artifacts
    for version, source in (('old', args.baseline), ('new', args.candidate)):
        path = Path(source)
        rubric[version + '_sha256_matches'] = (path.is_file()
            and rubric.get(version + '_artifact_sha256') == hashlib.sha256(path.read_bytes()).hexdigest())
    verdict = check_artifacts(baseline, candidate, rubric)
    verdict['missing'] = missing
    verdict['passed'] = verdict['passed'] and not missing
    verdict['artifacts'] = {name: {'path': getattr(args, name),
        'sha256': hashlib.sha256(Path(getattr(args, name)).read_bytes()).hexdigest()}
        for name in ('baseline', 'candidate', 'rubric') if Path(getattr(args, name)).is_file()}
    Path(args.out).write_text(json.dumps(verdict, indent=2) + '\n')
    for entry in verdict['checks']:
        print(('PASS ' if entry['passed'] else 'FAIL ') + entry['clause'])
    for clause in verdict['skipped']:
        print('SKIP ' + clause)
    return 0 if verdict['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
