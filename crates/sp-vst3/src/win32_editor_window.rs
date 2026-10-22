//! Worker-owned Win32 window for a native VST3 editor in the Windows worker, and the editor's
//! preview picture.
//!
//! The worker runs under Wine, which maps this top-level window to an X11 window on the private
//! display in `DISPLAY`; the window streamer shows it there. Wine names the X11 window after the
//! window title (plug-in, rack, and slot) and sets `_NET_WM_PID`. The worker adds the
//! `_SUPERPOSITION_RACK` and `_SUPERPOSITION_SLOT` properties that Linux editor windows carry,
//! through `sp_wine_bridge` (PE code cannot call Xlib), so the streamer maps both kinds of window
//! to their slot the same way. The plug-in view is a child window (`kPlatformTypeHWND`). Windows
//! editors run on window messages, so the loading thread calls [`pump_messages`] on every pass.

use std::{cell::Cell, ffi::c_void, marker::PhantomData, ptr, rc::Rc};

use windows_sys::Win32::{
    Foundation::{GetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, ReleaseDC, SRCCOPY,
        SelectObject,
    },
    System::{
        LibraryLoader::{GetModuleHandleW, GetProcAddress},
        Ole::OleInitialize,
    },
    UI::{
        Input::KeyboardAndMouse::SetFocus,
        WindowsAndMessaging::{
            AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
            GWLP_USERDATA, GetClientRect, GetPropW, GetWindowLongPtrW, MSG, PM_REMOVE,
            PeekMessageW,
            RegisterClassExW, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
            SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage,
            WM_CLOSE, WNDCLASSEXW, WS_CAPTION, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU,
        },
    },
};

use crate::editor_common::{PREVIEW_HEIGHT, PREVIEW_WIDTH, Pixels, encode_png};
pub use crate::editor_common::EditorWindowIdentity;

/// Size of a window before its editor reports one.
const INITIAL_SIZE: i32 = 64;
const STYLE: u32 = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
/// `ERROR_CLASS_ALREADY_EXISTS`: another window registered the class first.
const CLASS_EXISTS: u32 = 1410;
/// `sp_wine_bridge.dll`'s `int sp_wine_x11_label(uint64_t window, uint32_t rack, uint32_t slot)`.
type X11Label = unsafe extern "C" fn(u64, u32, u32) -> i32;

/// Sets up the loading thread for plug-in editors: a single-threaded COM apartment with OLE,
/// which editors use for drag and drop and the clipboard. Call it before any plug-in loads.
///
/// # Errors
/// Returns an error when OLE cannot start.
pub fn prepare_thread() -> Result<(), String> {
    // SAFETY: the reserved argument must be null.
    let status = unsafe { OleInitialize(ptr::null()) };
    if status < 0 {
        return Err(format!("could not start OLE for editors: {status:#x}"));
    }
    Ok(())
}

/// Dispatches every window message queued for the calling thread.
pub fn pump_messages() {
    // SAFETY: `MSG` is plain data that `PeekMessageW` fills before it is read.
    let mut message: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: a null window takes every message for this thread.
    while unsafe { PeekMessageW(&raw mut message, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
        // SAFETY: `message` came from `PeekMessageW`.
        unsafe {
            TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
}

/// Top-level editor window that may only be created and operated on the worker loading thread.
pub struct Win32EditorWindow {
    hwnd: HWND,
    rack: u32,
    slot: u32,
    /// Read by the window procedure through `GWLP_USERDATA` until the window is destroyed.
    state: Box<WindowState>,
    closed: Cell<bool>,
}

struct WindowState {
    close_requested: Cell<bool>,
}

/// The Win32 parent window handed to VST3 `IPlugView::attached`. It cannot cross threads.
pub struct EditorParentView {
    hwnd: HWND,
    _main_thread: PhantomData<Rc<()>>,
}

impl EditorParentView {
    pub(crate) const fn hwnd(&self) -> *mut c_void {
        self.hwnd
    }
}

impl Win32EditorWindow {
    /// Creates a hidden editor window.
    ///
    /// # Errors
    /// Returns an error when Windows (Wine) rejects the window class or window.
    pub fn new(identity: &EditorWindowIdentity<'_>) -> Result<Self, String> {
        let class = wide("SuperpositionPluginEditor");
        // SAFETY: a null name returns this executable's module.
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the class record is 80 bytes"
        )]
        let class_info = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_procedure),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            // SAFETY: the remaining fields are optional and zero means "none".
            ..unsafe { std::mem::zeroed() }
        };
        // SAFETY: `class_info` and the strings it points at outlive the call.
        if unsafe { RegisterClassExW(&raw const class_info) } == 0 {
            // SAFETY: reads this thread's last error.
            let error = unsafe { GetLastError() };
            if error != CLASS_EXISTS {
                return Err(format!("could not register the editor window class: {error}"));
            }
        }
        let title = wide(identity.title);
        let (width, height) = outer_size(INITIAL_SIZE, INITIAL_SIZE);
        // SAFETY: the class is registered and both strings are NUL-terminated.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                STYLE,
                0,
                0,
                width,
                height,
                ptr::null_mut(),
                ptr::null_mut(),
                instance,
                ptr::null(),
            )
        };
        if hwnd.is_null() {
            // SAFETY: reads this thread's last error.
            return Err(format!("could not create editor window: {}", unsafe {
                GetLastError()
            }));
        }
        let state = Box::new(WindowState {
            close_requested: Cell::new(false),
        });
        // SAFETY: the box outlives the window: `close` destroys the window before it drops.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr::from_ref(&*state) as isize) };
        Ok(Self {
            hwnd,
            rack: identity.rack,
            slot: identity.slot,
            state,
            closed: Cell::new(false),
        })
    }

    /// The parent window accepted by VST3 `IPlugView::attached` as `kPlatformTypeHWND`.
    ///
    /// # Errors
    /// Returns an error after the window has closed.
    pub fn content_view(&self) -> Result<EditorParentView, String> {
        if self.closed.get() {
            return Err("native editor window is closed".to_owned());
        }
        Ok(EditorParentView {
            hwnd: self.hwnd,
            _main_thread: PhantomData,
        })
    }

    /// Applies the plug-in's accepted content size.
    pub fn resize(&self, width: u32, height: u32) {
        if self.closed.get() {
            return;
        }
        let dimension = |value: u32| i32::try_from(value.max(1)).unwrap_or(i32::MAX);
        let (width, height) = outer_size(dimension(width), dimension(height));
        // SAFETY: the window is live.
        unsafe {
            SetWindowPos(
                self.hwnd,
                ptr::null_mut(),
                0,
                0,
                width,
                height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
    }

    /// Shows the window above the others and gives it keyboard focus.
    pub fn focus(&self) {
        if self.closed.get() {
            return;
        }
        // SAFETY: the window is live and belongs to this thread.
        unsafe {
            ShowWindow(self.hwnd, SW_SHOW);
            SetForegroundWindow(self.hwnd);
            SetFocus(self.hwnd);
        }
        // Wine creates the X11 window by now; labelling it again is harmless.
        self.label_x11_window();
    }

    /// Sets the rack and slot properties on Wine's X11 window for this window. Does nothing
    /// outside Wine or without the bridge.
    fn label_x11_window(&self) {
        let property = wide("__wine_x11_whole_window");
        let bridge = wide("sp_wine_bridge.dll");
        // SAFETY: both names are NUL-terminated; the window is live. The bridge stays loaded:
        // the worker's shared-memory backend loaded it first.
        unsafe {
            let window = GetPropW(self.hwnd, property.as_ptr()) as u64;
            let module = GetModuleHandleW(bridge.as_ptr());
            if window == 0 || module.is_null() {
                return;
            }
            if let Some(label) = GetProcAddress(module, c"sp_wine_x11_label".as_ptr().cast()) {
                let label =
                    std::mem::transmute::<unsafe extern "system" fn() -> isize, X11Label>(label);
                label(window, self.rack, self.slot);
            }
        }
    }

    /// Takes a close request without closing the window or detaching its plug-in view. A
    /// `WM_CLOSE` message or [`Self::request_close`] requests a close.
    pub fn take_close_request(&self) -> bool {
        !self.closed.get() && self.state.close_requested.replace(false)
    }

    /// Requests the same close as the window's close button.
    pub fn request_close(&self) {
        if !self.closed.get() {
            self.state.close_requested.set(true);
        }
    }

    /// Destroys the window after the plug-in view has detached.
    pub fn close(&self) {
        if !self.closed.replace(true) {
            // SAFETY: the window is live and belongs to this thread.
            unsafe { DestroyWindow(self.hwnd) };
        }
    }

    /// Moves the window's top-left corner to `left`, `top` pixels from the screen's top-left.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "positions are clamped to a 16-bit screen range first"
    )]
    pub fn set_top_left(&self, left: f64, top: f64) {
        if self.closed.get() {
            return;
        }
        let coordinate = |value: f64| value.round().clamp(-32_768.0, 32_767.0) as i32;
        // SAFETY: the window is live.
        unsafe {
            SetWindowPos(
                self.hwnd,
                ptr::null_mut(),
                coordinate(left),
                coordinate(top),
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
    }

    /// Pictures the editor's client area as a 320×200 PNG, scaled to cover and centre-cropped.
    /// The plug-in's child window draws into the same Wine window surface, so a copy from the
    /// window's device context includes it.
    ///
    /// # Errors
    /// Returns an error when the window is closed or empty, GDI fails, or PNG encoding fails.
    pub fn capture_preview_png(&self) -> Result<Vec<u8>, String> {
        if self.closed.get() {
            return Err("native editor window is closed".to_owned());
        }
        let mut client = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: the window is live and `client` is writable.
        unsafe { GetClientRect(self.hwnd, &raw mut client) };
        let (width, height) = (client.right - client.left, client.bottom - client.top);
        let (Ok(columns), Ok(rows)) = (usize::try_from(width), usize::try_from(height)) else {
            return Err("native editor window has no area".to_owned());
        };
        if columns == 0 || rows == 0 {
            return Err("native editor window has no area".to_owned());
        }
        let bgra = Capture::new(self.hwnd, width, height)?.copy()?;
        let rgb = bgra
            .chunks_exact(4)
            .flat_map(|pixel| [pixel[2], pixel[1], pixel[0]])
            .collect();
        let pixels = Pixels {
            width: columns,
            height: rows,
            rgb,
        };
        encode_png(&pixels.cover(PREVIEW_WIDTH, PREVIEW_HEIGHT))
    }
}

impl Drop for Win32EditorWindow {
    fn drop(&mut self) {
        self.close();
    }
}

/// GDI objects for one copy of a window's client area into a top-down 32-bit DIB.
struct Capture {
    hwnd: HWND,
    window_dc: *mut c_void,
    memory_dc: *mut c_void,
    bitmap: *mut c_void,
    previous: *mut c_void,
    bits: *mut c_void,
    width: i32,
    height: i32,
}

impl Capture {
    fn new(hwnd: HWND, width: i32, height: i32) -> Result<Self, String> {
        // SAFETY: the window is live; each object is released in `Drop`.
        unsafe {
            let window_dc = GetDC(hwnd);
            if window_dc.is_null() {
                return Err("could not picture editor window: no device context".to_owned());
            }
            let mut capture = Self {
                hwnd,
                window_dc,
                memory_dc: CreateCompatibleDC(window_dc),
                bitmap: ptr::null_mut(),
                previous: ptr::null_mut(),
                bits: ptr::null_mut(),
                width,
                height,
            };
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "the header is 40 bytes"
                )]
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Negative: rows run top to bottom.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            capture.bitmap = CreateDIBSection(
                window_dc,
                &raw const info,
                DIB_RGB_COLORS,
                &raw mut capture.bits,
                ptr::null_mut(),
                0,
            );
            if capture.memory_dc.is_null() || capture.bitmap.is_null() || capture.bits.is_null()
            {
                return Err("could not picture editor window: no bitmap".to_owned());
            }
            capture.previous = SelectObject(capture.memory_dc, capture.bitmap);
            Ok(capture)
        }
    }

    /// Copies the window's pixels: `width × height` BGRA values, rows top to bottom.
    fn copy(&self) -> Result<Vec<u8>, String> {
        // SAFETY: both device contexts are live and the bitmap holds `width × height` pixels.
        let copied = unsafe {
            BitBlt(
                self.memory_dc,
                0,
                0,
                self.width,
                self.height,
                self.window_dc,
                0,
                0,
                SRCCOPY,
            ) != 0
                && GdiFlush() != 0
        };
        if !copied {
            return Err("could not picture editor window: copy failed".to_owned());
        }
        let length = usize::try_from(self.width).unwrap_or_default()
            * usize::try_from(self.height).unwrap_or_default()
            * 4;
        // SAFETY: the DIB section holds exactly `length` bytes and GDI has finished with it.
        Ok(unsafe { std::slice::from_raw_parts(self.bits.cast::<u8>(), length) }.to_vec())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: each object was created by `new` and is released once.
        unsafe {
            if !self.previous.is_null() {
                SelectObject(self.memory_dc, self.previous);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            if !self.memory_dc.is_null() {
                DeleteDC(self.memory_dc);
            }
            ReleaseDC(self.hwnd, self.window_dc);
        }
    }
}

/// The outer window size for a client area of `width` × `height`.
fn outer_size(width: i32, height: i32) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    // SAFETY: `rect` is writable.
    unsafe { AdjustWindowRectEx(&raw mut rect, STYLE, 0, 0) };
    (rect.right - rect.left, rect.bottom - rect.top)
}

/// Turns the close button into a close request; everything else is default handling.
unsafe extern "system" fn window_procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_CLOSE {
        // SAFETY: `GWLP_USERDATA` is null or points at the window's live state.
        let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WindowState;
        // SAFETY: as above.
        if let Some(state) = unsafe { state.as_ref() } {
            state.close_requested.set(true);
            return 0;
        }
    }
    // SAFETY: forwards the message unchanged.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}
