param(
    [string]$Report = (Join-Path (Join-Path (Get-Location) 'reports') 'filename-product-acceptance.json'),
    [int]$Count = 100000
)
$ErrorActionPreference = 'Stop'
$repo = (Get-Location).Path
$e2eReport = Join-Path ([System.IO.Path]::GetDirectoryName([System.IO.Path]::GetFullPath($Report))) 'filename-e2e-product.json'
$guiReport = Join-Path ([System.IO.Path]::GetDirectoryName([System.IO.Path]::GetFullPath($Report))) 'filename-gui-startup-100k.json'
$root = $null
try {
    & powershell -ExecutionPolicy Bypass -File (Join-Path $repo 'scripts/Run-FilenameE2E.ps1') -Report $e2eReport -Count $Count -KeepRoot
    if ($LASTEXITCODE -ne 0) { throw "Filename E2E failed with exit code $LASTEXITCODE" }
    $e2e = Get-Content $e2eReport -Raw | ConvertFrom-Json
    $root = $e2e.root
    $parent = [System.IO.Path]::GetDirectoryName([System.IO.Path]::GetFullPath($root))
    $store = Join-Path $root '.personalrag-store/index.routec'
    dotnet run --project (Join-Path $repo 'tests/FilenameSearch.Gui.Tests/FilenameSearch.Gui.Tests.csproj') --configuration Release -- formal-startup $root $store $guiReport
    if ($LASTEXITCODE -ne 0) { throw "Filename GUI startup failed with exit code $LASTEXITCODE" }
    $gui = Get-Content $guiReport -Raw | ConvertFrom-Json
    $final = [ordered]@{
        version = 1
        pass = [bool]$e2e.pass -and [bool]$gui.pass
        core = $e2e
        gui = $gui
        acceptance = [ordered]@{
            first_batch_p95_ms = $e2e.first_batch_ms
            first_batch_hard_max_ms = $e2e.first_batch_ms
            existing_index_startup_ms = $gui.filename_ready_ms
            full_app_private_bytes = $gui.private_bytes
            live_create_ms = $e2e.create_ms
            live_delete_ms = $e2e.delete_ms
            live_rename_ms = $e2e.rename_ms
            live_move_ms = $e2e.move_ms
            restart_catchup = $e2e.restart_catchup
            crash_recovery = $e2e.corrupt_store_recovery
            real_files = $e2e.initial_entries
        }
        utc = [DateTime]::UtcNow
    }
    New-Item -ItemType Directory -Force -Path ([System.IO.Path]::GetDirectoryName([System.IO.Path]::GetFullPath($Report))) | Out-Null
    $final | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $Report -Encoding utf8
    $final | ConvertTo-Json -Depth 12
}
finally {
    if ($root -and (Test-Path -LiteralPath $root)) { Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue }
}
