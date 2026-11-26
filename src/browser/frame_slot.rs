//! CPU-side latest engine frame, shared with the stable-texture renderer.
//!
//! The old path minted a fresh `iced::image::Handle` for every engine frame.
//! Each handle is a new atlas id by construction, so iced paid a full atlas
//! allocation + upload + eviction per frame; at wide viewports the raster
//! worker could not finish before the next handle replaced it and the page
//! blinked. Here the bytes live in a slot; the custom `iced_wgpu` primitive
//! uploads them into one stable texture, only when the generation changes.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// One RGBA frame as read from the engine's software surface.
#[derive(Debug)]
pub struct FrameData {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Document URL that produced this paint. Replay uses it to reject a
    /// stale frame from the previous page.
    pub url: String,
}

/// Latest frame for one tab. `write` moves the engine buffer in place and
/// bumps the generation; the renderer uploads unseen generations only.
#[derive(Debug)]
pub struct FrameSlot {
    frame: Mutex<Option<FrameData>>,
    generation: AtomicU64,
    /// Cleared when the slot drops, so the render pipeline can destroy the
    /// associated texture instead of leaking one per closed tab.
    alive: Arc<AtomicBool>,
}

impl Default for FrameSlot {
    fn default() -> Self {
        Self {
            frame: Mutex::new(None),
            generation: AtomicU64::new(0),
            alive: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl FrameSlot {
    /// Install the newest frame. Frames that don't match the expected RGBA
    /// byte count are ignored (never upload a short/long buffer).
    pub fn write(&self, width: u32, height: u32, rgba: Vec<u8>, url: &str) {
        if width == 0 || height == 0 || rgba.len() != width as usize * height as usize * 4 {
            return;
        }
        *self.frame.lock().unwrap_or_else(|e| e.into_inner()) = Some(FrameData {
            width,
            height,
            rgba,
            url: url.to_string(),
        });
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Drop the pixels (new document / install placeholder) and bump the
    /// generation so the renderer sees the change.
    pub fn clear(&self) {
        *self.frame.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn has_frame(&self) -> bool {
        self.frame
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Document URL of the latest paint, if any.
    pub fn url(&self) -> Option<String> {
        self.frame
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|frame| frame.url.clone())
    }

    /// Run `f` with the latest frame while holding the slot lock. The
    /// renderer's `write_texture` copies from this borrow; the engine's
    /// buffer is never copied to hand it over.
    pub fn with_frame<R>(&self, f: impl FnOnce(&FrameData) -> R) -> Option<R> {
        let frame = self.frame.lock().unwrap_or_else(|e| e.into_inner());
        frame.as_ref().map(f)
    }

    /// Shared liveness flag for the render pipeline's `trim`.
    pub fn alive(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.alive)
    }
}

impl Drop for FrameSlot {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_bumps_generation_and_moves_bytes() {
        let slot = FrameSlot::default();
        assert!(!slot.has_frame());
        assert_eq!(slot.generation(), 0);
        slot.write(2, 2, vec![0x11; 16], "https://a.test/");
        assert!(slot.has_frame());
        assert_eq!(slot.generation(), 1);
        let dims = slot
            .with_frame(|f| (f.width, f.height, f.rgba[0]))
            .expect("frame");
        assert_eq!(dims, (2, 2, 0x11));
        slot.clear();
        assert!(!slot.has_frame());
        assert_eq!(slot.generation(), 2);
    }

    #[test]
    fn malformed_frame_is_ignored() {
        let slot = FrameSlot::default();
        slot.write(2, 2, vec![0; 15], "https://a.test/");
        assert!(!slot.has_frame());
        assert_eq!(slot.generation(), 0);
        slot.write(0, 2, vec![0; 16], "https://a.test/");
        assert!(!slot.has_frame());
    }

    #[test]
    fn alive_clears_on_drop() {
        let slot = FrameSlot::default();
        let alive = slot.alive();
        assert!(alive.load(Ordering::SeqCst));
        drop(slot);
        assert!(!alive.load(Ordering::SeqCst));
    }
}
