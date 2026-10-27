//! The system UI font the windows draw with: Segoe UI Variable Text (Windows 11) →
//! Segoe UI → Arial. Shared by the dashboard (`ui.rs`) and the console band
//! (`console_ui.rs`); exactly one file is loaded (the first that exists) and it also
//! backs the monospace family, so nothing extra is kept in memory.

use eframe::egui;

/// Use the machine's own fonts instead of egui's embedded ones.
pub(crate) fn install_fonts(ctx: &egui::Context) {
    let (fonts, chosen) = system_fonts();
    match chosen {
        Some(label) => log::info!("using the system font: {label}"),
        None => log::warn!("no system UI font found; text may not render"),
    }
    ctx.set_fonts(fonts);
}

/// The fonts the app draws with, and the name of the one that was found (None when the
/// machine has none of them and egui's own font is used).
pub(crate) fn system_fonts() -> (egui::FontDefinitions, Option<&'static str>) {
    const CANDIDATES: [(&str, &str); 3] = [
        ("C:\\Windows\\Fonts\\SegUIVar.ttf", "Segoe UI Variable Text"),
        ("C:\\Windows\\Fonts\\segoeui.ttf", "Segoe UI"),
        ("C:\\Windows\\Fonts\\arial.ttf", "Arial"),
    ];

    let mut fonts = egui::FontDefinitions::default();
    let mut chosen = None;
    for (path, label) in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "ui".to_owned(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            chosen = Some(label);
            break;
        }
    }

    if chosen.is_some() {
        let family = vec!["ui".to_owned()];
        fonts
            .families
            .insert(egui::FontFamily::Proportional, family.clone());
        fonts.families.insert(egui::FontFamily::Monospace, family);
    }

    (fonts, chosen)
}
