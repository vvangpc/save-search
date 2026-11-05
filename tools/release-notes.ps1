param(
    [Parameter(Mandatory = $true)][string]$Tag,
    [Parameter(Mandatory = $true)][string]$OutputPath
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
if ($Tag -notmatch '^v(\d+\.\d+\.\d+)$') {
    throw "Release tag must use vX.Y.Z: $Tag"
}
$version = $Matches[1]
$manifest = Get-Content -LiteralPath (Join-Path $projectRoot 'Cargo.toml') -Raw
$packageSection = [regex]::Match($manifest, '(?ms)^\[workspace\.package\]\s*\r?\n(.*?)(?=^\[|\z)').Groups[1].Value
$packageVersion = [regex]::Match($packageSection, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
if ($packageVersion -ne $version) {
    throw "Tag $Tag does not match Cargo.toml version $packageVersion"
}

$changelog = Get-Content -LiteralPath (Join-Path $projectRoot 'CHANGELOG.md') -Raw -Encoding utf8
$pattern = '(?ms)^## \[' + [regex]::Escape($version) + '\] - \d{4}-\d{2}-\d{2}\s*\r?\n(.*?)(?=^## |^\[Unreleased\]:|\z)'
$sections = [regex]::Matches($changelog, $pattern)
if ($sections.Count -ne 1) {
    throw "CHANGELOG.md must contain exactly one dated entry for $version"
}
$notes = $sections[0].Groups[1].Value.Trim()
if ($notes -notmatch '(?m)^### ' -or $notes -notmatch '(?m)^- \S') {
    throw "CHANGELOG.md entry for $version must contain categories and change items"
}
[System.IO.File]::WriteAllText($OutputPath, $notes + "`n", [System.Text.UTF8Encoding]::new($false))
Write-Output "Prepared release notes for $Tag"
