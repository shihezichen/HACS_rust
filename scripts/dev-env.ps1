# Source this on Windows when the machine has no MSVC build tools.
#
#   . .\scripts\dev-env.ps1
#
# It (1) puts cargo + a MinGW linker on PATH and (2) picks the toolchain that can
# actually link on this machine. Everything here is derived from the environment,
# so nothing machine-specific is committed.

# 1) cargo bin
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if (Test-Path $cargoBin) { $env:Path = "$cargoBin;$env:Path" }

# 2) MinGW linker (winget WinLibs layout, any version)
$pkgs = Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages'
if (Test-Path $pkgs) {
    $mingw = Get-ChildItem $pkgs -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -like 'BrechtSanders.WinLibs*' } |
        ForEach-Object { Join-Path $_.FullName 'mingw64\bin' } |
        Where-Object { Test-Path $_ } |
        Select-Object -First 1
    if ($mingw) { $env:Path = "$mingw;$env:Path" }
}

# 3) Prefer the GNU host toolchain if the MSVC linker is unavailable.
if (-not (Get-Command link.exe -ErrorAction SilentlyContinue) -and -not $env:RUSTUP_TOOLCHAIN) {
    $env:RUSTUP_TOOLCHAIN = 'stable-x86_64-pc-windows-gnu'
}

# 4) MinGW ld.exe cannot resolve paths with non-ASCII characters. If this checkout
#    lives under such a path, move build artifacts to an ASCII location.
$repoRoot = Split-Path -Parent $PSScriptRoot
if ($repoRoot -match '[^\x00-\x7F]') {
    $asciiTarget = Join-Path $env:LOCALAPPDATA 'hacs-rust-target'
    $env:CARGO_TARGET_DIR = $asciiTarget
    Write-Host "note: non-ASCII checkout path; CARGO_TARGET_DIR -> $asciiTarget"
}

Write-Host "dev-env ready: $(rustc --version 2>$null)"
