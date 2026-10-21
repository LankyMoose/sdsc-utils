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
pub fn wipe_art_caches() {
    use crate::platform::host::{Host, Icons as _};

    icon_cache::clear_all();
    Host::clear();
    crate::ui::svg_icon::clear_cache();
    crate::ui::shader::ambient::reset_pipeline_diag();
    crate::ui::shader::vignette::reset_pipeline_diag();
    crate::platform::wgpu_diag::reset();
}
