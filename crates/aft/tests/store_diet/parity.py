#!/usr/bin/env python3
"""Byte-for-byte baseline/candidate parity over an explicit NDJSON manifest.

A request is sent only after the previous reply. Push frames are saved separately,
never mistaken for replies. Neither results, ordering, ids nor provenance are
normalised. The manifest may include configure/readiness requests before the
measured queries. Use separate, empty cache directories for each capture.
"""
import argparse
import hashlib
import json
import os
import pathlib
import queue
import sqlite3
import subprocess
import threading

HERE = pathlib.Path(__file__).resolve().parent
BASELINE = '998c896b05753dd3a83158e30f97779b89ef28e2'
SEED = 'callgraph-store-diet-v1'


def dump(path, value):
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + '\n')


def sample(values, limit):
    return sorted(set(values), key=lambda value: (hashlib.sha256((SEED + '\0' + value).encode()).digest(), value))[:limit]


def manifest(symbols):
    """Symbols are path-qualified names, supplied from a finished baseline store."""
    requests = []
    for symbol in sample(symbols, 500):
        path, name = symbol.split('\0', 1)
        for op in ['callers', 'call_tree', 'impact']:
            requests.append({'command': op, 'file': path, 'symbol': name, 'include_tests': True})
        requests.append({'command': 'zoom', 'file': path, 'symbol': name, 'callgraph': True})
        requests.append({'command': 'semantic_search', 'query': name, 'include_tests': True})
    chosen = sample(symbols, 100)
    for i, symbol in enumerate(chosen):
        path, name = symbol.split('\0', 1)
        other_path, other = chosen[(i + 1) % len(chosen)].split('\0', 1)
        requests.append({'command': 'trace_to', 'file': path, 'symbol': name})
        requests.append({'command': 'trace_to_symbol', 'file': path, 'symbol': name, 'toSymbol': other, 'toFile': other_path})
        requests.append({'command': 'trace_data', 'file': path, 'symbol': name, 'expression': 'value'})
    for section in ['dead_code', 'unused_exports', 'cycles']:
        requests.append({'command': 'inspect', 'sections': [section]})
    return [dict(request, id=f'parity-{i:05}') for i, request in enumerate(requests)]


def capture(binary, root, cache, requests, output, timeout, views=False):
    assert not cache.exists(), f'cold capture requires a fresh cache: {cache}'
    cache.mkdir(parents=True)
    output.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ, AFT_CACHE_DIR=str(cache.resolve()), AFT_STORAGE_DIR=str(cache.resolve()),
               XDG_DATA_HOME=str(cache.resolve() / 'data'), XDG_CONFIG_HOME=str(cache.resolve() / 'config'),
               XDG_STATE_HOME=str(cache.resolve() / 'state'), XDG_CACHE_HOME=str(cache.resolve() / 'cache'),
               HOME=str(cache.resolve() / 'home'), USERPROFILE=str(cache.resolve() / 'home'),
               AFT_CALLGRAPH_BUILD_WAIT_MS=str(timeout * 1000), AFT_TEST_DISABLE_FILE_WATCHER='1')
    frames = queue.Queue()
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
                config = {'indexes': {'trigram': True, 'semantic': False, 'callgraph': True},
                          'views': {'enabled': views}, 'worktree': {'ram_overlay': False}}
                configure = {'id': 'configure-parity', 'command': 'configure', 'harness': 'opencode',
                             'project_root': str(root.resolve()), 'storage_dir': str(cache.resolve()),
                             'config': [{'tier': 'user', 'source': str(cache.resolve() / 'user.jsonc'), 'doc': json.dumps(config)}]}
                for request in [configure] + requests:
                    process.stdin.write(json.dumps(request, separators=(',', ':')).encode() + b'\n')
                    process.stdin.flush()
                    while True:
                        line = frames.get(timeout=timeout)
                        assert line is not None, f'binary exited before reply to {request["id"]}'
                        value = json.loads(line)
                        if value.get('id') == request['id']:
                            if request is configure:
                                assert value.get('success'), f'configure failed: {line!r}'
                            if request is not configure:
                                (output / (request['id'] + '.reply')).write_bytes(line)
                            break
                        assert value.get('id') is None, f'unexpected reply: {line!r}'
                        pushes.write(line)
            process.stdin.close()
            assert process.wait(timeout=timeout) == 0, 'binary exited unsuccessfully'
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
    dump(output / 'capture.json', {'baseline': BASELINE, 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                                  'manifest_sha256': hashlib.sha256(json.dumps(requests, sort_keys=True).encode()).hexdigest(),
                                  'normalised_fields': [], 'requests': len(requests), 'views': views})


def compare(left, right):
    names = {p.name for p in left.glob('*.reply')}
    assert names, 'empty captures are not parity evidence'
    assert names == {p.name for p in right.glob('*.reply')}, 'reply membership differs'
    for name in sorted(names):
        assert (left / name).read_bytes() == (right / name).read_bytes(), f'reply bytes differ: {name}'
    lmeta = json.loads((left / 'capture.json').read_text())
    rmeta = json.loads((right / 'capture.json').read_text())
    assert lmeta['manifest_sha256'] == rmeta['manifest_sha256'], 'different query manifests'
    print(f'{len(names)} raw-byte replies match; no normalisation')


def row_counts(database):
    """Read a finished database through SQLite, never a raw database descriptor."""
    tables = json.loads((HERE / 'table_encoding.json').read_text())
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
    run.add_argument('--views', action='store_true')
    diff = sub.add_parser('compare')
    diff.add_argument('baseline', type=pathlib.Path)
    diff.add_argument('candidate', type=pathlib.Path)
    counts = sub.add_parser('counts')
    counts.add_argument('database', type=pathlib.Path)
    counts.add_argument('output', type=pathlib.Path)
    args = parser.parse_args()
    if args.command == 'manifest':
        dump(args.output, manifest(json.loads(args.symbols.read_text())))
    elif args.command == 'fixture-manifest':
        symbols = [row['file'] + '\0' + row['symbol'] for row in json.loads(args.seeded.read_text()) if 'to_file' not in row]
        dump(args.output, manifest(symbols))
    elif args.command == 'capture':
        capture(args.binary, args.root, args.cache, json.loads(args.manifest.read_text()), args.output, args.timeout, args.views)
    elif args.command == 'compare':
        compare(args.baseline, args.candidate)
    else:
        dump(args.output, row_counts(args.database))


if __name__ == '__main__':
    main()
