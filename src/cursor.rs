//! The macOS cursor as RDP pointer updates.
//!
//! The client draws the pointer itself at its own mouse position, so it does not
//! lag behind the way a cursor drawn into the video does.

use std::ffi::c_void;
use std::hash::{Hash as _, Hasher as _};

use ironrdp_server::RGBAPointer;
use objc2::rc::autoreleasepool;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::msg_send;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColorSpace, CGContext, CGImage};

/// Largest side `RGBAPointer` carries (with the client's 96x96 large pointer flag).
const MAX_SIDE: f64 = 96.0;

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

unsafe extern "C-unwind" {
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: Option<&CGColorSpace>,
        bitmap_info: u32,
    ) -> Option<std::ptr::NonNull<CGContext>>;
    fn CGBitmapContextGetData(context: &CGContext) -> *mut u8;
}

/// `kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little`: BGRA in memory.
const BGRA_PREMULTIPLIED: u32 = 2 | (2 << 12);

/// Reports the system cursor when its shape changes.
#[derive(Default)]
pub struct CursorWatch {
    last: Option<u64>,
}

impl CursorWatch {
    /// The cursor drawn for `scale` pixels per point, if it changed since the last call.
    pub fn changed(&mut self, scale: f64) -> Option<RGBAPointer> {
        let pointer = autoreleasepool(|_| current(scale))?;
        let mut hasher = std::hash::DefaultHasher::new();
        (pointer.width, pointer.height, pointer.hot_x, pointer.hot_y).hash(&mut hasher);
        pointer.data.hash(&mut hasher);
        let hash = hasher.finish();
        if self.last == Some(hash) {
            return None;
        }
        self.last = Some(hash);
        Some(pointer)
    }
}

fn current(scale: f64) -> Option<RGBAPointer> {
    let class = AnyClass::get(c"NSCursor")?;
    // SAFETY: NSCursor and NSImage messages as declared by AppKit; the returned
    // objects are autoreleased and used inside the caller's pool.
    let (image, size, hot): (*const CGImage, CGSize, CGPoint) = unsafe {
        let cursor: *mut AnyObject = msg_send![class, currentSystemCursor];
        let cursor = cursor.as_ref()?;
        let ns_image: *mut AnyObject = msg_send![cursor, image];
        let ns_image = ns_image.as_ref()?;
        let size: CGSize = msg_send![ns_image, size];
        let hot: CGPoint = msg_send![cursor, hotSpot];
        let image: *const CGImage = msg_send![
            ns_image,
            CGImageForProposedRect: std::ptr::null_mut::<CGRect>(),
            context: std::ptr::null_mut::<AnyObject>(),
            hints: std::ptr::null_mut::<AnyObject>()
        ];
        (image, size, hot)
    };
    // SAFETY: owned by the autoreleased NSImage, alive for this pool.
    let image = unsafe { image.as_ref() }?;
    if size.width <= 0.0 || size.height <= 0.0 {
        return None;
    }
    let scale = scale.min(MAX_SIDE / size.width.max(size.height));
    let width = (size.width * scale).round().max(1.0) as usize;
    let height = (size.height * scale).round().max(1.0) as usize;
    let data = rasterize(image, width, height)?;
    Some(RGBAPointer {
        cache_index: 0,
        width: width as u16,
        height: height as u16,
        hot_x: ((hot.x * scale) as u16).min(width as u16 - 1),
        hot_y: ((hot.y * scale) as u16).min(height as u16 - 1),
        data,
    })
}

/// Draws `image` at `width`x`height` as straight-alpha BGRA, bottom row first
/// as RDP color pointers expect.
fn rasterize(image: &CGImage, width: usize, height: usize) -> Option<Vec<u8>> {
    let space = CGColorSpace::new_device_rgb()?;
    let stride = width * 4;
    // SAFETY: CoreGraphics allocates the buffer for the given geometry.
    let context = unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            width,
            height,
            8,
            stride,
            Some(&space),
            BGRA_PREMULTIPLIED,
        )
    }?;
    // SAFETY: created above, so owned.
    let context = unsafe { CFRetained::from_raw(context) };
    let rect = CGRect::new(CGPoint::ZERO, CGSize::new(width as f64, height as f64));
    CGContext::draw_image(Some(&context), rect, Some(image));
    // SAFETY: the context's own buffer of `stride * height` bytes.
    let pixels = unsafe {
        let data = CGBitmapContextGetData(&context);
        if data.is_null() {
            return None;
        }
        std::slice::from_raw_parts(data, stride * height)
    };
    let mut out = Vec::with_capacity(pixels.len());
    // The buffer holds the top row first.
    for row in pixels.chunks_exact(stride).rev() {
        for pixel in row.as_chunks::<4>().0 {
            let alpha = u32::from(pixel[3]);
            let straight = |c: u8| {
                (u32::from(c) * 255).checked_div(alpha).map_or(0, |v| v.min(255) as u8)
            };
            out.extend_from_slice(&[straight(pixel[0]), straight(pixel[1]), straight(pixel[2]), pixel[3]]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_system_cursor() {
        let pointer = CursorWatch::default().changed(2.0).expect("a cursor");
        assert_eq!(
            pointer.data.len(),
            usize::from(pointer.width) * usize::from(pointer.height) * 4
        );
        assert!(pointer.data.as_chunks::<4>().0.iter().any(|p| p[3] == 255), "opaque pixels");
        assert!(pointer.data.as_chunks::<4>().0.iter().any(|p| p[3] == 0), "transparent pixels");
        println!("{pointer:?}");
    }
}
