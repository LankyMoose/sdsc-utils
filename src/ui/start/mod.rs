pub mod art_worker;
pub mod carousel;
pub mod cursor_hide;
pub mod gesture;
pub mod icon_cache;
pub mod idle;
pub mod immersive;
pub mod input;
pub mod mode;
pub mod reveal;
pub mod translate;
pub mod view;
pub mod vstrip;

/// Wipe every Start-related image cache to a blank slate (process Exit).
///
/// Called from [`art_worker::shutdown`] (before and after join). Also resets
/// immersive shader one-shot diag state so the next process logs cleanly.
pub fn wipe_art_caches() {
    icon_cache::clear_all();
    crate::platform::file_icon::clear_all();
    crate::ui::svg_icon::clear_cache();
    crate::ui::shader::ambient::reset_pipeline_diag();
    crate::platform::wgpu_diag::reset();
}
