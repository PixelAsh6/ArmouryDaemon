//! App icons: the procedurally generated tray/window icon (a purple-magenta squircle with a
//! bold "A"), and `exe_icon_rgba`, which lifts a real executable's shell icon for the console
//! tiles.
//!
//! The tray shows the icon at 16-20 px, so everything is drawn for that size: a filled
//! squircle instead of a thin disc, and a letter with strokes thick enough to survive the
//! downscale.

use std::path::Path;

/// Squircle with a vertical gradient and a bold "A" - the normal tray icon.
pub fn rgba_icon(size: u32) -> (Vec<u8>, u32, u32) {
    let u = size as f32 / 32.0;
    let top = (150.0_f32, 95.0_f32, 255.0_f32);
    let bottom = (233.0_f32, 84.0_f32, 190.0_f32);
    draw_mark(size, u, top, bottom)
}

/// The killswitch variant: the same mark in red.
pub fn rgba_icon_active(size: u32) -> (Vec<u8>, u32, u32) {
    let u = size as f32 / 32.0;
    let top = (176.0_f32, 34.0_f32, 48.0_f32);
    let bottom = (255.0_f32, 108.0_f32, 88.0_f32);
    draw_mark(size, u, top, bottom)
}

/// A rounded square filled with a vertical gradient, with a white "A" on top.
fn draw_mark(
    size: u32,
    u: f32,
    top: (f32, f32, f32),
    bottom: (f32, f32, f32),
) -> (Vec<u8>, u32, u32) {
    let cx = 16.0 * u;
    let cy = 16.0 * u;
    let half = 15.0 * u;
    let radius = 7.5 * u;
    let stroke_half = 2.15 * u;
    let stroke_soft = 0.9 * u;
    let edge_soft = 0.8 * u;
    let white = (250.0_f32, 250.0_f32, 255.0_f32);

    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;

            // Squircle: signed distance, negative inside.
            let qx = (px - cx).abs() - (half - radius);
            let qy = (py - cy).abs() - (half - radius);
            let sd = (qx.max(0.0) * qx.max(0.0) + qy.max(0.0) * qy.max(0.0)).sqrt()
                + qx.max(qy).min(0.0)
                - radius;
            let a_bg = ((0.5 - sd) / edge_soft).clamp(0.0, 1.0);

            // Letter "A": two legs + crossbar, soft-edged strokes.
            let d_letter = [
                seg(px, py, 10.6 * u, 23.2 * u, 16.0 * u, 8.8 * u),
                seg(px, py, 16.0 * u, 8.8 * u, 21.4 * u, 23.2 * u),
                seg(px, py, 12.6 * u, 18.6 * u, 19.4 * u, 18.6 * u),
            ]
            .into_iter()
            .fold(f32::INFINITY, f32::min);
            let a_letter = ((stroke_half - d_letter) / stroke_soft).clamp(0.0, 1.0);

            let t = (py / (32.0 * u)).clamp(0.0, 1.0);
            let mut col = (
                top.0 + (bottom.0 - top.0) * t,
                top.1 + (bottom.1 - top.1) * t,
                top.2 + (bottom.2 - top.2) * t,
            );
            let mut a = a_bg;
            if a_letter > 0.0 {
                col = (
                    col.0 + (white.0 - col.0) * a_letter,
                    col.1 + (white.1 - col.1) * a_letter,
                    col.2 + (white.2 - col.2) * a_letter,
                );
                a += a_letter * (1.0 - a);
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

/// An HICON and who owns it. The two ways the shell hands one out are not the same: the
/// image list gives out a *copy* (ours to destroy), while `SHGFI_ICON` gives out the
/// shell's own icon (shared with the whole system, never to be destroyed).
enum ShellIcon {
    /// A copy from the system image list: the caller owns it and must destroy it.
    Owned(windows::Win32::UI::WindowsAndMessaging::HICON),
    /// The shell's own icon: shared, the caller must leave it alone.
    Shared(windows::Win32::UI::WindowsAndMessaging::HICON),
}

impl ShellIcon {
    fn handle(&self) -> windows::Win32::UI::WindowsAndMessaging::HICON {
        match self {
            ShellIcon::Owned(h) | ShellIcon::Shared(h) => *h,
        }
    }

    /// May the caller destroy this handle?
    fn owned(&self) -> bool {
        matches!(self, ShellIcon::Owned(_))
    }
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
fn extract_hicon(path: &Path) -> Option<ShellIcon> {
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
                    return Some(ShellIcon::Owned(hicon));
                }
            }
        }

        // 2. fallback: the classic large icon straight from the shell. This handle belongs to
        // the shell — destroying it would pull the icon out of every window that shows it.
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
        Some(ShellIcon::Shared(info.hIcon))
    }
}

/// HICON -> straight-alpha RGBA: draw the icon through a 32-bit image list onto a top-down
/// DIB section (the MS-sanctioned path that keeps the alpha channel), then swap BGRA -> RGBA.
/// Destroys the handle only when it is ours to destroy.
fn rasterize(icon: ShellIcon) -> Option<(Vec<u8>, u32, u32)> {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetObjectW, HGDIOBJ, SelectObject,
    };
    use windows::Win32::UI::Controls::{
        ILC_COLOR32, ILD_TRANSPARENT, ImageList_Create, ImageList_Destroy, ImageList_Draw,
        ImageList_ReplaceIcon,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

    let hicon = icon.handle();
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
        // Only a handle we made is ours to destroy: the shell's own icon is shared with
        // every other window that shows it.
        if icon.owned() {
            let _ = DestroyIcon(hicon);
        }
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

    #[test]
    fn only_a_copy_of_an_icon_is_destroyed() {
        // The image list hands out a copy (ours to destroy); the shell's own icon is
        // shared with the whole system and must survive us.
        use windows::Win32::UI::WindowsAndMessaging::HICON;
        let handle = HICON::default();
        assert!(ShellIcon::Owned(handle).owned());
        assert!(!ShellIcon::Shared(handle).owned());
    }
}
