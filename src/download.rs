//! Curated official Linux/OS ISO catalog and streaming background downloader.

use crate::devices::cmd_in_path;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Clone, Copy, Debug)]
pub struct IsoCatalogItem {
    pub name: &'static str,
    pub edition: &'static str,
    pub arch: &'static str,
    pub filename: &'static str,
    pub url: &'static str,
    pub is_web_portal: bool,
}

pub const ISO_CATALOG: &[IsoCatalogItem] = &[
    IsoCatalogItem {
        name: "Ubuntu 24.04.2 LTS",
        edition: "Desktop (Noble Numbat)",
        arch: "x86_64",
        filename: "ubuntu-24.04.2-desktop-amd64.iso",
        url: "https://releases.ubuntu.com/24.04/ubuntu-24.04.2-desktop-amd64.iso",
        is_web_portal: false,
    },
    IsoCatalogItem {
        name: "Debian 12 (Bookworm)",
        edition: "Netinst ISO",
        arch: "x86_64",
        filename: "debian-12.9.0-amd64-netinst.iso",
        url: "https://cdimage.debian.org/debian-cd/current/amd64/iso-cd/debian-12.9.0-amd64-netinst.iso",
        is_web_portal: false,
    },
    IsoCatalogItem {
        name: "Arch Linux",
        edition: "Latest Monthly Live ISO",
        arch: "x86_64",
        filename: "archlinux-x86_64.iso",
        url: "https://geo.mirror.pkgbuild.com/iso/latest/archlinux-x86_64.iso",
        is_web_portal: false,
    },
    IsoCatalogItem {
        name: "openSUSE Tumbleweed",
        edition: "Network Installer",
        arch: "x86_64",
        filename: "openSUSE-Tumbleweed-NET-x86_64-Current.iso",
        url: "https://download.opensuse.org/tumbleweed/iso/openSUSE-Tumbleweed-NET-x86_64-Current.iso",
        is_web_portal: false,
    },
    IsoCatalogItem {
        name: "Alpine Linux",
        edition: "Standard Live ISO",
        arch: "x86_64",
        filename: "alpine-standard-3.21.3-x86_64.iso",
        url: "https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/x86_64/alpine-standard-3.21.3-x86_64.iso",
        is_web_portal: false,
    },
    IsoCatalogItem {
        name: "FreeDOS 1.3",
        edition: "Bonus / Live USB Image",
        arch: "x86",
        filename: "FD13-LiveCD.zip",
        url: "https://www.ibiblio.org/pub/micro/pc-stuff/freedos/files/distributions/1.3/official/FD13-LiveCD.zip",
        is_web_portal: false,
    },
    IsoCatalogItem {
        name: "GParted Live",
        edition: "Partition Editor Rescue ISO",
        arch: "x86_64",
        filename: "gparted-live-amd64.iso",
        url: "https://gparted.org/download.php",
        is_web_portal: true,
    },
    IsoCatalogItem {
        name: "Clonezilla Live",
        edition: "Disk Imaging & Cloning ISO",
        arch: "x86_64",
        filename: "clonezilla-live-amd64.iso",
        url: "https://clonezilla.org/downloads.php",
        is_web_portal: true,
    },
    IsoCatalogItem {
        name: "Linux Mint",
        edition: "Official Download Page",
        arch: "x86_64",
        filename: "linuxmint-cinnamon-64bit.iso",
        url: "https://linuxmint.com/download.php",
        is_web_portal: true,
    },
    IsoCatalogItem {
        name: "Kali Linux",
        edition: "Live / Installer Download",
        arch: "x86_64 / ARM64",
        filename: "kali-linux-live-amd64.iso",
        url: "https://www.kali.org/get-kali/#kali-live",
        is_web_portal: true,
    },
    IsoCatalogItem {
        name: "Windows 11 (Microsoft)",
        edition: "Official Download Page",
        arch: "x86_64 / ARM64",
        filename: "Win11_x64.iso",
        url: "https://www.microsoft.com/software-download/windows11",
        is_web_portal: true,
    },
    IsoCatalogItem {
        name: "Windows 10 (Microsoft)",
        edition: "Official Download Page",
        arch: "x86_64",
        filename: "Win10_x64.iso",
        url: "https://www.microsoft.com/software-download/windows10ISO",
        is_web_portal: true,
    },
    IsoCatalogItem {
        name: "Fedora Workstation",
        edition: "Official Download Page",
        arch: "x86_64 / ARM64",
        filename: "Fedora-Workstation-Live.iso",
        url: "https://fedoraproject.org/workstation/download",
        is_web_portal: true,
    },
];

#[derive(Default)]
pub struct DownloadState {
    pub open: bool,
    pub selected_idx: usize,
    pub running: bool,
    pub progress: f32,
    pub status: String,
    pub error: Option<String>,
    pub completed_path: Option<PathBuf>,
}

pub fn start_iso_download(
    state: Arc<Mutex<DownloadState>>,
    cancel: Arc<AtomicBool>,
    item: IsoCatalogItem,
    dest_path: PathBuf,
) {
    if item.is_web_portal {
        let _ = Command::new("xdg-open").arg(item.url).spawn();
        if let Ok(mut s) = state.lock() {
            s.status = format!("Opened official download page: {}", item.url);
        }
        return;
    }

    if !cmd_in_path("curl") {
        if let Ok(mut s) = state.lock() {
            s.error = Some("curl is not installed on this system.".into());
        }
        return;
    }

    cancel.store(false, Ordering::SeqCst);
    {
        let mut s = state.lock().unwrap();
        s.running = true;
        s.progress = 0.0;
        s.error = None;
        s.completed_path = None;
        s.status = format!("Downloading {}...", item.name);
    }

    thread::spawn(move || {
        let mut child = match Command::new("curl")
            .args([
                "-fSL",
                "--progress-bar",
                "-o",
                &dest_path.display().to_string(),
                item.url,
            ])
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let mut s = state.lock().unwrap();
                s.running = false;
                s.error = Some(format!("Failed starting curl: {}", e));
                return;
            }
        };

        let stderr = child.stderr.take();
        if let Some(se) = stderr {
            let mut reader = BufReader::new(se);
            let mut buf = Vec::new();
            loop {
                if cancel.load(Ordering::SeqCst) {
                    let _ = child.kill();
                    break;
                }
                buf.clear();
                match reader.read_until(b'\r', &mut buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        let txt = String::from_utf8_lossy(&buf);
                        if let Some(pct) = parse_curl_percent(&txt) {
                            let mut s = state.lock().unwrap();
                            s.progress = (pct / 100.0).clamp(0.0, 1.0);
                            s.status = format!("{} — {:.1}%", item.name, pct);
                        }
                    }
                    Err(_) => break,
                }
            }
        }

        let status = child.wait();
        let mut s = state.lock().unwrap();
        s.running = false;
        if cancel.load(Ordering::SeqCst) {
            let _ = std::fs::remove_file(&dest_path);
            s.status = "Cancelled".into();
        } else if let Ok(st) = status {
            if st.success() {
                s.progress = 1.0;
                s.completed_path = Some(dest_path);
                s.status = format!("Downloaded {}", item.filename);
            } else {
                let _ = std::fs::remove_file(&dest_path);
                s.error = Some(format!("Download failed (exit {}).", st));
            }
        } else {
            s.error = Some("Download process failed.".into());
        }
    });
}

fn parse_curl_percent(line: &str) -> Option<f32> {
    let idx = line.rfind('%')?;
    let before = line[..idx].trim_end();
    let num_str: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    num_str.parse::<f32>().ok()
}
