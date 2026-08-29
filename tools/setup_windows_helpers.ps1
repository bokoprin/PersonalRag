param(
    [switch]$Install
)

$ErrorActionPreference = 'Stop'

function Resolve-CommandPath([string]$Name) {
    $cmd = Get-Command $Name -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    return $null
}

function Find-GitHelper([string]$Name) {
    $candidates = @(
        (Join-Path $env:ProgramFiles "Git\usr\bin\$Name"),
        (Join-Path ${env:ProgramFiles(x86)} "Git\usr\bin\$Name"),
        (Join-Path $env:LOCALAPPDATA "Programs\Git\usr\bin\$Name")
    ) | Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Leaf) }
    return $candidates | Select-Object -First 1
}

function Find-WinGetPackageHelper([string]$Name) {
    $root = Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages'
    if (-not (Test-Path -LiteralPath $root -PathType Container)) { return $null }
    return Get-ChildItem -LiteralPath $root -Filter $Name -File -Recurse -ErrorAction SilentlyContinue |
        Select-Object -First 1 -ExpandProperty FullName
}

function Resolve-Helper([string]$Name) {
    $path = Resolve-CommandPath $Name
    if ($path) { return $path }
    $path = Find-GitHelper $Name
    if ($path) { return $path }
    return Find-WinGetPackageHelper $Name
}

if ($Install) {
    if (-not (Get-Command winget.exe -ErrorAction SilentlyContinue)) {
        throw 'winget.exe is required for -Install. Install/update Windows App Installer first.'
    }
    if (-not (Resolve-Helper 'pdftotext.exe')) {
        winget install --id oschwartz10612.Poppler --exact --accept-package-agreements --accept-source-agreements
        if ($LASTEXITCODE -ne 0) { throw "Poppler winget install failed: $LASTEXITCODE" }
    }
    if (-not (Resolve-Helper 'zstd.exe')) {
        winget install --id Meta.Zstandard --exact --accept-package-agreements --accept-source-agreements
        if ($LASTEXITCODE -ne 0) { throw "Zstandard winget install failed: $LASTEXITCODE" }
    }
    if (-not (Resolve-Helper 'unzip.exe')) {
        winget install --id Git.Git --exact --accept-package-agreements --accept-source-agreements
        if ($LASTEXITCODE -ne 0) { throw "Git winget install failed: $LASTEXITCODE" }
    }
}

$helpers = [ordered]@{
    pdftotext = Resolve-Helper 'pdftotext.exe'
    unzip      = Resolve-Helper 'unzip.exe'
    zstd       = Resolve-Helper 'zstd.exe'
}

$helpers.GetEnumerator() | ForEach-Object {
    if ($_.Value) {
        Write-Host ("HELPER {0}=PASS path={1}" -f $_.Key, $_.Value)
    } else {
        Write-Host ("HELPER {0}=MISSING" -f $_.Key)
    }
}

if ($helpers.pdftotext) { Write-Host "PERSONALRAG_PDFTOTEXT=$($helpers.pdftotext)" }
if ($helpers.unzip)      { Write-Host "PERSONALRAG_UNZIP=$($helpers.unzip)" }
if ($helpers.zstd)       { Write-Host "PERSONALRAG_ZSTD=$($helpers.zstd)" }

if ($helpers.Values -contains $null) {
    exit 2
}
exit 0
