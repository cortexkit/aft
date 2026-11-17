### Machine-local memory profiles

- Added the user-only `memory` setting: `lean` uses less memory and builds more slowly, `balanced` keeps the previous resource limits, and `fast` keeps more data in memory to avoid reloads. When unset, AFT selects a profile from total physical RAM and reports its choice in health output.
- Profile changes apply to subsequent builds, cache sizing, and admissions without restarting the daemon. Meaning search remains controlled by its own setting.
- Removed the unused `callgraph_chunk_size` key for 0.67. Run `npx @cortexkit/aft doctor --fix` to remove it from user configuration.
