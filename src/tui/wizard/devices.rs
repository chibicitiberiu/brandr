//! Target devices as netflash shows them: whole disks only, with their bus.

use std::io::stdout;

use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

use crate::util::device::{self, WriteTarget, enumerate_devices};

/// Largest floppy format (2.88MB ED).
pub const FLOPPY_MAX: u64 = 2_949_120;

/// Floppy drives (fd0...): their size is unknown (0) until the medium is read.
pub fn is_floppy(d: &WriteTarget) -> bool {
    d.name.starts_with("fd")
}

/// Whole disks worth offering: no partitions, no virtual block devices (loop,
/// ramdisks, zram, nbd, optical), nothing empty, except floppy drives, which
/// report 0 bytes until a disk is read.
fn is_real_disk(d: &WriteTarget) -> bool {
    let virtual_dev = ["loop", "ram", "zram", "nbd", "sr"]
        .iter()
        .any(|p| d.name.starts_with(p));
    !virtual_dev
        && d.target_type == device::Type::Disk
        && (is_floppy(d) || d.size.bytes().is_some_and(|b| b > 0))
}

pub fn targets() -> Vec<WriteTarget> {
    let mut t: Vec<WriteTarget> = enumerate_devices().filter(is_real_disk).collect();
    t.sort();
    t
}

/// Capacity used for the "fits?" check; floppy drives count as the largest format.
pub fn capacity(d: &WriteTarget) -> Option<u64> {
    if is_floppy(d) {
        Some(FLOPPY_MAX)
    } else {
        d.size.bytes()
    }
}

pub fn size_label(d: &WriteTarget) -> String {
    if is_floppy(d) {
        "floppy".into()
    } else {
        d.size.to_string()
    }
}

/// Which bus a block device hangs off, from its sysfs path (Linux).
pub fn bus(d: &WriteTarget) -> &'static str {
    if is_floppy(d) {
        return "floppy";
    }
    let path = std::fs::canonicalize(format!("/sys/class/block/{}", d.name))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    [
        ("/usb", "USB"),
        ("/nvme", "NVMe"),
        ("/mmc", "SD/MMC"),
        ("/virtio", "virtio"),
        ("/ata", "ATA"),
        ("/ide", "IDE"),
    ]
    .iter()
    .find(|(needle, _)| path.contains(needle))
    .map_or("?", |(_, label)| label)
}

/// Make the floppy driver notice a swapped disk before we write to it.
///
/// After a media change Linux fails the first access (ENXIO on open, or EIO
/// "disk absent or changed during operation") and only then revalidates the
/// drive. Reading sector 0 a few times absorbs that. Err = still unreadable
/// (no disk in the drive, or an unformatted one).
pub fn settle_floppy(d: &WriteTarget) -> std::io::Result<()> {
    use std::io::Read;
    let mut last = None;
    for _ in 0..5 {
        match std::fs::File::open(&d.devnode).and_then(|mut f| f.read_exact(&mut [0u8; 512])) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(600));
    }
    Err(last.expect("at least one attempt"))
}

/// Run cfdisk (or fdisk) on a disk, with the terminal handed over to it.
pub fn partition(d: &WriteTarget) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen)?;
    let mut result = Ok(());
    let mut found = false;
    for tool in ["cfdisk", "fdisk"] {
        match std::process::Command::new(tool).arg(&d.devnode).status() {
            Ok(_) => {
                found = true;
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                result = Err(e.into());
                found = true;
                break;
            }
        }
    }
    if !found {
        eprintln!("Neither cfdisk nor fdisk is installed. Press Enter.");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    execute!(stdout(), EnterAlternateScreen)?;
    enable_raw_mode()?;
    result
}
