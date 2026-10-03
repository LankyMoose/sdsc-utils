//! Process-lifetime cache of downscaled iced image handles for Start icons.
//!
//! Full-size Steam capsules (e.g. 600×900) uploaded into iced's 2048² image atlas
//! force `Atlas::grow` → `Texture::create_view`. A failed grow (memory / device
//! loss) panics the process. We decode once, fit to 2× the portrait cell, and
//! reuse one [`Handle`] so atlas churn stays bounded.

use iced::widget::image::Handle;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// Display cell is 48×72; upload at 2× for Cover scaling.
pub const CACHE_W: u32 = 96;
pub const CACHE_H: u32 = 144;
/// Immersive hero capsule; upload at this size for Cover scaling (~2× a 160×240 cell).
pub const HERO_W: u32 = 320;
pub const HERO_H: u32 = 480;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CacheKey {
    /// Raster file (Steam library art, custom shortcut image).
    File(PathBuf),
    /// Shell-extracted icon for an executable / shortcut target.
    Shell(PathBuf),
    /// Larger raster for the immersive selected-game capsule.
    HeroFile(PathBuf),
    /// Larger shell extract for immersive manual shortcuts.
    HeroShell(PathBuf),
}

static CACHE: LazyLock<Mutex<HashMap<CacheKey, Option<Handle>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Fit `src` into `max_w`×`max_h` preserving aspect (integer sizes, at least 1).
pub fn fit_within(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 || max_w == 0 || max_h == 0 {
        return (1, 1);
    }
    if src_w <= max_w && src_h <= max_h {
        return (src_w, src_h);
    }
    let sw = f64::from(src_w);
    let sh = f64::from(src_h);
    let scale = (f64::from(max_w) / sw).min(f64::from(max_h) / sh);
    let dw = (sw * scale).round().max(1.0) as u32;
    let dh = (sh * scale).round().max(1.0) as u32;
    (dw.min(max_w).max(1), dh.min(max_h).max(1))
}

/// Cached handle for a raster image path (Steam art or custom icon). Quiet on miss.
pub fn handle_for_path(path: &Path) -> Option<Handle> {
    lookup(CacheKey::File(path.to_path_buf()), || {
        decode_file(path, false)
    })
}

/// Like [`handle_for_path`], but logs a warn when a user-chosen file fails to decode.
pub fn handle_for_path_warn(path: &Path) -> Option<Handle> {
    lookup(CacheKey::File(path.to_path_buf()), || {
        decode_file(path, true)
    })
}

/// Cached handle for a shell-extracted icon (exe / lnk target).
pub fn handle_for_shell(path: &Path) -> Option<Handle> {
    lookup(CacheKey::Shell(path.to_path_buf()), || {
        decode_shell(path, CACHE_H)
    })
}

/// Immersive hero handle for a raster path (selected game + neighbors only).
pub fn hero_for_path(path: &Path) -> Option<Handle> {
    lookup(CacheKey::HeroFile(path.to_path_buf()), || {
        decode_file_sized(path, HERO_W, HERO_H, false)
    })
}

/// Immersive hero handle for a shell-extracted icon.
pub fn hero_for_shell(path: &Path) -> Option<Handle> {
    lookup(CacheKey::HeroShell(path.to_path_buf()), || {
        decode_shell(path, HERO_H)
    })
}

/// Warm the cache for Steam library paths (call from a blocking worker).
pub fn prepare_paths<I, P>(paths: I)
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    for path in paths {
        let _ = handle_for_path(path.as_ref());
    }
}

fn lookup(key: CacheKey, load: impl FnOnce() -> Option<(u32, u32, Vec<u8>)>) -> Option<Handle> {
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(&key) {
            return entry.clone();
        }
    }
    let pixels = load();
    let handle = pixels.map(|(w, h, px)| Handle::from_rgba(w, h, px));
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    // Another thread may have filled the slot; prefer the first winner.
    if let Some(existing) = cache.get(&key) {
        return existing.clone();
    }
    cache.insert(key, handle.clone());
    handle
}

fn decode_file(path: &Path, warn_on_fail: bool) -> Option<(u32, u32, Vec<u8>)> {
    decode_file_sized(path, CACHE_W, CACHE_H, warn_on_fail)
}

fn decode_file_sized(
    path: &Path,
    max_w: u32,
    max_h: u32,
    warn_on_fail: bool,
) -> Option<(u32, u32, Vec<u8>)> {
    let img = match image::open(path) {
        Ok(img) => img,
        Err(err) => {
            if warn_on_fail {
                crate::platform::app_log::warn(format!(
                    "start icon: failed to decode {}: {err}",
                    path.display()
                ));
            }
            return None;
        }
    };
    let (src_w, src_h) = (img.width(), img.height());
    let (dst_w, dst_h) = fit_within(src_w, src_h, max_w, max_h);
    let rgba = if dst_w == src_w && dst_h == src_h {
        img.to_rgba8()
    } else {
        img.resize_exact(dst_w, dst_h, image::imageops::FilterType::Triangle)
            .to_rgba8()
    };
    crate::controller::hid::diag::diag_info(format!(
        "ui-diag: icon handle path={} src={src_w}x{src_h} dst={}x{} max={max_w}x{max_h}",
        path.display(),
        rgba.width(),
        rgba.height()
    ));
    Some((rgba.width(), rgba.height(), rgba.into_raw()))
}

fn decode_shell(path: &Path, size: u32) -> Option<(u32, u32, Vec<u8>)> {
    let (width, height, pixels) = crate::platform::file_icon::rgba_for_path(path, size)?;
    crate::controller::hid::diag::diag_info(format!(
        "ui-diag: icon handle shell={} src={width}x{height} dst={width}x{height} size={size}",
        path.display()
    ));
    Some((width, height, pixels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn fit_600x900_into_cache_cell() {
        assert_eq!(fit_within(600, 900, CACHE_W, CACHE_H), (96, 144));
    }

    #[test]
    fn fit_600x900_into_hero_cell() {
        assert_eq!(fit_within(600, 900, HERO_W, HERO_H), (320, 480));
    }

    #[test]
    fn fit_already_small_unchanged() {
        assert_eq!(fit_within(48, 72, CACHE_W, CACHE_H), (48, 72));
    }

    #[test]
    fn same_path_reuses_handle_id() {
        // Unique temp path so parallel tests do not collide on the process cache.
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sdsc-icon-cache-{n}"));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("tiny.png");
        {
            // 2×2 opaque red PNG
            let mut enc = png::Encoder::new(std::fs::File::create(&path).unwrap(), 2, 2);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer
                .write_image_data(&[
                    255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
                ])
                .unwrap();
        }
        let a = handle_for_path(&path).expect("decode");
        let b = handle_for_path(&path).expect("cached");
        assert_eq!(a.id(), b.id());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
