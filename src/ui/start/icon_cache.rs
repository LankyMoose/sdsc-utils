//! Bounded process cache of downscaled iced image handles for Start icons.
//!
//! Full-size Steam capsules (e.g. 600×900) uploaded into iced's 2048² image atlas
//! force `Atlas::grow` → `Texture::create_view`. A failed grow (memory / device
//! loss) panics the process. We decode once, fit to 2× the portrait cell, and
//! reuse one [`Handle`] so atlas churn stays bounded.
//!
//! Decode runs on the single-flight [`super::art_worker`]. UI code uses
//! [`icon_cached`] / [`hero_cached`] / [`backdrop_cached`] (and shell peeks) only.
//!
//! Pins mark the current selection window; unpinned entries are kept until the
//! per-tier LRU cap is hit. Until the first [`pin_art_window`], inserts are
//! unbounded (unit tests / cold boot).

use iced::widget::image::Handle;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

/// Bumped on each selection warm so UI can ignore stale ArtEvents.
static ART_WARM_GEN: AtomicU64 = AtomicU64::new(0);

/// Display cell is 48×72; upload at 2× for Cover scaling.
pub const CACHE_W: u32 = 96;
pub const CACHE_H: u32 = 144;
/// Immersive hero capsule; upload at this size for Cover scaling (~2× a 160×240 cell).
pub const HERO_W: u32 = 320;
pub const HERO_H: u32 = 480;
/// Immersive full-bleed landscape backdrop (fits atlas; Cover-scaled on screen).
pub const BACKDROP_W: u32 = 1280;
pub const BACKDROP_H: u32 = 720;
/// Catalog indices pinned / prefetched around the selection (all art tiers).
pub const ART_WINDOW: isize = 10;

const LRU_LIST_CAP: usize = 48;
const LRU_HERO_CAP: usize = 32;
const LRU_BACKDROP_CAP: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CacheKey {
    File(PathBuf),
    Shell(PathBuf),
    HeroFile(PathBuf),
    HeroShell(PathBuf),
    BackdropFile(PathBuf),
}

impl CacheKey {
    fn tier(&self) -> Tier {
        match self {
            Self::File(_) | Self::Shell(_) => Tier::List,
            Self::HeroFile(_) | Self::HeroShell(_) => Tier::Hero,
            Self::BackdropFile(_) => Tier::Backdrop,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    List,
    Hero,
    Backdrop,
}

impl Tier {
    fn cap(self) -> usize {
        match self {
            Self::List => LRU_LIST_CAP,
            Self::Hero => LRU_HERO_CAP,
            Self::Backdrop => LRU_BACKDROP_CAP,
        }
    }
}

/// Paths kept hot around the current selection (+ dialog / crossfade pins).
#[derive(Debug, Clone, Default)]
pub struct ArtRetainSet {
    pub list_files: Vec<PathBuf>,
    pub list_shells: Vec<PathBuf>,
    pub heroes: Vec<PathBuf>,
    pub hero_shells: Vec<PathBuf>,
    pub backdrops: Vec<PathBuf>,
}

impl ArtRetainSet {
    pub fn merged_with(&self, other: &ArtRetainSet) -> ArtRetainSet {
        let mut out = self.clone();
        for p in &other.list_files {
            if !out.list_files.contains(p) {
                out.list_files.push(p.clone());
            }
        }
        for p in &other.list_shells {
            if !out.list_shells.contains(p) {
                out.list_shells.push(p.clone());
            }
        }
        for p in &other.heroes {
            if !out.heroes.contains(p) {
                out.heroes.push(p.clone());
            }
        }
        for p in &other.hero_shells {
            if !out.hero_shells.contains(p) {
                out.hero_shells.push(p.clone());
            }
        }
        for p in &other.backdrops {
            if !out.backdrops.contains(p) {
                out.backdrops.push(p.clone());
            }
        }
        out
    }

    fn into_keys(self) -> HashSet<CacheKey> {
        let mut keys = HashSet::new();
        for path in self.list_files {
            keys.insert(CacheKey::File(path));
        }
        for path in self.list_shells {
            keys.insert(CacheKey::Shell(path));
        }
        for path in self.heroes {
            keys.insert(CacheKey::HeroFile(path));
        }
        for path in self.hero_shells {
            keys.insert(CacheKey::HeroShell(path));
        }
        for path in self.backdrops {
            keys.insert(CacheKey::BackdropFile(path));
        }
        keys
    }
}

struct CacheEntry {
    handle: Option<Handle>,
    tick: u64,
}

struct CacheState {
    map: HashMap<CacheKey, CacheEntry>,
    /// `None` until the first pin — decode inserts are unrestricted (tests).
    pins: Option<HashSet<CacheKey>>,
    tick: u64,
    /// Bumped by [`clear_all`]; in-flight decodes must not re-insert after a wipe.
    clear_epoch: u64,
}

impl CacheState {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            pins: None,
            tick: 0,
            clear_epoch: 0,
        }
    }

    fn next_tick(&mut self) -> u64 {
        self.tick = self.tick.wrapping_add(1);
        self.tick
    }

    fn is_pinned(&self, key: &CacheKey) -> bool {
        match &self.pins {
            None => true,
            Some(set) => set.contains(key),
        }
    }

    fn touch(&mut self, key: &CacheKey) {
        let tick = self.next_tick();
        if let Some(entry) = self.map.get_mut(key) {
            entry.tick = tick;
        }
    }

    fn tier_count(&self, tier: Tier) -> usize {
        self.map.keys().filter(|k| k.tier() == tier).count()
    }

    /// Evict unpinned LRU entries in `tier` until at or under cap.
    fn trim_tier(&mut self, tier: Tier) {
        let cap = tier.cap();
        loop {
            if self.tier_count(tier) <= cap {
                break;
            }
            let victim = self
                .map
                .iter()
                .filter(|(k, _)| k.tier() == tier && !self.is_pinned(k))
                .min_by_key(|(_, e)| e.tick)
                .map(|(k, _)| k.clone());
            let Some(key) = victim else {
                // All remaining are pinned — allow temporary overage.
                break;
            };
            self.map.remove(&key);
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: icon cache lru evict tier={tier:?}"
            ));
        }
    }
}

static CACHE: LazyLock<Mutex<CacheState>> = LazyLock::new(|| Mutex::new(CacheState::new()));

/// Catalog indices within `radius` of `selected`.
///
/// Immersive (circular) wraps when `len >=` strip visible count; compact is linear.
pub fn art_window_indices(
    selected: usize,
    len: usize,
    circular: bool,
    radius: isize,
) -> Vec<usize> {
    use crate::ui::start::vstrip::VISIBLE;
    if len == 0 {
        return Vec::new();
    }
    let radius = radius.max(0);
    let selected = selected.min(len - 1);
    let mut out = Vec::with_capacity((radius as usize * 2 + 1).min(len));
    if circular && len >= VISIBLE {
        for delta in -radius..=radius {
            let idx = (selected as isize + delta).rem_euclid(len as isize) as usize;
            if !out.contains(&idx) {
                out.push(idx);
            }
        }
    } else {
        let start = selected.saturating_sub(radius as usize);
        let end = (selected + radius as usize).min(len - 1);
        for idx in start..=end {
            out.push(idx);
        }
    }
    out
}

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

/// Start a new warm generation (UI ignores ArtEvents from older gens).
pub fn bump_art_warm_generation() -> u64 {
    ART_WARM_GEN.fetch_add(1, Ordering::Relaxed) + 1
}

/// True while `generation` is still the latest warm request.
pub fn art_warm_is_current(generation: u64) -> bool {
    ART_WARM_GEN.load(Ordering::Relaxed) == generation
}

/// Drop every cached handle and clear pins (process Exit).
///
/// In-flight worker decodes that finish after this call are not re-inserted.
/// Also resets the art-warm generation so the next process starts at gen 0.
pub fn clear_all() {
    let mut state = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let n = state.map.len();
    state.map.clear();
    state.pins = None;
    state.tick = 0;
    state.clear_epoch = state.clear_epoch.wrapping_add(1);
    ART_WARM_GEN.store(0, Ordering::Relaxed);
    crate::controller::hid::diag::diag_info(format!(
        "ui-diag: icon cache clear entries={n} epoch={}",
        state.clear_epoch
    ));
}

/// Update the pin set for the selection window. Does **not** drop unpinned entries;
/// LRU trims them when a tier exceeds its cap.
pub fn pin_art_window(set: ArtRetainSet) {
    let keys = set.into_keys();
    let mut state = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    state.pins = Some(keys);
    state.trim_tier(Tier::List);
    state.trim_tier(Tier::Hero);
    state.trim_tier(Tier::Backdrop);
    let pinned = state.pins.as_ref().map(|p| p.len()).unwrap_or(0);
    let kept = state.map.len();
    crate::controller::hid::diag::diag_info(format!(
        "ui-diag: icon cache pin pinned={pinned} cached={kept}"
    ));
}

/// Compatibility alias used by older call sites / tests.
pub fn retain_art_window(set: ArtRetainSet) {
    pin_art_window(set);
}

/// Cached handle for a raster image path (Steam art or custom icon). Quiet on miss.
pub fn handle_for_path(path: &Path) -> Option<Handle> {
    lookup(CacheKey::File(path.to_path_buf()), false, || {
        decode_file(path, false)
    })
}

/// Like [`handle_for_path`], but logs a warn when a user-chosen file fails to decode.
pub fn handle_for_path_warn(path: &Path) -> Option<Handle> {
    lookup(CacheKey::File(path.to_path_buf()), true, || {
        decode_file(path, true)
    })
}

/// Cached handle for a shell-extracted icon (exe / lnk target).
pub fn handle_for_shell(path: &Path) -> Option<Handle> {
    lookup(CacheKey::Shell(path.to_path_buf()), false, || {
        decode_shell(path, CACHE_H)
    })
}

/// Shell extract that always stores (add-shortcut dialog probe).
pub fn handle_for_shell_pinned(path: &Path) -> Option<Handle> {
    lookup(CacheKey::Shell(path.to_path_buf()), true, || {
        decode_shell(path, CACHE_H)
    })
}

/// Immersive hero handle for a raster path (worker / preload only).
pub fn hero_for_path(path: &Path) -> Option<Handle> {
    lookup(CacheKey::HeroFile(path.to_path_buf()), false, || {
        decode_file_sized(path, HERO_W, HERO_H, false)
    })
}

/// Immersive hero handle for a shell-extracted icon (worker / preload only).
pub fn hero_for_shell(path: &Path) -> Option<Handle> {
    lookup(CacheKey::HeroShell(path.to_path_buf()), false, || {
        decode_shell(path, HERO_H)
    })
}

/// Immersive landscape backdrop handle (worker / preload only).
pub fn backdrop_for_path(path: &Path) -> Option<Handle> {
    lookup(CacheKey::BackdropFile(path.to_path_buf()), false, || {
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

/// Warm the cache for list-tier raster paths (blocking / tests).
pub fn prepare_paths<I, P>(paths: I)
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    for path in paths {
        let _ = handle_for_path(path.as_ref());
    }
}

/// Warm immersive hero + backdrop tiers (blocking / tests).
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

/// Drop list-tier paths already cached. Returns `(files, shells, skipped)`.
pub fn filter_uncached_list(
    file_paths: impl IntoIterator<Item = PathBuf>,
    shell_paths: impl IntoIterator<Item = PathBuf>,
) -> (Vec<PathBuf>, Vec<PathBuf>, usize) {
    let mut skipped = 0usize;
    let mut files = Vec::new();
    for path in file_paths {
        if icon_cached(&path).is_some() {
            skipped += 1;
        } else {
            files.push(path);
        }
    }
    let mut shells = Vec::new();
    for path in shell_paths {
        if icon_shell_cached(&path).is_some() {
            skipped += 1;
        } else {
            shells.push(path);
        }
    }
    (files, shells, skipped)
}

/// Pull the selected cover out of a neighbor backdrop list so it can decode first.
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

fn peek(key: CacheKey) -> Option<Handle> {
    let mut state = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if state.map.contains_key(&key) {
        state.touch(&key);
        return state.map.get(&key).and_then(|e| e.handle.clone());
    }
    None
}

fn lookup(
    key: CacheKey,
    force_pin: bool,
    load: impl FnOnce() -> Option<(u32, u32, Vec<u8>)>,
) -> Option<Handle> {
    let epoch_at_start = {
        let mut state = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if state.map.contains_key(&key) {
            state.touch(&key);
            return state.map.get(&key).and_then(|e| e.handle.clone());
        }
        // When pins are active, skip decode for unpinned keys that would only be
        // dropped immediately (tier already at cap with no unpinned victims).
        if !force_pin
            && state.pins.is_some()
            && !state.is_pinned(&key)
            && state.tier_count(key.tier()) >= key.tier().cap()
            && state
                .map
                .iter()
                .filter(|(k, _)| k.tier() == key.tier() && !state.is_pinned(k))
                .count()
                == 0
        {
            return None;
        }
        state.clear_epoch
    };
    let pixels = load();
    let handle = pixels.map(|(w, h, px)| Handle::from_rgba(w, h, px));
    let mut state = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if state.clear_epoch != epoch_at_start {
        // Wiped while decoding — discard so close/exit stay at a blank slate.
        return None;
    }
    if let Some(existing) = state.map.get(&key) {
        return existing.handle.clone();
    }
    if force_pin {
        if let Some(pins) = state.pins.as_mut() {
            pins.insert(key.clone());
        }
    }
    let tick = state.next_tick();
    let tier = key.tier();
    state.map.insert(
        key,
        CacheEntry {
            handle: handle.clone(),
            tick,
        },
    );
    state.trim_tier(tier);
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

/// Hold while mutating the process-wide icon cache in tests (avoids clear/decode races).
#[cfg(test)]
pub fn with_cache_lock<R>(f: impl FnOnce() -> R) -> R {
    use std::sync::Mutex as StdMutex;
    static LOCK: StdMutex<()> = StdMutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn with_isolated_cache(f: impl FnOnce()) {
        with_cache_lock(|| {
            // Soft clear — do not bump clear_epoch (that is Exit-only and races
            // parallel view tests that decode into the shared process cache).
            {
                let mut state = CACHE.lock().unwrap_or_else(|e| e.into_inner());
                state.map.clear();
                state.pins = None;
                state.tick = 0;
            }
            f();
            {
                let mut state = CACHE.lock().unwrap_or_else(|e| e.into_inner());
                state.map.clear();
                state.pins = None;
                state.tick = 0;
            }
        });
    }

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

    fn unique_dir(label: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sdsc-icon-{label}-{n}"));
        let _ = std::fs::create_dir_all(&dir);
        dir
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
        with_isolated_cache(|| {
            let dir = unique_dir("reuse");
            let path = dir.join("tiny.png");
            write_tiny_png(&path);
            let a = handle_for_path(&path).expect("decode");
            let b = handle_for_path(&path).expect("cached");
            assert_eq!(a.id(), b.id());
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn peek_list_icon_none_until_prepare() {
        with_isolated_cache(|| {
            let dir = unique_dir("list-peek");
            let path = dir.join("list.png");
            write_tiny_png(&path);
            assert!(icon_cached(&path).is_none());
            assert!(handle_for_path(&path).is_some());
            assert!(icon_cached(&path).is_some());
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn peek_hero_none_until_prepare() {
        with_isolated_cache(|| {
            let dir = unique_dir("hero-peek");
            let path = dir.join("hero.png");
            write_tiny_png(&path);
            assert!(hero_cached(&path).is_none());
            assert!(hero_for_path(&path).is_some());
            assert!(hero_cached(&path).is_some());
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn peek_backdrop_none_until_prepare() {
        with_isolated_cache(|| {
            let dir = unique_dir("bd-peek");
            let path = dir.join("bd.png");
            write_tiny_png(&path);
            assert!(backdrop_cached(&path).is_none());
            prepare_immersive([&path], [&path]);
            assert!(hero_cached(&path).is_some());
            assert!(backdrop_cached(&path).is_some());
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn filter_uncached_skips_warm_hits() {
        with_isolated_cache(|| {
            let dir = unique_dir("filter");
            let warm = dir.join("warm.png");
            let cold = dir.join("cold.png");
            write_tiny_png(&warm);
            write_tiny_png(&cold);
            assert!(backdrop_for_path(&warm).is_some());
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
        });
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

    #[test]
    fn art_window_circular_wraps_at_zero() {
        let idxs = art_window_indices(0, 30, true, ART_WINDOW);
        assert!(idxs.contains(&0));
        assert!(idxs.contains(&1));
        assert!(idxs.contains(&(ART_WINDOW as usize)));
        assert!(idxs.contains(&((30 - ART_WINDOW as usize) % 30)));
        assert!(idxs.contains(&29));
        assert!(!idxs.contains(&15));
    }

    #[test]
    fn art_window_linear_does_not_wrap() {
        let idxs = art_window_indices(0, 30, false, ART_WINDOW);
        assert_eq!(idxs, (0..=ART_WINDOW as usize).collect::<Vec<_>>());
        assert!(!idxs.contains(&29));
    }

    #[test]
    fn pin_keeps_unpinned_until_lru_cap() {
        with_isolated_cache(|| {
            let dir = unique_dir("pin-lru");
            let inside = dir.join("inside.png");
            let outside = dir.join("outside.png");
            write_tiny_png(&inside);
            write_tiny_png(&outside);
            assert!(backdrop_for_path(&inside).is_some());
            assert!(backdrop_for_path(&outside).is_some());
            // Pin only inside — outside stays cached (soft pin, under cap).
            pin_art_window(ArtRetainSet {
                backdrops: vec![inside.clone()],
                ..ArtRetainSet::default()
            });
            assert!(backdrop_cached(&inside).is_some());
            assert!(backdrop_cached(&outside).is_some());
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn clear_all_rejects_inflight_insert() {
        with_isolated_cache(|| {
            let dir = unique_dir("clear-race");
            let path = dir.join("x.png");
            write_tiny_png(&path);
            let epoch = {
                let state = CACHE.lock().unwrap_or_else(|e| e.into_inner());
                state.clear_epoch
            };
            clear_all();
            {
                let state = CACHE.lock().unwrap_or_else(|e| e.into_inner());
                assert_ne!(state.clear_epoch, epoch);
            }
            // Simulate lookup insert gate after wipe during decode.
            let handle = Handle::from_rgba(1, 1, vec![0, 0, 0, 0]);
            {
                let mut state = CACHE.lock().unwrap_or_else(|e| e.into_inner());
                if state.clear_epoch == epoch {
                    state.map.insert(
                        CacheKey::BackdropFile(path.clone()),
                        CacheEntry {
                            handle: Some(handle),
                            tick: 1,
                        },
                    );
                }
            }
            assert!(backdrop_cached(&path).is_none());
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn lru_evicts_unpinned_over_cap() {
        with_isolated_cache(|| {
            let dir = unique_dir("lru-cap");
            // Force a tiny cap by filling beyond LRU_BACKDROP_CAP with unpinned entries
            // after pinning a single key.
            let pinned = dir.join("pinned.png");
            write_tiny_png(&pinned);
            assert!(backdrop_for_path(&pinned).is_some());
            pin_art_window(ArtRetainSet {
                backdrops: vec![pinned.clone()],
                ..ArtRetainSet::default()
            });
            let mut extras = Vec::new();
            for i in 0..=LRU_BACKDROP_CAP {
                let p = dir.join(format!("x{i}.png"));
                write_tiny_png(&p);
                assert!(backdrop_for_path(&p).is_some());
                extras.push(p);
            }
            // Pinned must survive; at least one unpinned extra must have been evicted.
            assert!(backdrop_cached(&pinned).is_some());
            let still = extras
                .iter()
                .filter(|p| backdrop_cached(p).is_some())
                .count();
            assert!(still <= LRU_BACKDROP_CAP);
            // Total backdrops = pinned + still <= cap + 1 (pinned can push +1).
            assert!(still + 1 <= LRU_BACKDROP_CAP + 1);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
