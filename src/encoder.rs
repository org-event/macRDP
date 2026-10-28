//! H.264 encoding with VideoToolbox, tuned for interactive desktop video:
//! realtime, low-latency rate control, no B-frames, Annex B output.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::mpsc;

use anyhow::{anyhow, bail};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMSampleBuffer, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMSampleAttachmentKey_NotSync,
    kCMVideoCodecType_H264,
};
use objc2_core_video::CVPixelBuffer;
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_H264EntropyMode,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTH264EntropyMode_CABAC, kVTProfileLevel_H264_ConstrainedHigh_AutoLevel,
    kVTProfileLevel_H264_High_AutoLevel, kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
};
use tracing::{debug, warn};

const START_CODE: [u8; 4] = [0, 0, 0, 1];

pub struct EncodedFrame {
    /// Annex B byte stream; keyframes start with SPS and PPS.
    pub data: Vec<u8>,
    pub keyframe: bool,
}

type Output = Result<EncodedFrame, String>;

pub struct Encoder {
    session: CFRetained<VTCompressionSession>,
    /// Owned by the session callback; freed in `Drop` after the session is invalidated.
    sender: *mut mpsc::Sender<Output>,
    receiver: mpsc::Receiver<Output>,
    frame_index: i64,
    fps: i32,
}

// SAFETY: a compression session can be driven from any one thread at a time.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(width: usize, height: usize, fps: i32, bitrate: u32) -> anyhow::Result<Self> {
        let (tx, receiver) = mpsc::channel();
        let sender = Box::into_raw(Box::new(tx));

        let session = match create_session(width, height, sender, true) {
            Ok(session) => session,
            Err(err) => {
                warn!(%err, "low-latency H.264 encoder unavailable, using the regular one");
                create_session(width, height, sender, false).inspect_err(|_| {
                    // SAFETY: no session holds the pointer.
                    drop(unsafe { Box::from_raw(sender) });
                })?
            }
        };

        let encoder = Self {
            session,
            sender,
            receiver,
            frame_index: 0,
            fps,
        };
        encoder.set(
            unsafe { kVTCompressionPropertyKey_RealTime },
            CFBoolean::new(true),
        );
        encoder.set(
            unsafe { kVTCompressionPropertyKey_AllowFrameReordering },
            CFBoolean::new(false),
        );
        encoder.set(
            unsafe { kVTCompressionPropertyKey_H264EntropyMode },
            unsafe { kVTH264EntropyMode_CABAC },
        );
        encoder.set(
            unsafe { kVTCompressionPropertyKey_ExpectedFrameRate },
            &CFNumber::new_i32(fps),
        );
        // The client asks for a keyframe when it needs one; periodic ones only
        // cost bandwidth on a reliable transport. Keep one per minute as a safety net.
        encoder.set(
            unsafe { kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration },
            &CFNumber::new_i32(60),
        );
        encoder.set_bitrate(bitrate);
        Ok(encoder)
    }

    pub fn set_bitrate(&self, bitrate: u32) {
        let bitrate = i32::try_from(bitrate).unwrap_or(i32::MAX);
        self.set(
            unsafe { kVTCompressionPropertyKey_AverageBitRate },
            &CFNumber::new_i32(bitrate),
        );
    }

    fn set(&self, key: &CFString, value: &CFType) {
        // SAFETY: the keys are VideoToolbox property keys and the values have the documented types.
        let status = unsafe { VTSessionSetProperty(&self.session, key, Some(value)) };
        if status != 0 {
            warn!(key = %key, status, "VTSessionSetProperty failed");
        }
    }

    /// Encodes one frame synchronously.
    pub fn encode(
        &mut self,
        image: &CVPixelBuffer,
        force_keyframe: bool,
    ) -> anyhow::Result<EncodedFrame> {
        let pts = CMTime {
            value: self.frame_index,
            timescale: self.fps,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        self.frame_index += 1;

        let options = force_keyframe.then(|| {
            CFDictionary::<CFString, CFType>::from_slices(
                &[unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }],
                &[CFBoolean::new(true).as_ref()],
            )
        });

        // SAFETY: valid session and pixel buffer; the output arrives through the callback.
        unsafe {
            let status = self.session.encode_frame(
                image,
                pts,
                CMTime {
                    value: 0,
                    timescale: 0,
                    flags: CMTimeFlags::empty(),
                    epoch: 0,
                },
                options.as_deref().map(|d| d.as_opaque()),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            if status != 0 {
                bail!("VTCompressionSessionEncodeFrame failed: {status}");
            }
            let status = self.session.complete_frames(pts);
            if status != 0 {
                bail!("VTCompressionSessionCompleteFrames failed: {status}");
            }
        }

        self.receiver
            .try_recv()
            .map_err(|_| anyhow!("encoder produced no output"))?
            .map_err(|e| anyhow!(e))
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: after invalidation the callback is never called again.
        unsafe {
            self.session.invalidate();
            drop(Box::from_raw(self.sender));
        }
    }
}

fn create_session(
    width: usize,
    height: usize,
    sender: *mut mpsc::Sender<Output>,
    low_latency: bool,
) -> anyhow::Result<CFRetained<VTCompressionSession>> {
    let spec = low_latency.then(|| {
        CFDictionary::<CFString, CFType>::from_slices(
            &[unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl }],
            &[CFBoolean::new(true).as_ref()],
        )
    });

    let mut session: *mut VTCompressionSession = ptr::null_mut();
    // SAFETY: out-pointer and callback contract per VTCompressionSessionCreate.
    let status = unsafe {
        VTCompressionSession::create(
            None,
            i32::try_from(width)?,
            i32::try_from(height)?,
            kCMVideoCodecType_H264,
            spec.as_deref().map(|d| d.as_opaque()),
            None,
            None,
            Some(output_callback),
            sender.cast(),
            NonNull::from(&mut session),
        )
    };
    let Some(session) = NonNull::new(session).filter(|_| status == 0) else {
        bail!("VTCompressionSessionCreate failed: {status}");
    };
    // SAFETY: Create returns a +1 reference.
    let session = unsafe { CFRetained::from_raw(session) };

    // Constrained High is what the low-latency encoder accepts; it is plain High without B-frames.
    let profile = if low_latency {
        unsafe { kVTProfileLevel_H264_ConstrainedHigh_AutoLevel }
    } else {
        unsafe { kVTProfileLevel_H264_High_AutoLevel }
    };
    // SAFETY: valid key/value pair.
    let status = unsafe {
        VTSessionSetProperty(
            &session,
            kVTCompressionPropertyKey_ProfileLevel,
            Some(profile),
        )
    };
    if status != 0 {
        bail!("cannot set H.264 profile: {status}");
    }
    debug!(width, height, low_latency, "H.264 encoder created");
    Ok(session)
}

unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: refcon is the sender owned by the Encoder, alive until the session is invalidated.
    let sender = unsafe { &*refcon.cast::<mpsc::Sender<Output>>() };
    let result = if status != 0 {
        Err(format!("encoder callback status {status}"))
    } else {
        // SAFETY: on success the sample buffer is valid (null means the frame was dropped).
        match unsafe { sample.as_ref() } {
            Some(sample) => unsafe { to_annex_b(sample) },
            None => Err("encoder dropped the frame".to_owned()),
        }
    };
    let _ = sender.send(result);
}

unsafe fn to_annex_b(sample: &CMSampleBuffer) -> Result<EncodedFrame, String> {
    let keyframe = unsafe { is_keyframe(sample) };
    let block = unsafe { sample.data_buffer() }.ok_or("no data buffer")?;
    let avcc = unsafe { block_bytes(&block) }?;

    let mut data = Vec::with_capacity(avcc.len() + 64);
    if keyframe {
        let format = unsafe { sample.format_description() }.ok_or("no format description")?;
        let mut count = 0usize;
        let mut index = 0usize;
        loop {
            let mut ptr: *const u8 = ptr::null();
            let mut size = 0usize;
            let status = unsafe {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    &format,
                    index,
                    &mut ptr,
                    &mut size,
                    &mut count,
                    ptr::null_mut(),
                )
            };
            if status != 0 {
                return Err(format!("no H.264 parameter set {index}: {status}"));
            }
            data.extend_from_slice(&START_CODE);
            data.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr, size) });
            index += 1;
            if index >= count {
                break;
            }
        }
    }

    // VideoToolbox emits 4-byte big-endian NAL lengths.
    let mut rest = avcc.as_slice();
    while rest.len() >= 4 {
        let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        let nal = rest.get(4..4 + len).ok_or("truncated NAL unit")?;
        data.extend_from_slice(&START_CODE);
        data.extend_from_slice(nal);
        rest = &rest[4 + len..];
    }
    Ok(EncodedFrame { data, keyframe })
}

unsafe fn is_keyframe(sample: &CMSampleBuffer) -> bool {
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
        return true;
    };
    if attachments.count() == 0 {
        return true;
    }
    // SAFETY: each element of the attachments array is a CFDictionary.
    let dict = unsafe { attachments.value_at_index(0) }.cast::<CFDictionary>();
    let Some(dict) = (unsafe { dict.as_ref() }) else {
        return true;
    };
    let key: *const CFString = unsafe { kCMSampleAttachmentKey_NotSync };
    let not_sync = unsafe { dict.value(key.cast()) }.cast::<CFBoolean>();
    match unsafe { not_sync.as_ref() } {
        Some(b) => !b.as_bool(),
        None => true,
    }
}

unsafe fn block_bytes(block: &CMBlockBuffer) -> Result<Vec<u8>, String> {
    let len = unsafe { block.data_length() };
    let mut out = vec![0u8; len];
    let status = unsafe {
        block.copy_data_bytes(0, len, NonNull::new(out.as_mut_ptr().cast()).ok_or("null")?)
    };
    if status != 0 {
        return Err(format!("CMBlockBufferCopyDataBytes failed: {status}"));
    }
    Ok(out)
}
