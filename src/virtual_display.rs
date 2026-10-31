//! A virtual display sized to the client (private `CGVirtualDisplay`, the API
//! behind Sidecar and AirPlay displays).
//!
//! While it exists the physical displays mirror it, so every window lives on a
//! desktop of the client's size and the menu bar and Dock stay in reach. The
//! mirroring is configured for this process only: macOS restores the previous
//! arrangement when the display goes away or the process exits.

use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2_core_foundation::CGSize;
use objc2_core_graphics::{
    CGBeginDisplayConfiguration, CGCompleteDisplayConfiguration, CGConfigureDisplayMirrorOfDisplay,
    CGConfigureOption, CGDisplayBounds, CGDisplayConfigRef, CGDisplayPixelsHigh,
    CGDisplayPixelsWide, CGGetActiveDisplayList, kCGNullDirectDisplay,
};
use objc2_foundation::NSArray;
use tracing::{debug, info, warn};

/// Largest supported size in pixels (8K).
const MAX_PIXELS: (u32, u32) = (7680, 4320);
/// Largest frame of H.264 levels 5.1/5.2 in 16x16 macroblocks (4096x2304).
/// Hardware encoders and decoders on both ends stop there: past it the
/// encoder drops frames and clients show garbage.
const MAX_MACROBLOCKS: u32 = 36_864;
/// Tries at mirroring the physical displays, 300 ms apart.
const MIRROR_ATTEMPTS: u32 = 5;

/// A display mode in pixels; HiDPI modes have half as many points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub hidpi: bool,
}

impl Mode {
    /// The mode for a client asking for `width`x`height` pixels at `scale` percent,
    /// in at most `max_pixels`.
    ///
    /// Retina-sized requests get a HiDPI mode even at 100%, as macOS would pick
    /// for such a panel. A larger size than the budget (or than H.264 can carry)
    /// is scaled down keeping the aspect ratio, and the client stretches the
    /// picture. A scaled-down Retina request drops to 1x unless it stays
    /// Retina-sized, so the interface keeps about the size the client expects
    /// instead of doubling. The result is a whole number of points.
    pub fn for_client(width: u32, height: u32, scale: u32, max_pixels: u32) -> Self {
        let retina = |w: u32, h: u32| w >= 2880 || h >= 1800;
        let budget = |w: u32, h: u32| {
            w.div_ceil(16) * h.div_ceil(16) <= MAX_MACROBLOCKS && w * h <= max_pixels
        };
        let fit = |value: u32, max: u32, step: u32| (value.clamp(2 * step, max) / step) * step;
        let mut hidpi = scale >= 175 || retina(width, height);
        let step = if hidpi { 2 } else { 1 };
        let (full_w, full_h) = (
            fit(width, MAX_PIXELS.0, step),
            fit(height, MAX_PIXELS.1, step),
        );
        let area = f64::from(full_w) * f64::from(full_h);
        let mut factor = (f64::from(max_pixels) / area).sqrt().min(1.0);
        let scaled = |factor: f64| {
            let side = |value: u32| ((f64::from(value) * factor) as u32).max(2);
            (side(full_w), side(full_h))
        };
        let (mut w, mut h) = scaled(factor);
        while !budget(w, h) && factor > 0.01 {
            factor -= 0.005;
            (w, h) = scaled(factor);
        }
        if (w, h) != (full_w, full_h) {
            hidpi &= retina(w, h);
            let step = if hidpi { 2 } else { 1 };
            (w, h) = (fit(w, MAX_PIXELS.0, step), fit(h, MAX_PIXELS.1, step));
        }
        Self {
            width: w,
            height: h,
            hidpi,
        }
    }

    fn points(self) -> (u32, u32) {
        if self.hidpi {
            (self.width / 2, self.height / 2)
        } else {
            (self.width, self.height)
        }
    }
}

pub struct VirtualDisplay {
    display: Retained<AnyObject>,
    _queue: DispatchRetained<DispatchQueue>,
    id: u32,
    mode: Mode,
    /// Physical displays switched to mirroring this one.
    mirrors: Vec<u32>,
}

// SAFETY: the object is only messaged through `&mut self`, one thread at a time;
// CGVirtualDisplay does its work on its own dispatch queue.
unsafe impl Send for VirtualDisplay {}

impl VirtualDisplay {
    pub fn new(mode: Mode) -> anyhow::Result<Self> {
        let class = |name: &std::ffi::CStr| {
            AnyClass::get(name)
                .ok_or_else(|| anyhow!("{} is not available on this macOS", name.to_string_lossy()))
        };
        let descriptor_class = class(c"CGVirtualDisplayDescriptor")?;
        let display_class = class(c"CGVirtualDisplay")?;

        let queue = DispatchQueue::new("openrdp.virtual-display", None);
        // SAFETY: selectors and argument types follow the CGVirtualDisplay headers
        // as shipped in CoreGraphics.
        let display = unsafe {
            let descriptor: Retained<AnyObject> = msg_send![descriptor_class, new];
            let name = objc2_foundation::NSString::from_str("OpenRDP");
            let _: () = msg_send![&*descriptor, setName: &*name];
            let _: () = msg_send![&*descriptor, setMaxPixelsWide: MAX_PIXELS.0];
            let _: () = msg_send![&*descriptor, setMaxPixelsHigh: MAX_PIXELS.1];
            // Physical size only feeds the DPI macOS reports; aim for a typical
            // laptop panel.
            let (points_w, points_h) = mode.points();
            let size = CGSize::new(
                f64::from(points_w) * 25.4 / 110.0,
                f64::from(points_h) * 25.4 / 110.0,
            );
            let _: () = msg_send![&*descriptor, setSizeInMillimeters: size];
            // A stable identity lets macOS remember the arrangement between sessions.
            let _: () = msg_send![&*descriptor, setVendorID: 0x6d72_u32];
            let _: () = msg_send![&*descriptor, setProductID: 0x6470_u32];
            let _: () = msg_send![&*descriptor, setSerialNum: 1_u32];
            let queue_object: *const AnyObject = (&*queue as *const DispatchQueue).cast();
            let _: () = msg_send![&*descriptor, setQueue: queue_object];

            let allocated: Allocated<AnyObject> = msg_send![display_class, alloc];
            let display: Option<Retained<AnyObject>> =
                msg_send![allocated, initWithDescriptor: &*descriptor];
            display.context("CGVirtualDisplay was not created")?
        };
        // SAFETY: displayID is a plain unsigned int property.
        let id: u32 = unsafe { msg_send![&*display, displayID] };
        if id == kCGNullDirectDisplay {
            bail!("CGVirtualDisplay has no display id");
        }

        let mut this = Self {
            display,
            _queue: queue,
            id,
            mode,
            mirrors: Vec::new(),
        };
        this.apply(mode)?;
        this.mirror_physical()?;
        this.verify();
        info!(id, ?mode, "virtual display created");
        Ok(this)
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// False once macOS has dropped the display, which happens when all
    /// displays sleep or the physical ones are unplugged.
    pub fn is_online(&self) -> bool {
        // Not CGDisplayIsOnline: it answers nonzero (-1) for a dropped display.
        online_displays().is_ok_and(|ids| ids.contains(&self.id))
    }

    pub fn set_mode(&mut self, mode: Mode) -> anyhow::Result<()> {
        if mode == self.mode {
            return Ok(());
        }
        // macOS keeps the mode of a mirror set's master fixed, so leave the set
        // for the change.
        self.unmirror()?;
        let applied = self.apply(mode);
        self.mirror_physical()?;
        applied?;
        self.verify();
        info!(id = self.id, ?mode, "virtual display resized");
        Ok(())
    }

    fn apply(&mut self, mode: Mode) -> anyhow::Result<()> {
        let mode_class = AnyClass::get(c"CGVirtualDisplayMode")
            .context("CGVirtualDisplayMode is not available")?;
        let settings_class = AnyClass::get(c"CGVirtualDisplaySettings")
            .context("CGVirtualDisplaySettings is not available")?;
        let (points_w, points_h) = mode.points();
        // SAFETY: see `new`.
        let applied = unsafe {
            let allocated: Allocated<AnyObject> = msg_send![mode_class, alloc];
            let display_mode: Retained<AnyObject> = msg_send![
                allocated,
                initWithWidth: points_w,
                height: points_h,
                refreshRate: 60.0_f64
            ];
            let modes = NSArray::from_retained_slice(&[display_mode]);
            let settings: Retained<AnyObject> = msg_send![settings_class, new];
            let _: () = msg_send![&*settings, setModes: &*modes];
            let _: () = msg_send![&*settings, setHiDPI: u32::from(mode.hidpi)];
            let applied: Bool = msg_send![&*self.display, applySettings: &*settings];
            applied.as_bool()
        };
        if !applied {
            bail!("virtual display rejected mode {mode:?}");
        }
        self.mode = mode;
        Ok(())
    }

    /// Checks that the display has the mode last applied.
    ///
    /// This process sees mode changes only after its own next display
    /// configuration, so call it after mirroring.
    fn verify(&self) {
        let (width, height) = self.mode.points();
        let points = CGDisplayBounds(self.id).size;
        let pixels = (CGDisplayPixelsWide(self.id), CGDisplayPixelsHigh(self.id));
        if (points.width, points.height) == (f64::from(width), f64::from(height)) {
            debug!(id = self.id, ?pixels, "virtual display mode applied");
        } else {
            warn!(id = self.id, ?points, mode = ?self.mode, "virtual display has another mode");
        }
    }

    /// Makes every other active display mirror this one.
    ///
    /// Right after a display appears or goes away macOS can refuse the
    /// configuration for a moment, so it is retried.
    fn mirror_physical(&mut self) -> anyhow::Result<()> {
        let mut attempt = 1;
        loop {
            match self.try_mirror_physical() {
                Ok(()) => return Ok(()),
                Err(err) if attempt < MIRROR_ATTEMPTS => {
                    debug!(id = self.id, attempt, "{err:#}; retrying");
                    std::thread::sleep(Duration::from_millis(300));
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn try_mirror_physical(&mut self) -> anyhow::Result<()> {
        let physical: Vec<u32> = active_displays()?
            .into_iter()
            .filter(|&id| id != self.id)
            .collect();
        configure(|config| {
            for &id in &physical {
                // SAFETY: config comes from CGBeginDisplayConfiguration.
                check(unsafe { CGConfigureDisplayMirrorOfDisplay(config, id, self.id) })
                    .with_context(|| format!("mirror display {id}"))?;
            }
            Ok(())
        })
        .context("mirror the physical displays")?;
        self.mirrors = physical;
        debug!(id = self.id, mirrors = ?self.mirrors, "physical displays mirror the virtual one");
        Ok(())
    }

    fn unmirror(&mut self) -> anyhow::Result<()> {
        let mirrors = std::mem::take(&mut self.mirrors);
        configure(|config| {
            for &id in &mirrors {
                // SAFETY: config comes from CGBeginDisplayConfiguration.
                check(unsafe {
                    CGConfigureDisplayMirrorOfDisplay(config, id, kCGNullDirectDisplay)
                })?;
            }
            Ok(())
        })
        .context("stop mirroring")
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        // Release the physical displays first so they keep their own modes
        // instead of briefly following the vanishing master.
        if let Err(err) = self.unmirror() {
            warn!(%err, "cannot stop mirroring");
        }
        // Releasing `display` after this removes the display itself.
        info!(id = self.id, "virtual display removed");
    }
}

fn configure(edit: impl FnOnce(CGDisplayConfigRef) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let mut config: CGDisplayConfigRef = std::ptr::null_mut();
    // SAFETY: out-pointer per CGBeginDisplayConfiguration.
    check(unsafe { CGBeginDisplayConfiguration(&mut config) })?;
    if let Err(err) = edit(config) {
        // SAFETY: the configuration was begun above and not completed.
        unsafe { objc2_core_graphics::CGCancelDisplayConfiguration(config) };
        return Err(err);
    }
    // SAFETY: completing the configuration begun above. App-only: reverted when
    // the process exits, so a crash cannot leave the screens mirrored.
    check(unsafe { CGCompleteDisplayConfiguration(config, CGConfigureOption::ForAppOnly) })
}

fn active_displays() -> anyhow::Result<Vec<u32>> {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    // SAFETY: the buffer holds `ids.len()` entries.
    check(unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) })?;
    Ok(ids[..count as usize].to_vec())
}

fn online_displays() -> anyhow::Result<Vec<u32>> {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    // SAFETY: the buffer holds `ids.len()` entries.
    check(unsafe {
        objc2_core_graphics::CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count)
    })?;
    Ok(ids[..count as usize].to_vec())
}

fn check(error: objc2_core_graphics::CGError) -> anyhow::Result<()> {
    if error == objc2_core_graphics::CGError::Success {
        Ok(())
    } else {
        Err(anyhow!("CoreGraphics error {}", error.0))
    }
}

#[cfg(test)]
mod tests {
    use super::Mode;

    #[test]
    fn fits_h264() {
        // A 16" MacBook Pro in Windows App with "Optimize for Retina displays".
        let mode = Mode::for_client(4096, 2560, 100, u32::MAX);
        assert!(mode.hidpi);
        assert!(mode.width.div_ceil(16) * mode.height.div_ceil(16) <= 36_864);
        assert_eq!((mode.width % 2, mode.height % 2), (0, 0));
        let ratio = f64::from(mode.width) / f64::from(mode.height);
        assert!((ratio - 1.6).abs() < 0.01, "{mode:?}");
        assert!(mode.width >= 3800, "{mode:?}");

        assert_eq!(
            Mode::for_client(2560, 1080, 100, u32::MAX),
            Mode {
                width: 2560,
                height: 1080,
                hidpi: false
            }
        );
        assert_eq!(
            Mode::for_client(3456, 2234, 200, u32::MAX),
            Mode {
                width: 3456,
                height: 2234,
                hidpi: true
            }
        );
    }

    #[test]
    fn fits_budget() {
        let budget = 2560 * 1600;
        // Retina request scaled to the budget drops to 1x.
        assert_eq!(
            Mode::for_client(4096, 2560, 200, budget),
            Mode {
                width: 2560,
                height: 1600,
                hidpi: false
            }
        );
        // Within budget: untouched.
        assert_eq!(
            Mode::for_client(2560, 1080, 100, budget),
            Mode {
                width: 2560,
                height: 1080,
                hidpi: false
            }
        );
        // Still Retina-sized after scaling: stays HiDPI on even pixels.
        let mode = Mode::for_client(4096, 2560, 200, 3200 * 2000);
        assert!(
            mode.hidpi && mode.width * mode.height <= 3200 * 2000,
            "{mode:?}"
        );
        assert_eq!((mode.width % 2, mode.height % 2), (0, 0));
    }
}
