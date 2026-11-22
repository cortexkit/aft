#!/usr/bin/env python3
"""Compare two native Cargo tuple ledgers using the recorded test moves."""

import argparse
from collections import Counter
import gzip
import hashlib
from pathlib import Path

PLATFORMS = ('linux', 'macos', 'windows')
MAPPING = Path(__file__).resolve().parent


def rows(path, width):
    opening = gzip.open if path.suffix == '.gz' else Path.open
    with opening(path, 'rt', encoding='utf-8') as stream:
        for number, line in enumerate(stream, 1):
            if line.startswith('#'):
                continue
            row = tuple(line.rstrip('\n').split('\t'))
            if len(row) != width or not all(row):
                raise ValueError(f'{path}:{number}: expected {width} nonempty columns')
            yield row


def count(row, start, index):
    values = tuple(map(int, row[start:]))
    if min(values) < 0 or not any(values):
        raise ValueError(f'invalid platform multiplicities: {row}')
    return values[index]


def reconcile(before, after, platform):
    index = PLATFORMS.index(platform)
    original = Counter(rows(before, 3))
    actual = Counter(rows(after, 3))
    moved_old = {}
    moved_new = Counter()
    for row in rows(MAPPING / 'moves.tsv', 9):
        old, new = row[:3], row[3:6]
        if old == new or old in moved_old:
            raise ValueError(f'not a unique changed source identity: {old}')
        if new in moved_new:
            raise ValueError(f'not a unique destination identity: {new}')
        n = count(row, 6, index)
        if original[old] != n:
            raise ValueError(f'expected {n} baseline occurrences of {old}, found {original[old]}')
        moved_old[old] = n
        moved_new[new] += n

    unchanged = original.copy()
    unchanged.subtract(moved_old)
    expected = (+unchanged) + moved_new
    additions = {}
    for row in rows(MAPPING / 'additions.tsv', 6):
        identity = row[:3]
        if identity in additions or identity in original or identity in moved_new:
            raise ValueError(f'not a unique addition: {identity}')
        additions[identity] = count(row, 3, index)
    expected += Counter(additions)

    missing, extra = expected - actual, actual - expected
    if missing or extra:
        raise ValueError(f'{sum(missing.values())} missing, {sum(extra.values())} extra\n'
                         f'missing: {sorted(missing.items())[:40]}\n'
                         f'extra: {sorted(extra.items())[:40]}')
    digest = hashlib.sha256(''.join('\t'.join(row) + '\n' for row in sorted(actual.elements())).encode())
    print(f'{platform}: {sum(original.values())} baseline, {sum(moved_new.values())} moved, '
          f'{sum(additions.values())} added, {sum(actual.values())} current, 0 missing, 0 extra; '
          f'sha256={digest.hexdigest()}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('platform', choices=PLATFORMS)
    parser.add_argument('baseline', type=Path)
    parser.add_argument('current', type=Path)
    args = parser.parse_args()
    try:
        reconcile(args.baseline, args.current, args.platform)
    except ValueError as err:
        parser.exit(1, f'{err}\n')
