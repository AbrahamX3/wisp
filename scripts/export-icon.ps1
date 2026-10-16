# Copies the build-time generated app icon beside the Inno Setup script.
#
# `assets/icon.svg` is the single source of truth; build.rs rasterises it into a
# multi-size .ico in the cargo OUT_DIR. Inno Setup needs a file path, so this
# finds the most recent generated icon and copies it to `packaging/icon.ico`
# (git-ignored). Run it after a successful build, before invoking ISCC:
#
#   cargo build --release
#   powershell -File scripts/export-icon.ps1
#   ISCC.exe /DMyAppVersion=0.1.0 packaging\wisp.iss

param(
    [string]$Profile = "release",
    [string]$Destination = "packaging/icon.ico"
)

$ErrorActionPreference = "Stop"

$search = Join-Path "target" (Join-Path $Profile "build")
$icon = Get-ChildItem -Recurse -Path $search -Filter "icon.ico" -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 1

if (-not $icon) {
    throw "No generated icon.ico found under $search. Build the crate first."
}

$directory = Split-Path -Parent $Destination
if ($directory -and -not (Test-Path $directory)) {
    New-Item -ItemType Directory -Force $directory | Out-Null
}

Copy-Item $icon.FullName $Destination -Force
Write-Output "Copied $($icon.FullName) -> $Destination"
