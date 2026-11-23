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
// Control connection, companion -> bridge.
/// Files dragged on Windows went to this machine. `Offer` payload; fetch
/// the bytes over a `get` data connection.
pub const OFFER_TO_MAC: u32 = 5;
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
    /// Whether the left button was still held when control left: a drag
    /// going on to Windows rather than one that ended here.
    pub button_held: bool,
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
    fn frames_round_trip_both_ways_including_unaligned_payloads() {
        let ((mut aw, mut ar), (mut bw, mut br)) = pair();
        aw.write_json(HELLO, &Hello { role: "control".into(), name: "mac".into(), op: None, id: None }).unwrap();
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
        let json = serde_json::to_string(&Hello { role: "data".into(), name: "mac".into(), op: Some("get".into()), id: Some(3) }).unwrap();
        assert_eq!(json, r#"{"role":"data","name":"mac","op":"get","id":3}"#);
        let json = serde_json::to_string(&Hello { role: "control".into(), name: "mac".into(), op: None, id: None }).unwrap();
        assert_eq!(json, r#"{"role":"control","name":"mac"}"#);
        assert_eq!(serde_json::to_string(&Inactive { button_held: true }).unwrap(), r#"{"button_held":true}"#);
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
