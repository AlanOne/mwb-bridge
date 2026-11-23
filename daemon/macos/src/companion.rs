// The Mac's side of the link to the Windows companion app (see
// `mwb_protocol::companion` for the protocol and why the companion
// exists). A background thread keeps a control connection to the companion
// on whichever Windows PC the bridge is connected to; the network thread
// sends it Active/Inactive/offers through `Link` and picks up the
// companion's offers of files dragged on Windows. Without a companion,
// drag-and-drop falls back to PowerToys' own single-file mechanism.

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use mwb_protocol::companion::{self as proto, FrameWriter, Hello, Offer};
use mwb_protocol::config::Config;
use serde::Serialize;

pub struct Link {
    /// The connected companion's control connection, if any.
    writer: Mutex<Option<FrameWriter<TcpStream>>>,
    /// Where the companion listens, for data connections.
    addr: Mutex<Option<SocketAddr>>,
    connected: AtomicBool,
    /// Offers of files dragged on Windows, waiting for the network thread.
    offers: Mutex<Vec<Offer>>,
    /// The Windows PC currently connected to (its key and our name).
    config: Arc<Mutex<Config>>,
}

static LINK: OnceLock<Arc<Link>> = OnceLock::new();

/// The process-wide link, once `start` has run.
pub fn link() -> Option<&'static Arc<Link>> {
    LINK.get()
}

/// Starts keeping a connection to the companion on the PC `peer()` names
/// (the Windows end of the bridge's live connection).
pub fn start(config: Arc<Mutex<Config>>, peer: fn() -> Option<SocketAddr>) {
    let link = Arc::new(Link {
        writer: Mutex::new(None),
        addr: Mutex::new(None),
        connected: AtomicBool::new(false),
        offers: Mutex::new(Vec::new()),
        config,
    });
    if LINK.set(link.clone()).is_ok() {
        let watched = link.clone();
        std::thread::spawn(move || watch(watched, peer));
        std::thread::spawn(move || run(link, peer));
    }
}

/// Closes the control connection once the bridge is no longer connected to
/// that companion's PC (moved to another PC, or disconnected), which ends
/// the blocked read in `session` so `run` reconnects where it should.
fn watch(link: Arc<Link>, peer: fn() -> Option<SocketAddr>) {
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let companion_ip = link.addr.lock().unwrap().map(|a| a.ip());
        if link.is_connected() && companion_ip != peer().map(|p| p.ip()) {
            println!("[companion] The bridge left the companion's PC, disconnecting from it.");
            if let Some(w) = link.writer.lock().unwrap().as_ref() {
                let _ = w.get_ref().shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

fn run(link: Arc<Link>, peer: fn() -> Option<SocketAddr>) {
    let mut last_error = String::new();
    loop {
        if let Some(addr) = peer().map(|p| with_port(p, proto::COMPANION_PORT)) {
            match link.session(addr) {
                Ok(()) => last_error.clear(),
                Err(e) => {
                    // A missing companion looks the same every few seconds:
                    // log it once, not on every retry.
                    let msg = e.to_string();
                    if msg != last_error {
                        println!("[companion] No companion at {addr}: {msg} (drag-and-drop uses PowerToys' own instead).");
                        last_error = msg;
                    }
                }
            }
            link.connected.store(false, Ordering::SeqCst);
            *link.writer.lock().unwrap() = None;
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn with_port(addr: SocketAddr, port: u16) -> SocketAddr {
    let mut addr = addr;
    addr.set_port(port);
    addr
}

impl Link {
    /// One control connection, until it drops or the bridge moves to
    /// another PC.
    fn session(&self, addr: SocketAddr) -> std::io::Result<()> {
        let cfg = self.config.lock().unwrap().clone();
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        let _ = stream.set_nodelay(true);
        let read_half = stream.try_clone()?;
        let (mut writer, mut reader) = proto::open(stream, read_half, &cfg.security_key)?;
        writer.write_json(proto::HELLO, &Hello { role: "control".into(), name: cfg.machine_name.clone(), op: None, id: None })?;
        *self.addr.lock().unwrap() = Some(addr);
        *self.writer.lock().unwrap() = Some(writer);
        self.connected.store(true, Ordering::SeqCst);
        println!("[companion] Connected to the companion at {addr}.");
        loop {
            // Blocks until the companion says something; `watch` cuts the
            // connection when the bridge moves to another PC.
            match reader.read_frame() {
                Ok((proto::OFFER_TO_MAC, body)) => match serde_json::from_slice::<Offer>(&body) {
                    Ok(offer) => {
                        let names: Vec<&str> = offer.files.iter().map(|f| f.name.as_str()).collect();
                        println!("[companion] Windows offers drag {}: {}", offer.id, names.join(", "));
                        self.offers.lock().unwrap().push(offer);
                    }
                    Err(e) => eprintln!("[companion] Unreadable offer: {e}"),
                },
                Ok((other, _)) => println!("[companion] Ignoring frame {other}."),
                Err(e) => {
                    println!("[companion] Connection to the companion ended: {e}");
                    return Ok(());
                }
            }
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn send_frame(&self, frame: u32, payload: &[u8]) -> bool {
        let mut writer = self.writer.lock().unwrap();
        let Some(w) = writer.as_mut() else { return false };
        match w.write_frame(frame, payload) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[companion] Sending to the companion failed: {e}");
                let _ = w.get_ref().shutdown(std::net::Shutdown::Both);
                *writer = None;
                false
            }
        }
    }

    pub fn send<T: Serialize>(&self, frame: u32, message: &T) -> bool {
        match serde_json::to_vec(message) {
            Ok(json) => self.send_frame(frame, &json),
            Err(_) => false,
        }
    }

    pub fn send_empty(&self, frame: u32) -> bool {
        self.send_frame(frame, &[])
    }

    /// Offers of files dragged on Windows that arrived since the last call.
    pub fn take_offers(&self) -> Vec<Offer> {
        std::mem::take(&mut *self.offers.lock().unwrap())
    }

    fn data_connection(&self, op: &str, id: u64) -> std::io::Result<(FrameWriter<TcpStream>, proto::FrameReader<TcpStream>)> {
        let addr = self.addr.lock().unwrap().ok_or_else(|| std::io::Error::other("not connected to the companion"))?;
        let cfg = self.config.lock().unwrap().clone();
        data_connection(addr, &cfg, op, id)
    }

    /// Fetches an offer's files into `dir`. Every file is created (empty)
    /// up front and passed to `on_created`, so a drag of them all can start
    /// while the bytes are still arriving.
    pub fn fetch_files(&self, offer: &Offer, dir: &Path, on_created: impl FnMut(&Path)) -> std::io::Result<Vec<PathBuf>> {
        let (_writer, mut reader) = self.data_connection("get", offer.id)?;
        receive_offer(&mut reader, offer, dir, on_created)
    }

    /// Sends `files` as drag `id`'s bytes.
    pub fn put_files(&self, id: u64, files: &[PathBuf]) -> std::io::Result<()> {
        let (mut writer, _reader) = self.data_connection("put", id)?;
        let named: Vec<(PathBuf, String)> = files
            .iter()
            .map(|p| (p.clone(), p.file_name().unwrap_or_default().to_string_lossy().into_owned()))
            .collect();
        proto::send_files(&mut writer, &named)
    }
}

fn data_connection(addr: SocketAddr, cfg: &Config, op: &str, id: u64) -> std::io::Result<(FrameWriter<TcpStream>, proto::FrameReader<TcpStream>)> {
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
    let read_half = stream.try_clone()?;
    let (mut writer, reader) = proto::open(stream, read_half, &cfg.security_key)?;
    writer.write_json(proto::HELLO, &Hello { role: "data".into(), name: cfg.machine_name.clone(), op: Some(op.into()), id: Some(id) })?;
    Ok((writer, reader))
}

/// Creates the offer's files in `dir` (names made safe and unique), then
/// fills them from `reader`.
fn receive_offer(reader: &mut proto::FrameReader<TcpStream>, offer: &Offer, dir: &Path, mut on_created: impl FnMut(&Path)) -> std::io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let mut paths: Vec<PathBuf> = Vec::new();
    for file in &offer.files {
        let path = unique_path(dir, &crate::clipboard::safe_file_name(&file.name), &paths);
        std::fs::File::create(&path)?;
        on_created(&path);
        paths.push(path);
    }
    proto::receive_files(reader, |h| {
        paths.get(h.index as usize).cloned().ok_or_else(|| std::io::Error::other(format!("file {} isn't in the offer", h.index)))
    })
}

/// `dir/name`, or `dir/stem (2).ext` and so on when an earlier file of the
/// same drag already took that name (same name, different folders).
fn unique_path(dir: &Path, name: &str, taken: &[PathBuf]) -> PathBuf {
    let first = dir.join(name);
    if !taken.contains(&first) {
        return first;
    }
    let p = Path::new(name);
    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|c| !taken.contains(c)).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mwb_protocol::companion::FileInfo;
    use std::net::TcpListener;

    const KEY: &str = "SharedKey0123456";

    fn config() -> Config {
        serde_json::from_value(serde_json::json!({
            "security_key": KEY, "windows_ip": "unused", "machine_name": "mac", "machine_id": 3,
        }))
        .unwrap()
    }

    #[test]
    fn a_fetch_creates_every_file_first_then_fills_them() {
        let dir = std::env::temp_dir().join(format!("mwb-companion-fetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("one.txt"), b"first").unwrap();
        std::fs::write(src.join("two.txt"), b"second").unwrap();

        // A fake companion serving a "get" for drag 9.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let served = src.clone();
        let companion = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let read = s.try_clone().unwrap();
            let (mut w, mut r) = proto::open(s, read, KEY).unwrap();
            let hello: Hello = r.read_json(proto::HELLO).unwrap();
            assert_eq!((hello.op.as_deref(), hello.id), (Some("get"), Some(9)));
            proto::send_files(&mut w, &[(served.join("one.txt"), "one.txt".into()), (served.join("two.txt"), "one.txt".into())]).unwrap();
        });

        // Two files with the same name, as from two different folders.
        let offer = Offer { id: 9, files: vec![FileInfo { name: "one.txt".into(), size: 5 }, FileInfo { name: "one.txt".into(), size: 6 }] };
        let (_w, mut r) = data_connection(addr, &config(), "get", 9).unwrap();
        let mut created = Vec::new();
        let dst = dir.join("dst");
        let paths = receive_offer(&mut r, &offer, &dst, |p| created.push((p.to_path_buf(), std::fs::metadata(p).unwrap().len()))).unwrap();
        companion.join().unwrap();

        assert_eq!(paths, vec![dst.join("one.txt"), dst.join("one (2).txt")]);
        assert_eq!(created, vec![(dst.join("one.txt"), 0), (dst.join("one (2).txt"), 0)], "both exist, empty, before any bytes");
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"first");
        assert_eq!(std::fs::read(&paths[1]).unwrap(), b"second");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
