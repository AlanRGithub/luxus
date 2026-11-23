mod config;
mod devices;
mod download;
mod hash;
mod i18n;
mod mechanics;
mod theme;
mod ui;
mod vhd;
mod windows;
mod writer;

use config::load_config;
use eframe::egui;
use i18n::{detect_system_language, tr, Lang};
use std::path::PathBuf;
use theme::{apply_luxus_style, detect_system_dark_mode, palette_for, ThemeMode};

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Privileged single-elevation worker entrypoints (invoked via pkexec/sudo/doas)
    if let Some(spec_path) = parse_arg_value(&args, "--worker-job") {
        let code = writer::run_worker_cli(&spec_path);
        std::process::exit(code);
    }
    if let Some(spec_path) = parse_arg_value(&args, "--backup-job") {
        let code = writer::run_backup_cli(&spec_path);
        std::process::exit(code);
    }
    if let Some(spec_path) = parse_arg_value(&args, "--wipe-job") {
        let code = writer::run_wipe_cli(&spec_path);
        std::process::exit(code);
    }

    let cfg = load_config();
    let screenshot_path = parse_arg_value(&args, "--screenshot");
    let theme_mode = parse_theme_arg(&args).unwrap_or_else(|| cfg.parsed_theme());
    let initial_dark = match theme_mode {
        ThemeMode::Auto => detect_system_dark_mode(),
        ThemeMode::Light => false,
        ThemeMode::Dark => true,
    };

    let (lang_override, initial_lang) = parse_locale_arg(&args, cfg.parsed_lang());
    let title = format!(
        "{} {}",
        tr(initial_lang).window_title,
        env!("CARGO_PKG_VERSION")
    );

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(title.clone())
        .with_inner_size([516.0, 636.0])
        .with_min_inner_size([480.0, 580.0]);

    if let Some(icon) = load_app_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }

    if screenshot_path.is_some() {
        viewport = viewport.with_active(false);
        #[cfg(target_os = "windows")]
        {
            viewport = viewport.with_window_level(egui::WindowLevel::AlwaysOnBottom);
        }
    }

    let mut options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    options.event_loop_builder = Some(Box::new(|_builder| {
        #[cfg(target_os = "macos")]
        {
            use winit::platform::macos::EventLoopBuilderExtMacOS;
            _builder.with_activate_ignoring_other_apps(false);
        }
    }));

    let app = ui::LuxusApp::new(
        screenshot_path,
        theme_mode,
        initial_dark,
        lang_override,
        initial_lang,
        cfg,
    );
    eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            apply_luxus_style(&cc.egui_ctx, palette_for(initial_dark));
            Ok(Box::new(app))
        }),
    )
}

fn parse_arg_value(args: &[String], flag: &str) -> Option<PathBuf> {
    let prefix = format!("{}=", flag);
    let mut i = 1;
    while i < args.len() {
        if args[i] == flag && i + 1 < args.len() {
            return Some(PathBuf::from(&args[i + 1]));
        }
        if let Some(v) = args[i].strip_prefix(&prefix) {
            return Some(PathBuf::from(v));
        }
        i += 1;
    }
    None
}

fn parse_theme_arg(args: &[String]) -> Option<ThemeMode> {
    if let Some(val) = parse_arg_value(args, "--theme") {
        if let Some(s) = val.to_str() {
            return Some(match s.to_lowercase().as_str() {
                "light" | "claro" => ThemeMode::Light,
                "dark" | "oscuro" => ThemeMode::Dark,
                _ => ThemeMode::Auto,
            });
        }
    }
    None
}

fn parse_locale_arg(args: &[String], saved: Option<Lang>) -> (Option<Lang>, Lang) {
    let raw = parse_arg_value(args, "--locale").or_else(|| parse_arg_value(args, "-l"));
    if let Some(val) = raw {
        if let Some(s) = val.to_str() {
            if s.eq_ignore_ascii_case("auto") {
                return (None, detect_system_language());
            }
            if let Some(lang) = Lang::from_locale(s) {
                return (Some(lang), lang);
            }
        }
    }
    match saved {
        Some(l) => (Some(l), l),
        None => (None, detect_system_language()),
    }
}

fn load_app_icon() -> Option<egui::IconData> {
    const ICON_PNG: &[u8] = include_bytes!("../assets/luxus-64.png");
    let img = image::load_from_memory(ICON_PNG).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    })
}
