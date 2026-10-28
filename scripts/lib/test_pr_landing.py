#!/usr/bin/env python3
"""Real scratch Git refs and a stateful fake gh exercise PR/release lifecycle."""

from __future__ import annotations

from datetime import datetime, timezone
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
OWNER = "94532+ualtinok@users.noreply.github.com"
COAUTHOR = "Co-authored-by: alfonso-aft <289616620+alfonso-aft@users.noreply.github.com>"

FAKE_GH = r'''
import datetime, io, json, os, pathlib, re, subprocess, sys, zipfile
a = sys.argv[1:]
p = pathlib.Path(os.environ['PR_TEST_STATE'])
s = json.loads(p.read_text())
with p.with_suffix('.calls').open('a') as f:
    f.write(json.dumps({'args': a, 'bypass': os.environ.get('GH_SHIM_BYPASS', '')}) + '\n')
def persist(): p.write_text(json.dumps(s))
def g(*args): return subprocess.check_output(['git', *args], text=True).strip()
def head():
    r = subprocess.run(['git', '--git-dir', s['origin'], 'rev-parse', 'refs/heads/' + s['branch']], text=True, capture_output=True)
    return r.stdout.strip() if r.returncode == 0 else g('rev-parse', 'HEAD')
def answer(v, exit_code=0):
    data = json.dumps(v)
    if '--jq' in a:
        print(subprocess.check_output(['jq', '-r', a[a.index('--jq')+1]], input=data, text=True).strip())
    else: print(data)
    sys.exit(exit_code)
def now(): return datetime.datetime.now(datetime.timezone.utc).isoformat()
def pr():
    return {'number': 7, 'state': 'closed' if s.get('merged') else 'open',
            'head': {'sha': head(), 'ref': s['branch'], 'repo': {'full_name': 'example/repo'}},
            'base': {'ref': 'main', 'repo': {'full_name': 'example/repo'}}}
if a == ['--status']:
    image = s.get('status_image', str(pathlib.Path(sys.argv[0]).resolve()))
    if s.get('wrong_gh_image'): image = '/untrusted/gh'
    inputs = {'agent_credentials_present': 'env:GH_TOKEN' if s.get('ambient_credentials') else 'absent'}
    answer({'executing_image': image,
            'last_rung': {'rung': s.get('rung', 'R3'), 'determination_inputs': inputs}})
if a == ['--co-author-line']:
    if not s.get('unbound'): print('Co-authored-by: alfonso-aft <289616620+alfonso-aft@users.noreply.github.com>')
    sys.exit(0)
if a[:2] == ['repo', 'view']: answer({'nameWithOwner': 'example/repo'})
if a[:2] == ['pr', 'view']: answer({'headRefOid': 'f'*40 if s.get('wrong_head') else head()})
if a[:3] == ['pr', 'checks', '7']:
    assert '--required' in a
    s['check_polls'] = s.get('check_polls', 0) + 1
    if s.get('checks_pending') and s.get('finish_after_polls') is not None and s['check_polls'] > s['finish_after_polls']:
        s['checks_pending'] = False
    persist()
    if s.get('failed_check'):
        answer([{'name': s['failed_check'], 'bucket': 'fail'}])
    if 'check_results' in s:
        answer(s['check_results'], s.get('checks_exit_code', 0))
    answer([{'name': 'Landing policy', 'bucket': 'pending' if s.get('checks_pending') else 'pass'}],
           8 if s.get('checks_pending') else 0)
if a[:2] == ['pr', 'create']:
    assert os.environ.get('GH_SHIM_BYPASS', '') == '', 'speech bypassed shim'
    assert '--base' in a and a[a.index('--base')+1] == 'main'
    assert '--label' not in a, 'the governed create route does not admit labels'
    s['created'] = s.get('created', 0) + 1; s['exists'] = True; persist()
    print('https://github.com/example/repo/pull/7'); sys.exit(0)
if a[:2] == ['pr', 'edit']:
    if '--add-label' in a:
        assert os.environ.get('GH_SHIM_BYPASS') == 'operator', 'label edit needs operator bypass'
        s.setdefault('labels', []).append(a[a.index('--add-label')+1]); persist()
    elif '--title' in a or '--body' in a:
        print('gh-shim: gh_shim_unclassified: verb "pr edit" has no bot-speech route in manifest 18; under GH_SHIM_BYPASS=operator gh pr edit admits only label changes', file=sys.stderr)
        sys.exit(86)
    else:
        assert os.environ.get('GH_SHIM_BYPASS', '') == '', 'PR text edit bypassed shim'
    sys.exit(0)
if a[:2] == ['pr', 'merge']:
    assert os.environ.get('GH_SHIM_BYPASS') == 'operator'
    assert '--match-head-commit' in a and a[a.index('--match-head-commit')+1] == head()
    assert '--admin' not in a
    if s.get('checks_pending') and not s.get('auto_merge'):
        raise SystemExit('GraphQL: Auto merge is not allowed for this repository (enablePullRequestAutoMerge)')
    s['merge_calls'] = s.get('merge_calls', 0) + 1
    if s.get('ghost_enqueue') or s['merge_calls'] <= s.get('ghost_merges', 0):
        # GitHub accepts the request and arms auto-merge, but never creates the
        # queue entry (no merge_group run follows).
        s['auto_merge_armed'] = True; s['ghost_armed'] = True; persist()
        print('! Pull request example/repo#7 will be added to the merge queue for main when ready', file=sys.stderr)
        sys.exit(0)
    if s.get('queue_on_merge'):
        s['queued'] = True; persist(); sys.exit(0)
    h = head(); base = g('--git-dir', s['origin'], 'rev-parse', 'main')
    tree = g('--git-dir', s['origin'], 'rev-parse', h+'^{tree}')
    oid = subprocess.check_output(['git', '--git-dir', s['origin'], 'commit-tree', tree,
                                  '-p', base, '-p', h, '-m', 'GitHub integration'], text=True,
                                  env={**os.environ, 'GIT_AUTHOR_NAME': 'GitHub', 'GIT_AUTHOR_EMAIL': 'noreply@github.com',
                                       'GIT_COMMITTER_NAME': 'GitHub', 'GIT_COMMITTER_EMAIL': 'noreply@github.com'}).strip()
    s['group'] = oid; s['polls'] = 0; s['admitted'] = True
    if s.get('auto_merge') and s.get('checks_pending'): s['auto_merge_armed'] = True
    if s.get('remove'): s['queued'] = True
    if not s.get('remove'):
        g('--git-dir', s['origin'], 'update-ref', 'refs/heads/main', oid)
        s['merged'] = True; s['merge_oid'] = oid
    persist(); sys.exit(0)
if a[:2] == ['run', 'list']:
    oid = s.get('merge_oid', '0'*40)
    answer([{'databaseId': 99, 'headSha': oid, 'headBranch': 'main', 'event': 'push',
             'status': 'completed', 'conclusion': 'success', 'attempt': 1,
             'url': 'https://github.com/example/repo/actions/runs/99'}])
if a[:2] == ['run', 'view']:
    answer({'jobs': [{'name': 'Landing policy', 'status': 'completed',
                     'conclusion': 'failure' if s.get('failed_required') else 'success'}]})
if not a or a[0] != 'api': raise SystemExit('unhandled fake gh: '+str(a))
endpoint = next((x for x in a[1:] if x.startswith('repos/')), '')
if endpoint.endswith('/check-runs?per_page=100'):
    s['greptile_polls'] = s.get('greptile_polls', 0) + 1
    pending = s.get('greptile_never_completes', False) or s['greptile_polls'] <= s.get('greptile_pending_polls', 0)
    persist()
    answer({'check_runs': [{'name': 'Greptile Review', 'app': {'slug': 'greptile-apps'},
                            'head_sha': head(), 'status': 'in_progress' if pending else 'completed',
                            'conclusion': None if pending else s.get('greptile_conclusion', 'success')}]})
# Fields this fake can answer, all real fields of GitHub's PullRequest type.
PULL_REQUEST_FIELDS = {'id', 'number', 'state', 'headRefOid', 'baseRefName', 'isMergeQueueEnabled',
                       'isInMergeQueue', 'mergeable', 'mergeCommit', 'autoMergeRequest',
                       'mergeQueueEntry', 'timelineItems', 'reviewThreads'}
def pull_request_fields(q):
    # Top-level field names of the pullRequest(...){...} selection set.
    depth, parens, fields = 0, 0, []
    for token in re.findall(r'[A-Za-z_]\w*|\S', q[q.index('{', q.index('pullRequest(')):]):
        if token == '{': depth += 1
        elif token == '}':
            depth -= 1
            if depth == 0: break
        elif token == '(': parens += 1
        elif token == ')': parens -= 1
        elif depth == 1 and parens == 0 and re.fullmatch(r'[A-Za-z_]\w*', token): fields.append(token)
    return fields
if 'graphql' in a:
    q = a[a.index('-f')+1]
    if 'pullRequest(' in q:
        # GitHub rejects the whole query when a selected field does not exist
        # (for example the REST-only `mergeable_state`); answer the same way.
        for field in pull_request_fields(q):
            if field not in PULL_REQUEST_FIELDS:
                print(f"gh: Field '{field}' doesn't exist on type 'PullRequest'", file=sys.stderr)
                sys.exit(1)
    if 'reviewThreads' in q:
        unresolved = s.get('unresolved', 0)
        if s.get('unresolved_after_admission') and s.get('admitted'): unresolved = 1
        # A reviewer opens a thread once the script has read mergeability (or
        # submitted) this many times, without moving the head.
        after_reads = s.get('unresolved_after_mergeable_reads')
        if after_reads is not None and s.get('mergeable_reads', 0) >= after_reads: unresolved = 1
        after_merges = s.get('unresolved_after_merge_calls')
        if after_merges is not None and s.get('merge_calls', 0) >= after_merges: unresolved = 1
        after = next((a[i+1].split('=', 1)[1] for i, arg in enumerate(a[:-1])
                      if arg == '-F' and a[i+1].startswith('after=')), None)
        page = 1 if after == 'cursor-1' else 0
        if s.get('thread_page_two') or s.get('thread_page_two_after_admission'):
            page_two_unresolved = bool(s.get('thread_page_two') or
                                       (s.get('thread_page_two_after_admission') and s.get('admitted')))
            if page == 0:
                nodes = [{'isResolved': True}] * 100
            else:
                nodes = [{'isResolved': not page_two_unresolved}]
        else:
            nodes = [{'isResolved': False}] * unresolved
        has_next_page = page == 0 and bool(s.get('thread_page_two') or s.get('thread_page_two_after_admission'))
        answer({'data': {'repository': {'pullRequest': {'reviewThreads': {'nodes':
                nodes, 'pageInfo': {'hasNextPage': has_next_page,
                                    'endCursor': 'cursor-1' if has_next_page else None}}}}}})
    if 'dequeuePullRequest' in q:
        # GitHub's DequeuePullRequestInput takes `id`; reject anything else
        # the way the real API does instead of accepting any field name.
        if 'input:{id:' not in q.replace(' ', ''):
            print("gh: Argument 'id' on InputObject 'DequeuePullRequestInput' is required.", file=sys.stderr)
            sys.exit(1)
        s['dequeued'] = s.get('dequeued', 0) + 1
        s['queued'] = False; persist()
        answer({'data': {'dequeuePullRequest': {'clientMutationId': None}}})
    if 'disablePullRequestAutoMerge' in q:
        if 'input:{pullRequestId:' not in q.replace(' ', ''):
            print("gh: Argument 'pullRequestId' on InputObject 'DisablePullRequestAutoMergeInput' is required.", file=sys.stderr)
            sys.exit(1)
        s['auto_merge_armed'] = False; s['ghost_armed'] = False
        s['disabled_auto_merge'] = s.get('disabled_auto_merge', 0) + 1
        persist()
        answer({'data': {'disablePullRequestAutoMerge': {'clientMutationId': None}}})
    if 'mergeable' in q:
        s['mergeable_reads'] = s.get('mergeable_reads', 0) + 1
        if s.get('ghost_armed') and s.get('ghost_mergeable'): mergeable = s['ghost_mergeable']
        elif s['mergeable_reads'] <= s.get('mergeable_unknown_reads', 0): mergeable = 'UNKNOWN'
        else: mergeable = s.get('mergeable', 'MERGEABLE')
        persist()
        answer({'data': {'repository': {'pullRequest': {
                'state': 'MERGED' if s.get('merged') else 'OPEN', 'mergeable': mergeable,
                'isInMergeQueue': bool(s.get('queued')) and not s.get('merged'),
                'autoMergeRequest': {'enabledAt': now()} if s.get('auto_merge_armed') else None}}}})
    if 'isInMergeQueue' in q and 'autoMergeRequest' in q and 'mergeQueueEntry' not in q:
        answer({'data': {'repository': {'pullRequest': {
                'id': s.get('pull_id', 'PR_fixture'),
                'isInMergeQueue': s.get('queued', False),
                'autoMergeRequest': {'enabledAt': now()} if s.get('auto_merge_armed') else None}}}})
    if 'mergeQueueEntry' not in q:
        answer({'data': {'repository': {'pullRequest': {'baseRefName': 'main',
                'isMergeQueueEnabled': s.get('queue', True)}}}})
    if s.get('null_admission_then_merge'):
        s['queue_snapshot_polls'] = s.get('queue_snapshot_polls', 0) + 1
        persist()
        if s['queue_snapshot_polls'] == 1:
            answer({'data': {'repository': {'pullRequest': {
                    'state': 'OPEN', 'baseRefName': 'main', 'headRefOid': head(),
                    'isMergeQueueEnabled': True, 'isInMergeQueue': True,
                    'autoMergeRequest': None, 'mergeCommit': None,
                    'mergeQueueEntry': None, 'timelineItems': {'nodes': []}}}}})
        answer({'data': {'repository': {'pullRequest': {
                'state': 'MERGED', 'baseRefName': 'main', 'headRefOid': head(),
                'isMergeQueueEnabled': True, 'isInMergeQueue': False,
                'autoMergeRequest': None, 'mergeCommit': {'oid': 'a'*40},
                'mergeQueueEntry': None, 'timelineItems': {'nodes': []}}}}})
    if s.get('membership_only_then_removed') and s.get('admitted'):
        s['membership_snapshot_polls'] = s.get('membership_snapshot_polls', 0) + 1
        persist()
        answer({'data': {'repository': {'pullRequest': {
                'state': 'OPEN', 'baseRefName': 'main', 'headRefOid': head(),
                'isMergeQueueEnabled': True,
                'isInMergeQueue': s['membership_snapshot_polls'] == 1,
                'autoMergeRequest': None, 'mergeCommit': None,
                'mergeQueueEntry': None, 'timelineItems': {'nodes': []}}}}})
    removed = s.get('remove') and s.get('admitted') and s.get('polls', 0) > 0
    queued = s.get('remove') and s.get('admitted') and not removed
    value = {'state': 'MERGED' if s.get('merged') else 'OPEN', 'baseRefName': 'main',
             'headRefOid': 'f'*40 if s.get('wrong_head') else head(),
              'isMergeQueueEnabled': s.get('queue', True), 'isInMergeQueue': bool(queued or (s.get('queued') and not removed)),
              'autoMergeRequest': {'enabledAt': now()} if s.get('auto_merge_armed') else None,
              'mergeCommit': {'oid': s.get('merge_oid')} if s.get('merged') else None,
              'mergeQueueEntry': {'headCommit': {'oid': head() if s.get('raw_entry_head') else s['group']}, 'enqueuedAt': now(), 'state': 'AWAITING_CHECKS'} if queued else None,
             'timelineItems': {'nodes': [{'createdAt': now(), 'reason': 'required check failed',
                                         'beforeCommit': {'oid': head() if s.get('raw_entry_head') else s['group']}}] if removed else []}}
    if s.get('admitted'): s['polls'] = s.get('polls', 0)+1; persist()
    answer({'data': {'repository': {'pullRequest': value}}})
slurp = '--slurp' in a
if endpoint == 'repos/example/repo/pulls/7':
    answer({'user': s.get('author', {'login': 'aft-alfonso[bot]', 'type': 'Bot', 'id': 318960130})})
if endpoint == 'repos/example/repo':
    answer({'allow_auto_merge': s.get('auto_merge', False)})
if '/protection/required_status_checks' in endpoint:
    if s.get('protection_not_found'): raise SystemExit('HTTP 404: branch protection not found')
    if s.get('protection_failure'): raise SystemExit('HTTP 403: protection unavailable')
    answer(s.get('required_status_checks', {'contexts': s.get('required_contexts', ['Landing policy'])}))
if endpoint.endswith('/rules/branches/main'):
    if s.get('rules_failure'): raise SystemExit('HTTP 403: rules unavailable')
    answer(s.get('branch_rules', []))
if '/pulls?' in endpoint:
    listed = [pr()] if s.get('exists') else []
    if s.get('abandoned') and 'state=all' in endpoint:
        listed = [{'number': 6, 'state': 'closed', 'merged_at': None,
                   'head': {'sha': '0' * 40, 'ref': s['branch'], 'repo': {'full_name': 'example/repo'}},
                   'base': {'ref': 'main', 'repo': {'full_name': 'example/repo'}}}] + listed
    answer([listed] if slurp else [pr()])
if endpoint.endswith('/comments'):
    if s.get('fail_comments'): raise SystemExit('HTTP 502')
    answer([{'created_at': '2025-01-01T00:00:00Z'}] if s.get('findings') else [])
if endpoint.endswith('/reviews'): answer([])
if '/commits/' in endpoint and '/git/commits/' not in endpoint:
    answer({'commit': {'committer': {'date': now() if s.get('recent') else '2000-01-01T00:00:00Z'}}})
if '/git/commits/' in endpoint:
    answer({'parents': [{'sha': x} for x in g('--git-dir', s['origin'], 'show', '-s', '--format=%P', endpoint.split('/')[-1]).split()]})
if '/actions/workflows/' in endpoint:
    run = {'id': 42, 'head_sha': s['group'], 'event': 'merge_group', 'conclusion': 'failure',
           'created_at': '2000-01-01T00:00:00Z' if s.get('stale_run') else now(),
           'html_url': 'https://github.com/example/repo/actions/runs/42'}
    answer([{'workflow_runs': [run]}])
if '/actions/runs/42/artifacts' in endpoint:
    answer([{'artifacts': [{'id': 123, 'name': 'queue-context-'+s['group'], 'expired': False}]}])
if '/actions/artifacts/123/zip' in endpoint:
    base = g('--git-dir', s['origin'], 'show', '-s', '--format=%P', s['group']).split()[0]
    record = {'base_sha': base, 'head_sha': 'f'*40 if s.get('wrong_context') else s['group'],
              'head_ref': 'refs/heads/gh-readonly-queue/main/pr-7-fixture'}
    stream = io.BytesIO()
    with zipfile.ZipFile(stream, 'w') as archive: archive.writestr('queue-context.json', json.dumps(record))
    sys.stdout.buffer.write(stream.getvalue()); sys.exit(0)
if '/actions/runs/42/jobs' in endpoint:
    # Real GitHub pages for this endpoint are objects, not arrays.
    answer([{'total_count': 1, 'jobs': [{'name': 'Unit / Unit tests (Linux)', 'conclusion': 'failure'}]}])
raise SystemExit('unhandled fake endpoint: '+endpoint)
'''


class PRFixture(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="aft-pr-landing-")
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.repo = root / "repo"
        self.repo.mkdir()
        self.origin = root / "origin.git"
        self.state_path = root / "state.json"
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith("GIT_CONFIG_") and not key.startswith("GIT_AUTHOR_")
                    and not key.startswith("GIT_COMMITTER_")}
        self.env.update(HOME=str(root / "home"), PR_TEST_STATE=str(self.state_path),
                        LAND_PR_POLL_SECONDS="0", LAND_PR_TIMEOUT_SECONDS="5",
                        AFT_MERGE_PR_GREPTILE_POLL_SECONDS="0.01",
                        AFT_MERGE_PR_GREPTILE_TIMEOUT_SECONDS="60",
                        AFT_MERGE_PR_MERGEABLE_POLL_SECONDS="0.01",
                        AFT_MERGE_PR_QUEUE_CONFIRM_POLL_SECONDS="0.01",
                        OPERATOR_GH_FALLBACK_PATHS="")
        Path(self.env["HOME"]).mkdir()
        for file in ("merge-pr.sh", "land-pr.sh", "landing-policy.py", "merge-queue-search.py",
                     "release.sh", "release-ci-gate.sh", "lib/land_pr.py", "lib/landing_policy.py",
                     "lib/release_queue.py", "lib/operator-gh.sh"):
            dest = self.repo / "scripts" / file
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / "scripts" / file, dest)
        for file in ("search_quality.py", "search_quality_lib.py"):
            dest = self.repo / "benchmarks/aft-search" / file
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / "benchmarks/aft-search" / file, dest)
        watch = self.repo / "scripts/watch-ci.sh"
        watch.write_text('#!/usr/bin/env bash\nset -euo pipefail\n[[ "$WATCH_CI_EVENT" == push && "$WATCH_CI_BRANCH" == main ]]\nprintf "%s\\n" "$1" > "$(dirname "$PR_TEST_STATE")/watched-oid"\n')
        watch.chmod(0o755)
        (self.repo / "scripts/release-gate-v049.mjs").write_text("// Fixture publication gate driven by the fake node executable.\n")
        self.git("init", "-qb", "main")
        self.git("config", "user.name", "ualtinok")
        self.git("config", "user.email", OWNER)
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "tag.gpgsign", "false")
        self.git("config", "core.hooksPath", "/dev/null")
        (self.repo / ".gitignore").write_text("__pycache__/\n*.pyc\n")
        for package in ("aft-bridge", "opencode-plugin", "pi-plugin"):
            path = self.repo / "packages" / package / "package.json"
            path.parent.mkdir(parents=True)
            path.write_text(json.dumps({"version": "1.0.0"}))
        checks = self.repo / ".github/required-checks.txt"
        checks.parent.mkdir()
        checks.write_text("Landing policy\n")
        self.git("add", ".")
        self.git("commit", "-qm", "base")
        self.base = self.git("rev-parse", "HEAD")
        self.git("init", "--bare", "-q", str(self.origin))
        self.git("remote", "add", "origin", str(self.origin))
        self.git("push", "-q", "origin", "main")
        self.git("fetch", "-q", "origin")
        self.git("checkout", "-qb", "accepted")
        self.git("commit", "--allow-empty", "-qm", "accepted delivery\n\n" + COAUTHOR)
        self.git("push", "-q", "origin", "HEAD:refs/heads/land/delivery")
        self.state = {"origin": str(self.origin), "branch": "land/delivery", "queue": True,
                      "author": {"login": "aft-alfonso[bot]", "type": "Bot", "id": 318960130}}
        self.write_state()
        bin_dir = root / "bin"
        bin_dir.mkdir()
        executable = bin_dir / "gh"
        executable.write_text(f"#!{sys.executable}\n" + FAKE_GH)
        executable.chmod(0o755)
        node = bin_dir / "node"
        node.write_text(f"#!{sys.executable}\n" + r'''
import json, os, pathlib, subprocess, sys
args = sys.argv[1:]
state = pathlib.Path(os.environ['PR_TEST_STATE'])
head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
with state.with_suffix('.calls').open('a') as f:
    f.write(json.dumps({'args': ['node', *args], 'source_oid': head}) + '\n')
folder = pathlib.Path(args[args.index('--snapshot-dir')+1])
record = folder / 'snapshot.json'
if '--prepare' in args:
    record.write_text(json.dumps({'source_oid': head}))
else:
    assert json.loads(record.read_text())['source_oid'] == head
''')
        node.chmod(0o755)
        self.env["PATH"] = str(bin_dir) + os.pathsep + self.env["PATH"]
        self.env["AFT_GH_SHIMS_DIR"] = str(bin_dir)

    def git(self, *args: str) -> str:
        return subprocess.check_output(["git", *args], cwd=self.repo, env=self.env,
                                       text=True, stderr=subprocess.DEVNULL).strip()

    def write_state(self, **values: object) -> None:
        self.state.update(values)
        self.state_path.write_text(json.dumps(self.state))

    def run_script(self, script: str, *args: str, env: dict | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(["bash", str(self.repo / "scripts" / script), *args], cwd=self.repo,
                              env={**self.env, **(env or {})}, text=True, capture_output=True)

    def calls(self) -> list[dict]:
        return [json.loads(line) for line in self.state_path.with_suffix(".calls").read_text().splitlines()]

    def merges(self) -> list[dict]:
        return [call for call in self.calls() if call["args"][:2] == ["pr", "merge"]]

    def merge(self, *args: str, env: dict | None = None) -> subprocess.CompletedProcess:
        return self.run_script("merge-pr.sh", "7", *args, env=env)

    def prepare_existing_pr_head(self) -> str:
        descriptor = self.repo / "benchmarks/aft-search/slice-descriptors/land-delivery.json"
        descriptor.parent.mkdir(parents=True, exist_ok=True)
        descriptor.write_text(json.dumps({"slice_class": "non_ranking", "targeted_mechanism": "none",
                                          "kind": "harness", "fixtures": ["harness-goldens"]}, indent=2) + "\n")
        self.git("add", str(descriptor.relative_to(self.repo)))
        self.git("commit", "-qm", "add existing landing descriptor\n\n" + COAUTHOR)
        self.git("push", "-q", "origin", "HEAD:refs/heads/land/delivery")
        return self.git("rev-parse", "HEAD")


class MergeAdmissionTests(PRFixture):
    def test_queue_admission_preserves_head_pin_and_operator_boundary(self) -> None:
        head = self.git("rev-parse", "HEAD")
        result = self.merge()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.merges()[0]["args"], ["pr", "merge", "7", "--match-head-commit", head])
        self.assertEqual(self.merges()[0]["bypass"], "operator")

    def test_classic_protection_404_uses_readable_ruleset_checks(self) -> None:
        self.write_state(
            protection_not_found=True,
            branch_rules=[{"type": "required_status_checks", "parameters": {
                "required_status_checks": [{"context": "Ruleset policy"}]}}],
            check_results=[],
            checks_exit_code=8,
            auto_merge=True,
            queue_on_merge=True,
        )
        result = self.merge()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.merges()), 1)
        self.assertIn("required checks still running for #7", result.stderr)
        api_paths = [arg for call in self.calls() for arg in call["args"]
                     if arg.startswith("repos/")]
        self.assertIn("repos/example/repo/rules/branches/main", api_paths)

    def test_classic_404_and_unreadable_rulesets_refuse(self) -> None:
        self.write_state(protection_not_found=True, rules_failure=True, auto_merge=True)
        result = self.merge()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("could not read effective ruleset checks", result.stderr)
        self.assertIn("classic branch protection returned 404", result.stderr)
        self.assertEqual(self.merges(), [])

    def test_malformed_classic_required_checks_refuse(self) -> None:
        self.write_state(required_status_checks={"contexts": [""]}, auto_merge=True)
        result = self.merge()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("malformed branch-protection required checks", result.stderr)
        self.assertEqual(self.merges(), [])

    def test_malformed_ruleset_required_checks_refuse(self) -> None:
        self.write_state(
            branch_rules=[{"type": "required_status_checks", "parameters": {
                "required_status_checks": [{"context": ""}]}}],
            auto_merge=True,
        )
        result = self.merge()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("malformed effective ruleset checks", result.stderr)
        self.assertEqual(self.merges(), [])

    def test_pending_required_checks_without_auto_merge_refuse_without_waiting(self) -> None:
        self.write_state(checks_pending=True)
        result = self.merge()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("required checks still running", result.stderr)
        self.assertIn("auto-merge is off", result.stderr)
        self.assertEqual(json.loads(self.state_path.read_text())["check_polls"], 1)
        self.assertEqual(self.merges(), [])

    def test_partial_required_results_leave_missing_contexts_pending(self) -> None:
        self.write_state(
            required_status_checks={"checks": [{"context": "Landing policy"}, {"context": "Tests / Unit"}]},
            branch_rules=[{"type": "required_status_checks", "parameters": {
                "required_status_checks": [{"context": "Tests / Integration"}]}}],
            check_results=[{"name": "Landing policy", "bucket": "skipping"}],
        )
        result = self.merge()
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn("required checks still running", result.stderr)
        self.assertIn("Tests / Unit", result.stderr)
        self.assertIn("Tests / Integration", result.stderr)
        self.assertEqual(self.merges(), [])

    def test_no_reported_required_checks_are_pending_by_name(self) -> None:
        self.write_state(
            required_contexts=["Unit tests", "Integration tests"],
            check_results=[],
            auto_merge=True,
            ghost_enqueue=True,
        )
        result = self.merge(env={"AFT_MERGE_PR_QUEUE_CONFIRM_TIMEOUT_SECONDS": "0"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("required checks still running for #7", result.stderr)
        self.assertIn("confirmed #7 with GitHub", result.stderr)
        state = json.loads(self.state_path.read_text())
        self.assertEqual(state["check_polls"], 2)
        self.assertTrue(state["auto_merge_armed"])
        self.assertEqual(len(self.merges()), 1)

    def test_required_check_protection_read_failure_refuses(self) -> None:
        self.write_state(protection_failure=True, auto_merge=True)
        result = self.merge()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("could not read branch-protection required checks", result.stderr)
        self.assertIn("base branch 'main'", result.stderr)
        self.assertEqual(self.merges(), [])

    def test_red_required_check_refuses_by_name(self) -> None:
        self.write_state(failed_check="Design gate")
        result = self.merge("--force-reason", "required checks cannot be waived")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Design gate", result.stderr)
        self.assertEqual(self.merges(), [])

    def test_auto_merge_submits_pending_checks_without_waiting(self) -> None:
        self.write_state(checks_pending=True, auto_merge=True)
        result = self.merge()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(self.state_path.read_text())["check_polls"], 1)
        self.assertIn("--match-head-commit", self.merges()[0]["args"])

    def test_auto_merge_still_refuses_a_red_check_by_name(self) -> None:
        self.write_state(failed_check="Design gate", auto_merge=True)
        result = self.merge()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Design gate", result.stderr)
        self.assertEqual(self.merges(), [])

    def test_unresolved_threads_refuse_admission(self) -> None:
        self.write_state(unresolved=1, auto_merge_armed=True, queued=True)
        result = self.merge()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.merges(), [])
        calls = [call["args"] for call in self.calls()]
        self.assertTrue(any("disablePullRequestAutoMerge" in " ".join(args) for args in calls))
        self.assertTrue(any("dequeuePullRequest" in " ".join(args) for args in calls))

    def test_merge_waits_until_greptile_review_of_exact_head_completes(self) -> None:
        self.write_state(greptile_pending_polls=2)
        result = self.merge()
        self.assertEqual(result.returncode, 0, result.stderr)
        state = json.loads(self.state_path.read_text())
        self.assertEqual(state.get("greptile_polls", 0), 3)
        calls = self.calls()
        review_calls = [call for call in calls if "/check-runs?per_page=100" in " ".join(call["args"])]
        self.assertTrue(review_calls)
        self.assertTrue(all(f"/commits/{self.git('rev-parse', 'HEAD')}/check-runs?" in " ".join(call["args"])
                            for call in review_calls))
        merge_index = next(i for i, call in enumerate(calls) if call["args"][:2] == ["pr", "merge"])
        last_review_index = max(i for i, call in enumerate(calls) if "/check-runs?per_page=100" in " ".join(call["args"]))
        self.assertLess(last_review_index, merge_index)

    def test_greptile_review_timeout_refuses_without_submitting(self) -> None:
        self.write_state(greptile_never_completes=True)
        result = self.merge(env={"AFT_MERGE_PR_GREPTILE_TIMEOUT_SECONDS": "0"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Greptile Review did not complete", result.stderr)
        self.assertEqual(self.merges(), [])

    def test_cancelled_or_skipped_greptile_review_refuses(self) -> None:
        for conclusion in ("cancelled", "skipped"):
            with self.subTest(conclusion=conclusion):
                self.write_state(greptile_conclusion=conclusion, greptile_polls=0)
                result = self.merge()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(conclusion, result.stderr)
                self.assertEqual(self.merges(), [])

    def test_neutral_greptile_review_is_accepted(self) -> None:
        self.write_state(greptile_conclusion="neutral")
        result = self.merge()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.merges()), 1)

    def test_armed_pr_is_disarmed_before_greptile_review_wait(self) -> None:
        self.write_state(auto_merge_armed=True, queued=True, greptile_pending_polls=1)
        result = self.merge()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = [call["args"] for call in self.calls()]
        review_index = next(i for i, args in enumerate(calls) if "/check-runs?per_page=100" in " ".join(args))
        dequeue_index = next(i for i, args in enumerate(calls) if "dequeuePullRequest" in " ".join(args))
        disable_index = next(i for i, args in enumerate(calls) if "disablePullRequestAutoMerge" in " ".join(args))
        self.assertLess(dequeue_index, review_index)
        self.assertLess(disable_index, review_index)

    def test_greptile_review_timeout_disarms_armed_pr(self) -> None:
        self.write_state(auto_merge_armed=True, queued=True, greptile_never_completes=True)
        result = self.merge(env={"AFT_MERGE_PR_GREPTILE_TIMEOUT_SECONDS": "0"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Greptile Review did not complete", result.stderr)
        self.assertEqual(self.merges(), [])
        state = json.loads(self.state_path.read_text())
        self.assertEqual(state.get("disabled_auto_merge"), 1)
        self.assertEqual(state.get("dequeued"), 1)
        self.assertFalse(state["auto_merge_armed"])
        self.assertFalse(state["queued"])

    def test_unresolved_thread_on_second_graphql_page_refuses_admission(self) -> None:
        self.write_state(thread_page_two=True)
        result = self.merge()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unresolved review thread", result.stderr)
        self.assertEqual(self.merges(), [])
        self.assertTrue(any("after=cursor-1" in " ".join(call["args"]) for call in self.calls()))

    def test_post_head_findings_require_acknowledgment(self) -> None:
        self.write_state(findings=True)
        self.assertNotEqual(self.merge().returncode, 0)
        self.assertEqual(self.merges(), [])

    def test_explicit_findings_acknowledgment_still_works(self) -> None:
        self.write_state(findings=True)
        result = self.merge(env={"AFT_MERGE_PR_ACK_FINDINGS": "resolved and reviewed"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ACK", result.stdout)

    def test_five_minute_settle_window_is_retained(self) -> None:
        self.write_state(recent=True)
        result = self.merge()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("settle window", result.stderr)

    def test_force_reason_remains_deliberate_admission_override(self) -> None:
        self.write_state(unresolved=1, recent=True, findings=True)
        result = self.merge("--force-reason", "operator reviewed all findings")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("proceeding anyway", result.stdout + result.stderr)

    def test_legacy_requires_explicit_disabled_queue_policy(self) -> None:
        self.write_state(queue=False)
        self.assertNotEqual(self.merge().returncode, 0)
        result = self.merge("--legacy-merge")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--merge", self.merges()[0]["args"])

    def test_force_reason_cannot_bypass_enabled_queue_with_legacy(self) -> None:
        result = self.merge("--legacy-merge", "--force-reason", "not an admin bypass")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.merges(), [])

    def test_unknown_queue_policy_fails_closed(self) -> None:
        self.write_state(queue=None)
        self.assertNotEqual(self.merge("--legacy-merge").returncode, 0)
        self.assertEqual(self.merges(), [])

    def test_accepted_head_change_is_not_force_overridable(self) -> None:
        result = self.merge("--expected-head", "f" * 40, "--force-reason", "cannot waive SHA")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.merges(), [])

    def test_failed_comments_query_is_not_hidden_by_pipeline(self) -> None:
        self.write_state(fail_comments=True)
        self.assertNotEqual(self.merge().returncode, 0)
        self.assertEqual(self.merges(), [])

    def queue_state_reads(self) -> list[int]:
        """Indexes of calls reading mergeability/queue membership."""
        return [i for i, call in enumerate(self.calls()) if "mergeable" in " ".join(call["args"])]

    def merge_indexes(self) -> list[int]:
        return [i for i, call in enumerate(self.calls()) if call["args"][:2] == ["pr", "merge"]]

    def test_fake_gh_rejects_unknown_pull_request_field_like_github(self) -> None:
        query = ("query($owner:String!,$repo:String!,$pr:Int!){repository(owner:$owner,name:$repo){"
                 "pullRequest(number:$pr){state mergeable_state}}}")
        result = subprocess.run(["gh", "api", "graphql", "-f", f"query={query}", "-F", "pr=7"],
                                cwd=self.repo, env=self.env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn("Field 'mergeable_state' doesn't exist on type 'PullRequest'", result.stderr)

    def test_enqueue_accepted_but_never_in_queue_refuses_and_disarms(self) -> None:
        self.write_state(ghost_enqueue=True)
        # Whole-second deadline arithmetic: 2s guarantees at least two reads.
        result = self.merge(env={"AFT_MERGE_PR_QUEUE_CONFIRM_TIMEOUT_SECONDS": "2",
                                 "AFT_MERGE_PR_QUEUE_CONFIRM_POLL_SECONDS": "0.5"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("PR #7 was submitted but GitHub never listed it in the merge queue", result.stderr)
        self.assertIn("mergeable MERGEABLE", result.stderr)
        self.assertIn("will be added to the merge queue for main when ready", result.stderr)
        self.assertNotIn("resubmitting", result.stderr)
        # A computed mergeability does not explain the missing entry: no retry.
        self.assertEqual(len(self.merges()), 1)
        self.assertGreater(len([i for i in self.queue_state_reads() if i > self.merge_indexes()[0]]), 1)
        state = json.loads(self.state_path.read_text())
        self.assertFalse(state["auto_merge_armed"])
        self.assertEqual(state.get("disabled_auto_merge"), 1)

    def test_unknown_mergeability_after_missing_entry_resubmits_once_then_refuses(self) -> None:
        self.write_state(ghost_enqueue=True, ghost_mergeable="UNKNOWN")
        result = self.merge(env={"AFT_MERGE_PR_QUEUE_CONFIRM_TIMEOUT_SECONDS": "0"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("resubmitting once", result.stderr)
        self.assertIn("PR #7 was submitted but GitHub never listed it in the merge queue", result.stderr)
        self.assertIn("mergeable UNKNOWN", result.stderr)
        self.assertIn("resubmission response:", result.stderr)
        self.assertEqual(len(self.merges()), 2)
        state = json.loads(self.state_path.read_text())
        self.assertFalse(state["auto_merge_armed"])

    def test_resubmission_after_unknown_mergeability_can_enter_queue(self) -> None:
        self.write_state(ghost_merges=1, ghost_mergeable="UNKNOWN", queue_on_merge=True)
        result = self.merge(env={"AFT_MERGE_PR_QUEUE_CONFIRM_TIMEOUT_SECONDS": "0"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("after resubmission", result.stderr)
        self.assertEqual(len(self.merges()), 2)

    def test_unknown_then_mergeable_waits_before_enqueueing(self) -> None:
        self.write_state(mergeable_unknown_reads=2, queue_on_merge=True)
        result = self.merge()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("waiting for it to be computed", result.stderr)
        merge_index = self.merge_indexes()
        self.assertEqual(len(merge_index), 1)
        self.assertEqual(len([i for i in self.queue_state_reads() if i < merge_index[0]]), 3)

    def test_mergeability_that_stays_unknown_refuses_without_submitting(self) -> None:
        self.write_state(mergeable="UNKNOWN")
        result = self.merge(env={"AFT_MERGE_PR_MERGEABLE_TIMEOUT_SECONDS": "0"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("has not computed mergeability for PR #7", result.stderr)
        self.assertEqual(self.merges(), [])

    def test_queue_entry_on_first_check_needs_no_extra_polling(self) -> None:
        self.write_state(queue_on_merge=True)
        # An extra poll would sleep 30s; the read count below catches it either way.
        result = self.merge(env={"AFT_MERGE_PR_QUEUE_CONFIRM_POLL_SECONDS": "30",
                                 "AFT_MERGE_PR_MERGEABLE_POLL_SECONDS": "30"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("confirmed #7 with GitHub", result.stderr)
        merge_index = self.merge_indexes()
        self.assertEqual(len(merge_index), 1)
        reads = self.queue_state_reads()
        self.assertEqual(len([i for i in reads if i < merge_index[0]]), 1)
        self.assertEqual(len([i for i in reads if i > merge_index[0]]), 1)

    def test_pending_checks_accept_armed_auto_merge_before_queue_entry(self) -> None:
        # With checks still running GitHub only arms auto-merge; the queue entry
        # follows once they pass, so the armed request is the confirmation.
        self.write_state(checks_pending=True, auto_merge=True, ghost_enqueue=True)
        result = self.merge(env={"AFT_MERGE_PR_QUEUE_CONFIRM_TIMEOUT_SECONDS": "0"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("confirmed #7 with GitHub", result.stderr)
        self.assertEqual(len(self.merges()), 1)
        state = json.loads(self.state_path.read_text())
        self.assertTrue(state["auto_merge_armed"])
        # One read at admission and a fresh one at confirmation time.
        self.assertEqual(state["check_polls"], 2)

    def test_checks_finished_before_confirmation_require_a_real_queue_entry(self) -> None:
        # Checks are pending at admission but pass before confirmation; the armed
        # request then has nothing left to wait on, so only a queue entry counts.
        self.write_state(checks_pending=True, auto_merge=True, ghost_enqueue=True, finish_after_polls=1)
        # Whole-second deadline arithmetic: 2s guarantees at least two reads.
        result = self.merge(env={"AFT_MERGE_PR_QUEUE_CONFIRM_TIMEOUT_SECONDS": "2",
                                 "AFT_MERGE_PR_QUEUE_CONFIRM_POLL_SECONDS": "0.5"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("required checks still running for #7", result.stderr)
        self.assertIn("PR #7 was submitted but GitHub never listed it in the merge queue", result.stderr)
        self.assertEqual(len(self.merges()), 1)
        self.assertGreater(len([i for i in self.queue_state_reads() if i > self.merge_indexes()[0]]), 1)
        state = json.loads(self.state_path.read_text())
        self.assertFalse(state["auto_merge_armed"])
        self.assertEqual(state.get("disabled_auto_merge"), 1)

    def thread_reads(self) -> list[int]:
        return [i for i, call in enumerate(self.calls()) if "reviewThreads" in " ".join(call["args"])]

    def test_thread_opened_during_mergeability_wait_refuses_before_submission(self) -> None:
        # The first mergeability read is UNKNOWN; while the script waits for it,
        # a reviewer opens a thread on the same head.
        self.write_state(mergeable_unknown_reads=1, unresolved_after_mergeable_reads=1)
        result = self.merge()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("waiting for it to be computed", result.stderr)
        self.assertIn("1 unresolved review thread(s)", result.stderr)
        self.assertEqual(self.merges(), [])
        first_mergeable_read = self.queue_state_reads()[0]
        self.assertTrue(any(i > first_mergeable_read for i in self.thread_reads()))

    def test_thread_opened_before_resubmission_refuses_without_resubmitting(self) -> None:
        # The first submission never enters the queue with mergeability UNKNOWN,
        # which normally earns one resubmission; a thread opened after the first
        # submission must stop it.
        self.write_state(ghost_enqueue=True, ghost_mergeable="UNKNOWN", unresolved_after_merge_calls=1)
        result = self.merge(env={"AFT_MERGE_PR_QUEUE_CONFIRM_TIMEOUT_SECONDS": "0"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("resubmitting once", result.stderr)
        self.assertIn("1 unresolved review thread(s)", result.stderr)
        self.assertEqual(len(self.merges()), 1)
        self.assertTrue(any(i > self.merge_indexes()[0] for i in self.thread_reads()))
        state = json.loads(self.state_path.read_text())
        self.assertFalse(state["auto_merge_armed"])


class DeliveryTests(PRFixture):
    def test_fake_gh_refuses_pr_text_edits_with_manifest_error(self) -> None:
        expected = ('gh-shim: gh_shim_unclassified: verb "pr edit" has no bot-speech route in manifest 18; '
                    'under GH_SHIM_BYPASS=operator gh pr edit admits only label changes')
        for flag in ("--title", "--body"):
            with self.subTest(flag=flag):
                result = subprocess.run(["gh", "pr", "edit", "7", flag, "not allowed"],
                                        cwd=self.repo, env=self.env, text=True, capture_output=True)
                self.assertEqual(result.returncode, 86)
                self.assertEqual(result.stderr.strip(), expected)

    def assert_status_refused(self, values: dict, refusal: str, env: dict | None = None) -> None:
        state = {"rung": "R3", "ambient_credentials": False, "wrong_gh_image": False}
        state.update(values)
        self.write_state(**state)
        result = self.run_script("land-pr.sh", "delivery", env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(refusal, result.stderr)
        self.assertFalse(any(call["args"][:2] == ["pr", "create"] for call in self.calls()))

    def test_governed_gh_status_requires_r3(self) -> None:
        self.assert_status_refused({"rung": "R2"}, "last_rung R3")

    def test_governed_gh_status_rejects_ambient_credentials(self) -> None:
        self.assert_status_refused({"ambient_credentials": True}, "ambient credentials")

    def test_governed_gh_status_must_match_executing_shim(self) -> None:
        self.assert_status_refused({"wrong_gh_image": True}, "does not resolve to the governed gh shim")

    def test_path_must_resolve_to_the_managed_shim_directory(self) -> None:
        other_dir = Path(self.temp.name) / "other-shims"
        other_dir.mkdir()
        image = other_dir / "gh"
        image.write_text("not executed")
        self.assert_status_refused(
            {"status_image": str(image)},
            "does not resolve to the governed gh shim",
            env={"AFT_GH_SHIMS_DIR": str(other_dir)},
        )

    def test_prefixed_shim_coauthor_line_allows_landing(self) -> None:
        result = self.run_script("land-pr.sh", "delivery")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_empty_unbound_shim_coauthor_uses_hook_identity(self) -> None:
        self.write_state(unbound=True)
        result = self.run_script(
            "land-pr.sh", "delivery",
            env={"AFT_GIT_CO_AUTHOR": "alfonso-aft <289616620+alfonso-aft@users.noreply.github.com>"},
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_delivery_is_idempotent_and_reports_final_merge_oid(self) -> None:
        result = self.run_script("land-pr.sh", "delivery", env={"GH_SHIM_BYPASS": "operator"})
        self.assertEqual(result.returncode, 0, result.stderr)
        final = result.stdout.strip()
        self.assertEqual(len(final), 40)
        self.assertNotEqual(final, self.git("rev-parse", "HEAD"))
        again = self.run_script("land-pr.sh", "delivery")
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertEqual(again.stdout.strip(), final)
        created = [call for call in self.calls() if call["args"][:2] == ["pr", "create"]]
        self.assertEqual(len(created), 1)
        self.assertEqual(created[0]["bypass"], "")
        # The design gate exempts the AFT bot by identity, so landing never
        # labels its own PR as an exception to design approval.
        self.assertEqual(json.loads(self.state_path.read_text()).get("labels", []), [])
        self.assertFalse([call for call in self.calls() if call["args"][:2] == ["pr", "edit"]])
        call_args = [call["args"] for call in self.calls()]
        create_index = next(i for i, args in enumerate(call_args) if args[:2] == ["pr", "create"])
        merge_index = next(i for i, args in enumerate(call_args) if args[:2] == ["pr", "merge"])
        self.assertLess(create_index, merge_index)
        self.assertTrue((self.repo / "benchmarks/aft-search/slice-descriptors/land-delivery.json").exists())

    def test_new_thread_after_auto_merge_arming_disables_and_dequeues(self) -> None:
        head = self.prepare_existing_pr_head()
        self.write_state(exists=True, admitted=True, remove=True, queued=True,
                         auto_merge_armed=True, raw_entry_head=True, unresolved_after_admission=True)
        result = self.run_script("land-pr.sh", "delivery")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unresolved review threads", result.stderr)
        self.assertNotIn("merge-pr: REFUSED", result.stderr)
        state = json.loads(self.state_path.read_text())
        self.assertEqual(state.get("disabled_auto_merge"), 1)
        self.assertEqual(state.get("dequeued"), 1)
        self.assertFalse(state["auto_merge_armed"])
        self.assertFalse(state["queued"])
        self.assertEqual(head, self.git("rev-parse", "HEAD"))

    def test_queued_null_admission_fields_do_not_refuse(self) -> None:
        self.prepare_existing_pr_head()
        self.write_state(exists=True, null_admission_then_merge=True)
        result = self.run_script("land-pr.sh", "delivery")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "a" * 40)
        self.assertNotIn("REFUSED", result.stderr)

    def test_landing_helper_finds_unresolved_thread_on_second_graphql_page(self) -> None:
        self.write_state(thread_page_two=True)
        script = ("import sys; sys.path.insert(0, 'scripts/lib'); import land_pr; "
                  "print(land_pr.unresolved_review_threads('example/repo', 7))")
        result = subprocess.run([sys.executable, "-c", script], cwd=self.repo, env=self.env,
                                text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "1")
        self.assertTrue(any("after=cursor-1" in " ".join(call["args"]) for call in self.calls()))

    def test_queue_removal_is_detected_from_membership_without_entry_or_timestamp(self) -> None:
        self.write_state(group="a" * 40)
        script = r'''import sys, time
from pathlib import Path
sys.path.insert(0, 'scripts/lib')
import land_pr
head = 'a' * 40
base = {'state': 'OPEN', 'headRefOid': head, 'baseRefName': 'main',
        'autoMergeRequest': None, 'mergeQueueEntry': None, 'mergeCommit': None,
        'timelineItems': {'nodes': []}}
snapshots = [{**base, 'isInMergeQueue': True}, {**base, 'isInMergeQueue': False}]
def next_snapshot(repo, number):
    return snapshots.pop(0) if snapshots else {**base, 'isInMergeQueue': False}
land_pr.snapshot = next_snapshot
state = {'pr': 7, 'head': head, 'groups': [], 'seen_queued': False, 'removed': False}
try:
    land_pr.wait_merged('example/repo', state, Path('land-state.json'), time.monotonic() + 1, 0)
except ValueError as error:
    print(error)
    sys.exit(0 if str(error).startswith('PR #7 removed from queue:') else 1)
sys.exit(2)
'''
        result = subprocess.run([sys.executable, "-c", script], cwd=self.repo, env=self.env,
                                text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertIn("removed from queue", result.stdout)
        self.assertIn("exact failing group/run could not be established", result.stderr)

    def test_landing_refuses_when_submission_never_enters_queue(self) -> None:
        self.write_state(ghost_enqueue=True)
        result = self.run_script("land-pr.sh", "delivery",
                                 env={"AFT_MERGE_PR_QUEUE_CONFIRM_TIMEOUT_SECONDS": "0"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("GitHub never listed it in the merge queue", result.stderr)
        self.assertIn("land-pr: REFUSED", result.stderr)
        self.assertNotIn("timed out waiting", result.stderr)

    def test_existing_open_pr_is_reused_and_text_stays_unchanged(self) -> None:
        self.write_state(exists=True)
        result = self.run_script("land-pr.sh", "delivery", "--title", "accepted revision")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(call["args"][:2] == ["pr", "create"] for call in self.calls()))
        self.assertFalse(any(call["args"][:2] == ["pr", "edit"] and
                             ("--title" in call["args"] or "--body" in call["args"])
                             for call in self.calls()))
        self.assertIn("leaving its current title and body unchanged", result.stderr)

    def test_new_head_push_disarms_armed_auto_merge_and_dequeues(self) -> None:
        self.write_state(exists=True, auto_merge_armed=True, queued=True)
        result = self.run_script("land-pr.sh", "delivery")
        self.assertEqual(result.returncode, 0, result.stderr)
        state = json.loads(self.state_path.read_text())
        self.assertEqual(state["disabled_auto_merge"], 1)
        self.assertEqual(state["dequeued"], 1)
        self.assertFalse(state["auto_merge_armed"])
        self.assertFalse(state["queued"])
        calls = [call["args"] for call in self.calls()]
        disable_index = next(i for i, args in enumerate(calls) if "disablePullRequestAutoMerge" in " ".join(args))
        merge_index = next(i for i, args in enumerate(calls) if args[:2] == ["pr", "merge"])
        self.assertLess(disable_index, merge_index)

    def test_pr_closed_unmerged_at_another_head_is_superseded_by_a_new_pr(self) -> None:
        self.write_state(abandoned=True)
        result = self.run_script("land-pr.sh", "delivery", "--title", "second attempt")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("already closed or merged", result.stderr)
        self.assertTrue(any(call["args"][:2] == ["pr", "create"] for call in self.calls()))

    def test_new_pr_receives_title_and_body_at_creation(self) -> None:
        body_file = Path(self.temp.name) / "body.md"
        body_file.write_text("first body")
        result = self.run_script("land-pr.sh", "delivery", "--title", "first title",
                                 "--body-file", str(body_file))
        self.assertEqual(result.returncode, 0, result.stderr)
        create = next(call for call in self.calls() if call["args"][:2] == ["pr", "create"])
        self.assertEqual(create["args"][create["args"].index("--title") + 1], "first title")
        self.assertEqual(create["args"][create["args"].index("--body") + 1], "first body")
        self.assertFalse(any(call["args"][:2] == ["pr", "edit"] and
                             ("--title" in call["args"] or "--body" in call["args"])
                             for call in self.calls()))

    def test_delivery_remote_head_change_refuses_stale_admission(self) -> None:
        self.write_state(wrong_head=True)
        result = self.run_script("land-pr.sh", "delivery")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("remote PR head does not match", result.stderr)
        self.assertEqual(self.merges(), [])

    def test_pr_with_non_app_author_is_refused_before_label_or_queue_admission(self) -> None:
        self.write_state(author={"login": "operator", "type": "User", "id": 1})
        result = self.run_script("land-pr.sh", "delivery")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("author is not the aft-alfonso app", result.stderr)
        self.assertFalse(any(call["args"][:2] == ["pr", "edit"] for call in self.calls()))
        self.assertEqual(self.merges(), [])

    def test_unattributed_delivery_is_refused_before_push_or_pr_creation(self) -> None:
        self.git("commit", "--allow-empty", "-qm", "manual without attribution")
        result = self.run_script("land-pr.sh", "delivery")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing alfonso-aft", result.stderr)
        self.assertFalse(any(call["args"][:2] == ["pr", "create"] for call in self.calls()))

    def test_queue_removal_reports_exact_run_and_failing_job(self) -> None:
        self.write_state(remove=True)
        result = self.run_script("land-pr.sh", "delivery")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("actions/runs/42", result.stderr)
        self.assertIn("Unit / Unit tests (Linux): failure", result.stderr)

    def test_removal_finds_group_from_artifact_when_entry_head_is_original(self) -> None:
        self.write_state(remove=True, raw_entry_head=True)
        result = self.run_script("land-pr.sh", "delivery")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("actions/runs/42", result.stderr)
        self.assertIn("Unit / Unit tests (Linux): failure", result.stderr)

    def test_wrong_group_context_is_not_attributed_to_removed_pr(self) -> None:
        self.write_state(remove=True, raw_entry_head=True, wrong_context=True)
        result = self.run_script("land-pr.sh", "delivery")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("actions/runs/42", result.stderr)
        self.assertIn("exact failing group/run could not be established", result.stderr)

    def test_stale_failed_group_is_not_attributed_to_new_admission(self) -> None:
        self.write_state(remove=True, stale_run=True)
        result = self.run_script("land-pr.sh", "delivery")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("actions/runs/42", result.stderr)
        self.assertIn("no stale run attributed", result.stderr)


class ReleaseQueueTests(PRFixture):
    def prepare(self) -> None:
        self.git("checkout", "-q", "main")
        self.write_state(branch="land/release-1.0.1")
        helper = self.repo / "scripts/lib/release_queue.py"
        subprocess.run([sys.executable, str(helper), "start", "1.0.1"], cwd=self.repo,
                       env=self.env, check=True, capture_output=True)
        for package in ("aft-bridge", "opencode-plugin", "pi-plugin"):
            (self.repo / "packages" / package / "package.json").write_text(json.dumps({"version": "1.0.1"}))
        self.git("add", "packages")
        self.git("commit", "-qm", "release: v1.0.1\n\n" + COAUTHOR)
        subprocess.run([sys.executable, str(helper), "checkpoint", "1.0.1"], cwd=self.repo,
                       env=self.env, check=True, capture_output=True)

    def test_release_tags_final_merge_oid_and_resumes_same_pr(self) -> None:
        self.prepare()
        source = self.git("rev-parse", "HEAD")
        result = self.run_script("release.sh", "1.0.1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        final = self.git("rev-list", "-n", "1", "v1.0.1")
        self.assertNotEqual(final, source)
        self.assertEqual(final, self.git("rev-parse", "origin/main"))
        publication = [call for call in self.calls() if call["args"][0] == "node"]
        self.assertEqual(len(publication), 2)
        self.assertTrue(all(call["source_oid"] == final for call in publication))
        self.assertIn("--prepare", publication[0]["args"])
        again = self.run_script("release.sh", "1.0.1")
        self.assertEqual(again.returncode, 0, again.stdout + again.stderr)
        self.assertEqual(len([call for call in self.calls() if call["args"][:2] == ["pr", "create"]]), 1)

    def test_release_refuses_required_job_failure_before_tagging(self) -> None:
        self.prepare()
        self.write_state(failed_required=True)
        result = self.run_script("release.sh", "1.0.1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Landing policy", result.stderr)
        exists = subprocess.run(["git", "show-ref", "--verify", "--quiet", "refs/tags/v1.0.1"], cwd=self.repo)
        self.assertNotEqual(exists.returncode, 0)


if __name__ == "__main__":
    unittest.main()
