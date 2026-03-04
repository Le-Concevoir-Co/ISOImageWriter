use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use indicatif::{ProgressBar, ProgressStyle};
use walkdir::WalkDir;
use zip::read::ZipArchive;

#[derive(Debug, Parser)]
#[command(
    name = "isoimagewriter",
    version,
    about = "Linux-only ISO/IMG backup and restore utility (Rust rewrite of dd Utility 1.6)"
)]
struct Cli {
    /// Increase output verbosity
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<CommandKind>,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    /// Backup a block device to an image file
    Backup {
        /// Source block device (e.g. /dev/sdb). If omitted, you'll be prompted.
        #[arg(short, long)]
        device: Option<PathBuf>,
        /// Destination image path. If omitted, you'll be prompted.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Compress backup with gzip (.gz)
        #[arg(long)]
        gzip: bool,
    },
    /// Restore an image file to a block device
    Restore {
        /// Image file to restore (img, iso, gz, zip, xz)
        #[arg(short, long)]
        image: Option<PathBuf>,
        /// Destination block device (e.g. /dev/sdb). If omitted, you'll be prompted.
        #[arg(short, long)]
        device: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CompressionKind {
    None,
    Gzip,
}

fn main() -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        bail!("This Rust rewrite currently supports Linux only.");
    }

    let cli = Cli::parse();

    if unsafe { libc::geteuid() } != 0 {
        eprintln!("WARNING: This tool is intended to run as root (via sudo). Some operations may fail otherwise.");
    }

    match cli.command {
        Some(CommandKind::Backup { device, output, gzip }) => {
            run_backup(device, output, if gzip { CompressionKind::Gzip } else { CompressionKind::None })?;
        }
        Some(CommandKind::Restore { image, device }) => {
            run_restore(image, device)?;
        }
        None => {
            println!("No command given. Starting interactive mode.");
            interactive_menu()?;
        }
    }

    Ok(())
}

fn interactive_menu() -> Result<()> {
    println!("=== ISOImageWriter (Rust) ===");
    println!("1) Backup (device -> image)");
    println!("2) Restore (image -> device)");
    println!("q) Quit");
    print!("Select option: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    match input.trim() {
        "1" => {
            let device = prompt_device(None)?;
            let (output, compression) = prompt_backup_output()?;
            run_backup(Some(device), Some(output), compression)
        }
        "2" => {
            let image = prompt_image_file(None)?;
            let device = prompt_device(None)?;
            run_restore(Some(image), Some(device))
        }
        "q" | "Q" => Ok(()),
        _ => {
            println!("Unknown option.");
            Ok(())
        }
    }
}

fn run_backup(device: Option<PathBuf>, output: Option<PathBuf>, compression: CompressionKind) -> Result<()> {
    let device = match device {
        Some(d) => d,
        None => prompt_device(None)?,
    };

    let output = match output {
        Some(o) => o,
        None => prompt_backup_output()?.0,
    };

    ensure_block_device(&device)?;
    ensure_not_system_device(&device)?;

    let dev_size = get_block_device_size(&device)
        .with_context(|| format!("Failed to determine size of {}", device.display()))?;

    check_free_space_for_backup(&output, dev_size)?;

    println!(
        "About to BACKUP:\n  Source: {}\n  Size:   {}\n  Output: {}\n  Compression: {:?}",
        device.display(),
        human_bytes(dev_size),
        output.display(),
        compression
    );
    if !prompt_confirm("Start backup? This will READ from the device but not write to it.")? {
        println!("Backup cancelled.");
        return Ok(());
    }

    unmount_device_partitions(&device)?;

    match compression {
        CompressionKind::None => copy_raw(&device, &output, dev_size)?,
        CompressionKind::Gzip => backup_gzip(&device, &output, dev_size)?,
    }

    println!("Backup complete.");
    Ok(())
}

fn run_restore(image: Option<PathBuf>, device: Option<PathBuf>) -> Result<()> {
    let image = match image {
        Some(i) => i,
        None => prompt_image_file(None)?,
    };
    let device = match device {
        Some(d) => d,
        None => prompt_device(None)?,
    };

    ensure_block_device(&device)?;
    ensure_not_system_device(&device)?;

    let (uncompressed_size, kind) = detect_image_kind(&image)
        .with_context(|| format!("Failed to analyze image {}", image.display()))?;
    let dev_size = get_block_device_size(&device)
        .with_context(|| format!("Failed to determine size of {}", device.display()))?;

    if uncompressed_size > dev_size {
        println!(
            "WARNING: image requires {} but device is only {}.",
            human_bytes(uncompressed_size),
            human_bytes(dev_size)
        );
        if !prompt_confirm("Continue anyway? (Data may be truncated or write may fail.)")? {
            println!("Restore cancelled.");
            return Ok(());
        }
    }

    println!(
        "About to RESTORE:\n  Image: {}\n  Type:  {:?}\n  Size:  {}\n  Device: {}",
        image.display(),
        kind,
        human_bytes(uncompressed_size),
        device.display()
    );
    if !prompt_confirm("Start restore? THIS WILL OVERWRITE ALL DATA on the device.")? {
        println!("Restore cancelled.");
        return Ok(());
    }

    unmount_device_partitions(&device)?;

    restore_image(&image, &device, uncompressed_size, kind)?;

    println!("Restore complete.");
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum ImageKind {
    Raw,
    Gzip,
    Zip,
    Xz,
    Iso,
}

fn detect_image_kind(path: &Path) -> Result<(u64, ImageKind)> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let meta = fs::metadata(path)?;
    let size = meta.len();

    match ext.as_str() {
        "img" => Ok((size, ImageKind::Raw)),
        "iso" => Ok((size, ImageKind::Iso)),
        "gz" => {
            // Try to read uncompressed size from footer if present; otherwise approximate
            // by reading and counting bytes once (may be slow but acceptable).
            let file = File::open(path)?;
            let mut decoder = GzDecoder::new(file);
            let mut buf = [0u8; 1024 * 1024];
            let mut total = 0u64;
            loop {
                let n = decoder.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                total += n as u64;
            }
            Ok((total, ImageKind::Gzip))
        }
        "zip" => {
            let file = File::open(path)?;
            let mut archive = ZipArchive::new(file)?;
            if archive.len() == 0 {
                bail!("ZIP archive is empty");
            }
            let mut total = 0u64;
            for i in 0..archive.len() {
                let f = archive.by_index(i)?;
                total += f.size();
            }
            Ok((total, ImageKind::Zip))
        }
        "xz" => {
            let file = File::open(path)?;
            let mut decoder = xz2::read::XzDecoder::new(file);
            let mut buf = [0u8; 1024 * 1024];
            let mut total = 0u64;
            loop {
                let n = decoder.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                total += n as u64;
            }
            Ok((total, ImageKind::Xz))
        }
        _ => Ok((size, ImageKind::Raw)),
    }
}

fn copy_raw(src: &Path, dst: &Path, total: u64) -> Result<()> {
    let mut input = BufReader::new(
        OpenOptions::new()
            .read(true)
            .open(src)
            .with_context(|| format!("Failed to open {}", src.display()))?,
    );
    let mut output = BufWriter::new(
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(dst)
            .with_context(|| format!("Failed to open {}", dst.display()))?,
    );

    let bar = progress_bar(total);

    let mut buf = vec![0u8; 1024 * 1024];
    let mut copied = 0u64;
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n])?;
        copied += n as u64;
        bar.set_position(copied.min(total));
    }

    output.flush()?;
    bar.finish_and_clear();
    Ok(())
}

fn backup_gzip(device: &Path, output: &Path, total: u64) -> Result<()> {
    let mut input = BufReader::new(
        OpenOptions::new()
            .read(true)
            .open(device)
            .with_context(|| format!("Failed to open {}", device.display()))?,
    );

    let out_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(output)
        .with_context(|| format!("Failed to open {}", output.display()))?;
    let mut encoder = GzEncoder::new(out_file, Compression::default());

    let bar = progress_bar(total);
    let mut buf = vec![0u8; 1024 * 1024];
    let mut read_total = 0u64;

    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        encoder.write_all(&buf[..n])?;
        read_total += n as u64;
        bar.set_position(read_total.min(total));
    }

    encoder.finish()?;
    bar.finish_and_clear();
    Ok(())
}

fn restore_image(image: &Path, device: &Path, total: u64, kind: ImageKind) -> Result<()> {
    match kind {
        ImageKind::Raw | ImageKind::Iso => copy_raw(image, device, total),
        ImageKind::Gzip => {
            let file = File::open(image)?;
            let mut decoder = GzDecoder::new(file);
            write_stream_to_device(&mut decoder, device, total)
        }
        ImageKind::Zip => {
            let file = File::open(image)?;
            let mut archive = ZipArchive::new(file)?;
            if archive.len() == 0 {
                bail!("ZIP archive is empty");
            }
            // Concatenate all entries in order into the device
            let bar = progress_bar(total);
            let mut output = BufWriter::new(
                OpenOptions::new()
                    .write(true)
                    .open(device)
                    .with_context(|| format!("Failed to open {}", device.display()))?,
            );
            let mut copied = 0u64;
            let mut buf = vec![0u8; 1024 * 1024];
            for i in 0..archive.len() {
                let mut entry = archive.by_index(i)?;
                loop {
                    let n = entry.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    output.write_all(&buf[..n])?;
                    copied += n as u64;
                    bar.set_position(copied.min(total));
                }
            }
            output.flush()?;
            bar.finish_and_clear();
            Ok(())
        }
        ImageKind::Xz => {
            let file = File::open(image)?;
            let mut decoder = xz2::read::XzDecoder::new(file);
            write_stream_to_device(&mut decoder, device, total)
        }
    }
}

fn write_stream_to_device<R: Read>(reader: &mut R, device: &Path, total: u64) -> Result<()> {
    let mut output = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .open(device)
            .with_context(|| format!("Failed to open {}", device.display()))?,
    );
    let bar = progress_bar(total);
    let mut buf = vec![0u8; 1024 * 1024];
    let mut copied = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n])?;
        copied += n as u64;
        bar.set_position(copied.min(total));
    }
    output.flush()?;
    bar.finish_and_clear();
    Ok(())
}

fn progress_bar(len: u64) -> ProgressBar {
    let bar = ProgressBar::new(len);
    bar.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
            .unwrap()
            .progress_chars("##-"),
    );
    bar.enable_steady_tick(Duration::from_millis(100));
    bar
}

fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1000.0;
    const MB: f64 = KB * 1000.0;
    const GB: f64 = MB * 1000.0;
    const TB: f64 = GB * 1000.0;
    let b = bytes as f64;
    if b >= TB {
        format!("{:.1} TB", b / TB)
    } else if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn prompt_confirm(msg: &str) -> Result<bool> {
    println!("{msg} [y/N]");
    print!("> ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(matches!(input.trim(), "y" | "Y" | "yes" | "YES"))
}

fn prompt_device(default: Option<PathBuf>) -> Result<PathBuf> {
    let devices = list_block_devices()?;
    if devices.is_empty() {
        bail!("No removable block devices detected. Insert a memory card or USB drive and try again.");
    }

    println!("Available removable block devices:");
    for (idx, dev) in devices.iter().enumerate() {
        println!(
            "  {}) {}  {}  {}",
            idx + 1,
            dev.path.display(),
            dev.size_human,
            dev.model.clone().unwrap_or_default()
        );
    }
    println!("WARNING: selecting the wrong device can destroy data.");

    loop {
        print!("Select device by number (or enter explicit path like /dev/sdX): ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            if let Some(d) = &default {
                return Ok(d.clone());
            }
            continue;
        }
        if let Ok(n) = trimmed.parse::<usize>() {
            if n >= 1 && n <= devices.len() {
                return Ok(devices[n - 1].path.clone());
            }
        }
        let p = PathBuf::from(trimmed);
        if p.exists() {
            return Ok(p);
        }
        println!("Invalid selection, try again.");
    }
}

fn prompt_backup_output() -> Result<(PathBuf, CompressionKind)> {
    println!("Enter destination image path (e.g. /home/user/backup.img or backup.img.gz):");
    print!("> ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let path = PathBuf::from(input.trim());
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            bail!("Destination directory {} does not exist", parent.display());
        }
    }
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let compression = if ext == "gz" {
        CompressionKind::Gzip
    } else {
        if prompt_confirm("Compress backup with gzip?")? {
            CompressionKind::Gzip
        } else {
            CompressionKind::None
        }
    };
    let final_path = if ext.is_empty() {
        match compression {
            CompressionKind::None => path.with_extension("img"),
            CompressionKind::Gzip => path.with_extension("img.gz"),
        }
    } else {
        path
    };
    Ok((final_path, compression))
}

fn prompt_image_file(default: Option<PathBuf>) -> Result<PathBuf> {
    println!("Enter path to image file (img, iso, gz, zip, xz):");
    print!("> ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        if let Some(d) = default {
            return Ok(d);
        }
    }
    let p = PathBuf::from(trimmed);
    if !p.exists() {
        bail!("Image file {} does not exist", p.display());
    }
    Ok(p)
}

#[derive(Debug)]
struct BlockDevice {
    path: PathBuf,
    size_bytes: u64,
    size_human: String,
    model: Option<String>,
}

fn list_block_devices() -> Result<Vec<BlockDevice>> {
    let mut devices = Vec::new();
    for entry in WalkDir::new("/sys/block").min_depth(1).max_depth(1) {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        // Heuristic: skip loop devices, RAM disks, optical drives
        if name.starts_with("loop")
            || name.starts_with("ram")
            || name.starts_with("sr")
            || name.starts_with("dm-")
        {
            continue;
        }
        let dev_path = PathBuf::from(format!("/dev/{name}"));
        if !dev_path.exists() {
            continue;
        }
        let removable = fs::read_to_string(entry.path().join("removable"))
            .unwrap_or_default()
            .trim()
            .to_string();
        if removable != "1" {
            continue;
        }
        let size_sectors = fs::read_to_string(entry.path().join("size"))
            .unwrap_or_default()
            .trim()
            .parse::<u64>()
            .unwrap_or(0);
        let sector_size = fs::read_to_string(entry.path().join("queue/logical_block_size"))
            .unwrap_or_else(|_| "512".to_string())
            .trim()
            .parse::<u64>()
            .unwrap_or(512);
        let size_bytes = size_sectors.saturating_mul(sector_size);
        let model = fs::read_to_string(entry.path().join("device/model"))
            .ok()
            .map(|s| s.trim().to_string());
        devices.push(BlockDevice {
            path: dev_path,
            size_bytes,
            size_human: human_bytes(size_bytes),
            model,
        });
    }
    Ok(devices)
}

fn get_block_device_size(dev: &Path) -> Result<u64> {
    // Prefer /sys/block if available
    if let Some(name) = dev.file_name().and_then(|s| s.to_str()) {
        let sys_path = PathBuf::from("/sys/block").join(name);
        if sys_path.exists() {
            let size_sectors = fs::read_to_string(sys_path.join("size"))
                .unwrap_or_default()
                .trim()
                .parse::<u64>()
                .unwrap_or(0);
            let sector_size = fs::read_to_string(sys_path.join("queue/logical_block_size"))
                .unwrap_or_else(|_| "512".to_string())
                .trim()
                .parse::<u64>()
                .unwrap_or(512);
            if size_sectors > 0 {
                return Ok(size_sectors.saturating_mul(sector_size));
            }
        }
    }

    // Fallback: use metadata size (works for regular files, not always for block devices)
    let meta = fs::metadata(dev)?;
    Ok(meta.len())
}

fn ensure_block_device(dev: &Path) -> Result<()> {
    let meta = fs::metadata(dev)
        .with_context(|| format!("{} does not exist", dev.display()))?;
    if !meta.file_type().is_block_device() {
        bail!("{} is not a block device", dev.display());
    }
    Ok(())
}

fn ensure_not_system_device(dev: &Path) -> Result<()> {
    let path = dev.to_string_lossy();
    if path == "/dev/sda" || path.starts_with("/dev/nvme0") {
        println!(
            "WARNING: {} looks like a primary system disk. Writing to it can destroy your OS.",
            path
        );
        if !prompt_confirm("Are you absolutely sure you want to use this device?")? {
            bail!("Aborting for safety.");
        }
    }
    Ok(())
}

fn check_free_space_for_backup(output: &Path, required_bytes: u64) -> Result<()> {
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let statvfs = nix::sys::statvfs::statvfs(parent)?;
    let free = statvfs.blocks_available() * statvfs.block_size() as u64;
    if free < required_bytes {
        println!(
            "WARNING: Destination filesystem has only {} free, but backup needs about {}.",
            human_bytes(free),
            human_bytes(required_bytes)
        );
        if !prompt_confirm("Continue anyway?")? {
            bail!("Not enough space for backup.");
        }
    }
    Ok(())
}

fn unmount_device_partitions(dev: &Path) -> Result<()> {
    let dev_str = dev.to_string_lossy();
    println!("Attempting to unmount partitions on {dev_str}...");

    // Find mounted entries that reference this device
    let mounts = fs::read_to_string("/proc/mounts")?;
    for line in mounts.lines() {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            let source = parts[0];
            let mountpoint = parts[1];
            if source.starts_with(&*dev_str) {
                println!("  Unmounting {source} from {mountpoint}...");
                let status = Command::new("umount").arg(mountpoint).status()?;
                if !status.success() {
                    println!("  Failed to unmount {mountpoint} (continuing).");
                }
            }
        }
    }
    Ok(())
}

