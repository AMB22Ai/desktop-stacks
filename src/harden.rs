// Process hardening for a portable exe that people download and run from anywhere
// (often the Downloads folder, where other downloaded files sit next to it).
//
// The risk: Windows looks for most DLLs in the exe's own folder first. A file named
// like a system DLL, dropped next to the exe, would be loaded and run with it.
//
// What is done about it, in two parts:
//
// 1) The exe itself only links to DLLs that Windows never takes from the exe's folder:
//    the "KnownDLLs" (kernel32, user32, gdi32, advapi32, ole32, oleaut32, shell32,
//    msvcrt...) and API sets (api-ms-win-*). `tools/check_imports.py` fails the build
//    if any other DLL shows up in the import table.
//
// 2) The graphics libraries (d3d11, d2d1, dcomp, dwrite, dwmapi) are NOT in that
//    protected set. So they are not linked in the usual way: the few entry functions
//    needed are fetched here at run time, with LOAD_LIBRARY_SEARCH_SYSTEM32, after
//    `restrict_dll_search` has told Windows to use System32 only for this process.
//
// 3) app.manifest is built into the exe (build.rs). Windows then ignores a
//    "DesktopStacks.exe.manifest" file or ".local" folder next to the exe, which could
//    otherwise redirect DLL names, System32 ones included, to planted files.
//
// An earlier version relied on the PE "DependentLoadFlags" field instead. A test on
// Windows 11 (build 26200) showed a fake dwmapi.dll next to the exe was still loaded,
// so that approach was dropped. tools/planting_test.ps1 repeats the test with about
// 40 fake DLLs and a fake manifest; bcryptprimitives.dll (used by the Rust standard
// library, not on the documented KnownDLLs list) is among them and was ignored.

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use windows::core::{s, w, Error, Interface, Result, GUID, HRESULT, PCSTR, PCWSTR};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::{D2D1_FACTORY_OPTIONS, D2D1_FACTORY_TYPE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION};
use windows::Win32::Graphics::DirectWrite::DWRITE_FACTORY_TYPE;
use windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE;
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LoadLibraryExW, SetDefaultDllDirectories, SetDllDirectoryW,
    LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwareness, PROCESS_PER_MONITOR_DPI_AWARE};

/// Call first thing in main: from here on, any DLL loaded by name (by us or by Windows
/// components inside this process) is searched in System32 only, never in the exe's
/// folder or the current directory.
pub fn restrict_dll_search() {
    unsafe {
        let _ = SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
        let _ = SetDllDirectoryW(w!(""));
    }
}

type AnyFn = unsafe extern "system" fn() -> isize;

/// One function of a system DLL, with the DLL taken from System32 and nowhere else.
/// The DLL stays loaded for the life of the process.
fn system_fn(dll: PCWSTR, name: PCSTR) -> Result<AnyFn> {
    unsafe {
        let module = LoadLibraryExW(dll, None, LOAD_LIBRARY_SEARCH_SYSTEM32)?;
        GetProcAddress(module, name).ok_or_else(Error::from_win32)
    }
}

/// Takes ownership of an interface pointer a system function handed out.
unsafe fn adopt<T: Interface>(raw: *mut c_void) -> Result<T> {
    if raw.is_null() {
        return Err(Error::empty());
    }
    // SAFETY: the caller got `raw` from a function documented to return this interface
    // with one reference, which the returned value now owns.
    Ok(T::from_raw(raw))
}

/// D3D11CreateDevice for the default adapter, without asking for a device context.
pub fn d3d11_create_device(driver: D3D_DRIVER_TYPE, flags: D3D11_CREATE_DEVICE_FLAG) -> Result<ID3D11Device> {
    type F = unsafe extern "system" fn(
        *mut c_void,
        D3D_DRIVER_TYPE,
        HMODULE,
        u32,
        *const D3D_FEATURE_LEVEL,
        u32,
        u32,
        *mut *mut c_void,
        *mut D3D_FEATURE_LEVEL,
        *mut *mut c_void,
    ) -> HRESULT;
    let f = system_fn(w!("d3d11.dll"), s!("D3D11CreateDevice"))?;
    let mut device: *mut c_void = null_mut();
    unsafe {
        // SAFETY: documented signature HRESULT D3D11CreateDevice(IDXGIAdapter*, D3D_DRIVER_TYPE,
        // HMODULE, UINT, const D3D_FEATURE_LEVEL*, UINT, UINT, ID3D11Device**, D3D_FEATURE_LEVEL*,
        // ID3D11DeviceContext**); the optional pointers may be null.
        let f: F = std::mem::transmute::<AnyFn, F>(f);
        f(null_mut(), driver, HMODULE::default(), flags.0, null(), 0, D3D11_SDK_VERSION, &mut device, null_mut(), null_mut()).ok()?;
        adopt(device)
    }
}

/// D2D1CreateFactory.
pub fn d2d1_create_factory<T: Interface>(kind: D2D1_FACTORY_TYPE, options: &D2D1_FACTORY_OPTIONS) -> Result<T> {
    type F = unsafe extern "system" fn(D2D1_FACTORY_TYPE, *const GUID, *const D2D1_FACTORY_OPTIONS, *mut *mut c_void) -> HRESULT;
    let f = system_fn(w!("d2d1.dll"), s!("D2D1CreateFactory"))?;
    let mut out: *mut c_void = null_mut();
    unsafe {
        // SAFETY: documented signature HRESULT D2D1CreateFactory(D2D1_FACTORY_TYPE, REFIID,
        // const D2D1_FACTORY_OPTIONS*, void**); it returns the interface named by the IID.
        let f: F = std::mem::transmute::<AnyFn, F>(f);
        f(kind, &T::IID, options, &mut out).ok()?;
        adopt(out)
    }
}

/// DCompositionCreateDevice2. `rendering_device` is the Direct2D device to draw with.
pub fn dcomposition_create_device2<D: Interface, T: Interface>(rendering_device: &D) -> Result<T> {
    type F = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
    let f = system_fn(w!("dcomp.dll"), s!("DCompositionCreateDevice2"))?;
    let mut out: *mut c_void = null_mut();
    unsafe {
        // SAFETY: documented signature HRESULT DCompositionCreateDevice2(IUnknown*, REFIID, void**);
        // every COM interface pointer is an IUnknown pointer, and it is only borrowed for the call.
        let f: F = std::mem::transmute::<AnyFn, F>(f);
        f(rendering_device.as_raw(), &T::IID, &mut out).ok()?;
        adopt(out)
    }
}

/// DWriteCreateFactory.
pub fn dwrite_create_factory<T: Interface>(kind: DWRITE_FACTORY_TYPE) -> Result<T> {
    type F = unsafe extern "system" fn(DWRITE_FACTORY_TYPE, *const GUID, *mut *mut c_void) -> HRESULT;
    let f = system_fn(w!("dwrite.dll"), s!("DWriteCreateFactory"))?;
    let mut out: *mut c_void = null_mut();
    unsafe {
        // SAFETY: documented signature HRESULT DWriteCreateFactory(DWRITE_FACTORY_TYPE, REFIID, IUnknown**).
        let f: F = std::mem::transmute::<AnyFn, F>(f);
        f(kind, &T::IID, &mut out).ok()?;
        adopt(out)
    }
}

/// DwmSetWindowAttribute for an attribute whose value is a BOOL.
pub fn dwm_set_window_flag(hwnd: HWND, attribute: DWMWINDOWATTRIBUTE, on: bool) -> Result<()> {
    type F = unsafe extern "system" fn(HWND, u32, *const c_void, u32) -> HRESULT;
    let f = system_fn(w!("dwmapi.dll"), s!("DwmSetWindowAttribute"))?;
    let value: i32 = on as i32;
    unsafe {
        // SAFETY: documented signature HRESULT DwmSetWindowAttribute(HWND, DWORD, LPCVOID, DWORD);
        // `value` is a 4-byte BOOL that outlives the call.
        let f: F = std::mem::transmute::<AnyFn, F>(f);
        f(hwnd, attribute.0 as u32, &value as *const i32 as *const c_void, 4).ok()
    }
}

/// Per-monitor DPI awareness that works on every Windows 10 build. The v2 API only
/// exists from Windows 10 1703, so it is looked up at run time (a static import
/// would stop the exe from starting on older builds); older builds get v1.
pub fn set_dpi_awareness() {
    type SetContext = unsafe extern "system" fn(isize) -> i32;
    unsafe {
        if let Ok(user32) = GetModuleHandleW(w!("user32.dll")) {
            if let Some(f) = GetProcAddress(user32, s!("SetProcessDpiAwarenessContext")) {
                // SAFETY: documented signature BOOL SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT)
                let f: SetContext = std::mem::transmute(f);
                // -4 = DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, -3 = PER_MONITOR_AWARE
                if f(-4) != 0 || f(-3) != 0 {
                    return;
                }
            }
        }
        let _ = SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);
    }
}
