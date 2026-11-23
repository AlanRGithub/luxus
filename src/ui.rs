//! Luxus GUI built with egui/eframe:
//! - Compact dual-theme (automatic OS dark/light) interface in 8 languages
//! - Windows bootable media + UEFI:NTFS + Windows User Experience (autounattend.xml) modal
//! - Linux persistent partition slider, ISO downloader, USB-to-IMG/VHD backup, drag-and-drop

use crate::config::{save_config, AppConfig};
use crate::devices::{
    eject_usb_device, list_usb_devices, probe_system_tools, screenshot_demo_device, DeviceInfo,
};
use crate::download::{start_iso_download, DownloadState, ISO_CATALOG};
use crate::hash::{compute_file_checksums, ChecksumResult};
use crate::i18n::{detect_system_language, tr, Lang};
use crate::mechanics::{
    allowed_clusters_for_mask, default_cluster_for_fs, format_cluster_label, human_size,
    inspect_image_file, normalize_label, CompressionKind, ImageReport, PersistenceStyle,
    EXFAT_CLUSTER_MASK, FAT32_CLUSTER_MASK, LARGE_FAT32_CLUSTER_MASK, LARGE_FAT32_THRESHOLD,
    NTFS_CLUSTER_MASK,
};
use crate::theme::{
    apply_luxus_style, detect_system_dark_mode, palette_for, ThemeMode, ThemePalette,
};
use crate::windows::{detect_windows_locale, generate_autounattend_xml, WindowsUxOptions};
use crate::writer::{
    format_elapsed, spawn_backup_job, spawn_gui_job, spawn_wipe_job, BackupSpec, BootMode,
    FileSystemKind, ImageWriteOption, JobSpec, JobState, PartitionScheme, TargetSystem, WipeSpec,
};
use eframe::egui::{
    self, Align, Align2, Color32, FontId, Pos2, Rect, Rounding, Sense, Stroke, Vec2,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[inline]
fn stroke(width: f32, color: Color32) -> Stroke {
    Stroke { width, color }
}

#[derive(Default)]
struct ChecksumDialogState {
    open: bool,
    running: bool,
    progress: f32,
    expected_hash: String,
    result: Option<ChecksumResult>,
    error: Option<String>,
}

pub struct LuxusApp {
    screenshot_path: Option<PathBuf>,
    screenshot_frames: u32,
    screenshot_saved: bool,

    lang_override: Option<Lang>,
    lang: Lang,
    theme_mode: ThemeMode,
    system_dark: bool,
    applied_dark: bool,
    theme_poll_rx: Option<std::sync::mpsc::Receiver<bool>>,
    last_theme_poll: Instant,

    devices: Vec<DeviceInfo>,
    selected_device: usize,
    device_refresh_rx: Option<std::sync::mpsc::Receiver<Vec<DeviceInfo>>>,
    last_device_refresh: Instant,

    boot_mode: BootMode,
    select_is_download_mode: bool,
    image_path: String,
    image_display_name: String,
    image_report: Option<ImageReport>,
    image_option: ImageWriteOption,
    persistence_gb: u32,

    partition_scheme: PartitionScheme,
    target_system: TargetSystem,

    show_advanced_drive: bool,
    list_usb_hard_drives: bool,
    old_bios_fixes: bool,
    use_uefi_mbr_id: bool,
    enable_uefi_ntfs: bool,

    volume_label: String,
    filesystem: FileSystemKind,
    cluster_size: u32, // 0 = auto default

    show_advanced_format: bool,
    quick_format: bool,
    extended_label_icon: bool,
    check_bad_blocks: bool,
    bad_blocks_passes: u8,
    ntfs_compression: bool,

    // Windows User Experience customization
    windows_ux: WindowsUxOptions,
    win_ux_open: bool,
    win_ux_pending_start: bool,

    // Settings
    verify_after_write: bool,
    buffer_mb: usize,

    // Modals / Popups
    confirm_open: bool,
    confirm_is_wipe: bool,
    log_open: bool,
    settings_open: bool,
    about_open: bool,
    help_target_open: bool,
    lang_popup_open: bool,
    select_menu_open: bool,

    download_state: Arc<Mutex<DownloadState>>,
    download_cancel: Arc<AtomicBool>,

    checksum_state: Arc<Mutex<ChecksumDialogState>>,
    checksum_cancel: Arc<AtomicBool>,

    job: Arc<Mutex<JobState>>,
    job_cancel: Arc<AtomicBool>,
}

impl LuxusApp {
    pub fn new(
        screenshot_path: Option<PathBuf>,
        theme_mode: ThemeMode,
        initial_dark: bool,
        lang_override: Option<Lang>,
        initial_lang: Lang,
        cfg: AppConfig,
    ) -> Self {
        let is_shot = screenshot_path.is_some();
        let mut app = Self {
            screenshot_path,
            screenshot_frames: 0,
            screenshot_saved: false,

            lang_override,
            lang: initial_lang,
            theme_mode,
            system_dark: initial_dark,
            applied_dark: initial_dark,
            theme_poll_rx: None,
            last_theme_poll: Instant::now(),

            devices: Vec::new(),
            selected_device: 0,
            device_refresh_rx: None,
            last_device_refresh: Instant::now() - Duration::from_secs(60),

            boot_mode: BootMode::IsoImage,
            select_is_download_mode: false,
            image_path: String::new(),
            image_display_name: String::new(),
            image_report: None,
            image_option: ImageWriteOption::WindowsStandard,
            persistence_gb: 0,

            partition_scheme: PartitionScheme::Gpt,
            target_system: TargetSystem::Uefi,

            show_advanced_drive: false,
            list_usb_hard_drives: cfg.list_usb_hard_drives,
            old_bios_fixes: cfg.old_bios_fixes,
            use_uefi_mbr_id: cfg.use_uefi_mbr_id,
            enable_uefi_ntfs: cfg.enable_uefi_ntfs,

            volume_label: String::new(),
            filesystem: FileSystemKind::Ntfs,
            cluster_size: 0,

            show_advanced_format: true,
            quick_format: true,
            extended_label_icon: true,
            check_bad_blocks: false,
            bad_blocks_passes: 1,
            ntfs_compression: cfg.ntfs_compression,

            windows_ux: cfg.windows_ux,
            win_ux_open: false,
            win_ux_pending_start: false,

            verify_after_write: cfg.verify_after_write,
            buffer_mb: cfg.buffer_mb.clamp(1, 16),

            confirm_open: false,
            confirm_is_wipe: false,
            log_open: false,
            settings_open: false,
            about_open: false,
            help_target_open: false,
            lang_popup_open: false,
            select_menu_open: false,

            download_state: Arc::new(Mutex::new(DownloadState::default())),
            download_cancel: Arc::new(AtomicBool::new(false)),

            checksum_state: Arc::new(Mutex::new(ChecksumDialogState::default())),
            checksum_cancel: Arc::new(AtomicBool::new(false)),

            job: Arc::new(Mutex::new(JobState::default())),
            job_cancel: Arc::new(AtomicBool::new(false)),
        };

        if let Ok(list) = list_usb_devices(app.list_usb_hard_drives) {
            app.devices = list;
        }
        app.last_device_refresh = Instant::now();
        app.sync_fs_for_device();

        if is_shot {
            if app.devices.is_empty() {
                app.devices.push(screenshot_demo_device());
            }
            if app.image_path.is_empty() {
                app.image_path = "/home/user/Descargas/Windows_11_24H2.iso".into();
                app.image_display_name = "Windows_11_24H2.iso".into();
                app.image_option = ImageWriteOption::WindowsStandard;
                app.volume_label = "WINDOWS 11 24H2".into();
                app.filesystem = FileSystemKind::Ntfs;
                app.cluster_size = 0;
            }
        }
        app
    }

    fn effective_dark(&self) -> bool {
        match self.theme_mode {
            ThemeMode::Auto => self.system_dark,
            ThemeMode::Light => false,
            ThemeMode::Dark => true,
        }
    }

    fn sync_theme(&mut self, ctx: &egui::Context) {
        if let Some(sys_theme) = ctx.system_theme() {
            self.system_dark = sys_theme == egui::Theme::Dark;
        }
        if let Some(rx) = &self.theme_poll_rx {
            if let Ok(is_dark) = rx.try_recv() {
                self.system_dark = is_dark;
                self.theme_poll_rx = None;
            }
        }
        if self.theme_mode == ThemeMode::Auto
            && self.theme_poll_rx.is_none()
            && self.last_theme_poll.elapsed() > Duration::from_millis(2500)
        {
            self.last_theme_poll = Instant::now();
            let (tx, rx) = std::sync::mpsc::channel();
            self.theme_poll_rx = Some(rx);
            thread::spawn(move || {
                let _ = tx.send(detect_system_dark_mode());
            });
        }
        let want_dark = self.effective_dark();
        if want_dark != self.applied_dark {
            self.applied_dark = want_dark;
            apply_luxus_style(ctx, palette_for(want_dark));
        }
    }

    fn fs_cluster_mask(&self) -> u32 {
        match self.filesystem {
            FileSystemKind::Fat32 => FAT32_CLUSTER_MASK,
            FileSystemKind::LargeFat32 => LARGE_FAT32_CLUSTER_MASK,
            FileSystemKind::Ntfs
            | FileSystemKind::Udf
            | FileSystemKind::Ext4
            | FileSystemKind::Ext3
            | FileSystemKind::Ext2 => NTFS_CLUSTER_MASK,
            FileSystemKind::Exfat => EXFAT_CLUSTER_MASK,
        }
    }

    fn sync_fs_for_device(&mut self) {
        let vol = self.selected_volume_bytes();
        if vol > LARGE_FAT32_THRESHOLD && self.filesystem == FileSystemKind::Fat32 {
            self.filesystem = FileSystemKind::LargeFat32;
        } else if vol <= LARGE_FAT32_THRESHOLD && self.filesystem == FileSystemKind::LargeFat32 {
            self.filesystem = FileSystemKind::Fat32;
        }
        let max_p = self.max_persistence_gb();
        if self.persistence_gb > max_p {
            self.persistence_gb = max_p;
        }
        self.sanitize_cluster_size();
    }

    fn sanitize_cluster_size(&mut self) {
        if self.cluster_size != 0 && (self.fs_cluster_mask() & self.cluster_size) == 0 {
            self.cluster_size = 0;
        }
    }

    fn max_persistence_gb(&self) -> u32 {
        let dev_bytes = self.selected_volume_bytes();
        let img_bytes = self
            .image_report
            .as_ref()
            .map(|r| r.file_size)
            .unwrap_or(2 * 1024 * 1024 * 1024);
        let free = dev_bytes.saturating_sub(img_bytes + 512 * 1024 * 1024);
        ((free / (1024 * 1024 * 1024)) as u32).min(64)
    }

    fn trigger_device_refresh(&mut self) {
        self.last_device_refresh = Instant::now();
        let include_hdd = self.list_usb_hard_drives;
        let is_shot = self.screenshot_path.is_some();
        let (tx, rx) = std::sync::mpsc::channel();
        self.device_refresh_rx = Some(rx);
        thread::spawn(move || {
            let mut list = list_usb_devices(include_hdd).unwrap_or_default();
            if list.is_empty() && is_shot {
                list.push(screenshot_demo_device());
            }
            let _ = tx.send(list);
        });
    }

    fn poll_device_refresh(&mut self) {
        if let Some(rx) = &self.device_refresh_rx {
            if let Ok(list) = rx.try_recv() {
                let prev_path = self.selected_device().map(|d| d.path.clone());
                self.devices = list;
                if let Some(prev) = prev_path {
                    if let Some(idx) = self.devices.iter().position(|d| d.path == prev) {
                        self.selected_device = idx;
                    } else {
                        self.selected_device = 0;
                    }
                } else {
                    self.selected_device = 0;
                }
                self.device_refresh_rx = None;
                self.sync_fs_for_device();
            }
        }
    }

    fn selected_device(&self) -> Option<&DeviceInfo> {
        self.devices.get(self.selected_device)
    }

    fn selected_volume_bytes(&self) -> u64 {
        self.selected_device()
            .map(|d| d.size_bytes)
            .unwrap_or(8 * 1024 * 1024 * 1024)
    }

    fn persist_config(&self) {
        if self.screenshot_path.is_some() {
            return;
        }
        let mut cfg = AppConfig {
            language: self.lang_override.map(|l| l.code().to_string()),
            theme: "auto".into(),
            buffer_mb: self.buffer_mb,
            verify_after_write: self.verify_after_write,
            ntfs_compression: self.ntfs_compression,
            list_usb_hard_drives: self.list_usb_hard_drives,
            enable_uefi_ntfs: self.enable_uefi_ntfs,
            old_bios_fixes: self.old_bios_fixes,
            use_uefi_mbr_id: self.use_uefi_mbr_id,
            windows_ux: self.windows_ux.clone(),
        };
        cfg.set_theme(self.theme_mode);
        save_config(&cfg);
    }

    fn log_msg(&self, msg: String) {
        if let Ok(mut j) = self.job.lock() {
            j.logs.push(crate::writer::LogLine {
                ts: crate::writer::now_hms(),
                msg,
            });
        }
    }

    fn eject_selected_device(&mut self) {
        let Some(dev) = self.selected_device().cloned() else {
            return;
        };
        let path = dev.path.clone();
        match eject_usb_device(&path) {
            Ok(msg) => {
                self.log_msg(msg);
                self.trigger_device_refresh();
            }
            Err(e) => {
                self.log_msg(format!("Aviso al expulsar {}: {}", path, e));
            }
        }
    }

    fn can_start(&self) -> bool {
        if self.selected_device().is_none() {
            return false;
        }
        match self.boot_mode {
            BootMode::NonBootable | BootMode::FreeDos => true,
            BootMode::IsoImage | BootMode::DdImage => !self.image_path.trim().is_empty(),
        }
    }

    fn pick_image_file(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter(
                "ISO / Disk / VHD / Compressed Images",
                &[
                    "iso", "img", "vhd", "bin", "raw", "dd", "wic", "usb", "gz", "xz", "zst",
                    "bz2", "bzip2", "lzma", "zip",
                ],
            )
            .pick_file()
        {
            self.load_image_path(p);
        }
    }

    fn pick_backup_destination(&mut self) {
        let Some(dev) = self.selected_device().cloned() else {
            return;
        };
        let default_name = format!(
            "{}_backup.vhd",
            normalize_label(&dev.model, true, "usb").replace(' ', "_")
        );
        if let Some(p) = rfd::FileDialog::new()
            .set_file_name(&default_name)
            .add_filter("Virtual Hard Disk (.vhd)", &["vhd"])
            .add_filter("Raw Disk Image (.img)", &["img", "raw"])
            .save_file()
        {
            let as_vhd = p
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("vhd"))
                .unwrap_or(false);
            let spec = BackupSpec {
                device: dev,
                output_path: p.display().to_string(),
                as_vhd,
                buffer_mb: self.buffer_mb,
            };
            spawn_backup_job(self.job.clone(), self.job_cancel.clone(), spec);
        }
    }

    fn load_image_path(&mut self, p: PathBuf) {
        let name = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("image.iso")
            .to_string();
        let ext_lower = p
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        self.image_path = p.display().to_string();
        self.image_display_name = name;
        self.persistence_gb = 0;

        let is_raw_ext = matches!(
            ext_lower.as_str(),
            "img"
                | "dd"
                | "raw"
                | "bin"
                | "vhd"
                | "wic"
                | "usb"
                | "gz"
                | "xz"
                | "zst"
                | "bz2"
                | "bzip2"
                | "lzma"
                | "zip"
        );
        if matches!(self.boot_mode, BootMode::NonBootable | BootMode::FreeDos) {
            self.boot_mode = if is_raw_ext {
                BootMode::DdImage
            } else {
                BootMode::IsoImage
            };
        }

        match inspect_image_file(&p) {
            Ok(report) => {
                let raw_lbl = report.iso_label.clone().unwrap_or_else(|| {
                    p.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("LUXUS_USB")
                        .to_string()
                });
                if report.compression != CompressionKind::None
                    || report.is_fixed_vhd
                    || report.force_dd
                    || is_raw_ext
                {
                    self.image_option = ImageWriteOption::DdStrict;
                    if report.compression != CompressionKind::None || report.is_fixed_vhd {
                        self.boot_mode = BootMode::DdImage;
                    }
                } else if report.is_windows_iso {
                    self.boot_mode = BootMode::IsoImage;
                    self.image_option = ImageWriteOption::WindowsStandard;
                    self.filesystem = FileSystemKind::Ntfs;
                    self.enable_uefi_ntfs = true;
                    self.partition_scheme = PartitionScheme::Gpt;
                    self.target_system = TargetSystem::Uefi;
                    self.sanitize_cluster_size();
                } else if (report.is_iso9660 || report.is_udf)
                    && !report.is_hybrid_mbr
                    && !report.has_gpt
                {
                    // Non-hybrid ISO9660/UDF cannot boot if written raw; use ISO extraction
                    self.boot_mode = BootMode::IsoImage;
                    self.image_option = ImageWriteOption::IsoExtract;
                    if report.has_over_4gb_file {
                        self.filesystem = FileSystemKind::Ntfs;
                        self.enable_uefi_ntfs = true;
                    } else {
                        self.filesystem = if self.selected_volume_bytes() > LARGE_FAT32_THRESHOLD {
                            FileSystemKind::LargeFat32
                        } else {
                            FileSystemKind::Fat32
                        };
                    }
                    self.sanitize_cluster_size();
                } else {
                    self.boot_mode = BootMode::IsoImage;
                    self.image_option = ImageWriteOption::HybridRaw;
                }
                self.volume_label = normalize_label(&raw_lbl, self.filesystem.is_fat32(), "USB");

                let mut j = self.job.lock().unwrap();
                j.logs.push(crate::writer::LogLine {
                    ts: crate::writer::now_hms(),
                    msg: format!(
                        "Imagen analizada: {} — {} ({})",
                        report.file_name,
                        report.signature,
                        human_size(report.file_size)
                    ),
                });
                for n in &report.notes {
                    j.logs.push(crate::writer::LogLine {
                        ts: crate::writer::now_hms(),
                        msg: n.clone(),
                    });
                }
                self.image_report = Some(report);
            }
            Err(e) => {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    self.volume_label = normalize_label(stem, self.filesystem.is_fat32(), "USB");
                }
                let mut j = self.job.lock().unwrap();
                j.logs.push(crate::writer::LogLine {
                    ts: crate::writer::now_hms(),
                    msg: format!("Aviso al inspeccionar imagen: {}", e),
                });
            }
        }
    }

    fn start_checksum_job(&mut self) {
        if self.image_path.trim().is_empty() {
            return;
        }
        let path = PathBuf::from(&self.image_path);
        self.checksum_cancel.store(false, Ordering::SeqCst);
        {
            let mut st = self.checksum_state.lock().unwrap();
            st.open = true;
            st.running = true;
            st.progress = 0.0;
            st.result = None;
            st.error = None;
        }
        let state = self.checksum_state.clone();
        let cancel = self.checksum_cancel.clone();
        thread::spawn(move || {
            let res = compute_file_checksums(&path, &cancel, |p| {
                if let Ok(mut st) = state.lock() {
                    st.progress = p;
                }
            });
            if let Ok(mut st) = state.lock() {
                st.running = false;
                match res {
                    Ok(r) => {
                        st.progress = 1.0;
                        st.result = Some(r);
                    }
                    Err(e) => {
                        st.error = Some(e);
                    }
                }
            }
        });
    }

    fn is_windows_install_selected(&self) -> bool {
        self.boot_mode == BootMode::IsoImage
            && !self.image_path.trim().is_empty()
            && self.image_option == ImageWriteOption::WindowsStandard
    }

    fn on_start_clicked(&mut self) {
        self.confirm_is_wipe = false;
        if self.is_windows_install_selected() {
            // Show Windows User Experience dialog first, then confirm erasure
            self.win_ux_pending_start = true;
            self.win_ux_open = true;
        } else {
            self.confirm_open = true;
        }
    }

    fn start_write_job(&mut self) {
        let Some(dev) = self.selected_device().cloned() else {
            return;
        };
        self.sanitize_cluster_size();
        let p_style = self
            .image_report
            .as_ref()
            .map(|r| r.persistence_style)
            .unwrap_or(PersistenceStyle::None);
        let win_ux = if self.is_windows_install_selected() {
            Some(self.windows_ux.clone())
        } else {
            None
        };
        let spec = JobSpec {
            device: dev,
            boot_mode: self.boot_mode,
            image_option: self.image_option,
            image_path: self.image_path.trim().to_string(),
            partition_scheme: self.partition_scheme,
            target_system: self.target_system,
            filesystem: self.filesystem,
            cluster_size: self.cluster_size,
            volume_label: self.volume_label.clone(),
            quick_format: self.quick_format,
            extended_label_icon: self.extended_label_icon,
            check_bad_blocks: self.check_bad_blocks,
            bad_blocks_passes: self.bad_blocks_passes,
            ntfs_compression: self.ntfs_compression,
            old_bios_fixes: self.old_bios_fixes,
            use_uefi_mbr_id: self.use_uefi_mbr_id,
            enable_uefi_ntfs: self.enable_uefi_ntfs,
            persistence_gb: self.persistence_gb,
            persistence_style: p_style,
            windows_ux: win_ux,
            verify_after_write: self.verify_after_write,
            buffer_mb: self.buffer_mb,
        };
        spawn_gui_job(self.job.clone(), self.job_cancel.clone(), spec);
    }

    fn available_filesystems(&self) -> Vec<FileSystemKind> {
        let vol = self.selected_volume_bytes();
        let fat = if vol > LARGE_FAT32_THRESHOLD {
            FileSystemKind::LargeFat32
        } else {
            FileSystemKind::Fat32
        };
        vec![
            fat,
            FileSystemKind::Ntfs,
            FileSystemKind::Udf,
            FileSystemKind::Exfat,
            FileSystemKind::Ext4,
            FileSystemKind::Ext3,
            FileSystemKind::Ext2,
        ]
    }

    fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.screenshot_path.clone() else {
            return;
        };
        if self.screenshot_saved {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let mut captured: Option<egui::ColorImage> = None;
        ctx.input(|i| {
            for ev in &i.raw.events {
                if let egui::Event::Screenshot { image, .. } = ev {
                    captured = Some((**image).clone());
                }
            }
        });
        if let Some(image) = captured {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let pixels: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
            if let Some(img) =
                image::RgbaImage::from_raw(image.width() as u32, image.height() as u32, pixels)
            {
                match img.save_with_format(&path, image::ImageFormat::Png) {
                    Ok(_) => {
                        eprintln!("SCREENSHOT_SAVED {}", path.display());
                        self.screenshot_saved = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        return;
                    }
                    Err(e) => eprintln!("SCREENSHOT_SAVE_ERR {}", e),
                }
            }
        }
        self.screenshot_frames += 1;
        if self.screenshot_frames == 120 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
        }
        if self.screenshot_frames > 240 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(16));
    }

    fn draw_main_content(&mut self, ui: &mut egui::Ui, p: &ThemePalette) {
        let t = tr(self.lang);
        let running = self.job.lock().unwrap().running;
        let full_w = ui.available_width();

        // ====================================================================
        // SECTION 1: Propiedades de la unidad
        // ====================================================================
        section_header(ui, t.drive_properties, p);
        ui.add_space(2.0);

        ui.add_enabled_ui(!running, |ui| {
            // Dispositivo
            field_label(ui, t.device, p);
            let dev_text = self
                .selected_device()
                .map(|d| d.display_label())
                .unwrap_or_else(|| t.no_device.to_string());
            let prev_dev = self.selected_device;
            egui::ComboBox::from_id_salt("luxus_device")
                .selected_text(dev_text)
                .width(full_w)
                .show_ui(ui, |ui| {
                    if self.devices.is_empty() {
                        ui.label(t.no_device);
                    } else {
                        for i in 0..self.devices.len() {
                            let label = self.devices[i].display_label();
                            ui.selectable_value(&mut self.selected_device, i, label);
                        }
                    }
                });
            if self.selected_device != prev_dev {
                self.sync_fs_for_device();
            }
            ui.add_space(4.0);

            // Elección de arranque + Checksum icon + SELECCIONAR/DESCARGAR | ▾ split button
            field_label(ui, t.boot_selection, p);
            let boot_text = match self.boot_mode {
                BootMode::NonBootable => t.boot_non_bootable.to_string(),
                BootMode::FreeDos => t.boot_freedos.to_string(),
                BootMode::IsoImage => {
                    if self.image_display_name.is_empty() {
                        t.boot_iso_prompt.to_string()
                    } else {
                        self.image_display_name.clone()
                    }
                }
                BootMode::DdImage => {
                    if self.image_display_name.is_empty() {
                        t.boot_dd_prompt.to_string()
                    } else {
                        self.image_display_name.clone()
                    }
                }
            };

            let has_image = matches!(self.boot_mode, BootMode::IsoImage | BootMode::DdImage)
                && !self.image_path.trim().is_empty();
            let split_w = 114.0;
            let chk_w = 24.0;
            let gap = 8.0;
            let combo_w = (full_w - split_w - chk_w - gap * 2.0).max(160.0);

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                egui::ComboBox::from_id_salt("luxus_boot")
                    .selected_text(boot_text)
                    .width(combo_w)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.boot_mode,
                            BootMode::NonBootable,
                            t.boot_non_bootable,
                        );
                        if ui
                            .selectable_value(
                                &mut self.boot_mode,
                                BootMode::FreeDos,
                                t.boot_freedos,
                            )
                            .clicked()
                        {
                            self.partition_scheme = PartitionScheme::Mbr;
                            self.target_system = TargetSystem::BiosUefi;
                            self.filesystem = if self.selected_volume_bytes() > LARGE_FAT32_THRESHOLD
                            {
                                FileSystemKind::LargeFat32
                            } else {
                                FileSystemKind::Fat32
                            };
                            if self.volume_label.trim().is_empty() {
                                self.volume_label = "FREEDOS".into();
                            }
                            self.sanitize_cluster_size();
                        }
                        ui.selectable_value(
                            &mut self.boot_mode,
                            BootMode::IsoImage,
                            t.boot_iso_prompt,
                        );
                        ui.selectable_value(
                            &mut self.boot_mode,
                            BootMode::DdImage,
                            t.boot_dd_prompt,
                        );
                    });

                if icon_checksum_button(ui, has_image, p)
                    .on_hover_text(t.checksum_title)
                    .clicked()
                    && has_image
                {
                    self.start_checksum_job();
                }

                let split_label = if self.select_is_download_mode {
                    t.download_btn
                } else {
                    t.select_btn
                };
                let (main_clicked, drop_clicked) =
                    split_select_button(ui, split_label, split_w, 24.0, p);
                if main_clicked {
                    if self.select_is_download_mode {
                        self.download_state.lock().unwrap().open = true;
                    } else {
                        self.pick_image_file();
                    }
                }
                if drop_clicked {
                    self.select_menu_open = !self.select_menu_open;
                }
            });

            if self.select_menu_open {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            if ui.button(t.select_btn).clicked() {
                                self.select_is_download_mode = false;
                                self.select_menu_open = false;
                                self.pick_image_file();
                            }
                            if ui.button(t.download_btn).clicked() {
                                self.select_is_download_mode = true;
                                self.select_menu_open = false;
                                self.download_state.lock().unwrap().open = true;
                            }
                            if ui.button(t.refresh_devices).clicked() {
                                self.select_menu_open = false;
                                self.trigger_device_refresh();
                            }
                        });
                    });
                });
            }

            // Opciones de imagen
            if matches!(self.boot_mode, BootMode::IsoImage | BootMode::DdImage)
                && !self.image_path.trim().is_empty()
            {
                ui.add_space(4.0);
                field_label(ui, t.image_options, p);
                let opt_label = match self.image_option {
                    ImageWriteOption::WindowsStandard => t.img_opt_windows,
                    ImageWriteOption::HybridRaw => t.img_opt_hybrid,
                    ImageWriteOption::IsoExtract => t.img_opt_iso_extract,
                    ImageWriteOption::DdStrict => t.img_opt_dd_strict,
                };
                let show_win_btn = self.image_option == ImageWriteOption::WindowsStandard;
                let opt_w = if show_win_btn {
                    (full_w - 104.0 - 6.0).max(180.0)
                } else {
                    full_w
                };
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    egui::ComboBox::from_id_salt("luxus_img_opt")
                        .selected_text(opt_label)
                        .width(opt_w)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.image_option,
                                ImageWriteOption::WindowsStandard,
                                t.img_opt_windows,
                            );
                            ui.selectable_value(
                                &mut self.image_option,
                                ImageWriteOption::HybridRaw,
                                t.img_opt_hybrid,
                            );
                            ui.selectable_value(
                                &mut self.image_option,
                                ImageWriteOption::IsoExtract,
                                t.img_opt_iso_extract,
                            );
                            ui.selectable_value(
                                &mut self.image_option,
                                ImageWriteOption::DdStrict,
                                t.img_opt_dd_strict,
                            );
                        });
                    if show_win_btn
                        && luxus_button(ui, t.win_ux_customize_btn, 104.0, 24.0, false, p)
                            .on_hover_text(t.win_ux_title)
                            .clicked()
                    {
                        self.win_ux_pending_start = false;
                        self.win_ux_open = true;
                    }
                });

                // Linux Live persistent partition slider (shown when a Linux Live ISO is loaded)
                let p_style = self
                    .image_report
                    .as_ref()
                    .map(|r| r.persistence_style)
                    .unwrap_or(PersistenceStyle::None);
                if p_style != PersistenceStyle::None && !show_win_btn {
                    let max_p = self.max_persistence_gb();
                    if max_p > 0 {
                        ui.add_space(4.0);
                        field_label(ui, t.persistence_label, p);
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::Slider::new(&mut self.persistence_gb, 0..=max_p)
                                    .suffix(" GB"),
                            );
                        });
                    }
                }
            }

            ui.add_space(4.0);

            // Aligned 2-column grid for partition scheme & target system
            let col_gap = 24.0;
            let left_col_w = ((full_w - col_gap) * 0.48).floor().max(150.0);
            let right_col_w = (full_w - col_gap - left_col_w).max(150.0);
            let help_w = 14.0;

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = col_gap;
                ui.vertical(|ui| {
                    ui.set_width(left_col_w);
                    field_label(ui, t.partition_scheme, p);
                    egui::ComboBox::from_id_salt("luxus_part")
                        .selected_text(self.partition_scheme.label())
                        .width(left_col_w)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_value(
                                    &mut self.partition_scheme,
                                    PartitionScheme::Mbr,
                                    "MBR",
                                )
                                .clicked()
                            {
                                self.target_system = TargetSystem::BiosUefi;
                            }
                            if ui
                                .selectable_value(
                                    &mut self.partition_scheme,
                                    PartitionScheme::Gpt,
                                    "GPT",
                                )
                                .clicked()
                            {
                                self.target_system = TargetSystem::Uefi;
                            }
                        });
                });

                ui.vertical(|ui| {
                    ui.set_width(right_col_w);
                    field_label(ui, t.target_system, p);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let target_txt = match self.target_system {
                            TargetSystem::Uefi => t.target_uefi,
                            TargetSystem::BiosUefi => t.target_bios_uefi,
                        };
                        egui::ComboBox::from_id_salt("luxus_target")
                            .selected_text(target_txt)
                            .width((right_col_w - help_w - 4.0).max(120.0))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.target_system,
                                    TargetSystem::Uefi,
                                    t.target_uefi,
                                );
                                ui.selectable_value(
                                    &mut self.target_system,
                                    TargetSystem::BiosUefi,
                                    t.target_bios_uefi,
                                );
                            });
                        if ui
                            .add(
                                egui::Label::new(
                                    egui::RichText::new("?")
                                        .color(p.icon_blue)
                                        .size(13.5)
                                        .strong(),
                                )
                                .sense(Sense::click()),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                        {
                            self.help_target_open = true;
                        }
                    });
                });
            });

            ui.add_space(4.0);

            // Expandable: Mostrar propiedades avanzadas de la unidad
            let adv_drive_label = if self.show_advanced_drive {
                t.hide_adv_drive
            } else {
                t.show_adv_drive
            };
            if chevron_toggle(ui, self.show_advanced_drive, adv_drive_label, p) {
                self.show_advanced_drive = !self.show_advanced_drive;
            }
            if self.show_advanced_drive {
                ui.add_space(2.0);
                if luxus_checkbox(ui, &mut self.list_usb_hard_drives, t.list_usb_hdd, p) {
                    self.persist_config();
                    self.trigger_device_refresh();
                }
                if luxus_checkbox(ui, &mut self.enable_uefi_ntfs, t.enable_uefi_ntfs, p) {
                    self.persist_config();
                }
                if luxus_checkbox(ui, &mut self.old_bios_fixes, t.old_bios_fixes, p) {
                    self.persist_config();
                }
                if luxus_checkbox(ui, &mut self.use_uefi_mbr_id, t.use_uefi_mbr_id, p) {
                    self.persist_config();
                }
                ui.add_space(3.0);
                let has_dev = self.selected_device().is_some();
                ui.horizontal(|ui| {
                    if luxus_button(ui, t.refresh_devices, 148.0, 24.0, false, p).clicked() {
                        self.trigger_device_refresh();
                    }
                    if ui
                        .add_enabled_ui(has_dev, |ui| {
                            luxus_button(ui, t.backup_usb_btn, 165.0, 24.0, false, p)
                        })
                        .inner
                        .clicked()
                    {
                        self.pick_backup_destination();
                    }
                    if ui
                        .add_enabled_ui(has_dev, |ui| {
                            luxus_button(ui, t.eject_usb_btn, 148.0, 24.0, false, p)
                        })
                        .inner
                        .clicked()
                    {
                        self.eject_selected_device();
                    }
                });
                ui.add_space(3.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled_ui(has_dev, |ui| {
                            luxus_button(ui, t.wipe_usb_btn, 180.0, 24.0, false, p)
                        })
                        .inner
                        .clicked()
                    {
                        self.confirm_is_wipe = true;
                        self.confirm_open = true;
                    }
                });
            }

            // ================================================================
            // SECTION 2: Opciones de formateo
            // ================================================================
            ui.add_space(5.0);
            section_header(ui, t.format_options, p);
            ui.add_space(2.0);

            // Etiqueta de volumen
            field_label(ui, t.volume_label, p);
            ui.add_sized(
                Vec2::new(full_w, 24.0),
                egui::TextEdit::singleline(&mut self.volume_label)
                    .vertical_align(Align::Center)
                    .margin(egui::Margin::symmetric(8.0, 3.0)),
            );
            ui.add_space(4.0);

            // Sistema de archivos | Tamaño del clúster
            let vol_bytes = self.selected_volume_bytes();
            let def_cluster = default_cluster_for_fs(
                self.filesystem.is_fat32(),
                self.filesystem == FileSystemKind::Exfat,
                vol_bytes,
            );

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = col_gap;
                ui.vertical(|ui| {
                    ui.set_width(left_col_w);
                    field_label(ui, t.file_system, p);
                    let fs_options = self.available_filesystems();
                    let prev_fs = self.filesystem;
                    egui::ComboBox::from_id_salt("luxus_fs")
                        .selected_text(self.filesystem.label())
                        .width(left_col_w)
                        .show_ui(ui, |ui| {
                            for fs in fs_options {
                                ui.selectable_value(&mut self.filesystem, fs, fs.label());
                            }
                        });
                    if self.filesystem != prev_fs {
                        self.cluster_size = 0;
                    }
                });

                ui.vertical(|ui| {
                    ui.set_width(right_col_w);
                    field_label(ui, t.cluster_size, p);
                    let active_cluster = if self.cluster_size == 0 {
                        def_cluster
                    } else {
                        self.cluster_size
                    };
                    let cluster_txt = format_cluster_label(
                        active_cluster,
                        if self.cluster_size == 0 || self.cluster_size == def_cluster {
                            Some(t.default_suffix)
                        } else {
                            None
                        },
                    );
                    let allowed_clusters = allowed_clusters_for_mask(self.fs_cluster_mask());
                    egui::ComboBox::from_id_salt("luxus_cluster")
                        .selected_text(cluster_txt)
                        .width(right_col_w)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.cluster_size,
                                0,
                                format_cluster_label(def_cluster, Some(t.default_suffix)),
                            );
                            for c in allowed_clusters {
                                if c != def_cluster {
                                    ui.selectable_value(
                                        &mut self.cluster_size,
                                        c,
                                        format_cluster_label(c, None),
                                    );
                                }
                            }
                        });
                });
            });

            ui.add_space(4.0);

            // Expandable: Ocultar/Mostrar opciones avanzadas de formato
            let adv_fmt_label = if self.show_advanced_format {
                t.hide_adv_format
            } else {
                t.show_adv_format
            };
            if chevron_toggle(ui, self.show_advanced_format, adv_fmt_label, p) {
                self.show_advanced_format = !self.show_advanced_format;
            }

            if self.show_advanced_format {
                ui.add_space(2.0);
                luxus_checkbox(ui, &mut self.quick_format, t.quick_format, p);
                luxus_checkbox(ui, &mut self.extended_label_icon, t.extended_label_icon, p);

                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let pass_w = (right_col_w - 16.0).max(150.0);
                    let left_w = (full_w - pass_w - 6.0).max(220.0);
                    ui.allocate_ui_with_layout(
                        Vec2::new(left_w, 24.0),
                        egui::Layout::left_to_right(Align::Center),
                        |ui| {
                            luxus_checkbox(ui, &mut self.check_bad_blocks, t.check_bad_blocks, p);
                        },
                    );
                    let pass_label = match self.bad_blocks_passes {
                        1 => t.pass_1,
                        2 => t.pass_2,
                        3 => t.pass_3,
                        _ => t.pass_4,
                    };
                    egui::ComboBox::from_id_salt("luxus_passes")
                        .selected_text(pass_label)
                        .width(pass_w)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.bad_blocks_passes, 1, t.pass_1);
                            ui.selectable_value(&mut self.bad_blocks_passes, 2, t.pass_2);
                            ui.selectable_value(&mut self.bad_blocks_passes, 3, t.pass_3);
                            ui.selectable_value(&mut self.bad_blocks_passes, 4, t.pass_4);
                        });
                });
            }
        });

        // ====================================================================
        // SECTION 3: Estado + Green Progress Bar
        // ====================================================================
        ui.add_space(6.0);
        section_header(ui, t.status_heading, p);
        ui.add_space(5.0);

        let (progress, status_msg, is_running, done, cancelled, has_err) = {
            let j = self.job.lock().unwrap();
            (
                j.progress,
                j.status.clone(),
                j.running,
                j.done,
                j.cancelled,
                j.error.is_some(),
            )
        };

        let (bar_fill_frac, bar_color, bar_text) = if is_running {
            (progress.clamp(0.0, 1.0), p.status_green, status_msg)
        } else if has_err {
            (1.0, p.status_red, status_msg)
        } else if cancelled {
            (
                1.0,
                Color32::from_rgb(200, 130, 20),
                t.status_cancelled.to_string(),
            )
        } else if done {
            (1.0, p.status_green, t.status_done.to_string())
        } else {
            (1.0, p.status_green, t.status_ready.to_string())
        };

        draw_status_bar(ui, full_w, 26.0, bar_fill_frac, bar_color, &bar_text, p);

        // ====================================================================
        // BOTTOM ACTION ROW: 4 blue vector icons | EMPEZAR + CERRAR
        // ====================================================================
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;

            if toolbar_icon_btn(ui, ToolbarIcon::Globe, p)
                .on_hover_text(t.language_lbl)
                .clicked()
            {
                self.lang_popup_open = !self.lang_popup_open;
            }
            if toolbar_icon_btn(ui, ToolbarIcon::Info, p)
                .on_hover_text(t.about_title)
                .clicked()
            {
                self.about_open = !self.about_open;
            }
            if toolbar_icon_btn(ui, ToolbarIcon::Settings, p)
                .on_hover_text(t.settings_title)
                .clicked()
            {
                self.settings_open = !self.settings_open;
            }
            if toolbar_icon_btn(ui, ToolbarIcon::Log, p)
                .on_hover_text(t.log_title)
                .clicked()
            {
                self.log_open = !self.log_open;
            }

            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                if ui
                    .add_enabled_ui(!is_running, |ui| {
                        luxus_button(ui, t.close_btn, 104.0, 28.0, false, p)
                    })
                    .inner
                    .clicked()
                {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }

                if is_running {
                    if luxus_button(ui, t.cancel_btn, 104.0, 28.0, true, p).clicked() {
                        self.job_cancel.store(true, Ordering::SeqCst);
                    }
                } else {
                    let enabled = self.can_start();
                    if ui
                        .add_enabled_ui(enabled, |ui| {
                            luxus_button(ui, t.start_btn, 104.0, 28.0, enabled, p)
                        })
                        .inner
                        .clicked()
                    {
                        self.on_start_clicked();
                    }
                }
            });
        });
    }

    fn draw_modals(&mut self, ctx: &egui::Context, p: &ThemePalette) {
        let t = tr(self.lang);

        // 0. Windows User Experience customization dialog (`autounattend.xml`)
        if self.win_ux_open {
            let mut open = self.win_ux_open;
            let mut close_ok = false;
            let mut close_cancel = false;
            egui::Window::new(t.win_ux_title)
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .default_width(440.0)
                .show(ctx, |ui| {
                    ui.label(egui::RichText::new(t.win_ux_intro).strong());
                    ui.add_space(6.0);
                    luxus_checkbox(
                        ui,
                        &mut self.windows_ux.bypass_tpm_ram_secureboot,
                        t.win_ux_bypass_tpm,
                        p,
                    );
                    ui.add_space(3.0);
                    luxus_checkbox(
                        ui,
                        &mut self.windows_ux.bypass_online_account,
                        t.win_ux_bypass_ms,
                        p,
                    );
                    ui.add_space(3.0);
                    luxus_checkbox(
                        ui,
                        &mut self.windows_ux.create_local_account,
                        t.win_ux_local_user,
                        p,
                    );
                    if self.windows_ux.create_local_account {
                        ui.horizontal(|ui| {
                            ui.add_space(22.0);
                            ui.add_sized(
                                Vec2::new(260.0, 24.0),
                                egui::TextEdit::singleline(&mut self.windows_ux.local_username),
                            );
                        });
                    }
                    ui.add_space(3.0);
                    luxus_checkbox(
                        ui,
                        &mut self.windows_ux.set_regional_options,
                        t.win_ux_regional,
                        p,
                    );
                    ui.add_space(3.0);
                    luxus_checkbox(
                        ui,
                        &mut self.windows_ux.disable_data_collection,
                        t.win_ux_privacy,
                        p,
                    );
                    ui.add_space(3.0);
                    luxus_checkbox(
                        ui,
                        &mut self.windows_ux.disable_bitlocker,
                        t.win_ux_bitlocker,
                        p,
                    );
                    ui.add_space(3.0);
                    luxus_checkbox(
                        ui,
                        &mut self.windows_ux.disable_internal_disks,
                        t.win_ux_internal_disks,
                        p,
                    );
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if luxus_button(ui, t.win_ux_export_xml, 118.0, 26.0, false, p).clicked() {
                            if let Some(dest) = rfd::FileDialog::new()
                                .set_file_name("autounattend.xml")
                                .add_filter("Windows Unattend XML", &["xml"])
                                .save_file()
                            {
                                let xml = generate_autounattend_xml(
                                    &self.windows_ux,
                                    &detect_windows_locale(),
                                );
                                if fs::write(&dest, xml).is_ok() {
                                    self.log_msg(format!(
                                        "Exportado autounattend.xml a {}",
                                        dest.display()
                                    ));
                                }
                            }
                        }
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if luxus_button(ui, t.confirm_cancel, 96.0, 26.0, false, p).clicked() {
                                close_cancel = true;
                            }
                            if luxus_button(ui, t.confirm_ok, 96.0, 26.0, true, p).clicked() {
                                close_ok = true;
                            }
                        });
                    });
                });
            if close_ok {
                self.persist_config();
                self.win_ux_open = false;
                if self.win_ux_pending_start {
                    self.win_ux_pending_start = false;
                    self.confirm_open = true;
                }
            } else if close_cancel || !open {
                self.win_ux_open = false;
                self.win_ux_pending_start = false;
            }
        }

        // 1. Confirmation dialog (Write or Zero-Wipe)
        if self.confirm_open {
            let dev_txt = self
                .selected_device()
                .map(|d| d.display_label())
                .unwrap_or_default();
            egui::Window::new(t.confirm_title)
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .default_width(410.0)
                .show(ctx, |ui| {
                    ui.colored_label(p.status_red, egui::RichText::new(t.confirm_warn).strong());
                    ui.add_space(6.0);
                    ui.label(format!("{}: {}", t.device, dev_txt));
                    if self.confirm_is_wipe {
                        ui.label(t.wipe_usb_btn);
                    } else {
                        if !self.image_display_name.is_empty()
                            && matches!(self.boot_mode, BootMode::IsoImage | BootMode::DdImage)
                        {
                            ui.label(format!("ISO/IMG: {}", self.image_display_name));
                        }
                        if self.is_windows_install_selected() && self.enable_uefi_ntfs {
                            ui.label("UEFI:NTFS + autounattend.xml");
                        }
                    }
                    ui.add_space(6.0);
                    ui.label(t.confirm_detail);
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if luxus_button(ui, t.confirm_cancel, 96.0, 26.0, false, p).clicked() {
                                self.confirm_open = false;
                                self.confirm_is_wipe = false;
                            }
                            if luxus_button(ui, t.confirm_ok, 96.0, 26.0, true, p).clicked() {
                                self.confirm_open = false;
                                if self.confirm_is_wipe {
                                    self.confirm_is_wipe = false;
                                    if let Some(dev) = self.selected_device().cloned() {
                                        spawn_wipe_job(
                                            self.job.clone(),
                                            self.job_cancel.clone(),
                                            WipeSpec { device: dev },
                                        );
                                    }
                                } else {
                                    self.start_write_job();
                                }
                            }
                        });
                    });
                });
        }

        // 2. Checksums dialog (with Expected Hash Matcher!)
        let mut open_chk = self.checksum_state.lock().unwrap().open;
        if open_chk {
            egui::Window::new(t.checksum_title)
                .open(&mut open_chk)
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .default_width(440.0)
                .show(ctx, |ui| {
                    let mut st = self.checksum_state.lock().unwrap();
                    if st.running {
                        ui.label(t.checksum_computing);
                        ui.add(egui::ProgressBar::new(st.progress).show_percentage());
                        ctx.request_repaint_after(Duration::from_millis(50));
                    } else if let Some(ref e) = st.error {
                        ui.colored_label(p.status_red, e);
                    } else if let Some(res) = st.result.clone() {
                        for (name, val) in [
                            ("MD5", &res.md5),
                            ("SHA1", &res.sha1),
                            ("SHA256", &res.sha256),
                            ("SHA512", &res.sha512),
                        ] {
                            ui.label(egui::RichText::new(name).strong().size(12.0));
                            ui.horizontal(|ui| {
                                let mut txt = val.clone();
                                ui.add_sized(
                                    Vec2::new(345.0, 22.0),
                                    egui::TextEdit::singleline(&mut txt)
                                        .font(egui::TextStyle::Monospace),
                                );
                                if ui.button(t.checksum_copy).clicked() {
                                    ui.output_mut(|o| o.copied_text = val.clone());
                                }
                            });
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                Vec2::new(330.0, 22.0),
                                egui::TextEdit::singleline(&mut st.expected_hash)
                                    .hint_text(t.checksum_compare_hint)
                                    .font(egui::TextStyle::Monospace),
                            );
                            let exp = st.expected_hash.trim().to_lowercase();
                            if !exp.is_empty() {
                                let matched = exp == res.md5
                                    || exp == res.sha1
                                    || exp == res.sha256
                                    || exp == res.sha512;
                                if matched {
                                    ui.colored_label(
                                        p.status_green,
                                        egui::RichText::new(t.checksum_match).strong(),
                                    );
                                } else {
                                    ui.colored_label(
                                        p.status_red,
                                        egui::RichText::new(t.checksum_mismatch).strong(),
                                    );
                                }
                            }
                        });
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "{} {} {:.2} s",
                                human_size(res.bytes_hashed),
                                t.checksum_elapsed_word,
                                res.elapsed_secs
                            ))
                            .color(p.text_muted)
                            .size(11.5),
                        );
                    }
                });
            if !open_chk {
                self.checksum_cancel.store(true, Ordering::SeqCst);
            }
            self.checksum_state.lock().unwrap().open = open_chk;
        }

        // 3. ISO Downloader dialog
        let mut open_dl = self.download_state.lock().unwrap().open;
        let mut completed_dl: Option<PathBuf> = None;
        if open_dl {
            egui::Window::new(t.download_title)
                .open(&mut open_dl)
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .default_width(430.0)
                .show(ctx, |ui| {
                    let mut st = self.download_state.lock().unwrap();
                    let cur_item = ISO_CATALOG[st.selected_idx.min(ISO_CATALOG.len() - 1)];
                    egui::ComboBox::from_id_salt("luxus_dl_iso")
                        .selected_text(format!("{} — {}", cur_item.name, cur_item.edition))
                        .width(400.0)
                        .show_ui(ui, |ui| {
                            for (idx, item) in ISO_CATALOG.iter().enumerate() {
                                ui.selectable_value(
                                    &mut st.selected_idx,
                                    idx,
                                    format!("{} — {} ({})", item.name, item.edition, item.arch),
                                );
                            }
                        });
                    ui.add_space(6.0);
                    let chosen = ISO_CATALOG[st.selected_idx.min(ISO_CATALOG.len() - 1)];
                    if st.running {
                        ui.label(&st.status);
                        ui.add(egui::ProgressBar::new(st.progress).show_percentage());
                        ctx.request_repaint_after(Duration::from_millis(80));
                        if ui.button(t.cancel_btn).clicked() {
                            self.download_cancel.store(true, Ordering::SeqCst);
                        }
                    } else {
                        if let Some(ref e) = st.error {
                            ui.colored_label(p.status_red, e);
                        } else if !st.status.is_empty() {
                            ui.label(&st.status);
                        }
                        if let Some(done_path) = st.completed_path.take() {
                            completed_dl = Some(done_path);
                        }
                        let btn_lbl = if chosen.is_web_portal {
                            t.download_open_web
                        } else {
                            t.download_start
                        };
                        if luxus_button(ui, btn_lbl, 170.0, 26.0, true, p).clicked() {
                            if chosen.is_web_portal {
                                drop(st);
                                start_iso_download(
                                    self.download_state.clone(),
                                    self.download_cancel.clone(),
                                    chosen,
                                    PathBuf::from(chosen.filename),
                                );
                                return;
                            } else if let Some(dest) = rfd::FileDialog::new()
                                .set_file_name(chosen.filename)
                                .save_file()
                            {
                                drop(st);
                                start_iso_download(
                                    self.download_state.clone(),
                                    self.download_cancel.clone(),
                                    chosen,
                                    dest,
                                );
                                return;
                            }
                        }
                    }
                });
            if !open_dl {
                self.download_cancel.store(true, Ordering::SeqCst);
            }
            self.download_state.lock().unwrap().open = open_dl;
        }
        if let Some(dl_path) = completed_dl {
            self.download_state.lock().unwrap().open = false;
            self.load_image_path(dl_path);
        }

        // 4. Log window
        if self.log_open {
            let mut open = self.log_open;
            egui::Window::new(t.log_title)
                .open(&mut open)
                .resizable(true)
                .default_size(Vec2::new(460.0, 300.0))
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button(t.log_save).clicked() {
                            if let Some(path) = rfd::FileDialog::new()
                                .set_file_name("luxus.log")
                                .save_file()
                            {
                                let content: String = self
                                    .job
                                    .lock()
                                    .unwrap()
                                    .logs
                                    .iter()
                                    .map(|l| format!("[{}] {}\n", l.ts, l.msg))
                                    .collect();
                                let _ = fs::write(path, content);
                            }
                        }
                        if ui.button(t.log_clear).clicked() {
                            self.job.lock().unwrap().logs.clear();
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            let logs = self.job.lock().unwrap().logs.clone();
                            if logs.is_empty() {
                                ui.label(egui::RichText::new(t.log_empty).color(p.text_muted));
                            }
                            for l in logs {
                                ui.label(
                                    egui::RichText::new(format!("[{}] {}", l.ts, l.msg))
                                        .monospace()
                                        .size(11.5),
                                );
                            }
                        });
                });
            self.log_open = open;
        }

        // 5. Settings dialog (Language, Automatic Dark Mode, SHA-256 Verify, NTFS Compression, Buffer)
        if self.settings_open {
            let mut open = self.settings_open;
            egui::Window::new(t.settings_title)
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .default_width(400.0)
                .show(ctx, |ui| {
                    let mut cfg_dirty = false;
                    ui.horizontal(|ui| {
                        ui.label(t.language_lbl);
                        let lang_txt = match self.lang_override {
                            None => format!("{} ({})", t.language_auto, self.lang.native_name()),
                            Some(l) => l.native_name().to_string(),
                        };
                        egui::ComboBox::from_id_salt("luxus_lang_mode")
                            .selected_text(lang_txt)
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(self.lang_override.is_none(), t.language_auto)
                                    .clicked()
                                {
                                    self.lang_override = None;
                                    self.lang = detect_system_language();
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
                                        "{} {}",
                                        tr(self.lang).window_title,
                                        env!("CARGO_PKG_VERSION")
                                    )));
                                    cfg_dirty = true;
                                }
                                for l in Lang::ALL {
                                    if ui
                                        .selectable_label(
                                            self.lang_override == Some(l),
                                            l.native_name(),
                                        )
                                        .clicked()
                                    {
                                        self.lang_override = Some(l);
                                        self.lang = l;
                                        ctx.send_viewport_cmd(egui::ViewportCommand::Title(
                                            format!(
                                                "{} {}",
                                                tr(self.lang).window_title,
                                                env!("CARGO_PKG_VERSION")
                                            ),
                                        ));
                                        cfg_dirty = true;
                                    }
                                }
                            });
                    });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(t.theme_lbl);
                        let prev_theme = self.theme_mode;
                        let theme_txt = match self.theme_mode {
                            ThemeMode::Auto => t.theme_auto,
                            ThemeMode::Light => t.theme_light,
                            ThemeMode::Dark => t.theme_dark,
                        };
                        egui::ComboBox::from_id_salt("luxus_theme_mode")
                            .selected_text(theme_txt)
                            .width(180.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.theme_mode,
                                    ThemeMode::Auto,
                                    t.theme_auto,
                                );
                                ui.selectable_value(
                                    &mut self.theme_mode,
                                    ThemeMode::Light,
                                    t.theme_light,
                                );
                                ui.selectable_value(
                                    &mut self.theme_mode,
                                    ThemeMode::Dark,
                                    t.theme_dark,
                                );
                            });
                        if self.theme_mode != prev_theme {
                            cfg_dirty = true;
                        }
                    });
                    ui.add_space(6.0);
                    if luxus_checkbox(ui, &mut self.verify_after_write, t.verify_after_write, p) {
                        cfg_dirty = true;
                    }
                    if luxus_checkbox(ui, &mut self.ntfs_compression, t.ntfs_compression, p) {
                        cfg_dirty = true;
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(t.buffer_size_lbl);
                        let prev_buf = self.buffer_mb;
                        egui::ComboBox::from_id_salt("luxus_buf_mb")
                            .selected_text(format!("{} MiB", self.buffer_mb))
                            .width(100.0)
                            .show_ui(ui, |ui| {
                                for mb in [1usize, 2, 4, 8, 16] {
                                    ui.selectable_value(
                                        &mut self.buffer_mb,
                                        mb,
                                        format!("{} MiB", mb),
                                    );
                                }
                            });
                        if self.buffer_mb != prev_buf {
                            cfg_dirty = true;
                        }
                    });
                    if cfg_dirty {
                        self.persist_config();
                    }
                });
            self.settings_open = open;
        }

        // 6. About, Shortcuts & Cross-Distro Diagnostics
        if self.about_open {
            let mut open = self.about_open;
            egui::Window::new(t.about_title)
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .default_width(440.0)
                .show(ctx, |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  (v{})",
                            t.about_heading,
                            env!("CARGO_PKG_VERSION")
                        ))
                        .strong()
                        .size(14.0),
                    );
                    ui.label(t.about_desc);
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(t.shortcuts_hdr).strong());
                    ui.label(
                        egui::RichText::new(t.shortcuts_body)
                            .color(p.text_muted)
                            .size(11.5),
                    );
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(t.about_tools_hdr).strong());
                    egui::ScrollArea::vertical()
                        .max_height(190.0)
                        .show(ui, |ui| {
                            egui::Grid::new("tools_grid").spacing([12.0, 3.0]).show(ui, |ui| {
                                for st in probe_system_tools() {
                                    let badge = if st.available { "OK" } else { "—" };
                                    let col = if st.available {
                                        p.status_green
                                    } else {
                                        p.text_muted
                                    };
                                    ui.colored_label(col, egui::RichText::new(badge).strong());
                                    ui.label(egui::RichText::new(st.name).monospace());
                                    ui.label(egui::RichText::new(st.purpose).color(p.text_muted));
                                    ui.end_row();
                                }
                            });
                        });
                });
            self.about_open = open;
        }

        // 7. Target system help
        if self.help_target_open {
            let mut open = self.help_target_open;
            egui::Window::new(t.help_target_title)
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .default_width(400.0)
                .show(ctx, |ui| {
                    ui.label(t.help_target_body);
                });
            self.help_target_open = open;
        }

        // 8. Language picker popup (bottom-left Globe icon)
        if self.lang_popup_open {
            let mut open = self.lang_popup_open;
            let mut close_now = false;
            egui::Window::new(t.language_lbl)
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::LEFT_BOTTOM, Vec2::new(14.0, -68.0))
                .default_width(210.0)
                .show(ctx, |ui| {
                    let auto_label = format!(
                        "{} ({})",
                        t.language_auto,
                        detect_system_language().native_name()
                    );
                    if ui
                        .selectable_label(self.lang_override.is_none(), auto_label)
                        .clicked()
                    {
                        self.lang_override = None;
                        self.lang = detect_system_language();
                        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
                            "{} {}",
                            tr(self.lang).window_title,
                            env!("CARGO_PKG_VERSION")
                        )));
                        self.persist_config();
                        close_now = true;
                    }
                    ui.separator();
                    for l in Lang::ALL {
                        let label = format!("{} ({})", l.native_name(), l.code().to_uppercase());
                        if ui
                            .selectable_label(self.lang_override == Some(l), label)
                            .clicked()
                        {
                            self.lang_override = Some(l);
                            self.lang = l;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
                                "{} {}",
                                tr(self.lang).window_title,
                                env!("CARGO_PKG_VERSION")
                            )));
                            self.persist_config();
                            close_now = true;
                        }
                    }
                });
            self.lang_popup_open = open && !close_now;
        }
    }
}

impl eframe::App for LuxusApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sync_theme(ctx);
        let p = palette_for(self.applied_dark);

        // Handle drag-and-drop ISO/IMG files and power-user keyboard shortcuts
        let mut dropped_path: Option<PathBuf> = None;
        let mut shortcut_refresh = false;
        let mut shortcut_log = false;
        let mut shortcut_hdd = false;
        let mut shortcut_dual = false;
        let mut shortcut_theme = false;
        let mut shortcut_badblocks = false;
        let mut shortcut_backup = false;
        let mut shortcut_wipe = false;
        ctx.input(|i| {
            if let Some(file) = i.raw.dropped_files.first() {
                if let Some(ref path) = file.path {
                    dropped_path = Some(path.clone());
                }
            }
            if i.key_pressed(egui::Key::F5) {
                shortcut_refresh = true;
            }
            if i.modifiers.ctrl && i.key_pressed(egui::Key::L) {
                shortcut_log = true;
            }
            if i.modifiers.alt && i.key_pressed(egui::Key::F) {
                shortcut_hdd = true;
            }
            if i.modifiers.alt && i.key_pressed(egui::Key::E) {
                shortcut_dual = true;
            }
            if i.modifiers.alt && i.key_pressed(egui::Key::D) {
                shortcut_theme = true;
            }
            if i.modifiers.alt && i.key_pressed(egui::Key::B) {
                shortcut_badblocks = true;
            }
            if i.modifiers.alt && i.key_pressed(egui::Key::S) {
                shortcut_backup = true;
            }
            if i.modifiers.alt && i.key_pressed(egui::Key::Z) {
                shortcut_wipe = true;
            }
        });
        let busy = self.job.lock().unwrap().running;
        if let Some(dp) = dropped_path {
            if !busy {
                self.load_image_path(dp);
            }
        }
        if shortcut_refresh && !busy {
            self.trigger_device_refresh();
        }
        if shortcut_log {
            self.log_open = !self.log_open;
        }
        if shortcut_hdd && !busy {
            self.list_usb_hard_drives = !self.list_usb_hard_drives;
            self.persist_config();
            self.log_msg(format!(
                "Modo discos duros USB (Alt+F): {}",
                if self.list_usb_hard_drives { "ON" } else { "OFF" }
            ));
            self.trigger_device_refresh();
        }
        if shortcut_dual && !busy {
            self.partition_scheme = PartitionScheme::Mbr;
            self.target_system = TargetSystem::BiosUefi;
            self.enable_uefi_ntfs = true;
            self.old_bios_fixes = !self.old_bios_fixes;
            self.persist_config();
            self.log_msg(format!(
                "Modo dual UEFI/BIOS (Alt+E): MBR + BIOS/UEFI + arreglos BIOS={}",
                self.old_bios_fixes
            ));
        }
        if shortcut_theme {
            self.theme_mode = if self.applied_dark {
                ThemeMode::Light
            } else {
                ThemeMode::Dark
            };
            self.persist_config();
            self.sync_theme(ctx);
        }
        if shortcut_badblocks && !busy {
            self.check_bad_blocks = !self.check_bad_blocks;
            self.log_msg(format!(
                "Prueba de bloques dañados (Alt+B): {}",
                if self.check_bad_blocks { "ON" } else { "OFF" }
            ));
        }
        if shortcut_backup && !busy && self.selected_device().is_some() {
            self.pick_backup_destination();
        }
        if shortcut_wipe && !busy && self.selected_device().is_some() {
            self.confirm_is_wipe = true;
            self.confirm_open = true;
        }

        self.poll_device_refresh();
        if self.last_device_refresh.elapsed() > Duration::from_secs(5)
            && self.device_refresh_rx.is_none()
            && !self.job.lock().unwrap().running
        {
            self.trigger_device_refresh();
        }

        let (is_running, speed_text, elapsed_secs) = {
            let mut j = self.job.lock().unwrap();
            if j.running {
                if let Some(st) = j.started_at {
                    j.elapsed_secs = st.elapsed().as_secs();
                }
            }
            (j.running, j.speed_text.clone(), j.elapsed_secs)
        };
        if is_running {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if self.theme_mode == ThemeMode::Auto && self.screenshot_path.is_none() {
            ctx.request_repaint_after(Duration::from_secs(3));
        }

        let t = tr(self.lang);
        let dev_count = self.devices.len();
        let left_status = if is_running && !speed_text.is_empty() {
            speed_text
        } else if dev_count == 1 {
            t.device_found_1.to_string()
        } else if dev_count == 0 {
            t.devices_found_0.to_string()
        } else {
            format!("{} {}", dev_count, t.devices_found_n)
        };
        let right_status = if is_running || elapsed_secs > 0 {
            format_elapsed(elapsed_secs)
        } else {
            String::new()
        };

        egui::TopBottomPanel::bottom("luxus_statusbar")
            .exact_height(26.0)
            .frame(
                egui::Frame::none()
                    .fill(p.bg_statusbar)
                    .stroke(stroke(1.0, p.border_subtle))
                    .inner_margin(egui::Margin::symmetric(10.0, 4.0)),
            )
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                let split_x = rect.right() - 78.0;
                ui.painter().line_segment(
                    [
                        Pos2::new(split_x, rect.top() - 2.0),
                        Pos2::new(split_x, rect.bottom() + 2.0),
                    ],
                    stroke(1.0, p.split_divider),
                );
                ui.horizontal_centered(|ui| {
                    ui.label(
                        egui::RichText::new(left_status)
                            .size(12.5)
                            .color(p.text_primary),
                    );
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(right_status)
                                .size(12.0)
                                .monospace()
                                .color(p.text_primary),
                        );
                    });
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(p.bg_panel)
                    .inner_margin(egui::Margin::symmetric(14.0, 8.0)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.draw_main_content(ui, p);
                    });
            });

        self.draw_modals(ctx, p);

        if self.screenshot_path.is_some() {
            self.handle_screenshot(ctx);
        }
    }
}

// ============================================================================
// Custom crisp vector widgets painted with egui::Painter
// ============================================================================

fn section_header(ui: &mut egui::Ui, title: &str, p: &ThemePalette) {
    let desired = Vec2::new(ui.available_width(), 24.0);
    let (rect, _) = ui.allocate_exact_size(desired, Sense::hover());
    let painter = ui.painter();
    let galley = painter.layout_no_wrap(
        title.to_string(),
        FontId::proportional(19.0),
        p.text_heading,
    );
    let text_pos = Pos2::new(rect.left(), rect.center().y - galley.size().y * 0.52);
    let line_x0 = (text_pos.x + galley.size().x + 10.0).min(rect.right());
    let line_y = rect.center().y + 2.0;
    painter.galley(text_pos, galley, p.text_heading);
    if line_x0 < rect.right() {
        painter.line_segment(
            [Pos2::new(line_x0, line_y), Pos2::new(rect.right(), line_y)],
            stroke(1.4, p.border_divider),
        );
    }
}

fn field_label(ui: &mut egui::Ui, text: &str, p: &ThemePalette) {
    ui.label(egui::RichText::new(text).size(12.5).color(p.text_primary));
}

fn chevron_toggle(ui: &mut egui::Ui, expanded: bool, text: &str, p: &ThemePalette) -> bool {
    let desired = Vec2::new(ui.available_width(), 20.0);
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
    let painter = ui.painter();
    let cx = rect.left() + 6.0;
    let cy = rect.center().y;
    let st = stroke(
        1.5,
        if resp.hovered() {
            p.icon_blue
        } else {
            p.text_primary
        },
    );
    if expanded {
        painter.line_segment([Pos2::new(cx - 4.0, cy + 2.0), Pos2::new(cx, cy - 2.5)], st);
        painter.line_segment([Pos2::new(cx, cy - 2.5), Pos2::new(cx + 4.0, cy + 2.0)], st);
    } else {
        painter.line_segment([Pos2::new(cx - 4.0, cy - 2.0), Pos2::new(cx, cy + 2.5)], st);
        painter.line_segment([Pos2::new(cx, cy + 2.5), Pos2::new(cx + 4.0, cy - 2.0)], st);
    }
    let col = if resp.hovered() {
        p.icon_blue
    } else {
        p.text_primary
    };
    painter.text(
        Pos2::new(rect.left() + 18.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        FontId::proportional(13.0),
        col,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

fn luxus_checkbox(ui: &mut egui::Ui, checked: &mut bool, text: &str, p: &ThemePalette) -> bool {
    let desired = Vec2::new(ui.available_width(), 19.0);
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
    let clicked = resp.clicked();
    if clicked {
        *checked = !*checked;
    }
    let painter = ui.painter();
    let box_rect = Rect::from_center_size(
        Pos2::new(rect.left() + 8.0, rect.center().y),
        Vec2::splat(15.0),
    );
    if *checked {
        let fill = if resp.hovered() {
            p.accent_blue_hover
        } else {
            p.accent_blue
        };
        painter.rect_filled(box_rect, Rounding::same(2.5), fill);
        let c = box_rect.center();
        let ck = stroke(1.8, Color32::WHITE);
        painter.line_segment(
            [Pos2::new(c.x - 4.0, c.y + 0.2), Pos2::new(c.x - 1.2, c.y + 3.0)],
            ck,
        );
        painter.line_segment(
            [Pos2::new(c.x - 1.2, c.y + 3.0), Pos2::new(c.x + 4.2, c.y - 3.0)],
            ck,
        );
    } else {
        let border = if resp.hovered() {
            p.accent_blue_hover
        } else {
            p.checkbox_border
        };
        painter.rect(box_rect, Rounding::same(2.5), p.bg_ctrl, stroke(1.0, border));
    }
    painter.text(
        Pos2::new(box_rect.right() + 7.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        FontId::proportional(13.0),
        p.text_primary,
    );
    clicked
}

fn icon_checksum_button(ui: &mut egui::Ui, enabled: bool, p: &ThemePalette) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
    let painter = ui.painter();
    let center = rect.center();
    let color = if !enabled {
        p.text_disabled
    } else if resp.hovered() {
        p.accent_blue_hover
    } else {
        p.icon_blue
    };
    if enabled && resp.hovered() {
        painter.circle_filled(center, 11.5, p.bg_ctrl_hover);
    }
    painter.circle_stroke(center, 9.2, stroke(1.4, color));
    let ck = stroke(1.5, color);
    painter.line_segment(
        [
            Pos2::new(center.x - 4.0, center.y + 0.3),
            Pos2::new(center.x - 1.0, center.y + 3.2),
        ],
        ck,
    );
    painter.line_segment(
        [
            Pos2::new(center.x - 1.0, center.y + 3.2),
            Pos2::new(center.x + 4.5, center.y - 3.2),
        ],
        ck,
    );
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

fn split_select_button(
    ui: &mut egui::Ui,
    label: &str,
    width: f32,
    height: f32,
    p: &ThemePalette,
) -> (bool, bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let split_x = rect.right() - 20.0;
    let main_rect = Rect::from_min_max(rect.min, Pos2::new(split_x, rect.max.y));
    let drop_rect = Rect::from_min_max(Pos2::new(split_x, rect.min.y), rect.max);

    let main_resp = ui.interact(main_rect, ui.id().with("split_main"), Sense::click());
    let drop_resp = ui.interact(drop_rect, ui.id().with("split_drop"), Sense::click());

    let painter = ui.painter();
    let any_hover = main_resp.hovered() || drop_resp.hovered();
    let bg = if main_resp.is_pointer_button_down_on() || drop_resp.is_pointer_button_down_on() {
        p.bg_ctrl_active
    } else if any_hover {
        p.bg_ctrl_hover
    } else {
        p.bg_ctrl
    };
    let border = if any_hover {
        p.accent_blue_hover
    } else {
        p.border_btn
    };

    painter.rect(rect, Rounding::same(2.0), bg, stroke(1.0, border));
    painter.line_segment(
        [
            Pos2::new(split_x, rect.top() + 3.0),
            Pos2::new(split_x, rect.bottom() - 3.0),
        ],
        stroke(1.0, p.border_ctrl),
    );
    painter.text(
        main_rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(12.5),
        p.text_primary,
    );

    let tc = drop_rect.center();
    painter.add(egui::Shape::convex_polygon(
        vec![
            Pos2::new(tc.x - 4.0, tc.y - 2.0),
            Pos2::new(tc.x + 4.0, tc.y - 2.0),
            Pos2::new(tc.x, tc.y + 3.0),
        ],
        p.text_primary,
        Stroke::NONE,
    ));

    (main_resp.clicked(), drop_resp.clicked())
}

fn luxus_button(
    ui: &mut egui::Ui,
    label: &str,
    width: f32,
    height: f32,
    primary_focus: bool,
    p: &ThemePalette,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    let painter = ui.painter();
    let enabled = ui.is_enabled();
    let bg = if !enabled {
        p.bg_btn_disabled
    } else if resp.is_pointer_button_down_on() {
        p.bg_ctrl_active
    } else if resp.hovered() {
        p.bg_ctrl_hover
    } else {
        p.bg_ctrl
    };
    let border = if !enabled {
        p.border_subtle
    } else if primary_focus || resp.hovered() {
        p.accent_blue_hover
    } else {
        p.border_btn
    };
    let stroke_w = if enabled && primary_focus { 1.4 } else { 1.0 };
    painter.rect(rect, Rounding::same(2.0), bg, stroke(stroke_w, border));
    if enabled && primary_focus {
        painter.rect_stroke(
            rect.shrink(2.5),
            Rounding::same(1.0),
            stroke(0.7, p.focus_inner),
        );
    }
    let text_col = if enabled {
        p.text_primary
    } else {
        p.text_disabled
    };
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(13.0),
        text_col,
    );
    resp
}

fn draw_status_bar(
    ui: &mut egui::Ui,
    width: f32,
    height: f32,
    fraction: f32,
    fill_color: Color32,
    text: &str,
    p: &ThemePalette,
) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let painter = ui.painter();

    painter.rect(
        rect,
        Rounding::same(1.0),
        p.status_track,
        stroke(1.0, p.status_border),
    );

    let inner = rect.shrink(1.0);
    let fill_w = (inner.width() * fraction.clamp(0.0, 1.0)).max(0.0);
    if fill_w > 0.5 {
        let fill_rect = Rect::from_min_size(inner.min, Vec2::new(fill_w, inner.height()));
        painter.rect_filled(fill_rect, Rounding::ZERO, fill_color);
    }

    let text_color = if fraction >= 0.45 || p.dark {
        Color32::WHITE
    } else {
        p.text_primary
    };
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(13.0),
        text_color,
    );
}

#[derive(Clone, Copy)]
enum ToolbarIcon {
    Globe,
    Info,
    Settings,
    Log,
}

fn toolbar_icon_btn(ui: &mut egui::Ui, icon: ToolbarIcon, p: &ThemePalette) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
    let painter = ui.painter();
    let c = rect.center();
    if resp.hovered() {
        painter.rect(
            rect,
            Rounding::same(3.0),
            p.bg_ctrl_hover,
            stroke(1.0, p.focus_inner),
        );
    }
    let col = if resp.hovered() {
        p.accent_blue_hover
    } else {
        p.icon_blue
    };
    let st = stroke(1.3, col);

    match icon {
        ToolbarIcon::Globe => {
            let r = 8.5;
            painter.circle_stroke(c, r, st);
            painter.line_segment([Pos2::new(c.x - r, c.y), Pos2::new(c.x + r, c.y)], st);
            painter.line_segment(
                [Pos2::new(c.x - 6.8, c.y - 4.2), Pos2::new(c.x + 6.8, c.y - 4.2)],
                stroke(1.0, col),
            );
            painter.line_segment(
                [Pos2::new(c.x - 6.8, c.y + 4.2), Pos2::new(c.x + 6.8, c.y + 4.2)],
                stroke(1.0, col),
            );
            painter.line_segment([Pos2::new(c.x, c.y - r), Pos2::new(c.x, c.y + r)], st);
            painter.line_segment(
                [Pos2::new(c.x - 3.5, c.y - 7.5), Pos2::new(c.x - 3.5, c.y + 7.5)],
                stroke(1.0, col),
            );
            painter.line_segment(
                [Pos2::new(c.x + 3.5, c.y - 7.5), Pos2::new(c.x + 3.5, c.y + 7.5)],
                stroke(1.0, col),
            );
        }
        ToolbarIcon::Info => {
            painter.circle_stroke(c, 8.5, st);
            painter.circle_filled(Pos2::new(c.x, c.y - 4.2), 1.1, col);
            painter.line_segment(
                [Pos2::new(c.x, c.y - 1.4), Pos2::new(c.x, c.y + 4.8)],
                stroke(1.7, col),
            );
        }
        ToolbarIcon::Settings => {
            for (dy, knob_x) in [(-5.0, 2.5), (0.0, -3.0), (5.0, 1.5)] {
                let y = c.y + dy;
                painter.line_segment([Pos2::new(c.x - 8.0, y), Pos2::new(c.x + 8.0, y)], st);
                painter.circle_filled(Pos2::new(c.x + knob_x, y), 2.3, p.bg_panel);
                painter.circle_stroke(Pos2::new(c.x + knob_x, y), 2.3, st);
            }
        }
        ToolbarIcon::Log => {
            let box_r = Rect::from_center_size(c, Vec2::new(16.0, 14.0));
            painter.rect_stroke(box_r, Rounding::same(1.5), st);
            painter.line_segment(
                [
                    Pos2::new(box_r.left(), box_r.top() + 4.0),
                    Pos2::new(box_r.right(), box_r.top() + 4.0),
                ],
                st,
            );
            for dy in [7.5, 10.5] {
                painter.line_segment(
                    [
                        Pos2::new(box_r.left() + 2.5, box_r.top() + dy),
                        Pos2::new(box_r.right() - 2.5, box_r.top() + dy),
                    ],
                    stroke(1.0, col),
                );
            }
            painter.line_segment(
                [
                    Pos2::new(box_r.left() + 5.5, box_r.top() + 4.0),
                    Pos2::new(box_r.left() + 5.5, box_r.bottom()),
                ],
                stroke(1.0, col),
            );
        }
    }

    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
