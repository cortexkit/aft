#!/usr/bin/env python3
import json
import hashlib
import pathlib
import sqlite3
import tempfile
import unittest
from unittest import mock

import inventory
import corpus
import parity
import record as recorder


class InventoryTests(unittest.TestCase):
    def test_created_file_patterns_expand_to_executable_queries(self):
        row = {'candidate': 'SELECT caller_file FROM refs WHERE ({js}) OR ({rust})'}
        variants = inventory.variants(row, [])
        self.assertEqual(len(variants), 18)
        query = next(v['sql'] for v in variants if v['variant'] == 'js-2-rust-1-crate-1/0')
        self.assertEqual(query, 'SELECT caller_file FROM refs WHERE (module_path GLOB ?1 OR module_path GLOB ?2) OR (module_path GLOB ?3 OR caller_file GLOB ?4)')
        with sqlite3.connect(':memory:') as connection:
            connection.execute('CREATE TABLE refs(caller_file TEXT, module_path TEXT)')
            for variant in variants:
                parameters = variant['sql'].count('?')
                connection.execute('EXPLAIN QUERY PLAN ' + variant['sql'], ['*'] * parameters)

    def test_corpus_tables_accept_inline_and_helper_returned_arrays(self):
        examples = [
            'fn fixtures() { for (path, language, source) in [("a.rs", "rust", "fn f() {} [x]")] {} }',
            'fn fixtures() -> [(&str, &str, &str); 1] { [("a.rs", "rust", "fn f() {} [x]")] }',
        ]
        for text in examples:
            with self.subTest(text=text), mock.patch.object(pathlib.Path, 'read_text', return_value=text):
                self.assertEqual(corpus.function_table('fixtures.rs', 'fixtures'),
                                 ['a.rs', 'rust', 'fn f() {} [x]'])

    def test_recorder_does_not_inherit_blush_or_trace_switches(self):
        with mock.patch.dict('os.environ', {'AFT_STORE_DIET_RECORD': '1', 'AFT_STORE_DIET_BASELINE': 'wrong', 'AFT_STORE_DIET_SQL_TRACE': 'old.log'}):
            env = recorder.clean_environment()
        self.assertNotIn('AFT_STORE_DIET_RECORD', env)
        self.assertNotIn('AFT_STORE_DIET_BASELINE', env)
        self.assertNotIn('AFT_STORE_DIET_SQL_TRACE', env)

    def test_rust_literals_ignore_comments_lifetimes_and_characters(self):
        source = '''// "SELECT ghost"
fn f<'a>(x: &'a str) { let c = 'x'; let q = "SELECT 1"; }
/* "SELECT ghost" */ let q = r##"SELECT 'quoted' FROM refs"##;
let q = "SELECT \\\n  ref_id FROM refs";'''
        self.assertEqual([s for _, s in inventory.strings(source)],
                         ['SELECT 1', "SELECT 'quoted' FROM refs", 'SELECT ref_id FROM refs'])

    def test_sql_owner_metadata_distinguishes_test_modules_and_live_functions(self):
        text = '#[cfg(test)] mod tests { fn setup() { let s = "SELECT { ignored }"; } } fn live() { let s = "SELECT real"; }'
        scopes = inventory.rust_scopes(text)
        path = pathlib.Path('src/mod.rs')
        self.assertEqual(inventory.owner_at(scopes, text.index('SELECT {'), path), {'owner': 'setup', 'test_only': True})
        self.assertEqual(inventory.owner_at(scopes, text.index('SELECT real'), path), {'owner': 'live', 'test_only': False})

    def test_array_signature_semicolons_are_not_declaration_terminators(self):
        text = 'fn declaration(); fn fixtures() -> [[u8; 2]; 1] { [[1, 2]] }'
        self.assertEqual([s[3] for s in inventory.rust_scopes(text)], ['fixtures'])

    def test_cooked_sql_strings_keep_escaped_backslashes_and_decode_rust_unicode(self):
        decoded = inventory.cooked_string(r"SELECT '\\n', '\u{e9}', '\x41'")
        self.assertEqual(decoded, "SELECT '\\n', 'é', 'A'")
        self.assertNotIn('\n', decoded)

    def test_sql_census_ignores_agent_prose(self):
        for text in ['with 2 files changed', 'create symlink', 'drop the cache', 'update failed']:
            self.assertIsNone(inventory.SQL.match(text))
        for text in ['WITH x AS (SELECT 1) SELECT * FROM x', 'UPDATE refs SET line=1',
                     'CREATE UNIQUE INDEX x ON refs(line)', 'SELECT count(*) FROM refs']:
            self.assertIsNotNone(inventory.SQL.match(text))

    def test_candidate_sql_keeps_its_baseline_statement_identity(self):
        baseline = 'SELECT id FROM nodes'
        candidate = baseline + ' ORDER BY id'
        key = 'sql-' + hashlib.sha256(baseline.encode()).hexdigest()[:20]
        expected = [{'id': key, 'baseline': baseline, 'candidate': candidate, 'sites': []}]
        actual = [{'id': 'different-candidate-hash', 'baseline': candidate, 'sites': [{'path': 'reader.rs', 'line': 1}]}]
        mapped = inventory.mapped_census(expected, actual)
        self.assertEqual(mapped[0]['id'], key)
        self.assertEqual(mapped[0]['baseline'], candidate)
        with self.assertRaisesRegex(AssertionError, 'baseline identity changed'):
            inventory.mapped_census([dict(expected[0], baseline='SELECT name FROM nodes')], actual)

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
                exemptions.write_text(json.dumps({'unresolved:one': 'Not queried.', 'resolved:one': 'Already traced.'}))
                with self.assertRaisesRegex(AssertionError, 'obsolete exemptions'):
                    inventory.coverage(rows, [trace])
                exemptions.write_text(json.dumps({'unresolved:one': ''}))
                with self.assertRaisesRegex(AssertionError, 'empty trace exemption'):
                    inventory.coverage(rows, [trace])

    def test_exemption_drafts_keep_owner_and_do_not_misclassify_json_literals(self):
        rows = [{'id': 'q', 'sites': [{'path': 'crates/aft/src/db/state.rs', 'line': 8, 'owner': 'save', 'test_only': False}]}]
        draft = inventory.draft_exemptions([{'id': 'q:literal/0', 'sql': "INSERT INTO state VALUES('{}')"}], rows)
        self.assertIn('Separate state/ledger', draft['q:literal/0'])
        self.assertIn('state.rs:8 (save)', draft['q:literal/0'])
        rows[0]['sites'][0]['test_only'] = True
        draft = inventory.draft_exemptions([{'id': 'q:literal/0', 'sql': 'SELECT 1'}], rows)
        self.assertTrue(draft['q:literal/0'].startswith('Test-only'))

    def test_sampling_is_seeded_bounded_and_input_order_independent(self):
        symbols = [f'file{i}.rs\0f{i}' for i in range(600)]
        self.assertEqual(parity.manifest(symbols), parity.manifest(list(reversed(symbols))))
        requests = parity.manifest(symbols)
        self.assertEqual(sum(parity.operation(r) == 'callers' for r in requests), 500)
        self.assertEqual(sum(parity.operation(r) == 'trace_to_symbol' for r in requests), 100)
        self.assertEqual(len({r['id'] for r in requests}), len(requests))

    def test_small_corpus_samples_pairs_not_only_neighbouring_symbols(self):
        self.assertEqual(set(parity.sample_pairs(['a', 'b'])), {('a', 'a'), ('a', 'b'), ('b', 'a'), ('b', 'b')})
        self.assertEqual(parity.sample_pairs(['a', 'b']), parity.sample_pairs(['b', 'a']))
        self.assertEqual(parity.sample_pairs([]), [])

    def test_metadata_normalisation_preserves_nested_rows_and_wire_bytes(self):
        raw = b' { "search_ms": 1.25, "rows": [{"search_ms": 9}], "notes": ["unchanged"] }\n'
        expected = b' { "search_ms": 0, "rows": [{"search_ms": 9}], "notes": ["unchanged"] }\n'
        self.assertEqual(parity.normalise_metadata(raw, ['search_ms']), expected)
        with self.assertRaisesRegex(AssertionError, 'cannot be normalised'):
            parity.normalise_metadata(raw, ['rows'])
        with self.assertRaisesRegex(AssertionError, 'metadata type changed'):
            parity.normalise_metadata(b'{"search_ms": []}', ['search_ms'])

    def test_transient_graph_admission_is_not_recorded_as_a_query_answer(self):
        request = {'command': 'callers'}
        self.assertTrue(parity.pending_reply(request, {'index': {'callgraph': {'status': 'unavailable', 'reason': 'identity_provisional'}}}))
        self.assertTrue(parity.pending_reply(request, {'index': {'callgraph': {'status': 'building'}}}))
        self.assertFalse(parity.pending_reply(request, {'success': True, 'complete': False, 'unresolved_call_sites': 1}))
        self.assertFalse(parity.pending_reply(request, {'success': False, 'code': 'symbol_not_found'}))
        self.assertTrue(parity.pending_reply({'command': 'inspect'}, {'success': False, 'code': 'inspect_not_fresh'}))
        self.assertFalse(parity.pending_reply({'command': 'inspect'}, {'success': True, 'inspect_terminal': 'partial', 'partial_reason': 'unknown diagnostics'}))

    def test_capture_trusts_only_its_checkout_and_discards_injected_git_config(self):
        scratch = inventory.ROOT / 'target/scratch'
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temporary:
            root = pathlib.Path(temporary)
            (root / '.git').mkdir()
            nested = root / 'fixtures'
            nested.mkdir()
            env = {'GIT_CONFIG_COUNT': '1', 'GIT_CONFIG_KEY_0': 'core.hooksPath', 'GIT_CONFIG_VALUE_0': 'outside'}
            parity.trust_selected_checkout(env, nested)
            trusted = root.resolve().as_posix().replace("'", "'\\''")
            self.assertEqual(env, {'GIT_CONFIG_PARAMETERS': f"'safe.directory'='{trusted}'"})

    def test_prepared_borrower_state_requires_an_explicit_cache_mode(self):
        scratch = inventory.ROOT / 'target/scratch'
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temporary:
            cache = pathlib.Path(temporary) / 'cache'
            with self.assertRaisesRegex(AssertionError, 'existing owner/base'):
                parity.prepare_cache(cache, True)
            parity.prepare_cache(cache, False)
            marker = cache / 'owner-state'
            marker.write_text('retained')
            with self.assertRaisesRegex(AssertionError, 'fresh cache'):
                parity.prepare_cache(cache, False)
            parity.prepare_cache(cache, True)
            self.assertEqual(marker.read_text(), 'retained')

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
