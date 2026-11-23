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
// thread reaches it through the main dispatch queue.

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

/// Set once the view has turned the synthetic press into a drag.
static STARTED: AtomicBool = AtomicBool::new(false);
/// Set while that drag is running.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether a drag started here is still running.
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

pub struct Ivars {
    paths: RefCell<Vec<PathBuf>>,
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

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _point: NSPoint, operation: NSDragOperation) {
            ACTIVE.store(false, Ordering::SeqCst);
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
    fn new(mtm: MainThreadMarker, frame: NSRect, paths: Vec<PathBuf>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars { paths: RefCell::new(paths) });
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
        let session = self.beginDraggingSessionWithItems_event_source(&items, event, ProtocolObject::from_ref(self));
        session.setAnimatesToStartingPositionsOnCancelOrFail(true);
        ACTIVE.store(true, Ordering::SeqCst);
        STARTED.store(true, Ordering::SeqCst);
        // Out of the way of the drop: invisible, and not a target for it.
        if let Some(window) = self.window() {
            window.setIgnoresMouseEvents(true);
            window.setAlphaValue(0.0);
        }
    }
}

thread_local! {
    // Main thread only: the panel currently (or last) used for a drag.
    static PANEL: RefCell<Option<Retained<NSPanel>>> = const { RefCell::new(None) };
}

unsafe extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(queue: *const c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

/// Runs `f` on the main thread and waits up to `timeout` for its result.
fn on_main<R: Send + 'static>(timeout: Duration, f: impl FnOnce(MainThreadMarker) -> R + Send + 'static) -> Option<R> {
    type Job = Box<dyn FnOnce() + Send>;
    extern "C" fn run(context: *mut c_void) {
        // SAFETY: `context` is the Box<Job> leaked below, run exactly once.
        let job = unsafe { Box::from_raw(context as *mut Job) };
        job();
    }
    let (tx, rx) = mpsc::channel();
    let job: Box<Job> = Box::new(Box::new(move || {
        let mtm = MainThreadMarker::new().expect("the main dispatch queue runs on the main thread");
        let _ = tx.send(f(mtm));
    }));
    // SAFETY: the main queue is a valid queue for the life of the process.
    unsafe { dispatch_async_f(&raw const _dispatch_main_q, Box::into_raw(job) as *mut c_void, run) };
    rx.recv_timeout(timeout).ok()
}

/// Puts the drag panel for `paths` under `cursor` (Quartz global
/// coordinates: origin at the main display's top left, y down). The caller
/// then posts a left-button press there and checks `wait_started`.
pub fn place_under_cursor(paths: &[PathBuf], cursor: (f64, f64), main_display_height: f64) -> bool {
    STARTED.store(false, Ordering::SeqCst);
    let paths = paths.to_vec();
    // Cocoa screen coordinates have their origin at the main display's
    // bottom left, y up.
    let center = NSPoint::new(cursor.0, main_display_height - cursor.1);
    on_main(Duration::from_secs(1), move |mtm| {
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
        let view = DragSourceView::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), frame.size), paths);
        panel.setContentView(Some(&view));
        panel.orderFrontRegardless();
        PANEL.with(|p| {
            if let Some(old) = p.borrow_mut().replace(panel) {
                old.orderOut(None);
            }
        });
    })
    .is_some()
}

/// Waits up to `timeout` for the panel to have started the drag.
pub fn wait_started(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if STARTED.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    STARTED.load(Ordering::SeqCst)
}

/// Takes the panel away again, for when the drag never started.
pub fn remove_panel() {
    on_main(Duration::from_secs(1), |_| {
        PANEL.with(|p| {
            if let Some(panel) = p.borrow_mut().take() {
                panel.orderOut(None);
            }
        });
    });
}
