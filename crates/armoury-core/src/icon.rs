//! App icons: the procedurally generated tray/window icon (dark disc with an amber "A"),
//! and `exe_icon_rgba`, which lifts a real executable's shell icon for the console tiles.

use std::path::Path;

pub fn rgba_icon(size: u32) -> (Vec<u8>, u32, u32) {
    let u = size as f32 / 32.0;
    let (cx, cy, r) = (16.0 * u, 16.0 * u, 15.0 * u);
    let bg = (24.0_f32, 30.0_f32, 44.0_f32);
    let accent = (245.0_f32, 158.0_f32, 11.0_f32);
    let soft = 1.4 * u;
    let stroke_half = 1.6 * u;
    let stroke_soft = 1.2 * u;

    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;

            let d_center = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
            let a_bg = ((r - d_center) / soft).clamp(0.0, 1.0);

            // Letter "A": two legs + crossbar, soft-edged strokes.
            let d_letter = [
                seg(px, py, 10.5 * u, 23.0 * u, 16.0 * u, 9.0 * u),
                seg(px, py, 16.0 * u, 9.0 * u, 21.5 * u, 23.0 * u),
                seg(px, py, 12.4 * u, 18.4 * u, 19.6 * u, 18.4 * u),
            ]
            .into_iter()
            .fold(f32::INFINITY, f32::min);
            let a_letter = ((stroke_half - d_letter) / stroke_soft).clamp(0.0, 1.0);

            let mut col = bg;
            let mut a = a_bg;
            if a_letter > 0.0 {
                col = (
                    col.0 + (accent.0 - col.0) * a_letter,
                    col.1 + (accent.1 - col.1) * a_letter,
                    col.2 + (accent.2 - col.2) * a_letter,
                );
                a += a_letter * (1.0 - a);
            }

            rgba.extend_from_slice(&[col.0 as u8, col.1 as u8, col.2 as u8, (a * 255.0) as u8]);
        }
    }
    (rgba, size, size)
}

/// The killswitch variant: same disc, red diagonal bar across the "A".
pub fn rgba_icon_active(size: u32) -> (Vec<u8>, u32, u32) {
    let u = size as f32 / 32.0;
    let (cx, cy, r) = (16.0 * u, 16.0 * u, 15.0 * u);
    let bg = (24.0_f32, 30.0_f32, 44.0_f32);
    let accent = (245.0_f32, 158.0_f32, 11.0_f32);
    let danger = (239.0_f32, 68.0_f32, 68.0_f32);
    let soft = 1.4 * u;
    let stroke_half = 1.6 * u;
    let stroke_soft = 1.2 * u;

    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let d_center = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
            let a_bg = ((r - d_center) / soft).clamp(0.0, 1.0);

            let d_letter = [
                seg(px, py, 10.5 * u, 23.0 * u, 16.0 * u, 9.0 * u),
                seg(px, py, 16.0 * u, 9.0 * u, 21.5 * u, 23.0 * u),
                seg(px, py, 12.4 * u, 18.4 * u, 19.6 * u, 18.4 * u),
            ]
            .into_iter()
            .fold(f32::INFINITY, f32::min);
            let a_letter = ((stroke_half - d_letter) / stroke_soft).clamp(0.0, 1.0);

            // red slash, bottom-left to top-right
            let d_slash = seg(px, py, 6.0 * u, 26.0 * u, 26.0 * u, 6.0 * u);
            let a_slash = ((2.2 * u - d_slash) / stroke_soft).clamp(0.0, 1.0);

            let mut col = bg;
            let mut a = a_bg;
            for (colour, alpha) in [(accent, a_letter), (danger, a_slash)] {
                if alpha > 0.0 {
                    col = (
                        col.0 + (colour.0 - col.0) * alpha,
                        col.1 + (colour.1 - col.1) * alpha,
                        col.2 + (colour.2 - col.2) * alpha,
                    );
                    a += alpha * (1.0 - a);
                }
            }
            rgba.extend_from_slice(&[col.0 as u8, col.1 as u8, col.2 as u8, (a * 255.0) as u8]);
        }
    }
    (rgba, size, size)
}

fn seg(px: f32, py: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {    let dx = x2 - x1;
    let dy = y2 - y1;
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (((px - x1) * dx + (py - y1) * dy) / len2).clamp(0.0, 1.0)
    };
    let qx = x1 + t * dx;
    let qy = y1 + t * dy;
    ((px - qx).powi(2) + (py - qy).powi(2)).sqrt()
}

/// The executable's shell icon as straight-alpha RGBA, ready for
/// `egui::ColorImage::from_rgba_unmultiplied`. Prefers the 256 px "jumbo" icon from the
/// system image list and falls back to the classic 32 px large icon; `None` when the file
/// carries no icon at all.
pub fn exe_icon_rgba(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    // The shell APIs want COM up on this thread; balanced right below. Extraction runs on a
    // short-lived worker thread, so this is normally an init/uninit pair.
    let com = unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        )
    };
    let out = extract_hicon(path).and_then(rasterize);
    if com.is_ok() {
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
    out
}

/// The best HICON the shell offers for `path`: the jumbo (256 px) entry of the system image
/// list when available, else the plain large icon.
fn extract_hicon(path: &Path) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
    use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};
    use windows::Win32::UI::Shell::{
        SHFILEINFOW, SHGetFileInfoW, SHGetImageList, SHGFI_FLAGS, SHGFI_ICON, SHGFI_LARGEICON,
        SHGFI_SYSICONINDEX, SHIL_JUMBO,
    };

    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let p = PCWSTR(wide.as_ptr());

    unsafe {
        // 1. system icon index -> jumbo list -> 256 px HICON (a copy; we must destroy it).
        let mut info = SHFILEINFOW::default();
        let flags = SHGFI_FLAGS(SHGFI_SYSICONINDEX.0 | SHGFI_LARGEICON.0);
        let ok = SHGetFileInfoW(
            p,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        );
        if ok != 0 {
            if let Ok(list) = SHGetImageList::<IImageList>(SHIL_JUMBO as i32) {
                if let Ok(hicon) = list.GetIcon(info.iIcon, ILD_TRANSPARENT.0) {
                    return Some(hicon);
                }
            }
        }

        // 2. fallback: the classic large icon straight from the shell.
        let mut info = SHFILEINFOW::default();
        let ok = SHGetFileInfoW(
            p,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_FLAGS(SHGFI_ICON.0 | SHGFI_LARGEICON.0),
        );
        if ok == 0 || info.hIcon.0.is_null() {
            return None;
        }
        Some(info.hIcon)
    }
}

/// HICON -> straight-alpha RGBA: draw the icon through a 32-bit image list onto a top-down
/// DIB section (the MS-sanctioned path that keeps the alpha channel), then swap BGRA -> RGBA.
/// Always destroys `hicon`.
fn rasterize(hicon: windows::Win32::UI::WindowsAndMessaging::HICON) -> Option<(Vec<u8>, u32, u32)> {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetObjectW, HGDIOBJ, SelectObject,
    };
    use windows::Win32::UI::Controls::{
        ILC_COLOR32, ILD_TRANSPARENT, ImageList_Create, ImageList_Destroy, ImageList_Draw,
        ImageList_ReplaceIcon,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

    unsafe {
        let out = (|| {
            // The icon's real size, from its own colour bitmap.
            let mut ii: ICONINFO = std::mem::zeroed();
            GetIconInfo(hicon, &mut ii).ok()?;
            let mut bm: BITMAP = std::mem::zeroed();
            let got = GetObjectW(
                HGDIOBJ(ii.hbmColor.0),
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut bm as *mut _ as *mut core::ffi::c_void),
            );
            let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
            let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
            if got == 0 || bm.bmWidth <= 0 {
                return None;
            }
            let (w, h) = (bm.bmWidth, bm.bmHeight.abs());

            let himl = ImageList_Create(w, h, ILC_COLOR32, 1, 1);
            if himl.0 == 0 {
                return None;
            }
            if ImageList_ReplaceIcon(himl, -1, hicon) < 0 {
                let _ = ImageList_Destroy(Some(himl));
                return None;
            }

            let hdc = CreateCompatibleDC(None);
            if hdc.0.is_null() {
                let _ = ImageList_Destroy(Some(himl));
                return None;
            }
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // negative = top-down rows
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let hbmp = match CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(b) => b,
                Err(_) => {
                    let _ = DeleteDC(hdc);
                    let _ = ImageList_Destroy(Some(himl));
                    return None;
                }
            };

            let old = SelectObject(hdc, HGDIOBJ(hbmp.0));
            let drew = ImageList_Draw(himl, 0, hdc, 0, 0, ILD_TRANSPARENT).as_bool();
            SelectObject(hdc, old);
            let mut out = None;
            if drew && !bits.is_null() {
                let src = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
                out = Some((bgra_to_rgba(src), w as u32, h as u32));
            }
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            let _ = DeleteDC(hdc);
            let _ = ImageList_Destroy(Some(himl));
            out
        })();
        let _ = DestroyIcon(hicon);
        out
    }
}

/// BGRA rows (as drawn into a 32-bit DIB) -> RGBA for egui.
fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = vec![0u8; bgra.len()];
    for (dst, src) in rgba.chunks_exact_mut(4).zip(bgra.chunks_exact(4)) {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = src[3];
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_shape_is_sane() {
        let (rgba, w, h) = rgba_icon(32);
        assert_eq!((w, h), (32, 32));
        assert_eq!(rgba.len(), 32 * 32 * 4);
        // center pixel (letter/background) is opaque
        let idx = ((16 * 32) + 16) * 4;
        assert!(rgba[idx + 3] > 200, "center should be opaque");
        // far corner is transparent
        assert_eq!(rgba[3], 0, "corner should be transparent");
    }

    #[test]
    fn bgra_swaps_channels_and_keeps_alpha() {
        let out = bgra_to_rgba(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(out, vec![3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn extracts_a_real_windows_icon() {
        // This repo is Windows-only; notepad.exe is the dependable probe.
        let probe = Path::new("C:\\Windows\\System32\\notepad.exe");
        if !probe.is_file() {
            return;
        }
        let (rgba, w, h) = exe_icon_rgba(probe).expect("notepad.exe carries a shell icon");
        assert!(w >= 32 && h >= 32, "unexpected size {w}x{h}");
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        assert!(rgba.chunks_exact(4).any(|p| p[3] > 0), "no opaque pixels");
    }
}
