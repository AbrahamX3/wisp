# Builds GitHub Release notes from Conventional Commits.
#
# Commits are grouped by their type prefix (feat/fix/perf/docs/refactor) so the
# release reads like a changelog. Works with commits pushed straight to the
# default branch, which is why we do not rely on GitHub's own generator (it
# lists pull requests, and this repository does not use them).
#
# Local use:
#   powershell -File scripts/release-notes.ps1 -Tag v0.1.3 `
#     -Repository AbrahamX3/wisp -OutputPath notes.md

param(
    [string]$Tag = $env:GITHUB_REF_NAME,
    [string]$Repository = $env:GITHUB_REPOSITORY,
    [string]$OutputPath = "dist/release-notes.md"
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($Tag)) { throw "Tag is required (pass -Tag or set GITHUB_REF_NAME)" }
if ([string]::IsNullOrWhiteSpace($Repository)) { throw "Repository is required (pass -Repository or set GITHUB_REPOSITORY)" }

# Previous release tag, so the notes cover only this release. Tags are sorted by
# version rather than date so an older backport does not win.
$previous = git tag --list "v*" --sort=-v:refname | Where-Object { $_ -ne $Tag } | Select-Object -First 1
$range = if ($previous) { "$previous..$Tag" } else { $Tag }

$sections = [ordered]@{
    "Features"      = @()
    "Bug Fixes"     = @()
    "Performance"   = @()
    "Documentation" = @()
    "Refactoring"   = @()
    "Maintenance"   = @()
}

$commits = git log --no-merges --format="%h|%s" $range
foreach ($line in $commits) {
    $parts = $line -split "\|", 2
    if ($parts.Count -lt 2) { continue }

    $sha = $parts[0].Trim()
    $subject = $parts[1].Trim()

    $type = ""
    $text = $subject
    if ($subject -match "^([A-Za-z]+)(\([^)]*\))?:\s*(.+)$") {
        $type = $matches[1].ToLower()
        $text = $matches[3].Trim()
    }

    if ($type -eq "feat") { $section = "Features" }
    elseif ($type -eq "fix") { $section = "Bug Fixes" }
    elseif ($type -eq "perf") { $section = "Performance" }
    elseif ($type -eq "docs") { $section = "Documentation" }
    elseif ($type -eq "refactor") { $section = "Refactoring" }
    else { $section = "Maintenance" }

    $sections[$section] += "- $text ([$sha](https://github.com/$Repository/commit/$sha))"
}

$out = @("## What's Changed", "")
foreach ($name in $sections.Keys) {
    if ($sections[$name].Count -eq 0) { continue }
    $out += "### $name"
    $out += $sections[$name]
    $out += ""
}

if ($previous) {
    $out += "**Full Changelog**: https://github.com/$Repository/compare/$previous...$Tag"
} else {
    $out += "**Full Changelog**: https://github.com/$Repository/commits/$Tag"
}

$directory = Split-Path -Parent $OutputPath
if ($directory -and -not (Test-Path $directory)) { New-Item -ItemType Directory -Force $directory | Out-Null }
$out -join "`n" | Set-Content -Path $OutputPath -Encoding utf8

Write-Output "Wrote $OutputPath ($($out.Count) lines) from $range"
