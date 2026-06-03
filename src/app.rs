use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Ui};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
#[cfg(windows)]
use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};

use crate::cache::ThumbnailCache;
use crate::loader::{spawn_loader, LoadItem, LoadRequest, LoadResult};

#[cfg(windows)]
use std::os::windows::ffi::OsStringExt;

// ── Constants ──────────────────────────────────────────────────────────────────

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "webp", "tiff", "tif"];
const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "avi", "wmv", "mkv", "webm", "mpg", "mpeg",
];

/// Fade duration in seconds for slideshow transitions.
const FADE_SECS: f32 = 0.4;

/// Default tile size in pixels at zoom 1.0.
/// Decoupled from THUMB_SIZE so thumbnails can be stored at higher resolution
/// (e.g. 300px) while the default grid layout stays compact (150px tiles).
/// The GPU bilinear filter handles 2:1 downscale acceptably; at 2× zoom tiles
/// hit 1:1 with the texture for maximum clarity.
const TILE_BASE_PX: f32 = 150.0;

/// Number of frames shown on each side of the current image in the filmstrip.
const FILMSTRIP_RADIUS: i64 = 3;
/// Thumbnail display size inside the filmstrip strip.
const FILMSTRIP_THUMB_PX: f32 = 72.0;
/// Total height of the filmstrip panel (thumb + top/bottom padding).
const FILMSTRIP_HEIGHT: f32 = FILMSTRIP_THUMB_PX + 16.0;
/// Minimum time to keep the boot splash visible.
const MIN_BOOT_SPLASH_SECS: f32 = 0.35;
/// How long slideshow controls stay visible after user interaction.
const SLIDESHOW_CONTROLS_VISIBLE_SECS: f32 = 2.6;
/// Exponential slider range for slideshow interval adjustment.
const SLIDESHOW_INTERVAL_MIN_SECS: f32 = 1.0;
const SLIDESHOW_INTERVAL_MAX_SECS: f32 = 60.0;
/// Max time spent turning loaded thumbnails into GPU textures per frame.
const THUMBNAIL_UPLOAD_BUDGET_MS: u64 = 4;
/// Max results pulled from the loader channel per frame.
const THUMBNAIL_PULL_BUDGET: usize = 24;
/// How long the thumbnail scroll streak remains active.
const THUMBNAIL_SCROLL_ACCEL_RESET_SECS: f32 = 0.3;
/// Minimum thumbnail-height fraction moved per mouse-wheel step.
const THUMBNAIL_SCROLL_MIN_STEP_RATIO: f32 = 0.5;
/// Maximum multiplier applied to the minimum thumbnail wheel step.
const THUMBNAIL_SCROLL_ACCEL_MAX: f32 = 4.5;
/// Same-direction wheel ticks that stay near the base speed.
const THUMBNAIL_SCROLL_ACCEL_FLAT_TICKS: u32 = 2;
/// Gentle bump applied to the second tick so it still feels near the base speed.
const THUMBNAIL_SCROLL_ACCEL_FLAT_BUMP: f32 = 0.04;
/// Number of ticks used to ramp from the post-flat speed to max.
const THUMBNAIL_SCROLL_ACCEL_RAMP_STEPS: f32 = 10.0;
/// Curve exponent for the acceleration ramp. Higher = slower start, stronger finish.
const THUMBNAIL_SCROLL_ACCEL_CURVE: f32 = 1.7;

// ── Helpers ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ThemePalette {
    sidebar_bg: egui::Color32,
    content_bg: egui::Color32,
    drive_bg: egui::Color32,
    drive_text: egui::Color32,
    accent_fill: egui::Color32,
    accent_hover: egui::Color32,
    accent_border: egui::Color32,
    accent_text: egui::Color32,
    text_main: egui::Color32,
    text_muted: egui::Color32,
}

impl Default for ThemePalette {
    fn default() -> Self {
        Self {
            sidebar_bg: egui::Color32::from_rgb(238, 239, 242),
            content_bg: egui::Color32::from_rgb(245, 246, 248),
            drive_bg: egui::Color32::from_rgb(226, 228, 233),
            drive_text: egui::Color32::from_rgb(52, 60, 72),
            accent_fill: egui::Color32::from_rgb(54, 142, 255),
            accent_hover: egui::Color32::from_rgb(82, 160, 255),
            accent_border: egui::Color32::from_rgb(122, 192, 255),
            accent_text: egui::Color32::from_rgb(245, 250, 255),
            text_main: egui::Color32::from_rgb(42, 49, 61),
            text_muted: egui::Color32::from_rgb(101, 110, 122),
        }
    }
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn is_media(path: &Path) -> bool {
    media_kind(path).is_some()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Image,
    Video,
}

fn media_kind(path: &Path) -> Option<MediaKind> {
    if is_image(path) {
        Some(MediaKind::Image)
    } else if is_video(path) {
        Some(MediaKind::Video)
    } else {
        None
    }
}

fn file_mtime(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

fn scan_images(folder: &Path) -> Vec<MediaEntry> {
    let Ok(rd) = std::fs::read_dir(folder) else {
        return vec![];
    };
    let mut files: Vec<MediaEntry> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| is_media(p))
        .filter_map(|path| {
            let mtime = file_mtime(&path)?;
            Some(MediaEntry { path, mtime })
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

fn scan_subdirs(folder: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(folder) else {
        return vec![];
    };
    let mut subdirs: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    subdirs.sort();
    subdirs
}

fn scan_folder_recursive(
    folder: &Path,
    depth: usize,
    direct_subdirs: &mut Vec<PathBuf>,
    groups: &mut Vec<FolderGroup>,
    snapshots: &mut HashMap<PathBuf, DirSnapshot>,
) {
    let images = scan_images(folder);
    if !images.is_empty() {
        let display_name = folder
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| folder.to_string_lossy().into_owned());
        let image_labels: Vec<String> = images
            .iter()
            .map(|entry| {
                entry
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        let image_labels_short18: Vec<String> = image_labels
            .iter()
            .map(|label| truncate(label, 18))
            .collect();
        let image_is_video: Vec<bool> = images.iter().map(|entry| is_video(&entry.path)).collect();
        groups.push(FolderGroup {
            path: folder.to_path_buf(),
            display_name,
            images: images.iter().map(|entry| entry.path.clone()).collect(),
            image_labels_short18,
            image_is_video,
        });
    }

    let subdirs = scan_subdirs(folder);
    snapshots.insert(
        folder.to_path_buf(),
        DirSnapshot {
            images: images.clone(),
            subdirs: subdirs.clone(),
        },
    );

    for subdir in subdirs {
        if depth == 0 {
            direct_subdirs.push(subdir.clone());
        }
        scan_folder_recursive(&subdir, depth + 1, direct_subdirs, groups, snapshots);
    }
}

struct FolderScan {
    direct_subdirs: Vec<PathBuf>,
    groups: Vec<FolderGroup>,
    snapshots: HashMap<PathBuf, DirSnapshot>,
    changed_media: Vec<PathBuf>,
    removed_media: Vec<PathBuf>,
}

fn scan_folder(folder: &Path) -> FolderScan {
    let mut direct_subdirs = vec![];
    let mut groups = vec![];
    let mut snapshots = HashMap::new();
    scan_folder_recursive(folder, 0, &mut direct_subdirs, &mut groups, &mut snapshots);
    let changed_media: Vec<PathBuf> = snapshots
        .values()
        .flat_map(|snapshot| snapshot.images.iter().map(|entry| entry.path.clone()))
        .collect();

    FolderScan {
        direct_subdirs,
        groups,
        snapshots,
        changed_media,
        removed_media: vec![],
    }
}

fn scan_library(folders: &[PathBuf]) -> FolderScan {
    let mut groups = Vec::new();
    let mut direct_subdirs = Vec::new();
    let mut snapshots = HashMap::new();
    let mut changed_media = HashSet::new();
    let mut seen_group_paths: HashSet<PathBuf> = HashSet::new();
    let mut seen_subdirs: HashSet<PathBuf> = HashSet::new();

    for folder in folders {
        let scan = scan_folder(folder);
        for subdir in scan.direct_subdirs {
            if seen_subdirs.insert(subdir.clone()) {
                direct_subdirs.push(subdir);
            }
        }
        for group in scan.groups {
            if seen_group_paths.insert(group.path.clone()) {
                groups.push(group);
            }
        }
        changed_media.extend(scan.changed_media);
        snapshots.extend(scan.snapshots);
    }

    groups.sort_by(|a, b| {
        a.path
            .to_string_lossy()
            .to_ascii_lowercase()
            .cmp(&b.path.to_string_lossy().to_ascii_lowercase())
    });
    direct_subdirs.sort_by(|a, b| {
        a.to_string_lossy()
            .to_ascii_lowercase()
            .cmp(&b.to_string_lossy().to_ascii_lowercase())
    });

    FolderScan {
        direct_subdirs,
        groups,
        snapshots,
        changed_media: changed_media.into_iter().collect(),
        removed_media: vec![],
    }
}

fn collect_removed_subtree(
    folder: &Path,
    snapshots: &HashMap<PathBuf, DirSnapshot>,
    removed_media: &mut HashSet<PathBuf>,
) {
    let Some(snapshot) = snapshots.get(folder) else {
        return;
    };
    for entry in &snapshot.images {
        removed_media.insert(entry.path.clone());
    }
    for subdir in &snapshot.subdirs {
        collect_removed_subtree(subdir, snapshots, removed_media);
    }
}

fn diff_entries(
    prev: Option<&DirSnapshot>,
    current: &DirSnapshot,
    added_or_changed: &mut HashSet<PathBuf>,
    removed: &mut HashSet<PathBuf>,
) {
    let Some(prev) = prev else {
        for entry in &current.images {
            added_or_changed.insert(entry.path.clone());
        }
        return;
    };

    let mut prev_idx = 0usize;
    let mut curr_idx = 0usize;
    while prev_idx < prev.images.len() || curr_idx < current.images.len() {
        match (prev.images.get(prev_idx), current.images.get(curr_idx)) {
            (Some(old), Some(new)) => match old.path.cmp(&new.path) {
                std::cmp::Ordering::Less => {
                    removed.insert(old.path.clone());
                    prev_idx += 1;
                }
                std::cmp::Ordering::Greater => {
                    added_or_changed.insert(new.path.clone());
                    curr_idx += 1;
                }
                std::cmp::Ordering::Equal => {
                    if old.mtime != new.mtime {
                        removed.insert(old.path.clone());
                        added_or_changed.insert(new.path.clone());
                    }
                    prev_idx += 1;
                    curr_idx += 1;
                }
            },
            (Some(old), None) => {
                removed.insert(old.path.clone());
                prev_idx += 1;
            }
            (None, Some(new)) => {
                added_or_changed.insert(new.path.clone());
                curr_idx += 1;
            }
            (None, None) => break,
        }
    }
}

fn scan_folder_diff_recursive(
    folder: &Path,
    depth: usize,
    prev_snapshots: &HashMap<PathBuf, DirSnapshot>,
    snapshots: &mut HashMap<PathBuf, DirSnapshot>,
    direct_subdirs: &mut Vec<PathBuf>,
    groups: &mut Vec<FolderGroup>,
    added_or_changed: &mut HashSet<PathBuf>,
    removed_media: &mut HashSet<PathBuf>,
) {
    let images = scan_images(folder);
    let subdirs = scan_subdirs(folder);
    let current = DirSnapshot {
        images: images.clone(),
        subdirs: subdirs.clone(),
    };
    let prev = prev_snapshots.get(folder);
    diff_entries(prev, &current, added_or_changed, removed_media);

    if !images.is_empty() {
        let display_name = folder
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| folder.to_string_lossy().into_owned());
        let image_labels: Vec<String> = images
            .iter()
            .map(|entry| {
                entry
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        let image_labels_short18: Vec<String> = image_labels
            .iter()
            .map(|label| truncate(label, 18))
            .collect();
        let image_is_video: Vec<bool> = images.iter().map(|entry| is_video(&entry.path)).collect();
        groups.push(FolderGroup {
            path: folder.to_path_buf(),
            display_name,
            images: images.iter().map(|entry| entry.path.clone()).collect(),
            image_labels_short18,
            image_is_video,
        });
    }

    if depth == 0 {
        direct_subdirs.extend(subdirs.iter().cloned());
    }

    let prev_subdirs: HashSet<PathBuf> = prev
        .map(|snapshot| snapshot.subdirs.iter().cloned().collect())
        .unwrap_or_default();

    let current_subdirs: HashSet<PathBuf> = subdirs.iter().cloned().collect();
    for removed_subdir in prev_subdirs.difference(&current_subdirs) {
        collect_removed_subtree(removed_subdir, prev_snapshots, removed_media);
    }

    snapshots.insert(folder.to_path_buf(), current);

    for subdir in subdirs {
        scan_folder_diff_recursive(
            &subdir,
            depth + 1,
            prev_snapshots,
            snapshots,
            direct_subdirs,
            groups,
            added_or_changed,
            removed_media,
        );
    }
}

fn scan_folder_diff(folder: &Path, prev_snapshots: &HashMap<PathBuf, DirSnapshot>) -> FolderScan {
    let mut direct_subdirs = vec![];
    let mut groups = vec![];
    let mut snapshots = HashMap::new();
    let mut added_or_changed = HashSet::new();
    let mut removed_media = HashSet::new();
    scan_folder_diff_recursive(
        folder,
        0,
        prev_snapshots,
        &mut snapshots,
        &mut direct_subdirs,
        &mut groups,
        &mut added_or_changed,
        &mut removed_media,
    );

    FolderScan {
        direct_subdirs,
        groups,
        snapshots,
        changed_media: added_or_changed.into_iter().collect(),
        removed_media: removed_media.into_iter().collect(),
    }
}

fn scan_library_diff(
    folders: &[PathBuf],
    prev_snapshots: &HashMap<PathBuf, DirSnapshot>,
) -> FolderScan {
    let mut groups = Vec::new();
    let mut direct_subdirs = Vec::new();
    let mut snapshots = HashMap::new();
    let mut seen_group_paths: HashSet<PathBuf> = HashSet::new();
    let mut seen_subdirs: HashSet<PathBuf> = HashSet::new();
    let mut changed_media: HashSet<PathBuf> = HashSet::new();
    let mut removed_media: HashSet<PathBuf> = HashSet::new();

    for folder in folders {
        let scan = scan_folder_diff(folder, prev_snapshots);
        for subdir in scan.direct_subdirs {
            if seen_subdirs.insert(subdir.clone()) {
                direct_subdirs.push(subdir);
            }
        }
        for group in scan.groups {
            if seen_group_paths.insert(group.path.clone()) {
                groups.push(group);
            }
        }
        changed_media.extend(scan.changed_media);
        removed_media.extend(scan.removed_media);
        snapshots.extend(scan.snapshots);
    }

    groups.sort_by(|a, b| {
        a.path
            .to_string_lossy()
            .to_ascii_lowercase()
            .cmp(&b.path.to_string_lossy().to_ascii_lowercase())
    });
    direct_subdirs.sort_by(|a, b| {
        a.to_string_lossy()
            .to_ascii_lowercase()
            .cmp(&b.to_string_lossy().to_ascii_lowercase())
    });

    FolderScan {
        direct_subdirs,
        groups,
        snapshots,
        changed_media: changed_media.into_iter().collect(),
        removed_media: removed_media.into_iter().collect(),
    }
}

fn single_image_wheel_units(unit: egui::MouseWheelUnit, delta_y: f32) -> f32 {
    match unit {
        egui::MouseWheelUnit::Line => delta_y,
        egui::MouseWheelUnit::Page => delta_y * 3.0,
        egui::MouseWheelUnit::Point => delta_y / 50.0,
    }
}

fn thumbnail_scroll_accel_multiplier(streak: u32) -> f32 {
    let streak = streak.max(1);
    if streak <= THUMBNAIL_SCROLL_ACCEL_FLAT_TICKS {
        return 1.0 + THUMBNAIL_SCROLL_ACCEL_FLAT_BUMP * (streak.saturating_sub(1) as f32);
    }

    let base = 1.0 + THUMBNAIL_SCROLL_ACCEL_FLAT_BUMP;
    let t = ((streak - THUMBNAIL_SCROLL_ACCEL_FLAT_TICKS) as f32
        / THUMBNAIL_SCROLL_ACCEL_RAMP_STEPS)
        .clamp(0.0, 1.0);
    base + (THUMBNAIL_SCROLL_ACCEL_MAX - base) * t.powf(THUMBNAIL_SCROLL_ACCEL_CURVE)
}

fn thumbnail_scroll_streak_after_decay(streak: u32, decay_ticks: u32) -> u32 {
    streak.saturating_sub(decay_ticks).max(1)
}

fn slideshow_interval_to_slider(interval: f32) -> f32 {
    let min = SLIDESHOW_INTERVAL_MIN_SECS;
    let max = SLIDESHOW_INTERVAL_MAX_SECS;
    let clamped = interval.clamp(min, max);
    (clamped / min).ln() / (max / min).ln()
}

fn slider_to_slideshow_interval(t: f32) -> f32 {
    let min = SLIDESHOW_INTERVAL_MIN_SECS;
    let max = SLIDESHOW_INTERVAL_MAX_SECS;
    let t = t.clamp(0.0, 1.0);
    min * (max / min).powf(t)
}

fn fade_ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

#[cfg_attr(not(windows), allow(dead_code))]
fn format_timecode(secs: f32) -> String {
    let total = secs.max(0.0).round() as i64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(windows)]
fn hwnd_from_frame(frame: &eframe::Frame) -> Option<windows_sys::Win32::Foundation::HWND> {
    use windows_sys::Win32::Foundation::HWND;

    let window_handle = frame.window_handle().ok()?;
    match window_handle.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as HWND),
        _ => None,
    }
}

#[cfg(windows)]
fn pick_folder_dialog() -> Option<PathBuf> {
    use std::ffi::OsString;
    use windows_sys::core::{PCWSTR, PWSTR};
    use windows_sys::Win32::Foundation::{HWND, MAX_PATH};
    use windows_sys::Win32::UI::Shell::{
        ILFree, SHBrowseForFolderW, SHGetPathFromIDListW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS,
        BROWSEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    let mut display_name = vec![0u16; MAX_PATH as usize];
    let title: Vec<u16> = "フォルダを選択".encode_utf16().chain([0]).collect();
    let hwnd: HWND = unsafe { GetForegroundWindow() };
    let browse = BROWSEINFOW {
        hwndOwner: hwnd,
        pidlRoot: core::ptr::null_mut(),
        pszDisplayName: display_name.as_mut_ptr(),
        lpszTitle: title.as_ptr() as PCWSTR,
        ulFlags: BIF_NEWDIALOGSTYLE | BIF_RETURNONLYFSDIRS,
        lpfn: None,
        lParam: 0,
        iImage: 0,
    };

    let pidl = unsafe { SHBrowseForFolderW(&browse) };
    if pidl.is_null() {
        return None;
    }

    let mut path_buf = vec![0u16; MAX_PATH as usize];
    let ok = unsafe { SHGetPathFromIDListW(pidl, path_buf.as_mut_ptr() as PWSTR) };
    unsafe { ILFree(pidl) };
    if ok == 0 {
        return None;
    }

    let len = path_buf
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(path_buf.len());
    let os = OsString::from_wide(&path_buf[..len]);
    let path = PathBuf::from(os);
    if path.is_dir() {
        Some(path)
    } else {
        None
    }
}

#[cfg(not(windows))]
fn pick_folder_dialog() -> Option<PathBuf> {
    None
}

fn filmstrip_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("filmstrip")
}

fn draw_single_image_filmstrip(
    ui: &mut Ui,
    labels_short18: &[String],
    strip_indices: &[usize; 7],
    strip_textures: &[Option<&egui::TextureHandle>; 7],
    current_index: usize,
    theme: &ThemePalette,
) -> Option<usize> {
    let gap = 4.0_f32;
    let total_w = strip_indices.len() as f32 * FILMSTRIP_THUMB_PX
        + strip_indices.len().saturating_sub(1) as f32 * gap;
    let strip_x = ui.max_rect().center().x - total_w / 2.0;
    let strip_y = 8.0_f32;

    let pad = 8.0;
    let bg_rect = egui::Rect::from_min_max(
        egui::pos2(strip_x - pad, strip_y),
        egui::pos2(strip_x + total_w + pad, strip_y + FILMSTRIP_HEIGHT),
    );
    ui.painter()
        .rect_filled(bg_rect, 6.0, egui::Color32::from_black_alpha(220));

    let mut navigate_to: Option<usize> = None;
    for (strip_pos, (&image_index, tex_opt)) in
        strip_indices.iter().zip(strip_textures.iter()).enumerate()
    {
        let x = strip_x + strip_pos as f32 * (FILMSTRIP_THUMB_PX + gap);
        let y = strip_y + (FILMSTRIP_HEIGHT - FILMSTRIP_THUMB_PX) / 2.0;
        let item_rect = egui::Rect::from_min_size(
            egui::pos2(x, y),
            egui::vec2(FILMSTRIP_THUMB_PX, FILMSTRIP_THUMB_PX),
        );

        let is_current = image_index == current_index;
        let resp = ui.interact(
            item_rect,
            egui::Id::new(("fs_item", strip_pos)),
            egui::Sense::click(),
        );

        if resp.clicked() && !is_current {
            navigate_to = Some(image_index);
        }

        ui.painter()
            .rect_filled(item_rect, 2.0, egui::Color32::from_gray(25));
        if let Some(tex) = tex_opt {
            draw_centered(ui, tex, item_rect);
        }
        if is_current {
            ui.painter()
                .rect_stroke(item_rect, 2.0, egui::Stroke::new(2.0, theme.accent_border));
        } else if resp.hovered() {
            ui.painter()
                .rect_stroke(item_rect, 2.0, egui::Stroke::new(1.0, theme.accent_hover));
        }

        ui.painter().text(
            egui::pos2(item_rect.center().x, item_rect.max.y + 2.0),
            egui::Align2::CENTER_TOP,
            labels_short18
                .get(image_index)
                .map(|s| s.as_str())
                .unwrap_or(""),
            egui::FontId::proportional(10.0),
            theme.text_main,
        );
    }

    navigate_to
}

fn db_path() -> PathBuf {
    app_data_file("thumbnails.db")
}

fn settings_path() -> PathBuf {
    app_data_file("settings.txt")
}

fn app_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(crate::APP_NAME)
}

fn legacy_app_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("picview")
}

fn migrate_legacy_app_data_dir(dir: &Path) {
    let legacy = legacy_app_data_dir();
    if !legacy.exists() || legacy == dir {
        return;
    }
    let _ = std::fs::create_dir_all(dir);
    for name in ["thumbnails.db", "settings.txt"] {
        let old_path = legacy.join(name);
        let new_path = dir.join(name);
        if old_path.exists() && !new_path.exists() {
            if std::fs::rename(&old_path, &new_path).is_err() {
                let _ = std::fs::copy(&old_path, &new_path);
                let _ = std::fs::remove_file(&old_path);
            }
        }
    }
}

fn app_data_file(name: &str) -> PathBuf {
    let dir = app_data_dir();
    let _ = std::fs::create_dir_all(&dir);
    migrate_legacy_app_data_dir(&dir);
    dir.join(name)
}

struct AppSettings {
    last_folder: Option<PathBuf>,
    interval: f32,
    fade_enabled: bool,
    pixel_perfect: bool,
    media_volume: f32,
    media_muted: bool,
    show_thumbnail_filenames: bool,
    managed_folders: Vec<PathBuf>,
    recent_folders: Vec<PathBuf>,
    recent_images: Vec<PathBuf>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            last_folder: None,
            interval: 3.0,
            fade_enabled: true,
            pixel_perfect: false,
            media_volume: 1.0,
            media_muted: false,
            show_thumbnail_filenames: true,
            managed_folders: vec![],
            recent_folders: vec![],
            recent_images: vec![],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Folder,
    Library,
}

fn load_settings() -> AppSettings {
    let content = std::fs::read_to_string(settings_path()).unwrap_or_default();
    let mut s = AppSettings::default();
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("last_folder=") {
            let p = PathBuf::from(v);
            if p.is_dir() {
                s.last_folder = Some(p);
            }
        } else if let Some(v) = line.strip_prefix("slideshow_interval=") {
            if let Ok(f) = v.parse::<f32>() {
                s.interval = f.clamp(1.0, 60.0);
            }
        } else if let Some(v) = line.strip_prefix("slideshow_fade=") {
            s.fade_enabled = v.trim() != "0";
        } else if let Some(v) = line.strip_prefix("slideshow_pixel_perfect=") {
            s.pixel_perfect = v.trim() != "0";
        } else if let Some(v) = line.strip_prefix("media_volume=") {
            if let Ok(f) = v.parse::<f32>() {
                s.media_volume = f.clamp(0.0, 1.0);
            }
        } else if let Some(v) = line.strip_prefix("media_muted=") {
            s.media_muted = v.trim() != "0";
        } else if let Some(v) = line.strip_prefix("show_thumbnail_filenames=") {
            s.show_thumbnail_filenames = v.trim() != "0";
        } else if let Some(v) = line.strip_prefix("recent_folders=") {
            s.recent_folders = v
                .split('|')
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .take(10)
                .collect();
        } else if let Some(v) = line.strip_prefix("recent_images=") {
            s.recent_images = v
                .split('|')
                .map(PathBuf::from)
                .filter(|p| p.is_file() && is_media(p))
                .take(10)
                .collect();
        } else if let Some(v) = line.strip_prefix("managed_folders=") {
            s.managed_folders = v
                .split('|')
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .take(20)
                .collect();
        }
    }
    s
}

fn save_settings(
    folder: Option<&Path>,
    interval: f32,
    fade_enabled: bool,
    pixel_perfect: bool,
    media_volume: f32,
    media_muted: bool,
    show_thumbnail_filenames: bool,
    managed_folders: &[PathBuf],
    recent_folders: &[PathBuf],
    recent_images: &[PathBuf],
) {
    let mut lines = Vec::new();
    if let Some(f) = folder {
        lines.push(format!("last_folder={}", f.display()));
    }
    lines.push(format!("slideshow_interval={interval}"));
    lines.push(format!(
        "slideshow_fade={}",
        if fade_enabled { 1 } else { 0 }
    ));
    lines.push(format!(
        "slideshow_pixel_perfect={}",
        if pixel_perfect { 1 } else { 0 }
    ));
    lines.push(format!("media_volume={media_volume:.6}"));
    lines.push(format!("media_muted={}", if media_muted { 1 } else { 0 }));
    lines.push(format!(
        "show_thumbnail_filenames={}",
        if show_thumbnail_filenames { 1 } else { 0 }
    ));
    if !managed_folders.is_empty() {
        let joined: Vec<_> = managed_folders
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        lines.push(format!("managed_folders={}", joined.join("|")));
    }
    if !recent_folders.is_empty() {
        let joined: Vec<_> = recent_folders
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        lines.push(format!("recent_folders={}", joined.join("|")));
    }
    if !recent_images.is_empty() {
        let joined: Vec<_> = recent_images
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        lines.push(format!("recent_images={}", joined.join("|")));
    }
    let _ = std::fs::write(settings_path(), lines.join("\n"));
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let end = s
            .char_indices()
            .nth(max.saturating_sub(1))
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}…", &s[..end])
    }
}

fn display_label_for_path(path: &Path, max: usize) -> String {
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    truncate(&label, max)
}

#[cfg(test)]
mod tests {
    use super::{
        is_image, is_video, scan_folder, single_image_wheel_units,
        thumbnail_scroll_accel_multiplier, thumbnail_scroll_streak_after_decay, truncate,
        THUMBNAIL_SCROLL_ACCEL_FLAT_BUMP, THUMBNAIL_SCROLL_ACCEL_MAX,
    };
    use std::path::Path;

    #[test]
    fn recognizes_supported_image_extensions() {
        assert!(is_image(Path::new("photo.JPG")));
        assert!(is_image(Path::new("album/image.webp")));
        assert!(is_image(Path::new("scan.tiff")));
        assert!(is_video(Path::new("movie.mp4")));
        assert!(!is_image(Path::new("notes.txt")));
    }

    #[test]
    fn truncates_by_character_count() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("あいうえお", 3), "あい…");
        assert_eq!(truncate("ok", 4), "ok");
    }

    #[test]
    fn scans_root_images_and_descendant_subdirs_recursively() {
        let root = std::env::temp_dir().join(format!(
            "{}-scan-{}-{}",
            crate::APP_NAME,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("b.png"), b"fake").unwrap();
        std::fs::write(root.join("a.JPG"), b"fake").unwrap();
        std::fs::write(root.join("clip.mp4"), b"fake").unwrap();
        std::fs::write(root.join("notes.txt"), b"nope").unwrap();

        let sub_a = root.join("alpha");
        let sub_b = root.join("beta");
        let sub_a_child = sub_a.join("nested");
        let hidden = root.join(".hidden");
        std::fs::create_dir_all(&sub_a).unwrap();
        std::fs::create_dir_all(&sub_b).unwrap();
        std::fs::create_dir_all(&sub_a_child).unwrap();
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(sub_a.join("nested.gif"), b"fake").unwrap();
        std::fs::write(sub_a_child.join("deep.webp"), b"fake").unwrap();
        std::fs::write(sub_b.join("readme.md"), b"nope").unwrap();
        std::fs::write(hidden.join("secret.png"), b"fake").unwrap();

        let scan = scan_folder(&root);

        assert_eq!(scan.direct_subdirs.len(), 2);
        assert_eq!(scan.groups.len(), 3);
        assert_eq!(scan.groups[0].path, root);
        assert_eq!(scan.groups[0].images.len(), 3);
        assert_eq!(scan.groups[1].path, sub_a);
        assert_eq!(scan.groups[1].images.len(), 1);
        assert_eq!(scan.groups[2].path, sub_a_child);
        assert_eq!(scan.groups[2].images.len(), 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn converts_wheel_units_into_navigation_steps() {
        assert_eq!(
            single_image_wheel_units(egui::MouseWheelUnit::Line, 1.0),
            1.0
        );
        assert_eq!(
            single_image_wheel_units(egui::MouseWheelUnit::Point, 50.0),
            1.0
        );
        assert_eq!(
            single_image_wheel_units(egui::MouseWheelUnit::Page, -1.0),
            -3.0
        );
    }

    #[test]
    fn accelerates_thumbnail_scroll_after_repeated_steps() {
        assert!((thumbnail_scroll_accel_multiplier(1) - 1.0).abs() < f32::EPSILON);
        assert!(
            (thumbnail_scroll_accel_multiplier(2) - thumbnail_scroll_accel_multiplier(1)).abs()
                <= THUMBNAIL_SCROLL_ACCEL_FLAT_BUMP + f32::EPSILON
        );
        assert!(thumbnail_scroll_accel_multiplier(3) > thumbnail_scroll_accel_multiplier(2));
        assert!(thumbnail_scroll_accel_multiplier(10) <= THUMBNAIL_SCROLL_ACCEL_MAX);
    }

    #[test]
    fn decays_thumbnail_scroll_streak_gradually() {
        assert_eq!(thumbnail_scroll_streak_after_decay(6, 1), 5);
        assert_eq!(thumbnail_scroll_streak_after_decay(6, 10), 1);
        assert_eq!(thumbnail_scroll_streak_after_decay(1, 10), 1);
    }
}

// ── Folder group (Picasa-style grouped right pane) ─────────────────────────────

#[derive(Clone)]
struct FolderGroup {
    path: PathBuf,
    display_name: String,
    images: Vec<PathBuf>,
    image_labels_short18: Vec<String>,
    image_is_video: Vec<bool>,
}

#[derive(Clone)]
struct MediaEntry {
    path: PathBuf,
    mtime: i64,
}

#[derive(Clone)]
struct DirSnapshot {
    images: Vec<MediaEntry>,
    subdirs: Vec<PathBuf>,
}

// ── Folder tree ────────────────────────────────────────────────────────────────

struct FolderNode {
    path: PathBuf,
    name: String,
    children_loaded: bool,
    has_visible_children: Option<bool>,
    children: Vec<FolderNode>,
}

impl FolderNode {
    fn new(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        Self {
            path,
            name,
            children_loaded: false,
            has_visible_children: None,
            children: vec![],
        }
    }

    fn has_visible_children(&mut self) -> bool {
        if let Some(has_children) = self.has_visible_children {
            return has_children;
        }
        let Ok(rd) = std::fs::read_dir(&self.path) else {
            self.has_visible_children = Some(false);
            return false;
        };
        let has_children = rd.filter_map(|e| e.ok()).any(|e| {
            e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && !e.file_name().to_string_lossy().starts_with('.')
        });
        self.has_visible_children = Some(has_children);
        has_children
    }

    fn ensure_children(&mut self) {
        if self.children_loaded {
            return;
        }
        self.children_loaded = true;
        let Ok(rd) = std::fs::read_dir(&self.path) else {
            return;
        };
        let mut dirs: Vec<FolderNode> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| FolderNode::new(e.path()))
            .collect();
        dirs.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        });
        for child in &mut dirs {
            child.has_visible_children = None;
        }
        self.children = dirs;
    }
}

fn get_roots() -> Vec<FolderNode> {
    #[cfg(windows)]
    {
        (b'A'..=b'Z')
            .map(|c| PathBuf::from(format!("{}:\\", c as char)))
            .filter(|p| p.exists())
            .map(FolderNode::new)
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![FolderNode::new(PathBuf::from("/"))]
    }
}

// ── Thumbnail state ────────────────────────────────────────────────────────────

enum ThumbState {
    Ready(egui::TextureHandle),
    #[allow(dead_code)]
    Failed,
}

// ── Slideshow ──────────────────────────────────────────────────────────────────

enum SsPhase {
    FadeIn(Instant),
    Visible,
    FadeOut(Instant),
}

struct Slideshow {
    index: usize,
    interval: f32,
    last_advance: Instant,
    paused: bool,
    full_tex: Option<egui::TextureHandle>,
    full_path: Option<PathBuf>,
    fade_enabled: bool,
    pixel_perfect: bool,
    controls_visible_until: Option<Instant>,
    interaction_pause_until: Option<Instant>,
    has_shown_first_frame: bool,
    suppress_next_fade: bool,
    phase: SsPhase,
}

impl Slideshow {
    fn new(index: usize, interval: f32, fade_enabled: bool, pixel_perfect: bool) -> Self {
        Self {
            index,
            interval,
            last_advance: Instant::now(),
            paused: false,
            full_tex: None,
            full_path: None,
            fade_enabled,
            pixel_perfect,
            controls_visible_until: None,
            interaction_pause_until: None,
            has_shown_first_frame: false,
            suppress_next_fade: false,
            phase: SsPhase::Visible,
        }
    }
}

struct StartupResult {
    roots: Vec<FolderNode>,
    selected_folder: Option<PathBuf>,
    view_mode: ViewMode,
    folder_groups: Vec<FolderGroup>,
    direct_subdirs: Vec<PathBuf>,
    dir_snapshots: HashMap<PathBuf, DirSnapshot>,
    media_mtimes: HashMap<PathBuf, i64>,
    loader_tx: Sender<LoadRequest>,
    loader_rx: Receiver<LoadResult>,
    cache: Arc<Mutex<ThumbnailCache>>,
    settings: AppSettings,
    initial_image: Option<PathBuf>,
}

#[derive(Clone)]
struct ThumbnailGridLayout {
    cols: usize,
    total_rows: usize,
    group_layouts: Vec<GroupLayout>,
}

#[derive(Clone, Copy)]
struct GroupLayout {
    group_idx: usize,
    row_start: usize,
    image_base: usize,
}

struct ScanResult {
    generation: u64,
    selected_folder: Option<PathBuf>,
    view_mode: ViewMode,
    folder_groups: Vec<FolderGroup>,
    direct_subdirs: Vec<PathBuf>,
    dir_snapshots: HashMap<PathBuf, DirSnapshot>,
    media_mtimes: HashMap<PathBuf, i64>,
    changed_media: Vec<PathBuf>,
    removed_media: Vec<PathBuf>,
    incremental: bool,
    push_history: bool,
    managed_folders: Option<Vec<PathBuf>>,
}

// ── Single image view ──────────────────────────────────────────────────────────

struct SingleImage {
    index: usize,
    tex: Option<egui::TextureHandle>,
    path: Option<PathBuf>,
    display_name: String,
    #[cfg(windows)]
    video: Option<VideoPlayer>,
    #[cfg(windows)]
    video_ui: VideoUiState,
}

impl SingleImage {
    fn new(index: usize) -> Self {
        Self {
            index,
            tex: None,
            path: None,
            display_name: String::new(),
            #[cfg(windows)]
            video: None,
            #[cfg(windows)]
            video_ui: VideoUiState::default(),
        }
    }
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct VideoUiState {
    seek_dragging: bool,
    seek_position_secs: f32,
    volume_expanded: bool,
}

#[cfg(windows)]
impl Default for VideoUiState {
    fn default() -> Self {
        Self {
            seek_dragging: false,
            seek_position_secs: 0.0,
            volume_expanded: false,
        }
    }
}

#[cfg(windows)]
struct VideoPlayer {
    hwnd: windows_sys::Win32::Foundation::HWND,
    player: *mut IMFPMediaPlayer,
}

#[cfg(windows)]
#[repr(C)]
struct IMFPMediaPlayer {
    lp_vtbl: *const IMFPMediaPlayerVtbl,
}

#[cfg(windows)]
#[repr(C)]
struct IMFPMediaPlayerVtbl {
    query_interface: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        riid: *const windows_sys::core::GUID,
        ppv: *mut *mut core::ffi::c_void,
    ) -> windows_sys::core::HRESULT,
    add_ref: unsafe extern "system" fn(this: *mut IMFPMediaPlayer) -> u32,
    release: unsafe extern "system" fn(this: *mut IMFPMediaPlayer) -> u32,
    play: unsafe extern "system" fn(this: *mut IMFPMediaPlayer) -> windows_sys::core::HRESULT,
    pause: unsafe extern "system" fn(this: *mut IMFPMediaPlayer) -> windows_sys::core::HRESULT,
    stop: unsafe extern "system" fn(this: *mut IMFPMediaPlayer) -> windows_sys::core::HRESULT,
    frame_step: unsafe extern "system" fn(this: *mut IMFPMediaPlayer) -> windows_sys::core::HRESULT,
    set_position: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        guid_position_type: *const windows_sys::core::GUID,
        pv_position_value: *const windows_sys::Win32::System::Com::StructuredStorage::PROPVARIANT,
    ) -> windows_sys::core::HRESULT,
    get_position: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        guid_position_type: *const windows_sys::core::GUID,
        pv_position_value: *mut windows_sys::Win32::System::Com::StructuredStorage::PROPVARIANT,
    ) -> windows_sys::core::HRESULT,
    get_duration: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        guid_position_type: *const windows_sys::core::GUID,
        pv_duration_value: *mut windows_sys::Win32::System::Com::StructuredStorage::PROPVARIANT,
    ) -> windows_sys::core::HRESULT,
    set_rate: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        rate: f32,
    ) -> windows_sys::core::HRESULT,
    get_rate: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        rate: *mut f32,
    ) -> windows_sys::core::HRESULT,
    get_supported_rates: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        forward: windows_sys::core::BOOL,
        slowest: *mut f32,
        fastest: *mut f32,
    ) -> windows_sys::core::HRESULT,
    get_state: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        state: *mut u32,
    ) -> windows_sys::core::HRESULT,
    create_media_item_from_url: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        url: windows_sys::core::PCWSTR,
        sync: windows_sys::core::BOOL,
        user_data: usize,
        media_item: *mut *mut IMFPMediaItem,
    ) -> windows_sys::core::HRESULT,
    create_media_item_from_object: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        unknown: *mut core::ffi::c_void,
        sync: windows_sys::core::BOOL,
        user_data: usize,
        media_item: *mut *mut IMFPMediaItem,
    ) -> windows_sys::core::HRESULT,
    set_media_item: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        media_item: *mut IMFPMediaItem,
    ) -> windows_sys::core::HRESULT,
    clear_media_item:
        unsafe extern "system" fn(this: *mut IMFPMediaPlayer) -> windows_sys::core::HRESULT,
    get_media_item: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        media_item: *mut *mut IMFPMediaItem,
    ) -> windows_sys::core::HRESULT,
    get_volume: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        volume: *mut f32,
    ) -> windows_sys::core::HRESULT,
    set_volume: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        volume: f32,
    ) -> windows_sys::core::HRESULT,
    get_balance: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        balance: *mut f32,
    ) -> windows_sys::core::HRESULT,
    set_balance: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        balance: f32,
    ) -> windows_sys::core::HRESULT,
    get_mute: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        mute: *mut windows_sys::core::BOOL,
    ) -> windows_sys::core::HRESULT,
    set_mute: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        mute: windows_sys::core::BOOL,
    ) -> windows_sys::core::HRESULT,
    get_native_video_size: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        video: *mut windows_sys::Win32::Foundation::SIZE,
        ar_video: *mut windows_sys::Win32::Foundation::SIZE,
    ) -> windows_sys::core::HRESULT,
    get_ideal_video_size: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        min: *mut windows_sys::Win32::Foundation::SIZE,
        max: *mut windows_sys::Win32::Foundation::SIZE,
    ) -> windows_sys::core::HRESULT,
    set_video_source_rect: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        source: *const MFVideoNormalizedRect,
    ) -> windows_sys::core::HRESULT,
    get_video_source_rect: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        source: *mut MFVideoNormalizedRect,
    ) -> windows_sys::core::HRESULT,
    set_aspect_ratio_mode: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        mode: u32,
    ) -> windows_sys::core::HRESULT,
    get_aspect_ratio_mode: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        mode: *mut u32,
    ) -> windows_sys::core::HRESULT,
    get_video_window: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        hwnd_video: *mut windows_sys::Win32::Foundation::HWND,
    ) -> windows_sys::core::HRESULT,
    update_video:
        unsafe extern "system" fn(this: *mut IMFPMediaPlayer) -> windows_sys::core::HRESULT,
    set_border_color: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        color: u32,
    ) -> windows_sys::core::HRESULT,
    get_border_color: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        color: *mut u32,
    ) -> windows_sys::core::HRESULT,
    insert_effect: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        effect: *mut core::ffi::c_void,
        optional: windows_sys::core::BOOL,
    ) -> windows_sys::core::HRESULT,
    remove_effect: unsafe extern "system" fn(
        this: *mut IMFPMediaPlayer,
        effect: *mut core::ffi::c_void,
    ) -> windows_sys::core::HRESULT,
    remove_all_effects:
        unsafe extern "system" fn(this: *mut IMFPMediaPlayer) -> windows_sys::core::HRESULT,
    shutdown: unsafe extern "system" fn(this: *mut IMFPMediaPlayer) -> windows_sys::core::HRESULT,
}

#[cfg(windows)]
#[repr(C)]
struct IMFPMediaPlayerCallback {
    _unused: [u8; 0],
}

#[cfg(windows)]
#[repr(C)]
struct IMFPMediaItem {
    _unused: [u8; 0],
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct MFVideoNormalizedRect {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

#[cfg(windows)]
impl Drop for VideoPlayer {
    fn drop(&mut self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow;
        unsafe {
            if !self.player.is_null() {
                ((*(*self.player).lp_vtbl).release)(self.player);
                self.player = core::ptr::null_mut();
            }
            if !self.hwnd.is_null() {
                let _ = DestroyWindow(self.hwnd);
                self.hwnd = core::ptr::null_mut();
            }
        }
    }
}

#[cfg(windows)]
impl VideoPlayer {
    fn move_to_rect(&self, rect: egui::Rect, pixels_per_point: f32) {
        use windows_sys::Win32::UI::WindowsAndMessaging::MoveWindow;
        let scale = pixels_per_point.max(0.0001);
        let min = rect.min * scale;
        let size = rect.size() * scale;
        unsafe {
            MoveWindow(
                self.hwnd,
                min.x.round() as i32,
                min.y.round() as i32,
                size.x.round().max(1.0) as i32,
                size.y.round().max(1.0) as i32,
                1,
            );
            self.update_video();
        }
    }

    fn update_video(&self) {
        unsafe {
            if !self.player.is_null() {
                let _ = ((*(*self.player).lp_vtbl).update_video)(self.player);
            }
        }
    }

    fn current_position_secs(&self) -> Option<f32> {
        use windows_sys::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
        use windows_sys::Win32::System::Variant::{VT_EMPTY, VT_I8};

        if self.player.is_null() {
            return None;
        }
        let mut value: PROPVARIANT = unsafe { core::mem::zeroed() };
        let hr = unsafe {
            ((*(*self.player).lp_vtbl).get_position)(
                self.player,
                &MFP_POSITIONTYPE_100NS,
                &mut value,
            )
        };
        if hr < 0 {
            return None;
        }
        let result = unsafe {
            let vt = value.Anonymous.Anonymous.vt;
            if vt == VT_I8 {
                Some(value.Anonymous.Anonymous.Anonymous.hVal as f64 / 10_000_000.0)
            } else if vt == VT_EMPTY {
                Some(0.0)
            } else {
                None
            }
        };
        unsafe {
            let _ = PropVariantClear(&mut value);
        }
        result.map(|v| v as f32)
    }

    fn duration_secs(&self) -> Option<f32> {
        use windows_sys::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
        use windows_sys::Win32::System::Variant::{VT_EMPTY, VT_I8};

        if self.player.is_null() {
            return None;
        }
        let mut value: PROPVARIANT = unsafe { core::mem::zeroed() };
        let hr = unsafe {
            ((*(*self.player).lp_vtbl).get_duration)(
                self.player,
                &MFP_POSITIONTYPE_100NS,
                &mut value,
            )
        };
        if hr < 0 {
            return None;
        }
        let result = unsafe {
            let vt = value.Anonymous.Anonymous.vt;
            if vt == VT_I8 {
                Some(value.Anonymous.Anonymous.Anonymous.hVal as f64 / 10_000_000.0)
            } else if vt == VT_EMPTY {
                Some(0.0)
            } else {
                None
            }
        };
        unsafe {
            let _ = PropVariantClear(&mut value);
        }
        result.map(|v| v as f32)
    }

    fn set_position_secs(&self, secs: f32) -> bool {
        use windows_sys::Win32::System::Com::StructuredStorage::PROPVARIANT;
        use windows_sys::Win32::System::Variant::VT_I8;

        if self.player.is_null() {
            return false;
        }
        let mut value: PROPVARIANT = unsafe { core::mem::zeroed() };
        value.Anonymous.Anonymous.vt = VT_I8;
        value.Anonymous.Anonymous.Anonymous.hVal = (secs.max(0.0) * 10_000_000.0) as i64;
        let ok = unsafe {
            ((*(*self.player).lp_vtbl).set_position)(self.player, &MFP_POSITIONTYPE_100NS, &value)
                >= 0
        };
        if ok {
            self.update_video();
        }
        ok
    }

    fn set_volume(&self, volume: f32) -> bool {
        if self.player.is_null() {
            return false;
        }
        unsafe { ((*(*self.player).lp_vtbl).set_volume)(self.player, volume.clamp(0.0, 1.0)) >= 0 }
    }

    fn set_mute(&self, mute: bool) -> bool {
        if self.player.is_null() {
            return false;
        }
        unsafe { ((*(*self.player).lp_vtbl).set_mute)(self.player, if mute { 1 } else { 0 }) >= 0 }
    }
}

#[cfg(windows)]
#[link(name = "mfplay")]
extern "system" {
    fn MFPCreateMediaPlayer(
        pwsz_url: windows_sys::core::PCWSTR,
        f_start_playback: i32,
        creation_options: u32,
        p_callback: *mut IMFPMediaPlayerCallback,
        hwnd_video: windows_sys::Win32::Foundation::HWND,
        pp_player: *mut *mut IMFPMediaPlayer,
    ) -> windows_sys::core::HRESULT;
}

#[cfg(windows)]
const MFP_POSITIONTYPE_100NS: windows_sys::core::GUID = windows_sys::core::GUID::from_u128(0);

// ── App ────────────────────────────────────────────────────────────────────────

pub struct PicViewApp {
    roots: Vec<FolderNode>,
    selected_folder: Option<PathBuf>,
    view_mode: ViewMode,

    folder_groups: Vec<FolderGroup>,
    all_media_cache: Vec<PathBuf>,
    all_image_cache: Vec<PathBuf>,
    all_image_labels_short18: Vec<String>,
    all_media_index_by_path: HashMap<PathBuf, usize>,
    all_image_index_by_path: HashMap<PathBuf, usize>,
    media_next_image_index: Vec<Option<usize>>,
    media_prev_image_index: Vec<Option<usize>>,
    /// Direct subdirectories of selected_folder (all of them, not just those with images).
    direct_subdirs: Vec<PathBuf>,
    dir_snapshots: HashMap<PathBuf, DirSnapshot>,
    media_mtimes: HashMap<PathBuf, i64>,
    thumbnails: HashMap<PathBuf, ThumbState>,
    queued: HashSet<PathBuf>,
    pending_loader_results: VecDeque<LoadResult>,
    deferred_loader_results: HashMap<PathBuf, LoadResult>,
    thumbnail_visible_paths: Vec<usize>,
    thumbnail_prefetch_paths: Vec<usize>,
    thumbnail_cache_queued_folders: usize,
    thumbnail_cache_current_path: Option<PathBuf>,
    thumbnail_total_media: usize,
    thumbnail_total_images: usize,

    loader_tx: Option<Sender<LoadRequest>>,
    loader_rx: Option<Receiver<LoadResult>>,

    _watcher: Option<RecommendedWatcher>,
    watcher_rx: Option<std::sync::mpsc::Receiver<notify::Result<notify::Event>>>,
    watcher_debounce: Option<Instant>,

    // Address bar
    address_text: String,
    address_editing: bool,
    address_needs_focus: bool,

    // Navigation history (like a browser)
    history: Vec<PathBuf>,
    history_pos: usize, // current index in history

    // Zoom
    thumb_scale: f32,
    /// Inertia velocity for Ctrl+Scroll zoom; decays each frame.
    zoom_vel: f32,

    // Views
    slideshow: Option<Slideshow>,
    single_image: Option<SingleImage>,
    /// Index of the last image opened in single-image view; used for ← → grid navigation.
    grid_cursor: Option<usize>,
    /// Windows shell folder icon, loaded once at startup.
    folder_icon_tex: Option<egui::TextureHandle>,
    theme: ThemePalette,
    /// Accumulated scroll delta for single-image prev/next inertia.
    si_scroll_accum: f32,
    /// Accumulated scroll delta for slideshow prev/next inertia.
    ss_scroll_accum: f32,
    /// Current thumbnail-cache request generation.
    thumbnail_generation: u64,
    /// Last visible-range bucket used to refresh thumbnail prefetch priority.
    thumbnail_viewport_signature: Option<(usize, usize, usize)>,
    thumbnail_cache_total: usize,
    thumbnail_cache_done: usize,
    thumbnail_scroll_streak: u32,
    thumbnail_scroll_last_dir: i8,
    thumbnail_scroll_last_at: Option<Instant>,
    thumbnail_scroll_last_decay_at: Option<Instant>,
    thumbnail_scroll_rect: Option<egui::Rect>,
    left_panel_scroll_to_selected: bool,
    viewport_outer_rect: Option<egui::Rect>,
    viewport_motion_until: Option<Instant>,
    thumbnail_grid_layout_signature: Option<(u32, u32, usize, usize)>,
    thumbnail_grid_layout: Option<ThumbnailGridLayout>,

    startup_rx: Option<Receiver<StartupResult>>,
    startup_init: Option<StartupResult>,
    scan_rx: Option<Receiver<ScanResult>>,
    scan_generation: u64,
    scan_in_progress: bool,
    scan_message: Option<String>,
    startup_ready: bool,
    boot_started_at: Instant,

    // Keeps the cache Arc alive so on_exit() can hand it off to a background thread.
    _cache: Option<Arc<Mutex<ThumbnailCache>>>,

    // Persisted
    default_slideshow_interval: f32,
    default_fade_enabled: bool,
    default_slideshow_pixel_perfect: bool,
    media_volume: f32,
    media_muted: bool,
    show_thumbnail_filenames: bool,
    last_folder: Option<PathBuf>,
    managed_folders: Vec<PathBuf>,
    managed_folder_labels: Vec<String>,
    recent_folders: Vec<PathBuf>,
    recent_folder_labels: Vec<String>,
    recent_images: Vec<PathBuf>,
    recent_image_labels: Vec<String>,
}

impl PicViewApp {
    pub fn new(_cc: &eframe::CreationContext, initial_file: Option<PathBuf>) -> Self {
        let (startup_tx, startup_rx) = channel();
        std::thread::spawn(move || {
            let settings = load_settings();
            let roots = get_roots();

            let requested_media = initial_file.filter(|p| is_media(p));
            let requested_folder = requested_media
                .as_ref()
                .and_then(|img| img.parent().filter(|p| !p.as_os_str().is_empty()))
                .map(Path::to_path_buf)
                .or_else(|| settings.last_folder.clone());

            let mut selected_folder = None;
            let mut view_mode = ViewMode::Folder;
            let mut folder_groups = vec![];
            let mut direct_subdirs = vec![];
            let mut dir_snapshots = HashMap::new();
            let mut initial_image = None;

            if let Some(folder) = requested_folder.filter(|p| p.is_dir()) {
                let scan = scan_folder(&folder);
                folder_groups = scan.groups;
                direct_subdirs = scan.direct_subdirs;
                dir_snapshots = scan.snapshots;
                selected_folder = Some(folder);
                initial_image = requested_media.filter(|p| p.is_file());
            } else if !settings.managed_folders.is_empty() {
                let scan = scan_library(&settings.managed_folders);
                folder_groups = scan.groups;
                direct_subdirs = scan.direct_subdirs;
                dir_snapshots = scan.snapshots;
                view_mode = ViewMode::Library;
            }

            let cache = Arc::new(Mutex::new(
                ThumbnailCache::open(&db_path()).expect("cannot open thumbnail cache"),
            ));
            let (loader_tx, loader_rx) = spawn_loader(Arc::clone(&cache));
            let media_mtimes: HashMap<PathBuf, i64> = dir_snapshots
                .values()
                .flat_map(|snapshot| {
                    snapshot
                        .images
                        .iter()
                        .map(|entry| (entry.path.clone(), entry.mtime))
                })
                .collect();

            let _ = startup_tx.send(StartupResult {
                roots,
                selected_folder,
                view_mode,
                folder_groups,
                direct_subdirs,
                dir_snapshots,
                media_mtimes,
                loader_tx,
                loader_rx,
                cache,
                settings,
                initial_image,
            });
        });

        Self {
            roots: vec![],
            selected_folder: None,
            view_mode: ViewMode::Folder,
            folder_groups: vec![],
            all_media_cache: vec![],
            all_image_cache: vec![],
            all_image_labels_short18: vec![],
            all_media_index_by_path: HashMap::new(),
            all_image_index_by_path: HashMap::new(),
            media_next_image_index: vec![],
            media_prev_image_index: vec![],
            direct_subdirs: vec![],
            dir_snapshots: HashMap::new(),
            media_mtimes: HashMap::new(),
            thumbnails: HashMap::new(),
            queued: HashSet::new(),
            pending_loader_results: VecDeque::new(),
            deferred_loader_results: HashMap::new(),
            thumbnail_visible_paths: Vec::new(),
            thumbnail_prefetch_paths: Vec::new(),
            thumbnail_cache_queued_folders: 0,
            thumbnail_cache_current_path: None,
            thumbnail_total_media: 0,
            thumbnail_total_images: 0,
            loader_tx: None,
            loader_rx: None,
            _watcher: None,
            watcher_rx: None,
            watcher_debounce: None,
            address_text: String::new(),
            address_editing: false,
            address_needs_focus: false,
            history: vec![],
            history_pos: 0,
            thumb_scale: 1.0,
            zoom_vel: 0.0,
            slideshow: None,
            single_image: None,
            grid_cursor: None,
            folder_icon_tex: None,
            theme: ThemePalette::default(),
            si_scroll_accum: 0.0,
            ss_scroll_accum: 0.0,
            thumbnail_generation: 0,
            thumbnail_viewport_signature: None,
            thumbnail_cache_total: 0,
            thumbnail_cache_done: 0,
            thumbnail_scroll_streak: 0,
            thumbnail_scroll_last_dir: 0,
            thumbnail_scroll_last_at: None,
            thumbnail_scroll_last_decay_at: None,
            thumbnail_scroll_rect: None,
            left_panel_scroll_to_selected: false,
            viewport_outer_rect: None,
            viewport_motion_until: None,
            thumbnail_grid_layout_signature: None,
            thumbnail_grid_layout: None,
            startup_rx: Some(startup_rx),
            startup_init: None,
            scan_rx: None,
            scan_generation: 0,
            scan_in_progress: false,
            scan_message: None,
            startup_ready: false,
            boot_started_at: Instant::now(),
            _cache: None,
            default_slideshow_interval: 3.0,
            default_fade_enabled: true,
            default_slideshow_pixel_perfect: false,
            media_volume: 1.0,
            media_muted: false,
            show_thumbnail_filenames: true,
            last_folder: None,
            managed_folders: vec![],
            managed_folder_labels: vec![],
            recent_folders: vec![],
            recent_folder_labels: vec![],
            recent_images: vec![],
            recent_image_labels: vec![],
        }
    }

    // ── Settings helpers ───────────────────────────────────────────────────────

    fn do_save_settings(&self) {
        let interval = self.default_slideshow_interval;
        let fade = self
            .slideshow
            .as_ref()
            .map(|ss| ss.fade_enabled)
            .unwrap_or(self.default_fade_enabled);
        let pixel_perfect = self
            .slideshow
            .as_ref()
            .map(|ss| ss.pixel_perfect)
            .unwrap_or(self.default_slideshow_pixel_perfect);
        save_settings(
            self.last_folder.as_deref(),
            interval,
            fade,
            pixel_perfect,
            self.media_volume,
            self.media_muted,
            self.show_thumbnail_filenames,
            &self.managed_folders,
            &self.recent_folders,
            &self.recent_images,
        );
    }

    fn push_recent_folder(&mut self, path: PathBuf) {
        let list = &mut self.recent_folders;
        list.retain(|p| p != &path);
        list.insert(0, path);
        list.truncate(10);
        self.recent_folder_labels = self
            .recent_folders
            .iter()
            .map(|p| display_label_for_path(p, 32))
            .collect();
    }

    fn has_managed_folder(&self, path: &Path) -> bool {
        self.managed_folders.iter().any(|p| p == path)
    }

    fn add_managed_folder(&mut self, path: PathBuf) {
        if !path.is_dir() {
            return;
        }
        self.managed_folders.retain(|p| p != &path);
        self.managed_folders.insert(0, path);
        self.managed_folders.truncate(20);
        self.managed_folder_labels = self
            .managed_folders
            .iter()
            .map(|p| display_label_for_path(p, 24))
            .collect();
        if matches!(self.view_mode, ViewMode::Library) {
            self.rescan_library();
            return;
        }
        self.do_save_settings();
    }

    fn remove_managed_folder(&mut self, path: &Path) {
        self.managed_folders.retain(|p| p != path);
        self.managed_folder_labels = self
            .managed_folders
            .iter()
            .map(|p| display_label_for_path(p, 24))
            .collect();
        if matches!(self.view_mode, ViewMode::Library) {
            self.rescan_library();
            return;
        }
        self.do_save_settings();
    }

    fn push_recent_image(&mut self, path: PathBuf) {
        let list = &mut self.recent_images;
        list.retain(|p| p != &path);
        list.insert(0, path);
        list.truncate(10);
        self.recent_image_labels = self
            .recent_images
            .iter()
            .map(|p| display_label_for_path(p, 32))
            .collect();
    }

    fn open_single_image_at(&mut self, idx: usize, ctx: &egui::Context) {
        self.grid_cursor = Some(idx);
        self.single_image = Some(SingleImage::new(idx));
        let images = self.all_images();
        if let Some(p) = images.get(idx) {
            self.push_recent_image(p.clone());
        }
        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
    }

    #[cfg(windows)]
    fn ensure_video_player(
        &mut self,
        parent_hwnd: windows_sys::Win32::Foundation::HWND,
        path: &Path,
    ) -> Option<&mut VideoPlayer> {
        let media_volume = self.media_volume;
        let media_muted = self.media_muted;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

        let si = self.single_image.as_mut()?;
        let needs_new = match &si.video {
            Some(_) => si.path.as_deref() != Some(path),
            None => true,
        };

        if needs_new {
            si.video = None;
            let hinstance = unsafe { GetModuleHandleW(core::ptr::null()) };
            let class_name: Vec<u16> = "Static".encode_utf16().chain([0]).collect();
            let hwnd = unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    core::ptr::null(),
                    (windows_sys::Win32::UI::WindowsAndMessaging::WS_CHILD
                        | windows_sys::Win32::UI::WindowsAndMessaging::WS_VISIBLE
                        | windows_sys::Win32::UI::WindowsAndMessaging::WS_CLIPCHILDREN
                        | windows_sys::Win32::UI::WindowsAndMessaging::WS_CLIPSIBLINGS)
                        as u32,
                    0,
                    0,
                    1,
                    1,
                    parent_hwnd,
                    core::ptr::null_mut(),
                    hinstance as _,
                    core::ptr::null(),
                )
            };
            if hwnd.is_null() {
                return None;
            }

            let mut wide_path: Vec<u16> = path.as_os_str().encode_wide().collect();
            wide_path.push(0);
            let mut player: *mut IMFPMediaPlayer = core::ptr::null_mut();
            let hr = unsafe {
                MFPCreateMediaPlayer(
                    wide_path.as_ptr(),
                    1,
                    0,
                    core::ptr::null_mut(),
                    hwnd,
                    &mut player,
                )
            };
            if hr < 0 || player.is_null() {
                unsafe {
                    let _ = windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
                }
                return None;
            }

            let video = VideoPlayer { hwnd, player };
            let _ = video.set_volume(media_volume);
            let _ = video.set_mute(media_muted);
            si.video = Some(video);
        }

        si.video.as_mut()
    }

    #[cfg(windows)]
    fn apply_media_audio_settings(&self, player: &VideoPlayer) {
        let _ = player.set_volume(self.media_volume);
        let _ = player.set_mute(self.media_muted);
    }

    fn set_slideshow_window_mode(&self, ctx: &egui::Context, active: bool) {
        let root = egui::ViewportId::ROOT;
        ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Fullscreen(active));
        ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Decorations(!active));
        ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Resizable(!active));
    }

    fn start_slideshow(&mut self, index: usize, ctx: &egui::Context) {
        self.ss_scroll_accum = 0.0;
        self.slideshow = Some(Slideshow::new(
            index,
            self.default_slideshow_interval,
            self.default_fade_enabled,
            self.default_slideshow_pixel_perfect,
        ));
        self.set_slideshow_window_mode(ctx, true);
        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
    }

    fn note_slideshow_activity(&mut self, ctx: &egui::Context) {
        let Some(ss) = &mut self.slideshow else {
            return;
        };
        if ss.paused {
            ss.controls_visible_until = None;
            ss.interaction_pause_until = None;
            return;
        }

        let until = Instant::now() + Duration::from_secs_f32(SLIDESHOW_CONTROLS_VISIBLE_SECS);
        ss.controls_visible_until = Some(until);
        ss.interaction_pause_until = Some(until);
        ctx.request_repaint_after(Duration::from_secs_f32(SLIDESHOW_CONTROLS_VISIBLE_SECS));
    }

    fn tick_thumbnail_scroll_decay(&mut self, ctx: &egui::Context) {
        if self.thumbnail_scroll_streak <= 1 {
            self.thumbnail_scroll_last_decay_at = None;
            return;
        }

        let Some(mut decay_at) = self.thumbnail_scroll_last_decay_at else {
            self.thumbnail_scroll_last_decay_at = self.thumbnail_scroll_last_at;
            return;
        };

        let now = Instant::now();
        let tick = Duration::from_secs_f32(THUMBNAIL_SCROLL_ACCEL_RESET_SECS);
        let mut changed = false;
        while self.thumbnail_scroll_streak > 1 && now.duration_since(decay_at) >= tick {
            decay_at += tick;
            self.thumbnail_scroll_streak =
                thumbnail_scroll_streak_after_decay(self.thumbnail_scroll_streak, 1);
            changed = true;
        }

        self.thumbnail_scroll_last_decay_at = Some(decay_at);

        if self.thumbnail_scroll_streak > 1 {
            ctx.request_repaint_after(tick - now.duration_since(decay_at));
        }
        if changed {
            ctx.request_repaint();
        }
    }

    fn tick_viewport_motion(&mut self, ctx: &egui::Context) {
        let current_rect = ctx.input(|i| i.viewport().outer_rect.or(i.viewport().inner_rect));
        let Some(current_rect) = current_rect else {
            return;
        };

        let changed = self.viewport_outer_rect.is_some_and(|prev| {
            let dx = (prev.min.x - current_rect.min.x).abs()
                + (prev.min.y - current_rect.min.y).abs()
                + (prev.max.x - current_rect.max.x).abs()
                + (prev.max.y - current_rect.max.y).abs();
            dx > 0.5
        });
        self.viewport_outer_rect = Some(current_rect);
        if changed {
            self.viewport_motion_until = Some(Instant::now() + Duration::from_millis(250));
        }
    }

    fn viewport_motion_active(&self) -> bool {
        self.viewport_motion_until
            .is_some_and(|until| until > Instant::now())
    }

    fn slideshow_activity_detected(ctx: &egui::Context) -> bool {
        ctx.input(|i| {
            i.pointer.delta() != egui::Vec2::ZERO
                || i.events.iter().any(|event| {
                    matches!(
                        event,
                        egui::Event::PointerMoved(_)
                            | egui::Event::MouseMoved(_)
                            | egui::Event::PointerButton { pressed: true, .. }
                            | egui::Event::Text(_)
                            | egui::Event::Scroll(_)
                            | egui::Event::MouseWheel { .. }
                            | egui::Event::Key { pressed: true, .. }
                    )
                })
        })
    }

    fn stop_slideshow(&mut self, ctx: &egui::Context) {
        self.ss_scroll_accum = 0.0;
        if let Some(ss) = &self.slideshow {
            self.default_slideshow_interval = ss.interval;
            self.default_fade_enabled = ss.fade_enabled;
            self.default_slideshow_pixel_perfect = ss.pixel_perfect;
        }
        self.slideshow = None;
        self.set_slideshow_window_mode(ctx, false);
        self.do_save_settings();
    }

    fn close_single_image(&mut self) {
        if let Some(si) = self.single_image.as_ref() {
            self.grid_cursor = Some(si.index);
        }
        self.single_image = None;
    }

    fn apply_startup_result(&mut self, init: StartupResult, ctx: &egui::Context) {
        setup_fonts(ctx);
        self.roots = init.roots;
        self.selected_folder = init.selected_folder.clone();
        self.view_mode = init.view_mode;
        self.folder_groups = init.folder_groups;
        self.refresh_flat_media_cache();
        self.direct_subdirs = init.direct_subdirs;
        self.dir_snapshots = init.dir_snapshots;
        self.media_mtimes = init.media_mtimes;
        self.thumbnail_total_media = self.all_media_cache.len();
        self.thumbnail_total_images = self.all_image_cache.len();
        self.loader_tx = Some(init.loader_tx);
        self.loader_rx = Some(init.loader_rx);
        self._cache = Some(init.cache);
        self.default_slideshow_interval = init.settings.interval;
        self.default_fade_enabled = init.settings.fade_enabled;
        self.default_slideshow_pixel_perfect = init.settings.pixel_perfect;
        self.media_volume = init.settings.media_volume;
        self.media_muted = init.settings.media_muted;
        self.show_thumbnail_filenames = init.settings.show_thumbnail_filenames;
        self.last_folder = init.settings.last_folder.clone();
        self.managed_folders = init.settings.managed_folders;
        self.recent_folders = init.settings.recent_folders;
        self.recent_images = init.settings.recent_images;
        self.managed_folder_labels = self
            .managed_folders
            .iter()
            .map(|p| display_label_for_path(p, 24))
            .collect();
        self.recent_folder_labels = self
            .recent_folders
            .iter()
            .map(|p| display_label_for_path(p, 32))
            .collect();
        self.recent_image_labels = self
            .recent_images
            .iter()
            .map(|p| display_label_for_path(p, 32))
            .collect();
        self.folder_icon_tex = load_folder_icon_tex(ctx);

        self.address_text = self
            .selected_folder
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                if matches!(self.view_mode, ViewMode::Library) {
                    "Library".to_owned()
                } else {
                    String::new()
                }
            });

        self.queue_thumbnail_jobs_full(self.prioritized_thumbnail_items());

        if let Some(folder) = self.selected_folder.clone() {
            self.last_folder = Some(folder.clone());
            self.setup_watcher_for_paths(std::slice::from_ref(&folder));
            self.push_recent_folder(folder);
            self.do_save_settings();
            self.left_panel_scroll_to_selected = true;
        } else if matches!(self.view_mode, ViewMode::Library) {
            let managed = self.managed_folders.clone();
            self.setup_watcher_for_paths(&managed);
        }

        if let Some(img) = init.initial_image {
            if let Some(idx) = self.all_media_index_by_path.get(&img).copied() {
                self.grid_cursor = Some(idx);
                self.single_image = Some(SingleImage::new(idx));
                self.push_recent_image(img);
            }
        }

        self.startup_ready = true;
    }

    fn setup_watcher_for_paths(&mut self, paths: &[PathBuf]) {
        self._watcher = None;
        self.watcher_rx = None;
        let (wtx, wrx) = channel();
        if let Ok(mut w) = notify::recommended_watcher(move |res| {
            wtx.send(res).ok();
        }) {
            let mut watched_any = false;
            for path in paths {
                if w.watch(path, RecursiveMode::Recursive).is_ok() {
                    watched_any = true;
                }
            }
            if watched_any {
                self._watcher = Some(w);
                self.watcher_rx = Some(wrx);
            }
        }
    }

    // ── Folder selection ───────────────────────────────────────────────────────

    /// Navigate to a folder and push it onto the history stack.
    fn select_folder(&mut self, folder: PathBuf) {
        self.request_folder_scan(folder, true, false);
    }

    fn select_library(&mut self) {
        self.request_library_scan(false);
    }

    /// Re-scan the current folder without changing history (used by watcher).
    fn rescan_folder(&mut self, folder: PathBuf) {
        self.request_folder_scan(folder, false, true);
    }

    fn rescan_library(&mut self) {
        self.request_library_scan(true);
    }

    fn request_library_scan(&mut self, incremental: bool) {
        crate::log::append("select_library".to_string());

        self.view_mode = ViewMode::Library;
        self.selected_folder = None;
        self.left_panel_scroll_to_selected = false;
        self.address_text = "Library".to_owned();
        self.address_editing = false;
        self.start_scan_request(
            None,
            ViewMode::Library,
            false,
            Some(self.managed_folders.clone()),
            incremental,
        );
    }

    fn request_folder_scan(&mut self, folder: PathBuf, push_history: bool, incremental: bool) {
        crate::log::append(format!("select_folder: {}", folder.display()));

        self.view_mode = ViewMode::Folder;
        self.address_text = folder.to_string_lossy().into_owned();
        self.address_editing = false;
        self.selected_folder = Some(folder.clone());
        self.left_panel_scroll_to_selected = true;

        if push_history {
            // Drop forward history when navigating to a new path
            if self.history_pos < self.history.len() {
                self.history.truncate(self.history_pos);
            }
            // Avoid duplicate consecutive entries
            if self.history.last() != Some(&folder) {
                self.history.push(folder.clone());
            }
            self.history_pos = self.history.len();
        }

        self.last_folder = Some(folder.clone());
        if push_history {
            self.push_recent_folder(folder.clone());
        }
        self.do_save_settings();

        self.start_scan_request(
            Some(folder),
            ViewMode::Folder,
            push_history,
            None,
            incremental,
        );
    }

    fn start_scan_request(
        &mut self,
        selected_folder: Option<PathBuf>,
        view_mode: ViewMode,
        push_history: bool,
        managed_folders: Option<Vec<PathBuf>>,
        incremental: bool,
    ) {
        let (tx, rx) = channel();
        self.scan_generation = self.scan_generation.wrapping_add(1);
        self.scan_rx = Some(rx);
        self.scan_in_progress = true;
        self.scan_message = Some(match &selected_folder {
            Some(folder) => format!("Scanning {}", folder.display()),
            None => "Scanning Library".to_owned(),
        });

        if !incremental {
            self.folder_groups.clear();
            self.all_media_cache.clear();
            self.all_image_cache.clear();
            self.all_image_labels_short18.clear();
            self.all_media_index_by_path.clear();
            self.all_image_index_by_path.clear();
            self.media_next_image_index.clear();
            self.media_prev_image_index.clear();
            self.direct_subdirs.clear();
            self.dir_snapshots.clear();
            self.media_mtimes.clear();
            self.thumbnails.clear();
            self.queued.clear();
            self.pending_loader_results.clear();
            self.deferred_loader_results.clear();
            self.thumbnail_visible_paths.clear();
            self.thumbnail_prefetch_paths.clear();
            self.thumbnail_generation = 0;
            self.thumbnail_viewport_signature = None;
            self.thumbnail_cache_total = 0;
            self.thumbnail_cache_done = 0;
            self.thumbnail_cache_queued_folders = 0;
            self.thumbnail_cache_current_path = None;
            self.thumbnail_total_media = 0;
            self.thumbnail_total_images = 0;
            self.thumbnail_scroll_streak = 0;
            self.thumbnail_scroll_last_dir = 0;
            self.thumbnail_scroll_last_at = None;
            self.thumbnail_scroll_last_decay_at = None;
            self.thumbnail_scroll_rect = None;
            self.viewport_outer_rect = None;
            self.viewport_motion_until = None;
            self.thumbnail_grid_layout_signature = None;
            self.thumbnail_grid_layout = None;
            self.slideshow = None;
            self.single_image = None;
            self.grid_cursor = None;
        }

        let generation = self.scan_generation;
        let selected_folder_for_thread = selected_folder.clone();
        let managed_folders_for_thread = managed_folders.clone();
        let prev_snapshots = if incremental {
            Some(self.dir_snapshots.clone())
        } else {
            None
        };
        std::thread::spawn(move || {
            let scan = match view_mode {
                ViewMode::Folder => {
                    let folder = selected_folder_for_thread.expect("folder scan requires a folder");
                    if let Some(prev_snapshots) = prev_snapshots.as_ref() {
                        scan_folder_diff(&folder, prev_snapshots)
                    } else {
                        scan_folder(&folder)
                    }
                }
                ViewMode::Library => {
                    let folders = managed_folders_for_thread.unwrap_or_default();
                    if let Some(prev_snapshots) = prev_snapshots.as_ref() {
                        scan_library_diff(&folders, prev_snapshots)
                    } else {
                        scan_library(&folders)
                    }
                }
            };
            let dir_snapshots = scan.snapshots;
            let media_mtimes: HashMap<PathBuf, i64> = dir_snapshots
                .values()
                .flat_map(|snapshot| {
                    snapshot
                        .images
                        .iter()
                        .map(|entry| (entry.path.clone(), entry.mtime))
                })
                .collect();
            let result = ScanResult {
                generation,
                selected_folder,
                view_mode,
                folder_groups: scan.groups,
                direct_subdirs: scan.direct_subdirs,
                dir_snapshots,
                media_mtimes,
                changed_media: scan.changed_media,
                removed_media: scan.removed_media,
                incremental,
                push_history,
                managed_folders,
            };
            tx.send(result).ok();
        });
    }

    fn poll_scan(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.scan_rx else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            if self.scan_in_progress {
                // Scanning is background work; a modest refresh rate keeps the UI
                // responsive without burning a frame every 16 ms.
                ctx.request_repaint_after(Duration::from_millis(50));
            }
            return;
        };
        if result.generation != self.scan_generation {
            return;
        }

        self.scan_in_progress = false;
        self.scan_message = None;
        self.selected_folder = result.selected_folder.clone();
        self.view_mode = result.view_mode;
        self.dir_snapshots = result.dir_snapshots;
        self.media_mtimes = result.media_mtimes;
        self.thumbnail_viewport_signature = None;
        self.folder_groups = result.folder_groups;
        self.refresh_flat_media_cache();
        self.direct_subdirs = result.direct_subdirs;
        self.thumbnail_total_media = self.all_media_cache.len();
        self.thumbnail_total_images = self.all_image_cache.len();

        crate::log::append(format!(
            "  groups={} total_items={} subdirs={}",
            self.folder_groups.len(),
            self.thumbnail_total_media,
            self.direct_subdirs.len()
        ));
        if result.incremental && self.single_image.is_none() && self.slideshow.is_none() {
            let changed_media: HashSet<PathBuf> = result.changed_media.into_iter().collect();
            let removed_media: HashSet<PathBuf> = result.removed_media.into_iter().collect();
            if !removed_media.is_empty() {
                self.queued.retain(|p| !removed_media.contains(p));
                self.pending_loader_results
                    .retain(|r| !removed_media.contains(&r.path));
                self.deferred_loader_results
                    .retain(|p, _| !removed_media.contains(p));
                for path in &removed_media {
                    self.thumbnails.remove(path);
                }
                if let Some(cache) = &self._cache {
                    if let Ok(mut cache) = cache.lock() {
                        let _ = cache.remove_paths(&removed_media);
                    }
                }
            }

            let queued_paths = self.prioritized_thumbnail_items_for_paths(&changed_media);
            self.queue_thumbnail_jobs_incremental(queued_paths);
        } else {
            self.queue_thumbnail_jobs_full(self.prioritized_thumbnail_items());
        }
        self.thumbnail_scroll_rect = None;
        self.viewport_outer_rect = None;
        self.viewport_motion_until = None;
        self.thumbnail_grid_layout_signature = None;
        self.thumbnail_grid_layout = None;

        match result.view_mode {
            ViewMode::Folder => {
                if let Some(folder) = result.selected_folder {
                    self.setup_watcher_for_paths(std::slice::from_ref(&folder));
                }
            }
            ViewMode::Library => {
                if let Some(managed) = result.managed_folders {
                    self.setup_watcher_for_paths(&managed);
                }
            }
        }

        if result.push_history {
            self.do_save_settings();
        }
        self.scan_rx = None;
        ctx.request_repaint();
    }

    // ── Navigation helpers ─────────────────────────────────────────────────────

    fn can_go_back(&self) -> bool {
        self.history_pos > 1
    }

    fn can_go_forward(&self) -> bool {
        self.history_pos < self.history.len()
    }

    fn can_go_up(&self) -> bool {
        self.parent_folder().is_some()
    }

    /// Returns the canonical parent of the current folder, if one exists.
    fn parent_folder(&self) -> Option<PathBuf> {
        let folder = self.selected_folder.as_ref()?;
        let parent = folder.parent()?;
        // parent() of a drive root (e.g. "C:\") returns Some("") on some platforms;
        // treat empty-component paths as "no parent".
        if parent.as_os_str().is_empty() {
            return None;
        }
        Some(parent.to_path_buf())
    }

    fn navigate_back(&mut self) {
        if !self.can_go_back() {
            return;
        }
        self.history_pos -= 1;
        let folder = self.history[self.history_pos - 1].clone();
        self.request_folder_scan(folder, false, false);
    }

    fn navigate_forward(&mut self) {
        if !self.can_go_forward() {
            return;
        }
        let folder = self.history[self.history_pos].clone();
        self.history_pos += 1;
        self.request_folder_scan(folder, false, false);
    }

    fn navigate_up(&mut self) {
        if let Some(parent) = self.parent_folder() {
            self.select_folder(parent);
        }
    }

    // ── Flat image list ────────────────────────────────────────────────────────

    fn refresh_flat_media_cache(&mut self) {
        self.all_media_cache.clear();
        self.all_image_cache.clear();
        self.all_image_labels_short18.clear();
        self.all_media_index_by_path.clear();
        self.all_image_index_by_path.clear();

        for group in &self.folder_groups {
            for path in &group.images {
                let media_index = self.all_media_cache.len();
                self.all_media_index_by_path
                    .insert(path.clone(), media_index);
                if is_image(path) {
                    let image_index = self.all_image_cache.len();
                    self.all_image_index_by_path
                        .insert(path.clone(), image_index);
                    self.all_image_cache.push(path.clone());
                    self.all_image_labels_short18.push(
                        path.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .map(|label| truncate(&label, 18))
                            .unwrap_or_default(),
                    );
                }
                self.all_media_cache.push(path.clone());
            }
        }

        self.media_next_image_index = vec![None; self.all_media_cache.len()];
        self.media_prev_image_index = vec![None; self.all_media_cache.len()];

        let mut next_image = None;
        for idx in (0..self.all_media_cache.len()).rev() {
            self.media_next_image_index[idx] = next_image;
            if let Some(&image_idx) = self.all_image_index_by_path.get(&self.all_media_cache[idx]) {
                next_image = Some(image_idx);
            }
        }

        let mut prev_image = None;
        for (idx, path) in self.all_media_cache.iter().enumerate() {
            self.media_prev_image_index[idx] = prev_image;
            if let Some(&image_idx) = self.all_image_index_by_path.get(path) {
                prev_image = Some(image_idx);
            }
        }
    }

    fn all_images(&self) -> &[PathBuf] {
        &self.all_media_cache
    }

    fn all_slideshow_images(&self) -> &[PathBuf] {
        &self.all_image_cache
    }

    fn queue_thumbnail_jobs_full(&mut self, items: Vec<LoadItem>) {
        let Some(tx) = &self.loader_tx else {
            return;
        };

        self.thumbnail_generation = self.thumbnail_generation.wrapping_add(1);
        self.thumbnail_viewport_signature = None;
        self.thumbnail_cache_done = 0;
        self.thumbnail_cache_total = items.len();
        self.thumbnail_cache_queued_folders = Self::count_queued_folders(&items);
        self.thumbnail_cache_current_path = items.first().map(|item| item.path.clone());
        self.pending_loader_results.clear();
        self.deferred_loader_results.clear();
        self.thumbnail_visible_paths.clear();
        self.thumbnail_prefetch_paths.clear();

        self.queued.clear();
        self.thumbnails.clear();

        for item in &items {
            self.queued.insert(item.path.clone());
        }

        if items.is_empty() {
            self.thumbnail_cache_queued_folders = 0;
            self.thumbnail_cache_current_path = None;
        }

        if !items.is_empty() {
            tx.send(LoadRequest {
                generation: self.thumbnail_generation,
                items,
            })
            .ok();
        }
    }

    fn queue_thumbnail_jobs_incremental(&mut self, items: Vec<LoadItem>) {
        let Some(tx) = &self.loader_tx else {
            return;
        };

        self.thumbnail_cache_done = 0;
        self.thumbnail_cache_total = items.len();
        self.thumbnail_cache_queued_folders = Self::count_queued_folders(&items);
        self.thumbnail_cache_current_path = items.first().map(|item| item.path.clone());

        for item in &items {
            self.queued.insert(item.path.clone());
        }

        if !items.is_empty() {
            tx.send(LoadRequest {
                generation: self.thumbnail_generation,
                items,
            })
            .ok();
        }
    }

    fn prioritized_thumbnail_items_for_paths(&self, paths: &HashSet<PathBuf>) -> Vec<LoadItem> {
        let active_group = self
            .selected_folder
            .as_ref()
            .and_then(|folder| self.folder_groups.iter().position(|g| &g.path == folder));

        let mut items = Vec::new();
        let mut push_group = |group: &FolderGroup, group_boost: i32| {
            for (image_pos, path) in group.images.iter().enumerate() {
                if !paths.contains(path) {
                    continue;
                }
                items.push(LoadItem {
                    priority: group_boost - image_pos as i32,
                    path: path.clone(),
                });
            }
        };

        if let Some(active) = active_group {
            push_group(&self.folder_groups[active], 1_000_000i32);
        }

        for (group_idx, group) in self.folder_groups.iter().enumerate() {
            if Some(group_idx) == active_group {
                continue;
            }
            push_group(group, 100_000i32 - (group_idx as i32 * 1_000));
        }

        items
    }

    fn prioritized_thumbnail_items(&self) -> Vec<LoadItem> {
        let active_group = self
            .selected_folder
            .as_ref()
            .and_then(|folder| self.folder_groups.iter().position(|g| &g.path == folder));

        let mut items = Vec::new();
        let mut push_group = |group: &FolderGroup, group_boost: i32| {
            for (image_pos, path) in group.images.iter().enumerate() {
                items.push(LoadItem {
                    priority: group_boost - image_pos as i32,
                    path: path.clone(),
                });
            }
        };

        if let Some(active) = active_group {
            push_group(&self.folder_groups[active], 1_000_000i32);
        }

        for (group_idx, group) in self.folder_groups.iter().enumerate() {
            if Some(group_idx) == active_group {
                continue;
            }
            push_group(group, 100_000i32 - (group_idx as i32 * 1_000));
        }

        items
    }

    fn count_queued_folders(items: &[LoadItem]) -> usize {
        let mut folders = HashSet::<PathBuf>::new();
        for item in items {
            if let Some(parent) = item.path.parent() {
                if !parent.as_os_str().is_empty() {
                    folders.insert(parent.to_path_buf());
                }
            }
        }
        folders.len()
    }

    fn slideshow_index_for_media_index(&self, media_index: usize) -> Option<usize> {
        if media_index >= self.all_media_cache.len() || self.all_image_cache.is_empty() {
            return None;
        }
        if let Some(idx) = self
            .media_next_image_index
            .get(media_index)
            .copied()
            .flatten()
        {
            return Some(idx);
        }
        self.media_prev_image_index
            .get(media_index)
            .copied()
            .flatten()
    }

    // ── Background polling ─────────────────────────────────────────────────────

    fn poll_loader(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.loader_rx else {
            return;
        };
        let motion_active = self.viewport_motion_active();
        if motion_active {
            ctx.request_repaint_after(Duration::from_millis(120));
            return;
        }
        let mut inserted_any = false;
        for _ in 0..THUMBNAIL_PULL_BUDGET {
            match rx.try_recv() {
                Ok(r) => self.pending_loader_results.push_back(r),
                Err(_) => break,
            }
        }

        let start = Instant::now();
        while let Some(r) = self.pending_loader_results.pop_front() {
            if r.generation == self.thumbnail_generation {
                self.deferred_loader_results.insert(r.path.clone(), r);
            }
        }

        let upload_budget_ms = if self.thumbnail_cache_total > 2_000 {
            THUMBNAIL_UPLOAD_BUDGET_MS.saturating_sub(2).max(1)
        } else {
            THUMBNAIL_UPLOAD_BUDGET_MS
        };
        let upload_budget = Duration::from_millis(upload_budget_ms);

        let mut has_high_priority_pending = false;

        for i in 0..self.thumbnail_visible_paths.len() {
            let media_index = self.thumbnail_visible_paths[i];
            if start.elapsed() >= upload_budget {
                break;
            }
            if let Some(path) = self.all_media_cache.get(media_index).cloned() {
                if self.deferred_loader_results.contains_key(&path) {
                    has_high_priority_pending = true;
                }
                if let Some(r) = self.deferred_loader_results.remove(&path) {
                    if self.upload_thumbnail_result(ctx, r) {
                        inserted_any = true;
                    }
                }
            }
        }

        if start.elapsed() < upload_budget {
            for i in 0..self.thumbnail_prefetch_paths.len().min(4) {
                let media_index = self.thumbnail_prefetch_paths[i];
                if start.elapsed() >= upload_budget {
                    break;
                }
                if let Some(path) = self.all_media_cache.get(media_index).cloned() {
                    if self.deferred_loader_results.contains_key(&path) {
                        has_high_priority_pending = true;
                    }
                    if let Some(r) = self.deferred_loader_results.remove(&path) {
                        if self.upload_thumbnail_result(ctx, r) {
                            inserted_any = true;
                        }
                    }
                }
            }
        }

        if start.elapsed() < upload_budget {
            for _ in 0..2 {
                let Some(path) = self.deferred_loader_results.keys().next().cloned() else {
                    break;
                };
                if start.elapsed() >= upload_budget {
                    break;
                }
                if let Some(r) = self.deferred_loader_results.remove(&path) {
                    if self.upload_thumbnail_result(ctx, r) {
                        inserted_any = true;
                    }
                }
            }
        }

        let busy = !self.pending_loader_results.is_empty()
            || !self.deferred_loader_results.is_empty()
            || !self.queued.is_empty();
        if !busy {
            self.thumbnail_cache_total = 0;
            self.thumbnail_cache_done = 0;
        }
        if busy || inserted_any {
            if inserted_any || has_high_priority_pending {
                ctx.request_repaint_after(Duration::from_millis(16));
            } else {
                // When only background thumbnails remain, back off a little so
                // window moves/resizes don't compete with unnecessary polling.
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }
    }

    fn upload_thumbnail_result(&mut self, ctx: &egui::Context, r: LoadResult) -> bool {
        if r.generation != self.thumbnail_generation {
            return false;
        }
        let path = r.path.clone();
        if self.media_mtimes.get(&r.path).copied() != Some(r.mtime) {
            return false;
        }
        if !self.queued.remove(&r.path) {
            return false;
        }
        let expected = r.width as usize * r.height as usize * 4;
        if r.width == 0 || r.height == 0 || r.rgba.len() != expected {
            return false;
        }
        let tex = ctx.load_texture(
            r.path.to_string_lossy(),
            egui::ColorImage::from_rgba_unmultiplied(
                [r.width as usize, r.height as usize],
                &r.rgba,
            ),
            egui::TextureOptions::default(),
        );
        self.thumbnails.insert(r.path, ThumbState::Ready(tex));
        self.thumbnail_cache_done = self.thumbnail_cache_done.saturating_add(1);
        if self.thumbnail_cache_done % 100 == 0
            || self.thumbnail_cache_done == self.thumbnail_cache_total
        {
            self.thumbnail_cache_current_path = Some(path);
        }
        true
    }

    fn poll_watcher(&mut self) {
        use notify::event::ModifyKind;
        use notify::EventKind;

        let mut relevant = false;
        if let Some(rx) = &self.watcher_rx {
            while let Ok(event_res) = rx.try_recv() {
                if let Ok(ev) = event_res {
                    match ev.kind {
                        EventKind::Create(_)
                        | EventKind::Remove(_)
                        | EventKind::Modify(ModifyKind::Name(_))
                        | EventKind::Modify(ModifyKind::Data(_)) => relevant = true,
                        _ => {}
                    }
                }
            }
        }
        if relevant {
            self.watcher_debounce = Some(Instant::now());
        }

        if let Some(t) = self.watcher_debounce {
            if t.elapsed() >= Duration::from_millis(800) {
                self.watcher_debounce = None;
                match self.view_mode {
                    ViewMode::Folder => {
                        if let Some(folder) = self.selected_folder.clone() {
                            self.rescan_folder(folder);
                        }
                    }
                    ViewMode::Library => {
                        self.rescan_library();
                    }
                }
            }
        }
    }

    fn show_boot_splash(&self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::from_rgb(9, 12, 18)))
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                let center = rect.center();
                let accent = self.theme.accent_fill;
                ui.painter().rect_filled(
                    egui::Rect::from_center_size(center, egui::vec2(420.0, 180.0)),
                    14.0,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 8),
                );
                ui.painter().circle_filled(
                    egui::pos2(center.x - 132.0, center.y - 18.0),
                    22.0,
                    accent,
                );
                ui.painter().text(
                    egui::pos2(center.x - 92.0, center.y - 34.0),
                    egui::Align2::LEFT_TOP,
                    crate::APP_NAME,
                    egui::FontId::proportional(34.0),
                    egui::Color32::from_rgb(242, 246, 252),
                );
                ui.painter().text(
                    egui::pos2(center.x - 92.0, center.y + 10.0),
                    egui::Align2::LEFT_TOP,
                    "Starting…",
                    egui::FontId::proportional(15.0),
                    egui::Color32::from_rgb(160, 172, 188),
                );
                let bar = egui::Rect::from_min_size(
                    egui::pos2(center.x - 92.0, center.y + 42.0),
                    egui::vec2(210.0, 4.0),
                );
                ui.painter()
                    .rect_filled(bar, 2.0, egui::Color32::from_rgb(28, 34, 45));
                let phase = (ctx.input(|i| i.time) as f32 * 2.4).fract();
                let dot_w = 54.0;
                let dot_x = bar.left() + phase * (bar.width() - dot_w);
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2(dot_x, bar.top()),
                        egui::vec2(dot_w, bar.height()),
                    ),
                    2.0,
                    accent,
                );
            });
    }

    fn tick_slideshow(&mut self, ctx: &egui::Context) {
        let n: usize = self.all_image_cache.len();
        if n == 0 {
            return;
        }
        let ss = match &self.slideshow {
            Some(s) => s,
            None => return,
        };
        if ss.paused {
            return;
        }

        if let Some(until) = ss.interaction_pause_until {
            let now = Instant::now();
            if until > now {
                ctx.request_repaint_after(until - now);
                return;
            }
        }

        let fade_enabled = ss.fade_enabled;
        let interval = ss.interval;

        match &ss.phase {
            SsPhase::FadeIn(start) => {
                let t = fade_ease(start.elapsed().as_secs_f32() / FADE_SECS);
                if t >= 1.0 {
                    let ss = self.slideshow.as_mut().unwrap();
                    ss.phase = SsPhase::Visible;
                    let show_secs =
                        (ss.interval - if fade_enabled { FADE_SECS } else { 0.0 }).max(0.1);
                    ctx.request_repaint_after(Duration::from_secs_f32(show_secs));
                } else {
                    ctx.request_repaint();
                }
            }
            SsPhase::Visible => {
                let elapsed = ss.last_advance.elapsed().as_secs_f32();
                let trigger = interval - if fade_enabled { FADE_SECS } else { 0.0 };
                if elapsed >= trigger {
                    let ss = self.slideshow.as_mut().unwrap();
                    if fade_enabled {
                        ss.phase = SsPhase::FadeOut(Instant::now());
                        ctx.request_repaint();
                    } else {
                        ss.index = (ss.index + 1) % n;
                        ss.last_advance = Instant::now();
                        ss.full_tex = None;
                        ss.full_path = None;
                        ctx.request_repaint();
                    }
                } else {
                    let wait = trigger - elapsed;
                    ctx.request_repaint_after(Duration::from_secs_f32(wait));
                }
            }
            SsPhase::FadeOut(start) => {
                let t = fade_ease(start.elapsed().as_secs_f32() / FADE_SECS);
                if t >= 1.0 {
                    let ss = self.slideshow.as_mut().unwrap();
                    ss.index = (ss.index + 1) % n;
                    ss.last_advance = Instant::now();
                    ss.full_tex = None;
                    ss.full_path = None;
                    // Phase transitions to FadeIn once the new texture loads in show_slideshow.
                }
                ctx.request_repaint();
            }
        }
    }

    // ── UI: address bar + nav buttons ──────────────────────────────────────────

    fn show_address_bar(&mut self, ui: &mut Ui) {
        let h = ui.spacing().interact_size.y;
        let can_go_back = self.can_go_back();
        let can_go_forward = self.can_go_forward();
        let can_go_up = self.can_go_up();
        let recent_folders = &self.recent_folders;
        let recent_images = &self.recent_images;
        let managed_folders = &self.managed_folders;
        let managed_folder_labels = &self.managed_folder_labels;
        let recent_folder_labels = &self.recent_folder_labels;
        let recent_image_labels = &self.recent_image_labels;
        let selected_folder = self.selected_folder.as_ref();
        let already_selected = selected_folder
            .as_ref()
            .map(|folder| self.has_managed_folder(folder))
            .unwrap_or(false);
        let library_active = matches!(self.view_mode, ViewMode::Library);
        let mut go_back = false;
        let mut go_forward = false;
        let mut go_up = false;
        let mut open_folder: Option<PathBuf> = None;
        let mut enter_folder: Option<PathBuf> = None;
        let mut open_image: Option<PathBuf> = None;
        let mut add_managed: Option<PathBuf> = None;
        let mut remove_managed: Option<PathBuf> = None;

        ui.horizontal(|ui| {
            // Navigation buttons
            if ui
                .add_enabled(can_go_back, egui::Button::new("◀"))
                .on_hover_text("戻る (Alt+←)")
                .clicked()
            {
                go_back = true;
            }
            if ui
                .add_enabled(can_go_forward, egui::Button::new("▶"))
                .on_hover_text("進む (Alt+→)")
                .clicked()
            {
                go_forward = true;
            }
            if ui
                .add_enabled(can_go_up, egui::Button::new("↑"))
                .on_hover_text("上のフォルダへ")
                .clicked()
            {
                go_up = true;
            }
            ui.label("📁");

            ui.menu_button("＋ Folder", |ui| {
                ui.set_min_width(240.0);
                ui.menu_button("フォルダを追加", |ui| {
                    ui.set_min_width(220.0);
                    if let Some(folder) = selected_folder {
                        if ui
                            .add_enabled(
                                !already_selected,
                                egui::Button::new("現在のフォルダを追加"),
                            )
                            .clicked()
                        {
                            add_managed = Some(folder.to_path_buf());
                            ui.close_menu();
                        }
                        if ui
                            .add_enabled(
                                already_selected,
                                egui::Button::new("現在のフォルダを削除"),
                            )
                            .clicked()
                        {
                            remove_managed = Some(folder.to_path_buf());
                            ui.close_menu();
                        }
                        ui.separator();
                    } else {
                        ui.label("(no folder selected)");
                    }
                    if ui.button("指定されたフォルダを追加").clicked() {
                        ui.close_menu();
                        if let Some(folder) = pick_folder_dialog() {
                            add_managed = Some(folder);
                        }
                    }
                });
                ui.separator();
                ui.label("Managed folders");
                if managed_folders.is_empty() {
                    ui.label("(empty)");
                }
                for (path, label) in managed_folders.iter().zip(managed_folder_labels.iter()) {
                    ui.horizontal(|ui| {
                        if ui
                            .button(label)
                            .on_hover_text(path.display().to_string())
                            .clicked()
                        {
                            open_folder = Some(path.clone());
                            ui.close_menu();
                        }
                        if ui.small_button("×").on_hover_text("Remove").clicked() {
                            remove_managed = Some(path.clone());
                            ui.close_menu();
                        }
                    });
                }
            });

            // Recent menu
            ui.menu_button("Recent▾", |ui| {
                ui.set_min_width(220.0);
                ui.menu_button("📁 Folders", |ui| {
                    ui.set_min_width(220.0);
                    if recent_folders.is_empty() {
                        ui.label("(empty)");
                    }
                    for (path, label) in recent_folders.iter().zip(recent_folder_labels.iter()) {
                        if ui
                            .button(label)
                            .on_hover_text(path.display().to_string())
                            .clicked()
                        {
                            open_folder = Some(path.clone());
                            ui.close_menu();
                        }
                    }
                });
                ui.menu_button("🖼 Images", |ui| {
                    ui.set_min_width(220.0);
                    if recent_images.is_empty() {
                        ui.label("(empty)");
                    }
                    for (path, label) in recent_images.iter().zip(recent_image_labels.iter()) {
                        if ui
                            .button(label)
                            .on_hover_text(path.display().to_string())
                            .clicked()
                        {
                            open_image = Some(path.clone());
                            ui.close_menu();
                        }
                    }
                });
            });

            let restore = if library_active {
                "Library".to_owned()
            } else {
                selected_folder
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };

            if self.address_editing {
                let resp = ui.add_sized(
                    egui::vec2(ui.available_width(), h),
                    egui::TextEdit::singleline(&mut self.address_text),
                );
                if self.address_needs_focus {
                    resp.request_focus();
                    self.address_needs_focus = false;
                }

                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));

                if escape {
                    self.address_text = restore;
                    self.address_editing = false;
                } else if enter {
                    let path = PathBuf::from(&self.address_text);
                    if path.is_dir() {
                        enter_folder = Some(path);
                    } else {
                        self.address_text = restore;
                    }
                    self.address_editing = false;
                } else if resp.lost_focus() {
                    self.address_text = restore;
                    self.address_editing = false;
                }
            } else {
                let resp = ui.add_sized(
                    egui::vec2(ui.available_width(), h),
                    egui::Label::new(egui::RichText::new(&self.address_text).monospace())
                        .sense(egui::Sense::click()),
                );
                if resp.clicked() {
                    self.address_editing = true;
                    self.address_needs_focus = true;
                }
            }
        });

        if go_back {
            self.navigate_back();
        }
        if go_forward {
            self.navigate_forward();
        }
        if go_up {
            self.navigate_up();
        }
        if let Some(folder) = enter_folder {
            self.select_folder(folder);
        }

        if let Some(folder) = open_folder {
            self.select_folder(folder);
        }
        if let Some(img) = open_image {
            self.open_recent_image(img, ui.ctx());
        }
        if let Some(folder) = add_managed {
            self.add_managed_folder(folder);
        }
        if let Some(folder) = remove_managed {
            self.remove_managed_folder(&folder);
        }
    }

    fn open_recent_image(&mut self, path: PathBuf, ctx: &egui::Context) {
        let folder = match path.parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(p) => p.to_path_buf(),
            None => return,
        };
        if matches!(self.view_mode, ViewMode::Library) {
            if self.managed_folders.iter().any(|p| p == &folder) {
                self.rescan_library();
            } else {
                self.request_folder_scan(folder, true, false);
            }
        } else if self.selected_folder.as_deref() != Some(&folder) {
            self.request_folder_scan(folder, true, false);
        }
        if let Some(idx) = self.all_media_index_by_path.get(&path).copied() {
            self.grid_cursor = Some(idx);
            self.single_image = Some(SingleImage::new(idx));
            self.push_recent_image(path);
            ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
        }
    }

    // ── UI: folder tree (left panel) ───────────────────────────────────────────

    fn show_folder_panel(&mut self, ui: &mut Ui) {
        ui.add_space(2.0);
        let theme = &self.theme;
        let managed_folders = &self.managed_folders;
        let managed_folder_labels = &self.managed_folder_labels;
        let mut open_managed: Option<PathBuf> = None;
        let mut add_current: Option<PathBuf> = None;
        let mut remove_managed: Option<PathBuf> = None;
        let mut open_library = false;

        let mut roots = std::mem::take(&mut self.roots);
        let prev = self.selected_folder.clone();
        let mut sel = prev.clone();
        let mut scroll_to_selected = self.left_panel_scroll_to_selected;
        let selected_folder = self.selected_folder.as_ref();
        let selected_folder_present = selected_folder.is_some();
        let library_active = matches!(self.view_mode, ViewMode::Library);
        let selected_is_managed = selected_folder
            .map(|folder| self.has_managed_folder(folder))
            .unwrap_or(false);
        let show_header = !managed_folders.is_empty() || selected_folder_present || library_active;

        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                if show_header {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let library_label = if library_active {
                            egui::RichText::new("Library")
                                .strong()
                                .size(13.0)
                                .color(theme.accent_text)
                        } else {
                            egui::RichText::new("Library").strong().size(13.0)
                        };
                        if ui
                            .selectable_label(library_active, library_label)
                            .on_hover_text("Show all managed folders")
                            .clicked()
                        {
                            open_library = true;
                        }
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new("追加フォルダ").strong().size(13.0));
                        if let Some(folder) = selected_folder {
                            if ui
                                .add_enabled(!selected_is_managed, egui::Button::new("＋"))
                                .on_hover_text("Add current folder")
                                .clicked()
                            {
                                add_current = Some(folder.to_path_buf());
                            }
                        }
                    });
                    ui.separator();
                    if managed_folders.is_empty() {
                        ui.label("(empty)");
                    }
                    for (path, label) in managed_folders.iter().zip(managed_folder_labels.iter()) {
                        ui.horizontal(|ui| {
                            if ui
                                .button(label)
                                .on_hover_text(path.display().to_string())
                                .clicked()
                            {
                                open_managed = Some(path.clone());
                            }
                            if ui.small_button("×").on_hover_text("Remove").clicked() {
                                remove_managed = Some(path.clone());
                            }
                        });
                    }
                    ui.add_space(8.0);
                }
                for node in roots.iter_mut() {
                    // Pass current selection so ancestors auto-expand.
                    show_node(
                        ui,
                        node,
                        &mut sel,
                        prev.as_deref(),
                        &theme,
                        &mut scroll_to_selected,
                    );
                }
            });

        self.roots = roots;
        self.left_panel_scroll_to_selected = scroll_to_selected;

        if let Some(folder) = add_current {
            self.add_managed_folder(folder);
        }
        if let Some(folder) = open_managed {
            self.select_folder(folder);
        }
        if open_library {
            self.select_library();
        }
        if let Some(folder) = remove_managed {
            self.remove_managed_folder(&folder);
        }
        if sel != prev {
            if let Some(f) = sel {
                self.select_folder(f);
            }
        }
    }

    // ── UI: thumbnail grid ─────────────────────────────────────────────────────

    fn show_thumbnail_grid(&mut self, ui: &mut Ui) {
        let selected_image = self.grid_cursor;
        let total_media = self.thumbnail_total_media;
        let total_images = self.thumbnail_total_images;
        let motion_active = self.viewport_motion_active();
        let mut start_ss: Option<usize> = None;
        let mut open_single: Option<usize> = None;
        let mut grid_cursor = self.grid_cursor;
        let mut save_thumbnail_label_setting = false;

        // Toolbar — zoom slider mutates self.thumb_scale live so thumb_px below is current.
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    total_images > 0,
                    egui::Button::new("▶ Slideshow")
                        .fill(self.theme.accent_fill)
                        .stroke(egui::Stroke::new(1.0, self.theme.accent_border)),
                )
                .clicked()
            {
                start_ss = Some(0);
            }
            ui.label(format!("{total_media} items"));
            if total_images != total_media {
                ui.label(format!("({total_images} images)"));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format!("{:.0}%", self.thumb_scale * 100.0));
                ui.add_sized(
                    [80.0, ui.available_height()],
                    egui::Slider::new(&mut self.thumb_scale, 0.25..=4.0)
                        .show_value(false)
                        .clamp_to_range(true),
                );
                ui.label("Zoom:");
            });
            let label_changed = ui
                .checkbox(&mut self.show_thumbnail_filenames, "File names")
                .changed();
            save_thumbnail_label_setting |= label_changed;
        });
        if save_thumbnail_label_setting {
            self.do_save_settings();
        }

        // Compute layout metrics after the slider may have changed thumb_scale.
        let thumb_px = (TILE_BASE_PX * self.thumb_scale).round().max(32.0);
        let cols = ((ui.available_width() / (thumb_px + 20.0)) as usize).max(1);
        let row_h = thumb_px
            + if self.show_thumbnail_filenames {
                26.0
            } else {
                10.0
            };
        self.thumbnail_scroll_rect = Some(ui.max_rect());

        let layout_sig = (
            ui.available_width().round().max(0.0) as u32,
            thumb_px.round().max(0.0) as u32,
            self.folder_groups.len(),
            total_media,
        );
        if self.thumbnail_grid_layout_signature != Some(layout_sig) {
            let mut group_layouts: Vec<GroupLayout> = Vec::with_capacity(self.folder_groups.len());
            let mut row_cursor = 0usize;
            let mut image_cursor = 0usize;
            for (group_idx, group) in self.folder_groups.iter().enumerate() {
                if group.images.is_empty() {
                    continue;
                }
                let image_row_count = (group.images.len() + cols - 1) / cols;
                group_layouts.push(GroupLayout {
                    group_idx,
                    row_start: row_cursor,
                    image_base: image_cursor,
                });
                row_cursor += 1 + image_row_count;
                image_cursor += group.images.len();
            }
            self.thumbnail_grid_layout = Some(ThumbnailGridLayout {
                cols,
                total_rows: row_cursor,
                group_layouts,
            });
            self.thumbnail_grid_layout_signature = Some(layout_sig);
        }
        let Some(grid_layout) = self.thumbnail_grid_layout.as_ref() else {
            return;
        };
        let thumbs = &self.thumbnails;
        let cols = grid_layout.cols;
        let total_rows = grid_layout.total_rows;
        let group_layouts = &grid_layout.group_layouts;
        let prefetch_rows = if motion_active { 0 } else { 3 };
        let visible_priority_paths = &mut self.thumbnail_visible_paths;
        let prefetch_priority_paths = &mut self.thumbnail_prefetch_paths;
        visible_priority_paths.clear();
        prefetch_priority_paths.clear();

        ui.separator();

        let find_layout = |row: usize| -> Option<usize> {
            if group_layouts.is_empty() {
                return None;
            }
            let idx = group_layouts.partition_point(|layout| layout.row_start <= row);
            Some(idx.saturating_sub(1))
        };

        let draw_image_row =
            |ui: &mut Ui,
             row_idx: usize,
             image_base: usize,
             group: &FolderGroup,
             selected_image: Option<usize>,
             grid_cursor: &mut Option<usize>,
             open_single: &mut Option<usize>,
             visible_priority_paths: &mut Vec<usize>| {
                let row_start = row_idx * cols;
                let row_end = (row_start + cols).min(group.images.len());
                ui.horizontal(|ui| {
                    for col in row_start..row_end {
                        let path = &group.images[col];
                        let label = &group.image_labels_short18[col];
                        let is_video = group.image_is_video[col];
                        let gi = image_base + col;
                        ui.vertical(|ui| {
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(thumb_px, thumb_px),
                                egui::Sense::click_and_drag(),
                            );
                            if resp.double_clicked() {
                                *open_single = Some(gi);
                            } else if resp.clicked() {
                                *grid_cursor = Some(gi);
                            }
                            if resp.drag_started() {
                                *grid_cursor = Some(gi);
                                let _ = crate::os_dnd::begin_file_drag(path.clone());
                            }
                            resp.context_menu(|ui| {
                                if ui.button("Open in Explorer").clicked() {
                                    open_in_explorer(path);
                                    ui.close_menu();
                                }
                            });

                            if ui.is_rect_visible(rect) {
                                ui.painter()
                                    .rect_filled(rect, 0.0, egui::Color32::from_gray(30));
                                match thumbs.get(path) {
                                    Some(ThumbState::Ready(tex)) => draw_centered(ui, tex, rect),
                                    Some(ThumbState::Failed) => {
                                        ui.painter().text(
                                            rect.center(),
                                            egui::Align2::CENTER_CENTER,
                                            "✗",
                                            egui::FontId::proportional(20.0),
                                            egui::Color32::from_rgb(200, 80, 80),
                                        );
                                    }
                                    _ => {
                                        ui.painter().text(
                                            rect.center(),
                                            egui::Align2::CENTER_CENTER,
                                            "⋯",
                                            egui::FontId::proportional(20.0),
                                            egui::Color32::from_gray(100),
                                        );
                                    }
                                }
                                if is_video {
                                    let badge = egui::Rect::from_min_size(
                                        egui::pos2(rect.max.x - 24.0, rect.min.y + 4.0),
                                        egui::vec2(20.0, 16.0),
                                    );
                                    ui.painter().rect_filled(
                                        badge,
                                        8.0,
                                        egui::Color32::from_black_alpha(180),
                                    );
                                    ui.painter().text(
                                        badge.center(),
                                        egui::Align2::CENTER_CENTER,
                                        "▶",
                                        egui::FontId::proportional(12.0),
                                        egui::Color32::WHITE,
                                    );
                                }
                                if selected_image == Some(gi) {
                                    ui.painter().rect_stroke(
                                        rect.expand(1.0),
                                        0.0,
                                        egui::Stroke::new(2.0, self.theme.accent_border),
                                    );
                                }
                            }

                            visible_priority_paths.push(gi);

                            if self.show_thumbnail_filenames {
                                ui.label(egui::RichText::new(label).size(10.0));
                            }
                        });
                        if col + 1 < row_end {
                            ui.add_space(8.0);
                        }
                    }
                });
            };

        let collect_row_paths = |row: usize, out: &mut Vec<usize>| {
            let Some(layout_idx) = find_layout(row) else {
                return;
            };
            let layout = &group_layouts[layout_idx];
            if row == layout.row_start {
                return;
            }
            let group = &self.folder_groups[layout.group_idx];
            let row_idx = row - layout.row_start - 1;
            let row_start = row_idx * cols;
            let row_end = (row_start + cols).min(group.images.len());
            out.extend(layout.image_base + row_start..layout.image_base + row_end);
        };

        let prev_scroll_style = ui.spacing().scroll;
        {
            let scroll_style = &mut ui.spacing_mut().scroll;
            scroll_style.bar_width *= 2.0;
            scroll_style.floating_width *= 2.0;
            scroll_style.floating_allocated_width *= 2.0;
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .drag_to_scroll(false)
            .show_rows(ui, row_h, total_rows, |ui, row_range| {
                let visible_start = row_range.start;
                let visible_end = row_range.end.min(total_rows);
                let prefetch_start = visible_start.saturating_sub(prefetch_rows);
                let prefetch_end = (visible_end + prefetch_rows).min(total_rows);

                let mut group_cursor = find_layout(visible_start).unwrap_or(0);
                for row in visible_start..visible_end {
                    while group_cursor + 1 < group_layouts.len()
                        && group_layouts[group_cursor + 1].row_start <= row
                    {
                        group_cursor += 1;
                    }
                    let layout = &group_layouts[group_cursor];
                    let group = &self.folder_groups[layout.group_idx];
                    if row == layout.row_start {
                        ui.label(egui::RichText::new(&group.display_name).strong().size(13.0));
                        ui.separator();
                        continue;
                    }

                    let row_idx = row - layout.row_start - 1;
                    draw_image_row(
                        ui,
                        row_idx,
                        layout.image_base,
                        group,
                        selected_image,
                        &mut grid_cursor,
                        &mut open_single,
                        visible_priority_paths,
                    );
                }

                if prefetch_rows > 0 {
                    for row in prefetch_start..visible_start {
                        collect_row_paths(row, prefetch_priority_paths);
                    }
                    for row in visible_end..prefetch_end {
                        collect_row_paths(row, prefetch_priority_paths);
                    }
                }
            });
        ui.spacing_mut().scroll = prev_scroll_style;
        // Vectors are reused in-place above.

        // Apply pending state changes after the borrow on self.thumbnails ends.
        if let Some(idx) = open_single {
            self.open_single_image_at(idx, ui.ctx());
        } else if let Some(idx) = start_ss {
            self.start_slideshow(idx, ui.ctx());
        }
        self.grid_cursor = grid_cursor;
    }

    // ── UI: single image view ──────────────────────────────────────────────────

    fn show_single_image(&mut self, ui: &mut Ui, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let n = self.all_images().len();
        if n == 0 {
            self.single_image = None;
            return;
        }

        // Keyboard / wheel navigation
        let key_esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let key_copy = ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::C));
        let key_left = ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft));
        let key_right = ctx.input(|i| i.key_pressed(egui::Key::ArrowRight));
        let key_f5 = ctx.input(|i| i.key_pressed(egui::Key::F5));
        let scroll_delta = ctx.input_mut(|i| {
            let mut wheel_steps = 0.0_f32;
            i.events.retain(|event| match event {
                egui::Event::MouseWheel {
                    unit,
                    delta: wheel_delta,
                    modifiers,
                } if !modifiers.ctrl && !modifiers.command => {
                    wheel_steps += single_image_wheel_units(*unit, wheel_delta.y);
                    false
                }
                _ => true,
            });
            wheel_steps
        });
        self.si_scroll_accum += scroll_delta;
        let mut scroll_prev = false;
        let mut scroll_next = false;
        while self.si_scroll_accum >= 1.0 {
            scroll_prev = true;
            self.si_scroll_accum -= 1.0;
        }
        while self.si_scroll_accum <= -1.0 {
            scroll_next = true;
            self.si_scroll_accum += 1.0;
        }

        if key_esc {
            self.close_single_image();
            return;
        }

        // F5: 現在の画像をスタートにしてスライドショー開始
        if key_f5 {
            let idx = self.single_image.as_ref().map(|si| si.index).unwrap_or(0);
            if let Some(ss_idx) = self.slideshow_index_for_media_index(idx) {
                self.start_slideshow(ss_idx, ctx);
                self.single_image = None;
                return;
            }
        }

        let go_prev = key_left || scroll_prev;
        let go_next = key_right || scroll_next;

        let idx = {
            let si = self.single_image.as_mut().unwrap();
            if go_prev {
                si.index = if si.index == 0 { n - 1 } else { si.index - 1 };
                si.tex = None;
            }
            if go_next {
                si.index = (si.index + 1) % n;
                si.tex = None;
            }
            si.index = si.index.min(n - 1);
            si.index
        };
        let media = self.all_images();
        let current_path = media[idx].clone();
        let current_is_video = is_video(&current_path);
        let current_display_name = self
            .single_image
            .as_ref()
            .map(|si| si.display_name.clone())
            .unwrap_or_default();

        #[cfg(windows)]
        let mut video_duration_secs = 0.0_f32;
        #[cfg(windows)]
        let mut video_position_secs = 0.0_f32;
        #[cfg(windows)]
        let video_volume = self.media_volume;
        #[cfg(windows)]
        let video_muted = self.media_muted;
        #[cfg(windows)]
        let mut video_volume_expanded = false;
        #[cfg(windows)]
        let mut video_seek_dragging = false;
        #[cfg(windows)]
        let mut have_video_player = false;
        #[cfg(windows)]
        if current_is_video {
            if let Some(si) = self.single_image.as_mut() {
                video_volume_expanded = si.video_ui.volume_expanded;
                video_seek_dragging = si.video_ui.seek_dragging;
                if let Some(player) = si.video.as_ref() {
                    have_video_player = true;
                    video_duration_secs = player.duration_secs().unwrap_or(0.0);
                    video_position_secs = if video_seek_dragging {
                        si.video_ui.seek_position_secs
                    } else {
                        player.current_position_secs().unwrap_or(0.0)
                    };
                    si.video_ui.seek_position_secs = video_position_secs;
                }
            }
        }

        // Load full-res image or prepare video playback if needed
        let needs_load = self
            .single_image
            .as_ref()
            .map(|si| si.path.as_ref() != Some(&current_path))
            .unwrap_or(false);
        if needs_load {
            if let Some(si) = &mut self.single_image {
                si.path = Some(current_path.clone());
                si.display_name = current_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                #[cfg(windows)]
                {
                    si.video = None;
                }
                if current_is_video {
                    si.tex = None;
                } else if let Some(tex) = load_full_res(&current_path, ctx) {
                    si.tex = Some(tex);
                }
            }
        }

        // ── ボトムパネル: コントロールバー ──────────────────────────────────────
        let controls_height = if current_is_video {
            #[cfg(windows)]
            {
                if video_volume_expanded {
                    100.0
                } else {
                    72.0
                }
            }
            #[cfg(not(windows))]
            {
                72.0
            }
        } else {
            28.0
        };

        #[cfg(windows)]
        let mut seek_commit: Option<f32> = None;
        #[cfg(windows)]
        let mut volume_commit: Option<f32> = None;
        #[cfg(windows)]
        let mut next_seek_dragging = video_seek_dragging;
        #[cfg(windows)]
        let mut next_volume_expanded = video_volume_expanded;
        #[cfg(windows)]
        let mut mute_toggle = false;
        #[cfg(windows)]
        let mut next_media_volume = self.media_volume;
        #[cfg(windows)]
        let mut next_media_muted = self.media_muted;

        egui::TopBottomPanel::bottom("si_controls")
            .exact_height(controls_height)
            .show_inside(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui.button("✕ 閉じる").clicked() {
                        self.grid_cursor = Some(idx);
                        self.single_image = None;
                        return;
                    }
                    ui.label(format!("{} / {n}  {}", idx + 1, current_display_name));
                    if current_is_video {
                        ui.label("Video");
                    }
                    ui.label("  ← → / Scroll  F5 スライドショー  Esc  Ctrl-C コピー");
                });

                if current_is_video {
                    ui.add_space(4.0);
                    #[cfg(windows)]
                    if have_video_player {
                        ui.horizontal(|ui| {
                            let time_label = format!(
                                "{} / {}",
                                format_timecode(video_position_secs),
                                format_timecode(video_duration_secs)
                            );
                            ui.label(time_label);
                            ui.separator();

                            let seek_max = video_duration_secs.max(0.0);
                            let mut seek_value = video_position_secs.clamp(0.0, seek_max);
                            let seek_resp = ui.add_enabled(
                                seek_max > 0.0,
                                egui::Slider::new(&mut seek_value, 0.0..=seek_max)
                                    .show_value(false)
                                    .clamp_to_range(true),
                            );
                            if seek_resp.changed() {
                                seek_commit = Some(seek_value);
                            }
                            next_seek_dragging =
                                seek_resp.dragged() || seek_resp.is_pointer_button_down_on();
                        });

                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            let icon = if video_muted || video_volume <= 0.001 {
                                "🔇"
                            } else {
                                "🔈"
                            };
                            let vol_resp = ui.button(icon);
                            if vol_resp.double_clicked() {
                                mute_toggle = true;
                            } else if vol_resp.clicked() {
                                next_volume_expanded = !next_volume_expanded;
                            }
                            ui.label("音量");
                            if next_volume_expanded {
                                let mut vol_value = video_volume.clamp(0.0, 1.0);
                                let vol_resp = ui.add_sized(
                                    [160.0, 18.0],
                                    egui::Slider::new(&mut vol_value, 0.0..=1.0)
                                        .show_value(false)
                                        .clamp_to_range(true),
                                );
                                if vol_resp.changed() {
                                    volume_commit = Some(vol_value);
                                }
                            }
                        });
                    } else {
                        ui.label("Video playback unavailable");
                    }
                    #[cfg(not(windows))]
                    {
                        ui.label("Video playback unavailable");
                    }
                }
            });

        // クローズボタンが押された（si_controls の return はクロージャのみ）
        if self.single_image.is_none() {
            return;
        }

        // ── 画像エリア: コントロールバー下の残り全面 ────────────────────────────────
        // フィルムストリップは TopBottomPanel を使わず painter で直接オーバーレイ描画。
        // こうすることで画像エリアが縮まず、ストリップが画像の手前に浮く。
        let avail = ui.available_rect_before_wrap();
        let avail_sz = avail.size();

        match (
            current_is_video,
            self.single_image.as_ref().and_then(|si| si.tex.as_ref()),
        ) {
            (true, _) => {
                ui.painter()
                    .rect_filled(avail, 0.0, egui::Color32::from_rgb(10, 10, 10));
                #[cfg(windows)]
                if let Some(parent_hwnd) = hwnd_from_frame(_frame) {
                    if let Some(player) = self.ensure_video_player(parent_hwnd, &current_path) {
                        player.move_to_rect(avail, ctx.pixels_per_point());
                    } else {
                        ui.centered_and_justified(|ui| {
                            ui.label("Video playback unavailable");
                        });
                    }
                }
                #[cfg(not(windows))]
                {
                    ui.centered_and_justified(|ui| {
                        ui.label("Video playback unavailable");
                    });
                }
            }
            (false, Some(tex)) => {
                let ts = tex.size_vec2();
                let scale = (avail_sz.x / ts.x).min(avail_sz.y / ts.y);
                let disp = ts * scale;
                let off = (avail_sz - disp) * 0.5;
                let paint_rect = egui::Rect::from_min_size(avail.min + off, disp);
                ui.painter().image(
                    tex.id(),
                    paint_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
                let drag_resp = ui.interact(
                    paint_rect,
                    ui.id().with("single_image_drag"),
                    egui::Sense::click_and_drag(),
                );
                if drag_resp.drag_started() {
                    let _ = crate::os_dnd::begin_file_drag(current_path.clone());
                }
            }
            (false, None) => {
                ui.centered_and_justified(|ui| {
                    ui.label("Loading…");
                });
            }
        }
        // 残りスペースを確保して下位ウィジェットが割り込まないようにする
        ui.allocate_rect(avail, egui::Sense::hover());

        #[cfg(windows)]
        if current_is_video && self.single_image.is_some() {
            if let Some(si) = self.single_image.as_mut() {
                si.video_ui.seek_dragging = next_seek_dragging;
                si.video_ui.volume_expanded = next_volume_expanded;
                if let Some(pos) = seek_commit {
                    si.video_ui.seek_position_secs = pos;
                }
            }
            if let Some(player) = self.single_image.as_ref().and_then(|si| si.video.as_ref()) {
                if let Some(pos) = seek_commit {
                    let _ = player.set_position_secs(pos);
                }
                if let Some(vol) = volume_commit {
                    next_media_volume = vol.clamp(0.0, 1.0);
                    if next_media_volume > 0.001 {
                        next_media_muted = false;
                    }
                }
                if mute_toggle {
                    next_media_muted = !next_media_muted;
                }
                if volume_commit.is_some() || mute_toggle {
                    self.media_volume = next_media_volume;
                    self.media_muted = next_media_muted;
                    self.apply_media_audio_settings(player);
                    self.do_save_settings();
                }
            }
        }

        if key_copy {
            #[cfg(windows)]
            {
                let player_hwnd = self
                    .single_image
                    .as_ref()
                    .and_then(|si| si.video.as_ref())
                    .map(|player| player.hwnd);
                let _ = copy_current_single_image_to_clipboard(
                    _frame,
                    &current_path,
                    current_is_video,
                    player_hwnd,
                );
            }
            #[cfg(not(windows))]
            {
                let _ = copy_current_single_image_to_clipboard(
                    _frame,
                    &current_path,
                    current_is_video,
                    None,
                );
            }
        }

        // サムネイル帯は別 viewport で表示する。
    }

    fn show_single_image_strip_viewport(&mut self, ctx: &egui::Context) {
        let current = match self.single_image.as_ref() {
            Some(si) => si.index,
            None => return,
        };

        let images = self.all_images();
        if images.is_empty() || current >= images.len() {
            return;
        }

        let strip_indices: [usize; 7] = std::array::from_fn(|i| {
            let offset = i as i64 - FILMSTRIP_RADIUS;
            ((current as i64 + offset).rem_euclid(images.len() as i64)) as usize
        });
        let strip_textures: [Option<&egui::TextureHandle>; 7] = std::array::from_fn(|i| {
            let image_index = strip_indices[i];
            match self.thumbnails.get(&images[image_index]) {
                Some(ThumbState::Ready(tex)) => Some(tex),
                _ => None,
            }
        });
        let strip_count = strip_indices.len();

        let parent_rect = ctx.input(|i| i.viewport().outer_rect);
        let strip_w =
            strip_count as f32 * FILMSTRIP_THUMB_PX + (strip_count - 1) as f32 * 4.0 + 16.0;
        let strip_h = FILMSTRIP_HEIGHT + 16.0;
        let builder = if let Some(parent_rect) = parent_rect {
            let pos = egui::pos2(
                parent_rect.center().x - strip_w / 2.0,
                parent_rect.max.y + 4.0,
            );
            egui::ViewportBuilder::default()
                .with_title(format!("{} Filmstrip", crate::APP_NAME))
                .with_decorations(false)
                .with_resizable(false)
                .with_taskbar(false)
                .with_inner_size([strip_w, strip_h])
                .with_position(pos)
        } else {
            egui::ViewportBuilder::default()
                .with_title(format!("{} Filmstrip", crate::APP_NAME))
                .with_decorations(false)
                .with_resizable(false)
                .with_taskbar(false)
                .with_inner_size([strip_w, strip_h])
        };

        let mut navigate_to = None;
        let mut close_single_image = false;
        ctx.show_viewport_immediate(filmstrip_viewport_id(), builder, |ctx, _class| {
            egui::CentralPanel::default()
                .frame(egui::Frame::none().fill(egui::Color32::from_rgb(12, 12, 12)))
                .show(ctx, |ui| {
                    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                        close_single_image = true;
                        return;
                    }
                    navigate_to = draw_single_image_filmstrip(
                        ui,
                        &self.all_image_labels_short18,
                        &strip_indices,
                        &strip_textures,
                        current,
                        &self.theme,
                    );
                });
        });

        if close_single_image {
            self.close_single_image();
            ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
            return;
        }

        if let Some(new_idx) = navigate_to {
            if let Some(si) = &mut self.single_image {
                si.index = new_idx;
                si.tex = None;
                si.path = None;
            }
            ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
        }
    }

    // ── UI: slideshow ──────────────────────────────────────────────────────────

    fn show_slideshow(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let n = self.all_slideshow_images().len();
        if n == 0 {
            self.stop_slideshow(ctx);
            return;
        }

        let now = Instant::now();
        let (idx, interval, paused, fade_enabled, pixel_perfect, controls_visible) = {
            let ss = self.slideshow.as_mut().unwrap();
            if !ss.paused {
                if matches!(ss.controls_visible_until, Some(until) if until <= now) {
                    ss.controls_visible_until = None;
                }
                if matches!(ss.interaction_pause_until, Some(until) if until <= now) {
                    ss.interaction_pause_until = None;
                }
            } else {
                ss.controls_visible_until = None;
                ss.interaction_pause_until = None;
            }
            (
                ss.index,
                ss.interval,
                ss.paused,
                ss.fade_enabled,
                ss.pixel_perfect,
                ss.paused || ss.controls_visible_until.map_or(false, |until| until > now),
            )
        };

        let key_esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let key_left = ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft));
        let key_right = ctx.input(|i| i.key_pressed(egui::Key::ArrowRight));
        let key_space = ctx.input(|i| i.key_pressed(egui::Key::Space));
        let scroll_delta = ctx.input_mut(|i| {
            let mut wheel_steps = 0.0_f32;
            i.events.retain(|event| match event {
                egui::Event::MouseWheel {
                    unit,
                    delta: wheel_delta,
                    modifiers,
                } if !modifiers.ctrl && !modifiers.command => {
                    wheel_steps += single_image_wheel_units(*unit, wheel_delta.y);
                    false
                }
                _ => true,
            });
            wheel_steps
        });
        self.ss_scroll_accum += scroll_delta;
        let mut scroll_prev = false;
        let mut scroll_next = false;
        while self.ss_scroll_accum >= 1.0 {
            scroll_prev = true;
            self.ss_scroll_accum -= 1.0;
        }
        while self.ss_scroll_accum <= -1.0 {
            scroll_next = true;
            self.ss_scroll_accum += 1.0;
        }

        let mut stop = key_esc;
        let mut go_prev = key_left || scroll_prev;
        let mut go_next = key_right || scroll_next;
        let mut toggle_pause = key_space;
        let mut new_interval = interval;
        let mut new_fade = fade_enabled;
        let mut new_pixel_perfect = pixel_perfect;

        if controls_visible {
            let screen_rect = ctx.screen_rect();
            egui::Area::new("ss_controls_overlay".into())
                .order(egui::Order::Foreground)
                .fixed_pos(screen_rect.min)
                .interactable(true)
                .show(ctx, |ui| {
                    ui.set_min_size(screen_rect.size());
                    ui.set_max_size(screen_rect.size());
                    let panel_w = screen_rect.width().min(1280.0);
                    let panel_h = 48.0;
                    let panel_rect = egui::Rect::from_center_size(
                        egui::pos2(
                            screen_rect.center().x,
                            screen_rect.max.y - panel_h * 0.5 - 12.0,
                        ),
                        egui::vec2(panel_w, panel_h),
                    );
                    ui.painter().rect_filled(
                        panel_rect,
                        10.0,
                        egui::Color32::from_black_alpha(190),
                    );
                    let mut panel_ui = ui.child_ui_with_id_source(
                        panel_rect,
                        egui::Layout::left_to_right(egui::Align::Center),
                        "ss_controls_panel",
                    );
                    panel_ui.set_clip_rect(panel_rect);
                    panel_ui.set_min_size(panel_rect.size());
                    panel_ui.set_max_size(panel_rect.size());
                    panel_ui.horizontal_centered(|ui| {
                        stop |= ui.button("⏹ Stop").clicked();
                        go_prev |= ui.button("◀").clicked();
                        toggle_pause |= ui.button(if paused { "▶" } else { "⏸" }).clicked();
                        go_next |= ui.button("▶▶").clicked();
                        ui.label(format!("{} / {n}", idx + 1));
                        ui.separator();
                        ui.label("Interval:");
                        let mut interval_pos = slideshow_interval_to_slider(new_interval);
                        if ui
                            .add(egui::Slider::new(&mut interval_pos, 0.0..=1.0).show_value(false))
                            .changed()
                        {
                            new_interval = slider_to_slideshow_interval(interval_pos);
                        }
                        ui.label(format!("{new_interval:.1} s"));
                        ui.separator();
                        ui.checkbox(&mut new_fade, "Fade");
                        ui.checkbox(&mut new_pixel_perfect, "Pixel perfect");
                    });
                });
        }

        if stop {
            if let Some(ss) = &mut self.slideshow {
                ss.interval = new_interval;
                ss.fade_enabled = new_fade;
                ss.pixel_perfect = new_pixel_perfect;
            }
            self.default_slideshow_interval = new_interval;
            self.default_fade_enabled = new_fade;
            self.default_slideshow_pixel_perfect = new_pixel_perfect;
            self.stop_slideshow(ctx);
            return;
        }

        if let Some(ss) = &mut self.slideshow {
            if go_prev {
                ss.index = if ss.index == 0 { n - 1 } else { ss.index - 1 };
                ss.last_advance = Instant::now();
                ss.full_tex = None;
                ss.full_path = None;
                ss.phase = SsPhase::Visible;
                ss.suppress_next_fade = true;
            }
            if go_next {
                ss.index = (ss.index + 1) % n;
                ss.last_advance = Instant::now();
                ss.full_tex = None;
                ss.full_path = None;
                ss.phase = SsPhase::Visible;
                ss.suppress_next_fade = true;
            }
            if toggle_pause {
                ss.paused = !ss.paused;
                if !ss.paused {
                    ss.last_advance = Instant::now();
                } else {
                    ss.controls_visible_until = None;
                    ss.interaction_pause_until = None;
                }
            }
            if (new_interval - ss.interval).abs() > 0.01 {
                ss.interval = new_interval;
                self.default_slideshow_interval = new_interval;
            }
            if new_fade != ss.fade_enabled {
                ss.fade_enabled = new_fade;
                self.default_fade_enabled = new_fade;
                if matches!(ss.phase, SsPhase::FadeOut(_)) {
                    ss.phase = SsPhase::Visible;
                }
            }
            if new_pixel_perfect != ss.pixel_perfect {
                ss.pixel_perfect = new_pixel_perfect;
                self.default_slideshow_pixel_perfect = new_pixel_perfect;
            }
        }

        let idx = self.slideshow.as_ref().unwrap().index.min(n - 1);
        if let Some(ss) = &mut self.slideshow {
            ss.index = idx;
        }
        let images = self.all_slideshow_images();
        let current_path = images[idx].clone();

        let needs_load = self
            .slideshow
            .as_ref()
            .map(|ss| ss.full_path.as_ref() != Some(&current_path))
            .unwrap_or(false);
        if needs_load {
            if let Some(tex) = load_full_res(&current_path, ctx) {
                if let Some(ss) = &mut self.slideshow {
                    let fade_in =
                        ss.fade_enabled && ss.has_shown_first_frame && !ss.suppress_next_fade;
                    ss.full_tex = Some(tex);
                    ss.full_path = Some(current_path.clone());
                    ss.phase = if fade_in {
                        SsPhase::FadeIn(Instant::now())
                    } else {
                        SsPhase::Visible
                    };
                    ss.has_shown_first_frame = true;
                    ss.suppress_next_fade = false;
                    ctx.request_repaint();
                }
            }
        }

        let avail = ui.available_rect_before_wrap();
        let available = avail.size();
        let (tex_id, tex_size, alpha) = match self.slideshow.as_ref() {
            Some(ss) => {
                let a = if ss.fade_enabled {
                    match &ss.phase {
                        SsPhase::FadeIn(start) => {
                            fade_ease(start.elapsed().as_secs_f32() / FADE_SECS)
                        }
                        SsPhase::Visible => 1.0,
                        SsPhase::FadeOut(start) => {
                            1.0 - fade_ease(start.elapsed().as_secs_f32() / FADE_SECS)
                        }
                    }
                } else {
                    1.0
                };
                match ss.full_tex.as_ref() {
                    Some(tex) => (Some(tex.id()), tex.size_vec2(), a),
                    None => (None, egui::Vec2::ZERO, a),
                }
            }
            None => (None, egui::Vec2::ZERO, 1.0),
        };

        match tex_id {
            Some(id) => {
                let paint_rect = if pixel_perfect {
                    let ppp = ctx.pixels_per_point().max(0.0001);
                    let avail_px = available * ppp;
                    let disp_px = tex_size;
                    let offset_px = ((avail_px - disp_px) * 0.5).round();
                    let min = avail.min + offset_px / ppp;
                    egui::Rect::from_min_size(min, disp_px / ppp)
                } else {
                    let scale = (available.x / tex_size.x).min(available.y / tex_size.y);
                    let disp = tex_size * scale;
                    let offset = (available - disp) * 0.5;
                    egui::Rect::from_min_size(avail.min + offset, disp)
                };
                let tint =
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, (alpha * 255.0) as u8);
                ui.painter().image(
                    id,
                    paint_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    tint,
                );
                ui.allocate_rect(paint_rect, egui::Sense::hover());
            }
            None => {
                ui.centered_and_justified(|ui| {
                    ui.label("Loading…");
                });
            }
        }
    }
}

// ── eframe::App ────────────────────────────────────────────────────────────────

impl eframe::App for PicViewApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if !self.startup_ready {
            if let Some(rx) = &self.startup_rx {
                if self.startup_init.is_none() {
                    if let Ok(init) = rx.try_recv() {
                        self.startup_init = Some(init);
                    }
                }
            }
            self.show_boot_splash(ctx);
            if self.startup_init.is_some()
                && self.boot_started_at.elapsed().as_secs_f32() >= MIN_BOOT_SPLASH_SECS
            {
                if let Some(init) = self.startup_init.take() {
                    self.apply_startup_result(init, ctx);
                    self.startup_rx = None;
                }
            }
            ctx.request_repaint_after(Duration::from_millis(16));
            return;
        }

        // ── スプラッシュ + 遅延初期化 ──────────────────────────────────────────────
        self.tick_viewport_motion(ctx);
        let motion_active = self.viewport_motion_active();
        if !motion_active {
            self.poll_scan(ctx);
            self.poll_loader(ctx);
            self.tick_thumbnail_scroll_decay(ctx);
        }
        self.poll_watcher();
        if self.slideshow.is_some() && Self::slideshow_activity_detected(ctx) {
            self.note_slideshow_activity(ctx);
        }
        self.tick_slideshow(ctx);

        // Alt + arrow keys for back/forward navigation (browser-style)
        let alt_left = ctx.input(|i| i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft));
        let alt_right = ctx.input(|i| i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight));
        if alt_left {
            self.navigate_back();
        }
        if alt_right {
            self.navigate_forward();
        }

        if self.slideshow.is_some() {
            self.thumbnail_visible_paths.clear();
            self.thumbnail_prefetch_paths.clear();
            // Full-screen slideshow
            egui::CentralPanel::default()
                .frame(egui::Frame::none().fill(egui::Color32::BLACK))
                .show(ctx, |ui| {
                    self.show_slideshow(ui, ctx);
                });
        } else if self.single_image.is_some() {
            self.thumbnail_visible_paths.clear();
            self.thumbnail_prefetch_paths.clear();
            // Full-screen single image view (no folder panel, no address bar)
            egui::CentralPanel::default()
                .frame(egui::Frame::none().fill(egui::Color32::BLACK))
                .show(ctx, |ui| {
                    self.show_single_image(ui, ctx, frame);
                });
            self.show_single_image_strip_viewport(ctx);
        } else {
            // Ctrl+Scroll → zoom with inertia.
            // Consume events so the inner ScrollArea doesn't also scroll.
            let zoom_delta = ctx.input_mut(|i| {
                if !i.modifiers.ctrl {
                    return 0.0;
                }
                let d = i.raw_scroll_delta.y;
                i.raw_scroll_delta = egui::Vec2::ZERO;
                i.events.retain(|e| !matches!(e, egui::Event::Scroll(_)));
                d
            });
            // 速度に加算して減衰させることで慣性を表現
            if zoom_delta != 0.0 {
                self.zoom_vel += zoom_delta * 0.00075;
            }
            self.zoom_vel *= 0.82;
            if self.zoom_vel.abs() > 0.0003 {
                self.thumb_scale = (self.thumb_scale + self.zoom_vel).clamp(0.25, 4.0);
                ctx.request_repaint();
            } else {
                self.zoom_vel = 0.0;
            }

            // Accelerate plain mouse-wheel scrolling in the thumbnail view when the
            // user keeps scrolling in the same direction. A single wheel step moves
            // at least half a thumbnail height so the grid feels less jittery.
            let mut thumb_scroll_modified = false;
            let scroll_hot = self.thumbnail_scroll_rect.is_some_and(|rect| {
                ctx.input(|i| i.pointer.hover_pos().is_some_and(|pos| rect.contains(pos)))
            });
            if scroll_hot {
                let thumb_scroll_px = (TILE_BASE_PX * self.thumb_scale).round().max(32.0);
                let thumb_scroll_min_step = thumb_scroll_px * THUMBNAIL_SCROLL_MIN_STEP_RATIO;
                let mouse_wheel_event = ctx.input(|i| {
                    i.events
                        .iter()
                        .any(|e| matches!(e, egui::Event::MouseWheel { .. }))
                });
                ctx.input_mut(|i| {
                    if i.modifiers.ctrl {
                        return;
                    }
                    let delta_y = i.raw_scroll_delta.y;
                    if delta_y.abs() <= f32::EPSILON {
                        return;
                    }

                    let now = Instant::now();
                    let dir = delta_y.signum() as i8;
                    // Treat same-direction wheel input within a short time window as
                    // one continuous gesture so the acceleration feels deliberate.
                    let streak_active = self.thumbnail_scroll_last_at.is_some_and(|t| {
                        now.duration_since(t).as_secs_f32() <= THUMBNAIL_SCROLL_ACCEL_RESET_SECS
                    });
                    if streak_active && self.thumbnail_scroll_last_dir == dir {
                        self.thumbnail_scroll_streak =
                            self.thumbnail_scroll_streak.saturating_add(1);
                    } else {
                        self.thumbnail_scroll_streak = 1;
                    }
                    self.thumbnail_scroll_last_dir = dir;
                    self.thumbnail_scroll_last_at = Some(now);
                    self.thumbnail_scroll_last_decay_at = Some(now);

                    let accel = thumbnail_scroll_accel_multiplier(self.thumbnail_scroll_streak);
                    let step = if mouse_wheel_event {
                        thumb_scroll_min_step
                    } else {
                        delta_y.abs().max(thumb_scroll_min_step)
                    };
                    let adjusted = dir as f32 * step * accel;
                    i.raw_scroll_delta.y = adjusted;
                    i.smooth_scroll_delta.y = adjusted;
                    thumb_scroll_modified = true;
                });
            }
            if thumb_scroll_modified {
                ctx.request_repaint();
            }

            egui::TopBottomPanel::bottom("thumbnail_status_bar")
                .exact_height(28.0)
                .frame(egui::Frame::none().fill(self.theme.sidebar_bg))
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        let location = self
                            .selected_folder
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "Library".to_owned());
                        ui.label(egui::RichText::new(location).strong());
                        ui.separator();
                        ui.label(format!("{} items", self.thumbnail_total_media));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if self.thumbnail_cache_total > 0 && !self.queued.is_empty() {
                                let mut text = format!(
                                    "画像をキャッシュ中 {}/{}",
                                    self.thumbnail_cache_done, self.thumbnail_cache_total
                                );
                                text.push_str(&format!(
                                    "  {} folders",
                                    self.thumbnail_cache_queued_folders
                                ));
                                if let Some(path) = &self.thumbnail_cache_current_path {
                                    text.push_str(&format!("  {}", path.display()));
                                }
                                ui.monospace(text);
                            } else {
                                ui.monospace(format!("zoom {:.0}%", self.thumb_scale * 100.0));
                            }
                        });
                    });
                });

            // Ctrl+L: focus address bar
            let ctrl_l = ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::L));
            if ctrl_l {
                self.address_editing = true;
                self.address_needs_focus = true;
            }

            // F5: スライドショー開始（先頭から）
            let key_f5 = ctx.input(|i| i.key_pressed(egui::Key::F5));
            if key_f5 && !self.address_editing {
                let n: usize = self.thumbnail_total_media;
                if n > 0 {
                    self.start_slideshow(0, ctx);
                }
            }

            // ← → in grid: open single-image view at prev/next position
            if !self.address_editing {
                let n: usize = self.thumbnail_total_media;
                if n > 0 {
                    let key_left =
                        ctx.input(|i| !i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft));
                    let key_right =
                        ctx.input(|i| !i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight));
                    let key_enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));
                    if key_enter {
                        if let Some(idx) = self.grid_cursor {
                            self.open_single_image_at(idx.min(n - 1), ctx);
                        }
                    }
                    if key_right {
                        let idx = self.grid_cursor.map(|c| (c + 1) % n).unwrap_or(0);
                        self.grid_cursor = Some(idx);
                        self.single_image = Some(SingleImage::new(idx));
                        ctx.send_viewport_cmd_to(
                            egui::ViewportId::ROOT,
                            egui::ViewportCommand::Focus,
                        );
                    } else if key_left {
                        let idx = self
                            .grid_cursor
                            .map(|c| if c == 0 { n - 1 } else { c - 1 })
                            .unwrap_or(n - 1);
                        self.grid_cursor = Some(idx);
                        self.single_image = Some(SingleImage::new(idx));
                        ctx.send_viewport_cmd_to(
                            egui::ViewportId::ROOT,
                            egui::ViewportCommand::Focus,
                        );
                    }
                }
            }

            egui::TopBottomPanel::top("address_bar")
                .exact_height(32.0)
                .show(ctx, |ui| {
                    ui.add_space(4.0);
                    self.show_address_bar(ui);
                });

            egui::SidePanel::left("folder_panel")
                .default_width(240.0)
                .min_width(160.0)
                .frame(egui::Frame::none().fill(self.theme.sidebar_bg))
                .show(ctx, |ui| {
                    self.show_folder_panel(ui);
                });

            egui::CentralPanel::default()
                .frame(egui::Frame::none().fill(self.theme.content_bg))
                .show(ctx, |ui| {
                    if self.scan_in_progress {
                        ui.centered_and_justified(|ui| {
                            ui.label(
                                self.scan_message
                                    .clone()
                                    .unwrap_or_else(|| "Scanning…".to_owned()),
                            );
                        });
                    } else {
                        self.show_thumbnail_grid(ui);
                    }
                });
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(ss) = &self.slideshow {
            self.default_slideshow_interval = ss.interval;
            self.default_fade_enabled = ss.fade_enabled;
        }
        self.do_save_settings();

        // DB の最終チェックポイントをバックグラウンドスレッドで実施。
        // loader_tx が drop された後にローダーが Arc を手放し、スレッドが
        // 最後のホルダーになった時点で SQLite 接続が閉じる。
        if let Some(cache) = self._cache.take() {
            std::thread::spawn(move || {
                // Arc の参照カウントが 1 になるまでスピン（ローダーが手放すのを待つ）。
                let mut c = cache;
                loop {
                    c = match Arc::try_unwrap(c) {
                        Ok(mutex) => {
                            // 最後の参照: チェックポイントしてクローズ
                            if let Ok(tc) = mutex.into_inner() {
                                tc.checkpoint_and_close();
                            }
                            return;
                        }
                        Err(arc) => arc,
                    };
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            });
        }
    }
}

// ── Free functions ─────────────────────────────────────────────────────────────

fn show_node(
    ui: &mut Ui,
    node: &mut FolderNode,
    selected: &mut Option<PathBuf>,
    expand_to: Option<&Path>,
    theme: &ThemePalette,
    scroll_to_selected: &mut bool,
) {
    let id = ui.make_persistent_id(node.path.as_os_str());
    let mut state =
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);

    // Auto-expand this node when the target path lives somewhere inside it.
    if expand_to
        .map(|t| t.starts_with(&node.path) && t != node.path)
        .unwrap_or(false)
    {
        if !state.is_open() {
            state.set_open(true);
            state.store(ui.ctx());
        }
    }

    let has_children = node.children_loaded || node.has_visible_children();
    let is_sel = selected.as_ref() == Some(&node.path);
    let is_drive = is_windows_drive_root(&node.path);
    let row_h = if is_drive {
        28.0
    } else {
        ui.spacing().interact_size.y.max(22.0)
    };
    let (row_rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), row_h),
        egui::Sense::click(),
    );

    if is_drive {
        ui.painter().rect_filled(row_rect, 4.0, theme.drive_bg);
    } else if is_sel {
        ui.painter().rect_filled(row_rect, 4.0, theme.accent_fill);
        let border_rect = egui::Rect::from_min_max(
            row_rect.min,
            egui::pos2(row_rect.min.x + 4.0, row_rect.max.y),
        );
        ui.painter()
            .rect_filled(border_rect, 0.0, theme.accent_hover);
        ui.painter()
            .rect_stroke(row_rect, 4.0, egui::Stroke::new(1.0, theme.accent_border));
    } else if response.hovered() {
        ui.painter()
            .rect_filled(row_rect, 4.0, egui::Color32::from_rgb(229, 231, 236));
    }

    if response.clicked() {
        *selected = Some(node.path.clone());
    }
    if is_sel && *scroll_to_selected {
        response.scroll_to_me(Some(egui::Align::Center));
        *scroll_to_selected = false;
    }

    let font_size = if is_drive { 15.0 } else { 13.0 };
    let text_color = if is_drive {
        theme.drive_text
    } else if is_sel {
        theme.accent_text
    } else {
        theme.text_main
    };

    let arrow = if has_children {
        if state.is_open() {
            "▾"
        } else {
            "▸"
        }
    } else {
        "•"
    };
    let arrow_color = if is_drive {
        theme.drive_text
    } else if is_sel {
        theme.accent_text
    } else {
        theme.text_muted
    };
    let arrow_pos = egui::pos2(row_rect.left() + 10.0, row_rect.center().y);
    ui.painter().text(
        arrow_pos,
        egui::Align2::CENTER_CENTER,
        arrow,
        egui::FontId::proportional(font_size),
        arrow_color,
    );

    if has_children {
        let toggle_rect = egui::Rect::from_min_size(
            egui::pos2(row_rect.left(), row_rect.top()),
            egui::vec2(22.0, row_rect.height()),
        );
        let toggle_resp = ui.interact(toggle_rect, id.with("toggle"), egui::Sense::click());
        if toggle_resp.clicked() || response.double_clicked() {
            state.toggle(ui);
        }
    }

    let text_pos = egui::pos2(row_rect.left() + 24.0, row_rect.center().y);
    ui.painter().text(
        text_pos,
        egui::Align2::LEFT_CENTER,
        &node.name,
        egui::FontId::proportional(font_size),
        text_color,
    );

    state.store(ui.ctx());

    if state.is_open() {
        ui.indent(id, |ui| {
            node.ensure_children();
            for child in node.children.iter_mut() {
                show_node(ui, child, selected, expand_to, theme, scroll_to_selected);
            }
        });
    }
}

fn is_windows_drive_root(path: &Path) -> bool {
    cfg!(windows) && path.to_string_lossy().ends_with(":\\")
}

fn draw_centered(ui: &Ui, tex: &egui::TextureHandle, rect: egui::Rect) {
    let ts = tex.size_vec2();
    let side = rect.width().min(rect.height());
    let scale = (side / ts.x).min(side / ts.y);
    let disp = ts * scale;
    let off = (rect.size() - disp) * 0.5;
    let img_rect = egui::Rect::from_min_size(rect.min + off, disp);
    ui.painter().image(
        tex.id(),
        img_rect,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
}

/// Load a Japanese system font as a fallback so CJK characters render correctly.
/// Tries common Windows fonts in order; silently skips if none are found.
#[cfg(windows)]
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    let candidates: &[(&str, u32)] = &[
        (r"C:\Windows\Fonts\yugothic.ttf", 0),
        (r"C:\Windows\Fonts\YuGothR.ttc", 0),
        (r"C:\Windows\Fonts\YuGothM.ttc", 0),
        (r"C:\Windows\Fonts\meiryo.ttc", 0),
        (r"C:\Windows\Fonts\msgothic.ttc", 0),
    ];
    for &(path, index) in candidates {
        if let Ok(data) = std::fs::read(path) {
            let mut fd = egui::FontData::from_owned(data);
            fd.index = index;
            fonts.font_data.insert("jp".to_owned(), fd);
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push("jp".to_owned());
            }
            crate::log::append(format!("font loaded: {path}"));
            break;
        }
    }

    ctx.set_fonts(fonts);
}

/// No-op on non-Windows platforms.
#[cfg(not(windows))]
fn setup_fonts(ctx: &egui::Context) {
    ctx.set_fonts(egui::FontDefinitions::default());
}

/// Load a folder icon texture using the Windows shell icon on Windows,
/// falling back to None on other platforms.
#[cfg(windows)]
fn load_folder_icon_tex(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let rgba = load_windows_folder_icon_data()?;
    Some(ctx.load_texture(
        "folder_icon",
        egui::ColorImage::from_rgba_unmultiplied([32, 32], &rgba),
        egui::TextureOptions::default(),
    ))
}

#[cfg(not(windows))]
fn load_folder_icon_tex(_ctx: &egui::Context) -> Option<egui::TextureHandle> {
    None
}

/// Extract the Windows shell folder icon (32×32) as RGBA bytes.
/// Uses SHGetFileInfoW with SHGFI_USEFILEATTRIBUTES so no real path is needed,
/// then renders it into a 32bpp DIBSECTION via DrawIconEx to preserve alpha.
#[cfg(windows)]
fn load_windows_folder_icon_data() -> Option<Vec<u8>> {
    use winapi::ctypes::c_void;
    use winapi::um::shellapi::{
        SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES,
    };
    use winapi::um::wingdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, SelectObject,
        BITMAPINFO, BITMAPV5HEADER, BI_BITFIELDS, DIB_RGB_COLORS,
    };
    use winapi::um::winuser::{DestroyIcon, DrawIconEx};
    const DI_NORMAL: u32 = 0x0003;
    use std::mem;
    use std::os::windows::ffi::OsStrExt;

    const SIZE: i32 = 32;
    // FILE_ATTRIBUTE_DIRECTORY = 0x10; no winnt feature needed.
    const FILE_ATTR_DIR: u32 = 0x10;

    unsafe {
        let path: Vec<u16> = std::ffi::OsStr::new("folder")
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut sfi: SHFILEINFOW = mem::zeroed();
        let ok = SHGetFileInfoW(
            path.as_ptr(),
            FILE_ATTR_DIR,
            &mut sfi,
            mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
        );
        if ok == 0 || sfi.hIcon.is_null() {
            return None;
        }
        let hicon = sfi.hIcon;

        // Create a memory DC and a 32bpp DIBSection with an explicit alpha mask
        // so that DrawIconEx writes correct alpha bytes.
        let hdc = CreateCompatibleDC(std::ptr::null_mut());
        if hdc.is_null() {
            DestroyIcon(hicon);
            return None;
        }

        let mut bmi: BITMAPV5HEADER = mem::zeroed();
        bmi.bV5Size = mem::size_of::<BITMAPV5HEADER>() as u32;
        bmi.bV5Width = SIZE;
        bmi.bV5Height = -SIZE; // top-down scanlines
        bmi.bV5Planes = 1;
        bmi.bV5BitCount = 32;
        bmi.bV5Compression = BI_BITFIELDS;
        bmi.bV5RedMask = 0x00FF_0000;
        bmi.bV5GreenMask = 0x0000_FF00;
        bmi.bV5BlueMask = 0x0000_00FF;
        bmi.bV5AlphaMask = 0xFF00_0000;

        let mut bits: *mut c_void = std::ptr::null_mut();
        let hbm = CreateDIBSection(
            hdc,
            &bmi as *const BITMAPV5HEADER as *const BITMAPINFO,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        if hbm.is_null() {
            DeleteDC(hdc);
            DestroyIcon(hicon);
            return None;
        }

        let old = SelectObject(hdc, hbm as *mut c_void);
        DrawIconEx(
            hdc,
            0,
            0,
            hicon,
            SIZE,
            SIZE,
            0,
            std::ptr::null_mut(),
            DI_NORMAL,
        );
        GdiFlush();
        SelectObject(hdc, old);

        let n = (SIZE * SIZE * 4) as usize;
        let mut bgra = vec![0u8; n];
        std::ptr::copy_nonoverlapping(bits as *const u8, bgra.as_mut_ptr(), n);

        DeleteObject(hbm as *mut c_void);
        DeleteDC(hdc);
        DestroyIcon(hicon);

        // Windows DIBs are BGRA; egui expects RGBA.
        for c in bgra.chunks_exact_mut(4) {
            c.swap(0, 2);
        }

        crate::log::append("Windows folder icon loaded".to_string());
        Some(bgra)
    }
}

/// Open the containing folder in the system file manager with the file selected.
fn open_in_explorer(path: &Path) {
    #[cfg(windows)]
    {
        // `/select,<path>` highlights the file in Explorer.
        let arg = format!("/select,{}", path.display());
        std::process::Command::new("explorer").arg(arg).spawn().ok();
    }
    #[cfg(not(windows))]
    {
        if let Some(parent) = path.parent() {
            std::process::Command::new("xdg-open")
                .arg(parent)
                .spawn()
                .ok();
        }
    }
}

fn load_full_res(path: &Path, ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let img = image::open(path).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(ctx.load_texture(
        path.to_string_lossy(),
        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba.into_raw()),
        egui::TextureOptions::default(),
    ))
}

#[cfg(windows)]
fn copy_rgba_to_clipboard(
    owner_hwnd: windows_sys::Win32::Foundation::HWND,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> bool {
    use std::mem;
    use std::ptr::copy_nonoverlapping;
    use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
    use windows_sys::Win32::Graphics::Gdi::{BITMAPV5HEADER, BI_BITFIELDS};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
    };
    use windows_sys::Win32::System::Ole::CF_DIBV5;

    if rgba.len() != width as usize * height as usize * 4 {
        return false;
    }

    let mut header: BITMAPV5HEADER = unsafe { mem::zeroed() };
    header.bV5Size = mem::size_of::<BITMAPV5HEADER>() as u32;
    header.bV5Width = width as i32;
    header.bV5Height = -(height as i32);
    header.bV5Planes = 1;
    header.bV5BitCount = 32;
    header.bV5Compression = BI_BITFIELDS;
    header.bV5RedMask = 0x00FF_0000;
    header.bV5GreenMask = 0x0000_FF00;
    header.bV5BlueMask = 0x0000_00FF;
    header.bV5AlphaMask = 0xFF00_0000;

    let payload_size = mem::size_of::<BITMAPV5HEADER>() + rgba.len();
    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, payload_size) };
    if hglobal.is_null() {
        return false;
    }

    let ptr = unsafe { GlobalLock(hglobal) };
    if ptr.is_null() {
        unsafe {
            let _ = GlobalFree(hglobal);
        }
        return false;
    }

    unsafe {
        copy_nonoverlapping(
            &header as *const BITMAPV5HEADER as *const u8,
            ptr as *mut u8,
            mem::size_of::<BITMAPV5HEADER>(),
        );
        copy_nonoverlapping(
            rgba.as_ptr(),
            (ptr as *mut u8).add(mem::size_of::<BITMAPV5HEADER>()),
            rgba.len(),
        );
        GlobalUnlock(hglobal);
    }

    let opened = unsafe { OpenClipboard(owner_hwnd) } != 0;
    if !opened {
        unsafe {
            let _ = GlobalFree(hglobal);
        }
        return false;
    }

    let mut ok = false;
    unsafe {
        if EmptyClipboard() != 0 {
            let set = SetClipboardData(CF_DIBV5 as u32, hglobal as HANDLE);
            if !set.is_null() {
                ok = true;
            } else {
                let _ = GlobalFree(hglobal);
            }
        } else {
            let _ = GlobalFree(hglobal);
        }
        let _ = CloseClipboard();
    }
    ok
}

#[cfg(windows)]
fn capture_window_rgba(hwnd: windows_sys::Win32::Foundation::HWND) -> Option<(u32, u32, Vec<u8>)> {
    use std::mem;
    use std::ptr::{copy_nonoverlapping, null_mut};
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC,
        SelectObject, BITMAPV5HEADER, BI_BITFIELDS, DIB_RGB_COLORS, SRCCOPY,
    };
    use windows_sys::Win32::Storage::Xps::PrintWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;

    let mut rect = RECT::default();
    let got = unsafe { GetClientRect(hwnd, &mut rect) };
    if got == 0 {
        return None;
    }
    let width = (rect.right - rect.left).max(0) as u32;
    let height = (rect.bottom - rect.top).max(0) as u32;
    if width == 0 || height == 0 {
        return None;
    }

    let screen_dc = unsafe { GetDC(hwnd) };
    if screen_dc.is_null() {
        return None;
    }
    let mem_dc = unsafe { CreateCompatibleDC(screen_dc) };
    if mem_dc.is_null() {
        unsafe {
            let _ = windows_sys::Win32::Graphics::Gdi::ReleaseDC(hwnd, screen_dc);
        }
        return None;
    }

    let mut bmi: BITMAPV5HEADER = unsafe { mem::zeroed() };
    bmi.bV5Size = mem::size_of::<BITMAPV5HEADER>() as u32;
    bmi.bV5Width = width as i32;
    bmi.bV5Height = -(height as i32);
    bmi.bV5Planes = 1;
    bmi.bV5BitCount = 32;
    bmi.bV5Compression = BI_BITFIELDS;
    bmi.bV5RedMask = 0x00FF_0000;
    bmi.bV5GreenMask = 0x0000_FF00;
    bmi.bV5BlueMask = 0x0000_00FF;
    bmi.bV5AlphaMask = 0xFF00_0000;

    let mut bits: *mut core::ffi::c_void = null_mut();
    let hbm = unsafe {
        CreateDIBSection(
            mem_dc,
            &bmi as *const BITMAPV5HEADER as *const _,
            DIB_RGB_COLORS,
            &mut bits,
            null_mut(),
            0,
        )
    };
    if hbm.is_null() || bits.is_null() {
        unsafe {
            let _ = DeleteDC(mem_dc);
            let _ = windows_sys::Win32::Graphics::Gdi::ReleaseDC(hwnd, screen_dc);
        }
        if !hbm.is_null() {
            unsafe {
                let _ = DeleteObject(hbm as _);
            }
        }
        return None;
    }

    let old = unsafe { SelectObject(mem_dc, hbm as _) };
    let painted = unsafe { PrintWindow(hwnd, mem_dc, 1) } != 0;
    if !painted {
        unsafe {
            let _ = windows_sys::Win32::Graphics::Gdi::BitBlt(
                mem_dc,
                0,
                0,
                width as i32,
                height as i32,
                screen_dc,
                0,
                0,
                SRCCOPY,
            );
        }
    }
    unsafe {
        GdiFlush();
        let _ = SelectObject(mem_dc, old);
    }

    let mut rgba = vec![0u8; (width * height * 4) as usize];
    unsafe {
        copy_nonoverlapping(bits as *const u8, rgba.as_mut_ptr(), rgba.len());
        let _ = DeleteObject(hbm as _);
        let _ = DeleteDC(mem_dc);
        let _ = windows_sys::Win32::Graphics::Gdi::ReleaseDC(hwnd, screen_dc);
    }

    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }

    Some((width, height, rgba))
}

#[cfg(windows)]
fn copy_current_single_image_to_clipboard(
    frame: &eframe::Frame,
    path: &Path,
    current_is_video: bool,
    player_hwnd: Option<windows_sys::Win32::Foundation::HWND>,
) -> bool {
    let owner_hwnd = hwnd_from_frame(frame).unwrap_or(core::ptr::null_mut());
    if current_is_video {
        let Some(hwnd) = player_hwnd else {
            return false;
        };
        let Some((width, height, rgba)) = capture_window_rgba(hwnd) else {
            return false;
        };
        return copy_rgba_to_clipboard(owner_hwnd, width, height, &rgba);
    }

    let Ok(img) = image::open(path) else {
        return false;
    };
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    copy_rgba_to_clipboard(owner_hwnd, width, height, &rgba)
}

#[cfg(not(windows))]
fn copy_current_single_image_to_clipboard(
    _frame: &eframe::Frame,
    _path: &Path,
    _current_is_video: bool,
    _player_hwnd: Option<()>,
) -> bool {
    false
}
