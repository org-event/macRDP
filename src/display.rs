//! Display side of the server: capture -> H.264 -> EGFX (AVC420).
//!
//! Each connection gets a pipeline thread. It waits for a captured frame or a
//! frame acknowledgement, keeps the EGFX surface in place (the client may reset
//! it at any time by advertising capabilities again), and sends one H.264 frame
//! whenever the client has room for it. Frames that arrive while the client is
//! behind are coalesced into the newest one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow};
use ironrdp_egfx::pdu::{Avc420Region, CapabilitiesAdvertisePdu, CapabilitySet};
use ironrdp_egfx::server::GraphicsPipelineHandler;
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_server::{
    DesktopSize, DisplayContext, EgfxServerMessage, GfxContext, GfxServerFactory, GfxServerHandle,
    RdpServerDisplay, RdpServerDisplayUpdates, ServerError, ServerErrorExt as _, ServerEvent,
    ServerEventSender, ServerResult,
};
use ironrdp_svc::ChannelFlags;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info, warn};

use crate::capture::{Capture, Frame, FrameSlot, TestPattern};
use crate::encoder::Encoder;

/// QP reported to the client for each region; informational for AVC420.
const REPORTED_QP: u8 = 22;

#[derive(Debug, Clone, Copy)]
pub struct DisplayConfig {
    pub display_id: u32,
    /// Desktop size announced to the client.
    pub desktop: (u16, u16),
    pub fps: i32,
    pub bitrate: u32,
    /// Send a synthetic picture instead of the screen.
    pub test_pattern: bool,
}

impl DisplayConfig {
    /// Encoded frame size: the desktop rounded up to whole 16x16 macroblocks.
    fn surface(&self) -> (u16, u16) {
        (
            self.desktop.0.next_multiple_of(16),
            self.desktop.1.next_multiple_of(16),
        )
    }
}

/// State shared by the display handler and the EGFX factory.
#[derive(Default)]
pub struct Shared {
    events: Mutex<Option<UnboundedSender<ServerEvent>>>,
    /// Wakes the pipeline on new frames and on EGFX events.
    slot: Arc<FrameSlot>,
}

pub struct MacDisplay {
    config: DisplayConfig,
    shared: Arc<Shared>,
}

impl MacDisplay {
    pub fn new(config: DisplayConfig, shared: Arc<Shared>) -> Self {
        Self { config, shared }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for MacDisplay {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: self.config.desktop.0,
            height: self.config.desktop.1,
        }
    }

    async fn updates(
        &mut self,
        ctx: DisplayContext,
    ) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        let session = Session::start(self.config, Arc::clone(&self.shared), ctx)
            .map_err(|e| ServerError::custom("display", std::io::Error::other(format!("{e:#}"))))?;
        Ok(Box::new(session))
    }
}

/// One connection's capture and pipeline; dropping it stops both.
struct Session {
    _source: Box<dyn Send>,
    stop: Arc<AtomicBool>,
    slot: Arc<FrameSlot>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    fn start(
        config: DisplayConfig,
        shared: Arc<Shared>,
        ctx: DisplayContext,
    ) -> anyhow::Result<Self> {
        let gfx = ctx
            .gfx_handle
            .clone()
            .ok_or_else(|| anyhow!("client did not open the graphics pipeline (EGFX)"))?;
        let slot = Arc::clone(&shared.slot);
        let surface = config.surface();
        let content = (usize::from(config.desktop.0), usize::from(config.desktop.1));
        let buffer = (usize::from(surface.0), usize::from(surface.1));
        let source: Box<dyn Send> = if config.test_pattern {
            Box::new(TestPattern::start(
                content,
                buffer,
                config.fps,
                Arc::clone(&slot),
            )?)
        } else {
            Box::new(Capture::start(
                config.display_id,
                content,
                buffer,
                config.fps,
                Arc::clone(&slot),
            )?)
        };

        let stop = Arc::new(AtomicBool::new(false));
        let pipeline = Pipeline {
            config,
            gfx,
            shared,
            stop: Arc::clone(&stop),
            suppressed: ctx.display_suppressed,
        };
        let thread = std::thread::Builder::new()
            .name("macrdp-pipeline".into())
            .spawn(move || pipeline.run())
            .context("spawn pipeline thread")?;
        info!(?config, "display session started");

        Ok(Self {
            _source: source,
            stop,
            slot,
            thread: Some(thread),
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.slot.poke();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        info!("display session stopped");
    }
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for Session {
    async fn next_update(&mut self) -> ServerResult<Option<ironrdp_server::DisplayUpdate>> {
        // All graphics go through EGFX; pointer updates will come through here.
        std::future::pending().await
    }
}

struct Pipeline {
    config: DisplayConfig,
    gfx: GfxServerHandle,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    suppressed: Arc<AtomicBool>,
}

impl Pipeline {
    fn run(self) {
        if let Err(err) = self.run_inner() {
            error!(%err, "display pipeline failed");
            if let Some(events) = self.shared.events.lock().expect("poisoned").as_ref() {
                let _ = events.send(ServerEvent::Quit(format!("display pipeline failed: {err}")));
            }
        }
    }

    fn run_inner(&self) -> anyhow::Result<()> {
        let surface_size = self.config.surface();
        let (desk_w, desk_h) = self.config.desktop;
        let mut encoder = Encoder::new(
            usize::from(surface_size.0),
            usize::from(surface_size.1),
            self.config.fps,
            self.config.bitrate,
        )?;
        let region = Avc420Region {
            left: 0,
            top: 0,
            right: desk_w,
            bottom: desk_h,
            quantization_parameter: REPORTED_QP,
            quality: 100,
        };

        let slot = &self.shared.slot;
        let started = Instant::now();
        let mut seen = 0;
        let mut surface: Option<u16> = None;
        let mut need_keyframe = true;
        let mut last: Option<Frame> = None;
        let mut frames: u64 = 0;
        let mut bytes: u64 = 0;
        let mut stats_at = Instant::now();

        loop {
            seen = slot.wait(seen, Duration::from_millis(500));
            if self.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            if self.suppressed.load(Ordering::Relaxed) {
                continue;
            }

            {
                let mut server = self.gfx.lock().expect("EGFX server poisoned");
                if !server.is_ready() {
                    surface = None;
                    continue;
                }
                if surface.is_none_or(|id| server.get_surface(id).is_none()) {
                    if !server.supports_avc420() {
                        warn!("client did not negotiate H.264 (AVC420); nothing will be shown");
                    }
                    // Some clients (FreeRDP) reject a ResetGraphics without monitors.
                    server.resize_with_monitors(
                        desk_w,
                        desk_h,
                        vec![Monitor {
                            left: 0,
                            top: 0,
                            right: i32::from(desk_w) - 1,
                            bottom: i32::from(desk_h) - 1,
                            flags: MonitorFlags::PRIMARY,
                        }],
                    );
                    let id = server
                        .create_surface(surface_size.0, surface_size.1)
                        .ok_or_else(|| anyhow!("cannot create EGFX surface"))?;
                    server.map_surface_to_output(id, 0, 0);
                    debug!(id, ?surface_size, "EGFX surface created");
                    surface = Some(id);
                    need_keyframe = true;
                }
                if server.should_backpressure() {
                    continue;
                }
            }

            let frame = match slot.take() {
                Some(frame) => frame,
                None if need_keyframe => match last.take() {
                    Some(frame) => frame,
                    None => continue,
                },
                None => continue,
            };

            let encoded = match encoder.encode(&frame.0, need_keyframe) {
                Ok(encoded) => encoded,
                Err(err) => {
                    warn!(%err, "encode failed");
                    need_keyframe = true;
                    slot.restore(frame);
                    continue;
                }
            };
            if encoded.keyframe {
                debug!(bytes = encoded.data.len(), "keyframe");
            }
            last = Some(frame);

            let timestamp = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
            let (messages, channel_id) = {
                let mut server = self.gfx.lock().expect("EGFX server poisoned");
                let Some(id) = surface.filter(|id| server.get_surface(*id).is_some()) else {
                    continue;
                };
                match server.try_send_avc420_frame(
                    id,
                    &encoded.data,
                    std::slice::from_ref(&region),
                    timestamp,
                ) {
                    // A lost P-frame breaks the decoder's reference chain.
                    Err(err) => {
                        debug!(?err, "frame rejected");
                        need_keyframe = true;
                    }
                    Ok(_) => need_keyframe = false,
                }
                (server.drain_output(), server.channel_id())
            };

            frames += 1;
            bytes += encoded.data.len() as u64;
            if stats_at.elapsed() >= Duration::from_secs(10) {
                let secs = stats_at.elapsed().as_secs_f64();
                info!(
                    fps = format!("{:.1}", frames as f64 / secs),
                    mbps = format!("{:.2}", bytes as f64 * 8.0 / secs / 1e6),
                    "video"
                );
                (frames, bytes, stats_at) = (0, 0, Instant::now());
            }

            self.send(messages, channel_id)?;
        }
    }

    fn send(
        &self,
        messages: Vec<ironrdp_dvc::DvcMessage>,
        channel_id: Option<u32>,
    ) -> anyhow::Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let channel_id = channel_id.ok_or_else(|| anyhow!("EGFX channel has no id"))?;
        let messages =
            ironrdp_dvc::encode_dvc_messages(channel_id, messages, ChannelFlags::empty())
                .map_err(|e| anyhow!("encode EGFX messages: {e}"))?;
        let events = self.shared.events.lock().expect("poisoned");
        let events = events
            .as_ref()
            .ok_or_else(|| anyhow!("server event channel not set"))?;
        events
            .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                messages,
            }))
            .map_err(|_| anyhow!("server event loop is gone"))
    }
}

/// Builds EGFX handlers that wake the pipeline on protocol events.
pub struct GfxFactory {
    shared: Arc<Shared>,
}

impl GfxFactory {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }
}

impl ServerEventSender for GfxFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.shared.events.lock().expect("poisoned") = Some(sender);
    }
}

impl GfxServerFactory for GfxFactory {
    fn build_gfx_handler(&self, _ctx: GfxContext) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(GfxHandler {
            slot: Arc::clone(&self.shared.slot),
        })
    }
}

struct GfxHandler {
    slot: Arc<FrameSlot>,
}

impl GraphicsPipelineHandler for GfxHandler {
    fn capabilities_advertise(&mut self, pdu: &CapabilitiesAdvertisePdu) {
        debug!(?pdu, "EGFX capabilities advertised");
    }

    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        info!(?negotiated, "EGFX ready");
        self.slot.poke();
    }

    fn on_frame_ack(&mut self, _frame_id: u32, _queue_depth: u32, _total_frames_decoded: u32) {
        self.slot.poke();
    }

    fn on_close(&mut self) {
        debug!("EGFX channel closed");
    }
}
