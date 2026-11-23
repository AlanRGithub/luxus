//! Pure-Rust fixed Microsoft VHD (Connectix) 512-byte footer builder & reader
//! for USB-to-VHD backups and VHD-to-USB restoration.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Detects whether `path` is a fixed Microsoft VHD disk image (trailing 512-byte `conectix`
/// footer with disk type 2) and returns the raw disk payload size (`file_size - 512`).
pub fn detect_fixed_vhd_payload_size(path: &Path) -> Option<u64> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len <= 512 {
        return None;
    }
    f.seek(SeekFrom::Start(len - 512)).ok()?;
    let mut footer = [0u8; 512];
    f.read_exact(&mut footer).ok()?;
    if &footer[0..8] != b"conectix" {
        return None;
    }
    let disk_type = u32::from_be_bytes([footer[60], footer[61], footer[62], footer[63]]);
    if disk_type != 2 {
        return None;
    }
    Some(len - 512)
}

/// Builds a spec-compliant 512-byte fixed-disk VHD footer (`conectix` magic, CHS geometry,
/// big-endian fields, and one's-complement checksum).
pub fn build_fixed_vhd_footer(disk_bytes: u64) -> [u8; 512] {
    let mut footer = [0u8; 512];

    // Cookie: "conectix" (0..8)
    footer[0..8].copy_from_slice(b"conectix");
    // Features: 0x00000002 (Reserved, must always be set) (8..12)
    footer[8..12].copy_from_slice(&2u32.to_be_bytes());
    // File Format Version: 0x00010000 (12..16)
    footer[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    // Data Offset: 0xFFFFFFFFFFFFFFFF for fixed disk (16..24)
    footer[16..24].copy_from_slice(&u64::MAX.to_be_bytes());

    // Time Stamp: seconds since Jan 1, 2000 12:00:00 AM UTC (24..28)
    const VHD_EPOCH_OFFSET: u64 = 946_684_800;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let vhd_ts = now.saturating_sub(VHD_EPOCH_OFFSET) as u32;
    footer[24..28].copy_from_slice(&vhd_ts.to_be_bytes());

    // Creator Application: "lux " (28..32)
    footer[28..32].copy_from_slice(b"lux ");
    // Creator Version: 1.0 (32..36)
    footer[32..36].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    // Creator Host OS: "Wi2k" / Linux compatible standard tag (36..40)
    footer[36..40].copy_from_slice(b"Wi2k");

    // Original Size & Current Size (40..56)
    footer[40..48].copy_from_slice(&disk_bytes.to_be_bytes());
    footer[48..56].copy_from_slice(&disk_bytes.to_be_bytes());

    // Disk Geometry (CHS per Microsoft VHD Image Format Specification) (56..60)
    let (cyls, heads, spt) = compute_vhd_chs(disk_bytes);
    footer[56..58].copy_from_slice(&cyls.to_be_bytes());
    footer[58] = heads;
    footer[59] = spt;

    // Disk Type: 2 = Fixed hard disk (60..64)
    footer[60..64].copy_from_slice(&2u32.to_be_bytes());

    // Unique ID (16 bytes at 68..84) derived deterministically from timestamp & size
    let mut seed = now ^ disk_bytes.rotate_left(17);
    for b in &mut footer[68..84] {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = (seed >> 56) as u8;
    }

    // Checksum (64..68): one's complement of the sum of all bytes excluding checksum field
    let sum: u32 = footer.iter().map(|&b| b as u32).sum();
    let checksum = !sum;
    footer[64..68].copy_from_slice(&checksum.to_be_bytes());

    footer
}

fn compute_vhd_chs(disk_bytes: u64) -> (u16, u8, u8) {
    let mut total_sectors = (disk_bytes / 512).min(65535 * 16 * 255);
    let (sectors_per_track, heads, cylinder_times_heads);
    if total_sectors >= 65535 * 16 * 63 {
        sectors_per_track = 255u64;
        heads = 16u64;
        cylinder_times_heads = total_sectors / sectors_per_track;
    } else {
        sectors_per_track = 17u64;
        let mut cth = total_sectors / sectors_per_track;
        let mut h = (cth + 1023) / 1024;
        if h < 4 {
            h = 4;
        }
        if cth >= (h * 1024) || h > 16 {
            let spt2 = 31u64;
            h = 16;
            cth = total_sectors / spt2;
            if cth >= (h * 1024) {
                let spt3 = 63u64;
                h = 16;
                cth = total_sectors / spt3;
                let cyls = (cth / h).min(65535) as u16;
                return (cyls, h as u8, spt3 as u8);
            }
            let cyls = (cth / h).min(65535) as u16;
            return (cyls, h as u8, spt2 as u8);
        }
        heads = h;
        cylinder_times_heads = cth;
        let _ = &mut total_sectors;
    }
    let cyls = (cylinder_times_heads / heads).min(65535) as u16;
    (cyls, heads as u8, sectors_per_track as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vhd_footer_valid_checksum() {
        let f = build_fixed_vhd_footer(16 * 1024 * 1024 * 1024);
        assert_eq!(&f[0..8], b"conectix");
        assert_eq!(u32::from_be_bytes([f[60], f[61], f[62], f[63]]), 2);
        let stored = u32::from_be_bytes([f[64], f[65], f[66], f[67]]);
        let mut sum: u32 = 0;
        for (i, &b) in f.iter().enumerate() {
            if !(64..68).contains(&i) {
                sum = sum.wrapping_add(b as u32);
            }
        }
        assert_eq!(stored, !sum);
    }
}
