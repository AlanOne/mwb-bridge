# Builds the companion from this checkout and installs it, on the Windows PC
# itself: powershell -ExecutionPolicy Bypass -File windows\companion\install.ps1
# Needs the .NET 8 SDK (`dotnet`); with -InstallSdk it installs one for the
# current user first (into %LOCALAPPDATA%\Microsoft\dotnet). Without the
# .NET 8 desktop runtime installed machine-wide, the runtime is bundled into
# the app. The security key comes from PowerToys' Mouse Without Borders
# settings.
param([switch]$InstallSdk)
$ErrorActionPreference = "Stop"

$dotnet = (Get-Command dotnet -ErrorAction SilentlyContinue).Source
$userDotnet = Join-Path $env:LOCALAPPDATA "Microsoft\dotnet\dotnet.exe"
if (-not $dotnet -and (Test-Path $userDotnet)) { $dotnet = $userDotnet }
$hasSdk = $dotnet -and ((& $dotnet --list-sdks) -match '^8\.')
if (-not $hasSdk) {
    if (-not $InstallSdk) { throw "No .NET 8 SDK found. Re-run with -InstallSdk, or install it from https://dot.net." }
    $sdkDir = Join-Path $env:LOCALAPPDATA "Microsoft\dotnet"
    $script = Join-Path $env:TEMP "dotnet-install.ps1"
    Invoke-WebRequest https://dot.net/v1/dotnet-install.ps1 -OutFile $script
    & $script -Channel 8.0 -InstallDir $sdkDir
    $dotnet = Join-Path $sdkDir "dotnet.exe"
}

# The app looks for its runtime only in the machine-wide .NET folder (a
# user-level one, like -InstallSdk's, isn't found): without the .NET 8
# desktop runtime there, bundle the runtime into the app instead.
$machineRuntime = Join-Path $env:ProgramFiles "dotnet\shared\Microsoft.WindowsDesktop.App"
$hasRuntime = (Test-Path $machineRuntime) -and (Get-ChildItem $machineRuntime -Directory | Where-Object Name -like "8.*")
$selfContained = if ($hasRuntime) { "false" } else { "true" }
if (-not $hasRuntime) { "No machine-wide .NET 8 desktop runtime: bundling the runtime into the app." }

$staging = Join-Path $env:TEMP "mwb-companion-stage"
Remove-Item $staging -Recurse -Force -ErrorAction SilentlyContinue
# nuget.org named explicitly: bundling needs the runtime packs from it, and
# a PC's own NuGet config may list no sources (seen on CHP-ALANJ).
& $dotnet publish (Join-Path $PSScriptRoot "MwbBridgeCompanion.csproj") -c Release -r win-x64 --self-contained $selfContained --source https://api.nuget.org/v3/index.json -o (Join-Path $staging "app") -v quiet
if ($LASTEXITCODE -ne 0) { throw "Build failed." }
& (Join-Path $PSScriptRoot "setup.ps1") -Staging $staging
