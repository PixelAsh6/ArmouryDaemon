//! Tray icon built from the shared procedural icon generator.

pub fn make_icon() -> tray_icon::Icon {
    let (rgba, w, h) = armoury_core::icon::rgba_icon(32);
    tray_icon::Icon::from_rgba(rgba, w, h).expect("tray icon rgba is 32x32")
}
