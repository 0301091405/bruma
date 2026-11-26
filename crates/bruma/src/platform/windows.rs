//! Everything that talks to Win32: screen capture, window list, clipboard,
//! local time, single-instance lock.

use std::ffi::c_void;

use bruma_core::redact::Word;
use bruma_core::{Image, Rect};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM,
    POINT, RECT, WAIT_OBJECT_0,
};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, GetCurrentProcessId, INFINITE, OpenEventW,
    SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, EnumWindows, GWL_EXSTYLE, GetClassNameW, GetCursorPos,
    GetMessagePos, GetSystemMetrics, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, w};

use crate::config::LocalTime;

/// A frozen frame of the whole virtual desktop.
pub struct Capture {
    pub image: Image,
    /// Desktop coordinates of the image's top-left pixel.
    pub origin: (i32, i32),
}

/// Opts the process into per-monitor DPI awareness so every coordinate is in
/// physical pixels. Must run before any window or capture call.
pub fn init_dpi() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Lets a GUI-subsystem build print to the terminal it was started from.
pub fn attach_console() {
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

static CAPTURE_CURSOR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether captures include the mouse cursor (the `cursor` setting).
pub fn set_capture_cursor(on: bool) {
    CAPTURE_CURSOR.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Draws the mouse cursor into a capture whose top-left pixel is at desktop
/// position `origin`.
unsafe fn draw_cursor(dc: windows::Win32::Graphics::Gdi::HDC, origin: (i32, i32)) {
    use windows::Win32::UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, ICONINFO,
    };
    unsafe {
        let mut info = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if GetCursorInfo(&mut info).is_err() || info.flags != CURSOR_SHOWING {
            return;
        }
        // The cursor's position is its hot spot, not its top-left corner.
        let mut icon = ICONINFO::default();
        let (hot_x, hot_y) = match GetIconInfo(info.hCursor.into(), &mut icon) {
            Ok(()) => {
                let _ = DeleteObject(icon.hbmMask.into());
                let _ = DeleteObject(icon.hbmColor.into());
                (icon.xHotspot as i32, icon.yHotspot as i32)
            }
            Err(_) => (0, 0),
        };
        let _ = DrawIconEx(
            dc,
            info.ptScreenPos.x - origin.0 - hot_x,
            info.ptScreenPos.y - origin.1 - hot_y,
            info.hCursor.into(),
            0,
            0,
            0,
            None,
            DI_NORMAL,
        );
    }
}

/// Where the virtual desktop starts and how large it is, in pixels.
pub fn virtual_screen() -> ((i32, i32), (u32, u32)) {
    unsafe {
        (
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
            ),
            (
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as u32,
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as u32,
            ),
        )
    }
}

/// The screen as the system hands it over: blue, green, red and an unused
/// byte for every pixel, top row first.
pub struct Raw<'a> {
    pub bytes: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub origin: (i32, i32),
}

/// A bitmap the size of the desktop that the screen is copied into. Whoever
/// copies the screen again and again keeps one, instead of asking for
/// megabytes of fresh memory every time.
pub struct Grabber {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *const u8,
    origin: (i32, i32),
    size: (i32, i32),
}

/// Where the virtual desktop starts and its size, as the system reports them.
pub(super) fn desktop() -> ((i32, i32), (i32, i32)) {
    unsafe {
        (
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
            ),
            (
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            ),
        )
    }
}

impl Grabber {
    pub fn new() -> Result<Self, String> {
        let (origin, size) = desktop();
        if size.0 <= 0 || size.1 <= 0 {
            return Err("the desktop has no size".into());
        }
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return Err("could not open the screen device context".into());
            }
            let dc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: size.0,
                    biHeight: -size.1, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                Ok(b) if !bits.is_null() => b,
                other => {
                    let _ = DeleteDC(dc);
                    return Err(format!(
                        "could not allocate the capture bitmap: {:?}",
                        other.err()
                    ));
                }
            };
            let previous = SelectObject(dc, bitmap.into());
            Ok(Self {
                dc,
                bitmap,
                previous,
                bits: bits as *const u8,
                origin,
                size,
            })
        }
    }

    /// Copies the screen as it is now. The bytes stay valid until the next
    /// grab.
    pub fn grab(&mut self) -> Result<Raw<'_>, String> {
        // Monitors come and go; a bitmap of the old desktop is of no use.
        if desktop() != (self.origin, self.size) {
            *self = Self::new()?;
        }
        let (x, y) = self.origin;
        let (w, h) = self.size;
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return Err("could not open the screen device context".into());
            }
            let blit = BitBlt(
                self.dc,
                0,
                0,
                w,
                h,
                Some(screen),
                x,
                y,
                SRCCOPY | CAPTUREBLT,
            );
            if blit.is_ok() && CAPTURE_CURSOR.load(std::sync::atomic::Ordering::Relaxed) {
                draw_cursor(self.dc, (x, y));
            }
            ReleaseDC(None, screen);
            blit.map_err(|e| format!("screen copy failed: {e}"))?;
            let _ = GdiFlush();
            Ok(Raw {
                bytes: std::slice::from_raw_parts(self.bits, w as usize * h as usize * 4),
                width: w as u32,
                height: h as u32,
                origin: (x, y),
            })
        }
    }
}

impl Drop for Grabber {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

/// Copies the virtual desktop into an [`Image`] with GDI.
pub fn capture() -> Result<Capture, String> {
    let mut grabber = Grabber::new()?;
    let raw = grabber.grab()?;
    Ok(Capture {
        image: Image::from_bgrx(raw.width, raw.height, raw.bytes),
        origin: raw.origin,
    })
}

/// Puts a hidden window on screen and leaves the active window alone.
pub fn show_without_activating(window: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{SW_SHOWNA, ShowWindow};
    unsafe {
        let _ = ShowWindow(HWND(window as *mut c_void), SW_SHOWNA);
    }
}

/// Whether captures include the mouse cursor.
pub fn captures_cursor() -> bool {
    CAPTURE_CURSOR.load(std::sync::atomic::Ordering::Relaxed)
}

/// Puts a thread that was told to stay in the background ahead of ordinary
/// work, for something the user is waiting for. The thread goes back by
/// calling [`run_this_thread_in_the_background`].
pub fn hurry(thread: &std::thread::JoinHandle<()>) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::System::Threading::{SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
    unsafe {
        let _ = SetThreadPriority(HANDLE(thread.as_raw_handle()), THREAD_PRIORITY_ABOVE_NORMAL);
    }
}

/// Lets everything else go first: for a thread whose work can wait.
pub fn run_this_thread_in_the_background() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// How long ago the user last touched the mouse or the keyboard.
pub fn idle_for() -> std::time::Duration {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    unsafe {
        if GetLastInputInfo(&mut info).as_bool() {
            std::time::Duration::from_millis(GetTickCount().wrapping_sub(info.dwTime) as u64)
        } else {
            std::time::Duration::ZERO
        }
    }
}

/// What the user is doing, as far as recording the screen is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attention {
    /// The ordinary desktop.
    Desktop,
    /// A game, a film or a presentation fills the screen.
    FullScreen,
    /// The machine is locked or showing a screen saver.
    Away,
}

pub fn attention() -> Attention {
    use windows::Win32::UI::Shell::SHQueryUserNotificationState;
    // 1: not present. 2: busy (full screen). 3: Direct3D full screen.
    // 4: presentation mode.
    match unsafe { SHQueryUserNotificationState() }.map(|state| state.0) {
        Ok(1) => Attention::Away,
        Ok(2..=4) => Attention::FullScreen,
        _ => Attention::Desktop,
    }
}

struct WindowScan {
    own_pid: u32,
    rects: Vec<Rect>,
}

unsafe extern "system" fn scan_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        let scan = &mut *(lparam.0 as *mut WindowScan);
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return true.into();
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return true.into();
        }
        if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return true.into();
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == scan.own_pid {
            return true.into();
        }
        let mut class = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut class) as usize;
        let class = String::from_utf16_lossy(&class[..n]);
        if matches!(class.as_str(), "Progman" | "WorkerW") {
            return true.into();
        }

        let mut rect = RECT::default();
        let framed = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut c_void,
            size_of::<RECT>() as u32,
        );
        if framed.is_err() && GetWindowRect(hwnd, &mut rect).is_err() {
            return true.into();
        }
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        if w >= 8 && h >= 8 {
            scan.rects.push(Rect::new(
                rect.left as f32,
                rect.top as f32,
                w as f32,
                h as f32,
            ));
        }
        true.into()
    }
}

/// Visible top-level windows in desktop coordinates, front-most first.
pub fn windows() -> Vec<Rect> {
    let mut scan = WindowScan {
        own_pid: unsafe { GetCurrentProcessId() },
        rects: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(scan_window),
            LPARAM(&mut scan as *mut WindowScan as isize),
        );
    }
    scan.rects
}

/// The mouse cursor in desktop coordinates.
pub fn cursor_position() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.ok()?;
    Some((point.x, point.y))
}

/// Where the cursor was when the message now being handled was posted.
///
/// A button message can be handled before the mouse-move that preceded it,
/// so a click must not trust the last move event for its position.
pub fn message_position() -> (i32, i32) {
    let packed = unsafe { GetMessagePos() };
    (
        (packed & 0xFFFF) as u16 as i16 as i32,
        (packed >> 16) as u16 as i16 as i32,
    )
}

pub fn local_time() -> LocalTime {
    let t = unsafe { GetLocalTime() };
    LocalTime {
        year: t.wYear as u32,
        month: t.wMonth as u32,
        day: t.wDay as u32,
        hour: t.wHour as u32,
        minute: t.wMinute as u32,
        second: t.wSecond as u32,
    }
}

/// Held for the life of the background process; a second holder is refused.
pub struct SingleInstance(#[allow(dead_code)] HANDLE);

impl SingleInstance {
    /// `None` when another bruma background process already runs.
    pub fn acquire() -> Option<Self> {
        unsafe {
            let handle = CreateMutexW(None, true, w!("Local\\bruma-background")).ok()?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                return None;
            }
            Some(Self(handle))
        }
    }
}

// ---- capture requests between processes -------------------------------------

const CAPTURE_REQUEST: windows::core::PCWSTR = w!("Local\\bruma-capture-request");

/// Asks the running background process to open the overlay, so a capture
/// started from the command line can still rewind. `false` when no background
/// process is listening.
pub fn request_capture() -> bool {
    unsafe {
        let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, CAPTURE_REQUEST) else {
            return false;
        };
        // Hand our right to take the foreground to whoever shows the overlay.
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        let sent = SetEvent(event).is_ok();
        let _ = CloseHandle(event);
        sent
    }
}

/// Background side: runs `on_request` every time another process asks for a
/// capture. Returns `false` when the listener could not be set up.
pub fn listen_for_capture_requests(on_request: impl Fn() + Send + 'static) -> bool {
    let Ok(event) = (unsafe { CreateEventW(None, false, false, CAPTURE_REQUEST) }) else {
        return false;
    };
    // A raw handle is just a number; move it into the thread as one.
    let raw = event.0 as isize;
    std::thread::Builder::new()
        .name("bruma-requests".into())
        .spawn(move || {
            let event = HANDLE(raw as *mut c_void);
            while unsafe { WaitForSingleObject(event, INFINITE) } == WAIT_OBJECT_0 {
                on_request();
            }
        })
        .is_ok()
}

// ---- clipboard --------------------------------------------------------------

struct OpenedClipboard;

impl OpenedClipboard {
    fn open() -> Result<Self, String> {
        // Another process may hold the clipboard for a moment; retry briefly.
        for _ in 0..10 {
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(Self);
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        Err("the clipboard is busy".into())
    }

    fn set(&self, format: u32, bytes: &[u8]) -> Result<(), String> {
        unsafe {
            let handle: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes.len())
                .map_err(|e| format!("clipboard memory: {e}"))?;
            let target = GlobalLock(handle) as *mut u8;
            if target.is_null() {
                let _ = GlobalFree(Some(handle));
                return Err("clipboard memory could not be locked".into());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
            let _ = GlobalUnlock(handle);
            // On success the system owns the memory.
            if let Err(e) = SetClipboardData(format, Some(HANDLE(handle.0))) {
                let _ = GlobalFree(Some(handle));
                return Err(format!("clipboard write: {e}"));
            }
            Ok(())
        }
    }
}

impl Drop for OpenedClipboard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// A `CF_DIB` payload: header plus bottom-up BGRA rows.
fn dib(image: &Image) -> Vec<u8> {
    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: image.width as i32,
        biHeight: image.height as i32, // bottom-up
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: image.width * image.height * 4,
        ..Default::default()
    };
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            &header as *const BITMAPINFOHEADER as *const u8,
            size_of::<BITMAPINFOHEADER>(),
        )
    };
    let mut out = Vec::with_capacity(header_bytes.len() + image.rgba.len());
    out.extend_from_slice(header_bytes);
    let stride = image.width as usize * 4;
    for row in image.rgba.chunks_exact(stride.max(1)).rev() {
        for px in row.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    out
}

/// Puts the image on the clipboard as both a bitmap and a PNG.
pub fn copy_image(image: &Image) -> Result<(), String> {
    let png = bruma_core::export::encode_png(image).map_err(|e| e.to_string())?;
    let bitmap = dib(image);
    let clipboard = OpenedClipboard::open()?;
    unsafe {
        EmptyClipboard().map_err(|e| format!("clipboard clear: {e}"))?;
        clipboard.set(CF_DIB.0 as u32, &bitmap)?;
        let png_format = RegisterClipboardFormatW(w!("PNG"));
        if png_format != 0 {
            clipboard.set(png_format, &png)?;
        }
    }
    Ok(())
}

pub fn copy_text(text: &str) -> Result<(), String> {
    let wide: Vec<u8> = text
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    let clipboard = OpenedClipboard::open()?;
    unsafe {
        EmptyClipboard().map_err(|e| format!("clipboard clear: {e}"))?;
    }
    clipboard.set(CF_UNICODETEXT.0 as u32, &wide)
}

// ---- text recognition --------------------------------------------------------

/// Recognises the text in `image` with the OCR engine built into Windows.
/// Word rectangles are in the image's own pixels.
pub fn ocr(image: &Image) -> Result<Vec<Word>, String> {
    use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::Ocr::OcrEngine;
    use windows::Security::Cryptography::CryptographicBuffer;

    let fail = |e: windows::core::Error| format!("text recognition failed: {}", e.message());
    if image.width == 0 || image.height == 0 {
        return Ok(Vec::new());
    }
    let limit = OcrEngine::MaxImageDimension().map_err(fail)?;
    if image.width.max(image.height) > limit {
        return Err(format!(
            "the selection is too large for text recognition (limit {limit} px)"
        ));
    }
    // Screen text is small; the engine reads it far better at twice the size.
    let factor = if image.width.max(image.height) * 2 <= limit {
        2
    } else {
        1
    };
    let (w, h) = (image.width * factor, image.height * factor);
    let rgba = if factor == 1 {
        image.rgba.clone()
    } else {
        let source = bruma_core::blur::Buf {
            w: image.width as usize,
            h: image.height as usize,
            px: image.rgba.clone(),
        };
        bruma_core::blur::upscale(&source, w as usize, h as usize, 1.0)
    };
    let mut bgra = rgba;
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }

    let buffer = CryptographicBuffer::CreateFromByteArray(&bgra).map_err(fail)?;
    let bitmap =
        SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, w as i32, h as i32)
            .map_err(fail)?;
    let engine = OcrEngine::TryCreateFromUserProfileLanguages()
        .map_err(|_| "no text recognition language is installed in Windows".to_string())?;
    let result = engine
        .RecognizeAsync(&bitmap)
        .and_then(|operation| operation.join())
        .map_err(fail)?;

    let scale = factor as f32;
    let mut words = Vec::new();
    for (line, text_line) in result.Lines().map_err(fail)?.into_iter().enumerate() {
        for word in text_line.Words().map_err(fail)? {
            let r = word.BoundingRect().map_err(fail)?;
            words.push(Word {
                text: word.Text().map_err(fail)?.to_string(),
                rect: Rect::new(r.X / scale, r.Y / scale, r.Width / scale, r.Height / scale),
                line: line as u32,
            });
        }
    }
    Ok(words)
}

// ---- start with Windows ------------------------------------------------------

const RUN_KEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: windows::core::PCWSTR = w!("Bruma");

/// The folder Windows gives the program for its own files when it was
/// installed as a Windows app package. `None` for an ordinary install.
///
/// A packaged program that writes under `AppData` has its files quietly moved
/// to a private place only it can see, so an editor or a browser asked to open
/// one of them finds nothing. This folder is real for everybody.
pub fn package_folder() -> Option<std::path::PathBuf> {
    use windows::Storage::ApplicationData;
    use windows::Win32::Foundation::APPMODEL_ERROR_NO_PACKAGE;
    use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
    static FOLDER: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    FOLDER
        .get_or_init(|| {
            let mut length = 0u32;
            if unsafe { GetCurrentPackageFullName(&mut length, None) } == APPMODEL_ERROR_NO_PACKAGE
            {
                return None;
            }
            let folder = ApplicationData::Current().ok()?.LocalFolder().ok()?;
            Some(std::path::PathBuf::from(folder.Path().ok()?.to_string()))
        })
        .clone()
}

/// The startup task a packaged install declares in its manifest. A package
/// cannot use the registry's Run key: its registry writes are private too.
fn startup_task() -> Option<windows::ApplicationModel::StartupTask> {
    use windows::ApplicationModel::StartupTask;
    use windows::core::HSTRING;
    package_folder()?;
    StartupTask::GetAsync(&HSTRING::from("BrumaStartup"))
        .and_then(|operation| operation.join())
        .ok()
}

/// Whether Bruma is set to start when you sign in.
pub fn autostart_enabled() -> bool {
    use windows::ApplicationModel::StartupTaskState;
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
    if let Some(task) = startup_task() {
        return task.State().is_ok_and(|state| {
            state == StartupTaskState::Enabled || state == StartupTaskState::EnabledByPolicy
        });
    }
    let mut size = 0u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
        .is_ok()
    }
}

/// Adds or removes this executable from the programs that start at sign-in.
/// Only the current user is affected; no administrator rights are needed.
pub fn set_autostart(on: bool) -> Result<(), String> {
    use windows::ApplicationModel::StartupTaskState;
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
    };
    if let Some(task) = startup_task() {
        let fail = |e: windows::core::Error| {
            format!("could not change the startup setting: {}", e.message())
        };
        if !on {
            return task.Disable().map_err(fail);
        }
        let state = task
            .RequestEnableAsync()
            .and_then(|operation| operation.join())
            .map_err(fail)?;
        return match state {
            StartupTaskState::Enabled | StartupTaskState::EnabledByPolicy => Ok(()),
            // Switched off in Task Manager or Settings: only the user can
            // switch it back on there.
            _ => Err("Windows keeps Bruma from starting by itself; allow it under Startup apps in Settings".into()),
        };
    }
    unsafe {
        if !on {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
            return Ok(());
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let command: Vec<u16> = format!("\"{}\"", exe.display())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            REG_SZ.0,
            Some(command.as_ptr() as *const c_void),
            (command.len() * 2) as u32,
        )
        .ok()
        .map_err(|e| format!("could not change the startup setting: {}", e.message()))
    }
}

// ---- scrolling for a scrolling capture ----------------------------------------

/// Turns the mouse wheel by `notches`; negative scrolls down.
pub fn scroll_wheel(notches: i32) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
    };
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: (notches * 120) as u32,
                dwFlags: MOUSEEVENTF_WHEEL,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(&[input], size_of::<INPUT>() as i32);
    }
}

/// Moves the mouse cursor to a desktop position.
pub fn set_cursor_position(x: i32, y: i32) {
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::SetCursorPos(x, y);
    }
}

/// Whether Esc is being held down right now.
pub fn escape_pressed() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE};
    unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8000 != 0 }
}
