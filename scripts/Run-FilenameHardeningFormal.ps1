param(
    [string]$DataRoot = (Join-Path ([System.IO.Path]::GetTempPath()) 'PersonalRag-FilenameFormal'),
    [string]$ReportRoot = (Join-Path $PSScriptRoot '..\reports\filename-hardening'),
    [int]$Count = 1000000,
    [int]$ChurnSeconds = 1800,
    [int]$IdleSeconds = 600,
    [int]$SupplementalIdleSeconds = 10
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ($Count -lt 1000000) { throw 'Formal series requires at least 1,000,000 filesystem entries.' }
if ($ChurnSeconds -lt 1800) { throw 'Formal churn duration must be at least 1,800 seconds.' }
if ($IdleSeconds -lt 600) { throw 'Formal idle duration must be at least 600 seconds.' }

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$data = [System.IO.Path]::GetFullPath($DataRoot)
$reports = [System.IO.Path]::GetFullPath($ReportRoot)
$logs = Join-Path $reports 'logs'
New-Item -ItemType Directory -Force -Path $data, $reports, $logs | Out-Null

$operations = [System.Collections.Generic.List[object]]::new()

function Invoke-Captured {
    param(
        [Parameter(Mandatory=$true)][string]$FilePath,
        [Parameter(Mandatory=$false)][string[]]$ArgumentList = @(),
        [Parameter(Mandatory=$true)][string]$Name,
        [Parameter(Mandatory=$true)][string]$WorkingDirectory
    )
    $stdoutPath = Join-Path $logs ($Name + '.stdout.log')
    $stderrPath = Join-Path $logs ($Name + '.stderr.log')
    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $FilePath
    $psi.WorkingDirectory = $WorkingDirectory
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    # Windows PowerShell 5.1/.NET Framework does not expose ProcessStartInfo.ArgumentList.
    # Quote every argument explicitly; all formal paths and probe values are ordinary
    # command-line strings, and this keeps the captured command identical across hosts.
    $psi.Arguments = (($ArgumentList | ForEach-Object { '"' + ([string]$_).Replace('"','\"') + '"' }) -join ' ')
    $command = $FilePath + ' ' + (($ArgumentList | ForEach-Object { [string]$_ }) -join ' ')
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    $process = [System.Diagnostics.Process]::Start($psi)
    if ($null -eq $process) { throw "Failed to start: $command" }
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()
    $process.WaitForExit()
    $stdout = $stdoutTask.GetAwaiter().GetResult()
    $stderr = $stderrTask.GetAwaiter().GetResult()
    $watch.Stop()
    [System.IO.File]::WriteAllText($stdoutPath, $stdout, [System.Text.UTF8Encoding]::new($false))
    [System.IO.File]::WriteAllText($stderrPath, $stderr, [System.Text.UTF8Encoding]::new($false))
    $result = [pscustomobject][ordered]@{
        name = $Name
        command = $command
        exit_code = $process.ExitCode
        elapsed_seconds = $watch.Elapsed.TotalSeconds
        stdout_path = $stdoutPath
        stderr_path = $stderrPath
    }
    $operations.Add($result)
    return $result
}

function Get-Sha256([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-CanonicalJsonHash([object]$Value) {
    $canonical = $Value | ConvertTo-Json -Depth 100 -Compress
    $bytes = [System.Text.UTF8Encoding]::new($false).GetBytes($canonical)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { return (([BitConverter]::ToString($sha.ComputeHash($bytes))) -replace '-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
}

function Get-Percentile([double[]]$Values, [double]$P) {
    if ($Values.Count -eq 0) { return 0.0 }
    $sorted = @($Values | Sort-Object)
    $index = [Math]::Max(0, [Math]::Min($sorted.Count - 1, [int][Math]::Ceiling($sorted.Count * $P) - 1))
    return [double]$sorted[$index]
}

function Add-CommonReportFields([object]$Value, [string]$ReportPath, [object]$Operation, [string]$ExecutablePath, [string]$CorpusManifest) {
    $sourceSha = (& git -C $repo rev-parse HEAD).Trim()
    $Value | Add-Member -NotePropertyName source_commit_sha -NotePropertyValue $sourceSha -Force
    $Value | Add-Member -NotePropertyName report_head_sha -NotePropertyValue $sourceSha -Force
    $Value | Add-Member -NotePropertyName report_path -NotePropertyValue ([System.IO.Path]::GetFullPath($ReportPath)) -Force
    $Value | Add-Member -NotePropertyName command -NotePropertyValue $Operation.command -Force
    $Value | Add-Member -NotePropertyName exit_code -NotePropertyValue $Operation.exit_code -Force
    $Value | Add-Member -NotePropertyName elapsed_seconds -NotePropertyValue $Operation.elapsed_seconds -Force
    $Value | Add-Member -NotePropertyName stdout_path -NotePropertyValue $Operation.stdout_path -Force
    $Value | Add-Member -NotePropertyName stderr_path -NotePropertyValue $Operation.stderr_path -Force
    $Value | Add-Member -NotePropertyName initial_remote_branch_sha -NotePropertyValue $initialRemoteSha -Force
    $Value | Add-Member -NotePropertyName ACLineStatus -NotePropertyValue $power.ACLineStatus -Force
    $Value | Add-Member -NotePropertyName battery_status -NotePropertyValue $power.BatteryStatus -Force
    $Value | Add-Member -NotePropertyName charging -NotePropertyValue $power.Charging -Force
    $Value | Add-Member -NotePropertyName power_scheme -NotePropertyValue $power.PowerScheme -Force
    $Value | Add-Member -NotePropertyName thermal_throttling -NotePropertyValue $power.ThermalThrottling -Force
    $Value | Add-Member -NotePropertyName executable_sha256 -NotePropertyValue (Get-Sha256 $ExecutablePath) -Force
    if (Test-Path -LiteralPath $CorpusManifest) {
        $Value | Add-Member -NotePropertyName corpus_manifest_sha256 -NotePropertyValue (Get-Sha256 $CorpusManifest) -Force
    }
    $Value | Add-Member -NotePropertyName query_set_sha256 -NotePropertyValue (Get-Sha256 $querySetPath) -Force
    $Value | Add-Member -NotePropertyName independent_oracle_sha256 -NotePropertyValue (Get-Sha256 $oraclePath) -Force
    $Value | Add-Member -NotePropertyName ac_requirement_overridden -NotePropertyValue $true -Force
    $limitationValue = if ($Value.PSObject.Properties.Name -contains 'limitation') { $Value.limitation } else { $null }
    $Value | Add-Member -NotePropertyName informational_limitation -NotePropertyValue $limitationValue -Force
    $Value | Add-Member -NotePropertyName os -NotePropertyValue $machine.os -Force
    $Value | Add-Member -NotePropertyName cpu -NotePropertyValue $machine.cpu -Force
    $Value | Add-Member -NotePropertyName ram_bytes -NotePropertyValue $machine.ram_bytes -Force
    $Value | Add-Member -NotePropertyName dotnet_sdk -NotePropertyValue $machine.dotnet_sdk -Force
    $Value | Add-Member -NotePropertyName data_root -NotePropertyValue $data -Force
    $Value | Add-Member -NotePropertyName physical_disks -NotePropertyValue $machine.physical_disks -Force
    $Value | ConvertTo-Json -Depth 100 | Set-Content -LiteralPath $ReportPath -Encoding utf8
    return $Value
}

function Add-GuiReportFields([object]$Value, [string]$ReportPath, [object]$Operation) {
    $Value | Add-Member -NotePropertyName source_commit_sha -NotePropertyValue $sourceSha -Force
    $Value | Add-Member -NotePropertyName report_head_sha -NotePropertyValue $sourceSha -Force
    $Value | Add-Member -NotePropertyName report_path -NotePropertyValue ([System.IO.Path]::GetFullPath($ReportPath)) -Force
    $Value | Add-Member -NotePropertyName command -NotePropertyValue $Operation.command -Force
    $Value | Add-Member -NotePropertyName exit_code -NotePropertyValue $Operation.exit_code -Force
    $Value | Add-Member -NotePropertyName elapsed_seconds -NotePropertyValue $Operation.elapsed_seconds -Force
    $Value | Add-Member -NotePropertyName stdout_path -NotePropertyValue $Operation.stdout_path -Force
    $Value | Add-Member -NotePropertyName stderr_path -NotePropertyValue $Operation.stderr_path -Force
    $Value | Add-Member -NotePropertyName initial_remote_branch_sha -NotePropertyValue $initialRemoteSha -Force
    $Value | Add-Member -NotePropertyName executable_sha256 -NotePropertyValue (Get-Sha256 $guiExe) -Force
    $Value | Add-Member -NotePropertyName corpus_manifest_sha256 -NotePropertyValue (Get-Sha256 $corpusManifest) -Force
    $Value | Add-Member -NotePropertyName query_set_sha256 -NotePropertyValue (Get-Sha256 $querySetPath) -Force
    $Value | Add-Member -NotePropertyName independent_oracle_sha256 -NotePropertyValue (Get-Sha256 $oraclePath) -Force
    $Value | Add-Member -NotePropertyName ACLineStatus -NotePropertyValue $power.ACLineStatus -Force
    $Value | Add-Member -NotePropertyName battery_status -NotePropertyValue $power.BatteryStatus -Force
    $Value | Add-Member -NotePropertyName charging -NotePropertyValue $power.Charging -Force
    $Value | Add-Member -NotePropertyName power_scheme -NotePropertyValue $power.PowerScheme -Force
    $Value | Add-Member -NotePropertyName thermal_throttling -NotePropertyValue $power.ThermalThrottling -Force
    $Value | Add-Member -NotePropertyName ac_requirement_overridden -NotePropertyValue $true -Force
    $Value | Add-Member -NotePropertyName informational_limitation -NotePropertyValue $null -Force
    $Value | Add-Member -NotePropertyName os -NotePropertyValue $machine.os -Force
    $Value | Add-Member -NotePropertyName cpu -NotePropertyValue $machine.cpu -Force
    $Value | Add-Member -NotePropertyName ram_bytes -NotePropertyValue $machine.ram_bytes -Force
    $Value | Add-Member -NotePropertyName dotnet_sdk -NotePropertyValue $machine.dotnet_sdk -Force
    $Value | Add-Member -NotePropertyName data_root -NotePropertyValue $data -Force
    $Value | Add-Member -NotePropertyName physical_disks -NotePropertyValue $machine.physical_disks -Force
    return $Value
}

function Invoke-FormalMode([string]$Mode, [string[]]$ModeArguments, [string]$ReportName) {
    $reportPath = Join-Path $reports $ReportName
    $rawPath = Join-Path $logs ('raw-' + [System.IO.Path]::GetFileNameWithoutExtension($ReportName) + '.json')
    if ($ModeArguments.Count -lt 2) { throw "Formal mode $Mode requires root and store arguments." }
    $runnerArguments = @($formalDll, $Mode, $ModeArguments[0], $ModeArguments[1], $rawPath) + @($ModeArguments | Select-Object -Skip 2)
    $operation = Invoke-Captured -FilePath $dotnet -ArgumentList $runnerArguments -Name ('formal-' + $Mode) -WorkingDirectory $worktree
    if (-not (Test-Path -LiteralPath $rawPath)) {
        throw "Formal runner did not write $rawPath (exit $($operation.exit_code))."
    }
    $value = Get-Content -LiteralPath $rawPath -Raw | ConvertFrom-Json
    Add-CommonReportFields $value $reportPath $operation $guiExe $corpusManifest | Out-Null
    return [pscustomobject]@{ Value = $value; Path = $reportPath; Operation = $operation }
}

function Get-PowerState {
    $ac = $null
    $batteryStatus = $null
    $charging = $null
    try {
        $status = @(Get-CimInstance -Namespace 'root\wmi' -ClassName BatteryStatus -ErrorAction Stop | Select-Object -First 1)
        if ($status.Count -gt 0) { $ac = if ($status[0].PowerOnLine) { 1 } else { 0 }; $charging = [bool]$status[0].Charging }
    } catch {}
    try {
        $battery = @(Get-CimInstance Win32_Battery -ErrorAction Stop | Select-Object -First 1)
        if ($battery.Count -gt 0) { $batteryStatus = $battery[0].BatteryStatus }
    } catch {}
    $scheme = $null
    try { $scheme = (& powercfg /getactivescheme 2>$null | Out-String).Trim() } catch {}
    return [pscustomobject][ordered]@{ ACLineStatus = $ac; BatteryStatus = $batteryStatus; Charging = $charging; PowerScheme = $scheme; ThermalThrottling = $null }
}

$power = Get-PowerState
$dotnet = (Get-Command dotnet).Source
$gitFetch = Invoke-Captured -FilePath (Get-Command git).Source -ArgumentList @('fetch','--all','--prune') -Name 'git-fetch-before-lock' -WorkingDirectory $repo
$branch = (& git -C $repo branch --show-current).Trim()
if ($branch -ne 'codex/filename-hardening') { throw "Formal series must run on codex/filename-hardening; current branch is '$branch'." }
$sourceSha = (& git -C $repo rev-parse HEAD).Trim()
$initialRemoteSha = (& git -C $repo ls-remote origin refs/heads/codex/filename-hardening 2>$null).Trim().Split("`t")[0]
if ([string]::IsNullOrWhiteSpace($initialRemoteSha)) { throw 'Unable to resolve origin/codex/filename-hardening.' }

$ramBytes = $null
try { $ramBytes = [System.GC]::GetGCMemoryInfo().TotalAvailableMemoryBytes } catch {}
if ($null -eq $ramBytes) {
    try { $ramBytes = [int64](Get-CimInstance Win32_ComputerSystem -ErrorAction Stop).TotalPhysicalMemory } catch { $ramBytes = $null }
}
$machine = [ordered]@{
    os = [System.Environment]::OSVersion.VersionString
    cpu = [System.Environment]::GetEnvironmentVariable('PROCESSOR_IDENTIFIER')
    ram_bytes = $ramBytes
    dotnet_sdk = (& dotnet --version).Trim()
    physical_disks = @()
}
try {
    $machine.physical_disks = @(Get-CimInstance Win32_DiskDrive -ErrorAction Stop | Select-Object DeviceID,Model,InterfaceType,MediaType,Size)
} catch { $machine.physical_disks = @([pscustomobject]@{ unavailable = $_.Exception.GetType().Name }) }

$querySetPath = Join-Path $repo 'tests\FilenameSearch.Formal\query-set.json'
$oraclePath = Join-Path $repo 'tests\FilenameSearch.Formal\oracle-v1.json'
$rulesPath = Join-Path $repo 'tests\FilenameSearch.Formal\acceptance-rules.json'
$formalSourcePath = Join-Path $repo 'tests\FilenameSearch.Formal\Program.cs'
$guiScriptPath = Join-Path $repo 'scripts\Run-FilenameActualGuiProbe.ps1'
$rules = Get-Content -LiteralPath $rulesPath -Raw | ConvertFrom-Json
$thresholdHash = Get-CanonicalJsonHash $rules.hardThresholds
$acceptanceHash = Get-CanonicalJsonHash $rules

$worktree = Join-Path ([System.IO.Path]::GetTempPath()) ('personalrag-formal-worktree-' + [guid]::NewGuid().ToString('N'))
$addWorktree = Invoke-Captured -FilePath (Get-Command git).Source -ArgumentList @('worktree','add','--detach',$worktree,$sourceSha) -Name 'git-worktree-add' -WorkingDirectory $repo
if ($addWorktree.exit_code -ne 0) { throw 'Unable to create isolated verification worktree.' }

$gate = [System.Collections.Generic.List[object]]::new()
$gate.Add((Invoke-Captured -FilePath $dotnet -ArgumentList @('--info') -Name 'dotnet-info' -WorkingDirectory $worktree))
$gate.Add((Invoke-Captured -FilePath $dotnet -ArgumentList @('restore','PersonalRag.sln') -Name 'dotnet-restore' -WorkingDirectory $worktree))
$gate.Add((Invoke-Captured -FilePath $dotnet -ArgumentList @('build','PersonalRag.sln','-c','Release','--no-restore') -Name 'dotnet-build-release' -WorkingDirectory $worktree))
$testDll = Join-Path $worktree 'tests\FilenameSearch.Tests\bin\Release\net8.0\FilenameSearch.Tests.dll'
$guiTestDll = Join-Path $worktree 'tests\FilenameSearch.Gui.Tests\bin\Release\net8.0-windows\FilenameSearch.Gui.Tests.dll'
$gate.Add((Invoke-Captured -FilePath $dotnet -ArgumentList @($testDll) -Name 'filename-tests' -WorkingDirectory $worktree))
$gate.Add((Invoke-Captured -FilePath $dotnet -ArgumentList @($guiTestDll) -Name 'filename-gui-tests' -WorkingDirectory $worktree))

$suppRoot = Join-Path $data 'supplemental-e2e-root'
$suppReport = Join-Path $logs 'supplemental-e2e.json'
$gate.Add((Invoke-Captured -FilePath $dotnet -ArgumentList @('run','--project','tests\FilenameSearch.E2E','-c','Release','--no-build','--',$suppRoot,$suppReport,'100000','--keep') -Name 'supplemental-e2e' -WorkingDirectory $worktree))
$suppIdleReport = Join-Path $logs 'supplemental-idle.json'
$suppStore = Join-Path $suppRoot '.personalrag-store\index.routec'
$gate.Add((Invoke-Captured -FilePath $dotnet -ArgumentList @('run','--project','tests\FilenameSearch.Idle','-c','Release','--no-build','--',$suppRoot,$suppStore,$suppIdleReport,$SupplementalIdleSeconds) -Name 'supplemental-idle' -WorkingDirectory $worktree))
$regressionPath = Join-Path $reports 'REGRESSION.json'
$gateFailures = @($gate | Where-Object { $_.exit_code -ne 0 }).Count
$regression = [ordered]@{ version = 1; source_commit_sha = $sourceSha; report_head_sha = $sourceSha; command = 'isolated worktree: dotnet --info; dotnet restore PersonalRag.sln; dotnet build PersonalRag.sln -c Release --no-restore; FilenameSearch.Tests; FilenameSearch.Gui.Tests; supplemental E2E/Idle'; operations = $gate; failed_operation_count = $gateFailures; pass = ($gateFailures -eq 0); ACLineStatus = $power.ACLineStatus; battery_status = $power.BatteryStatus; charging = $power.Charging; power_scheme = $power.PowerScheme; thermal_throttling = $power.ThermalThrottling; ac_requirement_overridden = $true }
$regression | ConvertTo-Json -Depth 50 | Set-Content -LiteralPath $regressionPath -Encoding utf8
if (-not $regression.pass) { throw 'Gate 0 regression failed; formal measurement is not started.' }

$corpusRoot = Join-Path $data 'corpus-1m'
$store = Join-Path $data 'stores\core.manifest'
$corpusManifest = $corpusRoot.TrimEnd('\') + '.formal-corpus.json'
$formalDll = Join-Path $worktree 'tests\FilenameSearch.Formal\bin\Release\net8.0\FilenameSearch.Formal.dll'
$guiExe = Join-Path $worktree 'src\FilenameSearch.Gui\bin\Release\net8.0-windows\FilenameSearch.Gui.exe'
$generateReport = Join-Path $logs 'generate.json'
$generate = Invoke-Captured -FilePath $dotnet -ArgumentList @($formalDll,'generate',$corpusRoot,$generateReport,$Count) -Name 'formal-generate-corpus' -WorkingDirectory $worktree
if ($generate.exit_code -ne 0 -or -not (Test-Path -LiteralPath $corpusManifest)) { throw '1M corpus generation failed.' }

$sourceHashes = [ordered]@{}
Get-ChildItem -LiteralPath (Join-Path $repo 'src\FilenameSearch.Core'), (Join-Path $repo 'src\FilenameSearch.RouteC'), (Join-Path $repo 'src\FilenameSearch'), (Join-Path $repo 'src\FilenameSearch.Gui'), (Join-Path $repo 'tests\FilenameSearch.Formal') -Recurse -File | Where-Object { $_.Extension -in @('.cs','.csproj','.json') } | Sort-Object FullName | ForEach-Object { $relative = [System.IO.Path]::GetRelativePath($repo,$_.FullName); $sourceHashes[$relative] = Get-Sha256 $_.FullName }
$lockPath = Join-Path $reports 'FORMAL_SERIES_LOCK.json'
$measurementScriptHashes = [ordered]@{}
Get-ChildItem -LiteralPath (Join-Path $repo 'scripts') -Filter '*.ps1' -File | Sort-Object FullName | ForEach-Object {
    $measurementScriptHashes[[System.IO.Path]::GetRelativePath($repo,$_.FullName)] = Get-Sha256 $_.FullName
}
$lock = [ordered]@{
    version = 1
    series_id = [guid]::NewGuid().ToString('N')
    benchmark_rounds = 20
    warmup_rounds = 2
    measured_rounds = 18
    query_shuffle_seed = 123456
    AC_requirement_overridden = $true
    formal_source_commit_sha = $sourceSha
    initial_remote_branch_sha = $initialRemoteSha
    production_executable_sha256 = Get-Sha256 $guiExe
    formal_runner_sha256 = Get-Sha256 $formalSourcePath
    measurement_script_sha256 = Get-Sha256 $PSCommandPath
    measurement_script_hashes = $measurementScriptHashes
    gui_probe_script_sha256 = Get-Sha256 $guiScriptPath
    corpus_generator_sha256 = Get-Sha256 $formalSourcePath
    corpus_manifest_sha256 = Get-Sha256 $corpusManifest
    query_set_sha256 = Get-Sha256 $querySetPath
    independent_oracle_sha256 = Get-Sha256 $oraclePath
    normalizer_casefold_version = 'unicode-15.1-full-casefold+nfc-v3'
    threshold_definition_hash = $thresholdHash
    acceptance_rule_hash = $acceptanceHash
    acceptance_rules_path = $rulesPath
    acceptance_plan_sha256 = Get-Sha256 (Join-Path $repo 'docs\FILENAME_HARDENING_ACCEPTANCE_PLAN.md')
    source_branch = $branch
    measurement_configuration = [ordered]@{ data_root = $data; corpus_root = $corpusRoot; store = $store; count = $Count; logical_bytes = 100GB; churn_seconds = $ChurnSeconds; idle_seconds = $IdleSeconds; build_configuration = 'Release'; gc_forced = $false; fixed_query_order = $true; benchmark_rounds = 20; warmup_rounds = 2; measured_rounds = 18; query_shuffle_seed = 123456 }
    ACLineStatus = $power.ACLineStatus
    battery_status = $power.BatteryStatus
    charging = $power.Charging
    power_scheme = $power.PowerScheme
    thermal_throttling = $power.ThermalThrottling
    source_hashes = $sourceHashes
}
$previousLock = $null
if (Test-Path -LiteralPath $lockPath) {
    try { $previousLock = Get-Content -LiteralPath $lockPath -Raw | ConvertFrom-Json } catch { $previousLock = $null }
}
$lock | ConvertTo-Json -Depth 100 | Set-Content -LiteralPath $lockPath -Encoding utf8
$invalidationsPath = Join-Path $reports 'FORMAL_SERIES_INVALIDATIONS.json'
$invalidated = @()
if ($previousLock -and $previousLock.series_id -and $previousLock.series_id -ne $lock.series_id) {
    $invalidated += [ordered]@{ series_id = $previousLock.series_id; source_commit_sha = $previousLock.formal_source_commit_sha; reason = 'New production/runner/source lock supersedes the earlier series; prior measurements cannot be reused.' }
}
[ordered]@{ version = 1; series_id = $lock.series_id; invalidated = $invalidated; note = 'Any series listed here is excluded from the aggregate; all current results use this lock.' } | ConvertTo-Json -Depth 20 | Set-Content -LiteralPath $invalidationsPath -Encoding utf8

$environmentRaw = Join-Path $logs 'environment-raw.json'
$environmentOp = Invoke-Captured -FilePath $dotnet -ArgumentList @($formalDll,'environment',$environmentRaw) -Name 'formal-environment' -WorkingDirectory $worktree
$environmentValue = Get-Content -LiteralPath $environmentRaw -Raw | ConvertFrom-Json
$environmentPath = Join-Path $reports 'ENVIRONMENT.json'
Add-CommonReportFields $environmentValue $environmentPath $environmentOp $guiExe $corpusManifest | Out-Null

$core = Invoke-FormalMode 'core' @($corpusRoot,$store,$Count,'123456') 'CORE_1M.json'
$sustained = Invoke-FormalMode 'sustained' @($corpusRoot,$store,'10000') 'SUSTAINED_SEARCH_1M.json'
$post = Invoke-FormalMode 'post-update' @($corpusRoot,$store) 'POST_UPDATE_1M.json'
$write = Invoke-FormalMode 'write' @($corpusRoot,$store,'100') 'WRITE_AMPLIFICATION.json'
$churn = Invoke-FormalMode 'churn' @($corpusRoot,$store,$ChurnSeconds) 'CHURN.json'
$restart = Invoke-FormalMode 'restart' @($corpusRoot,$store) 'RESTART_MODIFY.json'

$directoryRoot = Join-Path $data 'directory-root'
$directoryStore = Join-Path $data 'stores\directory.manifest'
$directory = Invoke-FormalMode 'directory' @($directoryRoot,$directoryStore) 'DIRECTORY_TREE.json'
$persistenceRoot = Join-Path $data 'persistence-root'
$persistenceStore = Join-Path $data 'stores\persistence.manifest'
$persistence = Invoke-FormalMode 'persistence' @($persistenceRoot,$persistenceStore) 'PERSISTENCE_INTEGRITY.json'
$multiRaw = Join-Path $logs 'multi-raw.json'
$multiStoreRoot = Join-Path $data 'stores\multi-volumes'
$multiOp = Invoke-Captured -FilePath $dotnet -ArgumentList @($formalDll,'multi',$multiRaw,$multiStoreRoot) -Name 'formal-multi' -WorkingDirectory $worktree
$multiValue = Get-Content -LiteralPath $multiRaw -Raw | ConvertFrom-Json
$multiPath = Join-Path $reports 'MULTI_VOLUME.json'
Add-CommonReportFields $multiValue $multiPath $multiOp $guiExe $corpusManifest | Out-Null
$inaccessibleRoot = Join-Path $data 'inaccessible-root'
$inaccessibleStore = Join-Path $data 'stores\inaccessible.manifest'
$inaccessible = Invoke-FormalMode 'inaccessible' @($inaccessibleRoot,$inaccessibleStore) 'INACCESSIBLE.json'

$guiSamples = [System.Collections.Generic.List[object]]::new()
$queries = @('fixture_','ReadMe','report_*.xlsx',([char]0x03A9),'zz','STRASSE',('caf' + [char]0x00E9),([char]0x65E5 + [char]0x672C + [char]0x8A9E),'absent_formal_query','fixture_0000001')
for ($i = 0; $i -lt $queries.Count; $i++) {
    $samplePath = Join-Path $logs ('gui-startup-{0:D2}.json' -f ($i + 1))
    $guiOp = Invoke-Captured -FilePath (Get-Command powershell).Source -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File',$guiScriptPath,'-GuiExe',$guiExe,'-Root',$corpusRoot,'-Store',$store,'-Report',$samplePath,'-Query',$queries[$i]) -Name ('gui-startup-{0:D2}' -f ($i + 1)) -WorkingDirectory $worktree
    $sample = Get-Content -LiteralPath $samplePath -Raw | ConvertFrom-Json
    $guiSamples.Add([ordered]@{ query = $queries[$i]; process_start_to_ready_ms = $sample.process_start_to_ready_ms; private_bytes = $sample.child.private_bytes; rows = $sample.child.rows; exit_code = $guiOp.exit_code; command = $guiOp.command; stdout_path = $guiOp.stdout_path; stderr_path = $guiOp.stderr_path })
}
$startupValues = @($guiSamples | ForEach-Object { [double]$_.process_start_to_ready_ms })
$startupP95 = Get-Percentile $startupValues .95
$startupPrivate = @($guiSamples | ForEach-Object { [int64]$_.private_bytes }) | Measure-Object -Maximum | Select-Object -ExpandProperty Maximum
$guiActualPath = Join-Path $reports 'GUI_ACTUAL_STARTUP.json'
$guiActual = [ordered]@{ version = 1; mode = 'actual-child-process'; source_commit_sha = $sourceSha; report_head_sha = $sourceSha; fresh_process_count = $guiSamples.Count; samples = $guiSamples; startup_p95_ms = $startupP95; max_private_bytes = $startupPrivate; hard_threshold = [ordered]@{ fresh_process_count = 10; startup_p95_ms = 2000; private_bytes = 1500000000 }; pass = $guiSamples.Count -ge 10 -and $startupP95 -le 2000 -and $startupPrivate -le 1500000000; ACLineStatus = $power.ACLineStatus; battery_status = $power.BatteryStatus; charging = $power.Charging; power_scheme = $power.PowerScheme; thermal_throttling = $power.ThermalThrottling; ac_requirement_overridden = $true }
$guiActual | ConvertTo-Json -Depth 50 | Set-Content -LiteralPath $guiActualPath -Encoding utf8

$warmPath = Join-Path $logs 'gui-warm-raw.json'
$warmOp = Invoke-Captured -FilePath $guiExe -ArgumentList @('--warm-probe',$warmPath,$corpusRoot,$store) -Name 'gui-warm-input' -WorkingDirectory $worktree
$warmValue = Get-Content -LiteralPath $warmPath -Raw | ConvertFrom-Json
$warmValue | Add-Member -NotePropertyName source_commit_sha -NotePropertyValue $sourceSha -Force
$warmValue | Add-Member -NotePropertyName report_head_sha -NotePropertyValue $sourceSha -Force
$warmPathReport = Join-Path $reports 'GUI_WARM_INPUT.json'
Add-GuiReportFields $warmValue $warmPathReport $warmOp | Out-Null
$warmValue | ConvertTo-Json -Depth 100 | Set-Content -LiteralPath $warmPathReport -Encoding utf8

$livePath = Join-Path $logs 'gui-live-raw.json'
$liveOp = Invoke-Captured -FilePath $guiExe -ArgumentList @('--live-probe',$livePath,$corpusRoot,$store) -Name 'gui-live-refresh' -WorkingDirectory $worktree
$liveValue = Get-Content -LiteralPath $livePath -Raw | ConvertFrom-Json
$liveValue | Add-Member -NotePropertyName source_commit_sha -NotePropertyValue $sourceSha -Force
$liveValue | Add-Member -NotePropertyName report_head_sha -NotePropertyValue $sourceSha -Force
$livePathReport = Join-Path $reports 'GUI_LIVE_REFRESH.json'
Add-GuiReportFields $liveValue $livePathReport $liveOp | Out-Null
$liveValue | ConvertTo-Json -Depth 100 | Set-Content -LiteralPath $livePathReport -Encoding utf8

$idle = Invoke-FormalMode 'idle' @($corpusRoot,$store,$IdleSeconds) 'IDLE_10M.json'

$reportNames = @('CORE_1M.json','SUSTAINED_SEARCH_1M.json','POST_UPDATE_1M.json','WRITE_AMPLIFICATION.json','CHURN.json','RESTART_MODIFY.json','DIRECTORY_TREE.json','PERSISTENCE_INTEGRITY.json','INACCESSIBLE.json','MULTI_VOLUME.json','GUI_ACTUAL_STARTUP.json','GUI_WARM_INPUT.json','GUI_LIVE_REFRESH.json','IDLE_10M.json')
$reportValues = [ordered]@{}
foreach ($name in $reportNames) {
    $path = Join-Path $reports $name
    if (Test-Path -LiteralPath $path) { $reportValues[$name] = Get-Content -LiteralPath $path -Raw | ConvertFrom-Json }
}
$formalPass = $true
foreach ($entry in $reportValues.GetEnumerator()) {
    if ($entry.Key -eq 'MULTI_VOLUME.json' -and $entry.Value.status -eq 'NOT_RUN_NO_SECOND_FIXED_VOLUME') { continue }
    if ($entry.Value.status -match 'NOT_RUN|BLOCKED|estimated') { $formalPass = $false }
    if (-not [bool]$entry.Value.pass) { $formalPass = $false }
}
$limitations = @($reportValues.GetEnumerator() | Where-Object { $_.Value.status -eq 'informational_limitation' -or $_.Value.informational_limitation } | ForEach-Object { [pscustomobject]@{ report = $_.Key; reason = if ($_.Value.informational_limitation) { $_.Value.informational_limitation } else { $_.Value.limitation } } })
$finalPath = Join-Path $reports 'FINAL_FILENAME_HARDENING.json'
$final = [ordered]@{ version = 1; series_id = $lock.series_id; evaluation_complete = $formalPass; pass = $formalPass; measured_source_commit_sha = $sourceSha; report_head_sha = $sourceSha; initial_remote_branch_sha = $initialRemoteSha; reports = $reportValues; allowed_not_run = @('NOT_RUN_NO_SECOND_FIXED_VOLUME'); informational_limitations = $limitations; AC_requirement_overridden = $true; ACLineStatus = $power.ACLineStatus; battery_status = $power.BatteryStatus; charging = $power.Charging; power_scheme = $power.PowerScheme; thermal_throttling = $power.ThermalThrottling; utc = [DateTime]::UtcNow }
$final | ConvertTo-Json -Depth 100 | Set-Content -LiteralPath $finalPath -Encoding utf8
$acceptancePath = Join-Path $reports 'FINAL_FILENAME_HARDENING_ACCEPTANCE.json'
$acceptance = [ordered]@{ version = 1; series_id = $lock.series_id; evaluation_complete = $formalPass; pass = $formalPass; measured_source_commit_sha = $sourceSha; report_commit_sha = $null; branch = 'codex/filename-hardening'; hard_thresholds = $rules.hardThresholds; final_report = $finalPath; AC_requirement_overridden = $true; remaining_issue = if ($formalPass) { $null } else { 'One or more formal report pass fields are false; use raw reports for the required FAIL loop.' } }
$acceptance | ConvertTo-Json -Depth 100 | Set-Content -LiteralPath $acceptancePath -Encoding utf8

Write-Output ($final | ConvertTo-Json -Depth 20)
if (-not $formalPass) { exit 1 }
