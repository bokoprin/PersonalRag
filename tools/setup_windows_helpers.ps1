param(
    [switch]$Install
)

$ErrorActionPreference = 'Stop'

function Resolve-CommandPath([string]$Name) {
    $cmd = Get-Command $Name -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    return $null
}

function Find-WinGetPackageHelper([string]$Name) {
    $root = Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages'
    if (-not (Test-Path -LiteralPath $root -PathType Container)) { return $null }
    return Get-ChildItem -LiteralPath $root -Filter $Name -File -Recurse -ErrorAction SilentlyContinue |
        Select-Object -First 1 -ExpandProperty FullName
}

function Resolve-NativeHelper([string]$Name) {
    $path = Resolve-CommandPath $Name
    if ($path) { return $path }
    return Find-WinGetPackageHelper $Name
}

function Resolve-ZipReader {
    if ($env:PERSONALRAG_UNZIP -and (Test-Path -LiteralPath $env:PERSONALRAG_UNZIP -PathType Leaf)) {
        return $env:PERSONALRAG_UNZIP
    }

    if ($env:SystemRoot) {
        $tar = Join-Path $env:SystemRoot 'System32\tar.exe'
        if (Test-Path -LiteralPath $tar -PathType Leaf) { return $tar }
    }

    $tar = Resolve-CommandPath 'tar.exe'
    if ($tar) { return $tar }

    # Do not auto-select Git\usr\bin\unzip.exe. It is an MSYS program and
    # does not reliably accept native Win32 verbatim paths used by PersonalRag.
    $unzip = Find-WinGetPackageHelper 'unzip.exe'
    if ($unzip -and $unzip -notmatch '\\Git\\usr\\bin\\') { return $unzip }
    return $null
}

if ($Install) {
    if (-not (Get-Command winget.exe -ErrorAction SilentlyContinue)) {
        throw 'winget.exe is required for -Install. Install/update Windows App Installer first.'
    }
    if (-not (Resolve-NativeHelper 'pdftotext.exe')) {
        winget install --id oschwartz10612.Poppler --exact --accept-package-agreements --accept-source-agreements
        if ($LASTEXITCODE -ne 0) { throw "Poppler winget install failed: $LASTEXITCODE" }
    }
    if (-not (Resolve-NativeHelper 'zstd.exe')) {
        winget install --id Meta.Zstandard --exact --accept-package-agreements --accept-source-agreements
        if ($LASTEXITCODE -ne 0) { throw "Zstandard winget install failed: $LASTEXITCODE" }
    }
}

$helpers = [ordered]@{
    pdftotext = Resolve-NativeHelper 'pdftotext.exe'
    zip_reader = Resolve-ZipReader
    zstd = Resolve-NativeHelper 'zstd.exe'
}

$helpers.GetEnumerator() | ForEach-Object {
    if ($_.Value) {
        Write-Host ("HELPER {0}=PASS path={1}" -f $_.Key, $_.Value)
    }
    else {
        Write-Host ("HELPER {0}=MISSING" -f $_.Key)
    }
}

if ($helpers.pdftotext) { Write-Host "PERSONALRAG_PDFTOTEXT=$($helpers.pdftotext)" }
if ($helpers.zip_reader) { Write-Host "PERSONALRAG_UNZIP=$($helpers.zip_reader)" }
if ($helpers.zstd) { Write-Host "PERSONALRAG_ZSTD=$($helpers.zstd)" }

if (-not $helpers.zip_reader) {
    Write-Error 'No native ZIP reader found. Windows 10/11 built-in tar.exe is preferred.'
}

if ($helpers.Values -contains $null) {
    exit 2
}
exit 0
