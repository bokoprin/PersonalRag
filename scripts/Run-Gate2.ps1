param(
    [Parameter(Mandatory=$false)][string]$DataRoot = (Join-Path $PSScriptRoot "..\bench-data"),
    [Parameter(Mandatory=$false)][string]$ReportRoot = (Join-Path $PSScriptRoot "..\reports\formal-gate2"),
    [Parameter(Mandatory=$false)][string]$Gate1ReportRoot = (Join-Path $PSScriptRoot "..\reports\formal-gate1"),
    [switch]$Generate,
    [int]$Repetitions = 100
)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$DataRoot = [IO.Path]::GetFullPath($DataRoot); $ReportRoot=[IO.Path]::GetFullPath($ReportRoot); $Gate1ReportRoot=[IO.Path]::GetFullPath($Gate1ReportRoot)
New-Item -ItemType Directory -Force -Path $DataRoot,$ReportRoot | Out-Null
function Invoke-Dotnet([string[]]$A) { & dotnet @A; if ($LASTEXITCODE -ne 0) { throw "dotnet failed: $($A -join ' ')" } }
function Read-J([string]$P) { Get-Content -Raw -LiteralPath $P | ConvertFrom-Json }
function PercentilePass($Measures,[string]$Group,[double]$P50,[double]$P95,[double]$P99,[double]$Max=[double]::PositiveInfinity) {
    $rows=@($Measures|Where-Object group -eq $Group); return $rows.Count -gt 0 -and -not ($rows|Where-Object { $_.errors -ne 0 -or $_.p50 -gt $P50 -or $_.p95 -gt $P95 -or $_.p99 -gt $P99 -or $_.max -gt $Max })
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
    $spec = Join-Path $repo 'specification\PersonalRag_ASTRA_SPEC_v1.md'; $gui = Join-Path $repo 'specification\PersonalRag_GUI_FROZEN_v1.html'
    $specHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $spec).Hash.ToLowerInvariant(); $guiHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $gui).Hash.ToLowerInvariant()
    if ($specHash -ne ([string]$delivery.spec_sha256).ToLowerInvariant()) { throw 'Frozen specification hash mismatch' }
    if ($guiHash -ne ([string]$delivery.gui_sha256).ToLowerInvariant()) { throw 'Frozen GUI hash mismatch' }
    return [ordered]@{ specSha256=$specHash; guiSha256=$guiHash }
}
function Assert-MixedCorpus([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) { return $false }
    $manifestPath = $Path + '.manifest.json'; if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) { throw "Mixed corpus manifest missing: $manifestPath" }
    $m = Read-J $manifestPath
    if ($m.generator -ne 'astra-mixed-v2-searchable' -or [int]$m.seed -ne 20260905 -or [double]$m.requestedGiB -ne 10 -or [long]$m.sourceBytes -lt 10L*1073741824L -or [int]$m.opaquePaddingBytes -ne 0) {
        throw "Mixed corpus is not frozen v2-searchable: $manifestPath"
    }
    $formats = @($m.formats)
    foreach ($format in @('text','docx','xlsx','pptx','pdf')) { if ($formats -notcontains $format) { throw "Mixed corpus missing format: $format" } }
    return $true
}
function Get-AcLineStatus {
    if (-not ('AstraFormal.NativePower' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace AstraFormal {
    public static class NativePower {
        [StructLayout(LayoutKind.Sequential)] public struct SYSTEM_POWER_STATUS {
            public byte ACLineStatus, BatteryFlag, BatteryLifePercent, SystemStatusFlag;
            public uint BatteryLifeTime, BatteryFullLifeTime;
        }
        [DllImport("kernel32.dll", SetLastError=true)] public static extern bool GetSystemPowerStatus(out SYSTEM_POWER_STATUS status);
    }
}
'@
    }
    $status=New-Object AstraFormal.NativePower+SYSTEM_POWER_STATUS
    if(-not [AstraFormal.NativePower]::GetSystemPowerStatus([ref]$status)){throw 'GetSystemPowerStatus failed'}
    return [int]$status.ACLineStatus
}
function Assert-OfficialMachine([string]$PathToData) {
    if($env:OS -ne 'Windows_NT'){throw 'Formal benchmark requires Windows 11'}
    $osCaption=[string](Get-CimInstance Win32_OperatingSystem|Select-Object -ExpandProperty Caption)
    $cpu=(Get-CimInstance Win32_Processor|Select-Object -ExpandProperty Name)-join '; '
    $ram=[int64](Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory
    $ac=Get-AcLineStatus
    $driveRoot=[IO.Path]::GetPathRoot([IO.Path]::GetFullPath($PathToData)); $drive=New-Object IO.DriveInfo($driveRoot)
    $letter=$driveRoot.TrimEnd('\').TrimEnd(':')
    $dataDisk=$null
    if(Get-Command Get-Partition -ErrorAction SilentlyContinue){$partition=Get-Partition -DriveLetter $letter -ErrorAction Stop;$dataDisk=$partition|Get-Disk -ErrorAction Stop|Select-Object Number,FriendlyName,BusType,HealthStatus,Size}
    if($osCaption -notmatch 'Windows 11'){throw "Official OS mismatch: $osCaption"}
    if($cpu -notmatch 'Core.*Ultra.*9.*285H'){throw "Official CPU mismatch: $cpu"}
    if($ram -lt 31GB){throw "Official RAM mismatch: $ram bytes"}
    if($ac -ne 1){throw 'Formal performance benchmark requires AC power'}
    if($drive.DriveType -ne [IO.DriveType]::Fixed){throw "DataRoot must be on a fixed local drive: $driveRoot"}
    if($null -eq $dataDisk){throw 'Could not resolve the physical disk that contains DataRoot'}
    if(([string]$dataDisk.BusType) -ne 'NVMe'){throw "DataRoot must reside on NVMe; actual BusType=$($dataDisk.BusType)"}
    return [ordered]@{osCaption=$osCaption;cpu=$cpu;ramBytes=$ram;acLineStatus=$ac;dataDrive=$driveRoot;dataDisk=$dataDisk;dotnet=(& dotnet --version).Trim()}
}
Push-Location $repo
try {
    $frozen=Assert-FrozenInputs
    $official=Assert-OfficialMachine $DataRoot
    if($official.dotnet -ne '8.0.424'){throw "Formal SDK must be 8.0.424; got $($official.dotnet)"}
    $gate1Path=Join-Path $Gate1ReportRoot 'GATE1_SUMMARY.json'
    if (-not (Test-Path $gate1Path)) { throw "Gate 1 summary is missing: $gate1Path" }
    $gate1=Read-J $gate1Path; if (-not $gate1.pass) { throw 'Gate 1 is not PASS. Gate 2 cannot be completed.' }
    $sourceTree=Get-SourceTreeHash
    if ([string]$gate1.sourceTreeSha256 -ne $sourceTree) { throw 'Source tree changed after Gate 1. Re-run Gate 1 before Gate 2.' }
    if ([string]$gate1.frozen.specSha256 -ne $frozen.specSha256 -or [string]$gate1.frozen.guiSha256 -ne $frozen.guiSha256) { throw 'Frozen inputs differ from Gate 1 run' }

    Invoke-Dotnet @('restore','PersonalRag.Astra.sln')
    Invoke-Dotnet @('build','PersonalRag.Astra.sln','-c','Release','--no-restore')
    Invoke-Dotnet @('run','--project','tests/Astra.Tests','-c','Release','--no-build')
    $documentFunctionalTests=$true
    Invoke-Dotnet @('run','--project','tests/Astra.Gui.Tests','-c','Release','--no-build','--',(Join-Path $ReportRoot 'gui'))

    $mixed=Join-Path $DataRoot 'mixed-documents-10g-v2'
    if (-not (Assert-MixedCorpus $mixed)) {
        if (-not $Generate) { throw "Missing mixed corpus: $mixed. Re-run with -Generate." }
        Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--','mixed',$mixed,'10')
        if (-not (Assert-MixedCorpus $mixed)) { throw 'Generated mixed corpus validation failed' }
    }
    $mixedManifest=Read-J ($mixed + '.manifest.json')
    $store=Join-Path $ReportRoot 'store-mixed-10g'; Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $store
    Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--','build',$mixed,$store,(Join-Path $ReportRoot 'build-mixed.json'))
    Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--','search',$mixed,$store,(Join-Path $ReportRoot 'search-mixed.json'),"$Repetitions")
    Invoke-Dotnet @('run','--project','tests/Astra.Gui.Tests','-c','Release','--no-build','--','formal-startup',$store,(Join-Path $ReportRoot 'gui-startup-mixed.json'))
    Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--','updates',$mixed,$store,(Join-Path $ReportRoot 'updates-mixed.json'),'100')
    Invoke-Dotnet @('run','--project','benchmarks/Astra.Bench','-c','Release','--no-build','--','recovery',$store,(Join-Path $ReportRoot 'recovery-mixed.json'))

    $b=Read-J (Join-Path $ReportRoot 'build-mixed.json'); $s=Read-J (Join-Path $ReportRoot 'search-mixed.json')
    $u=Read-J (Join-Path $ReportRoot 'updates-mixed.json'); $r=Read-J (Join-Path $ReportRoot 'recovery-mixed.json')
    $gui=Read-J (Join-Path $ReportRoot 'gui\result.json'); $guiStartup=Read-J (Join-Path $ReportRoot 'gui-startup-mixed.json')
    $checks=[ordered]@{
        gate1Regression=[bool]$gate1.pass
        documentFunctionalTests=[bool]$documentFunctionalTests
        gui=[bool]$gui.pass
        mixedSourceBytes=([long]$b.sourceBytes -eq [long]$mixedManifest.sourceBytes)
        mixedPersistentRatio=($b.ratio -le 0.05)
        mixedNormalLatency=(PercentilePass $s.measures 'normal' 50 100 200)
        mixedShortLatency=(PercentilePass $s.measures 'short' [double]::PositiveInfinity 200 [double]::PositiveInfinity 300)
        mixedFilenameLatency=(PercentilePass $s.measures 'filename' 20 50 100)
        mixedIndexLoad=($s.loadMs -le 2000)
        mixedGuiStartupFilename=([bool]$guiStartup.pass -and $guiStartup.filenameReadyMs -le 2000)
        mixedGuiStartupContent=([bool]$guiStartup.pass -and $null -ne $guiStartup.contentReadyMs -and $guiStartup.contentReadyMs -le 3000)
        mixedGuiReadyMemory=([bool]$guiStartup.pass -and $guiStartup.filenamePrivateBytes -le 2GB)
        mixedGuiSearchMemory=([bool]$guiStartup.pass -and $null -ne $guiStartup.contentPrivateBytes -and $guiStartup.contentPrivateBytes -le 3GB)
        mixedReadyMemory=($s.readyPrivateBytes -le 2GB)
        mixedSearchMemory=($s.searchPeakPrivateBytes -le 3GB)
        mixedBuildMemory=($b.buildPeakPrivateBytes -le 8GB)
        mixedUpdate=[bool]$u.pass
        mixedRecovery=[bool]$r.pass
        reportsComplete=([bool]$b.complete -and [bool]$s.complete -and [bool]$u.complete -and [bool]$r.complete)
        binaryIdentity=([string]$b.binarySha256 -eq [string]$s.binarySha256 -and [string]$s.binarySha256 -eq [string]$gate1.binarySha256)
    }
    $pass=-not ($checks.Values -contains $false)
    [ordered]@{
        gate='Gate 2';evaluationComplete=$true;complete=$pass;pass=$pass;checks=$checks;corpus='mixed-documents-10g-v2'
        sourceTreeSha256=$sourceTree;binarySha256=$s.binarySha256;frozen=$frozen;gate1BinarySha256=$gate1.binarySha256;machine=$official
        utc=[DateTime]::UtcNow.ToString('o')
    } | ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 (Join-Path $ReportRoot 'GATE2_SUMMARY.json')
    Get-Content -Raw (Join-Path $ReportRoot 'GATE2_SUMMARY.json')
    if (-not $pass) { exit 2 }
}
finally { Pop-Location }
