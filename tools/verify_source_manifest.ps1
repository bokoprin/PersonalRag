$ErrorActionPreference = 'Stop'

$manifest = Join-Path (Get-Location) 'SOURCE_MANIFEST.sha256'
if (-not (Test-Path -LiteralPath $manifest -PathType Leaf)) {
    throw "SOURCE_MANIFEST.sha256 not found in $(Get-Location)"
}

function Get-Sha256Hex([byte[]]$Bytes) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        return ([System.BitConverter]::ToString($sha.ComputeHash($Bytes))).Replace('-', '').ToLowerInvariant()
    }
    finally {
        $sha.Dispose()
    }
}

function Convert-CrlfToLf([byte[]]$Bytes) {
    $output = New-Object System.Collections.Generic.List[byte]
    for ($i = 0; $i -lt $Bytes.Length; $i++) {
        if ($Bytes[$i] -eq 13 -and ($i + 1) -lt $Bytes.Length -and $Bytes[$i + 1] -eq 10) {
            [void]$output.Add(10)
            $i++
        }
        else {
            [void]$output.Add($Bytes[$i])
        }
    }
    return $output.ToArray()
}

function Get-EolAttribute([string]$Path) {
    $result = & git check-attr eol -- $Path 2>$null
    if ($LASTEXITCODE -ne 0 -or -not $result) {
        return $null
    }
    if ($result -match ':\s+eol:\s+(.+)$') {
        return $matches[1].Trim()
    }
    return $null
}

$fail = @()
$count = 0
$normalizedLineEndingCount = 0

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
    if ($actual -eq $expected) {
        return
    }

    # Canonical repository text is LF. Existing Windows worktrees created before
    # .gitattributes may still contain CRLF while remaining Git-clean. Accept
    # only the exact CRLF->LF normalized bytes when Git declares eol=lf.
    if ((Get-EolAttribute $path) -eq 'lf') {
        $resolved = (Resolve-Path -LiteralPath $path).Path
        $bytes = [System.IO.File]::ReadAllBytes($resolved)
        $normalized = Convert-CrlfToLf $bytes
        $normalizedHash = Get-Sha256Hex $normalized
        if ($normalizedHash -eq $expected) {
            $normalizedLineEndingCount++
            Write-Warning "Accepted legacy CRLF worktree bytes for LF-canonical file: $path"
            return
        }
    }

    $fail += "Mismatch: $path expected=$expected actual=$actual"
}

if ($fail.Count) {
    $fail | ForEach-Object { Write-Error $_ }
    throw "SOURCE_MANIFEST verification failed: $($fail.Count) problem(s)"
}

Write-Host "SOURCE_MANIFEST: $count/$count PASS normalized_legacy_crlf=$normalizedLineEndingCount"
