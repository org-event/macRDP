/*
 * OpenRDP: macOS platform subsystem for the FreeRDP shadow server.
 *
 * Capture:  ScreenCaptureKit, main display, logical (point) resolution, dirty rects.
 * Input:    CGEventPost with explicit modifier flags (needs Accessibility permission).
 * Cursor:   shape is sent as an RDP pointer so the client draws it locally.
 *
 * Based on the unmaintained FreeRDP server/shadow/Mac subsystem (CGDisplayStream).
 */

#import <AppKit/AppKit.h>
#import <CoreMedia/CoreMedia.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>

#include <winpr/crt.h>
#include <winpr/input.h>
#include <winpr/synch.h>
#include <winpr/thread.h>

#include <freerdp/codec/color.h>
#include <freerdp/codec/region.h>
#include <freerdp/log.h>

#include "mac_subsystem.h"

#define TAG "com.openrdp.subsystem"

/* RDP pointer updates are limited to 384x384 (large pointer); keep to the common 96. */
#define MAX_CURSOR_SIZE 96

typedef struct
{
	rdpShadowSubsystem common;

	CGDirectDisplayID displayId;
	UINT32 width;  /* points */
	UINT32 height; /* points */
	BOOL haveFullFrame;

	CGEventFlags modifiers;
	BOOL buttonDown[3];

	UINT64 cursorHash;
	dispatch_source_t cursorTimer;

	HANDLE msgThread;
	void* capture; /* MacRdpCapture*, retained */
} MacRdpSubsystem;

@interface MacRdpCapture : NSObject <SCStreamOutput, SCStreamDelegate>
@property(nonatomic, assign) MacRdpSubsystem* subsystem;
@property(nonatomic, strong) SCStream* stream;
@property(nonatomic, strong) dispatch_queue_t queue;
- (BOOL)startWithDisplay:(CGDirectDisplayID)displayId width:(UINT32)w height:(UINT32)h;
- (void)stop;
@end

/* ---------- surface updates ---------- */

static size_t client_count(rdpShadowServer* server)
{
	ArrayList_Lock(server->clients);
	const size_t count = ArrayList_Count(server->clients);
	ArrayList_Unlock(server->clients);
	return count;
}

/* Hands the accumulated invalid region to the encoder threads and waits for them. */
static void publish_frame(MacRdpSubsystem* mac)
{
	rdpShadowServer* server = mac->common.server;
	rdpShadowSurface* surface = server->surface;

	EnterCriticalSection(&surface->lock);
	const BOOL empty = region16_is_empty(&surface->invalidRegion);
	LeaveCriticalSection(&surface->lock);

	if (empty || client_count(server) < 1)
		return;

	shadow_subsystem_frame_update(&mac->common);

	EnterCriticalSection(&surface->lock);
	region16_clear(&surface->invalidRegion);
	LeaveCriticalSection(&surface->lock);
}

static void copy_frame(MacRdpSubsystem* mac, CVPixelBufferRef pixels, NSArray* dirtyRects)
{
	rdpShadowSurface* surface = mac->common.server->surface;

	CVPixelBufferLockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly);
	const BYTE* src = CVPixelBufferGetBaseAddress(pixels);
	const UINT32 srcStep = (UINT32)CVPixelBufferGetBytesPerRow(pixels);
	const UINT32 srcW = (UINT32)CVPixelBufferGetWidth(pixels);
	const UINT32 srcH = (UINT32)CVPixelBufferGetHeight(pixels);

	EnterCriticalSection(&surface->lock);
	const UINT32 w = MIN(srcW, surface->width);
	const UINT32 h = MIN(srcH, surface->height);
	const RECTANGLE_16 bounds = { 0, 0, (UINT16)w, (UINT16)h };

	NSMutableArray* rects = [NSMutableArray array];
	if (!mac->haveFullFrame || dirtyRects.count == 0)
	{
		[rects addObject:[NSValue valueWithRect:NSMakeRect(0, 0, w, h)]];
		mac->haveFullFrame = TRUE;
	}
	else
	{
		for (NSDictionary* d in dirtyRects)
		{
			CGRect r = CGRectZero;
			if (CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)d, &r))
				[rects addObject:[NSValue valueWithRect:NSRectFromCGRect(r)]];
		}
	}

	for (NSValue* v in rects)
	{
		const CGRect r = CGRectIntegral(NSRectToCGRect(v.rectValue));
		RECTANGLE_16 rect = { (UINT16)MAX(0, r.origin.x), (UINT16)MAX(0, r.origin.y),
			                  (UINT16)MAX(0, CGRectGetMaxX(r)), (UINT16)MAX(0, CGRectGetMaxY(r)) };
		if (!rectangles_intersection(&rect, &bounds, &rect))
			continue;

		const UINT32 x = rect.left;
		const UINT32 y = rect.top;
		if (!freerdp_image_copy_no_overlap(surface->data, surface->format, surface->scanline, x, y,
		                                   rect.right - x, rect.bottom - y, src,
		                                   PIXEL_FORMAT_BGRX32, srcStep, x, y, NULL,
		                                   FREERDP_FLIP_NONE))
			continue;
		region16_union_rect(&surface->invalidRegion, &surface->invalidRegion, &rect);
	}
	LeaveCriticalSection(&surface->lock);

	CVPixelBufferUnlockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly);
}

@implementation MacRdpCapture

- (BOOL)startWithDisplay:(CGDirectDisplayID)displayId width:(UINT32)w height:(UINT32)h
{
	__block SCShareableContent* content = nil;
	__block NSError* error = nil;
	dispatch_semaphore_t done = dispatch_semaphore_create(0);
	[SCShareableContent getShareableContentWithCompletionHandler:^(SCShareableContent* c,
	                                                               NSError* e) {
	  content = c;
	  error = e;
	  dispatch_semaphore_signal(done);
	}];
	dispatch_semaphore_wait(done, DISPATCH_TIME_FOREVER);

	if (!content)
	{
		WLog_ERR(TAG, "ScreenCaptureKit: no shareable content (%s). Grant Screen Recording.",
		         error.localizedDescription.UTF8String);
		return NO;
	}

	SCDisplay* display = nil;
	for (SCDisplay* d in content.displays)
		if (d.displayID == displayId)
			display = d;
	if (!display)
	{
		WLog_ERR(TAG, "ScreenCaptureKit: display %u not found", displayId);
		return NO;
	}

	SCContentFilter* filter = [[SCContentFilter alloc] initWithDisplay:display
	                                                  excludingWindows:@[]];
	SCStreamConfiguration* config = [[SCStreamConfiguration alloc] init];
	config.width = w;
	config.height = h;
	config.pixelFormat = kCVPixelFormatType_32BGRA;
	config.showsCursor = NO;
	config.minimumFrameInterval = CMTimeMake(1, 60);
	config.queueDepth = 4;
	config.colorSpaceName = kCGColorSpaceSRGB;

	self.queue = dispatch_queue_create("openrdp.capture", DISPATCH_QUEUE_SERIAL);
	self.stream = [[SCStream alloc] initWithFilter:filter configuration:config delegate:self];

	if (![self.stream addStreamOutput:self
	                             type:SCStreamOutputTypeScreen
	               sampleHandlerQueue:self.queue
	                            error:&error])
	{
		WLog_ERR(TAG, "addStreamOutput: %s", error.localizedDescription.UTF8String);
		return NO;
	}

	__block BOOL ok = NO;
	[self.stream startCaptureWithCompletionHandler:^(NSError* e) {
	  ok = (e == nil);
	  if (e)
		  WLog_ERR(TAG, "startCapture: %s", e.localizedDescription.UTF8String);
	  dispatch_semaphore_signal(done);
	}];
	dispatch_semaphore_wait(done, DISPATCH_TIME_FOREVER);
	return ok;
}

- (void)stop
{
	dispatch_semaphore_t done = dispatch_semaphore_create(0);
	[self.stream stopCaptureWithCompletionHandler:^(NSError* e) {
	  dispatch_semaphore_signal(done);
	}];
	dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_SEC));
	self.stream = nil;
}

- (void)stream:(SCStream*)stream
    didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
                   ofType:(SCStreamOutputType)type
{
	if (type != SCStreamOutputTypeScreen || !CMSampleBufferIsValid(sampleBuffer))
		return;

	CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, false);
	if (!attachments || CFArrayGetCount(attachments) < 1)
		return;
	NSDictionary* info = (__bridge NSDictionary*)CFArrayGetValueAtIndex(attachments, 0);
	if ([info[SCStreamFrameInfoStatus] integerValue] != SCFrameStatusComplete)
		return;

	CVPixelBufferRef pixels = CMSampleBufferGetImageBuffer(sampleBuffer);
	if (!pixels)
		return;

	copy_frame(self.subsystem, pixels, info[SCStreamFrameInfoDirtyRects]);
	publish_frame(self.subsystem);
}

- (void)stream:(SCStream*)stream didStopWithError:(NSError*)error
{
	WLog_ERR(TAG, "capture stopped: %s", error.localizedDescription.UTF8String);
}

@end

/* ---------- cursor ---------- */

static void message_free(UINT32 id, SHADOW_MSG_OUT* msg)
{
	if (id == SHADOW_MSG_OUT_POINTER_ALPHA_UPDATE_ID)
	{
		SHADOW_MSG_OUT_POINTER_ALPHA_UPDATE* m = (SHADOW_MSG_OUT_POINTER_ALPHA_UPDATE*)msg;
		free(m->xorMaskData);
		free(m->andMaskData);
	}
	free(msg);
}

static UINT64 fnv1a(const BYTE* data, size_t len, UINT64 h)
{
	for (size_t i = 0; i < len; i++)
		h = (h ^ data[i]) * 0x100000001b3ULL;
	return h;
}

/* Renders the current system cursor and broadcasts it when the shape changes. Main thread. */
static void cursor_poll(MacRdpSubsystem* mac)
{
	@autoreleasepool
	{
		NSCursor* cursor = [NSCursor currentSystemCursor];
		if (!cursor || !cursor.image)
			return;

		const NSSize size = cursor.image.size;
		const UINT32 w = MIN(MAX_CURSOR_SIZE, (UINT32)ceil(size.width));
		const UINT32 h = MIN(MAX_CURSOR_SIZE, (UINT32)ceil(size.height));
		if (w == 0 || h == 0)
			return;

		BYTE* pixels = calloc(1ull * w * h, 4);
		if (!pixels)
			return;

		CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
		CGContextRef ctx =
		    CGBitmapContextCreate(pixels, w, h, 8, w * 4ull, cs,
		                          kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little);
		CGColorSpaceRelease(cs);
		if (!ctx)
		{
			free(pixels);
			return;
		}

		NSGraphicsContext* gc = [NSGraphicsContext graphicsContextWithCGContext:ctx flipped:NO];
		[NSGraphicsContext saveGraphicsState];
		NSGraphicsContext.currentContext = gc;
		[cursor.image drawInRect:NSMakeRect(0, 0, w, h)];
		[NSGraphicsContext restoreGraphicsState];
		CGContextRelease(ctx);

		const NSPoint hot = cursor.hotSpot;
		UINT64 hash = fnv1a(pixels, 4ull * w * h, 0xcbf29ce484222325ULL);
		hash = fnv1a((const BYTE*)&hot, sizeof(hot), hash);

		if (hash != mac->cursorHash && client_count(mac->common.server) > 0)
		{
			SHADOW_MSG_OUT_POINTER_ALPHA_UPDATE* msg = calloc(1, sizeof(*msg));
			if (msg)
			{
				msg->xHot = (UINT32)MIN(MAX(0, hot.x), w - 1);
				msg->yHot = (UINT32)MIN(MAX(0, hot.y), h - 1);
				msg->width = w;
				msg->height = h;
				msg->common.Free = message_free;
				if (shadow_subsystem_pointer_convert_alpha_pointer_data_to_format(
				        pixels, PIXEL_FORMAT_BGRA32, TRUE, w, h, msg) >= 0 &&
				    shadow_client_boardcast_msg(mac->common.server, NULL,
				                                SHADOW_MSG_OUT_POINTER_ALPHA_UPDATE_ID,
				                                (SHADOW_MSG_OUT*)msg, NULL) >= 0)
					mac->cursorHash = hash;
			}
		}
		free(pixels);
	}
}

/* ---------- input ---------- */

static CGEventFlags modifier_mask(CGKeyCode key)
{
	switch (key)
	{
		case 0x37: /* Command */
		case 0x36: /* Right Command */
			return kCGEventFlagMaskCommand;
		case 0x38: /* Shift */
		case 0x3C: /* Right Shift */
			return kCGEventFlagMaskShift;
		case 0x3A: /* Option */
		case 0x3D: /* Right Option */
			return kCGEventFlagMaskAlternate;
		case 0x3B: /* Control */
		case 0x3E: /* Right Control */
			return kCGEventFlagMaskControl;
		default:
			return 0;
	}
}

static void post_event(CGEventRef ev, CGEventFlags flags)
{
	if (!ev)
		return;
	CGEventSetFlags(ev, flags);
	CGEventPost(kCGHIDEventTap, ev);
	CFRelease(ev);
}

static BOOL input_synchronize(rdpShadowSubsystem* subsystem, rdpShadowClient* client, UINT32 flags)
{
	/* Client focus regained: forget held modifiers so none get stuck. */
	((MacRdpSubsystem*)subsystem)->modifiers = 0;
	return TRUE;
}

static BOOL input_keyboard(rdpShadowSubsystem* subsystem, rdpShadowClient* client, UINT16 flags,
                           UINT8 code)
{
	MacRdpSubsystem* mac = (MacRdpSubsystem*)subsystem;
	const BOOL extended = (flags & KBD_FLAGS_EXTENDED) != 0;
	const BOOL down = (flags & KBD_FLAGS_RELEASE) == 0;

	const DWORD scancode = code | (extended ? KBDEXT : 0);
	DWORD vk = GetVirtualKeyCodeFromVirtualScanCode(scancode, WINPR_KBD_TYPE_IBM_ENHANCED);
	if (extended)
		vk |= KBDEXT;
	const DWORD keycode = GetKeycodeFromVirtualKeyCode(vk, WINPR_KEYCODE_TYPE_APPLE);

	WLog_DBG(TAG, "key scancode=0x%03" PRIx32 " vk=0x%03" PRIx32 " mac=0x%02" PRIx32 " %s",
	         scancode, vk, keycode, down ? "down" : "up");

	if (keycode == 0 && (vk & 0xFF) != VK_KEY_A)
		return TRUE;

	const CGEventFlags mask = modifier_mask((CGKeyCode)keycode);
	if (mask)
		mac->modifiers = down ? (mac->modifiers | mask) : (mac->modifiers & ~mask);

	post_event(CGEventCreateKeyboardEvent(NULL, (CGKeyCode)keycode, down), mac->modifiers);
	return TRUE;
}

static BOOL input_unicode(rdpShadowSubsystem* subsystem, rdpShadowClient* client, UINT16 flags,
                          UINT16 code)
{
	MacRdpSubsystem* mac = (MacRdpSubsystem*)subsystem;
	const BOOL down = (flags & KBD_FLAGS_RELEASE) == 0;
	CGEventRef ev = CGEventCreateKeyboardEvent(NULL, 0, down);
	if (!ev)
		return FALSE;
	const UniChar ch = code;
	CGEventKeyboardSetUnicodeString(ev, 1, &ch);
	post_event(ev, mac->modifiers);
	return TRUE;
}

/* Sign-extends the 9-bit wheel rotation value. 120 units = one notch. */
static int wheel_delta(UINT16 flags)
{
	int v = flags & WheelRotationMask;
	if (flags & PTR_FLAGS_WHEEL_NEGATIVE)
		v -= 0x200;
	return v;
}

static void post_mouse(MacRdpSubsystem* mac, CGEventType type, CGMouseButton button, UINT16 x,
                       UINT16 y)
{
	CGEventRef ev = CGEventCreateMouseEvent(NULL, type, CGPointMake(x, y), button);
	post_event(ev, mac->modifiers);
}

static BOOL input_mouse(rdpShadowSubsystem* subsystem, rdpShadowClient* client, UINT16 flags,
                        UINT16 x, UINT16 y)
{
	MacRdpSubsystem* mac = (MacRdpSubsystem*)subsystem;

	if (flags & (PTR_FLAGS_WHEEL | PTR_FLAGS_HWHEEL))
	{
		/* Pixel units keep high-resolution trackpad deltas smooth: 120 units -> 40 px. */
		const int px = wheel_delta(flags) / 3;
		const BOOL horizontal = (flags & PTR_FLAGS_HWHEEL) != 0;
		CGEventRef ev = CGEventCreateScrollWheelEvent(NULL, kCGScrollEventUnitPixel, 2,
		                                              horizontal ? 0 : px, horizontal ? -px : 0);
		post_event(ev, mac->modifiers);
		return TRUE;
	}

	const BOOL down = (flags & PTR_FLAGS_DOWN) != 0;

	if (flags & PTR_FLAGS_MOVE)
	{
		CGEventType type = kCGEventMouseMoved;
		CGMouseButton button = kCGMouseButtonLeft;
		if (mac->buttonDown[0])
			type = kCGEventLeftMouseDragged;
		else if (mac->buttonDown[1])
			type = kCGEventRightMouseDragged, button = kCGMouseButtonRight;
		else if (mac->buttonDown[2])
			type = kCGEventOtherMouseDragged, button = kCGMouseButtonCenter;
		post_mouse(mac, type, button, x, y);
	}

	if (flags & PTR_FLAGS_BUTTON1)
	{
		mac->buttonDown[0] = down;
		post_mouse(mac, down ? kCGEventLeftMouseDown : kCGEventLeftMouseUp, kCGMouseButtonLeft, x,
		           y);
	}
	else if (flags & PTR_FLAGS_BUTTON2)
	{
		mac->buttonDown[1] = down;
		post_mouse(mac, down ? kCGEventRightMouseDown : kCGEventRightMouseUp, kCGMouseButtonRight,
		           x, y);
	}
	else if (flags & PTR_FLAGS_BUTTON3)
	{
		mac->buttonDown[2] = down;
		post_mouse(mac, down ? kCGEventOtherMouseDown : kCGEventOtherMouseUp,
		           kCGMouseButtonCenter, x, y);
	}
	return TRUE;
}

static BOOL input_extended_mouse(rdpShadowSubsystem* subsystem, rdpShadowClient* client,
                                 UINT16 flags, UINT16 x, UINT16 y)
{
	MacRdpSubsystem* mac = (MacRdpSubsystem*)subsystem;
	const BOOL down = (flags & PTR_XFLAGS_DOWN) != 0;
	CGMouseButton button = 0;
	if (flags & PTR_XFLAGS_BUTTON1)
		button = 3; /* back */
	else if (flags & PTR_XFLAGS_BUTTON2)
		button = 4; /* forward */
	else
		return TRUE;

	CGEventRef ev = CGEventCreateMouseEvent(
	    NULL, down ? kCGEventOtherMouseDown : kCGEventOtherMouseUp, CGPointMake(x, y), button);
	if (ev)
		CGEventSetIntegerValueField(ev, kCGMouseEventButtonNumber, button);
	post_event(ev, mac->modifiers);
	return TRUE;
}

/* ---------- lifecycle ---------- */

static void detect_display(MacRdpSubsystem* mac)
{
	mac->displayId = CGMainDisplayID();
	const CGRect bounds = CGDisplayBounds(mac->displayId);
	mac->width = (UINT32)bounds.size.width;
	mac->height = (UINT32)bounds.size.height;
}

static UINT32 enum_monitors(MONITOR_DEF* monitors, UINT32 maxMonitors)
{
	if (maxMonitors < 1)
		return 0;
	const CGRect bounds = CGDisplayBounds(CGMainDisplayID());
	monitors[0].left = 0;
	monitors[0].top = 0;
	monitors[0].right = (INT32)bounds.size.width;
	monitors[0].bottom = (INT32)bounds.size.height;
	monitors[0].flags = 1;
	return 1;
}

static int subsystem_init(rdpShadowSubsystem* subsystem)
{
	MacRdpSubsystem* mac = (MacRdpSubsystem*)subsystem;
	detect_display(mac);
	subsystem->numMonitors = enum_monitors(subsystem->monitors, ARRAYSIZE(subsystem->monitors));

	if (!AXIsProcessTrusted())
		WLog_WARN(TAG, "Accessibility permission missing: remote input will be ignored");

	WLog_INFO(TAG, "display %u: %" PRIu32 "x%" PRIu32 " points", mac->displayId, mac->width,
	          mac->height);
	return 1;
}

static int subsystem_uninit(rdpShadowSubsystem* subsystem)
{
	return 1;
}

static DWORD WINAPI message_thread(LPVOID arg)
{
	MacRdpSubsystem* mac = arg;
	wMessagePipe* pipe = mac->common.MsgPipe;
	wMessage message;

	while (MessageQueue_Wait(pipe->In))
	{
		if (!MessageQueue_Peek(pipe->In, &message, TRUE))
			continue;
		if (message.id == WMQ_QUIT)
			break;

		if (message.id == SHADOW_MSG_IN_REFRESH_REQUEST_ID)
		{
			rdpShadowSurface* surface = mac->common.server->surface;
			EnterCriticalSection(&surface->lock);
			const RECTANGLE_16 all = { 0, 0, (UINT16)surface->width, (UINT16)surface->height };
			region16_union_rect(&surface->invalidRegion, &surface->invalidRegion, &all);
			LeaveCriticalSection(&surface->lock);
			/* The capture queue serializes publishing with incoming frames. */
			MacRdpCapture* capture = (__bridge MacRdpCapture*)mac->capture;
			dispatch_async(capture.queue, ^{
			  publish_frame(mac);
			});
		}

		if (message.Free)
			message.Free(&message);
	}
	return 0;
}

static int subsystem_start(rdpShadowSubsystem* subsystem)
{
	MacRdpSubsystem* mac = (MacRdpSubsystem*)subsystem;

	MacRdpCapture* capture = [[MacRdpCapture alloc] init];
	capture.subsystem = mac;
	if (![capture startWithDisplay:mac->displayId width:mac->width height:mac->height])
		return -1;
	mac->capture = (__bridge_retained void*)capture;

	mac->msgThread = CreateThread(NULL, 0, message_thread, mac, 0, NULL);
	if (!mac->msgThread)
		return -1;

	mac->cursorTimer = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0,
	                                          dispatch_get_main_queue());
	dispatch_source_set_timer(mac->cursorTimer, DISPATCH_TIME_NOW, NSEC_PER_SEC / 30,
	                          NSEC_PER_SEC / 120);
	dispatch_source_set_event_handler(mac->cursorTimer, ^{
	  cursor_poll(mac);
	});
	dispatch_resume(mac->cursorTimer);
	return 1;
}

static int subsystem_stop(rdpShadowSubsystem* subsystem)
{
	MacRdpSubsystem* mac = (MacRdpSubsystem*)subsystem;

	if (mac->cursorTimer)
	{
		dispatch_source_cancel(mac->cursorTimer);
		mac->cursorTimer = nil;
	}
	if (mac->capture)
	{
		MacRdpCapture* capture = (__bridge_transfer MacRdpCapture*)mac->capture;
		mac->capture = NULL;
		[capture stop];
	}
	if (mac->msgThread)
	{
		(void)MessageQueue_PostQuit(mac->common.MsgPipe->In, 0);
		(void)WaitForSingleObject(mac->msgThread, INFINITE);
		(void)CloseHandle(mac->msgThread);
		mac->msgThread = NULL;
	}
	return 1;
}

static void subsystem_free(rdpShadowSubsystem* subsystem)
{
	free(subsystem);
}

static rdpShadowSubsystem* subsystem_new(void)
{
	MacRdpSubsystem* mac = calloc(1, sizeof(MacRdpSubsystem));
	if (!mac)
		return NULL;

	mac->common.SynchronizeEvent = input_synchronize;
	mac->common.KeyboardEvent = input_keyboard;
	mac->common.UnicodeKeyboardEvent = input_unicode;
	mac->common.MouseEvent = input_mouse;
	mac->common.ExtendedMouseEvent = input_extended_mouse;
	return &mac->common;
}

int MacRdpSubsystemEntry(RDP_SHADOW_ENTRY_POINTS* pEntryPoints)
{
	pEntryPoints->New = subsystem_new;
	pEntryPoints->Free = subsystem_free;
	pEntryPoints->Init = subsystem_init;
	pEntryPoints->Uninit = subsystem_uninit;
	pEntryPoints->Start = subsystem_start;
	pEntryPoints->Stop = subsystem_stop;
	pEntryPoints->EnumMonitors = enum_monitors;
	return 1;
}
