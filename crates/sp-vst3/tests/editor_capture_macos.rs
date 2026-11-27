//! The window server's pictures of an editor window that stays off the desktop
//! (`MacOsEditorWindow::request_capture`), for views that draw with Core Graphics, OpenGL and
//! Metal: each is pictured in its own colours while no window of this process is on screen. A
//! view of 1200 × 750 points (about the size of the editor of `FabFilter Pro-Q 4`) that draws 60
//! times a second is pictured at least 30 times a second, each picture soon after the view drew
//! it. It prints the rate, the delay and the process's CPU time.
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
    use objc2::{
        AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    };
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

    /// The animated view's size in points: about that of the editor of `FabFilter Pro-Q 4`.
    const ANIMATED: (u16, u16) = (1200, 750);

    define_class!(
        /// Core Graphics: a new picture each time it draws. The whole view changes colour with
        /// its frame number, and 8 blocks at the top left hold that number's bits (white is 1).
        #[unsafe(super(NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = std::cell::Cell<u8>]
        struct AnimatedView;

        impl AnimatedView {
            #[unsafe(method(drawRect:))]
            fn draw_rect(&self, _dirty: NSRect) {
                let Some(context) = NSGraphicsContext::currentContext() else {
                    return;
                };
                let cg = context.CGContext();
                let frame = self.ivars().get();
                let bounds = self.bounds();
                let shade = f64::from(frame) / 255.0;
                CGContext::set_rgb_fill_color(Some(&cg), shade, 0.3, 1.0 - shade, 1.0);
                CGContext::fill_rect(Some(&cg), bounds);
                for bit in 0..8 {
                    let level = f64::from(frame >> bit & 1);
                    CGContext::set_rgb_fill_color(Some(&cg), level, level, level, 1.0);
                    // The view is not flipped: its top is at its height.
                    CGContext::fill_rect(
                        Some(&cg),
                        NSRect::new(
                            NSPoint::new(f64::from(bit) * 16.0, bounds.size.height - 16.0),
                            NSSize::new(16.0, 16.0),
                        ),
                    );
                }
            }
        }
    );

    /// The frame number in the bit blocks of an animated view's picture.
    fn frame_number(pixels: &[u8]) -> u8 {
        (0..8).fold(0, |number, bit| {
            let index = (8 * usize::from(ANIMATED.0) + bit * 16 + 8) * 4;
            let on = pixels[index..index + 3].iter().all(|&value| value > 128);
            number | (u8::from(on) << bit)
        })
    }

    /// `struct timeval` and `struct rusage` of macOS.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct TimeValue {
        seconds: i64,
        micros: i32,
    }

    #[repr(C)]
    struct ResourceUsage {
        user: TimeValue,
        system: TimeValue,
        rest: [i64; 14],
    }

    unsafe extern "C" {
        fn getrusage(who: i32, usage: *mut ResourceUsage) -> i32;
    }

    /// The CPU time of this process so far.
    fn cpu_time() -> Duration {
        let mut usage = std::mem::MaybeUninit::<ResourceUsage>::zeroed();
        // SAFETY: getrusage(RUSAGE_SELF) fills the rusage it is given.
        let usage = unsafe {
            getrusage(0, usage.as_mut_ptr());
            usage.assume_init()
        };
        let time = |value: TimeValue| {
            Duration::from_secs(value.seconds.unsigned_abs())
                + Duration::from_micros(u64::from(value.micros.unsigned_abs()))
        };
        time(usage.user) + time(usage.system)
    }

    /// The worker's capture pace while the pictures change.
    const INTERVAL: Duration = Duration::from_millis(20);

    /// Draws the animated `view` 60 times a second for `seconds`, as a plug-in does, with the
    /// worker loop's 2 ms event pumps, and takes each new picture, as the worker does. Returns
    /// the time from drawing to taking of each new picture, the wall time and this process's
    /// CPU time.
    fn run_animated(
        view: &AnimatedView,
        window: &MacOsEditorWindow,
        seconds: u64,
    ) -> (Vec<Duration>, Duration, Duration) {
        let mut drawn_at = [Instant::now(); 256];
        let (mut ages, mut last) = (Vec::new(), None);
        let (started, cpu) = (Instant::now(), cpu_time());
        let mut next_draw = started;
        while started.elapsed() < Duration::from_secs(seconds) {
            if Instant::now() >= next_draw {
                next_draw += Duration::from_micros(16_667);
                let frame = view.ivars().get().wrapping_add(1);
                view.ivars().set(frame);
                view.setNeedsDisplay(true);
                view.displayIfNeeded();
                drawn_at[usize::from(frame)] = Instant::now();
            }
            pump_events(Duration::from_millis(2));
            window.request_capture(INTERVAL);
            match window.take_capture() {
                Some(Ok((width, height, pixels))) => {
                    assert_eq!(
                        (width, height),
                        ANIMATED,
                        "one pixel per point, the content"
                    );
                    let number = frame_number(&pixels);
                    if last != Some(number) {
                        last = Some(number);
                        ages.push(drawn_at[usize::from(number)].elapsed());
                    }
                }
                Some(Err(error)) => panic!("the window server gave no picture: {error}"),
                None => {}
            }
        }
        (ages, started.elapsed(), cpu_time().saturating_sub(cpu))
    }

    /// The animated view drawn 60 times a second for 3 s while the worker's loop takes each new
    /// picture: at least 30 new pictures a second, each soon after the view drew it.
    fn animated(mtm: MainThreadMarker) {
        let size = NSRect::new(
            NSPoint::ZERO,
            NSSize::new(f64::from(ANIMATED.0), f64::from(ANIMATED.1)),
        );
        let view: Retained<AnimatedView> = unsafe {
            msg_send![
                super(AnimatedView::alloc(mtm).set_ivars(std::cell::Cell::new(0))),
                initWithFrame: size
            ]
        };
        let window = MacOsEditorWindow::new("capture test").expect("the main thread");
        window.keep_off_desktop();
        window.resize(u32::from(ANIMATED.0), u32::from(ANIMATED.1));
        let content = window.content_view().expect("a content view");
        content.view().addSubview(&view);
        let (mut ages, wall, cpu) = run_animated(&view, &window, 3);
        assert_hidden(mtm);
        ages.sort();
        #[allow(clippy::cast_precision_loss, reason = "a small count")]
        let rate = ages.len() as f64 / wall.as_secs_f64();
        let (median, max) = (ages[ages.len() / 2], ages[ages.len() - 1]);
        println!(
            "animated {} x {}: {rate:.1} new pictures a second, {median:?} median and {max:?} \
             max from drawing to taking; this process used {:.1} % of a core (drawing too)",
            ANIMATED.0,
            ANIMATED.1,
            100.0 * cpu.as_secs_f64() / wall.as_secs_f64()
        );
        assert!(rate >= 30.0, "only {rate:.1} pictures a second");
        assert!(
            median < Duration::from_millis(50),
            "pictures come late: {median:?}"
        );
        window.close();
        pump_events(Duration::from_millis(20));
    }

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

    /// The next window-server picture of `window`, BGRX, and how long it took to come.
    fn picture(window: &MacOsEditorWindow) -> (Vec<u8>, Duration) {
        let started = Instant::now();
        loop {
            window.request_capture(INTERVAL);
            match window.take_capture() {
                Some(Ok((width, height, pixels))) => {
                    assert_eq!(
                        (width, height),
                        SIZE,
                        "one pixel per point, the content only"
                    );
                    return (pixels, started.elapsed());
                }
                Some(Err(error)) => panic!("the window server gave no picture: {error}"),
                None => {}
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "no picture in 5 s"
            );
            pump_events(Duration::from_millis(1));
        }
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
    fn pictured(mtm: MainThreadMarker, name: &str, view: &NSView, check: impl Fn(&[u8]) -> bool) {
        let window = window_with(view);
        let started = Instant::now();
        loop {
            let (pixels, took) = picture(&window);
            assert_hidden(mtm);
            if check(&pixels) {
                println!(
                    "{name}: pictured in its colours ({:?} from the open, the last picture \
                     {took:?} after the one before)",
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
        window.close();
        pump_events(Duration::from_millis(20));
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
        animated(mtm);
        println!("editor_capture_macos: 4 passed");
    }
}
