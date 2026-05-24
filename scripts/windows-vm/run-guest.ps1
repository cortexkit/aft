# =============================================================================
# Windows VM guest-side runner (per-test-cycle).
#
# Invoked from `scripts/windows-vm/test.ts` via `prlctl exec`. The Mac side
# already mounted the repo as a Parallels shared folder reachable at the UNC
# path \\.psf\aft-repo (preferred over Z:\ because drive letters don't carry
# across UAC token boundaries / prlctl exec sessions).
#
# This script assumes the Mac side has ALREADY built:
#   - target/x86_64-pc-windows-gnu/release/aft.exe   (cross-compiled on macOS)
#   - packages/aft-bridge/dist/                      (bun build)
#   - packages/opencode-plugin/dist/                 (bun build)
#
# Why we don't build inside the VM:
#   - cargo on Windows ARM64 needs the full MSVC toolchain + ring/clang
#     (ARM64 Windows isn't even a target AFT ships).
#   - We ship x86_64-pc-windows-msvc binaries; macOS cross-compiles
#     x86_64-pc-windows-gnu which is functionally identical for AFT's
#     surface (file I/O, JSON-RPC, process spawning).
#   - Cross-compile takes ~2 min on Mac vs ~15 min cold cargo build inside VM.
#   - VM stays minimal (no Rust toolchain in the snapshot).
#
# This script:
#   1. Copies pre-built artifacts from the host share into a writable
#      C:\aft tree so robocopy isn't writing into UNC paths.
#   2. Runs tests/windows-e2e/run.ps1 with the freshly built artifacts.
# =============================================================================

# NOTE: We deliberately don't set `$ErrorActionPreference = "Stop"` globally
# here. Bun (and many other native CLIs) write their normal progress output
# to stderr, which PowerShell's "Stop" mode interprets as a fatal error and
# bubbles up as a NativeCommandError. We rely on `$LASTEXITCODE` checks
# after each native invocation instead, which gives us per-command control
# and matches how Bun actually communicates failure.
$ErrorActionPreference = "Continue"

# -----------------------------------------------------------------------------
# PATH refresh
#
# `prlctl exec` invokes commands inside a session that does NOT inherit the
# logged-in user's interactive shell PATH. winget-installed tools (Bun,
# Node.js, OpenCode, aimock) live under user-scoped install dirs that aren't
# in the system-wide PATH:
#   - Bun:                 %USERPROFILE%\.bun\bin
#   - Node.js LTS (machine-wide MSI): %ProgramFiles%\nodejs (sometimes only HKLM)
#   - npm globals:         %APPDATA%\npm
# Without this, `bun`, `node`, `opencode`, `aimock` all surface as
# "command not recognized" inside prlctl exec sessions.
$MachinePath = [System.Environment]::GetEnvironmentVariable("Path", "Machine")
$UserPath = [System.Environment]::GetEnvironmentVariable("Path", "User")
$ExtraPaths = @(
    Join-Path $env:USERPROFILE ".bun\bin"
    Join-Path $env:APPDATA "npm"
) -join ";"
$env:Path = "$ExtraPaths;$UserPath;$MachinePath"

Write-Host "PATH refreshed for prlctl-exec context."
Write-Host "  bun at:   $((Get-Command bun -ErrorAction SilentlyContinue).Source)"
Write-Host "  node at:  $((Get-Command node -ErrorAction SilentlyContinue).Source)"
Write-Host ""

$SrcRoot = "\\.psf\aft-repo"
$LocalRoot = "C:\aft"

# We use the UNC path \\.psf\<share> rather than the Z:\ drive letter because
# `prlctl exec` runs PowerShell in a session that doesn't have the user's
# drive mappings (same UAC token-isolation behavior that affects elevated
# PowerShell). UNC works regardless of token. Verified empirically: Z:\
# returned "drive not found" via prlctl exec, but \\.psf\aft-repo\ resolves.
if (-not (Test-Path $SrcRoot)) {
    Write-Host "Shared folder not found at $SrcRoot." -ForegroundColor Red
    Write-Host "The Parallels host should have mounted the repo via 'prlctl set --shf-host-add'." -ForegroundColor Red
    Write-Host "If the share IS configured, verify Parallels Tools is running in the guest." -ForegroundColor Red
    exit 2
}

# Verify the Mac side built artifacts before we kicked off the VM run. The
# orchestrator on the Mac is responsible for the build; if any of these are
# missing it means the host-side build step failed silently and we'd surface
# a confusing error inside Windows. Fail fast here with the actual cause.
$WinExe = Join-Path $SrcRoot "target\x86_64-pc-windows-gnu\release\aft.exe"
$BridgeDist = Join-Path $SrcRoot "packages\aft-bridge\dist"
$PluginDistSrc = Join-Path $SrcRoot "packages\opencode-plugin\dist"
foreach ($p in @($WinExe, $BridgeDist, $PluginDistSrc)) {
    if (-not (Test-Path $p)) {
        Write-Host "Required pre-built artifact missing on host: $p" -ForegroundColor Red
        Write-Host "Mac orchestrator should have produced it before invoking this script." -ForegroundColor Red
        exit 1
    }
}

Write-Host "============================================"
Write-Host "  AFT Windows guest runner"
Write-Host "============================================"
Write-Host "Source:  $SrcRoot (host shared folder)"
Write-Host "Local:   $LocalRoot"
Write-Host ""

# -----------------------------------------------------------------------------
# Sync minimal artifact set to local NTFS
#
# We only need:
#   - tests/windows-e2e/                       (the harness scenarios)
#   - packages/aft-bridge/{dist,package.json}  (bridge runtime, prebuilt)
#   - packages/opencode-plugin/{dist,package.json,etc} (plugin, prebuilt)
#   - packages/npm/win32-x64/                  (we drop our cross-built aft.exe here)
#   - package.json + bun.lock + tsconfig.base.json (workspace config)
#
# We deliberately don't copy crates/, ts-bench/, .git, or full
# packages/* sources -- those aren't needed for runtime.
# -----------------------------------------------------------------------------
Write-Host "Syncing artifacts to local disk..."

# Ensure clean local tree
if (Test-Path $LocalRoot) {
    Remove-Item -Recurse -Force $LocalRoot
}
New-Item -ItemType Directory -Path $LocalRoot | Out-Null

# tests/windows-e2e/
$RobocopyCommon = @("/E", "/NFL", "/NDL", "/NJH", "/NJS", "/MT:8")
& robocopy (Join-Path $SrcRoot "tests\windows-e2e") (Join-Path $LocalRoot "tests\windows-e2e") @RobocopyCommon | Out-Null
if ($LASTEXITCODE -ge 8) { Write-Host "robocopy tests failed: $LASTEXITCODE" -ForegroundColor Red; exit 1 }

# .github/ — harness reads opencode-version.txt and aft-version files from here
& robocopy (Join-Path $SrcRoot ".github") (Join-Path $LocalRoot ".github") @RobocopyCommon | Out-Null
if ($LASTEXITCODE -ge 8) { Write-Host "robocopy .github failed: $LASTEXITCODE" -ForegroundColor Red; exit 1 }

# packages/aft-bridge (just dist + package.json + node_modules placeholder
# is sufficient since we copy a self-contained dist)
& robocopy (Join-Path $SrcRoot "packages\aft-bridge\dist") (Join-Path $LocalRoot "packages\aft-bridge\dist") @RobocopyCommon | Out-Null
if ($LASTEXITCODE -ge 8) { Write-Host "robocopy bridge dist failed: $LASTEXITCODE" -ForegroundColor Red; exit 1 }
Copy-Item (Join-Path $SrcRoot "packages\aft-bridge\package.json") (Join-Path $LocalRoot "packages\aft-bridge\package.json")

# packages/opencode-plugin
& robocopy (Join-Path $SrcRoot "packages\opencode-plugin\dist") (Join-Path $LocalRoot "packages\opencode-plugin\dist") @RobocopyCommon | Out-Null
if ($LASTEXITCODE -ge 8) { Write-Host "robocopy plugin dist failed: $LASTEXITCODE" -ForegroundColor Red; exit 1 }
Copy-Item (Join-Path $SrcRoot "packages\opencode-plugin\package.json") (Join-Path $LocalRoot "packages\opencode-plugin\package.json")

# packages/npm/win32-x64/bin/aft.exe  (where the resolver expects it)
$WinPkgBin = Join-Path $LocalRoot "packages\npm\win32-x64\bin"
New-Item -ItemType Directory -Path $WinPkgBin -Force | Out-Null
Copy-Item $WinExe (Join-Path $WinPkgBin "aft.exe")

# Also keep the cross-built binary at the canonical target path so the e2e
# harness (which sets AFT_BINARY_PATH) and any direct invocations work.
$LocalTarget = Join-Path $LocalRoot "target\x86_64-pc-windows-gnu\release"
New-Item -ItemType Directory -Path $LocalTarget -Force | Out-Null
Copy-Item $WinExe (Join-Path $LocalTarget "aft.exe")

Write-Host "  artifacts copied to $LocalRoot"
Write-Host ""

# -----------------------------------------------------------------------------
# Per-package bun install
#
# We DON'T copy the workspace root package.json because Bun would resolve
# its `"workspaces": ["packages/*", "benchmarks"]` glob against the local
# C:\aft tree and fail because most packages weren't copied.
#
# Instead each package gets its own self-contained `bun install` so its
# runtime deps land in C:\aft\packages\<pkg>\node_modules\. The plugin
# bundles all its deps via `bun build`, but aft-bridge is plain `tsc` and
# imports `undici` at runtime — that's the actual reason we need this step.
# -----------------------------------------------------------------------------
Push-Location $LocalRoot
try {
    foreach ($pkg in @("aft-bridge", "opencode-plugin")) {
        $pkgDir = Join-Path $LocalRoot "packages\$pkg"
        Write-Host "Installing deps for packages\$pkg ..."
        Push-Location $pkgDir
        try {
            & bun install --production 2>&1 | Out-Null
            if ($LASTEXITCODE -ne 0) {
                Write-Host "  bun install failed for $pkg (exit $LASTEXITCODE); harness may fail if deps aren't bundled" -ForegroundColor Yellow
            }
        } finally {
            Pop-Location
        }
    }

    $BinaryPath = Join-Path $LocalTarget "aft.exe"
    $PluginDist = Join-Path $LocalRoot "packages\opencode-plugin\dist"
    Write-Host "  plugin dist:   $PluginDist"
    Write-Host "  aft binary:    $BinaryPath"
    Write-Host ""

    # -------------------------------------------------------------------------
    # Run the harness
    # -------------------------------------------------------------------------
    $env:AFT_BINARY_PATH = $BinaryPath
    $env:AFT_PLUGIN_DIST = $PluginDist

    Write-Host "Running Windows E2E harness..."
    & pwsh -NoProfile -ExecutionPolicy Bypass -File (Join-Path $LocalRoot "tests\windows-e2e\run.ps1")
    $HarnessExit = $LASTEXITCODE

    Write-Host ""
    if ($HarnessExit -ne 0) {
        Write-Host "Harness exited with code $HarnessExit" -ForegroundColor Red
    } else {
        Write-Host "Harness exited cleanly" -ForegroundColor Green
    }

    exit $HarnessExit
} finally {
    Pop-Location
}
