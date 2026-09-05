param(
    [Parameter(Mandatory=$false)][string]$DataRoot = (Join-Path $PSScriptRoot "..\bench-data"),
    [Parameter(Mandatory=$false)][string]$ReportRoot = (Join-Path $PSScriptRoot "..\reports\formal"),
    [switch]$Generate,
    [int]$Repetitions = 100
)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
$repo=(Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$gate1=Join-Path $ReportRoot 'gate1'; $gate2=Join-Path $ReportRoot 'gate2'
New-Item -ItemType Directory -Force -Path $ReportRoot | Out-Null
$args1=@('-ExecutionPolicy','Bypass','-File',(Join-Path $PSScriptRoot 'Run-Gate1.ps1'),'-DataRoot',$DataRoot,'-ReportRoot',$gate1,'-Repetitions',"$Repetitions")
if($Generate){$args1+='-Generate'}
& powershell @args1
if($LASTEXITCODE -ne 0){ throw "Gate 1 failed. See $gate1" }
$args2=@('-ExecutionPolicy','Bypass','-File',(Join-Path $PSScriptRoot 'Run-Gate2.ps1'),'-DataRoot',$DataRoot,'-ReportRoot',$gate2,'-Gate1ReportRoot',$gate1,'-Repetitions',"$Repetitions")
if($Generate){$args2+='-Generate'}
& powershell @args2
if($LASTEXITCODE -ne 0){ throw "Gate 2 failed. See $gate2" }
$g1=Get-Content -Raw (Join-Path $gate1 'GATE1_SUMMARY.json')|ConvertFrom-Json
$g2=Get-Content -Raw (Join-Path $gate2 'GATE2_SUMMARY.json')|ConvertFrom-Json
if(-not $g1.pass -or -not $g2.pass){throw 'Formal gates did not both pass'}
if([string]$g1.sourceTreeSha256 -ne [string]$g2.sourceTreeSha256){throw 'Source identity differs between gates'}
$complete=[ordered]@{
    product='PersonalRag';version='v1';status='PERSONALRAG V1 COMPLETE';pass=$true
    gate1=$g1;gate2=$g2;sourceTreeSha256=$g1.sourceTreeSha256
    utc=[DateTime]::UtcNow.ToString('o')
}
$complete|ConvertTo-Json -Depth 12|Set-Content -Encoding UTF8 (Join-Path $ReportRoot 'PERSONALRAG_V1_COMPLETE.json')
Write-Host 'PERSONALRAG V1 COMPLETE'
Write-Host (Join-Path $ReportRoot 'PERSONALRAG_V1_COMPLETE.json')
