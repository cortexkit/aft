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


def cooked_string(value):
    # Decode in sequence so an escaped backslash followed by n stays literal.
    result, i = [], 0
    escapes = {'n': '\n', 'r': '\r', 't': '\t', '0': '\0', '\\': '\\', '"': '"', "'": "'"}
    while i < len(value):
        if value[i] != '\\':
            result.append(value[i])
            i += 1
            continue
        i += 1
        char = value[i]
        i += 1
        if char in '\r\n':
            while i < len(value) and value[i].isspace():
                i += 1
        elif char in escapes:
            result.append(escapes[char])
        elif char == 'x':
            result.append(chr(int(value[i:i + 2], 16)))
            i += 2
        elif char == 'u' and value[i] == '{':
            end = value.index('}', i)
            result.append(chr(int(value[i + 1:end].replace('_', ''), 16)))
            i = end + 1
        else:
            raise ValueError(f'unsupported Rust string escape: {char!r}')
    return ''.join(result)


def strings(text):
    for match in TOKEN.finditer(text):
        value = match.group('raw')
        if value is None and match.group('cooked') is not None:
            value = cooked_string(match.group('cooked'))
        if value is not None:
            yield match.start(), value


def rust_scopes(text):
    masked = TOKEN.sub(lambda match: ''.join('\n' if char == '\n' else ' ' for char in match[0]), text)
    # Array return types contain semicolons that are not declaration terminators.
    headers = list(masked)
    brackets = 0
    for i, char in enumerate(masked):
        if char == '[':
            brackets += 1
        elif char == ']':
            brackets -= 1
        elif char == ';' and brackets:
            headers[i] = ' '
    headers = ''.join(headers)
    stack, closing = [], {}
    for i, char in enumerate(masked):
        if char == '{':
            stack.append(i)
        elif char == '}' and stack:
            closing[stack.pop()] = i
    scopes = []
    for match in re.finditer(r'\b(fn|mod)\s+(\w+)[^{;]*\{', headers):
        body = match.end() - 1
        if body not in closing:
            continue
        boundary = max(masked.rfind(char, 0, match.start()) for char in '{};') + 1
        attributes = masked[boundary:match.start()]
        test = bool(re.search(r'#\[\s*test\s*\]|#\[\s*cfg\(\s*(?:all\(\s*)?test\b', attributes))
        scopes.append((match.start(), closing[body], match[1], match[2], test))
    return scopes


def owner_at(scopes, pos, path):
    containing = [scope for scope in scopes if scope[0] <= pos <= scope[1]]
    functions = [scope for scope in containing if scope[2] == 'fn']
    owner = min(functions, key=lambda scope: scope[1] - scope[0])[3] if functions else '<module>'
    test = path.name.endswith(('_tests.rs', '_test.rs')) or path.name == 'tests.rs' or '/tests/' in path.as_posix() or any(scope[4] for scope in containing)
    return {'owner': owner, 'test_only': test}


def census():
    found = {}
    sources = {path: path.read_text(encoding='utf-8') for path in sorted((ROOT / 'crates/aft/src').rglob('*.rs'))}
    test_files = set()
    for path, text in sources.items():
        directory = path.parent if path.stem in {'lib', 'mod'} else path.parent / path.stem
        for match in re.finditer(r'#\[\s*cfg\(\s*test\s*\)\s*\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;', text):
            test_files.update({directory / (match[1] + '.rs'), directory / match[1] / 'mod.rs'})
    for path, text in sources.items():
        scopes = rust_scopes(text)
        literals = list(strings(text))
        composed = []
        if path.as_posix().endswith('/views/materialization/dispatch.rs'):
            for pos, fragment in literals:
                if not fragment.startswith('{columns} '):
                    continue
                owner = owner_at(scopes, pos, path)['owner']
                heads = [value for head_pos, value in literals if head_pos < pos and value.lstrip().startswith('SELECT ') and owner_at(scopes, head_pos, path)['owner'] == owner]
                assert heads, 'dispatch SELECT prefix is not inventoried'
                composed.append((pos, fragment.replace('{columns}', heads[-1])))
        for pos, sql in literals + composed:
            if not SQL.match(re.sub(r'^\s*(?:(?:--[^\n]*(?:\n|$)|/\*.*?\*/)\s*)*', '', sql, flags=re.S)):
                continue
            # Keep whitespace inside SQL string literals significant. The text
            # itself is the stable identity; source lines are only citations.
            key = hashlib.sha256(sql.encode()).hexdigest()[:20]
            item = found.setdefault(key, {'id': 'sql-' + key, 'baseline': sql, 'candidate': sql, 'sites': []})
            site = owner_at(scopes, pos, path)
            site['test_only'] |= path in test_files
            item['sites'].append({'path': path.relative_to(ROOT).as_posix(), 'line': text.count('\n', 0, pos) + 1, **site})
    return sorted(found.values(), key=lambda row: row['id'])


def dump(path, value):
    with path.open('w', encoding='utf-8', newline='\n') as output:
        output.write(json.dumps(value, indent=2, ensure_ascii=False) + '\n')


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
        for sql in statements(row['baseline'].replace('{PROJECTION_DELTA_SPILL_TABLE}', 'projection_delta_spill')):
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
            graph = table in SHARED or table.startswith('view_') or table in {'emitted_view_rows', 'projection_delta_spill'}
            result[table] = {'table': table, 'schema_statement': row['id'], 'columns': [],
                             'graph_data': graph and table not in {'meta', 'view_sessions', 'view_usage'}}
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
    tables = schemas(rows)
    for row in rows:
        if not graph_statement(row):
            continue
        for variant in variants(row, tables):
            sql = variant['sql']
            if not re.match(r'\s*(SELECT|WITH)\b', sql, re.I):
                continue
            orders = re.findall(r'\b(?:ORDER BY|GROUP BY)\s+(.+?)(?=\b(?:LIMIT|HAVING|UNION|ORDER BY)\b|$)', sql, re.S | re.I)
            result.append({'statement': row['id'], 'variant': variant['variant'], 'keys': orders, 'implicit_order': not orders,
                           'legacy_replacement': 'decoded text for interned/vocabulary columns; order-preserving digest integer for digest ids; preserve rowid',
                           'views_replacement': 'same, except reconstruct complete former view/dispatch id text before ordering',
                           'selection': 'Preserve every preceding tie key; only the first salted identity key permits within-group reorder.'})
    return result


def variants(row, tables):
    sql = row['candidate'].replace('{PROJECTION_DELTA_SPILL_TABLE}', 'projection_delta_spill')
    expanded = [('literal', sql)]
    owners = {site.get('owner') for site in row.get('sites', [])}
    if 'load_name_match_refs' in owners and 'SELECT r.ref_id' in sql:
        expanded = [(label, sql + suffix + ' ORDER BY r.caller_file, r.byte_start, r.ref_id')
                    for label, suffix in [('all', ''), ('caller-file', ' AND r.caller_file = ?1')]]
    elif 'sample_absolute_data_path' in owners and '{column}' in sql:
        source = (ROOT / 'crates/aft/src/callgraph_store/mod.rs').read_text(encoding='utf-8')
        pairs = re.search(r'const STORE_DATA_PATH_COLUMNS[^=]*=\s*&\[(.*?)\];', source, re.S)
        assert pairs, 'stored-path column list is not inventoried'
        expanded = [(f'{table}-{column}', sql.replace('{table}', table).replace('{column}', column))
                    for table, column in re.findall(r'\("(\w+)",\s*"(\w+)"\)', pairs[1])]
    elif '{js}' in sql and '{rust}' in sql:
        expanded = []
        # Alias/package configuration is unbounded. Cover empty, single and OR
        # predicates, with the optional crate-directory predicate numbered last.
        for js_count in [0, 1, 2]:
            for rust_count in [0, 1, 2]:
                for crate_dir in [False, True]:
                    js = ' OR '.join(f'module_path GLOB ?{i+1}' for i in range(js_count)) or '0'
                    rust = ' OR '.join(f'module_path GLOB ?{js_count+i+1}' for i in range(rust_count)) or '0'
                    if crate_dir:
                        rust += f' OR caller_file GLOB ?{js_count+rust_count+1}'
                    label = f'js-{js_count}-rust-{rust_count}-crate-{int(crate_dir)}'
                    expanded.append((label, sql.replace('{js}', js).replace('{rust}', rust)))
    elif '{requested_values}' in sql:
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
    elif '{table}' in sql and graph_statement(row):
        expanded = []
        stable = any(site['path'].endswith('/stable_rows.rs') for site in row['sites'])
        shapes = stable_write_shapes() if stable else {}
        for table in tables:
            name = table['table']
            if stable and name not in shapes:
                continue
            if 'FROM {table} WHERE file_path IN' in sql and name not in {'view_bindings', 'view_file_surfaces'}:
                continue
            if 'materialize_reading' in owners and sql == 'DELETE FROM {table}' and name not in {'edges', 'refs', 'nodes', 'files', 'dispatch_hints', 'file_dependencies', 'view_bindings'}:
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
    source = (ROOT / 'crates/aft/src/views/materialization.rs').read_text(encoding='utf-8')
    return {table: [text for _, text in strings(columns)] for table, columns in
            re.findall(r'stable_rows::write\(\s*[^,]+,\s*"(\w+)",\s*&\[(.*?)\]', source, re.S)}


def checked_variants(row, tables):
    current = variants(row, tables)
    assert row['variants'] == current, 'executable SQL variants are stale; derive reviewed candidate SQL'
    return current


def source_inventory():
    result = {'rust_order': [], 'id_text_readers': [], 'connection_helpers': [], 'attach': []}
    for path in sorted((ROOT / 'crates/aft/src').rglob('*.rs')):
        lines = path.read_text(encoding='utf-8').splitlines()
        for i, line in enumerate(lines):
            site = {'path': path.relative_to(ROOT).as_posix(), 'line': i + 1, 'text': line.strip()}
            if re.search(r'\.sort(?:_unstable)?(?:_by(?:_key|_cached_key)?)?\(|\.(?:min|max)_by(?:_key)?\(|\.dedup(?:_by(?:_key)?)?\(|BTree(?:Map|Set)|BinaryHeap|\.group_by\(', line):
                result['rust_order'].append(site)
            if re.search(r'(?:strip_prefix|starts_with|split_once|rsplit_once|strip_suffix|split|contains)\(', line) and any(re.search(r'\b(?:id|ref_id|node_id|site_ref_id)\b|pos:symbol:|view:|top:|dispatch:|edge:', context) for context in lines[max(0, i - 2):i + 3]):
                result['id_text_readers'].append(site)
            if re.search(r'(?:Connection|Self)::open(?:_in_memory|_with_flags)?\(', line):
                result['connection_helpers'].append(site)
            if re.search(r'\bATTACH\b', line):
                result['attach'].append(site)
    return result



def sweep_references(tables, rows):
    source = (ROOT / 'crates/aft/src/callgraph_store/mod.rs').read_text(encoding='utf-8')
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


def coverage(inventory, traces, report=None):
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
    seen = {canonical(json.loads(line)) for trace in traces for line in trace.read_text(encoding='utf-8').splitlines()}
    exemptions = json.loads((HERE / 'trace_exemptions.json').read_text(encoding='utf-8'))
    variants = {row['id'] + ':' + variant['variant']: variant['sql'] for row in inventory for variant in row['variants']}
    missing, reached = [], set()
    for key, sql in variants.items():
        matcher = pattern(sql)
        if any(matcher.fullmatch(traced) for traced in seen):
            reached.add(key)
        elif key not in exemptions:
            missing.append(key)
    if report:
        dump(report, [{'id': key, 'sql': variants[key], 'sites': next(row['sites'] for row in inventory if key.startswith(row['id'] + ':'))} for key in missing])
    if missing:
        raise SystemExit('uncovered SQL statements: ' + ', '.join(missing))
    assert all(isinstance(reason, str) and reason.strip() for reason in exemptions.values()), 'empty trace exemption reason'
    assert not reached.intersection(exemptions), 'traced statements have obsolete exemptions; remove them'
    stale = set(exemptions) - variants.keys()
    if stale:
        raise SystemExit('stale trace exemptions: ' + ', '.join(sorted(stale)))
    print(f'{len(inventory)} statements: trace coverage/exemptions checked')


def draft_exemptions(missing, rows):
    by_id = {row['id']: row for row in rows}
    result = {}
    for entry in missing:
        row = by_id[entry['id'].split(':', 1)[0]]
        sites = row['sites']
        site = next((site for site in sites if not site.get('test_only')), sites[0])
        path, owner = site['path'], site.get('owner', '<module>')
        location = f'{path}:{site["line"]} ({owner})'
        if all(site.get('test_only') for site in sites):
            reason = 'Test-only setup, assertion, benchmark or fault-injection SQL; the focused store_diet filter does not run its owning test.'
        elif re.search(r'\{(?:[A-Za-z_]\w*|:[^}]*)\}', entry['sql']) or '{}' in re.sub(r"'(?:''|[^'])*'", "''", entry['sql']):
            reason = 'Static format/template fragment, not an independently emitted runtime SQL statement. Its caller must supply the identifiers, columns or predicate before this text can be traced; retain this explicit expansion gap before changing its encoding.'
        elif re.search(r':batch-(499|999)/', entry['id']) and graph_statement(row):
            reason = 'Maximum frontier batch variant; the committed fixture has fewer nodes than this batch boundary, so none of its queries can emit this exact expansion.'
        elif row['id'] == 'sql-931c083160eaedd719e7':
            reason = 'Test-only per-reference lookup control is disabled; ordinary materialization uses the batched production lookup.'
        elif '/views/materialization/' in path or path.endswith('/views/materialization.rs'):
            reason = 'This exact retained-row, dispatch-data or resolution-input SQL variant is not emitted by the selected cold and changed-manifest cases. Other variants in the owner may be traced; this exemption is not a claim that no diff workload ran.'
        elif path.endswith('/views/registry.rs') and 'view_tokens' in entry['sql']:
            reason = 'The v3 view-token mint, consume and expiry APIs are not requested by the six callgraph wire operations or the direct materialization fixtures.'
        elif path.endswith('/views/registry.rs'):
            reason = 'Legacy SQLite registry operation is not emitted by the v3 wire workload. V3 publication uses the blob-store registry rather than this legacy connection.'
        elif '/views/' in path:
            reason = 'Publication, borrowed/sealed generation, registry/eviction, verification or semantic-plane owner workload is absent from the direct cold graph fixture; its owner is not a graph adapter operation.'
        elif '/callgraph_store/' in path:
            reason = 'This exact refresh/staging-resume, read-only, backend/dispatch or resolver variant is not emitted by the selected cold-store and query cases. Other variants in the owner may execute; the corpus does not request every legacy refresh, migration, resumed generation or receiver combination.'
        elif '/inspect/' in path or path.endswith('/commands/inspect.rs'):
            reason = 'The direct adapter fixture does not run inspect, and the six-operation wire manifest explicitly disables inspect. No inspect cache/scanner/Tier-2 workload requests this statement.'
        else:
            reason = 'Separate state/ledger, storage maintenance, migration, blob-version or failure-path owner workload not requested by the graph query manifest; no corresponding administrative or mutation API call is present.'
        result[entry['id']] = reason + ' Owner: ' + location
    return result


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


def mapped_census(expected, actual):
    by_text = {row['baseline']: row for row in actual}
    assert set(by_text) == {row['candidate'] for row in expected}, 'SQL census changed; review baseline-to-candidate mappings'
    assert len({row['id'] for row in expected}) == len(expected), 'duplicate statement ids'
    mapped = []
    for row in expected:
        baseline = row['baseline']
        text = row['candidate']
        prefix = 'sql-' if baseline is not None else 'new-sql-'
        digest = hashlib.sha256((baseline if baseline is not None else text).encode()).hexdigest()[:20]
        assert row['id'] == prefix + digest, 'statement baseline identity changed'
        mapped.append(dict(row, baseline=text, sites=by_text[text]['sites']))
    return mapped


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['capture', 'derive', 'check', 'coverage', 'draft-exemptions', 'compare-plans'])
    parser.add_argument('paths', nargs='*', type=pathlib.Path)
    parser.add_argument('--report', type=pathlib.Path)
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
    elif args.command == 'derive':
        rows = json.loads(path.read_text(encoding='utf-8'))
        mapped = mapped_census(rows, census())
        tables = schemas(mapped)
        for row, current in zip(rows, mapped):
            row['sites'] = current['sites']
            row['variants'] = variants(row, tables)
        dump(path, rows)
        dump(HERE / 'order_sql.json', order_inventory(mapped))
        dump(HERE / 'source_inventory.json', source_inventory())
        dump(HERE / 'table_encoding.json', tables)
        dump(HERE / 'sweep_references.json', sweep_references(tables, mapped))
        print(f'derived inventories for {len(rows)} stable statement ids')
    elif args.command == 'check':
        actual = census()
        expected = json.loads(path.read_text(encoding='utf-8'))
        mapped = mapped_census(expected, actual)
        tables = schemas(mapped)
        for row in expected:
            checked_variants(row, tables)
        assert json.loads((HERE / 'table_encoding.json').read_text(encoding='utf-8')) == tables, 'table encoding census is stale'
        assert json.loads((HERE / 'order_sql.json').read_text(encoding='utf-8')) == order_inventory(mapped), 'SQL ordering census is stale'
        assert json.loads((HERE / 'source_inventory.json').read_text(encoding='utf-8')) == source_inventory(), 'Rust source census is stale'
        assert json.loads((HERE / 'sweep_references.json').read_text(encoding='utf-8')) == sweep_references(tables, mapped), 'sweep reference census is stale'
        print(f'checked {len(actual)} SQL statements')
    elif args.command == 'coverage':
        coverage(json.loads(path.read_text(encoding='utf-8')), args.paths, args.report)
    elif args.command == 'draft-exemptions':
        assert len(args.paths) == 2, 'draft-exemptions UNCOVERED.json OUTPUT.json'
        result = draft_exemptions(json.loads(args.paths[0].read_text(encoding='utf-8')), json.loads(path.read_text(encoding='utf-8')))
        dump(args.paths[1], result)
        print(f'wrote {len(result)} draft exemptions; review owner branches before accepting')
    else:
        assert len(args.paths) == 2, 'compare-plans BASELINE.json CANDIDATE.json'
        compare_plans(*(json.loads(p.read_text(encoding='utf-8')) for p in args.paths))
        print('query-plan access paths checked')


if __name__ == '__main__':
    main()
