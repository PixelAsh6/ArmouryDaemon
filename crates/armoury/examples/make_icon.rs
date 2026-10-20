//! Writes the app icon out as a multi-resolution `.ico`, ready to be embedded in the
//! executable as a PE resource.
//!
//! The pixels come from `armoury_core::icon`, the same drawing the tray and the windows
//! use, so the file icon and the tray icon cannot drift apart.
//!
//! Run it once after changing the artwork:
//! ```text
//! cargo run --example make_icon
//! ```
//! It writes `assets/ArmouryDaemon.ico` and a 64 px preview PNG next to it.

use std::fs;
use std::path::PathBuf;

use armoury_core::icon::rgba_icon;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};

/// The sizes Explorer, the taskbar and the large-icon view ask for.
const SIZES: [u32; 6] = [16, 24, 32, 48, 64, 256];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let assets = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");
    fs::create_dir_all(&assets)?;

    let frames: Vec<(u32, Vec<u8>)> = SIZES
        .iter()
        .map(|&size| {
            let (rgba, w, h) = rgba_icon(size);
            Ok((size, encode_png(&rgba, w, h)?))
        })
        .collect::<Result<_, Box<dyn std::error::Error>>>()?;

    let ico = build_ico(&frames);
    let ico_path = assets.join("ArmouryDaemon.ico");
    fs::write(&ico_path, &ico)?;
    println!("{} ({} bytes)", ico_path.display(), ico.len());

    // A flat PNG of one of the frames, so the artwork can be looked at without an
    // image viewer (and without asking Explorer for the icon).
    let (rgba, w, h) = rgba_icon(64);
    let preview_path = assets.join("ArmouryDaemon-preview64.png");
    fs::write(&preview_path, encode_png(&rgba, w, h)?)?;
    println!("{} ({} bytes)", preview_path.display(), fs::metadata(&preview_path)?.len());

    Ok(())
}

/// Straight-alpha RGBA -> PNG bytes.
fn encode_png(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>, image::ImageError> {
    let mut png = Vec::new();
    PngEncoder::new(&mut png).write_image(rgba, w, h, ExtendedColorType::Rgba8)?;
    Ok(png)
}

/// The ICO container: a 6-byte header, one 16-byte directory entry per frame, then the
/// PNG payloads. Windows reads PNG-compressed entries on every version this app supports.
fn build_ico(frames: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: 1 = icon
    out.extend_from_slice(&(frames.len() as u16).to_le_bytes());

    // The payload of the first frame starts right after the directory.
    let mut offset = 6 + 16 * frames.len();
    for (size, png) in frames {
        // 256 is written as 0 in the directory.
        let dim = if *size == 256 { 0u8 } else { *size as u8 };
        out.push(dim); // width
        out.push(dim); // height
        out.push(0); // colour count: 0 means 256 or more
        out.push(0); // reserved
        out.extend_from_slice(&0u16.to_le_bytes()); // colour planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += png.len();
    }
    for (_, png) in frames {
        out.extend_from_slice(png);
    }
    out
}
