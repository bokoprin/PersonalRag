param(
    [string]$Report = (Join-Path (Join-Path (Get-Location) 'reports') 'filename-product-acceptance.json'),
    [int]$Count = 100000,
    [int]$IdleSeconds = 600
)
$ErrorActionPreference = 'Stop'
$repo = (Get-Location).Path
$e2eReport = Join-Path ([System.IO.Path]::GetDirectoryName([System.IO.Path]::GetFullPath($Report))) 'filename-e2e-product.json'
$guiReport = Join-Path ([System.IO.Path]::GetDirectoryName([System.IO.Path]::GetFullPath($Report))) 'filename-gui-startup-100k.json'
$idleReport = Join-Path ([System.IO.Path]::GetDirectoryName([System.IO.Path]::GetFullPath($Report))) 'filename-idle-10m.json'
$root = $null
try {
    & powershell -ExecutionPolicy Bypass -File (Join-Path $repo 'scripts/Run-FilenameE2E.ps1') -Report $e2eReport -Count $Count -KeepRoot -Churn
    if ($LASTEXITCODE -ne 0) { throw "Filename E2E failed with exit code $LASTEXITCODE" }
    $e2e = Get-Content $e2eReport -Raw | ConvertFrom-Json
    $root = $e2e.root
    $parent = [System.IO.Path]::GetDirectoryName([System.IO.Path]::GetFullPath($root))
    $store = Join-Path $root '.personalrag-store/index.routec'
    dotnet run --project (Join-Path $repo 'tests/FilenameSearch.Gui.Tests/FilenameSearch.Gui.Tests.csproj') --configuration Release -- formal-startup $root $store $guiReport
    if ($LASTEXITCODE -ne 0) { throw "Filename GUI startup failed with exit code $LASTEXITCODE" }
    $gui = Get-Content $guiReport -Raw | ConvertFrom-Json
    dotnet run --project (Join-Path $repo 'tests/FilenameSearch.Idle/FilenameSearch.Idle.csproj') --configuration Release -- $root $store $idleReport $IdleSeconds
    if ($LASTEXITCODE -ne 0) { throw "Filename idle acceptance failed with exit code $LASTEXITCODE" }
    $idle = Get-Content $idleReport -Raw | ConvertFrom-Json
    $final = [ordered]@{
        version = 1
        pass = [bool]$e2e.pass -and [bool]$gui.pass -and [bool]$idle.pass
        core = $e2e
        gui = $gui
        idle = $idle
        acceptance = [ordered]@{
            first_batch_p95_ms = $gui.first_batch_p95_ms
            first_batch_hard_max_ms = $gui.first_batch_max_ms
            existing_index_startup_ms = $gui.filename_ready_ms
            full_app_private_bytes = $gui.private_bytes
            live_create_ms = $e2e.create_ms
            live_delete_ms = $e2e.delete_ms
            live_rename_ms = $e2e.rename_ms
            live_move_ms = $e2e.move_ms
            restart_catchup = $e2e.restart_catchup
            restart_catchup_ms = $e2e.restart_catchup_ms
            crash_recovery = $e2e.corrupt_store_recovery
            forced_termination_recovery = $e2e.forced_termination_recovery
            real_files = $e2e.initial_entries
            churn = $e2e.churn
            persistent_bytes = $e2e.persistent_bytes
            source_logical_bytes = $e2e.source_logical_bytes
            persistent_ratio = $e2e.persistent_ratio
            reparse_point = $e2e.reparse_point
            inaccessible_directory = $e2e.inaccessible_directory
            idle_cpu_percent = $idle.cpu_percent
            idle_store_unchanged = $idle.store_unchanged
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
