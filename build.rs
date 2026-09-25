//! Build script: embed Windows .exe icon from DualSense SVG (same as tray).

#[allow(dead_code)]
#[path = "src/ui/svg_icon.rs"]
mod svg_icon;

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const DISPLAY_NAME: &str = "SDSC Utils";
const TRAY_SIZE: u32 = 32;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let connected = svg_icon::render_dualsense_connected_rgba(TRAY_SIZE)
        .expect("rasterize connected DualSense icon");
    let dim = svg_icon::render_dualsense_dim_rgba(TRAY_SIZE).expect("rasterize dim DualSense icon");

    write_embedded_bytes(&out_dir, &connected, &dim);

    #[cfg(windows)]
    {
        write_app_ico(&out_dir);
        let ico_path = out_dir.join("app.ico");
        let mut res = winres::WindowsResource::new();
        res.set_icon(ico_path.to_str().expect("utf-8 icon path"));
        res.set("ProductName", DISPLAY_NAME);
        res.set(
            "FileDescription",
            "Show DualSense controller battery levels in the system tray",
        );
        if let Err(err) = res.compile() {
            // Missing Windows SDK / RC tooling should not block non-icon builds in CI-like envs.
            println!("cargo:warning=winres failed to embed icon: {err}");
        }
    }

    println!("cargo:rerun-if-changed=src/ui/svg_icon.rs");
    println!("cargo:rerun-if-changed=assets/icons/dualsense.svg");
    println!("cargo:rerun-if-changed=assets/icons/settings.svg");
    println!("cargo:rerun-if-changed=assets/icons/identify.svg");
    println!("cargo:rerun-if-changed=assets/icons/power.svg");
    println!("cargo:rerun-if-changed=assets/icons/close.svg");
    println!("cargo:rerun-if-changed=assets/icons/minimize.svg");
    println!("cargo:rerun-if-changed=assets/icons/check.svg");
    println!("cargo:rerun-if-changed=build.rs");
}

fn write_embedded_bytes(out_dir: &Path, connected: &[u8], dim: &[u8]) {
    let path = out_dir.join("icon_embedded.rs");
    let mut file = fs::File::create(&path).expect("create icon_embedded.rs");
    writeln!(file, "pub const CONNECTED_RGBA: &[u8] = &{:?};", connected).unwrap();
    writeln!(file, "pub const DIM_RGBA: &[u8] = &{:?};", dim).unwrap();
}

#[cfg(windows)]
fn write_app_ico(out_dir: &Path) {
    use ico::{IconDir, IconDirEntry, IconImage, ResourceType};

    let mut icon_dir = IconDir::new(ResourceType::Icon);
    for size in [16u32, 32, 48, 256] {
        let rgba = svg_icon::render_dualsense_connected_rgba(size)
            .unwrap_or_else(|err| panic!("rasterize DualSense {size}x{size}: {err}"));
        let image = IconImage::from_rgba_data(size, size, rgba);
        icon_dir.add_entry(IconDirEntry::encode(&image).expect("encode ico entry"));
    }

    let ico_path = out_dir.join("app.ico");
    let file = fs::File::create(&ico_path).expect("create app.ico");
    icon_dir.write(file).expect("write app.ico");
}
