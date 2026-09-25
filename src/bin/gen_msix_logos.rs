//! Generate Store / MSIX logo PNGs from the DualSense SVG.
//!
//! Usage: `cargo run --bin gen_msix_logos -- packaging/Assets`

#![allow(dead_code)]

#[path = "../ui/svg_icon.rs"]
mod svg_icon;

use std::env;
use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

fn main() {
    let out = PathBuf::from(
        env::args()
            .nth(1)
            .unwrap_or_else(|| "packaging/Assets".to_string()),
    );
    fs::create_dir_all(&out).expect("create Assets dir");

    write_square(&out, "StoreLogo.png", 50);
    write_square(&out, "Square44x44Logo.png", 44);
    write_square(&out, "Square150x150Logo.png", 150);
    write_square(&out, "Square310x310Logo.png", 310);
    write_wide(&out, "Wide310x150Logo.png", 310, 150);

    println!("wrote MSIX logos to {}", out.display());
}

fn write_square(dir: &Path, name: &str, size: u32) {
    let rgba = svg_icon::render_dualsense_connected_rgba(size).expect("rasterize");
    encode_png(dir.join(name), size, size, &rgba);
}

fn write_wide(dir: &Path, name: &str, width: u32, height: u32) {
    let icon = height;
    let rgba = svg_icon::render_dualsense_connected_rgba(icon).expect("rasterize");
    let mut canvas = vec![0u8; (width * height * 4) as usize];
    let x0 = (width - icon) / 2;
    for y in 0..icon {
        for x in 0..icon {
            let src = ((y * icon + x) * 4) as usize;
            let dst = ((y * width + (x0 + x)) * 4) as usize;
            canvas[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
        }
    }
    encode_png(dir.join(name), width, height, &canvas);
}

fn encode_png(path: PathBuf, width: u32, height: u32, rgba: &[u8]) {
    let file = fs::File::create(&path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(rgba).expect("png data");
}
