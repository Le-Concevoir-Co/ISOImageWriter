## ISOImageWriter Rust Port (Linux-only)

This project contains a Rust rewrite of the original `dd Utility` shell scripts found under the `source/` directory. The goal is to provide a safer, more ergonomic command-line tool for **Linux** that performs the same core tasks:

- **Backup** a removable block device (e.g. SD card, USB stick) to an image file
- **Restore** an image file back to a removable block device

Supported image formats on restore:

- **Raw**: `*.img`
- **ISO**: `*.iso`
- **Compressed**:
  - `*.gz` (gzip)
  - `*.zip`
  - `*.xz`

### Building

You need a recent Rust toolchain (Rust 1.70+ recommended).

```bash
cd /home/kernelghost/Desktop/ISOImageWriter
cargo build --release
```

The resulting binary will be at:

```bash
target/release/isoimagewriter
```

### Running (Linux only)

This tool is intended to be run as **root** (or via `sudo`), because it reads/writes raw block devices:

```bash
sudo ./target/release/isoimagewriter
```

If you run it without arguments, it starts in an **interactive mode** similar in spirit to the original Zenity UI:

1. Choose **Backup** or **Restore**
2. Select the device and/or image file
3. Confirm the operation after seeing a summary

You can also use explicit subcommands:

```bash
# Backup /dev/sdb to backup.img.gz with gzip compression
sudo isoimagewriter backup --device /dev/sdb --output /path/to/backup.img.gz --gzip

# Restore image.img (or .iso/.gz/.zip/.xz) to /dev/sdb
sudo isoimagewriter restore --image /path/to/image.img --device /dev/sdb
```

### Differences from the original shell scripts

- **Platform**: This Rust tool currently targets **Linux only**. All macOS-specific behavior and AppleScript dialogs are intentionally omitted.
- **UI**: Uses a **text-based CLI** instead of Zenity / notify-send GUI dialogs.
- **Progress**: Uses a terminal progress bar, based on estimated uncompressed size, rather than `lsof`/`dd` monitoring.
- **Device detection**: Uses `/sys/block` to list **removable** devices, filtering out loop, RAM, optical, and dm devices.
- **Safety**:
  - Attempts to unmount any mounted partitions on the target device before writing.
  - Warns if the image is larger than the destination.
  - Gives extra warnings when a device looks like a primary system disk (e.g. `/dev/sda`, `/dev/nvme0*`).

### Mapping to original `ddutility-1.6.sh`

Approximate mapping of behaviors:

- `Backup`:
  - Original: prompts via Zenity, can create `.img` or `.gz`.
  - Rust: `backup` subcommand or interactive mode, creates `.img` or `.img.gz` using gzip compression in-process.
- `Restore`:
  - Original: supports `img`, `iso`, `zip`, `gz`, `xz`, piping through `dd`.
  - Rust: supports the same extensions, using Rust libraries for gzip/xz/zip, streaming directly to the block device.
- **Size checks and warnings**:
  - Original: compares image size vs device size and free space, warns via GUI.
  - Rust: compares sizes and prints warnings in the terminal, asking for confirmation before proceeding.

If you relied on any specific detail from the original shell scripts that you do not see here (or in `src/main.rs`), it can usually be added on top of this foundation.

