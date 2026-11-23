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

echo "==> Building..."
"$DOTNET" publish "$DIR/MwbBridgeCompanion.csproj" -c Release -r win-x64 --self-contained false -o "$DIR/bin/publish" -v quiet

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
