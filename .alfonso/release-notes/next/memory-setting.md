### Machine-local memory profiles

- Added the user-only `memory` setting: `lean` uses less memory and can build more slowly; `balanced` keeps the previous resource limits. When unset, AFT selects `lean` below 8 GiB of total physical RAM and `balanced` otherwise, including when RAM cannot be read. Health output reports its choice and why.
- `lean` uses a smaller shared block cache, four call-graph resolver threads on Linux and macOS, and immediate return of freed memory on Linux. Windows keeps all CPUs. Other limits, build caches, and meaning-search settings stay unchanged.
- Profile changes apply to subsequent builds and shared block cache sizing without restarting the daemon.
- Removed the unused `callgraph_chunk_size` key for 0.67. Run `npx @cortexkit/aft doctor --fix` to remove it from user configuration.
