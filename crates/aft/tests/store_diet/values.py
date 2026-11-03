#!/usr/bin/env python3
"""Check independently inventoried stored vocabularies against recorded corpus rows."""
import argparse
import json

from inventory import HERE, dump


def inventory():
    locations, shapes, unresolved, json_fields = {}, {}, {}, {}
    for mode in ['legacy', 'views']:
        cases = json.loads((HERE / f'corpus-{mode}.json').read_text(encoding='utf-8'))
        for case, result in cases.items():
            where = {'case': case, 'mode': mode}
            for column, counts in result['values'].items():
                if not isinstance(counts, dict):
                    continue
                for value, count in counts.items():
                    if count:
                        locations.setdefault(column, {}).setdefault(value, []).append(dict(where, count=count))
                        if column == 'refs.unresolved_form':
                            unresolved.setdefault(value, []).append(dict(where, count=count))
            for shape, count in result.get('dispatch_shapes', {}).items():
                if count:
                    shapes.setdefault(shape, []).append(dict(where, count=count))
            for document, facts in result.get('json_facts', {}).items():
                for field, types in facts['schema'].items():
                    json_fields.setdefault(document.split(':', 1)[0] + ':' + field, []).append(dict(where, document=document, types=types))
    lifecycle = json.loads((HERE / 'lifecycle-values.json').read_text(encoding='utf-8'))
    for phase in ['after_cold', 'after_stale_mark']:
        for column, counts in lifecycle[phase].items():
            if isinstance(counts, dict):
                for value, count in counts.items():
                    if count:
                        locations.setdefault(column, {}).setdefault(value, []).append({'case': 'lifecycle', 'mode': 'legacy', 'phase': phase, 'count': count})
    assert lifecycle['staged_ref_write'], 'no actual staged-ref write observed'
    locations.setdefault('refs.status', {}).setdefault('staged', []).append({'case': 'lifecycle', 'mode': 'legacy', 'observation': 'successful cold-build INSERT', 'count': 1})
    ordering = json.loads((HERE / 'dispatch-order.json').read_text(encoding='utf-8'))['target_replacement']
    assert ordering['target_order'] == ['dispatch', 'exact'] and ordering['winning_provenance'] == 'exact'
    assert ordering['ref_row_exists'] is False
    locations.setdefault('edges.ref_row_presence', {}).setdefault('absent', []).append({'case': 'sqlite-writer', 'mode': 'views', 'observation': 'actual row-by-row writer, transaction rolled back after observation', 'count': 1})
    shapes['target:duplicate_provenance=true'] = [{'case': 'sqlite-writer', 'mode': 'views', 'count': 1}]
    stored = json.loads((HERE / 'stored-shape-values.json').read_text(encoding='utf-8'))
    for mode in ['legacy', 'views']:
        for column, counts in stored[mode].items():
            if isinstance(counts, dict):
                for value, count in counts.items():
                    if count:
                        locations.setdefault(column, {}).setdefault(value, []).append({'case': 'compatible-stored-shape', 'mode': mode, 'count': count})
    for shape, count in stored['dispatch_shapes'].items():
        if count:
            shapes.setdefault(shape, []).append({'case': 'compatible-stored-shape', 'mode': 'views', 'count': count})
    required = json.loads((HERE / 'required_values.json').read_text(encoding='utf-8'))
    missing, domains = [], {}
    for name, domain in required['domains'].items():
        covered = {}
        for value in domain['values']:
            matches = [dict(location, column=column) for column in domain['columns'] for location in locations.get(column, {}).get(value, [])]
            covered[value] = matches
            if not matches:
                missing.append(f'{name}={value}')
        domains[name] = {'source': domain['source'], 'values': covered}
    for value in required['dispatch_shape_values']:
        if not any(value in shape.split(':', 1)[-1].split(';') for shape in shapes):
            missing.append('dispatch_shape:' + value)
    return {'domains': domains, 'dispatch_shapes': shapes, 'unresolved_forms': unresolved, 'serialized_fact_fields': json_fields,
            'per_column': locations, 'missing': missing}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['record', 'check'])
    args = parser.parse_args()
    actual = inventory()
    path = HERE / 'value_inventory.json'
    if args.command == 'record':
        dump(path, actual)
    else:
        assert json.loads(path.read_text(encoding='utf-8')) == actual, 'stored-value locations changed'
    assert not actual['missing'], 'unexercised stored values: ' + ', '.join(actual['missing'])
    print(f'{len(actual["domains"])} vocabulary domains, {len(actual["dispatch_shapes"])} hint shapes, {len(actual["unresolved_forms"])} unresolved-ref forms checked')


if __name__ == '__main__':
    main()
