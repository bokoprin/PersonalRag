param(
    [int]$Gate5Docs = 20000,
    [int]$Gate5Rounds = 15,
    [int]$FirstNDocs = 100000,
    [int]$FirstNLimit = 2000,
    [switch]$LaunchShadow
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$SearchCore = Join-Path $Root 'search-core\Cargo.toml'
$BridgeCore = Join-Path $Root 'bridge-core\Cargo.toml'
$Tauri = Join-Path $Root 'src-tauri\Cargo.toml'
$EvidenceDir = Join-Path $Root 'windows-vnext-validation'
$Gate5Root = Join-Path $EvidenceDir 'gate5-work'
$Gate5Log = Join-Path $EvidenceDir 'gate5-windows.txt'
$Summary = Join-Path $EvidenceDir 'summary.txt'

function Step([string]$Text) {
    Write-Host "`n=== $Text ===" -ForegroundColor Cyan
}

New-Item -ItemType Directory -Force -Path $EvidenceDir | Out-Null
Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $Gate5Root

Step 'Full Windows GUI regression/build'
& (Join-Path $Root 'scripts\verify-and-build-windows.ps1')

Step 'Production switch shadow equivalence'
cargo test --manifest-path $SearchCore --locked --test production_switch_shadow -- --nocapture

Step 'Durable crash/restart, compaction and GC regression'
cargo test --manifest-path $SearchCore --locked --test vnext_durable_generation -- --nocapture
cargo test --manifest-path $SearchCore --locked --test vnext_durable_compaction -- --nocapture
cargo test --manifest-path $SearchCore --locked --test vnext_durable_gc -- --nocapture

Step 'Bridge production backend regression'
cargo test --manifest-path $BridgeCore --locked
cargo test --manifest-path $BridgeCore --locked shadow_compare_executor_runs_off_response_thread -- --nocapture
cargo test --manifest-path $BridgeCore --locked shadow_compare_executor_coalesces_duplicate_pending_job -- --nocapture
cargo test --manifest-path $BridgeCore --locked office_cache_reuses_media_only_change_and_refreshes_searchable_xml -- --nocapture
cargo test --manifest-path $BridgeCore --locked sort_order_cache_matches_gui_sort_semantics -- --nocapture
cargo test --manifest-path $BridgeCore --locked regex_prefilter_only_returns_provably_required_prefixes -- --nocapture
cargo clippy --manifest-path $BridgeCore --locked --all-targets -- -D warnings

Step 'Tauri production backend compile gate'
cargo check --manifest-path $Tauri --locked
cargo clippy --manifest-path $Tauri --locked --all-targets -- -D warnings

Step 'Windows native Gate 5 smoke'
$gate5Output = cargo run --release --manifest-path $SearchCore --locked --example vnext_gate5_final_bench -- $Gate5Docs $Gate5Rounds $Gate5Root 2>&1
$gate5Output | Tee-Object -FilePath $Gate5Log
if ($LASTEXITCODE -ne 0) {
    throw "Gate 5 benchmark failed with exit code $LASTEXITCODE"
}
if (-not ($gate5Output -match 'GATE5_BUILD')) { throw 'Gate 5 output is missing GATE5_BUILD' }
if (-not ($gate5Output -match 'GATE5_OPEN')) { throw 'Gate 5 output is missing GATE5_OPEN' }
if (-not ($gate5Output -match 'GATE5_DELTA')) { throw 'Gate 5 output is missing GATE5_DELTA' }
if (-not ($gate5Output -match 'GATE5_COMPACTION')) { throw 'Gate 5 output is missing GATE5_COMPACTION' }

Step 'Windows native GUI first-N gate'
$firstNRoot = Join-Path $EvidenceDir 'first-n-work'
Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $firstNRoot
$firstNOutput = cargo run --release --manifest-path $SearchCore --locked --example vnext_first_n_bench -- $FirstNDocs 15 $FirstNLimit $firstNRoot 2>&1
$firstNOutput | Tee-Object -FilePath (Join-Path $EvidenceDir 'first-n-windows.txt')
if ($LASTEXITCODE -ne 0) { throw "first-N benchmark failed with exit code $LASTEXITCODE" }
if (-not ($firstNOutput -match 'FIRST_N_BENCH')) { throw 'first-N benchmark output is missing FIRST_N_BENCH' }
if (-not ($firstNOutput -match 'adaptive_both_p50_ms=')) { throw 'first-N benchmark output is missing conjunctive adaptive first-N evidence' }

Step 'Record rollout evidence'
$exe = Join-Path $Root 'src-tauri\target\release\personalrag-tauri.exe'
@(
    "WINDOWS_VNEXT_PRODUCTION_SWITCH_VALIDATION_PASS",
    "timestamp=$([DateTimeOffset]::Now.ToString('o'))",
    "gate5_docs=$Gate5Docs",
    "gate5_rounds=$Gate5Rounds",
    "first_n_docs=$FirstNDocs",
    "first_n_limit=$FirstNLimit",
    "async_shadow_test=shadow_compare_executor_runs_off_response_thread",
    "shadow_coalesce_test=shadow_compare_executor_coalesces_duplicate_pending_job",
    "office_cache_test=office_cache_reuses_media_only_change_and_refreshes_searchable_xml",
    "sort_first_n_test=sort_order_cache_matches_gui_sort_semantics",
    "regex_prefilter_test=regex_prefilter_only_returns_provably_required_prefixes",
    "gate5_log=$Gate5Log",
    "exe=$exe",
    "recommended_rollout=shadow",
    "rollback=perf12"
) | Set-Content -Encoding UTF8 $Summary
Get-Content $Summary

if ($LaunchShadow) {
    Step 'Launch GUI in shadow mode'
    if (-not (Test-Path $exe)) { throw "Built executable not found: $exe" }
    $env:PERSONALRAG_SEARCH_CORE_BACKEND = 'shadow'
    Start-Process -FilePath $exe -WorkingDirectory (Split-Path -Parent $exe)
    Write-Host 'GUI launched with PERSONALRAG_SEARCH_CORE_BACKEND=shadow'
}
