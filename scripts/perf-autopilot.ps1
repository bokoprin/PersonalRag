[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Init', 'Evaluate', 'Status', 'Finalize', 'SelfTest')]
    [string]$Action,

    [string]$RunId = '',

    [ValidateSet('All', 'BestA', 'Candidate')]
    [string]$Phase = 'All',

    [ValidateRange(0, 5)]
    [int]$Iteration = 0,

    [string]$CandidatePath = '',
    [string]$SourceRoot = 'C:\Program Files',
    [switch]$DiagnosticPair,
    [switch]$StopCodexUsageLimit
)

# This script is intentionally a stateful executor, not a source-code optimizer.  A human or
# coding agent writes one candidate after BEST A has been captured; the executor owns the
# measurement ordering, identity checks, commits, and safe rollback.
$ErrorActionPreference = 'Stop'

$ScriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepositoryRoot = Split-Path -Parent $ScriptRoot
$AutopilotRoot = Join-Path $env:LOCALAPPDATA 'PersonalRag\perf-autopilot'

function Assert-Condition([bool]$Condition, [string]$Message) {
    if (-not $Condition) {
        throw $Message
    }
}

function Get-FullPath([string]$Path) {
    return [IO.Path]::GetFullPath($Path)
}

function Normalize-RelativePath([string]$Path) {
    return $Path.Replace('\', '/').TrimStart('/')
}

function Assert-PathUnderRoot([string]$Path, [string]$Root) {
    $fullPath = (Get-FullPath $Path).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    $fullRoot = (Get-FullPath $Root).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    $prefix = $fullRoot + [IO.Path]::DirectorySeparatorChar
    if (-not $fullPath.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase) -and
        -not $fullPath.Equals($fullRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing a path outside the allowed root: $fullPath"
    }
    return $fullPath
}

function Write-Json([object]$Value, [string]$Path) {
    $parent = Split-Path -Parent $Path
    [IO.Directory]::CreateDirectory($parent) | Out-Null
    $temporary = "$Path.$PID.$([guid]::NewGuid().ToString('N')).tmp"
    $json = $Value | ConvertTo-Json -Depth 48
    [IO.File]::WriteAllText($temporary, $json, (New-Object Text.UTF8Encoding($false)))
    Move-Item -LiteralPath $temporary -Destination $Path -Force
}

function Set-ObjectProperty([object]$Object, [string]$Name, [object]$Value) {
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) {
        $Object | Add-Member -NotePropertyName $Name -NotePropertyValue $Value
    } else {
        $property.Value = $Value
    }
}

function Read-Json([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Required JSON file was not found: $Path"
    }
    return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
}

function Get-StringSha256([string]$Value) {
    $hasher = [Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [Text.Encoding]::UTF8.GetBytes($Value)
        return ([BitConverter]::ToString($hasher.ComputeHash($bytes))).Replace('-', '').ToLowerInvariant()
    } finally {
        $hasher.Dispose()
    }
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

function Invoke-Git([string]$Repository, [string[]]$Arguments) {
    # Git legitimately writes progress (for example, "Preparing worktree") to
    # stderr.  Windows PowerShell promotes that redirected native stderr into a
    # terminating NativeCommandError when ErrorActionPreference is Stop, so keep
    # the wrapper's own error policy from changing a successful Git exit code.
    $previousErrorActionPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        # Keep Git's local conversion warning off the data stream.  A warning
        # emitted by `git diff --name-only` must never be mistaken for a changed
        # path when the candidate snapshot is frozen.
        $lines = @(& git -c core.safecrlf=false -C $Repository @Arguments 2>&1)
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    if ($exitCode -ne 0) {
        throw "git failed ($exitCode): git -c core.safecrlf=false -C $Repository $($Arguments -join ' ')`n$($lines -join "`n")"
    }
    return $lines
}

function Get-GitSingle([string]$Repository, [string[]]$Arguments) {
    return (@(Invoke-Git $Repository $Arguments) -join "`n").Trim()
}

function Assert-CleanWorktree([string]$Repository, [string]$Label) {
    $status = @(Invoke-Git $Repository @('status', '--porcelain=v1'))
    if ($status.Count -ne 0) {
        throw "$Label worktree must be clean before this operation:`n$($status -join "`n")"
    }
}

function Get-RunRoot([string]$Id) {
    if ([string]::IsNullOrWhiteSpace($Id)) {
        throw 'RunId is required for this action.'
    }
    if ($Id -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]{2,120}$') {
        throw 'RunId contains unsupported characters.'
    }
    return Join-Path $AutopilotRoot $Id
}

function Get-StatePath([string]$Root) {
    return Join-Path $Root 'state.json'
}

function Save-State([object]$State, [string]$Root) {
    Set-ObjectProperty $State 'updatedAtUtc' ([DateTime]::UtcNow.ToString('o'))
    Write-Json $State (Get-StatePath $Root)
}

function Load-State([string]$Root) {
    return Read-Json (Get-StatePath $Root)
}

function Stop-Run([string]$Root, [string]$Reason, [string]$Detail = '') {
    $statePath = Get-StatePath $Root
    if (Test-Path -LiteralPath $statePath -PathType Leaf) {
        $state = Load-State $Root
    } else {
        $state = [pscustomobject][ordered]@{
            schemaVersion = 1
            runId = Split-Path -Leaf $Root
            createdAtUtc = [DateTime]::UtcNow.ToString('o')
            rounds = @()
        }
    }
    Set-ObjectProperty $state 'status' 'stopped'
    Set-ObjectProperty $state 'stopReason' $Reason
    Set-ObjectProperty $state 'stopDetail' $Detail
    Save-State $state $Root
}

function Assert-NoApiKeys([string]$Root) {
    $openAiPresent = -not [string]::IsNullOrEmpty([Environment]::GetEnvironmentVariable('OPENAI_API_KEY'))
    $codexPresent = -not [string]::IsNullOrEmpty([Environment]::GetEnvironmentVariable('CODEX_API_KEY'))
    if ($openAiPresent -or $codexPresent) {
        Stop-Run $Root 'STOP_API_KEY_PRESENT' 'OPENAI_API_KEY or CODEX_API_KEY exists; values were not read or recorded.'
        throw 'STOP_API_KEY_PRESENT'
    }
    if ($StopCodexUsageLimit) {
        Stop-Run $Root 'STOP_CODEX_USAGE_LIMIT' 'The executor was explicitly asked to preserve a usage-limit stop state.'
        throw 'STOP_CODEX_USAGE_LIMIT'
    }
}

function Get-TreeManifest([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) {
        throw "Tree root was not found: $Path"
    }
    $root = (Get-FullPath $Path).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    $entries = @(
        Get-ChildItem -LiteralPath $root -File -Recurse -Force |
            ForEach-Object {
                $relative = Normalize-RelativePath $_.FullName.Substring($root.Length).TrimStart([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
                [pscustomobject][ordered]@{
                    relativePath = $relative
                    size = [Int64]$_.Length
                    sha256 = Get-FileSha256 $_.FullName
                }
            } |
            Sort-Object relativePath
    )
    $canonical = (($entries | ForEach-Object { "$($_.relativePath)`t$($_.size)`t$($_.sha256)" }) -join "`n")
    return [pscustomobject][ordered]@{
        root = $root
        files = $entries
        treeSha256 = Get-StringSha256 $canonical
    }
}

function Compare-Tree([object]$Left, [object]$Right) {
    $leftFiles = @($Left.files | Sort-Object relativePath)
    $rightFiles = @($Right.files | Sort-Object relativePath)
    if ($leftFiles.Count -ne $rightFiles.Count) {
        return [pscustomobject]@{ identical = $false; reason = "file count differs: $($leftFiles.Count) vs $($rightFiles.Count)"; leftTreeSha256 = $Left.treeSha256; rightTreeSha256 = $Right.treeSha256 }
    }
    for ($index = 0; $index -lt $leftFiles.Count; $index++) {
        $left = $leftFiles[$index]
        $right = $rightFiles[$index]
        if (-not $left.relativePath.Equals([string]$right.relativePath, [StringComparison]::Ordinal) -or
            [Int64]$left.size -ne [Int64]$right.size -or
            -not $left.sha256.Equals([string]$right.sha256, [StringComparison]::OrdinalIgnoreCase)) {
            return [pscustomobject]@{ identical = $false; reason = "first difference at position $index"; leftTreeSha256 = $Left.treeSha256; rightTreeSha256 = $Right.treeSha256 }
        }
    }
    return [pscustomobject]@{ identical = $true; reason = 'relative path, file size, and SHA-256 all match'; leftTreeSha256 = $Left.treeSha256; rightTreeSha256 = $Right.treeSha256 }
}

function Get-EnvironmentTuple([object]$Measured) {
    return [pscustomobject][ordered]@{
        sourceFiles = [Int64]$Measured.sourceFiles
        processedFiles = [Int64]$Measured.processedFiles
        indexedFiles = [Int64]$Measured.indexedFiles
        bytesRead = [Int64]$Measured.bytesRead
    }
}

function Compare-Environment([object]$Left, [object]$Right) {
    $differences = [System.Collections.Generic.List[string]]::new()
    foreach ($name in @('sourceFiles', 'processedFiles', 'indexedFiles', 'bytesRead')) {
        if ([Int64]$Left.$name -ne [Int64]$Right.$name) {
            $differences.Add("${name}: $($Left.$name) vs $($Right.$name)")
        }
    }
    return [pscustomobject]@{ identical = ($differences.Count -eq 0); differences = @($differences) }
}

function Get-PairedRepresentativeMs([double]$First, [double]$Second) {
    return ($First + $Second) / 2.0
}

function Get-MedianMs([double[]]$Values) {
    if ($Values.Count -eq 0) {
        throw 'Median requires at least one value.'
    }
    $ordered = @($Values | Sort-Object)
    $middle = [int][Math]::Floor($ordered.Count / 2)
    if (($ordered.Count % 2) -eq 1) {
        return [double]$ordered[$middle]
    }
    return ([double]$ordered[$middle - 1] + [double]$ordered[$middle]) / 2.0
}

function Test-PairedAcceptance([double]$BestA, [double]$CandidateA, [double]$CandidateB, [double]$BestB) {
    $bestRepresentative = Get-PairedRepresentativeMs $BestA $BestB
    $candidateRepresentative = Get-PairedRepresentativeMs $CandidateA $CandidateB
    return [pscustomobject][ordered]@{
        accepted = ($CandidateA -lt $BestA -and $CandidateB -lt $BestB -and $candidateRepresentative -le ($bestRepresentative * 0.97))
        bestRepresentativeMs = $bestRepresentative
        candidateRepresentativeMs = $candidateRepresentative
        improvementPercent = (($bestRepresentative - $candidateRepresentative) / $bestRepresentative) * 100.0
        pairAWon = ($CandidateA -lt $BestA)
        pairBWon = ($CandidateB -lt $BestB)
    }
}

function Get-MeasurementRecord([object]$Measurement) {
    return [pscustomobject][ordered]@{
        tag = $Measurement.tag
        scoreMs = [double]$Measurement.scoreMs
        profileDir = $Measurement.profileDir
        wrapperPath = $Measurement.wrapperPath
        treeManifestPath = $Measurement.treeManifestPath
        treeSha256 = $Measurement.tree.treeSha256
        warmPrimeTreeSha256 = $Measurement.primeTree.treeSha256
        environment = $Measurement.environment
        measured = $Measurement.measured
    }
}

function Invoke-ProfileConfig([string]$Repository, [string]$Root, [string]$RunRoot) {
    $profileDirectory = Join-Path $RunRoot 'config'
    [IO.Directory]::CreateDirectory($profileDirectory) | Out-Null
    $wrapperPath = Join-Path $profileDirectory 'profile-wrapper.json'
    $profileScript = Join-Path $Repository 'scripts\profile-index-build.ps1'
    Assert-Condition (Test-Path -LiteralPath $profileScript -PathType Leaf) "Profile script was not found in $Repository"
    & $profileScript -Mode Config -Root $Root -OutputRoot (Join-Path $profileDirectory 'output') -LogPath (Join-Path $profileDirectory 'raw.log') -SummaryPath $wrapperPath
    if (-not (Test-Path -LiteralPath $wrapperPath -PathType Leaf)) {
        throw 'Config profile did not create its wrapper JSON.'
    }
    $wrapper = Read-Json $wrapperPath
    if ([int]$wrapper.schemaVersion -ne 2) {
        throw "Unexpected profile config schemaVersion: $($wrapper.schemaVersion)"
    }
    return $wrapper
}

function Invoke-WarmProfile([string]$Repository, [string]$Root, [string]$RunRoot, [string]$Tag, [string]$FrozenConfigPath) {
    $profileDirectory = Join-Path (Join-Path $RunRoot 'profiles') $Tag
    if (Test-Path -LiteralPath $profileDirectory) {
        throw "Measurement directory already exists; refusing to overwrite evidence: $profileDirectory"
    }
    [IO.Directory]::CreateDirectory($profileDirectory) | Out-Null
    $outputRoot = Join-Path $profileDirectory 'output'
    $wrapperPath = Join-Path $profileDirectory 'profile-wrapper.json'
    $profileScript = Join-Path $Repository 'scripts\profile-index-build.ps1'
    Assert-Condition (Test-Path -LiteralPath $profileScript -PathType Leaf) "Profile script was not found in $Repository"
    & $profileScript -Mode Warm -Root $Root -FrozenConfigPath $FrozenConfigPath -OutputRoot $outputRoot -LogPath (Join-Path $profileDirectory 'raw.log') -SummaryPath $wrapperPath
    if (-not (Test-Path -LiteralPath $wrapperPath -PathType Leaf)) {
        throw "Warm profile did not create its wrapper JSON: $Tag"
    }
    $wrapper = Read-Json $wrapperPath
    if ([int]$wrapper.schemaVersion -ne 2 -or $null -eq $wrapper.summary -or $null -eq $wrapper.summary.measured) {
        throw "Warm profile emitted incomplete schema-v2 JSON: $Tag"
    }
    $primeManifestPath = Join-Path $outputRoot 'warm-prime-tree.json'
    $measuredManifestPath = Join-Path $outputRoot 'warm-measured-tree.json'
    $primeTree = Read-Json $primeManifestPath
    $tree = Read-Json $measuredManifestPath
    $warmIdentity = Compare-Tree $primeTree $tree
    if (-not $warmIdentity.identical) {
        Stop-Run $RunRoot 'STOP_DETERMINISM_FAILURE' "warm-prime and warm-measured trees differ for $Tag ($($warmIdentity.reason))"
        throw "INVALID_DETERMINISM: warm-prime and warm-measured trees differ for $Tag ($($warmIdentity.reason))"
    }
    $measured = $wrapper.summary.measured
    $measurement = [pscustomobject][ordered]@{
        tag = $Tag
        profileDir = $profileDirectory
        wrapperPath = $wrapperPath
        treeManifestPath = $measuredManifestPath
        primeTreeManifestPath = $primeManifestPath
        tree = $tree
        primeTree = $primeTree
        warmIdentity = $warmIdentity
        measured = $measured
        environment = Get-EnvironmentTuple $measured
        scoreMs = ([double]$measured.callWallMs + [double]$measured.verifyWallMs)
    }
    Write-Json (Get-MeasurementRecord $measurement) (Join-Path $profileDirectory 'measurement.json')
    return $measurement
}

function Get-CanonicalTree([object]$State) {
    return Read-Json $State.canonicalTree.manifestPath
}

function Test-CanonicalMeasurement([object]$State, [object]$Measurement) {
    $environment = Compare-Environment $State.canonicalEnvironment $Measurement.environment
    $tree = Compare-Tree (Get-CanonicalTree $State) $Measurement.tree
    return [pscustomobject]@{ environment = $environment; tree = $tree; valid = ($environment.identical -and $tree.identical) }
}

function Get-FreshBestMeasurement([object]$State, [string]$RunRoot, [string]$Tag) {
    for ($attempt = 1; $attempt -le 2; $attempt++) {
        $measurement = Invoke-WarmProfile $State.bestWorktree $State.sourceRoot $RunRoot "$Tag-attempt-$attempt" $State.frozenConfigPath
        $validation = Test-CanonicalMeasurement $State $measurement
        if ($validation.valid) {
            return $measurement
        }
        if ($attempt -eq 2) {
            $reason = if (-not $validation.environment.identical) { 'STOP_CORPUS_CHANGED' } else { 'STOP_CANONICAL_TREE_MISMATCH' }
            Stop-Run $RunRoot $reason "Fresh current-best does not match canonical output. Environment: $($validation.environment.differences -join '; '); tree: $($validation.tree.reason)"
            throw $reason
        }
    }
}

function Get-CandidateSnapshot([string]$Repository, [string]$CurrentBestSha) {
    $staged = @(Invoke-Git $Repository @('diff', '--cached', '--name-only'))
    if ($staged.Count -ne 0) {
        throw 'Candidate files must be unstaged until measurement and explicit ACCEPT staging.'
    }
    $patch = @(Invoke-Git $Repository @('diff', '--binary', $CurrentBestSha)) -join "`n"
    $trackedPaths = @(
        Invoke-Git $Repository @('diff', '--name-only', $CurrentBestSha) |
            ForEach-Object { Normalize-RelativePath $_ } |
            Sort-Object -Unique
    )
    $untrackedPaths = @(
        Invoke-Git $Repository @('ls-files', '--others', '--exclude-standard') |
            ForEach-Object { Normalize-RelativePath $_ } |
            Sort-Object -Unique
    )
    $untracked = @(
        foreach ($relative in $untrackedPaths) {
            $full = Assert-PathUnderRoot (Join-Path $Repository $relative) $Repository
            if (-not (Test-Path -LiteralPath $full -PathType Leaf)) {
                throw "Untracked candidate entry is not a regular file: $relative"
            }
            [pscustomobject][ordered]@{
                path = $relative
                sha256 = Get-FileSha256 $full
            }
        }
    )
    $allPaths = @($trackedPaths + $untrackedPaths | Sort-Object -Unique)
    $material = [ordered]@{ patch = $patch; untracked = $untracked } | ConvertTo-Json -Depth 12 -Compress
    return [pscustomobject][ordered]@{
        snapshotSha256 = Get-StringSha256 $material
        trackedPaths = $trackedPaths
        untracked = $untracked
        paths = $allPaths
    }
}

function Assert-SnapshotUnchanged([object]$Before, [string]$Repository, [string]$CurrentBestSha, [string]$RunRoot, [int]$Round) {
    $after = Get-CandidateSnapshot $Repository $CurrentBestSha
    if (-not $Before.snapshotSha256.Equals([string]$after.snapshotSha256, [StringComparison]::OrdinalIgnoreCase)) {
        $state = Load-State $RunRoot
        $state.rounds = @($state.rounds | Where-Object { [int]$_.iteration -ne $Round })
        $state.status = 'candidate_mutated_restart_required'
        Set-ObjectProperty $state 'lastInvalidation' ([pscustomobject]@{
            iteration = $Round
            reason = 'candidate diff changed after Candidate A; all measurements discarded and a new BEST A is required'
            beforeSnapshotSha256 = $Before.snapshotSha256
            afterSnapshotSha256 = $after.snapshotSha256
        })
        Save-State $state $RunRoot
        throw 'INVALID_CANDIDATE_MUTATION_AFTER_A: measurements were discarded; rerun Evaluate -Phase BestA after restoring the intended candidate state.'
    }
}

function Get-CandidateAllowedPaths([object]$Candidate) {
    $allowed = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($item in @($Candidate.filesExpectedToChange)) {
        $path = if ($item -is [string]) { $item } else { [string]$item.path }
        if (-not [string]::IsNullOrWhiteSpace($path)) {
            $allowed.Add((Normalize-RelativePath $path)) | Out-Null
        }
    }
    foreach ($item in @($Candidate.addedAfterImplementationStarted)) {
        $path = [string]$item.path
        if (-not [string]::IsNullOrWhiteSpace($path)) {
            $allowed.Add((Normalize-RelativePath $path)) | Out-Null
        }
    }
    return $allowed
}

function Assert-ValidCandidate([object]$Candidate, [object]$State, [int]$Round) {
    Assert-Condition ([int]$Candidate.iteration -eq $Round) "candidate.json iteration must be $Round."
    Assert-Condition (([string]$Candidate.currentBestSha).Equals([string]$State.currentBestSha, [StringComparison]::OrdinalIgnoreCase)) 'candidate.json currentBestSha does not match current best.'
    $hypotheses = @($Candidate.hypotheses)
    Assert-Condition ($hypotheses.Count -ge 1 -and $hypotheses.Count -le 3) 'candidate.json must record one to three hypotheses before implementation.'
    Assert-Condition (-not [string]::IsNullOrWhiteSpace([string]$Candidate.selectedHypothesisId)) 'candidate.json must select exactly one hypothesis.'
    $selected = @($hypotheses | Where-Object { ([string]$_.id).Equals([string]$Candidate.selectedHypothesisId, [StringComparison]::Ordinal) })
    Assert-Condition ($selected.Count -eq 1) 'selectedHypothesisId must identify exactly one recorded hypothesis.'
    Assert-Condition ((Get-CandidateAllowedPaths $Candidate).Count -gt 0) 'candidate.json filesExpectedToChange must not be empty.'
}

function Assert-CandidateCoverage([object]$Snapshot, [object]$Candidate) {
    $allowed = Get-CandidateAllowedPaths $Candidate
    foreach ($path in @($Snapshot.paths)) {
        if (-not $allowed.Contains($path)) {
            throw "Candidate changed an undeclared file: $path. Update candidate.json before Candidate A, then obtain a fresh BEST A."
        }
    }
    Assert-Condition ($Snapshot.paths.Count -gt 0) 'Candidate has no source changes to evaluate.'
}

function Restore-CandidateSnapshot([string]$Repository, [string]$CurrentBestSha, [object]$Snapshot) {
    $tracked = @($Snapshot.trackedPaths)
    if ($tracked.Count -gt 0) {
        Invoke-Git $Repository (@('restore', "--source=$CurrentBestSha", '--worktree', '--') + $tracked) | Out-Null
    }
    foreach ($entry in @($Snapshot.untracked)) {
        $full = Assert-PathUnderRoot (Join-Path $Repository ([string]$entry.path)) $Repository
        if (Test-Path -LiteralPath $full -PathType Leaf) {
            Remove-Item -LiteralPath $full -Force
        }
    }
    $remaining = @(Invoke-Git $Repository @('status', '--porcelain=v1'))
    if ($remaining.Count -ne 0) {
        throw "Explicit candidate rollback left unexpected changes; refusing broader cleanup:`n$($remaining -join "`n")"
    }
}

function Invoke-NativeInRepository([string]$Repository, [string]$Command, [string[]]$Arguments, [string]$Label) {
    Push-Location $Repository
    try {
        & $Command @Arguments
        $exitCode = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    if ($exitCode -ne 0) {
        throw "$Label failed with exit code $exitCode."
    }
}

function Invoke-TargetedTests([string]$Repository, [object]$Candidate) {
    $tests = @($Candidate.targetedTests)
    if ($tests.Count -eq 0) {
        Invoke-NativeInRepository $Repository 'cargo' @('+1.97.1', 'test', '--manifest-path', 'bridge-core/Cargo.toml', '--locked', 'production_build_config_resolver_preserves_every_legacy_setting') 'default resolver targeted test'
        Invoke-NativeInRepository $Repository 'cargo' @('+1.97.1', 'test', '--manifest-path', 'search-core/Cargo.toml', '--locked', '--test', 'retained_hydration') 'default hydration targeted test'
        return
    }
    foreach ($test in $tests) {
        $arguments = @('+1.97.1') + @($test.arguments | ForEach-Object { [string]$_ })
        Assert-Condition ($arguments.Count -gt 1) 'Each targetedTests entry needs a non-empty arguments array.'
        Invoke-NativeInRepository $Repository 'cargo' $arguments "targeted test $($test.name)"
    }
}

function Invoke-QualityGates([string]$Repository, [object]$Candidate = $null) {
    Invoke-Git $Repository @('diff', '--check') | Out-Null
    if ($null -ne $Candidate) {
        Invoke-TargetedTests $Repository $Candidate
    }
    foreach ($manifest in @('search-core/Cargo.toml', 'bridge-core/Cargo.toml', 'src-tauri/Cargo.toml')) {
        Invoke-NativeInRepository $Repository 'cargo' @('+1.97.1', 'fmt', '--manifest-path', $manifest, '--', '--check') "format $manifest"
    }
    foreach ($manifest in @('search-core/Cargo.toml', 'bridge-core/Cargo.toml', 'src-tauri/Cargo.toml')) {
        Invoke-NativeInRepository $Repository 'cargo' @('+1.97.1', 'clippy', '--manifest-path', $manifest, '--locked', '--all-targets', '--', '-D', 'warnings') "clippy $manifest"
    }
    Invoke-NativeInRepository $Repository 'cargo' @('+1.97.1', 'test', '--manifest-path', 'search-core/Cargo.toml', '--locked') 'search-core full regression'
    Invoke-NativeInRepository $Repository 'cargo' @('+1.97.1', 'test', '--manifest-path', 'bridge-core/Cargo.toml', '--locked') 'bridge-core full regression'
    Invoke-NativeInRepository $Repository 'cargo' @('+1.97.1', 'test', '--manifest-path', 'src-tauri/Cargo.toml', '--locked') 'src-tauri full regression'
    & (Join-Path $Repository 'scripts\verify-and-build-windows.ps1')
    if (-not $?) {
        throw 'scripts/verify-and-build-windows.ps1 failed.'
    }
    return [pscustomobject][ordered]@{
        diffCheck = $true
        targetedTest = ($null -ne $Candidate)
        format = $true
        clippy = $true
        fullRegression = $true
        deterministic = $true
        durabilityCorruption = $true
        windowsBuild = $true
    }
}

function Set-Round([object]$State, [object]$Round) {
    $State.rounds = @(@($State.rounds | Where-Object { [int]$_.iteration -ne [int]$Round.iteration }) + $Round)
}

function Add-HistoryRecord([string]$RunRoot, [object]$Round) {
    $path = Join-Path $RunRoot 'history.jsonl'
    $line = $Round | ConvertTo-Json -Depth 48 -Compress
    [IO.File]::AppendAllText($path, "$line`n", (New-Object Text.UTF8Encoding($false)))
}

function Format-Measurement([object]$Measurement) {
    if ($null -eq $Measurement) {
        return 'not collected'
    }
    return ('{0:N3} ms' -f [double]$Measurement.scoreMs)
}

function Format-Environment([object]$Environment) {
    if ($null -eq $Environment) {
        return 'not collected'
    }
    return "sourceFiles=$($Environment.sourceFiles), processedFiles=$($Environment.processedFiles), indexedFiles=$($Environment.indexedFiles), bytesRead=$($Environment.bytesRead)"
}

function Write-IterationReport([string]$RunRoot, [object]$Round) {
    $reports = Join-Path $RunRoot 'reports'
    [IO.Directory]::CreateDirectory($reports) | Out-Null
    $paired = $Round.paired
    $bestRepresentative = if ($null -ne $paired) { '{0:N3} ms' -f [double]$paired.bestRepresentativeMs } else { 'not available' }
    $candidateRepresentative = if ($null -ne $paired) { '{0:N3} ms' -f [double]$paired.candidateRepresentativeMs } else { 'not available' }
    $improvement = if ($null -ne $paired) { '{0:N3}%' -f [double]$paired.improvementPercent } else { 'not available' }
    $body = @"
# Performance autopilot iteration $($Round.iteration)

Decision: **$($Round.decision)**

## Fresh comparison

Best A: $(Format-Measurement $Round.bestA)
Candidate A: $(Format-Measurement $Round.candidateA)
Candidate B: $(Format-Measurement $Round.candidateB)
Best B: $(Format-Measurement $Round.bestB)

Best representative: $bestRepresentative
Candidate representative: $candidateRepresentative
Improvement: $improvement

## Environment comparison

BEST A: $(Format-Environment $Round.bestA.environment)
CANDIDATE A: $(Format-Environment $Round.candidateA.environment)
CANDIDATE B: $(Format-Environment $Round.candidateB.environment)
BEST B: $(Format-Environment $Round.bestB.environment)

## Byte identity

bestTreeSha256: $($Round.bestA.treeSha256)
candidateTreeSha256: $($Round.candidateA.treeSha256)
identical: $($Round.byteIdentity.identical)
reason: $($Round.byteIdentity.reason)

## Decision confidence

sample count: $($Round.sampleCount)
measurement consistency: $($Round.measurementConsistency)
noise observed: $($Round.noiseObserved)
reason: $($Round.reason)

## Learning

$($Round.learning)
"@
    [IO.File]::WriteAllText((Join-Path $reports "iteration-$($Round.iteration).md"), $body, (New-Object Text.UTF8Encoding($false)))
}

function Complete-Rejection([object]$State, [string]$RunRoot, [object]$Round, [object]$Snapshot, [string]$Reason, [string]$Learning) {
    if ($null -ne $Snapshot) {
        Restore-CandidateSnapshot $State.candidateWorktree $State.currentBestSha $Snapshot
    }
    $Round.decision = 'REJECT'
    $Round.reason = $Reason
    $Round.learning = $Learning
    $Round.completedAtUtc = [DateTime]::UtcNow.ToString('o')
    Set-Round $State $Round
    $State.rejected = [int]$State.rejected + 1
    $State.nextIteration = [int]$Round.iteration + 1
    $State.status = if ($State.nextIteration -gt 5) { 'ready_to_finalize' } else { 'ready' }
    Write-IterationReport $RunRoot $Round
    Add-HistoryRecord $RunRoot $Round
    Save-State $State $RunRoot
}

function Complete-Acceptance([object]$State, [string]$RunRoot, [object]$Round, [object]$Snapshot, [object]$Candidate) {
    $paths = @($Snapshot.paths)
    Invoke-Git $State.candidateWorktree (@('add', '--') + $paths) | Out-Null
    Invoke-Git $State.candidateWorktree @('diff', '--cached', '--check') | Out-Null
    $message = [string]$Candidate.commitMessage
    if ([string]::IsNullOrWhiteSpace($message)) {
        $message = "性能最適化: Iteration $($Round.iteration) の検証済み改善"
    }
    if ($message.Contains([Environment]::NewLine) -or $message.Contains([string][char]13) -or $message.Contains([string][char]10)) {
        throw 'commitMessage must be a single line.'
    }
    Invoke-Git $State.candidateWorktree @('commit', '-m', $message) | Out-Null
    $newSha = Get-GitSingle $State.candidateWorktree @('rev-parse', 'HEAD')
    Assert-CleanWorktree $State.candidateWorktree 'candidate'
    Assert-CleanWorktree $State.bestWorktree 'current-best'
    Invoke-Git $State.bestWorktree @('switch', '--detach', $newSha) | Out-Null
    $Round.decision = 'ACCEPT'
    $Round.reason = 'Both fresh pairs won, the arithmetic representative improved by at least 3%, and every required gate passed.'
    $Round.learning = [string]$Candidate.learning
    if ([string]::IsNullOrWhiteSpace($Round.learning)) {
        $Round.learning = 'The accepted hypothesis improved the primary score under fresh paired comparison.'
    }
    $Round.commit = $newSha
    $Round.completedAtUtc = [DateTime]::UtcNow.ToString('o')
    $State.currentBestSha = $newSha
    $State.canonicalEnvironment = $Round.candidateB.environment
    $State.canonicalTree = [pscustomobject]@{ treeSha256 = $Round.candidateB.treeSha256; manifestPath = $Round.candidateB.treeManifestPath }
    Set-Round $State $Round
    $State.accepted = [int]$State.accepted + 1
    $State.nextIteration = [int]$Round.iteration + 1
    $State.status = if ($State.nextIteration -gt 5) { 'ready_to_finalize' } else { 'ready' }
    Write-IterationReport $RunRoot $Round
    Add-HistoryRecord $RunRoot $Round
    Save-State $State $RunRoot
}

function Invoke-HarnessSelfTest([string]$RunRoot) {
    $testRoot = Join-Path $RunRoot ("self-test-" + [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmssfff'))
    $corpus = Join-Path $testRoot 'corpus'
    $left = Join-Path $testRoot 'tree-left'
    $right = Join-Path $testRoot 'tree-right'
    [IO.Directory]::CreateDirectory($corpus) | Out-Null
    [IO.Directory]::CreateDirectory($left) | Out-Null
    [IO.Directory]::CreateDirectory($right) | Out-Null
    [IO.File]::WriteAllText((Join-Path $corpus 'a.txt'), 'alpha benchmark corpus')
    [IO.File]::WriteAllText((Join-Path $corpus 'b.txt'), 'beta benchmark corpus')
    [IO.File]::WriteAllText((Join-Path $left 'same.bin'), 'same')
    [IO.File]::WriteAllText((Join-Path $right 'same.bin'), 'same')

    Assert-Condition ((Get-PairedRepresentativeMs 100.0 120.0) -eq 110.0) 'paired representative self-test failed.'
    Assert-Condition ((Get-MedianMs @(1.0, 2.0, 100.0)) -eq 2.0) 'median self-test failed.'
    Assert-Condition ((Test-PairedAcceptance 100.0 97.0 97.0 100.0).accepted) 'exact 3% paired threshold self-test failed.'
    Assert-Condition (-not (Test-PairedAcceptance 100.0 96.0 101.0 100.0).accepted) 'one-pair-loss self-test failed.'
    Assert-Condition ((Get-PairedRepresentativeMs 100.1 100.2) -eq 100.15) 'paired representative must not round.'
    $sameTree = Compare-Tree (Get-TreeManifest $left) (Get-TreeManifest $right)
    Assert-Condition $sameTree.identical 'tree equality self-test failed.'
    [IO.File]::WriteAllText((Join-Path $right 'same.bin'), 'different')
    Assert-Condition (-not (Compare-Tree (Get-TreeManifest $left) (Get-TreeManifest $right)).identical) 'tree difference self-test failed.'
    $sameEnvironment = Compare-Environment ([pscustomobject]@{ sourceFiles = 2; processedFiles = 2; indexedFiles = 2; bytesRead = 10 }) ([pscustomobject]@{ sourceFiles = 2; processedFiles = 2; indexedFiles = 2; bytesRead = 10 })
    $differentEnvironment = Compare-Environment ([pscustomobject]@{ sourceFiles = 2; processedFiles = 2; indexedFiles = 2; bytesRead = 10 }) ([pscustomobject]@{ sourceFiles = 3; processedFiles = 2; indexedFiles = 2; bytesRead = 10 })
    Assert-Condition $sameEnvironment.identical 'environment equality self-test failed.'
    Assert-Condition (-not $differentEnvironment.identical) 'environment difference self-test failed.'

    $gitRoot = Join-Path $testRoot 'temporary-git'
    [IO.Directory]::CreateDirectory($gitRoot) | Out-Null
    Invoke-Git $gitRoot @('init') | Out-Null
    Invoke-Git $gitRoot @('config', 'user.email', 'perf-autopilot@example.invalid') | Out-Null
    Invoke-Git $gitRoot @('config', 'user.name', 'PersonalRag perf self-test') | Out-Null
    Invoke-Git $gitRoot @('config', 'core.autocrlf', 'input') | Out-Null
    Invoke-Git $gitRoot @('config', 'core.safecrlf', 'true') | Out-Null
    [IO.File]::WriteAllText((Join-Path $gitRoot 'tracked.txt'), "base`n")
    Invoke-Git $gitRoot @('add', '--', 'tracked.txt') | Out-Null
    Invoke-Git $gitRoot @('commit', '-m', 'self-test base') | Out-Null
    $baseSha = Get-GitSingle $gitRoot @('rev-parse', 'HEAD')
    [IO.File]::WriteAllText((Join-Path $gitRoot 'tracked.txt'), "candidate`r`n")
    [IO.File]::WriteAllText((Join-Path $gitRoot 'untracked.txt'), 'candidate')
    $snapshot = Get-CandidateSnapshot $gitRoot $baseSha
    Assert-Condition (@($snapshot.trackedPaths).Count -eq 1 -and [string]$snapshot.trackedPaths[0] -eq 'tracked.txt') 'candidate snapshot self-test included a non-path Git warning.'
    Restore-CandidateSnapshot $gitRoot $baseSha $snapshot
    Assert-CleanWorktree $gitRoot 'self-test rollback'
    Assert-Condition ((Get-Content -LiteralPath (Join-Path $gitRoot 'tracked.txt') -Raw).Trim() -eq 'base') 'explicit rollback self-test did not restore the tracked file.'
    Assert-Condition (-not (Test-Path -LiteralPath (Join-Path $gitRoot 'untracked.txt'))) 'explicit rollback self-test did not remove the declared untracked file.'

    $sampleJson = 'PROFILE_SUMMARY_JSON {"schemaVersion":2,"measured":{"sourceFiles":2,"processedFiles":2,"indexedFiles":2,"bytesRead":10,"callWallMs":1.25,"verifyWallMs":0.75}}'
    $payload = $sampleJson.Substring('PROFILE_SUMMARY_JSON '.Length) | ConvertFrom-Json
    Assert-Condition ([int]$payload.schemaVersion -eq 2 -and [double]$payload.measured.callWallMs -eq 1.25) 'profile JSON extraction self-test failed.'

    # Exercise the real profile wrapper on a synthetic corpus.  The collector-disabled control
    # uses the same profile-enabled binary with PR_PROFILE_BUILD unset; it is evidence about the
    # collector's overhead, while the Cargo feature boundary keeps collectors out of normal GUI
    # builds entirely.
    $syntheticConfig = Invoke-ProfileConfig $RepositoryRoot $corpus $testRoot
    $syntheticFrozenPath = Join-Path $testRoot 'synthetic-frozen-config.json'
    Write-Json $syntheticConfig.config.frozenBenchmarkConfig $syntheticFrozenPath
    $profileScript = Join-Path $RepositoryRoot 'scripts\profile-index-build.ps1'
    $profileEnabledRoot = Join-Path $testRoot 'instrumentation-enabled'
    $profileDisabledRoot = Join-Path $testRoot 'instrumentation-disabled'
    & $profileScript -Mode Warm -Root $corpus -FrozenConfigPath $syntheticFrozenPath -OutputRoot $profileEnabledRoot -LogPath (Join-Path $profileEnabledRoot 'raw.log') -SummaryPath (Join-Path $profileEnabledRoot 'profile-wrapper.json')
    & $profileScript -Mode Warm -DisableInstrumentation -Root $corpus -FrozenConfigPath $syntheticFrozenPath -OutputRoot $profileDisabledRoot -LogPath (Join-Path $profileDisabledRoot 'raw.log') -SummaryPath (Join-Path $profileDisabledRoot 'profile-wrapper.json')
    $enabledWrapper = Read-Json (Join-Path $profileEnabledRoot 'profile-wrapper.json')
    $disabledWrapper = Read-Json (Join-Path $profileDisabledRoot 'profile-wrapper.json')
    Assert-Condition ([int]$enabledWrapper.schemaVersion -eq 2 -and [bool]$enabledWrapper.profileInstrumentationEnabled) 'enabled profile instrumentation self-test failed.'
    Assert-Condition ([int]$disabledWrapper.schemaVersion -eq 2 -and -not [bool]$disabledWrapper.profileInstrumentationEnabled) 'disabled profile instrumentation self-test failed.'
    $instrumentedTree = Read-Json (Join-Path $profileEnabledRoot 'warm-measured-tree.json')
    $controlTree = Read-Json (Join-Path $profileDisabledRoot 'warm-measured-tree.json')
    $instrumentationIdentity = Compare-Tree $instrumentedTree $controlTree
    Assert-Condition $instrumentationIdentity.identical 'profile collector changed synthetic persisted bytes.'
    $instrumentationImpact = [pscustomobject][ordered]@{
        schemaVersion = 1
        corpus = $corpus
        collectorEnabledScoreMs = ([double]$enabledWrapper.summary.measured.callWallMs + [double]$enabledWrapper.summary.measured.verifyWallMs)
        collectorDisabledScoreMs = ([double]$disabledWrapper.summary.measured.callWallMs + [double]$disabledWrapper.summary.measured.verifyWallMs)
        treeIdentical = $instrumentationIdentity.identical
        note = 'Synthetic collector impact only; no performance acceptance decision uses this control measurement.'
    }
    $instrumentationImpactPath = Join-Path $testRoot 'instrumentation-impact.json'
    Write-Json $instrumentationImpact $instrumentationImpactPath

    $artifactRoot = Join-Path $testRoot 'state-artifacts'
    $dummyRound = [pscustomobject][ordered]@{
        iteration = 99; decision = 'SELF_TEST'; bestA = $null; candidateA = $null; candidateB = $null; bestB = $null
        paired = $null; byteIdentity = [pscustomobject]@{ identical = $true; reason = 'self-test' }; sampleCount = 0
        measurementConsistency = 'self-test'; noiseObserved = 'none'; reason = 'self-test'; learning = 'self-test'
    }
    $dummyState = [pscustomobject][ordered]@{ schemaVersion = 1; runId = Split-Path -Leaf $RunRoot; rounds = @(); status = 'self-test' }
    Save-State $dummyState $artifactRoot
    Add-HistoryRecord $artifactRoot $dummyRound
    Write-IterationReport $artifactRoot $dummyRound
    $finalPath = Join-Path $artifactRoot 'reports\self-test-final-report.md'
    [IO.File]::WriteAllText($finalPath, "# Self-test final report`n", (New-Object Text.UTF8Encoding($false)))
    Assert-Condition (Test-Path -LiteralPath (Get-StatePath $artifactRoot)) 'state generation self-test failed.'
    Assert-Condition (Test-Path -LiteralPath (Join-Path $artifactRoot 'history.jsonl')) 'history generation self-test failed.'
    Assert-Condition (Test-Path -LiteralPath (Join-Path $artifactRoot 'reports\iteration-99.md')) 'iteration report generation self-test failed.'
    Assert-Condition (Test-Path -LiteralPath $finalPath) 'final report generation self-test failed.'

    $report = @"
# Performance autopilot harness self-test

PASS: real profile JSON extraction, arithmetic median/mean and paired threshold, SHA-256 tree equality/difference, environment classification, explicit-path candidate rollback, state/history/iteration/final report generation.

The synthetic profile collector control preserved output bytes. Its enabled/disabled timing evidence is recorded at `$instrumentationImpactPath`.

The temporary corpus and temporary Git repository are retained beneath this run's artifact root for auditability.
"@
    $reportPath = Join-Path $RunRoot 'reports\harness-self-test.md'
    [IO.Directory]::CreateDirectory((Split-Path -Parent $reportPath)) | Out-Null
    [IO.File]::WriteAllText($reportPath, $report, (New-Object Text.UTF8Encoding($false)))
    return [pscustomobject]@{ passed = $true; root = $testRoot; reportPath = $reportPath }
}

function Initialize-Autopilot([string]$EffectiveRunId) {
    $runRoot = Get-RunRoot $EffectiveRunId
    [IO.Directory]::CreateDirectory($runRoot) | Out-Null
    Assert-NoApiKeys $runRoot
    $expectedSource = 'C:\Program Files'.TrimEnd('\')
    $actualSource = (Get-FullPath $SourceRoot).TrimEnd('\')
    Assert-Condition ($actualSource.Equals($expectedSource, [StringComparison]::OrdinalIgnoreCase)) 'The production benchmark corpus must be exactly C:\Program Files and is read-only by design.'
    Assert-Condition (Test-Path -LiteralPath $actualSource -PathType Container) 'C:\Program Files was not found.'
    $statePath = Get-StatePath $runRoot
    Assert-Condition (-not (Test-Path -LiteralPath $statePath -PathType Leaf)) "Run already exists: $runRoot"
    Assert-CleanWorktree $RepositoryRoot 'candidate bootstrap'
    $branch = Get-GitSingle $RepositoryRoot @('branch', '--show-current')
    Assert-Condition ($branch -like 'perf/autopilot-5round-*') 'Init must run from the dedicated perf/autopilot-5round branch.'
    $startSha = Get-GitSingle $RepositoryRoot @('rev-parse', 'HEAD')
    $bestWorktree = Join-Path $runRoot 'best-worktree'
    [IO.Directory]::CreateDirectory((Split-Path -Parent $bestWorktree)) | Out-Null
    Invoke-Git $RepositoryRoot @('worktree', 'add', '--detach', $bestWorktree, $startSha) | Out-Null
    $state = [pscustomobject][ordered]@{
        schemaVersion = 1
        runId = $EffectiveRunId
        createdAtUtc = [DateTime]::UtcNow.ToString('o')
        status = 'initializing'
        sourceRoot = $actualSource
        candidateWorktree = (Get-FullPath $RepositoryRoot)
        bestWorktree = (Get-FullPath $bestWorktree)
        branch = $branch
        startSha = $startSha
        currentBestSha = $startSha
        frozenConfigPath = ''
        frozenBenchmarkConfig = $null
        initialBaseline = $null
        canonicalEnvironment = $null
        canonicalTree = $null
        nextIteration = 1
        accepted = 0
        rejected = 0
        rounds = @()
        anomalies = @()
        stopReason = $null
        stopDetail = $null
        harnessSelfTest = $null
        finalMeasurement = $null
        finalReport = $null
        lastInvalidation = $null
    }
    Save-State $state $runRoot
    $selfTest = Invoke-HarnessSelfTest $runRoot
    $state = Load-State $runRoot
    Set-ObjectProperty $state 'harnessSelfTest' $selfTest
    $config = Invoke-ProfileConfig $RepositoryRoot $actualSource $runRoot
    $frozen = $config.config.frozenBenchmarkConfig
    foreach ($name in @('hydrationWorkers', 'buildWorkers', 'segmentDocs', 'maxFileBytes', 'hydrationBatchBytes')) {
        Assert-Condition ([UInt64]$frozen.$name -gt 0) "Resolver did not concretely resolve $name."
    }
    $frozenPath = Join-Path $runRoot 'frozen-benchmark-config.json'
    Write-Json $frozen $frozenPath
    $state.frozenBenchmarkConfig = $frozen
    $state.frozenConfigPath = $frozenPath
    Save-State $state $runRoot
    $state = Load-State $runRoot
    $baseline = Invoke-WarmProfile $state.bestWorktree $actualSource $runRoot 'initial-baseline' $frozenPath
    $state.initialBaseline = Get-MeasurementRecord $baseline
    $state.canonicalEnvironment = $baseline.environment
    $state.canonicalTree = [pscustomobject]@{ treeSha256 = $baseline.tree.treeSha256; manifestPath = $baseline.treeManifestPath }
    $state.status = 'ready'
    Save-State $state $runRoot
    Write-Host "AUTOPILOT_INITIALIZED runId=$EffectiveRunId branch=$branch baselineScoreMs=$('{0:N3}' -f $baseline.scoreMs)"
}

function Start-BestA([object]$State, [string]$RunRoot, [int]$RoundNumber) {
    Assert-Condition ($RoundNumber -eq [int]$State.nextIteration) "Expected iteration $($State.nextIteration), not $RoundNumber."
    Assert-Condition ($RoundNumber -ge 1 -and $RoundNumber -le 5) 'Iteration must be in 1..5.'
    $existing = @($State.rounds | Where-Object { [int]$_.iteration -eq $RoundNumber })
    if ($existing.Count -ne 0) {
        throw "Iteration $RoundNumber already has saved state ($($existing[0].status))."
    }
    $bestA = Get-FreshBestMeasurement $State $RunRoot "iteration-$RoundNumber-best-a"
    $round = [pscustomobject][ordered]@{
        iteration = $RoundNumber
        status = 'best_a_ready'
        currentBestSha = $State.currentBestSha
        bestA = Get-MeasurementRecord $bestA
        candidateA = $null
        candidateB = $null
        bestB = $null
        candidateC = $null
        bestC = $null
        paired = $null
        byteIdentity = [pscustomobject]@{ identical = $false; reason = 'candidate not measured' }
        sampleCount = 1
        measurementConsistency = 'BEST A matches canonical current-best environment and tree.'
        noiseObserved = 'none'
        decision = 'PENDING'
        reason = 'Awaiting profile analysis, candidate.json, and one candidate implementation.'
        learning = ''
        candidatePatchSha256 = $null
        candidateFiles = @()
        gates = $null
        commit = $null
        completedAtUtc = $null
    }
    Set-Round $State $round
    $State.status = 'best_a_ready'
    Save-State $State $RunRoot
    Write-Host "BEST_A_READY iteration=$RoundNumber scoreMs=$('{0:N3}' -f $bestA.scoreMs)"
}

function Continue-CandidateEvaluation([object]$State, [string]$RunRoot, [int]$RoundNumber, [string]$JsonPath) {
    Assert-Condition ($RoundNumber -eq [int]$State.nextIteration) "Expected iteration $($State.nextIteration), not $RoundNumber."
    $round = @($State.rounds | Where-Object { [int]$_.iteration -eq $RoundNumber }) | Select-Object -First 1
    Assert-Condition ($null -ne $round -and $round.status -eq 'best_a_ready') 'Run Evaluate -Phase BestA first, then analyze BEST A before implementing the candidate.'
    Assert-Condition (Test-Path -LiteralPath $JsonPath -PathType Leaf) "candidate.json was not found: $JsonPath"
    $candidate = Read-Json $JsonPath
    Assert-ValidCandidate $candidate $State $RoundNumber
    $savedCandidatePath = Join-Path $RunRoot "candidates\iteration-$RoundNumber-candidate.json"
    if (-not (Get-FullPath $JsonPath).Equals((Get-FullPath $savedCandidatePath), [StringComparison]::OrdinalIgnoreCase)) {
        Copy-Item -LiteralPath $JsonPath -Destination $savedCandidatePath -Force
    }
    $candidateHead = Get-GitSingle $State.candidateWorktree @('rev-parse', 'HEAD')
    Assert-Condition ($candidateHead.Equals([string]$State.currentBestSha, [StringComparison]::OrdinalIgnoreCase)) 'Candidate worktree HEAD must still be currentBestSha; do not checkout/reset it during paired evaluation.'
    $gates = Invoke-QualityGates $State.candidateWorktree $candidate
    $snapshot = Get-CandidateSnapshot $State.candidateWorktree $State.currentBestSha
    Assert-CandidateCoverage $snapshot $candidate
    $round.candidatePatchSha256 = $snapshot.snapshotSha256
    $round.candidateFiles = $snapshot.paths
    $round.gates = $gates
    $candidateA = Invoke-WarmProfile $State.candidateWorktree $State.sourceRoot $RunRoot "iteration-$RoundNumber-candidate-a" $State.frozenConfigPath
    Assert-SnapshotUnchanged $snapshot $State.candidateWorktree $State.currentBestSha $RunRoot $RoundNumber
    $round.candidateA = Get-MeasurementRecord $candidateA
    $bestATree = Read-Json $round.bestA.treeManifestPath
    $environmentA = Compare-Environment $round.bestA.environment $candidateA.environment
    $identityA = Compare-Tree $bestATree $candidateA.tree
    $round.byteIdentity = $identityA
    $round.sampleCount = 2
    if (-not $environmentA.identical -or -not $identityA.identical) {
        $reason = if (-not $environmentA.identical) { "REJECT_ENVIRONMENT_MISMATCH: $($environmentA.differences -join '; ')" } else { "REJECT_BYTE_IDENTITY: $($identityA.reason)" }
        Complete-Rejection $State $RunRoot $round $snapshot $reason 'The candidate altered the corpus accounting or the persisted bytes, so its performance is not comparable.'
        Write-Host "ITERATION_REJECTED iteration=$RoundNumber reason=$reason"
        return
    }
    $improvementA = (([double]$round.bestA.scoreMs - [double]$candidateA.scoreMs) / [double]$round.bestA.scoreMs) * 100.0
    if ($improvementA -lt 2.5) {
        $round.measurementConsistency = 'Candidate A environment and tree match BEST A.'
        $round.noiseObserved = 'none; Candidate A is clearly below the 3% threshold before paired confirmation.'
        Complete-Rejection $State $RunRoot $round $snapshot ("REJECT_PRIMARY_SCORE_A={0:N3}% (< 2.5% clear-reject boundary)" -f $improvementA) 'This hypothesis did not improve the primary score enough to justify a second paired sample.'
        Write-Host "ITERATION_REJECTED iteration=$RoundNumber reason=clear-under-threshold"
        return
    }
    $candidateB = Invoke-WarmProfile $State.candidateWorktree $State.sourceRoot $RunRoot "iteration-$RoundNumber-candidate-b" $State.frozenConfigPath
    Assert-SnapshotUnchanged $snapshot $State.candidateWorktree $State.currentBestSha $RunRoot $RoundNumber
    $round.candidateB = Get-MeasurementRecord $candidateB
    $bestB = Get-FreshBestMeasurement $State $RunRoot "iteration-$RoundNumber-best-b"
    $round.bestB = Get-MeasurementRecord $bestB
    $environmentB = Compare-Environment $bestB.environment $candidateB.environment
    $identityB = Compare-Tree $bestB.tree $candidateB.tree
    if (-not $environmentB.identical -or -not $identityB.identical) {
        $round.byteIdentity = $identityB
        $reason = if (-not $environmentB.identical) { "REJECT_ENVIRONMENT_MISMATCH_B: $($environmentB.differences -join '; ')" } else { "REJECT_BYTE_IDENTITY_B: $($identityB.reason)" }
        Complete-Rejection $State $RunRoot $round $snapshot $reason 'The second fresh pair is not semantically identical, so its timing cannot be accepted.'
        Write-Host "ITERATION_REJECTED iteration=$RoundNumber reason=$reason"
        return
    }
    $round.byteIdentity = $identityB
    $paired = Test-PairedAcceptance ([double]$round.bestA.scoreMs) ([double]$candidateA.scoreMs) ([double]$candidateB.scoreMs) ([double]$bestB.scoreMs)
    $round.paired = $paired
    $round.sampleCount = 4
    $round.measurementConsistency = 'BEST A/B each match canonical current-best; candidate A/B each match their fresh BEST environment and byte tree.'
    $round.noiseObserved = if ($improvementA -lt 3.0) { 'Candidate A is within 0.5 percentage points of the threshold; paired B was required.' } else { 'none' }
    if ($DiagnosticPair -and $improvementA -ge 2.5 -and $improvementA -lt 3.0) {
        $candidateC = Invoke-WarmProfile $State.candidateWorktree $State.sourceRoot $RunRoot "iteration-$RoundNumber-candidate-c" $State.frozenConfigPath
        Assert-SnapshotUnchanged $snapshot $State.candidateWorktree $State.currentBestSha $RunRoot $RoundNumber
        $bestC = Get-FreshBestMeasurement $State $RunRoot "iteration-$RoundNumber-best-c"
        $round.candidateC = Get-MeasurementRecord $candidateC
        $round.bestC = Get-MeasurementRecord $bestC
        $round.sampleCount = 6
        $round.noiseObserved = 'Boundary diagnostic C pair collected; it is recorded only and does not weaken the A/B ACCEPT rule.'
    }
    if ($paired.accepted) {
        Complete-Acceptance $State $RunRoot $round $snapshot $candidate
        Write-Host "ITERATION_ACCEPTED iteration=$RoundNumber improvementPercent=$('{0:N3}' -f $paired.improvementPercent)"
    } else {
        $reason = "REJECT_PAIRED: representative=$('{0:N3}' -f $paired.improvementPercent)% pairAWin=$($paired.pairAWon) pairBWin=$($paired.pairBWon)"
        Complete-Rejection $State $RunRoot $round $snapshot $reason 'The candidate did not beat current-best in both fresh pairs by the required representative threshold.'
        Write-Host "ITERATION_REJECTED iteration=$RoundNumber reason=paired"
    }
}

function Write-FinalReport([object]$State, [string]$RunRoot, [object]$FinalMeasurement) {
    $baseline = [double]$State.initialBaseline.scoreMs
    $best = [double]$FinalMeasurement.scoreMs
    $improvement = (($baseline - $best) / $baseline) * 100.0
    $anomalies = @($State.anomalies)
    if ($anomalies.Count -eq 0) {
        $anomalyText = 'None recorded. Every accepted/rejected decision used fresh BEST/CANDIDATE comparison and canonical corpus/tree checks.'
    } else {
        $anomalyText = ($anomalies | ForEach-Object { "- $_" }) -join "`n"
    }
    $body = @"
# PersonalRag performance autopilot final report

## Result

Initial baseline score: $('{0:N3}' -f $baseline) ms
Final best score: $('{0:N3}' -f $best) ms
Overall improvement: $('{0:N3}' -f $improvement)%
Accepted: $($State.accepted)
Rejected: $($State.rejected)
Best commit: $($State.currentBestSha)

## Measurement methodology

- Frozen benchmark config: `$($State.frozenConfigPath)`
- Warm-prime then warm-measured was used for every sample; their SHA-256 trees had to match before the sample was eligible.
- Each iteration used fresh BEST A, CANDIDATE A, CANDIDATE B, then fresh BEST B from a detached current-best worktree.
- The primary score is `callWallMs + verifyWallMs`; the two-sample representative is the unrounded arithmetic mean `(A + B) / 2`.
- ACCEPT threshold: candidate representative at least 3% lower than current-best and both individual pairs must win.
- sourceFiles, processedFiles, indexedFiles, bytesRead, relative paths, file sizes, and SHA-256 output trees were checked on every comparison.

## Measurement anomalies

$anomalyText

## Iteration decisions

$(($State.rounds | Sort-Object iteration | ForEach-Object { "- Iteration $($_.iteration): $($_.decision) — $($_.reason)" }) -join "`n")
"@
    $path = Join-Path $RunRoot 'reports\final-report.md'
    [IO.File]::WriteAllText($path, $body, (New-Object Text.UTF8Encoding($false)))
    return [pscustomobject]@{ path = $path; baselineScoreMs = $baseline; bestScoreMs = $best; improvementPercent = $improvement }
}

function Finalize-Autopilot([object]$State, [string]$RunRoot) {
    Assert-Condition ([int]$State.nextIteration -eq 6) 'Finalize requires exactly five completed iterations.'
    Assert-Condition (([int]$State.accepted + [int]$State.rejected) -eq 5) 'Finalize requires exactly five ACCEPT/REJECT decisions.'
    Assert-CleanWorktree $State.bestWorktree 'current-best final'
    Invoke-QualityGates $State.bestWorktree | Out-Null
    $finalMeasurement = Get-FreshBestMeasurement $State $RunRoot 'final-best'
    $report = Write-FinalReport $State $RunRoot $finalMeasurement
    $State.status = 'complete'
    $State.finalMeasurement = Get-MeasurementRecord $finalMeasurement
    $State.finalReport = $report.path
    Save-State $State $RunRoot
    Write-Host 'AUTOPILOT_COMPLETE'
    Write-Host 'iterations=5'
    Write-Host "accepted=$($State.accepted)"
    Write-Host "rejected=$($State.rejected)"
    Write-Host "baselineScoreMs=$('{0:N3}' -f $report.baselineScoreMs)"
    Write-Host "bestScoreMs=$('{0:N3}' -f $report.bestScoreMs)"
    Write-Host "improvementPercent=$('{0:N3}' -f $report.improvementPercent)"
    Write-Host "branch=$($State.branch)"
    Write-Host "bestCommit=$($State.currentBestSha)"
    Write-Host "report=$($report.path)"
}

if ([string]::IsNullOrWhiteSpace($RunId) -and $Action -eq 'Init') {
    $RunId = 'run-' + [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss')
}

if ($Action -eq 'Init') {
    Initialize-Autopilot $RunId
    exit 0
}

$runRoot = Get-RunRoot $RunId
Assert-NoApiKeys $runRoot
if ($Action -eq 'SelfTest') {
    $result = Invoke-HarnessSelfTest $runRoot
    Write-Host "HARNESS_SELF_TEST_PASS report=$($result.reportPath)"
    exit 0
}

$state = Load-State $runRoot
if ($state.status -eq 'stopped') {
    throw "Run is stopped: $($state.stopReason)"
}
switch ($Action) {
    'Status' {
        [pscustomobject]@{
            runId = $state.runId
            status = $state.status
            branch = $state.branch
            currentBestSha = $state.currentBestSha
            nextIteration = $state.nextIteration
            accepted = $state.accepted
            rejected = $state.rejected
            frozenBenchmarkConfig = $state.frozenBenchmarkConfig
            finalReport = $state.finalReport
        } | ConvertTo-Json -Depth 12
    }
    'Evaluate' {
        Assert-Condition ($Iteration -ge 1 -and $Iteration -le 5) 'Evaluate requires -Iteration 1..5.'
        if ($Phase -eq 'BestA' -or $Phase -eq 'All') {
            Start-BestA $state $runRoot $Iteration
            if ($Phase -eq 'BestA') {
                exit 0
            }
            $state = Load-State $runRoot
        }
        if ([string]::IsNullOrWhiteSpace($CandidatePath)) {
            $CandidatePath = Join-Path $runRoot "candidates\iteration-$Iteration-candidate.json"
        }
        Continue-CandidateEvaluation $state $runRoot $Iteration $CandidatePath
    }
    'Finalize' {
        Finalize-Autopilot $state $runRoot
    }
}
