param(
    [string]$Root,
    [string]$Work,
    [string]$Report,
    [ValidateSet("all", "scan", "bloom", "trigram", "sqlite-fts5")]
    [string]$Backend = "all"
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$project = Join-Path $repo "tests\ContentSearch.Bakeoff\ContentSearch.Bakeoff.csproj"

$argsList = @(
    "run",
    "--project", $project,
    "-c", "Release",
    "--"
)

if ($Root) { $argsList += @("--root", $Root) }
if ($Work) { $argsList += @("--work", $Work) }
if ($Report) { $argsList += @("--report", $Report) }
$argsList += @("--backend", $Backend)

& dotnet @argsList
exit $LASTEXITCODE
