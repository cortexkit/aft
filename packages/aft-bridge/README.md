# @cortexkit/aft-bridge

Shared NDJSON transport, binary resolution, and ONNX runtime helpers used by AFT
agent-host plugins (OpenCode, Pi, and future MCP-based hosts).

This package is **not** intended for direct end-user consumption — it powers
[`@cortexkit/aft-opencode`](https://www.npmjs.com/package/@cortexkit/aft-opencode)
and [`@cortexkit/aft-pi`](https://www.npmjs.com/package/@cortexkit/aft-pi).

## What it owns

- **Transport** — `BinaryBridge` (one persistent `aft` child process), `BridgePool`
  (one bridge per canonical project root), wire envelope and push-frame types.
- **Binary resolution** — versioned cache, npm platform package lookup, PATH /
  cargo fallback, GitHub release download with SHA-256 verification.
- **ONNX runtime** — auto-download for supported targets, install detection and
  version probing, manual-install hints for unsupported platforms.

## What it does not own

Host-specific behaviour stays in the plugin packages:

- Configuration loading (each host has its own conventions and config paths).
- Permission UX (`ctx.ask()` and prompt rendering belong to the host SDK).
- Tool registration and rendering.
- Notifications, slash commands, and TUI integration.
- Session-ID lookup and per-request session injection.

## Versioning

Released in lock-step with `@cortexkit/aft-opencode` and `@cortexkit/aft-pi`.
The bridge protocol is versioned end-to-end: a plugin running an older bridge
package will detect a newer `aft` binary and refuse to talk to it.
