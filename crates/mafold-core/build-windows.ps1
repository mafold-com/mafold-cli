# Build mafold-core into a Windows DLL + UniFFI C# bindings, and drop them into
# the native Windows app. Mirrors build-macos.sh / build-ios.sh but targets
# Windows (MSVC) and uses NordSecurity's uniffi-bindgen-cs for C# output.
#
# Runs INSIDE the Windows build host (the Parallels "Windows 11" ARM64 VM or CI
# windows-latest). mafold-core is a standalone crate that path-depends on
# ../mafold-types, so both must be present as siblings.
#
# Usage:
#   pwsh build-windows.ps1 [-Arch aarch64|x86_64|both] [-AppDir ..\mafold-win]
#
# CPU: cargo parallelism is capped by ~/.cargo/config.toml (jobs = 2). An
# unthrottled build saturates the VM and spikes host CPU — keep the cap.

[CmdletBinding()]
param(
  [ValidateSet('aarch64', 'x86_64', 'both')]
  [string]$Arch = 'aarch64',
  [string]$AppDir = '..\mafold-win'
)

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

$targets = switch ($Arch) {
  'aarch64' { @('aarch64-pc-windows-msvc') }
  'x86_64'  { @('x86_64-pc-windows-msvc') }
  'both'    { @('aarch64-pc-windows-msvc', 'x86_64-pc-windows-msvc') }
}

Write-Host "==> rustup targets"
foreach ($t in $targets) { rustup target add $t | Out-Null }

foreach ($t in $targets) {
  Write-Host "==> cargo build --release --target $t (jobs capped by cargo config)"
  cargo build --release --target $t
  if ($LASTEXITCODE -ne 0) { throw "cargo build failed for $t" }
}

# Generate C# bindings from the compiled cdylib (proc-macro / UDL-less mode →
# must use --library, there is no .udl). Use the first-built arch's DLL; the
# generated .cs is arch-independent.
$primary = $targets[0]
$dll = Join-Path $PSScriptRoot "target\$primary\release\mafold_core.dll"
if (-not (Test-Path $dll)) { throw "missing built DLL: $dll" }

Write-Host "==> uniffi-bindgen-cs (C# bindings)"
Remove-Item -Recurse -Force generated -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path generated | Out-Null
uniffi-bindgen-cs --library $dll --out-dir generated
if ($LASTEXITCODE -ne 0) { throw "uniffi-bindgen-cs failed" }

$cs = Get-ChildItem generated -Filter *.cs | Select-Object -First 1
if (-not $cs) { throw "no .cs emitted by uniffi-bindgen-cs" }

# Drop DLL(s) + bindings into the app. DLL is per-arch (no universal binary on
# Windows); the .cs is shared. Both are gitignored build artifacts.
$appFull = (Resolve-Path (Join-Path $PSScriptRoot $AppDir) -ErrorAction SilentlyContinue)
if (-not $appFull) {
  Write-Host "==> AppDir $AppDir not found yet; leaving artifacts in .\generated"
  Write-Host "    DLL: $dll"
  Write-Host "    C#:  $($cs.FullName)"
  return
}

$genDir = Join-Path $appFull 'MafoldWin\Generated'
New-Item -ItemType Directory -Path $genDir -Force | Out-Null
Copy-Item $cs.FullName (Join-Path $genDir 'mafold_core.cs') -Force

foreach ($t in $targets) {
  $rid = if ($t -like 'aarch64*') { 'win-arm64' } else { 'win-x64' }
  $vendor = Join-Path $appFull "MafoldWin\Vendor\$rid"
  New-Item -ItemType Directory -Path $vendor -Force | Out-Null
  Copy-Item (Join-Path $PSScriptRoot "target\$t\release\mafold_core.dll") $vendor -Force
}

Write-Host "mafold-core -> $appFull (Windows DLL + C# bindings)"
