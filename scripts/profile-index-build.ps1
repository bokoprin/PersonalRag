param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Config', 'Warm', 'Cold')]
    [string]$Mode,

    [Parameter(Mandatory = $true)]
    [string]$Root,

    [int]$HydrationWorkers = 0,
    [int]$BuildWorkers = 0,
    [int]$SegmentDocs = 0,
    [UInt64]$MaxFileBytes = 33554432,
    [UInt64]$HydrationBatchBytes = 0,
    [ValidateSet('auto', 'walk_dir', 'windows_native')]
    [string]$ScannerMode = 'auto',
    [ValidateSet('balanced', 'full', 'adaptive_delta', 'none')]
    [string]$AccelerationProfile = 'balanced',
    [string]$FrozenConfigPath = '',
    [string]$OutputRoot = '',
    [string]$LogPath = '',
    [string]$SummaryPath = '',
    [switch]$DisableInstrumentation
)

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$BridgeManifest = Join-Path $RepoRoot 'bridge-core\Cargo.toml'

function Get-JsonLine([string[]]$Lines, [string]$Prefix) {
    $rows = @($Lines | Where-Object { $_.StartsWith($Prefix, [StringComparison]::Ordinal) })
    if ($rows.Count -eq 0) {
        return $null
    }
    return $rows[$rows.Count - 1].Substring($Prefix.Length) | ConvertFrom-Json
}

function Get-JsonLines([string[]]$Lines, [string]$Prefix) {
    return @(
        $Lines |
            Where-Object { $_.StartsWith($Prefix, [StringComparison]::Ordinal) } |
            ForEach-Object { $_.Substring($Prefix.Length) | ConvertFrom-Json }
    )
}

function Convert-BuildHydration([string[]]$Lines) {
    $result = [System.Collections.Generic.List[object]]::new()
    $label = ''
    foreach ($line in $Lines) {
        if ($line -match '^PROFILE_RUN_BEGIN label=([^ ]+)') {
            $label = $Matches[1]
            continue
        }
        if (-not $line.StartsWith('BUILD_HYDRATION ', [StringComparison]::Ordinal)) {
            continue
        }
        $values = [ordered]@{ label = $label }
        foreach ($match in [regex]::Matches($line.Substring('BUILD_HYDRATION '.Length), '(?<key>[A-Za-z0-9_]+)=(?<value>[^\s]+)')) {
            $key = $match.Groups['key'].Value
            $raw = $match.Groups['value'].Value
            $number = 0.0
            if ([double]::TryParse($raw, [Globalization.NumberStyles]::Float, [Globalization.CultureInfo]::InvariantCulture, [ref]$number)) {
                $values[$key] = $number
            } else {
                $values[$key] = $raw
            }
        }
        $result.Add([pscustomobject]$values)
    }
    return @($result)
}

function Get-FileSha256([string]$Path) {
    $hasher = [Security.Cryptography.SHA256]::Create()
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        return ([BitConverter]::ToString($hasher.ComputeHash($stream))).Replace('-', '').ToLowerInvariant()
    } finally {
        $stream.Dispose()
        $hasher.Dispose()
    }
}

function Get-Sha256Tree([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) {
        return $null
    }
    $root = [IO.Path]::GetFullPath($Path).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    $entries = @(
        Get-ChildItem -LiteralPath $root -File -Recurse -Force |
            ForEach-Object {
                $relative = $_.FullName.Substring($root.Length).TrimStart([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar).Replace('\', '/')
                [ordered]@{
                    relativePath = $relative
                    size = [Int64]$_.Length
                    sha256 = Get-FileSha256 $_.FullName
                }
            } |
            Sort-Object relativePath
    )
    $canonical = $entries | ConvertTo-Json -Depth 4 -Compress
    $hasher = [Security.Cryptography.SHA256]::Create()
    try {
        $treeHash = [System.BitConverter]::ToString($hasher.ComputeHash([Text.Encoding]::UTF8.GetBytes($canonical))).Replace('-', '').ToLowerInvariant()
    } finally {
        $hasher.Dispose()
    }
    [ordered]@{
        root = $root
        files = $entries
        treeSha256 = $treeHash
    }
}

function Get-Percentile([object[]]$Rows, [string]$Property, [double]$Percentile) {
    if ($Rows.Count -eq 0) {
        return 0.0
    }
    $values = @($Rows | ForEach-Object { [double]($_.$Property) } | Sort-Object)
    $index = [int][Math]::Ceiling(($values.Count - 1) * $Percentile)
    return [double]$values[$index]
}

function Get-Slowest([object[]]$Rows, [string]$Property) {
    if ($Rows.Count -eq 0) {
        return $null
    }
    return $Rows | Sort-Object $Property -Descending | Select-Object -First 1
}

if (-not (Test-Path -LiteralPath $Root -PathType Container)) {
    throw "Root directory not found: $Root"
}
if ($MaxFileBytes -eq 0) {
    throw 'MaxFileBytes must be greater than zero.'
}
if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $OutputRoot = Join-Path $env:TEMP "PersonalRag-index-profile-$stamp"
}
New-Item -ItemType Directory -Force -Path $OutputRoot | Out-Null
if ([string]::IsNullOrWhiteSpace($LogPath)) {
    $LogPath = Join-Path $OutputRoot 'profile.log'
}
if ([string]::IsNullOrWhiteSpace($SummaryPath)) {
    $SummaryPath = Join-Path $OutputRoot 'profile-wrapper.json'
}

if (-not [string]::IsNullOrWhiteSpace($FrozenConfigPath)) {
    if ($Mode -eq 'Config') {
        throw 'FrozenConfigPath is only valid for Warm or Cold.'
    }
    if (-not (Test-Path -LiteralPath $FrozenConfigPath -PathType Leaf)) {
        throw "Frozen config file not found: $FrozenConfigPath"
    }
    $frozen = Get-Content -Raw -LiteralPath $FrozenConfigPath | ConvertFrom-Json
    if ($null -ne $frozen.frozenBenchmarkConfig) {
        $frozen = $frozen.frozenBenchmarkConfig
    }
    $HydrationWorkers = [int]$frozen.hydrationWorkers
    $BuildWorkers = [int]$frozen.buildWorkers
    $SegmentDocs = [int]$frozen.segmentDocs
    $MaxFileBytes = [UInt64]$frozen.maxFileBytes
    $HydrationBatchBytes = [UInt64]$frozen.hydrationBatchBytes
    $ScannerMode = [string]$frozen.scannerMode
    $AccelerationProfile = [string]$frozen.accelerationProfile
}

$cargoArgs = @(
    'run', '--release', '--features', 'profile-build',
    '--manifest-path', $BridgeManifest,
    '--example', 'index_build_profile',
    '--', $Mode.ToLowerInvariant(), $Root, $OutputRoot
)
if ($Mode -eq 'Config') {
    $cargoArgs += [string]$MaxFileBytes
    $cargoArgs += $ScannerMode
} else {
    foreach ($value in @($HydrationWorkers, $BuildWorkers, $SegmentDocs, $MaxFileBytes, $HydrationBatchBytes)) {
        if ([UInt64]$value -eq 0) {
            throw 'Warm/Cold require fully resolved non-zero frozen settings.'
        }
        $cargoArgs += [string]$value
    }
    $cargoArgs += $ScannerMode
    $cargoArgs += $AccelerationProfile
}

$previousProfileBuild = $env:PR_PROFILE_BUILD
if ($DisableInstrumentation) {
    Remove-Item Env:PR_PROFILE_BUILD -ErrorAction SilentlyContinue
} else {
    $env:PR_PROFILE_BUILD = '1'
}
$currentRun = ''
$segments = [System.Collections.Generic.List[object]]::new()
$phases = [System.Collections.Generic.List[object]]::new()
$captured = [System.Collections.Generic.List[string]]::new()
$previousErrorActionPreference = $ErrorActionPreference

try {
    # Cargo writes normal progress to stderr.  In Windows PowerShell, redirecting that stream
    # while ErrorActionPreference is Stop turns ordinary "Compiling" lines into terminating
    # NativeCommandError records, so capture it with Continue and rely on the real exit code.
    $ErrorActionPreference = 'Continue'
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
            if ($line -match '^BUILD_PHASE base=(\d+) docs=(\d+) units=(\d+) name_grams_ms=([0-9.]+) dedup_ms=([0-9.]+) content_grams_ms=([0-9.]+) content_post_ms=([0-9.]+) name_post_ms=([0-9.]+) total_ms=([0-9.]+)') {
                $phases.Add([pscustomobject]@{
                    Label = $currentRun
                    Base = [uint64]$Matches[1]
                    Docs = [int]$Matches[2]
                    Units = [int]$Matches[3]
                    NameGramsMs = [double]$Matches[4]
                    DedupMs = [double]$Matches[5]
                    ContentGramsMs = [double]$Matches[6]
                    ContentPostMs = [double]$Matches[7]
                    NamePostMs = [double]$Matches[8]
                    TotalMs = [double]$Matches[9]
                })
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
    $ErrorActionPreference = $previousErrorActionPreference
    $env:PR_PROFILE_BUILD = $previousProfileBuild
}

$captured | Set-Content -LiteralPath $LogPath -Encoding utf8
$config = Get-JsonLine @($captured) 'PROFILE_CONFIG_JSON '
if ($null -eq $config) {
    throw 'PROFILE_CONFIG_JSON was not emitted.'
}
$summary = Get-JsonLine @($captured) 'PROFILE_SUMMARY_JSON '
$runSummaries = Get-JsonLines @($captured) 'PROFILE_RUN_JSON '
$hydration = Convert-BuildHydration @($captured)
$critical = $null

if ($Mode -ne 'Config') {
    $targetLabel = if ($Mode -eq 'Warm') { 'warm-measured' } else { 'cold' }
    $measuredSegments = @($segments | Where-Object { $_.Label -eq $targetLabel })
    $measuredPhases = @($phases | Where-Object { $_.Label -eq $targetLabel })
    if (($measuredSegments.Count -eq 0 -or $measuredPhases.Count -eq 0) -and -not $DisableInstrumentation) {
        throw "Profile output did not contain complete BUILD_SEGMENT_WALL/BUILD_PHASE rows for $targetLabel."
    }
    if ($null -eq $summary) {
        throw 'PROFILE_SUMMARY_JSON was not emitted.'
    }
    if ($DisableInstrumentation) {
        $critical = [ordered]@{
            mode = $Mode.ToLowerInvariant()
            measuredLabel = $targetLabel
            instrumentation = 'disabled'
            interpretation = 'Collector-disabled profile control: only PROFILE_SUMMARY_JSON is available; detailed stage logs are intentionally absent.'
        }
    } else {
        $critical = [ordered]@{
            mode = $Mode.ToLowerInvariant()
            measuredLabel = $targetLabel
            segmentCount = $measuredSegments.Count
            totalMsP50 = Get-Percentile $measuredSegments 'TotalMs' 0.50
            totalMsP95 = Get-Percentile $measuredSegments 'TotalMs' 0.95
            slowestTotal = Get-Slowest $measuredSegments 'TotalMs'
            slowestBase = Get-Slowest $measuredSegments 'BaseMs'
            slowestWrite = Get-Slowest $measuredSegments 'WriteMs'
            slowestAcceleration = Get-Slowest $measuredSegments 'AccelMs'
            segmentCore = [ordered]@{
                phaseCount = $measuredPhases.Count
                coreTotalMsP50 = Get-Percentile $measuredPhases 'TotalMs' 0.50
                coreTotalMsP95 = Get-Percentile $measuredPhases 'TotalMs' 0.95
                slowestCore = Get-Slowest $measuredPhases 'TotalMs'
                slowestContentGrams = Get-Slowest $measuredPhases 'ContentGramsMs'
                slowestContentPost = Get-Slowest $measuredPhases 'ContentPostMs'
                slowestDedup = Get-Slowest $measuredPhases 'DedupMs'
                slowestNameGrams = Get-Slowest $measuredPhases 'NameGramsMs'
                slowestNamePost = Get-Slowest $measuredPhases 'NamePostMs'
            }
            interpretation = 'BUILD_SEGMENT_WALL/BUILD_PHASE are per-segment wall measurements. DiskPathBuildReport *_work fields are summed worker time and can exceed end-to-end wall time.'
        }
    }
}

$wrapper = [ordered]@{
    schemaVersion = 2
    mode = $Mode.ToLowerInvariant()
    config = $config
    summary = $summary
    runSummaries = @($runSummaries)
    criticalPath = $critical
    hydration = @($hydration)
    logPath = $LogPath
    profileInstrumentationEnabled = (-not $DisableInstrumentation)
}
$wrapper | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $SummaryPath -Encoding utf8
$config | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $OutputRoot 'profile-config.json') -Encoding utf8
if ($null -ne $summary) {
    $summary | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $OutputRoot 'profile-summary.json') -Encoding utf8
}
foreach ($run in @($runSummaries)) {
    if (-not [string]::IsNullOrWhiteSpace([string]$run.label)) {
        $run | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $OutputRoot "$($run.label)-summary.json") -Encoding utf8
    }
}
if ($null -ne $critical) {
    $critical | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $OutputRoot 'critical-path.json') -Encoding utf8
    Write-Host "CRITICAL_PATH_JSON $($critical | ConvertTo-Json -Depth 12 -Compress)"
}
if ($hydration.Count -gt 0) {
    $hydration | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $OutputRoot 'hydration-profile.json') -Encoding utf8
}
if ($Mode -eq 'Warm') {
    foreach ($label in @('warm-prime', 'warm-measured')) {
        $tree = Get-Sha256Tree (Join-Path $OutputRoot $label)
        if ($null -eq $tree) {
            throw "Profile output directory was not created: $label"
        }
        $tree | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $OutputRoot "$label-tree.json") -Encoding utf8
    }
} elseif ($Mode -eq 'Cold') {
    $tree = Get-Sha256Tree (Join-Path $OutputRoot 'cold')
    if ($null -eq $tree) {
        throw 'Profile output directory was not created: cold'
    }
    $tree | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $OutputRoot 'cold-tree.json') -Encoding utf8
}
Write-Host "PROFILE_WRAPPER_JSON $($wrapper | ConvertTo-Json -Depth 12 -Compress)"
