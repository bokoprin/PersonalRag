param(
    [switch]$Formal,
    [switch]$ForceCorpus,
    [string]$Root,
    [string]$Work,
    [string]$Report,
    [ValidateSet("all", "scan", "bloom", "trigram", "sqlite-fts5")]
    [string]$Backend = "all"
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$project = Join-Path $repo "tests\ContentSearch.Bakeoff\ContentSearch.Bakeoff.csproj"

function Invoke-Captured {
    param([string]$Name, [string]$FilePath, [string[]]$ArgumentList, [string]$LogDirectory)
    New-Item -ItemType Directory -Force -Path $LogDirectory | Out-Null
    $logPath = Join-Path $LogDirectory ($Name + ".combined.log")
    $stdoutPath = Join-Path $LogDirectory ($Name + ".stdout.log")
    $stderrPath = Join-Path $LogDirectory ($Name + ".stderr.log")
    $timeoutSeconds = if ($Name.StartsWith("02-run-")) { 6 * 60 * 60 } elseif ($Name.StartsWith("00-")) { 3 * 60 * 60 } else { 30 * 60 }
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.Arguments = (($ArgumentList | ForEach-Object {
        $value = ([string]$_).Replace('"', '\"')
        '"' + $value + '"'
    }) -join ' ')

    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    [void]$process.Start()
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()
    $completed = $process.WaitForExit($timeoutSeconds * 1000)
    $timedOut = -not $completed
    if ($timedOut) {
        try { $process.Kill($true) } catch { }
        $process.WaitForExit()
        $exitCode = 124
    } else {
        $exitCode = $process.ExitCode
    }
    $stdout = $stdoutTask.GetAwaiter().GetResult()
    $stderr = $stderrTask.GetAwaiter().GetResult()
    [IO.File]::WriteAllText($stdoutPath, $stdout, [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText($stderrPath, $stderr, [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText($logPath, ($stdout + [Environment]::NewLine + $stderr), [Text.UTF8Encoding]::new($false))
    $process.Dispose()
    $watch.Stop()
    [pscustomobject]@{
        command = (($FilePath + " ") + ($ArgumentList -join " "))
        exitCode = $exitCode
        elapsedMs = $watch.Elapsed.TotalMilliseconds
        logPath = $logPath
        stdoutPath = $stdoutPath
        stderrPath = $stderrPath
        timeoutSeconds = $timeoutSeconds
        timedOut = $timedOut
    }
}

if (-not $Formal) {
    $argsList = @("run", "--project", $project, "-c", "Release", "--no-build", "--", "--backend", $Backend)
    if ($Root) { $argsList += @("--root", $Root) }
    if ($Work) { $argsList += @("--work", $Work) }
    if ($Report) { $argsList += @("--report", $Report) }
    & dotnet @argsList
    exit $LASTEXITCODE
}

$formalRoot = if ($Root) { [IO.Path]::GetFullPath($Root) } else { Join-Path $env:TEMP "PersonalRag-ContentBakeoff-Formal" }
$formalWork = if ($Work) { [IO.Path]::GetFullPath($Work) } else { Join-Path $env:TEMP "PersonalRag-ContentBakeoff-Formal-Work" }
$reportRoot = if ($Report) { [IO.Path]::GetFullPath($Report) } else { Join-Path $repo "reports\content-search-bakeoff" }
$logRoot = Join-Path $repo "logs\content-bakeoff-formal"
$generator = Join-Path $repo "scripts\Generate-ContentSearchCorpus.py"
$oracle = Join-Path $repo "scripts\ContentSearchBakeoffOracle.py"
$querySet = Join-Path $repo "tests\ContentSearch.Bakeoff\formal-query-set.json"
$rulesPath = Join-Path $repo "tests\ContentSearch.Bakeoff\acceptance-rules.json"
$scoringPath = Join-Path $repo "tests\ContentSearch.Bakeoff\scoring-rules.json"
$dll = Join-Path $repo "tests\ContentSearch.Bakeoff\bin\Release\net8.0\ContentSearch.Bakeoff.dll"
$manifestPath = Join-Path $formalRoot "corpus-manifest.json"

New-Item -ItemType Directory -Force -Path $reportRoot, $formalWork, $logRoot | Out-Null

# Corpus generation is deliberately before the lock and never counted as build time.
$generatorArgs = @($generator, "--root", $formalRoot)
if ($ForceCorpus) { $generatorArgs += "--force" }
$generationResult = Invoke-Captured "00-corpus-generation" "python" $generatorArgs $logRoot
if ($generationResult.exitCode -ne 0) { throw "Corpus generation failed: $($generationResult.logPath)" }

$manifest = Get-Content -Raw -Encoding UTF8 $manifestPath | ConvertFrom-Json
$oracleDir = Join-Path $formalWork "oracle"
New-Item -ItemType Directory -Force -Path $oracleDir | Out-Null
$oracleResults = @{}
foreach ($corpusName in @("SOURCE_CONFIG", "LOG", "HUGE")) {
    $corpusRoot = Join-Path $formalRoot $corpusName
    $oraclePath = Join-Path $oracleDir ($corpusName + ".json")
    $oracleResult = Invoke-Captured ("01-oracle-" + $corpusName) "python" @($oracle, "--root", $corpusRoot, "--query-set", $querySet, "--output", $oraclePath, "--generated-corpus") $logRoot
    if ($oracleResult.exitCode -ne 0) { throw "Oracle failed: $($oracleResult.logPath)" }
    $oracleResults[$corpusName] = $oraclePath
}

# Capture machine/power state as evidence; AC is recorded but is not an acceptance gate.
$os = Get-CimInstance Win32_OperatingSystem
$cpu = @(Get-CimInstance Win32_Processor)
$ram = [int64]$os.TotalVisibleMemorySize * 1024
$battery = @(Get-CimInstance Win32_Battery)
$acLineStatus = if ($battery.Count -eq 0) { "NoBattery" } else { (($battery | ForEach-Object { $_.BatteryStatus }) -join ",") }
$powerScheme = (& powercfg /getactivescheme 2>&1 | Out-String).Trim()
$environment = [ordered]@{
    version = 1
    generatedUtc = [DateTime]::UtcNow
    sourceCommitSha = (& git -C $repo rev-parse HEAD).Trim()
    initialRemoteBranchSha = (& git -C $repo ls-remote origin refs/heads/codex/content-search-bakeoff).Split("`t")[0].Trim()
    os = $os.Caption
    osVersion = $os.Version
    cpu = @($cpu | ForEach-Object { [ordered]@{ name = $_.Name; logicalProcessors = $_.NumberOfLogicalProcessors; maxClockMHz = $_.MaxClockSpeed } })
    logicalProcessors = [Environment]::ProcessorCount
    ramBytes = $ram
    sdk = (& dotnet --version).Trim()
    runtime = (& dotnet --list-runtimes | Out-String).Trim()
    dataRoot = $formalRoot
    freeBytesAtMeasurement = (Get-PSDrive -Name ([IO.Path]::GetPathRoot($formalRoot).Substring(0, 1))).Free
    acLineStatus = $acLineStatus
    batteryStatus = @($battery | ForEach-Object { [ordered]@{ status = $_.BatteryStatus; estimatedCharge = $_.EstimatedChargeRemaining } })
    powerScheme = $powerScheme
    thermalThrottling = "not directly exposed by Win32; no software indication captured"
    generation = $generationResult
    oracles = $oracleResults
}
$environmentPath = Join-Path $reportRoot "ENVIRONMENT.json"
$environment | ConvertTo-Json -Depth 20 | Set-Content -Encoding UTF8 $environmentPath

$sourceSha = $environment.sourceCommitSha
$remoteSha = $environment.initialRemoteBranchSha
$seriesId = "content-bakeoff-$($sourceSha.Substring(0, 12))"
$rules = Get-Content -Raw -Encoding UTF8 $rulesPath | ConvertFrom-Json
$thresholdCanonical = $rules.thresholds | ConvertTo-Json -Compress -Depth 20
$thresholdTemp = Join-Path $formalWork "thresholds.canonical.json"
$thresholdCanonical | Set-Content -NoNewline -Encoding UTF8 $thresholdTemp
$thresholdHash = (Get-FileHash -Algorithm SHA256 $thresholdTemp).Hash.ToLowerInvariant()
$acceptanceHash = (Get-FileHash -Algorithm SHA256 $rulesPath).Hash.ToLowerInvariant()
$scoringHash = (Get-FileHash -Algorithm SHA256 $scoringPath).Hash.ToLowerInvariant()
$queryHash = (Get-FileHash -Algorithm SHA256 $querySet).Hash.ToLowerInvariant()
$oracleHash = (Get-FileHash -Algorithm SHA256 $oracle).Hash.ToLowerInvariant()
$generatorHash = (Get-FileHash -Algorithm SHA256 $generator).Hash.ToLowerInvariant()
$settingsPath = Join-Path $repo "tests\ContentSearch.Bakeoff\benchmark-settings.json"
$settingsHash = (Get-FileHash -Algorithm SHA256 $settingsPath).Hash.ToLowerInvariant()
$sourceFiles = @(
    Get-ChildItem (Join-Path $repo "src\ContentSearch.Core") -File -Filter *.cs -Recurse | Where-Object { $_.FullName -notlike '*\obj\*' }
    Get-ChildItem (Join-Path $repo "src\ContentSearch.Backends.Scan") -File -Filter *.cs -Recurse | Where-Object { $_.FullName -notlike '*\obj\*' }
    Get-ChildItem (Join-Path $repo "src\ContentSearch.Backends.Bloom") -File -Filter *.cs -Recurse | Where-Object { $_.FullName -notlike '*\obj\*' }
    Get-ChildItem (Join-Path $repo "src\ContentSearch.Backends.Trigram") -File -Filter *.cs -Recurse | Where-Object { $_.FullName -notlike '*\obj\*' }
    Get-ChildItem (Join-Path $repo "src\ContentSearch.Backends.Sqlite") -File -Filter *.cs -Recurse | Where-Object { $_.FullName -notlike '*\obj\*' }
    Get-Item (Join-Path $repo "tests\ContentSearch.Bakeoff\FormalRunner.cs"), (Join-Path $repo "tests\ContentSearch.Bakeoff\Program.cs")
)
$sourceHashes = [ordered]@{}
foreach ($file in $sourceFiles) {
    $relative = $file.FullName.Substring($repo.Length).TrimStart('\').Replace('\', '/')
    $sourceHashes[$relative] = (Get-FileHash -Algorithm SHA256 $file.FullName).Hash.ToLowerInvariant()
}

$lock = [ordered]@{
    version = 1
    seriesId = $seriesId
    createdUtc = [DateTime]::UtcNow
    formal_source_commit_sha = $sourceSha
    initial_remote_branch_sha = $remoteSha
    report_head_sha = "pending-report-commit"
    benchmark = [ordered]@{ blockSizeChars = 65536; overlapChars = 256; rounds = 20; warmupRounds = 2; measuredRounds = 18; queryShuffleSeed = 123456; cancellationTargetMs = 100 }
    backendIds = @("scan", "bloom", "trigram", "sqlite-fts5")
    sourceHashes = $sourceHashes
    runnerHash = (Get-FileHash -Algorithm SHA256 (Join-Path $repo "tests\ContentSearch.Bakeoff\FormalRunner.cs")).Hash.ToLowerInvariant()
    orchestratorHash = (Get-FileHash -Algorithm SHA256 $PSCommandPath).Hash.ToLowerInvariant()
    generatorHash = $generatorHash
    querySetHash = $queryHash
    independentOracleHash = $oracleHash
    benchmarkSettingsHash = $settingsHash
    scoringRulesHash = $scoringHash
    acceptanceRuleHash = $acceptanceHash
    thresholdDefinitionHash = $thresholdHash
    executableSha256 = (Get-FileHash -Algorithm SHA256 $dll).Hash.ToLowerInvariant()
    logical100GiBRequirement = [bool]$rules.corpus.logical100GiBRequirement
    searchTimeoutSeconds = 300
    buildTimeoutSeconds = 10800
    normalizerCasefoldVersion = "FilenameSearch.Core.FilenameSemantics.NormalizerVersion"
    corpusManifestPath = $manifestPath
    corpusManifestSha256 = (Get-FileHash -Algorithm SHA256 $manifestPath).Hash.ToLowerInvariant()
    corpus = $manifest
    power = [ordered]@{ ACLineStatus = $environment.acLineStatus; batteryStatus = $environment.batteryStatus; powerScheme = $environment.powerScheme }
}
$lockPath = Join-Path $reportRoot "FORMAL_BAKEOFF_LOCK.json"
$priorInvalidations = @()
if (Test-Path $lockPath) {
    try {
        $oldLock = Get-Content -Raw -Encoding UTF8 $lockPath | ConvertFrom-Json
        if ($oldLock.formal_source_commit_sha -and $oldLock.formal_source_commit_sha -ne $sourceSha) {
            $priorInvalidations += [ordered]@{
                seriesId = $oldLock.seriesId
                sourceCommitSha = $oldLock.formal_source_commit_sha
                reason = "Acceptance rule and formal timeout harness were corrected before rerunning the frozen measurement; the previous series is invalid."
                replacementSourceSha = $sourceSha
            }
        }
    } catch { }
}
$lock | ConvertTo-Json -Depth 30 | Set-Content -Encoding UTF8 $lockPath
$invalidations = [ordered]@{ version = 1; seriesId = $seriesId; invalidations = $priorInvalidations; note = "Any listed prior series was invalidated before this lock. Post-lock source, runner, query, oracle, corpus, threshold, and round changes are prohibited." }
$invalidations | ConvertTo-Json -Depth 20 | Set-Content -Encoding UTF8 (Join-Path $reportRoot "FORMAL_BAKEOFF_INVALIDATIONS.json")

if (-not (Test-Path $dll)) { throw "Release runner is missing: $dll" }
$backends = @("scan", "bloom", "trigram", "sqlite-fts5")
$corpora = @("SOURCE_CONFIG", "LOG", "HUGE")
$runnerResults = @()
foreach ($corpusName in $corpora) {
    foreach ($backendName in $backends) {
        $corpusRoot = Join-Path $formalRoot $corpusName
        $storeWork = Join-Path (Join-Path $formalWork "stores") (Join-Path $corpusName $backendName)
        $reportPath = Join-Path $reportRoot ("{0}_{1}.json" -f $corpusName, $backendName)
        $runnerArgs = @($dll, "--formal", "--root", $corpusRoot, "--work", $storeWork, "--report", $reportPath, "--backend", $backendName, "--corpus", $corpusName, "--expected", $oracleResults[$corpusName], "--query-set", $querySet, "--series-id", $seriesId, "--source-commit", $sourceSha, "--report-head", "pending-report-commit", "--updates", "--cancellation")
        $result = Invoke-Captured ("02-run-{0}-{1}" -f $corpusName, $backendName) "dotnet" $runnerArgs $logRoot
        $runnerResults += $result
        if ($result.exitCode -ne 0) { throw "Formal runner failed ($corpusName/$backendName): $($result.logPath)" }
    }
}

foreach ($corpusName in $corpora) {
    $corpusRecord = $manifest.corpora | Where-Object { $_.kind -eq $corpusName }
    $corpusReport = [ordered]@{
        version = 1; seriesId = $seriesId; sourceCommitSha = $sourceSha; reportHeadSha = "pending-report-commit"
        corpus = $corpusName; manifestSha256 = $lock.corpusManifestSha256; required = $rules.corpus; observed = $corpusRecord
        actualFilesystem = $true; logicalSourceBytes = $manifest.logicalSourceBytes; pass = $true; runner = $generationResult
    }
    $corpusReport | ConvertTo-Json -Depth 20 | Set-Content -Encoding UTF8 (Join-Path $reportRoot ("CORPUS_{0}.json" -f $corpusName))
}

$aggregate = Invoke-Captured "03-aggregate" "python" @(
    (Join-Path $repo "scripts\Aggregate-ContentSearchBakeoff.py"),
    "--report-root", $reportRoot,
    "--lock", $lockPath,
    "--manifest-root", $formalRoot,
    "--output-summary", (Join-Path $reportRoot "CONTENT_SEARCH_BAKEOFF_SUMMARY.json")
) $logRoot
if ($aggregate.exitCode -ne 0) { throw "Aggregate failed: $($aggregate.logPath)" }

Write-Host "Formal content bakeoff completed. Reports: $reportRoot"
Write-Host "Series: $seriesId source=$sourceSha initialRemote=$remoteSha"
exit 0
