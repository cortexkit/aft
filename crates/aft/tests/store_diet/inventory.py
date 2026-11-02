#!/usr/bin/env python3
"""Reproducible SQL/source census. No third-party Python packages are required.

Run from the repository root. `capture` writes the baseline census; `check`
refuses unreviewed SQL additions/removals. SQL strings in tests are included:
otherwise a plan or trace fixture could silently disappear from the census.
"""
import argparse
import hashlib
import json
import pathlib
import re
import sqlite3

ROOT = pathlib.Path(__file__).resolve().parents[4]
HERE = pathlib.Path(__file__).resolve().parent
# Consume comments and character literals as well as strings, so commented-out
# queries and SQL-looking text in comments never become executable statements.
TOKEN = re.compile(r'//[^\n]*|/\*.*?\*/|r(?P<hash>\#*)"(?P<raw>.*?)"(?P=hash)|"(?P<cooked>(?:\\.|[^"\\])*)"|\'(?:\\.|[^\'\\\n])\'', re.S)
SQL = re.compile(r'^\s*(?:SELECT\s|INSERT\s+(?:OR\s+\w+\s+)?INTO\s|UPDATE\s+(?:OR\s+\w+\s+)?[\w{][\w{}.:]*\s+SET\s|DELETE\s+FROM\s|(?:CREATE|DROP|ALTER)\s+(?:UNIQUE\s+|TEMP\s+|VIRTUAL\s+)?(?:TABLE|INDEX|TRIGGER|VIEW)\s|PRAGMA\s|WITH\s+(?:RECURSIVE\s+)?\w+\s*(?:\([^)]*\)\s*)?AS\s*\(|REPLACE\s+INTO\s|EXPLAIN\s|ATTACH\s+DATABASE\s|DETACH\s+DATABASE\s|BEGIN(?:\s+(?:IMMEDIATE|TRANSACTION|EXCLUSIVE)|;|$)|COMMIT(?:;|$)|ROLLBACK(?:;|$)|ANALYZE(?:;|$)|VACUUM(?:;|$)|REINDEX\s)', re.I)


def strings(text):
    for match in TOKEN.finditer(text):
        value = match.group('raw')
        if value is None and match.group('cooked') is not None:
            value = match.group('cooked')
            value = re.sub(r'\\\r?\n\s*', '', value)
            value = value.replace('\\n', '\n').replace('\\t', '\t').replace('\\r', '\r').replace('\\"', '"').replace('\\\\', '\\')
        if value is not None:
            yield match.start(), value


def census():
    found = {}
    for path in sorted((ROOT / 'crates/aft/src').rglob('*.rs')):
        text = path.read_text()
        for pos, sql in strings(text):
            if not SQL.match(re.sub(r'^\s*(?:--[^\n]*\n|/\*.*?\*/)\s*', '', sql, flags=re.S)):
                continue
            # Keep whitespace inside SQL string literals significant. The text
            # itself is the stable identity; source lines are only citations.
            key = hashlib.sha256(sql.encode()).hexdigest()[:20]
            item = found.setdefault(key, {'id': 'sql-' + key, 'baseline': sql, 'candidate': sql, 'sites': []})
            item['sites'].append({'path': str(path.relative_to(ROOT)), 'line': text.count('\n', 0, pos) + 1})
    return sorted(found.values(), key=lambda row: row['id'])


def dump(path, value):
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + '\n')


def graph_statement(row):
    return any('/callgraph_store/' in s['path'] or '/views/' in s['path'] for s in row['sites'])


VOCAB = {'kind', 'status', 'import_kind', 'provenance', 'lang', 'language', 'backend'}
STRINGS = {'path', 'file_path', 'dep_file', 'caller_file', 'target_file', 'file', 'name', 'scoped_name',
           'signature', 'short_name', 'callee_last_segment', 'full_ref', 'module_path', 'local_name',
           'requested_name', 'namespace_alias', 'method_name', 'workspace_root', 'caller_symbol',
           'source_file', 'symbol', 'target_symbol', 'target', 'source', 'target_path', 'source_path', 'declared', 'dependency', 'input'}
IDENTITIES = {'id', 'ref_id', 'edge_id', 'caller_node', 'source_node', 'target_node'}
SHARED = {'files', 'nodes', 'refs', 'edges', 'dispatch_hints', 'file_dependencies', 'type_ref_names',
          'resolution_config_fields', 'backend_file_state', 'meta', 'staging_file_inventory', 'staging_ref_context'}
VIEW_INFRA = {'blob_payloads', 'blob_receipts', 'derived_outputs', 'evidence_meta', 'family_free_usage',
              'family_usage', 'members', 'pointer', 'proof_meta', 'readers', 'registry_meta', 'stat_evidence',
              'sweep_lease', 'sweep_schedule', 'usage_record', 'emitted_view_rows', 'changed_view_paths'}


def statements(batch):
    start = 0
    for i, char in enumerate(batch):
        if char == ';' and sqlite3.complete_statement(batch[start:i + 1]):
            yield batch[start:i + 1]
            start = i + 1
    if batch[start:].strip():
        yield batch[start:]


def schemas(rows):
    result = {}
    for row in rows:
        for sql in statements(row['baseline']):
            sql = re.sub(r'--[^\n]*', '', sql).strip()
            match = re.match(r'CREATE (?:TEMP )?TABLE (?:IF NOT EXISTS )?(\w+)\s*\(', sql, re.I)
            if not match or '{' in sql:
                continue
            table = match[1]
            if table in SHARED and not any(site['path'].endswith('/callgraph_store/mod.rs') for site in row['sites']):
                continue
            connection = sqlite3.connect(':memory:')
            try:
                connection.execute(sql)
                columns = list(connection.execute(f'PRAGMA table_info({table})'))
            except sqlite3.Error:
                continue
            finally:
                connection.close()
            # A test's old-shaped schema must not replace the current schema.
            old = result.get(table)
            if old and len(old['columns']) >= len(columns):
                continue
            graph = table in SHARED or table.startswith('view_') or table in {'emitted_view_rows'}
            result[table] = {'table': table, 'schema_statement': row['id'], 'columns': [], 'graph_data': graph}
            for _, column, kind, _, _, primary in columns:
                encoding = 'unchanged'
                if graph and column in VOCAB:
                    encoding = 'append-only vocabulary integer; compare/order by vocabulary text'
                elif graph and column in IDENTITIES and table in {'nodes', 'refs', 'edges', 'dispatch_hints', 'staging_ref_context'}:
                    encoding = 'content-derived 64-bit identity; retain full constructor inputs (identities.md)'
                elif graph and column == 'payload' and table in {'view_bindings', 'view_file_surfaces', 'view_dispatch_files'}:
                    encoding = 'compressed bytes of identical JSON'
                elif table == 'view_dispatch_files' and column in {'facts', 'sites', 'type_target_trace'}:
                    encoding = 'compressed bytes of identical JSON'
                elif graph and column in STRINGS:
                    encoding = 'interned string id; decode text for comparisons and output'
                if table == 'emitted_view_rows' and column == 'row_key':
                    encoding = 'typed stable-row primary key selected by table_name; preserve former text mapping (identities.md)'
                result[table]['columns'].append({'column': column, 'baseline_type': kind, 'primary_key_position': primary, 'encoding': encoding})
    return sorted(result.values(), key=lambda row: row['table'])


def order_inventory(rows):
    result = []
    for row in rows:
        if not graph_statement(row) or not re.match(r'\s*(SELECT|WITH)\b', row['baseline'], re.I):
            continue
        sql = row['baseline']
        orders = re.findall(r'\b(?:ORDER BY|GROUP BY)\s+(.+?)(?=\b(?:LIMIT|HAVING|UNION|ORDER BY)\b|$)', sql, re.S | re.I)
        result.append({'statement': row['id'], 'keys': orders, 'implicit_order': not orders,
                       'legacy_replacement': 'decoded text for interned/vocabulary columns; order-preserving digest integer for digest ids; preserve rowid',
                       'views_replacement': 'same, except reconstruct complete former view/dispatch id text before ordering',
                       'selection': 'Preserve every preceding tie key; only the first salted identity key permits within-group reorder.'})
    return result


def variants(row, tables):
    sql = row['baseline']
    expanded = [('literal', sql)]
    if '{requested_values}' in sql:
        expanded = [(f'batch-{n}', sql.replace('{requested_values}', ', '.join(['(?, ?)'] * n))) for n in [1, 499]]
    elif '{placeholders}' in sql and '{table}' not in sql:
        expanded = [(f'batch-{n}', sql.replace('{placeholders}', ', '.join(['?'] * n))) for n in [1, 999]]
    elif '{distinct}' in sql:
        expanded = [(label, sql.replace('{distinct}', value)) for label, value in [('all', ''), ('distinct', 'DISTINCT ')]]
    elif '{filter}' in sql:
        alias = 'n.' if 'FROM nodes n' in sql else ''
        expanded = [(label, sql.replace('{filter}', value)) for label, value in [('all', ''), ('non-test', f' AND COALESCE({alias}is_test, 0) = 0')]]
    elif '{identity}' in sql and 'FROM files' in sql:
        expanded = [(label, sql.replace('{identity}', value)) for label, value in [('identity', 'source_identity'), ('old-shape', 'NULL')]]
    elif '{table}' in sql:
        expanded = []
        stable = any(site['path'].endswith('/stable_rows.rs') for site in row['sites'])
        shapes = stable_write_shapes() if stable else {}
        for table in tables:
            name = table['table']
            if stable and name not in shapes:
                continue
            if 'FROM {table} WHERE file_path IN' in sql and name not in {'view_bindings', 'view_file_surfaces'}:
                continue
            columns = shapes.get(name, [c['column'] for c in table['columns']])
            if not table['graph_data'] or not columns:
                continue
            text = sql.replace('{table}', name).replace('{key}', columns[0])
            if '{assignments}' in text:
                for changed in [columns[1:2], columns[1:]]:
                    if changed:
                        expanded.append((f'{name}-update-{len(changed)}', text.replace('{assignments}', ','.join(f'{c}=?{i+2}' for i, c in enumerate(changed)))))
            else:
                text = text.replace('{placeholders}', ','.join(f'?{i+1}' for i in range(len(columns)))).replace('{}', ','.join(columns))
                expanded.append((name, text))
    result = []
    for label, text in expanded:
        for i, part in enumerate(statements(text)):
            result.append({'variant': f'{label}/{i}', 'sql': part})
    return result


def stable_write_shapes():
    source = (ROOT / 'crates/aft/src/views/materialization.rs').read_text()
    return {table: [text for _, text in strings(columns)] for table, columns in
            re.findall(r'stable_rows::write\(\s*[^,]+,\s*"(\w+)",\s*&\[(.*?)\]', source, re.S)}


def source_inventory():
    result = {'rust_order': [], 'id_text_readers': [], 'connection_helpers': [], 'attach': []}
    for folder in ['callgraph_store', 'views', 'commands', 'db']:
        for path in sorted((ROOT / 'crates/aft/src' / folder).rglob('*.rs')):
            if folder == 'commands' and not any(x in path.name for x in ['callgraph', 'inspect', 'search', 'zoom']):
                continue
            lines = path.read_text().splitlines()
            for i, line in enumerate(lines):
                site = {'path': str(path.relative_to(ROOT)), 'line': i + 1, 'text': line.strip()}
                if re.search(r'\.sort(?:_unstable)?(?:_by(?:_key|_cached_key)?)?\(|\.(?:min|max)_by(?:_key)?\(|\.dedup(?:_by(?:_key)?)?\(|BTree(?:Map|Set)|\.group_by\(', line):
                    result['rust_order'].append(site)
                if re.search(r'(?:strip_prefix|starts_with|split_once|rsplit_once|strip_suffix|split|contains)\(', line) and any(re.search(r'\b(?:id|ref_id|node_id|site_ref_id)\b|pos:symbol:|view:|top:|dispatch:|edge:', context) for context in lines[max(0, i - 2):i + 3]):
                    result['id_text_readers'].append(site)
                if re.search(r'(?:Connection|Self)::open(?:_in_memory|_with_flags)?\(', line):
                    result['connection_helpers'].append(site)
                if re.search(r'\bATTACH\b', line):
                    result['attach'].append(site)
    return result


def sweep_references(tables, rows):
    source = (ROOT / 'crates/aft/src/callgraph_store/mod.rs').read_text()
    index_list = source.split('const SECONDARY_INDEXES:', 1)[1].split('\n];', 1)[0]
    indexes = []
    for name, target in re.findall(r'\(\s*"([^"]+)",\s*"([^"]+)"\s*,?\s*\)', index_list):
        match = re.fullmatch(r'(\w+)\(([^)]+)\)', target)
        if match:
            indexes.append((name, match[1], match[2].split(',')[0].strip()))
    for row in rows:
        if graph_statement(row):
            for name, table, columns in re.findall(r'CREATE INDEX IF NOT EXISTS (\w+)\s+ON (\w+)\(([^)]+)\)', row['baseline']):
                indexes.append((name, table, columns.split(',')[0].strip()))
    result = []
    for table in tables:
        name = table['table']
        for column in table['columns']:
            if not column['encoding'].startswith('interned string'):
                continue
            col = column['column']
            existing = 'PRIMARY KEY' if column['primary_key_position'] == 1 else next((idx for idx, tab, first in indexes if tab == name and first == col), None)
            result.append({'table': name, 'column': col, 'index': existing or f'diet_sweep_{name}_{col}', 'new_index': existing is None,
                           'probe': f'SELECT 1 FROM {name} WHERE {col} = ?1 LIMIT 1'})
    for table, column in [('refs', 'hash_caller_symbol'), ('dispatch_hints', 'hash_caller_symbol')] + [('edges', col) for col in ['id_site_path', 'id_target_path', 'id_target_symbol', 'ref_identity_path']]:
        result.append({'table': table, 'column': column, 'index': f'diet_sweep_{table}_{column}', 'new_index': True, 'new_identity_input': True,
                       'probe': f'SELECT 1 FROM {table} WHERE {column} = ?1 LIMIT 1'})
    result.append({'table': 'emitted_view_rows', 'column': 'row_key', 'index': 'PRIMARY KEY(table_name,row_key)', 'new_index': False,
                   'probe': "SELECT 1 FROM emitted_view_rows WHERE table_name = ?1 AND row_key = ?2 LIMIT 1", 'condition': 'only path-keyed emitted tables, not identity-keyed tables'})
    return result


def coverage(inventory, traces):
    # SQLite expands parameters, but fixed literals remain part of a statement's
    # identity: a resolved-ref query must not cover an unresolved-ref query.
    def canonical(sql):
        return ''.join(part if i % 2 else re.sub(r'\s+', ' ', part)
                       for i, part in enumerate(re.split(r"('(?:''|[^'])*')", sql))).strip().rstrip(';')
    def pattern(sql):
        pieces = re.split(r"('(?:''|[^'])*'|\?[0-9]*|[:@$][A-Za-z_]\w*)", canonical(sql))
        bind = r"(?:NULL|[-+]?\d+(?:\.\d+)?(?:[eE][-+]?\d+)?|[xX]'[0-9a-fA-F]*'|'(?:''|[^'])*')"
        expression = ''.join(bind if re.fullmatch(r'\?[0-9]*|[:@$][A-Za-z_]\w*', piece) else re.escape(piece) for piece in pieces)
        return re.compile(expression)
    seen = {canonical(json.loads(line)) for trace in traces for line in trace.read_text().splitlines()}
    exemptions = json.loads((HERE / 'trace_exemptions.json').read_text())
    variants = {row['id'] + ':' + variant['variant']: variant['sql'] for row in inventory for variant in row['variants']}
    missing = []
    for key, sql in variants.items():
        matcher = pattern(sql)
        if not any(matcher.fullmatch(traced) for traced in seen) and key not in exemptions:
            missing.append(key)
    if missing:
        raise SystemExit('uncovered SQL statements: ' + ', '.join(missing))
    assert all(isinstance(reason, str) and reason.strip() for reason in exemptions.values()), 'empty trace exemption reason'
    stale = set(exemptions) - variants.keys()
    if stale:
        raise SystemExit('stale trace exemptions: ' + ', '.join(sorted(stale)))
    print(f'{len(inventory)} statements: trace coverage/exemptions checked')


def compare_plans(baseline, candidate):
    """Compare access paths, ignoring SQLite node numbering and join order."""
    assert baseline.keys() <= candidate.keys(), 'candidate omitted baseline statement plans'
    def accesses(plan):
        result = {}
        for detail in plan:
            match = re.search(r'\b(SCAN|SEARCH) (?:TABLE )?(\w+)(.*)', detail)
            if match:
                kind, table, tail = match.groups()
                equality = len(re.findall(r'=\?', tail))
                result.setdefault(table, []).append((kind, equality, tail))
        return result
    for key, new in candidate.items():
        old = baseline.get(key)
        if isinstance(new, dict) and 'unprepared' not in new:
            compare_plans(old or {}, new)
            continue
        if isinstance(old, dict) and old.get('unprepared') and old == new:
            # An intentionally unavailable fixture statement remains explicit
            # in the snapshot; it is not counted as a prepared query plan.
            continue
        if not isinstance(new, list) or (old is not None and not isinstance(old, list)):
            raise AssertionError(f'{key}: plan unavailable; resolve statement before comparison')
        before = accesses(old or [])
        for table, paths in accesses(new).items():
            previous = before.get(table, [])
            scans_before = sum(p[0] == 'SCAN' for p in previous)
            scans_after = sum(p[0] == 'SCAN' for p in paths)
            assert scans_after <= scans_before or (old is None and table == 'string_candidates'), f'{key}: new scan of {table}'
            equalities = sorted((p[1] for p in previous if p[0] == 'SEARCH'), reverse=True)
            searches = sorted((p for p in paths if p[0] == 'SEARCH'), key=lambda p: p[1], reverse=True)
            for index, (_, eq, _) in enumerate(searches[:len(equalities)]):
                assert eq >= equalities[index], f'{key}: fewer equality columns on {table}'
            for kind, eq, tail in paths:
                if not previous:
                    assert kind == 'SEARCH' or (old is None and table == 'string_candidates'), f'{key}: new scan of {table}'
                    if old is not None:
                        assert 'PRIMARY KEY' in tail or 'INTEGER PRIMARY KEY' in tail, f'{key}: new join {table} is not a primary-key search'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['capture', 'check', 'coverage', 'compare-plans'])
    parser.add_argument('paths', nargs='*', type=pathlib.Path)
    args = parser.parse_args()
    path = HERE / 'statements.json'
    if args.command == 'capture':
        rows = census()
        tables = schemas(rows)
        for row in rows:
            row['variants'] = variants(row, tables)
        dump(path, rows)
        dump(HERE / 'order_sql.json', order_inventory(rows))
        dump(HERE / 'source_inventory.json', source_inventory())
        dump(HERE / 'table_encoding.json', tables)
        dump(HERE / 'sweep_references.json', sweep_references(tables, rows))
        print(f'captured {len(rows)} SQL statements')
    elif args.command == 'check':
        actual = census()
        expected = json.loads(path.read_text())
        assert [(r['id'], r['baseline']) for r in actual] == [(r['id'], r['candidate']) for r in expected], 'SQL census changed; review and update statements.json'
        assert json.loads((HERE / 'table_encoding.json').read_text()) == schemas(actual), 'table encoding census is stale'
        assert json.loads((HERE / 'order_sql.json').read_text()) == order_inventory(actual), 'SQL ordering census is stale'
        assert json.loads((HERE / 'source_inventory.json').read_text()) == source_inventory(), 'Rust source census is stale'
        assert json.loads((HERE / 'sweep_references.json').read_text()) == sweep_references(schemas(actual), actual), 'sweep reference census is stale'
        print(f'checked {len(actual)} SQL statements')
    elif args.command == 'coverage':
        coverage(json.loads(path.read_text()), args.paths)
    else:
        assert len(args.paths) == 2, 'compare-plans BASELINE.json CANDIDATE.json'
        compare_plans(*(json.loads(p.read_text()) for p in args.paths))
        print('query-plan access paths checked')


if __name__ == '__main__':
    main()
