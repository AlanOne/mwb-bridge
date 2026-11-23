# Installs the companion from a staging folder holding the built app in
# `app\` (and optionally a `config.json`): copies the app, lets the bridge in
# through the firewall (local network only) and starts it now and at every
# logon, in the logged-on user's desktop session. Run by install.sh (over
# SSH) and install.ps1 (on the PC). Without admin rights the firewall rule
# is left to Windows' own prompt and a Startup-folder shortcut replaces the
# logon task.
param([Parameter(Mandatory)][string]$Staging)
$ErrorActionPreference = "Stop"

$app = Join-Path $env:LOCALAPPDATA "MwbBridgeCompanion\app"
$configDir = Join-Path $env:APPDATA "MwbBridgeCompanion"
$name = "MWB Bridge Companion"
$exe = Join-Path $app "MwbBridgeCompanion.exe"

Get-Process MwbBridgeCompanion -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

New-Item -ItemType Directory -Force $app, $configDir | Out-Null
Copy-Item (Join-Path $Staging "app\*") $app -Recurse -Force
$config = Join-Path $Staging "config.json"
if (Test-Path $config) {
    Move-Item $config (Join-Path $configDir "config.json") -Force
}

try {
    if (-not (Get-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue)) {
        New-NetFirewallRule -DisplayName $name -Direction Inbound -Protocol TCP -LocalPort 15102 `
            -RemoteAddress LocalSubnet -Action Allow -Profile Any | Out-Null
    }
} catch {
    "Couldn't add the firewall rule (needs admin): allow it when Windows asks, or add an inbound rule for TCP 15102."
}

$startup = Join-Path ([Environment]::GetFolderPath("Startup")) "$name.lnk"
try {
    $action = New-ScheduledTaskAction -Execute $exe
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
    $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName $name -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
    Remove-Item $startup -ErrorAction SilentlyContinue
    Start-ScheduledTask -TaskName $name
} catch {
    # Without admin: a Startup shortcut, and start it directly (this works
    # when setup itself runs in the user's desktop session).
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut($startup)
    $link.TargetPath = $exe
    $link.Save()
    Start-Process $exe
}

Remove-Item $Staging -Recurse -Force
# Running isn't enough: a missing .NET runtime shows its own dialog from a
# process of the same name. The app's "Listening" log line means it's up.
$log = Join-Path $env:LOCALAPPDATA "MwbBridgeCompanion\companion.log"
$since = Get-Date
for ($i = 0; $i -lt 20; $i++) {
    Start-Sleep -Milliseconds 500
    if ((Test-Path $log) -and (Get-Item $log).LastWriteTime -ge $since.AddSeconds(-1) -and (Select-String -Path $log -Pattern "Listening on port" -SimpleMatch -Quiet)) {
        $running = Get-Process MwbBridgeCompanion -ErrorAction SilentlyContinue
        "Companion running and listening (session $($running.SessionId))."
        exit 0
    }
}
Write-Error "The companion didn't start listening within 10s. If a .NET dialog is showing, its runtime is missing; otherwise see $log."
