# Feature-config branch-point evidence (2026-09-23)

These seven artifacts freeze the repository facts and target contracts before implementation. They are data for implementation and tests, not a claim that the current binary already implements the new policy.

| Artifact | Frozen evidence |
| --- | --- |
| `README.md` | Scope, provenance, baseline command and interpretation |
| `registration-inventory.json` | Literal pre-change registration sets for OpenCode V1/V2, Pi and OMP, across 1,536 surface/hoist/gate combinations; canonical names, seven historical aliases and conditional PowerShell/hashline variants |
| `loader-inventory.json` | Consumed paths, source-location migration paths, discovery boundaries and selector-to-existing-resolver mapping |
| `parity-baseline.json` | All pre-change config parity fixture IDs and the measured count |
| `consumer-responses.json` | Shared target-policy projections for each consumer in off, building, unavailable and partially-ready states |
| `catalog-v1.json` | Ordered plan tuples, runtime prerequisites and explicit exclusions |
| `migration-policy.json` | One shared new-policy version and exact retired-path/alias mapping |

## Registration provenance

The pre-change branches are `packages/opencode-plugin/src/tool-registration.ts:127-162`, `packages/opencode-plugin/src/tools/hoisted.ts:1338-1385`, `packages/opencode-plugin/src/tools/search.ts:292-296`, `packages/pi-plugin/src/tool-registration.ts:127-198,244-287`, `packages/pi-plugin/src/tools/bash.ts:711-716` and both plugins' `resolveBashConfig`. The V2 provider transform projects the V1 definitions, while OMP runs the Pi extension (`packages/pi-plugin/src/harness.ts`). Matrix rows use effective boolean bash and background values, not an assumed mapping from `bash` syntax. The explicit default for an absent `bash` differs by surface: minimal off, recommended/all on; an explicit true or object enables it even on Pi minimal. OpenCode minimal has no hoisted branch and so still cannot register bash. Each row is the pre-change set **before** `disabled_tools`; existing filtering deletes names only when present in that row. PowerShell can be host-dependent and is therefore specified as an explicit conditional variant, not silently treated as a canonical slot. URL/web and LSP are capabilities of existing read/outline/zoom/search/inspect tools, not extra registration names. Hashline is an edit schema arm, not an extra tool name.

The new registration target is exactly the 23 literal `canonical_tools` minus resolved disables. Unlike the historical rows, indexes, runtime gates and surface flags must not remove registrations. `aft_glob` is a real historical alias. The seven aliases are input migration names, never new canonical registrations. The `grep`/`glob` and `aft_search` rows can remain registered with missing indexes; consumers report readiness instead.

## Configuration and version provenance

`packages/aft-bridge/src/config-tiers.ts` only reads the shared user and selected-project `.cortexkit/aft.jsonc` files. `packages/aft-bridge/src/paths.ts` additionally lists *location-migration sources*; these are not simultaneously loaded tiers. Read the loader inventory's boundary notes before implementing doctor --fix: it must not sweep inactive legacy paths. OpenCode's worktree/root-sentinel selection and Pi/OMP's cwd-origin selection are not identical to ancestor walking. OpenCode V1 and V2 share the resolver label `opencode`; OMP shares `pi`, but the four setup selectors remain distinct adapter identities.

Package versions in `crates/aft/Cargo.toml` and the four npm package manifests are 0.57.2 at this branch point. The shipping new feature minor is therefore 0.58; the following reject minor is 0.59 (patch-insensitive). The already-retired `gh_read.enabled` and `gh_shim.enabled` aliases are explicitly excluded from this *new* release-policy artifact: they retain their prior v0.57.0 rejection schedule. The old `gh_shim.binary_path` remains. The chair ruling at spec lines 258-259 is applied for those aliases.

## Parity guard

From the repository root, run this **before** snapshot regeneration and after it; it pins the old IDs rather than only checking a self-derived fixture count:

```sh
python3 -c 'import json,pathlib,sys; b=json.loads(pathlib.Path("spec/feature-config/parity-baseline.json").read_text()); root=pathlib.Path("crates/aft/tests/fixtures/config_parity"); ids=b["fixture_ids"]; missing=[i for i in ids if not (root/i/"expected.json").is_file()]; actual=[x for x in root.iterdir() if x.is_dir()]; assert len(ids)==b["count"] and len(set(ids))==len(ids), "invalid pinned parity baseline"; assert not missing, f"missing baseline cases: {missing}"; assert len(actual)>=b["count"], "parity coverage regressed: {} < {}".format(len(actual), b["count"]); print(f"parity baseline: {len(ids)} pinned IDs present, {len(actual)} cases total")'
cargo test -p agent-file-tools --test integration config_resolver_matches_typescript_golden_fixtures
```

The first command checks each pinned ID's `expected.json` and fails even if other new fixture directories replace a deleted baseline directory. Its predicate can be challenged without touching fixtures: changing one copied in-memory ID to `NONEXISTENT_BASELINE_CASE` must yield a missing-baseline assertion. The Rust test currently has only a floor of 52; subsequent implementation should connect this guard to its own CI test or run the documented two-command gate.

## Consumer fixture interpretation

`consumer-responses.json` is a frozen *new policy* contract, not a snapshot of current legacy payloads. Normal matches remain visible when an index is unavailable, while unavailable dead-code findings and callgraph edges are nullable rather than empty. Consumer result `code` is distinct from index `reason`. The existing Rust response envelope is `Response::success/error` (`crates/aft/src/protocol.rs`); implementing slices must decide the envelope mapping once and test actual engine payloads against these projections rather than use each adapter's own invented shape. A partially-ready search lane returns results while disclosing the other lane; callgraph-unready enrichment does not erase those results.
