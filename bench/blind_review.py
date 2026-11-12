#!/usr/bin/env python3
"""Export anonymous search evidence and reveal only fully completed reviews.

Keep --mapping-out private and unread until every score is recorded. This tool
never scores a result automatically: URL/phrase metrics are separate evidence.
"""
import argparse
from collections import defaultdict
import hashlib
import json
from pathlib import Path
import random
import secrets
import headtohead

ROOT = Path(__file__).resolve().parent.parent


def read(path):
    raw = Path(path).read_bytes()
    return json.loads(raw), hashlib.sha256(raw).hexdigest()


def write(path, doc, private=False):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    # Exclusive creation keeps a repeated invocation from changing an existing
    # mapping after its evidence has already been read.
    with path.open('x') as stream:
        if private:
            path.chmod(0o600)
        json.dump(doc, stream, ensure_ascii=False, indent=2)
        stream.write('\n')


def export_facts(args):
    requirements, requirements_hash = read(args.requirements)
    packet = {'kind': 'facts', 'blinded': True, 'protocol': requirements['protocol'], 'queries': []}
    mapping = {'kind': 'facts', 'requirements_sha256': requirements_hash, 'artifacts': {}, 'assignments': []}
    for cohort in ('cloud', 'local'):
        source = getattr(args, cohort)
        if not source:
            continue
        artifact, digest = read(source)
        mapping['artifacts'][cohort] = {'path': source, 'sha256': digest}
        grouped = defaultdict(list)
        for row in artifact['rows']:
            grouped[row['id']].append(row)
        for rid, rows in sorted(grouped.items()):
            if len({r['provider'] for r in rows}) != len(rows):
                raise ValueError(f'{cohort}/{rid}: repeated provider row')
            secrets.SystemRandom().shuffle(rows)
            answers, scores, reasons = {}, {}, {}
            for n, row in enumerate(rows):
                label = chr(ord('A') + n)
                answers[label] = headtohead.normalize(row['results'])
                scores[label] = None
                reasons[label] = ''
                mapping['assignments'].append({'cohort': cohort, 'id': rid, 'label': label, 'provider': row['provider']})
            packet['queries'].append({'cohort': cohort, 'id': rid, 'query': rows[0]['query'],
                'requirement': requirements['requirements'][rid], 'answers': answers,
                'extractable': scores, 'reasons': reasons})
    return packet, mapping


def export_relevance(args):
    old, old_hash = read(args.old)
    new, new_hash = read(args.new)
    if not old.get('queries_sha256') or old['queries_sha256'] != new.get('queries_sha256'):
        raise ValueError('relevance review requires the same corpus')
    def first_rows(artifact):
        out = {}
        for row in artifact['queries']:
            if row.get('set') == 'held' and row.get('expect') == 'answer':
                out.setdefault(row['id'], row)
        return out
    before, after = first_rows(old), first_rows(new)
    if before.keys() != after.keys():
        raise ValueError('relevance review requires the same held cohort')
    groups = defaultdict(list)
    for rid, row in before.items():
        groups[row['shape']].append(rid)
    rng = random.Random(7)
    for ids in groups.values():
        rng.shuffle(ids)
    chosen = []
    while len(chosen) < 50:
        added = False
        for shape in sorted(groups):
            if groups[shape] and len(chosen) < 50:
                chosen.append(groups[shape].pop())
                added = True
        if not added:
            raise ValueError('fewer than 50 held answer rows')
    packet = {'kind': 'relevance', 'blinded': True,
        'protocol': 'Score each of five slots 0/1/2 using bench/rubric.md; missing slots are zero. Explain zeros and ties. Keep the version mapping unread until all scores are saved.', 'queries': []}
    mapping = {'kind': 'relevance', 'queries_sha256': old['queries_sha256'],
        'old_binary_sha256': old.get('binary_sha256'), 'new_binary_sha256': new.get('binary_sha256'),
        'artifacts': {'old': {'path': args.old, 'sha256': old_hash}, 'new': {'path': args.new, 'sha256': new_hash}}, 'assignments': []}
    for rid in chosen:
        versions = [('old', before[rid]), ('new', after[rid])]
        secrets.SystemRandom().shuffle(versions)
        answers = {}
        for label, (version, row) in zip(('A', 'B'), versions):
            if row['urls'] and not row.get('evidence'):
                raise ValueError(f'{rid}: no measured evidence in {version} artifact')
            answers[label] = headtohead.normalize(row.get('evidence', []))
            mapping['assignments'].append({'id': rid, 'label': label, 'version': version})
        packet['queries'].append({'id': rid, 'query': before[rid]['query'], 'answers': answers,
            'scores': {'A': [None]*5, 'B': [None]*5}, 'reason': ''})
    return packet, mapping


def evidence_hash(packet):
    fields = ('cohort', 'id', 'query', 'requirement', 'answers')
    queries = [{k:r[k] for k in fields if k in r} for r in packet['queries']]
    queries.sort(key=lambda r: (r.get('cohort', ''), r['id']))
    return hashlib.sha256(json.dumps({'kind':packet['kind'], 'queries':queries},
        ensure_ascii=False, sort_keys=True).encode()).hexdigest()


def reveal(packet, mapping):
    if packet.get('kind') != mapping.get('kind') or packet.get('blinded') is not True:
        raise ValueError('review and mapping do not match')
    if evidence_hash(packet) != mapping.get('evidence_sha256'):
        raise ValueError('the reviewed evidence or requirements changed after export')
    rows = packet['queries']
    if packet['kind'] == 'facts':
        indexed = {(r['cohort'], r['id']): r for r in rows}
        if len(indexed) != len(rows):
            raise ValueError('duplicate fact review query')
        scores = []
        for assignment in mapping['assignments']:
            row = indexed[(assignment['cohort'], assignment['id'])]
            label = assignment['label']
            score, reason = row['extractable'][label], row['reasons'][label]
            if type(score) is not bool or not reason.strip():
                raise ValueError('complete every fact verdict and reason before revealing')
            scores.append({**{k: assignment[k] for k in ('cohort', 'id', 'provider')},
                'extractable': score, 'reason': reason})
        return {'blinded': True, 'requirements_sha256': mapping['requirements_sha256'],
            **{cohort+'_sha256': source['sha256'] for cohort, source in mapping['artifacts'].items()}, 'rows': scores}
    indexed = {r['id']: r for r in rows}
    if len(indexed) != len(rows) or len(rows) != 50:
        raise ValueError('exactly 50 distinct relevance rows required')
    totals = defaultdict(dict)
    for assignment in mapping['assignments']:
        row, label = indexed[assignment['id']], assignment['label']
        scores = row['scores'][label]
        if len(scores) != 5 or any(type(s) is not int or s not in (0, 1, 2) for s in scores) or not row['reason'].strip():
            raise ValueError('complete all five relevance slots and the reason before revealing')
        if any(s != 0 for s in scores[len(row['answers'][label]):]):
            raise ValueError('missing result slots must score zero')
        totals[row['id']][assignment['version']] = sum(s*w for s, w in zip(scores, (5,4,3,2,1)))
    revealed = []
    for rid, scores in totals.items():
        winner = 'new' if scores['new'] > scores['old'] else 'old' if scores['old'] > scores['new'] else 'tie'
        revealed.append({'id': rid, **scores, 'winner': winner, 'reason': indexed[rid]['reason']})
    return {'blinded': True, 'scores': revealed,
        **{version+'_artifact_sha256': source['sha256'] for version, source in mapping['artifacts'].items()},
        **{k:v for k,v in mapping.items() if k.endswith('_sha256')}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    facts = commands.add_parser('facts')
    facts.add_argument('--cloud', required=True)
    facts.add_argument('--local')
    facts.add_argument('--requirements', default=str(ROOT/'bench/search/live_requirements.json'))
    relevant = commands.add_parser('relevance')
    relevant.add_argument('--old', required=True)
    relevant.add_argument('--new', required=True)
    for command in (facts, relevant):
        command.add_argument('--out', required=True)
        command.add_argument('--mapping-out', required=True)
    unblind = commands.add_parser('reveal')
    unblind.add_argument('--scores', required=True)
    unblind.add_argument('--mapping', required=True)
    unblind.add_argument('--out', required=True)
    args = parser.parse_args()
    if args.command == 'reveal':
        packet, digest = read(args.scores)
        mapping, _ = read(args.mapping)
        output = reveal(packet, mapping)
        output['scored_packet_sha256'] = digest
        write(args.out, output)
    else:
        packet, mapping = export_facts(args) if args.command == 'facts' else export_relevance(args)
        if Path(args.out).exists() or Path(args.mapping_out).exists():
            parser.error('output or mapping already exists; choose new paths')
        mapping['evidence_sha256'] = evidence_hash(packet)
        write(args.mapping_out, mapping, private=True)
        write(args.out, packet)


if __name__ == '__main__':
    main()
