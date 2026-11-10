// GPU drawing: Direct3D11 device -> Direct2D -> DirectComposition surface on our window.
// We only draw when something changes. Between changes Windows just shows the last frame.

use crate::layout::{Geom, Vis};
use crate::model::FileItem;
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use windows::core::{w, Interface, Result, PCWSTR};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;

pub enum DrawKind<'a> {
    File(&'a FileItem),
    Stack { label: &'a str, covers: Vec<&'a FileItem>, expanded: bool, scrub: Option<&'a FileItem>, count: usize },
}

pub struct DrawItem<'a> {
    pub vis: Vis,
    pub kind: DrawKind<'a>,
    pub hover: bool,
    pub selected: bool,
}

pub struct Renderer {
    d3d: ID3D11Device,
    d2d: ID2D1Device,
    res_ctx: ID2D1DeviceContext,
    dcomp: IDCompositionDesktopDevice,
    _target: IDCompositionTarget,
    visual: IDCompositionVisual2,
    surface: Option<IDCompositionSurface>,
    size: (u32, u32),
    dwrite: IDWriteFactory,
    /// User locale name (NUL-terminated), so text uses the right regional glyph forms.
    locale: Vec<u16>,
    format: Option<(f32, IDWriteTextFormat)>,
    layouts: HashMap<String, IDWriteTextLayout>,
    /// file key -> bitmap (None = Windows had no image for it)
    pub icons: HashMap<String, Option<ID2D1Bitmap1>>,
}

/// Largest texture side for a Direct3D feature level (9_1/9_2, 9_3, 10_x, 11+).
fn max_surface_px(feature_level: i32) -> u32 {
    match feature_level {
        l if l < 0x9300 => 2048,
        l if l < 0xa000 => 4096,
        l if l < 0xb000 => 8192,
        _ => 16384,
    }
}

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

fn rect(l: f32, t: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: l, top: t, right: l + w, bottom: t + h }
}

fn translate(x: f32, y: f32) -> Matrix3x2 {
    Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: x, M32: y }
}

fn mul(a: &Matrix3x2, b: &Matrix3x2) -> Matrix3x2 {
    Matrix3x2 {
        M11: a.M11 * b.M11 + a.M12 * b.M21,
        M12: a.M11 * b.M12 + a.M12 * b.M22,
        M21: a.M21 * b.M11 + a.M22 * b.M21,
        M22: a.M21 * b.M12 + a.M22 * b.M22,
        M31: a.M31 * b.M11 + a.M32 * b.M21 + b.M31,
        M32: a.M31 * b.M12 + a.M32 * b.M22 + b.M32,
    }
}

fn rotate_about(deg: f32, cx: f32, cy: f32) -> Matrix3x2 {
    let r = deg.to_radians();
    let (s, c) = r.sin_cos();
    Matrix3x2 { M11: c, M12: s, M21: -s, M22: c, M31: cx - cx * c + cy * s, M32: cy - cx * s - cy * c }
}

impl Renderer {
    /// `need_px` is the longest side of the surface we will draw on (the work area).
    /// `software` skips the graphics card (used after the card failed once).
    pub fn new(hwnd: HWND, need_px: u32, software: bool) -> Result<Renderer> {
        unsafe {
            let mut d3d: Option<ID3D11Device> = None;
            // Any graphics card first (Direct3D feature level 9.1 and up); the built-in
            // software renderer (WARP) when there is no usable GPU: remote desktop,
            // virtual machines, basic display driver.
            for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
                if software && driver == D3D_DRIVER_TYPE_HARDWARE {
                    continue;
                }
                // the graphics DLLs are loaded through harden.rs (System32 only), see there
                let Ok(dev) = crate::harden::d3d11_create_device(driver, D3D11_CREATE_DEVICE_BGRA_SUPPORT) else { continue };
                // Old cards cannot hold a surface as large as a 4K/8K screen
                // (limit 2048 to 8192 px by feature level): draw in software there.
                if driver == D3D_DRIVER_TYPE_HARDWARE && max_surface_px(dev.GetFeatureLevel().0) < need_px {
                    continue;
                }
                d3d = Some(dev);
                break;
            }
            let d3d = d3d.ok_or_else(windows::core::Error::empty)?;
            let dxgi: IDXGIDevice = d3d.cast()?;
            let factory: ID2D1Factory1 =
                crate::harden::d2d1_create_factory(D2D1_FACTORY_TYPE_SINGLE_THREADED, &D2D1_FACTORY_OPTIONS::default())?;
            let d2d = factory.CreateDevice(&dxgi)?;
            let res_ctx = d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            let dcomp: IDCompositionDesktopDevice = crate::harden::dcomposition_create_device2(&d2d)?;
            let target = dcomp.CreateTargetForHwnd(hwnd, true)?;
            let visual = dcomp.CreateVisual()?;
            target.SetRoot(&visual)?;
            let dwrite: IDWriteFactory = crate::harden::dwrite_create_factory(DWRITE_FACTORY_TYPE_SHARED)?;
            Ok(Renderer {
                d3d,
                d2d,
                res_ctx,
                dcomp,
                _target: target,
                visual,
                surface: None,
                size: (0, 0),
                dwrite,
                locale: {
                    let mut buf = [0u16; 85];
                    let n = windows::Win32::Globalization::GetUserDefaultLocaleName(&mut buf).max(1) as usize;
                    let mut v = buf[..n - 1].to_vec();
                    v.push(0);
                    v
                },
                format: None,
                layouts: HashMap::new(),
                icons: HashMap::new(),
            })
        }
    }

    pub fn resize(&mut self, w: u32, h: u32) -> Result<()> {
        if self.size == (w, h) && self.surface.is_some() {
            return Ok(());
        }
        unsafe {
            let s = self.dcomp.CreateSurface(w.max(1), h.max(1), DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_ALPHA_MODE_PREMULTIPLIED)?;
            self.visual.SetContent(&s)?;
            self.surface = Some(s);
            self.size = (w, h);
        }
        Ok(())
    }

    /// Uploads one icon/thumbnail to the GPU.
    pub fn add_icon(&mut self, key: String, w: u32, h: u32, data: &[u8]) {
        // size math in usize with overflow checks: `data` must really hold w x h pixels
        let need = (w as usize).checked_mul(h as usize).and_then(|n| n.checked_mul(4));
        if w == 0 || h == 0 || w > 8192 || h > 8192 || need.is_none_or(|n| data.len() < n) {
            self.icons.insert(key, None);
            return;
        }
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            colorContext: ManuallyDrop::new(None),
        };
        let bmp = unsafe {
            self.res_ctx
                .CreateBitmap(D2D_SIZE_U { width: w, height: h }, Some(data.as_ptr() as *const _), w * 4, &props)
                .ok()
        };
        self.icons.insert(key, bmp);
    }

    pub fn clear_text_cache(&mut self) {
        self.layouts.clear();
        self.format = None;
    }

    fn text_format(&mut self, g: &Geom) -> Result<IDWriteTextFormat> {
        if let Some((s, f)) = &self.format {
            if (*s - g.s).abs() < 0.001 {
                return Ok(f.clone());
            }
        }
        unsafe {
            let f = self.dwrite.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                12.0 * g.s,
                PCWSTR(self.locale.as_ptr()),
            )?;
            f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
            f.SetWordWrapping(DWRITE_WORD_WRAPPING_EMERGENCY_BREAK)?;
            let sign = self.dwrite.CreateEllipsisTrimmingSign(&f)?;
            let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
            f.SetTrimming(&trim, &sign)?;
            self.layouts.clear();
            self.format = Some((g.s, f.clone()));
            Ok(f)
        }
    }

    fn layout(&mut self, g: &Geom, text: &str) -> Result<IDWriteTextLayout> {
        if let Some(l) = self.layouts.get(text) {
            return Ok(l.clone());
        }
        if self.layouts.len() > 3000 {
            self.layouts.clear();
        }
        let f = self.text_format(g)?;
        let wide: Vec<u16> = text.encode_utf16().collect();
        let l = unsafe { self.dwrite.CreateTextLayout(&wide, &f, g.cell_w - 6.0 * g.s, 34.0 * g.s)? };
        self.layouts.insert(text.to_string(), l.clone());
        Ok(l)
    }

    /// Draws one frame. Returns file keys whose icons are still missing.
    /// True after a driver crash/update or GPU removal: everything must be rebuilt.
    pub fn device_lost(&self) -> bool {
        unsafe { self.d3d.GetDeviceRemovedReason().is_err() }
    }

    /// `more` = (icons hidden past the left edge, icons hidden past the right edge).
    /// `label_bg` = dark backdrop behind every file name (for light wallpapers).
    pub fn draw(&mut self, g: &Geom, items: &[DrawItem], more: (bool, bool), label_bg: bool) -> Result<Vec<(String, String)>> {
        let mut missing: Vec<(String, String)> = Vec::new();
        let surface = match &self.surface {
            Some(s) => s.clone(),
            None => return Ok(missing),
        };
        // Prepare text layouts before BeginDraw (keeps the draw section short).
        let mut labels: Vec<Option<IDWriteTextLayout>> = Vec::with_capacity(items.len());
        for it in items {
            let text: &str = match &it.kind {
                DrawKind::File(f) => &f.name,
                DrawKind::Stack { label, scrub: Some(f), .. } => {
                    let _ = label;
                    &f.name
                }
                DrawKind::Stack { label, .. } => label,
            };
            labels.push(self.layout(g, text).ok());
        }
        unsafe {
            // everything that can fail is created before BeginDraw, so a failure
            // never leaves the surface in drawing state
            let brush = self.res_ctx.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 1.0), None)?;
            let mut offset = POINT::default();
            let ctx: ID2D1DeviceContext = surface.BeginDraw(None, &mut offset)?;
            let base = translate(offset.x as f32, offset.y as f32);
            ctx.SetTransform(&base);
            let full = rect(0.0, 0.0, self.size.0 as f32, self.size.1 as f32);
            ctx.PushAxisAlignedClip(&full, D2D1_ANTIALIAS_MODE_ALIASED);
            ctx.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));

            for (it, label) in items.iter().zip(labels.iter()) {
                let v = it.vis;
                if v.a <= 0.01 {
                    continue;
                }
                let (x, y) = (v.x, v.y);
                // hover / selection highlight
                if it.hover || it.selected {
                    let a = if it.selected { 0.30 } else { 0.16 } * v.a;
                    brush.SetColor(&color(1.0, 1.0, 1.0, a));
                    let hl = D2D1_ROUNDED_RECT {
                        rect: rect(x + 6.0 * g.s, y + 1.0 * g.s, g.cell_w - 12.0 * g.s, g.icon + 8.0 * g.s),
                        radiusX: 8.0 * g.s,
                        radiusY: 8.0 * g.s,
                    };
                    ctx.FillRoundedRectangle(&hl, &brush);
                }
                let isz = g.icon * v.sc;
                let ix = x + (g.cell_w - isz) / 2.0;
                let iy = y + 5.0 * g.s + (g.icon - isz) / 2.0;
                match &it.kind {
                    DrawKind::File(f) => {
                        self.draw_icon(&ctx, &brush, &base, f, ix, iy, isz, 0.0, v.a, &mut missing);
                    }
                    DrawKind::Stack { covers, expanded, scrub, count, .. } => {
                        if *expanded {
                            // open stack: a round button with a down chevron (like macOS)
                            let r = isz * 0.36;
                            let (cx, cy) = (ix + isz / 2.0, iy + isz / 2.0);
                            brush.SetColor(&color(0.12, 0.12, 0.14, 0.55 * v.a));
                            let el = D2D1_ELLIPSE { point: D2D_POINT_2F { x: cx, y: cy }, radiusX: r, radiusY: r };
                            ctx.FillEllipse(&el, &brush);
                            brush.SetColor(&color(1.0, 1.0, 1.0, 0.9 * v.a));
                            ctx.DrawEllipse(&el, &brush, 1.5 * g.s, None);
                            let c = r * 0.42;
                            let p1 = D2D_POINT_2F { x: cx - c, y: cy - c * 0.45 };
                            let p2 = D2D_POINT_2F { x: cx, y: cy + c * 0.55 };
                            let p3 = D2D_POINT_2F { x: cx + c, y: cy - c * 0.45 };
                            ctx.DrawLine(p1, p2, &brush, 2.4 * g.s, None);
                            ctx.DrawLine(p2, p3, &brush, 2.4 * g.s, None);
                        } else if let Some(f) = scrub {
                            self.draw_icon(&ctx, &brush, &base, f, ix, iy, isz, 0.0, v.a, &mut missing);
                        } else {
                            // up to three covers, fanned slightly
                            let n = covers.len().min(3);
                            let angles = [0.0f32, -7.0, 6.0];
                            let lift = [0.0f32, 3.0, 6.0];
                            for i in (0..n).rev() {
                                let s2 = isz * (1.0 - 0.08 * i as f32);
                                let dx = ix + (isz - s2) / 2.0;
                                let dy = iy + (isz - s2) / 2.0 - lift[i] * g.s;
                                self.draw_icon(&ctx, &brush, &base, covers[i], dx, dy, s2, angles[i], v.a, &mut missing);
                            }
                            // small count badge
                            let badge = format!("{}", count);
                            if let Ok(l) = self.small_badge_layout(g, &badge) {
                                let br = 9.0 * g.s;
                                let (bx, by) = (ix + isz - br * 0.6, iy + br * 0.4);
                                brush.SetColor(&color(0.20, 0.45, 0.95, 0.95 * v.a));
                                let el = D2D1_ELLIPSE { point: D2D_POINT_2F { x: bx, y: by }, radiusX: br, radiusY: br };
                                ctx.SetTransform(&base);
                                ctx.FillEllipse(&el, &brush);
                                brush.SetColor(&color(1.0, 1.0, 1.0, v.a));
                                ctx.DrawTextLayout(
                                    D2D_POINT_2F { x: bx - br, y: by - br * 0.72 },
                                    &l,
                                    &brush,
                                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                                );
                            }
                        }
                    }
                }
                // label with a soft shadow
                ctx.SetTransform(&base);
                if let Some(l) = label {
                    let lx = x + 3.0 * g.s;
                    let ly = y + g.icon + 12.0 * g.s;
                    if it.selected {
                        brush.SetColor(&color(0.16, 0.42, 0.92, 0.85 * v.a));
                        let mut m = DWRITE_TEXT_METRICS::default();
                        if l.GetMetrics(&mut m).is_ok() {
                            let pill = D2D1_ROUNDED_RECT {
                                rect: rect(lx + m.left - 3.0 * g.s, ly + m.top - 1.0 * g.s, m.width + 6.0 * g.s, m.height + 2.0 * g.s),
                                radiusX: 4.0 * g.s,
                                radiusY: 4.0 * g.s,
                            };
                            ctx.FillRoundedRectangle(&pill, &brush);
                        }
                    } else if label_bg {
                        brush.SetColor(&color(0.0, 0.0, 0.0, 0.55 * v.a));
                        let mut m = DWRITE_TEXT_METRICS::default();
                        if l.GetMetrics(&mut m).is_ok() {
                            let pill = D2D1_ROUNDED_RECT {
                                rect: rect(lx + m.left - 3.0 * g.s, ly + m.top - 1.0 * g.s, m.width + 6.0 * g.s, m.height + 2.0 * g.s),
                                radiusX: 4.0 * g.s,
                                radiusY: 4.0 * g.s,
                            };
                            ctx.FillRoundedRectangle(&pill, &brush);
                        }
                    }
                    brush.SetColor(&color(0.0, 0.0, 0.0, 0.75 * v.a));
                    ctx.DrawTextLayout(D2D_POINT_2F { x: lx + 1.0 * g.s, y: ly + 1.0 * g.s }, l, &brush, D2D1_DRAW_TEXT_OPTIONS_CLIP);
                    brush.SetColor(&color(1.0, 1.0, 1.0, v.a));
                    ctx.DrawTextLayout(
                        D2D_POINT_2F { x: lx, y: ly },
                        l,
                        &brush,
                        D2D1_DRAW_TEXT_OPTIONS_CLIP | D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                    );
                }
            }
            // small arrows at the screen edges when the wheel can bring more icons into view
            ctx.SetTransform(&base);
            for (show, cx, dir) in [(more.0, 20.0 * g.s, -1.0f32), (more.1, g.w - 20.0 * g.s, 1.0f32)] {
                if !show {
                    continue;
                }
                let (cy, r) = (g.h / 2.0, 13.0 * g.s);
                brush.SetColor(&color(0.12, 0.12, 0.14, 0.6));
                ctx.FillEllipse(&D2D1_ELLIPSE { point: D2D_POINT_2F { x: cx, y: cy }, radiusX: r, radiusY: r }, &brush);
                brush.SetColor(&color(1.0, 1.0, 1.0, 0.9));
                let c = r * 0.4;
                let tip = D2D_POINT_2F { x: cx + dir * c * 0.5, y: cy };
                ctx.DrawLine(D2D_POINT_2F { x: cx - dir * c * 0.5, y: cy - c }, tip, &brush, 2.2 * g.s, None);
                ctx.DrawLine(tip, D2D_POINT_2F { x: cx - dir * c * 0.5, y: cy + c }, &brush, 2.2 * g.s, None);
            }
            ctx.PopAxisAlignedClip();
            surface.EndDraw()?;
            self.dcomp.Commit()?;
        }
        Ok(missing)
    }

    fn small_badge_layout(&mut self, g: &Geom, text: &str) -> Result<IDWriteTextLayout> {
        let key = format!("#badge#{}", text);
        if let Some(l) = self.layouts.get(&key) {
            return Ok(l.clone());
        }
        unsafe {
            let f = self.dwrite.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                10.0 * g.s,
                PCWSTR(self.locale.as_ptr()),
            )?;
            f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            let wide: Vec<u16> = text.encode_utf16().collect();
            let br = 18.0 * g.s;
            let l = self.dwrite.CreateTextLayout(&wide, &f, br, br)?;
            self.layouts.insert(key, l.clone());
            Ok(l)
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn draw_icon(
        &self,
        ctx: &ID2D1DeviceContext,
        brush: &ID2D1SolidColorBrush,
        base: &Matrix3x2,
        f: &FileItem,
        x: f32,
        y: f32,
        size: f32,
        angle: f32,
        alpha: f32,
        missing: &mut Vec<(String, String)>,
    ) {
        let key = f.key();
        let m = if angle != 0.0 { mul(&rotate_about(angle, x + size / 2.0, y + size / 2.0), base) } else { *base };
        ctx.SetTransform(&m);
        match self.icons.get(&key) {
            Some(Some(bmp)) => {
                let s = bmp.GetSize();
                let k = (size / s.width.max(1.0)).min(size / s.height.max(1.0));
                let (w, h) = (s.width * k, s.height * k);
                let dst = rect(x + (size - w) / 2.0, y + (size - h), w, h);
                ctx.DrawBitmap(bmp, Some(&dst), alpha, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, None, None);
            }
            other => {
                if other.is_none() && !missing.iter().any(|(k, _)| *k == key) {
                    missing.push((key, f.path.clone()));
                }
                brush.SetColor(&color(1.0, 1.0, 1.0, 0.18 * alpha));
                let rr = D2D1_ROUNDED_RECT { rect: rect(x + size * 0.12, y + size * 0.08, size * 0.76, size * 0.88), radiusX: size * 0.1, radiusY: size * 0.1 };
                ctx.FillRoundedRectangle(&rr, brush);
            }
        }
        ctx.SetTransform(base);
    }

    #[allow(dead_code)]
    pub fn device(&self) -> &ID2D1Device {
        &self.d2d
    }
}

#[allow(dead_code)]
pub fn work_rect_size(r: &RECT) -> (u32, u32) {
    ((r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32)
}
