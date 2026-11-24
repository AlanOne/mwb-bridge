# Agent notes

Read this before working in this repo. `README.md` is the user-facing guide
and `daemon/PROTOCOL.md` is the reverse-engineered wire protocol. Read
PROTOCOL.md before touching any crypto, framing, handshake or keymap code.

## Rules

- **Commit and push every finished change.** After each finished change,
  commit it and push to `master` without asking first. Don't leave work
  uncommitted or unpushed between changes.
- **Keep the README up to date with every commit.** Every commit must leave
  `README.md` accurate. When a change adds, changes or removes behavior, a
  requirement, a config field or a file path, update the README in the same
  commit, not in a later catch-up. Before committing, check the parts a
  change can affect: the status lines, **Known bugs**, the config docs, the
  **Layout** paths and any caveats the change makes stale. Keep this file
  accurate the same way, including the Linux gaps list below.
- **Reinstall the companion on both PCs after changing it.** Whenever a
  commit changes `windows/companion/` (or the shared protocol in a way the
  companion needs), push it and then reinstall on both PCs without asking:
  AlanOnePC with `windows/companion/install.sh alanonepc`, and CHP-ALANJ
  through a T3 task on its linked environment that runs `git pull` and
  `install.ps1 -InstallSdk` in its checkout (see below). Check each ends up
  running in the user's session and its log shows the bridge connecting.
- **Say what was live-tested.** Mark features as live-tested (with the date)
  only after trying them against a real Windows PC, and say "type-checked
  only" when that's all that happened.

## What this is

A bridge that lets a Windows PC running Microsoft PowerToys' **Mouse Without
Borders** (MWB) control other machines that MWB doesn't support: an Omarchy
(Hyprland) Linux machine and a Mac. The Windows keyboard and mouse drive the
other machine (edge-crossing or MWB's switch hotkeys), and the clipboard,
files and lock-screen double-tap carry across. The mainstream tools
(Synergy, Barrier, Input Leap, Deskflow) don't work on Hyprland because its
portal lacks the `RemoteDesktop` interface they need.

Each bridge pretends to be an MWB machine: it connects to Windows' MWB
message server, does MWB's handshake and per-connection AES encryption, and
turns MWB's Mouse/Keyboard packages into local input.

## How it works

`daemon/` is a Cargo workspace with three crates:

- `daemon/protocol/` (`mwb_protocol`): everything OS-agnostic. Crypto,
  framing, handshake, clipboard chunking, file-transfer framing,
  Mouse/Keyboard decoding (`input_handling.rs`), Windows VK to evdev
  translation (`vk_keycode.rs`), drag-and-drop packages (`drag_drop.rs`) and
  the JSON config/status format (`config.rs`). Both platform builds use it
  unchanged.
- `daemon/linux/` (`mwb_omarchy_bridge`, binary `daemon`): injects input
  through Hyprland's wlroots protocols (`zwlr_virtual_pointer_v1`,
  `zwp_virtual_keyboard_v1`) in `wayland_input.rs`, and syncs the clipboard
  with `wl-copy`/`wl-paste`. Runs as the systemd user service
  `mwb-omarchy-bridge.service`, managed by the Omarchy bar-widget plugin at
  the repo root (`manifest.json`, `BarWidget.qml`).
- `daemon/macos/` (`mwb_mac_bridge`): injects input with Core Graphics
  `CGEventPost` (`cg_input.rs`), and is its own menu-bar app (`tray.rs`)
  packaged as `MWB Mac Bridge.app` and started by a LaunchAgent.

`windows/companion/` is an optional C# (.NET 8, WinForms) tray app for the
Windows PC that takes over file drag-and-drop from PowerToys (whose
"Transfer file" must then be off): multi-file drags both ways, and drops
into any Windows app. The bridge connects to it on port 15102 with MWB's
encryption; `daemon/protocol/src/companion.rs` and
`windows/companion/Protocol.cs` are the two ends of that protocol and must
stay in step. Build it on the Mac with the .NET 8 SDK (`~/.dotnet/dotnet
build -c Release` in that folder) and install it with
`windows/companion/install.sh <ssh-host>`, or on the PC itself with
`windows\companion\install.ps1` (`-InstallSdk` adds a user-level .NET 8 SDK).
It reads the security key from MWB's own `settings.json` (`SecurityKey`,
stored in plain text), unless `%APPDATA%\MwbBridgeCompanion\config.json`
sets one. AlanOnePC is reachable as the SSH host `alanonepc` (PowerShell
shell); CHP-ALANJ has no SSH, only a T3 linked environment, with this repo
checked out at `C:\Users\Alan\Work\omarchy-plugins\mwb-bridge` (install
there with `install.ps1 -InstallSdk`: it has no machine-wide .NET 8, so the
runtime gets bundled, and its NuGet config lists no sources, hence the
explicit `--source`). A GUI app has to
be started in the user's session, which setup.ps1 does with a logon
scheduled task (or a Startup shortcut without admin). Its log is
`%LOCALAPPDATA%\MwbBridgeCompanion\companion.log`.

Network facts that matter:

- Windows' message server is on port **15101** (not 15100). The clipboard and
  file server is on 15100.
- **PowerToys v0.102 or newer is required.** Each TCP direction starts with a
  cleartext 16-byte salt and IV, and the AES key is
  PBKDF2-SHA512(key, salt, 100000). Only this scheme is implemented.
- Windows never opens a connection back to the bridge. To pull clipboard
  data or a file from us it sends a `ClipboardAsk` (type 78), and we connect
  to its port 15100 and push. See PROTOCOL.md's ClipboardAsk section.
- Windows must list the bridge in `MachineMatrixString` and `MachinePool` in
  `%LOCALAPPDATA%\Microsoft\PowerToys\MouseWithoutBorders\settings.json`
  (hand-edited with PowerToys closed; its settings UI doesn't save this
  reliably). The bridge's `machine_name` and `machine_id` must match.

## Setting it up on Linux (Omarchy)

1. Install the plugin: `omarchy plugin add
   https://github.com/AlanOne/omarchy-plugin-mwb-bridge.git --enable`, or
   check the repo out (or symlink it) to
   `~/.config/omarchy/plugins/io.github.alanone.mwb-bridge/` and add
   `{ "id": "io.github.alanone.mwb-bridge" }` to the bar layout in
   `~/.config/omarchy/shell.json`.
2. Reload the bar. On first load the widget runs `cargo build --release -p
   mwb_omarchy_bridge --target-dir ~/.cache/omarchy-mwb-bridge-build`
   (needs a Rust toolchain), writes
   `~/.config/systemd/user/mwb-omarchy-bridge.service` and enables it.
3. In the widget's popup, fill in the security key, the Windows host and
   port (e.g. `WINPC.local:15101`) and a machine name, then **Save and
   restart**. The keyboard layout is read from `hyprctl` automatically.
4. Add the machine on the Windows side (see above).

Files and commands:

- Config: `~/.local/share/omarchy-mwb-bridge/config.json` (read only at
  startup, so restart after editing). Status:
  `~/.local/share/omarchy-mwb-bridge/status.json`.
- Logs: `journalctl --user -u mwb-omarchy-bridge.service -f`.
- Restart: `systemctl --user restart mwb-omarchy-bridge.service`.
- Needs `wl-clipboard`, which Omarchy ships.

## Setting it up on macOS

1. `bash daemon/macos/packaging/install.sh` builds the release binary,
   assembles `~/Applications/MWB Mac Bridge.app`, signs it with a stable
   self-signed certificate (`mwb-mac-bridge-dev`, created on first run, may
   ask for the login keychain password once), installs the "Slovenian
   (Windows)" keyboard layout and loads the LaunchAgent
   `~/Library/LaunchAgents/com.alanone.mwb-mac-bridge.plist`. Re-run it after
   every change to rebuild and reinstall in place.
2. One-time manual steps: add the app under System Settings > Privacy &
   Security > Accessibility (without it, input silently does nothing), and
   pick the "Slovenian (Windows)" input source.
3. Fill in the main fields from the menu bar icon's "Settings..." (it saves
   and restarts the app), or anything else in config.json with "Edit
   Config...", then "Restart App".
4. Add the Mac on the Windows side (see above).

Files and commands:

- Config: `~/Library/Application Support/mwb-mac-bridge/config.json`
  (same fields as Linux, plus `other_windows_pcs`; `xkb_*` are ignored).
  Status: `status.json` in the same folder.
- Logs: `~/Library/Logs/mwb-mac-bridge/mwb-mac-bridge.log` and
  `mwb-mac-bridge.err.log`.
- Stop: `launchctl bootout gui/$(id -u)/com.alanone.mwb-mac-bridge` (it
  restarts on its own otherwise). Start again by re-running `install.sh` or
  `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.alanone.mwb-mac-bridge.plist`.
- Quick run without installing: `cargo run -p mwb_mac_bridge` in `daemon/`.

## Building and checking

From `daemon/`:

- `cargo test -p mwb_protocol` runs the shared protocol tests.
- `cargo test -p mwb_mac_bridge` runs the macOS tests (on a Mac).
- `~/.dotnet/dotnet run` in `windows/companion-tests/` checks the Windows
  companion's protocol code (it runs on the Mac too).
- `cargo check -p mwb_omarchy_bridge --target x86_64-unknown-linux-gnu`
  type-checks the Linux build from a Mac. It can't be run there, so Linux
  changes made on a Mac are "type-checked only" until tried on the Omarchy
  machine.

## Linux gaps compared with macOS

Most work since early November 2026 went into the macOS build. On Linux:

- **The v0.102 encryption is untested.** The Linux daemon was only
  type-checked after that change and has never connected to a v0.102+
  PowerToys. Test this first.
- **`ClipboardAsk` isn't answered.** Linux drops type 78, which is most
  likely why copying files and big images from Linux to Windows doesn't work
  (README **Known bugs**). Port the Mac's `push_to_windows`.
- **Only one Windows PC.** Linux connects to `windows_ip` only and ignores
  `other_windows_pcs`. The Mac loops over `Config::per_windows_pc()` and
  moves on to the next PC when a handshake fails.
- **No drag-and-drop** in either direction. On Wayland this needs a real
  drag source under the cursor, which is the hardest item here.
- **No HTML-only clipboard handling**, so a Windows copy that has only HTML
  (seen from browsers) probably arrives empty.
- **No big-image downscaling** for Linux to Windows (the Mac shrinks images
  over 1 MB to fit the small path). Answering `ClipboardAsk` may make this
  unnecessary.
- **Weaker diagnostics:** no "key or protocol mismatch" status when packages
  don't decrypt, and no logging of unhandled package types.
- **No companion client.** Linux never connects to the Windows companion,
  so it has none of what that brings the Mac: several files and folders per
  copy, any size, and transfer progress and cancel. The shared pieces are
  ready in `mwb_protocol::companion` (`open`, `send_files`,
  `receive_files`, `expand_offer_paths`, `Transfers`, `progress_line`);
  the Mac's `companion.rs` is the client to port.

Already on both, so no porting needed: fast reconnect after sleep (Linux
uses logind's `PrepareForSleep`, the Mac a wall-clock watchdog), relative
mouse mode with `windows_side` and `neighbors`, the lock-both-machines
double-tap, keeping the screen on while Windows is in use (MWB's `Awake`
packages: the Mac declares user activity, Linux holds a `systemd-inhibit`
idle lock), media and extra keys, and `self_register`. The Mac's
wired-over-Wi-Fi preference, Windows-style key remapping, staying awake on
power while connected, and its menu bar's Settings window, link line and
"Reconnect Now" are Mac-specific and not needed on Linux (the Omarchy
widget is its settings UI).
