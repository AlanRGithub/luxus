//! Cross-distro Linux automatic dark/light theme detection and Luxus color palettes.

use eframe::egui::{self, Color32, Rounding, Stroke};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeMode {
    Auto,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug)]
pub struct ThemePalette {
    pub dark: bool,
    pub bg_panel: Color32,
    pub bg_statusbar: Color32,
    pub bg_ctrl: Color32,
    pub bg_ctrl_hover: Color32,
    pub bg_ctrl_active: Color32,
    pub bg_btn_disabled: Color32,
    pub border_ctrl: Color32,
    pub border_btn: Color32,
    pub border_divider: Color32,
    pub border_subtle: Color32,
    pub text_heading: Color32,
    pub text_primary: Color32,
    pub text_muted: Color32,
    pub text_disabled: Color32,
    pub accent_blue: Color32,
    pub accent_blue_hover: Color32,
    pub focus_inner: Color32,
    pub icon_blue: Color32,
    pub status_green: Color32,
    pub status_red: Color32,
    pub status_track: Color32,
    pub status_border: Color32,
    pub checkbox_border: Color32,
    pub split_divider: Color32,
}

pub const LIGHT_PALETTE: ThemePalette = ThemePalette {
    dark: false,
    bg_panel: Color32::from_rgb(243, 243, 243),
    bg_statusbar: Color32::from_rgb(236, 236, 236),
    bg_ctrl: Color32::WHITE,
    bg_ctrl_hover: Color32::from_rgb(229, 241, 251),
    bg_ctrl_active: Color32::from_rgb(204, 228, 247),
    bg_btn_disabled: Color32::from_rgb(238, 238, 238),
    border_ctrl: Color32::from_rgb(198, 198, 198),
    border_btn: Color32::from_rgb(173, 173, 173),
    border_divider: Color32::from_rgb(28, 28, 28),
    border_subtle: Color32::from_rgb(214, 214, 214),
    text_heading: Color32::BLACK,
    text_primary: Color32::from_rgb(17, 17, 17),
    text_muted: Color32::from_rgb(100, 100, 100),
    text_disabled: Color32::from_rgb(145, 145, 145),
    accent_blue: Color32::from_rgb(0, 103, 192),
    accent_blue_hover: Color32::from_rgb(0, 120, 215),
    focus_inner: Color32::from_rgb(130, 175, 220),
    icon_blue: Color32::from_rgb(26, 115, 184),
    status_green: Color32::from_rgb(6, 176, 37),
    status_red: Color32::from_rgb(196, 43, 28),
    status_track: Color32::from_rgb(230, 230, 230),
    status_border: Color32::from_rgb(188, 188, 188),
    checkbox_border: Color32::from_rgb(122, 122, 122),
    split_divider: Color32::from_rgb(205, 205, 205),
};

pub const DARK_PALETTE: ThemePalette = ThemePalette {
    dark: true,
    bg_panel: Color32::from_rgb(32, 32, 32),
    bg_statusbar: Color32::from_rgb(25, 25, 25),
    bg_ctrl: Color32::from_rgb(45, 45, 45),
    bg_ctrl_hover: Color32::from_rgb(56, 56, 56),
    bg_ctrl_active: Color32::from_rgb(34, 68, 102),
    bg_btn_disabled: Color32::from_rgb(39, 39, 39),
    border_ctrl: Color32::from_rgb(72, 72, 72),
    border_btn: Color32::from_rgb(86, 86, 86),
    border_divider: Color32::from_rgb(128, 128, 128),
    border_subtle: Color32::from_rgb(52, 52, 52),
    text_heading: Color32::from_rgb(248, 248, 248),
    text_primary: Color32::from_rgb(238, 238, 238),
    text_muted: Color32::from_rgb(168, 168, 168),
    text_disabled: Color32::from_rgb(115, 115, 115),
    accent_blue: Color32::from_rgb(0, 120, 212),
    accent_blue_hover: Color32::from_rgb(38, 146, 230),
    focus_inner: Color32::from_rgb(96, 178, 245),
    icon_blue: Color32::from_rgb(96, 184, 255),
    status_green: Color32::from_rgb(16, 152, 52),
    status_red: Color32::from_rgb(214, 65, 52),
    status_track: Color32::from_rgb(48, 48, 48),
    status_border: Color32::from_rgb(76, 76, 76),
    checkbox_border: Color32::from_rgb(138, 138, 138),
    split_divider: Color32::from_rgb(66, 66, 66),
};

pub fn palette_for(dark: bool) -> &'static ThemePalette {
    if dark {
        &DARK_PALETTE
    } else {
        &LIGHT_PALETTE
    }
}

pub fn apply_luxus_style(ctx: &egui::Context, p: &ThemePalette) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::Vec2::new(8.0, 3.0);
    style.spacing.button_padding = egui::Vec2::new(9.0, 3.0);
    style.spacing.combo_height = 220.0;
    style.spacing.interact_size.y = 24.0;

    let mut visuals = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.panel_fill = p.bg_panel;
    visuals.window_fill = p.bg_panel;
    visuals.extreme_bg_color = p.bg_ctrl;
    visuals.faint_bg_color = if p.dark {
        Color32::from_rgb(38, 38, 38)
    } else {
        Color32::from_rgb(248, 248, 248)
    };
    visuals.override_text_color = Some(p.text_primary);
    visuals.window_rounding = Rounding::same(4.0);
    visuals.menu_rounding = Rounding::same(3.0);
    visuals.window_stroke = Stroke {
        width: 1.0,
        color: p.border_ctrl,
    };

    visuals.selection.bg_fill = p.accent_blue;
    visuals.selection.stroke = Stroke {
        width: 1.0,
        color: Color32::WHITE,
    };

    visuals.widgets.inactive.bg_fill = p.bg_ctrl;
    visuals.widgets.inactive.weak_bg_fill = p.bg_ctrl;
    visuals.widgets.inactive.bg_stroke = Stroke {
        width: 1.0,
        color: p.border_ctrl,
    };
    visuals.widgets.inactive.fg_stroke = Stroke {
        width: 1.0,
        color: p.text_primary,
    };
    visuals.widgets.inactive.rounding = Rounding::same(2.0);

    visuals.widgets.hovered.bg_fill = p.bg_ctrl_hover;
    visuals.widgets.hovered.weak_bg_fill = p.bg_ctrl_hover;
    visuals.widgets.hovered.bg_stroke = Stroke {
        width: 1.0,
        color: p.accent_blue_hover,
    };
    visuals.widgets.hovered.fg_stroke = Stroke {
        width: 1.0,
        color: p.text_primary,
    };
    visuals.widgets.hovered.rounding = Rounding::same(2.0);
    visuals.widgets.hovered.expansion = 0.0;

    visuals.widgets.active.bg_fill = p.bg_ctrl_active;
    visuals.widgets.active.weak_bg_fill = p.bg_ctrl_active;
    visuals.widgets.active.bg_stroke = Stroke {
        width: 1.0,
        color: p.accent_blue_hover,
    };
    visuals.widgets.active.fg_stroke = Stroke {
        width: 1.0,
        color: p.text_primary,
    };
    visuals.widgets.active.rounding = Rounding::same(2.0);
    visuals.widgets.active.expansion = 0.0;

    visuals.widgets.open.bg_fill = p.bg_ctrl;
    visuals.widgets.open.weak_bg_fill = p.bg_ctrl;
    visuals.widgets.open.bg_stroke = Stroke {
        width: 1.0,
        color: p.accent_blue_hover,
    };
    visuals.widgets.open.fg_stroke = Stroke {
        width: 1.0,
        color: p.text_primary,
    };
    visuals.widgets.open.rounding = Rounding::same(2.0);

    visuals.widgets.noninteractive.bg_fill = p.bg_panel;
    visuals.widgets.noninteractive.bg_stroke = Stroke {
        width: 1.0,
        color: p.border_subtle,
    };
    visuals.widgets.noninteractive.fg_stroke = Stroke {
        width: 1.0,
        color: p.text_primary,
    };
    visuals.widgets.noninteractive.rounding = Rounding::same(2.0);

    style.visuals = visuals;
    ctx.set_style(style);
}

/// Detects dark mode across GNOME, KDE Plasma, XFCE, Hyprland/Sway, Cinnamon, and GTK/XDG Portal.
pub fn detect_system_dark_mode() -> bool {
    // 1. GNOME / Cinnamon / Budgie / Pantheons via gsettings color-scheme
    if let Ok(out) = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).to_lowercase();
            if s.contains("prefer-dark") {
                return true;
            }
            if s.contains("prefer-light") {
                return false;
            }
        }
    }

    // 2. XDG Desktop Portal (works on GNOME, KDE Plasma 5.24+, XFCE, COSMIC, Sway/Hyprland)
    if let Ok(out) = Command::new("dbus-send")
        .args([
            "--session",
            "--print-reply=literal",
            "--dest=org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings.Read",
            "string:org.freedesktop.appearance",
            "string:color-scheme",
        ])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if s.contains("uint32 1") {
                return true;
            }
            if s.contains("uint32 2") {
                return false;
            }
        }
    }

    // 3. GNOME / GTK theme name fallback
    if let Ok(out) = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "gtk-theme"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).to_lowercase();
            if s.contains("dark") {
                return true;
            }
        }
    }

    // 4. Environment variable GTK_THEME
    if let Ok(val) = std::env::var("GTK_THEME") {
        let lower = val.to_lowercase();
        if lower.contains(":dark") || lower.contains("-dark") {
            return true;
        }
    }

    // 5. KDE Plasma ~/.config/kdeglobals
    if let Some(home) = std::env::var_os("HOME") {
        let cfg = PathBuf::from(&home).join(".config");
        if let Ok(kde) = fs::read_to_string(cfg.join("kdeglobals")) {
            if let Some(dark) = parse_kde_dark(&kde) {
                return dark;
            }
        }
        // 6. GTK 4 / GTK 3 settings.ini
        for rel in ["gtk-4.0/settings.ini", "gtk-3.0/settings.ini"] {
            if let Ok(ini) = fs::read_to_string(cfg.join(rel)) {
                if let Some(dark) = parse_gtk_ini_dark(&ini) {
                    return dark;
                }
            }
        }
    }

    false
}

fn parse_kde_dark(content: &str) -> Option<bool> {
    let mut section = "";
    for raw in content.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = &line[1..line.len() - 1];
            continue;
        }
        if section == "General" {
            if let Some(v) = line.strip_prefix("ColorScheme=") {
                let low = v.trim().to_lowercase();
                if low.contains("dark") || low.contains("noir") || low.contains("night") {
                    return Some(true);
                }
                if low.contains("light") || low.contains("day") {
                    return Some(false);
                }
            }
        }
        if section == "Colors:Window" {
            if let Some(v) = line.strip_prefix("BackgroundNormal=") {
                let rgb: Vec<u32> = v
                    .split(',')
                    .filter_map(|p| p.trim().parse::<u32>().ok())
                    .collect();
                if rgb.len() == 3 {
                    let luma = (rgb[0] * 299 + rgb[1] * 587 + rgb[2] * 114) / 1000;
                    return Some(luma < 128);
                }
            }
        }
    }
    None
}

fn parse_gtk_ini_dark(content: &str) -> Option<bool> {
    for raw in content.lines() {
        let line = raw.trim();
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim();
            let val = v.trim().to_lowercase();
            if key == "gtk-application-prefer-dark-theme" {
                if val == "1" || val == "true" {
                    return Some(true);
                }
                if val == "0" || val == "false" {
                    return Some(false);
                }
            }
            if key == "gtk-theme-name" && val.contains("dark") {
                return Some(true);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kde_and_gtk_theme_parsers() {
        assert_eq!(parse_kde_dark("[General]\nColorScheme=BreezeDark\n"), Some(true));
        assert_eq!(parse_kde_dark("[General]\nColorScheme=BreezeLight\n"), Some(false));
        assert_eq!(
            parse_kde_dark("[Colors:Window]\nBackgroundNormal=35,38,41\n"),
            Some(true)
        );
        assert_eq!(
            parse_kde_dark("[Colors:Window]\nBackgroundNormal=240,242,245\n"),
            Some(false)
        );
        assert_eq!(
            parse_gtk_ini_dark("[Settings]\ngtk-application-prefer-dark-theme=1\n"),
            Some(true)
        );
        assert_eq!(
            parse_gtk_ini_dark("[Settings]\ngtk-application-prefer-dark-theme=false\n"),
            Some(false)
        );
    }
}
