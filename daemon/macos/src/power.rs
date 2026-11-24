// Keeping the Mac reachable from Windows.
//
// When the Mac's display sleeps after its idle time, macOS also puts the
// whole Mac to sleep before long, and always does in clamshell mode (lid
// closed, external display). A sleeping Mac drops its connection to
// Windows, so nothing from Windows can wake it again: only its own keyboard
// or opening the lid can. So while the bridge is connected and the Mac is
// on power, it holds a "no idle system sleep" assertion: the display still
// turns off after its idle time (and input from Windows wakes it again:
// `cg_input::input_posted`), but the Mac stays awake behind it. A reconnect
// (after a network blip, or Windows restarting) doesn't count as
// disconnected: let go of in the gap, the assertion would let a Mac whose
// display is off sleep at once, before the bridge is back. On battery it doesn't, so
// an unattended laptop still sleeps. Closing the lid with no external
// display still sleeps it: only `pmset` (as root) can change that.

use std::time::{Duration, Instant};

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
const CHECK_EVERY: Duration = Duration::from_secs(2);
/// How often the power source is looked at.
const POWER_CHECK_EVERY: Duration = Duration::from_secs(20);
/// How long the bridge may be disconnected before the Mac may sleep again:
/// longer than a reconnect takes (up to ~15s trying addresses, plus up to
/// 4s of backoff).
const RECONNECT_GRACE: Duration = Duration::from_secs(45);

/// Whether the Mac is running on mains power (a desktop Mac always is),
/// counting a UPS as power. `None` when macOS couldn't say.
fn on_power() -> Option<bool> {
    // SAFETY: the snapshot is released below; the type string belongs to
    // it (a "Get" result), so it's read before that.
    unsafe {
        let snapshot = IOPSCopyPowerSourcesInfo();
        if snapshot.is_null() {
            return None;
        }
        let source = IOPSGetProvidingPowerSourceType(snapshot);
        // "AC Power", "UPS Power" or "Battery Power"; none on a Mac without
        // a battery.
        let powered = source.is_null() || CFString::wrap_under_get_rule(source) != "Battery Power";
        CFRelease(snapshot);
        Some(powered)
    }
}

/// Holds the "no idle system sleep" assertion while `connected()` says the
/// bridge is connected (or was, less than `RECONNECT_GRACE` ago) and the
/// Mac is on power. Runs forever; start it on its own thread.
pub fn keep_awake_while_connected(connected: impl Fn() -> bool) {
    let kind = CFString::new("PreventUserIdleSystemSleep");
    let name = CFString::new("MWB Mac Bridge: connected to Windows");
    let mut held: Option<u32> = None;
    let mut last_connected: Option<Instant> = None;
    let mut powered = true;
    let mut power_checked: Option<Instant> = None;
    loop {
        if connected() {
            last_connected = Some(Instant::now());
        }
        if power_checked.is_none_or(|t| t.elapsed() >= POWER_CHECK_EVERY) {
            // Unknown keeps the last answer.
            powered = on_power().unwrap_or(powered);
            power_checked = Some(Instant::now());
        }
        let recently_connected = last_connected.is_some_and(|t| t.elapsed() < RECONNECT_GRACE);
        let want = recently_connected && powered;
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
