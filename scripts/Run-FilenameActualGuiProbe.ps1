param(
    [Parameter(Mandatory=$true)][string]$GuiExe,
    [Parameter(Mandatory=$true)][string]$Root,
    [Parameter(Mandatory=$true)][string]$Store,
    [Parameter(Mandatory=$true)][string]$Report,
    [string]$Query = "fixture_",
    [int]$TimeoutSeconds = 120
)

$ErrorActionPreference = 'Stop'
$gui = [System.IO.Path]::GetFullPath($GuiExe)
$root = [System.IO.Path]::GetFullPath($Root)
$store = [System.IO.Path]::GetFullPath($Store)
$report = [System.IO.Path]::GetFullPath($Report)
if (-not (Test-Path -LiteralPath $gui)) { throw "GUI executable not found: $gui" }
if (Test-Path -LiteralPath $report) { Remove-Item -LiteralPath $report -Force }

$dir = [System.IO.Path]::GetDirectoryName($report)
New-Item -ItemType Directory -Force -Path $dir | Out-Null

$psi = [System.Diagnostics.ProcessStartInfo]::new()
$psi.FileName = $gui
$psi.UseShellExecute = $false
# Use the string Arguments property for compatibility with both PowerShell 5.1
# and PowerShell 7. The formal probe is a local child process, so quoting each
# argument as a Windows command-line token is sufficient and deterministic.
$probeArguments = @('--startup-probe', $report, $root, $store, $Query)
$psi.Arguments = (($probeArguments | ForEach-Object {
    '"' + ([string]$_).Replace('"', '\"') + '"'
}) -join ' ')

$watch = [System.Diagnostics.Stopwatch]::StartNew()
$process = [System.Diagnostics.Process]::Start($psi)
if ($null -eq $process) { throw "Failed to start GUI process" }

$deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
while (-not (Test-Path -LiteralPath $report)) {
    if ($process.HasExited) {
        throw "GUI exited before startup probe was written. ExitCode=$($process.ExitCode)"
    }
    if ([DateTime]::UtcNow -ge $deadline) {
        try { $process.Kill($true) } catch {}
        throw "Timed out waiting for actual GUI startup probe"
    }
    Start-Sleep -Milliseconds 10
}
$watch.Stop()

$child = Get-Content -LiteralPath $report -Raw | ConvertFrom-Json
$process.WaitForExit([Math]::Max(1000, $TimeoutSeconds * 1000)) | Out-Null

$result = [ordered]@{
    version = 1
    pass = [bool]$child.ready -and $watch.Elapsed.TotalMilliseconds -le 2000 -and [int64]$child.private_bytes -le 1500000000
    process_start_to_ready_ms = $watch.Elapsed.TotalMilliseconds
    child = $child
    query = $Query
    executable = $gui
    utc = [DateTime]::UtcNow
}
$result | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $report -Encoding utf8
$result | ConvertTo-Json -Depth 8
if (-not $result.pass) { exit 1 }
