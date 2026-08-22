param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Warm', 'Cold')]
    [string]$Mode,

    [Parameter(Mandatory = $true)]
    [string]$Root,

    [Parameter(Mandatory = $true)]
    [ValidateRange(1, 128)]
    [int]$HydrationWorkers,

    [int]$BuildWorkers = 0,
    [int]$SegmentDocs = 0,
    [string]$OutputRoot = '',
    [string]$LogPath = ''
)

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$BridgeManifest = Join-Path $RepoRoot 'bridge-core\Cargo.toml'

if (-not (Test-Path -LiteralPath $Root -PathType Container)) {
    throw "Root directory not found: $Root"
}
if ($BuildWorkers -lt 0 -or $SegmentDocs -lt 0) {
    throw 'BuildWorkers and SegmentDocs must be zero (auto/default) or positive.'
}
if ($SegmentDocs -gt 0 -and $BuildWorkers -eq 0) {
    throw 'Specify BuildWorkers when overriding SegmentDocs so optional CLI positions stay unambiguous.'
}

if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $OutputRoot = Join-Path $env:TEMP "PersonalRag-index-profile-$stamp"
}
New-Item -ItemType Directory -Force -Path $OutputRoot | Out-Null

if ([string]::IsNullOrWhiteSpace($LogPath)) {
    $LogPath = Join-Path $OutputRoot 'profile.log'
}

if ($Mode -eq 'Cold') {
    Write-Warning 'Cold A/B must run ONE hydration-worker candidate per verified cold-cache session.'
    Write-Warning 'Do not run 1,2,4,8,12 sequentially after one another and call that a cold comparison.'
    Write-Host 'Recommended candidates: 1, 2, 4, 8, 12 (reboot or otherwise establish a verified cold cache before each run).'
}

$cargoArgs = @(
    'run', '--release',
    '--manifest-path', $BridgeManifest,
    '--example', 'index_build_profile',
    '--', $Mode.ToLowerInvariant(), $Root, $OutputRoot, [string]$HydrationWorkers
)
if ($BuildWorkers -gt 0) {
    $cargoArgs += [string]$BuildWorkers
    if ($SegmentDocs -gt 0) {
        $cargoArgs += [string]$SegmentDocs
    }
}

$previousProfileBuild = $env:PR_PROFILE_BUILD
$env:PR_PROFILE_BUILD = '1'
$currentRun = ''
$segments = [System.Collections.Generic.List[object]]::new()
$captured = [System.Collections.Generic.List[string]]::new()

try {
    Push-Location $RepoRoot
    try {
        & cargo @cargoArgs 2>&1 | ForEach-Object {
            $line = [string]$_
            $captured.Add($line)
            Write-Host $line

            if ($line -match '^PROFILE_RUN_BEGIN label=([^ ]+)') {
                $currentRun = $Matches[1]
                return
            }
            if ($line -match '^BUILD_SEGMENT_WALL segment=(\d+) docs=(\d+) sample_ms=([0-9.]+) base_ms=([0-9.]+) base_write_ms=([0-9.]+) accel_ms=([0-9.]+) total_ms=([0-9.]+)') {
                $segments.Add([pscustomobject]@{
                    Label = $currentRun
                    Segment = [int]$Matches[1]
                    Docs = [int]$Matches[2]
                    SampleMs = [double]$Matches[3]
                    BaseMs = [double]$Matches[4]
                    WriteMs = [double]$Matches[5]
                    AccelMs = [double]$Matches[6]
                    TotalMs = [double]$Matches[7]
                })
            }
        }
        if ($LASTEXITCODE -ne 0) {
            throw "index_build_profile failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
} finally {
    $env:PR_PROFILE_BUILD = $previousProfileBuild
}

$captured | Set-Content -LiteralPath $LogPath -Encoding utf8

function Get-Percentile([object[]]$Rows, [string]$Property, [double]$Percentile) {
    if ($Rows.Count -eq 0) {
        return 0.0
    }
    $values = @($Rows | ForEach-Object { [double]($_.$Property) } | Sort-Object)
    $index = [int][Math]::Ceiling(($values.Count - 1) * $Percentile)
    return [double]$values[$index]
}

$targetLabel = if ($Mode -eq 'Warm') { 'warm-measured' } else { 'cold' }
$measuredSegments = @($segments | Where-Object { $_.Label -eq $targetLabel })
if ($measuredSegments.Count -eq 0) {
    throw "No BUILD_SEGMENT_WALL rows captured for $targetLabel. PR_PROFILE_BUILD output is required."
}

$slowestTotal = $measuredSegments | Sort-Object TotalMs -Descending | Select-Object -First 1
$slowestBase = $measuredSegments | Sort-Object BaseMs -Descending | Select-Object -First 1
$slowestWrite = $measuredSegments | Sort-Object WriteMs -Descending | Select-Object -First 1
$slowestAccel = $measuredSegments | Sort-Object AccelMs -Descending | Select-Object -First 1

$critical = [ordered]@{
    mode = $Mode.ToLowerInvariant()
    hydrationWorkers = $HydrationWorkers
    measuredLabel = $targetLabel
    segmentCount = $measuredSegments.Count
    totalMsP50 = Get-Percentile $measuredSegments 'TotalMs' 0.50
    totalMsP95 = Get-Percentile $measuredSegments 'TotalMs' 0.95
    slowestTotal = $slowestTotal
    slowestBase = $slowestBase
    slowestWrite = $slowestWrite
    slowestAcceleration = $slowestAccel
    interpretation = 'Segment rows are wall times for individual workers. Aggregated *_work metrics are summed worker time and can exceed end-to-end wall time.'
    logPath = $LogPath
}

$criticalJson = $critical | ConvertTo-Json -Depth 5 -Compress
Write-Host "CRITICAL_PATH_JSON $criticalJson"

if ($Mode -eq 'Cold') {
    Write-Host 'COLD_AB_NEXT: repeat this command with another HydrationWorkers value only after restoring a verified cold-cache state.'
}
