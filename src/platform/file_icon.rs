//! Extract shell icons for local game shortcuts (`.exe` / `.lnk`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// Cached RGBA pixels for a filesystem path (width × height × 4).
type IconRgba = (u32, u32, Vec<u8>);

static CACHE: LazyLock<Mutex<HashMap<PathBuf, Option<IconRgba>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Return a square RGBA icon for `path`, or `None` if extraction fails.
///
/// Results are cached (including failures) so start-screen refreshes stay cheap.
pub fn rgba_for_path(path: &Path, size: u32) -> Option<(u32, u32, Vec<u8>)> {
    let key = path.to_path_buf();
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(&key) {
            return entry.clone();
        }
    }
    let extracted = extract_icon_rgba(path, size);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.insert(key, extracted.clone());
    extracted
}

#[cfg(windows)]
fn extract_icon_rgba(path: &Path, size: u32) -> Option<(u32, u32, Vec<u8>)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HGDIOBJ, ReleaseDC, SelectObject,
    };
    use windows::Win32::UI::Shell::{ExtractIconExW, SHDefExtractIconW};
    use windows::Win32::UI::WindowsAndMessaging::{DI_NORMAL, DestroyIcon, DrawIconEx, HICON};
    use windows::core::PCWSTR;

    if !path.is_file() {
        return None;
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let size_i = size.max(16) as i32;
    // LOWORD = large, HIWORD = small (same size for both).
    let niconsize = (size << 16) | size;

    unsafe {
        let mut large = HICON::default();
        let hr = SHDefExtractIconW(
            PCWSTR(wide.as_ptr()),
            0,
            0,
            Some(&mut large as *mut HICON),
            None,
            niconsize,
        );
        if hr.is_err() || large.is_invalid() {
            let mut large2 = HICON::default();
            let mut small = HICON::default();
            let n = ExtractIconExW(
                PCWSTR(wide.as_ptr()),
                0,
                Some(&mut large2 as *mut HICON),
                Some(&mut small as *mut HICON),
                1,
            );
            if !small.is_invalid() {
                let _ = DestroyIcon(small);
            }
            if n == 0 || large2.is_invalid() {
                return None;
            }
            large = large2;
        }

        let screen = GetDC(Some(HWND::default()));
        if screen.is_invalid() {
            let _ = DestroyIcon(large);
            return None;
        }
        let mem = CreateCompatibleDC(Some(screen));
        if mem.is_invalid() {
            let _ = ReleaseDC(Some(HWND::default()), screen);
            let _ = DestroyIcon(large);
            return None;
        }
        let bitmap = CreateCompatibleBitmap(screen, size_i, size_i);
        if bitmap.is_invalid() {
            let _ = DeleteDC(mem);
            let _ = ReleaseDC(Some(HWND::default()), screen);
            let _ = DestroyIcon(large);
            return None;
        }
        let old = SelectObject(mem, HGDIOBJ(bitmap.0));
        let drawn = DrawIconEx(mem, 0, 0, large, size_i, size_i, 0, None, DI_NORMAL);
        let _ = DestroyIcon(large);

        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size_i,
                biHeight: -size_i, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; (size_i * size_i * 4) as usize];
        let rows = GetDIBits(
            mem,
            bitmap,
            0,
            size_i as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );

        let _ = SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(Some(HWND::default()), screen);

        if drawn.is_err() || rows == 0 {
            return None;
        }

        // BGRA (Windows) → RGBA (iced).
        for chunk in pixels.as_chunks_mut::<4>().0 {
            chunk.swap(0, 2);
        }

        Some((size_i as u32, size_i as u32, pixels))
    }
}

#[cfg(not(windows))]
fn extract_icon_rgba(_path: &Path, _size: u32) -> Option<(u32, u32, Vec<u8>)> {
    None
}
