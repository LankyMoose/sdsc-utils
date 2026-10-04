//! Process-lifetime cache of downscaled iced image handles for Start icons.
//!
//! Full-size Steam capsules (e.g. 600×900) uploaded into iced's 2048² image atlas
//! force `Atlas::grow` → `Texture::create_view`. A failed grow (memory / device
//! loss) panics the process. We decode once, fit to 2× the portrait cell, and
//! reuse one [`Handle`] so atlas churn stays bounded.
//!
//! Immersive hero/backdrop and list-icon decode must run off the UI thread
//! (`prepare_*` / warm workers). UI code uses [`icon_cached`] / [`hero_cached`] /
//! [`backdrop_cached`] (and shell peeks) only.

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
/// Immersive full-bleed landscape backdrop (fits atlas; Cover-scaled on screen).
pub const BACKDROP_W: u32 = 1280;
pub const BACKDROP_H: u32 = 720;

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
    /// Landscape Steam art for immersive atmosphere.
    BackdropFile(PathBuf),
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

/// Immersive hero handle for a raster path (worker / preload only).
pub fn hero_for_path(path: &Path) -> Option<Handle> {
    lookup(CacheKey::HeroFile(path.to_path_buf()), || {
        decode_file_sized(path, HERO_W, HERO_H, false)
    })
}

/// Immersive hero handle for a shell-extracted icon (worker / preload only).
pub fn hero_for_shell(path: &Path) -> Option<Handle> {
    lookup(CacheKey::HeroShell(path.to_path_buf()), || {
        decode_shell(path, HERO_H)
    })
}

/// Immersive landscape backdrop handle (worker / preload only).
pub fn backdrop_for_path(path: &Path) -> Option<Handle> {
    lookup(CacheKey::BackdropFile(path.to_path_buf()), || {
        decode_file_sized(path, BACKDROP_W, BACKDROP_H, false)
    })
}

/// Peek list-tier raster — never decodes on the calling thread.
pub fn icon_cached(path: &Path) -> Option<Handle> {
    peek(CacheKey::File(path.to_path_buf()))
}

/// Peek list-tier shell extract — never decodes.
pub fn icon_shell_cached(path: &Path) -> Option<Handle> {
    peek(CacheKey::Shell(path.to_path_buf()))
}

/// Peek hero raster — never decodes on the calling thread.
pub fn hero_cached(path: &Path) -> Option<Handle> {
    peek(CacheKey::HeroFile(path.to_path_buf()))
}

/// Peek hero shell extract — never decodes.
pub fn hero_shell_cached(path: &Path) -> Option<Handle> {
    peek(CacheKey::HeroShell(path.to_path_buf()))
}

/// Peek backdrop — never decodes on the calling thread.
pub fn backdrop_cached(path: &Path) -> Option<Handle> {
    peek(CacheKey::BackdropFile(path.to_path_buf()))
}

/// Warm the cache for Steam library list paths (call from a blocking worker).
pub fn prepare_paths<I, P>(paths: I)
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    for path in paths {
        let _ = handle_for_path(path.as_ref());
    }
}

/// Warm immersive hero + backdrop tiers (blocking worker only).
///
/// Backdrops first so splash peeks beat strip heroes when filling the catalog.
pub fn prepare_immersive(
    hero_paths: impl IntoIterator<Item = impl AsRef<Path>>,
    backdrop_paths: impl IntoIterator<Item = impl AsRef<Path>>,
) {
    let mut heroes = 0u32;
    let mut backdrops = 0u32;
    for path in backdrop_paths {
        if backdrop_for_path(path.as_ref()).is_some() {
            backdrops += 1;
        }
    }
    for path in hero_paths {
        if hero_for_path(path.as_ref()).is_some() {
            heroes += 1;
        }
    }
    crate::controller::hid::diag::diag_info(format!(
        "ui-diag: immersive art prepare heroes={heroes} backdrops={backdrops}"
    ));
}

/// Drop paths already in the process cache. Returns `(heroes, backdrops, skipped)`.
pub fn filter_uncached_immersive(
    hero_paths: impl IntoIterator<Item = PathBuf>,
    backdrop_paths: impl IntoIterator<Item = PathBuf>,
) -> (Vec<PathBuf>, Vec<PathBuf>, usize) {
    let mut skipped = 0usize;
    let mut heroes = Vec::new();
    for path in hero_paths {
        if hero_cached(&path).is_some() {
            skipped += 1;
        } else {
            heroes.push(path);
        }
    }
    let mut backdrops = Vec::new();
    for path in backdrop_paths {
        if backdrop_cached(&path).is_some() {
            skipped += 1;
        } else {
            backdrops.push(path);
        }
    }
    (heroes, backdrops, skipped)
}

/// Pull the selected cover out of a neighbor backdrop list so it can decode first.
///
/// Returns `(selected, rest)`. If `selected` is `Some` but missing from `backdrops`,
/// it is still returned so callers can priority-decode it.
pub fn split_selected_backdrop(
    selected: Option<PathBuf>,
    backdrops: Vec<PathBuf>,
) -> (Option<PathBuf>, Vec<PathBuf>) {
    let Some(sel) = selected else {
        return (None, backdrops);
    };
    let mut rest = Vec::with_capacity(backdrops.len());
    let mut found = None;
    for path in backdrops {
        if found.is_none() && path == sel {
            found = Some(path);
        } else {
            rest.push(path);
        }
    }
    (found.or(Some(sel)), rest)
}

/// Warm a window of hero + backdrop paths (selection neighbor warm).
///
/// Decodes **backdrops first** (splash cover), then heroes. Prefer
/// [`filter_uncached_immersive`] before calling so cache hits stay off the worker.
pub fn warm_immersive_paths(
    hero_paths: impl IntoIterator<Item = impl AsRef<Path>>,
    backdrop_paths: impl IntoIterator<Item = impl AsRef<Path>>,
) {
    for path in backdrop_paths {
        let _ = backdrop_for_path(path.as_ref());
    }
    for path in hero_paths {
        let _ = hero_for_path(path.as_ref());
    }
}

fn peek(key: CacheKey) -> Option<Handle> {
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.get(&key).and_then(|entry| entry.clone())
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

    fn write_tiny_png(path: &Path) {
        let mut enc = png::Encoder::new(std::fs::File::create(path).unwrap(), 2, 2);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer
            .write_image_data(&[
                255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
            ])
            .unwrap();
    }

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
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sdsc-icon-cache-{n}"));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("tiny.png");
        write_tiny_png(&path);
        let a = handle_for_path(&path).expect("decode");
        let b = handle_for_path(&path).expect("cached");
        assert_eq!(a.id(), b.id());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn peek_list_icon_none_until_prepare() {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sdsc-icon-list-peek-{n}"));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("list.png");
        write_tiny_png(&path);
        assert!(icon_cached(&path).is_none());
        assert!(handle_for_path(&path).is_some());
        assert!(icon_cached(&path).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn peek_hero_none_until_prepare() {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sdsc-icon-hero-peek-{n}"));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("hero.png");
        write_tiny_png(&path);
        assert!(hero_cached(&path).is_none());
        assert!(hero_for_path(&path).is_some());
        assert!(hero_cached(&path).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn peek_backdrop_none_until_prepare() {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sdsc-icon-backdrop-peek-{n}"));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bd.png");
        write_tiny_png(&path);
        assert!(backdrop_cached(&path).is_none());
        prepare_immersive([&path], [&path]);
        assert!(hero_cached(&path).is_some());
        assert!(backdrop_cached(&path).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filter_uncached_skips_warm_hits() {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sdsc-icon-filter-{n}"));
        let _ = std::fs::create_dir_all(&dir);
        let warm = dir.join("warm.png");
        let cold = dir.join("cold.png");
        write_tiny_png(&warm);
        write_tiny_png(&cold);
        assert!(backdrop_for_path(&warm).is_some());
        // Backdrop tier warm only — hero path still needs work.
        let (heroes, backdrops, skipped) =
            filter_uncached_immersive(vec![warm.clone()], vec![warm.clone(), cold.clone()]);
        assert_eq!(heroes, vec![warm.clone()]);
        assert_eq!(backdrops, vec![cold.clone()]);
        assert_eq!(skipped, 1);
        assert!(hero_for_path(&warm).is_some());
        let (heroes, backdrops, skipped) =
            filter_uncached_immersive(vec![warm.clone()], vec![warm.clone(), cold.clone()]);
        assert!(heroes.is_empty());
        assert_eq!(backdrops, vec![cold]);
        assert_eq!(skipped, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_selected_backdrop_pulls_cover_first() {
        let sel = PathBuf::from("sel.png");
        let a = PathBuf::from("a.png");
        let b = PathBuf::from("b.png");
        let (got, rest) =
            split_selected_backdrop(Some(sel.clone()), vec![a.clone(), sel.clone(), b.clone()]);
        assert_eq!(got, Some(sel));
        assert_eq!(rest, vec![a.clone(), b]);

        let sel2 = PathBuf::from("missing.png");
        let (got, rest) = split_selected_backdrop(Some(sel2.clone()), vec![a.clone()]);
        assert_eq!(got, Some(sel2));
        assert_eq!(rest, vec![a]);
    }
}
