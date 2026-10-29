//! Keyboard layout switching through Text Input Sources.
//!
//! macOS does not run its input-source shortcuts for posted events, so the
//! server switches layouts itself when the client sends the shortcut.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_foundation::{CFArray, CFBoolean, CFRetained, CFString, CFType};
use tracing::{debug, warn};

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    static kTISPropertyInputSourceID: &'static CFString;
    static kTISPropertyInputSourceCategory: &'static CFString;
    static kTISPropertyInputSourceIsSelectCapable: &'static CFString;
    static kTISCategoryKeyboardInputSource: &'static CFString;

    fn TISCreateInputSourceList(properties: *const c_void, include_all: u8) -> *mut CFArray;
    fn TISCopyCurrentKeyboardInputSource() -> *mut CFType;
    fn TISGetInputSourceProperty(source: &CFType, key: &CFString) -> *const CFType;
    fn TISSelectInputSource(source: &CFType) -> i32;
}

/// Selects the enabled keyboard input source after the current one.
pub fn select_next() {
    let sources = keyboard_sources();
    if sources.len() < 2 {
        return;
    }
    let current = current_id();
    let index = sources
        .iter()
        .position(|source| source_id(source) == current)
        .map_or(0, |index| (index + 1) % sources.len());
    let next = &sources[index];
    // SAFETY: `next` is an input source from TISCreateInputSourceList.
    let status = unsafe { TISSelectInputSource(next) };
    if status == 0 {
        debug!(from = ?current, to = ?source_id(next), "input source switched");
    } else {
        warn!(status, "cannot switch the input source");
    }
}

/// Enabled keyboard layouts and input methods the user can select.
fn keyboard_sources() -> Vec<CFRetained<CFType>> {
    // SAFETY: a null filter lists the enabled sources; the array is ours.
    let Some(list) = NonNull::new(unsafe { TISCreateInputSourceList(std::ptr::null(), 0) }) else {
        return Vec::new();
    };
    // SAFETY: created above, so owned.
    let list = unsafe { CFRetained::from_raw(list) };
    // SAFETY: TIS input source arrays hold CF objects.
    let list = unsafe { list.cast_unchecked::<CFType>() };
    list.iter()
        .filter(|source| {
            // SAFETY: valid sources and property keys.
            let category = unsafe { property(source, kTISPropertyInputSourceCategory) };
            let selectable = unsafe { property(source, kTISPropertyInputSourceIsSelectCapable) };
            category.is_some_and(|c| c == unsafe { kTISCategoryKeyboardInputSource }.as_ref())
                && selectable
                    .and_then(|s| s.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool))
                    .unwrap_or(false)
        })
        .collect()
}

fn current_id() -> Option<String> {
    // SAFETY: the copy rule applies, so the source is ours.
    let source = NonNull::new(unsafe { TISCopyCurrentKeyboardInputSource() })?;
    let source = unsafe { CFRetained::from_raw(source) };
    source_id(&source)
}

fn source_id(source: &CFType) -> Option<String> {
    // SAFETY: valid source and property key.
    let id = unsafe { property(source, kTISPropertyInputSourceID) }?;
    id.downcast_ref::<CFString>().map(ToString::to_string)
}

/// # Safety
/// `source` is a TIS input source; the result lives as long as it does.
unsafe fn property<'a>(source: &'a CFType, key: &CFString) -> Option<&'a CFType> {
    unsafe { TISGetInputSourceProperty(source, key).as_ref() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Switches the layout on this Mac and back; run by hand.
    #[test]
    #[ignore]
    fn switches_and_back() {
        let before = current_id();
        let count = keyboard_sources().len();
        println!("current {before:?}, {count} sources");
        select_next();
        let after = current_id();
        println!("after {after:?}");
        for _ in 1..count {
            select_next();
        }
        assert_ne!(before, after);
        assert_eq!(current_id(), before);
    }
}
