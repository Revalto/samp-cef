use std::path::PathBuf;

/// Папка cef игры: там лежат cef_host.exe, libcef.dll и renderer.exe.
pub fn cef_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Локальные файлы интерфейса — схема sampcef://assets/ (assets_scheme.rs).
pub fn assets_dir() -> PathBuf {
    cef_dir().join("assets")
}
