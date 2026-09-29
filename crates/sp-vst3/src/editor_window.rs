//! Worker-owned `AppKit` window for a native VST3 editor, and the editor's preview picture.

#![allow(unsafe_code)] // AppKit initialization is this crate's isolated Objective-C boundary.

use std::{
    cell::Cell,
    ffi::{c_char, c_void},
    marker::PhantomData,
    ptr::{self, NonNull},
    rc::Rc,
    sync::OnceLock,
    time::Duration,
};

use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Retained, autoreleasepool},
    runtime::{AnyObject, ProtocolObject},
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBitmapImageFileType,
    NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSEventMask, NSScreen, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask, NSWindowTabbingMode,
};
use objc2_core_foundation::{CFRetained, CGFloat};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGImage,
    CGImageAlphaInfo, CGInterpolationQuality, CGWindowImageOption, CGWindowListOption,
    kCGColorSpaceSRGB,
};
use objc2_foundation::{
    NSDate, NSDefaultRunLoopMode, NSDictionary, NSObject, NSObjectProtocol, NSPoint, NSRect,
    NSSize, NSString,
};

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

impl EditorWindowDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Cell::new(false));
        // SAFETY: NSObject's init selector returns an initialized retained object.
        unsafe { msg_send![super(this), init] }
    }
}

/// Top-level editor window that may only be created and operated on the worker main thread.
pub struct MacOsEditorWindow {
    window: Retained<NSWindow>,
    delegate: Retained<EditorWindowDelegate>,
    closed: Cell<bool>,
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
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Resizable
                    | NSWindowStyleMask::Miniaturizable,
                NSBackingStoreType::Buffered,
                false,
            )
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

    /// Shows and focuses the window.
    ///
    /// # Panics
    /// Panics when called off the worker main thread, which violates the window's contract.
    pub fn focus(&self) {
        NSApplication::sharedApplication(
            MainThreadMarker::new().expect("editor window remains on main thread"),
        )
        .activate();
        self.window.makeKeyAndOrderFront(None);
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
