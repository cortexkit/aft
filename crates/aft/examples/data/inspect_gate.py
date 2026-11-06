#!/usr/bin/env python3
"""Gate for aft_inspect's dead code read from the data layer.

Runs the same aft binary on the same checkout twice, once as today
(AFT_DATA_LAYER unset) and once with AFT_DATA_LAYER=1, sends both the same
newline-delimited JSON (NDJSON) requests, and compares every inspect reply
byte for byte. The data layer changes only where dead code reads its call
graph, so every reply must be identical. A third run of today's binary (--control) shows which bytes
vary between two runs of the same code.

Each process gets fresh, empty caches and storage, runs with the file watcher
off, and analyses one copy of the checkout, restored between processes, so
replies naming absolute paths compare as they are. The sequence per process:

1. configure;
2. a scoped todos inspect, which binds the data layer's checkout without
   running Tier-2; a call-graph query retried until today's call graph is
   ready; and, with the data layer on, a wait for its first publication
   (its "data layer built" log line);
3. dead code and unused exports, twice (cold, then warm), each resent until
   no analysis in the reply is still running;
4. scripted edits through AFT's own write and edit tools, which leave the
   data layer's call graph pending;
5. dead code and unused exports again: the first reply as it comes, and the
   reply once settled.

A reply that differs from today's only where the control run's reply
differs too is reported as varying between runs (`explained`); any other
difference is `unexplained`.

With views on and the data layer on, the log must show that before the
edits every dead-code snapshot came from the data layer, and that after them
the data layer either declined because the edits were pending or served a
graph that includes them (the replies, equal to today's, show that it was
current). With views off, dead code keeps the legacy store, and the log must
show that dead code never asked the data layer.

    python3 crates/aft/examples/data/inspect_gate.py BINARY OUT \
        [--root PATH]... [--corpus DIR] [--modes legacy,views] [--control]

Exits nonzero when any reply differs, the data layer was not exercised, or
nothing was compared (no --root or --corpus checkout, or no reply).
"""
import argparse
import json
import os
import pathlib
import queue
import re
import shutil
import subprocess
import sys
import threading
import time

TIMEOUT = 900
# How long a measured request is resent while an analysis in its reply is
# still running. A Tier-2 pass on a large checkout outlasts one request's
# wait budget; the pass keeps running, and a later request reads its result.
SETTLE = 900
PROBE_FILE = 'aft_gate_probe.ts'
PROBE = 'export function aftGateProbe() {\n  return aftGateHelper();\n}\n\nfunction aftGateHelper() {\n  return 1;\n}\n'
SOURCE_SUFFIXES = ('.rs', '.ts', '.tsx', '.js', '.go', '.py', '.c', '.cpp', '.cs', '.zig')


def pending_reply(request, reply):
    """As in tests/store_diet/parity.py: replies that ask to be retried."""
    if reply.get('code') == 'callgraph_building':
        return True
    if request.get('name') == 'inspect' and reply.get('code') == 'inspect_not_fresh':
        return True
    graph = (reply.get('index') or {}).get('callgraph') or {}
    return graph.get('reason') == 'identity_provisional' or graph.get('status') in {'building', 'refreshing'}


def unsettled(reply):
    """Whether an inspect reply was sent while one of its analyses was still
    running: two runs can then differ only in how far each had got."""
    text = reply.get('text') or ''
    state = reply.get('scanner_state') or {}
    return (bool(state.get('pending_categories')) or '"building"' in json.dumps(reply.get('summary') or {})
            or 'still building' in text or 'did not finish' in text)


def tool(name, arguments):
    return {'command': 'tool_call', 'name': name, 'arguments': arguments}


def git(root, *args):
    subprocess.run(['git', '-C', str(root), '-c', 'user.name=gate', '-c', 'user.email=gate@example.invalid',
                    '-c', 'commit.gpgsign=false', *args], check=True, stdout=subprocess.DEVNULL)


def prepare_root(source, dest):
    """A copy of the checkout's tracked files, committed as a repository of its own."""
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    listed = subprocess.run(['git', '-C', str(source), 'ls-files', '-z'], capture_output=True)
    if listed.returncode == 0 and listed.stdout:
        for name in listed.stdout.decode().split('\0'):
            if not name:
                continue
            src = source / name
            if src.is_file() and not src.is_symlink():
                target = dest / name
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(src, target)
    else:
        shutil.copytree(source, dest, dirs_exist_ok=True, ignore=shutil.ignore_patterns('.git', 'target'))
    git(dest, 'init', '-q')
    git(dest, 'add', '-A')
    git(dest, 'commit', '-qm', 'gate base')


def restore_root(root):
    git(root, 'checkout', '-qf', 'HEAD', '--', '.')
    git(root, 'clean', '-fdxq')


def edit_target(root):
    """The first tracked source file in path order, or else the first file:
    the scoped request's scope and the file an edit appends to."""
    listed = subprocess.run(['git', '-C', str(root), 'ls-files'], capture_output=True, text=True, check=True)
    names = sorted(name for name in listed.stdout.split('\n') if name)
    for name in names:
        if name.endswith(SOURCE_SUFFIXES) and '/tests/' not in f'/{name}':
            return name
    return names[0]


def measured(prefix):
    return [(f'{prefix}-{section}', tool('inspect', {'sections': [section]}))
            for section in ('dead_code', 'unused_exports')]


class Process:
    def __init__(self, binary, root, cache, data_layer, out):
        self.root, self.out = root, out
        out.mkdir(parents=True)
        env = {key: value for key, value in os.environ.items()
               if not key.startswith('GIT_CONFIG_') and key != 'AFT_DATA_LAYER'}
        env.update(AFT_CACHE_DIR=str(cache), AFT_STORAGE_DIR=str(cache),
                   XDG_DATA_HOME=str(cache / 'data'), XDG_CONFIG_HOME=str(cache / 'config'),
                   XDG_STATE_HOME=str(cache / 'state'), XDG_CACHE_HOME=str(cache / 'cache'),
                   HOME=str(cache / 'home'), USERPROFILE=str(cache / 'home'),
                   AFT_CALLGRAPH_BUILD_WAIT_MS=str(TIMEOUT * 1000), AFT_TEST_DISABLE_FILE_WATCHER='1',
                   RUST_LOG=env.get('RUST_LOG', 'info'))
        # The data layer is on by default, so today's run turns it off.
        env['AFT_DATA_LAYER'] = '1' if data_layer else '0'
        self.stderr_path = out / 'stderr.txt'
        self.stderr = self.stderr_path.open('wb')
        self.process = subprocess.Popen([str(binary)], cwd=root, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=self.stderr, env=env)
        self.frames = queue.Queue()
        threading.Thread(target=self._consume, daemon=True).start()
        self.replies = {}
        self.timings = {}

    def _consume(self):
        for line in self.process.stdout:
            self.frames.put(line)
        self.frames.put(None)

    def send(self, request_id, body, retry=True, settle=False):
        """Sends one request and returns its reply. `retry` resends while the
        reply asks for a retry; `settle` also resends, for up to SETTLE
        seconds, while an analysis in the reply was still running."""
        request = dict(body, id=request_id)
        started_all = time.monotonic()
        settle_deadline = started_all + SETTLE
        deadline = settle_deadline + TIMEOUT
        attempts = 0
        while True:
            attempts += 1
            started = time.monotonic()
            self.process.stdin.write(json.dumps(request, separators=(',', ':')).encode() + b'\n')
            self.process.stdin.flush()
            while True:
                line = self.frames.get(timeout=max(0.001, deadline - time.monotonic()))
                if line is None:
                    raise RuntimeError(f'binary exited before replying to {request_id}')
                value = json.loads(line)
                if value.get('id') == request_id:
                    break
            elapsed = (time.monotonic() - started) * 1000
            if retry and pending_reply(body, value):
                if time.monotonic() >= deadline:
                    raise RuntimeError(f'{request_id} never became ready: {line[:400]!r}')
                time.sleep(0.1)
                continue
            if settle and unsettled(value) and time.monotonic() < settle_deadline:
                time.sleep(1.0)
                continue
            self.replies[request_id] = line
            self.timings[request_id] = {'ms': round(elapsed, 1), 'attempts': attempts,
                                        'total_ms': round((time.monotonic() - started_all) * 1000, 1),
                                        'settled': not unsettled(value),
                                        'peak_mib': self.peak_mib(), 'resident_mib': self.peak_mib('VmRSS')}
            (self.out / f'{request_id}.reply').write_bytes(line)
            return value

    def wait_log(self, pattern, timeout=TIMEOUT):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if re.search(pattern, self.stderr_path.read_text(errors='replace')):
                return
            if self.process.poll() is not None:
                raise RuntimeError(f'binary exited while waiting for {pattern!r}')
            time.sleep(0.2)
        raise RuntimeError(f'log never showed {pattern!r}')

    def log(self):
        return self.stderr_path.read_text(errors='replace')

    def peak_mib(self, field='VmHWM'):
        """The process's peak resident memory so far, in MiB, or with
        `field='VmRSS'` its resident memory now (Linux only)."""
        try:
            status = pathlib.Path(f'/proc/{self.process.pid}/status').read_text()
        except OSError:
            return None
        match = re.search(rf'^{field}:\s+(\d+) kB', status, re.M)
        return int(match.group(1)) // 1024 if match else None

    def close(self):
        # Read before the process exits, while /proc still has it.
        peak = self.peak_mib()
        if peak is not None:
            print(f'peak resident {peak} MiB: {self.out.name}', file=sys.stderr, flush=True)
        try:
            self.process.stdin.close()
            self.process.wait(timeout=60)
        except Exception:
            self.process.kill()
            self.process.wait()
        self.stderr.close()


def run(binary, root, cache, config, data_layer, out, target):
    """One process through the whole sequence; returns its replies, timings and log marks.
    With views off, dead code does not use the data layer, so nothing waits for it."""
    process = Process(binary, root, cache, data_layer, out)
    marks = {}
    try:
        configure = {'command': 'configure', 'harness': 'opencode', 'project_root': str(root),
                     'storage_dir': str(cache),
                     'config': [{'tier': 'user', 'source': str(cache / 'user.jsonc'), 'doc': json.dumps(config)}]}
        reply = process.send('configure', configure, retry=False)
        assert reply.get('success'), f'configure failed: {reply}'
        process.send('prime', tool('inspect', {'scope': target, 'sections': ['todos']}))
        # Today's call graph must be ready in both runs before dead code is
        # asked, or the data layer's run would answer while today's index
        # still reports building in the same reply.
        process.send('callgraph-ready', tool('callgraph', {'op': 'callers', 'path': target,
                                                           'symbol': 'aftGateNoSuchSymbol'}))
        if data_layer and config['views']['enabled']:
            process.wait_log(r'data layer built ')
        for request_id, body in measured('cold') + measured('warm'):
            process.send(request_id, body, settle=True)
        # The log reaches the file through a queue; let it drain before
        # marking where the edits begin.
        time.sleep(1.0)
        marks['before_edits'] = len(process.log())
        process.send('edit-write', tool('write', {'path': PROBE_FILE, 'content': PROBE}), retry=False)
        process.send('edit-append', tool('edit', {'path': target, 'appendContent': '\n'}), retry=False)
        for request_id, body in measured('edited-first'):
            process.send(request_id, body, retry=False)
        for request_id, body in measured('edited'):
            process.send(request_id, body, settle=True)
        return process.replies, process.timings, marks, process.log()
    finally:
        process.close()


def differences(left, right, path='', found=None):
    """Paths into two JSON values where they differ, at most twenty."""
    found = [] if found is None else found
    if len(found) >= 20:
        return found
    if isinstance(left, dict) and isinstance(right, dict):
        for key in sorted(set(left) | set(right)):
            if key not in left or key not in right:
                found.append(f'{path}.{key}: only {"today" if key in left else "data layer"}')
            else:
                differences(left[key], right[key], f'{path}.{key}', found)
    elif isinstance(left, list) and isinstance(right, list) and len(left) == len(right):
        for index, (a, b) in enumerate(zip(left, right)):
            differences(a, b, f'{path}[{index}]', found)
    elif left != right:
        found.append(f'{path}: {json.dumps(left)[:160]} != {json.dumps(right)[:160]}')
    return found


def compare(name, left, right):
    """Every inspect reply of two runs; returns the differing ids with their differences."""
    differing = {}
    ids = sorted(request_id for request_id in left if request_id not in ('configure',))
    assert set(ids) <= set(right), f'{name}: reply membership differs'
    for request_id in ids:
        if left[request_id] != right[request_id]:
            differing[request_id] = differences(json.loads(left[request_id]), json.loads(right[request_id]))
    return ids, differing


def unexplained(differing, control):
    """The differences from today's replies at places where the control
    run's replies did not differ from today's."""
    found = {}
    for request_id, paths in differing.items():
        varying = {path.split(':', 1)[0] for path in control.get(request_id, [])}
        left = [path for path in paths if path.split(':', 1)[0] not in varying]
        if left:
            found[request_id] = left
    return found


def data_layer_checks(log, marks):
    """What the data layer's log says it did before and after the edits."""
    before, after = log[:marks['before_edits']], log[marks['before_edits']:]
    return {
        'served_before_edits': before.count('source=data_layer'),
        'declined_before_edits': before.count('data layer not used'),
        'today_projections_before_edits': len(re.findall(r'source=(callgraph_store|view) ', before)),
        'served_after_edits': after.count('source=data_layer'),
        'declined_pending_after_edits': len(re.findall(r'data layer not used .*: call graph Building', after)),
        'snapshot_ms': [int(ms) for ms in re.findall(r'source=data_layer .* ms=(\d+)', log)],
        'today_snapshot_ms': [int(ms) for ms in re.findall(r'source=(?:callgraph_store|view) .* ms=(\d+)', log)],
    }


def costs(log):
    """Each dead-code snapshot's build time in one run's log, by where it
    came from: the data layer, today's database (`callgraph_store`), today's
    view (`view`), or the store a views request pinned (`pinned_view`)."""
    found = {}
    for source, ms in re.findall(r'perf tier2_callgraph_snapshot: source=(\w+) .* ms=(\d+)', log):
        found.setdefault(source, []).append(int(ms))
    return found


def gate_case(binary, name, source, work, modes, control):
    results = []
    root = (work / 'roots' / name).resolve()
    prepare_root(source, root)
    target = edit_target(root)
    for mode in modes:
        config = {'indexes': {'trigram': True, 'semantic': False, 'callgraph': True},
                  'views': {'enabled': mode == 'views'}, 'worktree': {'ram_overlay': False},
                  # Language servers are not part of this change; their timing
                  # would make two runs of one binary differ.
                  'inspect': {'categories': {'diagnostics': False}}}
        runs = {}
        for label, data_layer in [('today', False), ('data_layer', True)] + ([('control', False)] if control else []):
            restore_root(root)
            cache = (work / 'caches' / name / mode / label).resolve()
            if cache.exists():
                shutil.rmtree(cache)
            cache.mkdir(parents=True)
            out = work / 'out' / name / mode / label
            if out.exists():
                shutil.rmtree(out)
            started = time.monotonic()
            runs[label] = run(binary, root, cache, config, data_layer, out, target)
            runs[label] += (round(time.monotonic() - started, 1),)
        restore_root(root)
        ids, differing = compare(name, runs['today'][0], runs['data_layer'][0])
        result = {'case': name, 'mode': mode, 'replies_compared': len(ids), 'differing': differing,
                  'seconds': {label: run_[4] for label, run_ in runs.items()},
                  'timings': {label: run_[1] for label, run_ in runs.items()},
                  'costs': {label: costs(run_[3]) for label, run_ in runs.items()},
                  'data_layer': data_layer_checks(runs['data_layer'][3], runs['data_layer'][2])}
        if control:
            result['control_differing'] = compare(name, runs['today'][0], runs['control'][0])[1]
            result['unexplained'] = unexplained(differing, result['control_differing'])
        checks = result['data_layer']
        if mode == 'views':
            # Before the edits every snapshot must come from the data layer.
            # After them it declines while the edits are pending, or serves a
            # graph that includes them; the replies, equal to today's, tell
            # which is current.
            result['exercised'] = (checks['served_before_edits'] > 0 and checks['declined_before_edits'] == 0
                                   and checks['today_projections_before_edits'] == 0
                                   and checks['served_after_edits'] + checks['declined_pending_after_edits'] > 0)
        else:
            # With views off dead code keeps the legacy store: the data layer
            # must not have been asked at all.
            result['exercised'] = (checks['served_before_edits'] + checks['declined_before_edits']
                                   + checks['served_after_edits'] + checks['declined_pending_after_edits'] == 0)
        results.append(result)
        print(json.dumps({key: result[key] for key in ('case', 'mode', 'replies_compared', 'exercised')}
                         | {'differing': sorted(differing)}), flush=True)
        print(json.dumps({'case': name, 'mode': mode, 'data_layer_checks': {
            key: value for key, value in checks.items() if not key.endswith('_ms')},
            'memory': {label: {request_id: (timing['peak_mib'], timing['resident_mib'])
                               for request_id, timing in run_[1].items()}
                       for label, run_ in runs.items()}}), flush=True)
        for request_id, paths in sorted(result.get('unexplained', differing).items()):
            print(f'  {request_id}: {len(paths)} differences; first: {paths[:4]}', flush=True)
    return results


def main():
    global SETTLE
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('binary', type=pathlib.Path)
    parser.add_argument('out', type=pathlib.Path)
    parser.add_argument('--root', type=pathlib.Path, action='append', default=[])
    parser.add_argument('--corpus', type=pathlib.Path)
    parser.add_argument('--modes', default='legacy,views')
    parser.add_argument('--control', action='store_true')
    parser.add_argument('--only', help='comma-separated corpus project names')
    parser.add_argument('--settle', type=int, default=SETTLE,
                        help='seconds a measured request is resent while an analysis is still running')
    args = parser.parse_args()
    SETTLE = args.settle
    binary = args.binary.resolve()
    work = args.out.resolve()
    work.mkdir(parents=True, exist_ok=True)
    modes = args.modes.split(',')
    cases = [(root.resolve().name or 'root', root.resolve()) for root in args.root]
    if args.corpus:
        only = set(args.only.split(',')) if args.only else None
        cases += [(f'corpus-{path.name}', path.resolve()) for path in sorted(args.corpus.iterdir())
                  if path.is_dir() and (only is None or path.name in only)]
    if not cases:
        # With nothing to analyse every comparison passes vacuously.
        print('no checkout to compare: pass --root or a --corpus with checkouts in it', file=sys.stderr)
        sys.exit(2)
    results = []
    for name, source in cases:
        try:
            results += gate_case(binary, name, source, work, modes, args.control)
        except Exception as error:  # a case that cannot run is a failed case, not a skipped one
            results.append({'case': name, 'error': repr(error)})
            print(json.dumps({'case': name, 'error': repr(error)}), flush=True)
    (work / 'report.json').write_text(json.dumps(results, indent=2) + '\n')
    # A case-mode that compared no reply checked nothing, so it fails too.
    failed = [result for result in results
              if result.get('error') or result['differing'] or not result['exercised']
              or not result.get('replies_compared')]
    compared = sum(result.get('replies_compared', 0) for result in results)
    print(f'{len(results)} case-modes, {compared} replies compared, {len(failed)} failed', flush=True)
    sys.exit(1 if failed or not compared else 0)


if __name__ == '__main__':
    main()
