// Keeping the Mac reachable from Windows.
//
// When the Mac's display sleeps after its idle time, macOS also puts the
// whole Mac to sleep before long, and always does in clamshell mode (lid
// closed, external display). A sleeping Mac drops its connection to
// Windows, so nothing from Windows can wake it again: only its own keyboard
// or opening the lid can. So while the bridge is connected and the Mac is
// on power, it holds a "no idle system sleep" assertion: the display still
// turns off after its idle time (and wakes as soon as input arrives from
// Windows), but the Mac stays awake behind it. On battery it doesn't, so
// an unattended laptop still sleeps. Closing the lid with no external
// display still sleeps it: only `pmset` (as root) can change that.

use std::time::Duration;

use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_foundation::string::{CFString, CFStringRef};

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(assertion_type: CFStringRef, level: u32, name: CFStringRef, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    fn IOPSGetProvidingPowerSourceType(snapshot: CFTypeRef) -> CFStringRef;
}

const ASSERTION_LEVEL_ON: u32 = 255;
const CHECK_EVERY: Duration = Duration::from_secs(20);

/// Whether the Mac is running on mains power (a desktop Mac always is).
fn on_power() -> bool {
    // SAFETY: the snapshot is released below; the type string belongs to
    // it (a "Get" result), so it's read before that.
    unsafe {
        let snapshot = IOPSCopyPowerSourcesInfo();
        if snapshot.is_null() {
            return true;
        }
        let source = IOPSGetProvidingPowerSourceType(snapshot);
        let ac = source.is_null() || CFString::wrap_under_get_rule(source) == "AC Power";
        CFRelease(snapshot);
        ac
    }
}

/// Holds the "no idle system sleep" assertion while `connected()` says the
/// bridge is connected and the Mac is on power, checking every 20 seconds.
/// Runs forever; start it on its own thread.
pub fn keep_awake_while_connected(connected: impl Fn() -> bool) {
    let kind = CFString::new("PreventUserIdleSystemSleep");
    let name = CFString::new("MWB Mac Bridge: connected to Windows");
    let mut held: Option<u32> = None;
    loop {
        let want = connected() && on_power();
        match (want, held) {
            (true, None) => {
                let mut id = 0;
                // SAFETY: valid CFStrings and a pointer to a u32 for the ID.
                let result = unsafe { IOPMAssertionCreateWithName(kind.as_concrete_TypeRef(), ASSERTION_LEVEL_ON, name.as_concrete_TypeRef(), &mut id) };
                if result == 0 {
                    println!("[power] Connected and on power: keeping the Mac awake (the display can still sleep).");
                    held = Some(id);
                } else {
                    eprintln!("[power] Couldn't keep the Mac awake: IOKit error {result:#x}");
                }
            }
            (false, Some(id)) => {
                // SAFETY: an assertion this function created.
                unsafe { IOPMAssertionRelease(id) };
                println!("[power] Disconnected or on battery: the Mac may sleep again.");
                held = None;
            }
            _ => {}
        }
        std::thread::sleep(CHECK_EVERY);
    }
}
