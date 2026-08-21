param(
    [switch]$Run
)
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$Frontend = Join-Path $Root 'frontend'
$SearchCore = Join-Path $Root 'search-core\Cargo.toml'
$BridgeCore = Join-Path $Root 'bridge-core\Cargo.toml'
$Tauri = Join-Path $Root 'src-tauri\Cargo.toml'

function Step([string]$Text) {
    Write-Host "`n=== $Text ===" -ForegroundColor Cyan
}

function Invoke-NativeChecked([string]$Command, [string[]]$Arguments) {
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Native command failed with exit code $LASTEXITCODE`: $Command $($Arguments -join ' ')"
    }
}

Step 'Tool versions'
Invoke-NativeChecked 'node' @('--version')
Invoke-NativeChecked 'npm' @('--version')
Invoke-NativeChecked 'cargo' @('--version')
Invoke-NativeChecked 'rustc' @('--version')

Step 'Architecture boundary contract'
& (Join-Path $Root 'scripts\verify-boundaries.ps1')
if (-not $?) {
    throw 'Architecture boundary contract failed'
}

Step 'Frontend install/test/build'
Push-Location $Frontend
try {
    Invoke-NativeChecked 'npm' @('ci')
    Invoke-NativeChecked 'npm' @('test')
    Invoke-NativeChecked 'npm' @('run', 'build')
} finally {
    Pop-Location
}

Step 'Portable Search Core regression'
Invoke-NativeChecked 'cargo' @('fmt', '--manifest-path', $SearchCore, '--', '--check')
Invoke-NativeChecked 'cargo' @('clippy', '--manifest-path', $SearchCore, '--all-targets', '--', '-D', 'warnings')
Invoke-NativeChecked 'cargo' @('test', '--manifest-path', $SearchCore, '--locked')

Step 'GUI bridge core regression'
Invoke-NativeChecked 'cargo' @('fmt', '--manifest-path', $BridgeCore, '--', '--check')
Invoke-NativeChecked 'cargo' @('clippy', '--manifest-path', $BridgeCore, '--all-targets', '--', '-D', 'warnings')
Invoke-NativeChecked 'cargo' @('test', '--manifest-path', $BridgeCore, '--locked')

Step 'Tauri bridge regression'
Invoke-NativeChecked 'cargo' @('fmt', '--manifest-path', $Tauri, '--', '--check')
Invoke-NativeChecked 'cargo' @('clippy', '--manifest-path', $Tauri, '--locked', '--all-targets', '--', '-D', 'warnings')
Invoke-NativeChecked 'cargo' @('check', '--manifest-path', $Tauri, '--locked')

Step 'Windows release build'
Invoke-NativeChecked 'cargo' @('build', '--release', '--manifest-path', $Tauri, '--locked')
$Exe = Join-Path $Root 'src-tauri\target\release\personalrag-tauri.exe'
if (-not (Test-Path $Exe)) {
    throw "Built executable not found: $Exe"
}
Write-Host "`nWINDOWS_GUI_OPTIMIZED_BUILD_PASS" -ForegroundColor Green
Write-Host "Executable: $Exe"

if ($Run) {
    Step 'Launch PersonalRag'
    Start-Process -FilePath $Exe -WorkingDirectory (Split-Path -Parent $Exe)
}
