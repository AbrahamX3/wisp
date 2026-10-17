# Reset Wisp to a true first-run state for testing.
#
#   1. Stops Wisp, nginx and the PHP workers
#   2. Deletes the isolated test config dir (projects, runtimes, settings,
#      including the onboarding-complete flag)
#   3. Clears Wisp's block from the Windows hosts file
#
# Run from an elevated PowerShell (the hosts write needs administrator rights):
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\reset-test.ps1

param(
    # Matches the WISP_CONFIG_DIR used for first-run testing.
    [string]$ConfigDir = (Join-Path $env:TEMP "opencode\wisp-test")
)

$ErrorActionPreference = "SilentlyContinue"

Write-Host "Stopping Wisp, nginx and php-cgi..."
foreach ($name in @("wisp", "nginx", "php-cgi")) {
    if (Get-Process -Name $name -ErrorAction SilentlyContinue) {
        & taskkill /IM "$name.exe" /F *> $null
    }
}
Start-Sleep -Seconds 2

# Remove the no-prompt hosts task, so onboarding's hosts step is exercised from
# scratch (a leftover task makes the step read as already set up).
$task = "WispElevatedControl"
$query = & schtasks /Query /TN $task 2>&1 | Out-String
if ($query -match $task) {
    Write-Host "Removing the no-prompt hosts task ($task)..."
    $delete = & schtasks /Delete /TN $task /F 2>&1 | Out-String
    if ($delete -match "success") {
        Write-Host "Task removed."
    } else {
        Write-Host "WARNING: could not remove the task: $delete" -ForegroundColor Yellow
    }
} else {
    Write-Host "No no-prompt hosts task to remove."
}

Write-Host "Removing test data dir: $ConfigDir"
Remove-Item -Recurse -Force $ConfigDir

# Clear the hosts block through the app itself, so the marker layout stays in
# one place. Falls back to a direct edit when the binary is not built yet.
$exe = Join-Path $PSScriptRoot "..\target\debug\wisp.exe"
if (Test-Path $exe) {
    & $exe --clear-hosts *> $null
} else {
    Write-Host "wisp.exe not built yet; clearing the hosts block directly"
    $hosts = "$env:SystemRoot\System32\drivers\etc\hosts"
    $kept = New-Object System.Collections.Generic.List[string]
    $inBlock = $false
    foreach ($line in Get-Content $hosts) {
        $t = $line.Trim()
        if ($t -eq "# wisp:start") { $inBlock = $true; continue }
        if ($t -eq "# wisp:end") { $inBlock = $false; continue }
        if (-not $inBlock) { $kept.Add($line) }
    }
    Set-Content -Path $hosts -Value $kept
}

# Verify, since the GUI-subsystem binary prints nothing to this console.
$hosts = "$env:SystemRoot\System32\drivers\etc\hosts"
if (Select-String -Path $hosts -Pattern "# wisp:start" -Quiet) {
    Write-Host "WARNING: the hosts block is still there. Run this script as Administrator." -ForegroundColor Yellow
} else {
    Write-Host "Hosts file: Wisp block cleared."
}

Write-Host ""
Write-Host "Clean slate ready. Now run:" -ForegroundColor Green
Write-Host "  `$env:WISP_CONFIG_DIR = `"$ConfigDir`""
Write-Host "  cd $((Resolve-Path (Join-Path $PSScriptRoot "..")).Path)"
Write-Host "  cargo run"
