// Windows -> Mac drag into any app: starting a real macOS drag of the file.
//
// A drag can only be started by an app's own view, from a mouse-down
// inside it. So, the same trick real MWB uses on Windows with its helper
// window: a tiny invisible panel is placed under the cursor, one synthetic
// left-button press is posted onto it (`CgInput::button`), and its view
// turns that press into a drag of the file. From then on the button stays
// held, the moves forwarded from Windows are posted as drags, and releasing
// the button on Windows drops the file on whatever app is under the cursor.
// The panel steps out of the way (invisible, click-through) as soon as the
// drag starts, so it can't be the drop target itself.
//
// AppKit only runs on the main thread (the menu bar app's), so the network
// thread reaches it through the main dispatch queue, without waiting: it
// checks `placed` and `started` on later input or ticks instead, so input
// never stalls behind the main thread.

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEvent, NSPanel, NSResponder, NSScreenSaverWindowLevel, NSView, NSWindowCollectionBehavior,
    NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL};

const PANEL_SIZE: f64 = 64.0;

/// Each placement's number, so a late answer from an earlier one isn't
/// taken for the current one's.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// The latest placement whose panel is on screen.
static PLACED: AtomicU64 = AtomicU64::new(0);
/// What became of the latest placement's press, decided exactly once: the
/// view starting the drag and the network thread abandoning it race, and
/// whichever comes first wins (see `abandon`).
static OUTCOME: Mutex<(u64, Outcome)> = Mutex::new((0, Outcome::Pending));

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Waiting for the press to reach the panel.
    Pending,
    /// The press asked AppKit for a drag session, which begins on the main
    /// thread's next turn (`draggingSession:willBeginAtPoint:`).
    Arming,
    /// The session has begun.
    Started,
    /// The network thread gave up first: no session will be asked for.
    Abandoned,
}
/// Set while that drag is running.
// The placement number of the drag running now (0: none). Only that
// drag's own `ended` clears it: an earlier drag's, which can come late
// (its snap-back animation runs first), must not.
static ACTIVE: AtomicU64 = AtomicU64::new(0);

/// Whether a drag started here is still running.
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::SeqCst) != 0
}

pub struct Ivars {
    paths: RefCell<Vec<PathBuf>>,
    generation: u64,
    // A drag session was asked for and hasn't ended yet: the panel stays
    // until it has (`ended` takes it away), so the session never ends over
    // an app it wasn't meant for.
    session_asked: std::cell::Cell<bool>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements beyond calling its
    // designated initializer (initWithFrame:, done in `new`), and this
    // class doesn't implement Drop.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MWBBridgeDragSourceView"]
    #[ivars = Ivars]
    struct DragSourceView;

    impl DragSourceView {
        // The panel belongs to a background app that never becomes active,
        // so the press must count even though its window isn't key.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.start_drag(event);
        }

        // The release of a press that didn't start a drag (it was
        // abandoned): the panel's job is done.
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            if !self.ivars().session_asked.get()
                && let Some(window) = self.window()
            {
                window.orderOut(None);
            }
        }
    }

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for DragSourceView {}

    // SAFETY: the method signatures match NSDraggingSource's.
    unsafe impl NSDraggingSource for DragSourceView {
        // Copy only: dropping into a Finder folder on the same volume as
        // the cache would otherwise move the file out of it.
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn source_operation_mask(&self, _session: &NSDraggingSession, _context: NSDraggingContext) -> NSDragOperation {
            NSDragOperation::Copy
        }

        // The drag has really begun: only now is it "started" for the
        // network thread, which holds mouse moves back until then (posted
        // earlier, a move would reach the app below as a drag of its own).
        #[unsafe(method(draggingSession:willBeginAtPoint:))]
        fn will_begin(&self, _session: &NSDraggingSession, _point: NSPoint) {
            let mut outcome = OUTCOME.lock().unwrap();
            if *outcome != (self.ivars().generation, Outcome::Arming) {
                // The network thread gave up waiting and let go: the panel
                // stays under the cursor, so this session ends over it
                // (not a drop target) rather than over an app.
                return;
            }
            *outcome = (self.ivars().generation, Outcome::Started);
            ACTIVE.store(self.ivars().generation, Ordering::SeqCst);
            drop(outcome);
            // Out of the way of the drop: invisible, and not a target for it.
            if let Some(window) = self.window() {
                window.setIgnoresMouseEvents(true);
                window.setAlphaValue(0.0);
            }
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _point: NSPoint, operation: NSDragOperation) {
            let _ = ACTIVE.compare_exchange(self.ivars().generation, 0, Ordering::SeqCst, Ordering::SeqCst);
            self.ivars().session_asked.set(false);
            // Over, whether or not it began: a network thread that hasn't
            // seen it start yet must not take it for a running drag.
            let mut outcome = OUTCOME.lock().unwrap();
            if outcome.0 == self.ivars().generation && matches!(outcome.1, Outcome::Arming | Outcome::Started) {
                outcome.1 = Outcome::Abandoned;
            }
            drop(outcome);
            let name = self.ivars().paths.borrow().iter().map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect::<Vec<_>>().join(", ");
            if operation.is_empty() {
                println!("[drop] {name} wasn't dropped anywhere that takes it (or the drag was cancelled).");
            } else {
                println!("[drop] {name} dropped into an app.");
            }
            if let Some(window) = self.window() {
                window.orderOut(None);
            }
        }
    }
);

impl DragSourceView {
    fn new(mtm: MainThreadMarker, frame: NSRect, paths: Vec<PathBuf>, generation: u64) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars { paths: RefCell::new(paths), generation, session_asked: std::cell::Cell::new(false) });
        // SAFETY: initWithFrame: is NSView's designated initializer.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn start_drag(&self, event: &NSEvent) {
        let paths = self.ivars().paths.borrow().clone();
        let at = self.convertPoint_fromView(event.locationInWindow(), None);
        // One item per file, each a few points further along so several
        // read as a stack, as in a Finder drag.
        let items: Vec<Retained<NSDraggingItem>> = paths
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let ns_path = NSString::from_str(&path.to_string_lossy());
                let url = NSURL::fileURLWithPath(&ns_path);
                let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), ProtocolObject::from_ref(&*url));
                let icon = NSWorkspace::sharedWorkspace().iconForFile(&ns_path);
                icon.setSize(NSSize::new(PANEL_SIZE, PANEL_SIZE));
                let offset = (i.min(4) * 6) as f64;
                let frame = NSRect::new(
                    NSPoint::new(at.x - PANEL_SIZE / 2.0 + offset, at.y - PANEL_SIZE / 2.0 - offset),
                    NSSize::new(PANEL_SIZE, PANEL_SIZE),
                );
                let contents: &AnyObject = &icon;
                // SAFETY: the contents of a dragging frame may be an NSImage.
                unsafe { item.setDraggingFrame_contents(frame, Some(contents)) };
                item
            })
            .collect();
        let items = NSArray::from_retained_slice(&items);
        // Only while the network thread still wants this drag: a press that
        // arrives after it gave up (a release, a disconnect) must not start
        // one. Decided under the lock together with starting the session,
        // so `started` only ever means a session exists, and `abandon`
        // either sees one or stops one from being started.
        let mut outcome = OUTCOME.lock().unwrap();
        if *outcome != (self.ivars().generation, Outcome::Pending) {
            return;
        }
        *outcome = (self.ivars().generation, Outcome::Arming);
        self.ivars().session_asked.set(true);
        let session = self.beginDraggingSessionWithItems_event_source(&items, event, ProtocolObject::from_ref(self));
        session.setAnimatesToStartingPositionsOnCancelOrFail(true);
    }
}

/// A placement's number, its panel and the panel's view.
type Panel = (u64, Retained<NSPanel>, Retained<DragSourceView>);

thread_local! {
    // Main thread only: the panel currently (or last) used for a drag, its
    // placement's number and its view.
    static PANEL: RefCell<Option<Panel>> = const { RefCell::new(None) };
}

unsafe extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(queue: *const c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_after_f(when: u64, queue: *const c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_time(when: u64, delta: i64) -> u64;
}
const DISPATCH_TIME_NOW: u64 = 0;

/// Queues `f` to run on the main thread after `delay`, without waiting.
fn on_main_after(delay: std::time::Duration, f: impl FnOnce(MainThreadMarker) + Send + 'static) {
    let job: Box<Box<dyn FnOnce() + Send>> = Box::new(Box::new(move || {
        let mtm = MainThreadMarker::new().expect("the main dispatch queue runs on the main thread");
        f(mtm);
    }));
    let when = unsafe { dispatch_time(DISPATCH_TIME_NOW, delay.as_nanos().min(i64::MAX as u128) as i64) };
    // SAFETY: the main queue is a valid queue for the life of the process,
    // and `run_job` takes back the box leaked here, exactly once.
    unsafe { dispatch_after_f(when, &raw const _dispatch_main_q, Box::into_raw(job) as *mut c_void, run_job) };
}

extern "C" fn run_job(context: *mut c_void) {
    // SAFETY: `context` is a Box<Box<dyn FnOnce>> leaked by on_main or
    // on_main_after, run exactly once.
    let job = unsafe { Box::from_raw(context as *mut Box<dyn FnOnce() + Send>) };
    job();
}

/// Queues `f` to run on the main thread, without waiting for it. Jobs run
/// in the order they were queued.
fn on_main(f: impl FnOnce(MainThreadMarker) + Send + 'static) {
    let job: Box<Box<dyn FnOnce() + Send>> = Box::new(Box::new(move || {
        let mtm = MainThreadMarker::new().expect("the main dispatch queue runs on the main thread");
        f(mtm);
    }));
    // SAFETY: the main queue is a valid queue for the life of the process,
    // and `run_job` takes back the box leaked here, exactly once.
    unsafe { dispatch_async_f(&raw const _dispatch_main_q, Box::into_raw(job) as *mut c_void, run_job) };
}

/// Queues putting the drag panel for `paths` under `cursor` (Quartz global
/// coordinates: origin at the main display's top left, y down), and
/// returns the placement's number. Once `placed` says it's on screen, the
/// caller posts a left-button press there and watches `started`.
pub fn place_under_cursor(paths: &[PathBuf], cursor: (f64, f64), main_display_height: f64) -> u64 {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    *OUTCOME.lock().unwrap() = (generation, Outcome::Pending);
    let paths = paths.to_vec();
    // Cocoa screen coordinates have their origin at the main display's
    // bottom left, y up.
    let center = NSPoint::new(cursor.0, main_display_height - cursor.1);
    on_main(move |mtm| {
        let frame = NSRect::new(NSPoint::new(center.x - PANEL_SIZE / 2.0, center.y - PANEL_SIZE / 2.0), NSSize::new(PANEL_SIZE, PANEL_SIZE));
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: the panel is kept alive in PANEL, not released on close.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setLevel(NSScreenSaverWindowLevel);
        panel.setHidesOnDeactivate(false);
        panel.setCollectionBehavior(NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::FullScreenAuxiliary);
        panel.setOpaque(false);
        panel.setHasShadow(false);
        // Nearly transparent, and explicitly not ignoring the mouse: a fully
        // transparent window lets clicks fall through to what's below.
        panel.setBackgroundColor(Some(&NSColor::colorWithCalibratedWhite_alpha(0.0, 0.01)));
        panel.setIgnoresMouseEvents(false);
        let view = DragSourceView::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), frame.size), paths, generation);
        panel.setContentView(Some(&view));
        panel.orderFrontRegardless();
        PANEL.with(|p| {
            if let Some((_, old, _)) = p.borrow_mut().replace((generation, panel, view)) {
                old.orderOut(None);
            }
        });
        PLACED.store(generation, Ordering::SeqCst);
    });
    generation
}

/// Whether placement `generation`'s panel is on screen.
pub fn placed(generation: u64) -> bool {
    PLACED.load(Ordering::SeqCst) == generation
}

/// Whether placement `generation`'s panel has started its drag.
pub fn started(generation: u64) -> bool {
    *OUTCOME.lock().unwrap() == (generation, Outcome::Started)
}

/// Gives up on placement `generation`'s drag, unless a drag session of it
/// has really begun: true if it has (the caller then has a real drag on its
/// hands, which Escape cancels), false if it's now abandoned (the caller
/// just lets go of any press it posted: a press still on its way won't
/// start a drag, and no Escape is needed). The panel goes right away when
/// no press was posted (`pressed` false), otherwise once that press is over
/// (its mouseUp, or a while later if that never comes), so the press can't
/// fall through to the app below; a panel whose session was asked for goes
/// when that session ends.
pub fn abandon(generation: u64, pressed: bool) -> bool {
    // A session already asked for begins on the main thread's next turn
    // (normally within a millisecond or two): wait briefly for it, and it's
    // a drag like any other. If the main thread is stuck that long, the
    // press is let go instead, and the session, if it still begins, ends
    // over the panel (see `will_begin`).
    let deadline = std::time::Instant::now() + ARMING_WAIT;
    loop {
        let mut outcome = OUTCOME.lock().unwrap();
        if outcome.0 != generation {
            break;
        }
        match outcome.1 {
            Outcome::Started => return true,
            Outcome::Arming if std::time::Instant::now() < deadline => {}
            Outcome::Arming | Outcome::Pending | Outcome::Abandoned => {
                outcome.1 = Outcome::Abandoned;
                break;
            }
        }
        drop(outcome);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    if pressed {
        on_main_after(PRESS_SETTLE, move |_| remove_panel_now(generation));
    } else {
        on_main(move |_| remove_panel_now(generation));
    }
    false
}

// How long `abandon` waits for an asked-for session to begin.
const ARMING_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

// How long an abandoned press gets to reach the panel before it goes.
const PRESS_SETTLE: std::time::Duration = std::time::Duration::from_millis(500);

/// Takes placement `generation`'s panel off the screen, if it's still the
/// current one.
fn remove_panel_now(generation: u64) {
    PANEL.with(|p| {
        let mut current = p.borrow_mut();
        // A panel whose session was asked for goes when that session ends.
        if current.as_ref().is_some_and(|(g, _, view)| *g == generation && !view.ivars().session_asked.get())
            && let Some((_, panel, _)) = current.take()
        {
            panel.orderOut(None);
        }
    });
}
