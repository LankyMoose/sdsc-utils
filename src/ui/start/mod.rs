#[cfg(debug_assertions)]
pub mod ambient_tune;
pub mod art_worker;
pub mod carousel;
pub mod clock;
pub mod cursor_hide;
pub mod gesture;
pub mod hint_ring;
pub mod icon_cache;
pub mod idle;
pub mod immersive;
pub mod input;
pub mod list;
pub mod mode;
pub mod position;
pub mod row;
pub mod settings;
pub mod translate;
pub mod view;
pub mod vstrip;

/// Wipe every Start-related image cache to a blank slate (process Exit).
///
/// Called from [`art_worker::ArtOwner::shutdown`] (before and after join). Also resets
/// immersive shader one-shot diag state so the next process logs cleanly.
///
/// [`icon_cache::clear_all`] wipes the iced handles plus the owned
/// shell-extract and SVG raster leaf caches (handles → shell → SVG, the
/// former wipe order, with the art-warm generation reset).
pub fn wipe_art_caches() {
    icon_cache::clear_all();
    crate::ui::shader::ambient::reset_pipeline_diag();
    crate::ui::shader::vignette::reset_pipeline_diag();
    crate::platform::wgpu_diag::reset();
}
