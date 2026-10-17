//! Notification-area (system tray) icon with a tooltip and an Open / Quit menu.
//!
//! The icon lives on its own thread with its own message loop, so it keeps working while the
//! main window is hidden and nothing depends on the GUI's frame loop. The thread reports
//! clicks through a callback and is stopped (icon removed, window destroyed, thread joined)
//! when the [`Tray`] is dropped.

use crate::error::{CoreError, Result};
use crate::strings::{tr, Lang};
use crate::util::wide;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use tracing::{info, warn};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, NOTIFY_ICON_MESSAGE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIcon, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW,
    GetWindowLongPtrW, IsWindowVisible, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, SetWindowLongPtrW, TrackPopupMenu,
    TranslateMessage, UnregisterClassW, GWLP_USERDATA, HICON, HMENU, MF_SEPARATOR, MF_STRING, MSG,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CONTEXTMENU,
    WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
};

/// Longest tooltip Windows shows (the buffer holds 128 UTF-16 units including the NUL).
pub const TIP_MAX: usize = 127;

const WM_TRAY: u32 = WM_APP + 1;
const WM_TRAY_TIP: u32 = WM_APP + 2;
const WM_TRAY_STOP: u32 = WM_APP + 3;
/// Self-test hook: `wparam` 1 = Open, 2 = Quit, run through the normal callback.
const WM_TRAY_SIMULATE: u32 = WM_APP + 4;
const ID_OPEN: usize = 1;
const ID_QUIT: usize = 2;
const ICON_ID: u32 = 1;
const ICON_SIZE: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Show the main window (left click or the menu item).
    Open,
    /// Quit the program (menu item).
    Quit,
}

/// `Rustacer - CPU 12% - RAM 48% (7.6 GiB / 16.0 GiB)`, cut to fit a tray tooltip.
pub fn format_tooltip(cpu: f32, ram_used: u64, ram_total: u64) -> String {
    let ram_pct = crate::metrics::percent(ram_used, ram_total);
    let text = format!(
        "Rustacer - CPU {:.0}% - RAM {:.0}% ({} / {})",
        cpu.clamp(0.0, 100.0),
        ram_pct.clamp(0.0, 100.0),
        crate::util::format_bytes(ram_used),
        crate::util::format_bytes(ram_total)
    );
    truncate_utf16(&text, TIP_MAX)
}

fn truncate_utf16(s: &str, max_units: usize) -> String {
    let mut out = String::new();
    let mut units = 0;
    for c in s.chars() {
        let n = c.len_utf16();
        if units + n > max_units {
            break;
        }
        units += n;
        out.push(c);
    }
    out
}

/// A `size` x `size` BGRA (top-down) icon: an orange ring with a centre dot, anti-aliased
/// and symmetric so it reads the same whichever row order the API assumes.
pub fn icon_pixels(size: usize) -> Vec<u8> {
    let mut px = vec![0u8; size * size * 4];
    let c = (size as f32 - 1.0) / 2.0;
    let r_out = size as f32 * 0.47;
    let r_in = size as f32 * 0.30;
    let r_dot = size as f32 * 0.14;
    // Coverage of a 1px-soft disc edge.
    let cover = |d: f32, r: f32| (r - d + 0.5).clamp(0.0, 1.0);
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let ring = cover(d, r_out) * (1.0 - cover(d, r_in));
            let dot = cover(d, r_dot);
            let a = (ring + dot).clamp(0.0, 1.0);
            let i = (y * size + x) * 4;
            // Orange #E8590C as BGRA, with straight alpha.
            px[i] = 0x0C;
            px[i + 1] = 0x59;
            px[i + 2] = 0xE8;
            px[i + 3] = (a * 255.0).round() as u8;
        }
    }
    px
}

/// Is the top-level window with this exact title currently visible? `None` if there is none.
/// Used by the self-test to check that the window really hides to the tray and comes back.
pub fn window_visible(title: &str) -> Option<bool> {
    let t = wide(title);
    // SAFETY: `t` is a live NUL-terminated buffer; FindWindowW only reads it.
    let hwnd = unsafe { FindWindowW(PCWSTR::null(), PCWSTR::from_raw(t.as_ptr())) }.ok()?;
    // SAFETY: `hwnd` was just returned by FindWindowW; a stale handle only returns FALSE.
    Some(unsafe { IsWindowVisible(hwnd) }.as_bool())
}

/// If another Rustacer is already running with a tray icon, ask it to show its window and
/// return true, so this launch can exit instead of adding a second icon. Returns false when
/// there is none, or when it cannot be reached (for example it runs elevated and this copy
/// does not).
pub fn signal_running_instance() -> bool {
    // SAFETY: FindWindowW only reads the static class name.
    let Ok(hwnd) = (unsafe { FindWindowW(CLASS, PCWSTR::null()) }) else {
        return false;
    };
    // SAFETY: posting a message to a window of another process; a stale handle only fails.
    unsafe { PostMessageW(Some(hwnd), WM_TRAY_SIMULATE, WPARAM(1), LPARAM(0)) }.is_ok()
}

struct IconGuard(HICON);

impl Drop for IconGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the icon came from CreateIcon and is destroyed exactly once, here.
            unsafe {
                let _ = DestroyIcon(self.0);
            }
        }
    }
}

struct MenuGuard(HMENU);

impl Drop for MenuGuard {
    fn drop(&mut self) {
        // SAFETY: the menu came from CreatePopupMenu and is destroyed exactly once, here.
        unsafe {
            let _ = DestroyMenu(self.0);
        }
    }
}

fn make_icon() -> Result<IconGuard> {
    let xor = icon_pixels(ICON_SIZE);
    // All-zero AND mask: fully opaque shape, with the alpha channel doing the cut-out.
    let and = vec![0u8; ICON_SIZE.div_ceil(16) * 2 * ICON_SIZE];
    // SAFETY: both buffers are large enough for a 32x32 1-plane image (32 bpp XOR, 1 bpp AND).
    let h = unsafe {
        CreateIcon(
            None,
            ICON_SIZE as i32,
            ICON_SIZE as i32,
            1,
            32,
            and.as_ptr(),
            xor.as_ptr(),
        )
    }?;
    Ok(IconGuard(h))
}

struct WndState {
    on_event: Box<dyn Fn(TrayEvent) + Send>,
    tip: Arc<Mutex<String>>,
    icon: HICON,
    taskbar_created: u32,
    shown: Arc<AtomicBool>,
}

fn notify(msg: NOTIFY_ICON_MESSAGE, hwnd: HWND, icon: HICON, tip: &str) -> bool {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    for (i, u) in truncate_utf16(tip, TIP_MAX).encode_utf16().enumerate() {
        nid.szTip[i] = u;
    }
    // SAFETY: `nid` is fully initialised and lives for the call.
    unsafe { Shell_NotifyIconW(msg, &nid) }.as_bool()
}

fn current_tip(state: &WndState) -> String {
    state
        .tip
        .lock()
        .map(|t| t.clone())
        .unwrap_or_else(|p| p.into_inner().clone())
}

fn fire(state: &WndState, ev: TrayEvent) {
    // A panic must not unwind through the window procedure.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (state.on_event)(ev)));
}

fn context_menu(hwnd: HWND) -> Option<TrayEvent> {
    // SAFETY: plain menu calls; the menu is destroyed by the guard, and the strings are live
    // NUL-terminated buffers for each call.
    unsafe {
        let menu = MenuGuard(CreatePopupMenu().ok()?);
        let open = wide(tr(Lang::English, "tray.open"));
        let quit = wide(tr(Lang::English, "tray.quit"));
        AppendMenuW(menu.0, MF_STRING, ID_OPEN, PCWSTR::from_raw(open.as_ptr())).ok()?;
        AppendMenuW(menu.0, MF_SEPARATOR, 0, PCWSTR::null()).ok()?;
        AppendMenuW(menu.0, MF_STRING, ID_QUIT, PCWSTR::from_raw(quit.as_ptr())).ok()?;
        let mut pt = POINT::default();
        GetCursorPos(&mut pt).ok()?;
        // Required so the menu closes when the user clicks elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu.0,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        match cmd.0 as usize {
            ID_OPEN => Some(TrayEvent::Open),
            ID_QUIT => Some(TrayEvent::Quit),
            _ => None,
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: GWLP_USERDATA is either 0 or the pointer to the WndState that `tray_thread`
    // keeps alive until after the window is destroyed.
    let state = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WndState).as_ref() };
    match msg {
        WM_TRAY => {
            if let Some(s) = state {
                match (lparam.0 as u32) & 0xFFFF {
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK => fire(s, TrayEvent::Open),
                    WM_RBUTTONUP | WM_CONTEXTMENU => {
                        if let Some(ev) = context_menu(hwnd) {
                            fire(s, ev);
                        }
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_TRAY_TIP => {
            if let Some(s) = state {
                notify(NIM_MODIFY, hwnd, s.icon, &current_tip(s));
            }
            LRESULT(0)
        }
        WM_TRAY_SIMULATE => {
            if let Some(s) = state {
                fire(
                    s,
                    if wparam.0 == 2 {
                        TrayEvent::Quit
                    } else {
                        TrayEvent::Open
                    },
                );
            }
            LRESULT(0)
        }
        WM_TRAY_STOP => {
            // SAFETY: destroying our own window from its own thread.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(s) = state {
                s.shown.store(false, Ordering::Relaxed);
                notify(NIM_DELETE, hwnd, s.icon, "");
            }
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        m if state.is_some_and(|s| s.taskbar_created != 0 && m == s.taskbar_created) => {
            // Explorer restarted (or finished starting after login): add the icon again.
            if let Some(s) = state {
                let ok = notify(NIM_ADD, hwnd, s.icon, &current_tip(s));
                s.shown.store(ok, Ordering::Relaxed);
                info!("tray icon re-added after the taskbar was created: {ok}");
            }
            LRESULT(0)
        }
        // SAFETY: default handling of a message we do not process.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

const CLASS: PCWSTR = w!("RustacerTrayWindow");

fn tray_thread(
    tip: Arc<Mutex<String>>,
    shown: Arc<AtomicBool>,
    on_event: Box<dyn Fn(TrayEvent) + Send>,
    ready: Sender<std::result::Result<isize, String>>,
) {
    let setup = || -> Result<(HWND, HINSTANCE, *mut WndState, IconGuard)> {
        let icon = make_icon()?;
        // SAFETY: querying the handle of the running executable.
        let hinst = HINSTANCE(unsafe { GetModuleHandleW(None) }?.0);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst,
            lpszClassName: CLASS,
            ..Default::default()
        };
        // SAFETY: `wc` is fully initialised; the class name is a static string.
        if unsafe { RegisterClassW(&wc) } == 0 {
            // ERROR_CLASS_ALREADY_EXISTS (1410) is fine; anything else is a real failure.
            // SAFETY: reads the calling thread's last error.
            let e = unsafe { GetLastError() };
            if e.0 != 1410 {
                return Err(CoreError::Windows(format!(
                    "Could not register the tray window class (error {})",
                    e.0
                )));
            }
        }
        // SAFETY: a never-shown, zero-size top-level window that only receives messages
        // (a top-level window is needed to receive the "TaskbarCreated" broadcast).
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS,
                w!("Rustacer tray"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinst),
                None,
            )
        }
        .inspect_err(|_| {
            // SAFETY: unregistering the class this thread registered; nothing uses it.
            unsafe {
                let _ = UnregisterClassW(CLASS, Some(hinst));
            }
        })?;
        // SAFETY: registering a message name; returns 0 on failure, which never matches.
        let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        let state = Box::into_raw(Box::new(WndState {
            on_event,
            tip: tip.clone(),
            icon: icon.0,
            taskbar_created,
            shown: shown.clone(),
        }));
        // SAFETY: `state` is a valid heap pointer that outlives the window (freed below,
        // after the message loop ends and the pointer is cleared).
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };
        Ok((hwnd, hinst, state, icon))
    };
    let (hwnd, hinst, state_ptr, _icon) = match setup() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    let first_tip = tip
        .lock()
        .map(|t| t.clone())
        .unwrap_or_else(|p| p.into_inner().clone());
    // SAFETY: `state_ptr` was just created and is only freed at the end of this function.
    let icon = unsafe { (*state_ptr).icon };
    let added = notify(NIM_ADD, hwnd, icon, &first_tip);
    shown.store(added, Ordering::Relaxed);
    if !added {
        warn!("could not add the tray icon yet (explorer may still be starting)");
    }
    let _ = ready.send(Ok(hwnd.0 as isize));

    let mut msg = MSG::default();
    // SAFETY: standard message loop on the thread that created the window.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // The window is gone: detach the state pointer, then free it.
        drop(Box::from_raw(state_ptr));
        let _ = UnregisterClassW(CLASS, Some(hinst));
    }
}

/// The notification-area icon. Dropping it removes the icon and stops its thread.
pub struct Tray {
    hwnd: isize,
    tip: Arc<Mutex<String>>,
    shown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Tray {
    /// Create the icon. `on_event` runs on the tray thread when the user clicks the icon or
    /// picks a menu item; keep it short and non-blocking.
    pub fn start(tooltip: &str, on_event: impl Fn(TrayEvent) + Send + 'static) -> Result<Tray> {
        let tip = Arc::new(Mutex::new(truncate_utf16(tooltip, TIP_MAX)));
        let shown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (t2, s2) = (tip.clone(), shown.clone());
        let thread = std::thread::Builder::new()
            .name("rustacer-tray".into())
            .spawn(move || tray_thread(t2, s2, Box::new(on_event), tx))
            .map_err(CoreError::Io)?;
        match rx.recv() {
            Ok(Ok(hwnd)) => Ok(Tray {
                hwnd,
                tip,
                shown,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(CoreError::Message(format!(
                    "Could not create the tray icon: {e}"
                )))
            }
            Err(_) => Err(CoreError::Message(
                "The tray thread ended before it was ready.".into(),
            )),
        }
    }

    fn post(&self, msg: u32, wparam: usize) {
        // SAFETY: posting to a window owned by the tray thread; a stale handle only fails.
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                msg,
                WPARAM(wparam),
                LPARAM(0),
            );
        }
    }

    /// Replace the tooltip text (cut to [`TIP_MAX`] UTF-16 units).
    pub fn set_tooltip(&self, text: &str) {
        let t = truncate_utf16(text, TIP_MAX);
        let mut g = self.tip.lock().unwrap_or_else(|p| p.into_inner());
        if *g != t {
            *g = t;
            drop(g);
            self.post(WM_TRAY_TIP, 0);
        }
    }

    /// True while the icon is actually present in the notification area.
    pub fn is_shown(&self) -> bool {
        self.shown.load(Ordering::Relaxed)
    }

    /// Run `ev` through the same callback a click would, for self-tests.
    pub fn simulate(&self, ev: TrayEvent) {
        self.post(WM_TRAY_SIMULATE, if ev == TrayEvent::Quit { 2 } else { 1 });
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.post(WM_TRAY_STOP, 0);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_shows_cpu_and_ram() {
        let t = format_tooltip(12.4, 8 * 1024 * 1024 * 1024, 16 * 1024 * 1024 * 1024);
        assert_eq!(t, "Rustacer - CPU 12% - RAM 50% (8.0 GiB / 16.0 GiB)");
    }

    #[test]
    fn tooltip_is_clamped_and_never_too_long() {
        let t = format_tooltip(250.0, 10, 0);
        assert!(t.contains("CPU 100%"));
        assert!(t.contains("RAM 0%"));
        assert!(t.encode_utf16().count() <= TIP_MAX);
        let long = "x".repeat(500);
        assert_eq!(truncate_utf16(&long, TIP_MAX).len(), TIP_MAX);
        // Never splits a surrogate pair.
        let emoji = "\u{1F980}".repeat(100);
        let cut = truncate_utf16(&emoji, 127);
        assert!(cut.encode_utf16().count() <= 127);
        assert_eq!(cut.chars().count(), 63);
    }

    #[test]
    fn icon_is_a_symmetric_ring_with_transparent_corners() {
        let n = 32;
        let px = icon_pixels(n);
        assert_eq!(px.len(), n * n * 4);
        let alpha = |x: usize, y: usize| px[(y * n + x) * 4 + 3];
        assert_eq!(alpha(0, 0), 0, "corner is transparent");
        assert!(alpha(n / 2, n / 2) > 200, "centre dot is opaque");
        assert!(alpha(n / 2, 2) > 200, "ring is opaque");
        assert_eq!(alpha(n / 2, 8), 0, "gap between ring and dot");
        // Vertical and horizontal symmetry, so row order cannot flip the picture.
        for y in 0..n {
            for x in 0..n {
                assert_eq!(alpha(x, y), alpha(x, n - 1 - y));
                assert_eq!(alpha(x, y), alpha(n - 1 - x, y));
            }
        }
        // Colour is the orange accent (BGRA).
        assert_eq!(
            &px[(16 * n + 16) * 4..(16 * n + 16) * 4 + 3],
            &[0x0C, 0x59, 0xE8]
        );
    }
}
