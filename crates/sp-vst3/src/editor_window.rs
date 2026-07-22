//! Worker-owned `AppKit` window for a native VST3 editor.

#![allow(unsafe_code)] // AppKit initialization is this crate's isolated Objective-C boundary.

use objc2::{MainThreadMarker, MainThreadOnly, rc::Retained};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEventMask, NSView,
    NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString};

/// Top-level editor window that may only be created and operated on the worker main thread.
pub struct MacOsEditorWindow {
    window: Retained<NSWindow>,
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
                    | NSWindowStyleMask::Resizable
                    | NSWindowStyleMask::Miniaturizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(title));
        Ok(Self { window })
    }

    /// Returns the stable `NSView` pointer accepted by VST3 `IPlugView::attached`.
    ///
    /// # Errors
    /// Returns an error when the window has no content view.
    pub fn content_view(&self) -> Result<usize, String> {
        self.window
            .contentView()
            .map(|view: Retained<NSView>| Retained::as_ptr(&view) as usize)
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

    /// Closes the top-level window after the plug-in view has detached.
    pub fn close(&self) {
        self.window.close();
    }
}

/// Drains pending `AppKit` events without taking ownership of the worker's outer loop.
pub fn pump_events() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let expiration = NSDate::distantPast();
    let mode = unsafe { NSDefaultRunLoopMode };
    while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
        NSEventMask::Any,
        Some(&expiration),
        mode,
        true,
    ) {
        app.sendEvent(&event);
    }
}
