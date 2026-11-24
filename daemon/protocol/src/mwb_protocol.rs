// Shared MWB (PowerToys v0.102+) protocol primitives (crypto, framing, packet
// building/parsing) — see PROTOCOL.md for the full spec and why each piece
// is the way it is. Extracted from the original mwb_probe.rs test client so
// the real daemon doesn't duplicate this.

use std::io::Write;

use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use flate2::write::DeflateEncoder;
use flate2::Compression;
use pbkdf2::pbkdf2_hmac;
use sha2::{Digest, Sha512};

pub const PACKAGE_SIZE: usize = 32;
pub const PACKAGE_SIZE_EX: usize = 64;

pub const PACKAGE_TYPE_HI: u8 = 2;
pub const PACKAGE_TYPE_HELLO: u8 = 3;
pub const PACKAGE_TYPE_BYEBYE: u8 = 4;
pub const PACKAGE_TYPE_HEARTBEAT: u8 = 20;
// Sent to every machine about once a minute while someone is using the
// sending PC, when its "Block screen saver on other machines" setting is on
// (Common.SendAwakeBeat). Real MWB treats it as user activity
// (HumanBeingDetected), so the screen of a machine nobody is touching
// directly stays on.
pub const PACKAGE_TYPE_AWAKE: u8 = 21;
pub const PACKAGE_TYPE_KEYBOARD: u8 = 122;
pub const PACKAGE_TYPE_MOUSE: u8 = 123;
pub const PACKAGE_TYPE_HANDSHAKE: u8 = 126;
pub const PACKAGE_TYPE_HANDSHAKE_ACK: u8 = 127;
// "You're now the active machine" — this is what actually gates real MWB's
// own file/big-clipboard auto-pull (Receiver.cs's MachineSwitched case),
// not the Clipboard beat itself. A plain 32-byte package (not in the
// is_big_package list), Des = the machine being told it's now active.
// Confirmed harmless to synthesize: its handler only checks timing against
// the last beat and re-triggers a pull — no state it touches is shared
// with Mouse/Keyboard forwarding. See PROTOCOL.md.
pub const PACKAGE_TYPE_MACHINE_SWITCHED: u8 = 77;
// Sent by a controlled machine in "Move mouse relatively" mode when its cursor
// is pushed off the edge facing the controller: asks the controller to switch
// to `WheelDelta` (the next machine's ID) with its cursor at X/Y (universal
// 0..65535). Plain 32-byte package. PowerToys Common.SendNextMachine /
// Receiver.cs's NextMachine case.
pub const PACKAGE_TYPE_NEXT_MACHINE: u8 = 121;
// Sent immediately before MachineSwitched in every *real* switch this repo
// has captured live from Windows (Ctrl+Alt+F1-style) — presumably just a
// cursor-visibility toggle for the switch UX, but included here to exactly
// mirror the real sequence rather than a synthesized subset of it.
pub const PACKAGE_TYPE_HIDE_MOUSE: u8 = 50;
pub const PACKAGE_TYPE_CLIPBOARD_DATA_END: u8 = 76;
pub const PACKAGE_TYPE_CLIPBOARD_TEXT: u8 = 124;
pub const PACKAGE_TYPE_CLIPBOARD_IMAGE: u8 = 125;
// "Clipboard" in the C# enum — the small "beat" broadcast over the message
// server announcing big data (here: a file) is available, and also the
// handshake package type used by whichever side of the *separate*
// port-15100 connection is pulling/receiving rather than pushing.
pub const PACKAGE_TYPE_CLIPBOARD: u8 = 69;
// The handshake package type used by whichever side of the port-15100
// connection is about to push/send the actual file bytes.
pub const PACKAGE_TYPE_CLIPBOARD_PUSH: u8 = 79;
// Sent by a machine that wants big-path data from us but has no outbound
// ("client") message socket to us, which is always the case for this
// bridge: Windows never connects back to it. Instead of connecting to our
// port 15100 to pull, it asks us to connect to *its* port 15100 and push
// (Clipboard.cs's ConnectAndGetData, Receiver.cs's ClipboardAsk case).
// A 64-byte package: PostAction at offset 16, the asker's name at 32..64.
pub const PACKAGE_TYPE_CLIPBOARD_ASK: u8 = 78;
// File drag-and-drop (DragDrop.cs), all plain 32-byte packages:
// "a machine has a file being dragged", broadcast to every machine.
pub const PACKAGE_TYPE_CLIPBOARD_DRAG_DROP: u8 = 70;
// "The drag went back without a drop", to the machine it was over.
pub const PACKAGE_TYPE_CLIPBOARD_DRAG_DROP_END: u8 = 71;
// "Is a file being dragged on your screen?", sent by Windows to the machine
// the cursor just left with the left button held. Src is the machine the
// cursor went to, i.e. where a drop would land.
pub const PACKAGE_TYPE_EXPLORER_DRAG_DROP: u8 = 72;
// "You're the drop target": pull the file when the button is released.
pub const PACKAGE_TYPE_CLIPBOARD_DRAG_DROP_OPERATION: u8 = 75;
// ClipboardPostAction: what the receiver of big-path data does with it.
// Other = put it on the clipboard; Desktop = a drag-and-drop, save it and
// show it (Windows: Desktop\MouseWithoutBorders\, opened in Explorer).
pub const POST_ACTION_OTHER: u32 = 0;
pub const POST_ACTION_DESKTOP: u32 = 1;
pub const ID_ALL: u32 = 255;

// Real MWB's own cap (MAX_CLIPBOARD_FILE_SIZE_CAN_BE_SENT), enforced by the
// sender before ever announcing a file.
pub const MAX_CLIPBOARD_FILE_SIZE: usize = 100 * 1024 * 1024;

// Real MWB's own small-path/big-path threshold
// (MAX_CLIPBOARD_DATA_SIZE_CAN_BE_SENT_INSTANTLY_TCP), applied uniformly to
// both text and image clipboard data — under this, push directly over the
// message-server connection; at or above it, announce a beat and let the
// peer pull it over the separate clipboard-server connection instead.
pub const MAX_SMALL_PATH_SIZE: usize = 1024 * 1024;

// The literal type-tag real MWB uses in the big-path file-transfer header's
// `name` field when the payload is a clipboard image rather than a real
// file — confirmed from source, not a filename to be sanitized/basenamed.
pub const BIG_PATH_IMAGE_NAME: &str = "image";

// The same for clipboard text too big for the small path: Windows sends
// `{size}*text` and a body in the small path's own format (raw-DEFLATE
// UTF-16 with TXT/RTF/HTM fragments, plus trailing zeros, since it sends
// its MemoryStream's whole buffer). SocketStuff.SendClipboardData.
pub const BIG_PATH_TEXT_NAME: &str = "text";

/// The header name for a file sent to Windows. Windows keeps only the
/// `Path.GetFileName` part, but before that treats any name starting with
/// "image" or "text" (in any case) as clipboard data (Clipboard.cs's
/// ReceiveAndProcessClipboardDataCore), so `image001.png` sent bare never
/// arrives as a file. Real MWB sends the full path, which never starts that
/// way; a leading `/` does the same and is gone after `GetFileName`.
pub fn file_header_name(file_name: &str) -> String {
    format!("/{file_name}")
}

/// Whether a copy holding both text and an image goes as the text. Real MWB
/// checks text first (FormHelper.cs), so a spreadsheet's cells arrive as
/// text rather than a picture of them. The exception is a lone URL next to
/// an image, which is how browsers copy an image: the image is the point.
pub fn text_wins_over_image(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let lone_url = !t.contains(char::is_whitespace)
        && ["http://", "https://", "file://"].iter().any(|p| t.len() > p.len() && t.get(..p.len()).is_some_and(|head| head.eq_ignore_ascii_case(p)));
    !lone_url
}

// Small-path clipboard chunking (Clipboard.cs's DATA_SIZE): a ClipboardText/
// ClipboardImage "big" (64-byte) package repurposes bytes 16-63 — normally
// Machine1-4 + MachineName — as one contiguous 48-byte raw-data region.
// Terminated by one empty ClipboardDataEnd package. Text payloads are
// "TXT" + text + SEP (+ optional "RTF"/"HTM" + content + SEP), UTF-16LE
// encoded, then raw-DEFLATE compressed (no zlib/gzip wrapper) before
// chunking — see PROTOCOL.md's clipboard section.
pub const CLIPBOARD_CHUNK_SIZE: usize = 48;
pub const CLIPBOARD_SEP: &str = "{4CFF57F7-BEDD-43d5-AE8F-27A61E886F2F}";

pub fn utf16le_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

// Per-connection encryption, PowerToys v0.102+ (Encryption.cs, #48742 and
// #49600): each direction starts with a cleartext header of a random salt
// and a random IV, and that direction's AES key is
// PBKDF2-HMAC-SHA512(security_key, salt, 100000 rounds).
const SALT_SIZE: usize = 16;
const KEY_DERIVATION_ITERATIONS: u32 = 100_000;

pub fn derive_key(security_key: &str, salt: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha512>(security_key.as_bytes(), salt, KEY_DERIVATION_ITERATIONS, &mut key);
    key
}

// A derivation takes about 30ms, and every connection needs two (one per
// direction), so both are cached. This side keeps one salt per security
// key for the life of the process; only the IV is new per connection, which
// is all CBC needs. The peer's derivations are cached by salt, which pays
// off with a peer that reuses its salt the same way (the companion does;
// Windows' MWB draws a new one per connection, so those never repeat).
type KeyCache = std::sync::Mutex<std::collections::HashMap<(String, [u8; SALT_SIZE]), [u8; 32]>>;
type OwnSalts = std::sync::Mutex<Vec<(String, [u8; SALT_SIZE], [u8; 32])>>;
static OWN_SALTS: OwnSalts = std::sync::Mutex::new(Vec::new());
static PEER_KEYS: std::sync::OnceLock<KeyCache> = std::sync::OnceLock::new();
const PEER_KEYS_KEPT: usize = 32;

/// This side's salt for `security_key`, and the key derived from it.
fn own_salt_and_key(security_key: &str) -> ([u8; SALT_SIZE], [u8; 32]) {
    use rand::RngExt;
    let mut own = OWN_SALTS.lock().unwrap();
    if let Some((_, salt, key)) = own.iter().find(|(k, _, _)| k == security_key) {
        return (*salt, *key);
    }
    let mut salt = [0u8; SALT_SIZE];
    rand::rng().fill(&mut salt);
    let key = derive_key(security_key, &salt);
    own.push((security_key.to_string(), salt, key));
    (salt, key)
}

/// The key for the peer's `salt`, derived once per salt.
fn peer_key(security_key: &str, salt: &[u8; SALT_SIZE]) -> [u8; 32] {
    let cache = PEER_KEYS.get_or_init(Default::default);
    let id = (security_key.to_string(), *salt);
    if let Some(key) = cache.lock().unwrap().get(&id) {
        return *key;
    }
    // Derived without holding the lock, so other connections don't wait.
    let key = derive_key(security_key, salt);
    let mut cache = cache.lock().unwrap();
    if cache.len() >= PEER_KEYS_KEPT {
        cache.clear();
    }
    cache.insert(id, key);
    key
}

/// An encrypted connection's cipher plus each direction's CBC chaining value.
pub struct EncryptedStream {
    pub cipher: CbcState,
    pub write_chain: [u8; 16],
    pub read_chain: [u8; 16],
}

/// Sets up encryption on a freshly connected MWB socket (message server and
/// clipboard server alike), in either role: sends our cleartext salt+IV
/// header and the encrypted 16-byte priming block, then reads the peer's
/// header and priming block. We always write before reading, so it works
/// whichever order the peer does them in.
pub fn open_encrypted_stream<S: std::io::Read + Write>(stream: &mut S, security_key: &str) -> std::io::Result<EncryptedStream> {
    use rand::RngExt;
    let (salt, encrypt_key) = own_salt_and_key(security_key);
    let mut header = [0u8; SALT_SIZE + 16];
    header[..SALT_SIZE].copy_from_slice(&salt);
    rand::rng().fill(&mut header[SALT_SIZE..]);
    stream.write_all(&header)?;
    let mut write_chain: [u8; 16] = header[SALT_SIZE..].try_into().unwrap();

    let mut cipher = CbcState::new(&encrypt_key);
    let mut priming = [0u8; 16];
    rand::rng().fill(&mut priming);
    stream.write_all(&cipher.encrypt(&mut write_chain, &priming))?;

    let mut peer_header = [0u8; SALT_SIZE + 16];
    stream.read_exact(&mut peer_header)?;
    cipher.set_decrypt_key(&peer_key(security_key, peer_header[..SALT_SIZE].try_into().unwrap()));
    let mut read_chain: [u8; 16] = peer_header[SALT_SIZE..].try_into().unwrap();

    let mut peer_priming = [0u8; 16];
    stream.read_exact(&mut peer_priming)?;
    let _ = cipher.decrypt(&mut read_chain, &peer_priming);
    Ok(EncryptedStream { cipher, write_chain, read_chain })
}

/// Encryption.Get24BitHash: SHA-512 the (byte-truncated) key string once,
/// then re-hash the digest 50000 more times, then pack specific bytes of
/// the final digest into a u32. Verified byte-for-byte against a real
/// PowerToys debug dump's logged magicNumber value.
pub fn get_24bit_hash(key: &str) -> u32 {
    // 50,000 SHA-512 rounds, for a value that never changes per key.
    static HASHES: std::sync::Mutex<Vec<(String, u32)>> = std::sync::Mutex::new(Vec::new());
    if let Some((_, hash)) = HASHES.lock().unwrap().iter().find(|(k, _)| k == key) {
        return *hash;
    }
    let hash = compute_24bit_hash(key);
    HASHES.lock().unwrap().push((key.to_string(), hash));
    hash
}

fn compute_24bit_hash(key: &str) -> u32 {
    let mut bytes = [0u8; PACKAGE_SIZE];
    for (i, ch) in key.chars().take(PACKAGE_SIZE).enumerate() {
        bytes[i] = ch as u8;
    }
    let mut hash = Sha512::digest(bytes);
    for _ in 0..50_000 {
        hash = Sha512::digest(hash);
    }
    ((hash[0] as u32) << 23) + ((hash[1] as u32) << 16) + ((hash[63] as u32) << 8) + (hash[2] as u32)
}

/// AES-256-CBC over a continuous stream (chaining persists across calls,
/// not reset per packet — mirrors .NET's single long-lived CryptoStream per
/// connection direction).
#[derive(Clone)]
pub struct CbcState {
    aes_encrypt: aes::Aes256,
    aes_decrypt: aes::Aes256,
}

impl CbcState {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            aes_encrypt: aes::Aes256::new(key.into()),
            aes_decrypt: aes::Aes256::new(key.into()),
        }
    }

    /// Each direction has its own key since PowerToys v0.102 (see
    /// `open_encrypted_stream`).
    pub fn set_decrypt_key(&mut self, key: &[u8; 32]) {
        self.aes_decrypt = aes::Aes256::new(key.into());
    }

    pub fn encrypt(&self, chain: &mut [u8; 16], plaintext: &[u8]) -> Vec<u8> {
        let mut ciphertext = vec![0u8; plaintext.len()];
        for (chunk, out) in plaintext.chunks_exact(16).zip(ciphertext.chunks_exact_mut(16)) {
            let mut block: aes::Block = [0u8; 16].into();
            for j in 0..16 {
                block[j] = chunk[j] ^ chain[j];
            }
            self.aes_encrypt.encrypt_block(&mut block);
            out.copy_from_slice(&block);
            chain.copy_from_slice(&block);
        }
        ciphertext
    }

    pub fn decrypt(&self, chain: &mut [u8; 16], ciphertext: &[u8]) -> Vec<u8> {
        let mut plaintext = vec![0u8; ciphertext.len()];
        for (chunk, out) in ciphertext.chunks_exact(16).zip(plaintext.chunks_exact_mut(16)) {
            let mut block: aes::Block = chunk.try_into().unwrap();
            self.aes_decrypt.decrypt_block(&mut block);
            for j in 0..16 {
                out[j] = block[j] ^ chain[j];
            }
            chain.copy_from_slice(chunk);
        }
        plaintext
    }
}

pub fn pack_u32_le(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn unpack_u32_le(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap())
}

/// See PROTOCOL.md's "IsBigPackage" list — these get a second, unvalidated
/// 32-byte read appended (the MachineName field).
pub fn is_big_package(t: u8) -> bool {
    matches!(t, 3 | 21 | 20 | 51 | 126 | 127 | 79 | 69 | 78 | 125 | 124 | 76) || (t & 0x80) == 0x80
}

/// Builds a 64-byte Handshake packet (Type=126). `src_id` must be a real,
/// non-zero, non-255 machine ID — see PROTOCOL.md's "Src must never be
/// ID.NONE" gotcha.
pub fn build_handshake(id: u32, src_id: u32, machine1_4: [u32; 4], machine_name: &str) -> [u8; PACKAGE_SIZE_EX] {
    let mut buf = [0u8; PACKAGE_SIZE_EX];
    buf[0] = PACKAGE_TYPE_HANDSHAKE;
    pack_u32_le(&mut buf, 4, id);
    pack_u32_le(&mut buf, 8, src_id);
    pack_u32_le(&mut buf, 12, 0); // Des
    for (i, v) in machine1_4.iter().enumerate() {
        pack_u32_le(&mut buf, 16 + i * 4, *v);
    }
    let name_padded = format!("{:<32}", machine_name);
    buf[32..64].copy_from_slice(&name_padded.as_bytes()[..32]);
    buf
}

/// Builds a HandshakeAck (Type=127) replying to an incoming Handshake:
/// echoes the sender's Machine1-4 bit-flipped, per the challenge-response.
pub fn build_handshake_ack(incoming: &[u8], src_id: u32, our_name: &str) -> [u8; PACKAGE_SIZE_EX] {
    let mut ack = [0u8; PACKAGE_SIZE_EX];
    ack.copy_from_slice(&incoming[..PACKAGE_SIZE_EX.min(incoming.len())]);
    ack[0] = PACKAGE_TYPE_HANDSHAKE_ACK;
    pack_u32_le(&mut ack, 8, src_id);
    for i in 0..4 {
        let v = unpack_u32_le(incoming, 16 + i * 4);
        pack_u32_le(&mut ack, 16 + i * 4, !v);
    }
    let name_padded = format!("{:<32}", our_name);
    ack[32..64].copy_from_slice(&name_padded.as_bytes()[..32]);
    ack
}

/// Builds the outgoing ClipboardText chunk sequence (+ trailing
/// ClipboardDataEnd) for Omarchy -> Windows sync: `"TXT" + text + SEP`,
/// UTF-16LE-encoded, raw-DEFLATE-compressed (no zlib/gzip wrapper — matches
/// what real MWB expects to receive), then split into `CLIPBOARD_CHUNK_SIZE`
/// pieces. `next_id` is the caller's running per-connection id counter,
/// incremented once per package built (real MWB's Id field is "used for
/// dedup on the receive side" — a repeated id across packages risks one
/// being silently dropped as a duplicate, so every package here needs its
/// own distinct value, not just a shared placeholder like the 32-byte
/// types use). Caller still owes each returned package a
/// `finalize_send_buf` call before sending — this only fills in the type/
/// id/src/des/data fields.
pub fn build_clipboard_text_packages(next_id: &mut u32, src_id: u32, text: &str) -> Vec<[u8; PACKAGE_SIZE_EX]> {
    let tagged = format!("TXT{text}{CLIPBOARD_SEP}");
    let utf16 = utf16le_bytes(&tagged);

    let mut compressed = Vec::new();
    {
        let mut enc = DeflateEncoder::new(&mut compressed, Compression::default());
        enc.write_all(&utf16).expect("compressing into a Vec<u8> cannot fail");
    }

    build_clipboard_chunk_packages(next_id, src_id, PACKAGE_TYPE_CLIPBOARD_TEXT, &compressed)
}

/// Builds the outgoing ClipboardImage chunk sequence (+ trailing
/// ClipboardDataEnd) for small-path image sync (payload under 1MB — see
/// PROTOCOL.md for the size threshold and the big-path fallback for larger
/// images). Unlike text, real MWB applies **no compression and no
/// text-format tagging** to image data at all — the wire payload is simply
/// the clipboard image's own PNG bytes, verbatim (confirmed from source:
/// `image.Save(stream, ImageFormat.Png)` on the sending side, `Image
/// .FromStream` on receipt — a plain PNG file both ways, nothing custom).
pub fn build_clipboard_image_packages(next_id: &mut u32, src_id: u32, png_bytes: &[u8]) -> Vec<[u8; PACKAGE_SIZE_EX]> {
    build_clipboard_chunk_packages(next_id, src_id, PACKAGE_TYPE_CLIPBOARD_IMAGE, png_bytes)
}

fn build_clipboard_chunk_packages(next_id: &mut u32, src_id: u32, package_type: u8, payload: &[u8]) -> Vec<[u8; PACKAGE_SIZE_EX]> {
    let mut packages = Vec::new();
    let mut build_header = |buf: &mut [u8; PACKAGE_SIZE_EX], pt: u8| {
        buf[0] = pt;
        pack_u32_le(buf, 4, *next_id);
        pack_u32_le(buf, 8, src_id);
        pack_u32_le(buf, 12, ID_ALL);
        *next_id += 1;
    };

    if payload.is_empty() {
        // An empty payload still needs at least one chunk package so the
        // receiver has something to accumulate before DataEnd.
        let mut buf = [0u8; PACKAGE_SIZE_EX];
        build_header(&mut buf, package_type);
        packages.push(buf);
    }
    for chunk in payload.chunks(CLIPBOARD_CHUNK_SIZE) {
        let mut buf = [0u8; PACKAGE_SIZE_EX];
        build_header(&mut buf, package_type);
        buf[16..16 + chunk.len()].copy_from_slice(chunk);
        packages.push(buf);
    }
    let mut end = [0u8; PACKAGE_SIZE_EX];
    build_header(&mut end, PACKAGE_TYPE_CLIPBOARD_DATA_END);
    packages.push(end);

    packages
}

/// Builds the "Clipboard" beat package (Type=69) broadcast over the
/// message-server connection to announce that a file is available to pull
/// — mirrors real MWB's `SendClipboardBeat`. Real MWB only auto-pulls this
/// around its own "machine switched" event (this bridge's fixed two-machine
/// topology has no equivalent concept), so this repo's receiver treats any
/// beat as "pull immediately" instead — a deliberate simplification, see
/// PROTOCOL.md.
pub fn build_clipboard_beat(id: u32, src_id: u32) -> [u8; PACKAGE_SIZE_EX] {
    let mut buf = [0u8; PACKAGE_SIZE_EX];
    buf[0] = PACKAGE_TYPE_CLIPBOARD;
    pack_u32_le(&mut buf, 4, id);
    pack_u32_le(&mut buf, 8, src_id);
    pack_u32_le(&mut buf, 12, ID_ALL);
    buf
}

/// Builds a Heartbeat (Type=20) carrying this machine's name, broadcast the
/// way real MWB machines send them: Windows adds a machine it doesn't know
/// yet to its pool on one (Receiver.cs, `MachineStuff.AddToMachinePool`).
/// Refused (with the reason) for a name Windows wouldn't read back as
/// itself (see `registrable_name`): it would then take this for a new
/// machine and rewrite the layout around a garbled name.
pub fn build_heartbeat(id: u32, src_id: u32, machine_name: &str) -> Result<[u8; PACKAGE_SIZE_EX], String> {
    let name = registrable_name(machine_name)?;
    let mut buf = [0u8; PACKAGE_SIZE_EX];
    buf[0] = PACKAGE_TYPE_HEARTBEAT;
    pack_u32_le(&mut buf, 4, id);
    pack_u32_le(&mut buf, 8, src_id);
    pack_u32_le(&mut buf, 12, ID_ALL);
    // As Windows writes it: ASCII, space-padded to 32 bytes.
    buf[32..64].copy_from_slice(format!("{name:<32}").as_bytes());
    Ok(buf)
}

/// `machine_name` trimmed, if Windows would read it back from a package's
/// 32-byte ASCII name field (`DATA.MachineName`: one byte per character,
/// `?` for anything not ASCII, trimmed) as exactly that, and could store it
/// in its pool and layout strings (`name:id` pairs and `,`-joined names).
/// Otherwise why not.
pub fn registrable_name(machine_name: &str) -> Result<&str, String> {
    let name = machine_name.trim();
    if name.is_empty() {
        return Err("the machine name is empty".into());
    }
    if let Some(c) = name.chars().find(|c| !c.is_ascii() || c.is_ascii_control()) {
        return Err(format!("the machine name has {c:?} in it, which Windows reads as '?' (use plain ASCII letters, digits and dashes)"));
    }
    if name.len() > 32 {
        return Err(format!("the machine name is {} characters long, and Windows keeps only 32", name.len()));
    }
    if name.contains([',', ':']) {
        return Err("the machine name has a comma or colon in it, which PowerToys' settings can't store".into());
    }
    Ok(name)
}

/// Builds the "MachineSwitched" package (Type=77) that actually triggers
/// real MWB's file/big-clipboard auto-pull — a plain 32-byte package
/// (unlike the beat above, `Des` here is the specific machine being told
/// it's now active, not a broadcast). Must arrive within 30s of a
/// `Clipboard` beat to have any effect (`BIG_CLIPBOARD_DATA_TIMEOUT`) —
/// send it right after the beat, same as this repo already does.
pub fn build_machine_switched(id: u32, src_id: u32, des_id: u32) -> [u8; PACKAGE_SIZE] {
    let mut buf = [0u8; PACKAGE_SIZE];
    buf[0] = PACKAGE_TYPE_MACHINE_SWITCHED;
    pack_u32_le(&mut buf, 4, id);
    pack_u32_le(&mut buf, 8, src_id);
    pack_u32_le(&mut buf, 12, des_id);
    buf
}

/// Starting value for a connection's outgoing package ids. MWB drops any
/// package whose id matches one of the last 50 it processed, across
/// reconnects (real MWB even persists its counter in settings), so a fixed
/// start like 1000 made every reconnect's first NextMachine a silent
/// duplicate. Seeded from the clock instead: it advances far faster than we
/// send packages, so a new connection never reuses a recent id.
pub fn initial_package_id() -> u32 {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // Positive i32 range (MWB reads ids as int) and well above the handshake's id=1.
    (ms % 0x3FFF_0000) as u32 + 0x1_0000
}

/// Builds a NextMachine package (Type=121) — see its constant doc comment.
/// `des_id` is the controller we're asking, `next_machine` the machine to
/// switch to (the controller itself, when handing control back).
pub fn build_next_machine(id: u32, src_id: u32, des_id: u32, x: u32, y: u32, next_machine: u32) -> [u8; PACKAGE_SIZE] {
    let mut buf = [0u8; PACKAGE_SIZE];
    buf[0] = PACKAGE_TYPE_NEXT_MACHINE;
    pack_u32_le(&mut buf, 4, id);
    pack_u32_le(&mut buf, 8, src_id);
    pack_u32_le(&mut buf, 12, des_id);
    pack_u32_le(&mut buf, 16, x);
    pack_u32_le(&mut buf, 20, y);
    pack_u32_le(&mut buf, 24, next_machine);
    buf
}

/// Builds a HideMouse package (Type=50) — see its constant doc comment.
pub fn build_hide_mouse(id: u32, src_id: u32, des_id: u32) -> [u8; PACKAGE_SIZE] {
    let mut buf = [0u8; PACKAGE_SIZE];
    buf[0] = PACKAGE_TYPE_HIDE_MOUSE;
    pack_u32_le(&mut buf, 4, id);
    pack_u32_le(&mut buf, 8, src_id);
    pack_u32_le(&mut buf, 12, des_id);
    buf
}

/// Builds the 64-byte handshake package used on the *separate* clipboard-
/// server connection (port 15100/`BASE_PORT`) that actual file bytes travel
/// over — distinct from the message-server's Handshake/HandshakeAck pair.
/// `package_type` is `PACKAGE_TYPE_CLIPBOARD` (this side is pulling/
/// receiving) or `PACKAGE_TYPE_CLIPBOARD_PUSH` (this side is about to push/
/// send the file). Real MWB leaves Des/Machine1-4 at zero here and only
/// populates Src + MachineName — matched exactly.
pub fn build_clipboard_handshake(package_type: u8, src_id: u32, machine_name: &str) -> [u8; PACKAGE_SIZE_EX] {
    build_clipboard_handshake_with_action(package_type, POST_ACTION_OTHER, src_id, machine_name)
}

/// `build_clipboard_handshake` with a `PostAction` (offset 16), which tells
/// the receiving side what to do with the data — see `POST_ACTION_*`.
pub fn build_clipboard_handshake_with_action(package_type: u8, post_action: u32, src_id: u32, machine_name: &str) -> [u8; PACKAGE_SIZE_EX] {
    let mut buf = [0u8; PACKAGE_SIZE_EX];
    buf[0] = package_type;
    pack_u32_le(&mut buf, 8, src_id);
    pack_u32_le(&mut buf, 16, post_action);
    let name_padded = format!("{:<32}", machine_name);
    buf[32..64].copy_from_slice(&name_padded.as_bytes()[..32]);
    buf
}

/// Builds one of the plain 32-byte drag-and-drop packages
/// (`PACKAGE_TYPE_CLIPBOARD_DRAG_DROP*`), which carry nothing but
/// Type/Id/Src/Des — Common.SendPackage on real MWB.
pub fn build_drag_drop_package(package_type: u8, id: u32, src_id: u32, des_id: u32) -> [u8; PACKAGE_SIZE] {
    let mut buf = [0u8; PACKAGE_SIZE];
    buf[0] = package_type;
    pack_u32_le(&mut buf, 4, id);
    pack_u32_le(&mut buf, 8, src_id);
    pack_u32_le(&mut buf, 12, des_id);
    buf
}

/// Zero padding real MWB appends after a pushed file's bytes
/// (SocketStuff.SendFileEx): always 1..=32 bytes, a full 32 when the size
/// is already a multiple. The receiver reads until it has `size` bytes, so
/// this tail is what lets it stop without waiting for the socket to close.
pub fn push_padding_len(size: usize) -> usize {
    PACKAGE_SIZE - size % PACKAGE_SIZE
}

pub const CLIPBOARD_FILE_HEADER_SIZE: usize = 1024;

/// Sends a `size`-byte body read from `body`, plus the 1..=32 zero bytes
/// real MWB always adds (`push_padding_len`: Windows stops reading once it
/// has the size, and a body ending exactly there would leave it waiting).
/// Streams it a megabyte at a time, so a big file never sits in memory
/// whole and starts going out at once. Fails if `body` runs out early.
pub fn write_padded_body(stream: &mut impl Write, cipher: &CbcState, chain: &mut [u8; 16], body: &mut impl std::io::Read, size: usize) -> std::io::Result<()> {
    const CHUNK: usize = 1 << 20; // a multiple of 32, like every total
    let total = size + push_padding_len(size);
    let mut buf = vec![0u8; CHUNK.min(total)];
    let mut sent = 0;
    while sent < total {
        let n = (total - sent).min(buf.len());
        let real = size.saturating_sub(sent).min(n);
        body.read_exact(&mut buf[..real])?;
        buf[real..n].fill(0);
        stream.write_all(&cipher.encrypt(chain, &buf[..n]))?;
        sent += n;
    }
    Ok(())
}

/// Reads a pulled body of `size` bytes from Windows' clipboard-server
/// connection, decrypting as it goes, and writes those bytes to `out`.
///
/// Windows pads a file with 1..=32 zero bytes (`push_padding_len`), but
/// clipboard text and images with `size % 32` (SocketStuff.SendData), and
/// its CryptoStream never sends a final partial block. So read up to the
/// next multiple of 32, accept the connection closing early once `size`
/// bytes are in, and afterwards read off what's left (a file's 32-byte tail
/// when its size is a multiple of 32) so closing doesn't reset the
/// connection under Windows.
pub fn read_pulled_body(
    stream: &mut std::net::TcpStream,
    cipher: &CbcState,
    chain: &mut [u8; 16],
    size: usize,
    out: &mut impl Write,
) -> std::io::Result<()> {
    use std::io::Read;
    const CHUNK: usize = 1 << 20;
    let padded = size.div_ceil(PACKAGE_SIZE) * PACKAGE_SIZE;
    let mut buf = vec![0u8; padded.clamp(PACKAGE_SIZE, CHUNK)];
    let (mut pending, mut read, mut written) = (0usize, 0usize, 0usize);
    while read < padded {
        let want = (padded - read).min(buf.len() - pending);
        let n = match stream.read(&mut buf[pending..pending + want]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        read += n;
        pending += n;
        let whole = pending / 16 * 16;
        if whole > 0 {
            let plain = cipher.decrypt(chain, &buf[..whole]);
            let real = (size - written).min(plain.len());
            out.write_all(&plain[..real])?;
            written += real;
            buf.copy_within(whole..pending, 0);
            pending -= whole;
        }
    }
    if written < size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("the connection closed after {written} of {size} bytes"),
        ));
    }
    // Windows sends the tail together with the body and closes right
    // after, so this is quick; the limits only guard against a peer that
    // keeps the connection open.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(250)));
    let mut sink = [0u8; 4096];
    let mut left = 64 * 1024usize;
    while left > 0 {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => left = left.saturating_sub(n),
        }
    }
    Ok(())
}

/// Builds the 1024-byte "{size}*{name}" header sent (through the same
/// continuing encrypted stream, right before the file's raw bytes) on the
/// clipboard-server connection. UTF-16LE-encoded, **null**-padded to fill
/// the buffer (not space-padded, unlike the small-path MachineName field) —
/// matches real MWB's framing exactly.
pub fn build_file_header(size: usize, name: &str) -> [u8; CLIPBOARD_FILE_HEADER_SIZE] {
    let bytes = utf16le_bytes(&format!("{size}*{name}"));
    let mut buf = [0u8; CLIPBOARD_FILE_HEADER_SIZE];
    let n = bytes.len().min(CLIPBOARD_FILE_HEADER_SIZE);
    buf[..n].copy_from_slice(&bytes[..n]);
    buf
}

/// Parses a file-transfer header back into `(size, name)`. Returns `None`
/// if there's no `*`-separated numeric size prefix at all; callers should
/// *also* treat a successfully-parsed `size == 0` as "no real file" — real
/// MWB stuffs an English error message into this same header shape on
/// failure (e.g. `"0*<path> - Folder is not supported, zip it first!"`)
/// rather than using a distinct error package type.
pub fn parse_file_header(buf: &[u8; CLIPBOARD_FILE_HEADER_SIZE]) -> Option<(usize, String)> {
    let utf16: Vec<u16> = buf.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
    let end = utf16.iter().position(|&u| u == 0).unwrap_or(utf16.len());
    let s = String::from_utf16_lossy(&utf16[..end]);
    let (size_str, name) = s.split_once('*')?;
    Some((size_str.parse().ok()?, name.to_string()))
}

/// SocketStuff.TcpSendData's checksum+magic packing, applied to the first
/// 32 bytes of `buf` regardless of whether it's a 32- or 64-byte package.
pub fn finalize_send_buf(buf: &mut [u8], magic_number: u32) {
    buf[3] = ((magic_number >> 24) & 0xFF) as u8;
    buf[2] = ((magic_number >> 16) & 0xFF) as u8;
    let mut checksum: u8 = 0;
    for &b in &buf[2..PACKAGE_SIZE] {
        checksum = checksum.wrapping_add(b);
    }
    buf[1] = checksum;
}

/// Finalizes and encrypts `packages` into one buffer, so a whole clipboard
/// transfer goes out in one write rather than thousands of 64-byte ones
/// (a 900KB image is about 19,000 packages).
pub fn encrypt_packages(cipher: &CbcState, chain: &mut [u8; 16], magic_number: u32, packages: Vec<[u8; PACKAGE_SIZE_EX]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(packages.len() * PACKAGE_SIZE_EX);
    for mut package in packages {
        finalize_send_buf(&mut package, magic_number);
        out.extend_from_slice(&cipher.encrypt(chain, &package));
    }
    out
}

/// SocketStuff.ProcessReceivedDataEx's validation: checks the magic/checksum
/// over the first 32 bytes, then zeroes bytes 1-3 so the caller reads a
/// clean PackageType out of buf[0]. Returns whether it validated.
pub fn validate_and_clean_recv_buf(buf: &mut [u8], magic_number: u32) -> bool {
    let magic = ((buf[3] as u32) << 24) + ((buf[2] as u32) << 16);
    let mut checksum: u8 = 0;
    for &b in &buf[2..PACKAGE_SIZE] {
        checksum = checksum.wrapping_add(b);
    }
    let ok = magic == (magic_number & 0xFFFF0000) && buf[1] == checksum;
    buf[1] = 0;
    buf[2] = 0;
    buf[3] = 0;
    ok
}

#[cfg(test)]
mod package_tests {
    use super::*;

    #[test]
    fn clipboard_handshake_carries_the_post_action_at_offset_16() {
        let hs = build_clipboard_handshake_with_action(PACKAGE_TYPE_CLIPBOARD_PUSH, POST_ACTION_DESKTOP, 2, "mac");
        assert_eq!(hs[0], PACKAGE_TYPE_CLIPBOARD_PUSH);
        assert_eq!(unpack_u32_le(&hs, 8), 2);
        assert_eq!(unpack_u32_le(&hs, 16), POST_ACTION_DESKTOP);
        assert_eq!(&hs[32..35], b"mac");
        // The old builder is unchanged: PostAction Other (0).
        let plain = build_clipboard_handshake(PACKAGE_TYPE_CLIPBOARD, 2, "mac");
        assert_eq!(unpack_u32_le(&plain, 16), POST_ACTION_OTHER);
    }

    #[test]
    fn drag_drop_packages_are_small_and_addressed() {
        let p = build_drag_drop_package(PACKAGE_TYPE_CLIPBOARD_DRAG_DROP_OPERATION, 42, 2, 7);
        assert_eq!(p.len(), PACKAGE_SIZE);
        assert_eq!(p[0], 75);
        assert_eq!(unpack_u32_le(&p, 4), 42);
        assert_eq!(unpack_u32_le(&p, 8), 2);
        assert_eq!(unpack_u32_le(&p, 12), 7);
        for t in [70, 71, 72, 75] {
            assert!(!is_big_package(t), "type {t} is a 32-byte package on real MWB");
        }
        assert!(is_big_package(PACKAGE_TYPE_CLIPBOARD_ASK));
    }

    #[test]
    fn a_heartbeat_names_this_machine_to_everyone() {
        let hb = build_heartbeat(5, 3, " alan-mac ").unwrap();
        assert_eq!(hb[0], PACKAGE_TYPE_HEARTBEAT);
        assert_eq!(unpack_u32_le(&hb, 8), 3);
        assert_eq!(unpack_u32_le(&hb, 12), ID_ALL);
        assert_eq!(&hb[32..64], format!("{:<32}", "alan-mac").as_bytes());
        assert!(is_big_package(PACKAGE_TYPE_HEARTBEAT));
        assert!(build_heartbeat(5, 3, &"x".repeat(32)).is_ok());
    }

    #[test]
    fn names_windows_would_misread_are_never_announced() {
        // Read back by Windows as something else, or not storable.
        for name in ["", "   ", "Alan\u{2019}s Mac", "caf\u{e9}", &"x".repeat(33), "a,b", "pc:2", "tab\there"] {
            assert!(build_heartbeat(5, 3, name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn push_padding_matches_real_mwb() {
        assert_eq!(push_padding_len(1), 31);
        assert_eq!(push_padding_len(31), 1);
        assert_eq!(push_padding_len(32), 32);
        assert_eq!(push_padding_len(0), 32);
    }
}

#[cfg(test)]
mod transfer_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn a_file_name_never_looks_like_clipboard_data() {
        for name in ["image001.png", "Text notes.txt", "TEXT", "plain.txt"] {
            let header = file_header_name(name);
            assert!(!header.to_lowercase().starts_with("image") && !header.to_lowercase().starts_with("text"), "{header}");
            // Windows' Path.GetFileName splits on both separators.
            assert_eq!(header.rsplit(['/', '\\']).next().unwrap(), name);
        }
    }

    #[test]
    fn text_wins_unless_it_is_a_lone_url() {
        assert!(text_wins_over_image("A1\tB1\nA2\tB2"));
        assert!(text_wins_over_image("see https://example.com"));
        assert!(!text_wins_over_image("https://example.com/cat.png"));
        assert!(!text_wins_over_image("  HTTP://example.com\n"));
        assert!(!text_wins_over_image(""));
        assert!(!text_wins_over_image("   "));
        assert!(text_wins_over_image("https://"));
        assert!(text_wins_over_image("čšžčšžčš and more"));
    }

    /// Sends `body` the way Windows does: encrypted in whole blocks only (a
    /// final partial block is never sent), then closes.
    fn windows_sends(cipher: CbcState, chain: [u8; 16], body: Vec<u8>) -> (TcpStream, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let sender = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut chain = chain;
            let whole = body.len() / 16 * 16;
            s.write_all(&cipher.encrypt(&mut chain, &body[..whole])).unwrap();
        });
        (TcpStream::connect(addr).unwrap(), sender)
    }

    fn pull(sent: Vec<u8>, size: usize) -> std::io::Result<Vec<u8>> {
        let cipher = CbcState::new(&[9u8; 32]);
        let (mut stream, sender) = windows_sends(CbcState::new(&[9u8; 32]), [1u8; 16], sent);
        let mut chain = [1u8; 16];
        let mut out = Vec::new();
        let result = read_pulled_body(&mut stream, &cipher, &mut chain, size, &mut out);
        sender.join().unwrap();
        result.map(|()| out)
    }

    #[test]
    fn a_file_with_its_padding_tail_is_read_whole() {
        for size in [1usize, 31, 32, 33, 64, 100] {
            let data: Vec<u8> = (0..size).map(|i| i as u8).collect();
            let mut sent = data.clone();
            sent.resize(size + push_padding_len(size), 0);
            assert_eq!(pull(sent, size).unwrap(), data, "size {size}");
        }
    }

    #[test]
    fn clipboard_data_padded_by_size_mod_32_is_read_whole() {
        // SendData pads by size % 32, and only whole blocks reach the wire:
        // for 8..=15 extra bytes the body is all there but short of 32.
        for size in [64usize, 72, 79] {
            let data: Vec<u8> = (0..size).map(|i| (i * 7) as u8).collect();
            let mut sent = data.clone();
            sent.resize(size + size % 32, 0);
            assert_eq!(pull(sent, size).unwrap(), data, "size {size}");
        }
    }

    #[test]
    fn a_body_cut_short_is_an_error() {
        let size = 70; // 70 % 32 = 6: the last 6 bytes never leave Windows
        let mut sent = vec![5u8; size];
        sent.resize(size + size % 32, 0);
        let err = pull(sent, size).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_streamed_body_round_trips_with_its_padding() {
        for size in [0usize, 1, 32, 1000, (1 << 20) + 5] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let cipher = CbcState::new(&[9u8; 32]);
            let mut chain = [1u8; 16];
            let mut wire = Vec::new();
            write_padded_body(&mut wire, &cipher, &mut chain, &mut data.as_slice(), size).unwrap();
            assert_eq!(wire.len(), size + push_padding_len(size), "size {size}");
            let mut chain = [1u8; 16];
            let plain = cipher.decrypt(&mut chain, &wire);
            assert_eq!(&plain[..size], &data[..]);
            assert!(plain[size..].iter().all(|&b| b == 0));
        }
        // A body shorter than promised is an error, not a short send.
        let cipher = CbcState::new(&[9u8; 32]);
        assert!(write_padded_body(&mut Vec::new(), &cipher, &mut [0u8; 16], &mut [1u8; 10].as_slice(), 20).is_err());
    }

    #[test]
    fn the_tail_is_read_off_before_closing() {
        let cipher = CbcState::new(&[9u8; 32]);
        let mut sent = vec![3u8; 64];
        sent.resize(64 + push_padding_len(64), 0);
        let (mut stream, sender) = windows_sends(CbcState::new(&[9u8; 32]), [1u8; 16], sent);
        let mut chain = [1u8; 16];
        read_pulled_body(&mut stream, &cipher, &mut chain, 64, &mut Vec::new()).unwrap();
        sender.join().unwrap();
        let mut rest = Vec::new();
        assert_eq!(stream.read_to_end(&mut rest).unwrap(), 0);
    }
}

#[cfg(test)]
mod encryption_tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    /// Both ends run the v0.102+ setup at once (each writes before reading,
    /// like Windows may), then a package goes each way and decrypts.
    #[test]
    fn open_encrypted_stream_round_trips_both_directions() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let peer = std::thread::spawn(move || {
            let mut s = open_encrypted_stream(&mut b, "SharedKey0123456").unwrap();
            let mut ct = [0u8; PACKAGE_SIZE];
            b.read_exact(&mut ct).unwrap();
            let pt = s.cipher.decrypt(&mut s.read_chain, &ct);
            b.write_all(&s.cipher.encrypt(&mut s.write_chain, &pt)).unwrap();
        });
        let mut s = open_encrypted_stream(&mut a, "SharedKey0123456").unwrap();
        let msg: Vec<u8> = (0..PACKAGE_SIZE as u8).collect();
        a.write_all(&s.cipher.encrypt(&mut s.write_chain, &msg)).unwrap();
        let mut ct = [0u8; PACKAGE_SIZE];
        a.read_exact(&mut ct).unwrap();
        assert_eq!(s.cipher.decrypt(&mut s.read_chain, &ct), msg);
        peer.join().unwrap();
    }

    /// A peer that has sent its salt, IV and priming block, and records
    /// what we write.
    struct Recorder {
        incoming: std::io::Cursor<Vec<u8>>,
        written: Vec<u8>,
    }
    impl std::io::Read for Recorder {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.incoming.read(buf)
        }
    }
    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn connections_reuse_this_sides_salt_but_never_the_iv() {
        let headers: Vec<Vec<u8>> = (0..2)
            .map(|_| {
                let mut peer = Recorder { incoming: std::io::Cursor::new(vec![7u8; SALT_SIZE + 32]), written: Vec::new() };
                open_encrypted_stream(&mut peer, "ReusedSaltKey").unwrap();
                peer.written[..SALT_SIZE + 16].to_vec()
            })
            .collect();
        assert_eq!(headers[0][..SALT_SIZE], headers[1][..SALT_SIZE]);
        assert_ne!(headers[0][SALT_SIZE..], headers[1][SALT_SIZE..]);
        // Another key gets its own salt.
        let mut peer = Recorder { incoming: std::io::Cursor::new(vec![7u8; SALT_SIZE + 32]), written: Vec::new() };
        open_encrypted_stream(&mut peer, "AnotherKey").unwrap();
        assert_ne!(peer.written[..SALT_SIZE], headers[0][..SALT_SIZE]);
    }

    #[test]
    fn cached_keys_match_fresh_derivations() {
        let salt = [3u8; SALT_SIZE];
        assert_eq!(peer_key("CacheKey", &salt), derive_key("CacheKey", &salt));
        assert_eq!(peer_key("CacheKey", &salt), derive_key("CacheKey", &salt));
        assert_eq!(get_24bit_hash("CacheKey"), compute_24bit_hash("CacheKey"));
        assert_eq!(get_24bit_hash("CacheKey"), compute_24bit_hash("CacheKey"));
    }
}
