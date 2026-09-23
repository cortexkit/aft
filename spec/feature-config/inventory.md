# Registration and configuration inventory

`registration-inventory.json` pins 1,536 literal pre-change registration sets: four adapter projections × three surfaces × two hoist choices × six binary gates (`search_index`, `semantic_search`, `backup.enabled`, `inspect.enabled`, effective bash enabled, effective bash background). Its `historical_gate_removals` pins the exact name differences for each adapter/surface/hoist profile. `canonical_tools` is the new-policy tool-name universe; `historical_aliases` contains all seven old prefixed host names, including `aft_glob`. `conditional_variants` records hashline schema selection and Pi/OMP's host-dependent PowerShell slot. OpenCode V2 projects the V1 definitions. Pi and OMP run the same registration function, with distinct host detection. URL/web, GitHub and LSP are capabilities of existing names, not additional tools.

## Historical bash registration gates (literal names)

For `bash:false` or effective `resolveBashConfig(...).enabled:false`, with the other gates on and background true, the old gate removes the following names **relative to bash enabled in the same surface/hoist profile**:

| Adapter | Surface | Hoist true removes | Hoist false removes |
| --- | --- | --- | --- |
| OpenCode V1 | minimal | `[]` | `[]` |
| OpenCode V1 | recommended/all | `["bash","bash_kill","bash_status","bash_watch","bash_write"]` | `["aft_bash","bash_kill","bash_status","bash_watch","bash_write"]` |
| OpenCode V2 | minimal | `[]` | `[]` |
| OpenCode V2 | recommended/all | `["bash","bash_kill","bash_status","bash_watch","bash_write"]` | `["aft_bash","bash_kill","bash_status","bash_watch","bash_write"]` |
| Pi | minimal/recommended/all | `["bash","bash_kill","bash_status","bash_watch","bash_write"]` | `["aft_bash","bash_kill","bash_status","bash_watch","bash_write"]` |
| OMP | minimal/recommended/all | `["bash","bash_kill","bash_status","bash_watch","bash_write"]` | `["aft_bash","bash_kill","bash_status","bash_watch","bash_write"]` |

The historical Pi/OMP host-PowerShell variant also removes `powershell` (hoist true) or `aft_powershell` (hoist false) on a false bash gate **when** the host reports PowerShell enabled or `bash.powershell_tool` fallback is true. These are host-dependent, not canonical seven-host-name slots. When bash remains enabled but `background:false`, historical registration removes exactly `["bash_kill","bash_status","bash_watch","bash_write"]` on a surface that registers bash; with Pi/OMP PowerShell registered it also removes those four companions regardless of whether bash itself registers. A missing top-level `bash` defaults off on minimal and on with background on for recommended/all; explicit `bash:true`/object enables it even on Pi/OMP minimal. OpenCode minimal never enters its hoisted branch. See `packages/opencode-plugin/src/tools/hoisted.ts:1338-1385`, `packages/pi-plugin/src/tool-registration.ts:127-198,244-287`, `packages/pi-plugin/src/tools/bash.ts:711-716` and both `resolveBashConfig` implementations. `backup.enabled:false` removes `["aft_safety"]` on all surfaces; `inspect.enabled:false` removes `["aft_inspect"]` on recommended/all, `[]` on minimal. This is historical behavior to migrate, not the new registration predicate.

## Raw-to-resolved carriers (literal paths)

| Raw input carrier | Resolved/configure carrier | Registration interpretation |
| --- | --- | --- |
| `user aft.jsonc` → `RawAftConfig.disabled_tools: Option<Vec<String>>` / TS `AftConfig.disabled_tools?: string[]` | `Config.disabled_tools: Vec<String>` / TS resolved `disabled_tools: string[]` → configure `config:[{tier:"user",source,doc}]` plus flat resolved fields | Preserve absent versus explicit `[]` through translation; inject `["aft_move","aft_delete"]` only when user base lacks a complete explicit/migrated choice. Resolved `[]` is serialized. |
| `project aft.jsonc` → `RawAftConfig.disabled_tools` / TS `AftConfig.disabled_tools` | merged resolved `Config.disabled_tools` | Project additions union after user/harness; protected host slots and `aft_safety` are ignored and logged. |
| `harnesses.opencode`, `harnesses.pi` (embedded raw tiers) | active Rust `Harness::Opencode` / `Harness::Pi`, then `apply_harness_override` before trust merge | OpenCode V1/V2 select `opencode`; Pi/OMP select `pi`. No raw `harnesses.omp` selection exists in current Rust parser. |
| `search_index`, `experimental_search_index` | `indexes.trigram` | Legacy precedence: canonical leaf, immediate legacy, experimental alias. |
| `semantic_search`, `experimental_semantic_search` | `indexes.semantic` | Backend is runtime readiness, not a registration gate or config readiness carrier. |
| `callgraph_store` | `indexes.callgraph` | Callgraph registration independent of index status. |
| `tool_surface`, `hoist_builtin_tools`, top-level `enabled`, false `backup.enabled`, false `inspect.enabled`, false `bash` | generated `disabled_tools` names before merging | Preserve literal old gate sets from `registration-inventory.json`; explicit canonical list wins within the raw block. Runtime settings remain distinct. |
| `github.enabled`, `gh_read.enabled`, `gh_shim.enabled` | `github.read`, `github.write`, `github.shim` | The `gh_*` aliases were already retired and are not part of the new release-policy artifact; preserve `gh_shim.binary_path`. |

The existing Rust tier carrier is `ConfigTier {tier,source,doc}` and the resolver is `resolve_config_for_harness` (`crates/aft/src/config_resolve.rs:624-677`): parse raw JSONC, apply active harness override and aliases, merge trusted user then restricted project tier, build fresh `Config::default()`, and apply resolved values. `resolve_config_onto_with_diagnostics_for_harness` preserves process state separately (`config_resolve.rs:730-765`), not as raw configuration. The plugins send raw tiers via `buildConfigTierConfigureParams` (`packages/opencode-plugin/src/config.ts:1892`, `packages/pi-plugin/src/config.ts:1879`); do not confuse their flat process-state flags with user config. Loader boundaries and legacy location-migration sources are in `loader-inventory.json`. Actual CLI selector values and their adapter mapping are in `harnesses.json`; the loader inventory's `accepted_setup_harness_ids` labels describe four projections, **not** the current `--harness` accepted syntax.
