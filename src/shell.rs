// Windows shell integration. Every file action goes through Windows' own shell,
// so menus, drag and drop, delete, copy, "Open with" etc. behave like Explorer.

use std::cell::RefCell;
use windows::core::{implement, Interface, Result, HSTRING, PCSTR, PCWSTR, PSTR};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, POINTL, WPARAM};
use windows::Win32::System::Com::{CoCreateInstance, IDataObject, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE,
    DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

thread_local! {
    /// The shell menu currently on screen, so owner-drawn submenus ("Send to", "Open with") work.
    pub static ACTIVE_MENU: RefCell<Option<IContextMenu2>> = const { RefCell::new(None) };
}

pub fn item(path: &str) -> Option<IShellItem> {
    unsafe { SHCreateItemFromParsingName(&HSTRING::from(path), None).ok() }
}

struct Pidl(*mut ITEMIDLIST);
impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { ILFree(Some(self.0 as *const _)) }
    }
}
fn pidl(path: &str) -> Option<Pidl> {
    unsafe {
        let mut p: *mut ITEMIDLIST = std::ptr::null_mut();
        SHParseDisplayName(&HSTRING::from(path), None, &mut p, 0, None).ok()?;
        if p.is_null() {
            None
        } else {
            Some(Pidl(p))
        }
    }
}

thread_local! {
    /// The item opened last. ShellExecuteEx may finish in the background (SEE_MASK_ASYNCOK),
    /// so its item id list is kept alive until the next open instead of being freed at once.
    static LAST_OPENED: RefCell<Option<Pidl>> = const { RefCell::new(None) };
}

pub fn open(hwnd: HWND, path: &str) {
    if let Some(p) = pidl(path) {
        unsafe {
            let mut info = SHELLEXECUTEINFOW {
                cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
                fMask: SEE_MASK_IDLIST | SEE_MASK_ASYNCOK,
                hwnd,
                lpIDList: p.0 as *mut _,
                nShow: SW_SHOWNORMAL.0,
                ..Default::default()
            };
            let _ = ShellExecuteExW(&mut info);
        }
        LAST_OPENED.with(|l| *l.borrow_mut() = Some(p));
    }
}

fn items_array(paths: &[String]) -> Option<IShellItemArray> {
    let pidls: Vec<Pidl> = paths.iter().filter_map(|p| pidl(p)).collect();
    if pidls.is_empty() {
        return None;
    }
    let raw: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| p.0 as *const _).collect();
    unsafe { SHCreateShellItemArrayFromIDLists(&raw).ok() }
}

pub fn folder_of(dir: &str) -> Option<IShellFolder> {
    let it = item(dir)?;
    unsafe { it.BindToHandler(None, &BHID_SFObject).ok() }
}

pub enum MenuOutcome {
    None,
    Rename,
}

/// A delete chosen for several files at once (right-click on a closed stack) is asked
/// about first: on a default Windows the Recycle Bin does not ask, and most files of a
/// closed stack are not even visible.
fn needs_delete_confirmation(verb: &str, count: usize) -> bool {
    count > 1 && verb.eq_ignore_ascii_case("delete")
}

fn confirm_delete(hwnd: HWND, count: usize) -> bool {
    let text = if crate::i18n::is_arabic() {
        format!("\u{062D}\u{0630}\u{0641} \u{0643}\u{0644} \u{0627}\u{0644}\u{0639}\u{0646}\u{0627}\u{0635}\u{0631} \u{0641}\u{064A} \u{0647}\u{0630}\u{0647} \u{0627}\u{0644}\u{0643}\u{0648}\u{0645}\u{0629} (\u{0627}\u{0644}\u{0639}\u{062F}\u{062F}: {count})\u{061F}")
    } else {
        format!("Delete all {count} items in this stack?")
    };
    let mut style = MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_SETFOREGROUND;
    if crate::i18n::is_arabic() {
        style |= MB_RTLREADING | MB_RIGHT;
    }
    // "No" is the default button, so Enter or Esc deletes nothing.
    unsafe { MessageBoxW(hwnd, &HSTRING::from(text), windows::core::w!("Desktop Stacks"), style) == IDYES }
}

/// Shows a shell context menu and runs the chosen command (except "rename", which we handle).
/// `count` is the number of files the menu is for (0 for the desktop background).
fn run_menu(hwnd: HWND, cm: IContextMenu, pt: POINT, extra: &[(usize, String)], count: usize) -> (MenuOutcome, usize) {
    const FIRST: u32 = 1;
    const LAST: u32 = 0x6FFF;
    unsafe {
        let menu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(_) => return (MenuOutcome::None, 0),
        };
        for (id, label) in extra {
            let _ = AppendMenuW(menu, MF_STRING, *id, &HSTRING::from(label.as_str()));
        }
        if !extra.is_empty() {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        }
        let pos = GetMenuItemCount(menu).max(0) as u32;
        if cm.QueryContextMenu(menu, pos, FIRST, LAST, CMF_NORMAL | CMF_CANRENAME).is_err() {
            let _ = DestroyMenu(menu);
            return (MenuOutcome::None, 0);
        }
        ACTIVE_MENU.with(|m| *m.borrow_mut() = cm.cast::<IContextMenu2>().ok());
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenuEx(menu, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, pt.x, pt.y, hwnd, None).0 as u32;
        ACTIVE_MENU.with(|m| *m.borrow_mut() = None);
        let _ = DestroyMenu(menu);
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        if cmd == 0 {
            return (MenuOutcome::None, 0);
        }
        if !(FIRST..=LAST).contains(&cmd) {
            return (MenuOutcome::None, cmd as usize);
        }
        let offset = (cmd - FIRST) as usize;
        let mut wide = [0u16; 64];
        let got = cm.GetCommandString(offset, GCS_VERBW, None, PSTR(wide.as_mut_ptr() as *mut u8), wide.len() as u32);
        let n = wide.iter().position(|&c| c == 0).unwrap_or(0);
        let mut verb = String::from_utf16_lossy(&wide[..n]);
        if got.is_err() || verb.is_empty() {
            // some menu handlers only answer in the older, non-Unicode form
            let mut narrow = [0u8; 64];
            if cm.GetCommandString(offset, GCS_VERBA, None, PSTR(narrow.as_mut_ptr()), narrow.len() as u32).is_ok() {
                let n = narrow.iter().position(|&c| c == 0).unwrap_or(0);
                verb = String::from_utf8_lossy(&narrow[..n]).into_owned();
            }
        }
        if verb.eq_ignore_ascii_case("rename") {
            return (MenuOutcome::Rename, 0);
        }
        if needs_delete_confirmation(&verb, count) && !confirm_delete(hwnd, count) {
            return (MenuOutcome::None, 0);
        }
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: 0x4000 | CMIC_MASK_PTINVOKE, // CMIC_MASK_UNICODE
            hwnd,
            lpVerb: PCSTR(offset as *const u8),
            lpVerbW: PCWSTR(offset as *const u16),
            nShow: SW_SHOWNORMAL.0,
            ptInvoke: pt,
            ..Default::default()
        };
        let _ = cm.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO);
        (MenuOutcome::None, 0)
    }
}

/// Right-click on one or more files.
pub fn item_menu(hwnd: HWND, paths: &[String], pt: POINT, extra: &[(usize, String)]) -> (MenuOutcome, usize) {
    let cm: Option<IContextMenu> = if paths.len() == 1 {
        item(&paths[0]).and_then(|i| unsafe { i.BindToHandler(None, &BHID_SFUIObject).ok() })
    } else {
        items_array(paths).and_then(|a| unsafe { a.BindToHandler(None, &BHID_SFUIObject).ok() })
    };
    match cm {
        Some(cm) => run_menu(hwnd, cm, pt, extra, paths.len()),
        None => {
            // Fall back to only our own items.
            unsafe {
                let Ok(menu) = CreatePopupMenu() else { return (MenuOutcome::None, 0) };
                for (id, label) in extra {
                    let _ = AppendMenuW(menu, MF_STRING, *id, &HSTRING::from(label.as_str()));
                }
                let _ = SetForegroundWindow(hwnd);
                let cmd = TrackPopupMenuEx(menu, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, pt.x, pt.y, hwnd, None).0;
                let _ = DestroyMenu(menu);
                let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
                (MenuOutcome::None, cmd as usize)
            }
        }
    }
}

/// Right-click on empty desktop: the folder's own menu (New, Paste, Display settings, ...).
/// Right-click on empty desktop. On the real desktop this is the desktop's own
/// background menu (Display settings, Personalize, graphics driver items, New...),
/// taken from the shell's root Desktop folder. In test mode (--dir) it is the
/// test folder's menu. `extra` items (Paste, Refresh) go on top, like Explorer.
/// Returns the chosen extra command id, or 0.
pub fn background_menu(hwnd: HWND, dir: &str, real_desktop: bool, pt: POINT, extra: &[(usize, String)]) -> usize {
    let cm: Option<IContextMenu> = unsafe {
        let root = if real_desktop {
            SHGetDesktopFolder().ok().and_then(|f| f.CreateViewObject::<_, IContextMenu>(hwnd).ok())
        } else {
            None
        };
        // fallback: the folder's own background menu
        root.or_else(|| folder_of(dir).and_then(|f| f.CreateViewObject::<_, IContextMenu>(hwnd).ok()))
    };
    match cm {
        Some(cm) => run_menu(hwnd, cm, pt, extra, 0).1,
        None => 0,
    }
}

/// True when the clipboard holds files (from Copy or Cut in Explorer or another app).
pub fn clipboard_has_files() -> bool {
    const CF_HDROP: u32 = 15;
    unsafe { windows::Win32::System::DataExchange::IsClipboardFormatAvailable(CF_HDROP).is_ok() }
}

/// Copy or Cut, as the source asked ("Preferred DropEffect"); copy if unknown.
fn preferred_effect(data: &IDataObject) -> DROPEFFECT {
    use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::ReleaseStgMedium;
    unsafe {
        let cf = windows::Win32::System::DataExchange::RegisterClipboardFormatW(windows::core::w!("Preferred DropEffect"));
        if cf == 0 {
            return DROPEFFECT_COPY;
        }
        let fmt = FORMATETC {
            cfFormat: cf as u16,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let Ok(mut medium) = data.GetData(&fmt) else { return DROPEFFECT_COPY };
        let mut eff = DROPEFFECT_COPY;
        if medium.tymed == TYMED_HGLOBAL.0 as u32 {
            let h = medium.u.hGlobal;
            if GlobalSize(h) >= 4 {
                let p = GlobalLock(h) as *const u32;
                if !p.is_null() {
                    // SAFETY: GlobalSize >= 4 and the block is locked for the read
                    let v = std::ptr::read_unaligned(p);
                    let _ = GlobalUnlock(h);
                    if v & DROPEFFECT_MOVE.0 != 0 {
                        eff = DROPEFFECT_MOVE;
                    }
                }
            }
        }
        ReleaseStgMedium(&mut medium);
        eff
    }
}

/// Paste files from the clipboard into the folder, through the folder's own drop
/// target: the same copy/move engine (progress, conflicts, Undo) Explorer uses.
pub fn paste_into(hwnd: HWND, dir: &str) {
    unsafe {
        // the clipboard can be briefly locked by another app: retry a few times
        let mut data = None;
        for _ in 0..3 {
            if let Ok(d) = windows::Win32::System::Ole::OleGetClipboard() {
                data = Some(d);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let Some(data) = data else { return paste_failed() };
        let Some(target) = folder_drop_target(hwnd, dir) else { return paste_failed() };
        let allowed = preferred_effect(&data);
        let pt = POINTL { x: 0, y: 0 };
        let keys = MODIFIERKEYS_FLAGS(0);
        let mut eff = allowed;
        if target.DragEnter(&data, keys, pt, &mut eff).is_err() || eff == DROPEFFECT_NONE {
            let _ = target.DragLeave();
            return paste_failed();
        }
        let mut eff = allowed;
        if target.Drop(&data, keys, pt, &mut eff).is_err() {
            return paste_failed();
        }
        // After Cut + Paste the files have moved: clear the clipboard like Explorer
        // does, so "Paste" disappears and a second paste can't fail on missing files.
        if allowed == DROPEFFECT_MOVE {
            let _ = windows::Win32::System::Ole::OleSetClipboard(None);
        }
    }
}

/// Paste could not run: give the standard Windows warning sound instead of silence.
fn paste_failed() {
    unsafe {
        let _ = windows::Win32::System::Diagnostics::Debug::MessageBeep(MB_ICONEXCLAMATION);
    }
}

/// Drag files out of the desktop into any app (Windows draws the drag image).
pub fn drag_out(hwnd: HWND, paths: &[String]) {
    let data: Option<IDataObject> = if paths.len() == 1 {
        item(&paths[0]).and_then(|i| unsafe { i.BindToHandler(None, &BHID_DataObject).ok() })
    } else {
        items_array(paths).and_then(|a| unsafe { a.BindToHandler(None, &BHID_DataObject).ok() })
    };
    if let Some(d) = data {
        unsafe {
            let _ = SHDoDragDrop(hwnd, &d, None, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK);
        }
    }
}

/// Renames through the shell (supports Undo, same as Explorer).
pub fn rename(hwnd: HWND, path: &str, new_name: &str) {
    let Some(it) = item(path) else { return };
    unsafe {
        if let Ok(op) = CoCreateInstance::<_, IFileOperation>(&FileOperation, None, CLSCTX_INPROC_SERVER) {
            let _ = op.SetOwnerWindow(hwnd);
            let _ = op.SetOperationFlags(FOF_ALLOWUNDO);
            if op.RenameItem(&it, &HSTRING::from(new_name), None).is_ok() {
                let _ = op.PerformOperations();
            }
        }
    }
}

// ---------- drop target: files dropped on our desktop go to the real desktop folder ----------

pub type DelegateFn = fn(POINT) -> Option<IDropTarget>;

#[implement(IDropTarget)]
pub struct DropTarget {
    hwnd: HWND,
    pick: DelegateFn,
    current: RefCell<Option<IDropTarget>>,
    data: RefCell<Option<IDataObject>>,
    helper: Option<IDropTargetHelper>,
    /// The target in `current` accepted DragEnter. Only then may it get DragOver, Drop or DragLeave.
    entered: std::cell::Cell<bool>,
}

impl DropTarget {
    pub fn create(hwnd: HWND, pick: DelegateFn) -> IDropTarget {
        let helper = unsafe { CoCreateInstance::<_, IDropTargetHelper>(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER).ok() };
        DropTarget { hwnd, pick, current: RefCell::new(None), data: RefCell::new(None), helper, entered: std::cell::Cell::new(false) }.into()
    }
}

fn pt(p: &POINTL) -> POINT {
    POINT { x: p.x, y: p.y }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(&self, data: Option<&IDataObject>, keys: MODIFIERKEYS_FLAGS, p: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        *self.data.borrow_mut() = data.cloned();
        let target = (self.pick)(pt(p));
        self.entered.set(false);
        unsafe {
            let mut eff = *effect;
            match (&target, data) {
                (Some(t), Some(d)) => {
                    let ok = t.DragEnter(d, keys, *p, &mut eff).is_ok();
                    self.entered.set(ok);
                    if !ok {
                        eff = DROPEFFECT_NONE;
                    }
                }
                _ => eff = DROPEFFECT_NONE,
            }
            *effect = eff;
            if let (Some(h), Some(d)) = (&self.helper, data) {
                let _ = h.DragEnter(self.hwnd, d, &pt(p), eff);
            }
        }
        *self.current.borrow_mut() = target;
        Ok(())
    }

    fn DragOver(&self, keys: MODIFIERKEYS_FLAGS, p: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let target = (self.pick)(pt(p));
        let changed = match (&target, &*self.current.borrow()) {
            (Some(a), Some(b)) => a.as_raw() != b.as_raw(),
            (None, None) => false,
            _ => true,
        };
        unsafe {
            let mut eff = *effect;
            if changed {
                if let Some(old) = self.current.borrow().as_ref().filter(|_| self.entered.get()) {
                    let _ = old.DragLeave();
                }
                self.entered.set(false);
                let data = self.data.borrow().clone();
                if let (Some(t), Some(d)) = (&target, &data) {
                    let ok = t.DragEnter(d, keys, *p, &mut eff).is_ok();
                    self.entered.set(ok);
                    if !ok {
                        eff = DROPEFFECT_NONE;
                    }
                } else {
                    eff = DROPEFFECT_NONE;
                }
                *self.current.borrow_mut() = target;
            } else if let Some(t) = self.current.borrow().as_ref().filter(|_| self.entered.get()) {
                if t.DragOver(keys, *p, &mut eff).is_err() {
                    eff = DROPEFFECT_NONE;
                }
            } else {
                eff = DROPEFFECT_NONE;
            }
            *effect = eff;
            if let Some(h) = &self.helper {
                let _ = h.DragOver(&pt(p), eff);
            }
        }
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        unsafe {
            if let Some(t) = self.current.borrow_mut().take().filter(|_| self.entered.get()) {
                let _ = t.DragLeave();
            }
            self.entered.set(false);
            if let Some(h) = &self.helper {
                let _ = h.DragLeave();
            }
        }
        *self.data.borrow_mut() = None;
        Ok(())
    }

    fn Drop(&self, data: Option<&IDataObject>, keys: MODIFIERKEYS_FLAGS, p: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        unsafe {
            let mut eff = *effect;
            let target = self.current.borrow_mut().take().filter(|_| self.entered.get());
            self.entered.set(false);
            match (&target, data) {
                (Some(t), Some(d)) => {
                    if t.Drop(d, keys, *p, &mut eff).is_err() {
                        eff = DROPEFFECT_NONE;
                    }
                }
                _ => eff = DROPEFFECT_NONE,
            }
            *effect = eff;
            if let (Some(h), Some(d)) = (&self.helper, data) {
                let _ = h.Drop(d, &pt(p), eff);
            }
        }
        *self.data.borrow_mut() = None;
        Ok(())
    }
}

/// Drop target of a specific item (a folder, the Recycle Bin, an app shortcut).
pub fn item_drop_target(hwnd: HWND, path: &str) -> Option<IDropTarget> {
    let _ = hwnd;
    item(path).and_then(|i| unsafe { i.BindToHandler(None, &BHID_SFUIObject).ok() })
}

/// Drop target of the folder background (drop onto empty desktop).
pub fn folder_drop_target(hwnd: HWND, dir: &str) -> Option<IDropTarget> {
    folder_of(dir).and_then(|f| unsafe { f.CreateViewObject::<_, IDropTarget>(hwnd).ok() })
}

#[cfg(test)]
mod tests {
    use super::needs_delete_confirmation;

    #[test]
    fn delete_for_several_files_is_asked_about_first() {
        assert!(needs_delete_confirmation("delete", 2));
        assert!(needs_delete_confirmation("Delete", 20));
    }

    #[test]
    fn one_file_and_other_commands_go_straight_to_windows() {
        assert!(!needs_delete_confirmation("delete", 1));
        assert!(!needs_delete_confirmation("delete", 0));
        assert!(!needs_delete_confirmation("cut", 20));
        assert!(!needs_delete_confirmation("properties", 20));
        assert!(!needs_delete_confirmation("", 20));
    }
}
