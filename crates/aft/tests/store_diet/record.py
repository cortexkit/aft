#!/usr/bin/env python3
"""Record all parity oracles from one explicitly named main commit.

Run on a build machine with the repository's Rust/Python toolchains. The task
checkout is never built as the baseline: a scratch clone checks out the supplied
SHA and receives only the committed test scaffolding and trace-only patch.
"""
import argparse
import gzip
import hashlib
import os
import pathlib
import shutil
import subprocess
import sys
import tarfile

from inventory import HERE, ROOT, dump
from capture_io import capture_path, load_json

OUTPUTS = [
    'baseline.json', 'statements.json', 'order_sql.json', 'source_inventory.json',
    'table_encoding.json', 'sweep_references.json', 'trace_helpers.json',
    'baseline-trace.patch', 'trace_exemptions.json', 'baseline-plans.json',
    'legacy-replies.json', 'legacy-counts.json', 'legacy-seeded.json',
    'legacy-query-manifest.json', 'views-replies.json', 'views-counts.json',
    'views-seeded.json', 'views-query-manifest.json', 'views-json-facts.json',
    'corpus-legacy.json', 'corpus-views.json', 'corpus_manifest.json',
    'corpus_sources.json', 'required_values.json', 'lifecycle-values.json',
    'stored-shape-values.json', 'value_inventory.json', 'sqlite-order.json',
    'dispatch-order.json',
]


def clean_environment():
    env = dict(os.environ)
    for key in ['AFT_STORE_DIET_RECORD', 'AFT_STORE_DIET_BASELINE', 'AFT_STORE_DIET_SQL_TRACE', 'AFT_STORE_DIET_PYTHON']:
        env.pop(key, None)
    return env


def run(argv, cwd, env=None, allowed=()):
    print('→ ' + ' '.join(map(str, argv)), flush=True)
    result = subprocess.run(list(map(str, argv)), cwd=cwd, env=env if env is not None else clean_environment())
    if result.returncode and result.returncode not in allowed:
        raise subprocess.CalledProcessError(result.returncode, argv)
    return result.returncode


def git(*args):
    return subprocess.check_output(['git', *args], cwd=ROOT, text=True).strip()


def clone_baseline(sha, tree):
    # An older baseline does not have today's byte-pinning attributes. Keep its
    # checkout defaults at LF regardless of the machine's global Git settings.
    run(['git', 'clone', '--quiet', '--shared', '--no-checkout',
         '--config', 'core.autocrlf=false', '--config', 'core.eol=lf', ROOT, tree], ROOT)
    run(['git', 'checkout', '--quiet', '--detach', sha], tree)


def copy_tools(destination):
    if destination.exists():
        shutil.rmtree(destination)
    destination.mkdir(parents=True)
    for path in HERE.iterdir():
        if path.name.startswith('.') or path.name == '__pycache__':
            continue
        target = destination / path.name
        if path.is_dir():
            shutil.copytree(path, target)
        else:
            shutil.copy2(path, target)


def copy_outputs(tools, output_dir):
    output_dir.mkdir(parents=True, exist_ok=True)
    copied = []
    for name in OUTPUTS:
        path = capture_path(tools / name)
        if path.exists():
            destination = output_dir / path.name
            shutil.copy2(path, destination)
            alternative = destination.with_suffix('') if path.suffix == '.gz' else destination.with_name(destination.name + '.gz')
            if alternative.exists():
                alternative.unlink()
            copied.append(destination)
    return copied


def capture(binary, tree, tools, work, label, trace=None, cache_root=None):
    output = work / label
    env = clean_environment()
    if trace:
        env['AFT_STORE_DIET_SQL_TRACE'] = str(trace)
    argv = [sys.executable, tools / 'parity.py', 'capture', binary,
            tree / 'crates/aft/tests/fixtures/callgraph', (cache_root or work) / (label + '-cache'),
            tools / 'wire-manifest.json', output, '--config', tools / 'wire-policy.json']
    run(argv, tree, env)
    return output


def record(sha, include_trace, output_dir):
    sha = git('rev-parse', '--verify', sha + '^{commit}')
    if len(sha) != 40:
        raise ValueError('baseline must resolve to a full commit SHA')
    workspace = ROOT / 'target/scratch' / ('store-diet-record-' + sha)
    # Derived SQLite filenames are long; keep cache prefixes below the Unix
    # virtual file system's 512-byte pathname limit in nested worker checkouts.
    caches = ROOT / 'target/scratch' / ('diet-cache-' + sha[:12])
    for directory in [workspace, caches]:
        marker = directory / '.store-diet-recorder'
        if directory.exists():
            assert marker.read_text(encoding='utf-8').strip() == sha, 'refuse to replace an unrelated scratch directory'
            shutil.rmtree(directory)
        directory.mkdir(parents=True)
        marker.write_text(sha, encoding='utf-8')
    tree = workspace / 'baseline'
    clone_baseline(sha, tree)
    tools = tree / 'crates/aft/tests/store_diet'
    copy_tools(tools)
    patch = tools / 'baseline-harness.patch'
    check = subprocess.run(['git', 'apply', '--check', str(patch)], cwd=tree, capture_output=True)
    if check.returncode == 0:
        run(['git', 'apply', patch], tree)
    else:
        run(['git', 'apply', '--reverse', '--check', patch], tree)
    metadata = load_json((tools / 'baseline.json'))
    metadata['code_revision'] = sha
    metadata['status'] = 'recorded'
    metadata.pop('requested_code_revision', None)
    metadata['record_command'] = f'python3 crates/aft/tests/store_diet/record.py --baseline {sha}'
    metadata['fixture_record_command'] = metadata['record_command']
    metadata['normalised_fields'] = []
    dump(tools / 'baseline.json', metadata)
    for name, key in [('corpus_sources.json', 'baseline'), ('required_values.json', 'source_revision')]:
        data = load_json((tools / name))
        data[key] = sha
        dump(tools / name, data)
    for name, command in [('corpus.py', 'generate'), ('inventory.py', 'capture'), ('trace_patch.py', None)]:
        run([sys.executable, tools / name] + ([command] if command else []), tree)
    run(['cargo', 'build', '-p', 'agent-file-tools', '--bin', 'aft'], tree)
    target = pathlib.Path(os.environ.get('CARGO_TARGET_DIR', tree / 'target'))
    if not target.is_absolute():
        target = tree / target
    binary = target / 'debug' / ('aft.exe' if os.name == 'nt' else 'aft')
    # Historical artifact labels name past configurations and select no runtime variant now.
    for mode in ['legacy', 'views']:
        first = capture(binary, tree, tools, workspace, 'wire-' + mode + '-first', cache_root=caches)
        second = capture(binary, tree, tools, workspace, 'wire-' + mode + '-second', cache_root=caches)
        run([sys.executable, tools / 'parity.py', 'compare', first, second], tree)
        destination = tools / ('wire-' + mode)
        if destination.exists():
            shutil.rmtree(destination)
        destination.mkdir()
        for path in [*first.glob('*.reply'), first / 'capture.json']:
            shutil.copy2(path, destination / path.name)
    env = dict(clean_environment(), AFT_STORE_DIET_RECORD='1', AFT_STORE_DIET_BASELINE=sha, AFT_STORE_DIET_PYTHON=sys.executable)
    run(['cargo', 'test', '-p', 'agent-file-tools', '--lib', '--', 'store_diet', '--nocapture'], tree, env)
    run([sys.executable, tools / 'values.py', 'record'], tree)
    for mode in ['legacy', 'views']:
        run([sys.executable, tools / 'parity.py', 'fixture-manifest', tools / (mode + '-seeded.json'), tools / (mode + '-query-manifest.json')], tree)
    run(['cargo', 'test', '-p', 'agent-file-tools', '--lib', '--', 'store_diet'], tree)
    trace_report = None
    if include_trace:
        trace = workspace / 'sql-trace.jsonl'
        run(['git', 'apply', tools / 'baseline-trace.patch'], tree)
        traced_env = dict(clean_environment(), AFT_STORE_DIET_SQL_TRACE=str(trace))
        run(['cargo', 'test', '-p', 'agent-file-tools', '--lib', '--', 'store_diet'], tree, traced_env)
        run(['cargo', 'build', '-p', 'agent-file-tools', '--bin', 'aft'], tree)
        for mode in ['legacy', 'views']:
            output = capture(binary, tree, tools, workspace, 'wire-' + mode + '-traced', trace, cache_root=caches)
            run([sys.executable, tools / 'parity.py', 'compare', tools / ('wire-' + mode), output], tree)
        trace_report = workspace / 'uncovered.json'
        dump(tools / 'trace_exemptions.json', {})
        run([sys.executable, tools / 'inventory.py', 'coverage', trace, '--report', trace_report], tree, allowed=(1,))
        run([sys.executable, tools / 'inventory.py', 'draft-exemptions', trace_report, tools / 'trace_exemptions.json'], tree)
        run([sys.executable, tools / 'inventory.py', 'coverage', trace], tree)
        evidence = ROOT / '.cortexkit/internal-docs/investigations'
        evidence.mkdir(parents=True, exist_ok=True)
        report = capture_path(trace_report)
        shutil.copy2(report, evidence / ('store-diet-uncovered-' + sha + ''.join(report.suffixes)))
        with trace.open('rb') as source, gzip.open(evidence / ('store-diet-trace-' + sha + '.jsonl.gz'), 'wb') as destination:
            shutil.copyfileobj(source, destination)
    # Publish only a complete, self-replayed baseline. A failure leaves the old
    # oracle untouched and the scratch workspace available for diagnosis.
    outputs = copy_outputs(tools, output_dir)
    corpus = output_dir / 'corpus'
    if corpus.exists():
        shutil.rmtree(corpus)
    shutil.copytree(tools / 'corpus', corpus)
    for mode in ['legacy', 'views']:
        destination = output_dir / ('wire-' + mode)
        destination.mkdir(exist_ok=True)
        for path in (tools / ('wire-' + mode)).iterdir():
            shutil.copy2(path, destination / path.name)
    result = {'baseline': sha, 'trace_only_replay': include_trace,
              'normalised_fields': [], 'artifact_sha256': {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in outputs},
              'review_required': 'Review regenerated fixtures, statement mappings and trace exemption owner branches before committing.'}
    dump(output_dir / 'recording.json', result)
    archive = ROOT / 'target/scratch' / ('store-diet-recorded-' + sha + '.tar.gz')
    published = {path.name for path in outputs}
    with tarfile.open(archive, 'w:gz') as package:
        for path in sorted(output_dir.iterdir()):
            if path.name in published or path.name in {'corpus', 'wire-legacy', 'wire-views', 'recording.json'}:
                package.add(path, arcname=path.name)
    print(f'baseline recorded and replayed: {sha}', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', required=True, help='main commit whose producer code supplies the oracle')
    parser.add_argument('--without-trace', action='store_true', help='record provisional goldens only; not an acceptance run')
    parser.add_argument('--output-dir', type=pathlib.Path, default=HERE, help='artifact destination; defaults to the committed oracle directory')
    args = parser.parse_args()
    destination = args.output_dir.resolve()
    assert destination.is_relative_to(ROOT.resolve()), 'artifact output must stay within the checkout'
    record(args.baseline, not args.without_trace, destination)


if __name__ == '__main__':
    main()
