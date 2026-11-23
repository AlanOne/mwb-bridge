# Installs the companion from a staging folder (run by install.sh over SSH,
# or by hand on the PC): copies the app, puts the config in place, lets the
# bridge in through the firewall (local network only) and starts it now and
# at every logon, in the logged-on user's desktop session.
param([Parameter(Mandatory)][string]$Staging)
$ErrorActionPreference = "Stop"

$app = Join-Path $env:LOCALAPPDATA "MwbBridgeCompanion\app"
$configDir = Join-Path $env:APPDATA "MwbBridgeCompanion"
$taskName = "MWB Bridge Companion"

Get-Process MwbBridgeCompanion -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

New-Item -ItemType Directory -Force $app, $configDir | Out-Null
Copy-Item (Join-Path $Staging "app\*") $app -Recurse -Force
$config = Join-Path $Staging "config.json"
if (Test-Path $config) {
    Move-Item $config (Join-Path $configDir "config.json") -Force
}

if (-not (Get-NetFirewallRule -DisplayName $taskName -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -DisplayName $taskName -Direction Inbound -Protocol TCP -LocalPort 15102 `
        -RemoteAddress LocalSubnet -Action Allow -Profile Any | Out-Null
}

$exe = Join-Path $app "MwbBridgeCompanion.exe"
$action = New-ScheduledTaskAction -Execute $exe
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
Start-ScheduledTask -TaskName $taskName

Remove-Item $Staging -Recurse -Force
Start-Sleep -Seconds 2
$running = Get-Process MwbBridgeCompanion -ErrorAction SilentlyContinue
if ($running) { "Companion running (session $($running.SessionId))." } else { "Companion did not start: see $(Join-Path $env:LOCALAPPDATA 'MwbBridgeCompanion\companion.log')." }
