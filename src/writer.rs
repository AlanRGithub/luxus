//! Luxus native Rust engine:
//! - Double-buffered 4 MiB streaming raw & compressed (.gz/.xz/.zst/.bz2) image writer
//! - Windows 10/11 bootable USB creator with UEFI:NTFS secondary ESP + autounattend.xml
//! - Linux Live persistence partition creator (casper-rw / persistence)
//! - USB-to-IMG and USB-to-VHD (Microsoft Connectix footer) backup engine
//! - Multi-pass bad-blocks & fake-capacity flash checker
//! - Cross-distro partitioning/formatting (FAT32, Large FAT32, NTFS, UDF, exFAT, ext4/3/2)
//! - Single-elevation privileged worker (pkexec / sudo / doas)

use crate::devices::{cmd_in_path, is_same_or_partition_of, unescape_proc_mount, DeviceInfo};
use crate::hash::Sha256;
use crate::mechanics::{
    human_size, inspect_image_file, normalize_label, validate_fat32, CompressionKind,
    PersistenceStyle, BADBLOCKS_BATCH_BLOCKS, BADBLOCKS_BLOCK_BYTES, BADBLOCKS_MAX_ERRORS,
    BADBLOCKS_PATTERNS, DD_MODE_MARKER, DEFAULT_LABEL, FAT32_LBA_MBR_TYPE, GPT_EFI_START_BYTES,
    LARGE_DRIVE_BYTES, MIN_SECTOR_BYTES, UEFI_MBR_DISK_ID, UEFI_NTFS_PART_BYTES,
};
use crate::vhd::{build_fixed_vhd_footer, detect_fixed_vhd_payload_size};
use crate::windows::{
    install_uefi_ntfs_partition, write_autounattend_to_mount, WindowsUxOptions,
};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BootMode {
    NonBootable,
    FreeDos,
    IsoImage,
    DdImage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageWriteOption {
    WindowsStandard,
    HybridRaw,
    IsoExtract,
    DdStrict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartitionScheme {
    Mbr,
    Gpt,
}

impl PartitionScheme {
    pub fn label(&self) -> &'static str {
        match self {
            PartitionScheme::Mbr => "MBR",
            PartitionScheme::Gpt => "GPT",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetSystem {
    Uefi,
    BiosUefi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileSystemKind {
    Fat32,
    LargeFat32,
    Ntfs,
    Udf,
    Exfat,
    Ext4,
    Ext3,
    Ext2,
}

impl FileSystemKind {
    pub fn label(&self) -> &'static str {
        match self {
            FileSystemKind::Fat32 => "FAT32",
            FileSystemKind::LargeFat32 => "Large FAT32",
            FileSystemKind::Ntfs => "NTFS",
            FileSystemKind::Udf => "UDF",
            FileSystemKind::Exfat => "exFAT",
            FileSystemKind::Ext4 => "ext4",
            FileSystemKind::Ext3 => "ext3",
            FileSystemKind::Ext2 => "ext2",
        }
    }

    pub fn is_fat32(&self) -> bool {
        matches!(self, FileSystemKind::Fat32 | FileSystemKind::LargeFat32)
    }

    pub fn needs_uefi_ntfs(&self) -> bool {
        matches!(
            self,
            FileSystemKind::Ntfs | FileSystemKind::Exfat | FileSystemKind::Udf
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobSpec {
    pub device: DeviceInfo,
    pub boot_mode: BootMode,
    pub image_option: ImageWriteOption,
    pub image_path: String,
    pub partition_scheme: PartitionScheme,
    pub target_system: TargetSystem,
    pub filesystem: FileSystemKind,
    pub cluster_size: u32,
    pub volume_label: String,
    pub quick_format: bool,
    pub extended_label_icon: bool,
    pub check_bad_blocks: bool,
    pub bad_blocks_passes: u8,
    pub ntfs_compression: bool,
    pub old_bios_fixes: bool,
    pub use_uefi_mbr_id: bool,
    pub enable_uefi_ntfs: bool,
    pub persistence_gb: u32,
    pub persistence_style: PersistenceStyle,
    pub windows_ux: Option<WindowsUxOptions>,
    pub verify_after_write: bool,
    pub buffer_mb: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WipeSpec {
    pub device: DeviceInfo,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackupSpec {
    pub device: DeviceInfo,
    pub output_path: String,
    pub as_vhd: bool,
    pub buffer_mb: usize,
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub ts: String,
    pub msg: String,
}

#[derive(Clone, Debug)]
pub struct JobState {
    pub running: bool,
    pub progress: f32,
    pub status: String,
    pub speed_text: String,
    pub logs: Vec<LogLine>,
    pub done: bool,
    pub cancelled: bool,
    pub error: Option<String>,
    pub started_at: Option<Instant>,
    pub elapsed_secs: u64,
}

impl Default for JobState {
    fn default() -> Self {
        Self {
            running: false,
            progress: 0.0,
            status: String::new(),
            speed_text: String::new(),
            logs: Vec::new(),
            done: false,
            cancelled: false,
            error: None,
            started_at: None,
            elapsed_secs: 0,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum WorkerEvent {
    Progress {
        progress: f32,
        status: String,
        speed: String,
    },
    Log {
        msg: String,
    },
    Done,
    Error {
        msg: String,
    },
}

pub fn now_hms() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    )
}

pub fn is_root() -> bool {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(2))
                .map(|euid| euid == "0")
        })
        .unwrap_or(false)
}

pub fn format_elapsed(secs: u64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

fn push_log(j: &mut JobState, msg: String) {
    j.logs.push(LogLine { ts: now_hms(), msg });
}

/// Launches a format/write job from the GUI using a single elevation prompt if not root.
pub fn spawn_gui_job(job: Arc<Mutex<JobState>>, cancel: Arc<AtomicBool>, spec: JobSpec) {
    cancel.store(false, Ordering::SeqCst);
    {
        let mut j = job.lock().unwrap();
        j.running = true;
        j.progress = 0.0;
        j.done = false;
        j.cancelled = false;
        j.error = None;
        j.speed_text.clear();
        j.started_at = Some(Instant::now());
        j.elapsed_secs = 0;
        j.status = "Iniciando...".into();
        push_log(
            &mut j,
            format!(
                "Dispositivo seleccionado: {} [{}]",
                spec.device.display_label(),
                spec.device.size
            ),
        );
        if !spec.image_path.is_empty() {
            push_log(&mut j, format!("Imagen: {}", spec.image_path));
        }
    }

    thread::spawn(move || {
        let outcome = if is_root() {
            execute_job_direct(&spec, &cancel, |ev| apply_event(&job, ev))
        } else {
            let json = match serde_json::to_string(&spec) {
                Ok(j) => j,
                Err(e) => {
                    finish_job_state(&job, &cancel, Err(e.to_string()));
                    return;
                }
            };
            run_elevated_cli("--worker-job", &json, &spec.device.path, &job, &cancel)
        };
        finish_job_state(&job, &cancel, outcome);
    });
}

/// Launches a USB-to-IMG/VHD backup job using a single elevation prompt if not root.
pub fn spawn_backup_job(job: Arc<Mutex<JobState>>, cancel: Arc<AtomicBool>, spec: BackupSpec) {
    cancel.store(false, Ordering::SeqCst);
    {
        let mut j = job.lock().unwrap();
        j.running = true;
        j.progress = 0.0;
        j.done = false;
        j.cancelled = false;
        j.error = None;
        j.speed_text.clear();
        j.started_at = Some(Instant::now());
        j.elapsed_secs = 0;
        j.status = "Iniciando respaldo...".into();
        push_log(
            &mut j,
            format!(
                "Respaldando {} -> {} ({})",
                spec.device.path,
                spec.output_path,
                if spec.as_vhd { "VHD" } else { "IMG" }
            ),
        );
    }

    thread::spawn(move || {
        let outcome = if is_root() {
            execute_backup_direct(&spec, &cancel, |ev| apply_event(&job, ev))
        } else {
            let json = match serde_json::to_string(&spec) {
                Ok(j) => j,
                Err(e) => {
                    finish_job_state(&job, &cancel, Err(e.to_string()));
                    return;
                }
            };
            run_elevated_cli("--backup-job", &json, &spec.device.path, &job, &cancel)
        };
        finish_job_state(&job, &cancel, outcome);
    });
}

/// Launches a fast zero-wipe job (`Alt+Z`) to clear partition tables & signatures on a USB drive.
pub fn spawn_wipe_job(job: Arc<Mutex<JobState>>, cancel: Arc<AtomicBool>, spec: WipeSpec) {
    cancel.store(false, Ordering::SeqCst);
    {
        let mut j = job.lock().unwrap();
        j.running = true;
        j.progress = 0.0;
        j.done = false;
        j.cancelled = false;
        j.error = None;
        j.speed_text.clear();
        j.started_at = Some(Instant::now());
        j.elapsed_secs = 0;
        j.status = "Limpiando unidad a ceros...".into();
        push_log(
            &mut j,
            format!(
                "Limpieza rápida de tablas y firmas (Alt+Z) en {} [{}]",
                spec.device.path, spec.device.size
            ),
        );
    }

    thread::spawn(move || {
        let outcome = if is_root() {
            execute_wipe_direct(&spec, &cancel, |ev| apply_event(&job, ev))
        } else {
            let json = match serde_json::to_string(&spec) {
                Ok(j) => j,
                Err(e) => {
                    finish_job_state(&job, &cancel, Err(e.to_string()));
                    return;
                }
            };
            run_elevated_cli("--wipe-job", &json, &spec.device.path, &job, &cancel)
        };
        finish_job_state(&job, &cancel, outcome);
    });
}

fn finish_job_state(
    job: &Arc<Mutex<JobState>>,
    cancel: &Arc<AtomicBool>,
    outcome: Result<(), String>,
) {
    let mut j = job.lock().unwrap();
    j.running = false;
    j.done = true;
    if let Some(st) = j.started_at {
        j.elapsed_secs = st.elapsed().as_secs();
    }
    match outcome {
        Ok(()) => {
            j.progress = 1.0;
            j.status = "COMPLETADO".into();
            j.speed_text.clear();
            push_log(&mut j, "Operación completada correctamente.".into());
        }
        Err(e) if cancel.load(Ordering::SeqCst) || e == "CANCELLED" => {
            j.cancelled = true;
            j.status = "CANCELADO".into();
            j.speed_text.clear();
            push_log(&mut j, "Operación cancelada por el usuario.".into());
        }
        Err(e) => {
            j.error = Some(e.clone());
            j.status = format!("Error: {}", e);
            j.speed_text.clear();
            push_log(&mut j, format!("ERROR: {}", e));
        }
    }
}

fn apply_event(job: &Arc<Mutex<JobState>>, ev: WorkerEvent) {
    let mut j = job.lock().unwrap();
    if let Some(st) = j.started_at {
        j.elapsed_secs = st.elapsed().as_secs();
    }
    match ev {
        WorkerEvent::Progress {
            progress,
            status,
            speed,
        } => {
            j.progress = progress.clamp(0.0, 1.0);
            j.status = status;
            j.speed_text = speed;
        }
        WorkerEvent::Log { msg } => push_log(&mut j, msg),
        WorkerEvent::Done => {
            j.progress = 1.0;
        }
        WorkerEvent::Error { msg } => {
            j.error = Some(msg.clone());
            push_log(&mut j, format!("ERROR: {}", msg));
        }
    }
}

fn run_elevated_cli(
    flag: &str,
    json: &str,
    dev_path: &str,
    job: &Arc<Mutex<JobState>>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("No se pudo localizar el binario actual: {}", e))?;
    let tmp_path = std::env::temp_dir().join(format!(
        "luxus-job-{}-{}.json",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    fs::write(&tmp_path, json).map_err(|e| format!("No se pudo escribir spec temporal: {}", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600));
    }

    let (elevator, mut args) = if cmd_in_path("pkexec") {
        ("pkexec", vec![exe.display().to_string()])
    } else if cmd_in_path("sudo") {
        ("sudo", vec![exe.display().to_string()])
    } else if cmd_in_path("doas") {
        ("doas", vec![exe.display().to_string()])
    } else {
        let _ = fs::remove_file(&tmp_path);
        return Err(
            "Se requieren privilegios de administrador (instale pkexec/polkit o ejecute con sudo)."
                .into(),
        );
    };
    args.push(flag.into());
    args.push(tmp_path.display().to_string());

    apply_event(
        job,
        WorkerEvent::Log {
            msg: format!(
                "Solicitando elevación única con {} para operación en {}...",
                elevator, dev_path
            ),
        },
    );

    let mut child = Command::new(elevator)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            let _ = fs::remove_file(&tmp_path);
            format!("No se pudo iniciar {}: {}", elevator, e)
        })?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let child_pid = child.id();
    let done_flag = Arc::new(AtomicBool::new(false));
    let done_watch = done_flag.clone();
    let cancel_killer = cancel.clone();
    let killer_thread = thread::spawn(move || {
        while !done_watch.load(Ordering::SeqCst) {
            if cancel_killer.load(Ordering::SeqCst) {
                let _ = Command::new("kill")
                    .args(["-TERM", &child_pid.to_string()])
                    .status();
                break;
            }
            thread::sleep(Duration::from_millis(120));
        }
    });

    let job_err = job.clone();
    let err_thread = thread::spawn(move || {
        let mut last_err = String::new();
        if let Some(se) = stderr {
            for line in BufReader::new(se).lines().flatten() {
                if !line.trim().is_empty() {
                    last_err = line.clone();
                    apply_event(&job_err, WorkerEvent::Log { msg: line });
                }
            }
        }
        last_err
    });

    let cancel_watch = cancel.clone();
    let mut worker_err: Option<String> = None;
    if let Some(so) = stdout {
        for line in BufReader::new(so).lines().flatten() {
            if cancel_watch.load(Ordering::SeqCst) {
                let _ = child.kill();
                break;
            }
            if let Some(payload) = line.strip_prefix("LUXUS_EV:") {
                if let Ok(ev) = serde_json::from_str::<WorkerEvent>(payload) {
                    if let WorkerEvent::Error { ref msg } = ev {
                        worker_err = Some(msg.clone());
                    }
                    apply_event(job, ev);
                }
            } else if !line.trim().is_empty() {
                apply_event(job, WorkerEvent::Log { msg: line });
            }
        }
    }

    let status = child.wait().map_err(|e| e.to_string())?;
    done_flag.store(true, Ordering::SeqCst);
    let _ = killer_thread.join();
    let stderr_tail = err_thread.join().unwrap_or_default();
    let _ = fs::remove_file(&tmp_path);

    if cancel.load(Ordering::SeqCst) {
        return Err("CANCELLED".into());
    }
    if let Some(e) = worker_err {
        return Err(e);
    }
    if !status.success() {
        if !stderr_tail.is_empty() {
            return Err(format!("Elevación/proceso falló: {}", stderr_tail));
        }
        return Err(format!("Proceso finalizó con código {}", status));
    }
    Ok(())
}

/// Entrypoint for `--worker-job <spec.json>` (runs as root under a single pkexec/sudo call).
pub fn run_worker_cli(spec_path: &Path) -> i32 {
    let emit = |ev: WorkerEvent| {
        if let Ok(line) = serde_json::to_string(&ev) {
            println!("LUXUS_EV:{}", line);
            let _ = std::io::stdout().flush();
        }
    };
    let Ok(raw) = fs::read_to_string(spec_path) else {
        emit(WorkerEvent::Error {
            msg: format!("No se pudo leer {}", spec_path.display()),
        });
        return 1;
    };
    let Ok(spec) = serde_json::from_str::<JobSpec>(&raw) else {
        emit(WorkerEvent::Error {
            msg: "Especificación de trabajo JSON inválida".into(),
        });
        return 1;
    };
    let cancel = Arc::new(AtomicBool::new(false));
    match execute_job_direct(&spec, &cancel, emit) {
        Ok(()) => {
            emit(WorkerEvent::Done);
            0
        }
        Err(e) => {
            emit(WorkerEvent::Error { msg: e });
            1
        }
    }
}

/// Entrypoint for `--backup-job <spec.json>` (USB -> IMG / VHD backup).
pub fn run_backup_cli(spec_path: &Path) -> i32 {
    let emit = |ev: WorkerEvent| {
        if let Ok(line) = serde_json::to_string(&ev) {
            println!("LUXUS_EV:{}", line);
            let _ = std::io::stdout().flush();
        }
    };
    let Ok(raw) = fs::read_to_string(spec_path) else {
        emit(WorkerEvent::Error {
            msg: format!("No se pudo leer {}", spec_path.display()),
        });
        return 1;
    };
    let Ok(spec) = serde_json::from_str::<BackupSpec>(&raw) else {
        emit(WorkerEvent::Error {
            msg: "Especificación de respaldo JSON inválida".into(),
        });
        return 1;
    };
    let cancel = Arc::new(AtomicBool::new(false));
    match execute_backup_direct(&spec, &cancel, emit) {
        Ok(()) => {
            emit(WorkerEvent::Done);
            0
        }
        Err(e) => {
            emit(WorkerEvent::Error { msg: e });
            1
        }
    }
}

/// Entrypoint for `--wipe-job <spec.json>` (fast zero-wipe of MBR/GPT/backup GPT & signatures).
pub fn run_wipe_cli(spec_path: &Path) -> i32 {
    let emit = |ev: WorkerEvent| {
        if let Ok(line) = serde_json::to_string(&ev) {
            println!("LUXUS_EV:{}", line);
            let _ = std::io::stdout().flush();
        }
    };
    let Ok(raw) = fs::read_to_string(spec_path) else {
        emit(WorkerEvent::Error {
            msg: format!("No se pudo leer {}", spec_path.display()),
        });
        return 1;
    };
    let Ok(spec) = serde_json::from_str::<WipeSpec>(&raw) else {
        emit(WorkerEvent::Error {
            msg: "Especificación de limpieza JSON inválida".into(),
        });
        return 1;
    };
    let cancel = Arc::new(AtomicBool::new(false));
    match execute_wipe_direct(&spec, &cancel, emit) {
        Ok(()) => {
            emit(WorkerEvent::Done);
            0
        }
        Err(e) => {
            emit(WorkerEvent::Error { msg: e });
            1
        }
    }
}

fn execute_wipe_direct<F>(
    spec: &WipeSpec,
    cancel: &Arc<AtomicBool>,
    emit: F,
) -> Result<(), String>
where
    F: Fn(WorkerEvent) + Sync + Send,
{
    let log = |m: String| emit(WorkerEvent::Log { msg: m });
    let prog = |p: f32, s: String, sp: String| {
        emit(WorkerEvent::Progress {
            progress: p,
            status: s,
            speed: sp,
        })
    };

    if !Path::new(&spec.device.path).exists() {
        return Err(format!("Dispositivo {} no encontrado", spec.device.path));
    }
    prog(0.10, "Desmontando particiones...".into(), String::new());
    unmount_all_partitions(&spec.device.path, &log)?;
    if cancel.load(Ordering::SeqCst) {
        return Err("CANCELLED".into());
    }

    if cmd_in_path("wipefs") {
        prog(0.25, "Eliminando firmas de partición...".into(), String::new());
        let _ = run_sys_cmd("wipefs", &["-a".into(), spec.device.path.clone()], &log);
    }

    prog(0.45, "Escribiendo ceros al inicio y final del disco...".into(), String::new());
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&spec.device.path)
        .map_err(|e| format!("No se pudo abrir {} para escritura: {}", spec.device.path, e))?;
    let zero_buf = vec![0u8; 1024 * 1024]; // 1 MiB
    let mut cmp_buf = vec![0u8; 1024 * 1024];
    let head_mb = (spec.device.size_bytes / (1024 * 1024)).min(16);
    let mut skipped_mb = 0u64;
    for i in 0..head_mb {
        if cancel.load(Ordering::SeqCst) {
            return Err("CANCELLED".into());
        }
        let off = i * 1024 * 1024;
        // Fast-zero comparison: skip writing blocks already all 0x00 or all 0xFF (reduces flash wear)
        let already_clean = f.seek(SeekFrom::Start(off)).is_ok()
            && f.read_exact(&mut cmp_buf).is_ok()
            && (cmp_buf.iter().all(|&b| b == 0x00) || cmp_buf.iter().all(|&b| b == 0xff));
        if already_clean {
            skipped_mb += 1;
        } else {
            f.seek(SeekFrom::Start(off))
                .map_err(|e| format!("Error de posicionamiento al limpiar: {}", e))?;
            f.write_all(&zero_buf)
                .map_err(|e| format!("Error limpiando inicio de disco: {}", e))?;
        }
        prog(
            0.45 + ((i + 1) as f32 / head_mb.max(1) as f32) * 0.35,
            format!("Limpiando inicio: {} / {} MiB", i + 1, head_mb),
            String::new(),
        );
    }
    if skipped_mb > 0 {
        log(format!(
            "Fast-zeroing: omitidos {} MiB ya vacíos para reducir desgaste flash.",
            skipped_mb
        ));
    }
    // Also wipe backup GPT table at the end of the disk (last 4 MiB)
    let tail_bytes = (4 * 1024 * 1024u64).min(spec.device.size_bytes);
    if spec.device.size_bytes > tail_bytes {
        let start_tail = spec.device.size_bytes - tail_bytes;
        if f.seek(SeekFrom::Start(start_tail)).is_ok() {
            let tail_mb = (tail_bytes / (1024 * 1024)).max(1);
            for _ in 0..tail_mb {
                let _ = f.write_all(&zero_buf);
            }
        }
    }
    f.sync_all().map_err(|e| e.to_string())?;
    let _ = Command::new("partprobe").arg(&spec.device.path).output();
    log(format!(
        "Limpieza completada: {} está ahora sin particiones ni firmas residuales.",
        spec.device.path
    ));
    prog(1.0, "COMPLETADO".into(), String::new());
    Ok(())
}

fn execute_backup_direct<F>(
    spec: &BackupSpec,
    cancel: &Arc<AtomicBool>,
    emit: F,
) -> Result<(), String>
where
    F: Fn(WorkerEvent) + Sync + Send,
{
    let log = |m: String| emit(WorkerEvent::Log { msg: m });
    let prog = |p: f32, s: String, sp: String| {
        emit(WorkerEvent::Progress {
            progress: p,
            status: s,
            speed: sp,
        })
    };

    if !Path::new(&spec.device.path).exists() {
        return Err(format!("Dispositivo {} no encontrado", spec.device.path));
    }
    let total = spec.device.size_bytes.max(1);
    let chunk_size = spec.buffer_mb.clamp(1, 16) * 1024 * 1024;
    let mut src = File::open(&spec.device.path)
        .map_err(|e| format!("No se pudo abrir {} para lectura: {}", spec.device.path, e))?;
    let mut dst = File::create(&spec.output_path)
        .map_err(|e| format!("No se pudo crear {}: {}", spec.output_path, e))?;

    let mut buf = vec![0u8; chunk_size];
    let mut copied = 0u64;
    let t0 = Instant::now();
    let mut last_ui = Instant::now() - Duration::from_secs(1);

    loop {
        if cancel.load(Ordering::SeqCst) {
            drop(dst);
            let _ = fs::remove_file(&spec.output_path);
            return Err("CANCELLED".into());
        }
        let n = src
            .read(&mut buf)
            .map_err(|e| format!("Error leyendo dispositivo: {}", e))?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])
            .map_err(|e| format!("Error escribiendo imagen de respaldo: {}", e))?;
        copied += n as u64;
        if last_ui.elapsed() >= Duration::from_millis(120) || copied >= total {
            last_ui = Instant::now();
            let pct = (copied as f64 / total as f64).clamp(0.0, 1.0);
            let mb_s =
                (copied as f64 / (1024.0 * 1024.0)) / t0.elapsed().as_secs_f64().max(0.05);
            prog(
                (pct as f32) * 0.96,
                format!(
                    "Respaldando: {:.1}% ({} / {})",
                    pct * 100.0,
                    human_size(copied),
                    human_size(total)
                ),
                format!("{:.1} MB/s", mb_s),
            );
        }
    }

    if spec.as_vhd {
        log("Escribiendo pie de página VHD fijo (Microsoft Connectix 512 B)...".into());
        let footer = build_fixed_vhd_footer(copied);
        dst.write_all(&footer)
            .map_err(|e| format!("Error escribiendo footer VHD: {}", e))?;
    }
    dst.sync_all().map_err(|e| e.to_string())?;
    log(format!(
        "Respaldo completado: {} ({})",
        spec.output_path,
        human_size(copied)
    ));
    prog(1.0, "COMPLETADO".into(), String::new());
    Ok(())
}

fn execute_job_direct<F>(spec: &JobSpec, cancel: &Arc<AtomicBool>, emit: F) -> Result<(), String>
where
    F: Fn(WorkerEvent) + Sync + Send,
{
    let log = |m: String| emit(WorkerEvent::Log { msg: m });
    let prog = |p: f32, s: String, sp: String| {
        emit(WorkerEvent::Progress {
            progress: p,
            status: s,
            speed: sp,
        })
    };

    if !Path::new(&spec.device.path).exists() {
        return Err(format!("Dispositivo {} no encontrado", spec.device.path));
    }
    if spec.device.size_bytes >= LARGE_DRIVE_BYTES {
        log("Notice: Large drive detected (may produce short writes)".into());
    }
    log(format!(
        "Tipo de medio: {} (sector {} B)",
        if spec.device.removable {
            "Extraíble"
        } else {
            "FIJO"
        },
        spec.device.sector_bytes
    ));

    // Phase 0: Unmount all partitions on the target device
    prog(0.03, "Desmontando particiones...".into(), String::new());
    unmount_all_partitions(&spec.device.path, &log)?;
    if cancel.load(Ordering::SeqCst) {
        return Err("CANCELLED".into());
    }

    // Optional Phase: multi-pass Bad-Blocks Check
    if spec.check_bad_blocks {
        let passes = spec.bad_blocks_passes.clamp(1, 4);
        run_bad_blocks_check(&spec.device, passes, cancel, &log, &prog)?;
    }

    let real_label = normalize_label(
        &spec.volume_label,
        spec.filesystem.is_fat32(),
        &spec.device.model,
    );
    log(format!(
        "Etiqueta normalizada: '{}' (default: {})",
        real_label, DEFAULT_LABEL
    ));

    match spec.boot_mode {
        BootMode::NonBootable => {
            let chosen_cluster = if spec.filesystem.is_fat32() {
                validate_fat32(
                    spec.device.size_bytes,
                    spec.device.sector_bytes,
                    spec.cluster_size,
                )?
            } else {
                spec.cluster_size
            };
            let _ = format_device_partitions(spec, chosen_cluster, &real_label, false, 0, &log, &prog)?;
        }
        BootMode::FreeDos => {
            let mut dos_spec = spec.clone();
            dos_spec.partition_scheme = PartitionScheme::Mbr;
            dos_spec.target_system = TargetSystem::BiosUefi;
            if !dos_spec.filesystem.is_fat32() {
                dos_spec.filesystem = FileSystemKind::Fat32;
            }
            let chosen_cluster = validate_fat32(
                dos_spec.device.size_bytes,
                dos_spec.device.sector_bytes,
                dos_spec.cluster_size,
            )?;
            let dos_label = normalize_label(&real_label, true, "FREEDOS");
            let layout = format_device_partitions(
                &dos_spec,
                chosen_cluster,
                &dos_label,
                false,
                0,
                &log,
                &prog,
            )?;
            install_freedos_to_partition(&dos_spec.device.path, &layout.main_part, &log, &prog)?;
        }
        BootMode::DdImage | BootMode::IsoImage => {
            if spec.image_path.trim().is_empty() {
                return Err("No se ha seleccionado ninguna imagen ISO/IMG".into());
            }
            let img_path = Path::new(&spec.image_path);
            if !img_path.exists() {
                return Err(format!("Imagen no encontrada: {}", spec.image_path));
            }
            let report = inspect_image_file(img_path)?;
            log(format!(
                "Firma detectada: {} ({})",
                report.signature,
                human_size(report.file_size)
            ));
            for note in &report.notes {
                log(note.clone());
            }

            // Streaming decompression path for compressed images (.gz, .xz, .zst, .bz2)
            if report.compression != CompressionKind::None {
                write_compressed_image_streaming(
                    spec,
                    img_path,
                    report.compression,
                    cancel,
                    &log,
                    &prog,
                )?;
            } else {
                let is_extract_mode = spec.boot_mode == BootMode::IsoImage
                    && matches!(
                        spec.image_option,
                        ImageWriteOption::WindowsStandard | ImageWriteOption::IsoExtract
                    )
                    && !report.force_dd;
                if report.force_dd {
                    log(format!("Enforcing DD image mode (marker={})", DD_MODE_MARKER));
                }

                if !is_extract_mode {
                    // Hybrid ISO / DD sector-by-sector write
                    write_image_double_buffered(spec, img_path, cancel, &log, &prog)?;
                    // Optional appended Linux persistence partition after hybrid ISO
                    if spec.persistence_gb > 0 && spec.persistence_style != PersistenceStyle::None {
                        append_linux_persistence_partition(
                            &spec.device.path,
                            spec.persistence_gb,
                            spec.persistence_style,
                            &log,
                            &prog,
                        )?;
                    }
                } else {
                    // File-extraction path: Windows Standard (with UEFI:NTFS + autounattend.xml)
                    // or Linux ISO extraction (with optional persistence partition)
                    let want_uefi_ntfs = spec.enable_uefi_ntfs
                        && spec.filesystem.needs_uefi_ntfs()
                        && (spec.image_option == ImageWriteOption::WindowsStandard
                            || spec.target_system == TargetSystem::Uefi);
                    let persist_gb = if spec.image_option == ImageWriteOption::IsoExtract
                        && spec.persistence_style != PersistenceStyle::None
                    {
                        spec.persistence_gb
                    } else {
                        0
                    };
                    let chosen_cluster = if spec.filesystem.is_fat32() {
                        validate_fat32(
                            spec.device.size_bytes,
                            spec.device.sector_bytes,
                            spec.cluster_size,
                        )?
                    } else {
                        spec.cluster_size
                    };
                    let layout = format_device_partitions(
                        spec,
                        chosen_cluster,
                        &real_label,
                        want_uefi_ntfs,
                        persist_gb,
                        &log,
                        &prog,
                    )?;
                    extract_iso_to_partition(
                        spec,
                        img_path,
                        &layout.main_part,
                        &real_label,
                        cancel,
                        &log,
                        &prog,
                    )?;
                    if let Some(ref uefi_part) = layout.uefi_ntfs_part {
                        prog(
                            0.94,
                            "Instalando cargador UEFI:NTFS...".into(),
                            String::new(),
                        );
                        let main_uuid = query_partition_uuid(&layout.main_part);
                        install_uefi_ntfs_partition(uefi_part, main_uuid.as_deref(), &log)?;
                    }
                    if let Some(ref p_part) = layout.persistence_part {
                        format_persistence_partition(p_part, spec.persistence_style, &log)?;
                    }
                }
            }
        }
    }

    prog(0.99, "Sincronizando caché de disco...".into(), String::new());
    let _ = Command::new("sync").status();
    prog(1.0, "COMPLETADO".into(), String::new());
    Ok(())
}

/// Pure-Rust multi-pass bad-blocks and fake-capacity flash verification:
/// - Block unit: 512 KiB (0x80000), batch size: 64 blocks (32 MiB chunk)
/// - Passes 1..4 with patterns 0x55, 0xAA, 0xFF, 0x00
/// - Pass 1 embeds unique u64 block indices at a pseudo-random offset to catch fake flash drives
/// - Aborts if bad blocks exceed 255
fn run_bad_blocks_check<L, P>(
    dev: &DeviceInfo,
    passes: u8,
    cancel: &Arc<AtomicBool>,
    log: &L,
    prog: &P,
) -> Result<(), String>
where
    L: Fn(String),
    P: Fn(f32, String, String),
{
    let total_blocks = (dev.size_bytes / BADBLOCKS_BLOCK_BYTES as u64) as usize;
    if total_blocks == 0 {
        return Err("Dispositivo demasiado pequeño para prueba de bloques dañados".into());
    }
    log(format!(
        "Bad Blocks: Checking from block 0 to {} (1 block = {} KiB, {} pass(es))",
        total_blocks - 1,
        BADBLOCKS_BLOCK_BYTES / 1024,
        passes
    ));

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&dev.path)
        .map_err(|e| {
            format!(
                "No se pudo abrir {} para lectura/escritura: {}",
                dev.path, e
            )
        })?;

    let chunk_bytes = BADBLOCKS_BLOCK_BYTES * BADBLOCKS_BATCH_BLOCKS;
    let mut write_buf = vec![0u8; chunk_bytes];
    let mut read_buf = vec![0u8; chunk_bytes];

    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as usize;
    let fake_offset = ((seed % (BADBLOCKS_BLOCK_BYTES - 16)) / 8) * 8;
    log(format!(
        "Bad Blocks: Using offset {} for fake device check",
        fake_offset
    ));

    let mut write_errors = 0usize;
    let mut read_errors = 0usize;
    let mut corruption_errors = 0usize;
    let mut total_bad = 0usize;

    for pass in 0..(passes as usize) {
        let pat = BADBLOCKS_PATTERNS[pass % BADBLOCKS_PATTERNS.len()];
        log(format!(
            "Bad Blocks: Pass {}/{} — Writing test pattern 0x{:02X}",
            pass + 1,
            passes,
            pat
        ));
        write_buf.fill(pat);

        // Write phase with explicit seek and single-block fallback on batch write errors
        let mut blk = 0usize;
        let t0 = Instant::now();
        while blk < total_blocks {
            if cancel.load(Ordering::SeqCst) {
                return Err("CANCELLED".into());
            }
            let n_blks = (total_blocks - blk).min(BADBLOCKS_BATCH_BLOCKS);
            let bytes = n_blks * BADBLOCKS_BLOCK_BYTES;
            if pass == 0 {
                for i in 0..n_blks {
                    let pos = i * BADBLOCKS_BLOCK_BYTES + fake_offset;
                    write_buf[pos..pos + 8].copy_from_slice(&((blk + i) as u64).to_le_bytes());
                }
            }
            let batch_offset = (blk as u64).saturating_mul(BADBLOCKS_BLOCK_BYTES as u64);
            let batch_ok = file.seek(SeekFrom::Start(batch_offset)).is_ok()
                && file.write_all(&write_buf[..bytes]).is_ok();
            if !batch_ok {
                for i in 0..n_blks {
                    let b_off = ((blk + i) as u64).saturating_mul(BADBLOCKS_BLOCK_BYTES as u64);
                    let s = i * BADBLOCKS_BLOCK_BYTES;
                    let e = s + BADBLOCKS_BLOCK_BYTES;
                    if file.seek(SeekFrom::Start(b_off)).is_err()
                        || file.write_all(&write_buf[s..e]).is_err()
                    {
                        write_errors += 1;
                        total_bad += 1;
                        if total_bad >= BADBLOCKS_MAX_ERRORS {
                            log("Too many bad blocks, aborting test".into());
                            return Err(
                                "Demasiados bloques dañados (>255), prueba abortada".into(),
                            );
                        }
                    }
                }
            }
            blk += n_blks;
            let frac = ((pass as f32 * 2.0) + (blk as f32 / total_blocks as f32))
                / (passes as f32 * 2.0);
            let mb_s = (blk * BADBLOCKS_BLOCK_BYTES) as f64
                / (1024.0 * 1024.0 * t0.elapsed().as_secs_f64().max(0.05));
            prog(
                0.05 + frac * 0.25,
                format!(
                    "Bloques dañados {}/{} (0x{:02X} Escribiendo) — {:.1}%",
                    pass + 1,
                    passes,
                    pat,
                    (blk as f32 / total_blocks as f32) * 100.0
                ),
                format!("{:.1} MB/s", mb_s),
            );
        }
        let _ = file.sync_all();

        // Read & compare phase with explicit seek and single-block fallback
        log("Bad Blocks: Reading and comparing".into());
        blk = 0;
        let t1 = Instant::now();
        while blk < total_blocks {
            if cancel.load(Ordering::SeqCst) {
                return Err("CANCELLED".into());
            }
            let n_blks = (total_blocks - blk).min(BADBLOCKS_BATCH_BLOCKS);
            let bytes = n_blks * BADBLOCKS_BLOCK_BYTES;
            if pass == 0 {
                for i in 0..n_blks {
                    let pos = i * BADBLOCKS_BLOCK_BYTES + fake_offset;
                    write_buf[pos..pos + 8].copy_from_slice(&((blk + i) as u64).to_le_bytes());
                }
            }
            let batch_offset = (blk as u64).saturating_mul(BADBLOCKS_BLOCK_BYTES as u64);
            let read_ok = file.seek(SeekFrom::Start(batch_offset)).is_ok()
                && file.read_exact(&mut read_buf[..bytes]).is_ok();
            if !read_ok {
                for i in 0..n_blks {
                    let b_off = ((blk + i) as u64).saturating_mul(BADBLOCKS_BLOCK_BYTES as u64);
                    let s = i * BADBLOCKS_BLOCK_BYTES;
                    let e = s + BADBLOCKS_BLOCK_BYTES;
                    if file.seek(SeekFrom::Start(b_off)).is_err()
                        || file.read_exact(&mut read_buf[s..e]).is_err()
                    {
                        read_errors += 1;
                        total_bad += 1;
                    } else if read_buf[s..e] != write_buf[s..e] {
                        corruption_errors += 1;
                        total_bad += 1;
                    }
                    if total_bad >= BADBLOCKS_MAX_ERRORS {
                        log("Too many bad blocks, aborting test".into());
                        return Err("Demasiados bloques dañados (>255), prueba abortada".into());
                    }
                }
            } else if read_buf[..bytes] != write_buf[..bytes] {
                for i in 0..n_blks {
                    let s = i * BADBLOCKS_BLOCK_BYTES;
                    let e = s + BADBLOCKS_BLOCK_BYTES;
                    if read_buf[s..e] != write_buf[s..e] {
                        corruption_errors += 1;
                        total_bad += 1;
                        if total_bad >= BADBLOCKS_MAX_ERRORS {
                            log("Too many bad blocks, aborting test".into());
                            return Err(
                                "Demasiados bloques dañados (>255), prueba abortada".into(),
                            );
                        }
                    }
                }
            }
            blk += n_blks;
            let frac = ((pass as f32 * 2.0 + 1.0) + (blk as f32 / total_blocks as f32))
                / (passes as f32 * 2.0);
            let mb_s = (blk * BADBLOCKS_BLOCK_BYTES) as f64
                / (1024.0 * 1024.0 * t1.elapsed().as_secs_f64().max(0.05));
            prog(
                0.05 + frac * 0.25,
                format!(
                    "Bloques dañados {}/{} (Verificando) — {:.1}%",
                    pass + 1,
                    passes,
                    (blk as f32 / total_blocks as f32) * 100.0
                ),
                format!("{:.1} MB/s", mb_s),
            );
        }
    }

    log(format!(
        "Bad Blocks: Check completed, {} bad block{} found. ({}/{}/{} errors)",
        total_bad,
        if total_bad == 1 { "" } else { "s" },
        read_errors,
        write_errors,
        corruption_errors
    ));
    if total_bad > 0 {
        return Err(format!(
            "Se encontraron {} bloques dañados ({} lectura / {} escritura / {} corrupción)",
            total_bad, read_errors, write_errors, corruption_errors
        ));
    }
    Ok(())
}

/// Streams a compressed image (.gz/.xz/.zst/.bz2) directly to the target USB without temp files.
fn write_compressed_image_streaming<L, P>(
    spec: &JobSpec,
    img_path: &Path,
    kind: CompressionKind,
    cancel: &Arc<AtomicBool>,
    log: &L,
    prog: &P,
) -> Result<(), String>
where
    L: Fn(String),
    P: Fn(f32, String, String),
{
    let (tool, args): (&str, Vec<String>) = match kind {
        CompressionKind::Gzip => {
            if cmd_in_path("zcat") {
                ("zcat", vec![img_path.display().to_string()])
            } else {
                ("gzip", vec!["-dc".into(), img_path.display().to_string()])
            }
        }
        CompressionKind::Xz => {
            if cmd_in_path("xzcat") {
                ("xzcat", vec![img_path.display().to_string()])
            } else {
                ("xz", vec!["-dc".into(), img_path.display().to_string()])
            }
        }
        CompressionKind::Zstd => {
            if cmd_in_path("zstdcat") {
                ("zstdcat", vec![img_path.display().to_string()])
            } else {
                ("zstd", vec!["-dc".into(), img_path.display().to_string()])
            }
        }
        CompressionKind::Bzip2 => {
            if cmd_in_path("bzcat") {
                ("bzcat", vec![img_path.display().to_string()])
            } else {
                ("bunzip2", vec!["-c".into(), img_path.display().to_string()])
            }
        }
        CompressionKind::Zip => {
            if cmd_in_path("unzip") {
                ("unzip", vec!["-p".into(), img_path.display().to_string()])
            } else if cmd_in_path("7z") {
                (
                    "7z",
                    vec!["x".into(), "-so".into(), img_path.display().to_string()],
                )
            } else if cmd_in_path("7zz") {
                (
                    "7zz",
                    vec!["x".into(), "-so".into(), img_path.display().to_string()],
                )
            } else {
                ("unzip", vec!["-p".into(), img_path.display().to_string()])
            }
        }
        CompressionKind::Lzma => {
            if cmd_in_path("lzcat") {
                ("lzcat", vec![img_path.display().to_string()])
            } else {
                (
                    "xz",
                    vec![
                        "-dc".into(),
                        "--format=lzma".into(),
                        img_path.display().to_string(),
                    ],
                )
            }
        }
        CompressionKind::None => return Err("Imagen no comprimida".into()),
    };

    if !cmd_in_path(tool) {
        return Err(format!(
            "No se encontró '{}' para descomprimir esta imagen en vuelo.",
            tool
        ));
    }

    if cmd_in_path("wipefs") {
        prog(0.08, "Limpiando firmas previas...".into(), String::new());
        let _ = Command::new("wipefs")
            .args(["-a", &spec.device.path])
            .output();
    }

    log(format!(
        "Descompresión en flujo con '{}' hacia {}...",
        tool, spec.device.path
    ));
    let mut child = Command::new(tool)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Error iniciando {}: {}", tool, e))?;
    let mut stream = child
        .stdout
        .take()
        .ok_or_else(|| "No se pudo abrir flujo de salida".to_string())?;
    let mut dst = OpenOptions::new()
        .write(true)
        .open(&spec.device.path)
        .map_err(|e| format!("No se pudo abrir {} para escritura: {}", spec.device.path, e))?;

    let chunk_size = spec.buffer_mb.clamp(1, 16) * 1024 * 1024;
    let sector_align = (spec.device.sector_bytes as usize).max(MIN_SECTOR_BYTES as usize);
    let mut buf = vec![0u8; chunk_size];
    let mut written = 0u64;
    let mut since_sync = 0usize;
    let t0 = Instant::now();
    let mut last_ui = Instant::now() - Duration::from_secs(1);

    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            return Err("CANCELLED".into());
        }
        let mut filled = 0usize;
        while filled < chunk_size {
            match stream.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => {
                    let _ = child.kill();
                    return Err(format!("Error descomprimiendo: {}", e));
                }
            }
        }
        if filled == 0 {
            break;
        }
        let mut write_len = filled;
        let rem = write_len % sector_align;
        if rem != 0 {
            let pad = sector_align - rem;
            if write_len + pad <= buf.len() {
                buf[write_len..write_len + pad].fill(0);
                write_len += pad;
            }
        }
        dst.write_all(&buf[..write_len])
            .map_err(|e| format!("Error escribiendo en {}: {}", spec.device.path, e))?;
        written += filled as u64;
        since_sync += write_len;
        if since_sync >= 32 * 1024 * 1024 {
            let _ = dst.sync_data();
            since_sync = 0;
        }
        if last_ui.elapsed() >= Duration::from_millis(150) {
            last_ui = Instant::now();
            let mb_s =
                (written as f64 / (1024.0 * 1024.0)) / t0.elapsed().as_secs_f64().max(0.05);
            let frac = (written as f64 / spec.device.size_bytes.max(1) as f64).clamp(0.05, 0.92);
            prog(
                0.10 + (frac as f32) * 0.85,
                format!("Descomprimiendo y escribiendo: {}", human_size(written)),
                format!("{:.1} MB/s", mb_s),
            );
        }
    }

    let status = child.wait().map_err(|e| e.to_string())?;
    dst.sync_all().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("{} finalizó con error ({})", tool, status));
    }
    log(format!(
        "Imagen comprimida escrita: {} en {:.1} s",
        human_size(written),
        t0.elapsed().as_secs_f64()
    ));
    let _ = Command::new("partprobe").arg(&spec.device.path).output();
    Ok(())
}

/// Ultra-fast pure-Rust double-buffered zero-allocation ring-pool raw writer.
/// Automatically strips the trailing 512-byte Connectix footer when restoring a fixed `.vhd` image.
fn write_image_double_buffered<L, P>(
    spec: &JobSpec,
    img_path: &Path,
    cancel: &Arc<AtomicBool>,
    log: &L,
    prog: &P,
) -> Result<(), String>
where
    L: Fn(String),
    P: Fn(f32, String, String),
{
    let raw_file_size = fs::metadata(img_path)
        .map(|m| m.len())
        .map_err(|e| format!("No se pudo leer el tamaño de la imagen: {}", e))?;
    let img_size = if let Some(vhd_payload) = detect_fixed_vhd_payload_size(img_path) {
        log(format!(
            "Imagen VHD fija detectada: omitiendo footer Connectix de 512 B ({} útiles).",
            human_size(vhd_payload)
        ));
        vhd_payload
    } else {
        raw_file_size
    };
    if img_size == 0 {
        return Err("La imagen seleccionada está vacía".into());
    }
    if img_size > spec.device.size_bytes {
        return Err(format!(
            "La imagen ({}) es más grande que el dispositivo ({})",
            human_size(img_size),
            spec.device.size
        ));
    }

    if cmd_in_path("wipefs") {
        prog(0.08, "Limpiando firmas previas...".into(), String::new());
        let _ = Command::new("wipefs")
            .args(["-a", &spec.device.path])
            .output();
    }

    let chunk_size = (spec.buffer_mb.clamp(1, 16)) * 1024 * 1024;
    let sector_align = (spec.device.sector_bytes as usize).max(MIN_SECTOR_BYTES as usize);
    log(format!(
        "Motor de escritura Rust (anillo 3x{} MiB sin realocación, fdatasync cada 32 MiB)...",
        chunk_size / (1024 * 1024)
    ));

    let mut dst = OpenOptions::new()
        .write(true)
        .read(true)
        .open(&spec.device.path)
        .map_err(|e| {
            format!(
                "No se pudo abrir {} para escritura: {}",
                spec.device.path, e
            )
        })?;

    // 3 pre-allocated buffers recycled between reader & writer threads (zero heap churn)
    let (data_tx, data_rx) = mpsc::sync_channel::<Result<(Vec<u8>, usize), String>>(3);
    let (free_tx, free_rx) = mpsc::sync_channel::<Vec<u8>>(3);
    for _ in 0..3 {
        let _ = free_tx.send(vec![0u8; chunk_size + sector_align]);
    }

    let src_path = img_path.to_path_buf();
    let cancel_r = cancel.clone();
    let need_hash = spec.verify_after_write;

    let reader_handle = thread::spawn(move || -> Option<String> {
        let Ok(mut f) = File::open(&src_path) else {
            let _ = data_tx.send(Err("Error al abrir el archivo de imagen".into()));
            return None;
        };
        let mut hasher = if need_hash { Some(Sha256::new()) } else { None };
        let mut remaining = img_size;
        while remaining > 0 {
            if cancel_r.load(Ordering::SeqCst) {
                return None;
            }
            let Ok(mut buf) = free_rx.recv() else {
                return None;
            };
            let target = (remaining as usize).min(chunk_size);
            let mut filled = 0usize;
            while filled < target {
                match f.read(&mut buf[filled..target]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) => {
                        let _ = data_tx.send(Err(format!("Error leyendo imagen: {}", e)));
                        return None;
                    }
                }
            }
            if filled == 0 {
                break;
            }
            remaining -= filled as u64;
            if let Some(ref mut h) = hasher {
                h.update(&buf[..filled]);
            }
            if data_tx.send(Ok((buf, filled))).is_err() {
                return None;
            }
        }
        hasher.map(|h| h.finalize_hex())
    });

    let start_base = if spec.check_bad_blocks { 0.30 } else { 0.10 };
    let write_span = if spec.verify_after_write {
        0.65 - start_base
    } else {
        0.92 - start_base
    };

    let t0 = Instant::now();
    let mut written: u64 = 0;
    let mut since_sync: usize = 0;
    let sync_interval: usize = 32 * 1024 * 1024;
    let mut last_ui = Instant::now() - Duration::from_secs(1);

    for item in data_rx {
        if cancel.load(Ordering::SeqCst) {
            return Err("CANCELLED".into());
        }
        let (mut buf, raw_len) = item?;
        let mut write_len = raw_len;
        let rem = write_len % sector_align;
        if rem != 0 {
            let pad = sector_align - rem;
            buf[write_len..write_len + pad].fill(0);
            write_len += pad;
        }
        // Write with up to 3 automatic retries on transient USB I/O errors (rewind + backoff)
        let write_offset = written;
        let mut attempt = 0u8;
        loop {
            match dst.write_all(&buf[..write_len]) {
                Ok(()) => break,
                Err(e) => {
                    attempt += 1;
                    if attempt > 3 || cancel.load(Ordering::SeqCst) {
                        cancel.store(true, Ordering::SeqCst);
                        return Err(format!("Error de escritura en {}: {}", spec.device.path, e));
                    }
                    log(format!(
                        "Error transitorio en sector {} ({}); reintentando {}/3 en 2s...",
                        write_offset / sector_align as u64,
                        e,
                        attempt
                    ));
                    thread::sleep(Duration::from_secs(2));
                    if let Err(se) = dst.seek(SeekFrom::Start(write_offset)) {
                        cancel.store(true, Ordering::SeqCst);
                        return Err(format!("No se pudo reposicionar escritura: {}", se));
                    }
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }
        let _ = free_tx.send(buf);
        written += raw_len as u64;
        since_sync += write_len;
        if since_sync >= sync_interval {
            let _ = dst.sync_data();
            since_sync = 0;
        }
        if last_ui.elapsed() >= Duration::from_millis(120) || written == img_size {
            last_ui = Instant::now();
            let pct = (written as f64 / img_size as f64).clamp(0.0, 1.0);
            let elapsed = t0.elapsed().as_secs_f64().max(0.05);
            let mb_s = (written as f64 / (1024.0 * 1024.0)) / elapsed;
            let eta = if mb_s > 0.1 {
                ((img_size.saturating_sub(written)) as f64 / (1024.0 * 1024.0) / mb_s) as u64
            } else {
                0
            };
            prog(
                start_base + (pct as f32) * write_span,
                format!(
                    "Escribiendo imagen: {:.1}% ({} / {})",
                    pct * 100.0,
                    human_size(written),
                    human_size(img_size)
                ),
                format!("{:.1} MB/s  ETA {}", mb_s, format_elapsed(eta)),
            );
        }
    }

    dst.sync_all()
        .map_err(|e| format!("Error en fsync final: {}", e))?;
    let src_sha256 = reader_handle.join().unwrap_or(None);

    let elapsed = t0.elapsed().as_secs_f64().max(0.05);
    log(format!(
        "Escritura finalizada: {} en {:.1} s ({:.1} MB/s)",
        human_size(written),
        elapsed,
        (written as f64 / (1024.0 * 1024.0)) / elapsed
    ));

    if spec.verify_after_write {
        if let Some(expected_hash) = src_sha256 {
            log("Verificando integridad SHA-256 del dispositivo escrito...".into());
            dst.seek(SeekFrom::Start(0))
                .map_err(|e| format!("Seek error en verificación: {}", e))?;
            let mut hasher = Sha256::new();
            let mut vbuf = vec![0u8; chunk_size];
            let mut remaining = img_size;
            let mut verified = 0u64;
            let tv = Instant::now();
            while remaining > 0 {
                if cancel.load(Ordering::SeqCst) {
                    return Err("CANCELLED".into());
                }
                let to_read = (remaining as usize).min(vbuf.len());
                dst.read_exact(&mut vbuf[..to_read])
                    .map_err(|e| format!("Error de lectura durante verificación: {}", e))?;
                hasher.update(&vbuf[..to_read]);
                remaining -= to_read as u64;
                verified += to_read as u64;
                let pct = (verified as f64 / img_size as f64).clamp(0.0, 1.0);
                let mb_s = (verified as f64 / (1024.0 * 1024.0))
                    / tv.elapsed().as_secs_f64().max(0.05);
                prog(
                    0.65 + (pct as f32) * 0.28,
                    format!("Verificando SHA-256: {:.1}%", pct * 100.0),
                    format!("{:.1} MB/s", mb_s),
                );
            }
            let actual_hash = hasher.finalize_hex();
            if actual_hash != expected_hash {
                return Err(format!(
                    "¡Error de verificación SHA-256! Esperado {}..., leído {}...",
                    &expected_hash[..16],
                    &actual_hash[..16]
                ));
            }
            log(format!("Verificación SHA-256 OK: {}", actual_hash));
        }
    }

    let _ = Command::new("partprobe").arg(&spec.device.path).output();
    Ok(())
}

/// Appends a Linux persistence ext4 partition (`casper-rw` or `persistence`) in the remaining
/// free space after a hybrid ISO has been written to the USB drive.
fn append_linux_persistence_partition<L, P>(
    dev_path: &str,
    size_gb: u32,
    style: PersistenceStyle,
    log: &L,
    prog: &P,
) -> Result<(), String>
where
    L: Fn(String),
    P: Fn(f32, String, String),
{
    if !cmd_in_path("sfdisk") {
        log("Aviso: sfdisk no encontrado; se omitió la creación de partición persistente.".into());
        return Ok(());
    }
    prog(
        0.94,
        format!("Creando partición persistente de {} GB...", size_gb),
        String::new(),
    );
    let _ = Command::new("partprobe").arg(dev_path).output();
    thread::sleep(Duration::from_millis(600));

    let before_parts = list_device_partitions(dev_path);
    let spec_line = format!(",{}G,L\n", size_gb);
    let mut child = Command::new("sfdisk")
        .args(["--append", "--no-reread", dev_path])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Error iniciando sfdisk --append: {}", e))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(spec_line.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        log(format!(
            "Aviso: no se pudo anexar partición persistente ({}).",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
        return Ok(());
    }
    let _ = Command::new("partprobe").arg(dev_path).output();
    thread::sleep(Duration::from_millis(900));

    let after_parts = list_device_partitions(dev_path);
    if let Some(new_part) = after_parts
        .into_iter()
        .find(|p| !before_parts.contains(p))
    {
        format_persistence_partition(&new_part, style, log)?;
    }
    Ok(())
}

fn format_persistence_partition<L: Fn(String)>(
    part: &str,
    style: PersistenceStyle,
    log: &L,
) -> Result<(), String> {
    let label = match style {
        PersistenceStyle::UbuntuCasper => "casper-rw",
        PersistenceStyle::DebianLive => "persistence",
        PersistenceStyle::None => return Ok(()),
    };
    log(format!(
        "Formateando partición persistente {} como ext4 ('{}')...",
        part, label
    ));
    if !cmd_in_path("mkfs.ext4") {
        return Err("No se encontró mkfs.ext4 para crear la partición persistente.".into());
    }
    run_sys_cmd(
        "mkfs.ext4",
        &["-F".into(), "-L".into(), label.into(), part.into()],
        log,
    )?;

    if style == PersistenceStyle::DebianLive {
        let mnt = std::env::temp_dir().join(format!("luxus-persist-{}", std::process::id()));
        if fs::create_dir_all(&mnt).is_ok() {
            if Command::new("mount")
                .args([part, &mnt.display().to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                let _ = fs::write(mnt.join("persistence.conf"), "/ union\n");
                log("Escrito /persistence.conf ('/ union') para Debian/Kali Live.".into());
                let _ = Command::new("sync").status();
                let _ = Command::new("umount").arg(&mnt).status();
            }
            let _ = fs::remove_dir_all(&mnt);
        }
    }
    Ok(())
}

fn unmount_all_partitions<L: Fn(String)>(dev_path: &str, log: &L) -> Result<(), String> {
    if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
        let mut targets: Vec<(String, String)> = Vec::new();
        for line in mounts.lines() {
            let mut parts = line.split_whitespace();
            let Some(src) = parts.next() else { continue };
            let Some(mp_raw) = parts.next() else { continue };
            let real_src = fs::canonicalize(src)
                .ok()
                .and_then(|p| p.to_str().map(|s| s.to_string()))
                .unwrap_or_else(|| src.to_string());
            if is_same_or_partition_of(&real_src, dev_path) {
                targets.push((real_src, unescape_proc_mount(mp_raw)));
            }
        }
        for (src, mp) in targets {
            log(format!("Desmontando {} ({})", src, mp));
            let ok = Command::new("umount")
                .arg(&src)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                let _ = Command::new("umount").args(["-l", &src]).status();
            }
            let _ = Command::new("udisksctl")
                .args(["unmount", "-b", &src])
                .output();
        }
    }
    Ok(())
}

fn run_sys_cmd<L: Fn(String)>(program: &str, args: &[String], log: &L) -> Result<(), String> {
    log(format!("Ejecutando: {} {}", program, args.join(" ")));
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("No se pudo ejecutar {}: {}", program, e))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    for l in stdout.lines().chain(stderr.lines()) {
        if !l.trim().is_empty() {
            log(l.to_string());
        }
    }
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{} falló (código {})", program, out.status))
    }
}

struct PartitionLayout {
    main_part: String,
    uefi_ntfs_part: Option<String>,
    persistence_part: Option<String>,
}

fn format_device_partitions<L, P>(
    spec: &JobSpec,
    cluster: u32,
    label: &str,
    want_uefi_ntfs: bool,
    persistence_gb: u32,
    log: &L,
    prog: &P,
) -> Result<PartitionLayout, String>
where
    L: Fn(String),
    P: Fn(f32, String, String),
{
    prog(0.12, "Inicializando disco...".into(), String::new());
    if cmd_in_path("wipefs") {
        let _ = run_sys_cmd("wipefs", &["-a".into(), spec.device.path.clone()], log);
    }

    let label_type = match spec.partition_scheme {
        PartitionScheme::Mbr => "msdos",
        PartitionScheme::Gpt => "gpt",
    };
    let start_mib = GPT_EFI_START_BYTES / (1024 * 1024);
    let total_mib = spec.device.size_bytes / (1024 * 1024);
    let reserve_uefi_mib = if want_uefi_ntfs {
        (UEFI_NTFS_PART_BYTES / (1024 * 1024)).max(2)
    } else {
        0
    };
    let reserve_persist_mib = (persistence_gb as u64).saturating_mul(1024);
    let main_end_mib = total_mib
        .saturating_sub(reserve_uefi_mib + reserve_persist_mib + 2)
        .max(start_mib + 64);

    prog(
        0.22,
        format!(
            "Creando tabla de particiones {}{}...",
            spec.partition_scheme.label(),
            if want_uefi_ntfs { " + UEFI:NTFS" } else { "" }
        ),
        String::new(),
    );

    if cmd_in_path("parted") {
        run_sys_cmd(
            "parted",
            &[
                "-s".into(),
                spec.device.path.clone(),
                "mklabel".into(),
                label_type.into(),
            ],
            log,
        )?;
        let fs_hint = match spec.filesystem {
            FileSystemKind::Fat32 | FileSystemKind::LargeFat32 => "fat32",
            FileSystemKind::Ntfs | FileSystemKind::Exfat | FileSystemKind::Udf => "ntfs",
            FileSystemKind::Ext4 | FileSystemKind::Ext3 | FileSystemKind::Ext2 => "ext4",
        };
        let end_str = if reserve_uefi_mib > 0 || reserve_persist_mib > 0 {
            format!("{}MiB", main_end_mib)
        } else {
            "100%".to_string()
        };
        run_sys_cmd(
            "parted",
            &[
                "-s".into(),
                spec.device.path.clone(),
                "mkpart".into(),
                "primary".into(),
                fs_hint.into(),
                format!("{}MiB", start_mib),
                end_str,
            ],
            log,
        )?;

        if spec.boot_mode != BootMode::NonBootable && !want_uefi_ntfs {
            let flag = if spec.partition_scheme == PartitionScheme::Gpt {
                "esp"
            } else {
                "boot"
            };
            let _ = run_sys_cmd(
                "parted",
                &[
                    "-s".into(),
                    spec.device.path.clone(),
                    "set".into(),
                    "1".into(),
                    flag.into(),
                    "on".into(),
                ],
                log,
            );
        }

        let mut next_start = main_end_mib;
        let mut next_part_num = 2u32;

        if reserve_persist_mib > 0 {
            let p_end = (next_start + reserve_persist_mib).min(total_mib.saturating_sub(reserve_uefi_mib + 1));
            run_sys_cmd(
                "parted",
                &[
                    "-s".into(),
                    spec.device.path.clone(),
                    "mkpart".into(),
                    "primary".into(),
                    "ext4".into(),
                    format!("{}MiB", next_start),
                    format!("{}MiB", p_end),
                ],
                log,
            )?;
            next_start = p_end;
            next_part_num += 1;
        }

        if want_uefi_ntfs {
            // Secondary FAT EFI System Partition at the end of the disk for UEFI:NTFS
            run_sys_cmd(
                "parted",
                &[
                    "-s".into(),
                    spec.device.path.clone(),
                    "mkpart".into(),
                    "primary".into(),
                    "fat16".into(),
                    format!("{}MiB", next_start),
                    "100%".into(),
                ],
                log,
            )?;
            let _ = run_sys_cmd(
                "parted",
                &[
                    "-s".into(),
                    spec.device.path.clone(),
                    "set".into(),
                    next_part_num.to_string(),
                    "esp".into(),
                    "on".into(),
                ],
                log,
            );
            if spec.partition_scheme == PartitionScheme::Mbr {
                // Set boot flag on main partition for legacy BIOS + explicit 0xef ESP type for UEFI:NTFS
                let _ = run_sys_cmd(
                    "parted",
                    &[
                        "-s".into(),
                        spec.device.path.clone(),
                        "set".into(),
                        "1".into(),
                        "boot".into(),
                        "on".into(),
                    ],
                    log,
                );
                if cmd_in_path("sfdisk") {
                    let _ = run_sys_cmd(
                        "sfdisk",
                        &[
                            "--part-type".into(),
                            spec.device.path.clone(),
                            next_part_num.to_string(),
                            "ef".into(),
                        ],
                        log,
                    );
                }
            }
        }
        if spec.partition_scheme == PartitionScheme::Mbr && spec.old_bios_fixes {
            log("Aplicando arreglos de compatibilidad para BIOS antiguas (flag bootable + alineación MBR)...".into());
            let _ = run_sys_cmd(
                "parted",
                &[
                    "-s".into(),
                    spec.device.path.clone(),
                    "set".into(),
                    "1".into(),
                    "boot".into(),
                    "on".into(),
                ],
                log,
            );
        }
    } else if cmd_in_path("sfdisk") {
        let mut script = if spec.partition_scheme == PartitionScheme::Gpt {
            String::from("label: gpt\n")
        } else {
            String::from("label: dos\n")
        };
        if want_uefi_ntfs {
            let main_sectors = (main_end_mib - start_mib) * 2048;
            if spec.partition_scheme == PartitionScheme::Gpt {
                script.push_str(&format!(
                    "start=2048, size={}, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\n",
                    main_sectors
                ));
                script.push_str("type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B\n");
            } else {
                script.push_str(&format!("start=2048, size={}, type=7, bootable\n", main_sectors));
                script.push_str("type=ef\n");
            }
        } else if spec.partition_scheme == PartitionScheme::Gpt {
            script.push_str("start=2048, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\n");
        } else {
            let t = match spec.filesystem {
                FileSystemKind::Fat32 | FileSystemKind::LargeFat32 => {
                    format!("{:x}", FAT32_LBA_MBR_TYPE)
                }
                FileSystemKind::Ntfs | FileSystemKind::Exfat | FileSystemKind::Udf => "7".into(),
                FileSystemKind::Ext4 | FileSystemKind::Ext3 | FileSystemKind::Ext2 => "83".into(),
            };
            script.push_str(&format!("start=2048, type={}, bootable\n", t));
        }

        let mut child = Command::new("sfdisk")
            .arg(&spec.device.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Error iniciando sfdisk: {}", e))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(script.as_bytes());
        }
        let status = child.wait().map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("sfdisk falló ({})", status));
        }
    } else {
        return Err("No se encontró parted ni sfdisk. Instale parted o util-linux.".into());
    }

    if spec.partition_scheme == PartitionScheme::Mbr && spec.use_uefi_mbr_id {
        if let Ok(mut f) = OpenOptions::new().write(true).open(&spec.device.path) {
            if f.seek(SeekFrom::Start(0x1b8)).is_ok() {
                let _ = f.write_all(&UEFI_MBR_DISK_ID.to_le_bytes());
                log(format!(
                    "Escrito ID de disco MBR UEFI: 0x{:08X}",
                    UEFI_MBR_DISK_ID
                ));
            }
        }
    }

    let _ = Command::new("partprobe").arg(&spec.device.path).output();
    prog(
        0.38,
        "Esperando nodos de partición del kernel...".into(),
        String::new(),
    );

    let expected_count = 1
        + usize::from(reserve_persist_mib > 0)
        + usize::from(want_uefi_ntfs);
    let mut parts = Vec::new();
    for _ in 0..20 {
        parts = list_device_partitions(&spec.device.path);
        if parts.len() >= expected_count {
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    let main_part = parts
        .first()
        .cloned()
        .ok_or_else(|| "No se pudo detectar la partición principal creada".to_string())?;
    log(format!("Partición principal: {}", main_part));

    let persistence_part = if reserve_persist_mib > 0 {
        parts.get(1).cloned()
    } else {
        None
    };
    let uefi_ntfs_part = if want_uefi_ntfs {
        let idx = if reserve_persist_mib > 0 { 2 } else { 1 };
        parts.get(idx).cloned()
    } else {
        None
    };

    prog(
        0.48,
        format!("Formateando {} en {}...", main_part, spec.filesystem.label()),
        String::new(),
    );
    make_filesystem(
        &main_part,
        spec.filesystem,
        cluster,
        spec.device.sector_bytes,
        label,
        spec.quick_format,
        spec.ntfs_compression,
        log,
    )?;

    Ok(PartitionLayout {
        main_part,
        uefi_ntfs_part,
        persistence_part,
    })
}

fn list_device_partitions(dev_path: &str) -> Vec<String> {
    let mut out_parts = Vec::new();
    if let Ok(out) = Command::new("lsblk")
        .args(["-lnpo", "NAME,TYPE", dev_path])
        .output()
    {
        let txt = String::from_utf8_lossy(&out.stdout);
        for line in txt.lines() {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() >= 2 && cols[1] == "part" {
                out_parts.push(cols[0].to_string());
            }
        }
    }
    if out_parts.is_empty() {
        for idx in 1..=4 {
            let c1 = format!("{}{}", dev_path, idx);
            let c2 = format!("{}p{}", dev_path, idx);
            if Path::new(&c1).exists() {
                out_parts.push(c1);
            } else if Path::new(&c2).exists() {
                out_parts.push(c2);
            }
        }
    }
    out_parts
}

fn query_partition_uuid(part: &str) -> Option<String> {
    let out = Command::new("blkid")
        .args(["-s", "UUID", "-o", "value", part])
        .output()
        .ok()?;
    let uuid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if uuid.is_empty() {
        None
    } else {
        Some(uuid)
    }
}

fn make_filesystem<L: Fn(String)>(
    part: &str,
    fs: FileSystemKind,
    cluster: u32,
    sector_bytes: u64,
    label: &str,
    quick: bool,
    ntfs_compression: bool,
    log: &L,
) -> Result<(), String> {
    let lbl = if label.is_empty() { DEFAULT_LABEL } else { label };
    let sec = (sector_bytes.max(MIN_SECTOR_BYTES) as u32).max(512);
    log(if quick {
        "Quick format was selected".into()
    } else {
        "Slow format was selected".into()
    });

    match fs {
        FileSystemKind::Fat32 | FileSystemKind::LargeFat32 => {
            let mut args = vec!["-F".into(), "32".into(), "-I".into()];
            if cluster >= sec {
                let spc = (cluster / sec).clamp(1, 128);
                args.push("-s".into());
                args.push(spc.to_string());
                log(format!(
                    "Using cluster size: {} bytes ({} sectors/cluster)",
                    cluster, spc
                ));
            } else {
                log("Using default cluster size".into());
            }
            args.push("-n".into());
            args.push(normalize_label(lbl, true, ""));
            args.push(part.into());
            if cmd_in_path("mkfs.vfat") {
                run_sys_cmd("mkfs.vfat", &args, log)
            } else if cmd_in_path("mkfs.fat") {
                run_sys_cmd("mkfs.fat", &args, log)
            } else {
                Err("No se encontró mkfs.vfat. Instale dosfstools.".into())
            }
        }
        FileSystemKind::Ntfs => {
            let mut args = vec!["-F".into()];
            if quick {
                args.push("-f".into());
            }
            if cluster >= MIN_SECTOR_BYTES as u32 {
                args.push("-c".into());
                args.push(cluster.to_string());
            }
            if ntfs_compression {
                args.push("-C".into());
                log("NTFS compression is enabled".into());
            }
            args.push("-L".into());
            args.push(lbl.into());
            args.push(part.into());
            if cmd_in_path("mkfs.ntfs") {
                run_sys_cmd("mkfs.ntfs", &args, log)
            } else {
                Err("No se encontró mkfs.ntfs. Instale ntfs-3g / ntfsprogs.".into())
            }
        }
        FileSystemKind::Udf => {
            let args = vec!["-l".into(), lbl.into(), part.into()];
            if cmd_in_path("mkfs.udf") {
                run_sys_cmd("mkfs.udf", &args, log)
            } else if cmd_in_path("mkudffs") {
                run_sys_cmd("mkudffs", &args, log)
            } else {
                Err("No se encontró mkfs.udf / mkudffs. Instale udftools.".into())
            }
        }
        FileSystemKind::Exfat => {
            if !cmd_in_path("mkfs.exfat") {
                return Err("No se encontró mkfs.exfat. Instale exfatprogs.".into());
            }
            let mut args = vec!["-L".into(), lbl.into()];
            if !quick {
                args.push("-f".into());
            }
            if cluster >= 4096 {
                args.push("-c".into());
                args.push(cluster.to_string());
            }
            args.push(part.into());
            match run_sys_cmd("mkfs.exfat", &args, log) {
                Ok(()) => Ok(()),
                Err(_) => {
                    let legacy = vec!["-n".into(), lbl.into(), part.into()];
                    run_sys_cmd("mkfs.exfat", &legacy, log)
                }
            }
        }
        FileSystemKind::Ext4 | FileSystemKind::Ext3 | FileSystemKind::Ext2 => {
            let tool = match fs {
                FileSystemKind::Ext4 => "mkfs.ext4",
                FileSystemKind::Ext3 => "mkfs.ext3",
                _ => "mkfs.ext2",
            };
            let mut args = vec!["-F".into(), "-L".into(), lbl.into()];
            if !quick && fs == FileSystemKind::Ext4 {
                args.push("-E".into());
                args.push("lazy_itable_init=0,lazy_journal_init=0".into());
            }
            args.push(part.into());
            if cmd_in_path(tool) {
                run_sys_cmd(tool, &args, log)
            } else {
                Err(format!("No se encontró {}. Instale e2fsprogs.", tool))
            }
        }
    }
}

/// Mounts the target partition cleanly across Linux distros (uses ntfs3 / ntfs-3g for NTFS).
fn mount_partition_rw<L: Fn(String)>(
    part: &str,
    fs: FileSystemKind,
    dst_mnt: &Path,
    log: &L,
) -> Result<(), String> {
    let dst = dst_mnt.display().to_string();
    if fs == FileSystemKind::Ntfs {
        let ok_ntfs3 = Command::new("mount")
            .args(["-t", "ntfs3", "-o", "rw", part, &dst])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok_ntfs3 {
            log(format!("Montado {} con controlador kernel ntfs3 (rw).", part));
            return Ok(());
        }
        if cmd_in_path("ntfs-3g") {
            let ok_3g = Command::new("ntfs-3g")
                .args([part, &dst])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok_3g {
                log(format!("Montado {} con ntfs-3g (rw).", part));
                return Ok(());
            }
        }
    }
    run_sys_cmd("mount", &[part.to_string(), dst], log)
}

/// Installs FreeDOS bootable environment on a FAT32 MBR partition (for BIOS flashing & diagnostics).
fn install_freedos_to_partition<L, P>(
    dev_path: &str,
    part: &str,
    log: &L,
    prog: &P,
) -> Result<(), String>
where
    L: Fn(String),
    P: Fn(f32, String, String),
{
    prog(0.65, "Instalando arranque FreeDOS...".into(), String::new());
    if cmd_in_path("ms-sys") {
        let _ = run_sys_cmd("ms-sys", &["-m".into(), dev_path.to_string()], log);
        let _ = run_sys_cmd("ms-sys", &["-5".into(), part.to_string()], log);
        log("Escrito MBR + sector de arranque FAT32 FreeDOS mediante ms-sys.".into());
    } else if cmd_in_path("syslinux") {
        let _ = run_sys_cmd("syslinux", &["-i".into(), part.to_string()], log);
        install_syslinux_mbr(dev_path, log);
    }

    let mnt = std::env::temp_dir().join(format!("luxus-freedos-{}", std::process::id()));
    fs::create_dir_all(&mnt).map_err(|e| e.to_string())?;
    let mounted = Command::new("mount")
        .args([part, &mnt.display().to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if mounted {
        let locale_dir = mnt.join("LOCALE");
        let _ = fs::create_dir_all(&locale_dir);
        let _ = fs::write(
            mnt.join("AUTOEXEC.BAT"),
            "@echo off\r\nPROMPT $P$G\r\necho FreeDOS USB prepared by Luxus.\r\n",
        );
        let _ = fs::write(
            mnt.join("FDCONFIG.SYS"),
            "DOS=HIGH,UMB\r\nFILES=40\r\nBUFFERS=20\r\n",
        );
        // Copy cached KERNEL.SYS / COMMAND.COM if present in /usr/share/freedos or ~/.cache/luxus/freedos
        let mut copied_kernel = false;
        let mut dirs = vec![
            PathBuf::from("/usr/share/freedos"),
            PathBuf::from("/usr/share/luxus/freedos"),
        ];
        if let Some(h) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(h).join(".cache").join("luxus").join("freedos"));
        }
        for d in dirs {
            let k = d.join("KERNEL.SYS");
            let c = d.join("COMMAND.COM");
            if k.is_file() {
                let _ = fs::copy(&k, mnt.join("KERNEL.SYS"));
                copied_kernel = true;
            }
            if c.is_file() {
                let _ = fs::copy(&c, mnt.join("COMMAND.COM"));
            }
            if copied_kernel {
                break;
            }
        }
        if copied_kernel {
            log("Copiados KERNEL.SYS y COMMAND.COM de FreeDOS en la unidad.".into());
        } else {
            log("Aviso: unidad FAT32 preparada para FreeDOS (AUTOEXEC.BAT / FDCONFIG.SYS). Para arranque DOS completo, copie KERNEL.SYS y COMMAND.COM o descargue FreeDOS 1.3 desde DESCARGAR.".into());
        }
        let _ = Command::new("sync").status();
        let _ = Command::new("umount").arg(&mnt).status();
    }
    let _ = fs::remove_dir_all(&mnt);
    Ok(())
}

/// Writes Syslinux standard MBR (`mbr.bin`) from standard cross-distro locations if installed.
fn install_syslinux_mbr<L: Fn(String)>(dev_path: &str, log: &L) {
    let candidates = [
        "/usr/lib/syslinux/mbr/mbr.bin",
        "/usr/lib/syslinux/bios/mbr.bin",
        "/usr/share/syslinux/mbr.bin",
        "/usr/lib/SYSLINUX/mbr.bin",
    ];
    for c in candidates {
        if let Ok(bytes) = fs::read(c) {
            if !bytes.is_empty() && bytes.len() <= 440 {
                if let Ok(mut f) = OpenOptions::new().write(true).open(dev_path) {
                    if f.write_all(&bytes).is_ok() {
                        let _ = f.sync_all();
                        log(format!("Instalado MBR Syslinux desde {}", c));
                        return;
                    }
                }
            }
        }
    }
}

/// Mounts the target partition, extracts ISO contents (via loop-mount with `7z`/`7zz`/`bsdtar`
/// fallbacks), installs Legacy BIOS bootloaders if applicable, injects `autounattend.xml` for
/// Windows User Experience customization, and optionally writes `autorun.inf` extended label.
fn extract_iso_to_partition<L, P>(
    spec: &JobSpec,
    img_path: &Path,
    part: &str,
    label: &str,
    cancel: &Arc<AtomicBool>,
    log: &L,
    prog: &P,
) -> Result<(), String>
where
    L: Fn(String),
    P: Fn(f32, String, String),
{
    let base = std::env::temp_dir().join(format!("luxus-mnt-{}", std::process::id()));
    let iso_mnt = base.join("iso");
    let dst_mnt = base.join("dst");
    fs::create_dir_all(&iso_mnt).map_err(|e| e.to_string())?;
    fs::create_dir_all(&dst_mnt).map_err(|e| e.to_string())?;

    let cleanup = || {
        let _ = Command::new("umount").arg(&iso_mnt).status();
        let _ = Command::new("umount").arg(&dst_mnt).status();
        let _ = fs::remove_dir_all(&base);
    };

    if let Err(e) = mount_partition_rw(part, spec.filesystem, &dst_mnt, log) {
        cleanup();
        return Err(e);
    }

    // Try loop-mount first; if UDF/loop fails, fall back cleanly to 7z/7zz/bsdtar extraction
    let loop_ok = Command::new("mount")
        .args([
            "-o",
            "loop,ro",
            &img_path.display().to_string(),
            &iso_mnt.display().to_string(),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if loop_ok {
        let mut files: Vec<(PathBuf, PathBuf, u64)> = Vec::new();
        let mut total_bytes: u64 = 0;
        collect_files_recursive(&iso_mnt, &dst_mnt, &mut files, &mut total_bytes, 0);
        if spec.filesystem.is_fat32() {
            if let Some((big_src, _, big_sz)) =
                files.iter().find(|(_, _, sz)| *sz > u32::MAX as u64)
            {
                cleanup();
                return Err(format!(
                    "La imagen contiene un archivo mayor de 4 GB ({} = {}), incompatible con FAT32. Seleccione NTFS (con UEFI:NTFS) o exFAT.",
                    big_src.file_name().and_then(|s| s.to_str()).unwrap_or("archivo"),
                    human_size(*big_sz)
                ));
            }
        }
        log(format!(
            "Copiando {} archivos ({}) desde la imagen ISO...",
            files.len(),
            human_size(total_bytes)
        ));

        let mut copied: u64 = 0;
        let mut buf = vec![0u8; 2 * 1024 * 1024];
        let t0 = Instant::now();
        let mut last_ui = Instant::now() - Duration::from_secs(1);

        for (src, dst, _sz) in files {
            if cancel.load(Ordering::SeqCst) {
                cleanup();
                return Err("CANCELLED".into());
            }
            if let Some(parent) = dst.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let Ok(mut r) = File::open(&src) else { continue };
            let Ok(mut w) = File::create(&dst) else { continue };
            loop {
                let n = match r.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => {
                        cleanup();
                        return Err(format!("Error leyendo {}: {}", src.display(), e));
                    }
                };
                if let Err(e) = w.write_all(&buf[..n]) {
                    cleanup();
                    return Err(format!("Error copiando {}: {}", dst.display(), e));
                }
                copied += n as u64;
                if last_ui.elapsed() >= Duration::from_millis(120) {
                    last_ui = Instant::now();
                    let pct = (copied as f64 / total_bytes.max(1) as f64).clamp(0.0, 1.0);
                    let mb_s = (copied as f64 / (1024.0 * 1024.0))
                        / t0.elapsed().as_secs_f64().max(0.05);
                    prog(
                        0.52 + (pct as f32) * 0.40,
                        format!(
                            "Copiando archivos ISO: {:.1}% ({} / {})",
                            pct * 100.0,
                            human_size(copied),
                            human_size(total_bytes)
                        ),
                        format!("{:.1} MB/s", mb_s),
                    );
                }
            }
        }
    } else if cmd_in_path("7z") || cmd_in_path("7zz") {
        let bin = if cmd_in_path("7z") { "7z" } else { "7zz" };
        log(format!("Montaje loop no disponible para esta ISO; extrayendo con {}...", bin));
        prog(0.55, format!("Extrayendo imagen ISO con {}...", bin), String::new());
        if let Err(e) = run_sys_cmd(
            bin,
            &[
                "x".into(),
                "-y".into(),
                format!("-o{}", dst_mnt.display()),
                img_path.display().to_string(),
            ],
            log,
        ) {
            cleanup();
            return Err(e);
        }
    } else if cmd_in_path("bsdtar") {
        log("Montaje loop no disponible; extrayendo imagen ISO con bsdtar...".into());
        prog(0.55, "Extrayendo imagen ISO con bsdtar...".into(), String::new());
        if let Err(e) = run_sys_cmd(
            "bsdtar",
            &[
                "-xf".into(),
                img_path.display().to_string(),
                "-C".into(),
                dst_mnt.display().to_string(),
            ],
            log,
        ) {
            cleanup();
            return Err(e);
        }
    } else {
        cleanup();
        return Err(
            "No se pudo montar la ISO en loop ni se encontró 7z/bsdtar para extraerla.".into(),
        );
    }

    // Apply Windows User Experience customization (`autounattend.xml`) if configured
    if let Some(ref win_ux) = spec.windows_ux {
        if let Err(e) = write_autounattend_to_mount(&dst_mnt, win_ux, log) {
            cleanup();
            return Err(e);
        }
    }

    // For extracted Linux ISOs with isolinux/, create syslinux.cfg so Syslinux can boot in Legacy BIOS
    let mut has_isolinux_dir = false;
    let isolinux_dir = dst_mnt.join("isolinux");
    if isolinux_dir.is_dir() {
        has_isolinux_dir = true;
        let iso_cfg = isolinux_dir.join("isolinux.cfg");
        let sys_cfg = isolinux_dir.join("syslinux.cfg");
        if iso_cfg.is_file() && !sys_cfg.exists() {
            let _ = fs::copy(&iso_cfg, &sys_cfg);
            log("Creado isolinux/syslinux.cfg para arranque Legacy BIOS.".into());
        }
    }

    if spec.extended_label_icon {
        let autorun = dst_mnt.join("autorun.inf");
        if !autorun.exists() {
            let _ = fs::write(&autorun, format!("[AutoRun]\r\nlabel={}\r\n", label));
            log(format!("Creado autorun.inf con etiqueta '{}'", label));
        }
    }

    let _ = Command::new("sync").status();
    cleanup();

    // Post-unmount Legacy BIOS bootloader installation on MBR drives
    if spec.partition_scheme == PartitionScheme::Mbr {
        if spec.image_option == ImageWriteOption::WindowsStandard && cmd_in_path("ms-sys") {
            let _ = run_sys_cmd("ms-sys", &["-7".into(), spec.device.path.clone()], log);
            if spec.filesystem == FileSystemKind::Ntfs {
                let _ = run_sys_cmd("ms-sys", &["-n".into(), part.to_string()], log);
            }
            log("Instalado MBR/PBR de Windows Legacy BIOS mediante ms-sys.".into());
        } else if has_isolinux_dir && spec.filesystem.is_fat32() && cmd_in_path("syslinux") {
            let _ = run_sys_cmd(
                "syslinux",
                &["-i".into(), "-d".into(), "/isolinux".into(), part.to_string()],
                log,
            );
            install_syslinux_mbr(&spec.device.path, log);
        }
    }
    Ok(())
}

fn collect_files_recursive(
    src_dir: &Path,
    dst_dir: &Path,
    out: &mut Vec<(PathBuf, PathBuf, u64)>,
    total: &mut u64,
    depth: usize,
) {
    if depth > 48 {
        return;
    }
    let Ok(entries) = fs::read_dir(src_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let target = dst_dir.join(entry.file_name());
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            collect_files_recursive(&path, &target, out, total, depth + 1);
        } else if ft.is_file() {
            if let Ok(meta) = entry.metadata() {
                *total += meta.len();
                out.push((path, target, meta.len()));
            }
        } else if ft.is_symlink() {
            // Follow file symlinks safely without recursing into symlinked directories
            if let Ok(meta) = fs::metadata(&path) {
                if meta.is_file() {
                    *total += meta.len();
                    out.push((path, target, meta.len()));
                }
            }
        }
    }
}
