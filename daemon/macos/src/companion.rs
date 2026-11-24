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
    /// Offers of files copied on Windows, waiting for the network thread.
    clipboard_offers: Mutex<Vec<Offer>>,
    /// The Windows PC currently connected to (its key and our name).
    config: Arc<Mutex<Config>>,
    /// File transfers to and from the companion going on now.
    pub transfers: proto::Transfers,
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
        clipboard_offers: Mutex::new(Vec::new()),
        config,
        transfers: proto::Transfers::default(),
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
        // Sends happen on the input thread: never let one hang there. A
        // companion that vanished is noticed by keepalive within ~30s.
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        if let Err(e) = mwb_protocol::net::set_keepalive(&stream, Duration::from_secs(15), Duration::from_secs(5), 3) {
            eprintln!("[companion] Couldn't turn on TCP keepalive: {e}");
        }
        let read_half = stream.try_clone()?;
        let (mut writer, mut reader) = proto::open(stream, read_half, &cfg.security_key)?;
        writer.write_json(proto::HELLO, &Hello::control(&cfg.machine_name))?;
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
                Ok((proto::CLIPBOARD_TO_MAC, body)) => match serde_json::from_slice::<Offer>(&body) {
                    Ok(offer) => {
                        let names: Vec<&str> = offer.files.iter().map(|f| f.name.as_str()).collect();
                        println!("[companion] Files copied on Windows ({}): {}", offer.id, names.join(", "));
                        self.clipboard_offers.lock().unwrap().push(offer);
                    }
                    Err(e) => eprintln!("[companion] Unreadable clipboard offer: {e}"),
                },
                Ok((proto::CANCEL, body)) => match serde_json::from_slice::<proto::Cancel>(&body) {
                    Ok(cancel) => {
                        println!("[companion] Windows cancelled transfer {}.", cancel.id);
                        self.transfers.cancel(cancel.id);
                    }
                    Err(e) => eprintln!("[companion] Unreadable cancel: {e}"),
                },
                Ok((other, _)) => println!("[companion] Ignoring frame {other}."),
                Err(e) => {
                    println!("[companion] Connection to the companion ended: {e}");
                    return Ok(());
                }
            }
        }
    }

    /// Drops the control connection (its session then reconnects), e.g.
    /// after a sleep it is unlikely to have survived.
    pub fn disconnect(&self, why: &str) {
        if let Some(w) = self.writer.lock().unwrap().take() {
            println!("[companion] Dropping the companion connection: {why}.");
            let _ = w.get_ref().shutdown(std::net::Shutdown::Both);
        }
        self.connected.store(false, Ordering::SeqCst);
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
                self.connected.store(false, Ordering::SeqCst);
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

    /// Stops every transfer going on (the menu bar's "Cancel Transfer"):
    /// tells the companion first, so it takes the closed connection for a
    /// cancel, then cuts it here.
    pub fn cancel_all(&self) {
        for id in self.transfers.ids() {
            println!("[companion] Cancelling transfer {id}.");
            self.send(proto::CANCEL, &proto::Cancel { id });
            self.transfers.cancel(id);
        }
    }

    /// Offers of files dragged on Windows that arrived since the last call.
    pub fn take_offers(&self) -> Vec<Offer> {
        std::mem::take(&mut *self.offers.lock().unwrap())
    }

    /// Offers of files copied on Windows that arrived since the last call.
    pub fn take_clipboard_offers(&self) -> Vec<Offer> {
        std::mem::take(&mut *self.clipboard_offers.lock().unwrap())
    }

    /// Offers `files` under a fresh id with `frame` (a drag or a copy) and
    /// sends their bytes in the background. False if the companion isn't
    /// reachable.
    /// `paths` may include folders: they're offered as every file in them,
    /// named by their path from the folder down, so the companion rebuilds
    /// them. Returns false only when the offer couldn't be sent (the caller
    /// then falls back to PowerToys); with nothing sendable in `paths` (only
    /// empty folders, say) there's nothing to do, which counts as handled.
    /// Walking folders can take a while, so it happens on a thread of its
    /// own, not the caller's (the input thread). Returns false only when the
    /// companion isn't connected (the caller then falls back to PowerToys);
    /// if the link drops while the folders are walked, the offer is lost.
    pub fn offer_files(self: &Arc<Self>, frame: u32, paths: Vec<PathBuf>, what: &'static str) -> bool {
        if !self.is_connected() {
            return false;
        }
        let link = self.clone();
        std::thread::spawn(move || {
            let (files, skipped) = proto::expand_offer_paths(&paths);
            for path in &skipped {
                println!("[companion] Not sending {}: it couldn't be read, or is a link to a folder inside one.", path.display());
            }
            if files.is_empty() {
                println!("[companion] Nothing to send in that {what} (only empty folders?).");
                return;
            }
            let id = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
            let offer = Offer {
                id,
                files: files
                    .iter()
                    .map(|(p, name)| proto::FileInfo { name: name.clone(), size: std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) })
                    .collect(),
            };
            if !link.send(frame, &offer) {
                eprintln!("[companion] Couldn't offer {what} {id}: the companion link dropped.");
                return;
            }
            let names: Vec<&str> = offer.files.iter().take(20).map(|f| f.name.as_str()).collect();
            println!("[companion] Sending {what} {id} to Windows: {}{}", names.join(", "), if offer.files.len() > 20 { ", ..." } else { "" });
            match link.put_files(id, &files) {
                Ok(()) => println!("[companion] Sent {what} {id}'s {} file(s).", files.len()),
                Err(e) => eprintln!("[companion] Sending {what} {id} failed: {e}"),
            }
        });
        true
    }

    fn data_connection(&self, op: &str, id: u64) -> std::io::Result<(FrameWriter<TcpStream>, proto::FrameReader<TcpStream>)> {
        let addr = self.addr.lock().unwrap().ok_or_else(|| std::io::Error::other("not connected to the companion"))?;
        let cfg = self.config.lock().unwrap().clone();
        data_connection(addr, &cfg, op, id)
    }

    /// Fetches an offer's files into `dir`. Every file is created (empty)
    /// up front and passed to `on_created`, so a drag of them all can start
    /// while the bytes are still arriving.
    /// The files are created (empty) before connecting, so a drag of them
    /// can start while the connection is still being set up.
    /// If fetching fails, the files are deleted again (a drag already using
    /// them is cancelled on release, since its fetch failed).
    /// Returns (and passes to `on_created`) the offer's top-level items:
    /// its files, and the folders files inside folders were rebuilt in.
    pub fn fetch_files(&self, offer: &Offer, dir: &Path, on_created: impl FnMut(&Path)) -> std::io::Result<Vec<PathBuf>> {
        let (files, top) = create_placeholders(offer, dir, on_created)?;
        let total = offer.files.iter().map(|f| f.size).sum();
        let result = match self.transfers.begin(offer.id, false, total) {
            Some(transfer) => {
                let received = self.data_connection("get", offer.id).and_then(|(_writer, mut reader)| {
                    transfer.attach(reader.get_ref());
                    receive_into(&mut reader, files, |n| transfer.add(n))
                });
                transfer.outcome(received)
            }
            None => Err(proto::cancelled_error()),
        };
        if let Err(e) = result {
            for path in &top {
                let _ = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
            }
            let _ = std::fs::remove_dir(dir); // only if empty
            return Err(e);
        }
        Ok(top)
    }

    /// Sends `files` (each with the name it was offered under) as drag
    /// `id`'s bytes.
    pub fn put_files(&self, id: u64, files: &[(PathBuf, String)]) -> std::io::Result<()> {
        let total = files.iter().map(|(p, _)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum();
        let transfer = self.transfers.begin(id, true, total).ok_or_else(proto::cancelled_error)?;
        let sent = self.data_connection("put", id).and_then(|(mut writer, _reader)| {
            transfer.attach(writer.get_ref());
            proto::send_files(&mut writer, files, |n| transfer.add(n))
        });
        transfer.outcome(sent)
    }
}

fn data_connection(addr: SocketAddr, cfg: &Config, op: &str, id: u64) -> std::io::Result<(FrameWriter<TcpStream>, proto::FrameReader<TcpStream>)> {
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
    let read_half = stream.try_clone()?;
    let (mut writer, reader) = proto::open(stream, read_half, &cfg.security_key)?;
    writer.write_json(proto::HELLO, &Hello::data(&cfg.machine_name, op, id))?;
    Ok((writer, reader))
}

/// Creates the offer's files, empty, in `dir`, rebuilding folders from
/// their relative names (`folder/sub/a.txt`), each part made safe. Every
/// offered folder keeps one place, and any name that would land on
/// something already there (a file where a folder goes, or a name APFS sees
/// as the same, which by default ignores case) gets " (2)" and so on. Each
/// loose file is a top-level item of its own. Returns every file in offer
/// order, and the top-level items, each passed to `on_created` once
/// everything exists. On failure, everything made in `dir` is removed.
fn create_placeholders(offer: &Offer, dir: &Path, mut on_created: impl FnMut(&Path)) -> std::io::Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    std::fs::create_dir_all(dir)?;
    let laid_out = lay_out(offer, dir).and_then(|(files, top)| {
        for path in &files {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::File::create(path)?;
        }
        Ok((files, top))
    });
    match laid_out {
        Ok((files, top)) => {
            for path in &top {
                on_created(path);
            }
            Ok((files, top))
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(dir);
            Err(e)
        }
    }
}

/// Where each of the offer's files goes under `dir`, and the top-level
/// items (see `create_placeholders`).
fn lay_out(offer: &Offer, dir: &Path) -> std::io::Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    // Lowercased, as APFS compares names by default.
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Offered folder path (as sent) -> where it was rebuilt.
    let mut folders: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
    let mut files = Vec::new();
    let mut top = Vec::new();
    for file in &offer.files {
        let parts = proto::name_parts(&file.name);
        let mut parent = dir.to_path_buf();
        let mut raw_prefix = String::new();
        for (i, part) in parts[..parts.len() - 1].iter().enumerate() {
            raw_prefix.push('/');
            raw_prefix.push_str(part);
            let folder = match folders.get(&raw_prefix) {
                Some(folder) => folder.clone(),
                None => {
                    let folder = claim(&parent, &crate::clipboard::safe_file_name(part), &mut taken);
                    folders.insert(raw_prefix.clone(), folder.clone());
                    if i == 0 {
                        top.push(folder.clone());
                    }
                    folder
                }
            };
            parent = folder;
        }
        let path = claim(&parent, &crate::clipboard::safe_file_name(parts[parts.len() - 1]), &mut taken);
        if parts.len() == 1 {
            top.push(path.clone());
        }
        files.push(path);
    }
    Ok((files, top))
}

/// `parent/name`, or `parent/stem (2).ext` and so on, whichever nothing in
/// `taken` has yet; then it has.
fn claim(parent: &Path, name: &str, taken: &mut std::collections::HashSet<String>) -> PathBuf {
    let p = Path::new(name);
    let stem = p.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let mut path = parent.join(name);
    let mut n = 2;
    while !taken.insert(path.to_string_lossy().to_lowercase()) {
        path = parent.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    path
}

/// Fills the files at `paths` from `reader`.
fn receive_into(reader: &mut proto::FrameReader<TcpStream>, paths: Vec<PathBuf>, progress: impl FnMut(u64)) -> std::io::Result<Vec<PathBuf>> {
    proto::receive_files(
        reader,
        |h| paths.get(h.index as usize).cloned().ok_or_else(|| std::io::Error::other(format!("file {} isn't in the offer", h.index))),
        progress,
    )
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
            proto::send_files(&mut w, &[(served.join("one.txt"), "one.txt".into()), (served.join("two.txt"), "one.txt".into())], |_| {}).unwrap();
        });

        // Two files with the same name, as from two different folders.
        let offer = Offer { id: 9, files: vec![FileInfo { name: "one.txt".into(), size: 5 }, FileInfo { name: "one.txt".into(), size: 6 }] };
        let (_w, mut r) = data_connection(addr, &config(), "get", 9).unwrap();
        let mut created = Vec::new();
        let dst = dir.join("dst");
        let (placeholders, _) = create_placeholders(&offer, &dst, |p| created.push((p.to_path_buf(), std::fs::metadata(p).unwrap().len()))).unwrap();
        let paths = receive_into(&mut r, placeholders, |_| {}).unwrap();
        companion.join().unwrap();

        assert_eq!(paths, vec![dst.join("one.txt"), dst.join("one (2).txt")]);
        assert_eq!(created, vec![(dst.join("one.txt"), 0), (dst.join("one (2).txt"), 0)], "both exist, empty, before any bytes");
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"first");
        assert_eq!(std::fs::read(&paths[1]).unwrap(), b"second");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn folders_are_rebuilt_and_only_top_level_items_are_handed_on() {
        let dst = std::env::temp_dir().join(format!("mwb-companion-folders-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dst);
        let names = ["photos/2026/a.jpg", "photos/b.jpg", "photos", "../evil.txt", "photos/2026/a.jpg", "Photos/c.jpg", "docs/readme", "docs/readme/notes.txt"];
        let offer = Offer { id: 1, files: names.iter().map(|n| FileInfo { name: n.to_string(), size: 0 }).collect() };
        let mut created = Vec::new();
        let (files, top) = create_placeholders(&offer, &dst, |p| created.push(p.to_path_buf())).unwrap();
        assert_eq!(files, vec![
            dst.join("photos/2026/a.jpg"),
            dst.join("photos/b.jpg"),
            dst.join("photos (2)"),
            dst.join("evil.txt"),
            dst.join("photos/2026/a (2).jpg"),
            // Another folder only by case: its own place on a case-blind disk.
            dst.join("Photos (3)/c.jpg"),
            // A file where a folder of the same name also goes.
            dst.join("docs/readme"),
            dst.join("docs/readme (2)/notes.txt"),
        ]);
        assert_eq!(top, vec![dst.join("photos"), dst.join("photos (2)"), dst.join("evil.txt"), dst.join("Photos (3)"), dst.join("docs")]);
        assert_eq!(created, top);
        assert_eq!(top.len(), mwb_protocol::companion::top_level_count(&offer));
        assert!(files.iter().all(|f| f.is_file()));
        std::fs::remove_dir_all(&dst).unwrap();
    }
}
