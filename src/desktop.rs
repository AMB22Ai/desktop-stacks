// Explorer / desktop integration: hide and restore the normal desktop icons,
// keep our window just above the wallpaper, detect fullscreen apps, list monitors.

use windows::core::w;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::*;

const TOGGLE_DESKTOP_ICONS: usize = 0x7402;

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Returns (SHELLDLL_DefView, SysListView32) of the desktop, wherever Explorer hosts it.
fn find_desktop_view() -> Option<(HWND, HWND)> {
    unsafe {
        let mut defview = HWND::default();
        if let Ok(progman) = FindWindowW(w!("Progman"), None) {
            defview = FindWindowExW(progman, None, w!("SHELLDLL_DefView"), None).unwrap_or_default();
        }
        if defview.0.is_null() {
            let mut worker = HWND::default();
            loop {
                worker = match FindWindowExW(None, worker, w!("WorkerW"), None) {
                    Ok(h) if !h.0.is_null() => h,
                    _ => break,
                };
                if let Ok(dv) = FindWindowExW(worker, None, w!("SHELLDLL_DefView"), None) {
                    if !dv.0.is_null() {
                        defview = dv;
                        break;
                    }
                }
            }
        }
        if defview.0.is_null() {
            return None;
        }
        let list = FindWindowExW(defview, None, w!("SysListView32"), None).ok()?;
        if list.0.is_null() {
            return None;
        }
        Some((defview, list))
    }
}

fn list_visible(list: HWND) -> bool {
    // Check the list's own WS_VISIBLE style: IsWindowVisible would also be false
    // when a parent is temporarily hidden (Win+D, Explorer restarting).
    unsafe { (GetWindowLongW(list, GWL_STYLE) as u32 & WS_VISIBLE.0) != 0 }
}

pub fn icons_visible() -> Option<bool> {
    let (_, list) = find_desktop_view()?;
    Some(list_visible(list))
}

fn send_toggle(defview: HWND) {
    unsafe {
        let mut res = 0usize;
        let _ = SendMessageTimeoutW(
            defview,
            WM_COMMAND,
            WPARAM(TOGGLE_DESKTOP_ICONS),
            LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            2000,
            Some(&mut res),
        );
    }
}

/// Uses the same command as Explorer's "View > Show desktop icons", so the
/// desktop right-click menu and everything else in Explorer keeps working.
/// Returns true only when the desktop is verified to be in the wanted state.
pub fn set_icons_visible(visible: bool) -> bool {
    match find_desktop_view() {
        Some((defview, list)) => {
            if list_visible(list) != visible {
                send_toggle(defview);
            }
            list_visible(list) == visible
        }
        None => {
            // Explorer not reachable: at least fix the saved setting so the
            // next Explorer start shows icons.
            if visible {
                set_hideicons_registry(false);
            }
            false
        }
    }
}

fn set_hideicons_registry(hide: bool) {
    use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD};
    let v: u32 = if hide { 1 } else { 0 };
    unsafe {
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced"),
            w!("HideIcons"),
            REG_DWORD.0,
            Some(&v as *const u32 as *const _),
            4,
        );
    }
}

pub fn is_desktop_window(hwnd: HWND) -> bool {
    matches!(class_name(hwnd).as_str(), "Progman" | "WorkerW")
}

fn is_shell_window(hwnd: HWND) -> bool {
    matches!(
        class_name(hwnd).as_str(),
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "#32768"
    )
}

/// Places `hwnd` directly above the desktop/wallpaper windows (below every normal window).
/// Walks up from the bottom of the z-order, ignoring invisible windows, to find the
/// highest visible Progman/WorkerW, then inserts our window right above it.
pub fn place_above_desktop(hwnd: HWND) {
    unsafe {
        let mut w = match GetWindow(hwnd, GW_HWNDLAST) {
            Ok(h) => h,
            Err(_) => return,
        };
        let mut desk = HWND::default();
        let mut guard = 0;
        while !w.0.is_null() && guard < 4000 {
            guard += 1;
            if w != hwnd && IsWindowVisible(w).as_bool() {
                if is_desktop_window(w) {
                    desk = w;
                } else {
                    break;
                }
            }
            w = GetWindow(w, GW_HWNDPREV).unwrap_or_default();
        }
        if desk.0.is_null() {
            // No desktop window found under the apps (Explorer restarting, or another
            // tool sits at the very bottom): never stay above the user's windows.
            let shell = GetShellWindow();
            if shell.0.is_null() {
                let _ = SetWindowPos(hwnd, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER);
                return;
            }
            desk = shell;
        }
        let above = GetWindow(desk, GW_HWNDPREV).unwrap_or_default();
        if above == hwnd {
            return;
        }
        let topmost = !above.0.is_null() && (GetWindowLongW(above, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0) != 0;
        let insert_after = if above.0.is_null() || topmost { HWND_TOP } else { above };
        let _ = SetWindowPos(
            hwnd,
            insert_after,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );
    }
}

/// Used while "Show desktop" (Win+D) has raised the desktop above all windows.
pub fn place_on_top(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            HWND_TOP,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );
    }
}

/// True when the foreground window covers the whole monitor (game, video, F11).
pub fn fullscreen_on(monitor: &RECT, ours: HWND) -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() || fg == ours || is_shell_window(fg) {
            return false;
        }
        let mut r = RECT::default();
        if GetWindowRect(fg, &mut r).is_err() {
            return false;
        }
        r.left <= monitor.left && r.top <= monitor.top && r.right >= monitor.right && r.bottom >= monitor.bottom
    }
}

pub fn foreground_is_desktop() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        !fg.0.is_null() && is_desktop_window(fg)
    }
}

#[derive(Clone, Debug)]
pub struct Monitor {
    pub device: String,
    pub rect: RECT,
    pub work: RECT,
    pub primary: bool,
    pub dpi: u32,
}

unsafe extern "system" fn enum_proc(hmon: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    let list = &mut *(data.0 as *mut Vec<Monitor>);
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(hmon, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
        let n = info.szDevice.iter().position(|&c| c == 0).unwrap_or(0);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        list.push(Monitor {
            device: String::from_utf16_lossy(&info.szDevice[..n]),
            rect: info.monitorInfo.rcMonitor,
            work: info.monitorInfo.rcWork,
            primary: info.monitorInfo.dwFlags & 1 != 0,
            dpi: dx.max(96),
        });
    }
    BOOL(1)
}

pub fn monitors() -> Vec<Monitor> {
    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(enum_proc), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

/// The chosen monitor by device name, falling back to the primary one.
pub fn pick_monitor(device: &str) -> Option<Monitor> {
    let list = monitors();
    list.iter()
        .find(|m| !device.is_empty() && m.device == device)
        .or_else(|| list.iter().find(|m| m.primary))
        .or(list.first())
        .cloned()
}
