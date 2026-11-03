#!/usr/bin/env python3
"""Storage encoding for generated captures; comparisons use decoded bytes."""
import argparse
import gzip
import io
import json
import pathlib
import sys

GZIP_THRESHOLD = 64 * 1024
PLAIN_JSON = {
    'baseline.json', 'required_values.json', 'recording.json',
    'corpus_sources.json', 'wire-policy.json',
}


def capture_path(path):
    path = pathlib.Path(path)
    if path.suffix == '.gz':
        if path.exists() and path.with_suffix('').exists():
            raise ValueError(f'ambiguous capture: both {path.with_suffix("")} and {path} exist')
        return path
    compressed = path.with_name(path.name + '.gz')
    if path.exists() and compressed.exists():
        raise ValueError(f'ambiguous capture: both {path} and {compressed} exist')
    return compressed if compressed.exists() else path


def read_bytes(path):
    path = capture_path(path)
    data = path.read_bytes()
    return gzip.decompress(data) if path.suffix == '.gz' else data


def read_text(path):
    return read_bytes(path).decode('utf-8')


def load_json(path):
    return json.loads(read_text(path))


def write_bytes(path, data):
    path = pathlib.Path(path)
    logical = path.with_suffix('') if path.suffix == '.gz' else path
    keep_plain = logical.name in PLAIN_JSON or logical.name.endswith(('-counts.json', '-policy.json')) or logical.suffix == '.patch'
    compress = not keep_plain and (path.suffix == '.gz' or len(data) > GZIP_THRESHOLD)
    destination = logical.with_name(logical.name + '.gz') if compress else logical
    destination.parent.mkdir(parents=True, exist_ok=True)
    if compress:
        buffer = io.BytesIO()
        # Explicitly omit the original filename and wall-clock time from the header.
        with gzip.GzipFile(filename='', fileobj=buffer, mode='wb', compresslevel=9, mtime=0) as stream:
            stream.write(data)
        destination.write_bytes(buffer.getvalue())
    else:
        destination.write_bytes(data)
    alternative = logical if compress else logical.with_name(logical.name + '.gz')
    if alternative.exists():
        alternative.unlink()
    return destination


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['write'])
    parser.add_argument('path', type=pathlib.Path)
    args = parser.parse_args()
    write_bytes(args.path, sys.stdin.buffer.read())


if __name__ == '__main__':
    main()
