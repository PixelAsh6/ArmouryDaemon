//! The native "open file" dialog (`IFileOpenDialog`), shared by the dashboard's pickers and the
//! console launcher's "Add app…" tile. No extra dependencies.

use std::path::PathBuf;

/// Show the dialog and return the chosen file. It runs its own modal loop — call it from a
/// worker thread, never from the egui event thread.
pub fn open_dialog(title: &str, filter_label: &str, filter_ext: &str) -> Option<PathBuf> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
        CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{
        FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FileOpenDialog, IFileOpenDialog,
        SIGDN_FILESYSPATH,
    };
    use windows::core::PCWSTR;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let title_w = wide(title);
    let label_w = wide(filter_label);
    let ext_w = wide(filter_ext);

    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let result = (|| -> Option<PathBuf> {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            if let Ok(opts) = dialog.GetOptions() {
                let _ = dialog.SetOptions(
                    opts | FOS_FILEMUSTEXIST | FOS_PATHMUSTEXIST | FOS_FORCEFILESYSTEM,
                );
            }
            let _ = dialog.SetFileTypes(&[COMDLG_FILTERSPEC {
                pszName: PCWSTR(label_w.as_ptr()),
                pszSpec: PCWSTR(ext_w.as_ptr()),
            }]);
            let _ = dialog.SetTitle(PCWSTR(title_w.as_ptr()));
            if dialog.Show(None).is_err() {
                return None; // cancelled
            }
            let item = dialog.GetResult().ok()?;
            let display = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let path = display.to_string().ok().map(PathBuf::from);
            CoTaskMemFree(Some(display.0 as *const core::ffi::c_void));
            path
        })();
        if hr.is_ok() {
            CoUninitialize();
        }
        result
    }
}
