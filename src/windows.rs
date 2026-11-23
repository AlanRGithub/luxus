//! Windows bootable media support:
//! - Windows User Experience customization (`autounattend.xml` generator)
//! - UEFI:NTFS secondary EFI System Partition builder (cached upstream `uefi-ntfs.img`
//!   or 100% offline GRUB2 standalone UEFI-NTFS chainloader fallback)

use crate::devices::cmd_in_path;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WindowsUxOptions {
    /// Remove 4 GB+ RAM, Secure Boot and TPM 2.0 requirements (Windows 11)
    pub bypass_tpm_ram_secureboot: bool,
    /// Remove requirement for an online Microsoft account (BypassNRO)
    pub bypass_online_account: bool,
    /// Create a local administrator account automatically
    pub create_local_account: bool,
    /// Username for the local account
    pub local_username: String,
    /// Set Windows regional/locale options to the same values as this Linux user
    pub set_regional_options: bool,
    /// Disable data collection / skip privacy questions
    pub disable_data_collection: bool,
    /// Disable BitLocker automatic device encryption
    pub disable_bitlocker: bool,
    /// Prevent Windows from automatically mounting internal host disks (SanPolicy = 4)
    pub disable_internal_disks: bool,
}

impl Default for WindowsUxOptions {
    fn default() -> Self {
        let user = std::env::var("USER")
            .ok()
            .filter(|u| !u.trim().is_empty() && u != "root")
            .unwrap_or_else(|| "user".to_string());
        Self {
            bypass_tpm_ram_secureboot: true,
            bypass_online_account: true,
            create_local_account: false,
            local_username: user,
            set_regional_options: true,
            disable_data_collection: true,
            disable_bitlocker: true,
            disable_internal_disks: false,
        }
    }
}

impl WindowsUxOptions {
    pub fn any_enabled(&self) -> bool {
        self.bypass_tpm_ram_secureboot
            || self.bypass_online_account
            || (self.create_local_account && !self.local_username.trim().is_empty())
            || self.set_regional_options
            || self.disable_data_collection
            || self.disable_bitlocker
            || self.disable_internal_disks
    }
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Converts a POSIX locale like `es_MX.UTF-8` into a BCP-47 Windows locale tag like `es-MX`.
pub fn detect_windows_locale() -> String {
    for var in ["LC_ALL", "LANG", "LC_MESSAGES"] {
        if let Ok(val) = std::env::var(var) {
            if let Some(tag) = posix_to_bcp47(&val) {
                return tag;
            }
        }
    }
    "en-US".to_string()
}

pub fn posix_to_bcp47(raw: &str) -> Option<String> {
    let base = raw.trim().split(&['.', '@'][..]).next()?.trim();
    if base.is_empty() || base.eq_ignore_ascii_case("C") || base.eq_ignore_ascii_case("POSIX") {
        return None;
    }
    if let Some((lang, region)) = base.split_once('_') {
        if lang.len() == 2 && region.len() == 2 {
            return Some(format!(
                "{}-{}",
                lang.to_lowercase(),
                region.to_uppercase()
            ));
        }
    }
    match base.to_lowercase().as_str() {
        "es" => Some("es-ES".into()),
        "en" => Some("en-US".into()),
        "pt" => Some("pt-BR".into()),
        "fr" => Some("fr-FR".into()),
        "de" => Some("de-DE".into()),
        "it" => Some("it-IT".into()),
        "nl" => Some("nl-NL".into()),
        "pl" => Some("pl-PL".into()),
        _ => None,
    }
}

/// Reserved Windows local account names rejected during unattended user creation.
pub const DISALLOWED_LOCAL_ACCOUNTS: &[&str] = &[
    "Administrator",
    "Järjestelmänvalvoja",
    "Administrateur",
    "Rendszergazda",
    "Administrador",
    "Администратор",
    "Administratör",
    "Guest",
    "DefaultAccount",
    "WDAGUtilityAccount",
    "HelpAssistant",
    "KRBTGT",
    "Local",
    "NONE",
    "SYSTEM",
];

/// Characters disallowed in Windows local account names, sanitized to `_`.
pub const DISALLOWED_ACCOUNT_CHARS: &str = "/\\[]:;|=,+*?<>%@&\"";

/// Validates and sanitizes a Windows local account username.
/// Returns `None` if empty or matching a reserved Windows built-in account name.
pub fn sanitize_local_username(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if DISALLOWED_LOCAL_ACCOUNTS
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(trimmed))
    {
        return None;
    }
    let sanitized: String = trimmed
        .chars()
        .map(|c| {
            if DISALLOWED_ACCOUNT_CHARS.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

/// Generates a complete, spec-compliant Microsoft `autounattend.xml` for Windows 10/11 Setup.
pub fn generate_autounattend_xml(opts: &WindowsUxOptions, win_locale: &str) -> String {
    let mut xml = String::with_capacity(4096);
    xml.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    xml.push_str("<unattend xmlns=\"urn:schemas-microsoft-com:unattend\" xmlns:wcm=\"http://schemas.microsoft.com/WMIConfig/2002/State\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n");

    // ------------------------------------------------------------------------
    // Pass 1: windowsPE (TPM/RAM/SecureBoot/CPU/Storage bypass + setup locale)
    // ------------------------------------------------------------------------
    if opts.bypass_tpm_ram_secureboot || opts.set_regional_options {
        xml.push_str("  <settings pass=\"windowsPE\">\n");
        if opts.bypass_tpm_ram_secureboot {
            xml.push_str("    <component name=\"Microsoft-Windows-Setup\" processorArchitecture=\"amd64\" publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" versionScope=\"nonSxS\">\n");
            xml.push_str("      <UserData>\n");
            xml.push_str("        <AcceptEula>true</AcceptEula>\n");
            xml.push_str("        <ProductKey>\n");
            xml.push_str("          <Key />\n");
            xml.push_str("        </ProductKey>\n");
            xml.push_str("      </UserData>\n");
            xml.push_str("      <RunSynchronous>\n");
            let cmds = [
                (1, "reg add HKLM\\SYSTEM\\Setup\\LabConfig /v BypassTPMCheck /t REG_DWORD /d 1 /f"),
                (2, "reg add HKLM\\SYSTEM\\Setup\\LabConfig /v BypassSecureBootCheck /t REG_DWORD /d 1 /f"),
                (3, "reg add HKLM\\SYSTEM\\Setup\\LabConfig /v BypassRAMCheck /t REG_DWORD /d 1 /f"),
                (4, "reg add HKLM\\SYSTEM\\Setup\\LabConfig /v BypassStorageCheck /t REG_DWORD /d 1 /f"),
                (5, "reg add HKLM\\SYSTEM\\Setup\\LabConfig /v BypassCPUCheck /t REG_DWORD /d 1 /f"),
            ];
            for (order, cmd) in cmds {
                xml.push_str("        <RunSynchronousCommand wcm:action=\"add\">\n");
                xml.push_str(&format!("          <Order>{}</Order>\n", order));
                xml.push_str(&format!("          <Path>{}</Path>\n", cmd));
                xml.push_str("        </RunSynchronousCommand>\n");
            }
            xml.push_str("      </RunSynchronous>\n");
            xml.push_str("    </component>\n");
        }
        if opts.set_regional_options {
            let loc = xml_escape(win_locale);
            xml.push_str("    <component name=\"Microsoft-Windows-International-Core-WinPE\" processorArchitecture=\"amd64\" publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" versionScope=\"nonSxS\">\n");
            xml.push_str(&format!("      <InputLocale>{}</InputLocale>\n", loc));
            xml.push_str(&format!("      <SystemLocale>{}</SystemLocale>\n", loc));
            xml.push_str(&format!("      <UILanguage>{}</UILanguage>\n", loc));
            xml.push_str(&format!("      <UserLocale>{}</UserLocale>\n", loc));
            xml.push_str("    </component>\n");
        }
        xml.push_str("  </settings>\n");
    }

    // ------------------------------------------------------------------------
    // Pass 2: specialize (BypassNRO offline account + Prevent BitLocker)
    // ------------------------------------------------------------------------
    if opts.bypass_online_account || opts.disable_bitlocker {
        xml.push_str("  <settings pass=\"specialize\">\n");
        xml.push_str("    <component name=\"Microsoft-Windows-Deployment\" processorArchitecture=\"amd64\" publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" versionScope=\"nonSxS\">\n");
        xml.push_str("      <RunSynchronous>\n");
        let mut order = 1;
        if opts.bypass_online_account {
            xml.push_str("        <RunSynchronousCommand wcm:action=\"add\">\n");
            xml.push_str(&format!("          <Order>{}</Order>\n", order));
            xml.push_str("          <Path>reg add \"HKLM\\Software\\Microsoft\\Windows\\CurrentVersion\\OOBE\" /v BypassNRO /t REG_DWORD /d 1 /f</Path>\n");
            xml.push_str("        </RunSynchronousCommand>\n");
            order += 1;
        }
        if opts.disable_bitlocker {
            xml.push_str("        <RunSynchronousCommand wcm:action=\"add\">\n");
            xml.push_str(&format!("          <Order>{}</Order>\n", order));
            xml.push_str("          <Path>reg add \"HKLM\\SYSTEM\\CurrentControlSet\\Control\\BitLocker\" /v PreventDeviceEncryption /t REG_DWORD /d 1 /f</Path>\n");
            xml.push_str("        </RunSynchronousCommand>\n");
        }
        xml.push_str("      </RunSynchronous>\n");
        xml.push_str("    </component>\n");
        xml.push_str("  </settings>\n");
    }

    // ------------------------------------------------------------------------
    // Pass 3: oobeSystem (Local account, privacy, regional, BitLocker filter)
    // ------------------------------------------------------------------------
    let sanitized_user = if opts.create_local_account {
        sanitize_local_username(&opts.local_username)
    } else {
        None
    };
    if opts.bypass_online_account
        || opts.disable_data_collection
        || sanitized_user.is_some()
        || opts.set_regional_options
        || opts.disable_bitlocker
    {
        xml.push_str("  <settings pass=\"oobeSystem\">\n");
        if opts.bypass_online_account || opts.disable_data_collection || sanitized_user.is_some() {
            xml.push_str("    <component name=\"Microsoft-Windows-Shell-Setup\" processorArchitecture=\"amd64\" publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" versionScope=\"nonSxS\">\n");
            if opts.bypass_online_account || opts.disable_data_collection {
                xml.push_str("      <OOBE>\n");
                if opts.disable_data_collection {
                    xml.push_str("        <HideEULAPage>true</HideEULAPage>\n");
                    xml.push_str("        <ProtectYourPC>3</ProtectYourPC>\n");
                }
                if opts.bypass_online_account {
                    xml.push_str("        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>\n");
                    xml.push_str("        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>\n");
                }
                xml.push_str("      </OOBE>\n");
            }
            if let Some(ref user) = sanitized_user {
                let safe_user = xml_escape(user);
                xml.push_str("      <UserAccounts>\n");
                xml.push_str("        <LocalAccounts>\n");
                xml.push_str("          <LocalAccount wcm:action=\"add\">\n");
                xml.push_str(&format!("            <Name>{}</Name>\n", safe_user));
                xml.push_str(&format!(
                    "            <DisplayName>{}</DisplayName>\n",
                    safe_user
                ));
                xml.push_str("            <Group>Administrators;Power Users</Group>\n");
                xml.push_str("            <Password>\n");
                xml.push_str("              <Value>UABhAHMAcwB3AG8AcgBkAA==</Value>\n");
                xml.push_str("              <PlainText>false</PlainText>\n");
                xml.push_str("            </Password>\n");
                xml.push_str("          </LocalAccount>\n");
                xml.push_str("        </LocalAccounts>\n");
                xml.push_str("      </UserAccounts>\n");
                xml.push_str("      <FirstLogonCommands>\n");
                xml.push_str("        <SynchronousCommand wcm:action=\"add\">\n");
                xml.push_str("          <Order>1</Order>\n");
                xml.push_str(&format!(
                    "          <CommandLine>net user &quot;{}&quot; /logonpasswordchg:yes</CommandLine>\n",
                    safe_user
                ));
                xml.push_str("        </SynchronousCommand>\n");
                xml.push_str("        <SynchronousCommand wcm:action=\"add\">\n");
                xml.push_str("          <Order>2</Order>\n");
                xml.push_str("          <CommandLine>net accounts /maxpwage:unlimited</CommandLine>\n");
                xml.push_str("        </SynchronousCommand>\n");
                xml.push_str("      </FirstLogonCommands>\n");
            }
            xml.push_str("    </component>\n");
        }
        if opts.set_regional_options {
            let loc = xml_escape(win_locale);
            xml.push_str("    <component name=\"Microsoft-Windows-International-Core\" processorArchitecture=\"amd64\" publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" versionScope=\"nonSxS\">\n");
            xml.push_str(&format!("      <InputLocale>{}</InputLocale>\n", loc));
            xml.push_str(&format!("      <SystemLocale>{}</SystemLocale>\n", loc));
            xml.push_str(&format!("      <UserLocale>{}</UserLocale>\n", loc));
            xml.push_str(&format!("      <UILanguage>{}</UILanguage>\n", loc));
            xml.push_str("    </component>\n");
        }
        if opts.disable_bitlocker {
            xml.push_str("    <component name=\"Microsoft-Windows-SecureStartup-FilterDriver\" processorArchitecture=\"amd64\" publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" versionScope=\"nonSxS\">\n");
            xml.push_str("      <PreventDeviceEncryption>true</PreventDeviceEncryption>\n");
            xml.push_str("    </component>\n");
            xml.push_str("    <component name=\"Microsoft-Windows-EnhancedStorage-Adm\" processorArchitecture=\"amd64\" publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" versionScope=\"nonSxS\">\n");
            xml.push_str("      <TCGSecurityActivationDisabled>1</TCGSecurityActivationDisabled>\n");
            xml.push_str("    </component>\n");
        }
        xml.push_str("  </settings>\n");
    }

    // ------------------------------------------------------------------------
    // Pass 4: offlineServicing (SanPolicy = 4 prevents internal disk mounting)
    // ------------------------------------------------------------------------
    if opts.disable_internal_disks {
        xml.push_str("  <settings pass=\"offlineServicing\">\n");
        xml.push_str("    <component name=\"Microsoft-Windows-PartitionManager\" processorArchitecture=\"amd64\" publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" versionScope=\"nonSxS\">\n");
        xml.push_str("      <SanPolicy>4</SanPolicy>\n");
        xml.push_str("    </component>\n");
        xml.push_str("  </settings>\n");
    }

    xml.push_str("</unattend>\n");
    xml
}

/// Writes `autounattend.xml` to the mounted Windows USB root (and `sources/$OEM$/$$/Panther/unattend.xml`),
/// and applies the `appraiserres.dll` placeholder bypass when TPM/RAM/SecureBoot bypass is enabled.
pub fn write_autounattend_to_mount<L: Fn(String)>(
    mount_root: &Path,
    opts: &WindowsUxOptions,
    log: &L,
) -> Result<(), String> {
    if !opts.any_enabled() {
        return Ok(());
    }
    let locale = detect_windows_locale();
    let content = generate_autounattend_xml(opts, &locale);
    let root_xml = mount_root.join("autounattend.xml");
    fs::write(&root_xml, &content)
        .map_err(|e| format!("Failed writing autounattend.xml: {}", e))?;
    log(format!(
        "Generated Windows customization autounattend.xml (locale={}, bypass_tpm={}, bypass_nro={}, local_user={}, privacy={}, no_bitlocker={})",
        locale,
        opts.bypass_tpm_ram_secureboot,
        opts.bypass_online_account,
        opts.create_local_account,
        opts.disable_data_collection,
        opts.disable_bitlocker
    ));

    // Also place a copy in sources/$OEM$/$$/Panther/unattend.xml for maximum compatibility
    let sources_dir = mount_root.join("sources");
    if sources_dir.is_dir() {
        let panther = sources_dir.join("$OEM$").join("$$").join("Panther");
        if fs::create_dir_all(&panther).is_ok() {
            let _ = fs::write(panther.join("unattend.xml"), &content);
        }

        // In-place upgrade TPM/RAM/SecureBoot bypass: back up appraiserres.dll -> appraiserres.bak
        // and replace with an empty 0-byte placeholder.
        if opts.bypass_tpm_ram_secureboot {
            if let Ok(entries) = fs::read_dir(&sources_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().eq_ignore_ascii_case("appraiserres.dll") {
                        let dll_path = entry.path();
                        let bak_path = sources_dir.join("appraiserres.bak");
                        if fs::rename(&dll_path, &bak_path).is_ok() {
                            if File::create(&dll_path).is_ok() {
                                log("Created empty 'sources/appraiserres.dll' placeholder (original backed up to appraiserres.bak).".into());
                            }
                        }
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Installs the UEFI:NTFS bootloader onto the secondary FAT EFI System Partition (`uefi_part`).
/// Strategy:
/// 1. Check cached open-source `uefi-ntfs.img` (`/usr/share/luxus/uefi-ntfs.img` or `~/.cache/luxus/uefi-ntfs.img`)
///    or fetch the official GPLv3 `uefi-ntfs.img` release if `curl` is available and online.
/// 2. If offline or unavailable, build a 100% native offline GRUB2 standalone UEFI-NTFS chainloader
///    (`EFI/BOOT/BOOTX64.EFI`) with embedded `ntfs`, `exfat`, `part_gpt`, `part_msdos`, `chain`, `search`
///    modules directly onto the FAT ESP!
pub fn install_uefi_ntfs_partition<L: Fn(String)>(
    uefi_part: &str,
    main_part_uuid: Option<&str>,
    log: &L,
) -> Result<(), String> {
    log(format!(
        "Setting up UEFI:NTFS boot partition on {}...",
        uefi_part
    ));

    // 1. Try local cached or upstream open-source uefi-ntfs.img (1 MiB FAT12/16 image)
    if let Some(img_path) = locate_or_fetch_uefi_ntfs_img(log) {
        log(format!(
            "Writing UEFI:NTFS image ({}) to {}...",
            img_path.display(),
            uefi_part
        ));
        let data = fs::read(&img_path)
            .map_err(|e| format!("Cannot read {}: {}", img_path.display(), e))?;
        let mut dst = fs::OpenOptions::new()
            .write(true)
            .open(uefi_part)
            .map_err(|e| format!("Cannot open {} for UEFI:NTFS write: {}", uefi_part, e))?;
        dst.write_all(&data)
            .map_err(|e| format!("Error writing UEFI:NTFS image: {}", e))?;
        let _ = dst.sync_all();
        log("UEFI:NTFS boot partition written successfully.".into());
        return Ok(());
    }

    // 2. Offline native Linux fallback: format FAT16/32 ESP and generate standalone GRUB2 NTFS chainloader
    log("Offline fallback: building native GRUB2 UEFI-NTFS chainloader on ESP...".into());
    let mkfs_cmd = if cmd_in_path("mkfs.vfat") {
        "mkfs.vfat"
    } else if cmd_in_path("mkfs.fat") {
        "mkfs.fat"
    } else {
        return Err("UEFI:NTFS requires mkfs.vfat (dosfstools) for ESP formatting.".into());
    };
    let status = Command::new(mkfs_cmd)
        .args(["-F", "16", "-n", "UEFI_NTFS", uefi_part])
        .status()
        .map_err(|e| format!("Failed running {}: {}", mkfs_cmd, e))?;
    if !status.success() {
        let _ = Command::new(mkfs_cmd)
            .args(["-F", "32", "-n", "UEFI_NTFS", uefi_part])
            .status();
    }

    let tmp_base = std::env::temp_dir().join(format!("luxus-uefintfs-{}", std::process::id()));
    let mnt = tmp_base.join("esp");
    let cfg_path = tmp_base.join("grub.cfg");
    fs::create_dir_all(&mnt).map_err(|e| e.to_string())?;

    let mut grub_cfg = String::new();
    grub_cfg.push_str("insmod part_gpt\ninsmod part_msdos\ninsmod ntfs\ninsmod exfat\ninsmod fat\ninsmod chain\ninsmod search_fs_uuid\ninsmod search_fs_file\n");
    if let Some(uuid) = main_part_uuid {
        grub_cfg.push_str(&format!(
            "search --no-floppy --fs-uuid --set=root {}\n",
            uuid
        ));
    } else {
        grub_cfg.push_str("search --no-floppy --file --set=root /efi/boot/bootx64.efi\n");
    }
    grub_cfg.push_str("if [ -f ($root)/efi/boot/bootx64.efi ]; then\n  chainloader ($root)/efi/boot/bootx64.efi\n  boot\nfi\n");
    grub_cfg.push_str("if [ -f ($root)/efi/microsoft/boot/bootmgfw.efi ]; then\n  chainloader ($root)/efi/microsoft/boot/bootmgfw.efi\n  boot\nfi\n");
    fs::write(&cfg_path, &grub_cfg).map_err(|e| e.to_string())?;

    let cleanup = || {
        let _ = Command::new("umount").arg(&mnt).status();
        let _ = fs::remove_dir_all(&tmp_base);
    };

    let mnt_ok = Command::new("mount")
        .args([uefi_part, &mnt.display().to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !mnt_ok {
        cleanup();
        return Err(format!("Could not mount UEFI:NTFS partition {}", uefi_part));
    }

    let efi_boot = mnt.join("EFI").join("BOOT");
    let _ = fs::create_dir_all(&efi_boot);
    let bootx64 = efi_boot.join("BOOTX64.EFI");

    // 2. Try standalone open-source UEFI:NTFS + EfiFs drivers from upstream repositories
    if install_upstream_uefi_ntfs_drivers(&mnt, log) {
        let _ = Command::new("sync").status();
        cleanup();
        return Ok(());
    }

    // 3. Offline native Linux fallback: generate standalone GRUB2 NTFS/exFAT chainloader
    let mkstandalone = if cmd_in_path("grub2-mkstandalone") {
        Some("grub2-mkstandalone")
    } else if cmd_in_path("grub-mkstandalone") {
        Some("grub-mkstandalone")
    } else {
        None
    };

    if let Some(tool) = mkstandalone {
        let out = Command::new(tool)
            .args([
                "-O",
                "x86_64-efi",
                "-o",
                &bootx64.display().to_string(),
                &format!("boot/grub/grub.cfg={}", cfg_path.display()),
            ])
            .output();
        match out {
            Ok(o) if o.status.success() && bootx64.exists() => {
                let _ = File::open(&bootx64).and_then(|f| f.sync_all());
                log("Built native GRUB2 UEFI:NTFS chainloader (EFI/BOOT/BOOTX64.EFI).".into());
                let _ = Command::new("sync").status();
                cleanup();
                return Ok(());
            }
            _ => {
                log(format!(
                    "Warning: {} could not build x86_64-efi image (install grub2-efi-x64-modules or connect online for UEFI:NTFS drivers).",
                    tool
                ));
            }
        }
    } else {
        log("Warning: grub2-mkstandalone not found and offline; UEFI:NTFS partition created as empty FAT ESP.".into());
    }

    let _ = Command::new("sync").status();
    cleanup();
    Ok(())
}

fn cache_dir_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".cache").join("luxus"))
        .unwrap_or_else(|| std::env::temp_dir().join("luxus-cache"))
}

fn locate_or_fetch_uefi_ntfs_img<L: Fn(String)>(_log: &L) -> Option<PathBuf> {
    let candidates = [
        PathBuf::from("/usr/share/luxus/uefi-ntfs.img"),
        PathBuf::from("/usr/local/share/luxus/uefi-ntfs.img"),
        cache_dir_path().join("uefi-ntfs.img"),
    ];
    for c in &candidates {
        if let Ok(meta) = fs::metadata(c) {
            if meta.len() >= 256 * 1024 && meta.len() <= 4 * 1024 * 1024 {
                return Some(c.clone());
            }
        }
    }
    None
}

fn download_cached_file(url: &str, dest: &Path, min_bytes: u64) -> bool {
    if let Ok(meta) = fs::metadata(dest) {
        if meta.len() >= min_bytes {
            return true;
        }
    }
    if !cmd_in_path("curl") {
        return false;
    }
    if let Some(parent) = dest.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = dest.with_extension("part");
    let ok = Command::new("curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "10",
            "--max-time",
            "25",
            "-o",
            &tmp.display().to_string(),
            url,
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        if let Ok(meta) = fs::metadata(&tmp) {
            if meta.len() >= min_bytes && fs::rename(&tmp, dest).is_ok() {
                return true;
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    false
}

/// Installs the standalone open-source UEFI:NTFS chainloader and EfiFs NTFS/exFAT drivers
/// (`EFI/BOOT/bootx64.efi`, `EFI/uefi-ntfs/ntfs_x64.efi`, `EFI/uefi-ntfs/exfat_x64.efi`).
fn install_upstream_uefi_ntfs_drivers<L: Fn(String)>(esp_mount: &Path, log: &L) -> bool {
    let cache = cache_dir_path().join("uefi-ntfs");
    let boot_cache = cache.join("bootx64.efi");
    let ntfs_cache = cache.join("ntfs_x64.efi");
    let exfat_cache = cache.join("exfat_x64.efi");

    let boot_ok = download_cached_file(
        "https://github.com/pbatard/uefi-ntfs/releases/latest/download/bootx64.efi",
        &boot_cache,
        16 * 1024,
    );
    let ntfs_ok = download_cached_file(
        "https://github.com/pbatard/EfiFs/releases/latest/download/ntfs_x64.efi",
        &ntfs_cache,
        32 * 1024,
    );
    if !boot_ok || !ntfs_ok {
        return false;
    }
    let exfat_ok = download_cached_file(
        "https://github.com/pbatard/EfiFs/releases/latest/download/exfat_x64.efi",
        &exfat_cache,
        16 * 1024,
    );

    let efi_boot = esp_mount.join("EFI").join("BOOT");
    let drv_dir = esp_mount.join("EFI").join("uefi-ntfs");
    if fs::create_dir_all(&efi_boot).is_err() || fs::create_dir_all(&drv_dir).is_err() {
        return false;
    }
    if fs::copy(&boot_cache, efi_boot.join("bootx64.efi")).is_err() {
        return false;
    }
    if fs::copy(&ntfs_cache, drv_dir.join("ntfs_x64.efi")).is_err() {
        return false;
    }
    if exfat_ok {
        let _ = fs::copy(&exfat_cache, drv_dir.join("exfat_x64.efi"));
    }
    log("Installed open-source UEFI:NTFS bootloader and EfiFs NTFS/exFAT drivers onto ESP.".into());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_posix_to_bcp47() {
        assert_eq!(posix_to_bcp47("es_MX.UTF-8"), Some("es-MX".into()));
        assert_eq!(posix_to_bcp47("en_US.UTF-8"), Some("en-US".into()));
        assert_eq!(posix_to_bcp47("pt_BR"), Some("pt-BR".into()));
        assert_eq!(posix_to_bcp47("C.UTF-8"), None);
    }

    #[test]
    fn test_local_account_sanitization_and_reserved() {
        assert_eq!(sanitize_local_username("Administrator"), None);
        assert_eq!(sanitize_local_username("guest"), None);
        assert_eq!(sanitize_local_username("SYSTEM"), None);
        assert_eq!(sanitize_local_username("Administrador"), None);
        assert_eq!(
            sanitize_local_username("  Alan&Test/1  "),
            Some("Alan_Test_1".into())
        );
    }

    #[test]
    fn test_autounattend_xml_generation() {
        let opts = WindowsUxOptions {
            bypass_tpm_ram_secureboot: true,
            bypass_online_account: true,
            create_local_account: true,
            local_username: "Alan&Test".into(),
            set_regional_options: true,
            disable_data_collection: true,
            disable_bitlocker: true,
            disable_internal_disks: true,
        };
        let xml = generate_autounattend_xml(&opts, "es-MX");
        assert!(xml.contains("BypassTPMCheck"));
        assert!(xml.contains("BypassSecureBootCheck"));
        assert!(xml.contains("BypassRAMCheck"));
        assert!(xml.contains("BypassNRO"));
        assert!(xml.contains("<PreventDeviceEncryption>true</PreventDeviceEncryption>"));
        assert!(xml.contains("<TCGSecurityActivationDisabled>1</TCGSecurityActivationDisabled>"));
        assert!(xml.contains("<SanPolicy>4</SanPolicy>"));
        assert!(xml.contains("<ProtectYourPC>3</ProtectYourPC>"));
        assert!(xml.contains("<Name>Alan_Test</Name>"));
        assert!(xml.contains("<Group>Administrators;Power Users</Group>"));
        assert!(xml.contains("net user &quot;Alan_Test&quot; /logonpasswordchg:yes"));
        assert!(xml.contains("net accounts /maxpwage:unlimited"));
        assert!(xml.contains("<UILanguage>es-MX</UILanguage>"));
    }
}
