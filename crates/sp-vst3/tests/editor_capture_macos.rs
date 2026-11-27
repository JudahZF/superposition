//! The window server's picture of an editor window that stays off the desktop
//! (`MacOsEditorWindow::request_capture`), for views that draw with Core Graphics, OpenGL and
//! Metal: each is pictured in its own colours while no window of this process is on screen.
//! It also prints the time of one capture of an 800 × 600 window.
//!
//! `AppKit` needs the main thread, so this test has its own `main`. It needs a Mac with a
//! display and runs only with `--ignored`; it shows no window and asks for no permission:
//! `cargo test -p sp-vst3 --features editor-window --test editor_capture_macos -- --ignored`.

fn main() {
    let ignored = std::env::args().any(|arg| arg == "--ignored" || arg == "--include-ignored");
    if ignored {
        #[cfg(target_os = "macos")]
        mac::run();
    } else {
        println!("editor_capture_macos: ignored; run it with `-- --ignored` on a Mac");
    }
}

#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "self-drawn test views: AppKit, OpenGL and Metal calls"
)]
#[allow(deprecated, reason = "an OpenGL view is what the test pictures")]
mod mac {
    use std::ptr::NonNull;
    use std::time::{Duration, Instant};

    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{AllocAnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSGraphicsContext, NSOpenGLPixelFormat,
        NSOpenGLView, NSResponder, NSView, NSWindow, NSWindowNumberListOptions,
    };
    use objc2_core_foundation::CGSize;
    use objc2_core_graphics::CGContext;
    use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize};
    use objc2_metal::{
        MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
        MTLCreateSystemDefaultDevice, MTLDevice, MTLLoadAction, MTLPixelFormat,
        MTLRenderPassDescriptor, MTLStoreAction,
    };
    use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
    use sp_vst3::editor_window::{MacOsEditorWindow, pump_events};

    /// The editor's size in points.
    const SIZE: (u16, u16) = (800, 600);

    #[link(name = "OpenGL", kind = "framework")]
    unsafe extern "C" {
        fn glClearColor(red: f32, green: f32, blue: f32, alpha: f32);
        fn glClear(mask: u32);
    }
    const GL_COLOR_BUFFER_BIT: u32 = 0x4000;

    define_class!(
        /// Core Graphics: the left half red, the right half green.
        #[unsafe(super(NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        struct HalvesView;

        impl HalvesView {
            #[unsafe(method(drawRect:))]
            fn draw_rect(&self, _dirty: NSRect) {
                let Some(context) = NSGraphicsContext::currentContext() else {
                    return;
                };
                let cg = context.CGContext();
                let bounds = self.bounds();
                let half = NSSize::new(bounds.size.width / 2.0, bounds.size.height);
                CGContext::set_rgb_fill_color(Some(&cg), 1.0, 0.0, 0.0, 1.0);
                CGContext::fill_rect(Some(&cg), NSRect::new(NSPoint::ZERO, half));
                CGContext::set_rgb_fill_color(Some(&cg), 0.0, 1.0, 0.0, 1.0);
                CGContext::fill_rect(Some(&cg), NSRect::new(NSPoint::new(half.width, 0.0), half));
            }
        }
    );

    define_class!(
        /// OpenGL: clears to blue.
        #[unsafe(super(NSOpenGLView, NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        struct BlueView;

        impl BlueView {
            #[unsafe(method(drawRect:))]
            fn draw_rect(&self, _dirty: NSRect) {
                let Some(context) = self.openGLContext() else {
                    return;
                };
                context.makeCurrentContext();
                // SAFETY: The view's context is current on this thread.
                unsafe {
                    glClearColor(0.0, 0.0, 1.0, 1.0);
                    glClear(GL_COLOR_BUFFER_BIT);
                }
                context.flushBuffer();
            }
        }
    );

    fn frame() -> NSRect {
        NSRect::new(
            NSPoint::ZERO,
            NSSize::new(f64::from(SIZE.0), f64::from(SIZE.1)),
        )
    }

    /// A window kept off the desktop with `view` filling its content.
    fn window_with(view: &NSView) -> MacOsEditorWindow {
        let window = MacOsEditorWindow::new("capture test").expect("the main thread");
        window.keep_off_desktop();
        window.resize(u32::from(SIZE.0), u32::from(SIZE.1));
        let content = window.content_view().expect("a content view");
        content.view().addSubview(view);
        window
    }

    /// The next window-server picture of `window`, BGRX, and how long it took.
    fn picture(window: &MacOsEditorWindow) -> (Vec<u8>, Duration) {
        let before = window.captures();
        let started = Instant::now();
        window.request_capture();
        while window.captures() == before {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "no picture in 5 s"
            );
            pump_events(Duration::from_millis(1));
        }
        let took = started.elapsed();
        let mut pixels = Vec::new();
        let size = window
            .latest_capture(&mut pixels)
            .expect("an answer came")
            .expect("the window server pictured the window");
        assert_eq!(size, SIZE, "one pixel per point, the content only");
        (pixels, took)
    }

    /// The colour at `(x, y)` as RGB.
    fn at(pixels: &[u8], x: u16, y: u16) -> [u8; 3] {
        let index = (usize::from(y) * usize::from(SIZE.0) + usize::from(x)) * 4;
        [pixels[index + 2], pixels[index + 1], pixels[index]]
    }

    /// True if `rgb` is near `want`; the window server may convert the colour space.
    fn near(rgb: [u8; 3], want: [u8; 3]) -> bool {
        rgb.iter()
            .zip(want)
            .all(|(got, want)| got.abs_diff(want) <= 64)
    }

    /// No window of this process is on the desktop, and the process cannot take the keyboard
    /// from the application in front.
    fn assert_hidden(mtm: MainThreadMarker) {
        let shown = NSWindow::windowNumbersWithOptions(NSWindowNumberListOptions::empty(), mtm);
        assert!(
            shown.is_none_or(|numbers| numbers.is_empty()),
            "a window of this process is on screen"
        );
        let app = NSApplication::sharedApplication(mtm);
        assert_eq!(
            app.activationPolicy(),
            NSApplicationActivationPolicy::Prohibited,
            "this process can become the active application"
        );
        assert!(!app.isActive(), "this process is the active application");
    }

    /// Pictures the window of `view` once it drew: `check` holds for the picture.
    fn pictured(
        mtm: MainThreadMarker,
        name: &str,
        view: &NSView,
        check: impl Fn(&[u8]) -> bool,
    ) -> Duration {
        let window = window_with(view);
        let started = Instant::now();
        loop {
            pump_events(Duration::from_millis(20));
            let (pixels, took) = picture(&window);
            assert_hidden(mtm);
            if check(&pixels) {
                println!(
                    "{name}: pictured in its colours ({:?} from the open), one capture {took:?}",
                    started.elapsed()
                );
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{name}: not its colours: {:?} {:?}",
                at(&pixels, 200, 300),
                at(&pixels, 600, 300)
            );
        }
        let mut times: Vec<Duration> = (0..10).map(|_| picture(&window).1).collect();
        times.sort();
        println!(
            "{name}: 10 more captures of {} x {}: median {:?}, max {:?}",
            SIZE.0, SIZE.1, times[5], times[9]
        );
        window.close();
        pump_events(Duration::from_millis(20));
        times[5]
    }

    pub fn run() {
        let mtm = MainThreadMarker::new().expect("the test's main runs on the main thread");

        let halves: Retained<HalvesView> = unsafe {
            msg_send![super(HalvesView::alloc(mtm).set_ivars(())), initWithFrame: frame()]
        };
        pictured(mtm, "core graphics", &halves, |pixels| {
            near(at(pixels, 200, 300), [255, 0, 0]) && near(at(pixels, 600, 300), [0, 255, 0])
        });

        // NSOpenGLPFADoubleBuffer, NSOpenGLPFAColorSize 24, the end.
        let mut attributes = [5_u32, 8, 24, 0];
        let format = unsafe {
            NSOpenGLPixelFormat::initWithAttributes(
                NSOpenGLPixelFormat::alloc(),
                NonNull::new(attributes.as_mut_ptr()).expect("an array"),
            )
        }
        .expect("an OpenGL pixel format");
        let blue: Retained<BlueView> = unsafe {
            msg_send![
                super(BlueView::alloc(mtm).set_ivars(())),
                initWithFrame: frame(),
                pixelFormat: &*format
            ]
        };
        pictured(mtm, "opengl", &blue, |pixels| {
            near(at(pixels, 200, 300), [0, 0, 255]) && near(at(pixels, 600, 300), [0, 0, 255])
        });

        let device = MTLCreateSystemDefaultDevice().expect("a Metal device");
        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        layer.setDrawableSize(CGSize::new(
            f64::from(SIZE.0) * 2.0,
            f64::from(SIZE.1) * 2.0,
        ));
        let magenta = NSView::initWithFrame(NSView::alloc(mtm), frame());
        magenta.setLayer(Some(&layer));
        magenta.setWantsLayer(true);
        let drawable = layer.nextDrawable().expect("a drawable");
        let pass = MTLRenderPassDescriptor::new();
        let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setTexture(Some(&drawable.texture()));
        attachment.setLoadAction(MTLLoadAction::Clear);
        attachment.setClearColor(MTLClearColor {
            red: 1.0,
            green: 0.0,
            blue: 1.0,
            alpha: 1.0,
        });
        attachment.setStoreAction(MTLStoreAction::Store);
        let queue = device.newCommandQueue().expect("a command queue");
        let buffer = queue.commandBuffer().expect("a command buffer");
        let encoder = buffer
            .renderCommandEncoderWithDescriptor(&pass)
            .expect("an encoder");
        encoder.endEncoding();
        buffer.presentDrawable(ProtocolObject::from_ref(&*drawable));
        buffer.commit();
        pictured(mtm, "metal", &magenta, |pixels| {
            near(at(pixels, 200, 300), [255, 0, 255]) && near(at(pixels, 600, 300), [255, 0, 255])
        });
        println!("editor_capture_macos: 3 passed");
    }
}
