#!/usr/bin/env python3
"""Byte-for-byte baseline/candidate parity over an explicit NDJSON manifest.

A request is sent only after the previous reply. Push frames are saved separately,
never mistaken for replies. Neither results, ordering, ids nor provenance are
normalised. The manifest may include configure/readiness requests before the
measured queries. Use separate, empty cache directories for each capture.
"""
import argparse
from capture_io import load_json, write_bytes
import hashlib
import json
import os
import pathlib
import queue
import sqlite3
import subprocess
import threading
import time

HERE = pathlib.Path(__file__).resolve().parent
SEED = 'callgraph-store-diet-v1'
METADATA_FIELDS = {'search_ms', 'tier2_last_run'}


def dump(path, value):
    write_bytes(path, (json.dumps(value, indent=2, ensure_ascii=False) + '\n').encode('utf-8'))


def sample(values, limit):
    return sorted(set(values), key=lambda value: (hashlib.sha256((SEED + '\0' + value).encode()).digest(), value))[:limit]


def sample_pairs(values):
    ordered = sorted(set(values))
    count = len(ordered)
    if not count:
        return []
    total = count * count
    start = int.from_bytes(hashlib.sha256((SEED + '\0pairs').encode()).digest(), 'little') % total
    # count + 1 is coprime to count squared, so these indexes never repeat.
    indexes = ((start + i * (count + 1)) % total for i in range(min(100, total)))
    return [(ordered[index // count], ordered[index % count]) for index in indexes]


def manifest(symbols):
    """Symbols are path-qualified names, supplied from a finished baseline store."""
    requests = []
    for symbol in sample(symbols, 500):
        path, name = symbol.split('\0', 1)
        for op in ['callers', 'call_tree', 'impact']:
            requests.append(tool_request('callgraph', {'op': op, 'path': path, 'symbol': name, 'includeTests': True}))
        requests.append(tool_request('zoom', {'batch': [{'target': path, 'symbols': name}], 'callgraph': True}))
        requests.append(tool_request('search', {'query': name, 'includeTests': True}))
    for symbol, other_symbol in sample_pairs(symbols):
        path, name = symbol.split('\0', 1)
        other_path, other = other_symbol.split('\0', 1)
        requests.append(tool_request('callgraph', {'op': 'trace_to', 'path': path, 'symbol': name}))
        requests.append(tool_request('callgraph', {'op': 'trace_to_symbol', 'path': path, 'symbol': name, 'toSymbol': other, 'toPath': other_path}))
        requests.append(tool_request('callgraph', {'op': 'trace_data', 'path': path, 'symbol': name, 'expression': 'value'}))
    for section in ['dead_code', 'unused_exports', 'cycles']:
        requests.append(tool_request('inspect', {'sections': [section]}))
    return [dict(request, id=f'parity-{i:05}') for i, request in enumerate(requests)]


def tool_request(name, arguments):
    return {'command': 'tool_call', 'name': name, 'arguments': arguments}


def operation(request):
    if request['command'] == 'tool_call':
        return request['arguments'].get('op', request['name'])
    return request['command']


def pending_reply(request, reply):
    if reply.get('code') == 'callgraph_building':
        return True
    if operation(request) == 'inspect' and reply.get('code') == 'inspect_not_fresh':
        return True
    graph = (reply.get('index') or {}).get('callgraph') or {}
    return graph.get('reason') == 'identity_provisional' or graph.get('status') in {'building', 'refreshing'}


def trust_selected_checkout(env, root):
    # A copied checkout may belong to a different user. Trust that checkout
    # alone, not arbitrary repositories or the operator's global Git settings.
    for key in list(env):
        if key.startswith('GIT_CONFIG_'):
            del env[key]
    root = root.resolve()
    for candidate in [root, *root.parents]:
        if (candidate / '.git').exists():
            path = str(candidate).replace('\\', '/').replace("'", "'\\''")
            env['GIT_CONFIG_PARAMETERS'] = f"'safe.directory'='{path}'"
            break


def prepare_cache(cache, prepared):
    if prepared:
        assert cache.is_dir(), 'prepared capture requires an existing owner/base cache'
    else:
        assert not cache.exists(), f'cold capture requires a fresh cache: {cache}'
        cache.mkdir(parents=True)


def capture(binary, root, cache, requests, output, timeout, prepared=False, overrides=None, require_borrowed=False):
    prepare_cache(cache, prepared)
    output.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ, AFT_CACHE_DIR=str(cache.resolve()), AFT_STORAGE_DIR=str(cache.resolve()),
               XDG_DATA_HOME=str(cache.resolve() / 'data'), XDG_CONFIG_HOME=str(cache.resolve() / 'config'),
               XDG_STATE_HOME=str(cache.resolve() / 'state'), XDG_CACHE_HOME=str(cache.resolve() / 'cache'),
               HOME=str(cache.resolve() / 'home'), USERPROFILE=str(cache.resolve() / 'home'),
               AFT_CALLGRAPH_BUILD_WAIT_MS=str(timeout * 1000), AFT_TEST_DISABLE_FILE_WATCHER='1')
    trust_selected_checkout(env, root)
    frames = queue.Queue()
    successes = {operation(request): 0 for request in requests}
    failures = {}
    borrowed = 0
    with (output / 'stderr.txt').open('wb') as stderr:
        process = subprocess.Popen([str(binary.resolve())], cwd=root,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, env=env)
        def consume():
            for line in process.stdout:
                frames.put(line)
            frames.put(None)
        reader = threading.Thread(target=consume, daemon=True)
        reader.start()
        try:
            with (output / 'push.ndjson').open('wb') as pushes:
                config = {'indexes': {'trigram': True, 'semantic': False, 'callgraph': True}}
                for key, value in (overrides or {}).items():
                    if isinstance(value, dict) and isinstance(config.get(key), dict):
                        config[key].update(value)
                    else:
                        config[key] = value
                configure = {'id': 'configure-parity', 'command': 'configure', 'harness': 'opencode',
                             'project_root': str(root.resolve()), 'storage_dir': str(cache.resolve()),
                             'config': [{'tier': 'user', 'source': str(cache.resolve() / 'user.jsonc'), 'doc': json.dumps(config)}]}
                for request in [configure] + requests:
                    deadline = time.monotonic() + timeout
                    process.stdin.write(json.dumps(request, separators=(',', ':')).encode() + b'\n')
                    process.stdin.flush()
                    while True:
                        line = frames.get(timeout=max(0.001, deadline - time.monotonic()))
                        assert line is not None, f'binary exited before reply to {request["id"]}'
                        value = json.loads(line)
                        if value.get('id') == request['id']:
                            if request is configure:
                                assert value.get('success'), f'configure failed: {line!r}'
                            if request is not configure:
                                if pending_reply(request, value):
                                    assert time.monotonic() < deadline, f'query never became ready: {line!r}'
                                    time.sleep(0.1)
                                    process.stdin.write(json.dumps(request, separators=(',', ':')).encode() + b'\n')
                                    process.stdin.flush()
                                    continue
                                (output / (request['id'] + '.reply')).write_bytes(line)
                                successes[operation(request)] += int(value.get('success') is True)
                                if value.get('success') is not True:
                                    failures[request['id']] = value
                                borrowed += int(bool(value.get('borrowed_callgraph')))
                            break
                        assert value.get('id') is None, f'unexpected reply: {line!r}'
                        pushes.write(line)
            process.stdin.close()
            assert process.wait(timeout=timeout) == 0, 'binary exited unsuccessfully'
            assert all(successes.values()), f'no successful query for a manifest operation: {successes}; replies: {json.dumps(failures)}'
            assert not require_borrowed or borrowed > 0, 'capture never served a borrowed_callgraph reply'
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
    baseline = load_json((HERE / 'baseline.json'))['code_revision']
    dump(output / 'capture.json', {'baseline': baseline, 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                                  'manifest_sha256': hashlib.sha256(json.dumps(requests, sort_keys=True).encode()).hexdigest(),
                                  'normalised_fields': [], 'requests': len(requests), 'successful_queries': successes,
                                  'config_sha256': hashlib.sha256(json.dumps(config, sort_keys=True).encode()).hexdigest(),
                                  'prepared_cache': prepared, 'required_borrowed': require_borrowed})


def normalise_metadata(raw, fields):
    """Replace only whitelisted top-level value spans; retain all other bytes."""
    assert set(fields) <= METADATA_FIELDS, 'rows, counts, notes and provenance cannot be normalised'
    text = raw.decode('utf-8')
    decoder = json.JSONDecoder()
    cursor = len(text) - len(text.lstrip())
    assert text[cursor] == '{'
    cursor += 1
    replacements = []
    while True:
        while text[cursor].isspace():
            cursor += 1
        if text[cursor] == '}':
            break
        key, cursor = decoder.raw_decode(text, cursor)
        while text[cursor].isspace():
            cursor += 1
        assert text[cursor] == ':'
        cursor += 1
        while text[cursor].isspace():
            cursor += 1
        start = cursor
        value, cursor = decoder.raw_decode(text, cursor)
        if key in fields:
            assert isinstance(value, (float, int)) and not isinstance(value, bool), f'{key}: metadata type changed'
            replacements.append((start, cursor))
        while text[cursor].isspace():
            cursor += 1
        if text[cursor] == ',':
            cursor += 1
        else:
            assert text[cursor] == '}'
            break
    for start, end in reversed(replacements):
        text = text[:start] + '0' + text[end:]
    return text.encode('utf-8')


def compare(left, right, fields=()):
    names = {p.name for p in left.glob('*.reply')}
    assert names, 'empty captures are not parity evidence'
    assert names == {p.name for p in right.glob('*.reply')}, 'reply membership differs'
    for name in sorted(names):
        before, after = (left / name).read_bytes(), (right / name).read_bytes()
        if fields:
            before, after = normalise_metadata(before, fields), normalise_metadata(after, fields)
        assert before == after, f'reply bytes differ: {name}'
    lmeta = load_json((left / 'capture.json'))
    rmeta = load_json((right / 'capture.json'))
    assert lmeta['manifest_sha256'] == rmeta['manifest_sha256'], 'different query manifests'
    for field in ['views', 'config_sha256', 'prepared_cache', 'required_borrowed']:
        assert lmeta.get(field) == rmeta.get(field), f'different capture policy: {field}'
    print(f'{len(names)} raw-byte replies match; normalised metadata: {list(fields)}')


def prove_normalisation(left, right, output):
    lmeta, rmeta = (load_json((root / 'capture.json')) for root in [left, right])
    assert lmeta['binary_sha256'] == rmeta['binary_sha256'], 'normalisation proof requires the same baseline binary'
    fields = set()
    for path in left.glob('*.reply'):
        before, after = json.loads(path.read_bytes()), json.loads((right / path.name).read_bytes())
        for field in METADATA_FIELDS:
            if field in before and field in after and before[field] != after[field]:
                fields.add(field)
    compare(left, right, fields)
    dump(output, {'normalised_fields': sorted(fields), 'baseline_binary_sha256': lmeta['binary_sha256'],
                  'manifest_sha256': lmeta['manifest_sha256'], 'proof_captures': [str(left), str(right)]})


def row_counts(database):
    """Read a finished database through SQLite, never a raw database descriptor."""
    tables = load_json((HERE / 'table_encoding.json'))
    connection = sqlite3.connect(database.resolve().as_uri() + '?mode=ro', uri=True)
    try:
        present = {row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        result = {table['table']: connection.execute(f'SELECT count(*) FROM {table["table"]}').fetchone()[0]
                  if table['table'] in present else None for table in tables if table['graph_data']}
        result['refs_unresolved'] = connection.execute("SELECT count(*) FROM refs WHERE status='unresolved'").fetchone()[0]
        return result
    finally:
        connection.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    make = sub.add_parser('manifest')
    make.add_argument('symbols', type=pathlib.Path, help='JSON array of path + NUL + symbol strings')
    make.add_argument('output', type=pathlib.Path)
    fixture = sub.add_parser('fixture-manifest')
    fixture.add_argument('seeded', type=pathlib.Path)
    fixture.add_argument('output', type=pathlib.Path)
    run = sub.add_parser('capture')
    for arg in ['binary', 'root', 'cache', 'manifest', 'output']:
        run.add_argument(arg, type=pathlib.Path)
    run.add_argument('--timeout', type=int, default=300)
    run.add_argument('--prepared-cache', action='store_true')
    run.add_argument('--require-borrowed', action='store_true')
    run.add_argument('--config', type=pathlib.Path, help='current user config overrides')
    diff = sub.add_parser('compare')
    diff.add_argument('baseline', type=pathlib.Path)
    diff.add_argument('candidate', type=pathlib.Path)
    diff.add_argument('--normalisation', type=pathlib.Path)
    proof = sub.add_parser('prove-normalisation')
    proof.add_argument('left', type=pathlib.Path)
    proof.add_argument('right', type=pathlib.Path)
    proof.add_argument('output', type=pathlib.Path)
    counts = sub.add_parser('counts')
    counts.add_argument('database', type=pathlib.Path)
    counts.add_argument('output', type=pathlib.Path)
    args = parser.parse_args()
    if args.command == 'manifest':
        dump(args.output, manifest(load_json(args.symbols)))
    elif args.command == 'fixture-manifest':
        symbols = [row['file'] + '\0' + row['symbol'] for row in load_json(args.seeded) if 'to_file' not in row]
        dump(args.output, manifest(symbols))
    elif args.command == 'capture':
        overrides = load_json(args.config) if args.config else None
        assert overrides is None or isinstance(overrides, dict), '--config must contain an object'
        capture(args.binary, args.root, args.cache, load_json(args.manifest), args.output, args.timeout, args.prepared_cache, overrides, args.require_borrowed)
    elif args.command == 'compare':
        fields = []
        if args.normalisation:
            proof = load_json(args.normalisation)
            metadata = load_json((args.baseline / 'capture.json'))
            assert proof['baseline_binary_sha256'] == metadata['binary_sha256'] and proof['manifest_sha256'] == metadata['manifest_sha256'], 'normalisation proof is for another baseline/manifest'
            fields = proof['normalised_fields']
        compare(args.baseline, args.candidate, fields)
    elif args.command == 'prove-normalisation':
        prove_normalisation(args.left, args.right, args.output)
    else:
        dump(args.output, row_counts(args.database))


if __name__ == '__main__':
    main()
