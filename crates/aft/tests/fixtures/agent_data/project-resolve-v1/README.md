# Served project resolution v1

Vendored from `cortexkit/entorhinal`, `test-vectors/project-resolve-v1`, at
commit `bdb409e` (2026-10-25). The JSON files and `SHA256SUMS` are unchanged.

These vectors capture entorhinal's real `ProjectsHandler` replies for project path resolution, durable project-id lookup, owned GitHub remote lookup, and enumeration. The fixture uses fixed project/workspace ids, clock `700`, and module incarnation `0123456789abcdef`. It creates checkout and linked-worktree metadata under a scratch root; the linked worktree uses the `.git` file, gitdir, and `commondir` layout used by entorhinal's binding tests.

Every path that a reply can name is explicitly mapped to a complete `/fixture/...` spelling so neither the host's temporary-directory spelling nor Windows separators affect the checked-in bytes. Registered paths are canonicalized first, including macOS's `/var` to `/private/var` resolution. The implicit ids are re-derived from those normalized canonical paths. The registered root's generated registration epoch is normalized to `00000000000000000000000000000001`; its checkout incarnation token is seeded with the fixed 64-hex value recorded in the reply. The remote-lookup fixture seeds the same token, although its replies do not expose it. The vector replies otherwise preserve the handler's exact UTF-8 response bytes, including the `result` envelope and key order.

## Route

The management surface sends these project operations to module `entorhinal` (`docs/module-surface.md`, **Projects**). The request is the full `{method, params}` envelope, with `method` set to `resolve`, `resolve_project_id`, `resolve_remote`, or `enumerate`. Successful handler replies have a `result` envelope; refusals have `code`, `message`, and `detail`.

The `resolve` request parameter `canonicalRoot` is named for its usual input, a project's canonical root path, but accepts **any absolute path**, including paths that are not registered roots or repository roots. The reply also has a `canonicalRoot` field, which holds the canonical form of the queried path.

## Admission

These reads are open to every principal, including flow-scoped routes. `ProjectsHandler::admit_with` in `crates/entorhinal-module/src/main.rs` applies the general permission, flow-write, and write-authorization checks before dispatch. The permission gate passes unlisted query methods, and flow-scoped routes refuse mutations rather than these reads.

## Versions

The original `resolve.json`, `resolve-project-id.json`, and `enumerate.json` capture replies from entorhinal module version **0.1.28**. The additional `resolve-root-records.json` and `resolve-remote.json` replies were captured from **0.1.46**; regenerating with that version also preserves the original files byte for byte.

## Files and fields

Each JSON file is an array of records with these fields:

- **`request`**: the complete `{method, params}` request object. `resolve` uses `params.canonicalRoot`; its malformed request omits it. `resolve_project_id` uses `params.projectId`. `resolve_remote` requires `params.owner` and `params.repo` (no aliases); its malformed request uses `{}`. `enumerate` uses empty params `{}`.
- **`principal`**: the reserved module principal that opened the read route, always `{"kind":"reserved","moduleId":"phone"}` in these records.
- **`reply`**: the exact successful response bytes embedded as JSON, or the refusal outcome. Successful responses are `{ "result": ... }`; refusals are `{ "code", "message", "detail" }`. `detail` is `null` when the handler supplied no details. The malformed `resolve` and `resolve_remote` requests are refused with code `invalid_params` for missing fields `canonicalRoot` and `owner`, respectively.

For every successful `resolve`, the result includes `projectId`, `via`, `gone`, `generation`, and the handler-added `incarnation`. `workspaceId`, `workspaceRoot`, and `projectName` are `null` when the path has no corresponding workspace or project metadata. `canonicalRoot` is the canonical queried path. Root-record mode adds `matchedRoot`, `sourceRoot`, `rootRecords`, and `bindingStatus`; fields that do not apply are `null`, and `rootRecords` is an empty array when there are no matching records. A `via: worktree` result omits those root-record fields and includes `worktreeOf`; `worktreeOf` is otherwise absent.

A root record includes `root`, `remotes`, `identity`, `incarnation`, `registrationEpoch`, and `approval`. `incarnation` and `registrationEpoch` are `null` unless the identity is `bound`. Optional `remotesError`, `rootKey`, and `rootKeyMismatch` are omitted when unavailable. `approval.journalSeq` is `null` until approval has a journal entry.

The root-record examples cover `unbound` (registered but not bound), `bound`, `replaced` (the checkout token no longer matches its binding), `retired`, and `unverifiable`. `retired` is served by `resolve` through `via: containment`: a linked worktree's attached container retains an old source binding epoch after the source checkout is replaced and rebound. Registered-root records themselves never use `retired` (`crates/entorhinal-core/src/binding.rs`, `root_record` and `worktree_record`).

Each populated `remotes` entry includes `name`, `owner`, `repo`, and `owned`. The invented GitHub remotes demonstrate the default ownership rule: `origin` is owned, while `upstream` is not. A config containing invalid UTF-8 is unreadable on every supported host, including privileged test runners; its root record has empty `remotes` and a `remotesError` string instead of silently omitting the error.

A successful `resolve_project_id` result always includes `projectId`, `via`, `gone`, `generation`, and the handler-added `incarnation`. The records cover `current`, `alias`, and unknown-id `gone` results.

A successful `resolve_remote` result always includes `status`, `generation`, and the handler-added `incarnation`. `status: found` additionally requires non-null `projectId` and `root`; `status: ambiguous` requires non-null `projectIds`, a sorted list of distinct matching project ids, not roots. Those shape-specific fields are omitted from the other statuses; `status: none` includes neither. Only owned GitHub remotes match (by default `origin`), with ASCII case-insensitive owner/repo comparison. Registration refuses an already owned repository, so the ambiguous example registers different repositories first, then changes one checkout's current config to match the other. A config that becomes unreadable is skipped rather than causing a `remotes_unreadable` refusal or adding an error field; the before/after records show its repository disappearing while another root still matches (`crates/entorhinal-core/src/mutations.rs:668-676`; `crates/entorhinal-core/src/ownership.rs:142-174`).

A successful `enumerate` result includes `workspaces`, `projects`, `generation`, and the handler-added `incarnation`. Workspace entries contain `name`, `root`, and `workspaceId`; `root` is `null` when no workspace root is set. Project entries contain `projectId`, `name`, `roots`, `implicit`, `refKind`, and `rootRecords` in this root-record-enabled fixture. `workspaceId` is omitted for unplaced projects. `deviceFingerprint` and `lastRouteActivityMs` are omitted when unavailable. Empty `roots` and `rootRecords` are represented as arrays.

The implicit path id `pj-implicit1-30f9d5fcd6aa4ef2` demonstrates the derivation: BLAKE3 of the canonical path's UTF-8 bytes, taking the first 8 digest bytes as 16 lowercase hexadecimal characters and prefixing `pj-implicit1-`. The alias input is the corresponding id for `/fixture/alias-source`.

- **`resolve.json`**: exact registered root (`via: root`), its subfolder (`via: walk`), an external linked worktree (`via: worktree`, with `worktreeOf`), an unregistered implicit directory, a removed registered root (`gone: true`), and the missing-`canonicalRoot` refusal.
- **`resolve-root-records.json`**: five additional `resolve` records, in order: `/fixture/remotes-checkout` unbound with two remotes; `/fixture/unreadable-config-checkout` unbound with `remotesError`; `/fixture/remotes-checkout` bound and approved with two remotes; the same checkout replaced with two remotes; and `/fixture/attached-worktrees/retired-worktree` retired through containment, retaining the source's two remotes. A separate file and fixture preserve every byte and generation in the three original files, including all of `resolve.json`.
- **`resolve-project-id.json`**: a current project id, an implicit id retained as an alias after promotion, and an unknown id (`via: gone`, `gone: true`).
- **`resolve-remote.json`**: eight records, in order: found `example-org/widgets`; none for `example-org/missing`; ambiguous `example-org/shared` after a registered checkout's config changes; case-insensitive found `EXAMPLE-ORG/WiDgEtS`; found `example-org/hidden`; none for hidden after its config becomes unreadable; found widgets despite the unreadable root; and malformed params `{}` refused for missing `owner`. A separate fixture preserves every existing file's bytes and generations.
- **`enumerate.json`**: an unfiltered `{}` request returning two projects in one workspace, with names for mapping project ids to dashboard labels.
- **`SHA256SUMS`**: SHA-256 hashes of the five JSON files, with names relative to this directory.

## SHA-256 hashes

| File | SHA-256 |
| --- | --- |
| `enumerate.json` | `89afedb88fa4ddec219f4932df22f26ca8395022c5c1dd399e3e50c9d50b94dd` |
| `resolve-project-id.json` | `4af3d3d6dd6b279146da2e50dd48dd507674f2806f6ac35e8f04b380bc5e901e` |
| `resolve-remote.json` | `e0504720dc0f4a81b532fbe4e0cfa4a43d0fa1b218f703cbc3efca5387e17cba` |
| `resolve-root-records.json` | `5eed8037c03eacca74ed64e7701c849af6e0a394f2743ba31bc7958eeaa5af89` |
| `resolve.json` | `a15d8e09bf4e5ee852097ee423c33e5feca8bff205797c01394d8667a9596bce` |

## Reproduce and verify

From the repository root, regenerate the vector files from the real handler with:

```sh
ENTO_WRITE_SERVED_VECTORS=1 cargo test --workspace --all-targets --locked project_resolve_ -- --nocapture
```

Refresh the `project-resolve-v1` manifest from the repository root with:

```sh
python3 - <<'PY'
import hashlib
from pathlib import Path
root = Path('test-vectors/project-resolve-v1')
names = ['enumerate.json', 'resolve-project-id.json', 'resolve-remote.json', 'resolve-root-records.json', 'resolve.json']
root.joinpath('SHA256SUMS').write_text(''.join(
    f'{hashlib.sha256(root.joinpath(name).read_bytes()).hexdigest()}  {name}\n'
    for name in names), encoding='utf-8')
PY
```

Without `ENTO_WRITE_SERVED_VECTORS=1`, the non-ignored `served_vectors::project_resolve_v1_records_match_published_files` and `served_vectors::project_resolve_remote_v1_records_match_published_file` tests regenerate all five files from entorhinal's real request handling and compare each file byte for byte. Both tests also run under nextest:

```sh
cargo nextest run --workspace --all-targets --locked -E 'test(served_vectors::project_resolve_)'
```
