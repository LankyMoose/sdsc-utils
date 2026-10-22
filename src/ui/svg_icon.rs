//! SVG rasterization for tray / UI icons (`resvg`).

use crate::{AppError, AppResult};
use resvg::tiny_skia;
use resvg::usvg;
use std::collections::HashMap;
use std::sync::Mutex;

pub const DUALSENSE_SVG: &str = include_str!("../../assets/icons/dualsense.svg");
pub const SETTINGS_SVG: &str = include_str!("../../assets/icons/settings.svg");
pub const IDENTIFY_SVG: &str = include_str!("../../assets/icons/identify.svg");
pub const POWER_SVG: &str = include_str!("../../assets/icons/power.svg");
pub const EDIT_SVG: &str = include_str!("../../assets/icons/edit.svg");
pub const CLOSE_SVG: &str = include_str!("../../assets/icons/close.svg");
pub const CHECK_SVG: &str = include_str!("../../assets/icons/check.svg");
pub const PIN_SVG: &str = include_str!("../../assets/icons/pin.svg");
pub const FACE_CROSS_SVG: &str = include_str!("../../assets/icons/face-cross.svg");
pub const FACE_CIRCLE_SVG: &str = include_str!("../../assets/icons/face-circle.svg");
pub const FACE_TRIANGLE_SVG: &str = include_str!("../../assets/icons/face-triangle.svg");
pub const FACE_SQUARE_SVG: &str = include_str!("../../assets/icons/face-square.svg");
pub const GAME_SVG: &str = include_str!("../../assets/icons/game.svg");
pub const BUG_SVG: &str = include_str!("../../assets/icons/bug.svg");
pub const GITHUB_SVG: &str = include_str!("../../assets/icons/github.svg");
pub const MICROSOFT_STORE_SVG: &str = include_str!("../../assets/icons/microsoft-store.svg");
/// Indeterminate arc; rotated by [`crate::ui::configure`] for pending states.
pub const SPINNER_SVG: &str = include_str!("../../assets/icons/spinner.svg");
/// Disclosure chevron; rotated 90° when a row is expanded.
pub const CHEVRON_SVG: &str = include_str!("../../assets/icons/chevron.svg");

/// Canonical DualSense body fill in [`DUALSENSE_SVG`].
pub const BODY_HEX: &str = "#EBEBF0";
/// Canonical DualSense shade fill in [`DUALSENSE_SVG`].
pub const SHADE_HEX: &str = "#282830";
/// Canonical DualSense accent / lightbar fill in [`DUALSENSE_SVG`].
pub const ACCENT_HEX: &str = "#005AFF";

pub const BODY: [u8; 4] = [235, 235, 240, 255];
pub const BODY_DIM: [u8; 4] = [120, 120, 128, 255];
pub const SHADE: [u8; 4] = [40, 40, 48, 255];
pub const SHADE_DIM: [u8; 4] = [70, 70, 78, 255];
/// Canonical accent fill for the connected DualSense tray/EXE icon.
///
/// Read by [`render_dualsense_connected_rgba`], which rasterizes the icons
/// embedded by `build.rs` (tray + Windows `.exe`) and `gen_msix_logos`.
pub const ACCENT: [u8; 4] = [0, 90, 255, 255];
pub const ACCENT_DIM: [u8; 4] = [80, 80, 90, 255];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RgbaColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl RgbaColor {
    /// Opaque color helper (unit tests; production palettes use [`Self::from_rgba`]).
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn from_rgba(rgba: [u8; 4]) -> Self {
        Self {
            r: rgba[0],
            g: rgba[1],
            b: rgba[2],
            a: rgba[3],
        }
    }

    pub fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ColorMap {
    replacements: Vec<(String, String)>,
}

impl ColorMap {
    /// Single-accent map (unit tests; production icons use the DualSense maps).
    pub fn current_color(color: RgbaColor) -> Self {
        Self {
            replacements: vec![("currentColor".into(), color.to_hex())],
        }
    }

    pub fn dualsense(body: RgbaColor, shade: RgbaColor, accent: RgbaColor) -> Self {
        Self {
            replacements: vec![
                (BODY_HEX.into(), body.to_hex()),
                (SHADE_HEX.into(), shade.to_hex()),
                (ACCENT_HEX.into(), accent.to_hex()),
            ],
        }
    }

    pub fn dualsense_connected(accent: RgbaColor) -> Self {
        Self::dualsense(
            RgbaColor::from_rgba(BODY),
            RgbaColor::from_rgba(SHADE),
            accent,
        )
    }

    pub fn dualsense_dim() -> Self {
        Self::dualsense(
            RgbaColor::from_rgba(BODY_DIM),
            RgbaColor::from_rgba(SHADE_DIM),
            RgbaColor::from_rgba(ACCENT_DIM),
        )
    }

    fn apply(&self, svg: &str) -> String {
        let mut out = svg.to_string();
        for (from, to) in &self.replacements {
            if from != to {
                out = out.replace(from, to);
            }
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    svg: String,
    size: u32,
    colors: ColorMap,
}

/// Owned SVG raster cache.
///
/// Owned by the Start image layer
/// ([`crate::ui::start::icon_cache::IconCache`]: constructed in
/// `IconCache::new`, wiped on process Exit) — no process-global instance.
/// Runtime tray / iced views use `svg::Handle::from_memory` directly; the
/// build-time embed (`build.rs`, `gen_msix_logos`) rasterizes through a
/// short-lived local instance via the `render_dualsense_*` helpers below.
pub struct SvgRasterCache {
    inner: Mutex<HashMap<CacheKey, Vec<u8>>>,
}

impl SvgRasterCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Rasterize an SVG into straight RGBA (`size × size × 4`).
    pub fn rasterize(&self, svg: &str, size: u32, colors: &ColorMap) -> AppResult<Vec<u8>> {
        if size == 0 {
            return Err(AppError::Ui("svg rasterize size must be > 0".into()));
        }

        let key = CacheKey {
            svg: svg.to_string(),
            size,
            colors: colors.clone(),
        };
        if let Ok(cache) = self.inner.lock()
            && let Some(hit) = cache.get(&key)
        {
            return Ok(hit.clone());
        }

        let rgba = rasterize_uncached(svg, size, colors)?;
        if let Ok(mut cache) = self.inner.lock() {
            cache.insert(key, rgba.clone());
        }
        Ok(rgba)
    }

    /// Drop every cached SVG raster (process Exit).
    pub fn clear_all(&self) {
        if let Ok(mut cache) = self.inner.lock() {
            cache.clear();
        }
    }
}

impl Default for SvgRasterCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Connected DualSense raster for the tray / Windows `.exe` icon.
///
/// Called by `build.rs` (tray bytes + `app.ico`) and `src/bin/gen_msix_logos.rs`
/// (Store logos); both include this file via `#[path]` so the helper must
/// stay even though the app itself rasterizes through [`SvgRasterCache`].
pub fn render_dualsense_connected_rgba(size: u32) -> AppResult<Vec<u8>> {
    SvgRasterCache::new().rasterize(
        DUALSENSE_SVG,
        size,
        &ColorMap::dualsense_connected(RgbaColor::from_rgba(ACCENT)),
    )
}

/// Dim DualSense raster for the disconnected tray icon (see above).
pub fn render_dualsense_dim_rgba(size: u32) -> AppResult<Vec<u8>> {
    SvgRasterCache::new().rasterize(DUALSENSE_SVG, size, &ColorMap::dualsense_dim())
}

fn rasterize_uncached(svg: &str, size: u32, colors: &ColorMap) -> AppResult<Vec<u8>> {
    let tinted = colors.apply(svg);
    let tree = usvg::Tree::from_str(&tinted, &usvg::Options::default())
        .map_err(|e| AppError::Ui(format!("parse SVG: {e}")))?;

    let mut pixmap = tiny_skia::Pixmap::new(size, size)
        .ok_or_else(|| AppError::Ui(format!("allocate {size}x{size} pixmap")))?;

    let svg_size = tree.size();
    let svg_w = svg_size.width().max(1.0);
    let svg_h = svg_size.height().max(1.0);
    let scale = (size as f32 / svg_w).min(size as f32 / svg_h);
    let dx = (size as f32 - svg_w * scale) / 2.0;
    let dy = (size as f32 - svg_h * scale) / 2.0;
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    Ok(straight_rgba_from_premultiplied(pixmap.data()))
}

fn straight_rgba_from_premultiplied(premul: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(premul.len());
    for &[pr, pg, pb, a] in premul.as_chunks::<4>().0 {
        if a == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else if a == 255 {
            out.extend_from_slice(&[pr, pg, pb, a]);
        } else {
            let a32 = a as u32;
            let r = ((pr as u32) * 255 / a32).min(255) as u8;
            let g = ((pg as u32) * 255 / a32).min(255) as u8;
            let b = ((pb as u32) * 255 / a32).min(255) as u8;
            out.extend_from_slice(&[r, g, b, a]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_svg_rasterizes_with_alpha() {
        let rgba = SvgRasterCache::new()
            .rasterize(
                POWER_SVG,
                24,
                &ColorMap::current_color(RgbaColor::rgb(170, 178, 189)),
            )
            .unwrap();
        assert_eq!(rgba.len(), 24 * 24 * 4);
        assert!(rgba.as_chunks::<4>().0.iter().any(|px| px[3] > 0));
        assert!(rgba.as_chunks::<4>().0.iter().any(|px| px[3] == 0));
    }

    #[test]
    fn dualsense_palette_swap_changes_accent() {
        let cache = SvgRasterCache::new();
        let blue = cache
            .rasterize(
                DUALSENSE_SVG,
                32,
                &ColorMap::dualsense_connected(RgbaColor::rgb(0, 90, 255)),
            )
            .unwrap();
        let red = cache
            .rasterize(
                DUALSENSE_SVG,
                32,
                &ColorMap::dualsense_connected(RgbaColor::rgb(255, 0, 0)),
            )
            .unwrap();
        assert_eq!(blue.len(), 32 * 32 * 4);
        assert!(blue.as_chunks::<4>().0.iter().any(|px| px[3] > 0));
        assert_ne!(blue, red);
    }
}
