param(
    [Parameter(Mandatory = $true)]
    [string]$Root,
    [int]$Rounds = 7
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$bridge = Join-Path $repo 'bridge-core'

if (-not (Test-Path -LiteralPath $Root -PathType Container)) {
    throw "Root is not a directory: $Root"
}

$oldRequire = $env:PR_NATIVE_SCANNER_REQUIRE
$oldProfile = $env:PR_PROFILE_SCANNER
$oldRounds = $env:PR_SCANNER_BENCH_ROUNDS
$oldBufferKib = $env:PR_NATIVE_DIR_BUFFER_KIB
try {
    $env:PR_NATIVE_SCANNER_REQUIRE = '1'
    $env:PR_PROFILE_SCANNER = '1'
    $env:PR_SCANNER_BENCH_ROUNDS = [string][Math]::Max(3, $Rounds)

    Push-Location $bridge
    try {
        Write-Host '== Windows native scanner correctness oracle =='
        cargo test --locked native_scanner_matches_walkdir_oracle_on_real_windows_filesystem -- --nocapture
        if ($LASTEXITCODE -ne 0) { throw "Windows native scanner correctness test failed" }

        Write-Host '== WalkDir vs Win32 batch scanner benchmark (default 1024 KiB) =='
        $env:PR_NATIVE_DIR_BUFFER_KIB = '1024'
        cargo run --release --locked --example windows_native_scanner_bench -- $Root
        if ($LASTEXITCODE -ne 0) { throw "Windows native scanner benchmark failed" }

        Write-Host '== Native buffer A/B reference: 256 KiB baseline =='
        $env:PR_NATIVE_DIR_BUFFER_KIB = '256'
        cargo run --release --locked --example windows_native_scanner_bench -- $Root
        if ($LASTEXITCODE -ne 0) { throw "Windows native scanner 256 KiB benchmark failed" }
    }
    finally {
        Pop-Location
    }
}
finally {
    $env:PR_NATIVE_SCANNER_REQUIRE = $oldRequire
    $env:PR_PROFILE_SCANNER = $oldProfile
    $env:PR_SCANNER_BENCH_ROUNDS = $oldRounds
    $env:PR_NATIVE_DIR_BUFFER_KIB = $oldBufferKib
}
