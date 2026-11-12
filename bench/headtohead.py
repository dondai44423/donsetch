#!/usr/bin/env python3
"""Same queries, evidence limits and scoring for local search and provider APIs.

Artifacts contain results and measurements, never credentials. Failed requests
remain failures in the denominator. This is a corpus-specific comparison, not
an assertion that its accuracy equals a separately published benchmark.
"""
import argparse
import contextlib
from email.utils import parsedate_to_datetime
import hashlib
import html
import json
import os
import platform
import shutil
import socket
from pathlib import Path
import random
import re
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
try:
    import tiktoken
except ImportError:
    tiktoken = None


def evidence_tokens(results):
    if tiktoken is None:
        return None
    text = '\n'.join(f"{r['title']} {r['url']}\n{r['snippet']}" for r in results)
    return len(tiktoken.get_encoding('cl100k_base').encode(text, disallowed_special=()))

ROOT = Path(__file__).resolve().parent.parent


def native_overrides():
    controls = {'DONSETCH_CACHE_DIR', 'DONSETCH_CONFIG', 'DONSETCH_NO_CONFIG_FILE',
                'DONSETCH_NO_UPDATE_CHECK', 'DONSETCH_NO_SPONSOR'}
    return sorted(name for name in os.environ if name not in controls
        and name.startswith(('DONSETCH_', 'DONSEEK_')))


@contextlib.contextmanager
def refusal_lock(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('a+b') as lock:
        if os.name == 'nt':
            import msvcrt
            if lock.seek(0, 2) == 0:
                lock.write(b'0')
                lock.flush()
        else:
            import fcntl
        until = time.monotonic() + .5
        while True:
            try:
                if os.name == 'nt':
                    lock.seek(0)
                    msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
                else:
                    fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except OSError:
                if time.monotonic() >= until:
                    raise TimeoutError('Cannot lock source refusal journal') from None
                time.sleep(.002)
        try:
            yield
        finally:
            if os.name == 'nt':
                lock.seek(0)
                msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(lock.fileno(), fcntl.LOCK_UN)


def carry_refusals(source, target):
    """Carry declared waits only; a cold run never inherits answers or trust."""
    def read(path):
        if not path.exists():
            return {'version': 1, 'lanes': {}}
        data = json.loads(path.read_text())
        if data.get('version') != 1:
            raise ValueError('Unknown refusal ledger version')
        return data
    with refusal_lock(target.with_suffix('.lock')):
        data, dest = read(source), read(target)
        now = int(time.time())
        for name, lane in data['lanes'].items():
            if lane.get('rest_cause') not in (None, 'legacy', 'refusal', 'retry-after') or lane.get('rest_until', 0) <= now:
                continue
            entry = dest['lanes'].setdefault(name, {'tokens': 2, 'refilled': now, 'fails': 0, 'last_ok': 0})
            if lane['rest_until'] > entry.get('rest_until', 0):
                entry.update(rest_until=lane['rest_until'], rest_secs=lane.get('rest_secs', 0),
                             rest_cause=lane.get('rest_cause', 'legacy'))
        dest['lanes'] = {name: lane for name, lane in dest['lanes'].items()
            if not name.startswith('origin:') or lane.get('rest_until', 0) > now}
        temp = target.with_suffix(f'.{os.getpid()}.tmp')
        # Atomic replacement keeps readers from seeing a truncated ledger.
        with os.fdopen(os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600), 'w') as out:
            json.dump(dest, out)
        os.replace(temp, target)


def request_json(url, headers=None, body=None):
    request = urllib.request.Request(url, headers=headers or {},
        data=None if body is None else json.dumps(body).encode())
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def refusal_until(error):
    """A provider refusal stops later rows from issuing more requests."""
    now = time.time()
    retry = error.headers.get('Retry-After') if error.headers else None
    wait = None
    if retry:
        try:
            wait = max(0, int(retry))
        except ValueError:
            try:
                wait = max(0, parsedate_to_datetime(retry).timestamp() - now)
            except (TypeError, ValueError, OverflowError):
                pass
    if error.code in (401, 403, 429) or (error.code == 503 and wait is not None):
        return now + (wait if wait is not None else (900 if error.code == 429 else 45))
    return 0


def key(args, provider):
    path = getattr(args, provider + '_key_file', None)
    value = Path(path).read_text().strip() if path else os.environ.get(provider.upper() + '_API_KEY')
    if not value:
        raise ValueError(f'{provider}: supply --{provider}-key-file or {provider.upper()}_API_KEY')
    return value


def normalize(results):
    rows = []
    for i, result in enumerate(results[:5]):
        snippet = result.get('snippet') or result.get('content') or result.get('text')
        snippet = snippet or ' '.join(result.get('highlights') or [])
        rows.append({'title': html.unescape(result.get('title') or '')[:240],
            'url': result.get('url') or '', 'snippet': html.unescape(snippet)[:320 if i < 3 else 180]})
    return rows


def search(provider, q, args, cache):
    if provider == 'donsetch':
        env = dict(os.environ, DONSETCH_CACHE_DIR=str(cache), DONSETCH_NO_CONFIG_FILE='1', DONSETCH_NO_UPDATE_CHECK='1')
        env.pop('DONSETCH_CONFIG', None)
        proc = subprocess.run([args.bin, 'search', '--json', '--max-results', '5', '--', q],
            env=env, capture_output=True, text=True, timeout=35)
        if proc.returncode:
            raise RuntimeError(f'donsetch exited {proc.returncode}')
        doc = json.loads(proc.stdout)
        meta = doc.get('meta', {})
        # Current CLI keeps debug fields in meta. Fail closed on schema drift.
        if 'results' not in meta:
            raise ValueError('donsetch response has no meta.results')
        if meta.get('provider') is not None:
            raise ValueError('Native comparison unexpectedly used a configured provider')
        return normalize(meta['results'])
    if provider == 'exa':
        doc = request_json('https://api.exa.ai/search',
            {'x-api-key': key(args, provider), 'Content-Type': 'application/json'},
            {'query': q, 'numResults': 5, 'type': args.exa_type,
             'contents': {'highlights': {'maxCharacters': 320}}})
    elif provider == 'tinyfish':
        doc = request_json('https://api.search.tinyfish.ai/?' + urllib.parse.urlencode(
            {'query': q, 'num': 5, 'domain_type': 'web'}), {'X-API-Key': key(args, provider)})
    else:
        if not args.endpoint:
            raise ValueError(f'{provider}: --endpoint required')
        route, params = {
            'searxng': ('/search', {'q': q, 'format': 'json'}),
            '4get': ('/api/v1/web', {'s': q}),
            'librey': ('/api.php', {'q': q, 'p': 0, 't': 0}),
        }[provider]
        doc = request_json(args.endpoint.rstrip('/') + route + '?' + urllib.parse.urlencode(params))
        if provider == '4get':
            if doc.get('status') != 'ok':
                raise ValueError('4get did not answer')
            return normalize(doc.get('web', []))
        if provider == 'librey':
            return normalize(doc if isinstance(doc, list) else doc.get('results', []))
    return normalize(doc.get('results', []))


def judge(q, results):
    def rank(patterns):
        return next((i + 1 for i, r in enumerate(results)
            if any(re.search(p, r['url']) for p in patterns)), None)
    key_at = next((i + 1 for i, r in enumerate(results)
        if q.get('answer_key') and all(' '.join(phrase.lower().split()) in ' '.join((r['title'] + ' ' + r['snippet']).lower().split())
               for phrase in q.get('answer_key', []))), None)
    junk = sum(any(re.search(p, r['title'] + ' ' + r['url'])
        for p in q.get('must_not', [])) for r in results)
    return {'hit': rank(q.get('gold', [])), 'key_at': key_at, 'junk5': junk}


def percentile(values, fraction):
    return sorted(values)[min(int(len(values) * fraction), len(values)-1)] if values else 0


def summarize(rows):
    n = len(rows)
    return {'n': n, 'key_coverage': sum(bool(r['key_at']) for r in rows)/n,
        'ar1': sum(r['hit'] == 1 for r in rows)/n,
        'ar5': sum(bool(r['hit']) for r in rows)/n,
        'zero': sum(not r['results'] for r in rows)/n,
        'junk5': sum(r['junk5'] > 0 for r in rows)/n,
        'p50_ms': percentile([r['ms'] for r in rows], .5),
        'p95_ms': percentile([r['ms'] for r in rows], .95),
        'errors': sum('error' in r for r in rows),
        'max_evidence_tokens': max((r.get('evidence_tokens') or 0 for r in rows), default=0) if all(r.get('evidence_tokens') is not None for r in rows) else None}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--include', nargs='+', default=['donsetch', 'exa', 'tinyfish'],
        choices=['donsetch', 'exa', 'tinyfish', 'searxng', '4get', 'librey'])
    parser.add_argument('--queries', default=str(ROOT/'bench/search/live.json'))
    parser.add_argument('--bin', default=str(ROOT/'target/dist-thin/donsetch'))
    parser.add_argument('--exa-type', default='fast', choices=['auto', 'fast', 'instant'])
    parser.add_argument('--exa-key-file')
    parser.add_argument('--tinyfish-key-file')
    parser.add_argument('--endpoint')
    parser.add_argument('--availability-file', help='same-hour public JSON endpoint preflight for the local comparison')
    parser.add_argument('--pace-ms', type=int, default=2600)
    parser.add_argument('--limit', type=int)
    parser.add_argument('--out', required=True)
    args = parser.parse_args(argv)
    if args.pace_ms < 2500:
        parser.error('--pace-ms must be at least 2500')
    if 'donsetch' in args.include and native_overrides():
        parser.error('Native comparison requires default settings; unset: ' + ', '.join(native_overrides()))
    # Credentials checked before spending any requests.
    for provider in args.include:
        if provider in ('exa', 'tinyfish'):
            key(args, provider)
    corpus_bytes = Path(args.queries).read_bytes()
    queries = json.loads(corpus_bytes)['queries']
    if args.limit:
        queries = queries[:args.limit]
    artifact = {'schema': 1, 'started_unix': int(time.time()),
        'queries_sha256': hashlib.sha256(corpus_bytes).hexdigest(), 'exa_type': args.exa_type,
        'endpoint': args.endpoint, 'pace_ms': args.pace_ms,
        'machine': {'hostname': socket.gethostname(), 'system': platform.system(), 'machine': platform.machine()},
        'evidence_limits': {'results': 5, 'title_chars': 240, 'snippet_chars': [320,320,320,180,180]},
        'metric': 'phrase-key coverage; answer extractability requires a separate fact-key audit',
        'latency': 'service/CLI wall time including parsing and normalization; excludes offline scoring and tokenization',
        'native_state': 'fresh at run start; shared within run to preserve source refusals',
        'native_file_config': 'disabled', 'native_env_overrides': native_overrides(),
        'tokenizer': 'cl100k_base' if tiktoken else None, 'rows': [], 'summary': {}}
    path = Path(args.out)
    path.parent.mkdir(parents=True, exist_ok=True)
    if args.availability_file:
        artifact['availability'] = json.loads(Path(args.availability_file).read_text())
    refusals = path.parent/'search-refusals.json'
    artifact['refusal_policy'] = 'preserve-active-source-refusals-v1'
    provider_rests = {}
    with tempfile.TemporaryDirectory(prefix='donsetch-h2h-') as temp:
        if 'donsetch' in args.include:
            source = Path(args.bin).resolve()
            frozen = Path(temp)/('measured.exe' if os.name == 'nt' else 'measured')
            shutil.copy2(source, frozen)
            artifact['binary'] = {'path': str(source), 'sha256': hashlib.sha256(frozen.read_bytes()).hexdigest()}
            args.bin = str(frozen)
            carry_refusals(refusals, Path(temp)/'native/search-ledger.json')
        for n, q in enumerate(queries):
            # Pair providers in a seeded rotating order to balance time drift.
            providers = list(args.include)
            random.Random(n).shuffle(providers)
            for provider in providers:
                start = time.monotonic()
                row = {'id': q['id'], 'query': q['query'], 'shape': q['shape'], 'provider': provider}
                try:
                    cache = Path(temp)/'native'
                    cache.mkdir(exist_ok=True)
                    if provider_rests.get(provider, 0) > time.time():
                        results = []
                        row['error'] = 'rested after provider refusal'
                    else:
                        results = search(provider, q['query'], args, cache)
                except urllib.error.HTTPError as error:
                    results = []
                    row['error'] = f'HTTP {error.code}'
                    until = refusal_until(error)
                    error.close()
                    if until:
                        provider_rests[provider] = max(provider_rests.get(provider, 0), until)
                        row['rest_until_unix'] = provider_rests[provider]
                except Exception as error:
                    results = []
                    # Never log a request, body, headers or exception text with credentials.
                    row['error'] = type(error).__name__
                row['ms'] = round(1000*(time.monotonic()-start))
                if provider == 'donsetch':
                    carry_refusals(cache/'search-ledger.json', refusals)
                row['evidence_tokens'] = evidence_tokens(results)
                row.update(results=results, **judge(q, results))
                artifact['rows'].append(row)
            for provider in args.include:
                artifact['summary'][provider] = summarize([r for r in artifact['rows'] if r['provider'] == provider])
            path.write_text(json.dumps(artifact, ensure_ascii=False, indent=2)+'\n')
            print(f"{n+1}/{len(queries)} {q['id']}", flush=True)
            time.sleep(args.pace_ms/1000)
    artifact['finished_unix'] = int(time.time())
    path.write_text(json.dumps(artifact, ensure_ascii=False, indent=2)+'\n')
    print(json.dumps(artifact['summary'], indent=2))
    return int(any(s['errors'] for s in artifact['summary'].values()))


if __name__ == '__main__':
    raise SystemExit(main())
