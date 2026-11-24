// The link between a bridge and the Windows companion app
// (`windows/companion/`), which takes over file drag-and-drop on the
// Windows side: catching drags that leave Windows, and starting drags of
// files that arrive. Mouse Without Borders itself can only hand over a
// single dragged file and always saves it to a fixed folder.
//
// The bridge connects to the companion on `COMPANION_PORT`. Every
// connection uses MWB's own v0.102 encryption with the same security key
// (`open_encrypted_stream`), then carries frames: a 16-byte header (type
// u32, reserved u32, payload length u64, little-endian) and the payload,
// zero-padded to a multiple of 16 bytes so the CBC stream stays block
// aligned. A connection's first frame is `HELLO`: one long-lived control
// connection carries small JSON messages both ways, and each file transfer
// gets a short data connection of its own, so a big file never holds up a
// control message. The bridge opens every connection (only the companion
// listens), which keeps firewall setup to one inbound rule on Windows.
// The C# side mirrors this file: `windows/companion/Protocol.cs`.

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};

use crate::mwb_protocol::{open_encrypted_stream, CbcState, EncryptedStream};

pub const COMPANION_PORT: u16 = 15102;

/// Sent in `Hello`, so the companion can tell when the bridge is from a
/// different version of this repo. 2: `Inactive` carries `drag_over` and
/// `let_go` (version 1, sent as no version at all, had only `button_held`).
/// 3: an offer's file names can be relative paths (`folder/sub/a.txt`,
/// always `/`), so folders carry across; an older receiver flattens them.
pub const PROTOCOL_VERSION: u32 = 3;

/// First frame on every connection.
pub const HELLO: u32 = 1;
// Control connection, bridge -> companion.
/// This machine just became the active one (MWB's MachineSwitched): if a
/// drag was going on on Windows, it's now over this machine.
pub const ACTIVE: u32 = 2;
/// Control left this machine (MWB's HideMouse), `Inactive` payload.
pub const INACTIVE: u32 = 3;
/// Files dragged on this machine went to Windows: start a Windows drag of
/// them under the cursor. `Offer` payload; the bytes follow over a `put`
/// data connection.
pub const OFFER_TO_WINDOWS: u32 = 4;
/// The drag of an offer ended without a drop. `Cancel` payload.
pub const CANCEL: u32 = 7;
/// Files copied on this machine: put them on Windows' clipboard once they
/// arrive. `Offer` payload; the bytes follow over a `put` data connection.
pub const CLIPBOARD_TO_WINDOWS: u32 = 8;
// Control connection, companion -> bridge.
/// Files dragged on Windows went to this machine. `Offer` payload; fetch
/// the bytes over a `get` data connection.
pub const OFFER_TO_MAC: u32 = 5;
/// Files copied on Windows: fetch them (a `get` data connection) and put
/// them on this machine's clipboard. `Offer` payload.
pub const CLIPBOARD_TO_MAC: u32 = 6;
// Data connections: per file a header, its bytes in chunks, an end; then
// one `DONE` after the last file.
pub const FILE_HEADER: u32 = 10;
pub const FILE_CHUNK: u32 = 11;
pub const FILE_END: u32 = 12;
pub const DONE: u32 = 13;

/// Bytes per `FILE_CHUNK` frame.
pub const CHUNK_SIZE: usize = 1 << 20;
/// Biggest frame accepted, so a corrupt length can't allocate gigabytes.
const MAX_FRAME: u64 = 4 << 20;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// "control" or "data".
    pub role: String,
    pub name: String,
    /// Data connections: "get" (companion sends the offer's files) or "put"
    /// (bridge sends them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    /// `PROTOCOL_VERSION`; missing (0) from bridges before it existed.
    #[serde(default)]
    pub version: u32,
}

impl Hello {
    pub fn control(name: &str) -> Self {
        Self { role: "control".into(), name: name.into(), op: None, id: None, version: PROTOCOL_VERSION }
    }

    pub fn data(name: &str, op: &str, id: u64) -> Self {
        Self { role: "data".into(), name: name.into(), op: Some(op.into()), id: Some(id), version: PROTOCOL_VERSION }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub name: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub id: u64,
    pub files: Vec<FileInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Inactive {
    /// Whether a drag from Windows was still over this machine, not let go,
    /// when control left: it's going back to Windows. Otherwise any drag
    /// the companion caught ended here, and it ends Windows' leftover drag.
    /// Deliberately not the button state: the button may be held for
    /// something else entirely (a drag that started here) by then.
    pub drag_over: bool,
    /// Whether the left button was held: what companions before version 2
    /// go by instead (they ignore `drag_over`), still sent for them.
    #[serde(default)]
    pub button_held: bool,
    /// Whether the left button was let go of on this machine since control
    /// came here (or the connection to Windows was lost meanwhile). A drag
    /// from Windows that wasn't let go here is coming back, even when no
    /// drop of it started here (a quick bounce beats the offer, or it
    /// carried no files); one that was ended here, even before its offer
    /// was taken.
    #[serde(default)]
    pub let_go: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Cancel {
    pub id: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FileHeader {
    pub index: u32,
    pub name: String,
    pub size: u64,
}

/// Writes frames to one direction of an encrypted connection.
pub struct FrameWriter<S: Write> {
    stream: S,
    cipher: CbcState,
    chain: [u8; 16],
}

/// Reads frames from the other direction.
pub struct FrameReader<S: Read> {
    stream: S,
    cipher: CbcState,
    chain: [u8; 16],
}

/// Sets up encryption on a fresh connection (both sides call this, each
/// writing first) and splits it into a writer on `stream` and a reader on
/// `read_half` (a second handle to the same connection, e.g.
/// `TcpStream::try_clone`).
pub fn open<S: Read + Write, R: Read>(mut stream: S, read_half: R, security_key: &str) -> std::io::Result<(FrameWriter<S>, FrameReader<R>)> {
    let EncryptedStream { cipher, write_chain, read_chain } = open_encrypted_stream(&mut stream, security_key)?;
    Ok((
        FrameWriter { stream, cipher: cipher.clone(), chain: write_chain },
        FrameReader { stream: read_half, cipher, chain: read_chain },
    ))
}

fn padded(len: usize) -> usize {
    len.div_ceil(16) * 16
}

impl<S: Write> FrameWriter<S> {
    pub fn write_frame(&mut self, frame_type: u32, payload: &[u8]) -> std::io::Result<()> {
        let mut buf = vec![0u8; 16 + padded(payload.len())];
        buf[0..4].copy_from_slice(&frame_type.to_le_bytes());
        buf[8..16].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        buf[16..16 + payload.len()].copy_from_slice(payload);
        self.stream.write_all(&self.cipher.encrypt(&mut self.chain, &buf))
    }

    pub fn write_json<T: Serialize>(&mut self, frame_type: u32, value: &T) -> std::io::Result<()> {
        let json = serde_json::to_vec(value).map_err(std::io::Error::other)?;
        self.write_frame(frame_type, &json)
    }

    pub fn get_ref(&self) -> &S {
        &self.stream
    }
}

impl<S: Read> FrameReader<S> {
    pub fn read_frame(&mut self) -> std::io::Result<(u32, Vec<u8>)> {
        let mut header = [0u8; 16];
        self.stream.read_exact(&mut header)?;
        let header = self.cipher.decrypt(&mut self.chain, &header);
        let frame_type = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let len = u64::from_le_bytes(header[8..16].try_into().unwrap());
        if len > MAX_FRAME {
            return Err(std::io::Error::other(format!("frame of {len} bytes is too big (wrong security key?)")));
        }
        let mut body = vec![0u8; padded(len as usize)];
        self.stream.read_exact(&mut body)?;
        let mut body = self.cipher.decrypt(&mut self.chain, &body);
        body.truncate(len as usize);
        Ok((frame_type, body))
    }

    /// Reads one frame, which must be of `expected` type, as JSON.
    pub fn read_json<T: for<'de> Deserialize<'de>>(&mut self, expected: u32) -> std::io::Result<T> {
        let (frame_type, body) = self.read_frame()?;
        if frame_type != expected {
            return Err(std::io::Error::other(format!("expected frame {expected}, got {frame_type}")));
        }
        serde_json::from_slice(&body).map_err(std::io::Error::other)
    }
}

/// The files to offer for `paths` (files and folders, as dragged or
/// copied), each with the name it's offered under: a file's own name, or
/// for a file inside a folder its path from that folder down
/// (`folder/sub/a.txt`). Folders are walked in name order; symbolic links
/// to folders aren't followed (they could loop), and empty folders carry
/// nothing, so they don't arrive. Also returns what couldn't be read.
pub fn expand_offer_paths(paths: &[std::path::PathBuf]) -> (Vec<(std::path::PathBuf, String)>, Vec<std::path::PathBuf>) {
    fn walk(dir: &std::path::Path, prefix: &str, out: &mut Vec<(std::path::PathBuf, String)>, skipped: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            skipped.push(dir.to_path_buf());
            return;
        };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            let name = format!("{prefix}/{}", path.file_name().unwrap_or_default().to_string_lossy());
            let is_link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
            match std::fs::metadata(&path) {
                Ok(m) if m.is_file() => out.push((path, name)),
                Ok(m) if m.is_dir() && !is_link => walk(&path, &name, out, skipped),
                _ => skipped.push(path),
            }
        }
    }
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for path in paths {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        match std::fs::metadata(path) {
            Ok(m) if m.is_file() => out.push((path.clone(), name)),
            Ok(m) if m.is_dir() => walk(path, &name, &mut out, &mut skipped),
            _ => skipped.push(path.clone()),
        }
    }
    (out, skipped)
}

/// An offered name's path parts, for a receiver to rebuild it under its
/// own folder: split on `/` (and `\\`), with empty, `.` and `..` parts
/// dropped so nothing can land outside that folder. Never empty.
pub fn name_parts(name: &str) -> Vec<&str> {
    let parts: Vec<&str> = name.split(['/', '\\']).filter(|p| !p.is_empty() && *p != "." && *p != "..").collect();
    if parts.is_empty() { vec!["file"] } else { parts }
}

/// How many top-level items an offer rebuilds into: each loose file, plus
/// one folder per distinct folder name its other files are inside.
pub fn top_level_count(offer: &Offer) -> usize {
    let mut folders = std::collections::HashSet::new();
    let mut loose = 0;
    for file in &offer.files {
        let parts = name_parts(&file.name);
        if parts.len() > 1 {
            folders.insert(parts[0]);
        } else {
            loose += 1;
        }
    }
    loose + folders.len()
}

/// Sends files over a data connection whose `HELLO` is done: per file a
/// header, its bytes, an end, then `DONE`.
pub fn send_files<S: Write>(writer: &mut FrameWriter<S>, files: &[(std::path::PathBuf, String)]) -> std::io::Result<()> {
    for (index, (path, name)) in files.iter().enumerate() {
        let mut file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        writer.write_json(FILE_HEADER, &FileHeader { index: index as u32, name: name.clone(), size })?;
        let mut buf = vec![0u8; CHUNK_SIZE];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            writer.write_frame(FILE_CHUNK, &buf[..n])?;
        }
        writer.write_frame(FILE_END, &[])?;
    }
    writer.write_frame(DONE, &[])
}

/// Receives files sent by `send_files`, writing file `index` to
/// `dest(index, header)` as it arrives. Returns the paths written, in order.
pub fn receive_files<R: Read>(
    reader: &mut FrameReader<R>,
    mut dest: impl FnMut(&FileHeader) -> std::io::Result<std::path::PathBuf>,
) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut written = Vec::new();
    let mut current: Option<(std::fs::File, u64, FileHeader)> = None;
    loop {
        let (frame_type, body) = reader.read_frame()?;
        match frame_type {
            FILE_HEADER => {
                let header: FileHeader = serde_json::from_slice(&body).map_err(std::io::Error::other)?;
                let path = dest(&header)?;
                current = Some((std::fs::File::create(&path)?, 0, header));
                written.push(path);
            }
            FILE_CHUNK => {
                let Some((file, received, _)) = current.as_mut() else {
                    return Err(std::io::Error::other("file data before its header"));
                };
                file.write_all(&body)?;
                *received += body.len() as u64;
            }
            FILE_END => {
                let Some((file, received, header)) = current.take() else {
                    return Err(std::io::Error::other("file end before its header"));
                };
                file.sync_all()?;
                if received != header.size {
                    return Err(std::io::Error::other(format!("{} arrived with {received} of {} bytes", header.name, header.size)));
                }
            }
            DONE => return Ok(written),
            other => return Err(std::io::Error::other(format!("unexpected frame {other} in a file transfer"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    const KEY: &str = "SharedKey0123456";

    fn pair() -> ((FrameWriter<UnixStream>, FrameReader<UnixStream>), (FrameWriter<UnixStream>, FrameReader<UnixStream>)) {
        let (a, b) = UnixStream::pair().unwrap();
        let b_side = std::thread::spawn(move || {
            let read = b.try_clone().unwrap();
            open(b, read, KEY).unwrap()
        });
        let read = a.try_clone().unwrap();
        let a_side = open(a, read, KEY).unwrap();
        (a_side, b_side.join().unwrap())
    }

    #[test]
    fn folders_expand_to_their_files_with_relative_names() {
        let root = std::env::temp_dir().join(format!("mwb-expand-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("photos/2026")).unwrap();
        std::fs::create_dir_all(root.join("photos/empty")).unwrap();
        std::fs::write(root.join("photos/b.jpg"), b"b").unwrap();
        std::fs::write(root.join("photos/2026/a.jpg"), b"a").unwrap();
        std::fs::write(root.join("notes.txt"), b"n").unwrap();
        std::os::unix::fs::symlink(root.join("photos"), root.join("photos/loop")).unwrap();
        let (files, skipped) = expand_offer_paths(&[root.join("photos"), root.join("notes.txt"), root.join("missing")]);
        let names: Vec<&str> = files.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(names, vec!["photos/2026/a.jpg", "photos/b.jpg", "notes.txt"]);
        assert_eq!(files[0].0, root.join("photos/2026/a.jpg"));
        assert_eq!(skipped, vec![root.join("photos/loop"), root.join("missing")]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn top_level_items_are_loose_files_and_distinct_folders() {
        let offer = |names: &[&str]| Offer { id: 1, files: names.iter().map(|n| FileInfo { name: n.to_string(), size: 0 }).collect() };
        assert_eq!(top_level_count(&offer(&["a.txt", "b.txt"])), 2);
        assert_eq!(top_level_count(&offer(&["photos/a.jpg", "photos/2026/b.jpg", "notes.txt"])), 2);
        assert_eq!(top_level_count(&offer(&["photos", "photos/a.jpg"])), 2);
        assert_eq!(top_level_count(&offer(&["a.txt", "a.txt"])), 2);
    }

    #[test]
    fn name_parts_cannot_climb_out() {
        assert_eq!(name_parts("photos/2026/a.jpg"), vec!["photos", "2026", "a.jpg"]);
        assert_eq!(name_parts("../../etc/passwd"), vec!["etc", "passwd"]);
        assert_eq!(name_parts("a\\b/./c"), vec!["a", "b", "c"]);
        assert_eq!(name_parts("/"), vec!["file"]);
        assert_eq!(name_parts(".."), vec!["file"]);
    }

    #[test]
    fn frames_round_trip_both_ways_including_unaligned_payloads() {
        let ((mut aw, mut ar), (mut bw, mut br)) = pair();
        aw.write_json(HELLO, &Hello::control("mac")).unwrap();
        aw.write_frame(ACTIVE, &[]).unwrap();
        bw.write_json(OFFER_TO_MAC, &Offer { id: 7, files: vec![FileInfo { name: "a b.txt".into(), size: 3 }] }).unwrap();

        let hello: Hello = br.read_json(HELLO).unwrap();
        assert_eq!(hello.name, "mac");
        assert_eq!(br.read_frame().unwrap(), (ACTIVE, vec![]));
        let offer: Offer = ar.read_json(OFFER_TO_MAC).unwrap();
        assert_eq!(offer.files[0].name, "a b.txt");
    }

    #[test]
    fn the_wire_format_is_what_the_csharp_side_expects() {
        // Hello's optional fields are left out, not sent as null.
        let json = serde_json::to_string(&Hello::data("mac", "get", 3)).unwrap();
        assert_eq!(json, r#"{"role":"data","name":"mac","op":"get","id":3,"version":3}"#);
        let json = serde_json::to_string(&Hello::control("mac")).unwrap();
        assert_eq!(json, r#"{"role":"control","name":"mac","version":3}"#);
        assert_eq!(
            serde_json::to_string(&Inactive { drag_over: true, button_held: false, let_go: false }).unwrap(),
            r#"{"drag_over":true,"button_held":false,"let_go":false}"#
        );
        // A hello from before versions existed still parses.
        let old: Hello = serde_json::from_str(r#"{"role":"control","name":"mac"}"#).unwrap();
        assert_eq!(old.version, 0);
    }

    #[test]
    fn files_survive_a_transfer_in_order() {
        let dir = std::env::temp_dir().join(format!("mwb-companion-files-{}", std::process::id()));
        let (src, dst) = (dir.join("src"), dir.join("dst"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        let big: Vec<u8> = (0..CHUNK_SIZE + 1000).map(|i| (i % 251) as u8).collect();
        std::fs::write(src.join("big.bin"), &big).unwrap();
        std::fs::write(src.join("empty.txt"), b"").unwrap();

        let ((mut aw, _ar), (_bw, mut br)) = pair();
        let files = vec![(src.join("big.bin"), "big.bin".to_string()), (src.join("empty.txt"), "empty.txt".to_string())];
        let sender = std::thread::spawn(move || send_files(&mut aw, &files).unwrap());
        let written = receive_files(&mut br, |h| Ok(dst.join(&h.name))).unwrap();
        sender.join().unwrap();

        assert_eq!(written, vec![dir.join("dst/big.bin"), dir.join("dst/empty.txt")]);
        assert_eq!(std::fs::read(&written[0]).unwrap(), big);
        assert_eq!(std::fs::read(&written[1]).unwrap(), b"");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
