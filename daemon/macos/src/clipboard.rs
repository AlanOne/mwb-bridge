// Windows <-> macOS clipboard sync: text and images, both small- and
// big-path (mirroring the Linux build's "Clipboard images" milestone).
// Real MWB applies the same ~1MB small/big-path threshold to images as
// text, and real screenshots routinely exceed it — small-path-only would
// silently fail for most actual screenshots, so both paths are ported
// together rather than incrementally like text was. Plain file copy/paste
// (arbitrary files, not images) is *not* ported — a separate, later
// milestone on the Linux build too, not attempted here.
//
// Uses `arboard` (NSPasteboard) for clipboard access and the `image` crate
// for PNG<->RGBA conversion — real MWB's own wire payload for both text and
// images is exactly what the Linux build already worked out (raw-DEFLATE-
// wrapped UTF-16LE text; a plain, uncompressed PNG file for images, no
// wrapper at all), all in the shared `mwb_protocol` crate already; only
// clipboard *access* and codec conversion are new macOS-specific code.

use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arboard::{Clipboard, ImageData};
use flate2::read::DeflateDecoder;
use image::ExtendedColorType;
use image::codecs::png::PngEncoder;
use image::ImageEncoder;
use mwb_protocol::config::Config;
use mwb_protocol::mwb_protocol::*;

// The separate "clipboard server" port (BASE_PORT) that big-path bytes
// travel over — a distinct TCP connection from the message server, opened
// on demand in whichever direction a big-path transfer is actually
// happening. Same port Linux's daemon.rs uses.
pub const FILE_LISTEN_PORT: u16 = 15100;

pub enum ClipboardEvent {
    Text(String),
    Image(Vec<u8>),    // PNG bytes, small-path (<1MB)
    BigImage(PathBuf),  // PNG bytes already written to a temp file, big-path
    Files(Vec<PathBuf>), // a Finder file copy, sent big-path
}

/// Big-path content waiting for Windows to fetch it, either by connecting
/// to our port 15100 or, in practice, by asking us to push it (ClipboardAsk).
#[derive(Clone, Debug)]
pub enum Outbound {
    /// A clipboard image too big for the small path, as a PNG file.
    Image(PathBuf),
    /// Files dragged from this Mac to Windows, sent one after another.
    Files(Vec<PathBuf>),
}

/// One file of pending content, and the name its header carries.
type Item = (PathBuf, String);

/// What `outbound` sends, one item per transfer. A file goes by its base
/// name, the only part of the header's name Windows keeps, behind a `/`
/// (see `file_header_name`).
fn outbound_items(outbound: &Outbound) -> Vec<Item> {
    match outbound {
        Outbound::Image(path) => vec![(path.clone(), BIG_PATH_IMAGE_NAME.to_string())],
        Outbound::Files(paths) => paths
            .iter()
            .map(|p| (p.clone(), file_header_name(&p.file_name().map_or_else(|| "file".to_string(), |n| n.to_string_lossy().into_owned()))))
            .collect(),
    }
}

/// Shared state between the client connection's receive loop, the file-
/// server listener (port 15100), and the local clipboard-watcher thread —
/// mirrors the Linux build's `ClipboardShared`, minus the file-transfer
/// fields that milestone doesn't apply to macOS (no plain file copy/paste
/// here).
#[derive(Clone)]
pub struct ClipboardShared {
    last_applied_text: Arc<Mutex<Option<String>>>,
    // The file last put on the clipboard from Windows, so the watcher
    // doesn't send it straight back as a new copy.
    last_applied_files: Arc<Mutex<Vec<PathBuf>>>,
    // Decoded RGBA pixels, *not* PNG container bytes — see
    // `apply_incoming_clipboard_image`'s doc comment for why the comparison
    // has to happen on pixels rather than the outer PNG encoding.
    last_applied_image_rgba: Arc<Mutex<Option<(usize, usize, Vec<u8>)>>>,
    pending_outbound: Arc<Mutex<Option<Outbound>>>,
    // Windows' own machine ID, learned from the Src field of anything it's
    // sent us — needed to address a MachineSwitched package directly to it
    // (see `announce_big_image`).
    peer_machine_id: Arc<Mutex<Option<u32>>>,
}

impl ClipboardShared {
    pub fn new() -> Self {
        Self {
            last_applied_text: Arc::new(Mutex::new(None)),
            last_applied_files: Arc::new(Mutex::new(Vec::new())),
            last_applied_image_rgba: Arc::new(Mutex::new(None)),
            pending_outbound: Arc::new(Mutex::new(None)),
            peer_machine_id: Arc::new(Mutex::new(None)),
        }
    }

    /// Called from the client connection's receive loop for every package
    /// carrying a `Src`, so a later big-path announcement can address
    /// Windows directly (see `announce_big_image`'s doc comment on why).
    pub fn note_peer_machine_id(&self, src_id: u32) {
        if src_id != 0 && src_id != ID_ALL {
            *self.peer_machine_id.lock().unwrap() = Some(src_id);
        }
    }
}

impl Default for ClipboardShared {
    fn default() -> Self {
        Self::new()
    }
}

fn cache_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    PathBuf::from(home).join("Library/Caches/mwb-mac-bridge")
}

/// Applies incoming ClipboardText bytes (already-accumulated small-path
/// chunks) to the local clipboard: raw-DEFLATE-inflate, UTF-16LE-decode,
/// strip the `"TXT" + text + SEP` wrapper real MWB sends — see
/// PROTOCOL.md's clipboard section. Records what was applied so the
/// outbound watcher can recognize its own echo and not immediately bounce
/// it right back to Windows.
pub fn apply_incoming_clipboard_text(compressed: &[u8], shared: &ClipboardShared) {
    let mut inflated = Vec::new();
    if let Err(e) = DeflateDecoder::new(compressed).read_to_end(&mut inflated) {
        eprintln!("(clipboard: failed to inflate incoming text, ignoring: {e})");
        return;
    }
    let utf16: Vec<u16> = inflated.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
    let decoded = String::from_utf16_lossy(&utf16);

    let Some(text) = decoded.split(CLIPBOARD_SEP).find_map(|frag| frag.strip_prefix("TXT")) else {
        // Some Windows copies (seen live from a browser, 2026-09-29) arrive
        // as an `HTM` fragment only — CF_HTML, no plain-text fragment at
        // all. Apply those as HTML plus a tag-stripped plain-text
        // alternative, so both rich and plain paste targets get something.
        if let Some(cf_html) = decoded.split(CLIPBOARD_SEP).find_map(|frag| frag.strip_prefix("HTM")) {
            apply_incoming_clipboard_html(cf_html, shared);
            return;
        }
        let shape: Vec<String> = decoded
            .split(CLIPBOARD_SEP)
            .map(|f| {
                let head: Vec<String> = f.chars().take(4).map(|c| format!("{:04x}", c as u32)).collect();
                format!("[len={} head={}]", f.chars().count(), head.join(" "))
            })
            .collect();
        // Shape only (tag + length per fragment, first chars' code points),
        // never the clipboard content itself.
        eprintln!("(clipboard: no TXT or HTM fragment in incoming data, ignoring: inflated={}B fragments={})", inflated.len(), shape.join(" "));
        return;
    };

    match Clipboard::new().and_then(|mut cb| cb.set_text(text.to_string())) {
        Ok(()) => {
            *shared.last_applied_text.lock().unwrap() = Some(text.to_string());
            println!("(clipboard: applied {} chars from Windows)", text.chars().count());
        }
        Err(e) => eprintln!("(clipboard: failed to set local clipboard: {e})"),
    }
}

/// Applies a CF_HTML payload (`Version:...` header lines, then the HTML
/// document with the copied part between `<!--StartFragment-->` and
/// `<!--EndFragment-->`). The header's StartFragment/EndFragment byte
/// offsets aren't used — they count UTF-8 bytes of Windows' original
/// buffer, which doesn't survive MWB's UTF-16 round trip reliably; the
/// comment markers are part of the CF_HTML spec and always present.
fn apply_incoming_clipboard_html(cf_html: &str, shared: &ClipboardShared) {
    let fragment = match (cf_html.find("<!--StartFragment-->"), cf_html.find("<!--EndFragment-->")) {
        (Some(a), Some(b)) if a < b => &cf_html[a + "<!--StartFragment-->".len()..b],
        _ => cf_html.find('<').map_or(cf_html, |i| &cf_html[i..]),
    };
    let plain = html_to_plain_text(fragment);
    match Clipboard::new().and_then(|mut cb| cb.set_html(fragment, Some(plain.as_str()))) {
        Ok(()) => {
            // The watcher sees the plain-text alternative via get_text() —
            // record it so that isn't bounced back to Windows as new.
            println!("(clipboard: applied {} chars of HTML from Windows, {} chars plain-text alternative)", fragment.chars().count(), plain.chars().count());
            *shared.last_applied_text.lock().unwrap() = Some(plain);
        }
        Err(e) => eprintln!("(clipboard: failed to set local clipboard HTML: {e})"),
    }
}

/// Crude but adequate HTML-to-text for the plain-text alternative: block
/// ends become newlines, tags are dropped, the common entities decoded,
/// runs of blank lines collapsed. Not a real HTML parser — rich targets get
/// the actual HTML anyway; this only needs to be sensible in a plain one.
fn html_to_plain_text(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let Some(gt) = rest[lt..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[lt + 1..lt + gt].trim_start_matches('/').to_ascii_lowercase();
        let name = tag.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
        let is_block = matches!(name, "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6");
        if is_block && !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        rest = &rest[lt + gt + 1..];
    }
    out.push_str(rest);
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let mut lines: Vec<&str> = Vec::new();
    for line in decoded.lines().map(str::trim) {
        if !(line.is_empty() && lines.last().is_none_or(|l| l.is_empty())) {
            lines.push(line);
        }
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Applies received clipboard-image bytes directly to the local clipboard.
/// Real MWB's wire payload for `ClipboardImage` is a plain PNG file
/// verbatim, no wrapper/compression (confirmed on the Linux build from
/// source: `image.Save(stream, ImageFormat.Png)`) — decoded here via the
/// `image` crate into the raw RGBA bytes `arboard`'s `set_image` wants
/// (unlike `wl-copy`, which took the PNG bytes directly — NSPasteboard's
/// Rust API here works in decoded pixels, not an arbitrary MIME payload).
///
/// Records the *decoded pixels*, not the PNG bytes, as what was applied —
/// real bug found live, 2026-09-23: `arboard`'s `get_image()` only ever
/// returns decoded RGBA, never the original PNG container, so the outbound
/// watcher re-encodes it from scratch every poll. That re-encoding is not
/// byte-identical to Windows' own PNG encoder even for pixel-identical
/// images (different compression/filter choices) — comparing PNG bytes for
/// echo-detection therefore never matched, and every incoming image
/// immediately bounced straight back to Windows as if it were a fresh local
/// change. Comparing decoded pixels instead is encoder-independent and
/// fixes this.
pub fn apply_incoming_clipboard_image(png_bytes: &[u8], shared: &ClipboardShared) {
    let rgba = match image::load_from_memory(png_bytes) {
        Ok(img) => img.to_rgba8(),
        Err(e) => {
            eprintln!("(clipboard: failed to decode incoming PNG, ignoring: {e})");
            return;
        }
    };
    let (width, height) = (rgba.width() as usize, rgba.height() as usize);
    let raw = rgba.into_raw();
    let image_data = ImageData { width, height, bytes: raw.clone().into() };
    match Clipboard::new().and_then(|mut cb| cb.set_image(image_data)) {
        Ok(()) => {
            *shared.last_applied_image_rgba.lock().unwrap() = Some((width, height, raw));
            println!("(clipboard: applied a {}-byte image from Windows)", png_bytes.len());
        }
        Err(e) => eprintln!("(clipboard: failed to set local clipboard image: {e})"),
    }
}

/// Reads the local clipboard's current image, if any, as decoded RGBA
/// pixels — the comparison unit for change/echo detection (see
/// `apply_incoming_clipboard_image`'s doc comment for why PNG bytes don't
/// work for this). Encoded to PNG only once actually decided to send, in
/// `run_clipboard_watcher`.
fn read_local_clipboard_image_rgba() -> Option<(usize, usize, Vec<u8>)> {
    let image_data = Clipboard::new().ok()?.get_image().ok()?;
    Some((image_data.width, image_data.height, image_data.bytes.into_owned()))
}

fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut png_bytes = Vec::new();
    PngEncoder::new(&mut png_bytes).write_image(rgba, width as u32, height as u32, ExtendedColorType::Rgba8).ok()?;
    Some(png_bytes)
}

/// Downscales an image until its PNG encoding fits the small path, for
/// images whose full-resolution PNG doesn't. The big path is the "right"
/// transport for these, but Windows never connects back to pull an image
/// this bridge announces (same unresolved wall as the Linux build — see the
/// project notes' "File/big-image transfer back to Windows"; confirmed live
/// on macOS 2026-09-29: two announcements, zero pulls), so a full-size
/// Retina screenshot otherwise never arrives at all. Arriving at reduced
/// resolution beats not arriving. First guess assumes PNG size scales with
/// pixel count; each retry shrinks a further 20%. `None` only if even that
/// fails (caller falls back to the big-path announcement).
fn shrink_png_to_fit(width: usize, height: usize, rgba: &[u8], full_png_len: usize) -> Option<Vec<u8>> {
    use image::{imageops, RgbaImage};
    let full = RgbaImage::from_raw(width as u32, height as u32, rgba.to_vec())?;
    let mut scale = ((MAX_SMALL_PATH_SIZE as f64 / full_png_len as f64).sqrt() * 0.95).min(0.95);
    for _ in 0..8 {
        let w = ((width as f64 * scale).round() as u32).max(1);
        let h = ((height as f64 * scale).round() as u32).max(1);
        let resized = imageops::resize(&full, w, h, imageops::FilterType::Triangle);
        let png = encode_png(w as usize, h as usize, resized.as_raw())?;
        if png.len() < MAX_SMALL_PATH_SIZE {
            println!("(clipboard: image too big for the small path ({full_png_len} bytes at {width}x{height}), sending it downscaled to {w}x{h}, {} bytes)", png.len());
            return Some(png);
        }
        scale *= 0.8;
    }
    None
}

/// macOS -> Windows clipboard sync: polls the local clipboard (no simple
/// blocking "notify me on change" primitive here either, same as
/// `wl-clipboard`'s CLI tools on Linux) and forwards genuinely new content
/// into `clip_tx`. Files come first, then text, then an image, the order
/// real MWB uses (see `text_wins_over_image` for the one exception). Each
/// kind's "last seen" is forgotten while another kind is on the clipboard,
/// so copying the same text again after an image still sends it.
pub fn run_clipboard_watcher(clip_tx: Sender<ClipboardEvent>, shared: ClipboardShared) {
    let mut last_change = pasteboard_change_count();
    let mut last_seen_image = read_local_clipboard_image_rgba();
    let mut last_seen_text = Clipboard::new().ok().and_then(|mut cb| cb.get_text().ok());
    let mut last_seen_files = read_local_clipboard_files();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        // Nothing is read until the pasteboard has changed: reading an
        // image means fetching it as TIFF and converting it to RGBA, every
        // half second for as long as one sits on the clipboard otherwise. A
        // change only counts as handled once a read has found files, text
        // or an image in it: an app may clear the pasteboard and fill it a
        // moment later under the same count, and a read can fail, so until
        // then it's looked at again each time.
        let change = pasteboard_change_count();
        if change == last_change {
            continue;
        }

        let files = read_local_clipboard_files();
        if !files.is_empty() {
            last_change = change;
            if is_new_file_copy(&files, &last_seen_files, &shared.last_applied_files.lock().unwrap()) {
                let _ = clip_tx.send(ClipboardEvent::Files(files.clone()));
            }
            last_seen_files = files;
            (last_seen_text, last_seen_image) = (None, None);
            // A Finder file copy also puts the file's *icon* on the
            // pasteboard as TIFF (and its name as plain text): sending
            // either would clobber Windows' clipboard with a picture of a
            // document icon or a bare file name.
            continue;
        }
        last_seen_files = files;

        // Text first, as real MWB does, unless it's only a URL next to an
        // image (see `text_wins_over_image`). Reading the text is cheap; the
        // image is only read when it could be what's sent.
        let text = Clipboard::new().ok().and_then(|mut cb| cb.get_text().ok());
        let text_wins = text.as_deref().is_some_and(text_wins_over_image);
        let current_image = if text_wins { None } else { read_local_clipboard_image_rgba() };
        if text_wins || current_image.is_none() {
            last_seen_image = None;
        }
        if let Some((width, height, ref raw)) = current_image {
            last_change = change;
            last_seen_text = None;
            if last_seen_image != current_image {
                let is_echo = shared.last_applied_image_rgba.lock().unwrap().as_ref() == current_image.as_ref();
                last_seen_image = current_image.clone();
                if !is_echo {
                    if let Some(png_bytes) = encode_png(width, height, raw) {
                        if png_bytes.len() < MAX_SMALL_PATH_SIZE {
                            let _ = clip_tx.send(ClipboardEvent::Image(png_bytes));
                        } else if let Some(small) = shrink_png_to_fit(width, height, raw, png_bytes.len()) {
                            let _ = clip_tx.send(ClipboardEvent::Image(small));
                        } else {
                            let dir = cache_dir();
                            if std::fs::create_dir_all(&dir).is_ok() {
                                let path = dir.join("outbound-image.png");
                                if std::fs::write(&path, &png_bytes).is_ok() {
                                    let _ = clip_tx.send(ClipboardEvent::BigImage(path));
                                }
                            }
                        }
                    }
                }
            }
            continue; // an image is present — don't also fall through to text
        }

        let Some(current) = text else { continue };
        last_change = change;
        if last_seen_text.as_deref() == Some(current.as_str()) {
            continue;
        }
        last_seen_text = Some(current.clone());
        if shared.last_applied_text.lock().unwrap().as_deref() == Some(current.as_str()) {
            continue; // our own echo from applying Windows' clipboard moments ago
        }
        let _ = clip_tx.send(ClipboardEvent::Text(current));
    }
}

/// The general pasteboard's change count, which goes up with every new
/// copy (ours included).
fn pasteboard_change_count() -> isize {
    objc2_app_kit::NSPasteboard::generalPasteboard().changeCount()
}

/// The files on the local clipboard (a Finder copy), if any.
fn read_local_clipboard_files() -> Vec<PathBuf> {
    Clipboard::new().ok().and_then(|mut cb| cb.get().file_list().ok()).unwrap_or_default()
}

/// Whether the files on the clipboard are a new copy to send to Windows:
/// not what the last poll already saw, and not the file just received from
/// Windows (`last_applied`).
fn is_new_file_copy(current: &[PathBuf], last_seen: &[PathBuf], last_applied: &[PathBuf]) -> bool {
    !current.is_empty() && current != last_seen && current != last_applied
}

/// Announces big-path content (a big image, or a copied file) to the peer:
/// the `Clipboard` beat, then — if we've learned the peer's machine ID from
/// something it's sent us already — a `MachineSwitched` addressed directly
/// to it. Windows only fetches announced content around a switch
/// (Receiver.cs's MachineSwitched case, or its own switch back to itself
/// within 30s of the beat); the `MachineSwitched` makes it fetch right away,
/// which reaches us as a ClipboardAsk (see `push_to_windows`). Real switches
/// also send `HideMouse` first; it's left out here, since on Windows it
/// hides the cursor and releases every held key while the user is still
/// working on the Mac.
pub fn announce_big(
    stream: &mut TcpStream,
    cipher: &CbcState,
    write_chain: &mut [u8; 16],
    magic_number: u32,
    src_id: u32,
    peer_machine_id: Option<u32>,
) -> std::io::Result<()> {
    use std::io::Write;
    use rand::RngExt;
    let mut beat = build_clipboard_beat(rand::rng().random(), src_id);
    finalize_send_buf(&mut beat, magic_number);
    stream.write_all(&cipher.encrypt(write_chain, &beat))?;

    if let Some(des_id) = peer_machine_id {
        let mut switched = build_machine_switched(rand::rng().random(), src_id, des_id);
        finalize_send_buf(&mut switched, magic_number);
        stream.write_all(&cipher.encrypt(write_chain, &switched))?;
    } else {
        eprintln!("(clipboard: haven't learned the peer's machine ID yet, skipping MachineSwitched — beat sent alone, may not trigger a real pull)");
    }
    Ok(())
}

/// Returns `addr` with its port changed, preserving an IPv6 address's scope
/// ID/flow info if present — see the Linux build's identical helper for why
/// this matters (a plain `SocketAddr::new` silently drops a link-local
/// IPv6 address's scope ID, breaking the connection).
fn with_port(addr: SocketAddr, port: u16) -> SocketAddr {
    match addr {
        SocketAddr::V4(v4) => SocketAddr::new((*v4.ip()).into(), port),
        SocketAddr::V6(v6) => SocketAddr::V6(std::net::SocketAddrV6::new(*v6.ip(), port, v6.flowinfo(), v6.scope_id())),
    }
}

/// Opens a connection to Windows' clipboard-server socket (port 15100), at
/// the address the live message connection already uses (see PROTOCOL.md
/// on why not to resolve the hostname again).
fn connect_to_windows_clipboard_port(cfg: &Config, peer_addr: Option<SocketAddr>) -> std::io::Result<TcpStream> {
    connect_clipboard(windows_clipboard_addr(cfg, peer_addr)?)
}

fn windows_clipboard_addr(cfg: &Config, peer_addr: Option<SocketAddr>) -> std::io::Result<SocketAddr> {
    use std::net::ToSocketAddrs;
    match peer_addr {
        Some(addr) => Ok(with_port(addr, FILE_LISTEN_PORT)),
        None => {
            let host = cfg.windows_ip.rsplit_once(':').map(|(h, _)| h).unwrap_or(&cfg.windows_ip);
            (host, FILE_LISTEN_PORT).to_socket_addrs()?.next().ok_or_else(|| std::io::Error::other("Windows' address didn't resolve"))
        }
    }
}

fn connect_clipboard(addr: SocketAddr) -> std::io::Result<TcpStream> {
    let stream = TcpStream::connect(addr)?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(300)));
    Ok(stream)
}

/// A pull from Windows' clipboard-server socket with the header read, ready
/// for its body.
struct Pull {
    stream: TcpStream,
    cipher: CbcState,
    read_chain: [u8; 16],
    size: usize,
    name: String,
}

impl Pull {
    fn read_body_into(&mut self, out: &mut impl std::io::Write) -> std::io::Result<()> {
        read_pulled_body(&mut self.stream, &self.cipher, &mut self.read_chain, self.size, out)
    }
}

/// Connects to Windows' separate clipboard-server socket (port 15100) and
/// reads the header of whatever it has for us: a big-path image (name
/// `"image"`), big-path text (name `"text"`) or a copied or dragged file
/// (name = its full Windows path).
/// `post_action` tells Windows why we're asking (`POST_ACTION_*`). `None`
/// when Windows sends no real payload (its errors come as a size-0 header).
fn open_pull(cfg: &Config, peer_addr: Option<SocketAddr>, post_action: u32) -> std::io::Result<Option<Pull>> {
    use std::io::Write;
    let mut stream = connect_to_windows_clipboard_port(cfg, peer_addr)?;

    let EncryptedStream { cipher, mut write_chain, mut read_chain } = open_encrypted_stream(&mut stream, &cfg.security_key)?;

    // Pulling/receiving, so our handshake type is Clipboard (not
    // ClipboardPush — that's Windows' side, since it's about to push the
    // actual bytes). No finalize_send_buf here — this handshake has no
    // checksum/magic-number scheme, unlike the message server's.
    let hs = build_clipboard_handshake_with_action(PACKAGE_TYPE_CLIPBOARD, post_action, cfg.machine_id, &cfg.machine_name);
    stream.write_all(&cipher.encrypt(&mut write_chain, &hs))?;

    let mut peer_hs_ct = [0u8; PACKAGE_SIZE_EX];
    stream.read_exact(&mut peer_hs_ct)?;
    let _peer_hs = cipher.decrypt(&mut read_chain, &peer_hs_ct);

    let mut header_ct = [0u8; CLIPBOARD_FILE_HEADER_SIZE];
    stream.read_exact(&mut header_ct)?;
    let header_pt = cipher.decrypt(&mut read_chain, &header_ct);
    let header_arr: [u8; CLIPBOARD_FILE_HEADER_SIZE] = header_pt.try_into().unwrap();
    let Some((size, name)) = parse_file_header(&header_arr) else {
        eprintln!("[pull] Couldn't parse the file header, aborting.");
        return Ok(None);
    };
    if size == 0 || size > MAX_CLIPBOARD_FILE_SIZE {
        eprintln!("[pull] Rejected: size={size} name={name:?} (likely a sender-side error, not a real payload).");
        return Ok(None);
    }
    Ok(Some(Pull { stream, cipher, read_chain, size, name }))
}

/// Pulls whatever Windows announced for copy and paste and puts it on the
/// clipboard once fully received. Mirrors the Linux build's
/// `pull_file_from_windows`.
pub fn pull_from_windows(cfg: &Config, peer_addr: Option<SocketAddr>, shared: &ClipboardShared) -> std::io::Result<()> {
    let Some(mut pull) = open_pull(cfg, peer_addr, POST_ACTION_OTHER)? else { return Ok(()) };
    let (size, name) = (pull.size, pull.name.clone());

    if name == BIG_PATH_IMAGE_NAME || name == BIG_PATH_TEXT_NAME {
        let mut body = Vec::with_capacity(size);
        pull.read_body_into(&mut body)?;
        if name == BIG_PATH_IMAGE_NAME {
            apply_incoming_clipboard_image(&body, shared);
            println!("[image-pull] Received a big clipboard image ({size} bytes) from Windows.");
        } else {
            apply_incoming_clipboard_text(&body, shared);
            println!("[text-pull] Received big clipboard text ({size} bytes compressed) from Windows.");
        }
        return Ok(());
    }

    // A file goes straight to disk as it arrives, not through memory.
    let dir = received_files_dir();
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(safe_file_name(&name));
    let written = std::fs::File::create(&dest).and_then(|file| {
        let mut out = std::io::BufWriter::new(file);
        pull.read_body_into(&mut out)?;
        out.into_inner().map_err(|e| e.into_error())?.sync_all()
    });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&dest);
        return Err(e);
    }
    // Fully written before the clipboard is touched below.
    match Clipboard::new().and_then(|mut cb| cb.set().file_list(&[&dest])) {
        Ok(()) => {
            *shared.last_applied_files.lock().unwrap() = vec![dest.clone()];
            println!("[file-pull] Received {:?} ({size} bytes) from Windows -> {}", dest.file_name().unwrap_or_default(), dest.display());
        }
        Err(e) => eprintln!("[file-pull] Saved {} but failed to put it on the clipboard: {e}", dest.display()),
    }
    Ok(())
}

/// Where files received from Windows get written before being placed on
/// the clipboard (as a file URL, same as a Finder copy — pasting in Finder
/// copies it out of here). Same scheme as the Linux build's
/// `~/.cache/omarchy-mwb-bridge-files`; a later file with the same name
/// overwrites the earlier one, also as on Linux.
fn received_files_dir() -> PathBuf {
    cache_dir().join("received-files")
}

/// Files copied on Windows, offered by the companion: fetches them all
/// into a folder of their own under `received_files_dir`, then puts them on
/// the clipboard, as a Finder copy of them would.
pub fn receive_companion_copy(link: &Arc<crate::companion::Link>, offer: mwb_protocol::companion::Offer, shared: &ClipboardShared) {
    let root = received_files_dir().join("copies");
    crate::drag_in::prune_old_drags(&root, Duration::from_secs(24 * 3600));
    let dir = root.join(offer.id.to_string());
    let link = link.clone();
    let shared = shared.clone();
    std::thread::spawn(move || match link.fetch_files(&offer, &dir, |_| {}) {
        Ok(paths) => match Clipboard::new().and_then(|mut cb| cb.set().file_list(&paths)) {
            Ok(()) => {
                *shared.last_applied_files.lock().unwrap() = paths.clone();
                println!("[file-copy] {} file(s) copied on Windows are on the clipboard.", paths.len());
            }
            Err(e) => eprintln!("[file-copy] Received {} file(s) but couldn't put them on the clipboard: {e}", paths.len()),
        },
        Err(e) => eprintln!("[file-copy] Fetching files copied on Windows failed: {e}"),
    });
}

/// Where a file dragged from Windows lands when it's let go before it could
/// be handed to an app: the Mac counterpart of where real MWB saves a drop
/// on Windows (`Desktop\MouseWithoutBorders\`). A later file with the same
/// name replaces the earlier one, as there.
pub fn dropped_files_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    PathBuf::from(home).join("Desktop/MouseWithoutBorders")
}

/// Where files dragged from Windows are fetched to while being dragged
/// here, one folder per drag: the app they're dropped on reads them from
/// there, possibly a while later (a chat app uploading an attachment).
pub fn dragged_files_dir() -> PathBuf {
    cache_dir().join("dragged-files")
}

/// Fetches the file being dragged on Windows into `dir`, writing it to disk
/// as it arrives. `on_created` gets its path as soon as the (still empty)
/// file exists, so a drag of it can start while the rest downloads.
pub fn fetch_dragged_file(cfg: &Config, peer_addr: Option<SocketAddr>, dir: &std::path::Path, on_created: impl FnOnce(&std::path::Path)) -> std::io::Result<PathBuf> {
    let Some(mut pull) = open_pull(cfg, peer_addr, POST_ACTION_DESKTOP)? else {
        return Err(std::io::Error::other("Windows sent no file"));
    };
    std::fs::create_dir_all(dir)?;
    let dest = dir.join(safe_file_name(&pull.name));
    let mut file = std::fs::File::create(&dest)?;
    on_created(&dest);
    let result = pull.read_body_into(&mut file).and_then(|()| file.sync_all());
    if let Err(e) = result {
        // Not left half-written (a drag of it is cancelled on release).
        let _ = std::fs::remove_file(&dest);
        return Err(e);
    }
    Ok(dest)
}

/// The header's `name` is a full Windows path (`C:\Users\...\file.txt`) —
/// `Path::file_name` only splits on `/` here, so split on both manually
/// (the Linux build hit this live: a file landed named with the literal
/// full path). Also refuses `.`/`..`/empty, which would otherwise make
/// `dir.join` resolve outside the received-files directory.
pub(crate) fn safe_file_name(windows_path: &str) -> String {
    let base = windows_path.rsplit(['/', '\\']).next().unwrap_or(windows_path).trim();
    match base {
        "" | "." | ".." => "received-file".to_string(),
        _ => base.to_string(),
    }
}

/// Sends one item as the 1024-byte header plus body, over a clipboard-
/// server connection whose handshake is done.
fn send_item(stream: &mut TcpStream, cipher: &CbcState, chain: &mut [u8; 16], (path, name): &Item) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len() as usize;
    let header = build_file_header(size, name);
    stream.write_all(&cipher.encrypt(chain, &header))?;
    write_padded_body(stream, cipher, chain, &mut std::io::BufReader::new(&mut file), size)?;
    println!("[big-send] Sent {name:?} ({size} bytes) to Windows.");
    Ok(())
}

/// Serves the pending big-path content to a peer that just connected to
/// our clipboard-server listener (port 15100) to pull it — mirrors the
/// Linux build's `serve_file_to_peer`.
fn serve_pending(mut stream: TcpStream, cfg: &Config, pending: &Arc<Mutex<Option<Outbound>>>) -> std::io::Result<()> {
    use std::io::Write;
    let EncryptedStream { cipher, mut write_chain, mut read_chain } = open_encrypted_stream(&mut stream, &cfg.security_key)?;

    let hs = build_clipboard_handshake(PACKAGE_TYPE_CLIPBOARD_PUSH, cfg.machine_id, &cfg.machine_name);
    stream.write_all(&cipher.encrypt(&mut write_chain, &hs))?;

    let mut peer_hs_ct = [0u8; PACKAGE_SIZE_EX];
    stream.read_exact(&mut peer_hs_ct)?;
    let _peer_hs = cipher.decrypt(&mut read_chain, &peer_hs_ct);

    // A pull carries one file, so only the first of several. In practice
    // Windows never pulls (see push_to_windows), this is for completeness.
    let first = pending.lock().unwrap().as_ref().and_then(|o| outbound_items(o).into_iter().next());
    let Some(item) = first else {
        let header = build_file_header(0, "Nothing currently available");
        stream.write_all(&cipher.encrypt(&mut write_chain, &header))?;
        return Ok(());
    };
    send_item(&mut stream, &cipher, &mut write_chain, &item)
}

/// Answers Windows' ClipboardAsk: connects to *its* clipboard-server port
/// and pushes the pending big-path content (a dragged file, or a big
/// image). This is how Windows fetches anything big from this bridge: it
/// only pulls over a message connection it opened itself
/// (`IsConnectedByAClientSocketTo` in real MWB), never opens one to us, and
/// so asks us to push instead. `post_action` is echoed from the ask and
/// tells Windows what to do with the data (save a drop to its desktop, or
/// put it on the clipboard). Several dragged files go one per connection:
/// Windows takes one file per transfer, but accepts further pushes.
pub fn push_to_windows(cfg: &Config, peer_addr: Option<SocketAddr>, post_action: u32, shared: &ClipboardShared) -> std::io::Result<()> {
    let Some(outbound) = shared.pending_outbound.lock().unwrap().clone() else {
        // Connecting with nothing to send would leave Windows saving an
        // empty file, so just don't answer.
        println!("[big-send] Windows asked for big-path data, but nothing is pending.");
        return Ok(());
    };
    push_items(cfg, windows_clipboard_addr(cfg, peer_addr)?, post_action, &outbound_items(&outbound));
    Ok(())
}

/// Pushes each item over its own connection to `addr`, strictly one after
/// another: Windows handles one transfer at a time and refuses a new one
/// that waits more than 3s for the last (Clipboard.cs's
/// ExecuteClipboardReceive). A failed item is logged and skipped.
fn push_items(cfg: &Config, addr: SocketAddr, post_action: u32, items: &[Item]) {
    for (i, item) in items.iter().enumerate() {
        if let Err(e) = push_item(cfg, addr, post_action, item) {
            eprintln!("[big-send] Sending {:?} ({} of {}) failed: {e}", item.1, i + 1, items.len());
        }
    }
}

fn push_item(cfg: &Config, addr: SocketAddr, post_action: u32, item: &Item) -> std::io::Result<()> {
    use std::io::Write;
    let mut stream = connect_clipboard(addr)?;
    let EncryptedStream { cipher, mut write_chain, mut read_chain } = open_encrypted_stream(&mut stream, &cfg.security_key)?;

    let hs = build_clipboard_handshake_with_action(PACKAGE_TYPE_CLIPBOARD_PUSH, post_action, cfg.machine_id, &cfg.machine_name);
    stream.write_all(&cipher.encrypt(&mut write_chain, &hs))?;

    let mut peer_hs_ct = [0u8; PACKAGE_SIZE_EX];
    stream.read_exact(&mut peer_hs_ct)?;
    let _peer_hs = cipher.decrypt(&mut read_chain, &peer_hs_ct);

    send_item(&mut stream, &cipher, &mut write_chain, item)?;
    // Windows keeps reading past the file's size until we close our side,
    // then saves the file and closes its side: wait for that, so the next
    // transfer doesn't arrive while this one is still being saved.
    stream.shutdown(std::net::Shutdown::Write)?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let mut rest = Vec::new();
    let _ = stream.read_to_end(&mut rest);
    Ok(())
}

/// Listens on the clipboard-server port for Windows connecting in to pull
/// pending big-path content. Mirrors the Linux build's
/// `run_file_server_listener`.
pub fn run_file_server_listener(active: Arc<Mutex<Config>>, pending: Arc<Mutex<Option<Outbound>>>) {
    let listener = match TcpListener::bind(("0.0.0.0", FILE_LISTEN_PORT)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[image-serve] Failed to bind :{FILE_LISTEN_PORT}: {e}");
            return;
        }
    };
    println!("[image-serve] Listening on :{FILE_LISTEN_PORT} for big-path image pulls.");

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(300)));
                // The PC we're currently connected to (its security key).
                let cfg = active.lock().unwrap().clone();
                let pending = pending.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve_pending(stream, &cfg, &pending) {
                        eprintln!("[image-serve] Session ended: {e}");
                    }
                });
            }
            Err(e) => eprintln!("[image-serve] Accept error: {e}"),
        }
    }
}

/// Exposes the shared pending-outbound slot to `main.rs`'s receive loop
/// (set when a `BigImage` event arrives or a file is dragged out) and the
/// file-server listener (read when Windows connects to pull it).
pub fn pending_outbound(shared: &ClipboardShared) -> Arc<Mutex<Option<Outbound>>> {
    shared.pending_outbound.clone()
}

pub fn peer_machine_id(shared: &ClipboardShared) -> Option<u32> {
    *shared.peer_machine_id.lock().unwrap()
}


#[cfg(test)]
mod push_tests {
    use super::*;
    use std::io::Write;

    /// Plays Windows' side of pushed transfers (Clipboard.cs's receive
    /// path) for `count` connections: handshake, header, body, then keeps
    /// reading until the sender closes its side, as Windows does, and only
    /// then closes. Returns each file's name and bytes.
    fn fake_windows(listener: TcpListener, key: String, count: usize) -> std::thread::JoinHandle<Vec<(String, Vec<u8>, u32)>> {
        std::thread::spawn(move || {
            let mut received = Vec::new();
            for _ in 0..count {
                let (mut s, _) = listener.accept().unwrap();
                let EncryptedStream { cipher, mut write_chain, mut read_chain } = open_encrypted_stream(&mut s, &key).unwrap();
                let hs = build_clipboard_handshake(PACKAGE_TYPE_CLIPBOARD_PUSH, 7, "windows");
                s.write_all(&cipher.encrypt(&mut write_chain, &hs)).unwrap();
                let mut peer_hs = [0u8; PACKAGE_SIZE_EX];
                s.read_exact(&mut peer_hs).unwrap();
                let peer_hs = cipher.decrypt(&mut read_chain, &peer_hs);
                assert_eq!(peer_hs[0], PACKAGE_TYPE_CLIPBOARD_PUSH);
                let post_action = unpack_u32_le(&peer_hs, 16);
                let mut header = [0u8; CLIPBOARD_FILE_HEADER_SIZE];
                s.read_exact(&mut header).unwrap();
                let header: [u8; CLIPBOARD_FILE_HEADER_SIZE] = cipher.decrypt(&mut read_chain, &header).try_into().unwrap();
                let (size, name) = parse_file_header(&header).unwrap();
                let mut body = Vec::new();
                read_pulled_body(&mut s, &cipher, &mut read_chain, size, &mut body).unwrap();
                // Windows reads on past the size until the sender closes.
                let mut rest = Vec::new();
                s.read_to_end(&mut rest).unwrap();
                received.push((name, body, post_action));
            }
            received
        })
    }

    #[test]
    fn several_files_are_pushed_one_connection_each_in_order() {
        let dir = std::env::temp_dir().join(format!("mwb-push-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A name starting with "image" would be taken for clipboard data
        // if it went bare (B12).
        let (a, b) = (dir.join("image001.txt"), dir.join("two.bin"));
        std::fs::write(&a, b"first file").unwrap();
        std::fs::write(&b, vec![7u8; 64]).unwrap(); // a multiple of 32: needs the full padding tail

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let key = "SharedKey0123456".to_string();
        let windows = fake_windows(listener, key.clone(), 2);

        let cfg: Config = serde_json::from_value(serde_json::json!({
            "security_key": key, "windows_ip": "unused", "machine_name": "mac", "machine_id": 3,
        }))
        .unwrap();
        push_items(&cfg, addr, POST_ACTION_DESKTOP, &outbound_items(&Outbound::Files(vec![a, b])));

        let received = windows.join().unwrap();
        assert_eq!(received, vec![
            ("/image001.txt".to_string(), b"first file".to_vec(), POST_ACTION_DESKTOP),
            ("/two.bin".to_string(), vec![7u8; 64], POST_ACTION_DESKTOP),
        ]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod file_copy_tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_new_file_copy_is_sent() {
        assert!(is_new_file_copy(&paths(&["/a.txt"]), &[], &[]));
        assert!(is_new_file_copy(&paths(&["/b.txt"]), &paths(&["/a.txt"]), &[]));
    }

    #[test]
    fn the_same_copy_seen_again_is_not_resent() {
        assert!(!is_new_file_copy(&paths(&["/a.txt"]), &paths(&["/a.txt"]), &[]));
    }

    #[test]
    fn a_file_just_received_from_windows_is_not_bounced_back() {
        let received = paths(&["/cache/received-files/a.txt"]);
        assert!(!is_new_file_copy(&received, &[], &received));
    }

    #[test]
    fn no_files_is_nothing_to_send() {
        assert!(!is_new_file_copy(&[], &paths(&["/a.txt"]), &[]));
    }
}
