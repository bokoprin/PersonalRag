[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$IndexPath,

    [Parameter(Mandatory = $true)]
    [string]$OutputPath,

    [ValidateRange(1024, 1048576)]
    [int]$QueriesPerWorkload = 16384,

    [ValidateRange(3, 15)]
    [int]$Repeats = 5,

    [ValidateRange(16, 4096)]
    [int]$BatchSize = 256,

    [long]$Seed = 20260825
)

$ErrorActionPreference = 'Stop'

$resolvedIndex = [IO.Path]::GetFullPath($IndexPath).TrimEnd('\', '/')
if (-not (Test-Path -LiteralPath $resolvedIndex -PathType Container)) {
    throw "Index directory not found: $resolvedIndex"
}

$resolvedOutput = [IO.Path]::GetFullPath($OutputPath)
if ($resolvedOutput.StartsWith($resolvedIndex + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase) -or
    $resolvedOutput.Equals($resolvedIndex, [StringComparison]::OrdinalIgnoreCase)) {
    throw "OutputPath must be outside the index directory. Refusing to write into the index."
}

$outParent = Split-Path -Parent $resolvedOutput
if ([string]::IsNullOrWhiteSpace($outParent)) {
    $outParent = (Get-Location).Path
}
[IO.Directory]::CreateDirectory($outParent) | Out-Null

$prototypeRoot = Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) 'cq3dir-prototype'
$sourcePaths = @(
    (Join-Path $prototypeRoot 'Cq3Common.cs'),
    (Join-Path $prototypeRoot 'Cq3Blocked.cs'),
    (Join-Path $prototypeRoot 'Cq3Benchmark.cs')
)
foreach ($sourcePath in $sourcePaths) {
    if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf)) {
        throw "Prototype source file not found: $sourcePath"
    }
}
$csharp = ($sourcePaths | ForEach-Object { Get-Content -LiteralPath $_ -Raw }) -join "`n"
Add-Type -TypeDefinition $csharp -Language CSharp

$result = [Cq3PrototypeBenchmark]::Run(
    $resolvedIndex,
    $QueriesPerWorkload,
    $Repeats,
    $BatchSize,
    $Seed
)

$json = $result | ConvertTo-Json -Depth 20
[IO.File]::WriteAllText($resolvedOutput, $json, (New-Object Text.UTF8Encoding($false)))

Write-Host 'CQ3DIR_PROTOTYPE_BENCHMARK_COMPLETE'
Write-Host "indexGiB=$('{0:N6}' -f $result.IndexGiB)"
Write-Host "segmentCount=$($result.SegmentCount)"
Write-Host "entries=$($result.Entries)"
Write-Host "currentCq3DirGiB=$('{0:N6}' -f $result.CurrentCq3DirGiB)"
foreach ($rep in $result.Representations) {
    Write-Host ("representation={0} cq3DirGiB={1:N6} cq3DirReductionPercent={2:N3} wholeIndexGiB={3:N6} wholeIndexReductionPercent={4:N3} encodeMs={5:N3}" -f
        $rep.Name, $rep.DirectoryGiB, $rep.DirectoryReductionPercent, $rep.EstimatedWholeIndexGiB, $rep.EstimatedWholeIndexReductionPercent, $rep.PrototypeEncodeMs)
}
foreach ($timing in $result.Timings) {
    Write-Host ("timing={0}/{1} medianNsPerOp={2:N3} ratioVsCurrent={3:N3} batchP95NsPerOp={4:N3} batchP99NsPerOp={5:N3}" -f
        $timing.Representation, $timing.Workload, $timing.MedianRunNsPerOp, $timing.RatioVsCurrentMedian, $timing.BatchP95NsPerOp, $timing.BatchP99NsPerOp)
}
Write-Host "correctnessValidationKeys=$($result.CorrectnessValidationKeys)"
Write-Host "output=$resolvedOutput"
