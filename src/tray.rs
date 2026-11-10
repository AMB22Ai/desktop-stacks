// Tray icon, its menu, settings file, start-with-Windows, and the crash marker.

use std::path::PathBuf;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS,
};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const ID_PAUSE: usize = 1;
pub const ID_AUTOSTART: usize = 2;
pub const ID_EXIT: usize = 3;
pub const ID_FOLDERS: usize = 4;
pub const ID_LABEL_BG: usize = 5;
pub const ID_HOTKEY: usize = 6;
pub const ID_MONITOR_BASE: usize = 100;
pub const ID_LANG_BASE: usize = 200;
pub const ID_SIZE_BASE: usize = 300;
pub const ID_SIDE_BASE: usize = 310;

/// Icon size choices, in menu order: small, medium, large.
pub const SIZES: [(i8, f32); 3] = [(-1, 0.8), (0, 1.0), (1, 1.25)];
/// Manual Apps / Games choices kept on disk: newest wins, oldest are dropped past this.
pub const MAX_KIND_CHOICES: usize = 300;

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("DesktopStacks");

// ---------- settings ----------

/// %LOCALAPPDATA%\DesktopStacks (this PC only, never roamed), asked from Windows itself (not from the APPDATA
/// environment variable, which whoever started the process can change).
/// None if Windows cannot tell: then nothing is saved and icons are never hidden.
fn data_dir() -> Option<PathBuf> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let base = crate::known_folder(&windows::Win32::UI::Shell::FOLDERID_LocalAppData)?;
        let p = PathBuf::from(base).join("DesktopStacks");
        std::fs::create_dir_all(&p).ok()?;
        Some(p)
    })
    .clone()
}

/// Reads a small text file of ours; anything unexpectedly large is ignored.
fn read_small(path: &std::path::Path) -> Option<String> {
    read_limited(path, 64 * 1024)
}

fn read_limited(path: &std::path::Path, max: u64) -> Option<String> {
    let len = std::fs::metadata(path).ok()?.len();
    if len > max {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

#[derive(Default, Clone)]
pub struct Config {
    pub monitor: String,
    pub paused: bool,
    pub lang: crate::i18n::Lang,
    /// Icon size: -1 small, 0 medium, 1 large.
    pub size: i8,
    /// Stacks at the left screen edge instead of the right one.
    pub left: bool,
    /// Folders form a Folders stack.
    pub folders: bool,
    /// Dark backdrop behind file names (for light wallpapers).
    pub label_bg: bool,
    /// Ctrl+Alt+D pauses and resumes.
    pub hotkey: bool,
}

impl Config {
    pub fn size_factor(&self) -> f32 {
        SIZES.iter().find(|(id, _)| *id == self.size).map(|(_, f)| *f).unwrap_or(1.0)
    }
}

pub fn load_config() -> Config {
    let mut c = Config::default();
    if let Some(text) = data_dir().and_then(|d| read_small(&d.join("config.txt"))) {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                match k.trim() {
                    "monitor" => c.monitor = v.trim().to_string(),
                    "paused" => c.paused = v.trim() == "1",
                    "lang" => c.lang = crate::i18n::Lang::from_config(v.trim()),
                    "size" => c.size = match v.trim() { "small" => -1, "large" => 1, _ => 0 },
                    "side" => c.left = v.trim() == "left",
                    "folders" => c.folders = v.trim() == "1",
                    "labelbg" => c.label_bg = v.trim() == "1",
                    "hotkey" => c.hotkey = v.trim() == "1",
                    _ => {}
                }
            }
        }
    }
    c
}

pub fn save_config(c: &Config) {
    let bit = |b: bool| if b { 1 } else { 0 };
    let text = format!(
        "monitor={}\npaused={}\nlang={}\nsize={}\nside={}\nfolders={}\nlabelbg={}\nhotkey={}\n",
        c.monitor,
        bit(c.paused),
        c.lang.as_config(),
        match c.size { -1 => "small", 1 => "large", _ => "medium" },
        if c.left { "left" } else { "right" },
        bit(c.folders),
        bit(c.label_bg),
        bit(c.hotkey),
    );
    if let Some(d) = data_dir() {
        let _ = std::fs::create_dir_all(&d); // the folder may have been deleted while running
        let _ = std::fs::write(d.join("config.txt"), text);
    }
}

// ---------- manual Apps / Games choices ----------

/// Lines of "games|<path>" or "apps|<path>" in kinds.txt. true = Games.
pub fn load_kind_choices() -> Vec<(String, bool)> {
    let mut out = Vec::new();
    // 300 choices of up to 520 bytes each fit well inside this limit
    if let Some(text) = data_dir().and_then(|d| read_limited(&d.join("kinds.txt"), 512 * 1024)) {
        for line in text.lines() {
            if let Some((k, path)) = line.split_once('|') {
                let games = match k { "games" => true, "apps" => false, _ => continue };
                if !path.is_empty() && path.len() <= 520 && out.len() < MAX_KIND_CHOICES {
                    out.push((path.to_string(), games));
                }
            }
        }
    }
    out
}

pub fn save_kind_choices(list: &[(String, bool)]) {
    let Some(d) = data_dir() else { return };
    let mut text = String::new();
    for (path, games) in list.iter().rev().take(MAX_KIND_CHOICES).rev() {
        // a path cannot hold a line break; anything odd is simply not saved
        if path.len() <= 520 && !path.contains(['\n', '\r']) {
            text.push_str(if *games { "games|" } else { "apps|" });
            text.push_str(path);
            text.push('\n');
        }
    }
    let _ = std::fs::create_dir_all(&d);
    let _ = std::fs::write(d.join("kinds.txt"), text);
}

// ---------- crash marker: "we hid the desktop icons" ----------

fn marker() -> Option<PathBuf> {
    data_dir().map(|d| d.join("icons-hidden.flag"))
}
pub fn marker_exists() -> bool {
    marker().is_some_and(|m| m.exists())
}
/// Process id written into the marker by the run that hid the icons (None: no marker, or not readable).
pub fn marker_owner() -> Option<u32> {
    read_small(&marker()?)?.trim().parse().ok()
}
/// Returns true when the marker is in the wanted state on disk.
pub fn set_marker(on: bool) -> bool {
    let Some(m) = marker() else { return !on };
    if on {
        // the folder may have been deleted while the app was running
        if let Some(d) = m.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        // holds our process id, so a watchdog left over from an earlier run can tell it is not its marker
        std::fs::write(&m, std::process::id().to_string()).is_ok() && m.exists()
    } else {
        let _ = std::fs::remove_file(&m);
        !m.exists()
    }
}

// ---------- start with Windows ----------

/// The command Windows should run at sign-in: this exe, where it is now.
fn own_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\"", exe.display()))
}

/// The command stored under the Run key, if there is one (None also when it is longer
/// than any path this app would have written).
fn stored_command() -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut size = std::mem::size_of_val(&buf) as u32;
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut size))
            .ok()
            .ok()?;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// Windows paths do not depend on letter case.
fn same_command(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// The exe a stored command starts. This app only ever writes the quoted path.
fn command_target(cmd: &str) -> &str {
    cmd.trim().trim_matches('"')
}

/// Called once at start. If "Start with Windows" was ticked and the exe has been moved
/// since, the entry still names the old place. When no file is there any more, the
/// entry is corrected to where the exe is now, so the user's choice keeps working and
/// no entry is left pointing at an empty place. An entry for another copy that still
/// exists is left alone.
pub fn repair_autostart() {
    if let (Some(stored), Some(own)) = (stored_command(), own_command()) {
        if !same_command(&stored, &own) && !std::path::Path::new(command_target(&stored)).exists() {
            set_autostart(true);
        }
    }
}

/// Ticked only when the stored entry starts this very exe. An entry for another copy
/// shows as unticked, and ticking it writes the current place.
pub fn autostart_enabled() -> bool {
    match (stored_command(), own_command()) {
        (Some(stored), Some(own)) => same_command(&stored, &own),
        _ => false,
    }
}

pub fn set_autostart(on: bool) {
    unsafe {
        if on {
            let Some(cmd) = own_command() else { return };
            let wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(wide.as_ptr() as *const _),
                (wide.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
        }
    }
}

// ---------- tray icon ----------

/// Draws a small "three stacked cards" icon in code (no resource files needed).
fn make_icon() -> HICON {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
    let s = size as f32;
    unsafe {
        let bmi = BITMAPINFO { bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        }, ..Default::default() };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let color = match CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(b) => b,
            Err(_) => return LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        };
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);
        // Three rounded cards, back to front.
        let cards = [
            (0.30f32, 0.10f32, 0xFF8AB4F8u32),
            (0.20, 0.24, 0xFFC5D9FB),
            (0.10, 0.38, 0xFFFFFFFF),
        ];
        for y in 0..size {
            for x in 0..size {
                let (fx, fy) = ((x as f32 + 0.5) / s, (y as f32 + 0.5) / s);
                let mut out = 0u32;
                for (cx, cy, col) in cards {
                    let (w, h, r) = (0.60f32, 0.50f32, 0.12f32);
                    let dx = (fx - (cx + w / 2.0)).abs() - (w / 2.0 - r);
                    let dy = (fy - (cy + h / 2.0)).abs() - (h / 2.0 - r);
                    let d = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() - r + dx.max(dy).min(0.0);
                    if d < 0.0 {
                        // dark outline near the edge
                        out = if d > -1.2 / s { 0xFF303845 } else { col };
                    }
                }
                px[(y * size + x) as usize] = out;
            }
        }
        // 1-bit mask, all zero (the colour bitmap's alpha decides the shape); rows are 16-bit aligned
        let mask_bits = vec![0u8; (((size + 15) / 16) * 2 * size) as usize];
        let mask = CreateBitmap(size, size, 1, 1, Some(mask_bits.as_ptr() as *const _));
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color);
        let _ = DeleteObject(mask);
        icon
    }
}

static ICON: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
fn cached_icon() -> HICON {
    use std::sync::atomic::Ordering;
    let v = ICON.load(Ordering::SeqCst);
    if v != 0 {
        return HICON(v as *mut _);
    }
    let i = make_icon();
    ICON.store(i.0 as isize, Ordering::SeqCst);
    i
}

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = crate::WM_APP_TRAY;
    nid
}

pub fn add_tray(hwnd: HWND) {
    let mut nid = tray_data(hwnd);
    nid.hIcon = cached_icon();
    let tip: Vec<u16> = "Desktop Stacks".encode_utf16().collect();
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

pub fn remove_tray(hwnd: HWND) {
    let nid = tray_data(hwnd);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// Shows the tray menu and returns the chosen command id (0 = none).
pub fn show_menu(hwnd: HWND, paused: bool, monitors: &[crate::desktop::Monitor], current: &str, cfg: &Config) -> usize {
    let lang = cfg.lang;
    use crate::i18n::{is_arabic, t, Lang};
    unsafe {
        let menu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(_) => return 0,
        };
        let pause_label = if paused { t("Resume stacks", "\u{062A}\u{0634}\u{063A}\u{064A}\u{0644} \u{0627}\u{0644}\u{0643}\u{0648}\u{0645}\u{0627}\u{062A}") } else { t("Pause (normal Windows desktop)", "\u{0625}\u{064A}\u{0642}\u{0627}\u{0641} \u{0645}\u{0624}\u{0642}\u{062A} (\u{0633}\u{0637}\u{062D} \u{0627}\u{0644}\u{0645}\u{0643}\u{062A}\u{0628} \u{0627}\u{0644}\u{0639}\u{0627}\u{062F}\u{064A})") };
        let _ = AppendMenuW(menu, MF_STRING, ID_PAUSE, &HSTRING::from(pause_label));
        if monitors.len() > 1 {
            if let Ok(sub) = CreatePopupMenu() {
                for (i, m) in monitors.iter().enumerate() {
                    let label = format!("{} {}{}", t("Display", "\u{0634}\u{0627}\u{0634}\u{0629}"), i + 1, if m.primary { t(" (primary)", " (\u{0627}\u{0644}\u{0631}\u{0626}\u{064A}\u{0633}\u{064A}\u{0629})") } else { "" });
                    let flags = MF_STRING | if m.device == current { MF_CHECKED } else { MF_UNCHECKED };
                    let _ = AppendMenuW(sub, flags, ID_MONITOR_BASE + i, &HSTRING::from(label));
                }
                let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, &HSTRING::from(t("Display", "\u{0627}\u{0644}\u{0634}\u{0627}\u{0634}\u{0629}")));
            }
        }
        if let Ok(sub) = CreatePopupMenu() {
            // each language is shown in its own language, so it can always be found
            let choices = [(Lang::Auto, t("Automatic", "\u{062A}\u{0644}\u{0642}\u{0627}\u{0626}\u{064A}")), (Lang::Ar, "\u{0627}\u{0644}\u{0639}\u{0631}\u{0628}\u{064A}\u{0629}"), (Lang::En, "English")];
            for (i, (choice, label)) in choices.into_iter().enumerate() {
                let flags = MF_STRING | if choice == lang { MF_CHECKED } else { MF_UNCHECKED };
                let _ = AppendMenuW(sub, flags, ID_LANG_BASE + i, &HSTRING::from(label));
            }
            let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, &HSTRING::from(t("Language", "\u{0627}\u{0644}\u{0644}\u{063A}\u{0629}")));
        }
        let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
        if let Ok(sub) = CreatePopupMenu() {
            let names = [t("Small", "\u{0635}\u{063A}\u{064A}\u{0631}"), t("Medium", "\u{0645}\u{062A}\u{0648}\u{0633}\u{0637}"), t("Large", "\u{0643}\u{0628}\u{064A}\u{0631}")];
            for (i, name) in names.into_iter().enumerate() {
                let _ = AppendMenuW(sub, MF_STRING | check(SIZES[i].0 == cfg.size), ID_SIZE_BASE + i, &HSTRING::from(name));
            }
            let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, &HSTRING::from(t("Icon size", "\u{062D}\u{062C}\u{0645} \u{0627}\u{0644}\u{0623}\u{064A}\u{0642}\u{0648}\u{0646}\u{0627}\u{062A}")));
        }
        if let Ok(sub) = CreatePopupMenu() {
            let _ = AppendMenuW(sub, MF_STRING | check(!cfg.left), ID_SIDE_BASE, &HSTRING::from(t("Right edge", "\u{0627}\u{0644}\u{062D}\u{0627}\u{0641}\u{0629} \u{0627}\u{0644}\u{064A}\u{0645}\u{0646}\u{0649}")));
            let _ = AppendMenuW(sub, MF_STRING | check(cfg.left), ID_SIDE_BASE + 1, &HSTRING::from(t("Left edge", "\u{0627}\u{0644}\u{062D}\u{0627}\u{0641}\u{0629} \u{0627}\u{0644}\u{064A}\u{0633}\u{0631}\u{0649}")));
            let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, &HSTRING::from(t("Stacks position", "\u{0645}\u{0643}\u{0627}\u{0646} \u{0627}\u{0644}\u{0643}\u{0648}\u{0645}\u{0627}\u{062A}")));
        }
        let _ = AppendMenuW(menu, MF_STRING | check(cfg.folders), ID_FOLDERS, &HSTRING::from(t("Stack folders", "\u{062A}\u{062C}\u{0645}\u{064A}\u{0639} \u{0627}\u{0644}\u{0645}\u{062C}\u{0644}\u{062F}\u{0627}\u{062A} \u{0641}\u{064A} \u{0643}\u{0648}\u{0645}\u{0629}")));
        let _ = AppendMenuW(menu, MF_STRING | check(cfg.label_bg), ID_LABEL_BG, &HSTRING::from(t("Dark background behind names", "\u{062E}\u{0644}\u{0641}\u{064A}\u{0629} \u{062F}\u{0627}\u{0643}\u{0646}\u{0629} \u{062E}\u{0644}\u{0641} \u{0627}\u{0644}\u{0623}\u{0633}\u{0645}\u{0627}\u{0621}")));
        let _ = AppendMenuW(menu, MF_STRING | check(cfg.hotkey), ID_HOTKEY, &HSTRING::from(t("Ctrl+Alt+D pauses and resumes", "Ctrl+Alt+D \u{0644}\u{0644}\u{0625}\u{064A}\u{0642}\u{0627}\u{0641} \u{0627}\u{0644}\u{0645}\u{0624}\u{0642}\u{062A} \u{0648}\u{0627}\u{0644}\u{062A}\u{0634}\u{063A}\u{064A}\u{0644}")));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let auto = if autostart_enabled() { MF_CHECKED } else { MF_UNCHECKED };
        let _ = AppendMenuW(menu, MF_STRING | auto, ID_AUTOSTART, &HSTRING::from(t("Start with Windows", "\u{0627}\u{0644}\u{062A}\u{0634}\u{063A}\u{064A}\u{0644} \u{0645}\u{0639} \u{0648}\u{064A}\u{0646}\u{062F}\u{0648}\u{0632}")));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, &HSTRING::from(t("Exit", "\u{062E}\u{0631}\u{0648}\u{062C}")));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        // right-to-left menu only for Arabic
        let layout = if is_arabic() { TPM_LAYOUTRTL } else { TRACK_POPUP_MENU_FLAGS(0) };
        let cmd = TrackPopupMenuEx(menu, (TPM_RETURNCMD | TPM_RIGHTBUTTON | layout).0, pt.x, pt.y, hwnd, None);
        let _ = DestroyMenu(menu);
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        cmd.0 as usize
    }
}

#[cfg(test)]
mod tests {
    use super::{command_target, same_command};

    #[test]
    fn the_stored_command_gives_back_the_path_of_the_exe() {
        assert_eq!(command_target("\"C:\\Tools\\DesktopStacks.exe\""), "C:\\Tools\\DesktopStacks.exe");
        assert_eq!(command_target(" \"C:\\My Tools\\DesktopStacks.exe\" "), "C:\\My Tools\\DesktopStacks.exe");
        assert_eq!(command_target(""), "");
    }

    #[test]
    fn the_start_with_windows_tick_follows_the_stored_place() {
        assert!(same_command("\"C:\\Tools\\DesktopStacks.exe\"", "\"c:\\tools\\desktopstacks.exe\""));
        assert!(same_command(" \"C:\\Tools\\DesktopStacks.exe\" ", "\"C:\\Tools\\DesktopStacks.exe\""));
        assert!(!same_command("\"C:\\Users\\x\\Downloads\\DesktopStacks.exe\"", "\"C:\\Tools\\DesktopStacks.exe\""));
        assert!(!same_command("", "\"C:\\Tools\\DesktopStacks.exe\""));
    }
}
