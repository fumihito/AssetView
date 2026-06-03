#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cache;
mod loader;
mod log;
mod os_dnd;

pub const APP_NAME: &str = "AssetView";

#[cfg(windows)]
struct SingleInstanceGuard {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
struct MediaFoundationGuard;

#[cfg(windows)]
impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
impl Drop for MediaFoundationGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}

#[cfg(windows)]
fn startup_media_foundation() -> Option<MediaFoundationGuard> {
    const MFSTARTUP_FULL: u32 = 0;
    let hr = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) };
    if hr < 0 {
        log::append(format!("MFStartup failed: 0x{hr:08x}"));
        None
    } else {
        Some(MediaFoundationGuard)
    }
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn startup_media_foundation() -> Option<()> {
    None
}

#[cfg(windows)]
const MF_VERSION: u32 = 0x0002_0070;

#[cfg(windows)]
#[link(name = "mfplat")]
extern "system" {
    fn MFStartup(version: u32, flags: u32) -> windows_sys::core::HRESULT;
    fn MFShutdown() -> windows_sys::core::HRESULT;
}

#[cfg(windows)]
fn acquire_single_instance() -> Option<SingleInstanceGuard> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = "Local\\AssetViewSingleInstanceMutex"
        .encode_utf16()
        .chain([0])
        .collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    if handle.is_null() {
        return None;
    }

    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_exists {
        unsafe {
            let _ = windows_sys::Win32::Foundation::CloseHandle(handle);
        }
        None
    } else {
        Some(SingleInstanceGuard { handle })
    }
}

#[cfg(windows)]
fn bring_existing_window_to_front() {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, FindWindowW, IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    let title: Vec<u16> = APP_NAME.encode_utf16().chain([0]).collect();
    let hwnd: HWND = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if hwnd.is_null() {
        return;
    }

    unsafe {
        if IsIconic(hwnd) != 0 {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
    }
}

fn main() -> eframe::Result<()> {
    // パニック・クラッシュログを %TEMP%\AssetView_debug.log に書く
    log::init();
    log::append(format!("log: {}", log::path()));

    #[cfg(windows)]
    os_dnd::init_ole();

    #[cfg(windows)]
    let _mf_guard = startup_media_foundation();

    #[cfg(windows)]
    let _single_instance = match acquire_single_instance() {
        Some(guard) => guard,
        None => {
            log::append(format!(
                "another {APP_NAME} instance is already running; exiting"
            ));
            bring_existing_window_to_front();
            return Ok(());
        }
    };

    // 画像ファイルが引数として渡された場合はその画像を直接開く
    let initial_file: Option<std::path::PathBuf> = std::env::args()
        .nth(1)
        .and_then(|s| std::fs::canonicalize(s).ok())
        .filter(|p| p.is_file());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| Box::new(app::PicViewApp::new(cc, initial_file))),
    );

    #[cfg(windows)]
    os_dnd::uninit_ole();

    result
}
