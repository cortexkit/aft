# =============================================================================
# Windows VM guest-side ONE-TIME setup.
#
# Run this manually inside the Windows VM AFTER `bun run windows-vm:setup`
# has copied it in (or copy it yourself via shared folder + double-click).
#
# Installs the toolchain we'll need on every test cycle:
#   - Bun (for running aft-bridge / aft-opencode / our orchestrator)
#   - Rust (rustup + stable toolchain, builds AFT binary)
#   - Git (rust-analyzer / cargo deps fetching)
#   - Node.js LTS (OpenCode runs on Node)
#   - OpenCode (npm install -g opencode-ai)
#   - aimock (npm install -g @copilotkit/aimock)
#   - Windows Defender exclusions for C:\aft and C:\Users\<you>\.cargo
#     (huge speedup for cargo builds -- Defender real-time scan on .rlib
#     files is the #1 cargo-on-Windows perf killer)
#
# After this finishes successfully, snapshot the VM as "aft-ready" and
# subsequent test cycles can revert to it for fast iteration.
# =============================================================================

$ErrorActionPreference = "Stop"

Write-Host "============================================"
Write-Host "  AFT Windows VM one-time setup"
Write-Host "============================================"
Write-Host ""

# Sanity check -- must run as admin for Defender exclusions and global npm.
$IsAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole] "Administrator")
if (-not $IsAdmin) {
    Write-Host "ERROR: This script must run as Administrator." -ForegroundColor Red
    Write-Host "       Right-click PowerShell -> Run as administrator, then re-run." -ForegroundColor Red
    exit 1
}

# -----------------------------------------------------------------------------
# Defender exclusions FIRST -- every step below benefits from these.
# -----------------------------------------------------------------------------
Write-Host "-- Configuring Windows Defender exclusions --"
$Exclusions = @(
    "C:\aft",
    "$env:USERPROFILE\.cargo",
    "$env:USERPROFILE\.rustup",
    "$env:USERPROFILE\.bun",
    "$env:USERPROFILE\.cache\aft",
    "$env:USERPROFILE\AppData\Roaming\npm",
    "$env:USERPROFILE\AppData\Local\npm-cache"
)
foreach ($path in $Exclusions) {
    try {
        Add-MpPreference -ExclusionPath $path -ErrorAction SilentlyContinue
        Write-Host "  + $path"
    } catch {
        Write-Host "  ! could not exclude $path : $_" -ForegroundColor Yellow
    }
}
Write-Host ""

# -----------------------------------------------------------------------------
# winget: prefer when available; fall back to direct downloads otherwise.
# -----------------------------------------------------------------------------
$HasWinget = $null -ne (Get-Command winget -ErrorAction SilentlyContinue)

# -----------------------------------------------------------------------------
# Git
# -----------------------------------------------------------------------------
if ($null -eq (Get-Command git -ErrorAction SilentlyContinue)) {
    Write-Host "-- Installing Git --"
    if ($HasWinget) {
        & winget install --id Git.Git -e --source winget --accept-source-agreements --accept-package-agreements
    } else {
        Write-Host "  winget not available; please install Git manually from https://git-scm.com/" -ForegroundColor Yellow
        Write-Host "  Continuing -- some steps may fail." -ForegroundColor Yellow
    }
} else {
    Write-Host "Git already installed: $((& git --version))"
}
Write-Host ""

# -----------------------------------------------------------------------------
# Node.js LTS -- required by OpenCode + aimock
# -----------------------------------------------------------------------------
if ($null -eq (Get-Command node -ErrorAction SilentlyContinue)) {
    Write-Host "-- Installing Node.js LTS --"
    if ($HasWinget) {
        & winget install --id OpenJS.NodeJS.LTS -e --source winget --accept-source-agreements --accept-package-agreements
        # Refresh PATH for current session
        $env:Path = [System.Environment]::GetEnvironmentVariable("Path","Machine") + ";" + [System.Environment]::GetEnvironmentVariable("Path","User")
    } else {
        Write-Host "  winget not available; please install Node.js manually from https://nodejs.org/" -ForegroundColor Yellow
    }
} else {
    Write-Host "Node already installed: $((& node --version))"
}
Write-Host ""

# -----------------------------------------------------------------------------
# Bun (PowerShell installer is the official path)
# -----------------------------------------------------------------------------
if ($null -eq (Get-Command bun -ErrorAction SilentlyContinue)) {
    Write-Host "-- Installing Bun --"
    Invoke-RestMethod -Uri "https://bun.sh/install.ps1" -OutFile "$env:TEMP\install-bun.ps1"
    & powershell -ExecutionPolicy Bypass -File "$env:TEMP\install-bun.ps1"
    # Bun installs to $env:USERPROFILE\.bun\bin\bun.exe
    $BunPath = Join-Path $env:USERPROFILE ".bun\bin"
    if (Test-Path (Join-Path $BunPath "bun.exe")) {
        # Add to user PATH permanently
        $UserPath = [System.Environment]::GetEnvironmentVariable("Path", "User")
        if ($UserPath -notlike "*$BunPath*") {
            [System.Environment]::SetEnvironmentVariable("Path", "$UserPath;$BunPath", "User")
        }
        $env:Path = "$env:Path;$BunPath"
        Write-Host "  Bun installed to $BunPath"
    } else {
        Write-Host "  Bun install script ran but bun.exe not found at expected location" -ForegroundColor Yellow
    }
} else {
    Write-Host "Bun already installed: $((& bun --version))"
}
Write-Host ""

# -----------------------------------------------------------------------------
# Visual Studio Build Tools (C++ workload)
#
# Required for the Rust MSVC toolchain. Without it, cargo build fails at the
# linker step with:
#   "error: linker `link.exe` not found ... please ensure that Visual Studio
#    2017 or later, or Build Tools for Visual Studio were installed with the
#    Visual C++ option"
# This is a 3-7 GB install but it's the canonical Rust-on-Windows setup;
# alternatives (rustup gnu toolchain) produce non-MSVC binaries that don't
# match what AFT actually ships to Windows users.
#
# Detection: Visual Studio Locator (vswhere.exe) ships with VS2017+ Build
# Tools and is the official way to detect installed VS instances.
# -----------------------------------------------------------------------------
$VSWherePath = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$HasVCTools = $false
if (Test-Path $VSWherePath) {
    # -requires Microsoft.VisualStudio.Component.VC.Tools.* checks for the
    # C++ build tools workload specifically (any architecture variant).
    $vsInstance = & $VSWherePath -latest -products * `
        -requires Microsoft.VisualStudio.Workload.VCTools `
        -property installationPath 2>$null
    if ($vsInstance) {
        $HasVCTools = $true
        Write-Host "Visual Studio Build Tools (VC++) already installed at: $vsInstance"
    }
}

if (-not $HasVCTools) {
    Write-Host "-- Installing Visual Studio 2022 Build Tools (VC++ workload) --"
    Write-Host "   This is large (~3-7 GB) and may take 10-20 minutes."
    if ($HasWinget) {
        # --override is the documented way to pass arguments through winget to
        # the VS installer. We use --quiet --wait so winget doesn't return
        # before the install actually finishes (otherwise rustup runs against
        # a half-installed toolchain and we hit the linker error anyway).
        #
        # IMPORTANT: on ARM64 Windows, Microsoft's --includeRecommended for the
        # VCTools workload installs ONLY the x64-host C++ toolchain, NOT the
        # ARM64-native compiler. That's enough to satisfy vswhere ("VCTools
        # workload installed") but NOT to actually link an aarch64-msvc Rust
        # build, because cargo defaults to the host triple
        # (aarch64-pc-windows-msvc) and looks for Hostarm64\arm64\link.exe,
        # which only ships with the explicit VC.Tools.ARM64 component.
        # We add it unconditionally; on x64 hosts it's a small no-op
        # extra, on ARM64 hosts it's the difference between cargo working
        # and the "linker `link.exe` not found" error.
        $vsArgs = @(
            "--quiet"
            "--wait"
            "--add"
            "Microsoft.VisualStudio.Workload.VCTools"
            "--add"
            "Microsoft.VisualStudio.Component.VC.Tools.ARM64"
            "--includeRecommended"
        ) -join " "
        & winget install --id Microsoft.VisualStudio.2022.BuildTools -e `
            --source winget `
            --accept-source-agreements --accept-package-agreements `
            --override $vsArgs
        if ($LASTEXITCODE -ne 0) {
            Write-Host "  VS Build Tools install failed (exit $LASTEXITCODE)" -ForegroundColor Red
            Write-Host "  Cargo builds will fail at the linker step until this is resolved." -ForegroundColor Red
            exit 1
        }
    } else {
        Write-Host "  winget not available; install Visual Studio Build Tools manually:" -ForegroundColor Yellow
        Write-Host "  https://visualstudio.microsoft.com/visual-cpp-build-tools/" -ForegroundColor Yellow
        Write-Host "  Select the 'Desktop development with C++' workload AND the" -ForegroundColor Yellow
        Write-Host "  'MSVC v143 - VS 2022 C++ ARM64/ARM64EC build tools' component," -ForegroundColor Yellow
        Write-Host "  then re-run this script." -ForegroundColor Yellow
        exit 1
    }
    Write-Host "  VS Build Tools installed."
}
Write-Host ""

# -----------------------------------------------------------------------------
# Rust toolchain via rustup
# -----------------------------------------------------------------------------
if ($null -eq (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "-- Installing Rust (rustup) --"
    # Pick the right rustup-init based on the actual VM architecture.
    # Apple Silicon hosts will run ARM64 Windows; Intel hosts run x64.
    $rustupArch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "aarch64" } else { "x86_64" }
    $rustupUrl = "https://win.rustup.rs/$rustupArch"
    Write-Host "  detected arch: $env:PROCESSOR_ARCHITECTURE -> rustup-init for $rustupArch"
    Invoke-WebRequest -Uri $rustupUrl -OutFile "$env:TEMP\rustup-init.exe"
    # -y = non-interactive, --default-toolchain stable
    & "$env:TEMP\rustup-init.exe" -y --default-toolchain stable --no-modify-path
    # Update PATH for current session
    $CargoPath = Join-Path $env:USERPROFILE ".cargo\bin"
    $env:Path = "$env:Path;$CargoPath"
    # Persist to user PATH
    $UserPath = [System.Environment]::GetEnvironmentVariable("Path", "User")
    if ($UserPath -notlike "*$CargoPath*") {
        [System.Environment]::SetEnvironmentVariable("Path", "$UserPath;$CargoPath", "User")
    }
    Write-Host "  Rust installed: $((& cargo --version))"
} else {
    Write-Host "Rust already installed: $((& cargo --version))"
}
Write-Host ""

# -----------------------------------------------------------------------------
# OpenCode + aimock via npm (need Node first; we just installed it above)
# -----------------------------------------------------------------------------
Write-Host "-- Installing OpenCode --"
& npm install -g opencode-ai
if ($LASTEXITCODE -ne 0) {
    Write-Host "  npm install -g opencode-ai failed" -ForegroundColor Red
    exit 1
}
Write-Host "  OpenCode installed: $((& opencode --version))"
Write-Host ""

Write-Host "-- Installing aimock --"
& npm install -g `@copilotkit/aimock
if ($LASTEXITCODE -ne 0) {
    Write-Host "  npm install -g @copilotkit/aimock failed" -ForegroundColor Red
    exit 1
}
Write-Host "  aimock installed."
Write-Host ""

# -----------------------------------------------------------------------------
# pwsh (PowerShell 7+) -- newer tooling than Windows PowerShell 5.
# Our run.ps1 works on either, but pwsh has better defaults.
# -----------------------------------------------------------------------------
if ($null -eq (Get-Command pwsh -ErrorAction SilentlyContinue)) {
    Write-Host "-- Installing PowerShell 7 --"
    if ($HasWinget) {
        & winget install --id Microsoft.PowerShell -e --source winget --accept-source-agreements --accept-package-agreements
    } else {
        Write-Host "  pwsh not available; will use Windows PowerShell 5. Install pwsh manually for newer features." -ForegroundColor Yellow
    }
}
Write-Host ""

# -----------------------------------------------------------------------------
# Final summary
# -----------------------------------------------------------------------------
Write-Host "============================================"
Write-Host "  Setup complete." -ForegroundColor Green
Write-Host "============================================"
Write-Host ""
Write-Host "Next steps:"
Write-Host "  1. Return to the Mac terminal that's running 'bun run windows-vm:setup'."
Write-Host "  2. Press Enter there. The script will snapshot the VM as 'aft-ready'"
Write-Host "     and suspend it."
Write-Host "  3. Run the test cycle from the Mac:"
Write-Host "       bun run test:windows-e2e"
Write-Host ""
