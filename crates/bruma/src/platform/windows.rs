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
    ASFW_ANY, AllowSetForegroundWindow, EnumChildWindows, EnumWindows, GWL_EXSTYLE, GetClassNameW,
    GetCursorPos, GetMessagePos, GetSystemMetrics, GetWindowLongW, GetWindowRect,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WS_EX_TOOLWINDOW,
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

/// Gives a window of Bruma's the keyboard. Right after the hotkey Windows
/// allows it; `false` when it did not.
pub fn bring_forward(window: isize) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};
    let hwnd = HWND(window as *mut c_void);
    unsafe { GetForegroundWindow() == hwnd || SetForegroundWindow(hwnd).as_bool() }
}

/// Gives a window of Bruma's the keyboard even when Windows would rather
/// leave it where it is: a game in front, played with, keeps it otherwise.
/// Windows lets the program that sent the last key take the keyboard, so a
/// key-up of a key no keyboard has is sent first, which nothing acts on.
/// Over a game run as administrator this works only when Bruma runs as
/// administrator too. `true` when the window has the keyboard.
pub fn force_forward(window: isize) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};
    if bring_forward(window) {
        return true;
    }
    let hwnd = HWND(window as *mut c_void);
    let nothing = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                // 0x88 to 0x8F are not given to any key.
                wVk: VIRTUAL_KEY(0x88),
                dwFlags: KEYEVENTF_KEYUP,
                ..Default::default()
            },
        },
    };
    unsafe {
        SendInput(&[nothing], size_of::<INPUT>() as i32);
        let _ = SetForegroundWindow(hwnd);
        GetForegroundWindow() == hwnd
    }
}

/// Hands the keyboard back to a window that had it, bringing it back up if
/// it went down to the taskbar (a full-screen game does when it loses it).
pub fn give_back(window: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{
        IsWindow, SW_RESTORE, SetForegroundWindow, ShowWindow,
    };
    let hwnd = HWND(window as *mut c_void);
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return;
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
    }
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

/// Puts a thread that was told to stay in the background ahead of
/// everything else, for something the user is waiting for. The thread goes
/// back by calling [`run_this_thread_in_the_background`].
pub fn hurry(thread: &std::thread::JoinHandle<()>) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::System::Threading::{SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL};
    unsafe {
        let _ = SetThreadPriority(
            HANDLE(thread.as_raw_handle()),
            THREAD_PRIORITY_TIME_CRITICAL,
        );
    }
}

/// Puts the calling thread ahead of all ordinary work, a game's included.
/// For the thread that answers the hotkey: it sleeps until then, so this
/// costs nothing, and when the key comes it is not left waiting behind
/// whatever keeps the processor busy.
pub fn run_this_thread_first() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    };
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }
}

/// Puts a thread ahead of all ordinary work, like [`run_this_thread_first`]:
/// for helpers the hotkey's thread waits on.
pub fn put_first(thread: &impl std::os::windows::io::AsRawHandle) {
    use windows::Win32::System::Threading::THREAD_PRIORITY_TIME_CRITICAL;
    set_thread_priority(thread, THREAD_PRIORITY_TIME_CRITICAL.0);
}

/// The calling thread's priority, to hand on to a helper thread.
pub fn this_thread_priority() -> i32 {
    use windows::Win32::System::Threading::{GetCurrentThread, GetThreadPriority};
    unsafe { GetThreadPriority(GetCurrentThread()) }
}

/// Gives a thread just started a priority read with [`this_thread_priority`].
/// The thread that started it does this, since on a busy machine the new
/// thread may not get to run to do it itself.
pub fn set_thread_priority(thread: &impl std::os::windows::io::AsRawHandle, level: i32) {
    use windows::Win32::System::Threading::{SetThreadPriority, THREAD_PRIORITY};
    unsafe {
        let _ = SetThreadPriority(HANDLE(thread.as_raw_handle()), THREAD_PRIORITY(level));
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
        Ok(3) => Attention::FullScreen,
        // "Busy" is also said when a window nobody sees covers the screen
        // (the overlays of graphics drivers and chat programs), with the
        // desktop in front: then it is the desktop. Only a window in front
        // that covers its monitor makes it full screen.
        Ok(2 | 4) if front_covers_its_monitor() => Attention::FullScreen,
        _ => Attention::Desktop,
    }
}

/// Whether the window in front covers the whole of its monitor and is not
/// the desktop or the taskbar.
fn front_covers_its_monitor() -> bool {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe {
        let front = GetForegroundWindow();
        if front.is_invalid() {
            return false;
        }
        let mut class = [0u16; 64];
        let n = GetClassNameW(front, &mut class) as usize;
        let class = String::from_utf16_lossy(&class[..n]);
        if matches!(
            class.as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        ) {
            return false;
        }
        // Windows nobody sees or uses cover the screen too: the overlays of
        // graphics drivers and chat programs let clicks through and never
        // take the keyboard, and hidden system windows (Start, search) are
        // cloaked. The keyboard can still land on one for a moment after a
        // window closes. None of them is a game.
        const NOACTIVATE: u32 = 0x0800_0000;
        const TRANSPARENT: u32 = 0x0000_0020;
        if GetWindowLongW(front, GWL_EXSTYLE) as u32 & (NOACTIVATE | TRANSPARENT) != 0 {
            return false;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(
            front,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return false;
        }
        let mut rect = RECT::default();
        if GetWindowRect(front, &mut rect).is_err() {
            return false;
        }
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let monitor = MonitorFromWindow(front, MONITOR_DEFAULTTONEAREST);
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return false;
        }
        let m = info.rcMonitor;
        rect.left <= m.left && rect.top <= m.top && rect.right >= m.right && rect.bottom >= m.bottom
    }
}

struct WindowScan {
    own_pid: u32,
    rects: Vec<Rect>,
    /// Parts of the front-most windows, each with its window.
    panes: Vec<(Rect, Rect)>,
}

/// Only the front windows are searched for parts, and only so many parts are
/// kept: a browser or an editor has a handful, and the search must stay quick.
const PANE_WINDOWS: usize = 8;
const MAX_PANES: usize = 96;

struct PaneScan<'a> {
    owner: Rect,
    panes: &'a mut Vec<(Rect, Rect)>,
}

/// A visible child window large enough to be a part worth selecting: the page
/// of a browser, the text of an editor, the list in a file window.
unsafe extern "system" fn scan_pane(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        let scan = &mut *(lparam.0 as *mut PaneScan);
        if scan.panes.len() >= MAX_PANES {
            return false.into();
        }
        if !IsWindowVisible(hwnd).as_bool() {
            return true.into();
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return true.into();
        }
        let pane = Rect::new(
            rect.left as f32,
            rect.top as f32,
            (rect.right - rect.left) as f32,
            (rect.bottom - rect.top) as f32,
        )
        .intersect(&scan.owner);
        if pane.w >= 120.0 && pane.h >= 80.0 {
            scan.panes.push((scan.owner, pane));
        }
        true.into()
    }
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
            let owner = Rect::new(rect.left as f32, rect.top as f32, w as f32, h as f32);
            scan.rects.push(owner);
            if scan.rects.len() <= PANE_WINDOWS {
                let mut panes = PaneScan {
                    owner,
                    panes: &mut scan.panes,
                };
                let _ = EnumChildWindows(
                    Some(hwnd),
                    Some(scan_pane),
                    LPARAM(&mut panes as *mut PaneScan as isize),
                );
            }
        }
        true.into()
    }
}

/// Visible top-level windows in desktop coordinates, front-most first, and
/// the larger parts of the front ones, each with its window.
pub fn windows_and_panes() -> (Vec<Rect>, Vec<(Rect, Rect)>) {
    let mut scan = WindowScan {
        own_pid: unsafe { GetCurrentProcessId() },
        rects: Vec::new(),
        panes: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(scan_window),
            LPARAM(&mut scan as *mut WindowScan as isize),
        );
    }
    (scan.rects, scan.panes)
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
pub struct SingleInstance(HANDLE);

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

impl Drop for SingleInstance {
    /// Lets go, so that another Bruma (one run as administrator, taking
    /// over) can be the one in the background.
    fn drop(&mut self) {
        use windows::Win32::System::Threading::ReleaseMutex;
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

// ---- requests between processes ----------------------------------------------

/// A named event other programs of this user can signal, also when Bruma
/// runs as administrator and they do not: by default such an event would be
/// out of their reach. Auto-reset, unsignalled.
fn open_event(name: windows::core::PCWSTR) -> Option<HANDLE> {
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    unsafe {
        // Signed-in users may use it, and its integrity level is medium, so
        // that a program not run as administrator may signal it.
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let described = ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:(A;;GA;;;AU)S:(ML;;NW;;;ME)"),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .is_ok();
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let event = CreateEventW(
            described.then_some(&attributes as *const _),
            false,
            false,
            name,
        );
        if described {
            let _ = windows::Win32::Foundation::LocalFree(Some(
                windows::Win32::Foundation::HLOCAL(descriptor.0),
            ));
        }
        event.ok()
    }
}

/// Runs `on_request` every time the event `name` is signalled.
fn listen(
    name: windows::core::PCWSTR,
    thread: &str,
    on_request: impl Fn() + Send + 'static,
) -> bool {
    let Some(event) = open_event(name) else {
        return false;
    };
    // A raw handle is just a number; move it into the thread as one.
    let raw = event.0 as isize;
    std::thread::Builder::new()
        .name(thread.into())
        .spawn(move || {
            let event = HANDLE(raw as *mut c_void);
            while unsafe { WaitForSingleObject(event, INFINITE) } == WAIT_OBJECT_0 {
                on_request();
            }
        })
        .is_ok()
}

/// Signals the event `name`; `false` when nobody is listening.
fn signal(name: windows::core::PCWSTR) -> bool {
    unsafe {
        let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, name) else {
            return false;
        };
        // Hand our right to take the foreground to whoever answers.
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        let sent = SetEvent(event).is_ok();
        let _ = CloseHandle(event);
        sent
    }
}

const QUIT_REQUEST: windows::core::PCWSTR = w!("Local\\bruma-quit-request");

/// Asks the background process to quit (setup does, before replacing it).
/// `false` when none is running.
pub fn request_quit() -> bool {
    signal(QUIT_REQUEST)
}

/// Background side of [`request_quit`].
pub fn listen_for_quit_requests(on_request: impl Fn() + Send + 'static) -> bool {
    listen(QUIT_REQUEST, "bruma-quit-requests", on_request)
}

// ---- capture requests between processes -------------------------------------

const CAPTURE_REQUEST: windows::core::PCWSTR = w!("Local\\bruma-capture-request");

/// Asks the running background process to open the overlay, so a capture
/// started from the command line can still rewind. `false` when no background
/// process is listening.
pub fn request_capture() -> bool {
    signal(CAPTURE_REQUEST)
}

/// Background side: runs `on_request` every time another process asks for a
/// capture. Returns `false` when the listener could not be set up.
pub fn listen_for_capture_requests(on_request: impl Fn() + Send + 'static) -> bool {
    listen(CAPTURE_REQUEST, "bruma-requests", on_request)
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

/// Puts files on the clipboard, the way Explorer's Copy does: pasted into a
/// chat or a folder, they arrive as files. A GIF goes this way, since there
/// is no picture format for moving pictures.
pub fn copy_files(paths: &[std::path::PathBuf]) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::DROPFILES;
    let mut names: Vec<u16> = Vec::new();
    for path in paths {
        names.extend(path.as_os_str().encode_wide());
        names.push(0);
    }
    names.push(0);
    let header = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        pt: POINT::default(),
        fNC: false.into(),
        fWide: true.into(),
    };
    let mut bytes = Vec::with_capacity(size_of::<DROPFILES>() + names.len() * 2);
    // SAFETY: DROPFILES is plain data; its bytes are what the clipboard wants.
    bytes.extend_from_slice(unsafe {
        std::slice::from_raw_parts(
            &header as *const DROPFILES as *const u8,
            size_of::<DROPFILES>(),
        )
    });
    bytes.extend(names.iter().flat_map(|c| c.to_le_bytes()));
    let clipboard = OpenedClipboard::open()?;
    unsafe {
        EmptyClipboard().map_err(|e| format!("clipboard clear: {e}"))?;
    }
    clipboard.set(CF_HDROP.0 as u32, &bytes)
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

/// A text value from the registry; `None` when it is not there.
fn registry_text(
    root: windows::Win32::System::Registry::HKEY,
    key: &str,
    value: &str,
) -> Option<String> {
    use windows::Win32::System::Registry::{RRF_RT_REG_SZ, RegGetValueW};
    let key: Vec<u16> = key.encode_utf16().chain([0]).collect();
    let value: Vec<u16> = value.encode_utf16().chain([0]).collect();
    let mut buffer = [0u16; 512];
    let mut size = (buffer.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            root,
            windows::core::PCWSTR(key.as_ptr()),
            windows::core::PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    let n = (size as usize / 2).saturating_sub(1).min(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..n]))
}

/// A number from the registry; `None` when it is not there.
fn registry_number(
    root: windows::Win32::System::Registry::HKEY,
    key: &str,
    value: &str,
) -> Option<u32> {
    use windows::Win32::System::Registry::{RRF_RT_REG_DWORD, RegGetValueW};
    let key: Vec<u16> = key.encode_utf16().chain([0]).collect();
    let value: Vec<u16> = value.encode_utf16().chain([0]).collect();
    let mut number = 0u32;
    let mut size = 4u32;
    unsafe {
        RegGetValueW(
            root,
            windows::core::PCWSTR(key.as_ptr()),
            windows::core::PCWSTR(value.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut number as *mut u32 as *mut c_void),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    Some(number)
}

/// The Windows version, as people know it: "Windows 11 24H2 (build 26100.2605)".
pub fn windows_version() -> String {
    use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    let key = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let build: u32 = registry_text(HKEY_LOCAL_MACHINE, key, "CurrentBuild")
        .and_then(|b| b.parse().ok())
        .unwrap_or(0);
    let update = registry_number(HKEY_LOCAL_MACHINE, key, "UBR").unwrap_or(0);
    let release = registry_text(HKEY_LOCAL_MACHINE, key, "DisplayVersion").unwrap_or_default();
    // The product name says "Windows 10" on Windows 11 as well.
    let name = if build >= 22000 {
        "Windows 11"
    } else {
        "Windows 10"
    };
    format!("{name} {release} (build {build}.{update})")
}

/// The Windows build number.
pub fn windows_build() -> u32 {
    use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    registry_text(
        HKEY_LOCAL_MACHINE,
        r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
        "CurrentBuild",
    )
    .and_then(|b| b.parse().ok())
    .unwrap_or(0)
}

/// Whether Windows itself takes the PrintScreen key to open its Snipping
/// Tool ("Use the Print screen key to open screen capture"). Windows 11 has
/// it on unless it was turned off; older Windows only when it was turned on.
pub fn windows_takes_printscreen() -> bool {
    use windows::Win32::System::Registry::HKEY_CURRENT_USER;
    match registry_number(
        HKEY_CURRENT_USER,
        r"Control Panel\Keyboard",
        "PrintScreenKeyForSnippingEnabled",
    ) {
        Some(value) => value != 0,
        None => windows_build() >= 22621,
    }
}

/// Memory in the machine, in bytes.
pub fn installed_memory() -> u64 {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    unsafe { GlobalMemoryStatusEx(&mut status) }
        .map(|()| status.ullTotalPhys)
        .unwrap_or(0)
}

/// How the machine is powered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    Mains,
    Battery,
    /// On battery with the battery saver on.
    Saving,
}

pub fn power() -> Power {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut status = SYSTEM_POWER_STATUS::default();
    if unsafe { GetSystemPowerStatus(&mut status) }.is_err() {
        return Power::Mains;
    }
    match (status.ACLineStatus, status.SystemStatusFlag) {
        (_, 1) => Power::Saving,
        (0, _) => Power::Battery,
        _ => Power::Mains,
    }
}

/// What Windows says about the window in front, as a number: 1 away, 2 full
/// screen, 3 Direct3D exclusive full screen, 4 presenting, 5 normal.
pub fn attention_state() -> i32 {
    use windows::Win32::UI::Shell::SHQueryUserNotificationState;
    unsafe { SHQueryUserNotificationState() }.map_or(0, |state| state.0)
}

/// Whether the program of window `window` runs as administrator.
pub fn window_is_elevated(window: isize) -> bool {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(HWND(window as *mut c_void), Some(&mut pid)) };
    pid != 0 && is_elevated(pid)
}

/// The other Bruma processes running, each with whether it runs as
/// administrator.
pub fn other_brumas() -> Vec<(u32, bool)> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::GetCurrentProcessId;
    let me = unsafe { GetCurrentProcessId() };
    let mut found = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return found;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            let n = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
            let name = String::from_utf16_lossy(&entry.szExeFile[..n]).to_lowercase();
            if name == "bruma.exe" && entry.th32ProcessID != me {
                found.push((entry.th32ProcessID, is_elevated(entry.th32ProcessID)));
            }
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    found
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

// ---- for Bruma's own windows --------------------------------------------------

/// The part of the monitor at `point` that windows may use, without the
/// taskbar: x, y, width, height.
pub fn work_area(point: (i32, i32)) -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    };
    unsafe {
        let monitor = MonitorFromPoint(
            POINT {
                x: point.0,
                y: point.1,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
        let r = info.rcWork;
        Some((r.left, r.top, r.right - r.left, r.bottom - r.top))
    }
}

/// The window in front, the one the keyboard goes to.
pub fn foreground_window() -> isize {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe { GetForegroundWindow().0 as isize }
}

/// The key the keyboard hook watches for: its virtual-key code in the low
/// 16 bits and [`MOD_CTRL`]-style modifier bits above; 0 for none.
static WATCHED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Whether the watched key is down, so holding it takes one capture.
static WATCHED_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// What a press of the watched key does.
static ON_WATCHED: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();

pub const MOD_CTRL: u32 = 1;
pub const MOD_SHIFT: u32 = 2;
pub const MOD_ALT: u32 = 4;
pub const MOD_WIN: u32 = 8;

/// The modifier keys held right now, as `MOD_*` bits.
fn modifiers_held() -> u32 {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    };
    let held = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| unsafe {
        GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0
    };
    let mut mods = 0;
    if held(VK_CONTROL) {
        mods |= MOD_CTRL;
    }
    if held(VK_SHIFT) {
        mods |= MOD_SHIFT;
    }
    if held(VK_MENU) {
        mods |= MOD_ALT;
    }
    if held(VK_LWIN) || held(VK_RWIN) {
        mods |= MOD_WIN;
    }
    mods
}

unsafe extern "system" fn keyboard_hook(
    code: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use std::sync::atomic::Ordering::Relaxed;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, HC_ACTION, KBDLLHOOKSTRUCT, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
        WM_SYSKEYUP,
    };
    let watched = WATCHED.load(Relaxed);
    if code == HC_ACTION as i32 && watched != 0 {
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if info.vkCode == watched & 0xFFFF {
            let message = wparam.0 as u32;
            if message == WM_KEYDOWN || message == WM_SYSKEYDOWN {
                if modifiers_held() == watched >> 16 {
                    if !WATCHED_DOWN.swap(true, Relaxed) {
                        if let Some(on) = ON_WATCHED.get() {
                            on();
                        }
                    }
                    // Bruma's key: the program in front does not get it.
                    return windows::Win32::Foundation::LRESULT(1);
                }
            } else if (message == WM_KEYUP || message == WM_SYSKEYUP)
                && WATCHED_DOWN.swap(false, Relaxed)
            {
                return windows::Win32::Foundation::LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Whether process `pid` runs as administrator.
fn is_elevated(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut token = HANDLE::default();
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let ok = OpenProcessToken(process, TOKEN_QUERY, &mut token).is_ok()
            && GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elevation as *mut TOKEN_ELEVATION as *mut c_void),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut size,
            )
            .is_ok();
        if !token.is_invalid() {
            let _ = CloseHandle(token);
        }
        let _ = CloseHandle(process);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Whether the program behind the window in front runs as administrator;
/// Windows then keeps its keys from Bruma's hook and hotkey. `cache` holds
/// the last answer, by process.
fn front_is_elevated(cache: &mut (u32, bool)) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid)) };
    if pid == 0 {
        return false;
    }
    if cache.0 != pid {
        *cache = (pid, is_elevated(pid));
    }
    cache.1
}

/// While a program run as administrator is in front, Windows gives its
/// keys to no hook or hotkey of Bruma's; the key's state can still be asked
/// for. So then, and only then, the watched key is looked at every 15 ms.
fn poll_watched_key(down: &mut bool) {
    use std::sync::atomic::Ordering::Relaxed;
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    let watched = WATCHED.load(Relaxed);
    if watched == 0 {
        *down = false;
        return;
    }
    let pressed = unsafe { GetAsyncKeyState((watched & 0xFFFF) as i32) } as u16 & 0x8000 != 0;
    if pressed && !*down && modifiers_held() == watched >> 16 {
        if let Some(on) = ON_WATCHED.get() {
            on();
        }
    }
    *down = pressed;
}

/// Watches the keyboard for the hotkey as well, from a thread of its own.
///
/// Registering a hotkey is not enough in front of many games: a program
/// reading the keyboard directly can tell Windows to hold back other
/// programs' hotkeys while it is in front, and PrintScreen then never
/// arrives. A keyboard hook sees the key all the same. The hook only looks
/// at the one key and passes everything else straight on; its thread does
/// nothing else and goes ahead of other work, so typing never waits for it.
/// `false` when the hook could not be set.
pub fn watch_for_hotkey(on_press: impl Fn() + Send + Sync + 'static) -> bool {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, HHOOK, KillTimer, MSG, SetTimer, SetWindowsHookExW,
        TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_TIMER,
    };
    if ON_WATCHED.set(Box::new(on_press)).is_err() {
        return true;
    }
    let (told, started) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("bruma-keys".into())
        .spawn(move || {
            run_this_thread_first();
            let module = unsafe { GetModuleHandleW(None) }.ok().map(Into::into);
            let hook_up =
                || unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), module, 0) };
            // Once now, to know that it can be done.
            let mut hook: Option<HHOOK> = match hook_up() {
                Ok(hook) => Some(hook),
                Err(e) => {
                    eprintln!("bruma: no keyboard hook: {e}");
                    let _ = told.send(false);
                    return;
                }
            };
            let _ = told.send(true);
            // Twice a second: is the hook needed (a game in front, or
            // Windows wanting PrintScreen), and is a program run as
            // administrator in front? While one is, the key is looked at
            // every 15 ms. Run as administrator itself, Bruma sees every
            // key anyway.
            let watch_front = !is_elevated(unsafe { GetCurrentProcessId() });
            let slow = unsafe { SetTimer(None, 0, 500, None) };
            let mut fast = 0usize;
            let mut front = (0u32, false);
            let mut down = false;
            let mut message = MSG::default();
            unsafe {
                while GetMessageW(&mut message, None, 0, 0).as_bool() {
                    if message.message == WM_TIMER && message.hwnd.is_invalid() {
                        if message.wParam.0 == slow {
                            let needed = HOOK_ALWAYS.load(std::sync::atomic::Ordering::Relaxed)
                                || attention() == Attention::FullScreen;
                            match (&hook, needed) {
                                (None, true) => hook = hook_up().ok(),
                                (Some(h), false) => {
                                    let _ = UnhookWindowsHookEx(*h);
                                    hook = None;
                                    WATCHED_DOWN.store(false, std::sync::atomic::Ordering::Relaxed);
                                }
                                _ => {}
                            }
                            let elevated = watch_front && front_is_elevated(&mut front);
                            if elevated && fast == 0 {
                                fast = SetTimer(None, 0, 15, None);
                            } else if !elevated && fast != 0 {
                                let _ = KillTimer(None, fast);
                                fast = 0;
                                down = false;
                            }
                        } else if message.wParam.0 == fast {
                            poll_watched_key(&mut down);
                        }
                        continue;
                    }
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        });
    spawned.is_ok() && started.recv().unwrap_or(false)
}

/// Whether the keyboard hook stays on all the time, rather than only while a
/// game or a film fills the screen: when Windows would otherwise take the
/// hotkey (PrintScreen for its Snipping Tool).
static HOOK_ALWAYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// See [`HOOK_ALWAYS`].
pub fn keep_keyboard_hook(always: bool) {
    HOOK_ALWAYS.store(always, std::sync::atomic::Ordering::Relaxed);
}

/// Sets the key the hook watches for: a virtual-key code and `MOD_*` bits;
/// `None` to watch for nothing (while the settings listen for a new key).
pub fn set_watched_hotkey(key: Option<(u16, u32)>) {
    let value = key.map_or(0, |(vk, mods)| vk as u32 | mods << 16);
    WATCHED.store(value, std::sync::atomic::Ordering::Relaxed);
    WATCHED_DOWN.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Plays the shutter sound without waiting for it.
pub fn play_shutter() {
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
    // Kept for good: the sound plays from it after this returns.
    static SOUND: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let wav = SOUND.get_or_init(bruma_core::sound::shutter);
    unsafe {
        let _ = PlaySoundW(
            windows::core::PCWSTR(wav.as_ptr() as *const u16),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}

/// The middle of a window, in desktop coordinates.
pub fn window_centre(window: isize) -> Option<(i32, i32)> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(HWND(window as *mut c_void), &mut rect) }.ok()?;
    Some(((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2))
}

/// `path` as Bruma can reach it. A drive letter mapped to a network folder
/// belongs to the person's ordinary sign-in only: run as administrator,
/// Bruma does not see it, so the network folder it stands for is used.
pub fn reachable(path: &std::path::Path) -> std::path::PathBuf {
    use windows::Win32::System::Registry::HKEY_CURRENT_USER;
    let text = path.to_string_lossy();
    let mut chars = text.chars();
    let (Some(letter), Some(':')) = (chars.next(), chars.next()) else {
        return path.to_path_buf();
    };
    if !letter.is_ascii_alphabetic() || std::path::Path::new(&format!("{letter}:\\")).exists() {
        return path.to_path_buf();
    }
    match registry_text(
        HKEY_CURRENT_USER,
        &format!(r"Network\{letter}"),
        "RemotePath",
    ) {
        Some(remote) => std::path::PathBuf::from(on_share(&remote, &text[2..])),
        None => path.to_path_buf(),
    }
}

/// `rest` of a path (after its drive letter) on the network folder `remote`.
fn on_share(remote: &str, rest: &str) -> String {
    format!(
        "{}\\{}",
        remote.trim_end_matches('\\'),
        rest.trim_start_matches(['\\', '/'])
    )
    .trim_end_matches('\\')
    .to_string()
}

/// Whether files can be made in `folder` (made first if need be); why not
/// otherwise.
pub fn writable_folder(folder: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    let probe = folder.join(format!(".bruma-{}", std::process::id()));
    std::fs::write(&probe, b"").map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// For the trace: what kind of window the one in front is, and where.
pub fn describe_window(window: isize) -> String {
    let hwnd = HWND(window as *mut c_void);
    let mut class = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut class) } as usize;
    let mut rect = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
    let ex = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    format!(
        "class {}, pid {pid}, at {},{} size {}x{}, ex style {ex:08X}",
        String::from_utf16_lossy(&class[..n]),
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top
    )
}

/// Asks for a folder with the system's own dialog, starting in `start`.
/// `None` when the person cancels.
pub fn pick_folder(owner: Option<isize>, start: &std::path::Path) -> Option<std::path::PathBuf> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoTaskMemFree,
    };
    use windows::Win32::UI::Shell::{
        FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem,
        SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
    };
    use windows::core::HSTRING;
    unsafe {
        // Already set up on this thread by the window library; harmless if so.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog
            .SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
            .ok()?;
        let _ = std::fs::create_dir_all(start);
        if let Ok(folder) =
            SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(start.as_os_str()), None)
        {
            let _ = dialog.SetFolder(&folder);
        }
        let owner = owner.map(|h| HWND(h as *mut c_void));
        dialog.Show(owner).ok()?;
        let chosen = dialog.GetResult().ok()?;
        let name = chosen.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const c_void));
        path.map(std::path::PathBuf::from)
    }
}

const SETTINGS_REQUEST: windows::core::PCWSTR = w!("Local\\bruma-settings-request");

/// Asks the background process to show its settings. `false` when there is
/// none.
pub fn request_settings() -> bool {
    signal(SETTINGS_REQUEST)
}

/// Background side of [`request_settings`].
pub fn listen_for_settings_requests(on_request: impl Fn() + Send + 'static) -> bool {
    listen(SETTINGS_REQUEST, "bruma-settings-requests", on_request)
}

// ---- running as administrator, for games that do --------------------------------

/// The task Windows' Task Scheduler starts Bruma with when it is to run as
/// administrator. Made once, with the person's consent, it starts Bruma
/// that way without asking each time. One for each person using the
/// computer, so that one person's task does not replace another's.
fn games_task() -> String {
    let user: String = std::env::var("USERNAME")
        .unwrap_or_default()
        .chars()
        .filter(|c| !"\\/:*?\"<>|".contains(*c))
        .collect();
    if user.is_empty() {
        "Bruma".into()
    } else {
        format!("Bruma - {user}")
    }
}

/// The task's name in 0.7.2, one for the whole computer.
const OLD_GAMES_TASK: &str = "Bruma";

/// The name a task for running as administrator has, for `bruma doctor`.
pub fn games_task_name() -> String {
    games_task()
}

/// Whether this Bruma runs as administrator.
pub fn running_as_administrator() -> bool {
    use windows::Win32::System::Threading::GetCurrentProcessId;
    is_elevated(unsafe { GetCurrentProcessId() })
}

/// Runs `schtasks` with `args`, quietly; whether it succeeded.
fn schtasks(args: &[&str]) -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("schtasks")
        .args(args)
        .creation_flags(0x0800_0000)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether a task that starts Bruma as administrator is there and starts
/// `exe`: a task left by a copy elsewhere would start that one, or nothing.
pub fn games_task_starts(exe: &std::path::Path) -> bool {
    games_task_for(exe).is_some()
}

/// The name of the task that starts `exe` as administrator, if there is one.
fn games_task_for(exe: &std::path::Path) -> Option<String> {
    [games_task(), OLD_GAMES_TASK.to_string()]
        .into_iter()
        .find(|name| task_starts(name, exe))
}

/// Whether the task `name` is there and starts `exe`.
fn task_starts(name: &str, exe: &std::path::Path) -> bool {
    use std::os::windows::process::CommandExt;
    let Ok(out) = std::process::Command::new("schtasks")
        .args(["/Query", "/TN", name, "/XML"])
        .creation_flags(0x0800_0000)
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let xml = String::from_utf8_lossy(&out.stdout).to_lowercase();
    let wanted = exe
        .display()
        .to_string()
        .to_lowercase()
        .replace('&', "&amp;");
    xml.contains(&format!("<command>{wanted}</command>"))
}

/// Starts Bruma as administrator through its task; no question is asked.
pub fn start_games_task() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| games_task_for(&exe))
        .is_some_and(|name| schtasks(&["/Run", "/TN", &name]))
}

/// Makes the task that starts `exe` as administrator. Needs to run as
/// administrator itself.
pub fn create_games_task(exe: &std::path::Path) -> Result<(), String> {
    // Escaped for XML.
    let command = exe
        .display()
        .to_string()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    // Started on request only; at the usual priority (the scheduler's own
    // default is below it); never stopped after a while or on battery.
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Starts Bruma with administrator rights, so that its hotkey also works over games that run that way.</Description></RegistrationInfo>
  <Principals><Principal id="Author"><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
  </Settings>
  <Actions Context="Author"><Exec><Command>{command}</Command><Arguments>takeover</Arguments></Exec></Actions>
</Task>
"#
    );
    let file = std::env::temp_dir().join(format!("bruma-task-{}.xml", std::process::id()));
    // The scheduler reads the file as UTF-16, as its first line says.
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&file, bytes).map_err(|e| e.to_string())?;
    let file_arg = file.display().to_string();
    let made = schtasks(&["/Create", "/TN", &games_task(), "/XML", &file_arg, "/F"]);
    let _ = std::fs::remove_file(&file);
    if made {
        Ok(())
    } else {
        Err("the task could not be made".into())
    }
}

/// Removes the task, and the one 0.7.2 made if it starts this Bruma. Needs
/// to run as administrator.
pub fn delete_games_task() -> bool {
    if let Ok(exe) = std::env::current_exe() {
        if task_starts(OLD_GAMES_TASK, &exe) {
            schtasks(&["/Delete", "/TN", OLD_GAMES_TASK, "/F"]);
        }
    }
    schtasks(&["/Delete", "/TN", &games_task(), "/F"])
}

/// Runs this program again as administrator with `args`, after Windows has
/// asked the person, and waits for it. `Ok(true)` when it ran and
/// succeeded, `Ok(false)` when it failed, `Err` when the person said no or
/// it could not be started.
pub fn run_as_administrator(args: &str) -> Result<bool, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::System::Threading::GetExitCodeProcess;
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let file: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
    let params: Vec<u16> = args.encode_utf16().chain([0]).collect();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: windows::core::PCWSTR(file.as_ptr()),
        lpParameters: windows::core::PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).map_err(|_| "Windows did not allow it".to_string())?;
        if info.hProcess.is_invalid() {
            return Err("it did not start".into());
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        if code == 2 {
            return Err("Windows asked for another account. Bruma can run as administrator only                  when your own account is an administrator"
                .into());
        }
        Ok(code == 0)
    }
}

// ---- the icon in the notification area ---------------------------------------

/// A click on the tray icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayClick {
    /// A left click, or Enter on the icon.
    Left,
    /// A right click, or the menu key on the icon, at this place on the screen.
    Right((i32, i32)),
}

/// The message the shell sends the icon's window about the icon.
const TRAY_MESSAGE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;

struct TrayState {
    on_click: Box<dyn Fn(TrayClick)>,
    data: windows::Win32::UI::Shell::NOTIFYICONDATAW,
    /// Sent to every window when Explorer (re)starts: the icon must be added
    /// again then.
    taskbar_created: u32,
}

thread_local! {
    static TRAY: std::cell::RefCell<Option<TrayState>> = const { std::cell::RefCell::new(None) };
}

/// The size Windows draws notification icons at, in pixels.
pub fn small_icon_size() -> u32 {
    use windows::Win32::UI::WindowsAndMessaging::SM_CXSMICON;
    unsafe { GetSystemMetrics(SM_CXSMICON).clamp(16, 64) as u32 }
}

/// An icon from straight RGBA pixels, `size` by `size`.
fn icon_from_rgba(
    rgba: &[u8],
    size: u32,
) -> Result<windows::Win32::UI::WindowsAndMessaging::HICON, String> {
    use windows::Win32::Graphics::Gdi::CreateBitmap;
    use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO};
    if rgba.len() != (size * size * 4) as usize {
        return Err("the icon has the wrong size".into());
    }
    unsafe {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                biHeight: -(size as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|e| e.to_string())?;
        if bits.is_null() {
            let _ = DeleteObject(color.into());
            return Err("could not make the icon".into());
        }
        let out = std::slice::from_raw_parts_mut(bits as *mut u8, rgba.len());
        for (o, p) in out.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
            o.copy_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
        // An empty mask: the alpha in the colour bitmap decides.
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, None);
        let icon = CreateIconIndirect(&ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        });
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon.map_err(|e| e.to_string())
    }
}

thread_local! {
    /// What a hotkey press does, for [`answer_hotkeys_directly`].
    static ON_HOTKEY: std::cell::RefCell<Option<Box<dyn Fn()>>> = const { std::cell::RefCell::new(None) };
}

/// The window procedure the hotkey library gave its window, called for
/// everything but the hotkey itself.
static HOTKEY_PROC: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

unsafe extern "system" fn hotkey_proc(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{CallWindowProcW, WM_HOTKEY, WNDPROC};
    if msg == WM_HOTKEY {
        let answered = ON_HOTKEY.with(|on| on.borrow().as_ref().map(|on| on()).is_some());
        if answered {
            return windows::Win32::Foundation::LRESULT(0);
        }
    }
    let previous = HOTKEY_PROC.load(std::sync::atomic::Ordering::Relaxed);
    unsafe {
        let previous: WNDPROC = std::mem::transmute(previous);
        CallWindowProcW(previous, hwnd, msg, wparam, lparam)
    }
}

/// Answers the hotkeys registered by the hotkey library on this thread
/// directly, with `on_hotkey`, instead of through the library. The library
/// starts a thread on every press to watch for the key's release, which
/// Bruma does not use; on a machine kept busy by a game, showing the
/// capture was seen to wait up to two seconds behind that thread.
pub fn answer_hotkeys_directly(on_hotkey: impl Fn() + 'static) -> bool {
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumThreadWindows, GWLP_WNDPROC, SetWindowLongPtrW,
    };
    unsafe extern "system" fn find(hwnd: HWND, found: LPARAM) -> BOOL {
        let mut class = [0u16; 32];
        let n = unsafe { GetClassNameW(hwnd, &mut class) } as usize;
        if String::from_utf16_lossy(&class[..n]) == "global_hotkey_app" {
            unsafe { *(found.0 as *mut HWND) = hwnd };
            return false.into();
        }
        true.into()
    }
    let mut window = HWND::default();
    unsafe {
        let _ = EnumThreadWindows(
            GetCurrentThreadId(),
            Some(find),
            LPARAM(&mut window as *mut HWND as isize),
        );
    }
    if window.is_invalid() || HOTKEY_PROC.load(std::sync::atomic::Ordering::Relaxed) != 0 {
        return false;
    }
    ON_HOTKEY.with(|on| *on.borrow_mut() = Some(Box::new(on_hotkey)));
    let previous = unsafe {
        SetWindowLongPtrW(
            window,
            GWLP_WNDPROC,
            hotkey_proc as *const () as usize as isize,
        )
    };
    if previous == 0 {
        ON_HOTKEY.with(|on| *on.borrow_mut() = None);
        return false;
    }
    HOTKEY_PROC.store(previous, std::sync::atomic::Ordering::Relaxed);
    true
}

thread_local! {
    static OWNER: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

/// A window that is never shown, to own Bruma's other windows. An owned
/// window has no taskbar button; one with a button makes Windows wait for
/// the taskbar when it is shown and given the keyboard, and on a machine
/// kept busy by a game that wait was seen to reach two seconds. Made on
/// first use, on the thread that runs the window loop.
pub fn owner_window() -> isize {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, RegisterClassW, WINDOW_EX_STYLE, WNDCLASSW, WS_POPUP,
    };
    if let Some(owner) = OWNER.with(|o| Some(o.get()).filter(|&h| h != 0)) {
        return owner;
    }
    unsafe extern "system" fn plain(
        hwnd: HWND,
        msg: u32,
        wparam: windows::Win32::Foundation::WPARAM,
        lparam: LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }
    let owner = unsafe {
        let Ok(instance) = GetModuleHandleW(None) else {
            return 0;
        };
        let class = w!("bruma_owner");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(plain),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        });
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("bruma owner"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map(|h| h.0 as isize)
        .unwrap_or(0)
    };
    OWNER.with(|o| o.set(owner));
    owner
}

/// The icon's lasting name. With it, an icon left behind by a Bruma that was
/// ended without closing (killed, or stopped by an update) is replaced
/// instead of piling up next to the new one.
const TRAY_GUID: windows::core::GUID =
    windows::core::GUID::from_u128(0x6fa6fde6_286d_488c_9d76_5ffe633bfe59);

/// Puts the icon in the notification area and asks for the shell's newer,
/// simpler way of reporting clicks. An icon of an earlier Bruma still there
/// is taken away first. Windows ties the lasting name to the program's path,
/// so a copy run from somewhere else falls back to an icon without it.
fn show_tray_icon(data: &mut windows::Win32::UI::Shell::NOTIFYICONDATAW) -> bool {
    use windows::Win32::UI::Shell::{
        NIF_GUID, NIM_ADD, NIM_DELETE, NIM_SETVERSION, Shell_NotifyIconW,
    };
    let add = |data: &windows::Win32::UI::Shell::NOTIFYICONDATAW| unsafe {
        Shell_NotifyIconW(NIM_ADD, data).as_bool()
            && Shell_NotifyIconW(NIM_SETVERSION, data).as_bool()
    };
    if data.uFlags.0 & NIF_GUID.0 != 0 {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, data);
        }
        if add(data) {
            return true;
        }
        data.uFlags.0 &= !NIF_GUID.0;
        data.guidItem = windows::core::GUID::zeroed();
    }
    add(data)
}

unsafe extern "system" fn tray_proc(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::Shell::NIN_SELECT;
    use windows::Win32::UI::WindowsAndMessaging::{DefWindowProcW, WM_CONTEXTMENU};
    if msg == TRAY_MESSAGE {
        // The newer reporting: the event in the low word, and where it
        // happened in `wparam`.
        let event = lparam.0 as u32 & 0xFFFF;
        let x = (wparam.0 & 0xFFFF) as u16 as i16 as i32;
        let y = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
        let click = match event {
            e if e == NIN_SELECT || e == NIN_SELECT + 1 => Some(TrayClick::Left),
            WM_CONTEXTMENU => Some(TrayClick::Right((x, y))),
            _ => None,
        };
        if let Some(click) = click {
            TRAY.with(|tray| {
                if let Some(state) = &*tray.borrow() {
                    (state.on_click)(click);
                }
            });
        }
        return windows::Win32::Foundation::LRESULT(0);
    }
    let again = TRAY.with(|tray| {
        tray.borrow()
            .as_ref()
            .filter(|state| state.taskbar_created != 0 && msg == state.taskbar_created)
            .map(|state| state.data)
    });
    if let Some(mut data) = again {
        show_tray_icon(&mut data);
        TRAY.with(|tray| {
            if let Some(state) = tray.borrow_mut().as_mut() {
                state.data = data;
            }
        });
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// The icon in the notification area. Only one at a time, on the thread
/// that runs the window loop; it goes away when dropped.
pub struct NotifyIcon {
    hwnd: HWND,
    icon: windows::Win32::UI::WindowsAndMessaging::HICON,
}

impl NotifyIcon {
    pub fn new(
        rgba: &[u8],
        size: u32,
        tip: &str,
        on_click: impl Fn(TrayClick) + 'static,
    ) -> Result<Self, String> {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::Shell::{
            NIF_GUID, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NOTIFYICON_VERSION_4,
            NOTIFYICONDATAW,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, RegisterClassW, RegisterWindowMessageW, WINDOW_EX_STYLE, WNDCLASSW,
            WS_POPUP,
        };
        unsafe {
            let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            let class = w!("bruma_tray");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(tray_proc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            // Fails harmlessly if the class is already there.
            RegisterClassW(&wc);
            // A real (hidden) window rather than a message-only one: those
            // do not hear that Explorer has restarted.
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("bruma tray"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .map_err(|e| e.to_string())?;
            let icon = icon_from_rgba(rgba, size)?;
            let mut data = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP | NIF_GUID,
                uCallbackMessage: TRAY_MESSAGE,
                hIcon: icon,
                guidItem: TRAY_GUID,
                ..Default::default()
            };
            for (slot, c) in data.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
                *slot = c;
            }
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
            // Early at sign-in the taskbar may not be there yet; the icon is
            // added again when Explorer says it has started.
            show_tray_icon(&mut data);
            TRAY.with(|tray| {
                *tray.borrow_mut() = Some(TrayState {
                    on_click: Box::new(on_click),
                    data,
                    taskbar_created,
                });
            });
            Ok(Self { hwnd, icon })
        }
    }
}

impl Drop for NotifyIcon {
    fn drop(&mut self) {
        use windows::Win32::UI::Shell::{NIM_DELETE, Shell_NotifyIconW};
        use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, DestroyWindow};
        let data = TRAY.with(|tray| tray.borrow_mut().take().map(|state| state.data));
        unsafe {
            if let Some(data) = data {
                let _ = Shell_NotifyIconW(NIM_DELETE, &data);
            }
            let _ = DestroyIcon(self.icon);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_on_a_mapped_drive_becomes_the_network_folder() {
        assert_eq!(on_share(r"\server\pics", r"\bruma"), r"\server\pics\bruma");
        assert_eq!(on_share(r"\server\pics\", r"\"), r"\server\pics");
        assert_eq!(on_share(r"\server\pics", ""), r"\server\pics");
    }
}
