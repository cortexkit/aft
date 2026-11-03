#!/usr/bin/env python3
import json
import hashlib
import pathlib
import sqlite3
import gzip
import io
import os
import subprocess
import sys
import zlib
import tempfile
import unittest
from contextlib import closing
from unittest import mock

import inventory
import capture_io
import corpus
import parity
import record as recorder


def checkout_fixture_bytes(directory, files):
    """Export fixture blobs through Git with Windows-style checkout settings."""
    directory = pathlib.Path(directory).resolve()
    repository, checkout = directory / 'repository', directory / 'checkout'
    repository.mkdir()
    checkout.mkdir()
    empty_config = directory / 'empty-config'
    empty_config.write_bytes(b'')
    (repository / '.gitattributes').write_bytes((inventory.ROOT / '.gitattributes').read_bytes())
    for name, data in files.items():
        path = repository / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    env = {key: value for key, value in os.environ.items()
           if not key.startswith('GIT_CONFIG_') and key not in {'GIT_DIR', 'GIT_COMMON_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE'}}
    env.update(GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=str(empty_config), GIT_ATTR_NOSYSTEM='1')

    def git(*args):
        result = subprocess.run(['git', '-c', 'core.attributesFile=' + str(empty_config), *args],
                                cwd=repository, env=env, capture_output=True)
        assert result.returncode == 0, result.stderr.decode('utf-8', errors='replace')

    git('init', '--quiet')
    git('-c', 'core.autocrlf=false', 'add', '--all')
    git('-c', 'core.autocrlf=true', '-c', 'core.eol=crlf', 'checkout-index', '--all', '--force',
        '--prefix=' + checkout.resolve().as_posix() + '/')
    return {name: (checkout / name).read_bytes() for name in files}


def single_gzip_payload(data):
    assert data[:8] == b'\x1f\x8b\x08\x00\x00\x00\x00\x00', 'gzip header must have zero mtime and no optional fields'
    decoder = zlib.decompressobj(16 + zlib.MAX_WBITS)
    payload = decoder.decompress(data)
    assert decoder.eof, 'truncated gzip capture'
    assert not decoder.unused_data, 'capture must contain exactly one gzip member and no trailing data'
    return payload + decoder.flush()


class InventoryTests(unittest.TestCase):
    def test_stored_gzip_validation_rejects_metadata_extra_members_and_corruption(self):
        payload = b' {"rows": [1, 2]} \n'
        valid = gzip.compress(payload, compresslevel=9, mtime=0)
        self.assertEqual(single_gzip_payload(valid), payload)
        for index, value in [(3, 4), (3, 8), (3, 16), (4, 1)]:
            with self.subTest(header_index=index, value=value):
                changed = bytearray(valid)
                changed[index] = value
                with self.assertRaisesRegex(AssertionError, 'gzip header'):
                    single_gzip_payload(changed)
        for extra in [valid, b'junk']:
            with self.subTest(extra=extra):
                with self.assertRaisesRegex(AssertionError, 'exactly one gzip member'):
                    single_gzip_payload(valid + extra)
        with self.assertRaisesRegex(AssertionError, 'truncated'):
            single_gzip_payload(valid[:-1])
        corrupt = bytearray(valid)
        corrupt[-8] ^= 1
        with self.assertRaises(zlib.error):
            single_gzip_payload(corrupt)

    def test_recorder_clone_does_not_inherit_windows_line_endings(self):
        tree = pathlib.Path('baseline')
        with mock.patch.object(recorder, 'run') as run:
            recorder.clone_baseline('a' * 40, tree)
        clone = run.call_args_list[0].args[0]
        self.assertEqual(clone[:5], ['git', 'clone', '--quiet', '--shared', '--no-checkout'])
        self.assertIn('core.autocrlf=false', clone)
        self.assertIn('core.eol=lf', clone)
        self.assertEqual(run.call_args_list[1].args[0], ['git', 'checkout', '--quiet', '--detach', 'a' * 40])

    def test_git_checkout_preserves_byte_exact_parity_inputs(self):
        root = 'crates/aft/tests/store_diet/'
        files = {root + name: b'{\n  "fixture": true\n}\n' for name in [
            'legacy-replies.json', 'views-replies.json', 'lifecycle-values.json',
            'sqlite-order.json', 'dispatch-order.json', 'stored-shape-values.json',
        ]}
        files.update({
            root + 'corpus/ruby-factory/greeter.rb': b'module Demo\nend\n',
            root + 'corpus/tsx-components/sample.tsx': b'function F() {\n return <div/>;\n}\n',
            root + 'wire-legacy/wire-callers.reply': b'{"success":true}\n',
            root + 'baseline-trace.patch': b'--- a/file\n+++ b/file\n',
            root + 'corpus-views.json.gz': gzip.compress(b'one\ntwo\n', compresslevel=0, mtime=0),
            'crates/aft/tests/fixtures/callgraph/helpers.ts': b'function f() {}\n',
            'crates/aft/tests/fixtures/sample.tsx': b'function F() {\n return <div/>;\n}\n',
            'crates/aft/tests/fixtures/imports_ts.ts': b'import { value } from "./module";\n',
        })
        with tempfile.TemporaryDirectory() as directory:
            actual = checkout_fixture_bytes(directory, files)
        for name, expected in files.items():
            with self.subTest(path=name):
                self.assertEqual(actual[name], expected)

    def test_capture_gzip_is_deterministic_and_compares_decoded_bytes(self):
        payload = b' {"rows": [1, 2]} \n' + b' ' * capture_io.GZIP_THRESHOLD
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            first = capture_io.write_bytes(root / 'first.json', payload)
            second = capture_io.write_bytes(root / 'second.json', payload)
            self.assertEqual(first.suffix, '.gz')
            self.assertFalse((root / 'first.json').exists())
            self.assertEqual(first.read_bytes(), second.read_bytes())
            self.assertEqual(first.read_bytes()[4:8], b'\0\0\0\0')
            self.assertEqual(first.read_bytes()[3] & 8, 0)
            self.assertEqual(capture_io.read_bytes(root / 'first.json'), payload)
            subprocess.run([sys.executable, capture_io.__file__, 'write', str(root / 'cli.json')], input=payload, check=True)
            self.assertEqual((root / 'cli.json.gz').read_bytes(), first.read_bytes())
            alternative = io.BytesIO()
            with gzip.GzipFile(filename='different-name', fileobj=alternative, mode='wb', mtime=123, compresslevel=1) as output:
                output.write(payload)
            second.write_bytes(alternative.getvalue())
            self.assertNotEqual(first.read_bytes(), second.read_bytes())
            self.assertEqual(capture_io.read_bytes(first), capture_io.read_bytes(second))
            capture_io.write_bytes(second, payload.replace(b'[1, 2]', b'[2, 1]'))
            self.assertNotEqual(capture_io.read_bytes(first), capture_io.read_bytes(second))

    def test_capture_threshold_and_reviewed_plain_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            logical = root / 'capture.json'
            self.assertEqual(capture_io.write_bytes(logical, b'x' * capture_io.GZIP_THRESHOLD), logical)
            self.assertEqual(capture_io.write_bytes(logical, b'x' * (capture_io.GZIP_THRESHOLD + 1)), root / 'capture.json.gz')
            self.assertFalse(logical.exists())
            self.assertEqual(capture_io.write_bytes(logical, b'small'), logical)
            self.assertFalse((root / 'capture.json.gz').exists())
            for name in [*capture_io.PLAIN_JSON, 'legacy-counts.json', 'custom-policy.json', 'baseline-harness.patch', 'baseline-trace.patch']:
                with self.subTest(name=name):
                    path = root / name
                    self.assertEqual(capture_io.write_bytes(path, b'x' * (capture_io.GZIP_THRESHOLD + 1)), path)
            (root / 'capture.json.gz').write_bytes(gzip.compress(b'second copy'))
            with self.assertRaisesRegex(ValueError, 'ambiguous capture'):
                capture_io.read_bytes(logical)
            with self.assertRaisesRegex(ValueError, 'ambiguous capture'):
                capture_io.read_bytes(root / 'capture.json.gz')

    def test_recorder_publishes_compressed_outputs_without_plain_duplicates(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            source, destination = root / 'source', root / 'destination'
            payload = b' ' * (capture_io.GZIP_THRESHOLD + 1)
            capture_io.write_bytes(source / 'statements.json', payload)
            capture_io.write_bytes(source / 'baseline.json', b'{}\n')
            capture_io.write_bytes(source / 'baseline-trace.patch', b'plain patch\n')
            destination.mkdir()
            (destination / 'statements.json').write_bytes(b'obsolete')
            with mock.patch.object(recorder, 'OUTPUTS', ['statements.json', 'baseline.json', 'baseline-trace.patch']):
                outputs = recorder.copy_outputs(source, destination)
            self.assertEqual([path.name for path in outputs], ['statements.json.gz', 'baseline.json', 'baseline-trace.patch'])
            self.assertEqual(capture_io.read_bytes(destination / 'statements.json'), payload)
            self.assertFalse((destination / 'statements.json').exists())
            self.assertEqual((source / 'statements.json.gz').read_bytes(), (destination / 'statements.json.gz').read_bytes())

    def test_committed_capture_headers_payloads_and_hashes_are_valid(self):
        recording = capture_io.load_json(inventory.HERE / 'recording.json')
        for name, digest in recording['artifact_sha256'].items():
            path = inventory.HERE / name
            self.assertEqual(hashlib.sha256(path.read_bytes()).hexdigest(), digest, name)
        compressed = sorted(inventory.HERE.glob('*.json.gz'))
        self.assertTrue(compressed, 'large committed captures must use gzip')
        # Python/zlib upgrades may encode identical payloads into different valid
        # DEFLATE streams. Pin the committed bytes by SHA-256, validate their
        # headers/framing, and compare decoded bytes instead of recompressing.
        # Per-runtime writer determinism is exercised separately above.
        for path in compressed:
            with self.subTest(path=path.name):
                decoded = single_gzip_payload(path.read_bytes())
                self.assertEqual(decoded, capture_io.read_bytes(path))
                self.assertGreater(len(decoded), capture_io.GZIP_THRESHOLD)
                self.assertFalse(path.with_suffix('').exists())
        for path in inventory.HERE.glob('*.json'):
            if path.stat().st_size > capture_io.GZIP_THRESHOLD:
                self.assertTrue(path.name in capture_io.PLAIN_JSON or path.name.endswith(('-counts.json', '-policy.json')), path.name)

    def test_dispatch_composition_uses_the_local_select_not_an_earlier_delete(self):
        path = inventory.ROOT / 'crates/aft/src/views/materialization/dispatch.rs'
        source = '''fn read() {
let columns = "SELECT id FROM edges";
let sql = format!("{columns} WHERE ref_id=?1");
let old = "DELETE FROM edges WHERE ref_id=?1";
let columns = "SELECT caller_file FROM view_dispatch_sites";
let sql = format!("{columns} WHERE caller_file=?1");
}'''
        with mock.patch.object(pathlib.Path, 'rglob', return_value=[path]), mock.patch.object(pathlib.Path, 'read_text', return_value=source):
            sql = {row['candidate'] for row in inventory.census()}
        self.assertIn('SELECT id FROM edges WHERE ref_id=?1', sql)
        self.assertIn('SELECT caller_file FROM view_dispatch_sites WHERE caller_file=?1', sql)
        self.assertNotIn('DELETE FROM edges WHERE ref_id=?1 WHERE caller_file=?1', sql)

    def test_external_cfg_test_modules_are_not_production_sql(self):
        parent = inventory.ROOT / 'crates/aft/src/views/materialization.rs'
        child = parent.parent / 'materialization/parity.rs'
        sources = {parent: '#[cfg(test)] mod parity;', child: 'fn snapshot() { let sql = "SELECT 1"; }'}
        with mock.patch.object(pathlib.Path, 'rglob', return_value=list(sources)), mock.patch.object(pathlib.Path, 'read_text', new=lambda path, **kwargs: sources[path]):
            rows = inventory.census()
        self.assertTrue(rows[0]['sites'][0]['test_only'])

    def test_composed_dispatch_and_spill_sql_variants_keep_runtime_text(self):
        row = {'candidate': 'SELECT r.ref_id FROM refs r WHERE r.kind=\'call\'', 'sites': [{'owner': 'load_name_match_refs'}]}
        sql = {variant['variant']: variant['sql'] for variant in inventory.variants(row, [])}
        self.assertEqual(sql['caller-file/0'], "SELECT r.ref_id FROM refs r WHERE r.kind='call' AND r.caller_file = ?1 ORDER BY r.caller_file, r.byte_start, r.ref_id")
        row = {'candidate': 'SELECT revision FROM {PROJECTION_DELTA_SPILL_TABLE}'}
        self.assertEqual(inventory.variants(row, [])[0]['sql'], 'SELECT revision FROM projection_delta_spill')

    def test_order_inventory_uses_executable_variants_not_unordered_prefixes(self):
        row = {'id': 'stable', 'baseline': 'SELECT r.ref_id FROM refs r',
               'candidate': 'SELECT r.ref_id FROM refs r',
               'sites': [{'path': 'crates/aft/src/callgraph_store/mod.rs', 'owner': 'load_name_match_refs'}]}
        orders = inventory.order_inventory([row])
        self.assertEqual([entry['variant'] for entry in orders], ['all/0', 'caller-file/0'])
        self.assertTrue(all(entry['statement'] == 'stable' for entry in orders))
        self.assertTrue(all(entry['keys'] == ['r.caller_file, r.byte_start, r.ref_id'] for entry in orders))
        self.assertTrue(all(not entry['implicit_order'] for entry in orders))

    def test_stale_plan_variants_cannot_hide_changed_candidate_sql(self):
        row = {'candidate': 'SELECT 1', 'variants': [{'variant': 'literal/0', 'sql': 'SELECT 2'}]}
        with self.assertRaisesRegex(AssertionError, 'variants are stale'):
            inventory.checked_variants(row, [])
        row['variants'] = [{'variant': 'literal/0', 'sql': 'SELECT 1'}]
        self.assertEqual(inventory.checked_variants(row, []), [{'variant': 'literal/0', 'sql': 'SELECT 1'}])

    def test_view_token_exemptions_do_not_claim_a_legacy_registry_connection(self):
        row = {'id': 'sql-token', 'sites': [{'path': 'crates/aft/src/views/registry.rs', 'owner': 'mint_view_token', 'line': 1, 'test_only': False}]}
        missing = [{'id': 'sql-token:literal/0', 'sql': 'SELECT token FROM view_tokens'}]
        reason = inventory.draft_exemptions(missing, [row])['sql-token:literal/0']
        self.assertIn('v3 view-token', reason)
        self.assertNotIn('Legacy SQLite', reason)

    def test_recorder_uses_short_cache_prefix_for_sqlite_paths(self):
        root = pathlib.Path('/' + 'w' * 177)
        work = root / 'target/scratch' / ('store-diet-record-' + 'a' * 40)
        caches = root / 'target/scratch' / ('diet-cache-' + 'a' * 12)
        with mock.patch.object(recorder, 'run') as run:
            recorder.capture(root / 'aft', root, root / 'tools', work,
                             'wire-views-first', True, cache_root=caches)
        cache = run.call_args.args[0][5]
        self.assertEqual(cache, caches / 'wire-views-first-cache')
        # A representative derived-file suffix consumes 246 bytes.
        self.assertLess(len(str(cache)) + 246, 512)
        self.assertGreaterEqual(len(str(work / cache.name)) + 246, 512)

    def test_created_file_patterns_expand_to_executable_queries(self):
        row = {'candidate': 'SELECT caller_file FROM refs WHERE ({js}) OR ({rust})'}
        variants = inventory.variants(row, [])
        self.assertEqual(len(variants), 18)
        query = next(v['sql'] for v in variants if v['variant'] == 'js-2-rust-1-crate-1/0')
        self.assertEqual(query, 'SELECT caller_file FROM refs WHERE (module_path GLOB ?1 OR module_path GLOB ?2) OR (module_path GLOB ?3 OR caller_file GLOB ?4)')
        with closing(sqlite3.connect(':memory:')) as connection:
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
        with mock.patch.dict('os.environ', {'AFT_STORE_DIET_RECORD': '1', 'AFT_STORE_DIET_BASELINE': 'wrong', 'AFT_STORE_DIET_SQL_TRACE': 'old.log', 'AFT_STORE_DIET_PYTHON': 'wrong-python'}):
            env = recorder.clean_environment()
        self.assertNotIn('AFT_STORE_DIET_RECORD', env)
        self.assertNotIn('AFT_STORE_DIET_BASELINE', env)
        self.assertNotIn('AFT_STORE_DIET_SQL_TRACE', env)
        self.assertNotIn('AFT_STORE_DIET_PYTHON', env)

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
        self.assertTrue(parity.pending_reply(request, {'success': False, 'code': 'callgraph_building'}))
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
