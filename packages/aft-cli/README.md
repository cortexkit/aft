# @cortexkit/aft

Unified CLI for [Agent File Tools (AFT)](https://github.com/cortexkit/aft) — setup, doctor, and diagnostics across supported agent harnesses.

## Usage

```bash
npx @cortexkit/aft setup            # interactive setup for one or more harnesses
npx @cortexkit/aft doctor           # check and fix configuration issues
npx @cortexkit/aft doctor --force   # force clear plugin cache
npx @cortexkit/aft doctor --issue   # collect diagnostics and open a GitHub issue
```

By default the CLI auto-detects which harnesses are installed on your system (OpenCode, Pi). When multiple are detected, it prompts you to choose. Use `--harness opencode` or `--harness pi` to target one explicitly.

## Supported harnesses

- **OpenCode** — `@cortexkit/aft-opencode` plugin
- **Pi** — `@cortexkit/aft-pi` extension

## Learn more

- Main repository: <https://github.com/cortexkit/aft>
- Issues: <https://github.com/cortexkit/aft/issues>
