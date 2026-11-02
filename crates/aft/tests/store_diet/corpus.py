#!/usr/bin/env python3
"""Copy declared baseline inputs into isolated parity projects; never execute them."""
import argparse
import hashlib
import json
import pathlib
import re

from inventory import HERE, ROOT, TOKEN, dump, owner_at, rust_scopes, strings

EXTENSIONS = dict(typescript='ts', tsx='tsx', javascript='js', python='py', rust='rs', go='go',
                  c='c', cpp='cpp', cuda='cu', metal='metal', zig='zig', csharp='cs', bash='sh',
                  html='html', markdown='md', solidity='sol', scss='scss', vue='vue', json='json',
                  scala='scala', java='java', ruby='rb', kotlin='kt', swift='swift', php='php',
                  lua='lua', perl='pl', yaml='yaml', pascal='pas', r='r', groovy='groovy', objc='m', toml='toml')


def function_table(source, function):
    text = (ROOT / source).read_text(encoding='utf-8')
    scope = next(s for s in rust_scopes(text) if s[2] == 'fn' and s[3] == function)
    masked = TOKEN.sub(lambda m: ''.join('\n' if c == '\n' else ' ' for c in m[0]), text)
    match = re.search(r'\bin\s*\[', masked[scope[0]:scope[1]])
    assert match, f'{source}:{function}: no literal table'
    opening = scope[0] + match.end() - 1
    depth = 1
    end = opening + 1
    while depth:
        if masked[end] == '[':
            depth += 1
        elif masked[end] == ']':
            depth -= 1
        end += 1
    return [value for _, value in strings(text[opening + 1:end - 1])]


def selected_bytes(selection):
    if 'text' in selection:
        assert selection.get('new_for'), 'new corpus inputs require a stored-value/order obligation'
        return selection['text'].encode('utf-8')
    source = ROOT / selection['source']
    if 'contains' not in selection and 'literal_sha256' not in selection:
        return source.read_bytes()
    text = source.read_text(encoding='utf-8')
    scopes = rust_scopes(text)
    found = [value for pos, value in strings(text)
             if ('contains' not in selection or selection['contains'] in value) and
             ('literal_sha256' not in selection or hashlib.sha256(value.encode('utf-8')).hexdigest() == selection['literal_sha256']) and
             ('owner' not in selection or owner_at(scopes, pos, source)['owner'] == selection['owner'])]
    assert found and len(set(found)) == 1, f'ambiguous/missing source literal: {selection}'
    value = found[0].format(**selection['format_args']) if 'format_args' in selection else found[0]
    return value.encode('utf-8')


def cases():
    config = json.loads((HERE / 'corpus_sources.json').read_text(encoding='utf-8'))
    result = []
    smoke = config['smoke_table']
    entries = function_table(smoke['source'], smoke['owner'])
    assert len(entries) % 3 == 0
    for i in range(0, len(entries), 3):
        path, language, body = entries[i:i + 3]
        result.append({'name': 'smoke-' + language, 'language': language,
                       'shapes': ['existing scoped-node identity smoke'],
                       'files': [{'target': path, 'source': smoke['source'], 'owner': smoke['owner'], 'literal_sha256': hashlib.sha256(body.encode('utf-8')).hexdigest()}]})
    receiver = config['receiver_table']
    entries = function_table(receiver['source'], receiver['owner'])
    assert len(entries) % 2 == 0
    for i in range(0, len(entries), 2):
        language, body = entries[i:i + 2]
        result.append({'name': 'receiver-' + language, 'language': language,
                       'shapes': ['typed receiver', 'method call', 'constructor/factory receiver where present', 'dispatch hint'],
                       'files': [{'target': 'receiver.' + EXTENSIONS[language], 'source': receiver['source'], 'owner': receiver['owner'], 'literal_sha256': hashlib.sha256(body.encode('utf-8')).hexdigest()}]})
    result.extend(config['cases'])
    assert {case['language'] for case in result} == set(EXTENSIONS), 'language coverage changed'
    assert len({case['name'] for case in result}) == len(result)
    return sorted(result, key=lambda case: case['name'])


def generate(check):
    manifest = []
    for case in cases():
        entry = dict(case, files=[])
        for selection in case['files']:
            data = selected_bytes(selection)
            path = HERE / 'corpus' / case['name'] / selection['target']
            assert path.resolve().is_relative_to((HERE / 'corpus').resolve())
            if check:
                assert path.read_bytes() == data, f'corpus input differs: {path}'
            else:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
            origin = {key: value for key, value in selection.items() if key != 'contains'}
            origin['sha256'] = hashlib.sha256(data).hexdigest()
            entry['files'].append(origin)
        manifest.append(entry)
    path = HERE / 'corpus_manifest.json'
    if check:
        assert json.loads(path.read_text(encoding='utf-8')) == manifest, 'corpus manifest is stale'
    else:
        dump(path, manifest)
    print(f'{len(manifest)} projects, {len(EXTENSIONS)} language labels, {sum(len(c["files"]) for c in manifest)} declared inputs')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['generate', 'check'])
    args = parser.parse_args()
    generate(args.command == 'check')


if __name__ == '__main__':
    main()
