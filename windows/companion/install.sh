#!/bin/bash
# Builds the companion on this Mac and installs it on a Windows PC over SSH:
#   windows/companion/install.sh <ssh-host>
# The PC needs the .NET 8 desktop runtime (PowerToys installs it) and an SSH
# server with PowerShell as the shell; the SSH user is the one it runs for.
# The companion reads the security key from PowerToys' own settings on the
# PC. Re-run after any change to update it in place.
set -euo pipefail

HOST="${1:?usage: install.sh <ssh-host>}"
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DOTNET="$(command -v dotnet || echo "$HOME/.dotnet/dotnet")"

# Without the .NET 8 desktop runtime installed machine-wide on the PC (the
# app finds no other), bundle the runtime into the app.
HAS_RUNTIME="$(ssh -o BatchMode=yes "$HOST" '[bool](Get-ChildItem "$env:ProgramFiles\dotnet\shared\Microsoft.WindowsDesktop.App" -Directory -ErrorAction SilentlyContinue | Where-Object Name -like "8.*")' | tr -d '\r')"
SELF_CONTAINED=false
if [ "$HAS_RUNTIME" != "True" ]; then
    echo "==> No machine-wide .NET 8 desktop runtime on $HOST: bundling the runtime into the app."
    SELF_CONTAINED=true
fi
echo "==> Building..."
rm -rf "$DIR/bin/publish"
"$DOTNET" publish "$DIR/MwbBridgeCompanion.csproj" -c Release -r win-x64 --self-contained "$SELF_CONTAINED" --source https://api.nuget.org/v3/index.json -o "$DIR/bin/publish" -v quiet

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir "$STAGE/app"
cp -R "$DIR/bin/publish/." "$STAGE/app/"
cp "$DIR/setup.ps1" "$STAGE/"
REMOTE_STAGE='$env:TEMP\mwb-companion-stage'
REMOTE_TEMP="$(ssh -o BatchMode=yes "$HOST" '$env:TEMP' | tr -d '\r')"
echo "==> Copying to $HOST..."
ssh -o BatchMode=yes "$HOST" "Remove-Item -Recurse -Force $REMOTE_STAGE -ErrorAction SilentlyContinue; New-Item -ItemType Directory $REMOTE_STAGE | Out-Null"
scp -q -o BatchMode=yes -r "$STAGE/app" "$STAGE/setup.ps1" "$HOST:${REMOTE_TEMP//\\//}/mwb-companion-stage/"
echo "==> Installing..."
ssh -o BatchMode=yes "$HOST" "powershell -NoProfile -ExecutionPolicy Bypass -File $REMOTE_STAGE\\setup.ps1 -Staging $REMOTE_STAGE"
