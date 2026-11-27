//! Worker-owned `AppKit` window for a native VST3 editor, the editor's preview picture, and the
//! pictures and input that stream the editor to remote clients.

#![allow(unsafe_code)] // AppKit initialization is this crate's isolated Objective-C boundary.

use std::{
    cell::{Cell, RefCell},
    ffi::{c_char, c_void},
    marker::PhantomData,
    ptr::{self, NonNull},
    rc::Rc,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

use block2::RcBlock;

use objc2::{
    AllocAnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class,
    msg_send,
    rc::{Retained, autoreleasepool},
    runtime::{AnyObject, Bool, ProtocolObject},
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBitmapImageFileType,
    NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSEvent, NSEventMask, NSEventModifierFlags,
    NSEventType, NSResponder, NSScreen, NSView, NSWindow, NSWindowDelegate, NSWindowOrderingMode,
    NSWindowStyleMask, NSWindowTabbingMode,
};
use objc2_core_foundation::{CFRetained, CGFloat};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGEvent,
    CGEventField, CGEventFlags, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
    CGInterpolationQuality, CGScrollEventUnit, CGWindowImageOption, CGWindowListOption,
    kCGColorSpaceSRGB,
};
use objc2_foundation::{
    NSDate, NSDefaultRunLoopMode, NSDictionary, NSError, NSInteger, NSObject, NSObjectProtocol,
    NSPoint, NSProcessInfo, NSRect, NSSize, NSString,
};
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
};

use crate::editor_input::{EditorInput, Modifiers, MouseButton};

// Editor preview pictures are exactly 320×200 pixels.
const PREVIEW_WIDTH: u32 = 320;
const PREVIEW_HEIGHT: u32 = 200;

define_class!(
    // SAFETY: NSObject has no subclassing requirements; this class has no Drop implementation.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = Cell<bool>]
    struct EditorWindowDelegate;

    // SAFETY: NSObjectProtocol has no additional safety requirements.
    unsafe impl NSObjectProtocol for EditorWindowDelegate {}

    // SAFETY: The callback signature matches NSWindowDelegate's windowShouldClose: selector.
    unsafe impl NSWindowDelegate for EditorWindowDelegate {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            self.ivars().set(true);
            false
        }
    }
);

define_class!(
    // SAFETY: NSWindow allows subclassing; the overrides below call the same selectors on super
    // with the arguments they got, or return without doing anything. No Drop implementation.
    #[unsafe(super(NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Cell<bool>]
    struct EditorNSWindow;

    /// While the ivar is set, the window stays off the desktop: a plug-in that orders its host
    /// window in or makes it visible (DPF's `puglShow` sets `isVisible`) changes nothing, and
    /// the window says it is visible.
    impl EditorNSWindow {
        #[unsafe(method(orderWindow:relativeTo:))]
        fn order_window(&self, place: NSWindowOrderingMode, other: NSInteger) {
            if self.ivars().get() && place != NSWindowOrderingMode::Out {
                return;
            }
            // SAFETY: The same selector and arguments on NSWindow.
            unsafe { msg_send![super(self), orderWindow: place, relativeTo: other] }
        }

        #[unsafe(method(setIsVisible:))]
        fn set_is_visible(&self, visible: bool) {
            if self.ivars().get() && visible {
                return;
            }
            // SAFETY: The same selector and argument on NSWindow.
            unsafe { msg_send![super(self), setIsVisible: visible] }
        }

        /// A window kept off the desktop tells the plug-in it shows: pugl (DPF) draws only
        /// while its window is visible (`-[PuglWrapperView dispatchExpose:]`), and the window
        /// server pictures what the plug-in draws.
        #[unsafe(method(isVisible))]
        fn is_visible(&self) -> Bool {
            if self.ivars().get() {
                return Bool::YES;
            }
            // SAFETY: The same selector on NSWindow.
            unsafe { msg_send![super(self), isVisible] }
        }

        #[unsafe(method(orderFrontRegardless))]
        fn order_front_regardless(&self) {
            if self.ivars().get() {
                return;
            }
            // SAFETY: The same selector on NSWindow.
            unsafe { msg_send![super(self), orderFrontRegardless] }
        }

        #[unsafe(method(makeKeyAndOrderFront:))]
        fn make_key_and_order_front(&self, sender: Option<&AnyObject>) {
            if self.ivars().get() {
                return;
            }
            // SAFETY: The same selector and argument on NSWindow.
            unsafe { msg_send![super(self), makeKeyAndOrderFront: sender] }
        }
    }
);

impl EditorWindowDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Cell::new(false));
        // SAFETY: NSObject's init selector returns an initialized retained object.
        unsafe { msg_send![super(this), init] }
    }
}

/// Top-level editor window that may only be created and operated on the worker main thread.
pub struct MacOsEditorWindow {
    window: Retained<EditorNSWindow>,
    delegate: Retained<EditorWindowDelegate>,
    closed: Cell<bool>,
    /// The view that took the last mouse press while the window is off the desktop
    /// ([`Self::inject`]).
    pressed: RefCell<Option<Retained<NSView>>>,
    /// The window server's pictures of this window ([`Self::request_capture`]).
    capture: Arc<Mutex<WindowCapture>>,
}

/// A picture of the editor's content as BGRX pixels and its size, or why there is none.
type Picture = Result<(u16, u16, Vec<u8>), String>;

/// The most captures of one window on their way at a time. One capture takes about 20 ms
/// (1200 × 750 points, M4 Pro), so one at a time gives fewer than 30 a second.
const MAX_IN_FLIGHT: u8 = 2;

/// The window server's pictures of one editor window. `ScreenCaptureKit`'s completion handlers
/// run on its own queues: they leave each picture here for the worker main thread to take.
#[derive(Default)]
struct WindowCapture {
    /// The window's content filter, once the process's shareable content listed the window.
    filter: Option<SendFilter>,
    /// The process's shareable content is being listed for the filter.
    listing: bool,
    /// The captures on their way.
    in_flight: u8,
    /// When the last capture started.
    started: Option<Instant>,
    /// The number of the last capture that started, and of the newest answer that came.
    started_number: u64,
    answered_number: u64,
    /// The newest answer: a picture until it is taken, a failure until a newer answer comes.
    picture: Option<Picture>,
    /// The pictures and failures that came.
    count: u64,
}

/// A content filter that crosses to `ScreenCaptureKit`'s queues.
struct SendFilter(Retained<SCContentFilter>);

// SAFETY: A content filter is not changed after it is made, and ScreenCaptureKit takes filters
// on any queue: the capture calls only read it.
unsafe impl Send for SendFilter {}

/// The sizes of one capture: the whole window, title bar included, and its content, in points.
#[derive(Clone, Copy)]
struct CaptureSize {
    window: (u16, u16),
    content: (u16, u16),
}

fn lock(capture: &Mutex<WindowCapture>) -> std::sync::MutexGuard<'_, WindowCapture> {
    capture.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Ends capture `number`, which gave `picture`; an answer older than the newest one is dropped.
/// A failure forgets the filter, so the next capture lists the window again (a window that the
/// window server recreated has a new filter).
fn finish(capture: &Mutex<WindowCapture>, number: u64, picture: Picture) {
    let mut state = lock(capture);
    state.in_flight = state.in_flight.saturating_sub(1);
    if picture.is_err() {
        state.filter = None;
    }
    if number > state.answered_number {
        state.answered_number = number;
        state.picture = Some(picture);
    }
    state.count += 1;
}

/// The words of a `ScreenCaptureKit` error, or `fallback`.
fn describe(error: *mut NSError, fallback: &str) -> String {
    // SAFETY: ScreenCaptureKit passes a valid error or null.
    unsafe { error.as_ref() }.map_or_else(
        || fallback.to_owned(),
        |error| error.localizedDescription().to_string(),
    )
}

/// `kCVPixelFormatType_32BGRA`: B, G, R, A bytes, the window-stream tile layout.
const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

/// `kCVPixelBufferLock_ReadOnly`.
const LOCK_READ_ONLY: u64 = 1;

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetImageBuffer(buffer: *mut c_void) -> *mut c_void;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferLockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetWidth(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetPixelFormatType(buffer: *mut c_void) -> u32;
}

/// Asks the window server for one picture of the window of `filter`, at one pixel per point,
/// as BGRA in sRGB.
fn capture_window(
    capture: &Arc<Mutex<WindowCapture>>,
    filter: &SCContentFilter,
    size: CaptureSize,
    number: u64,
) {
    // SAFETY: A new configuration is a plain value object; the setters take plain values.
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(usize::from(size.window.0));
        config.setHeight(usize::from(size.window.1));
        config.setPixelFormat(PIXEL_FORMAT_BGRA);
        config.setColorSpaceName(kCGColorSpaceSRGB);
        config.setShowsCursor(false);
        config.setIgnoreShadowsSingleWindow(true);
    }
    let shared = Arc::clone(capture);
    let done = RcBlock::new(move |sample: *mut c_void, error: *mut NSError| {
        let picture = if sample.is_null() {
            Err(describe(error, "the window server gave no picture"))
        } else {
            // SAFETY: ScreenCaptureKit passes a valid sample buffer for the call.
            unsafe { content_bgrx(sample, size) }
        };
        finish(&shared, number, picture);
    });
    // SAFETY: `captureSampleBufferWithFilter:configuration:completionHandler:` (macOS 14) takes
    // a filter, a configuration and a block of a `CMSampleBufferRef` and an `NSError *`. They
    // outlive the call; the handler only touches the shared state.
    unsafe {
        let () = msg_send![
            SCScreenshotManager::class(),
            captureSampleBufferWithFilter: filter,
            configuration: &*config,
            completionHandler: &*done
        ];
    }
}

/// The content of a window-server picture of the whole window, as BGRX: the bottom
/// `size.content` rows, below the title bar.
///
/// # Safety
/// `sample` is a valid `CMSampleBufferRef`.
unsafe fn content_bgrx(sample: *mut c_void, size: CaptureSize) -> Picture {
    let unusable = || Err("the window server's picture has another format or size".to_owned());
    // SAFETY: The image buffer of a screenshot is a pixel buffer. Its pixels are read only while
    // its base address is locked, and only rows inside its height and bytes per row.
    unsafe {
        let image = CMSampleBufferGetImageBuffer(sample);
        if image.is_null()
            || CVPixelBufferGetPixelFormatType(image) != PIXEL_FORMAT_BGRA
            || CVPixelBufferLockBaseAddress(image, LOCK_READ_ONLY) != 0
        {
            return unusable();
        }
        let (width, height) = (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image));
        let stride = CVPixelBufferGetBytesPerRow(image);
        let base = CVPixelBufferGetBaseAddress(image).cast::<u8>();
        let (columns, rows) = (usize::from(size.content.0), usize::from(size.content.1));
        let picture = if base.is_null()
            || columns == 0
            || rows == 0
            || (width, height) != (usize::from(size.window.0), usize::from(size.window.1))
            || columns > width
            || rows > height
            || stride < width * 4
        {
            unusable()
        } else {
            let image = std::slice::from_raw_parts(base, stride * height);
            let mut pixels = Vec::with_capacity(columns * rows * 4);
            for row in image.chunks_exact(stride).skip(height - rows) {
                pixels.extend_from_slice(&row[..columns * 4]);
            }
            Ok((size.content.0, size.content.1, pixels))
        };
        CVPixelBufferUnlockBaseAddress(image, LOCK_READ_ONLY);
        picture
    }
}

/// Draws `image` at `drawn` points from the bottom-left corner into BGRX `pixels` of `size`,
/// rows top to bottom: the window-stream tile format. Whatever lies above `size` is cut off.
fn draw_bgrx(
    image: &CGImage,
    size: (u16, u16),
    drawn: (f64, f64),
    pixels: &mut Vec<u8>,
) -> Result<(u16, u16), String> {
    let (width, height) = size;
    if width == 0 || height == 0 {
        return Err("native editor window has no content".to_owned());
    }
    // SAFETY: `kCGColorSpaceSRGB` is an immutable CoreGraphics constant.
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
        .ok_or("sRGB color space is unavailable")?;
    let (columns, rows) = (usize::from(width), usize::from(height));
    pixels.clear();
    pixels.resize(columns * rows * 4, 0);
    // Little-endian 32-bit pixels with the alpha byte skipped first are B, G, R, X in memory.
    // The context's first row in memory is the top of the drawn image.
    // SAFETY: `pixels` holds `rows` rows of `columns * 4` bytes. The context writes only inside
    // them and is released at the end of this function, before `pixels` is read.
    let context = unsafe {
        CGBitmapContextCreate(
            pixels.as_mut_ptr().cast(),
            columns,
            rows,
            8,
            columns * 4,
            Some(&space),
            CGImageAlphaInfo::NoneSkipFirst.0 | CGImageByteOrderInfo::Order32Little.0,
        )
    }
    .ok_or("native editor capture context could not be made")?;
    CGContext::draw_image(
        Some(&context),
        NSRect::new(NSPoint::ZERO, NSSize::new(drawn.0, drawn.1)),
        Some(image),
    );
    drop(context);
    Ok((width, height))
}

/// A retained `AppKit` parent view. It cannot cross threads or outlive its owned `NSView`.
pub struct EditorParentView {
    view: Retained<NSView>,
    _main_thread: PhantomData<Rc<()>>,
}

impl EditorParentView {
    pub(crate) fn as_ptr(&self) -> *mut c_void {
        Retained::as_ptr(&self.view).cast_mut().cast()
    }

    /// The window's content view, where the plug-in's view goes.
    #[must_use]
    pub fn view(&self) -> &NSView {
        &self.view
    }
}

impl MacOsEditorWindow {
    /// Creates a hidden resizable editor window with a stable content view.
    ///
    /// # Errors
    /// Returns an error when called off the worker main thread.
    pub fn new(title: &str) -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("native editor requires the worker main thread")?;
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.finishLaunching();
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(640.0, 480.0));
        let this = EditorNSWindow::alloc(mtm).set_ivars(Cell::new(false));
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::Miniaturizable;
        // SAFETY: NSWindow's designated initializer with a valid frame, style and backing.
        let window: Retained<EditorNSWindow> = unsafe {
            msg_send![
                super(this),
                initWithContentRect: frame,
                styleMask: style,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        unsafe { window.setReleasedWhenClosed(false) };
        // Each editor is its own window; macOS would otherwise tab same-titled windows together.
        window.setTabbingMode(NSWindowTabbingMode::Disallowed);
        window.setTitle(&NSString::from_str(title));
        let delegate = EditorWindowDelegate::new(mtm);
        window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        Ok(Self {
            window,
            delegate,
            closed: Cell::new(false),
            pressed: RefCell::default(),
            capture: Arc::default(),
        })
    }

    /// Retains the content view accepted by VST3 `IPlugView::attached`.
    ///
    /// # Errors
    /// Returns an error when the window has no content view.
    pub fn content_view(&self) -> Result<EditorParentView, String> {
        self.window
            .contentView()
            .map(|view| EditorParentView {
                view,
                _main_thread: PhantomData,
            })
            .ok_or_else(|| "native editor window has no content view".to_owned())
    }

    /// Applies the plug-in's accepted content size.
    pub fn resize(&self, width: u32, height: u32) {
        self.window
            .setContentSize(NSSize::new(f64::from(width), f64::from(height)));
    }

    /// Shows and focuses the window, also one that was kept off the desktop.
    ///
    /// # Panics
    /// Panics when called off the worker main thread, which violates the window's contract.
    pub fn focus(&self) {
        self.window.ivars().set(false);
        let app = NSApplication::sharedApplication(
            MainThreadMarker::new().expect("editor window remains on main thread"),
        );
        // A window that was kept off the desktop left the process unable to activate.
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.activate();
        self.window.makeKeyAndOrderFront(None);
    }

    /// Keeps the window off the desktop until [`Self::focus`]: the plug-in's own calls that
    /// order its host window in or make it visible do nothing, and the window tells it that it
    /// is visible, so that it draws. The window stays ordered out, and
    /// [`Self::request_capture`] and [`Self::inject`] still work on it.
    ///
    /// The process also cannot become the active application until then. `AppKit` asks to
    /// activate an accessory application when its event loop first runs, and the window server
    /// grants it when the app that started the worker is in front: that app would lose the
    /// keyboard to a process with no window on the desktop.
    ///
    /// # Panics
    /// Panics when called off the worker main thread, which violates the window's contract.
    pub fn keep_off_desktop(&self) {
        self.window.ivars().set(true);
        NSApplication::sharedApplication(
            MainThreadMarker::new().expect("editor window remains on main thread"),
        )
        .setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    }

    /// Takes a user close request without closing the window or detaching its plug-in view.
    pub fn take_close_request(&self) -> bool {
        !self.closed.get() && self.delegate.ivars().replace(false)
    }

    /// Sends the same close action as the window's red button.
    pub fn request_close(&self) {
        if !self.closed.get() {
            self.window.performClose(None);
        }
    }

    /// Closes the top-level window after the plug-in view has detached.
    pub fn close(&self) {
        if !self.closed.replace(true) {
            self.window.setDelegate(None);
            self.window.close();
        }
    }

    /// Moves the window's top-left corner to `left` points from the primary screen's left edge
    /// and `top` points below its top edge.
    ///
    /// # Panics
    /// Panics when called off the worker main thread, which violates the window's contract.
    pub fn set_top_left(&self, left: f64, top: f64) {
        let mtm = MainThreadMarker::new().expect("editor window remains on main thread");
        // AppKit measures from the primary screen's bottom-left corner, y upwards.
        if let Some(primary) = NSScreen::screens(mtm).firstObject() {
            self.window
                .setFrameTopLeftPoint(NSPoint::new(left, primary.frame().size.height - top));
        }
    }

    /// Pictures the editor's content as a 320×200 PNG, scaled to cover and centre-cropped.
    ///
    /// Plug-ins often draw with Metal or OpenGL layers, which `NSView` caching leaves blank, so
    /// the window server's image of this window comes first. For a window of the calling process
    /// it needs no screen-recording permission and shows no prompt. The view cache is the
    /// fallback when that call is unavailable.
    ///
    /// # Errors
    /// Returns an error when neither source yields a picture or PNG encoding fails.
    pub fn capture_preview_png(&self) -> Result<Vec<u8>, String> {
        autoreleasepool(|_| {
            let picture = self
                .window_picture()
                .or_else(|| self.view_picture())
                .ok_or("native editor window could not be pictured")?;
            let bitmap = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &picture);
            let properties = NSDictionary::<NSBitmapImageRepPropertyKey, AnyObject>::new();
            // SAFETY: the empty dictionary has the property dictionary's key and value types.
            let png = unsafe {
                bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &properties)
            }
            .ok_or("native editor picture could not be encoded as PNG")?;
            Ok(png.to_vec())
        })
    }

    /// The window server's number for this window. It is unique across processes, so it names
    /// the window in the editor stream. `None` before the window has a window-server device.
    pub fn window_number(&self) -> Option<u32> {
        u32::try_from(self.window.windowNumber())
            .ok()
            .filter(|&number| number > 0)
    }

    /// Pictures the editor's content at one pixel per point into `pixels` as BGRX, rows top to
    /// bottom: the window-stream tile format. Returns the width and height in pixels.
    ///
    /// The view draws itself into a bitmap inside this process, so this needs no permission and
    /// changes nothing on screen. Layer-backed Metal or OpenGL content stays blank here.
    ///
    /// # Errors
    /// Returns an error when the window has no content, or a bitmap cannot be made.
    pub fn capture_bgrx(&self, pixels: &mut Vec<u8>) -> Result<(u16, u16), String> {
        autoreleasepool(|_| {
            let view = self
                .window
                .contentView()
                .ok_or("native editor window has no content view")?;
            let bounds = view.bounds();
            let (width, height) = (
                whole_pixels(bounds.size.width),
                whole_pixels(bounds.size.height),
            );
            if width == 0 || height == 0 {
                return Err("native editor window has no content".to_owned());
            }
            let bitmap = view
                .bitmapImageRepForCachingDisplayInRect(bounds)
                .ok_or("native editor view has no cache bitmap")?;
            view.cacheDisplayInRect_toBitmapImageRep(bounds, &bitmap);
            let image = bitmap
                .CGImage()
                .ok_or("native editor cache bitmap has no image")?;
            draw_bgrx(
                &image,
                (width, height),
                (f64::from(width), f64::from(height)),
                pixels,
            )
        })
    }

    /// Starts a picture of this window from the window server, unless [`MAX_IN_FLIGHT`] are on
    /// their way or the last one started less than `interval` ago. It has what the
    /// server composes for the window, so a view that draws with OpenGL or Metal is in it too,
    /// also while the window is ordered out. [`Self::take_capture`] gives it once it came.
    ///
    /// A capture stream (`SCStream`) gives no pictures of a window that is ordered out: its
    /// frames say "suspended" and have no image. So each picture is one screenshot.
    ///
    /// The worker pictures its own window. `ScreenCaptureKit` lists that window in the current
    /// process's shareable content, which (macOS 14.4) holds what "is available to capture by
    /// current process without user consent via TCC": no Screen Recording permission, no prompt.
    pub fn request_capture(&self, interval: Duration) {
        let Some(number) = self.window_number() else {
            return;
        };
        let Some(view) = self.window.contentView() else {
            return;
        };
        let (frame, bounds) = (self.window.frame(), view.bounds());
        let size = CaptureSize {
            window: (
                whole_pixels(frame.size.width),
                whole_pixels(frame.size.height),
            ),
            content: (
                whole_pixels(bounds.size.width),
                whole_pixels(bounds.size.height),
            ),
        };
        let now = Instant::now();
        let mut state = lock(&self.capture);
        if state.listing
            || state.in_flight >= MAX_IN_FLIGHT
            || state
                .started
                .is_some_and(|started| now < started + interval)
        {
            return;
        }
        state.in_flight += 1;
        state.started = Some(now);
        state.started_number += 1;
        let sequence = state.started_number;
        if let Some(filter) = state.filter.as_ref().map(|filter| filter.0.clone()) {
            drop(state);
            capture_window(&self.capture, &filter, size, sequence);
            return;
        }
        state.listing = true;
        drop(state);
        let shared = Arc::clone(&self.capture);
        let listed = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                // SAFETY: ScreenCaptureKit passes valid content or null; its windows are SCWindows.
                let window = unsafe { content.as_ref() }.and_then(|content| {
                    unsafe { content.windows() }
                        .iter()
                        .find(|window| unsafe { window.windowID() } == number)
                });
                lock(&shared).listing = false;
                let Some(window) = window else {
                    finish(
                        &shared,
                        sequence,
                        Err(describe(
                            error,
                            "the window server does not list this window",
                        )),
                    );
                    return;
                };
                // SAFETY: A desktop-independent window filter takes any listed window.
                let filter = unsafe {
                    SCContentFilter::initWithDesktopIndependentWindow(
                        SCContentFilter::alloc(),
                        &window,
                    )
                };
                lock(&shared).filter = Some(SendFilter(filter.clone()));
                capture_window(&shared, &filter, size, sequence);
            },
        );
        // SAFETY: The handler outlives the call and only touches the shared state.
        unsafe {
            SCShareableContent::getCurrentProcessShareableContentWithCompletionHandler(&listed);
        }
    }

    /// The newest answer to [`Self::request_capture`]: BGRX pixels, rows top to bottom, and
    /// their width and height in pixels, once; or why the window server gave none, until a newer
    /// answer comes.
    pub fn take_capture(&self) -> Option<Picture> {
        let mut state = lock(&self.capture);
        match &state.picture {
            Some(Err(error)) => Some(Err(error.clone())),
            _ => state.picture.take(),
        }
    }

    /// How many answers to [`Self::request_capture`] came: pictures and failures.
    #[must_use]
    pub fn captures(&self) -> u64 {
        lock(&self.capture).count
    }

    /// Delivers synthesized input to this window as if the user gave it. The events go through
    /// `-[NSWindow sendEvent:]`, or straight to the views while the window is kept off the
    /// desktop, so only this window sees them: the system pointer does not move and no
    /// Accessibility permission is needed.
    ///
    /// # Panics
    /// Panics when called off the worker main thread, which violates the window's contract.
    pub fn inject(&self, input: &EditorInput) {
        let mtm = MainThreadMarker::new().expect("editor window remains on main thread");
        autoreleasepool(|_| {
            let event = match *input {
                EditorInput::MouseMoved {
                    x,
                    y,
                    dragging,
                    modifiers,
                } => {
                    let kind = match dragging {
                        None => NSEventType::MouseMoved,
                        Some(MouseButton::Left) => NSEventType::LeftMouseDragged,
                        Some(MouseButton::Right) => NSEventType::RightMouseDragged,
                        Some(MouseButton::Middle) => NSEventType::OtherMouseDragged,
                    };
                    self.mouse_event(kind, (x, y), modifiers, 0, dragging.is_some())
                }
                EditorInput::MouseButton {
                    x,
                    y,
                    button,
                    pressed,
                    click_count,
                    modifiers,
                } => {
                    let kind = match (button, pressed) {
                        (MouseButton::Left, true) => NSEventType::LeftMouseDown,
                        (MouseButton::Left, false) => NSEventType::LeftMouseUp,
                        (MouseButton::Right, true) => NSEventType::RightMouseDown,
                        (MouseButton::Right, false) => NSEventType::RightMouseUp,
                        (MouseButton::Middle, true) => NSEventType::OtherMouseDown,
                        (MouseButton::Middle, false) => NSEventType::OtherMouseUp,
                    };
                    let clicks = isize::try_from(click_count).unwrap_or(isize::MAX);
                    self.mouse_event(kind, (x, y), modifiers, clicks, pressed)
                }
                EditorInput::Scroll {
                    x,
                    y,
                    dx,
                    dy,
                    modifiers,
                } => self.scroll_event(mtm, (x, y), dx, dy, modifiers),
                EditorInput::Key {
                    key_code,
                    ref characters,
                    pressed,
                    modifiers,
                } => {
                    let kind = if pressed {
                        NSEventType::KeyDown
                    } else {
                        NSEventType::KeyUp
                    };
                    self.key_event(kind, key_code, characters, modifiers)
                }
                EditorInput::ModifiersChanged {
                    key_code,
                    modifiers,
                } => self.key_event(NSEventType::FlagsChanged, key_code, "", modifiers),
                EditorInput::Focus => {
                    self.focus();
                    None
                }
                EditorInput::Close => {
                    self.request_close();
                    None
                }
            };
            if let Some(event) = event {
                if self.window.ivars().get() {
                    self.deliver_off_desktop(mtm, &event);
                } else {
                    self.window.sendEvent(&event);
                }
            }
        });
    }

    /// `-[NSWindow sendEvent:]` drops mouse events for a window that is not ordered in, so a
    /// window kept off the desktop gets them as `AppKit` would route them: a press goes to the
    /// view under it, which also takes the drags and the release; moves and wheels go to the
    /// view under the pointer; keys go to the first responder. A control that runs its own
    /// tracking loop (`NSTextView`, `NSSlider`) takes a press as a click and does not follow
    /// drags; plug-in views draw their own controls and follow them.
    fn deliver_off_desktop(&self, mtm: MainThreadMarker, event: &NSEvent) {
        let hit = || {
            let content = self.window.contentView()?;
            // The content view's superview, the frame view, uses window coordinates.
            content.hitTest(event.locationInWindow())
        };
        match event.r#type() {
            NSEventType::LeftMouseDown
            | NSEventType::RightMouseDown
            | NSEventType::OtherMouseDown => {
                let Some(view) = hit() else { return };
                if view.acceptsFirstResponder() {
                    self.window.makeFirstResponder(Some(&view));
                }
                let (release, press): (_, fn(&NSView, &NSEvent)) = match event.r#type() {
                    NSEventType::LeftMouseDown => (NSEventType::LeftMouseUp, |view, event| {
                        view.mouseDown(event);
                    }),
                    NSEventType::RightMouseDown => (NSEventType::RightMouseUp, |view, event| {
                        view.rightMouseDown(event);
                    }),
                    _ => (NSEventType::OtherMouseUp, |view, event| {
                        view.otherMouseDown(event);
                    }),
                };
                // A control with a tracking loop (`NSTextView`, `NSSlider`) waits in its
                // press for a release from the event queue and would hold the main thread
                // until one came. This release ends such a loop at once; a view without one
                // never sees it, as the window drops queued mouse events.
                if let Some(up) = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                    release,
                    event.locationInWindow(),
                    event.modifierFlags(),
                    event.timestamp(),
                    event.windowNumber(),
                    None,
                    0,
                    event.clickCount(),
                    0.0,
                ) {
                    NSApplication::sharedApplication(mtm).postEvent_atStart(&up, false);
                }
                press(&view, event);
                self.pressed.replace(Some(view));
            }
            NSEventType::LeftMouseDragged
            | NSEventType::RightMouseDragged
            | NSEventType::OtherMouseDragged => {
                let Some(view) = self.pressed.borrow().clone().or_else(hit) else {
                    return;
                };
                match event.r#type() {
                    NSEventType::LeftMouseDragged => view.mouseDragged(event),
                    NSEventType::RightMouseDragged => view.rightMouseDragged(event),
                    _ => view.otherMouseDragged(event),
                }
            }
            NSEventType::LeftMouseUp | NSEventType::RightMouseUp | NSEventType::OtherMouseUp => {
                let Some(view) = self.pressed.take().or_else(hit) else {
                    return;
                };
                match event.r#type() {
                    NSEventType::LeftMouseUp => view.mouseUp(event),
                    NSEventType::RightMouseUp => view.rightMouseUp(event),
                    _ => view.otherMouseUp(event),
                }
            }
            NSEventType::MouseMoved => {
                if let Some(view) = hit() {
                    view.mouseMoved(event);
                }
            }
            NSEventType::ScrollWheel => {
                if let Some(view) = hit() {
                    view.scrollWheel(event);
                }
            }
            NSEventType::KeyDown | NSEventType::KeyUp | NSEventType::FlagsChanged => {
                let Some(responder) = self.window.firstResponder() else {
                    return;
                };
                match event.r#type() {
                    NSEventType::KeyDown => responder.keyDown(event),
                    NSEventType::KeyUp => responder.keyUp(event),
                    _ => responder.flagsChanged(event),
                }
            }
            _ => {}
        }
    }

    /// A content-view position, in points from its top-left corner, in window coordinates, which
    /// grow upwards.
    fn window_location(&self, (x, y): (f64, f64)) -> Option<NSPoint> {
        let view = self.window.contentView()?;
        let y = if view.isFlipped() {
            y
        } else {
            view.bounds().size.height - y
        };
        Some(view.convertPoint_toView(NSPoint::new(x, y), None))
    }

    fn mouse_event(
        &self,
        kind: NSEventType,
        position: (f64, f64),
        modifiers: Modifiers,
        clicks: isize,
        pressed: bool,
    ) -> Option<Retained<NSEvent>> {
        NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            kind,
            self.window_location(position)?,
            modifier_flags(modifiers),
            NSProcessInfo::processInfo().systemUptime(),
            self.window.windowNumber(),
            None,
            0,
            clicks,
            if pressed { 1.0 } else { 0.0 },
        )
    }

    fn key_event(
        &self,
        kind: NSEventType,
        key_code: u16,
        characters: &str,
        modifiers: Modifiers,
    ) -> Option<Retained<NSEvent>> {
        let characters = NSString::from_str(characters);
        NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            kind,
            NSPoint::ZERO,
            modifier_flags(modifiers),
            NSProcessInfo::processInfo().systemUptime(),
            self.window.windowNumber(),
            None,
            &characters,
            &characters,
            false,
            key_code,
        )
    }

    /// `AppKit` has no scroll-wheel event constructor, so this builds a `CGEvent` at the
    /// screen position of `position` and tags it with this window, which makes `AppKit` give it
    /// window coordinates.
    fn scroll_event(
        &self,
        mtm: MainThreadMarker,
        position: (f64, f64),
        dx: i32,
        dy: i32,
        modifiers: Modifiers,
    ) -> Option<Retained<NSEvent>> {
        // CoreGraphics wheels are positive upwards and leftwards; the stream's are downwards and
        // rightwards. Wheel 1 is vertical.
        let event = CGEvent::new_scroll_wheel_event2(
            None,
            CGScrollEventUnit::Line,
            2,
            dy.saturating_neg(),
            dx.saturating_neg(),
            0,
        )?;
        let screen = self
            .window
            .convertPointToScreen(self.window_location(position)?);
        // CoreGraphics measures from the primary screen's top-left corner, y downwards.
        let primary = NSScreen::screens(mtm).firstObject()?.frame();
        CGEvent::set_location(
            Some(&event),
            NSPoint::new(screen.x, primary.size.height - screen.y),
        );
        // Explicit flags stop AppKit from reading the hardware keyboard state.
        CGEvent::set_flags(
            Some(&event),
            CGEventFlags::from_bits_retain(modifier_flags(modifiers).bits() as u64),
        );
        let number = i64::try_from(self.window.windowNumber()).ok()?;
        for field in [
            CGEventField::MouseEventWindowUnderMousePointer,
            CGEventField::MouseEventWindowUnderMousePointerThatCanHandleThisEvent,
        ] {
            CGEvent::set_integer_value_field(Some(&event), field, number);
        }
        NSEvent::eventWithCGEvent(&event)
    }

    /// Scales the window server's image of this window. The image includes the title bar; only
    /// the content area covers the picture.
    fn window_picture(&self) -> Option<CFRetained<CGImage>> {
        let create = window_list_create_image()?;
        let window = u32::try_from(self.window.windowNumber()).ok()?;
        let everything = NSRect::new(
            NSPoint::new(CGFloat::INFINITY, CGFloat::INFINITY),
            NSSize::ZERO,
        );
        // SAFETY: `create` has `CGWindowListCreateImage`'s signature. It returns an owned image
        // or null.
        let image = unsafe {
            create(
                everything,
                CGWindowListOption::OptionIncludingWindow,
                window,
                CGWindowImageOption::BoundsIgnoreFraming | CGWindowImageOption::NominalResolution,
            )
        };
        // SAFETY: a non-null result is a +1 reference that this `CFRetained` now owns.
        let image = unsafe { CFRetained::from_raw(NonNull::new(image)?) };
        let frame = self.window.frame();
        let content = self.window.contentRectForFrameRect(frame);
        let pixels = image_size(&image)?;
        let (x_scale, y_scale) = (
            pixels.width / frame.size.width,
            pixels.height / frame.size.height,
        );
        cover(
            &image,
            NSRect::new(
                NSPoint::new(
                    (content.origin.x - frame.origin.x) * x_scale,
                    (content.origin.y - frame.origin.y) * y_scale,
                ),
                NSSize::new(content.size.width * x_scale, content.size.height * y_scale),
            ),
        )
    }

    /// Scales the content view drawn into a bitmap. Layer-backed Metal or OpenGL content stays
    /// blank here.
    fn view_picture(&self) -> Option<CFRetained<CGImage>> {
        let view = self.window.contentView()?;
        let bounds = view.bounds();
        let bitmap = view.bitmapImageRepForCachingDisplayInRect(bounds)?;
        view.cacheDisplayInRect_toBitmapImageRep(bounds, &bitmap);
        let image = bitmap.CGImage()?;
        cover(&image, NSRect::new(NSPoint::ZERO, image_size(&image)?))
    }
}

/// Whole pixels for a length in points, at one pixel per point.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "rounded and clamped to the u16 range first"
)]
fn whole_pixels(points: CGFloat) -> u16 {
    points.round().clamp(0.0, f64::from(u16::MAX)) as u16
}

fn modifier_flags(modifiers: Modifiers) -> NSEventModifierFlags {
    let mut flags = NSEventModifierFlags::empty();
    flags.set(NSEventModifierFlags::Shift, modifiers.shift);
    flags.set(NSEventModifierFlags::Control, modifiers.control);
    flags.set(NSEventModifierFlags::Option, modifiers.option);
    flags.set(NSEventModifierFlags::Command, modifiers.command);
    flags
}

/// `CGWindowListCreateImage`. It is looked up at run time because current SDK headers mark it
/// unavailable and a later macOS may remove it.
type CreateWindowImage =
    unsafe extern "C" fn(NSRect, CGWindowListOption, u32, CGWindowImageOption) -> *mut CGImage;

fn window_list_create_image() -> Option<CreateWindowImage> {
    static FUNCTION: OnceLock<Option<CreateWindowImage>> = OnceLock::new();
    *FUNCTION.get_or_init(|| {
        // `RTLD_DEFAULT` searches every loaded image, including CoreGraphics, which AppKit links.
        let default = ptr::without_provenance_mut::<c_void>(usize::MAX - 1);
        // SAFETY: the name is NUL-terminated and `RTLD_DEFAULT` is a valid handle.
        let symbol = unsafe { dlsym(default, c"CGWindowListCreateImage".as_ptr()) };
        // SAFETY: the exported symbol has the C signature `CreateWindowImage` declares.
        (!symbol.is_null())
            .then(|| unsafe { std::mem::transmute::<*mut c_void, CreateWindowImage>(symbol) })
    })
}

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

fn image_size(image: &CGImage) -> Option<NSSize> {
    Some(NSSize::new(
        f64::from(u32::try_from(CGImage::width(Some(image))).ok()?),
        f64::from(u32::try_from(CGImage::height(Some(image))).ok()?),
    ))
}

/// Draws `content`, a rectangle of `image` in pixels from its bottom-left corner, so it covers
/// the preview picture, centred.
fn cover(image: &CGImage, content: NSRect) -> Option<CFRetained<CGImage>> {
    if content.size.width < 1.0 || content.size.height < 1.0 {
        return None;
    }
    let target = NSSize::new(f64::from(PREVIEW_WIDTH), f64::from(PREVIEW_HEIGHT));
    let scale = (target.width / content.size.width).max(target.height / content.size.height);
    let source = image_size(image)?;
    // SAFETY: `kCGColorSpaceSRGB` is an immutable CoreGraphics constant.
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    // SAFETY: a null data pointer makes CoreGraphics allocate and own the pixel buffer. The
    // zeroed buffer is opaque black under any transparent source pixels.
    let canvas = unsafe {
        CGBitmapContextCreate(
            ptr::null_mut(),
            PREVIEW_WIDTH as usize,
            PREVIEW_HEIGHT as usize,
            8,
            0,
            Some(&space),
            CGImageAlphaInfo::NoneSkipLast.0,
        )
    }?;
    CGContext::set_interpolation_quality(Some(&canvas), CGInterpolationQuality::High);
    CGContext::draw_image(
        Some(&canvas),
        NSRect::new(
            NSPoint::new(
                (target.width - content.size.width * scale) / 2.0 - content.origin.x * scale,
                (target.height - content.size.height * scale) / 2.0 - content.origin.y * scale,
            ),
            NSSize::new(source.width * scale, source.height * scale),
        ),
        Some(image),
    );
    CGBitmapContextCreateImage(Some(&canvas))
}

impl Drop for MacOsEditorWindow {
    fn drop(&mut self) {
        // NSWindow does not retain its delegate. Clear the pointer before releasing ours.
        self.window.setDelegate(None);
    }
}

/// Runs the main `AppKit` event loop for a bounded interval, then drains a bounded event batch.
/// Timers and run-loop sources need a future deadline to run while no input event is pending.
pub fn pump_events(wait: Duration) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    autoreleasepool(|_| {
        let app = NSApplication::sharedApplication(mtm);
        let mode = unsafe { NSDefaultRunLoopMode };
        let deadline = NSDate::dateWithTimeIntervalSinceNow(wait.as_secs_f64());
        if let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&deadline),
            mode,
            true,
        ) {
            app.sendEvent(&event);
            let expired = NSDate::distantPast();
            for _ in 1..32 {
                let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&expired),
                    mode,
                    true,
                ) else {
                    break;
                };
                app.sendEvent(&event);
            }
        }
        // A manual event pump must also perform NSApplication's window-update pass.
        app.updateWindows();
    });
}
