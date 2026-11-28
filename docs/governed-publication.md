# Governed publication records

## Source policy versus release records

Normal trains commit source and reviewed policy, not branch-dependent publication
records. `node scripts/release-gate-v049.mjs --check-inputs` generates a disposable
snapshot, validates it, stages and validates all six activation profiles, then
deletes it. It runs no native build and never updates the index or creates a
commit. `scripts/align-governed-docs.sh` is now a read-only compatibility alias.

The publication process recomputes prefix captures, source-byte manifests and
activation evidence in CI, and freezes those records at release time:

- Merely checking committed generated copies in CI would retain the conflicts.
  Recomputing disposable copies validates the actual merged source instead.
- Generating records only at release would defer source and activation errors.
  The same source-only check runs before `train-push.sh` pushes a train branch
  and in the unit CI workflow.
- Per-entry committed hashes would help individual source rows, but the complete
  prefix captures, commit identity and activation snapshots still depend on the
  whole candidate. Splitting those records would not remove cross-train churn.

`docs/v0.49-release-manifest.json` is now checked release policy. It no longer has
a moving `source_commit`. The three checked prefix/surface/evidence documents
remain historical examples, not live release inputs; do not refresh them for a
train. The source inventory, distribution inventory and legacy-vocabulary
allowlist remain reviewed inputs. In particular, preflight never regenerates
the allowlist to bless a new occurrence automatically. The generated embedded
subc schema remains a source artifact: `--check-source-only` verifies all three
JS-generated schema/preset files without invoking Cargo. The unit CI workflow
also runs the full generator's `--check` after its existing debug build, including
the byte comparison for the Rust-generated hashline schema. Release preparation
and saved-snapshot validation keep the same full native schema check. Moving that
native comparison out of local train preflight does not remove it from the
  automatic CI gate that must pass before a train can land.

This removes generated-file conflicts, not the exact-SHA CI or fast-forward
landing requirements. Combining divergent source branches can still require
CI for their new merged or rebased SHA; no publication-document regeneration or
alignment commit is involved.

`release.sh` checks inputs before its slow suites, then prepares and validates a
snapshot of the final committed version bump before tagging. The publish workflow
independently prepares a snapshot from the tagged source, validates that **saved
snapshot without regenerating it**, checks all platform binaries, and retains
the four JSON records as a GitHub Actions artifact before publication. No manual
alignment step or generated commit is required. An existing snapshot directory
cannot be silently prepared again.

```sh
node scripts/release-gate-v049.mjs --prepare --snapshot-dir /tmp/release-records --candidate
node scripts/release-gate-v049.mjs --snapshot-dir /tmp/release-records --candidate --require-platform-artifacts
```

Both generation and validation require the `v0.48.1` activation baseline tag,
installed workspace dependencies and the built bridge. The generation label
`0.49.0` still identifies surface lineage; package versions are checked against
the current Cargo workspace version. `--build` remains an explicit release-only
packaging option; neither `--stage` nor `--check-inputs` invokes it. Combining
`--check-inputs` with `--build` is rejected.

## Publication outcomes and crates.io retries

On a release tag, crates.io publication, the GitHub release and npm publication
all require the exact-commit CI gate, every release build and every shipped E2E
job to succeed. npm also waits for the GitHub release. GitHub and npm do not wait
for crates.io: a crates.io refusal or outage leaves the Actions run red, with
`Publish to crates.io` failed, but does not stop the other publications.

`scripts/release.sh` stops after tagging; it does not wait for publication.
Use `scripts/wait-release.sh <tag>` to monitor the release run. The waiter does
not cancel the other publications when crates.io fails. Once the run finishes,
it reports when GitHub and npm succeeded but crates.io failed, and still exits
1. Other blocking job failures retain fail-fast cancellation. Check the named
jobs before treating a red run as a complete release failure.

The GitHub release includes `release-manifest.json` alongside the `ck-aft` ZIPs
and checksum sidecars. Its `requires_core` is the oldest `ck-subc` version this
AFT release can register with. To change the floor for future releases, edit
`REQUIRES_CORE` in `scripts/release-installer-assets.py` before tagging. The
release job checks the ZIP inventory, digests, archive contents and manifest
before uploading them to the draft. The manifest records the tested binary's
version output for `ck setup` to check after installation.

After the GitHub release exists, retry crates.io without rebuilding, rerunning
E2E or republishing GitHub/npm:

```sh
gh workflow run crates-retry.yml --ref main -f tag=vX.Y.Z
gh run list --workflow crates-retry.yml --limit 5
gh run watch <retry-run-id> --exit-status
```

Alternatively, select **Retry crates.io publication** in GitHub Actions, choose
the `main` branch and enter the existing release tag. The workflow must already
be available on the default branch. It refuses absent or draft GitHub releases,
checks out `refs/tags/<tag>` as the publication source, syncs the manifests to
that tag's version, and runs only the crates.io publish job. Retry tooling comes
from the selected workflow revision so tags older than the retry helper work.
The shared publisher retains the leaf-crate ordering, sparse-index wait and
`already exists` skips, so a partial publication can be retried safely. It does
not create or move a tag, change the released source or repair the original
run's red conclusion. If crates.io still refuses the unchanged package (for
example, its size limit), a retry will remain red until that refusal is resolved.

## Field ownership and checks

Logical `docs/...` record paths in policy and artifact rows are resolved to the
corresponding basename inside the snapshot directory. Source paths still refer
to the repository's exact bytes, not normalized or reserialized content.

| Record / fields | Purpose and automatic reader/check |
| --- | --- |
| Prefix: `artifact_id`, `artifact_version`, `capture_scope` | Identify the capture and plugin-owned boundary; descriptive lineage metadata emitted by the audit. Host-owned base prompts are not claimed. |
| Prefix: top-level and per-capture `source_commit` | Identify the audited candidate. The surface manifest binds the complete capture bytes; the release/source manifest commits must agree. |
| Prefix: `captures[].capture_id`, `profile_id`, `harness`, `capture_method` | Identify six production registration captures: minimal, recommended and all-tools profiles for each of OpenCode and Pi, along with their instrumentation method. The audit emits these registration maps. |
| Prefix: `host_version.{value,method}`, `production_cache_key.{value,exposed,source}` | Environment observations, not a release version requirement or proof of cache invalidation. Host version queries are bounded and shared across each host's profiles. |
| Prefix: `prefix_input.{registered_tool_names,tools,workflow_hints}` | Complete production registration maps and workflow-hint inputs. The audit checks emitted and captured tool maps for prohibited vocabulary, with the existing host-display exemption. `verifyManifest` checks the complete saved capture's byte length/hash. |
| Surface: `$schema`, `artifact_id`, `artifact_version`, `manifest_id`, `hash_rule`, `source_inventory` | Define lineage, exact-byte hashing and the reviewed source inventory. The release gate checks generation version and consumes that inventory for governed paths and profile membership. |
| Surface: top-level / artifact `source_commit` | One candidate identity. Audit verification requires every row to share it; the release gate requires an existing ancestor of HEAD shared with the release record. |
| Surface: `artifacts[].{id,path,kind,owner,profiles}` | Source ownership/profile mapping plus inventory, allowlist, audit implementation and generated prefix capture rows. The audit verifies every row's current bytes; the release gate requires every governed inventory path to have a row and uses those rows for profile snapshots. |
| Surface: `artifacts[].{encoding,byte_length,sha256}` | Exact UTF-8 source or capture bytes. Audit verification checks size/hash for all rows. `checkGovernedBytes` independently checks governed source rows against both current bytes and Git bytes at the source commit. |
| Release policy: `$schema`, IDs, generation version, links, `hash_rule`, `candidate` | Stable source/distribution/version rules. The release gate checks manifest generation, source-manifest commit agreement, candidate package/Cargo/tokenizer versions and source bytes. The prepared release record adds `source_commit`; saved policy must equal the checked policy and its source-commit bytes. |
| Release policy: `independently_versioned_artifacts[]` IDs/kinds/names/sources/versions/builds/activation/verification-only | Artifact/build inventory. `checkDistributionInventory` checks S0 declarations and versions; `checkCandidateVersions` checks current packages. Explicit `--build` checks generated subc, builds npm artifacts, dry-run packs them and builds the native release. Descriptive activation fields are not independent runtime measurements. |
| Release policy: `platform_binary_artifacts[]` IDs/names/directories/versions/assets | Exact six-platform declaration coverage; publish validation checks package versions and binary presence/nonempty bytes. This is not a new claim of binary digest or execution validation. |
| Release policy: `profiles[]` IDs/harness/previous and candidate artifacts/activation | Required profile coverage and staging configuration. Evidence validation requires exactly the declared profiles and recomputes their expected old/candidate snapshots. |
| Release policy: `gates`, `atomicity`, `cache_observability`, campaign entries | Document commands and existing publication limitations. Campaign-entry obligations remain asserted by `packages/pi-plugin/src/__tests__/inspect.test.ts`. Atomicity is per artifact/profile, not across registries or end-user installs. |
| Evidence: IDs/generation, `source_commit`, `previous_version`, `candidate_version`, `capture_rule` | Identify activation staging. `validateEvidence` checks source-commit agreement; expected snapshots use the previous release tag and candidate surface rows. Version/description fields are emitted metadata, not independent host observations. |
| Evidence: `transitions[].{profile_id,harness,activation,staging}` | Profile coverage and disposable staging mechanism/root/atomic-replacement metadata. The existing harness writes old/candidate artifact descriptors; it does not measure live host installation. |
| Evidence: `steps[].{state,artifact_version,agent_prefix,generated_subc}` | Complete old/candidate snapshots (`complete`, byte length, SHA-256). `validateTransition` requires two old-then-candidate observations, complete captures and matching independently recomputed prefix/schema hashes. As before, snapshot byte lengths are recorded; the transition validator compares hashes. |
| Evidence: `transition_count`, `mixed_surface`, `cache` | Require one transition, no mixed surface, and exactly one invalidation only when a production cache key is observable. Otherwise an invalidation count is prohibited. |
| Evidence: top-level `atomicity`, `cache_observability` | Copies of the policy's limitations, not a claim of globally atomic release or production cache measurement. |
| Allowlist: IDs/generation/vocabulary and occurrence entries (path/location/line/column/token/class/reason/text/ordinal) | Content-keyed reviewed vocabulary exceptions. The audit rejects missing or stale occurrence pins and independently audits emitted production surfaces. This is an authored input now, not an automatic train output. |

## Executable readers and regression coverage

- `audit-v049-agent-surface.ts`: inventories sources, validates the reviewed
  allowlist and emitted maps, writes/validates prefix and surface records only
  with an explicit snapshot directory.
- `release-gate-v049.mjs`: source bytes/ancestry, policy/distribution and versions,
  generated schema, steering, activation evidence, explicit packaging and
  publish-time platforms. Saved-snapshot validation never repairs stale data.
- `scripts/train-push.sh`, `scripts/gated-push.sh`,
  `scripts/align-governed-docs.sh` and `.github/workflows/_unit-suite.yml`:
  disposable `--check-inputs`. Restack can drop legacy generated alignment
  commits but never replaces them with fresh generated commits.
- `release.sh` and `release.yml`: automatic preparation followed by validation of
  the frozen release snapshot. The workflow also checks platform binaries.
- Pi inspect tests read the stable release policy; OpenCode registration-parity
  tests read the stable distribution inventory. The allowlist's standalone
  tests exercise content-keyed occurrence drift.
- `governed-publication.test.ts` creates two branches from one fixture
  `origin/main`, changes different governed source files, checks both without
  Git mutations, lands the first and merges it into the second, then checks the
  merged candidate without regenerating any committed file. It also deliberately
  tampers with a saved digest and edits source after capture to prove the release
  gate refuses both. `train-push.test.sh` defends legacy-alignment restacking and
  preflight refusal behavior.
- Release-record tests skip the unrelated native schema generator explicitly;
  the source-only train test uses a failing Cargo sentinel to prove there is no
  native compilation. The mandatory CI/release commands do not use that skip.
