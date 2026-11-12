#!/usr/bin/env python3
"""Fail closed on every local-search release requirement. No network calls.

Usage: python3 bench/release_gate.py --baseline old.json --candidate new.json
  --cloud cloud.json --local local.json --rubric scored.json --out gate.json
Missing, incomplete or incomparable evidence blocks release.
"""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import statistics
import headtohead


def valid_relevance_score(score):
    old, new = score.get('old'), score.get('new')
    if not all(type(n) is int and 0 <= n <= 30 for n in (old, new)):
        return False
    winner = 'new' if new > old else 'old' if old > new else 'tie'
    return (score.get('winner') == winner and isinstance(score.get('reason'), str)
        and bool(score['reason'].strip()))


def check_artifacts(baseline, candidate, cloud, local, rubric, fact_review=None):
    checks = []
    def check(name, passed, detail):
        checks.append({'clause': name, 'passed': bool(passed), 'detail': detail})
    rows = candidate.get('queries', [])
    answers = [r for r in rows if r.get('expect', 'answer') == 'answer']
    held = [r for r in answers if r.get('set') == 'held']
    distinct = {r.get('id') for r in rows}
    hours = {int(t)//3600 for t in candidate.get('run_started_unix', [])}
    counts = Counter(r.get('id') for r in rows)
    runs = candidate.get('runs', 0)
    balanced = runs >= 3 and bool(counts) and all(n == runs for n in counts.values())
    check('full cold corpus across three hours', balanced and len(distinct) >= 150 and len(hours) >= 3
          and candidate.get('runs', 0) >= 3 and candidate.get('cold') is True and candidate.get('pace_ms', 0) >= 2500, {'rows': len(distinct), 'hours': len(hours)})
    same = baseline.get('queries_sha256') and baseline.get('queries_sha256') == candidate.get('queries_sha256')
    old_ids = {r.get('id') for r in baseline.get('queries', [])}
    same = same and old_ids == distinct and None not in distinct
    check('same corpus as baseline', same, candidate.get('queries_sha256'))
    split = Counter(r.get('set') for r in {r.get('id'): r for r in rows}.values())
    check('half the corpus is held out', bool(distinct) and split['held'] >= len(distinct)/2, dict(split))
    check('frozen measured binary', len(candidate.get('binary_sha256', '')) == 64, candidate.get('binary_sha256'))
    check('default search settings; file configuration disabled', candidate.get('file_config') == 'disabled'
        and not candidate.get('search_env_overrides'), candidate.get('search_env_overrides'))
    check('cold runs preserve active source refusals', candidate.get('refusal_policy') == 'preserve-active-source-refusals-v1', candidate.get('refusal_policy'))
    fingerprint = candidate.get('binary_sha256')
    check('peer comparisons measure the same candidate binary', bool(fingerprint)
        and all(data.get('binary', {}).get('sha256') == fingerprint for data in (cloud, local)),
        {name: data.get('binary', {}).get('sha256') for name, data in (('cloud', cloud), ('local', local))})
    check('peer comparisons use default native settings', all(data.get('native_file_config') == 'disabled'
        and data.get('native_env_overrides') == [] for data in (cloud, local)),
        {name: data.get('native_env_overrides') for name, data in (('cloud', cloud), ('local', local))})
    def rate(data, pred):
        return sum(bool(pred(r)) for r in data)/len(data) if data else 0
    zero = rate(answers, lambda r: not r.get('urls'))
    junk = rate(rows, lambda r: r.get('junk5', 0) > 0)
    hit3 = rate(held, lambda r: r.get('hit') and r['hit'] <= 3)
    check('first-call zero rate <=2%', bool(answers) and zero <= .02, zero)
    check('junk@5 <=5%', bool(rows) and junk <= .05, junk)
    check('held hit@3 >=85%', bool(held) and hit3 >= .85, hit3)
    old = baseline.get('queries', [])
    shape_losses = {}
    for shape in {r.get('shape') for r in held}:
        before = [r for r in old if r.get('shape') == shape and r.get('set') == 'held' and r.get('expect', 'answer') == 'answer']
        after = [r for r in held if r.get('shape') == shape]
        delta = rate(after, lambda r: r.get('hit') and r['hit'] <= 3) - rate(before, lambda r: r.get('hit') and r['hit'] <= 3)
        if not before or delta < -.02-1e-9:
            shape_losses[shape] = delta
    check('no held shape loses >2 points', bool(held) and not shape_losses, shape_losses)
    operator = [r for r in rows if r.get('shape') in ('operator', 'adversarial')]
    operator_violations = [r.get('id') for r in operator if r.get('violations')]
    url_purity = [r.get('id') for r in rows if any(r.get(k, 0) for k in ('redirects','stale','undated','foreign'))]
    bad_refusals = [r.get('id') for r in operator if r.get('expect') == 'refuse' and len(r.get('urls', [])) >= 3]
    check('operator contracts and URL purity 100%', bool(operator) and not operator_violations and not url_purity and not bad_refusals,
          {'operator_violations': operator_violations, 'url_purity': url_purity, 'false_answers': bad_refusals})
    latencies = sorted(r.get('ms', 10**9) for r in answers)
    p50 = statistics.median(latencies) if latencies else 10**9
    p95 = latencies[min(int(len(latencies)*.95), len(latencies)-1)] if latencies else 10**9
    requests = [r['requests'] for r in answers if r.get('requests') is not None]
    check('p50 <=1.75s and p95 <=2.5s', p50 <= 1750 and p95 <= 2500, {'p50_ms': p50, 'p95_ms': p95})
    check('median requests <=5', candidate.get('request_accounting') == 'http-exchanges-v1' and candidate.get('default_pipeline') == 'native-search-and-default-repair-v1' and len(requests) == len(answers) and bool(requests) and statistics.median(requests) <= 5,
          statistics.median(requests) if requests else None)
    cloud_rows = cloud.get('rows', [])
    summary = {p: headtohead.summarize([r for r in cloud_rows if r.get('provider') == p])
        for p in ('donsetch','exa','tinyfish') if any(r.get('provider') == p for r in cloud_rows)}
    ours = summary.get('donsetch', {})
    cohorts = {p: {r['id'] for r in cloud_rows if r.get('provider') == p} for p in ('donsetch','exa','tinyfish')}
    complete = len(cohorts['donsetch']) == 100 and all(v == cohorts['donsetch'] for v in cohorts.values())
    complete = complete and all(sum(r.get('provider') == p for r in cloud_rows) == 100 for p in cohorts)
    complete = complete and cloud.get('pace_ms', 0) >= 2500 and bool(cloud.get('finished_unix'))
    check('100 paired cloud rows: Exa and TinyFish', complete and all(not summary.get(p, {}).get('errors', 1) for p in cohorts),
          {p: len(v) for p, v in cohorts.items()})
    fact_review = fact_review or {}
    reviewed = fact_review.get('rows', [])
    cloud_reviewed = [r for r in reviewed if r.get('cohort') == 'cloud']
    pairs = {(r.get('provider'), r.get('id')): r for r in cloud_reviewed}
    expected_pairs = {(provider, rid) for provider, ids in cohorts.items() for rid in ids}
    fact_valid = (complete and fact_review.get('blinded') is True
        and fact_review.get('cloud_sha256_matches') is True
        and fact_review.get('requirements_sha256_matches') is True
        and set(pairs) == expected_pairs and len(pairs) == len(cloud_reviewed) == 300
        and all(type(r.get('extractable')) is bool and bool(r.get('reason')) for r in cloud_reviewed))
    check('300 blinded fact-extractability reviews bound to the measured cloud run', fact_valid,
        {'reviewed': len(cloud_reviewed), 'required': 300})
    # Topic-word coverage stays a separate diagnostic. Only the blinded fact
    # review can supply the release's accuracy definition.
    for provider, metrics in summary.items():
        metrics['accuracy'] = (sum(pairs[(provider, rid)]['extractable'] for rid in cohorts[provider])/100
            if fact_valid else None)
    check('cloud accuracy >=82%, AR1 >=65%, AR5 >=90%', fact_valid
          and ours.get('accuracy', 0) >= .82 and ours.get('ar1', 0) >= .65 and ours.get('ar5', 0) >= .90, ours)
    exa = summary.get('exa', {})
    tiny = summary.get('tinyfish', {})
    check('within 10 points of Exa fast', fact_valid and cloud.get('exa_type') == 'fast'
          and all(ours.get(k, 0) >= exa.get(k, 1)-.10 for k in ('accuracy', 'ar1')), exa)
    check('beats TinyFish on AR1 and AR5', fact_valid
          and all(ours.get(k, 0) > tiny.get(k, 1) for k in ('ar1', 'ar5')), tiny)
    local_rows = local.get('rows', [])
    local_summary = {p: headtohead.summarize([r for r in local_rows if r.get('provider') == p])
        for p in ('donsetch','searxng','4get','librey') if any(r.get('provider') == p for r in local_rows)}
    local_ours = local_summary.get('donsetch', {})
    availability = local.get('availability', {})
    peers = ['searxng'] + [p for p in ('4get','librey') if p in local_summary]
    absent = [p for p in ('4get','librey') if p not in peers]
    unavailable_proved = all(availability.get(p, {}).get('json_api_available') is False
        and availability[p].get('endpoint') and availability[p].get('checked_unix')
        and availability[p].get('outcome')
        and abs(availability[p]['checked_unix'] - local.get('started_unix', 0)) <= 3600 for p in absent)
    local_ids = {r.get('id') for r in local_rows if r.get('provider') == 'donsetch'}
    local_reviewed = [r for r in reviewed if r.get('cohort') == 'local']
    local_pairs = {(r.get('provider'), r.get('id')): r for r in local_reviewed}
    expected_local = {(p, rid) for p in ['donsetch'] + peers for rid in local_ids}
    local_facts_valid = (fact_valid and fact_review.get('local_sha256_matches') is True
        and len(local_ids) == 100 and set(local_pairs) == expected_local
        and len(local_pairs) == len(local_reviewed)
        and all(type(r.get('extractable')) is bool and bool(r.get('reason')) for r in local_reviewed))
    for provider, metrics in local_summary.items():
        metrics['accuracy'] = (sum(local_pairs[(provider, rid)]['extractable'] for rid in local_ids)/100
            if local_facts_valid else None)
    local_ok = (local_facts_valid and len(local_ids) == 100 and local_ids == cohorts['donsetch']
        and local.get('queries_sha256') == cloud.get('queries_sha256')
        and bool(local.get('machine')) and local.get('machine') == cloud.get('machine') and unavailable_proved
        and local.get('pace_ms', 0) >= 2500 and bool(local.get('finished_unix'))
        and local_ours.get('n') == 100 and not local_ours.get('errors', 1)
        and all(p in local_summary and local_summary[p].get('n') == 100
        and {r.get('id') for r in local_rows if r.get('provider') == p} == local_ids
        and not local_summary[p].get('errors', 1)
        and local_ours.get('accuracy', 0) > local_summary[p].get('accuracy', 1)
        and local_ours.get('p50_ms', 10**9) <= local_summary[p].get('p50_ms', 0) + 250 for p in peers))
    check('same-machine local comparison; unavailable public JSON peers disclosed', local_ok, local_summary)
    evidence = [r for r in cloud_rows + local_rows if r.get('provider') == 'donsetch']
    full_tokens = [headtohead.evidence_tokens(r.get('evidence', []))
        if r.get('evidence') or not r.get('urls') else None for r in rows]
    check('full 12-result evidence <=1500 measured tokens', bool(full_tokens)
        and candidate.get('max_results') == 12
        and all(isinstance(n, int) and n <= 1500 for n in full_tokens),
        {'max_tokens': max((n for n in full_tokens if n is not None), default=None),
         'unmeasured_rows': sum(n is None for n in full_tokens), 'tokenizer': 'cl100k_base' if headtohead.tiktoken else None})
    check('paired evidence <=1500 measured tokens', bool(evidence) and all(
        isinstance(r.get('evidence_tokens'), int) and r['evidence_tokens'] <= 1500 for r in evidence)
        and cloud.get('tokenizer') == local.get('tokenizer') == 'cl100k_base',
        max((r.get('evidence_tokens') or 0 for r in evidence), default=None))
    scores = rubric.get('scores', [])
    ids = {s.get('id') for s in scores}
    held_ids = {r.get('id') for r in held}
    check('50 blinded rubric rows', len(ids) == len(scores) == 50 and ids <= held_ids and rubric.get('blinded') is True
          and rubric.get('old_binary_sha256') == baseline.get('binary_sha256')
          and rubric.get('new_binary_sha256') == candidate.get('binary_sha256')
          and rubric.get('queries_sha256') == candidate.get('queries_sha256')
          and rubric.get('old_sha256_matches') is True and rubric.get('new_sha256_matches') is True
          and all(valid_relevance_score(s) for s in scores)
          and sum(s['winner'] == 'new' for s in scores) > sum(s['winner'] == 'old' for s in scores),
          dict(Counter(s.get('winner') for s in scores)))
    held_before = [r for r in old if r.get('set') == 'held' and r.get('expect', 'answer') == 'answer']
    gain = hit3-rate(held_before, lambda r: r.get('hit') and r['hit'] <= 3)
    check('held hit@3 improves on 5.2.1', bool(held_before) and gain > 0, gain)
    return {'passed': all(c['passed'] for c in checks), 'checks': checks}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('baseline','candidate','cloud','local','rubric'):
        parser.add_argument('--'+name, required=True)
    parser.add_argument('--requirements', default=str(Path(__file__).resolve().parent/'search/live_requirements.json'))
    parser.add_argument('--fact-review', required=True, help='blinded answer-extractability review, separate from phrase keys')
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    artifacts = []
    missing = []
    for name in ('baseline','candidate','cloud','local','rubric'):
        path = Path(getattr(args, name))
        try:
            artifacts.append(json.loads(path.read_text()))
        except (OSError, ValueError):
            artifacts.append({})
            missing.append(str(path))
    rubric = artifacts[4]
    for version, source in (('old', args.baseline), ('new', args.candidate)):
        path = Path(source)
        rubric[version+'_sha256_matches'] = (path.is_file()
            and rubric.get(version+'_artifact_sha256') == hashlib.sha256(path.read_bytes()).hexdigest())
    try:
        fact_review = json.loads(Path(args.fact_review).read_text())
        fact_review['cloud_sha256_matches'] = fact_review.get('cloud_sha256') == hashlib.sha256(Path(args.cloud).read_bytes()).hexdigest()
        fact_review['local_sha256_matches'] = fact_review.get('local_sha256') == hashlib.sha256(Path(args.local).read_bytes()).hexdigest()
        fact_review['requirements_sha256_matches'] = fact_review.get('requirements_sha256') == hashlib.sha256(Path(args.requirements).read_bytes()).hexdigest()
    except (OSError, ValueError):
        fact_review = {}
        missing.append(args.fact_review)
    verdict = check_artifacts(*artifacts, fact_review)
    verdict['missing'] = missing
    verdict['passed'] &= not missing
    verdict['artifacts'] = {name: {'path': getattr(args, name),
        'sha256': hashlib.sha256(Path(getattr(args, name)).read_bytes()).hexdigest()}
        for name in ('baseline','candidate','cloud','local','rubric') if Path(getattr(args, name)).is_file()}
    if Path(args.fact_review).is_file():
        verdict['artifacts']['fact_review'] = {'path': args.fact_review, 'sha256': hashlib.sha256(Path(args.fact_review).read_bytes()).hexdigest()}
    Path(args.out).write_text(json.dumps(verdict, indent=2)+'\n')
    for check in verdict['checks']:
        print(('PASS ' if check['passed'] else 'FAIL ') + check['clause'])
    return 0 if verdict['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
