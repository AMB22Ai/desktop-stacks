#![windows_subsystem = "windows"]
// Desktop Stacks for Windows: macOS-style desktop stacks.
// Usage:  desktop-stacks.exe               (real desktop)
//         desktop-stacks.exe --dir <path>  (test on any folder)

mod desktop;
mod games;
mod harden;
mod i18n;
mod icons;
mod layout;
mod model;
mod rename;
mod render;
mod shell;
mod tray;

use layout::{build_entries, stack_membership, Animator, Entry, EntryRef, Geom, Vis};
use model::{Kind, Model};
use render::{DrawItem, DrawKind, Renderer};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use windows::core::{w, Interface, HSTRING};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DWMWA_EXCLUDED_FROM_PEEK, DWMWA_TRANSITIONS_FORCEDISABLED};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{IDropTarget, OleInitialize, RegisterDragDrop, RevokeDragDrop};
use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, ReleaseCapture, SetCapture, TrackMouseEvent, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::Shell::{
    IContextMenu3, SHGetKnownFolderPath, FOLDERID_Desktop, FOLDERID_PublicDesktop, KF_FLAG_DEFAULT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_APP_ICON: u32 = WM_APP + 1;
pub const WM_APP_FS: u32 = WM_APP + 2;
pub const WM_APP_TRAY: u32 = WM_APP + 3;
pub const WM_APP_FG: u32 = WM_APP + 4;

const TIMER_ANIM: usize = 1;
const TIMER_RESCAN: usize = 2;
const TIMER_SUSPEND: usize = 3;
const TIMER_TRIM: usize = 4;
const TIMER_HIDE_RETRY: usize = 5;
const TIMER_GPU_RETRY: usize = 6;
const TIMER_RESTORE_RETRY: usize = 7;
const TIMER_REPLACE: usize = 8;
const TIMER_REPLAY: usize = 9;
const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
const WM_CAPTURECHANGED_: u32 = 0x0215;
const MK_LBUTTON_: usize = 0x0001;

static MAIN_THREAD: std::sync::OnceLock<std::thread::ThreadId> = std::sync::OnceLock::new();

/// Gives the normal desktop icons back if we hid them. Never touches APP, so it is
/// safe from any context (logoff, panic hook, modal loops). The marker file is the
/// single source of truth for "we hid them".
fn restore_icons() -> bool {
    if !tray::marker_exists() {
        return true;
    }
    desktop::set_icons_visible(true) && tray::set_marker(false)
}

const CMD_TOGGLE_STACK: usize = 0x7100;
const CMD_PASTE: usize = 0x7101;
const CMD_REFRESH: usize = 0x7102;
const CMD_MOVE_KIND: usize = 0x7103;
const HOTKEY_PAUSE: i32 = 1;
const WM_MOUSELEAVE: u32 = 0x02A3;

static MAIN_HWND: AtomicUsize = AtomicUsize::new(0);
static PLACING: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the app state. Returns None if the state is busy (re-entrant call),
/// which can only happen while a modal loop runs; such messages are simply skipped.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

// Work that must not be lost when its message arrives while the app state is busy
// (Windows can deliver a message in the middle of another one): it is noted here and
// done a moment later by TIMER_REPLAY.
static PENDING: AtomicUsize = AtomicUsize::new(0);
const PEND_ICONS: usize = 1;
const PEND_FOREGROUND: usize = 2;
const PEND_MONITOR: usize = 4;
const PEND_DEVICE: usize = 8;

fn run_pending(a: &mut App, bits: usize) {
    if bits & PEND_MONITOR != 0 && !a.paused {
        a.apply_monitor();
    }
    if bits & PEND_DEVICE != 0 {
        a.check_device();
        a.render();
    }
    if bits & PEND_FOREGROUND != 0 {
        a.on_foreground();
    }
    if bits & PEND_ICONS != 0 {
        a.take_icons();
    }
}

/// Does the work now, or notes it for later if the app state is busy.
fn now_or_later(hwnd: HWND, bit: usize) {
    let busy = APP.with(|a| a.try_borrow_mut().is_err());
    if busy {
        PENDING.fetch_or(bit, Ordering::SeqCst);
        unsafe {
            SetTimer(hwnd, TIMER_REPLAY, 50, None);
        }
    } else {
        with_app(|a| run_pending(a, bit));
    }
}

/// TIMER_REPLAY: runs what was noted, once the app state is free again.
fn replay_pending(hwnd: HWND) {
    if APP.with(|a| a.try_borrow_mut().is_err()) {
        return; // still busy: the timer fires again
    }
    unsafe {
        let _ = KillTimer(hwnd, TIMER_REPLAY);
    }
    let bits = PENDING.swap(0, Ordering::SeqCst);
    if bits != 0 {
        with_app(|a| run_pending(a, bits));
    }
}

struct MouseDown {
    x: i32,
    y: i32,
    key: Option<String>,
}

enum Action {
    None,
    Open(String),
    /// `move_to`: the other program stack (Apps or Games) this single file can be moved to.
    ItemMenu { paths: Vec<String>, pt: POINT, stack: Option<Kind>, move_to: Option<Kind> },
    BackgroundMenu { dir: String, real_desktop: bool, pt: POINT },
    DragOut(Vec<String>),
}

struct App {
    hwnd: HWND,
    dirs: Vec<String>,
    include_system: bool,
    cfg: tray::Config,
    /// The user's manual Apps / Games choices (path, kind), oldest first.
    kind_choices: Vec<(String, Kind)>,
    /// Windows accepted the Ctrl+Alt+D registration this run (the saved wish is cfg.hotkey).
    hotkey_ok: bool,
    model: Model,
    expanded: HashSet<Kind>,
    entries: Vec<Entry>,
    anim: Animator,
    geom: Geom,
    mon_rect: RECT,
    work: RECT,
    renderer: Option<Renderer>,
    icon_tx: Sender<icons::IconReq>,
    icon_rx: Receiver<icons::IconRes>,
    requested: HashSet<String>,
    hover: Option<String>,
    selected: Option<String>,
    scrub: Option<(Kind, usize)>,
    wheel_acc: i32,
    mouse_down: Option<MouseDown>,
    tracking: bool,
    suspended: bool,
    dirty: bool,
    needs_rescan: bool,
    paused: bool,
    desktop_mode: bool,
    gpu_down: bool,
    gpu_wait_ms: u32,
    /// Draw with the software renderer (set after the graphics card failed).
    gpu_soft: bool,
    scan_down: bool,
    scan_failures: u32,
    restore_retries: u32,
    hide_retries: u32,
    drop_cache: Option<(String, IDropTarget)>,
}

fn place(hwnd: HWND, on_top: bool) {
    PLACING.store(true, Ordering::SeqCst);
    if on_top {
        desktop::place_on_top(hwnd);
    } else {
        desktop::place_above_desktop(hwnd);
    }
    PLACING.store(false, Ordering::SeqCst);
}

#[allow(clippy::too_many_arguments)]
fn build_items<'a>(
    model: &'a Model,
    entries: &'a [Entry],
    anim: &'a Animator,
    geom: &Geom,
    expanded: &HashSet<Kind>,
    hover: &Option<String>,
    selected: &Option<String>,
    scrub: &Option<(Kind, usize)>,
) -> Vec<DrawItem<'a>> {
    let mut items = Vec::with_capacity(entries.len() + anim.ghosts.len());
    let vis_of = |key: &str, i: usize| {
        anim.cur.get(key).copied().unwrap_or_else(|| {
            let (x, y) = geom.cell_pos(i);
            Vis { x, y, a: 1.0, sc: 1.0 }
        })
    };
    if !anim.ghosts.is_empty() {
        let by_key: HashMap<String, usize> =
            model.files.iter().enumerate().map(|(i, f)| (layout::file_key(&f.path), i)).collect();
        for g in &anim.ghosts {
            if let (Some(&fi), Some(v)) = (by_key.get(&g.entry.key), anim.cur.get(&g.entry.key)) {
                items.push(DrawItem { vis: *v, kind: DrawKind::File(&model.files[fi]), hover: false, selected: false });
            }
        }
    }
    for (i, e) in entries.iter().enumerate() {
        let v = vis_of(&e.key, i);
        // only what is on screen is prepared and drawn
        if v.x + geom.cell_w <= 0.0 || v.x >= geom.w {
            continue;
        }
        let hov = hover.as_deref() == Some(e.key.as_str());
        let sel = selected.as_deref() == Some(e.key.as_str());
        match &e.r {
            EntryRef::File(fi) => {
                if let Some(f) = model.files.get(*fi) {
                    items.push(DrawItem { vis: v, kind: DrawKind::File(f), hover: hov, selected: sel });
                }
            }
            EntryRef::Stack(k) => {
                if let Some((_, idx)) = model.groups.iter().find(|(gk, _)| gk == k) {
                    let covers: Vec<&model::FileItem> = idx.iter().take(3).map(|&i| &model.files[i]).collect();
                    let sc = match scrub {
                        Some((sk, si)) if sk == k && !expanded.contains(k) => idx.get(*si).map(|&i| &model.files[i]),
                        _ => None,
                    };
                    items.push(DrawItem {
                        vis: v,
                        kind: DrawKind::Stack { label: k.label(), covers, expanded: expanded.contains(k), scrub: sc, count: idx.len() },
                        hover: hov,
                        selected: sel,
                    });
                }
            }
        }
    }
    items
}

impl App {
    fn render(&mut self) {
        self.render_inner(true);
    }

    fn render_inner(&mut self, retry: bool) {
        if self.paused {
            return;
        }
        if self.suspended {
            self.dirty = true;
            return;
        }
        if self.gpu_down {
            self.dirty = true;
            return;
        }
        if self.renderer.is_none() {
            match Renderer::new(self.hwnd, self.geom.w.max(self.geom.h) as u32, self.gpu_soft) {
                Ok(r) => self.renderer = Some(r),
                Err(_) => {
                    self.gpu_failed();
                    return;
                }
            }
            self.requested.clear();
        }
        let (w, h) = (self.geom.w as u32, self.geom.h as u32);
        let items = build_items(
            &self.model,
            &self.entries,
            &self.anim,
            &self.geom,
            &self.expanded,
            &self.hover,
            &self.selected,
            &self.scrub,
        );
        let max_scroll = self.geom.max_scroll(self.entries.len());
        // (more icons past the left edge, more past the right edge)
        let (further, back) = (self.geom.scroll < max_scroll - 0.5, self.geom.scroll > 0.5);
        let more = if self.geom.left { (back, further) } else { (further, back) };
        let label_bg = self.cfg.label_bg;
        let Some(r) = self.renderer.as_mut() else { return };
        let result = r.resize(w, h).and_then(|_| r.draw(&self.geom, &items, more, label_bg));
        drop(items);
        match result {
            Ok(missing) => {
                let px = self.geom.icon.round() as i32;
                for (key, path) in missing {
                    if self.requested.insert(key.clone()) {
                        let _ = self.icon_tx.send(icons::IconReq { key, path, px });
                    }
                }
            }
            Err(_) => {
                // GPU device lost (driver update, sleep): rebuild once.
                self.renderer = None;
                self.requested.clear();
                if retry {
                    self.render_inner(false);
                } else {
                    self.gpu_failed();
                }
                return;
            }
        }
        self.dirty = false;
        self.gpu_wait_ms = 5000;
        unsafe {
            SetTimer(self.hwnd, TIMER_TRIM, 3000, None);
        }
    }

    fn relayout(&mut self, animate: bool) {
        let new = build_entries(&self.model, &self.expanded);
        self.geom.scroll = self.geom.scroll.clamp(0.0, self.geom.max_scroll(new.len()));
        let membership = stack_membership(&self.model);
        // nothing moved (e.g. a file was only modified): one redraw, no animation
        let same = new.len() == self.entries.len() && new.iter().zip(&self.entries).all(|(a, b)| a.key == b.key);
        let anim = animate && !same && !self.suspended && !self.paused;
        self.anim.retarget(&self.geom, &self.entries, &new, &membership, anim);
        self.entries = new;
        if self.anim.active {
            unsafe {
                SetTimer(self.hwnd, TIMER_ANIM, 8, None);
            }
        }
        self.render();
    }

    fn rescan(&mut self, animate: bool) {
        if self.suspended || self.paused {
            self.needs_rescan = true;
            return;
        }
        self.needs_rescan = false;
        match model::scan(&self.dirs, self.include_system, self.cfg.folders) {
            Some(m) => {
                self.model = m;
                let recovered = self.scan_down || self.scan_failures > 0;
                self.scan_failures = 0;
                if recovered {
                    // folder readable again: back to stacks
                    self.scan_down = false;
                    self.hide_retries = 0;
                    self.come_back();
                }
            }
            None => {
                // Folder not readable right now (OneDrive, network): keep the last good
                // list and retry. If it keeps failing, give the normal desktop back.
                self.scan_failures += 1;
                if self.scan_failures >= 5 && !self.scan_down {
                    self.scan_down = true;
                    self.step_aside();
                }
                let wait = if self.scan_failures >= 5 { 10_000 } else { 2_000 };
                unsafe {
                    SetTimer(self.hwnd, TIMER_RESCAN, wait, None);
                }
                return;
            }
        }
        // forget stacks that no longer exist
        let live: HashSet<Kind> = self.model.groups.iter().filter(|(_, v)| v.len() > 1).map(|(k, _)| *k).collect();
        self.expanded.retain(|k| live.contains(k));
        if let Some((k, i)) = self.scrub {
            let len = self.model.groups.iter().find(|(gk, _)| *gk == k).map(|g| g.1.len()).unwrap_or(0);
            if i >= len {
                self.scrub = None;
            }
        }
        // drop GPU icons of files that are gone
        let keys: HashSet<String> = self.model.files.iter().map(|f| f.key()).collect();
        if let Some(r) = self.renderer.as_mut() {
            // failed loads (None) are dropped so they get another try; requests still
            // waiting in the queue stay marked as requested (no duplicates)
            let failed: HashSet<String> = r.icons.iter().filter(|(_, v)| v.is_none()).map(|(k, _)| k.clone()).collect();
            r.icons.retain(|k, v| keys.contains(k) && v.is_some());
            self.requested.retain(|k| keys.contains(k) && !failed.contains(k));
        } else {
            self.requested.retain(|k| keys.contains(k));
        }
        self.drop_cache = None;
        self.relayout(animate);
    }

    fn apply_monitor(&mut self) {
        let Some(m) = desktop::pick_monitor(&self.cfg.monitor) else { return };
        let (w, h) = (m.work.right - m.work.left, m.work.bottom - m.work.top);
        // DPI scale times the user's icon size; a change of either needs new icons and text
        let geom = Geom::with_options(w as f32, h as f32, m.dpi as f32 / 96.0, self.cfg.size_factor(), self.cfg.left);
        let scale_changed = (geom.s - self.geom.s).abs() > 0.001;
        unsafe {
            let _ = SetWindowPos(self.hwnd, None, m.work.left, m.work.top, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        }
        self.mon_rect = m.rect;
        self.work = m.work;
        self.geom = geom;
        if scale_changed {
            if let Some(r) = self.renderer.as_mut() {
                r.icons.clear();
                r.clear_text_cache();
            }
            self.requested.clear();
        }
        self.anim = Animator::new();
        self.relayout(false);
    }

    /// GPU unusable (driver reset, no device): show the normal desktop and retry later.
    fn gpu_failed(&mut self) {
        self.gpu_down = true;
        self.dirty = true;
        // next attempt uses the other renderer (graphics card <-> software)
        self.gpu_soft = !self.gpu_soft;
        self.step_aside();
        self.arm_gpu_retry();
    }

    /// Retry with growing pauses (5 s up to 60 s), so a GPU that stays unusable costs nothing.
    fn arm_gpu_retry(&mut self) {
        let wait = self.gpu_wait_ms;
        self.gpu_wait_ms = (wait * 2).min(60_000);
        unsafe {
            SetTimer(self.hwnd, TIMER_GPU_RETRY, wait, None);
        }
    }

    /// Something is wrong (GPU, folder): hide our layer and give the normal desktop back.
    fn step_aside(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.ensure_restored();
    }

    /// Back to normal after step_aside.
    fn come_back(&mut self) {
        if self.paused || self.gpu_down || self.scan_down {
            return;
        }
        self.hide_explorer_icons();
        if desktop::icons_visible() == Some(true) {
            // Explorer's own icons are still showing (it is busy): the retry timer
            // calls this again. Never draw a second set of icons on top of them.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            return;
        }
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        place(self.hwnd, false);
    }

    /// After a driver crash/update, GPU removal or wake from sleep the drawn layer can
    /// be gone while nothing triggers a redraw: detect it and rebuild.
    fn check_device(&mut self) {
        if self.paused || self.gpu_down {
            return;
        }
        if self.renderer.as_ref().is_some_and(|r| r.device_lost()) {
            self.renderer = None;
            self.requested.clear();
            self.render();
        }
    }

    /// Restores Explorer icons, and keeps retrying in the background if Explorer is busy.
    fn ensure_restored(&mut self) {
        if restore_icons() {
            self.restore_retries = 0;
        } else if self.restore_retries < 60 {
            self.restore_retries += 1;
            unsafe {
                SetTimer(self.hwnd, TIMER_RESTORE_RETRY, 2000, None);
            }
        }
    }

    fn gpu_retry(&mut self) {
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_GPU_RETRY);
        }
        if self.paused {
            return;
        }
        match Renderer::new(self.hwnd, self.geom.w.max(self.geom.h) as u32, self.gpu_soft) {
            Ok(r) => {
                self.renderer = Some(r);
                self.requested.clear();
                self.gpu_down = false;
                self.render();
                self.come_back();
            }
            Err(_) => {
                self.gpu_soft = !self.gpu_soft;
                self.arm_gpu_retry();
            }
        }
    }

    fn hide_explorer_icons(&mut self) {
        match desktop::icons_visible() {
            Some(true) => {
                // Marker first, so the watchdog can always undo it. No marker, no hiding.
                if !tray::set_marker(true) {
                    return;
                }
                if !desktop::set_icons_visible(false) && self.hide_retries < 30 {
                    // Explorer busy (e.g. at sign-in): try again shortly
                    self.hide_retries += 1;
                    unsafe {
                        SetTimer(self.hwnd, TIMER_HIDE_RETRY, 2000, None);
                    }
                }
            }
            Some(false) => {
                // Already hidden: by us earlier (marker exists) or by the user (left alone).
            }
            None => {
                // Explorer not ready yet (early at sign-in): try again shortly.
                if self.hide_retries < 30 {
                    self.hide_retries += 1;
                    unsafe {
                        SetTimer(self.hwnd, TIMER_HIDE_RETRY, 2000, None);
                    }
                }
            }
        }
    }

    fn restore_explorer_icons(&mut self) {
        self.ensure_restored();
    }

    fn resume(&mut self) {
        self.paused = false;
        self.gpu_down = false;
        self.scan_down = false;
        self.scan_failures = 0;
        self.hide_retries = 0;
        self.gpu_soft = false;
        self.gpu_wait_ms = 5000;
        // List the files and draw first; only then hide Explorer's icons and show ours,
        // so the desktop is never blank while a large folder is being read.
        // know about a full-screen game before the first draw (resume can now come from
        // the keyboard shortcut while a game is in front)
        if let Some(m) = desktop::pick_monitor(&self.cfg.monitor) {
            self.mon_rect = m.rect;
        }
        self.update_suspend();
        self.apply_monitor();
        self.rescan(false);
        if self.scan_failures == 0 {
            self.come_back();
        }
    }

    fn pause(&mut self) {
        unsafe {
            for t in [TIMER_ANIM, TIMER_RESCAN, TIMER_SUSPEND, TIMER_TRIM, TIMER_HIDE_RETRY, TIMER_GPU_RETRY, TIMER_RESTORE_RETRY, TIMER_REPLACE] {
                let _ = KillTimer(self.hwnd, t);
            }
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.paused = true;
        self.suspended = false;
        self.gpu_down = false;
        self.scan_down = false;
        self.restore_retries = 0;
        self.restore_explorer_icons();
        self.renderer = None; // free GPU memory while paused
        self.requested.clear();
        self.hover = None;
        self.scrub = None;
        trim();
    }

    /// Stop drawing while a fullscreen app (game, video) covers our monitor.
    fn update_suspend(&mut self) {
        let fs = desktop::fullscreen_on(&self.mon_rect, self.hwnd);
        if fs && !self.suspended {
            self.suspended = true;
            unsafe {
                let _ = KillTimer(self.hwnd, TIMER_ANIM);
                SetTimer(self.hwnd, TIMER_SUSPEND, 5000, None);
            }
            self.anim.finish();
        } else if !fs && self.suspended {
            self.suspended = false;
            unsafe {
                let _ = KillTimer(self.hwnd, TIMER_SUSPEND);
            }
            if self.needs_rescan {
                self.rescan(false);
            } else if self.dirty {
                self.render();
            }
        }
    }

    fn on_foreground(&mut self) {
        if self.paused {
            return;
        }
        self.replace_now();
        // Explorer may still be rearranging its desktop windows (Win+D): place once more shortly after.
        unsafe {
            SetTimer(self.hwnd, TIMER_REPLACE, 250, None);
        }
        self.check_device();
        self.update_suspend();
    }

    fn replace_now(&mut self) {
        if self.paused {
            return;
        }
        if desktop::foreground_is_desktop() {
            // "Show desktop" (Win+D) raised the desktop: stay visible on top of it.
            self.desktop_mode = true;
            place(self.hwnd, true);
        } else {
            self.desktop_mode = false;
            place(self.hwnd, false);
        }
    }

    fn hit(&self, x: f32, y: f32) -> Option<Entry> {
        let g = &self.geom;
        for (i, e) in self.entries.iter().enumerate() {
            let v = self.anim.cur.get(&e.key).copied().unwrap_or_else(|| {
                let (x, y) = g.cell_pos(i);
                Vis { x, y, a: 1.0, sc: 1.0 }
            });
            let (l, t) = (v.x + 6.0 * g.s, v.y);
            let (r, b) = (v.x + g.cell_w - 6.0 * g.s, v.y + g.cell_h - 4.0 * g.s);
            if x >= l && x < r && y >= t && y < b {
                return Some(e.clone());
            }
        }
        None
    }

    fn paths_of(&self, e: &Entry) -> Vec<String> {
        match &e.r {
            EntryRef::File(i) => self.model.files.get(*i).map(|f| vec![f.path.clone()]).unwrap_or_default(),
            EntryRef::Stack(k) => self
                .model
                .groups
                .iter()
                .find(|(gk, _)| gk == k)
                .map(|(_, idx)| idx.iter().map(|&i| self.model.files[i].path.clone()).collect())
                .unwrap_or_default(),
        }
    }

    fn toggle_stack(&mut self, k: Kind) {
        if !self.expanded.remove(&k) {
            self.expanded.insert(k);
        }
        self.scrub = None;
        self.relayout(true);
    }

    fn to_screen(&self, x: i32, y: i32) -> POINT {
        let mut p = POINT { x, y };
        unsafe {
            let _ = ClientToScreen(self.hwnd, &mut p);
        }
        p
    }

    fn label_rect_screen(&self, key: &str) -> Option<RECT> {
        let i = self.entries.iter().position(|e| e.key == key)?;
        let v = self.anim.cur.get(key).copied().unwrap_or_else(|| {
            let (x, y) = self.geom.cell_pos(i);
            Vis { x, y, a: 1.0, sc: 1.0 }
        });
        let g = &self.geom;
        let tl = self.to_screen((v.x - 10.0 * g.s) as i32, (v.y + g.icon + 10.0 * g.s) as i32);
        Some(RECT { left: tl.x, top: tl.y, right: tl.x + (g.cell_w + 20.0 * g.s) as i32, bottom: tl.y + (24.0 * g.s) as i32 })
    }

    // ---------- mouse ----------

    fn on_mouse_move(&mut self, x: i32, y: i32) -> Action {
        if self.suspended {
            self.update_suspend();
        }
        if !self.tracking {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            unsafe {
                let _ = TrackMouseEvent(&mut tme);
            }
            self.tracking = true;
        }
        if let Some(md) = &self.mouse_down {
            let (dx, dy) = unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) };
            if md.key.is_some() && ((x - md.x).abs() > dx || (y - md.y).abs() > dy) {
                let key = md.key.clone().unwrap_or_default();
                self.mouse_down = None;
                unsafe {
                    let _ = ReleaseCapture();
                }
                if let Some(e) = self.entries.iter().find(|e| e.key == key).cloned() {
                    let paths = self.paths_of(&e);
                    if !paths.is_empty() {
                        return Action::DragOut(paths);
                    }
                }
            }
            return Action::None;
        }
        let hit = self.hit(x as f32, y as f32).map(|e| e.key);
        if hit != self.hover {
            if let Some((k, _)) = self.scrub {
                if hit.as_deref() != Some(layout::stack_key(k).as_str()) {
                    self.scrub = None;
                }
            }
            self.hover = hit;
            self.render();
        }
        Action::None
    }

    fn on_left_down(&mut self, x: i32, y: i32) {
        let key = self.hit(x as f32, y as f32).map(|e| e.key);
        self.mouse_down = Some(MouseDown { x, y, key });
        unsafe {
            SetCapture(self.hwnd);
        }
    }

    fn on_left_up(&mut self, x: i32, y: i32) {
        unsafe {
            let _ = ReleaseCapture();
        }
        let Some(md) = self.mouse_down.take() else { return };
        let hit = self.hit(x as f32, y as f32);
        let same = hit.as_ref().map(|e| e.key.clone()) == md.key;
        match hit {
            Some(e) if same => match e.r {
                EntryRef::Stack(k) => {
                    self.selected = None;
                    self.toggle_stack(k);
                }
                EntryRef::File(_) => {
                    self.selected = Some(e.key);
                    self.render();
                }
            },
            None if self.selected.take().is_some() => self.render(),
            _ => {}
        }
    }

    fn on_double_click(&mut self, x: i32, y: i32) -> Action {
        self.mouse_down = None;
        match self.hit(x as f32, y as f32) {
            // Open only the item the first click selected: if the icons moved between
            // the two clicks (a download just landed), nothing else gets opened.
            Some(Entry { r: EntryRef::File(i), key }) if self.selected.as_deref() == Some(key.as_str()) => {
                self.model.files.get(i).map(|f| Action::Open(f.path.clone())).unwrap_or(Action::None)
            }
            _ => Action::None,
        }
    }

    fn on_right_up(&mut self, x: i32, y: i32) -> Action {
        let pt = self.to_screen(x, y);
        match self.hit(x as f32, y as f32) {
            Some(e) => {
                let paths = self.paths_of(&e);
                let mut move_to = None;
                let stack = match e.r {
                    EntryRef::Stack(k) => Some(k),
                    EntryRef::File(i) => {
                        if let Some(f) = self.model.files.get(i).filter(|f| !f.is_dir && !f.is_system) {
                            move_to = match f.kind {
                                Kind::Apps => Some(Kind::Games),
                                Kind::Games => Some(Kind::Apps),
                                _ => None,
                            };
                        }
                        self.selected = Some(e.key.clone());
                        self.render();
                        None
                    }
                };
                Action::ItemMenu { paths, pt, stack, move_to }
            }
            None => Action::BackgroundMenu { dir: self.dirs[0].clone(), real_desktop: self.include_system, pt },
        }
    }

    fn on_wheel(&mut self, screen_x: i32, screen_y: i32, delta: i32) {
        let mut p = POINT { x: screen_x, y: screen_y };
        unsafe {
            let _ = ScreenToClient(self.hwnd, &mut p);
        }
        if let Some(Entry { r: EntryRef::Stack(k), .. }) = self.hit(p.x as f32, p.y as f32) {
            if !self.expanded.contains(&k) {
                self.scrub_stack(k, delta);
                return;
            }
        }
        // Anywhere else: when there are more icons than fit the screen (small screen,
        // large scaling, a big open stack), the wheel slides the columns sideways.
        let max = self.geom.max_scroll(self.entries.len());
        if max <= 0.0 {
            return;
        }
        let next = (self.geom.scroll - delta as f32 / 120.0 * self.geom.cell_w).clamp(0.0, max);
        if (next - self.geom.scroll).abs() > 0.5 {
            self.geom.scroll = next;
            self.hover = None;
            self.relayout(false);
        }
    }

    /// Wheel over a closed stack: flip through its files.
    fn scrub_stack(&mut self, k: Kind, delta: i32) {
        let len = self.model.groups.iter().find(|(gk, _)| *gk == k).map(|g| g.1.len()).unwrap_or(0);
        if len == 0 {
            return;
        }
        let cur = match self.scrub {
            Some((sk, i)) if sk == k => Some(i),
            _ => None,
        };
        if let Some(next) = layout::scrub_step(cur, len, &mut self.wheel_acc, delta) {
            self.scrub = Some((k, next));
            self.render();
        }
    }

    /// Which shell drop target lies under a screen point.
    fn drop_target_at(&mut self, screen: POINT) -> Option<IDropTarget> {
        let mut p = screen;
        unsafe {
            let _ = ScreenToClient(self.hwnd, &mut p);
        }
        let (key, path) = match self.hit(p.x as f32, p.y as f32) {
            Some(Entry { r: EntryRef::File(i), key }) => {
                // Only folders and system items (Recycle Bin) take a drop. Dropping on a
                // program or shortcut would run it with the files, so those count as desktop.
                let f = &self.model.files[i];
                if f.is_dir || f.is_system {
                    (key, Some(f.path.clone()))
                } else {
                    (String::new(), None)
                }
            }
            _ => (String::new(), None),
        };
        // show which item would receive the drop
        let highlight = path.as_ref().map(|_| key.clone());
        if self.hover != highlight {
            self.hover = highlight;
            self.render();
        }
        if let Some((k, t)) = &self.drop_cache {
            if *k == key {
                return Some(t.clone());
            }
        }
        let t = match path {
            Some(p) => shell::item_drop_target(self.hwnd, &p),
            None => shell::folder_drop_target(self.hwnd, &self.dirs[0]),
        }?;
        self.drop_cache = Some((key, t.clone()));
        Some(t)
    }

    /// Remembers that the user wants this program file in Apps or in Games.
    fn set_kind_choice(&mut self, path: &str, kind: Kind) {
        let lower = path.to_lowercase();
        self.kind_choices.retain(|(p, _)| p.to_lowercase() != lower);
        self.kind_choices.push((path.to_string(), kind));
        let extra = self.kind_choices.len().saturating_sub(tray::MAX_KIND_CHOICES);
        self.kind_choices.drain(..extra);
        model::set_overrides(&self.kind_choices);
        let list: Vec<(String, bool)> = self.kind_choices.iter().map(|(p, k)| (p.clone(), *k == Kind::Games)).collect();
        tray::save_kind_choices(&list);
        self.rescan(true);
    }

    /// Takes finished icons from the worker threads and shows them.
    fn take_icons(&mut self) {
        let mut got = false;
        while let Ok(res) = self.icon_rx.try_recv() {
            if let Some(r) = self.renderer.as_mut() {
                r.add_icon(res.key, res.w, res.h, &res.data);
                got = true;
            }
        }
        if got {
            self.render();
        }
    }

    fn toggle_pause(&mut self) {
        if self.paused {
            self.resume();
        } else {
            self.pause();
        }
        self.cfg.paused = self.paused;
        tray::save_config(&self.cfg);
    }

    fn cleanup(&mut self) {
        unsafe {
            let _ = UnregisterHotKey(self.hwnd, HOTKEY_PAUSE);
        }
        self.restore_explorer_icons();
        tray::remove_tray(self.hwnd);
    }
}

fn trim() {
    unsafe {
        let _ = K32EmptyWorkingSet(GetCurrentProcess());
    }
}

fn pick_drop(screen: POINT) -> Option<IDropTarget> {
    with_app(|a| a.drop_target_at(screen)).flatten()
}

fn run_action(hwnd: HWND, action: Action) {
    match action {
        Action::None => {}
        Action::Open(p) => shell::open(hwnd, &p),
        Action::DragOut(paths) => shell::drag_out(hwnd, &paths),
        Action::BackgroundMenu { dir, real_desktop, pt } => {
            let mut extra = Vec::new();
            if shell::clipboard_has_files() {
                extra.push((CMD_PASTE, i18n::t("Paste", "\u{0644}\u{0635}\u{0642}").to_string()));
            }
            extra.push((CMD_REFRESH, i18n::t("Refresh", "\u{062A}\u{062D}\u{062F}\u{064A}\u{062B}").to_string()));
            match shell::background_menu(hwnd, &dir, real_desktop, pt, &extra) {
                CMD_PASTE => shell::paste_into(hwnd, &dir),
                CMD_REFRESH => {
                    with_app(|a| a.rescan(false));
                }
                _ => {}
            }
        }
        Action::ItemMenu { paths, pt, stack, move_to } => {
            let mut extra: Vec<(usize, String)> = match stack {
                Some(_) => vec![(
                    CMD_TOGGLE_STACK,
                    i18n::t("Open / close stack", "\u{0641}\u{062A}\u{062D} / \u{0625}\u{063A}\u{0644}\u{0627}\u{0642} \u{0627}\u{0644}\u{0643}\u{0648}\u{0645}\u{0629}").to_string(),
                )],
                None => vec![],
            };
            match move_to {
                Some(Kind::Games) => extra.push((CMD_MOVE_KIND, i18n::t("Move to Games stack", "\u{0646}\u{0642}\u{0644} \u{0625}\u{0644}\u{0649} \u{0643}\u{0648}\u{0645}\u{0629} \u{0627}\u{0644}\u{0623}\u{0644}\u{0639}\u{0627}\u{0628}").to_string())),
                Some(Kind::Apps) => extra.push((CMD_MOVE_KIND, i18n::t("Move to Apps stack", "\u{0646}\u{0642}\u{0644} \u{0625}\u{0644}\u{0649} \u{0643}\u{0648}\u{0645}\u{0629} \u{0627}\u{0644}\u{062A}\u{0637}\u{0628}\u{064A}\u{0642}\u{0627}\u{062A}").to_string())),
                _ => {}
            }
            let (outcome, cmd) = shell::item_menu(hwnd, &paths, pt, &extra);
            if cmd == CMD_TOGGLE_STACK {
                if let Some(k) = stack {
                    with_app(|a| a.toggle_stack(k));
                }
            }
            if cmd == CMD_MOVE_KIND && paths.len() == 1 {
                if let Some(k) = move_to {
                    with_app(|a| a.set_kind_choice(&paths[0], k));
                }
            }
            if let shell::MenuOutcome::Rename = outcome {
                if paths.len() == 1 {
                    let info = with_app(|a| (a.label_rect_screen(&layout::file_key(&paths[0])), a.geom.s));
                    if let Some((Some(r), s)) = info {
                        rename::start(hwnd, &paths[0], r, s);
                    }
                }
            }
        }
    }
}

/// Turns the global Ctrl+Alt+D shortcut on or off. False if Windows refused (already taken).
fn set_hotkey(hwnd: HWND, on: bool) -> bool {
    unsafe {
        let _ = UnregisterHotKey(hwnd, HOTKEY_PAUSE);
        !on || RegisterHotKey(hwnd, HOTKEY_PAUSE, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 0x44).is_ok()
    }
}

fn tray_menu(hwnd: HWND) {
    let Some((paused, mons, current, cfg)) = with_app(|a| {
        let mons = desktop::monitors();
        let cur = desktop::pick_monitor(&a.cfg.monitor).map(|m| m.device).unwrap_or_default();
        let mut shown = a.cfg.clone();
        shown.hotkey = a.cfg.hotkey && a.hotkey_ok; // the tick shows what really works now
        (a.paused, mons, cur, shown)
    }) else {
        return;
    };
    let cmd = tray::show_menu(hwnd, paused, &mons, &current, &cfg);
    match cmd {
        tray::ID_PAUSE => {
            with_app(|a| a.toggle_pause());
        }
        tray::ID_FOLDERS => {
            with_app(|a| {
                a.cfg.folders = !a.cfg.folders;
                tray::save_config(&a.cfg);
                a.rescan(true);
            });
        }
        tray::ID_LABEL_BG => {
            with_app(|a| {
                a.cfg.label_bg = !a.cfg.label_bg;
                tray::save_config(&a.cfg);
                a.render();
            });
        }
        tray::ID_HOTKEY => {
            let want = !cfg.hotkey;
            let ok = set_hotkey(hwnd, want);
            if ok {
                with_app(|a| {
                    a.cfg.hotkey = want;
                    a.hotkey_ok = want;
                    tray::save_config(&a.cfg);
                });
            }
            if want && !ok {
                message(i18n::t(
                    "Ctrl+Alt+D is already used by another program, so the shortcut was not turned on.",
                    "\u{0627}\u{0644}\u{0627}\u{062E}\u{062A}\u{0635}\u{0627}\u{0631} Ctrl+Alt+D \u{0645}\u{0633}\u{062A}\u{062E}\u{062F}\u{0645} \u{0641}\u{064A} \u{0628}\u{0631}\u{0646}\u{0627}\u{0645}\u{062C} \u{0622}\u{062E}\u{0631}\u{060C} \u{0644}\u{0630}\u{0644}\u{0643} \u{0644}\u{0645} \u{064A}\u{062A}\u{0645} \u{062A}\u{0641}\u{0639}\u{064A}\u{0644}\u{0647}.",
                ));
            }
        }
        c if (tray::ID_SIZE_BASE..tray::ID_SIZE_BASE + tray::SIZES.len()).contains(&c) => {
            with_app(|a| {
                a.cfg.size = tray::SIZES[c - tray::ID_SIZE_BASE].0;
                tray::save_config(&a.cfg);
                if !a.paused {
                    a.apply_monitor();
                }
            });
        }
        c if c == tray::ID_SIDE_BASE || c == tray::ID_SIDE_BASE + 1 => {
            with_app(|a| {
                a.cfg.left = c == tray::ID_SIDE_BASE + 1;
                tray::save_config(&a.cfg);
                if !a.paused {
                    a.apply_monitor();
                }
            });
        }
        tray::ID_AUTOSTART => tray::set_autostart(!tray::autostart_enabled()),
        tray::ID_EXIT => unsafe {
            with_app(|a| a.cleanup());
            let _ = DestroyWindow(hwnd);
        },
        c if (tray::ID_LANG_BASE..tray::ID_LANG_BASE + 3).contains(&c) => {
            let choice = [i18n::Lang::Auto, i18n::Lang::Ar, i18n::Lang::En][c - tray::ID_LANG_BASE];
            i18n::set(choice);
            with_app(|a| {
                a.cfg.lang = choice;
                tray::save_config(&a.cfg);
                a.relayout(false); // stack names change: redraw
            });
        }
        c if c >= tray::ID_MONITOR_BASE && c < tray::ID_MONITOR_BASE + mons.len() => {
            let dev = mons[c - tray::ID_MONITOR_BASE].device.clone();
            with_app(|a| {
                a.cfg.monitor = dev;
                tray::save_config(&a.cfg);
                if !a.paused {
                    a.apply_monitor();
                }
            });
        }
        _ => {}
    }
}

fn lparam_xy(lp: LPARAM) -> (i32, i32) {
    ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32)
}

fn forward_menu_msg(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    shell::ACTIVE_MENU.with(|m| {
        let guard = m.try_borrow().ok()?;
        let cm = guard.as_ref()?;
        unsafe {
            if let Ok(cm3) = cm.cast::<IContextMenu3>() {
                let mut res = LRESULT(0);
                cm3.HandleMenuMsg2(msg, wp, lp, Some(&mut res)).ok().map(|_| res)
            } else {
                cm.HandleMenuMsg(msg, wp, lp).ok().map(|_| LRESULT(0))
            }
        }
    })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_WINDOWPOSCHANGING => {
            // Never let anything raise us above normal windows (we live on the desktop).
            if !PLACING.load(Ordering::SeqCst) {
                let pos = lp.0 as *mut WINDOWPOS;
                if !pos.is_null() {
                    (*pos).flags |= SWP_NOZORDER;
                }
            }
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_CAPTURECHANGED_ => {
            with_app(|a| a.mouse_down = None);
        }
        WM_MOUSEMOVE => {
            let (x, y) = lparam_xy(lp);
            if wp.0 & MK_LBUTTON_ == 0 {
                with_app(|a| a.mouse_down = None);
            }
            let action = with_app(|a| a.on_mouse_move(x, y)).unwrap_or(Action::None);
            run_action(hwnd, action);
            return LRESULT(0);
        }
        WM_MOUSELEAVE => {
            with_app(|a| {
                a.tracking = false;
                if a.hover.is_some() || a.scrub.is_some() {
                    a.hover = None;
                    a.scrub = None;
                    a.render();
                }
            });
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => {
            let (x, y) = lparam_xy(lp);
            with_app(|a| a.on_left_down(x, y));
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            let (x, y) = lparam_xy(lp);
            with_app(|a| a.on_left_up(x, y));
            return LRESULT(0);
        }
        WM_LBUTTONDBLCLK => {
            let (x, y) = lparam_xy(lp);
            let action = with_app(|a| a.on_double_click(x, y)).unwrap_or(Action::None);
            run_action(hwnd, action);
            return LRESULT(0);
        }
        WM_RBUTTONUP => {
            let (x, y) = lparam_xy(lp);
            let action = with_app(|a| a.on_right_up(x, y)).unwrap_or(Action::None);
            run_action(hwnd, action);
            return LRESULT(0);
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let (x, y) = lparam_xy(lp);
            let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as i32;
            // tilt wheel: the view follows the tilt on either screen edge
            let tilt = msg == WM_MOUSEHWHEEL;
            with_app(|a| a.on_wheel(x, y, if tilt && a.geom.left { -delta } else { delta }));
            return LRESULT(0);
        }
        WM_TIMER => {
            let id = wp.0;
            if id == TIMER_REPLAY {
                replay_pending(hwnd);
                return LRESULT(0);
            }
            with_app(|a| match id {
                TIMER_ANIM => {
                    let running = a.anim.tick();
                    a.render();
                    if !running {
                        let _ = KillTimer(hwnd, TIMER_ANIM);
                    }
                }
                TIMER_RESCAN => {
                    let _ = KillTimer(hwnd, TIMER_RESCAN);
                    a.rescan(true);
                }
                TIMER_SUSPEND => a.update_suspend(),
                TIMER_TRIM => {
                    let _ = KillTimer(hwnd, TIMER_TRIM);
                    if !a.anim.active {
                        trim();
                    }
                }
                TIMER_GPU_RETRY => a.gpu_retry(),
                TIMER_RESTORE_RETRY => {
                    let _ = KillTimer(hwnd, TIMER_RESTORE_RETRY);
                    if a.paused || a.gpu_down || a.scan_down {
                        a.ensure_restored();
                    }
                }
                TIMER_HIDE_RETRY => {
                    let _ = KillTimer(hwnd, TIMER_HIDE_RETRY);
                    a.come_back();
                }
                TIMER_REPLACE => {
                    let _ = KillTimer(hwnd, TIMER_REPLACE);
                    a.replace_now();
                }
                _ => {}
            });
            return LRESULT(0);
        }
        WM_APP_ICON => {
            now_or_later(hwnd, PEND_ICONS);
            return LRESULT(0);
        }
        WM_APP_FS => {
            // debounce: rescan after the folder has been quiet for a moment
            SetTimer(hwnd, TIMER_RESCAN, 250, None);
            return LRESULT(0);
        }
        WM_APP_FG => {
            now_or_later(hwnd, PEND_FOREGROUND);
            return LRESULT(0);
        }
        WM_HOTKEY => {
            if wp.0 as i32 == HOTKEY_PAUSE {
                with_app(|a| {
                    // only while the user has the shortcut switched on (another program could
                    // send this message), and not in the middle of a click or drag
                    if a.cfg.hotkey && a.hotkey_ok && a.mouse_down.is_none() {
                        a.toggle_pause();
                    }
                });
            }
            return LRESULT(0);
        }
        WM_APP_TRAY => {
            let ev = (lp.0 & 0xFFFF) as u32;
            if ev == WM_RBUTTONUP || ev == WM_LBUTTONUP {
                tray_menu(hwnd);
            }
            return LRESULT(0);
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            now_or_later(hwnd, PEND_MONITOR);
            return LRESULT(0);
        }
        WM_SETTINGCHANGE => {
            if wp.0 == SPI_SETWORKAREA.0 as usize {
                now_or_later(hwnd, PEND_MONITOR);
            }
            // Display names ("show extensions") or desktop icon settings may have changed.
            // lParam is deliberately not read (it is a pointer supplied by the sender);
            // a rescan is cheap because names are cached.
            SetTimer(hwnd, TIMER_RESCAN, 400, None);
        }
        WM_POWERBROADCAST => {
            if wp.0 == PBT_APMRESUMEAUTOMATIC {
                // woke from sleep: make sure the layer is still there
                now_or_later(hwnd, PEND_DEVICE);
            }
        }
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR => {
            if let Some(r) = forward_menu_msg(msg, wp, lp) {
                return r;
            }
        }
        WM_ENDSESSION => {
            if wp.0 != 0 {
                restore_icons();
            }
            return LRESULT(0);
        }
        WM_DESTROY => {
            let _ = RevokeDragDrop(hwnd);
            PostQuitMessage(0);
            return LRESULT(0);
        }
        _ => {
            let tc = TASKBAR_CREATED.load(Ordering::SeqCst) as u32;
            if tc != 0 && msg == tc {
                // Explorer restarted: it shows its icons again and lost our tray icon.
                tray::add_tray(hwnd);
                with_app(|a| {
                    a.hide_retries = 0;
                    a.come_back();
                });
                return LRESULT(0);
            }
        }
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

unsafe extern "system" fn win_event(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    let h = MAIN_HWND.load(Ordering::SeqCst);
    if h != 0 {
        let _ = PostMessageW(HWND(h as *mut _), WM_APP_FG, WPARAM(0), LPARAM(0));
    }
}

pub(crate) fn known_folder(id: &windows::core::GUID) -> Option<String> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn spawn_watcher(dir: String, hwnd: usize) {
    std::thread::spawn(move || unsafe {
        let mut buf = vec![0u32; 4096];
        loop {
            // (Re)open the folder; if it is missing or redirected, wait and try again.
            let Ok(h) = CreateFileW(
                &HSTRING::from(dir.as_str()),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            ) else {
                std::thread::sleep(std::time::Duration::from_secs(10));
                continue;
            };
            // catch up on anything that changed while the folder was not watched
            let _ = PostMessageW(HWND(hwnd as *mut _), WM_APP_FS, WPARAM(0), LPARAM(0));
            loop {
                let mut ret = 0u32;
                let ok = ReadDirectoryChangesW(
                    h,
                    buf.as_mut_ptr() as *mut _,
                    (buf.len() * 4) as u32,
                    false,
                    FILE_NOTIFY_CHANGE_FILE_NAME
                        | FILE_NOTIFY_CHANGE_DIR_NAME
                        | FILE_NOTIFY_CHANGE_LAST_WRITE
                        | FILE_NOTIFY_CHANGE_ATTRIBUTES,
                    Some(&mut ret),
                    None,
                    None,
                );
                if ok.is_err() {
                    break;
                }
                let _ = PostMessageW(HWND(hwnd as *mut _), WM_APP_FS, WPARAM(0), LPARAM(0));
            }
            let _ = CloseHandle(h);
            let _ = PostMessageW(HWND(hwnd as *mut _), WM_APP_FS, WPARAM(0), LPARAM(0));
            std::thread::sleep(std::time::Duration::from_secs(10));
        }
    });
}

/// Second tiny process: waits for the main process to end and, if it ended
/// without restoring the normal desktop icons (crash, killed), restores them.
fn run_watchdog(pid: u32) {
    unsafe {
        let _ = SetPriorityClass(GetCurrentProcess(), IDLE_PRIORITY_CLASS);
        let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return; // cannot watch it: never restore while it may still be running
        };
        WaitForSingleObject(h, INFINITE);
        let _ = CloseHandle(h);
        for _ in 0..60 {
            // the marker now belongs to a newer run of the app: its icons are not ours to restore
            if tray::marker_owner().is_some_and(|owner| owner != pid) {
                return;
            }
            if restore_icons() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }
}

fn spawn_watchdog() {
    use std::os::windows::process::CommandExt;
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .arg("--watchdog")
            .arg(std::process::id().to_string())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn();
    }
}

fn set_low_impact() {
    unsafe {
        let me = GetCurrentProcess();
        let _ = SetPriorityClass(me, BELOW_NORMAL_PRIORITY_CLASS);
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        };
        let _ = SetProcessInformation(
            me,
            ProcessPowerThrottling,
            &state as *const _ as *const _,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

fn message(text: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), w!("Desktop Stacks"), MB_OK | MB_ICONINFORMATION);
    }
}

fn main() {
    harden::restrict_dll_search();
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--watchdog" {
        if let Ok(pid) = args[2].parse::<u32>() {
            run_watchdog(pid);
        }
        return;
    }
    // The shell only accepts full paths without "." or ".." parts (icons, menus and opening
    // files all go through it), so a relative or dotted --dir is made absolute first.
    let test_dir = args
        .iter()
        .position(|a| a == "--dir")
        .and_then(|i| args.get(i + 1))
        .map(|d| std::path::absolute(d).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| d.clone()));

    unsafe {
        harden::set_dpi_awareness();
        let _mutex = CreateMutexW(None, true, w!("Local\\DesktopStacks.SingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            message("Desktop Stacks is already running (see the tray icon).");
            return;
        }
        set_low_impact();
        let _ = OleInitialize(None);

        // Safety net: if anything panics, give the user the normal desktop back.
        let _ = MAIN_THREAD.set(std::thread::current().id());
        std::panic::set_hook(Box::new(|_| {
            if MAIN_THREAD.get() == Some(&std::thread::current().id()) {
                restore_icons();
            }
        }));

        let (dirs, include_system) = match &test_dir {
            Some(d) => (vec![d.trim_end_matches('\\').to_string()], false),
            None => {
                let mut v = Vec::new();
                if let Some(d) = known_folder(&FOLDERID_Desktop) {
                    v.push(d);
                }
                if let Some(d) = known_folder(&FOLDERID_PublicDesktop) {
                    v.push(d);
                }
                (v, true)
            }
        };
        if dirs.is_empty() || !std::path::Path::new(&dirs[0]).is_dir() {
            message("Desktop folder not found.");
            return;
        }

        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("DesktopStacksWindow");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
            class,
            w!("Desktop Stacks"),
            WS_POPUP,
            0,
            0,
            100,
            100,
            None,
            None,
            hinst,
            None,
        ) else {
            message("Could not create the window.");
            return;
        };
        MAIN_HWND.store(hwnd.0 as usize, Ordering::SeqCst);
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")) as usize, Ordering::SeqCst);
        let _ = harden::dwm_set_window_flag(hwnd, DWMWA_EXCLUDED_FROM_PEEK, true);
        let _ = harden::dwm_set_window_flag(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED, true);

        spawn_watchdog();

        let (req_tx, req_rx) = channel();
        let (res_tx, res_rx) = channel();
        icons::spawn(hwnd.0 as usize, req_rx, res_tx);
        for d in &dirs {
            spawn_watcher(d.clone(), hwnd.0 as usize);
        }

        let cfg = tray::load_config();
        i18n::set(cfg.lang);
        let paused = cfg.paused;
        // taken by another program this time: shown unticked, but the saved wish is kept
        let hotkey_ok = cfg.hotkey && set_hotkey(hwnd, true);
        let kind_choices: Vec<(String, Kind)> = tray::load_kind_choices()
            .into_iter()
            .map(|(p, games)| (p, if games { Kind::Games } else { Kind::Apps }))
            .collect();
        model::set_overrides(&kind_choices);
        let app = App {
            hwnd,
            dirs,
            include_system,
            cfg,
            kind_choices,
            hotkey_ok,
            model: Model { files: vec![], groups: vec![], loose_tail: vec![] },
            expanded: HashSet::new(),
            entries: vec![],
            anim: Animator::new(),
            geom: Geom::new(100.0, 100.0, 1.0),
            mon_rect: RECT::default(),
            work: RECT::default(),
            renderer: None,
            icon_tx: req_tx,
            icon_rx: res_rx,
            requested: HashSet::new(),
            hover: None,
            selected: None,
            scrub: None,
            wheel_acc: 0,
            mouse_down: None,
            tracking: false,
            suspended: false,
            dirty: false,
            needs_rescan: false,
            paused,
            desktop_mode: false,
            gpu_down: false,
            gpu_wait_ms: 5000,
            gpu_soft: false,
            scan_down: false,
            scan_failures: 0,
            restore_retries: 0,
            hide_retries: 0,
            drop_cache: None,
        };
        APP.with(|a| *a.borrow_mut() = Some(app));

        let drop_target = shell::DropTarget::create(hwnd, pick_drop);
        let _ = RegisterDragDrop(hwnd, &drop_target);
        tray::add_tray(hwnd);

        with_app(|a| {
            if a.paused {
                a.pause();
            } else {
                a.resume();
            }
        });

        let hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(win_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnhookWinEvent(hook);
        with_app(|a| a.cleanup());
        APP.with(|a| *a.borrow_mut() = None);
        drop(drop_target);
    }
}

