//! Persistent user settings stored in `$XDG_CONFIG_HOME/luxus/config.json`
//! (or `~/.config/luxus/config.json`).

use crate::i18n::Lang;
use crate::theme::ThemeMode;
use crate::windows::WindowsUxOptions;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub language: Option<String>,
    pub theme: String,
    pub buffer_mb: usize,
    pub verify_after_write: bool,
    pub ntfs_compression: bool,
    pub list_usb_hard_drives: bool,
    pub enable_uefi_ntfs: bool,
    pub old_bios_fixes: bool,
    pub use_uefi_mbr_id: bool,
    pub windows_ux: WindowsUxOptions,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            language: None,
            theme: "auto".into(),
            buffer_mb: 4,
            verify_after_write: false,
            ntfs_compression: false,
            list_usb_hard_drives: false,
            enable_uefi_ntfs: true,
            old_bios_fixes: false,
            use_uefi_mbr_id: false,
            windows_ux: WindowsUxOptions::default(),
        }
    }
}

impl AppConfig {
    pub fn parsed_lang(&self) -> Option<Lang> {
        self.language.as_deref().and_then(Lang::from_locale)
    }

    pub fn parsed_theme(&self) -> ThemeMode {
        match self.theme.to_lowercase().as_str() {
            "light" => ThemeMode::Light,
            "dark" => ThemeMode::Dark,
            _ => ThemeMode::Auto,
        }
    }

    pub fn set_theme(&mut self, mode: ThemeMode) {
        self.theme = match mode {
            ThemeMode::Auto => "auto",
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
        }
        .to_string();
    }
}

pub fn config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("luxus").join("config.json"));
        }
    }
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(".config")
            .join("luxus")
            .join("config.json")
    })
}

pub fn load_config() -> AppConfig {
    let Some(path) = config_path() else {
        return AppConfig::default();
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return AppConfig::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save_config(cfg: &AppConfig) {
    let Some(path) = config_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_roundtrip() {
        let mut cfg = AppConfig::default();
        cfg.language = Some("es".into());
        cfg.set_theme(ThemeMode::Dark);
        cfg.buffer_mb = 8;
        cfg.verify_after_write = true;
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.parsed_lang(), Some(Lang::Es));
        assert_eq!(parsed.parsed_theme(), ThemeMode::Dark);
        assert_eq!(parsed.buffer_mb, 8);
        assert!(parsed.verify_after_write);
    }
}
