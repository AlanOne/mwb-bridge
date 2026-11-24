# Review findings and implementation plan (2026-11-23)

Two independent deep reviews of commit `cd0e06b`, both read-only:
Claude Fable 5.2 and Grok 4.7, each at xhigh effort. This file merges them,
drops duplicates, and records which findings were checked against the code
or the PowerToys source. Nothing here is live-tested yet.

"Both" means both reviewers found it independently. "Checked" means the
claim was confirmed against our code or PowerToys' (`microsoft/PowerToys`,
`src/modules/MouseWithoutBorders/App`).

## 1. Bugs

### High

| # | Bug | Where | Found by | Status |
|---|-----|-------|----------|--------|
| B1 | Held modifiers, keys and buttons aren't released when control leaves. Real MWB calls `ReleaseAllKeys()` on `HideMouse`. After a hotkey switch (Ctrl+Alt+F1) the key-ups go to the new machine, so Ctrl and Alt stay down on the Mac and every later click is a shortcut. A re-press counts as a repeat, so it doesn't heal. On Linux, a stuck non-modifier key auto-repeats. | `macos/src/main.rs` HideMouse arm; `linux/src/bin/daemon.rs`; `protocol/src/input_handling.rs` repeat dedup | Fable | Checked: `CgInput::reset` only runs at connection start and end |
| B2 | The Linux daemon prints every key VK and mouse packet to the journal. Passwords end up in `journalctl --user`, and every event pays for a synchronous write. | `daemon.rs` (key and mouse logging) | Both | Both reviewers cite the same lines |
| B3 | Releasing over the Mac before a dragged file has arrived blocks the input thread for up to 120s. Starting a drag also sleeps 40ms, polls for up to 300ms, and waits up to 1s for the main thread on that thread. `cancel_drag` sleeps 50ms. Input freezes, then replays late. | `macos/src/drag_in.rs` `dropped`, `on_move`; `cg_input.rs` `cancel_drag` | Both | Checked |
| B4 | The companion link survives sleep: `watch_for_wake` only cuts the MWB socket. The companion socket has no read or write timeout and no keepalive, so `is_connected` stays true on a dead link. Copies go to the companion only and are lost, with no fallback to the PowerToys path. `send_frame` writes on the input thread while holding the writer lock. | `macos/src/main.rs` `watch_for_wake`; `macos/src/companion.rs` | Grok | Checked |
| B5 | Linux never answers `ClipboardAsk` (78), so files and big images never reach Windows from Linux. It also still sends `HideMouse` with the announcement. | `daemon.rs` receive match, `announce_big_path` | Grok (known, in AGENTS.md) | Checked |

### Medium

| # | Bug | Where | Found by | Status |
|---|-----|-------|----------|--------|
| B6 | Outbound clipboard, file copies and companion offers only go out when a packet arrives from Windows, because the queue is drained just before a blocking read. An inactive machine only gets a heartbeat every 1 or 5 minutes. A copy made on the Mac just before flicking to Windows can wait that long. PROTOCOL.md's "well under a second" is wrong for an idle machine. | `macos/src/main.rs` loop head; Linux the same | Fable | Checked: `frmScreen.cs` "Send out Heartbeat every 1 or 5 mins" |
| B7 | Letting go on Windows before every file of a Mac drag has arrived: `QueryContinueDrag` answers `Continue` with the button up. The drop then lands wherever the cursor is when the last byte arrives, not where you let go. It may also wait for the next input event, since nothing wakes OLE when a file finishes. | `windows/companion/Coordinator.cs` `OnSourcePressed`, `IncomingDrop.FileDone` | Both | Checked |
| B8 | Two files with the same name in one Mac-to-Windows offer overwrite each other. The Mac side has `unique_path`, the companion doesn't. | `Coordinator.cs` `IncomingDrop.Create` | Both | Checked |
| B9 | When a control connection is replaced (Mac reconnects), the old one's `finally` still sets the status to "waiting" and cancels every in-flight drop, including the new connection's. | `windows/companion/Server.cs` `RunControl` | Fable | Checked |
| B10 | `INACTIVE.button_held` is the Mac's synthetic button state, and the companion reads it as "the drag came back to Windows". Drop on the Mac, then come back with the button held for something else, and Explorer's leftover drag goes live again. A quick bounce can end a drag that's still held. Disconnecting hides the catcher while a leftover drag exists. | `macos/src/main.rs` HideMouse; `Coordinator.cs` `OnInactive`, `OnBridgeDisconnected` | Fable | Plausible, needs a live test |
| B11 | Mouse and keyboard packages are injected before the handshake challenge has matched. Each direction's key comes from the sender's own salt, so a recorded Windows-to-bridge stream can be replayed to a bridge whose connection is redirected on the LAN. | `macos/src/main.rs`, `daemon.rs` | Fable | Checked: the ack only sets the status |
| B12 | Without the companion (PowerToys path), Windows treats any header name starting with `image` or `text` (case-insensitive) as clipboard data. Dragging `image001.png` or `Text notes.txt` from the Mac delivers nothing. Real MWB sends a full path, so it never collides. | `macos/src/clipboard.rs` `outbound_items`; `daemon.rs` | Fable | Checked: `Clipboard.cs` `StartsWith("image"/"text")` |
| B13 | Text over 1MB (deflated) copied on Windows arrives as the header `{len}*text`, and the bridges save it as a file named `text`. | `macos/src/clipboard.rs` `pull_from_windows`; `daemon.rs` | Fable | Checked: `SocketStuff.cs` `headerString = $"{data.Length}*{"text"}"` |
| B14 | Linux waits for `omarchy-system-lock` on the input thread, so nothing is read while the screen is locked and Windows' sends can block. | `daemon.rs` `lock_local_machine` | Grok | Reviewer quoted the code |
| B15 | Windows-style keys do an Accessibility lookup (`focused_app`) on the input thread for many keys, up to 100ms each, and mouse packets queue behind it. | `macos/src/windows_keys.rs`, `cg_input.rs` | Grok | Known trade-off, worth caching |
| B16 | The 300s socket timeout equals MWB's 5-minute heartbeat, so an idle healthy connection can time out, and a dead peer takes 5 minutes to notice. | `macos/src/main.rs` `SOCKET_TIMEOUT` | Fable | Plausible |

### Low

- Every `DragEnter` on the catcher sends a new `OFFER_TO_MAC`. A re-entry can start a second, unwanted Mac drag. Offer once per catch.
- The 1.5s stand-down timer from an earlier `OnActive` can hide a newer catch session.
- The companion's accept loop has no try/catch, so one accept error ends the server. There's no timeout before `HELLO`.
- Companion temp folders (drops, copies) are only pruned at startup, with no size cap.
- The companion link has no MAC and no freshness: a recorded stream replays (including the injected press), and file bytes can be altered undetected.
- When a pulled body's size is a multiple of 32, Windows' 32 padding bytes are left unread and the socket closes with a reset.
- No `panic = "abort"`: a panic on the network thread leaves a live tray icon over a dead bridge that launchd won't restart.
- The Mac clipboard watcher prefers an image over text; real MWB prefers text, so spreadsheet cells may arrive as a picture. Suspicion.
- "Restart App" spawns a copy and exits; under launchd's KeepAlive a plain exit would do. Suspicion.

## 2. Optimizations

Input path (biggest wins first):
- Keep the input thread to read, decrypt and inject. Move waits, sleeps, Accessibility lookups and network sends off it (B3, B4, B15).
- One mutex around encrypt-and-write on the MWB connection, so the clipboard watcher and companion threads can send directly. This fixes B6 and takes big sends off the input thread. Encrypt and write must stay under the same lock to keep the CBC chain in order.
- Coalesce queued mouse moves after a stall: when `FIONREAD` shows more packets waiting, inject only the latest position. Never coalesce clicks or wheel steps.
- Cache the focused app for a few hundred milliseconds, and only look it up for keys whose rewrite depends on it.
- Linux: drop the per-packet logging, replace the `roundtrip()` after every event with a flush, set `TCP_NODELAY`.

Clipboard and transfers:
- Poll `NSPasteboard.changeCount` and only decode the image when it changes. Today an image on the clipboard is fetched as TIFF, converted to RGBA and compared every 500ms.
- Linux: use `wl-paste --watch` (or the data-control protocol) instead of spawning up to three `wl-paste` processes every 500ms.
- Stream big-path bodies in 1MB chunks, both ways and on both platforms. Today a 100MB file costs about 200MB of memory and can't start sending until it's fully read.
- Batch small-path clipboard sends into one write. A 900KB image is about 19,000 separate 64-byte writes with Nagle off.
- Companion data connections: create placeholder files before connecting, and cache the PBKDF2 derivation per salt (each connection runs it twice per side).
- Transfer policy: with the companion, every file copy is sent immediately, at any size. Consider fetching on switch with a size cap.

Connection:
- Detect a dead peer in seconds with TCP keepalive or an MWB `Hello` ping (Windows answers with a Heartbeat), instead of the 300s timeout.
- Reconnect faster: no fixed 3s sleep after a wake or wired cut, back off from 250ms, try candidate addresses in parallel, cache `get_24bit_hash`.

## 3. New features

| Feature | Value | Effort |
|---------|-------|--------|
| Full-size big images Mac to Windows (big path plus `ClipboardAsk`, or the companion) instead of downscaling | Screenshots arrive at full resolution | Small to medium |
| Folders through the companion (relative paths in `FILE_HEADER`), or auto-zip | Removes the "zip it first" limit | Medium |
| Transfer progress and cancel in both trays, wiring up the unused `CANCEL` frame | You can see big transfers and stop them | Small to medium |
| Paste-time transfer (virtual files on Windows, file promises on the Mac) | No eager copies; drops finish at once, which fixes B3 and B7 properly | Large |
| Self-registration: send MWB `Heartbeat`s so Windows adds the bridge to `MachinePool` itself (`MachineStuff.AddToMachinePool`) | Possibly no more hand-editing `settings.json` | Small, needs a careful live test |
| Companion setup check: warn when the bridge is missing from the pool or matrix (it already warns about "Transfer file") | Faster setup and diagnosis | Small |
| Media and missing keys: volume, play/pause, Print Screen, the menu key, F13-F24 | Fewer dead keys on the Mac and Linux | Small |
| Keep-awake: act on MWB's `Awake` packages | The bridged display stays on while you work on Windows | Small |
| Mac settings and health panel: edit config in the app, show link type and latency, "Reconnect now" | No hand-editing JSON | Small to medium |
| Linux parity: `ClipboardAsk`, several PCs, HTML-only copies, diagnostics, companion client | Linux catches up with the Mac | Medium |

## 4. Implementation plan

Each phase is a few commits; every commit updates README and AGENTS.md as the repo rules require, and marks what was live-tested.

### Phase 0: safety and privacy (small, do first)

**Done** (Grok-reviewed; type-checked and unit-tested, not live-tested).
The review changed the plan: real MWB's `ReleaseAllKeys` releases keys
only, so `release_keys` leaves mouse buttons alone (releasing one drops a
drag at the edge). On the Mac a file drag nothing on Windows takes is
cancelled instead, a pending Windows-key tap is forgotten first (else
Spotlight opens), and on disconnect Escape goes before any button release.

- Remove Linux key and mouse logging (B2).
- Add a shared `release_all` to the input sinks and call it on `HideMouse` (after the drag handling) and at session end, Mac and Linux (B1).
- Inject nothing until the handshake challenge has matched (B11).
- `panic = "abort"` in the release profile, so launchd restarts a crashed bridge.
- Tests: unit tests for release-on-HideMouse and for input gating.
- Live: hotkey-switch away with Ctrl+Alt held, come back by mouse; no stuck modifiers on either machine.
- Risk: releasing buttons during a drag-out. Keep the drag cancel first.

### Phase 1: the input thread never blocks (B3, B4, B6, B14, B16)

**Done** (Grok-reviewed; type-checked and unit-tested, not live-tested).
Done differently from the plan below: instead of a shared locked writer,
the input loop waits for data with `poll` for at most 50ms, so its top
(clipboard events, companion offers, the held drop release) runs at least
that often and the MWB connection keeps a single writer. TCP keepalive
(15s, 5s, 3 probes) on the MWB and companion connections detects a dead
peer. An early release on the Mac is held back for up to 3 seconds, then
the macOS drag is cancelled and the files go to the drop folder. The
review found the hold leaking: a click, key, new drag or HideMouse during
it ended the macOS drag behind its back (and told the companion the
button was still held), and a lost connection lost the files. Each of
those now settles the hold first, and ending the session saves the files.
It also found that `poll` never trips the read timeout, so the reverse
connection gets keepalive too and the wait gives up after 5 quiet
minutes. Kept as it was: the drag start still sleeps (40ms, plus up to 300ms waiting for
the session to start), which needs the main thread's answer and is left
for Phase 4.

- Shared locked writer for the MWB connection. The clipboard watcher and the companion thread send directly.
- `dropped()` records "release when the fetch completes" and returns. A worker waits for the fetch and then signals the input thread, which posts the release, so injection stays on one thread. Drag start becomes a small state machine instead of sleeps.
- Companion socket: read and write timeouts, keepalive, cut on wake with the MWB socket. A failed send marks it disconnected and falls back to the PowerToys path.
- Linux: spawn the locker without waiting.
- Keepalive or `Hello` ping on the MWB connection, then a longer read timeout.
- Tests: a socketpair test with concurrent writers; `dropped()` returns immediately with a blocked fetch.
- Live: drag a large file to the Mac and release early while moving the mouse; copy on the Mac and flick to Windows at once; sleep and wake the Mac with the companion connected, then copy a file.

### Phase 2: companion correctness (B7-B10, low-severity companion items)

**Done** (Grok-reviewed; type-checked and unit-tested, not live-tested).
`INACTIVE` now carries `drag_over` (`drag.is_dropping()`) and `HELLO` a
protocol version (2), so the bridge and the companion must be updated
together. Also done: keepalive on the companion's control connection, a
30s stall limit on data connections, a lock so a drop can't be saved to
the desktop twice, and a disconnect mid-catch keeping the catcher up
(no injected release, the Mac may still have control) until the parked
cursor moves, then ending the leftover drag. `INACTIVE` also carries
`let_go` (the release seen on the Mac, kept across reconnects), so a
quick bounce that beats the offer is handed back rather than ended, the
Mac ignores offers once control has left, and an offer arriving after a
release on the Mac is saved to its drop folder instead of starting a drag.
A failed offer send is retried when the drag re-enters the catcher. Known
gap, left after a watch for it misfired: a drag let go on the Mac within
~100ms of crossing, before OLE entered the catcher, carries on on Windows.
The review found: ending a leftover drag kept the catch open for 350ms
(a quick return to the Mac wasn't caught), so the release is now
immediate and the catch ends at once; pruning on the offer's path could
outlast the data connection's 2s wait, so it runs in the background at
most hourly; and a version mismatch broke drag-back, so `INACTIVE` still
carries `button_held` and the companion reads whichever the bridge's
version means. Open: whether OLE re-polls `QueryContinueDrag` on the 50ms
`WM_TIMER` (documented as on input changes). If the live test shows an
early release still waiting for a mouse move, nudge it with a zero-length
injected move. Not done: a size cap on the temp folders (pruned by age),
and the companion link's missing MAC and freshness.

- Unique names in `IncomingDrop.Create`, with a C# test.
- Early release on Windows: keep a short UI timer during the drag so `QueryContinueDrag` is re-polled. If the files aren't complete within about a second of letting go, cancel the drag and save to `Desktop\MouseWithoutBorders\` rather than dropping at a moving cursor.
- `RunControl`'s `finally` only resets state when it's still the current connection.
- Send "a drop is over this Mac" (`drag.is_dropping()`) rather than the synthetic button state. The companion ends the leftover drag itself in every case except a real drag-back.
- One offer per catch session, a cancellable stand-down timer, a try/catch around accept, a `HELLO` timeout, periodic pruning, and a protocol version field in `HELLO`.
- Live: round-trip drag, quick bounce, Mac sleep right after a drop, two same-named files, a large file released early.

### Phase 3: PowerToys-path correctness (B12, B13)
- Send a path-like name in the file header (Windows keeps `Path.GetFileName`).
- Recognise the `text` header and apply it as clipboard text.
- Read Windows' padding tail before closing.
- Check the image-over-text clipboard order against real MWB.
- Tests: extend the `fake_windows` test. Live: drag `image001.png` with the companion off; copy more than 1MB of text on Windows.

**Done 2026-11-24.** Live-tested on the Mac the same day against CHP-ALANJ with the companion off: `image02.jpg` dragged to Windows arrived (sent as `/image02.jpg`); 2MB of random text copied on Windows was pulled as big-path `text` and pasted as text; spreadsheet cells went as text; Chrome and Safari "Copy Image" arrived as an image in Paint and Word. Linux type-checked only.
- B12: file names go to Windows as `/` plus the base name (`file_header_name`), on both bridges.
- B13: a pulled `text` header is applied as clipboard text on both bridges.
- Padding: one shared reader, `read_pulled_body`, replaces both bridges' own. It reads up to the next multiple of 32, accepts an early close once the body is in (Windows' `SendData` pads by `size % 32` and never sends a final partial AES block), and reads off the tail before closing. Linux now pads what it sends with `push_padding_len`, like the Mac.
- Order: checked against `FormHelper.cs`, which prefers text. Both bridges now send text over an image, except a lone URL next to an image (browsers' image copies). Each kind's "last seen" is now forgotten while another kind is on the clipboard, so re-copying the same text after an image sends it again (a bug in the old order too).
- Tests: the `fake_windows` push test now sends `image001.txt` and checks the `/` names; new protocol tests cover both padding styles, a short body, draining the tail, the name rule and the text-or-image rule.

### Phase 4: performance
- `changeCount` polling, streaming bodies, batched small-path sends, mouse-move coalescing, the focused-app cache, faster reconnect, cached key derivation, placeholders before connecting.
- Linux: flush instead of roundtrip, `TCP_NODELAY`, `wl-paste --watch`.
- Live: a 100MB file both ways and a large screenshot left on the clipboard, watching CPU and memory.

**Done 2026-11-24** (type-checked and unit-tested; Linux type-checked only; nothing live-tested):
- Drag start: the 40ms sleep, the 300ms wait and the 1s main-thread wait are gone from the input thread. `drag_source` queues its main-thread work without waiting and reports `placed`/`started` per placement number; `IncomingDrag` steps through `Start` (Placing, Placed, Pressed) from `on_move` and `tick`, holding back mouse moves (not clicks) meanwhile. A release, a cancel or the session ending mid-start takes the panel away. `cancel_drag`'s 50ms pause after Escape is kept: it's short and only runs when a drag is cancelled.
- Mouse moves: `net::next_is_absolute_move` peeks (without consuming) at the next queued package, decrypting with a copy of the chain; an absolute move followed by another is skipped, unless it's near a screen edge. Relative moves are never skipped. Both bridges.
- Focused app: cached for 500ms, forgotten on any button and when the app switcher closes.
- Clipboard: the Mac watcher reads nothing until `NSPasteboard.changeCount` moves. Linux waits on `wl-paste --watch echo`, falling back to polling and retrying every 30s.
- Transfers: `write_padded_body` streams from a reader in 1MB chunks (shared by both bridges); pulled files stream to disk; small-path clipboard sends are one write (`encrypt_packages`).
- Connection: reconnect backoff 250ms doubling to 4s, reset by a connection that lasted 5s (and, on the Mac, got through the handshake); `get_24bit_hash` cached; one salt per security key per process with a fresh IV per connection, and peer keys cached by salt, on both bridges and the companion (which is why the companion is reinstalled).
- Companion: placeholders before connecting were already there on Windows; the Mac now creates them before opening the data connection.
- Linux: input is flushed per event with a roundtrip at most every 500ms; `TCP_NODELAY` on the client connection.
- Review round 1 fixes: a press is decided once per placement (`drag_source::abandon` vs the view's `mouseDown`, under one lock), so a release, cancel or disconnect mid-start never presses, and a press already posted can't start a drag or click through (the panel goes on its `mouseUp`, or 500ms later); a session ending mid-press lets go plainly (`release_abandoned_press`) instead of `CgInput::reset`'s Escape; a pasteboard change only counts as handled once a read found something; `wl-paste --watch` runs `sh -c 'cat >/dev/null; echo'` so the copied data is drained; failed fetches delete their files.
- Review round 2 fixes: the view decides and begins the drag session under the same lock, so `started` (and `abandon`'s "already started") means a session exists, and moves stay held until then; Linux tolerates `WouldBlock` from a Wayland flush (the rest goes with the next one); Linux times the backoff's "worked" from when the connect succeeded.
- Review round 3 fixes: a drag counts as started only at `draggingSession:willBeginAtPoint:` (a session asked for is `Arming` until then, `abandon` waits up to 100ms for it, and one that ends without beginning counts as abandoned), so moves stay held and the panel stays hit-testable until AppKit really runs the drag; Linux also tolerates `WouldBlock` from the periodic roundtrip.
- Review round 4 fixes: one rule for the press: it's a drag only once `willBegin` has run; otherwise (still `Pending`, ended at once, or a main thread stuck past `abandon`'s 100ms wait) it's let go plainly, never with Escape. A panel whose session was asked for stays until that session ends, so a session that begins after the press was let go ends over the panel, not an app. Linux only roundtrips after a flush that got everything out, at most every 500ms.
- Review round 5 fixes: a drag the network thread saw start but whose session has since ended by itself (`dragging` with `is_active` false) is let go plainly on the next move, or by `release_abandoned_press` on a disconnect, and its files are saved on release; coalescing never skips an absolute move near a screen edge (`MouseState::near_edge`), since relative mode learns which side Windows is on from those.
- Review round 6 fixes: a release or cancel after the drag ended by itself also lets go plainly and saves (or discards) the files; `ACTIVE` holds the running drag's placement number and only that drag's `ended` clears it, so a late `ended` from an earlier drag can't end a newer one; a skipped move still counts as input for edge learning (`MouseState::note_skipped`).
- Review round 7 fixes: Linux coalesces moves the same way as the Mac (it had been left out); the Linux roundtrip's 500ms quiet period starts when the answer comes back; the Mac backoff's "worked" is timed from when the connect succeeded, like Linux.
- Not done: trying candidate addresses in parallel (the wired-first ordering in `route.rs` would need rethinking), and a single mutex around encrypt-and-write for sends from other threads (Phase 1's 50ms tick already keeps queued sends prompt).

### Phase 5: features
**Done** (each feature Grok-reviewed and its findings fixed; the
whole-phase review on 2026-11-24 found six more, fixed in 6597624:
stale Mac copies on the Windows clipboard, the PowerToys fallback for
failed copies, the owed release, two drags from the Mac at once, and
offer ids made in the same millisecond). Live-tested on the Mac and both Windows PCs
2026-11-24: big transfers both ways with progress and cancel, folders, the
shorter drop hold (the 3s one froze the cursor), media keys, stay-awake,
keep-screen-on, the settings window, the setup check, and
self-registration with the Mac already in the pool. Not yet live-tested:
self-registration of a machine Windows doesn't know yet, full-size
images, and everything on Linux.
In order: full-size images, folders, progress and cancel, the companion setup check, media keys, keep-awake, self-registration (careful: it rewrites the matrix on first registration), the Mac settings panel. Paste-time transfer and an authenticated companion cipher come after.

### Phase 6: Linux parity
First a live v0.102 connection test (Linux has never connected to a v0.102 PowerToys). Then `ClipboardAsk`, `other_windows_pcs`, HTML-only copies, diagnostics, and a companion client for multi-file copy. Wayland drag-and-drop stays a separate project.
