param(
    [int[]]$MiB = @(4, 96, 256),
    [string]$Indexer = ".\target\release\personalrag-v2-indexer.exe",
    [switch]$Keep
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $Indexer -PathType Leaf)) {
    throw "Indexer not found: $Indexer"
}

function Get-TreeBytes([string]$Root) {
    $sum = (Get-ChildItem -LiteralPath $Root -File -Recurse | Measure-Object -Property Length -Sum).Sum
    if ($null -eq $sum) { return [int64]0 }
    return [int64]$sum
}

$results = @()
foreach ($size in $MiB) {
    if ($size -lt 1) { throw "MiB must be >= 1: $size" }

    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss-ffff'
    $base = Join-Path $env:TEMP "PersonalRag-Capacity-$size-$stamp"
    $root = Join-Path $base 'root'
    $store = Join-Path $base 'store'
    New-Item -ItemType Directory -Force $root, $store | Out-Null

    try {
        $fileBytes = 1MB
        $buffer = New-Object byte[] $fileBytes
        [Array]::Fill[byte]($buffer, [byte][char]'a')
        for ($i = 0; $i -lt $size; $i++) {
            $marker = [Text.Encoding]::UTF8.GetBytes(("PR_CAP_FILE_{0:D4}" + [Environment]::NewLine -f $i))
            [Array]::Copy($marker, 0, $buffer, 0, $marker.Length)
            [IO.File]::WriteAllBytes((Join-Path $root ("file-{0:D4}.txt" -f $i)), $buffer)
        }

        & $Indexer init --root $root --store $store
        if ($LASTEXITCODE -ne 0) { throw "init failed for ${size}MiB: $LASTEXITCODE" }
        $sourceInit = Get-TreeBytes $root
        $storeInit = Get-TreeBytes $store

        $changes = [Math]::Max(1, [int][Math]::Ceiling($size * 0.02))
        for ($i = 0; $i -lt $changes; $i++) {
            $path = Join-Path $root ("file-{0:D4}.txt" -f $i)
            $stream = [IO.File]::Open($path, [IO.FileMode]::Open, [IO.FileAccess]::Write, [IO.FileShare]::Read)
            try {
                $marker = [Text.Encoding]::UTF8.GetBytes(("UPDATED_{0:D4}" + [Environment]::NewLine -f $i))
                $stream.Write($marker, 0, $marker.Length)
                $stream.Flush()
            }
            finally {
                $stream.Dispose()
            }
        }

        & $Indexer update --root $root --store $store
        if ($LASTEXITCODE -ne 0) { throw "update failed for ${size}MiB: $LASTEXITCODE" }
        $sourceFinal = Get-TreeBytes $root
        $storeFinal = Get-TreeBytes $store

        $initRatio = if ($sourceInit) { $storeInit / [double]$sourceInit } else { 0.0 }
        $finalRatio = if ($sourceFinal) { $storeFinal / [double]$sourceFinal } else { 0.0 }
        $hardGate = if ($size -ge 4) { $finalRatio -le 0.10 } else { $null }

        $result = [pscustomobject]@{
            MiB = $size
            ChangedFiles = $changes
            InitSourceBytes = $sourceInit
            InitStoreBytes = $storeInit
            InitRatio = $initRatio
            FinalSourceBytes = $sourceFinal
            CompleteStoreBytes = $storeFinal
            CompleteRatio = $finalRatio
            HardGate = $hardGate
        }
        $results += $result
        Write-Host ("CAPACITY mib={0} changes={1} init_ratio={2:P6} complete_ratio={3:P6} hard_gate={4}" -f $size, $changes, $initRatio, $finalRatio, $hardGate)

        if ($size -ge 4 -and -not $hardGate) {
            throw ("Persistent capacity hard gate exceeded at {0}MiB: {1:P6}" -f $size, $finalRatio)
        }
    }
    finally {
        if (-not $Keep -and (Test-Path -LiteralPath $base)) {
            Remove-Item -LiteralPath $base -Recurse -Force
        }
        elseif ($Keep) {
            Write-Host "KEPT $base"
        }
    }
}

$results | Format-Table -AutoSize
