param(
    [Parameter(Mandatory=$false)][string]$DataRoot = (Join-Path $PSScriptRoot "..\bench-data"),
    [Parameter(Mandatory=$false)][string]$ReportRoot = (Join-Path $PSScriptRoot "..\reports\formal-gate1"),
    [switch]$Generate,
    [int]$Repetitions = 100
)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$DataRoot = [IO.Path]::GetFullPath($DataRoot)
$ReportRoot = [IO.Path]::GetFullPath($ReportRoot)
New-Item -ItemType Directory -Force -Path $DataRoot,$ReportRoot | Out-Null

function Invoke-Dotnet([string[]]$A) {
    & dotnet @A
    if ($LASTEXITCODE -ne 0) { throw "dotnet failed: $($A -join ' ')" }
}
function Read-J([string]$P) { Get-Content -Raw -LiteralPath $P | ConvertFrom-Json }
function PercentilePass($Measures, [string]$Group, [double]$P50, [double]$P95, [double]$P99, [double]$Max = [double]::PositiveInfinity) {
    $rows = @($Measures | Where-Object group -eq $Group)
    return $rows.Count -gt 0 -and -not ($rows | Where-Object { $_.errors -ne 0 -or $_.p50 -gt $P50 -or $_.p95 -gt $P95 -or $_.p99 -gt $P99 -or $_.max -gt $Max })
}
function Get-SourceTreeHash {
    $excluded = @('artifacts','reports','bench-data','bin','obj','.git')
    $rows = New-Object System.Collections.Generic.List[string]
    Get-ChildItem -LiteralPath $repo -File -Recurse | ForEach-Object {
        $relative = $_.FullName.Substring($repo.Length).TrimStart('\','/')
        $parts = $relative -split '[\\/]'
        if (@($parts | Where-Object { $excluded -contains $_ }).Count -eq 0) {
            $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash.ToLowerInvariant()
            $rows.Add(($relative.Replace('\','/') + ':' + $hash))
        }
    }
    $canonical = ($rows | Sort-Object) -join "`n"
    $sha = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($canonical))).Replace('-','').ToLowerInvariant()) }
    finally { $sha.Dispose() }
}
function Assert-FrozenInputs {
    $delivery = Read-J (Join-Path $repo 'specification\PersonalRag_ASTRA_DELIVERY_v1.json')
    $spec = Join-Path $repo 'specification\PersonalRag_ASTRA_SPEC_v1.md'
    $gui = Join-Path $repo 'specification\PersonalRag_GUI_FROZEN_v1.html'
    $specHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $spec).Hash.ToLowerInvariant()
    $guiHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $gui).Hash.ToLowerInvariant()
    if ($specHash -ne ([string]$delivery.spec_sha256).ToLowerInvariant()) { throw 'Frozen specification hash mismatch' }
    if ($guiHash -ne ([string]$delivery.gui_sha256).ToLowerInvariant()) { throw 'Frozen GUI hash mismatch' }
    return [ordered]@{ specSha256=$specHash; guiSha256=$guiHash }
}
function Get-AcLineStatus {
    if (-not ('AstraFormal.NativePower' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace AstraFormal {
    public static class NativePower {
        [StructLayout(LayoutKind.Sequential)]
        public struct SYSTEM_POWER_STATUS {
            public byte ACLineStatus, BatteryFlag, BatteryLifePercent, SystemStatusFlag;
            public uint BatteryLifeTime, BatteryFullLifeTime;
        }
        [DllImport("kernel32.dll", SetLastError=true)]
        public static extern bool GetSystemPowerStatus(out SYSTEM_POWER_STATUS status);
    }
}
'@
    }
    $status = New-Object AstraFormal.NativePower+SYSTEM_POWER_STATUS
    if (-not [AstraFormal.NativePower]::GetSystemPowerStatus([ref]$status)) { throw 'GetSystemPowerStatus failed' }
    return [int]$status.ACLineStatus
}
function Assert-OfficialMachine([string]$PathToData) {
    if ($env:OS -ne 'Windows_NT') { throw 'Formal benchmark requires Windows 11' }
    $osCaption = [string](Get-CimInstance Win32_OperatingSystem | Select-Object -ExpandProperty Caption)
    $cpu = (Get-CimInstance Win32_Processor | Select-Object -ExpandProperty Name) -join '; '
    $ram = [int64](Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory
    $ac = Get-AcLineStatus
    $driveRoot = [IO.Path]::GetPathRoot([IO.Path]::GetFullPath($PathToData))
    $drive = New-Object IO.DriveInfo($driveRoot)
    $physical = @()
    if (Get-Command Get-PhysicalDisk -ErrorAction SilentlyContinue) { $physical = @(Get-PhysicalDisk -ErrorAction SilentlyContinue | Select-Object FriendlyName,MediaType,BusType,HealthStatus,Size) }
    $letter = $driveRoot.TrimEnd('\').TrimEnd(':')
    $dataDisk = $null
    if (Get-Command Get-Partition -ErrorAction SilentlyContinue) {
        $partition = Get-Partition -DriveLetter $letter -ErrorAction Stop
        $dataDisk = $partition | Get-Disk -ErrorAction Stop | Select-Object Number,FriendlyName,BusType,HealthStatus,Size
    }
    if ($osCaption -notmatch 'Windows 11') { throw "Official OS mismatch: $osCaption" }
    if ($cpu -notmatch 'Core.*Ultra.*9.*285H') { throw "Official CPU mismatch: $cpu" }
    if ($ram -lt 31GB) { throw "Official RAM mismatch: $ram bytes" }
    if ($ac -ne 1) { throw 'Formal performance benchmark requires AC power' }
    if ($drive.DriveType -ne [IO.DriveType]::Fixed) { throw "DataRoot must be on a fixed local drive: $driveRoot" }
    if ($null -eq $dataDisk) { throw 'Could not resolve the physical disk that contains DataRoot' }
    if (([string]$dataDisk.BusType) -ne 'NVMe') { throw "DataRoot must reside on NVMe; actual BusType=$($dataDisk.BusType)" }
    return [ordered]@{ osCaption=$osCaption; cpu=$cpu; ramBytes=$ram; acLineStatus=$ac; dataDrive=$driveRoot; dataDisk=$dataDisk; physicalDisks=$physical }
}
function Assert-CoreCorpus([string]$Path, [double]$GiB, [bool]$Names, [long]$ExpectedCount) {
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) { return $false }
    $manifestPath = $Path + '.manifest.json'
    if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) { throw "Corpus manifest missing: $manifestPath" }
    $m = Read-J $manifestPath
    $expectedBytes = if ($Names) { $ExpectedCount * 8192L } else { [long]($GiB * 1073741824L) }
    $expectedFileSize = if ($Names) { 8192 } else { 1048576 }
    if ($m.generator -ne 'astra-corpus-v1' -or [int]$m.seed -ne 20260905 -or [bool]$m.names -ne $Names -or [long]$m.count -ne $ExpectedCount -or [long]$m.bytes -ne $expectedBytes -or [int]$m.fileSize -ne $expectedFileSize) {
        throw "Corpus manifest does not match frozen Gate 1 corpus: $manifestPath"
    }
    return $true
}
function Require-CoreCorpus([string]$Mode,[string]$Path,[string]$Size,[double]$GiB,[bool]$Names,[long]$ExpectedCount) {
    if (Assert-CoreCorpus $Path $GiB $Names $ExpectedCount) { return }
    if (-not $Generate) { throw "Missing corpus: $Path. Re-run with -Generate or point DataRoot at the frozen corpus." }
    Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--',$Mode,$Path,$Size)
    if (-not (Assert-CoreCorpus $Path $GiB $Names $ExpectedCount)) { throw "Generated corpus validation failed: $Path" }
}

Push-Location $repo
try {
    $frozen = Assert-FrozenInputs
    $official = Assert-OfficialMachine $DataRoot
    $machine = [ordered]@{
        utc = [DateTime]::UtcNow.ToString('o')
        os = [Environment]::OSVersion.VersionString
        osCaption = $official.osCaption
        cpu = $official.cpu
        ramBytes = $official.ramBytes
        acLineStatus = $official.acLineStatus
        dataDrive = $official.dataDrive
        physicalDisks = $official.physicalDisks
        dotnet = (& dotnet --version).Trim()
        sourceTreeSha256 = Get-SourceTreeHash
        frozen = $frozen
    }
    $machine | ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 (Join-Path $ReportRoot 'machine.json')

    if ($machine.dotnet -ne '8.0.424') { throw "Formal SDK must be 8.0.424 from global.json; got $($machine.dotnet)" }

    Invoke-Dotnet @('restore','PersonalRag.Astra.sln')
    Invoke-Dotnet @('build','PersonalRag.Astra.sln','-c','Release','--no-restore')
    Invoke-Dotnet @('run','--project','tests/Astra.Tests','-c','Release','--no-build')
    $coreTests = $true
    Invoke-Dotnet @('run','--project','tests/Astra.Gui.Tests','-c','Release','--no-build','--',(Join-Path $ReportRoot 'gui'))

    $core10 = Join-Path $DataRoot 'core-10g'
    $core100 = Join-Path $DataRoot 'core-100g'
    $names = Join-Path $DataRoot 'names-1m'
    Require-CoreCorpus 'generate' $core10 '10' 10 $false 10240
    Require-CoreCorpus 'generate' $core100 '100' 100 $false 102400
    Require-CoreCorpus 'names' $names '1000000' 0 $true 1000000

    $store10 = Join-Path $ReportRoot 'store-10g'
    $store100 = Join-Path $ReportRoot 'store-100g'
    $storeNames = Join-Path $ReportRoot 'store-names'
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $store10,$store100,$storeNames

    Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--','build',$core10,$store10,(Join-Path $ReportRoot 'build-10g.json'))
    Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--','search',$core10,$store10,(Join-Path $ReportRoot 'search-10g.json'),"$Repetitions")
    Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--','updates',$core10,$store10,(Join-Path $ReportRoot 'updates.json'),'100')
    Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--','churn',$core10,$store10,(Join-Path $ReportRoot 'churn.json'),'10000')
    Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--','recovery',$store10,(Join-Path $ReportRoot 'recovery.json'))
    Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--','build',$core100,$store100,(Join-Path $ReportRoot 'build-100g.json'))
    Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--','search',$core100,$store100,(Join-Path $ReportRoot 'search-100g.json'),"$Repetitions")
    Invoke-Dotnet @('run','--project','tests/Astra.Gui.Tests','-c','Release','--no-build','--','formal-startup',$store10,(Join-Path $ReportRoot 'gui-startup-10g.json'))
    Invoke-Dotnet @('run','--project','tests/Astra.Gui.Tests','-c','Release','--no-build','--','formal-startup',$store100,(Join-Path $ReportRoot 'gui-startup-100g.json'))
    Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--','idle',$store100,(Join-Path $ReportRoot 'idle-100g.json'),'600')

    Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--','build',$names,$storeNames,(Join-Path $ReportRoot 'build-names.json'))
    Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--','names-search',$names,$storeNames,(Join-Path $ReportRoot 'search-names.json'),"$Repetitions")
    Invoke-Dotnet @('run','--project','tests/Astra.Gui.Tests','-c','Release','--no-build','--','formal-startup',$storeNames,(Join-Path $ReportRoot 'gui-startup-names.json'),'filename-only')

    $b10=Read-J (Join-Path $ReportRoot 'build-10g.json'); $s10=Read-J (Join-Path $ReportRoot 'search-10g.json')
    $b100=Read-J (Join-Path $ReportRoot 'build-100g.json'); $s100=Read-J (Join-Path $ReportRoot 'search-100g.json')
    $bn=Read-J (Join-Path $ReportRoot 'build-names.json'); $sn=Read-J (Join-Path $ReportRoot 'search-names.json')
    $updates=Read-J (Join-Path $ReportRoot 'updates.json'); $churn=Read-J (Join-Path $ReportRoot 'churn.json')
    $recovery=Read-J (Join-Path $ReportRoot 'recovery.json'); $idle=Read-J (Join-Path $ReportRoot 'idle-100g.json')
    $gui=Read-J (Join-Path $ReportRoot 'gui\result.json'); $guiStartup10=Read-J (Join-Path $ReportRoot 'gui-startup-10g.json'); $guiStartup=Read-J (Join-Path $ReportRoot 'gui-startup-100g.json')
    $guiStartupNames=Read-J (Join-Path $ReportRoot 'gui-startup-names.json')

    $checks = [ordered]@{
        coreTests = [bool]$coreTests
        gui = [bool]$gui.pass
        filenameLatency1M = (PercentilePass $sn.measures 'filename' 20 50 100)
        pathLatency1M = (PercentilePass $sn.measures 'path' 20 50 100)
        contentNormal100G = (PercentilePass $s100.measures 'normal' 50 100 200)
        contentShort100G = (PercentilePass $s100.measures 'short' [double]::PositiveInfinity 200 [double]::PositiveInfinity 300)
        build10G = ($b10.buildSeconds -le 120)
        build100G = ($b100.buildSeconds -le 900)
        ratio10G = ($b10.ratio -le 0.05)
        ratio100G = ($b100.ratio -le 0.05)
        sourceBytes10G = ([long]$b10.sourceBytes -eq 10L*1073741824L)
        sourceBytes100G = ([long]$b100.sourceBytes -eq 100L*1073741824L)
        fileCount1M = ([long]$bn.files -eq 1000000)
        indexLoad1M = ($sn.loadMs -le 2000)
        readyMemory1M = ($sn.readyPrivateBytes -le 2GB)
        searchMemory1M = ($sn.searchPeakPrivateBytes -le 3GB)
        buildMemory1M = ($bn.buildPeakPrivateBytes -le 8GB)
        indexLoad10G = ($s10.loadMs -le 2000)
        indexLoad100G = ($s100.loadMs -le 2000)
        guiStartupFilename1M = ([bool]$guiStartupNames.pass -and $guiStartupNames.filenameReadyMs -le 2000)
        guiReadyMemory1M = ([bool]$guiStartupNames.pass -and $guiStartupNames.filenamePrivateBytes -le 2GB)
        guiStartupFilename100G = ([bool]$guiStartup.pass -and $guiStartup.filenameReadyMs -le 2000)
        guiReadyMemory100G = ([bool]$guiStartup.pass -and $guiStartup.filenamePrivateBytes -le 2GB)
        guiStartupContent100G = ([bool]$guiStartup.pass -and $null -ne $guiStartup.contentReadyMs -and $guiStartup.contentReadyMs -le 3000)
        guiSearchMemory100G = ([bool]$guiStartup.pass -and $null -ne $guiStartup.contentPrivateBytes -and $guiStartup.contentPrivateBytes -le 3GB)
        guiStartupFilename10G = ([bool]$guiStartup10.pass -and $guiStartup10.filenameReadyMs -le 2000)
        guiReadyMemory10G = ([bool]$guiStartup10.pass -and $guiStartup10.filenamePrivateBytes -le 2GB)
        guiStartupContent10G = ([bool]$guiStartup10.pass -and $null -ne $guiStartup10.contentReadyMs -and $guiStartup10.contentReadyMs -le 3000)
        guiSearchMemory10G = ([bool]$guiStartup10.pass -and $null -ne $guiStartup10.contentPrivateBytes -and $guiStartup10.contentPrivateBytes -le 3GB)
        readyMemory100G = ($s100.readyPrivateBytes -le 2GB)
        searchMemory100G = ($s100.searchPeakPrivateBytes -le 3GB)
        buildMemory100G = ($b100.buildPeakPrivateBytes -le 8GB)
        updateLatency = [bool]$updates.pass
        churn = [bool]$churn.pass
        recovery = [bool]$recovery.pass
        idle = [bool]$idle.pass
        reportsComplete = ([bool]$b10.complete -and [bool]$s10.complete -and [bool]$b100.complete -and [bool]$s100.complete -and [bool]$bn.complete -and [bool]$sn.complete -and
            [bool]$updates.complete -and [bool]$churn.complete -and [bool]$recovery.complete -and [bool]$idle.complete)
        binaryIdentity = ([string]$b10.binarySha256 -eq [string]$s10.binarySha256 -and [string]$s10.binarySha256 -eq [string]$b100.binarySha256 -and
            [string]$b100.binarySha256 -eq [string]$s100.binarySha256 -and [string]$s100.binarySha256 -eq [string]$bn.binarySha256 -and
            [string]$bn.binarySha256 -eq [string]$sn.binarySha256)
    }
    $pass = -not ($checks.Values -contains $false)
    [ordered]@{
        gate='Gate 1'; evaluationComplete=$true; complete=$pass; pass=$pass; checks=$checks
        sourceTreeSha256=$machine.sourceTreeSha256; binarySha256=$s100.binarySha256; frozen=$frozen
        utc=[DateTime]::UtcNow.ToString('o')
    } | ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 (Join-Path $ReportRoot 'GATE1_SUMMARY.json')
    Get-Content -Raw (Join-Path $ReportRoot 'GATE1_SUMMARY.json')
    if (-not $pass) { exit 2 }
}
finally { Pop-Location }
