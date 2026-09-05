param(
    [string]$Report = (Join-Path (Join-Path (Get-Location) 'reports') 'filename-e2e.json'),
    [int]$Count = 100000
)
$ErrorActionPreference = 'Stop'
$repo = (Get-Location).Path
$root = Join-Path ([System.IO.Path]::GetTempPath()) ('personalrag-filename-e2e-' + [guid]::NewGuid().ToString('N'))
try {
    dotnet run --project (Join-Path $repo 'tests/FilenameSearch.E2E/FilenameSearch.E2E.csproj') --configuration Release -- $root $Report $Count
    if ($LASTEXITCODE -ne 0) { throw "Filename E2E failed with exit code $LASTEXITCODE" }
    $result = Get-Content $Report -Raw | ConvertFrom-Json
    if (-not $result.pass) { throw 'Filename E2E report is not PASS' }
    $result | ConvertTo-Json -Depth 10
}
finally {
    if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue }
}
