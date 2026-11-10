// Small in-place rename box (used for the "Rename" item of the right-click menu).

use std::cell::RefCell;
use windows::core::{w, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateFontW, DeleteObject, HFONT, HGDIOBJ};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::Shell::SIGDN_PARENTRELATIVEEDITING;
use windows::Win32::UI::WindowsAndMessaging::*;

struct State {
    edit: HWND,
    owner: HWND,
    old_proc: isize,
    path: String,
    original: String,
    font: HFONT,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

const EM_SETSEL: u32 = 0x00B1;
const ES_CENTER: u32 = 0x0001;
const ES_AUTOHSCROLL: u32 = 0x0080;

pub fn start(owner: HWND, path: &str, screen: RECT, scale: f32) {
    finish(false);
    let Some(item) = crate::shell::item(path) else { return };
    let current = unsafe {
        match item.GetDisplayName(SIGDN_PARENTRELATIVEEDITING) {
            Ok(p) => {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                s
            }
            Err(_) => return,
        }
    };
    unsafe {
        let style = WINDOW_STYLE(WS_POPUP.0 | WS_BORDER.0 | ES_CENTER | ES_AUTOHSCROLL);
        let Ok(edit) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            w!("EDIT"),
            &HSTRING::from(current.as_str()),
            style,
            screen.left,
            screen.top,
            (screen.right - screen.left).max(60),
            (screen.bottom - screen.top).max(20),
            owner,
            None,
            None,
            None,
        ) else {
            return;
        };
        let font = CreateFontW(
            -(13.0 * scale) as i32,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            1,
            0,
            0,
            5,
            0,
            w!("Segoe UI"),
        );
        SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        let old = SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as isize);
        STATE.with(|s| {
            *s.borrow_mut() = Some(State { edit, owner, old_proc: old, path: path.to_string(), original: current.clone(), font })
        });
        // select the name without its extension, like Explorer
        let stem = current.rfind('.').filter(|&i| i > 0).unwrap_or(current.len());
        let stem16 = current[..stem].encode_utf16().count();
        SendMessageW(edit, EM_SETSEL, WPARAM(0), LPARAM(stem16 as isize));
        let _ = ShowWindow(edit, SW_SHOW);
        let _ = SetForegroundWindow(edit);
        let _ = SetFocus(edit);
    }
}

fn finish(commit: bool) {
    // Read the typed text first, while the edit control is still fully wired up
    // (the text request travels through edit_proc, which needs STATE).
    let Some(edit) = STATE.with(|s| s.borrow().as_ref().map(|st| st.edit)) else { return };
    let mut buf = [0u16; 520];
    let n = unsafe { GetWindowTextW(edit, &mut buf) }.max(0) as usize;
    let text = String::from_utf16_lossy(&buf[..n]).trim().to_string();
    let st = STATE.with(|s| s.borrow_mut().take());
    let Some(st) = st else { return };
    unsafe {
        SetWindowLongPtrW(st.edit, GWLP_WNDPROC, st.old_proc);
        let _ = DestroyWindow(st.edit);
        let _ = DeleteObject(HGDIOBJ(st.font.0));
        if commit && !text.is_empty() && text != st.original {
            crate::shell::rename(st.owner, &st.path, &text);
        }
    }
}

unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let old = STATE.with(|s| s.borrow().as_ref().map(|st| st.old_proc));
    match msg {
        WM_KEYDOWN if wp.0 == VK_RETURN.0 as usize => {
            finish(true);
            return LRESULT(0);
        }
        WM_KEYDOWN if wp.0 == VK_ESCAPE.0 as usize => {
            finish(false);
            return LRESULT(0);
        }
        WM_CHAR if wp.0 == 13 || wp.0 == 27 => return LRESULT(0),
        WM_KILLFOCUS => {
            finish(true);
            return LRESULT(0);
        }
        _ => {}
    }
    match old {
        Some(p) if p != 0 => {
            let f: WNDPROC = std::mem::transmute::<isize, WNDPROC>(p);
            CallWindowProcW(f, hwnd, msg, wp, lp)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
