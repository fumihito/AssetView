use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use rayon::prelude::*;

use crate::cache::ThumbnailCache;

#[cfg(windows)]
use windows_sys::core::{GUID, HRESULT, PCWSTR};
#[cfg(windows)]
use windows_sys::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
#[cfg(windows)]
use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
#[cfg(windows)]
use windows_sys::Win32::UI::Shell::SHCreateItemFromParsingName;

/// Long side of generated thumbnails (pixels).
/// Higher values give better quality when tiles are zoomed in.
pub const THUMB_SIZE: u32 = 300;

const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "avi", "wmv", "mkv", "webm", "mpg", "mpeg",
];

pub struct LoadResult {
    pub generation: u64,
    pub path: PathBuf,
    pub mtime: i64,
    pub width: u32,
    pub height: u32,
    /// Raw RGBA8 bytes, row-major.  Length == width * height * 4 (guaranteed).
    pub rgba: Vec<u8>,
}

pub struct LoadItem {
    pub priority: i32,
    pub path: PathBuf,
}

pub struct LoadRequest {
    pub generation: u64,
    pub items: Vec<LoadItem>,
}

fn is_video(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

#[cfg(windows)]
const IID_ISHELL_ITEM_IMAGE_FACTORY: GUID = GUID {
    data1: 0xbcc18b79,
    data2: 0xba16,
    data3: 0x442f,
    data4: [0x80, 0xc4, 0x8a, 0x59, 0xc3, 0x0c, 0x46, 0x3b],
};

#[cfg(windows)]
const SIIGBF_RESIZETOFIT: u32 = 0x0000_0000;
#[cfg(windows)]
const SIIGBF_THUMBNAILONLY: u32 = 0x0000_0008;
#[cfg(windows)]
const SIIGBF_CROPTOSQUARE: u32 = 0x0000_0020;

#[cfg(windows)]
#[repr(C)]
struct IShellItemImageFactoryVtbl {
    query_interface: unsafe extern "system" fn(
        this: *mut IShellItemImageFactory,
        riid: *const GUID,
        ppv: *mut *mut core::ffi::c_void,
    ) -> HRESULT,
    add_ref: unsafe extern "system" fn(this: *mut IShellItemImageFactory) -> u32,
    release: unsafe extern "system" fn(this: *mut IShellItemImageFactory) -> u32,
    get_image: unsafe extern "system" fn(
        this: *mut IShellItemImageFactory,
        size: windows_sys::Win32::Foundation::SIZE,
        flags: u32,
        phbm: *mut HBITMAP,
    ) -> HRESULT,
}

#[cfg(windows)]
#[repr(C)]
struct IShellItemImageFactory {
    lp_vtbl: *const IShellItemImageFactoryVtbl,
}

struct QueuedJob {
    generation: u64,
    priority: i32,
    seq: u64,
    path: PathBuf,
}

impl Ord for QueuedJob {
    fn cmp(&self, other: &Self) -> Ordering {
        self.generation
            .cmp(&other.generation)
            .then_with(|| self.priority.cmp(&other.priority))
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for QueuedJob {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for QueuedJob {
    fn eq(&self, other: &Self) -> bool {
        self.generation == other.generation
            && self.priority == other.priority
            && self.seq == other.seq
            && self.path == other.path
    }
}

impl Eq for QueuedJob {}

/// Spawns a coordinator thread backed by a dedicated 2-thread Rayon pool.
///
/// Send a `Vec<PathBuf>` of images to load; receive `LoadResult`s as they finish.
/// If a newer batch arrives while one is processing, the stale batch is discarded.
/// Dropping the sender shuts down the coordinator thread.
pub fn spawn_loader(
    cache: Arc<Mutex<ThumbnailCache>>,
) -> (Sender<LoadRequest>, Receiver<LoadResult>) {
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let worker_threads = (cpu_count / 2).max(1);

    // Use up to half of the logical cores. This keeps the UI thread responsive
    // while still allowing parallel thumbnail decode.
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_threads)
        .start_handler(|_| {
            #[cfg(windows)]
            unsafe {
                use windows_sys::Win32::System::Threading::{
                    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
                };
                SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
            }
        })
        .build()
        .unwrap_or_else(|_| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .unwrap()
        });

    let (req_tx, req_rx) = channel::<LoadRequest>();
    let (res_tx, res_rx) = channel::<LoadResult>();

    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }

    std::thread::spawn(move || {
        let mut pending = BinaryHeap::<QueuedJob>::new();
        let mut current_generation = 0_u64;
        let mut seq = 0_u64;

        loop {
            let first = match req_rx.recv() {
                Ok(req) => req,
                Err(_) => break,
            };

            let mut newest_generation = first.generation;
            if newest_generation > current_generation {
                current_generation = newest_generation;
                pending.clear();
            }
            if first.generation == current_generation {
                for item in first.items {
                    pending.push(QueuedJob {
                        generation: first.generation,
                        priority: item.priority,
                        seq,
                        path: item.path,
                    });
                    seq = seq.wrapping_add(1);
                }
            }

            while let Ok(req) = req_rx.try_recv() {
                if req.generation > newest_generation {
                    newest_generation = req.generation;
                }
                if req.generation > current_generation {
                    current_generation = req.generation;
                    pending.clear();
                }
                if req.generation == current_generation {
                    for item in req.items {
                        pending.push(QueuedJob {
                            generation: req.generation,
                            priority: item.priority,
                            seq,
                            path: item.path,
                        });
                        seq = seq.wrapping_add(1);
                    }
                }
            }

            while let Some(job) = pending.pop() {
                let job_generation = job.generation;
                let job_priority = job.priority;
                let mut batch = vec![job.path];
                while pending
                    .peek()
                    .map(|p| p.generation == job_generation && p.priority == job_priority)
                    .unwrap_or(false)
                {
                    batch.push(pending.pop().unwrap().path);
                }

                let res_tx = res_tx.clone();
                let cache = cache.clone();
                pool.install(move || {
                    batch
                        .into_par_iter()
                        .filter_map(|p| load_thumbnail(&p, &cache, job_generation))
                        .for_each(|r| {
                            res_tx.send(r).ok();
                        });
                });

                while let Ok(req) = req_rx.try_recv() {
                    if req.generation > current_generation {
                        current_generation = req.generation;
                        pending.clear();
                    }
                    if req.generation == current_generation {
                        for item in req.items {
                            pending.push(QueuedJob {
                                generation: req.generation,
                                priority: item.priority,
                                seq,
                                path: item.path,
                            });
                            seq = seq.wrapping_add(1);
                        }
                    }
                }
            }
        }
    });

    (req_tx, res_rx)
}

fn load_thumbnail(
    path: &PathBuf,
    cache: &Arc<Mutex<ThumbnailCache>>,
    generation: u64,
) -> Option<LoadResult> {
    let path_str = path.to_string_lossy();

    let mtime = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;

    // Cache hit – validate stored dimensions match blob length before using.
    if let Some((w, h, data)) = cache.lock().ok()?.get(&path_str, mtime, THUMB_SIZE) {
        if w as usize * h as usize * 4 == data.len() && w > 0 && h > 0 {
            return Some(LoadResult {
                generation,
                path: path.clone(),
                mtime,
                width: w,
                height: h,
                rgba: data,
            });
        }
        // Stale/corrupt cache entry – fall through to re-decode.
    }

    // Decode + resize. Wrap in catch_unwind so a buggy image decoder never
    // kills the loader thread.
    let result = std::panic::catch_unwind(|| {
        if is_video(path) {
            #[cfg(windows)]
            {
                return load_video_thumbnail(path);
            }
        }

        let img = image::open(path).ok()?;

        // Use Lanczos3 for high-quality downscaling; skip resize for images that
        // are already smaller than THUMB_SIZE (avoids upscaling noise).
        let thumb = if img.width() > THUMB_SIZE || img.height() > THUMB_SIZE {
            img.resize(
                THUMB_SIZE,
                THUMB_SIZE,
                image::imageops::FilterType::Lanczos3,
            )
        } else {
            img
        };

        let rgba = thumb.to_rgba8();
        let (w, h) = rgba.dimensions();
        Some((w, h, rgba.into_raw()))
    });

    let (w, h, data) = result.ok()??;

    // Sanity check before writing to cache or returning.
    if w == 0 || h == 0 || w as usize * h as usize * 4 != data.len() {
        return None;
    }

    cache
        .lock()
        .ok()?
        .put(&path_str, mtime, THUMB_SIZE, w, h, &data)
        .ok();

    Some(LoadResult {
        generation,
        path: path.clone(),
        mtime,
        width: w,
        height: h,
        rgba: data,
    })
}

#[cfg(windows)]
fn load_video_thumbnail(path: &PathBuf) -> Option<(u32, u32, Vec<u8>)> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::SIZE;

    let mut wide_path: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide_path.push(0);

    let com_started = unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) };
    if com_started < 0 {
        return None;
    }

    let mut shell_item_factory: *mut core::ffi::c_void = null_mut();
    let hr = unsafe {
        SHCreateItemFromParsingName(
            wide_path.as_ptr() as PCWSTR,
            null_mut(),
            &IID_ISHELL_ITEM_IMAGE_FACTORY,
            &mut shell_item_factory,
        )
    };
    if hr < 0 || shell_item_factory.is_null() {
        unsafe { CoUninitialize() };
        return None;
    }

    let factory = shell_item_factory as *mut IShellItemImageFactory;
    let mut hbitmap: HBITMAP = null_mut();
    let size = SIZE {
        cx: THUMB_SIZE as i32,
        cy: THUMB_SIZE as i32,
    };
    let hr = unsafe {
        ((*(*factory).lp_vtbl).get_image)(
            factory,
            size,
            SIIGBF_RESIZETOFIT | SIIGBF_THUMBNAILONLY | SIIGBF_CROPTOSQUARE,
            &mut hbitmap,
        )
    };
    unsafe {
        ((*(*factory).lp_vtbl).release)(factory);
    }
    if hr < 0 || hbitmap.is_null() {
        unsafe { CoUninitialize() };
        return None;
    }

    let rgba = hbitmap_to_rgba(hbitmap);
    unsafe {
        DeleteObject(hbitmap as HGDIOBJ);
        CoUninitialize();
    }
    rgba
}

#[cfg(windows)]
fn hbitmap_to_rgba(hbitmap: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    use std::ptr::null_mut;

    let mut bm = BITMAP::default();
    let got = unsafe {
        GetObjectW(
            hbitmap as HGDIOBJ,
            core::mem::size_of::<BITMAP>() as i32,
            &mut bm as *mut _ as *mut core::ffi::c_void,
        )
    };
    if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
        return None;
    }

    let width = bm.bmWidth as u32;
    let height = bm.bmHeight as u32;
    let mut info = BITMAPINFO::default();
    info.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = bm.bmWidth;
    info.bmiHeader.biHeight = -(bm.bmHeight as i32);
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB;
    info.bmiHeader.biSizeImage = width.saturating_mul(height).saturating_mul(4);

    let mut rgba = vec![0u8; info.bmiHeader.biSizeImage as usize];
    let hdc = unsafe { GetDC(null_mut()) };
    if hdc.is_null() {
        return None;
    }
    let scanlines = unsafe {
        GetDIBits(
            hdc,
            hbitmap,
            0,
            height,
            rgba.as_mut_ptr() as *mut core::ffi::c_void,
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    unsafe {
        ReleaseDC(null_mut(), hdc);
    }
    if scanlines == 0 {
        return None;
    }

    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Some((width, height, rgba))
}

#[cfg_attr(not(windows), allow(dead_code))]
#[cfg(not(windows))]
fn load_video_thumbnail(_path: &PathBuf) -> Option<(u32, u32, Vec<u8>)> {
    None
}
