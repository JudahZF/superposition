//! Worker-owned `AppKit` window for a native VST3 editor, the editor's preview picture, and the
//! pictures and input that stream the editor to remote clients.

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
    NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSEvent, NSEventMask, NSEventModifierFlags,
    NSEventType, NSScreen, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
    NSWindowTabbingMode,
};
use objc2_core_foundation::{CFRetained, CGFloat};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGEvent,
    CGEventField, CGEventFlags, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
    CGInterpolationQuality, CGScrollEventUnit, CGWindowImageOption, CGWindowListOption,
    kCGColorSpaceSRGB,
};
use objc2_foundation::{
    NSDate, NSDefaultRunLoopMode, NSDictionary, NSObject, NSObjectProtocol, NSPoint, NSProcessInfo,
    NSRect, NSSize, NSString,
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
            // SAFETY: `kCGColorSpaceSRGB` is an immutable CoreGraphics constant.
            let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
                .ok_or("sRGB color space is unavailable")?;
            let (columns, rows) = (usize::from(width), usize::from(height));
            pixels.clear();
            pixels.resize(columns * rows * 4, 0);
            // Little-endian 32-bit pixels with the alpha byte skipped first are B, G, R, X in
            // memory. The context's first row in memory is the top of the drawn image.
            // SAFETY: `pixels` holds `rows` rows of `columns * 4` bytes. The context writes only
            // inside them and is released at the end of this block, before `pixels` is read.
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
                NSRect::new(
                    NSPoint::ZERO,
                    NSSize::new(f64::from(width), f64::from(height)),
                ),
                Some(&image),
            );
            drop(context);
            Ok((width, height))
        })
    }

    /// Delivers synthesized input to this window as if the user gave it. The events go through
    /// `-[NSWindow sendEvent:]`, so only this window sees them: the system pointer does not move
    /// and no Accessibility permission is needed.
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
                self.window.sendEvent(&event);
            }
        });
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
