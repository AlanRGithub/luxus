//! Filesystem limits, cluster-size tables, label sanitization, and deep ISO-9660/UDF/Windows/Linux inspection.

use crate::devices::cmd_in_path;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Command;

// Device discovery limits
pub const MAX_DRIVES: usize = 0x40; // 64
pub const MAX_CARD_BYTES: u64 = 0x7d00_0000_00; // 500 GiB

// Geometry & partitioning constants
pub const MIN_SECTOR_BYTES: u64 = 0x200; // 512 bytes
pub const UEFI_MBR_DISK_ID: u32 = 0x4946_4555; // "UEFI"
pub const GPT_EFI_START_BYTES: u64 = 0x10_0000; // 1 MiB
#[allow(dead_code)]
pub const GPT_EFI_SIZE_BYTES: u64 = 0x1040_0000; // 260 MiB
pub const UEFI_NTFS_PART_BYTES: u64 = 0x80_0000; // 8 MiB secondary ESP (fits both uefi-ntfs.img and offline GRUB2 EFI)
#[allow(dead_code)]
pub const FAT16_LBA_MBR_TYPE: u8 = 0x0e;
pub const FAT32_LBA_MBR_TYPE: u8 = 0x0c;

// FAT32 format limits
pub const FAT32_MIN_CLUSTERS: u64 = 0x1_0000; // 65536
pub const FAT32_MAX_DATA_CLUSTERS: u64 = 0x1000_0000; // 2^28
pub const FAT32_MAX_VOLUME_BYTES: u64 = 0x200_0000_0000; // 2 TiB
pub const LARGE_FAT32_THRESHOLD: u64 = 0x8_0000_0000; // 32 GiB

// Volume label limits
pub const DEFAULT_LABEL: &str = "NO_LABEL";
pub const FAT_LABEL_MAX: usize = 0x0b; // 11
pub const GENERIC_LABEL_MAX: usize = 0x20; // 32

// Progress, large drives, and bad-blocks check parameters
pub const LARGE_DRIVE_BYTES: u64 = 0x100_0000_0000; // 1 TiB
pub const DD_MODE_MARKER: u8 = 3;
pub const BADBLOCKS_BLOCK_BYTES: usize = 0x8_0000; // 512 KiB
pub const BADBLOCKS_BATCH_BLOCKS: usize = 0x40; // 64 blocks = 32 MiB chunk
pub const BADBLOCKS_MAX_ERRORS: usize = 0x100; // aborts above 255 bad blocks
pub const BADBLOCKS_PATTERNS: [u8; 4] = [0x55, 0xaa, 0xff, 0x00];

/// Supported power-of-two cluster sizes.
pub const CLUSTER_SIZES: [u32; 9] = [
    0x200, 0x400, 0x800, 0x1000, 0x2000, 0x4000, 0x8000, 0x10000, 0x20000,
];

// Allowed cluster-size bitmasks per filesystem
pub const FAT32_CLUSTER_MASK: u32 = 0x0001_fe00; // 512..=65536
pub const LARGE_FAT32_CLUSTER_MASK: u32 = 0x0001_c000; // 16K..=64K
pub const NTFS_CLUSTER_MASK: u32 = 0x0001_f000; // 4K..=64K
pub const EXFAT_CLUSTER_MASK: u32 = 0x03ff_fe00; // 512..=32M
pub const CLUSTER_BYTES_DISPLAY_MAX: u32 = 0x2000; // <= 8192 shown as "bytes", >= 16384 as "kilobytes"

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CompressionKind {
    #[default]
    None,
    Gzip,
    Xz,
    Zstd,
    Bzip2,
    Zip,
    Lzma,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum PersistenceStyle {
    #[default]
    None,
    /// Ubuntu, Linux Mint, Pop!_OS, Elementary, Zorin (`casper-rw`)
    UbuntuCasper,
    /// Debian, Kali, Parrot, MX (`persistence` + `persistence.conf`)
    DebianLive,
}

/// Default FAT32 cluster size selected by volume capacity.
pub fn fat32_default_cluster(volume_bytes: u64) -> u32 {
    if volume_bytes < 0x400_0000 {
        0x200
    } else if volume_bytes < 0x800_0000 {
        0x400
    } else if volume_bytes < 0x1000_0000 {
        0x800
    } else if volume_bytes < 0x2_0000_0000 {
        0x1000
    } else if volume_bytes < 0x4_0000_0000 {
        0x2000
    } else if volume_bytes < 0x8_0000_0000 {
        0x4000
    } else if volume_bytes < FAT32_MAX_VOLUME_BYTES {
        0x8000
    } else {
        0x10000
    }
}

/// Default NTFS cluster size scaled by volume capacity (16 TiB..256 TiB).
pub fn ntfs_default_cluster(volume_bytes: u64) -> u32 {
    let mut step: u64 = 0x10;
    while step <= 0x100 {
        if volume_bytes < (step << 40) {
            return ((step >> 2) << 10) as u32;
        }
        step *= 2;
    }
    0x10000
}

/// Default exFAT cluster size thresholds.
pub fn exfat_default_cluster(volume_bytes: u64) -> u32 {
    if volume_bytes < 0x1000_0000 {
        0x1000 // < 256 MiB -> 4096 bytes
    } else if volume_bytes < 0x8_0000_0000 {
        0x8000 // < 32 GiB -> 32 KiB
    } else {
        0x20000 // >= 32 GiB -> 128 KiB
    }
}

/// Default cluster size for supported filesystems.
pub fn default_cluster_for_fs(fs_is_fat32: bool, fs_is_exfat: bool, volume_bytes: u64) -> u32 {
    if fs_is_fat32 {
        fat32_default_cluster(volume_bytes)
    } else if fs_is_exfat {
        exfat_default_cluster(volume_bytes)
    } else {
        ntfs_default_cluster(volume_bytes)
    }
}

/// Returns allowed cluster sizes matching a filesystem bitmask.
pub fn allowed_clusters_for_mask(mask: u32) -> Vec<u32> {
    CLUSTER_SIZES
        .iter()
        .copied()
        .filter(|&c| (mask & c) != 0)
        .collect()
}

/// Validates FAT32 volume size and cluster count limits.
pub fn validate_fat32(volume_bytes: u64, sector_bytes: u64, cluster: u32) -> Result<u32, String> {
    let sec = sector_bytes.max(MIN_SECTOR_BYTES);
    let sectors = volume_bytes / sec;
    if sectors < FAT32_MIN_CLUSTERS {
        return Err(
            "This drive is too small for FAT32 - there must be at least 64K clusters".into(),
        );
    }
    if sectors >= 0xffff_ffff || volume_bytes > FAT32_MAX_VOLUME_BYTES {
        return Err("This drive is too big for FAT32 - max 2TB supported".into());
    }
    let chosen = if cluster < 0x200 {
        fat32_default_cluster(volume_bytes)
    } else {
        cluster
    };
    let data_clusters = volume_bytes / chosen as u64;
    if data_clusters < FAT32_MIN_CLUSTERS {
        return Err("FAT32 must have at least 65536 clusters, try to specify a smaller cluster size or use the default".into());
    }
    if data_clusters >= FAT32_MAX_DATA_CLUSTERS {
        return Err("This drive has more than 2^28 clusters, try to specify a larger cluster size or use the default".into());
    }
    Ok(chosen)
}

/// Normalizes volume labels according to FAT (11-char) or generic (32-char) rules.
pub fn normalize_label(raw: &str, fat: bool, fallback: &str) -> String {
    let source = if raw.trim().is_empty() {
        DEFAULT_LABEL
    } else {
        raw.trim()
    };
    let cap = if fat { FAT_LABEL_MAX } else { GENERIC_LABEL_MAX };
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ' ' {
                    c
                } else {
                    '_'
                }
            })
            .take(cap)
            .collect()
    };
    let mut out = clean(source);
    if fat && out.len() >= 2 && out.chars().filter(|c| *c == '_').count() * 2 > out.len() {
        let fb = fallback.replace('.', "_");
        let candidate = clean(&fb);
        if !candidate.trim().is_empty() {
            out = candidate;
        }
    }
    if out.trim().is_empty() {
        DEFAULT_LABEL.to_string()
    } else {
        out
    }
}

/// ISO volume labels that require raw DD image mode for proper booting.
pub fn forced_dd_label(label: &str) -> bool {
    let l = label.trim_start();
    if l.starts_with("Install-SUSE")
        || l.starts_with("Install-LEAP")
        || l.starts_with("openSUSE-Tumbleweed")
    {
        return true;
    }
    let major_8_9 = |prefix: &str| {
        l.strip_prefix(prefix)
            .and_then(|r| r.as_bytes().first())
            .map(|b| *b == b'8' || *b == b'9')
            .unwrap_or(false)
    };
    if major_8_9("AlmaLinux-")
        || major_8_9("CentOS-")
        || major_8_9("Circle-")
        || major_8_9("OL-")
        || major_8_9("RHEL-")
        || major_8_9("Rocky-")
        || major_8_9("MIRACLE-LINUX-")
    {
        return true;
    }
    if let Some(rest) = l.strip_prefix("Fedora") {
        let b = rest.as_bytes();
        for w in b.windows(3) {
            if w[0] == b'-' && w[1] == b'3' && (b'3'..=b'9').contains(&w[2]) {
                return true;
            }
        }
    }
    false
}

/// Detects whether a Linux ISO supports Ubuntu (`casper-rw`) or Debian (`persistence`) live persistence.
pub fn detect_persistence_style(label_or_name: &str) -> PersistenceStyle {
    let low = label_or_name.to_lowercase();
    if low.contains("ubuntu")
        || low.contains("mint")
        || low.contains("pop-os")
        || low.contains("pop_os")
        || low.contains("zorin")
        || low.contains("elementary")
        || low.contains("kubuntu")
        || low.contains("xubuntu")
        || low.contains("lubuntu")
    {
        PersistenceStyle::UbuntuCasper
    } else if low.contains("debian")
        || low.contains("kali")
        || low.contains("parrot")
        || low.contains("mx-")
        || low.contains("tails")
    {
        PersistenceStyle::DebianLive
    } else {
        PersistenceStyle::None
    }
}

#[derive(Clone, Debug, Default)]
pub struct ImageReport {
    #[allow(dead_code)]
    pub path: String,
    pub file_name: String,
    pub file_size: u64,
    pub signature: String,
    pub compression: CompressionKind,
    pub is_iso9660: bool,
    pub is_udf: bool,
    pub is_el_torito: bool,
    pub is_hybrid_mbr: bool,
    pub has_gpt: bool,
    pub iso_label: Option<String>,
    pub iso_reported_size: Option<u64>,
    pub force_dd: bool,
    pub is_windows_iso: bool,
    pub has_over_4gb_file: bool,
    pub has_isolinux: bool,
    pub is_fixed_vhd: bool,
    pub persistence_style: PersistenceStyle,
    pub notes: Vec<String>,
}

/// Inspects an ISO/IMG/compressed image in pure Rust using ISO-9660 PVD (0x8000),
/// UDF descriptors (0x8800/0x9000), El Torito Boot Record, MBR/GPT headers,
/// and optional fast `7z l` peek for Windows `install.wim`/`install.esd` and >4 GiB files.
pub fn inspect_image_file(path: &Path) -> Result<ImageReport, String> {
    let mut f = File::open(path).map_err(|e| format!("Cannot open image: {}", e))?;
    let file_size = f
        .metadata()
        .map(|m| m.len())
        .map_err(|e| format!("Cannot stat image: {}", e))?;
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("image.iso")
        .to_string();

    let mut report = ImageReport {
        path: path.display().to_string(),
        file_name: file_name.clone(),
        file_size,
        signature: "(Unrecognized)".into(),
        ..Default::default()
    };

    // Read first 1024 bytes (MBR + GPT header at LBA 1)
    let mut head = [0u8; 1024];
    let n_head = f.read(&mut head).unwrap_or(0);

    if n_head >= 512 && head[510] == 0x55 && head[511] == 0xaa {
        report.is_hybrid_mbr = true;
        report.signature = "Bootable MBR / Hybrid image (0x55AA)".into();
    }
    if n_head >= 520 && &head[512..520] == b"EFI PART" {
        report.has_gpt = true;
        report
            .notes
            .push("GPT partition header detected (EFI PART)".into());
    }

    // Check well-known filesystem & compression magic signatures
    if n_head >= 4 {
        let magic_le = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
        match magic_le {
            0x7371_7368 | 0x28cd_3d45 => report.signature = "SquashFS".into(),
            0x4253_4658 => report.signature = "MD RAID".into(),
            0x1eab_b7d7 | 0xe0f5_e1e2 => report.signature = "EROFS".into(),
            _ => {}
        }
        if n_head >= 36 && &head[32..36] == b"NXSB" {
            report.signature = "APFS".into();
        }
        if head[0] == 0x1f && head[1] == 0x8b {
            report.compression = CompressionKind::Gzip;
            report.signature = "GZIP compressed disk image".into();
            report
                .notes
                .push("Compressed GZIP image detected (streaming decompression enabled)".into());
        } else if n_head >= 6 && &head[0..6] == b"\xfd7zXZ\x00" {
            report.compression = CompressionKind::Xz;
            report.signature = "XZ compressed disk image".into();
            report
                .notes
                .push("Compressed XZ image detected (streaming decompression enabled)".into());
        } else if magic_le == 0xfd2f_b528 {
            report.compression = CompressionKind::Zstd;
            report.signature = "Zstandard compressed disk image".into();
            report
                .notes
                .push("Compressed Zstd image detected (streaming decompression enabled)".into());
        } else if n_head >= 3 && &head[0..3] == b"BZh" {
            report.compression = CompressionKind::Bzip2;
            report.signature = "BZIP2 compressed disk image".into();
            report
                .notes
                .push("Compressed BZIP2 image detected (streaming decompression enabled)".into());
        } else if n_head >= 4 && &head[0..4] == b"PK\x03\x04" {
            report.compression = CompressionKind::Zip;
            report.signature = "ZIP compressed disk image".into();
            report
                .notes
                .push("Compressed ZIP image detected (streaming decompression enabled)".into());
        } else if file_name.to_lowercase().ends_with(".lzma") {
            report.compression = CompressionKind::Lzma;
            report.signature = "LZMA compressed disk image".into();
            report
                .notes
                .push("Compressed LZMA image detected (streaming decompression enabled)".into());
        }
    }

    // Detect fixed Microsoft VHD footer ("conectix") at EOF so raw restore strips the 512-byte footer
    if crate::vhd::detect_fixed_vhd_payload_size(path).is_some() {
        report.is_fixed_vhd = true;
        report.signature = "Fixed Microsoft VHD disk image (conectix)".into();
        report
            .notes
            .push("Fixed VHD footer detected: trailing 512-byte footer will be stripped when writing.".into());
    }

    // Check ISO-9660 Primary Volume Descriptor at sector 16 (offset 0x8000) and UDF BEA01/NSR02/NSR03
    if file_size >= 0x9800 && f.seek(SeekFrom::Start(0x8000)).is_ok() {
        let mut desc_buf = [0u8; 6144]; // Sectors 16, 17, 18
        if f.read_exact(&mut desc_buf).is_ok() {
            let pvd = &desc_buf[0..2048];
            if pvd[0] == 0x01 && &pvd[1..6] == b"CD001" {
                report.is_iso9660 = true;
                report.signature = if report.is_hybrid_mbr {
                    "ISOHybrid (ISO9660 + MBR)".into()
                } else {
                    "ISO9660".into()
                };

                let vol = String::from_utf8_lossy(&pvd[40..72]).trim().to_string();
                if !vol.is_empty() {
                    let upper = vol.to_uppercase();
                    if upper.contains("WIN")
                        || upper.contains("CCCOMA")
                        || upper.contains("ESD-ISO")
                        || upper.contains("IR5_")
                        || upper.contains("CENA_")
                    {
                        report.is_windows_iso = true;
                    }
                    if forced_dd_label(&vol) {
                        report.force_dd = true;
                        report.notes.push(
                            "Note: DD image mode enforced since this ISOHybrid is not ISO mode compatible.".into(),
                        );
                    }
                    report.iso_label = Some(vol);
                }

                let blocks = u32::from_le_bytes([pvd[80], pvd[81], pvd[82], pvd[83]]) as u64;
                let blk_size = u16::from_le_bytes([pvd[128], pvd[129]]) as u64;
                let blk = if blk_size == 0 { 2048 } else { blk_size };
                let iso_bytes = blocks.saturating_mul(blk);
                if iso_bytes > 0 {
                    report.iso_reported_size = Some(iso_bytes);
                    if file_size < iso_bytes {
                        let diff = iso_bytes - file_size;
                        report.notes.push(format!(
                            "ERROR: Detected that file on disk has been truncated by {}!",
                            human_size(diff)
                        ));
                    } else if file_size > iso_bytes + 4 * 1024 * 1024 {
                        let diff = file_size - iso_bytes;
                        report.notes.push(format!(
                            "Note: File on disk is larger than reported ISO size by {}...",
                            human_size(diff)
                        ));
                    }
                }
            }

            // Scan sectors 16..18 for El Torito and UDF descriptors (Windows 10/11 ISOs use UDF!)
            for chunk in desc_buf.chunks_exact(2048) {
                if chunk[0] == 0x00
                    && &chunk[1..6] == b"CD001"
                    && &chunk[7..30] == b"EL TORITO SPECIFICATION"
                {
                    report.is_el_torito = true;
                    report.notes.push("Uses: El Torito bootable ISO".into());
                }
                if &chunk[1..6] == b"BEA01" || &chunk[1..6] == b"NSR02" || &chunk[1..6] == b"NSR03"
                {
                    report.is_udf = true;
                }
            }
            if report.is_udf && !report.is_iso9660 {
                report.signature = "UDF Bootable ISO (Windows / Modern DVD)".into();
            }
        }
    }

    // Fallback: check ISO9660 signature at offset 1
    if !report.is_iso9660 && n_head >= 6 && &head[1..6] == b"CD001" {
        report.is_iso9660 = true;
        report.signature = "ISO9660 (CD001 at offset 1)".into();
    }

    // Filename heuristic for Windows ISOs (e.g. Win11_24H2_Spanish_x64.iso)
    let fn_low = file_name.to_lowercase();
    if (report.is_iso9660 || report.is_udf || fn_low.ends_with(".iso"))
        && (fn_low.contains("win11")
            || fn_low.contains("win10")
            || fn_low.contains("windows")
            || fn_low.contains("win_11")
            || fn_low.contains("win_10"))
    {
        report.is_windows_iso = true;
    }

    // Fast 7z content peek on ISOs to detect Windows `sources/install.wim|esd` and >4 GiB files
    if (report.is_iso9660 || report.is_udf) && report.compression == CompressionKind::None {
        peek_iso_contents_7z(path, &mut report);
    }

    if !report.is_windows_iso && report.compression == CompressionKind::None {
        let probe_name = report
            .iso_label
            .as_deref()
            .unwrap_or(&report.file_name);
        let style = detect_persistence_style(probe_name);
        report.persistence_style = if style != PersistenceStyle::None {
            style
        } else {
            detect_persistence_style(&report.file_name)
        };
    }

    if report.is_windows_iso {
        report
            .notes
            .push("Detected: Windows installation ISO (UEFI:NTFS + Windows User Experience ready)".into());
    }
    if report.has_over_4gb_file {
        report
            .notes
            .push("Has a >4GB file (NTFS + UEFI:NTFS recommended instead of FAT32)".into());
    }

    Ok(report)
}

fn peek_iso_contents_7z(path: &Path, report: &mut ImageReport) {
    let tool = if cmd_in_path("7z") {
        Some("7z")
    } else if cmd_in_path("7zz") {
        Some("7zz")
    } else {
        None
    };

    if let Some(bin) = tool {
        if let Ok(out) = Command::new(bin)
            .args(["l", "-slt", &path.display().to_string()])
            .output()
        {
            if out.status.success() {
                let txt = String::from_utf8_lossy(&out.stdout);
                let mut cur_path = String::new();
                for line in txt.lines() {
                    if let Some(p) = line.strip_prefix("Path = ") {
                        cur_path = p.trim().to_lowercase().replace('\\', "/");
                        inspect_iso_entry_path(&cur_path, report);
                    } else if let Some(sz_str) = line.strip_prefix("Size = ") {
                        if let Ok(sz) = sz_str.trim().parse::<u64>() {
                            if sz > u32::MAX as u64 && !cur_path.is_empty() {
                                report.has_over_4gb_file = true;
                            }
                        }
                    }
                }
                return;
            }
        }
    }

    // Fallback: bsdtar -tvf for systems without 7z
    if cmd_in_path("bsdtar") {
        if let Ok(out) = Command::new("bsdtar")
            .args(["-tvf", &path.display().to_string()])
            .output()
        {
            if out.status.success() {
                let txt = String::from_utf8_lossy(&out.stdout);
                for line in txt.lines() {
                    let cols: Vec<&str> = line.split_whitespace().collect();
                    if cols.len() >= 9 {
                        if let Ok(sz) = cols[4].parse::<u64>() {
                            if sz > u32::MAX as u64 {
                                report.has_over_4gb_file = true;
                            }
                        }
                        let entry_path = cols[8..].join(" ").to_lowercase().replace('\\', "/");
                        let trimmed = entry_path.trim_start_matches("./");
                        inspect_iso_entry_path(trimmed, report);
                    }
                }
            }
        }
    }
}

fn inspect_iso_entry_path(cur_path: &str, report: &mut ImageReport) {
    if cur_path == "sources/install.wim"
        || cur_path == "sources/install.esd"
        || cur_path == "sources/install.swm"
        || cur_path == "sources/boot.wim"
        || cur_path == "bootmgr.efi"
    {
        report.is_windows_iso = true;
    }
    if cur_path.starts_with("isolinux/") || cur_path.starts_with("syslinux/") {
        report.has_isolinux = true;
    }
    if cur_path.starts_with("casper/") {
        report.persistence_style = PersistenceStyle::UbuntuCasper;
    } else if cur_path.starts_with("live/filesystem.squashfs")
        && report.persistence_style == PersistenceStyle::None
    {
        report.persistence_style = PersistenceStyle::DebianLive;
    }
}

pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;
    let b = bytes as f64;
    if b >= TB {
        format!("{:.1} TB", b / TB)
    } else if b >= GB {
        let g = b / GB;
        if (g - g.round()).abs() < 0.05 {
            format!("{:.0} GB", g.round())
        } else {
            format!("{:.1} GB", g)
        }
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// Values <= 8192 display in "bytes", larger power-of-two sizes display in "kilobytes".
pub fn format_cluster_label(bytes: u32, default_suffix: Option<&str>) -> String {
    let base = if bytes > CLUSTER_BYTES_DISPLAY_MAX && bytes % 1024 == 0 {
        format!("{} kilobytes", bytes / 1024)
    } else {
        format!("{} bytes", bytes)
    };
    if let Some(suffix) = default_suffix {
        format!("{} ({})", base, suffix)
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fat32_cluster_table() {
        assert_eq!(fat32_default_cluster(32 * 1024 * 1024), 512);
        assert_eq!(fat32_default_cluster(100 * 1024 * 1024), 1024);
        assert_eq!(fat32_default_cluster(200 * 1024 * 1024), 2048);
        assert_eq!(fat32_default_cluster(4 * 1024 * 1024 * 1024), 4096);
        assert_eq!(fat32_default_cluster(12 * 1024 * 1024 * 1024), 8192);
        assert_eq!(fat32_default_cluster(24 * 1024 * 1024 * 1024), 16384);
        assert_eq!(fat32_default_cluster(64 * 1024 * 1024 * 1024), 32768);
        assert_eq!(fat32_default_cluster(3 * 1024 * 1024 * 1024 * 1024), 65536);
    }

    #[test]
    fn test_ntfs_exfat_cluster_and_display_rules() {
        assert_eq!(ntfs_default_cluster(1024 * 1024 * 1024), 4096);
        assert_eq!(ntfs_default_cluster(20u64 << 40), 8192);
        assert_eq!(exfat_default_cluster(128 * 1024 * 1024), 4096);
        assert_eq!(exfat_default_cluster(16 * 1024 * 1024 * 1024), 32768);
        assert_eq!(exfat_default_cluster(64 * 1024 * 1024 * 1024), 131072);
        assert_eq!(format_cluster_label(8192, None), "8192 bytes");
        assert_eq!(format_cluster_label(16384, None), "16 kilobytes");
        assert_eq!(
            format_cluster_label(4096, Some("Por defecto")),
            "4096 bytes (Por defecto)"
        );
    }

    #[test]
    fn test_validate_fat32_bounds() {
        assert!(validate_fat32(16 * 1024 * 1024, 512, 0).is_err());
        assert_eq!(validate_fat32(8 * 1024 * 1024 * 1024, 512, 0).unwrap(), 8192);
        assert!(validate_fat32(FAT32_MAX_VOLUME_BYTES + 1, 512, 0).is_err());
    }

    #[test]
    fn test_normalize_label_rules() {
        assert_eq!(normalize_label("", true, ""), "NO_LABEL");
        assert_eq!(normalize_label("Ubuntu 24.04 LTS", true, "USB"), "Ubuntu 24_0");
        assert_eq!(normalize_label("?????", true, "My.Drive"), "My_Drive");
        assert_eq!(
            normalize_label("Linux_Installer_Very_Long_Name_Here", false, "").len(),
            32
        );
    }

    #[test]
    fn test_forced_dd_and_persistence_patterns() {
        assert!(forced_dd_label("Fedora-WS-Live-39-1-5"));
        assert!(forced_dd_label("AlmaLinux-9-4-x86_64-dvd"));
        assert!(forced_dd_label("Rocky-8-10-x86_64-dvd"));
        assert!(forced_dd_label("openSUSE-Tumbleweed-DVD-x86_64"));
        assert!(!forced_dd_label("UBUNTU 24_04 LTS"));
        assert_eq!(
            detect_persistence_style("ubuntu-24.04-desktop-amd64.iso"),
            PersistenceStyle::UbuntuCasper
        );
        assert_eq!(
            detect_persistence_style("kali-linux-2024.3-live-amd64.iso"),
            PersistenceStyle::DebianLive
        );
    }
}
