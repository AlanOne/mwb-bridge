// Windows -> Mac drag into any app: the download side.
//
// Real MWB only pulls a dragged file once it's dropped, and then saves it to
// a fixed folder. Here the file is fetched as soon as the drag reaches the
// Mac, and a real macOS drag of it is started under the cursor
// (`drag_source`), so whichever app the button is released over gets an
// ordinary file drop. `Fetch` is the download both sides share: the thread
// fetching it, and the network thread deciding what happens to it when the
// drag ends.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

use mwb_protocol::config::Config;
use mwb_protocol::input_handling::{InputSink, BTN_LEFT};

use crate::cg_input::CgInput;

/// What to do with the fetched file once it has fully arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum After {
    /// Leave it where it is: an app was handed it by a drop.
    Keep,
    /// Released before a drag of it could start: save it where real MWB
    /// would (`clipboard::dropped_files_dir`) and show it in Finder.
    SaveToDropFolder,
    /// The drag went back to Windows: delete it.
    Discard,
}

#[derive(Default)]
struct State {
    path: Option<PathBuf>,
    outcome: Option<Result<PathBuf, String>>,
    after: Option<After>,
}

pub struct Fetch {
    state: Mutex<State>,
    done: Condvar,
    drop_dir: PathBuf,
    reveal: bool,
}

impl Fetch {
    /// `drop_dir` is where `After::SaveToDropFolder` saves to; `reveal`
    /// shows the saved file in Finder (off in tests).
    pub fn new(drop_dir: PathBuf, reveal: bool) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(State::default()), done: Condvar::new(), drop_dir, reveal })
    }

    /// The fetching thread created the (still empty) destination file.
    pub fn created(&self, path: &Path) {
        self.state.lock().unwrap().path = Some(path.to_path_buf());
    }

    /// The fetching thread is done, one way or the other.
    pub fn finished(&self, result: std::io::Result<PathBuf>) {
        let outcome = result.map_err(|e| e.to_string());
        let after = {
            let mut state = self.state.lock().unwrap();
            state.outcome = Some(outcome.clone());
            state.after
        };
        self.done.notify_all();
        if let Some(after) = after {
            self.apply(after, &outcome);
        }
    }

    /// Where the file is being written, once that's known.
    pub fn path(&self) -> Option<PathBuf> {
        self.state.lock().unwrap().path.clone()
    }

    /// Waits up to `timeout` for the download to end: the file's path, or
    /// why it failed. `None` if it's still going.
    pub fn wait(&self, timeout: Duration) -> Option<Result<PathBuf, String>> {
        let state = self.state.lock().unwrap();
        let (state, _) = self.done.wait_timeout_while(state, timeout, |s| s.outcome.is_none()).unwrap();
        state.outcome.clone()
    }

    /// Decides what happens to the file once it has arrived: right away if
    /// it already has, otherwise when it does.
    pub fn then(&self, after: After) {
        let outcome = {
            let mut state = self.state.lock().unwrap();
            state.after = Some(after);
            state.outcome.clone()
        };
        if let Some(outcome) = outcome {
            self.apply(after, &outcome);
        }
    }

    fn apply(&self, after: After, outcome: &Result<PathBuf, String>) {
        let path = match outcome {
            Ok(path) => path,
            Err(e) => {
                eprintln!("[drop] Fetching the dragged file from Windows failed: {e}");
                return;
            }
        };
        match after {
            After::Keep => {}
            After::Discard => {
                let _ = std::fs::remove_file(path);
                if let Some(dir) = path.parent() {
                    let _ = std::fs::remove_dir(dir); // only if empty
                }
            }
            After::SaveToDropFolder => match save_to(&self.drop_dir, path) {
                Ok(dest) => {
                    println!("[drop] Saved {} (released before it could be handed to an app).", dest.display());
                    if self.reveal
                        && let Err(e) = std::process::Command::new("open").arg("-R").arg(&dest).status()
                    {
                        eprintln!("[drop] Couldn't reveal it in Finder: {e}");
                    }
                }
                Err(e) => eprintln!("[drop] Couldn't save {} to {}: {e}", path.display(), self.drop_dir.display()),
            },
        }
    }
}

/// The drag from Windows currently over this Mac, if any, as the network
/// thread drives it: one method per thing that happens to it.
#[derive(Default)]
pub struct IncomingDrag {
    current: Option<Incoming>,
}

struct Incoming {
    fetch: Arc<Fetch>,
    // A macOS drag of the file is running (the left button is held here).
    dragging: bool,
    // Starting one failed, so don't try again for this drag.
    gave_up: bool,
}

// How long a release waits for the file to finish downloading before the
// drop goes ahead (an app reads the file when it's dropped).
const DOWNLOAD_WAIT: Duration = Duration::from_secs(120);
// Drag folders older than this are deleted when a new drag starts.
const KEEP_DRAGS: Duration = Duration::from_secs(24 * 3600);

impl IncomingDrag {
    /// A file dragged on Windows reached this Mac: start fetching it.
    pub fn start(&mut self, cfg: &Config, peer_addr: Option<SocketAddr>) {
        let root = crate::clipboard::dragged_files_dir();
        prune_old_drags(&root, KEEP_DRAGS);
        let dir = new_drag_dir(&root);
        let fetch = Fetch::new(crate::clipboard::dropped_files_dir(), true);
        self.current = Some(Incoming { fetch: fetch.clone(), dragging: false, gave_up: false });
        let cfg = cfg.clone();
        std::thread::spawn(move || {
            let result = crate::clipboard::fetch_dragged_file(&cfg, peer_addr, &dir, |path| fetch.created(path));
            if let Ok(path) = &result {
                let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                println!("[drop] Fetched {} ({size} bytes) from Windows.", path.display());
            }
            fetch.finished(result);
        });
    }

    /// A mouse move is about to be posted: once the file exists on disk,
    /// start a macOS drag of it under the cursor.
    pub fn on_move(&mut self, cg: &mut CgInput) {
        let Some(incoming) = self.current.as_mut() else { return };
        if incoming.dragging || incoming.gave_up || cg.left_button_held() {
            return;
        }
        let Some(path) = incoming.fetch.path() else { return };
        let main_height = core_graphics::display::CGDisplay::main().bounds().size.height;
        if !crate::drag_source::place_under_cursor(&path, cg.cursor(), main_height) {
            eprintln!("[drop] Couldn't show the drag panel, the file will be saved instead when dropped.");
            incoming.gave_up = true;
            return;
        }
        // Let the panel reach the screen before pressing on it.
        std::thread::sleep(Duration::from_millis(40));
        cg.button(BTN_LEFT, true);
        if crate::drag_source::wait_started(Duration::from_millis(300)) {
            incoming.dragging = true;
            println!("[drop] Dragging {} on this Mac; let go over any app to drop it there.", path.display());
        } else {
            eprintln!("[drop] The drag didn't start, the file will be saved instead when dropped.");
            cg.button(BTN_LEFT, false);
            crate::drag_source::remove_panel();
            incoming.gave_up = true;
        }
    }

    /// The button was released over this Mac. Called before the release is
    /// posted, which is what drops the file into the app under the cursor.
    pub fn dropped(&mut self, cg: &mut CgInput) {
        let Some(incoming) = self.current.take() else { return };
        if !incoming.dragging {
            incoming.fetch.then(After::SaveToDropFolder);
            return;
        }
        match incoming.fetch.wait(DOWNLOAD_WAIT) {
            Some(Ok(_)) => incoming.fetch.then(After::Keep),
            failed => {
                let why = match failed {
                    Some(Err(e)) => e,
                    _ => "it took too long".to_string(),
                };
                eprintln!("[drop] Fetching the file failed ({why}), cancelling the drop.");
                cg.cancel_drag();
                incoming.fetch.then(After::Discard);
            }
        }
    }

    /// The drag left without a drop here.
    pub fn cancelled(&mut self, cg: &mut CgInput) {
        let Some(incoming) = self.current.take() else { return };
        if incoming.dragging {
            cg.cancel_drag();
        }
        incoming.fetch.then(After::Discard);
    }
}

fn save_to(dir: &Path, path: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let dest = dir.join(path.file_name().unwrap_or_default());
    // Copy rather than move: the cache and the Desktop can be on different
    // volumes, and the copy left behind is cleaned up with old drags.
    std::fs::copy(path, &dest)?;
    Ok(dest)
}

/// A fresh folder under `root` for one drag's file, so a file dropped into
/// an app earlier is never overwritten by a later drag of the same name.
pub fn new_drag_dir(root: &Path) -> PathBuf {
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis();
    root.join(stamp.to_string())
}

/// Deletes drag folders under `root` older than `max_age`. Apps may read a
/// dropped file a while after the drop, so they're kept for a day rather
/// than deleted as soon as the drop is done.
pub fn prune_old_drags(root: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= max_age);
        if old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("mwb-drag-in-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A fetch whose file has been written to `<tmp>/drag/report.pdf`.
    fn fetched(tmp: &TempDir) -> (Arc<Fetch>, PathBuf) {
        let fetch = Fetch::new(tmp.0.join("drop"), false);
        let path = tmp.0.join("drag").join("report.pdf");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"pdf").unwrap();
        fetch.created(&path);
        (fetch, path)
    }

    #[test]
    fn released_early_saves_to_the_drop_folder_once_the_download_ends() {
        let tmp = TempDir::new("early");
        let (fetch, path) = fetched(&tmp);
        fetch.then(After::SaveToDropFolder);
        assert!(!tmp.0.join("drop/report.pdf").exists(), "nothing to save before it has arrived");
        fetch.finished(Ok(path));
        assert_eq!(std::fs::read(tmp.0.join("drop/report.pdf")).unwrap(), b"pdf");
    }

    #[test]
    fn a_drag_that_went_back_deletes_the_file_and_its_folder() {
        let tmp = TempDir::new("discard");
        let (fetch, path) = fetched(&tmp);
        fetch.finished(Ok(path.clone()));
        fetch.then(After::Discard);
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn a_drop_into_an_app_keeps_the_file() {
        let tmp = TempDir::new("keep");
        let (fetch, path) = fetched(&tmp);
        fetch.finished(Ok(path.clone()));
        fetch.then(After::Keep);
        assert!(path.exists());
        assert!(!tmp.0.join("drop").exists());
    }

    #[test]
    fn wait_reports_the_outcome_or_times_out() {
        let tmp = TempDir::new("wait");
        let (fetch, path) = fetched(&tmp);
        assert_eq!(fetch.wait(Duration::from_millis(10)), None);
        let f = fetch.clone();
        let p = path.clone();
        std::thread::spawn(move || f.finished(Ok(p)));
        assert_eq!(fetch.wait(Duration::from_secs(5)), Some(Ok(path)));
    }

    #[test]
    fn a_failed_download_is_reported_and_nothing_is_saved() {
        let tmp = TempDir::new("failed");
        let fetch = Fetch::new(tmp.0.join("drop"), false);
        fetch.finished(Err(std::io::Error::other("connection reset")));
        fetch.then(After::SaveToDropFolder);
        assert_eq!(fetch.wait(Duration::ZERO), Some(Err("connection reset".to_string())));
        assert!(!tmp.0.join("drop").exists());
    }

    #[test]
    fn old_drag_folders_are_pruned() {
        let tmp = TempDir::new("prune");
        let dir = new_drag_dir(&tmp.0);
        std::fs::create_dir_all(&dir).unwrap();
        prune_old_drags(&tmp.0, Duration::from_secs(3600));
        assert!(dir.exists(), "a fresh folder is kept");
        prune_old_drags(&tmp.0, Duration::ZERO);
        assert!(!dir.exists());
    }
}
