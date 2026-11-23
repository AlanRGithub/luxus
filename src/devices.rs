//! Zero-subprocess cross-distro Linux USB device discovery via /sys/block & /proc/mounts
//! with automatic lsblk fallback and safety filtering for removable storage.

use crate::mechanics::{human_size, MAX_CARD_BYTES, MAX_DRIVES, MIN_SECTOR_BYTES};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub path: String,
    pub name: String,
    pub vendor: String,
    pub model: String,
    pub size: String,
    pub size_bytes: u64,
    pub sector_bytes: u64,
    pub tran: String,
    pub removable: bool,
    pub mounts: Vec<String>,
}

impl DeviceInfo {
    /// Compact dropdown formatting: `Model (/dev/sdb) [32 GB]`
    pub fn display_label(&self) -> String {
        let title = match (self.vendor.is_empty(), self.model.is_empty()) {
            (false, false) => {
                if self
                    .model
                    .to_lowercase()
                    .starts_with(&self.vendor.to_lowercase())
                {
                    self.model.clone()
                } else {
                    format!("{} {}", self.vendor, self.model)
                }
            }
            (true, false) => self.model.clone(),
            (false, true) => self.vendor.clone(),
            (true, true) => "USB Storage".to_string(),
        };
        format!("{} ({}) [{}]", title, self.path, self.size)
    }
}

pub fn screenshot_demo_device() -> DeviceInfo {
    DeviceInfo {
        path: "/dev/sdb".into(),
        name: "sdb".into(),
        vendor: "SanDisk".into(),
        model: "Ultra USB 3.0".into(),
        size: "32 GB".into(),
        size_bytes: 32_000_000_000,
        sector_bytes: 512,
        tran: "usb".into(),
        removable: true,
        mounts: vec![],
    }
}

/// Strict check: returns true only if `candidate` is the exact `disk` path
/// or one of its partitions (e.g., `/dev/sdb1`, `/dev/nvme0n1p1`, `/dev/mmcblk0p2`),
/// preventing prefix collisions like `/dev/sda` matching `/dev/sdaa1`.
pub fn is_same_or_partition_of(candidate: &str, disk: &str) -> bool {
    if candidate == disk {
        return true;
    }
    let Some(suffix) = candidate.strip_prefix(disk) else {
        return false;
    };
    let bytes = suffix.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    if bytes[0].is_ascii_digit() {
        return bytes.iter().all(|b| b.is_ascii_digit());
    }
    if bytes[0] == b'p' && bytes.len() >= 2 {
        return bytes[1..].iter().all(|b| b.is_ascii_digit());
    }
    false
}

/// Decodes `/proc/mounts` octal escape sequences (`\040` -> space, `\011` -> tab, `\134` -> `\`).
pub fn unescape_proc_mount(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let d0 = bytes[i + 1];
            let d1 = bytes[i + 2];
            let d2 = bytes[i + 3];
            if (b'0'..=b'7').contains(&d0)
                && (b'0'..=b'7').contains(&d1)
                && (b'0'..=b'7').contains(&d2)
            {
                let val = ((d0 - b'0') << 6) | ((d1 - b'0') << 3) | (d2 - b'0');
                out.push(val as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Fast zero-subprocess mount map parsed directly from `/proc/mounts`.
fn read_proc_mounts() -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let Ok(text) = fs::read_to_string("/proc/mounts") else {
        return map;
    };
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(src) = parts.next() else { continue };
        let Some(mp_raw) = parts.next() else { continue };
        if !src.starts_with("/dev/") {
            continue;
        }
        let mp = unescape_proc_mount(mp_raw);
        let dev_real = fs::canonicalize(src)
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()))
            .unwrap_or_else(|| src.to_string());
        map.entry(dev_real).or_default().push(mp);
    }
    map
}

fn read_trimmed(path: &Path) -> String {
    fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Pure-Rust `/sys/block` enumeration — works on every Linux distro (Arch, Debian, Ubuntu,
/// Fedora, openSUSE, Alpine, Void, NixOS, Gentoo) without spawning subprocesses.
pub fn list_usb_devices(list_usb_hard_drives: bool) -> Result<Vec<DeviceInfo>, String> {
    if let Ok(devs) = list_via_sysfs(list_usb_hard_drives) {
        if !devs.is_empty() || Path::new("/sys/block").exists() {
            return Ok(devs);
        }
    }
    list_via_lsblk(list_usb_hard_drives)
}

fn list_via_sysfs(list_usb_hard_drives: bool) -> Result<Vec<DeviceInfo>, String> {
    let entries = fs::read_dir("/sys/block").map_err(|e| e.to_string())?;
    let mounts_map = read_proc_mounts();
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();

    let mut devs = Vec::new();
    for name in names {
        // Skip virtual/loop/ram/zram/dm/sr/md devices
        if name.starts_with("loop")
            || name.starts_with("ram")
            || name.starts_with("zram")
            || name.starts_with("dm-")
            || name.starts_with("md")
            || name.starts_with("sr")
            || name.starts_with("fd")
            || name.starts_with("nbd")
        {
            continue;
        }

        let sys_dir = Path::new("/sys/block").join(&name);
        let sectors: u64 = read_trimmed(&sys_dir.join("size")).parse().unwrap_or(0);
        if sectors == 0 {
            continue;
        }
        // Linux sysfs `size` is always in 512-byte sectors.
        let size_bytes = sectors.saturating_mul(512);
        let sector_bytes: u64 = read_trimmed(&sys_dir.join("queue/logical_block_size"))
            .parse::<u64>()
            .unwrap_or(MIN_SECTOR_BYTES)
            .max(MIN_SECTOR_BYTES);

        let rm_flag = read_trimmed(&sys_dir.join("removable")) == "1";

        // Inspect canonical device path to detect USB / MMC / NVMe / SATA transport
        let dev_link = fs::canonicalize(sys_dir.join("device"))
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_lowercase()))
            .unwrap_or_default();
        let is_usb = dev_link.contains("/usb");
        let is_mmc = name.starts_with("mmcblk") || dev_link.contains("/mmc");
        let tran = if is_usb {
            "usb".to_string()
        } else if is_mmc {
            "mmc".to_string()
        } else if name.starts_with("nvme") {
            "nvme".to_string()
        } else {
            "sata".to_string()
        };

        let removable = rm_flag || is_usb;
        let vendor = read_trimmed(&sys_dir.join("device/vendor"));
        let model = read_trimmed(&sys_dir.join("device/model"));
        let dev_path = format!("/dev/{}", name);

        // Collect mounts matching `/dev/<name>` or its real partitions (avoiding /dev/sda vs /dev/sdaa collisions)
        let mut mounts = Vec::new();
        for (k, v) in &mounts_map {
            if is_same_or_partition_of(k, &dev_path) {
                for m in v {
                    mounts.push(m.clone());
                }
            }
        }
        mounts.sort();
        mounts.dedup();

        // Protect root/boot/system partitions unconditionally
        let has_system_mount = mounts
            .iter()
            .any(|m| m == "/" || m == "/boot" || m == "/boot/efi" || m == "/home" || m == "/usr");
        if has_system_mount {
            continue;
        }

        // Removable-storage safety filters:
        // 1) Eliminate non-removable devices that contain mounted partitions
        if !removable && !mounts.is_empty() {
            continue;
        }
        // 2) Unless "List USB Hard Drives" is enabled:
        //    - keep USB removable / SD cards below 500 GiB card ceiling
        //    - eliminate non-USB devices and large non-removable USB HDDs
        if !list_usb_hard_drives {
            if !is_usb && !(is_mmc && rm_flag) {
                continue;
            }
            if !rm_flag && size_bytes > MAX_CARD_BYTES {
                continue;
            }
            if size_bytes > MAX_CARD_BYTES && is_mmc {
                continue;
            }
        } else if !is_usb && !is_mmc {
            // Even with "List USB Hard Drives" enabled, never show internal NVMe/SATA boot disks
            continue;
        }

        devs.push(DeviceInfo {
            path: dev_path,
            name,
            vendor,
            model,
            size: human_size(size_bytes),
            size_bytes,
            sector_bytes,
            tran,
            removable,
            mounts,
        });

        if devs.len() >= MAX_DRIVES {
            eprintln!(
                "WARNING: Found more than {} drives - ignoring remaining ones...",
                MAX_DRIVES
            );
            break;
        }
    }

    Ok(devs)
}

#[derive(Clone, Debug, Deserialize)]
struct LsblkOutput {
    blockdevices: Vec<BlockDev>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize, Default)]
struct BlockDev {
    name: Option<String>,
    path: Option<String>,
    size: Option<serde_json::Value>,
    model: Option<String>,
    vendor: Option<String>,
    tran: Option<String>,
    rm: Option<serde_json::Value>,
    #[serde(rename = "type")]
    dtype: Option<String>,
    mountpoint: Option<String>,
    mountpoints: Option<Vec<Option<String>>>,
    children: Option<Vec<BlockDev>>,
}

fn val_to_string(v: &Option<serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Bool(b)) => if *b { "1" } else { "0" }.into(),
        _ => String::new(),
    }
}

fn collect_mounts(dev: &BlockDev, out: &mut Vec<String>) {
    if let Some(m) = &dev.mountpoint {
        if !m.is_empty() {
            out.push(m.clone());
        }
    }
    if let Some(mps) = &dev.mountpoints {
        for m in mps.iter().flatten() {
            if !m.is_empty() {
                out.push(m.clone());
            }
        }
    }
    if let Some(ch) = &dev.children {
        for c in ch {
            collect_mounts(c, out);
        }
    }
}

fn list_via_lsblk(list_usb_hard_drives: bool) -> Result<Vec<DeviceInfo>, String> {
    let out = Command::new("lsblk")
        .args([
            "--json",
            "-b",
            "-o",
            "NAME,PATH,SIZE,VENDOR,MODEL,TRAN,RM,TYPE,MOUNTPOINT,MOUNTPOINTS",
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    let parsed: LsblkOutput = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let mut devs = Vec::new();
    for b in parsed.blockdevices {
        if b.dtype.as_deref() != Some("disk") {
            continue;
        }
        let tran = b.tran.clone().unwrap_or_default().to_lowercase();
        let rm = val_to_string(&b.rm);
        let is_usb = tran == "usb" || tran.starts_with("usb ");
        let removable = rm == "1" || rm == "true" || is_usb;
        let name = b.name.clone().unwrap_or_default();
        let path = b
            .path
            .clone()
            .unwrap_or_else(|| format!("/dev/{}", name));
        let vendor = b.vendor.clone().unwrap_or_default().trim().to_string();
        let model = b.model.clone().unwrap_or_default().trim().to_string();
        let size_bytes: u64 = match &b.size {
            Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
            Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
            _ => 0,
        };
        let mut mounts = Vec::new();
        collect_mounts(&b, &mut mounts);
        mounts.sort();
        mounts.dedup();
        if mounts.iter().any(|m| m == "/" || m == "/boot" || m == "/boot/efi") {
            continue;
        }
        if !removable && !mounts.is_empty() {
            continue;
        }
        if !list_usb_hard_drives {
            if size_bytes > MAX_CARD_BYTES && removable {
                continue;
            }
            if !is_usb {
                continue;
            }
        } else if !is_usb {
            continue;
        }
        devs.push(DeviceInfo {
            path,
            name,
            vendor,
            model,
            size: human_size(size_bytes),
            size_bytes,
            sector_bytes: MIN_SECTOR_BYTES,
            tran,
            removable,
            mounts,
        });
        if devs.len() >= MAX_DRIVES {
            break;
        }
    }
    Ok(devs)
}

/// Checks available Linux system utilities for diagnostics.
pub struct SystemToolStatus {
    pub name: &'static str,
    pub purpose: &'static str,
    pub available: bool,
}

pub fn probe_system_tools() -> Vec<SystemToolStatus> {
    let tools = [
        ("pkexec", "Polkit GUI elevation"),
        ("sudo", "CLI elevation"),
        ("parted", "MBR / GPT partitioning"),
        ("sfdisk", "Partitioning & MBR ESP type"),
        ("wipefs", "Signature cleanup"),
        ("mkfs.vfat", "FAT32 / ESP formatting"),
        ("mkfs.ntfs", "NTFS (Windows USB)"),
        ("mkfs.exfat", "exFAT formatting"),
        ("mkfs.udf", "UDF formatting"),
        ("mkfs.ext4", "ext4 / Linux persistence"),
        ("7z", "UDF / Windows ISO & .zip extraction"),
        ("bsdtar", "Fallback ISO archive extraction"),
        ("syslinux", "Legacy BIOS Linux/DOS boot"),
        ("ms-sys", "Legacy BIOS Windows MBR boot"),
        ("grub2-mkstandalone", "Offline UEFI:NTFS chainloader"),
        ("curl", "ISO & UEFI:NTFS download"),
        ("xzcat", "Streaming .xz/.lzma decompression"),
        ("zstdcat", "Streaming .zst decompression"),
        ("unzip", "Streaming .zip decompression"),
        ("udisksctl", "Desktop unmount & safe power-off"),
    ];
    tools
        .iter()
        .map(|&(name, purpose)| SystemToolStatus {
            name,
            purpose,
            available: cmd_in_path(name),
        })
        .collect()
}

/// Safely unmounts all partitions on `dev_path` and powers off / ejects the USB drive.
pub fn eject_usb_device(dev_path: &str) -> Result<String, String> {
    let _ = Command::new("sync").status();
    if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
        for line in mounts.lines() {
            let mut parts = line.split_whitespace();
            let Some(src) = parts.next() else { continue };
            let real_src = fs::canonicalize(src)
                .ok()
                .and_then(|p| p.to_str().map(|s| s.to_string()))
                .unwrap_or_else(|| src.to_string());
            if is_same_or_partition_of(&real_src, dev_path) {
                if cmd_in_path("udisksctl") {
                    let _ = Command::new("udisksctl")
                        .args(["unmount", "-b", &real_src])
                        .output();
                }
                let _ = Command::new("umount").arg(&real_src).output();
            }
        }
    }
    if cmd_in_path("udisksctl") {
        let out = Command::new("udisksctl")
            .args(["power-off", "-b", dev_path])
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            return Ok(format!("{} powered off safely.", dev_path));
        }
    }
    if cmd_in_path("eject") {
        let out = Command::new("eject")
            .arg(dev_path)
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            return Ok(format!("{} ejected safely.", dev_path));
        }
    }
    Ok(format!("{} synced and unmounted.", dev_path))
}

pub fn cmd_in_path(cmd: &str) -> bool {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let candidate = Path::new(dir).join(cmd);
            if candidate.is_file() {
                return true;
            }
        }
    }
    // Also check standard sbin directories across distros (Debian/Ubuntu/Fedora/Arch)
    for sbin in ["/usr/sbin", "/sbin", "/usr/local/sbin"] {
        if Path::new(sbin).join(cmd).is_file() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partition_matching_avoids_prefix_collision() {
        assert!(is_same_or_partition_of("/dev/sda", "/dev/sda"));
        assert!(is_same_or_partition_of("/dev/sda1", "/dev/sda"));
        assert!(is_same_or_partition_of("/dev/sda12", "/dev/sda"));
        assert!(is_same_or_partition_of("/dev/mmcblk0p1", "/dev/mmcblk0"));
        assert!(is_same_or_partition_of("/dev/nvme0n1p2", "/dev/nvme0n1"));
        assert!(!is_same_or_partition_of("/dev/sdaa", "/dev/sda"));
        assert!(!is_same_or_partition_of("/dev/sdaa1", "/dev/sda"));
        assert!(!is_same_or_partition_of("/dev/sdb1", "/dev/sda"));
    }

    #[test]
    fn test_unescape_proc_mount() {
        assert_eq!(
            unescape_proc_mount("/run/media/user/Mi\\040USB"),
            "/run/media/user/Mi USB"
        );
    }
}
