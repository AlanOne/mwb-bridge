// macOS port of the mwb-omarchy-bridge daemon. Everything protocol-level
// (crypto, framing, handshake, VK->evdev translation, mouse/keyboard
// decoding) is unchanged, shared code from `mwb_protocol` — this file is
// mostly platform wiring: the TCP connect/reconnect loop and a status-item
// menu bar app. Mouse/keyboard forwarding, clipboard sync (text, and images
// both small- and big-path), and lock-both-machines are ported; plain file
// transfer and suspend/resume handling are deliberately not yet — see the
// mwb-omarchy-bridge project memory's "macOS port" section for the full
// remaining list, being added incrementally the same way the Linux build was.

mod cg_input;
mod clipboard;
mod companion;
mod drag_in;
mod drag_out;
mod drag_source;
mod keycode_macos;
mod power;
mod route;
mod windows_keys;

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use mwb_protocol::config::{self, Config};
use mwb_protocol::drag_drop::{DragAction, DragDropState};
use mwb_protocol::input_handling::{handle_keyboard, handle_mouse, release_keys, KeyboardState, LockComboDetector, MouseState, ScreenEdge, LLKHF_UP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE};
use mwb_protocol::mwb_protocol::*;
use rand::RngExt;

use cg_input::CgInput;
use clipboard::{ClipboardEvent, ClipboardShared, Outbound};
use drag_out::DragOut;

// Same 5-minute idle-tolerant timeout the Linux build settled on after
// finding both failure modes live (a no-timeout 3+ hour hang, then an
// over-aggressive 30s timeout that false-triggered during normal idle
// gaps) — see the mwb-omarchy-bridge project memory.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(300);

// The input loop's longest sleep while Windows sends nothing.
const INPUT_LOOP_TICK: Duration = Duration::from_millis(50);

// Same message-server port MWB uses on every machine. Windows expects a
// symmetric pair of sockets (one per direction) between two paired
// machines — live-tested against the real Windows PC on 2026-09-22:
// without accepting this reverse leg, the handshake completed and matched,
// but Windows then sent *nothing* further at all afterward (not even
// periodic Hi liveness pings), for a brand-new machine being paired for
// the first time. The Linux build calls this leg "cosmetic only," but that
// finding was on an already-established pairing — a first-time pairing
// appears to need it. See the mwb-omarchy-bridge project memory.
const LISTEN_PORT: u16 = 15101;

/// The outbound connection to Windows, kept so a wake from sleep or a newly
/// plugged-in cable can cut it (see `watch_for_wake`, `watch_for_wired_path`).
static CURRENT_STREAM: Mutex<Option<TcpStream>> = Mutex::new(None);

/// Whether the left button was let go of over this Mac since control last
/// came here (MachineSwitched): the companion's INACTIVE `let_go`, which
/// says whether a drag it caught ended here or is coming back. The press
/// of a drag from Windows happened there, so the release is the only part
/// this Mac sees. Kept across reconnects (Windows may still think this Mac
/// has control), and a lost connection counts as letting go, since it
/// cancels the drag here.
static LET_GO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The Windows end of the live connection, if there is one.
pub fn current_peer() -> Option<std::net::SocketAddr> {
    CURRENT_STREAM.lock().unwrap().as_ref().and_then(|s| s.peer_addr().ok())
}

/// Local wall-clock time as HH:MM:SS.mmm, for lining log lines up with when
/// a hitch was felt.
fn timestamp() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!("{:02}:{:02}:{:02}.{:03}", tm.tm_hour, tm.tm_min, tm.tm_sec, now.subsec_millis())
}

/// Bytes already waiting in the socket's receive buffer.
fn bytes_queued(stream: &TcpStream) -> usize {
    use std::os::fd::AsRawFd;
    let mut n: libc::c_int = 0;
    if unsafe { libc::ioctl(stream.as_raw_fd(), libc::FIONREAD, &mut n) } == 0 { n.max(0) as usize } else { 0 }
}

// Input hitch diagnostics (see LagMonitor).
const LAG_GAP: Duration = Duration::from_millis(40);
const LAG_BURST_PACKETS: usize = 4;
const LAG_SLOW_INJECT: Duration = Duration::from_millis(8);

/// Logs input hitches and which side caused them: a gap in input followed
/// by several packets already queued at once means the network held them
/// back (Windows sent them during the gap); an event that took long to
/// inject means this Mac was slow.
struct LagMonitor {
    last_input: Option<std::time::Instant>,
}

impl LagMonitor {
    fn new() -> Self {
        Self { last_input: None }
    }

    /// Call when an input packet has just been read.
    fn arrived(&mut self, stream: &TcpStream) {
        let now = std::time::Instant::now();
        if let Some(prev) = self.last_input.replace(now) {
            let gap = now.duration_since(prev);
            if gap >= LAG_GAP && gap < Duration::from_secs(2) {
                let queued = bytes_queued(stream) / PACKAGE_SIZE;
                if queued >= LAG_BURST_PACKETS {
                    println!(
                        "[lag] {} network stall: no input for {} ms, then {queued} more packets arrived at once",
                        timestamp(),
                        gap.as_millis()
                    );
                }
            }
        }
    }

    /// Call after injecting an input packet that started at `started`.
    fn injected(&self, kind: &str, started: std::time::Instant) {
        let took = started.elapsed();
        if took >= LAG_SLOW_INJECT {
            println!("[lag] {} slow on this Mac: injecting {kind} took {} ms", timestamp(), took.as_millis());
        }
    }
}

fn set_socket_timeouts(stream: &TcpStream) {
    let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
}

/// Handshake + Mouse/Keyboard/ClipboardText receive loop over an
/// already-connected stream. `cg` is `Some` on the outbound client
/// connection (real forwarding runs there) and `None` on the inbound
/// reverse-listener side, which only needs to complete the protocol so
/// Windows sees a healthy pair — mirrors the Linux build's `role`/
/// `wl: Option<&mut WaylandInput>` split. `clipboard` is only `Some` on the
/// client role too, same reasoning as the Linux build: outbound clipboard
/// sync happens over the one connection real forwarding already runs over.
/// Deliberately narrower than the Linux build otherwise: no image
/// clipboard/lock/file handling yet. Returns (via `?`) on any I/O error so
/// the caller reconnects/re-accepts. `handshook` is set once the peer
/// answers our handshake challenge correctly, i.e. it's a PC we can work
/// with (reachable, same key, knows this machine).
fn run_session(
    stream: &mut TcpStream,
    mut cg: Option<&mut CgInput>,
    cfg: &Config,
    clipboard: Option<(&Receiver<ClipboardEvent>, &ClipboardShared)>,
    handshook: &mut bool,
) -> std::io::Result<()> {
    // A peer that vanished (sleep, cable, Windows crash) is noticed within
    // about half a minute, on the reverse leg as well as ours.
    if let Err(e) = mwb_protocol::net::keep_session_alive(stream) {
        eprintln!("[client] Couldn't turn on TCP keepalive: {e}");
    }
    let magic_number = get_24bit_hash(&cfg.security_key);
    let EncryptedStream { cipher, mut write_chain, mut read_chain } = open_encrypted_stream(stream, &cfg.security_key)?;

    let our_machine1_4: [u32; 4] = {
        let mut r = rand::rng();
        [r.random(), r.random(), r.random(), r.random()]
    };
    let mut handshake = build_handshake(1, cfg.machine_id, our_machine1_4, &cfg.machine_name);
    finalize_send_buf(&mut handshake, magic_number);
    for _ in 0..10 {
        let ct = cipher.encrypt(&mut write_chain, &handshake);
        stream.write_all(&ct)?;
    }
    let our_flipped: [u32; 4] = our_machine1_4.map(|v| !v);
    println!("[client] Handshake sent, entering receive loop.");

    let mut kb_state = KeyboardState::new();
    let mut mouse_state = MouseState::new(cfg.windows_side.as_deref().and_then(ScreenEdge::parse), cfg.neighbor_edges());
    let mut lock_combo = LockComboDetector::new();
    let mut hi_count = 0u64;
    let mut clipboard_buf: Vec<u8> = Vec::new();
    let mut clipboard_kind: Option<u8> = None;
    // Must stay unique across reconnects, not just within this connection
    // (see initial_package_id and build_clipboard_text_packages).
    let mut next_clip_id: u32 = initial_package_id();
    let mut lag = LagMonitor::new();
    let mut invalid_count = 0u32;
    let mut drag = DragDropState::new(cfg.machine_id);
    let mut drag_out = DragOut::new();
    let mut incoming = drag_in::IncomingDrag::default();
    // Package types this loop ignores, logged once each so new traffic
    // from Windows (like a ClipboardAsk this bridge didn't handle for a
    // long time) shows up in the log instead of vanishing.
    let mut unhandled_seen = std::collections::HashSet::new();
    let mut ignored_before_handshake = false;
    let mut last_data = std::time::Instant::now();
    // Whether this Mac has control (between MachineSwitched to it and
    // HideMouse): a companion drag offer arriving after control left is
    // ignored.
    let mut active_here = false;

    loop {
        // A release held back for files still arriving (see IncomingDrag).
        if let Some(cg) = cg.as_deref_mut() {
            incoming.tick(cg);
        }
        // Files dragged on Windows that the companion says are now over
        // this Mac, and files copied there.
        if let Some((_, shared)) = clipboard.as_ref()
            && let Some(link) = companion::link()
        {
            for offer in link.take_clipboard_offers() {
                clipboard::receive_companion_copy(link, offer, shared);
            }
            for offer in link.take_offers() {
                if LET_GO.load(std::sync::atomic::Ordering::Relaxed) {
                    // Let go here before the offer came (or the connection
                    // dropped meanwhile), whether or not control has left
                    // since: the companion ends Explorer's drag, so there's
                    // no drag to start, and the files go to the drop
                    // folder, as for a release before a drag could start.
                    println!("{} [drop] Let go of the companion's drag {} before it arrived, saving its files instead.", timestamp(), offer.id);
                    if let Some(cg) = cg.as_deref_mut() {
                        incoming.settle(cg);
                        incoming.start_companion(link, offer);
                        incoming.dropped(cg);
                    }
                } else if !active_here {
                    // Came back before the offer did: the companion hands
                    // the drag back on Windows.
                    println!("{} [drop] Ignoring the companion's drag {}: control already left this Mac.", timestamp(), offer.id);
                } else if drag.start_drop() == DragAction::DropStarted {
                    println!("{} [drop] {} file(s) dragged on Windows are over this Mac.", timestamp(), offer.files.len());
                    if let Some(cg) = cg.as_deref_mut() {
                        incoming.settle(cg);
                    }
                    incoming.start_companion(link, offer);
                } else {
                    println!("{} [drop] Ignoring the companion's drag {}: another drop is already going on.", timestamp(), offer.id);
                }
            }
        }
        if let Some((clip_rx, shared)) = clipboard.as_ref() {
            let mut latest = None;
            while let Ok(ev) = clip_rx.try_recv() {
                latest = Some(ev); // coalesce rapid successive changes to the last one
            }
            if latest.is_some() {
                // Whatever an older copy still has waiting is stale now.
                clipboard::copied();
            }
            match latest {
                Some(ClipboardEvent::Text(text)) => {
                    let packages = build_clipboard_text_packages(&mut next_clip_id, cfg.machine_id, &text);
                    stream.write_all(&encrypt_packages(&cipher, &mut write_chain, magic_number, packages))?;
                    println!("[client] Sent clipboard text ({} chars) to peer.", text.chars().count());
                }
                Some(ClipboardEvent::Image(png_bytes)) => {
                    let len = png_bytes.len();
                    let packages = build_clipboard_image_packages(&mut next_clip_id, cfg.machine_id, &png_bytes);
                    stream.write_all(&encrypt_packages(&cipher, &mut write_chain, magic_number, packages))?;
                    println!("[client] Sent clipboard image ({len} bytes) to peer.");
                }
                Some(ClipboardEvent::Files(paths)) if companion::link().is_some_and(|l| l.is_connected()) && {
                    // The companion takes every file, of any size, and
                    // folders. If the offer can't be sent (the link just
                    // died), the copy goes the PowerToys way below instead.
                    companion::link().unwrap().offer_files(mwb_protocol::companion::CLIPBOARD_TO_WINDOWS, paths.clone(), "copy")
                } => {}
                Some(ClipboardEvent::Files(paths)) => {
                    let (files, skipped) = drag_out::split_sendable(paths);
                    for (path, reason) in &skipped {
                        println!("{} [file-copy] Not sending {} to Windows: {reason}.", timestamp(), path.display());
                    }
                    // Windows' clipboard takes one received file per
                    // transfer, so a multi-file copy sends its first.
                    if let Some(first) = files.first() {
                        if files.len() > 1 {
                            println!("{} [file-copy] {} files copied, only the first can be pasted on Windows.", timestamp(), files.len());
                        }
                        clipboard::set_pending_copy(shared, Outbound::Files(vec![first.clone()]));
                        clipboard::announce_big(stream, &cipher, &mut write_chain, magic_number, cfg.machine_id, clipboard::peer_machine_id(shared))?;
                        println!("{} [file-copy] Copied {}, announced to Windows.", timestamp(), first.display());
                    }
                }
                Some(ClipboardEvent::BigImage { preview, path }) => {
                    if let Some(png) = preview {
                        let len = png.len();
                        let packages = build_clipboard_image_packages(&mut next_clip_id, cfg.machine_id, &png);
                        stream.write_all(&encrypt_packages(&cipher, &mut write_chain, magic_number, packages))?;
                        println!("[client] Sent a downscaled clipboard image ({len} bytes) to peer, the full-size one follows.");
                    }
                    clipboard::set_pending_copy(shared, Outbound::Image(path));
                    clipboard::announce_big(
                        stream,
                        &cipher,
                        &mut write_chain,
                        magic_number,
                        cfg.machine_id,
                        clipboard::peer_machine_id(shared),
                    )?;
                    println!("[client] Announced a big clipboard image to peer (served if/when it connects to pull it).");
                }
                None => {}
            }
        }

        // Wake up at least this often even when Windows sends nothing (an
        // inactive machine gets a heartbeat only every few minutes), so a
        // copy made here, a companion offer or a held-back drop isn't left
        // waiting for the next package. A dead peer is caught by TCP
        // keepalive (set above), which makes the socket readable with an
        // error, or else by nothing arriving for SOCKET_TIMEOUT.
        if !mwb_protocol::net::wait_for_package(stream, INPUT_LOOP_TICK, &mut last_data, SOCKET_TIMEOUT)? {
            continue;
        }
        let mut ct = [0u8; PACKAGE_SIZE];
        stream.read_exact(&mut ct)?;
        let mut pt = cipher.decrypt(&mut read_chain, &ct);
        let package_type = pt[0];
        let valid = validate_and_clean_recv_buf(&mut pt, magic_number);

        let mut full = pt.clone();
        if is_big_package(package_type) {
            let mut ct2 = [0u8; PACKAGE_SIZE];
            stream.read_exact(&mut ct2)?;
            let pt2 = cipher.decrypt(&mut read_chain, &ct2);
            full.extend_from_slice(&pt2);
        }
        if !valid {
            // A peer using a different security key looks exactly like this:
            // the connection opens, but nothing it sends decrypts.
            invalid_count += 1;
            // The first few, decrypted, so a protocol change on the Windows
            // side can be told apart from a wrong key (garbage either way,
            // but a wrong key leaves no structure at all).
            if invalid_count <= 3 {
                let hex: String = full.iter().map(|b| format!("{b:02x}")).collect();
                println!("{} [client] Undecryptable package #{invalid_count} (raw type {package_type}): {hex}", timestamp());
            }
            if invalid_count == 5 {
                println!("{} [client] Peer's packets don't decrypt with our security key — has it changed in PowerToys?", timestamp());
                config::write_status(false, "", "Peer's packets don't decrypt — check the security key");
            }
            continue;
        }
        // Nothing but the handshake counts until the peer has answered our
        // challenge: a replayed or redirected stream must not inject input.
        if !*handshook && !matches!(package_type, PACKAGE_TYPE_HANDSHAKE | PACKAGE_TYPE_HANDSHAKE_ACK) {
            if !ignored_before_handshake {
                println!("{} [client] Ignoring packages until the peer answers the handshake challenge.", timestamp());
                ignored_before_handshake = true;
            }
            continue;
        }
        let src_id = unpack_u32_le(&full, 8);
        if let Some((_, shared)) = clipboard.as_ref() {
            shared.note_peer_machine_id(src_id);
        }

        match package_type {
            PACKAGE_TYPE_HANDSHAKE => {
                println!("[client] Replying to peer's Handshake with HandshakeAck.");
                let mut ack = build_handshake_ack(&full, cfg.machine_id, &cfg.machine_name);
                finalize_send_buf(&mut ack, magic_number);
                let ct = cipher.encrypt(&mut write_chain, &ack);
                stream.write_all(&ct)?;
            }
            PACKAGE_TYPE_HANDSHAKE_ACK => {
                let m1 = unpack_u32_le(&full, 16);
                let m2 = unpack_u32_le(&full, 20);
                let m3 = unpack_u32_le(&full, 24);
                let m4 = unpack_u32_le(&full, 28);
                let matched =
                    m1 == our_flipped[0] && m2 == our_flipped[1] && m3 == our_flipped[2] && m4 == our_flipped[3];
                println!("[client] HandshakeAck received, challenge match: {matched}");
                // Announce this machine once per connection, if asked to
                // (see Config::self_register).
                if matched && !*handshook && cfg.self_register {
                    let mut hb = build_heartbeat(next_clip_id, cfg.machine_id, &cfg.machine_name);
                    next_clip_id = next_clip_id.wrapping_add(1);
                    finalize_send_buf(&mut hb, magic_number);
                    stream.write_all(&cipher.encrypt(&mut write_chain, &hb))?;
                    println!("[client] Announced this machine to Windows (self_register is on).");
                }
                *handshook |= matched;
                let peer = String::from_utf8_lossy(&full[32..64]).trim_end().to_string();
                let detail = if matched { "Connected" } else { "Handshake failed — check the security key" };
                config::write_status(matched, &peer, detail);
            }
            PACKAGE_TYPE_MOUSE => {
                let x = unpack_u32_le(&full, 16);
                let y = unpack_u32_le(&full, 20);
                let wheel = unpack_u32_le(&full, 24);
                let flags = unpack_u32_le(&full, 28);
                if let Some(cg) = cg.as_deref_mut() {
                    lag.arrived(stream);
                    // After a stall, absolute moves queue up: injecting each
                    // of them only replays the stall, so skip to the latest.
                    // Not one near a screen edge, though: that's where the
                    // cursor enters, which is how relative mode learns which
                    // side Windows is on.
                    if flags == WM_MOUSEMOVE
                        && mwb_protocol::net::is_absolute_move(&full)
                        && !MouseState::near_edge(x, y)
                        && mwb_protocol::net::next_is_absolute_move(stream, &cipher, &read_chain, magic_number)
                    {
                        mouse_state.note_skipped();
                        continue;
                    }
                    // Anything but a move while a release is held back
                    // (a click, the wheel) would end the macOS drag behind
                    // its back, so the held drop is settled first.
                    if flags != WM_MOUSEMOVE {
                        incoming.settle(cg);
                    }
                    if flags == WM_LBUTTONDOWN {
                        drag_out.mouse_down();
                    }
                    if flags == WM_LBUTTONUP {
                        LET_GO.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    // A drag from Windows is settled before this packet is
                    // posted: the release posted below is what drops it.
                    let mut post = true;
                    match drag.on_mouse(flags) {
                        DragAction::Dropped => {
                            println!("{} [drop] A file dragged on Windows was let go over this Mac.", timestamp());
                            // Held back while the files are still arriving.
                            post = !incoming.dropped(cg);
                        }
                        DragAction::DropCancelled => incoming.cancelled(cg),
                        _ if flags == WM_MOUSEMOVE => incoming.on_move(cg),
                        _ => {}
                    }
                    // While a release is held back, the drag mustn't move
                    // away from where it was let go (other input settles it,
                    // above). While a drag is being started, the cursor
                    // stays on its panel until the press is on it.
                    if incoming.holding_release() || (flags == WM_MOUSEMOVE && incoming.starting()) {
                        post = false;
                    }
                    let started = std::time::Instant::now();
                    let switch = if post { handle_mouse(cg, &mut mouse_state, x, y, wheel, flags, cfg.scroll_speed) } else { None };
                    lag.injected(if flags == 0x200 { "a mouse move" } else { "a click/scroll" }, started);
                    // Relative mode: the cursor was pushed off the edge facing
                    // Windows or a configured neighbour, so ask Windows to switch.
                    if let Some(back) = switch {
                        let windows_id = unpack_u32_le(&full, 8);
                        let mut pkg = build_next_machine(next_clip_id, cfg.machine_id, windows_id, back.x, back.y, back.to.unwrap_or(windows_id));
                        next_clip_id = next_clip_id.wrapping_add(1);
                        finalize_send_buf(&mut pkg, magic_number);
                        let ct = cipher.encrypt(&mut write_chain, &pkg);
                        stream.write_all(&ct)?;
                    }
                }
            }
            PACKAGE_TYPE_KEYBOARD => {
                // Offsets verified empirically on the Linux build against
                // real traffic: wVk at 24, dwFlags at 28.
                let vk = unpack_u32_le(&full, 24);
                let flags = unpack_u32_le(&full, 28);
                if lock_combo.observe(vk, (flags & LLKHF_UP) == 0) {
                    println!("[client] Windows' lock-both-machines combo detected — locking this machine too.");
                    cg_input::lock_screen();
                }
                if let Some(cg) = cg.as_deref_mut() {
                    lag.arrived(stream);
                    // A key during a held-back drop (Escape, say) would end
                    // the macOS drag behind its back: settle it first.
                    incoming.settle(cg);
                    let started = std::time::Instant::now();
                    handle_keyboard(cg, &mut kb_state, vk, flags);
                    lag.injected("a key", started);
                }
            }
            PACKAGE_TYPE_CLIPBOARD_TEXT | PACKAGE_TYPE_CLIPBOARD_IMAGE => {
                // Bytes 16-63 of a ClipboardText/ClipboardImage package are
                // one contiguous 48-byte raw-data chunk (not Machine1-4 +
                // MachineName, despite the same package size) — see
                // PROTOCOL.md's clipboard section.
                clipboard_buf.extend_from_slice(&full[16..64]);
                clipboard_kind = Some(package_type);
            }
            PACKAGE_TYPE_CLIPBOARD_DATA_END => {
                if let Some((_, shared)) = clipboard.as_ref() {
                    match clipboard_kind {
                        Some(PACKAGE_TYPE_CLIPBOARD_TEXT) => clipboard::apply_incoming_clipboard_text(&clipboard_buf, shared),
                        Some(PACKAGE_TYPE_CLIPBOARD_IMAGE) => clipboard::apply_incoming_clipboard_image(&clipboard_buf, shared),
                        _ => {}
                    }
                }
                clipboard_buf.clear();
                clipboard_kind = None;
            }
            PACKAGE_TYPE_CLIPBOARD => {
                // The "beat" announcing big-path content is available.
                // Real MWB only auto-pulls this around its own
                // machine-switch event; this bridge's fixed two-machine
                // topology has no equivalent, so treat any beat as "pull
                // immediately" instead — see PROTOCOL.md. Spawned in its
                // own thread since a full pull over a fresh connection
                // could take a while and shouldn't block this connection's
                // own receive loop.
                if let Some((_, shared)) = clipboard.as_ref() {
                    println!("[client] Received a big-path-available beat, pulling now.");
                    let cfg = cfg.clone();
                    let shared = (*shared).clone();
                    let peer_addr = stream.peer_addr().ok();
                    std::thread::spawn(move || {
                        if let Err(e) = clipboard::pull_from_windows(&cfg, peer_addr, &shared) {
                            eprintln!("(clipboard: big-path pull failed: {e})");
                        }
                    });
                }
            }
            PACKAGE_TYPE_HI => {
                hi_count += 1;
                if hi_count % 500 == 1 {
                    println!("[client] (Hi liveness pings received so far: {hi_count})");
                }
            }
            PACKAGE_TYPE_CLIPBOARD_DRAG_DROP
            | PACKAGE_TYPE_CLIPBOARD_DRAG_DROP_END
            | PACKAGE_TYPE_CLIPBOARD_DRAG_DROP_OPERATION
            | PACKAGE_TYPE_EXPLORER_DRAG_DROP
            | PACKAGE_TYPE_HIDE_MOUSE => {
                let des_id = unpack_u32_le(&full, 12);
                // With the companion connected, it handles drag-and-drop
                // and PowerToys' own should be off ("Transfer file").
                let companion = companion::link().filter(|l| l.is_connected() && clipboard.is_some());
                if package_type == PACKAGE_TYPE_HIDE_MOUSE {
                    if des_id == cfg.machine_id {
                        active_here = false;
                    }
                    // A drop held back here ends now (the user let go here),
                    // before the button state is read: it holds the left
                    // button down, which would look like a file drag of its
                    // own going to Windows.
                    if des_id == cfg.machine_id
                        && let Some(cg) = cg.as_deref_mut()
                    {
                        incoming.settle(cg);
                    }
                    if des_id == cfg.machine_id
                        && let (Some(link), Some(cg)) = (companion, cg.as_deref_mut())
                    {
                        // Both read before a drop here is cancelled below: a
                        // drag from Windows still over this Mac is going back.
                        let drag_over = drag.is_dropping();
                        let button_held = cg.left_button_held();
                        if button_held && !drag_over {
                            offer_to_companion(link, cg, &drag_out);
                        }
                        let let_go = LET_GO.load(std::sync::atomic::Ordering::Relaxed);
                        link.send(mwb_protocol::companion::INACTIVE, &mwb_protocol::companion::Inactive { drag_over, button_held, let_go });
                    }
                } else if companion.is_some() {
                    println!("{} [drag] Ignoring PowerToys' drag package {package_type}: the companion handles drag-and-drop (turn off \"Transfer file\" in PowerToys).", timestamp());
                    continue;
                } else {
                    println!("{} [drag] Received drag package type {package_type} (src {src_id}, des {des_id}).", timestamp());
                }
                let was_dropping = drag.is_dropping();
                let action = drag.on_package(package_type, src_id, des_id);
                if drag.is_dropping() != was_dropping {
                    let what = if was_dropping { "left without a drop" } else { "is over this Mac" };
                    println!("{} [drop] A file dragged on Windows {what}.", timestamp());
                }
                if let (Some(cg), Some((_, shared))) = (cg.as_deref_mut(), clipboard.as_ref()) {
                    match action {
                        DragAction::DropStarted => {
                            incoming.settle(cg);
                            incoming.start(cfg, stream.peer_addr().ok());
                        }
                        DragAction::DropCancelled => incoming.cancelled(cg),
                        DragAction::CheckOutgoingDrag { to } => {
                            offer_outgoing_drag(stream, &cipher, &mut write_chain, magic_number, cfg.machine_id, to, &mut next_clip_id, cg, &drag_out, shared)?;
                        }
                        _ => {}
                    }
                }
                // Control left this Mac: let go of the keys Windows left held,
                // after the drag handling above has had its say. The mouse
                // button stays held, as on real MWB (a release would drop a
                // drag at the edge), except that a file drag nobody took
                // (no companion, PowerToys' own drag-and-drop off) is
                // cancelled, not left hanging.
                if package_type == PACKAGE_TYPE_HIDE_MOUSE
                    && des_id == cfg.machine_id
                    && let Some(cg) = cg.as_deref_mut()
                {
                    if cg.left_button_held() && !drag_out.dragged_files().is_empty() {
                        println!("{} [drag-out] A file drag left for Windows with nothing to take it, cancelling it here.", timestamp());
                        cg.cancel_drag();
                    }
                    cg.forget_taps();
                    release_keys(cg, &mut kb_state);
                    cg.reset_keys();
                }
            }
            PACKAGE_TYPE_CLIPBOARD_ASK => {
                let des_id = unpack_u32_le(&full, 12);
                if des_id == cfg.machine_id
                    && let Some((_, shared)) = clipboard.as_ref()
                {
                    let post_action = unpack_u32_le(&full, 16);
                    println!("{} [big-send] Windows asked us to push big-path data (post action {post_action}).", timestamp());
                    let cfg = cfg.clone();
                    let shared = (*shared).clone();
                    let peer_addr = stream.peer_addr().ok();
                    std::thread::spawn(move || {
                        if let Err(e) = clipboard::push_to_windows(&cfg, peer_addr, post_action, &shared) {
                            eprintln!("[big-send] Pushing to Windows failed: {e}");
                        }
                    });
                }
            }
            PACKAGE_TYPE_MACHINE_SWITCHED => {
                // This Mac just became the active machine: if a drag was
                // going on on Windows, it's now over here.
                let to_us = unpack_u32_le(&full, 12) == cfg.machine_id;
                active_here |= to_us;
                // Only the client session forwards input and sees the
                // release, so only it starts a new activation's LET_GO.
                if to_us && cg.is_some() {
                    // The display may be asleep: wake it for the user who
                    // just crossed over (the Mac itself is kept awake, see
                    // `power`).
                    cg_input::declare_user_activity();
                    LET_GO.store(false, std::sync::atomic::Ordering::Relaxed);
                }
                if to_us
                    && clipboard.is_some()
                    && let Some(link) = companion::link().filter(|l| l.is_connected())
                {
                    link.send_empty(mwb_protocol::companion::ACTIVE);
                }
            }
            PACKAGE_TYPE_AWAKE => {
                // Someone is using Windows: keep this Mac's display awake,
                // as real MWB keeps its machines' screens on.
                if cg.is_some() {
                    cg_input::declare_user_activity();
                }
            }
            PACKAGE_TYPE_HELLO | PACKAGE_TYPE_BYEBYE | PACKAGE_TYPE_HEARTBEAT => {}
            _ => {
                if unhandled_seen.insert(package_type) {
                    let des_id = unpack_u32_le(&full, 12);
                    println!("{} [client] (first package of unhandled type {package_type}, src {src_id}, des {des_id})", timestamp());
                }
            }
        }
    }
}

/// Windows saw the cursor leave this Mac with the left button held and asks
/// whether a file is being dragged (ExplorerDragDrop). If one is, offer it
/// to machine `to` the way real MWB does (DragDropStep06: a ClipboardDragDrop
/// broadcast, then ClipboardDragDropOperation to the drop target), keep it
/// ready for Windows to fetch on drop, and cancel the drag here so it
/// doesn't drop anything on this Mac.
#[allow(clippy::too_many_arguments)]
fn offer_outgoing_drag(
    stream: &mut TcpStream,
    cipher: &CbcState,
    write_chain: &mut [u8; 16],
    magic_number: u32,
    my_id: u32,
    to: u32,
    next_id: &mut u32,
    cg: &mut CgInput,
    drag_out: &DragOut,
    shared: &ClipboardShared,
) -> std::io::Result<()> {
    if !cg.left_button_held() {
        return Ok(());
    }
    let dragged = drag_out.dragged_files();
    if dragged.is_empty() {
        println!("{} [drag-out] Left with the button held, but no file is being dragged.", timestamp());
        return Ok(());
    }
    let (files, skipped) = drag_out::split_sendable(dragged);
    for (path, reason) in &skipped {
        println!("{} [drag-out] Not sending {} to Windows: {reason}.", timestamp(), path.display());
    }
    if files.is_empty() {
        cg.cancel_drag();
        return Ok(());
    }
    let names: Vec<String> = files.iter().map(|f| f.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect();
    clipboard::set_pending_drag(shared, Outbound::Files(files));
    for (package_type, des) in [(PACKAGE_TYPE_CLIPBOARD_DRAG_DROP, ID_ALL), (PACKAGE_TYPE_CLIPBOARD_DRAG_DROP_OPERATION, to)] {
        let mut pkg = build_drag_drop_package(package_type, *next_id, my_id, des);
        *next_id = next_id.wrapping_add(1);
        finalize_send_buf(&mut pkg, magic_number);
        stream.write_all(&cipher.encrypt(write_chain, &pkg))?;
    }
    cg.cancel_drag();
    println!("{} [drag-out] Dragging {} to Windows ({}); sent when dropped there.", timestamp(), if names.len() == 1 { "1 file".to_string() } else { format!("{} files", names.len()) }, names.join(", "));
    Ok(())
}

/// Control just left this Mac with the left button held: if that's a file
/// drag, hand it to the companion, which carries it on as a Windows drag.
/// The drag here is cancelled so nothing gets dropped at the screen edge.
fn offer_to_companion(link: &std::sync::Arc<companion::Link>, cg: &mut CgInput, drag_out: &DragOut) {
    let dragged = drag_out.dragged_files();
    if dragged.is_empty() {
        return;
    }
    cg.cancel_drag();
    link.offer_files(mwb_protocol::companion::OFFER_TO_WINDOWS, dragged, "drag");
}

fn run_client(
    cfg: &Config,
    cg: &mut CgInput,
    clip_rx: &Receiver<ClipboardEvent>,
    clip_shared: &ClipboardShared,
    handshook: &mut bool,
    connected: &mut Option<Instant>,
) -> std::io::Result<()> {
    println!("Connecting to {}...", cfg.windows_ip);
    config::write_status(false, "", "Connecting...");
    let mut stream = route::connect_preferring_wired(&cfg.windows_ip)?;
    *connected = Some(Instant::now());
    set_socket_timeouts(&stream);
    *CURRENT_STREAM.lock().unwrap() = stream.try_clone().ok();
    // Input is only sent one packet at a time; don't let Nagle batch the
    // rare reply (e.g. NextMachine) behind a pending ACK.
    let _ = stream.set_nodelay(true);
    // A new connection starts with nothing held — see CgInput::reset.
    cg.reset();
    let result = run_session(&mut stream, Some(&mut *cg), cfg, Some((clip_rx, clip_shared)), handshook);
    CURRENT_STREAM.lock().unwrap().take();
    // Any drag from Windows over this Mac is cancelled below, so to the
    // companion it ended here (see LET_GO).
    LET_GO.store(true, std::sync::atomic::Ordering::Relaxed);
    drag_in::release_abandoned_press(cg);
    // A drag from Windows cut off by the connection ending: cancel it, or
    // releasing the button in reset() would drop it wherever the cursor is.
    if drag_source::is_active() {
        cg.cancel_drag();
    }
    cg.reset();
    result
}

/// Drops the connection to Windows whenever the Mac wakes from sleep, so it
/// reconnects right away. A connection that lived through a sleep is usually
/// dead without either side knowing (the Mac sends nothing while idle), and
/// it otherwise sat there until SOCKET_TIMEOUT — live-reported 2026-11-05:
/// the lid opened at 08:08:03 but input only came back at 08:10:55. Sleep is
/// noticed as the wall clock running ahead of `Instant`, which on macOS
/// doesn't advance while asleep.
fn watch_for_wake() {
    const TICK: Duration = Duration::from_secs(2);
    const SLEPT: Duration = Duration::from_secs(5);
    loop {
        let (wall, awake) = (SystemTime::now(), Instant::now());
        std::thread::sleep(TICK);
        let wall_elapsed = wall.elapsed().unwrap_or_default();
        if wall_elapsed > awake.elapsed() + SLEPT
            && let Some(stream) = CURRENT_STREAM.lock().unwrap().take()
        {
            println!("[client] {} Woke from sleep, reconnecting.", timestamp());
            let _ = stream.shutdown(Shutdown::Both);
            // The companion link lived through the same sleep.
            if let Some(link) = companion::link() {
                link.disconnect("woke from sleep");
            }
        }
    }
}

/// Moves a connection that is on Wi-Fi over to a wired path once one exists
/// (cable plugged in after connecting, or mDNS answering on Wi-Fi first) —
/// see `route`'s module docs. Does nothing while already wired. Only cuts
/// once per wired interface showing up, so a cable to a network Windows
/// isn't on can't make it reconnect over and over.
fn watch_for_wired_path(active: Arc<Mutex<Config>>) {
    const TICK: Duration = Duration::from_secs(5);
    let mut tried: Option<String> = None;
    loop {
        std::thread::sleep(TICK);
        let target = active.lock().unwrap().windows_ip.clone();
        let local = CURRENT_STREAM.lock().unwrap().as_ref().and_then(|s| s.local_addr().ok());
        let Some(current) = local.and_then(route::interface_of_local) else { continue };
        if !route::is_wifi(&current) {
            tried = None;
            continue;
        }
        let wired = route::ranked_candidates(&target)
            .ok()
            .and_then(|c| c.into_iter().find_map(|(_, i)| i.filter(|i| !route::is_wifi(i))));
        match wired {
            None => tried = None,
            Some(w) if tried.as_ref() != Some(&w) => {
                if let Some(stream) = CURRENT_STREAM.lock().unwrap().take() {
                    println!("[client] {} On Wi-Fi ({current}) but Windows is reachable over {w}, reconnecting.", timestamp());
                    let _ = stream.shutdown(Shutdown::Both);
                }
                tried = Some(w);
            }
            Some(_) => {}
        }
    }
}

/// Keeps forwarded input smooth while this menu-bar-only app has no visible
/// window: macOS otherwise counts it as idle and App Naps it, which
/// throttles its threads and delays socket wake-ups, so mouse packets
/// arrive in bursts (stuttering movement, late or lost-feeling clicks). Also
/// runs the calling (network) thread at the QoS real input handling uses.
/// Idle system sleep stays allowed.
fn keep_input_responsive() {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
    let options = NSActivityOptions::UserInitiatedAllowingIdleSystemSleep | NSActivityOptions::LatencyCritical;
    let activity = NSProcessInfo::processInfo()
        .beginActivityWithOptions_reason(options, &NSString::from_str("Forwarding mouse and keyboard input"));
    // Held for the life of the process.
    std::mem::forget(activity);
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

/// Accepts Windows' own reverse connection back to us — see `LISTEN_PORT`'s
/// doc comment for why this turned out to matter live. Runs forever in its
/// own thread; each accepted connection gets its own thread in turn.
fn run_server_listener(active: Arc<Mutex<Config>>) {
    let listener = match TcpListener::bind(("0.0.0.0", LISTEN_PORT)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[server] Failed to bind :{LISTEN_PORT} for the reverse connection: {e}");
            return;
        }
    };
    println!("[server] Listening on :{LISTEN_PORT} for Windows' reverse connection.");

    for conn in listener.incoming() {
        match conn {
            Ok(mut stream) => {
                println!("[server] Accepted connection from {:?}", stream.peer_addr());
                set_socket_timeouts(&stream);
                // The PC we're currently connected to (its security key).
                let cfg = active.lock().unwrap().clone();
                std::thread::spawn(move || {
                    if let Err(e) = run_session(&mut stream, None, &cfg, None, &mut false) {
                        eprintln!("[server] Session ended: {e}");
                    }
                });
            }
            Err(e) => eprintln!("[server] Accept error: {e}"),
        }
    }
}

fn network_thread(cfg: Config) {
    keep_input_responsive();
    windows_keys::ENABLED.store(cfg.windows_keys, std::sync::atomic::Ordering::Relaxed);
    // Every Windows PC this Mac can be controlled from; `active` is the one
    // currently connected to (or last tried), which the listeners below
    // answer with.
    let pcs = cfg.per_windows_pc();
    let active = Arc::new(Mutex::new(pcs[0].clone()));
    {
        let active = active.clone();
        std::thread::spawn(move || run_server_listener(active));
    }
    std::thread::spawn(watch_for_wake);
    std::thread::spawn(|| power::keep_awake_while_connected(|| CURRENT_STREAM.lock().unwrap().is_some()));
    {
        let active = active.clone();
        std::thread::spawn(move || watch_for_wired_path(active));
    }

    companion::start(active.clone(), current_peer);
    let clip_shared = ClipboardShared::new();
    let (clip_tx, clip_rx) = mpsc::channel::<ClipboardEvent>();
    {
        let clip_shared = clip_shared.clone();
        std::thread::spawn(move || clipboard::run_clipboard_watcher(clip_tx, clip_shared));
    }
    {
        let active = active.clone();
        let pending = clipboard::pending_outbound(&clip_shared);
        std::thread::spawn(move || clipboard::run_file_server_listener(active, pending));
    }

    // CGEventSource/CGEvent creation doesn't require the main thread —
    // unlike the menu bar UI, which tao/AppKit does require there — so this
    // runs entirely on its own background thread.
    let mut cg = CgInput::new();
    let mut i = 0;
    let mut delay = RECONNECT_FIRST;
    loop {
        let pc = &pcs[i];
        *active.lock().unwrap() = pc.clone();
        let mut handshook = false;
        let mut connected = None;
        let result = run_client(pc, &mut cg, &clip_rx, &clip_shared, &mut handshook, &mut connected);
        // A connection that worked for a while (cut by sleep, a cable change
        // or Windows restarting its side) is retried at once; failing
        // attempts back off, and so does one Windows keeps dropping. Timed
        // from when it connected: trying addresses that time out first
        // isn't a connection that worked.
        let worked = handshook && connected.is_some_and(|at: Instant| at.elapsed() >= RECONNECT_WORKED);
        delay = next_delay(delay, worked);
        if let Err(e) = result {
            eprintln!("{} Connection ended: {e}. Reconnecting in {} ms...", timestamp(), delay.as_millis());
            config::write_status(false, "", &format!("Disconnected: {e}. Reconnecting..."));
        }
        i = next_pc(i, pcs.len(), handshook);
        std::thread::sleep(delay);
    }
}

// How long to wait before reconnecting: RECONNECT_FIRST after a connection
// that got through the handshake and lasted RECONNECT_WORKED, then doubling up to RECONNECT_MAX while
// attempts keep failing.
const RECONNECT_FIRST: Duration = Duration::from_millis(250);
const RECONNECT_MAX: Duration = Duration::from_secs(4);
const RECONNECT_WORKED: Duration = Duration::from_secs(5);

fn next_delay(previous: Duration, worked: bool) -> Duration {
    if worked { RECONNECT_FIRST } else { (previous * 2).min(RECONNECT_MAX) }
}

/// Which Windows PC to try after PC `i` of `count`: the same one again if
/// its session got through the handshake (a real connection that dropped),
/// else the next. Deciding by handshake rather than by how long the attempt
/// took matters: a PC that is on the network but not listening only fails
/// after every address has timed out (~15s), which looked like a real
/// connection and kept the loop on that PC forever (seen live 2026-11-14).
fn next_pc(i: usize, count: usize, handshook: bool) -> usize {
    if handshook { i } else { (i + 1) % count }
}

/// Writes a starter config.json (with an obviously-fake security key and
/// Windows host) if none exists yet, so "Edit Config..." always has
/// something to open rather than erroring on a missing file. Never
/// overwrites an existing file, even a malformed one — a parse failure
/// should surface as "fix your edit," not silently discard it.
fn ensure_config_template() {
    let path = config::config_path();
    if path.exists() {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let template = r#"{
  "security_key": "CHANGE-ME-must-match-PowerToys-Settings-key",
  "windows_ip": "windows-pc.local:15101",
  "machine_name": "mac",
  "machine_id": 2,
  "scroll_speed": 1.0
}
"#;
    let _ = std::fs::write(&path, template);
}

fn main() {
    ensure_config_template();

    std::thread::spawn(|| {
        let cfg = loop {
            if let Some(cfg) = config::load_config() {
                break cfg;
            }
            config::write_status(
                false,
                "",
                "Not configured — edit config.json from the menu bar icon, then Restart Connection.",
            );
            std::thread::sleep(Duration::from_secs(5));
        };
        network_thread(cfg);
    });

    tray::run();
}

mod tray;

#[cfg(test)]
mod tests {
    use super::next_pc;

    #[test]
    fn retries_a_pc_that_completed_the_handshake() {
        assert_eq!(next_pc(0, 2, true), 0);
        assert_eq!(next_pc(1, 2, true), 1);
    }

    #[test]
    fn moves_on_from_a_pc_that_never_completed_the_handshake() {
        // Unreachable, slow to time out, or the wrong key: all the same.
        assert_eq!(next_pc(0, 2, false), 1);
        assert_eq!(next_pc(1, 2, false), 0);
    }

    #[test]
    fn reconnects_back_off_until_a_connection_works() {
        use super::{next_delay, RECONNECT_FIRST, RECONNECT_MAX};
        use std::time::Duration;
        let mut delay = RECONNECT_FIRST;
        let waits: Vec<u128> = (0..6).map(|_| { delay = next_delay(delay, false); delay.as_millis() }).collect();
        assert_eq!(waits, vec![500, 1000, 2000, 4000, 4000, 4000]);
        assert_eq!(next_delay(RECONNECT_MAX, true), RECONNECT_FIRST);
        assert_eq!(next_delay(Duration::from_millis(250), true), Duration::from_millis(250));
    }

    #[test]
    fn a_single_pc_is_always_retried() {
        assert_eq!(next_pc(0, 1, false), 0);
        assert_eq!(next_pc(0, 1, true), 0);
    }
}
