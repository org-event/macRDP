//! Power assertions: the Mac stays awake while the server runs, and its
//! display stays on while a client is connected.

use objc2_core_foundation::CFString;
use tracing::{debug, warn};

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: &CFString,
        level: u32,
        name: &CFString,
        id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
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
