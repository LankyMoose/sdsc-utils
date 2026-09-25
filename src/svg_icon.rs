//! SVG rasterization for tray / UI icons (`resvg`).

use resvg::tiny_skia;
use resvg::usvg;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

pub const DUALSENSE_SVG: &str = include_str!("../assets/icons/dualsense.svg");
pub const SETTINGS_SVG: &str = include_str!("../assets/icons/settings.svg");
pub const IDENTIFY_SVG: &str = include_str!("../assets/icons/identify.svg");
pub const POWER_SVG: &str = include_str!("../assets/icons/power.svg");
pub const EDIT_SVG: &str = include_str!("../assets/icons/edit.svg");
pub const CLOSE_SVG: &str = include_str!("../assets/icons/close.svg");
#[allow(dead_code)] // available for title-bar chrome
pub const MINIMIZE_SVG: &str = include_str!("../assets/icons/minimize.svg");
pub const CHECK_SVG: &str = include_str!("../assets/icons/check.svg");
pub const FACE_CROSS_SVG: &str = include_str!("../assets/icons/face-cross.svg");
pub const FACE_CIRCLE_SVG: &str = include_str!("../assets/icons/face-circle.svg");
pub const FACE_TRIANGLE_SVG: &str = include_str!("../assets/icons/face-triangle.svg");
pub const FACE_SQUARE_SVG: &str = include_str!("../assets/icons/face-square.svg");
pub const GAME_SVG: &str = include_str!("../assets/icons/game.svg");
pub const BUG_SVG: &str = include_str!("../assets/icons/bug.svg");

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
#[allow(dead_code)] // used by build.rs / connected tray embed
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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

static RASTER_CACHE: LazyLock<Mutex<HashMap<CacheKey, Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Rasterize an SVG into straight RGBA (`size × size × 4`).
pub fn rasterize(svg: &str, size: u32, colors: &ColorMap) -> Result<Vec<u8>, String> {
    if size == 0 {
        return Err("svg rasterize size must be > 0".into());
    }

    let key = CacheKey {
        svg: svg.to_string(),
        size,
        colors: colors.clone(),
    };
    if let Ok(cache) = RASTER_CACHE.lock()
        && let Some(hit) = cache.get(&key)
    {
        return Ok(hit.clone());
    }

    let rgba = rasterize_uncached(svg, size, colors)?;
    if let Ok(mut cache) = RASTER_CACHE.lock() {
        cache.insert(key, rgba.clone());
    }
    Ok(rgba)
}

fn rasterize_uncached(svg: &str, size: u32, colors: &ColorMap) -> Result<Vec<u8>, String> {
    let tinted = colors.apply(svg);
    let tree = usvg::Tree::from_str(&tinted, &usvg::Options::default())
        .map_err(|e| format!("parse SVG: {e}"))?;

    let mut pixmap = tiny_skia::Pixmap::new(size, size)
        .ok_or_else(|| format!("allocate {size}x{size} pixmap"))?;

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

#[allow(dead_code)] // used by build.rs
pub fn render_dualsense_connected_rgba(size: u32) -> Result<Vec<u8>, String> {
    rasterize(
        DUALSENSE_SVG,
        size,
        &ColorMap::dualsense_connected(RgbaColor::from_rgba(ACCENT)),
    )
}

#[allow(dead_code)] // used by build.rs
pub fn render_dualsense_dim_rgba(size: u32) -> Result<Vec<u8>, String> {
    rasterize(DUALSENSE_SVG, size, &ColorMap::dualsense_dim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_svg_rasterizes_with_alpha() {
        let rgba = rasterize(
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
        let blue = rasterize(
            DUALSENSE_SVG,
            32,
            &ColorMap::dualsense_connected(RgbaColor::rgb(0, 90, 255)),
        )
        .unwrap();
        let red = rasterize(
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
