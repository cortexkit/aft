#!/usr/bin/env python3
import json
import pathlib
import tempfile
import unittest
from unittest import mock

import inventory
import parity


class InventoryTests(unittest.TestCase):
    def test_rust_literals_ignore_comments_lifetimes_and_characters(self):
        source = '''// "SELECT ghost"
fn f<'a>(x: &'a str) { let c = 'x'; let q = "SELECT 1"; }
/* "SELECT ghost" */ let q = r##"SELECT 'quoted' FROM refs"##;
let q = "SELECT \\\n  ref_id FROM refs";'''
        self.assertEqual([s for _, s in inventory.strings(source)],
                         ['SELECT 1', "SELECT 'quoted' FROM refs", 'SELECT ref_id FROM refs'])

    def test_sql_census_ignores_agent_prose(self):
        for text in ['with 2 files changed', 'create symlink', 'drop the cache', 'update failed']:
            self.assertIsNone(inventory.SQL.match(text))
        for text in ['WITH x AS (SELECT 1) SELECT * FROM x', 'UPDATE refs SET line=1',
                     'CREATE UNIQUE INDEX x ON refs(line)', 'SELECT count(*) FROM refs']:
            self.assertIsNotNone(inventory.SQL.match(text))

    def test_plan_guard_rejects_search_to_scan_and_lost_equalities(self):
        baseline = {'q': ['SEARCH refs USING INDEX r (caller_file=? AND kind=?)']}
        for plan in [['SCAN refs'], ['SEARCH refs USING INDEX r (caller_file=?)'],
                     ['SEARCH refs USING INDEX r (caller_file=? AND kind=?)', 'SCAN strings']]:
            with self.assertRaises(AssertionError):
                inventory.compare_plans(baseline, {'q': plan})
        inventory.compare_plans(baseline, baseline)
        inventory.compare_plans(baseline, {'q': baseline['q'] + ['SEARCH strings USING INTEGER PRIMARY KEY (rowid=?)']})

    def test_plan_guard_does_not_silently_accept_unavailable_plan(self):
        with self.assertRaises(AssertionError):
            inventory.compare_plans({'q': []}, {'q': {'error': 'no such table'}})

    def test_trace_coverage_preserves_fixed_literals_and_requires_reasons(self):
        scratch = inventory.ROOT / 'target/scratch'
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temporary:
            root = pathlib.Path(temporary)
            trace = root / 'trace.jsonl'
            trace.write_text(json.dumps("SELECT * FROM refs WHERE status='resolved' AND caller_file='main.ts'") + '\n')
            rows = [{'id': state, 'variants': [{'variant': 'one', 'sql': f"SELECT * FROM refs WHERE status='{state}' AND caller_file=?1"}]} for state in ['resolved', 'unresolved']]
            exemptions = root / 'trace_exemptions.json'
            exemptions.write_text('{}')
            with mock.patch.object(inventory, 'HERE', root):
                with self.assertRaisesRegex(SystemExit, 'unresolved:one'):
                    inventory.coverage(rows, [trace])
                exemptions.write_text(json.dumps({'unresolved:one': 'This manifest has no unresolved-reference query.'}))
                inventory.coverage(rows, [trace])
                exemptions.write_text(json.dumps({'unresolved:one': ''}))
                with self.assertRaisesRegex(AssertionError, 'empty trace exemption'):
                    inventory.coverage(rows, [trace])

    def test_sampling_is_seeded_bounded_and_input_order_independent(self):
        symbols = [f'file{i}.rs\0f{i}' for i in range(600)]
        self.assertEqual(parity.manifest(symbols), parity.manifest(list(reversed(symbols))))
        requests = parity.manifest(symbols)
        self.assertEqual(sum(r['command'] == 'callers' for r in requests), 500)
        self.assertEqual(sum(r['command'] == 'trace_to_symbol' for r in requests), 100)
        self.assertEqual(len({r['id'] for r in requests}), len(requests))

    def test_metadata_normalisation_preserves_nested_rows_and_wire_bytes(self):
        raw = b' { "search_ms": 1.25, "rows": [{"search_ms": 9}], "notes": ["unchanged"] }\n'
        expected = b' { "search_ms": 0, "rows": [{"search_ms": 9}], "notes": ["unchanged"] }\n'
        self.assertEqual(parity.normalise_metadata(raw, ['search_ms']), expected)
        with self.assertRaisesRegex(AssertionError, 'cannot be normalised'):
            parity.normalise_metadata(raw, ['rows'])
        with self.assertRaisesRegex(AssertionError, 'metadata type changed'):
            parity.normalise_metadata(b'{"search_ms": []}', ['search_ms'])

    def test_parity_rejects_changed_reply_bytes_and_empty_capture(self):
        # Keep scratch beneath the checkout.
        scratch = inventory.ROOT / 'target/scratch'
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temporary:
            left, right = pathlib.Path(temporary) / 'left', pathlib.Path(temporary) / 'right'
            left.mkdir()
            right.mkdir()
            with self.assertRaises(AssertionError):
                parity.compare(left, right)
            for root in [left, right]:
                (root / 'capture.json').write_text(json.dumps({'manifest_sha256': 'same'}))
                (root / 'one.reply').write_bytes(b'{"rows":[1,2]}\n')
            parity.compare(left, right)
            (right / 'one.reply').write_bytes(b'{"rows":[2,1]}\n')
            with self.assertRaises(AssertionError):
                parity.compare(left, right)


if __name__ == '__main__':
    unittest.main()
