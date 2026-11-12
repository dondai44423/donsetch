#!/usr/bin/env python3
"""Offline regressions for release evidence and comparison arithmetic."""
import contextlib
from concurrent.futures import ThreadPoolExecutor
import io
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
import urllib.error
from unittest.mock import patch
import compare
import blind_review
import headtohead
import release_gate


class ProtocolTests(unittest.TestCase):
    def test_cold_measurement_keeps_refusals_without_inheriting_learned_state(self):
        with tempfile.TemporaryDirectory() as temp, patch('headtohead.time.time', return_value=1000):
            source, target = Path(temp)/'source.json', Path(temp)/'target.json'
            def lane(cause, until):
                return {'rest_cause':cause, 'rest_until':until, 'rest_secs':900,
                        'tokens':-12, 'fails':8, 'last_ok':900}
            source.write_text(json.dumps({'version':1, 'lanes':{
                'quota':lane('retry-after', 1900), 'blocked':lane('refusal', 1020),
                'expired':lane('retry-after', 999), 'timeout':lane('backoff', 1900),
                'legacy':{'rest_until':1100, 'rest_secs':100}}}))
            headtohead.carry_refusals(source, target)
            copied = json.loads(target.read_text())['lanes']
            self.assertEqual(set(copied), {'quota', 'blocked', 'legacy'})
            self.assertEqual(copied['quota']['rest_until'], 1900)
            self.assertEqual(copied['quota']['fails'], 0)
            self.assertEqual(copied['quota']['tokens'], 2)
            saved = json.loads(target.read_text())
            saved['lanes']['origin:https://expired.example|http'] = lane('retry-after', 999)
            target.write_text(json.dumps(saved))
            headtohead.carry_refusals(source, target)
            self.assertNotIn('origin:https://expired.example|http', json.loads(target.read_text())['lanes'])
            sources = []
            for i in range(4):
                path = Path(temp)/f'source-{i}.json'
                path.write_text(json.dumps({'version':1,'lanes':{f'new-{i}':lane('retry-after',1900)}}))
                sources.append(path)
            with ThreadPoolExecutor(max_workers=4) as pool:
                list(pool.map(lambda source: headtohead.carry_refusals(source, target), sources))
            self.assertEqual(len(json.loads(target.read_text())['lanes']), 7)

    def test_comparison_retains_every_repeated_row(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp)/'runs.json'
            path.write_text(json.dumps({'queries': [{'id': 'x', 'hit': 1}, {'id': 'x', 'hit': 5}]}))
            _, rows = compare.load(path)
            self.assertEqual(len(rows), 2)
            self.assertEqual([r['hit'] for r in rows.values()], [1, 5])

    def test_different_corpora_are_not_a_comparison(self):
        with tempfile.TemporaryDirectory() as temp:
            paths = [Path(temp)/'a.json', Path(temp)/'b.json']
            for path, fingerprint in zip(paths, ['aaa', 'bbb']):
                path.write_text(json.dumps({'queries_sha256': fingerprint, 'queries': [{'id': 'x'}]}))
            with self.assertRaises(SystemExit):
                compare.main(['compare.py', *map(str, paths)])

    def test_empty_release_evidence_cannot_pass(self):
        verdict = release_gate.check_artifacts({}, {}, {}, {}, {})
        self.assertFalse(verdict['passed'])
        self.assertTrue(all(not c['passed'] for c in verdict['checks']))
        self.assertFalse(release_gate.valid_relevance_score({'winner':'new'}))
        score = {'old':10, 'new':20, 'winner':'new', 'reason':'The new excerpts supply usable instructions.'}
        self.assertTrue(release_gate.valid_relevance_score(score))
        for delta in ({'winner':'old'}, {'reason':' '}, {'new':31}, {'old':True}):
            self.assertFalse(release_gate.valid_relevance_score({**score, **delta}))

    def test_duplicate_partial_runs_cannot_claim_three_full_runs(self):
        candidate = {'runs': 3, 'cold': True, 'pace_ms': 2500,
            'run_started_unix': [3600, 7200, 10800],
            'queries': [{'id': str(i), 'expect': 'answer', 'set': 'held',
                         'shape': 'web', 'urls': ['https://example.com'], 'hit': 1,
                         'requests': 1, 'ms': 100} for i in range(150)]}
        verdict = release_gate.check_artifacts({}, candidate, {}, {}, {})
        self.assertFalse(verdict['checks'][0]['passed'])

    def test_blind_fact_review_cannot_reveal_incomplete_or_changed_evidence(self):
        packet = {'kind':'facts', 'blinded':True, 'queries':[{'cohort':'cloud','id':'x',
            'query':'What is the fact?', 'requirement':'State the fact.',
            'answers':{'A':[]}, 'extractable':{'A':None}, 'reasons':{'A':''}}]}
        mapping = {'kind':'facts', 'requirements_sha256':'a'*64, 'artifacts':{},
            'evidence_sha256':blind_review.evidence_hash(packet),
            'assignments':[{'cohort':'cloud','id':'x','label':'A','provider':'exa'}]}
        with self.assertRaises(ValueError):
            blind_review.reveal(packet, mapping)
        packet['queries'][0]['extractable']['A'] = False
        packet['queries'][0]['reasons']['A'] = 'No evidence was returned.'
        revealed = blind_review.reveal(packet, mapping)
        self.assertFalse(revealed['rows'][0]['extractable'])
        packet['queries'][0]['query'] = 'A different question'
        with self.assertRaises(ValueError):
            blind_review.reveal(packet, mapping)

    def test_provider_failures_remain_in_the_denominator(self):
        rows = [dict(hit=1, key_at=1, junk5=0, ms=100, results=[{}]),
                dict(hit=None, key_at=None, junk5=0, ms=100, results=[], error='HTTP 429')]
        summary = headtohead.summarize(rows)
        self.assertEqual(summary['key_coverage'], .5)
        self.assertNotIn('accuracy', summary)
        self.assertEqual(summary['zero'], .5)
        self.assertEqual(summary['errors'], 1)
        with patch('headtohead.time.time', return_value=1000):
            for status, headers, expected in [(429, {}, 1900), (403, {}, 1045),
                    (429, {'Retry-After':'7200'}, 8200),
                    (503, {'Retry-After':'Thu, 01 Jan 1970 00:20:00 GMT'}, 1200),
                    (500, {}, 0)]:
                error = urllib.error.HTTPError('https://example.com', status, 'refused', headers, None)
                self.assertEqual(headtohead.refusal_until(error), expected)
                error.close()

    def test_relevance_review_binds_files_and_rejects_unscored_slots(self):
        with tempfile.TemporaryDirectory() as temp:
            paths = [Path(temp)/'old.json', Path(temp)/'new.json']
            rows = [{'id':str(i), 'set':'held', 'expect':'answer', 'shape':'web',
                'query':f'question {i}', 'urls':['https://example.com/fact'],
                'evidence':[{'title':'Fact', 'url':'https://example.com/fact', 'snippet':'An actual answer.'}]}
                for i in range(50)]
            for path, version in zip(paths, ('a','b')):
                path.write_text(json.dumps({'queries_sha256':'c'*64, 'binary_sha256':version*64, 'queries':rows}))
            args = type('Args', (), {'old':str(paths[0]), 'new':str(paths[1])})()
            packet, mapping = blind_review.export_relevance(args)
            mapping['evidence_sha256'] = blind_review.evidence_hash(packet)
            with self.assertRaises(ValueError):
                blind_review.reveal(packet, mapping)
            for row in packet['queries']:
                row['scores'] = {'A':[2,0,0,0,0], 'B':[2,0,0,0,0]}
                row['reason'] = 'Both lists supply the same usable fact; missing slots score zero.'
            review = blind_review.reveal(packet, mapping)
            self.assertEqual(review['old_artifact_sha256'], hashlib.sha256(paths[0].read_bytes()).hexdigest())
            self.assertEqual(review['new_artifact_sha256'], hashlib.sha256(paths[1].read_bytes()).hexdigest())
            self.assertTrue(all(release_gate.valid_relevance_score(s) for s in review['scores']))
            packet['queries'][0]['scores']['A'][1] = 2
            with self.assertRaises(ValueError):
                blind_review.reveal(packet, mapping)

    def test_answer_key_requires_returned_evidence_and_limits_are_equal(self):
        q = {'gold': ['example.com/fact'], 'answer_key': ['42'], 'must_not': ['wrong.example']}
        rows = [{'url': 'https://example.com/fact/42', 'title': 'A fact', 'snippet': 'Unknown'}]
        self.assertEqual(headtohead.judge(q, rows)['hit'], 1)
        self.assertIsNone(headtohead.judge(q, rows)['key_at'])
        q['answer_key'] = ['42', 'million']
        rows[0]['snippet'] = '42'
        self.assertIsNone(headtohead.judge(q, rows)['key_at'])
        rows[0]['snippet'] = '42 million'
        self.assertEqual(headtohead.judge(q, rows)['key_at'], 1)
        limited = headtohead.normalize([{'title':'T','url':'https://example.com','snippet':'x'*2000}]*8)
        self.assertEqual([len(r['snippet']) for r in limited], [320,320,320,180,180])

    def test_exclusions_are_not_cli_flags(self):
        args = type('Args', (), {'bin': '/example/donsetch'})()
        proc = type('Proc', (), {'returncode': 0, 'stdout': '{"meta":{"results":[]}}'})()
        with patch('headtohead.subprocess.run', return_value=proc) as run:
            headtohead.search('donsetch', '-python javascript array sort', args, '/tmp/cache')
            self.assertEqual(run.call_args.args[0][-2:], ['--', '-python javascript array sort'])
        with patch.dict('os.environ', {'DONSETCH_STATE__NO_DISK_STATE':'true'}), \
             patch('headtohead.search') as acquire, contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                headtohead.main(['--include','donsetch','--out','unused.json'])
            acquire.assert_not_called()


if __name__ == '__main__':
    unittest.main()
