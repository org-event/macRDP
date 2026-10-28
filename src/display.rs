//! Display side of the server: capture -> H.264 -> EGFX (AVC420).
//!
//! Each connection gets a pipeline thread. It waits for a captured frame or a
//! frame acknowledgement, keeps the EGFX surface in place (the client may reset
//! it at any time by advertising capabilities again), and sends one H.264 frame
//! whenever the client has room for it. Frames that arrive while the client is
//! behind are coalesced into the newest one.
//!
//! With `match_client` the stream comes from a virtual display sized to the
//! client, and follows the client's window as it is resized (Display Control).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow};
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_egfx::pdu::{Avc420Region, CapabilitiesAdvertisePdu, CapabilitySet};
use ironrdp_egfx::server::GraphicsPipelineHandler;
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_server::{
    DesktopSize, DisplayContext, EgfxServerMessage, GfxContext, GfxServerFactory, GfxServerHandle,
    RdpServerDisplay, RdpServerDisplayUpdates, ServerError, ServerErrorExt as _, ServerEvent,
    ServerEventSender, ServerResult,
};
use ironrdp_svc::ChannelFlags;
use objc2_core_graphics::{CGDisplayBounds, CGMainDisplayID};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info, warn};

use crate::capture::{Capture, Frame, FrameSlot, TestPattern};
use crate::encoder::Encoder;
use crate::virtual_display::{Mode, VirtualDisplay};

/// QP reported to the client for each region; informational for AVC420.
const REPORTED_QP: u8 = 22;

#[derive(Debug, Clone, Copy)]
pub struct DisplayConfig {
    pub fps: i32,
    pub bitrate: u32,
    /// Stream physical displays in pixels instead of points.
    pub hidpi: bool,
    /// Stream a virtual display sized to the client instead of the main display.
    pub match_client: bool,
    /// Send a synthetic picture instead of the screen.
    pub test_pattern: bool,
}

/// The display being streamed and how desktop pixels map to global points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Screen {
    pub display_id: u32,
    /// Desktop size announced to the client, in pixels.
    pub desktop: (u16, u16),
    /// Top-left corner in global display coordinates (points).
    pub origin: (f64, f64),
    /// Pixels per point.
    pub scale: f64,
}

impl Screen {
    /// Describes `display_id` in pixels (`hidpi`) or points.
    pub fn of_display(display_id: u32, hidpi: bool) -> anyhow::Result<Self> {
        let bounds = CGDisplayBounds(display_id);
        let scale = if hidpi {
            backing_scale(display_id)
        } else {
            1.0
        };
        // RDP clients expect an even desktop width and height.
        let even = |value: f64| u16::try_from(value.round() as i64).map(|v| v & !1);
        Ok(Self {
            display_id,
            desktop: (
                even(bounds.size.width * scale).context("display too wide")?,
                even(bounds.size.height * scale).context("display too tall")?,
            ),
            origin: (bounds.origin.x, bounds.origin.y),
            scale,
        })
    }

    /// Encoded frame size: the desktop rounded up to whole 16x16 macroblocks.
    fn surface(&self) -> (u16, u16) {
        (
            self.desktop.0.next_multiple_of(16),
            self.desktop.1.next_multiple_of(16),
        )
    }
}

fn backing_scale(display_id: u32) -> f64 {
    let points = CGDisplayBounds(display_id).size.width;
    let mode = objc2_core_graphics::CGDisplayCopyDisplayMode(display_id);
    let pixels = objc2_core_graphics::CGDisplayMode::pixel_width(mode.as_deref());
    if points > 0.0 && pixels > 0 {
        pixels as f64 / points
    } else {
        1.0
    }
}

/// The current [`Screen`], shared with input; each change bumps the generation.
pub struct ScreenState {
    screen: Mutex<Screen>,
    generation: AtomicU64,
}

impl ScreenState {
    pub fn new(screen: Screen) -> Self {
        Self {
            screen: Mutex::new(screen),
            generation: AtomicU64::new(0),
        }
    }

    pub fn get(&self) -> Screen {
        *self.screen.lock().expect("poisoned")
    }

    fn snapshot(&self) -> (u64, Screen) {
        let screen = self.screen.lock().expect("poisoned");
        (self.generation.load(Ordering::Acquire), *screen)
    }

    fn set(&self, screen: Screen) {
        let mut current = self.screen.lock().expect("poisoned");
        if *current != screen {
            *current = screen;
            self.generation.fetch_add(1, Ordering::AcqRel);
            info!(?screen, "screen changed");
        }
    }
}

/// State shared by the display handler and the EGFX factory.
pub struct Shared {
    events: Mutex<Option<UnboundedSender<ServerEvent>>>,
    /// Wakes the pipeline on new frames, EGFX events and screen changes.
    slot: Arc<FrameSlot>,
    screen: Arc<ScreenState>,
    /// Exists while a client is connected in `match_client` mode.
    virtual_display: Mutex<Option<VirtualDisplay>>,
}

impl Shared {
    pub fn new(screen: Arc<ScreenState>) -> Self {
        Self {
            events: Mutex::new(None),
            slot: Arc::default(),
            screen,
            virtual_display: Mutex::new(None),
        }
    }
}

pub struct MacDisplay {
    config: DisplayConfig,
    shared: Arc<Shared>,
    /// What the client asked for last; the virtual display follows it.
    wanted: Option<Mode>,
}

impl MacDisplay {
    pub fn new(config: DisplayConfig, shared: Arc<Shared>) -> Self {
        Self {
            config,
            shared,
            wanted: None,
        }
    }

    fn virtual_enabled(&self) -> bool {
        self.config.match_client && !self.config.test_pattern
    }

    /// Brings the virtual display to the wanted mode, creating it if needed.
    /// On failure streaming falls back to the main display.
    fn sync_virtual_display(&mut self) {
        let Some(mode) = self.wanted.filter(|_| self.virtual_enabled()) else {
            return;
        };
        let mut slot = self.shared.virtual_display.lock().expect("poisoned");
        let result = match slot.as_mut() {
            Some(display) => display.set_mode(mode).map(|()| display.id()),
            None => VirtualDisplay::new(mode).map(|display| {
                let id = display.id();
                *slot = Some(display);
                id
            }),
        };
        let screen = result.and_then(|id| Screen::of_display(id, true));
        drop(slot);
        match screen {
            Ok(screen) => self.shared.screen.set(screen),
            Err(err) => {
                warn!(%err, "cannot match the client size; streaming the main display");
                self.shared.virtual_display.lock().expect("poisoned").take();
                self.wanted = None;
                self.restore_main_screen();
            }
        }
        self.shared.slot.poke();
    }

    fn restore_main_screen(&self) {
        match Screen::of_display(CGMainDisplayID(), self.config.hidpi) {
            Ok(screen) => self.shared.screen.set(screen),
            Err(err) => warn!(%err, "cannot describe the main display"),
        }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for MacDisplay {
    async fn size(&mut self) -> DesktopSize {
        let (width, height) = self.shared.screen.get().desktop;
        DesktopSize { width, height }
    }

    async fn request_initial_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        if self.virtual_enabled() {
            let (width, height) = (u32::from(client_size.width), u32::from(client_size.height));
            info!(width, height, "client desktop size");
            self.wanted = Some(Mode::for_client(width, height, 100));
            self.sync_virtual_display();
        }
        self.size().await
    }

    async fn offers_display_control(&mut self) -> bool {
        self.virtual_enabled()
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let monitors = layout.monitors();
        let Some(monitor) = monitors
            .iter()
            .find(|m| m.is_primary())
            .or_else(|| monitors.first())
        else {
            return;
        };
        let (width, height) = monitor.dimensions();
        let scale = monitor.desktop_scale_factor().unwrap_or(100);
        info!(
            width,
            height,
            scale,
            monitors = monitors.len(),
            "client layout"
        );
        self.wanted = Some(Mode::for_client(width, height, scale));
        self.sync_virtual_display();
    }

    async fn updates(
        &mut self,
        ctx: DisplayContext,
    ) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        // A previous session of this connection removed the display on drop.
        self.sync_virtual_display();
        let session = Session::start(self.config, Arc::clone(&self.shared), ctx)
            .map_err(|e| ServerError::custom("display", std::io::Error::other(format!("{e:#}"))))?;
        Ok(Box::new(session))
    }
}

/// One connection's pipeline; dropping it stops it and removes the virtual display.
struct Session {
    shared: Arc<Shared>,
    hidpi: bool,
    stop: Arc<AtomicBool>,
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
        let stop = Arc::new(AtomicBool::new(false));
        let pipeline = Pipeline {
            config,
            gfx,
            shared: Arc::clone(&shared),
            stop: Arc::clone(&stop),
            suppressed: ctx.display_suppressed,
        };
        let thread = std::thread::Builder::new()
            .name("macrdp-pipeline".into())
            .spawn(move || pipeline.run())
            .context("spawn pipeline thread")?;
        info!(?config, screen = ?shared.screen.get(), "display session started");

        Ok(Self {
            shared,
            hidpi: config.hidpi,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.shared.slot.poke();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if self
            .shared
            .virtual_display
            .lock()
            .expect("poisoned")
            .take()
            .is_some()
        {
            match Screen::of_display(CGMainDisplayID(), self.hidpi) {
                Ok(screen) => self.shared.screen.set(screen),
                Err(err) => warn!(%err, "cannot describe the main display"),
            }
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

/// Capture and encoder for one screen size.
struct Video {
    screen: Screen,
    _source: Box<dyn Send>,
    encoder: Encoder,
    region: Avc420Region,
}

impl Video {
    fn open(screen: Screen, config: &DisplayConfig, slot: &Arc<FrameSlot>) -> anyhow::Result<Self> {
        let surface = screen.surface();
        let content = (usize::from(screen.desktop.0), usize::from(screen.desktop.1));
        let buffer = (usize::from(surface.0), usize::from(surface.1));
        let source: Box<dyn Send> = if config.test_pattern {
            Box::new(TestPattern::start(
                content,
                buffer,
                config.fps,
                Arc::clone(slot),
            )?)
        } else {
            Box::new(Self::capture(
                screen.display_id,
                content,
                buffer,
                config.fps,
                slot,
            )?)
        };
        let encoder = Encoder::new(buffer.0, buffer.1, config.fps, config.bitrate)?;
        Ok(Self {
            screen,
            _source: source,
            encoder,
            region: Avc420Region {
                left: 0,
                top: 0,
                right: screen.desktop.0,
                bottom: screen.desktop.1,
                quantization_parameter: REPORTED_QP,
                quality: 100,
            },
        })
    }
}

impl Video {
    fn capture(
        display_id: u32,
        content: (usize, usize),
        buffer: (usize, usize),
        fps: i32,
        slot: &Arc<FrameSlot>,
    ) -> anyhow::Result<Capture> {
        let mut attempt = 1;
        loop {
            match Capture::start(display_id, content, buffer, fps, Arc::clone(slot)) {
                Ok(capture) => return Ok(capture),
                Err(err) if attempt < 3 => {
                    warn!(%err, attempt, "capture did not start, retrying");
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
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
        let slot = &self.shared.slot;
        let started = Instant::now();
        let mut seen = 0;
        let mut video: Option<Video> = None;
        let mut generation = u64::MAX;
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
                // The client may drop what arrived while it was hidden.
                need_keyframe = true;
                continue;
            }

            let (current, screen) = self.shared.screen.snapshot();
            if current != generation {
                // Stop the old capture before starting one on the same display.
                drop(video.take());
                last = None;
                while slot.take().is_some() {}
                video = Some(Video::open(screen, &self.config, slot)?);
                generation = current;
                surface = None;
                debug!(?screen, "video source ready");
            }
            let video = video.as_mut().expect("opened above");
            let (desk_w, desk_h) = video.screen.desktop;
            let surface_size = video.screen.surface();

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
                    // Also drops the previous surface. Some clients (FreeRDP)
                    // reject a ResetGraphics without monitors.
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

            let encoded = match video.encoder.encode(&frame.0, need_keyframe) {
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
                    std::slice::from_ref(&video.region),
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
