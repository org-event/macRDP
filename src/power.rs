//! Power assertions: the Mac stays awake while the server runs, and its
//! display stays on while a client is connected. A connecting client wakes a
//! display that went to sleep meanwhile.

use std::time::{Duration, Instant};

use objc2_core_foundation::CFString;
use tracing::{debug, info, warn};

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: &CFString,
        level: u32,
        name: &CFString,
        id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
    fn IOPMAssertionDeclareUserActivity(name: &CFString, user_type: u32, id: *mut u32) -> i32;
}

/// `kIOPMUserActiveLocal`: as if someone touched the keyboard.
const USER_ACTIVE_LOCAL: u32 = 0;
/// How long a waking display gets before capture is tried anyway.
const WAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Wakes the display if it sleeps and waits for it: a sleeping display can be
/// neither captured nor mirrored, and no virtual display can be added.
pub fn wake_display() {
    let main = objc2_core_graphics::CGMainDisplayID();
    if !objc2_core_graphics::CGDisplayIsAsleep(main) {
        return;
    }
    info!("display asleep; waking it for the client");
    let mut id = 0;
    // SAFETY: valid CF string and out-pointer.
    let status = unsafe {
        IOPMAssertionDeclareUserActivity(
            &CFString::from_str("macRDP client connecting"),
            USER_ACTIVE_LOCAL,
            &mut id,
        )
    };
    if status != 0 {
        warn!(status, "cannot wake the display");
        return;
    }
    let started = Instant::now();
    while objc2_core_graphics::CGDisplayIsAsleep(main) && started.elapsed() < WAKE_TIMEOUT {
        std::thread::sleep(Duration::from_millis(100));
    }
    // Displays report awake a moment before they accept configuration.
    std::thread::sleep(Duration::from_millis(500));
    debug!(ms = started.elapsed().as_millis(), "display awake");
    // SAFETY: the assertion created above; it would also time out on its own.
    unsafe { IOPMAssertionRelease(id) };
}

const ASSERTION_LEVEL_ON: u32 = 255;

/// Held until dropped.
pub struct Awake {
    id: u32,
}

impl Awake {
    /// Keeps the system from idle sleep, so the server stays reachable.
    pub fn system() -> Option<Self> {
        Self::new("PreventUserIdleSystemSleep", "macRDP server is running")
    }

    /// Keeps the display from idle sleep, so a remote session does not go dark.
    pub fn display() -> Option<Self> {
        Self::new("PreventUserIdleDisplaySleep", "macRDP client is connected")
    }

    fn new(kind: &str, reason: &str) -> Option<Self> {
        let mut id = 0;
        // SAFETY: valid CF strings and out-pointer.
        let status = unsafe {
            IOPMAssertionCreateWithName(
                &CFString::from_str(kind),
                ASSERTION_LEVEL_ON,
                &CFString::from_str(reason),
                &mut id,
            )
        };
        if status != 0 {
            warn!(kind, status, "cannot create a power assertion");
            return None;
        }
        debug!(kind, "power assertion held");
        Some(Self { id })
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        // SAFETY: an assertion this value created.
        unsafe { IOPMAssertionRelease(self.id) };
    }
}
