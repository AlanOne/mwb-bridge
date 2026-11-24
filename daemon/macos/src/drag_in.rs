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
use std::time::{Duration, Instant, SystemTime};

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
    paths: Vec<PathBuf>,
    outcome: Option<Result<Vec<PathBuf>, String>>,
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

    /// The fetching thread created a (still empty) destination file.
    pub fn created(&self, path: &Path) {
        self.state.lock().unwrap().paths.push(path.to_path_buf());
    }

    /// The fetching thread is done, one way or the other.
    pub fn finished(&self, result: std::io::Result<Vec<PathBuf>>) {
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

    /// Where the files are being written, as far as that's known yet.
    pub fn paths(&self) -> Vec<PathBuf> {
        self.state.lock().unwrap().paths.clone()
    }

    /// Waits up to `timeout` for the download to end: the files' paths, or
    /// why it failed. `None` if it's still going.
    pub fn wait(&self, timeout: Duration) -> Option<Result<Vec<PathBuf>, String>> {
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

    fn apply(&self, after: After, outcome: &Result<Vec<PathBuf>, String>) {
        let paths = match outcome {
            Ok(paths) => paths,
            Err(e) => {
                eprintln!("[drop] Fetching the dragged files from Windows failed: {e}");
                return;
            }
        };
        match after {
            After::Keep => {}
            After::Discard => {
                for path in paths {
                    let _ = std::fs::remove_file(path);
                }
                if let Some(dir) = paths.first().and_then(|p| p.parent()) {
                    let _ = std::fs::remove_dir(dir); // only if empty
                }
            }
            After::SaveToDropFolder => {
                let mut saved = Vec::new();
                for path in paths {
                    match save_to(&self.drop_dir, path) {
                        Ok(dest) => saved.push(dest),
                        Err(e) => eprintln!("[drop] Couldn't save {} to {}: {e}", path.display(), self.drop_dir.display()),
                    }
                }
                println!("[drop] Saved {} file(s) to {} (released before they could be handed to an app).", saved.len(), self.drop_dir.display());
                if self.reveal
                    && !saved.is_empty()
                    && let Err(e) = std::process::Command::new("open").arg("-R").args(&saved).status()
                {
                    eprintln!("[drop] Couldn't reveal them in Finder: {e}");
                }
            }
        }
    }
}

/// The drag from Windows currently over this Mac, if any, as the network
/// thread drives it: one method per thing that happens to it.
#[derive(Default)]
pub struct IncomingDrag {
    current: Option<Incoming>,
    // Let go of over this Mac before the files had arrived: the release
    // is held back (the macOS drag stays where it was let go) until they
    // have, or until EARLY_RELEASE_WAIT runs out.
    held_release: Option<(Arc<Fetch>, std::time::Instant)>,
}

struct Incoming {
    fetch: Arc<Fetch>,
    // How many files the drag carries: its macOS drag starts once they all
    // exist on disk.
    expected: usize,
    // A macOS drag of the file is running (the left button is held here).
    dragging: bool,
    // Starting one failed, so don't try again for this drag.
    gave_up: bool,
    // A macOS drag being started, step by step (see `Start`).
    starting: Option<Start>,
}

/// Starting a macOS drag takes a few steps that each wait on the main
/// thread or the window server. They're advanced from later input and the
/// input loop's ticks rather than waited for, so input never stalls; mouse
/// moves aren't posted meanwhile, so the press lands on the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Start {
    /// The panel is queued to go under the cursor.
    Placing { generation: u64, since: Instant },
    /// It's on screen; the press follows once it has had time to show.
    Placed { generation: u64, since: Instant },
    /// The press is posted; waiting for the panel to start the drag.
    Pressed { generation: u64, since: Instant },
}

// How long the main thread gets to put the panel on screen, the panel to
// reach the screen before the press, and the press to start the drag.
const PLACE_TIMEOUT: Duration = Duration::from_secs(1);
const SHOW_DELAY: Duration = Duration::from_millis(40);
const START_TIMEOUT: Duration = Duration::from_millis(300);

/// What a starting drag does next, given how far it got and for how long.
#[derive(Debug, PartialEq, Eq)]
enum StartStep {
    Wait,
    /// The panel is up and has had time to show: press on it.
    Press,
    /// The drag is running.
    Started,
    /// Give up: the files are saved instead when dropped.
    GiveUp,
}

fn start_step(start: Start, placed: bool, started: bool) -> StartStep {
    match start {
        Start::Placing { since, .. } if !placed && since.elapsed() >= PLACE_TIMEOUT => StartStep::GiveUp,
        Start::Placing { .. } => StartStep::Wait,
        Start::Placed { since, .. } if since.elapsed() >= SHOW_DELAY => StartStep::Press,
        Start::Placed { .. } => StartStep::Wait,
        Start::Pressed { .. } if started => StartStep::Started,
        Start::Pressed { since, .. } if since.elapsed() >= START_TIMEOUT => StartStep::GiveUp,
        Start::Pressed { .. } => StartStep::Wait,
    }
}

// How long a release let go before the files have arrived is held back
// waiting for them (an app reads the files when they're dropped). After
// that the drag is cancelled and the files are saved to the drop folder
// once they arrive, rather than keeping the cursor frozen.
pub const EARLY_RELEASE_WAIT: Duration = Duration::from_secs(3);

/// What to do about a held-back release, given the download's outcome so
/// far and how long the release has been held.
#[derive(Debug, PartialEq, Eq)]
pub enum ReleaseStep {
    Wait,
    /// Post the release: the files are in, drop them into the app.
    Drop,
    /// Cancel the macOS drag, then this for the files.
    Cancel(After),
}

pub fn release_step(outcome: &Option<Result<Vec<PathBuf>, String>>, waited: Duration) -> ReleaseStep {
    match outcome {
        Some(Ok(_)) => ReleaseStep::Drop,
        Some(Err(_)) => ReleaseStep::Cancel(After::Discard),
        None if waited >= EARLY_RELEASE_WAIT => ReleaseStep::Cancel(After::SaveToDropFolder),
        None => ReleaseStep::Wait,
    }
}
// Drag folders older than this are deleted when a new drag starts.
const KEEP_DRAGS: Duration = Duration::from_secs(24 * 3600);

impl IncomingDrag {
    /// A file dragged on Windows reached this Mac: start fetching it.
    pub fn start(&mut self, cfg: &Config, peer_addr: Option<SocketAddr>) {
        let root = crate::clipboard::dragged_files_dir();
        prune_old_drags(&root, KEEP_DRAGS);
        let dir = new_drag_dir(&root);
        let fetch = Fetch::new(crate::clipboard::dropped_files_dir(), true);
        self.current = Some(Incoming { fetch: fetch.clone(), expected: 1, dragging: false, gave_up: false, starting: None });
        let cfg = cfg.clone();
        std::thread::spawn(move || {
            let result = crate::clipboard::fetch_dragged_file(&cfg, peer_addr, &dir, |path| fetch.created(path));
            if let Ok(path) = &result {
                let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                println!("[drop] Fetched {} ({size} bytes) from Windows.", path.display());
            }
            fetch.finished(result.map(|p| vec![p]));
        });
    }

    /// Files dragged on Windows reached this Mac through the companion,
    /// which knows every file of a drag: fetch them all from it.
    pub fn start_companion(&mut self, link: &Arc<crate::companion::Link>, offer: mwb_protocol::companion::Offer) {
        let root = crate::clipboard::dragged_files_dir();
        prune_old_drags(&root, KEEP_DRAGS);
        let dir = new_drag_dir(&root);
        let fetch = Fetch::new(crate::clipboard::dropped_files_dir(), true);
        let expected = offer.files.len();
        self.current = Some(Incoming { fetch: fetch.clone(), expected, dragging: false, gave_up: false, starting: None });
        let link = link.clone();
        std::thread::spawn(move || {
            let result = link.fetch_files(&offer, &dir, |path| fetch.created(path));
            if let Ok(paths) = &result {
                println!("[drop] Fetched {} file(s) from the companion.", paths.len());
            }
            fetch.finished(result);
        });
    }

    /// A mouse move is about to be posted: once the files exist on disk,
    /// start a macOS drag of them under the cursor (see `Start`).
    pub fn on_move(&mut self, cg: &mut CgInput) {
        let Some(incoming) = self.current.as_mut() else { return };
        // The macOS drag ended on its own (it failed, or began and ended at
        // once) while the button is still held: let go, so this and later
        // moves aren't drags of whatever is under the cursor. The release
        // then saves the files to the drop folder.
        if incoming.dragging && !crate::drag_source::is_active() {
            eprintln!("[drop] The drag on this Mac ended by itself, the files will be saved instead when dropped.");
            cg.button(BTN_LEFT, false);
            incoming.dragging = false;
            incoming.gave_up = true;
            return;
        }
        if incoming.starting.is_some() {
            self.advance_start(cg);
            return;
        }
        if incoming.dragging || incoming.gave_up || cg.left_button_held() {
            return;
        }
        let paths = incoming.fetch.paths();
        if paths.len() < incoming.expected {
            return;
        }
        let main_height = core_graphics::display::CGDisplay::main().bounds().size.height;
        let generation = crate::drag_source::place_under_cursor(&paths, cg.cursor(), main_height);
        incoming.starting = Some(Start::Placing { generation, since: Instant::now() });
    }

    /// Whether a drag is being started: mouse moves mustn't be posted
    /// meanwhile, or the press would miss the panel.
    pub fn starting(&self) -> bool {
        self.current.as_ref().is_some_and(|i| i.starting.is_some())
    }

    /// Takes a starting drag one step further, if it's ready for one.
    fn advance_start(&mut self, cg: &mut CgInput) {
        let Some(incoming) = self.current.as_mut() else { return };
        let Some(start) = incoming.starting else { return };
        let generation = match start {
            Start::Placing { generation, .. } | Start::Placed { generation, .. } | Start::Pressed { generation, .. } => generation,
        };
        let placed = crate::drag_source::placed(generation);
        if let Start::Placing { .. } = start
            && placed
        {
            incoming.starting = Some(Start::Placed { generation, since: Instant::now() });
            return;
        }
        match start_step(start, placed, crate::drag_source::started(generation)) {
            StartStep::Wait => {}
            StartStep::Press => {
                cg.button(BTN_LEFT, true);
                incoming.starting = Some(Start::Pressed { generation, since: Instant::now() });
            }
            StartStep::Started => {
                incoming.starting = None;
                incoming.dragging = true;
                let names: Vec<String> = incoming.fetch.paths().iter().map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect();
                println!("[drop] Dragging {} on this Mac; let go over any app to drop it there.", names.join(", "));
            }
            StartStep::GiveUp => {
                if Self::abandon_start(incoming, cg) {
                    incoming.dragging = true;
                } else {
                    eprintln!("[drop] The drag didn't start, the files will be saved instead when dropped.");
                    incoming.gave_up = true;
                }
            }
        }
    }

    /// Stops a drag being started, unless it started after all (true: the
    /// caller has a running drag). Otherwise lets go of the press if one
    /// was posted, and the panel goes (see `drag_source::abandon`).
    fn abandon_start(incoming: &mut Incoming, cg: &mut CgInput) -> bool {
        match incoming.starting.take() {
            Some(Start::Pressed { generation, .. }) => {
                if crate::drag_source::abandon(generation, true) {
                    return true;
                }
                cg.button(BTN_LEFT, false);
                false
            }
            Some(Start::Placing { generation, .. } | Start::Placed { generation, .. }) => crate::drag_source::abandon(generation, false),
            None => false,
        }
    }

    /// The button was released over this Mac. Called before the release is
    /// posted, which is what drops the files into the app under the cursor.
    /// Returns true when the release must not be posted yet: the files are
    /// still arriving, and `tick` posts it once they have.
    pub fn dropped(&mut self, cg: &mut CgInput) -> bool {
        let Some(mut incoming) = self.current.take() else { return false };
        // Let go while the drag was being started: nothing new is pressed
        // now. If the press is already posted, the release about to be
        // posted ends it on the panel (which then goes), unless the panel
        // started the drag meanwhile, which then drops as usual.
        match incoming.starting.take() {
            Some(Start::Pressed { generation, .. }) => incoming.dragging = crate::drag_source::abandon(generation, true),
            Some(Start::Placing { generation, .. } | Start::Placed { generation, .. }) => {
                crate::drag_source::abandon(generation, false);
            }
            None => {}
        }
        // A drag that ended by itself before this release (see `on_move`)
        // can't drop anything: the release just lets go, and the files are
        // saved instead.
        if incoming.dragging && !crate::drag_source::is_active() {
            incoming.dragging = false;
        }
        if !incoming.dragging {
            incoming.fetch.then(After::SaveToDropFolder);
            return false;
        }
        match release_step(&incoming.fetch.wait(Duration::ZERO), Duration::ZERO) {
            ReleaseStep::Drop => {
                incoming.fetch.then(After::Keep);
                false
            }
            ReleaseStep::Cancel(after) => {
                eprintln!("[drop] Fetching the files failed, cancelling the drop.");
                cg.cancel_drag();
                incoming.fetch.then(after);
                false
            }
            ReleaseStep::Wait => {
                println!("[drop] Let go before the files arrived, holding the drop for up to {}s.", EARLY_RELEASE_WAIT.as_secs());
                self.held_release = Some((incoming.fetch, std::time::Instant::now()));
                true
            }
        }
    }

    /// Whether a release is being held back: mouse moves mustn't be posted
    /// meanwhile, or the drop would land somewhere else.
    pub fn holding_release(&self) -> bool {
        self.held_release.is_some()
    }

    /// Called regularly by the input loop: finishes a held-back release
    /// once the files are in, or once it has waited long enough.
    pub fn tick(&mut self, cg: &mut CgInput) {
        self.advance_start(cg);
        let Some((_, since)) = self.held_release.as_ref() else { return };
        let waited = since.elapsed();
        self.finish_held(cg, waited);
    }

    /// Ends a held-back release now, without waiting any longer: dropped
    /// into the app if the files are in, otherwise cancelled and saved to
    /// the drop folder (or discarded if fetching them failed). For anything
    /// but a mouse move happening meanwhile (a click, a key, the wheel, a
    /// new drag) and for control leaving this Mac, since each of those
    /// would otherwise end the macOS drag behind this one's back. Leaves
    /// the left button up either way.
    pub fn settle(&mut self, cg: &mut CgInput) {
        self.finish_held(cg, EARLY_RELEASE_WAIT);
    }

    fn finish_held(&mut self, cg: &mut CgInput, waited: Duration) {
        let Some((fetch, _)) = self.held_release.as_ref() else { return };
        match release_step(&fetch.wait(Duration::ZERO), waited) {
            ReleaseStep::Wait => return,
            ReleaseStep::Drop => {
                println!("[drop] The files arrived, dropping them.");
                cg.button(BTN_LEFT, false);
                fetch.then(After::Keep);
            }
            ReleaseStep::Cancel(after) => {
                if after == After::SaveToDropFolder {
                    println!("[drop] The files are still arriving, saving them to the drop folder instead.");
                } else {
                    eprintln!("[drop] Fetching the files failed, cancelling the drop.");
                }
                cg.cancel_drag();
                fetch.then(after);
            }
        }
        self.held_release = None;
    }

    /// The drag left without a drop here.
    pub fn cancelled(&mut self, cg: &mut CgInput) {
        let Some(mut incoming) = self.current.take() else { return };
        if Self::abandon_start(&mut incoming, cg) {
            incoming.dragging = true;
        }
        if incoming.dragging && crate::drag_source::is_active() {
            cg.cancel_drag();
        } else if incoming.dragging {
            // It ended by itself: no drag to cancel (Escape would go to the
            // app in front), only the press to let go of.
            cg.button(BTN_LEFT, false);
        }
        incoming.fetch.then(After::Discard);
    }
}

impl Drop for IncomingDrag {
    // The connection ended while a release was held back. The caller
    // cancels the macOS drag (CgInput::reset); the user did let go, so the
    // files still end up in the drop folder once they arrive.
    fn drop(&mut self) {
        if let Some((fetch, _)) = self.held_release.take() {
            fetch.then(After::SaveToDropFolder);
        }
        // A drag being started is abandoned, so the panel doesn't sit under
        // the cursor catching clicks and a press still on its way doesn't
        // start a drag. A press already posted is let go by the caller with
        // `release_abandoned_press`, plainly: CgInput::reset would post
        // Escape first, which goes to whatever app has focus. One that did
        // start a drag is cancelled by the caller as any running drag is.
        // A drag that ended by itself with the press still held (see
        // `on_move`): let go plainly too.
        if self.current.as_ref().is_some_and(|i| i.dragging) && !crate::drag_source::is_active() {
            ABANDONED_PRESS.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        match self.current.as_mut().and_then(|i| i.starting.take()) {
            Some(Start::Pressed { generation, .. }) => {
                if !crate::drag_source::abandon(generation, true) {
                    ABANDONED_PRESS.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            Some(Start::Placing { generation, .. } | Start::Placed { generation, .. }) => {
                crate::drag_source::abandon(generation, false);
            }
            None => {}
        }
    }
}

// A press posted for a drag start that the session ended in the middle of.
static ABANDONED_PRESS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Lets go of a press left by a drag start the session ended in the middle
/// of (see `IncomingDrag`'s Drop). Call after the session, before
/// `CgInput::reset`.
pub fn release_abandoned_press(cg: &mut CgInput) {
    if ABANDONED_PRESS.swap(false, std::sync::atomic::Ordering::SeqCst) {
        cg.button(BTN_LEFT, false);
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
        fetch.finished(Ok(vec![path]));
        assert_eq!(std::fs::read(tmp.0.join("drop/report.pdf")).unwrap(), b"pdf");
    }

    #[test]
    fn a_drag_that_went_back_deletes_the_file_and_its_folder() {
        let tmp = TempDir::new("discard");
        let (fetch, path) = fetched(&tmp);
        fetch.finished(Ok(vec![path.clone()]));
        fetch.then(After::Discard);
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn a_drop_into_an_app_keeps_the_file() {
        let tmp = TempDir::new("keep");
        let (fetch, path) = fetched(&tmp);
        fetch.finished(Ok(vec![path.clone()]));
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
        std::thread::spawn(move || f.finished(Ok(vec![p])));
        assert_eq!(fetch.wait(Duration::from_secs(5)), Some(Ok(vec![path])));
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
    fn every_file_of_a_multi_file_drag_is_saved_or_discarded() {
        let tmp = TempDir::new("multi");
        let fetch = Fetch::new(tmp.0.join("drop"), false);
        let dir = tmp.0.join("drag");
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<PathBuf> = ["a.txt", "b.txt"].iter().map(|n| dir.join(n)).collect();
        for p in &paths {
            std::fs::write(p, b"x").unwrap();
            fetch.created(p);
        }
        assert_eq!(fetch.paths(), paths);
        fetch.finished(Ok(paths.clone()));
        fetch.then(After::SaveToDropFolder);
        assert!(tmp.0.join("drop/a.txt").exists() && tmp.0.join("drop/b.txt").exists());
        fetch.then(After::Discard);
        assert!(!dir.exists(), "both files and their folder are gone");
    }

    #[test]
    fn an_early_release_waits_then_drops_or_gives_up() {
        let done: Option<Result<Vec<PathBuf>, String>> = Some(Ok(vec![PathBuf::from("/a")]));
        let failed: Option<Result<Vec<PathBuf>, String>> = Some(Err("reset".into()));
        assert_eq!(release_step(&None, Duration::from_millis(100)), ReleaseStep::Wait);
        assert_eq!(release_step(&done, Duration::from_millis(100)), ReleaseStep::Drop);
        assert_eq!(release_step(&failed, Duration::from_millis(100)), ReleaseStep::Cancel(After::Discard));
        // Still downloading after the wait: hand control back and save the
        // files where a drop would have gone without an app.
        assert_eq!(release_step(&None, EARLY_RELEASE_WAIT), ReleaseStep::Cancel(After::SaveToDropFolder));
        // Finished just in time still drops.
        assert_eq!(release_step(&done, EARLY_RELEASE_WAIT), ReleaseStep::Drop);
    }

    #[test]
    fn a_connection_lost_during_a_held_drop_still_saves_the_files() {
        let tmp = TempDir::new("held-lost");
        let (fetch, path) = fetched(&tmp);
        let mut incoming = IncomingDrag::default();
        incoming.held_release = Some((fetch.clone(), std::time::Instant::now()));
        drop(incoming); // the session ended
        fetch.finished(Ok(vec![path]));
        assert_eq!(std::fs::read(tmp.0.join("drop/report.pdf")).unwrap(), b"pdf");
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
