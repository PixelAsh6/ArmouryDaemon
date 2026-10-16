//! Procedurally generated app icon (RGBA): dark disc with an amber "A".
//! Shared by the tray icon (daemon) and the window icon (UI).

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

fn seg(px: f32, py: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
    let dx = x2 - x1;
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
}
