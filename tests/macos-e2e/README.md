# macOS native E2E

End-to-end test for the AFT plugin running inside OpenCode on a real macOS
host (no containers, no QEMU). Runs as `e2e-macos` in the reusable
`.github/workflows/_e2e-suite.yml` workflow, alongside the Linux Docker and
Windows native E2E jobs.

## What this catches

- FSEvents watcher behavior (different coalescing latency from Linux inotify;
  the v0.19.5 release fixed two flaky watcher tests with exactly this shape).
- `/var` vs `/private/var` symlink canonicalization in `context.rs`.
- Broken-symlink-chain fallback in `context.rs`.
- macOS dylib loading (`.dylib` extension, `/usr/local/lib` and
  `/opt/homebrew/lib` probe paths) for ONNX Runtime — distinct from the Linux
  `.so` / `/usr/local/lib` path.
- Apple Silicon native ARM64 codegen for the `aft` binary itself.

## How it works

`run.sh` performs the host setup that `Dockerfile.linux-x64` does on Linux
(install OpenCode, Bun, aimock, write configs, place locally-built AFT binary
+ plugin dist), then invokes the shared `tests/docker/test-e2e.sh` harness
with `AFT_E2E_PLATFORM=macos`. The shared harness reads platform-specific
paths (e.g. `libonnxruntime.dylib` vs `libonnxruntime.so`) from env so we
don't fork the scenario logic.

## Local invocation

The harness expects `AFT_BINARY_PATH` and `AFT_PLUGIN_DIST` to point at the
locally-built AFT binary and OpenCode plugin dist:

```bash
cargo build --release -p agent-file-tools
bun run --cwd packages/aft-bridge build
bun run --cwd packages/opencode-plugin build

AFT_BINARY_PATH="$PWD/target/release/aft" \
AFT_PLUGIN_DIST="$PWD/packages/opencode-plugin/dist" \
bash tests/macos-e2e/run.sh
```

The script writes its OpenCode config under `$RUNNER_TEMP/aft-e2e-xdg/opencode/`
(or `$TMPDIR/aft-e2e-xdg/opencode/` outside CI) and the test project under
`$RUNNER_TEMP/aft-e2e-project/`, leaving your real `~/.config/opencode/`
untouched.
