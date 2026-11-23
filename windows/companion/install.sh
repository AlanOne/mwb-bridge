#!/bin/bash
# Builds the companion on this Mac and installs it on a Windows PC over SSH:
#   windows/companion/install.sh <ssh-host>
# The PC needs the .NET 8 desktop runtime (PowerToys installs it) and an SSH
# server with PowerShell as the shell; the SSH user is the one it runs for.
# The security key is taken from the Mac bridge's config, so the two always
# match. Re-run after any change to update it in place.
set -euo pipefail

HOST="${1:?usage: install.sh <ssh-host>}"
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DOTNET="$(command -v dotnet || echo "$HOME/.dotnet/dotnet")"
MAC_CONFIG="$HOME/Library/Application Support/mwb-mac-bridge/config.json"

echo "==> Building..."
"$DOTNET" publish "$DIR/MwbBridgeCompanion.csproj" -c Release -r win-x64 --self-contained false -o "$DIR/bin/publish" -v quiet

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir "$STAGE/app"
cp -R "$DIR/bin/publish/." "$STAGE/app/"
cp "$DIR/setup.ps1" "$STAGE/"
# The key of the config entry for this PC (each PC can have its own),
# found by the PC's name. Written without echoing the key anywhere.
PC_NAME="$(ssh -o BatchMode=yes "$HOST" hostname | tr -d '\r')"
( umask 077; python3 - "$MAC_CONFIG" "$STAGE/config.json" "$PC_NAME" <<'PY'
import json, sys
config, out, pc = json.load(open(sys.argv[1])), sys.argv[2], sys.argv[3].lower()
host = lambda e: e.get("windows_ip", "").rsplit(":", 1)[0].lower().removesuffix(".local")
entries = [config] + config.get("other_windows_pcs", [])
match = [e for e in entries if host(e) == pc]
if not match:
    sys.exit(f"No entry for {sys.argv[3]} in {sys.argv[1]} (windows_ip names: {', '.join(host(e) for e in entries)})")
json.dump({"security_key": match[0].get("security_key", config["security_key"]), "port": 15102}, open(out, "w"))
PY
)

REMOTE_STAGE='$env:TEMP\mwb-companion-stage'
REMOTE_TEMP="$(ssh -o BatchMode=yes "$HOST" '$env:TEMP' | tr -d '\r')"
echo "==> Copying to $HOST..."
ssh -o BatchMode=yes "$HOST" "Remove-Item -Recurse -Force $REMOTE_STAGE -ErrorAction SilentlyContinue; New-Item -ItemType Directory $REMOTE_STAGE | Out-Null"
scp -q -o BatchMode=yes -r "$STAGE/app" "$STAGE/setup.ps1" "$STAGE/config.json" "$HOST:${REMOTE_TEMP//\\//}/mwb-companion-stage/"
echo "==> Installing..."
ssh -o BatchMode=yes "$HOST" "powershell -NoProfile -ExecutionPolicy Bypass -File $REMOTE_STAGE\\setup.ps1 -Staging $REMOTE_STAGE"
