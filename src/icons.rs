// Background thread that asks Windows for each file's icon or thumbnail
// (the same images Explorer shows) and hands back ready-to-upload pixels.

use std::sync::mpsc::{Receiver, Sender};
use windows::core::HSTRING;
use windows::Win32::Foundation::{HWND, LPARAM, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::Storage::FileSystem::GetFileAttributesW;
use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY, SIIGBF_RESIZETOFIT};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

pub struct IconReq {
    pub key: String,
    pub path: String,
    pub px: i32,
}

pub struct IconRes {
    pub key: String,
    pub w: u32,
    pub h: u32,
    /// Premultiplied BGRA, top-down. Empty on failure.
    pub data: Vec<u8>,
}

/// Two workers share the queue, so one slow file (for example on a network drive
/// that stopped answering) does not hold up every other icon.
pub fn spawn(hwnd: usize, rx: Receiver<IconReq>, tx: Sender<IconRes>) {
    let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
    for _ in 0..2 {
        let (rx, tx) = (rx.clone(), tx.clone());
        std::thread::spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            loop {
                // the lock is held only while waiting for the next request
                let next = match rx.lock() {
                    Ok(queue) => queue.recv(),
                    Err(_) => break,
                };
                let Ok(req) = next else { break };
                let res = load(&req).unwrap_or(IconRes { key: req.key.clone(), w: 0, h: 0, data: Vec::new() });
                if tx.send(res).is_err() {
                    break;
                }
                let _ = PostMessageW(HWND(hwnd as *mut _), crate::WM_APP_ICON, WPARAM(0), LPARAM(0));
            }
        });
    }
}

unsafe fn load(req: &IconReq) -> Option<IconRes> {
    let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(req.path.as_str()), None).ok()?;
    // OneDrive "online-only" files: never trigger a download just to draw a thumbnail.
    let attrs = GetFileAttributesW(&HSTRING::from(req.path.as_str()));
    let cloud = attrs != u32::MAX && attrs & (0x0040_0000 | 0x0004_0000 | 0x0000_1000) != 0;
    let flags = if cloud { SIIGBF_ICONONLY } else { SIIGBF_RESIZETOFIT };
    let hbm = factory.GetImage(SIZE { cx: req.px, cy: req.px }, flags).ok()?;
    let out = read_bitmap(hbm);
    let _ = DeleteObject(HGDIOBJ(hbm.0));
    let (w, h, data) = out?;
    Some(IconRes { key: req.key.clone(), w, h, data })
}

unsafe fn read_bitmap(hbm: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    let mut bm = BITMAP::default();
    if GetObjectW(HGDIOBJ(hbm.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) == 0 {
        return None;
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
    if w <= 0 || h <= 0 || w > 4096 || h > 4096 {
        return None;
    }
    let mut bmi = BITMAPINFO { bmiHeader: BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: -h,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    }, ..Default::default() };
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let dc = CreateCompatibleDC(None);
    let lines = GetDIBits(dc, hbm, 0, h as u32, Some(buf.as_mut_ptr() as *mut _), &mut bmi, DIB_RGB_COLORS);
    let _ = DeleteDC(dc);
    if lines == 0 {
        return None;
    }
    // Some bitmaps carry no alpha at all (photos): make them opaque.
    let any_alpha = buf.chunks_exact(4).any(|p| p[3] != 0);
    if !any_alpha {
        for p in buf.chunks_exact_mut(4) {
            p[3] = 255;
        }
    } else {
        // Premultiply only if the data is straight alpha.
        let straight = buf.chunks_exact(4).any(|p| p[0] > p[3] || p[1] > p[3] || p[2] > p[3]);
        if straight {
            for p in buf.chunks_exact_mut(4) {
                let a = p[3] as u32;
                p[0] = ((p[0] as u32 * a + 127) / 255) as u8;
                p[1] = ((p[1] as u32 * a + 127) / 255) as u8;
                p[2] = ((p[2] as u32 * a + 127) / 255) as u8;
            }
        }
    }
    Some((w as u32, h as u32, buf))
}
