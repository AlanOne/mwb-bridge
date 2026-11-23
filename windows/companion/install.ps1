# Builds the companion from this checkout and installs it, on the Windows PC
# itself: powershell -ExecutionPolicy Bypass -File windows\companion\install.ps1
# Needs the .NET 8 SDK (`dotnet`); with -InstallSdk it installs one for the
# current user first (into %LOCALAPPDATA%\Microsoft\dotnet). The security
# key comes from PowerToys' Mouse Without Borders settings.
param([switch]$InstallSdk)
$ErrorActionPreference = "Stop"

$dotnet = (Get-Command dotnet -ErrorAction SilentlyContinue).Source
$hasSdk = $dotnet -and ((& $dotnet --list-sdks) -match '^8\.')
if (-not $hasSdk) {
    if (-not $InstallSdk) { throw "No .NET 8 SDK found. Re-run with -InstallSdk, or install it from https://dot.net." }
    $sdkDir = Join-Path $env:LOCALAPPDATA "Microsoft\dotnet"
    $script = Join-Path $env:TEMP "dotnet-install.ps1"
    Invoke-WebRequest https://dot.net/v1/dotnet-install.ps1 -OutFile $script
    & $script -Channel 8.0 -InstallDir $sdkDir
    $dotnet = Join-Path $sdkDir "dotnet.exe"
}

$staging = Join-Path $env:TEMP "mwb-companion-stage"
Remove-Item $staging -Recurse -Force -ErrorAction SilentlyContinue
& $dotnet publish (Join-Path $PSScriptRoot "MwbBridgeCompanion.csproj") -c Release -r win-x64 --self-contained false -o (Join-Path $staging "app") -v quiet
if ($LASTEXITCODE -ne 0) { throw "Build failed." }
& (Join-Path $PSScriptRoot "setup.ps1") -Staging $staging
