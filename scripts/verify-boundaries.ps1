$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$TauriCargo = Get-Content -Raw (Join-Path $Root 'src-tauri\Cargo.toml')
$TauriMain = Get-Content -Raw (Join-Path $Root 'src-tauri\src\main.rs')
$BridgeEngine = Get-Content -Raw (Join-Path $Root 'bridge-core\src\engine.rs')
$FrontendContract = Get-Content -Raw (Join-Path $Root 'frontend\src\app_contract_v1.ts')
$Contract = Get-Content -Raw (Join-Path $Root 'app-contract\v1\contract.json') | ConvertFrom-Json

if ($TauriCargo -match 'personalrag-portable-search') {
    throw 'Boundary violation: src-tauri must not depend directly on search-core.'
}
if ($TauriMain -match 'personalrag_portable_search') {
    throw 'Boundary violation: src-tauri source must not import search-core.'
}
if ($BridgeEngine -notmatch 'pub trait SearchEngine' -or $BridgeEngine -notmatch 'pub trait IndexEngine') {
    throw 'Boundary violation: bridge SearchEngine/IndexEngine facade is missing.'
}
if ([int]$Contract.version -ne 1 -or [string]$Contract.name -ne 'personalrag-app-contract') {
    throw 'App Contract manifest is not v1.'
}
if ($FrontendContract -notmatch 'APP_CONTRACT_VERSION = 1') {
    throw 'Frontend App Contract version drift.'
}
Write-Host 'APP_BOUNDARY_CONTRACT_V1_PASS' -ForegroundColor Green
