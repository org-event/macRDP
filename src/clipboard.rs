//! Clipboard sharing (CLIPRDR): text and images both ways.
//!
//! The Mac side is the general pasteboard, polled for changes since macOS has no
//! change notification. A copy on the Mac announces its formats to the client,
//! which fetches the data when it pastes. A copy on the client is fetched right
//! away and written to the pasteboard, so any Mac app can paste it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use ironrdp_cliprdr::backend::{
    ClipboardMessage, CliprdrBackend, CliprdrBackendFactory,
};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardFormatName, ClipboardGeneralCapabilityFlags,
    FileContentsRequest, FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
    OwnedFormatDataResponse,
};
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use objc2::rc::autoreleasepool;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypePNG,
    NSPasteboardTypeString, NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSData, NSDictionary, NSString};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

const POLL: Duration = Duration::from_millis(300);
/// Our id for the registered "PNG" format; registered ids start at 0xC000.
const PNG_FORMAT: ClipboardFormatId = ClipboardFormatId(0xC0A0);
const PNG_NAME: &str = "PNG";

#[derive(Default)]
pub struct MacClipboardFactory {
    sender: Option<UnboundedSender<ServerEvent>>,
}

impl CliprdrBackendFactory for MacClipboardFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(MacClipboard::new(self.sender.clone()))
    }
}

impl ServerEventSender for MacClipboardFactory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        self.sender = Some(sender);
    }
}

impl CliprdrServerFactory for MacClipboardFactory {}

/// What a client copy is fetched as, in request order.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Fetch {
    Text,
    Png,
    Dib,
}

#[derive(Default)]
struct Incoming {
    queue: VecDeque<(Fetch, ClipboardFormatId)>,
    text: Option<String>,
    png: Option<Vec<u8>>,
}

struct Shared {
    sender: Option<UnboundedSender<ServerEvent>>,
    /// Pasteboard change count of our own last write, not to be announced back.
    own_change: Mutex<isize>,
    stop: AtomicBool,
}

impl Shared {
    fn send(&self, message: ClipboardMessage) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(ServerEvent::Clipboard(message));
        }
    }

    /// Tells the client what the pasteboard holds now.
    fn announce(&self) {
        let formats = autoreleasepool(|_| local_formats());
        debug!(?formats, "announcing the Mac clipboard");
        self.send(ClipboardMessage::SendInitiateCopy(formats));
    }
}

pub struct MacClipboard {
    shared: Arc<Shared>,
    watcher: Option<JoinHandle<()>>,
    incoming: Incoming,
}

impl core::fmt::Debug for MacClipboard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("MacClipboard")
    }
}

impl ironrdp_pdu::ironrdp_core::AsAny for MacClipboard {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

impl MacClipboard {
    fn new(sender: Option<UnboundedSender<ServerEvent>>) -> Self {
        Self {
            shared: Arc::new(Shared {
                sender,
                own_change: Mutex::new(-1),
                stop: AtomicBool::new(false),
            }),
            watcher: None,
            incoming: Incoming::default(),
        }
    }

    fn start_watching(&mut self) {
        if self.watcher.is_some() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        self.watcher = std::thread::Builder::new()
            .name("macrdp-clipboard".into())
            .spawn(move || watch(&shared))
            .map_err(|err| warn!(%err, "cannot watch the clipboard"))
            .ok();
    }

    fn request_next(&mut self) {
        if let Some(&(_, format)) = self.incoming.queue.front() {
            self.shared.send(ClipboardMessage::SendInitiatePaste(format));
            return;
        }
        let Incoming { text, png, .. } = std::mem::take(&mut self.incoming);
        if text.is_none() && png.is_none() {
            return;
        }
        let change = autoreleasepool(|_| write_pasteboard(text.as_deref(), png.as_deref()));
        *self.shared.own_change.lock().expect("poisoned") = change;
        info!(
            text = text.as_ref().map(String::len),
            png = png.as_ref().map(Vec::len),
            "client clipboard copied to the Mac"
        );
    }
}

impl Drop for MacClipboard {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

impl CliprdrBackend for MacClipboard {
    fn temporary_directory(&self) -> &str {
        ".cliprdr"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {
        self.shared.announce();
        self.start_watching();
    }

    fn on_request_format_list(&mut self) {
        self.shared.announce();
    }

    fn on_process_negotiated_capabilities(&mut self, capabilities: ClipboardGeneralCapabilityFlags) {
        debug!(?capabilities, "clipboard capabilities");
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        debug!(?available_formats, "client clipboard changed");
        let find = |wanted: &dyn Fn(&ClipboardFormat) -> bool| {
            available_formats.iter().find(|f| wanted(f)).map(ClipboardFormat::id)
        };
        let mut queue = VecDeque::new();
        if let Some(id) = find(&|f| f.id() == ClipboardFormatId::CF_UNICODETEXT) {
            queue.push_back((Fetch::Text, id));
        }
        let png = find(&|f| f.name().is_some_and(|n| n.value().eq_ignore_ascii_case(PNG_NAME)));
        let dib = find(&|f| f.id() == ClipboardFormatId::CF_DIBV5)
            .or_else(|| find(&|f| f.id() == ClipboardFormatId::CF_DIB));
        // PNG first; the bitmap is the fallback if the client cannot render it.
        if let Some(id) = png {
            queue.push_back((Fetch::Png, id));
        }
        if let Some(id) = dib {
            queue.push_back((Fetch::Dib, id));
        }
        // A new copy replaces one still being fetched.
        self.incoming = Incoming {
            queue,
            ..Incoming::default()
        };
        self.request_next();
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let response = autoreleasepool(|_| local_data(request.format));
        let response = response.unwrap_or_else(|| {
            debug!(format = ?request.format, "clipboard format not available");
            OwnedFormatDataResponse::new_error()
        });
        self.shared.send(ClipboardMessage::SendFormatData(response));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let Some((fetch, format)) = self.incoming.queue.pop_front() else {
            return;
        };
        if response.is_error() {
            debug!(?format, "client could not render the clipboard format");
        } else {
            match fetch {
                Fetch::Text => match response.to_unicode_string() {
                    Ok(text) => self.incoming.text = Some(text),
                    Err(err) => warn!(%err, "bad clipboard text"),
                },
                Fetch::Png => {
                    self.incoming.png = Some(response.data().to_vec());
                    self.incoming.queue.retain(|&(fetch, _)| fetch != Fetch::Dib);
                }
                Fetch::Dib => {
                    let data = response.data();
                    let png = if format == ClipboardFormatId::CF_DIBV5 {
                        ironrdp_cliprdr_format::bitmap::dibv5_to_png(data)
                    } else {
                        ironrdp_cliprdr_format::bitmap::dib_to_png(data)
                    };
                    match png {
                        Ok(png) => self.incoming.png = Some(png),
                        Err(err) => warn!(%err, "bad clipboard bitmap"),
                    }
                }
            }
        }
        self.request_next();
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

/// Announces every pasteboard change that is not our own write.
fn watch(shared: &Shared) {
    let mut seen = NSPasteboard::generalPasteboard().changeCount();
    while !shared.stop.load(Ordering::Acquire) {
        std::thread::sleep(POLL);
        let change = NSPasteboard::generalPasteboard().changeCount();
        if change == seen {
            continue;
        }
        seen = change;
        if change != *shared.own_change.lock().expect("poisoned") {
            shared.announce();
        }
    }
}

fn local_formats() -> Vec<ClipboardFormat> {
    let pasteboard = NSPasteboard::generalPasteboard();
    let mut formats = Vec::new();
    // SAFETY: AppKit's pasteboard type constants.
    let (string, png, tiff) =
        unsafe { (NSPasteboardTypeString, NSPasteboardTypePNG, NSPasteboardTypeTIFF) };
    if pasteboard.stringForType(string).is_some() {
        formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
    }
    if pasteboard.dataForType(png).is_some() || pasteboard.dataForType(tiff).is_some() {
        formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIB));
        formats.push(
            ClipboardFormat::new(PNG_FORMAT).with_name(ClipboardFormatName::new_static(PNG_NAME)),
        );
    }
    formats
}

fn local_data(format: ClipboardFormatId) -> Option<OwnedFormatDataResponse> {
    let pasteboard = NSPasteboard::generalPasteboard();
    if format == ClipboardFormatId::CF_UNICODETEXT {
        // SAFETY: AppKit's pasteboard type constant.
        let text = pasteboard.stringForType(unsafe { NSPasteboardTypeString })?;
        return Some(OwnedFormatDataResponse::new_unicode_string(&text.to_string()));
    }
    let png = local_png(&pasteboard)?;
    if format == PNG_FORMAT {
        Some(OwnedFormatDataResponse::new_data(png))
    } else if format == ClipboardFormatId::CF_DIB {
        match ironrdp_cliprdr_format::bitmap::png_to_cf_dib(&png) {
            Ok(dib) => Some(OwnedFormatDataResponse::new_data(dib)),
            Err(err) => {
                warn!(%err, "cannot convert the clipboard image");
                None
            }
        }
    } else {
        None
    }
}

/// The pasteboard image as PNG, converted from TIFF when that is all there is.
fn local_png(pasteboard: &NSPasteboard) -> Option<Vec<u8>> {
    // SAFETY: AppKit's pasteboard type constants.
    if let Some(png) = pasteboard.dataForType(unsafe { NSPasteboardTypePNG }) {
        return Some(png.to_vec());
    }
    let tiff = pasteboard.dataForType(unsafe { NSPasteboardTypeTIFF })?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
    // SAFETY: PNG takes no required properties.
    let png = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    Some(png.to_vec())
}

/// Replaces the pasteboard contents; returns the resulting change count.
fn write_pasteboard(text: Option<&str>, png: Option<&[u8]>) -> isize {
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    if let Some(text) = text {
        // SAFETY: AppKit's pasteboard type constant.
        pasteboard.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString });
    }
    if let Some(png) = png {
        let data = NSData::with_bytes(png);
        // SAFETY: AppKit's pasteboard type constant.
        pasteboard.setData_forType(Some(&data), unsafe { NSPasteboardTypePNG });
    }
    pasteboard.changeCount()
}
