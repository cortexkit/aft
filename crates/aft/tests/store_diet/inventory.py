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
SQL = re.compile(r'^\s*(?:SELECT\s|INSERT\s+(?:OR\s+\w+\s+)?INTO\s|UPDATE\s+(?:OR\s+\w+\s+)?[\w{][\w{}.:]*\s+SET\s|DELETE\s+FROM\s|(?:CREATE|DROP|ALTER)\s+(?:UNIQUE\s+|TEMP\s+|VIRTUAL\s+)?(?:TABLE|INDEX|TRIGGER|VIEW)\s|PRAGMA\s|WITH\s+(?:RECURSIVE\s+)?\w+\s+AS\s*\(|REPLACE\s+INTO\s|EXPLAIN\s|ATTACH\s+DATABASE\s|DETACH\s|BEGIN(?:\s+(?:IMMEDIATE|TRANSACTION|EXCLUSIVE)|;|$)|COMMIT(?:;|$)|ROLLBACK(?:;|$)|ANALYZE(?:;|$)|VACUUM(?:;|$)|REINDEX\s)', re.I)


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
            if not SQL.match(sql):
                continue
            # Whitespace inside a literal is retained in text. Only the census
            # identity ignores indentation, to survive rustfmt-only changes.
            key = hashlib.sha256(re.sub(r'\s+', ' ', sql).strip().encode()).hexdigest()[:20]
            item = found.setdefault(key, {'id': 'sql-' + key, 'baseline': sql, 'candidate': sql, 'sites': []})
            item['sites'].append({'path': str(path.relative_to(ROOT)), 'line': text.count('\n', 0, pos) + 1})
    return sorted(found.values(), key=lambda row: row['id'])


def dump(path, value):
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + '\n')


def graph_statement(row):
    return any('/callgraph_store/' in s['path'] or '/views/' in s['path'] for s in row['sites'])


VOCAB = {'kind', 'status', 'import_kind', 'provenance', 'lang', 'backend'}
STRINGS = {'path', 'file_path', 'dep_file', 'caller_file', 'target_file', 'file', 'name', 'scoped_name',
           'signature', 'short_name', 'callee_last_segment', 'full_ref', 'module_path', 'local_name',
           'requested_name', 'namespace_alias', 'method_name', 'workspace_root', 'caller_symbol',
           'source_file', 'symbol', 'target', 'source', 'target_path', 'source_path'}
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
        if not graph_statement(row):
            continue
        for sql in statements(row['baseline']):
            sql = re.sub(r'--[^\n]*', '', sql).strip()
            match = re.match(r'CREATE (?:TEMP )?TABLE (?:IF NOT EXISTS )?(\w+)\s*\(', sql, re.I)
            if not match or '{' in sql:
                continue
            table = match[1]
            if table not in SHARED | VIEW_INFRA and not table.startswith('view_'):
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
                elif graph and column in STRINGS:
                    encoding = 'interned string id; decode text for comparisons and output'
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


def source_inventory():
    result = {'rust_order': [], 'id_text_readers': [], 'connection_helpers': [], 'attach': []}
    for folder in ['callgraph_store', 'views', 'commands', 'db']:
        for path in sorted((ROOT / 'crates/aft/src' / folder).rglob('*.rs')):
            if folder == 'commands' and not any(x in path.name for x in ['callgraph', 'inspect', 'search', 'zoom']):
                continue
            lines = path.read_text().splitlines()
            for i, line in enumerate(lines):
                site = {'path': str(path.relative_to(ROOT)), 'line': i + 1, 'text': line.strip()}
                if re.search(r'\.sort(?:_unstable)?(?:_by(?:_key)?)?\(|\.dedup(?:_by(?:_key)?)?\(|BTree(?:Map|Set)|\.group_by\(', line):
                    result['rust_order'].append(site)
                if re.search(r'(?:strip_prefix|starts_with|split_once|rsplit_once|strip_suffix|split|contains)\(', line) and any(re.search(r'\b(?:id|ref_id|node_id|site_ref_id)\b|pos:symbol:|view:|top:|dispatch:|edge:', context) for context in lines[max(0, i - 2):i + 3]):
                    result['id_text_readers'].append(site)
                if re.search(r'(?:Connection|Self)::open(?:_in_memory|_with_flags)?\(', line):
                    result['connection_helpers'].append(site)
                if re.search(r'\bATTACH\b', line):
                    result['attach'].append(site)
    return result


def coverage(inventory, traces):
    # SQLite expands bound parameters in trace callbacks. Match literals in a
    # parameter-neutral representation, not by substring (SELECT 1 must not
    # accidentally cover SELECT 10).
    def shape(sql):
        sql = re.sub(r"'(?:''|[^'])*'|\?[0-9]*|:[A-Za-z_]\w*|\b[0-9]+\b", '?', sql)
        return re.sub(r'\s+', ' ', sql).strip().rstrip(';').lower()
    seen = {shape(line) for trace in traces for line in trace.read_text().splitlines()}
    exemptions = json.loads((HERE / 'trace_exemptions.json').read_text())
    missing = [row['id'] for row in inventory if shape(row['candidate']) not in seen and row['id'] not in exemptions]
    if missing:
        raise SystemExit('uncovered SQL statements: ' + ', '.join(missing))
    stale = set(exemptions) - {row['id'] for row in inventory}
    if stale:
        raise SystemExit('stale trace exemptions: ' + ', '.join(sorted(stale)))
    print(f'{len(inventory)} statements: trace coverage/exemptions checked')


def compare_plans(baseline, candidate):
    """Compare access paths, ignoring SQLite node numbering and join order."""
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
        if not isinstance(new, list) or (old is not None and not isinstance(old, list)):
            raise AssertionError(f'{key}: plan unavailable; resolve statement before comparison')
        before = accesses(old or [])
        for table, paths in accesses(new).items():
            for kind, eq, tail in paths:
                previous = before.get(table, [])
                if not previous:
                    assert kind == 'SEARCH', f'{key}: new scan of {table}'
                    if old is not None:
                        assert 'PRIMARY KEY' in tail or 'INTEGER PRIMARY KEY' in tail, f'{key}: new join {table} is not a primary-key search'
                elif kind == 'SCAN':
                    assert any(p[0] == 'SCAN' for p in previous), f'{key}: SEARCH became SCAN on {table}'
                else:
                    assert eq >= max(p[1] for p in previous), f'{key}: fewer equality columns on {table}'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['capture', 'check', 'coverage', 'compare-plans'])
    parser.add_argument('paths', nargs='*', type=pathlib.Path)
    args = parser.parse_args()
    path = HERE / 'statements.json'
    if args.command == 'capture':
        rows = census()
        dump(path, rows)
        dump(HERE / 'order_sql.json', order_inventory(rows))
        dump(HERE / 'source_inventory.json', source_inventory())
        dump(HERE / 'table_encoding.json', schemas(rows))
        print(f'captured {len(rows)} SQL statements')
    elif args.command == 'check':
        actual = census()
        expected = json.loads(path.read_text())
        assert [(r['id'], r['baseline']) for r in actual] == [(r['id'], r['candidate']) for r in expected], 'SQL census changed; review and update statements.json'
        print(f'checked {len(actual)} SQL statements')
    elif args.command == 'coverage':
        coverage(json.loads(path.read_text()), args.paths)
    else:
        assert len(args.paths) == 2, 'compare-plans BASELINE.json CANDIDATE.json'
        compare_plans(*(json.loads(p.read_text()) for p in args.paths))
        print('query-plan access paths checked')


if __name__ == '__main__':
    main()
