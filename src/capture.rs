//! Screen capture through ScreenCaptureKit.
//!
//! Frames arrive as full-range BT.709 NV12 (`420f`), the colour space MS-RDPEGFX
//! uses for AVC, so they go to VideoToolbox without conversion. Only the newest
//! frame is kept: if the encoder is busy, older frames are dropped.

use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread as _, DefinedClass as _, define_class, msg_send};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::CVPixelBuffer;
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamOutput, SCStreamOutputType,
};
use tracing::{error, warn};

/// `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange`.
pub const PIXEL_FORMAT_420F: u32 = u32::from_be_bytes(*b"420f");

/// A captured frame. The pixel buffer is IOSurface-backed and immutable once
/// delivered, so handing it to another thread is sound.
pub struct Frame(pub CFRetained<CVPixelBuffer>);

// SAFETY: CVPixelBuffer is a thread-safe CF object; ScreenCaptureKit does not
// write to a buffer after delivering it.
unsafe impl Send for Frame {}

/// Newest captured frame plus a wakeup for the consumer.
#[derive(Default)]
pub struct FrameSlot {
    state: Mutex<SlotState>,
    cond: Condvar,
}

#[derive(Default)]
struct SlotState {
    frame: Option<Frame>,
    /// Bumped on any event the consumer should look at: a frame, an ack, stop.
    wakeups: u64,
}

impl FrameSlot {
    fn put(&self, frame: Frame) {
        let mut state = self.state.lock().expect("frame slot poisoned");
        state.frame = Some(frame);
        state.wakeups += 1;
        self.cond.notify_all();
    }

    /// Wakes the consumer without a new frame.
    pub fn poke(&self) {
        self.state.lock().expect("frame slot poisoned").wakeups += 1;
        self.cond.notify_all();
    }

    pub fn take(&self) -> Option<Frame> {
        self.state.lock().expect("frame slot poisoned").frame.take()
    }

    /// Puts a frame back if no newer one arrived meanwhile.
    pub fn restore(&self, frame: Frame) {
        let mut state = self.state.lock().expect("frame slot poisoned");
        if state.frame.is_none() {
            state.frame = Some(frame);
        }
    }

    /// Waits until the wakeup counter moves past `seen` or the timeout passes.
    pub fn wait(&self, seen: u64, timeout: Duration) -> u64 {
        let state = self.state.lock().expect("frame slot poisoned");
        let (state, _) = self
            .cond
            .wait_timeout_while(state, timeout, |s| s.wakeups == seen)
            .expect("frame slot poisoned");
        state.wakeups
    }
}

struct OutputIvars {
    slot: Arc<FrameSlot>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "MacRdpCaptureOutput"]
    #[ivars = OutputIvars]
    struct CaptureOutput;

    unsafe impl NSObjectProtocol for CaptureOutput {}

    unsafe impl SCStreamOutput for CaptureOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // Idle and blank frames carry no image; only complete frames do.
            // SAFETY: the sample buffer is valid for the duration of the callback.
            let Some(image) = (unsafe { sample.image_buffer() }) else {
                return;
            };
            self.ivars().slot.put(Frame(image));
        }
    }

    unsafe impl SCStreamDelegate for CaptureOutput {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            error!(error = %error.localizedDescription(), "screen capture stopped");
        }
    }
);

impl CaptureOutput {
    fn new(slot: Arc<FrameSlot>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { slot });
        // SAFETY: NSObject's init on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

/// A running capture of one display.
pub struct Capture {
    stream: Retained<SCStream>,
    _output: Retained<CaptureOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: SCStream may be stopped from any thread; the other fields are only kept alive.
unsafe impl Send for Capture {}

impl Capture {
    /// Starts capturing `display_id` scaled to `content` pixels, placed at the
    /// top-left of a `buffer`-sized frame (the rest stays black). The encoder
    /// needs macroblock-aligned frames; this way no copy is needed to pad them.
    pub fn start(
        display_id: u32,
        content: (usize, usize),
        buffer: (usize, usize),
        fps: i32,
        slot: Arc<FrameSlot>,
    ) -> anyhow::Result<Self> {
        let display = find_display(display_id)?;

        // SAFETY: plain Objective-C object construction and property setters.
        unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            );

            let config = SCStreamConfiguration::new();
            config.setWidth(buffer.0);
            config.setHeight(buffer.1);
            config.setScalesToFit(false);
            config.setDestinationRect(CGRect::new(
                CGPoint::ZERO,
                CGSize::new(content.0 as f64, content.1 as f64),
            ));
            config.setPixelFormat(PIXEL_FORMAT_420F);
            config.setColorMatrix(objc2_core_video::kCVImageBufferYCbCrMatrix_ITU_R_709_2);
            config.setColorSpaceName(objc2_core_graphics::kCGColorSpaceSRGB);
            // The cursor is drawn into the video until it gets its own channel.
            config.setShowsCursor(true);
            config.setMinimumFrameInterval(CMTime {
                value: 1,
                timescale: fps,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            });
            config.setQueueDepth(5);

            let output = CaptureOutput::new(slot);
            let queue = DispatchQueue::new("macrdp.capture", None);
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            );
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| anyhow!("addStreamOutput: {}", e.localizedDescription()))?;

            let (tx, rx) = mpsc::channel();
            let done = block2::RcBlock::new(move |err: *mut NSError| {
                let result = match err.as_ref() {
                    Some(err) => Err(err.localizedDescription().to_string()),
                    None => Ok(()),
                };
                let _ = tx.send(result);
            });
            stream.startCaptureWithCompletionHandler(Some(&done));
            rx.recv_timeout(Duration::from_secs(10))
                .context("startCapture timed out")?
                .map_err(|e| anyhow!("startCapture: {e}"))?;

            Ok(Self {
                stream,
                _output: output,
                _queue: queue,
            })
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let (tx, rx) = mpsc::channel();
        let done = block2::RcBlock::new(move |_err: *mut NSError| {
            let _ = tx.send(());
        });
        // SAFETY: stopping a started stream.
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&done)) };
        if rx.recv_timeout(Duration::from_secs(2)).is_err() {
            warn!("stopCapture did not complete in time");
        }
    }
}

fn find_display(display_id: u32) -> anyhow::Result<Retained<SCDisplay>> {
    let (tx, rx) = mpsc::channel();
    let handler =
        block2::RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
            // SAFETY: the pointers are valid for the duration of the callback; retain to keep them.
            let result = unsafe {
                match Retained::retain(content) {
                    Some(content) => Ok(content),
                    None => Err(err
                        .as_ref()
                        .map(|e| e.localizedDescription().to_string())
                        .unwrap_or_default()),
                }
            };
            let _ = tx.send(result);
        });
    // SAFETY: the handler is retained by ScreenCaptureKit until called.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };

    let content = rx
        .recv_timeout(Duration::from_secs(10))
        .context("ScreenCaptureKit did not answer")?
        .map_err(|e| {
            anyhow!("no shareable content ({e}); grant Screen Recording to this binary")
        })?;

    // SAFETY: reading properties of a valid SCShareableContent.
    let displays = unsafe { content.displays() };
    for display in displays.iter() {
        if unsafe { display.displayID() } == display_id {
            return Ok(display);
        }
    }
    bail!("display {display_id} not found")
}

/// Synthetic moving picture in place of the screen, for testing the video path
/// without the Screen Recording permission.
pub struct TestPattern {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestPattern {
    pub fn start(
        content: (usize, usize),
        buffer: (usize, usize),
        fps: i32,
        slot: Arc<FrameSlot>,
    ) -> anyhow::Result<Self> {
        use std::sync::atomic::{AtomicBool, Ordering};

        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let interval = Duration::from_secs(1) / u32::try_from(fps.max(1))?;
        let thread = std::thread::Builder::new()
            .name("macrdp-pattern".into())
            .spawn(move || {
                let mut tick = 0usize;
                while !stop_flag.load(Ordering::Relaxed) {
                    match pattern_frame(content, buffer, tick) {
                        Ok(frame) => slot.put(frame),
                        Err(err) => {
                            error!(%err, "test pattern");
                            return;
                        }
                    }
                    tick += 1;
                    std::thread::sleep(interval);
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for TestPattern {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn pattern_frame(
    content: (usize, usize),
    buffer: (usize, usize),
    tick: usize,
) -> anyhow::Result<Frame> {
    use objc2_core_foundation::{CFDictionary, CFString, CFType};
    use objc2_core_video::{
        CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
        CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress, kCVPixelBufferIOSurfacePropertiesKey,
    };

    let attrs = CFDictionary::<CFString, CFType>::from_slices(
        &[unsafe { kCVPixelBufferIOSurfacePropertiesKey }],
        &[CFDictionary::<CFString, CFType>::empty().as_ref()],
    );
    let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: out-pointer per CVPixelBufferCreate.
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            buffer.0,
            buffer.1,
            PIXEL_FORMAT_420F,
            Some(attrs.as_opaque()),
            std::ptr::NonNull::from(&mut raw),
        )
    };
    let raw = std::ptr::NonNull::new(raw)
        .filter(|_| status == 0)
        .ok_or_else(|| anyhow!("CVPixelBufferCreate: {status}"))?;
    // SAFETY: Create returns a +1 reference.
    let pixels = unsafe { CFRetained::from_raw(raw) };

    // SAFETY: the planes are written only while the buffer is locked.
    unsafe {
        CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::empty());
        let y = CVPixelBufferGetBaseAddressOfPlane(&pixels, 0).cast::<u8>();
        let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&pixels, 0);
        let uv = CVPixelBufferGetBaseAddressOfPlane(&pixels, 1).cast::<u8>();
        let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(&pixels, 1);
        let bar = (tick * 8) % content.0.max(1);
        for row in 0..buffer.1 {
            let line = std::slice::from_raw_parts_mut(y.add(row * y_stride), buffer.0);
            for (col, px) in line.iter_mut().enumerate() {
                *px = if row >= content.1 || col >= content.0 {
                    0
                } else if col.abs_diff(bar) < 16 {
                    255
                } else if (row / 8) % 4 == 0 && (col / 3) % 2 == 0 {
                    // Fine stripes, like text, to judge sharpness.
                    40
                } else {
                    (col * 200 / content.0.max(1) + 30) as u8
                };
            }
        }
        for row in 0..buffer.1 / 2 {
            let line = std::slice::from_raw_parts_mut(uv.add(row * uv_stride), buffer.0);
            for (i, px) in line.iter_mut().enumerate() {
                // Cb on even bytes, Cr on odd; a colour ramp top to bottom.
                *px = if i % 2 == 0 {
                    128
                } else {
                    (64 + row * 128 / (buffer.1 / 2).max(1)) as u8
                };
            }
        }
        CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::empty());
    }
    Ok(Frame(pixels))
}
