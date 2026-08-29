$ErrorActionPreference = 'Stop'
$manifest = Join-Path (Get-Location) 'SOURCE_MANIFEST.sha256'
if (-not (Test-Path -LiteralPath $manifest -PathType Leaf)) {
    throw "SOURCE_MANIFEST.sha256 not found in $(Get-Location)"
}
$fail = @()
$count = 0
Get-Content -LiteralPath $manifest | ForEach-Object {
    if ($_ -notmatch '^([0-9a-fA-F]{64})\s+\*?(.+)$') {
        $fail += "Malformed manifest line: $_"
        return
    }
    $count++
    $expected = $matches[1].ToLowerInvariant()
    $path = $matches[2].Trim()
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        $fail += "Missing: $path"
        return
    }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        $fail += "Mismatch: $path expected=$expected actual=$actual"
    }
}
if ($fail.Count) {
    $fail | ForEach-Object { Write-Error $_ }
    throw "SOURCE_MANIFEST verification failed: $($fail.Count) problem(s)"
}
Write-Host "SOURCE_MANIFEST: $count/$count PASS"
