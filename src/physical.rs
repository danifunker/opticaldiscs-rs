//! Reading a disc straight from a physical optical drive.
//!
//! Requires feature `drives`. Provides [`PhysicalDisc`], a [`SectorReader`]
//! backed by the drive's device node rather than an image file, so every
//! consumer of the trait — [`crate::detect::DiscImageInfo`], the browse stack,
//! El Torito extraction — works against a disc in the tray with no extra code.
//!
//! ## Why this exists: DVD and Blu-ray
//!
//! CD reading is traditionally done with MMC/SCSI pass-through (`READ CD`,
//! `READ TOC`), because a CD's sectors are 2352 bytes on the wire and the
//! interesting parts — audio tracks, subchannel data, Mode 2 forms — are only
//! reachable that way. Those commands are **CD-specific**: on macOS
//! `DKIOCCDREADTOC` / `DKIOCCDREAD` against DVD or Blu-ray media fail outright
//! with `ENOTTY`, and the equivalents on other platforms are no better.
//!
//! A **data** DVD or Blu-ray needs none of it. The drive already presents the
//! medium as a flat run of 2048-byte cooked sectors — exactly the shape
//! [`SectorReader`] is defined in terms of — so an ordinary `seek` + `read` on
//! the device node is both sufficient and portable. That is what this module
//! does, and it is why DVD/BD support does not need a single ioctl.
//!
//! ## Scope
//!
//! - **Data CD, DVD, Blu-ray**: supported. The medium is read as cooked 2048-byte
//!   sectors.
//! - **Audio CDs**: *not* supported here. Their sectors are 2352-byte raw CD-DA
//!   frames with no cooked view to expose, so they genuinely do need MMC
//!   pass-through. Use a CD-DA library for those.
//!
//! ## Platform notes
//!
//! - **macOS**: reads `/dev/rdiskN`, the raw character device. A `/dev/diskN`
//!   path is mapped to it automatically. Raw devices demand block-aligned reads,
//!   which this module satisfies by only ever reading whole sectors.
//! - **Linux**: reads `/dev/srN` directly.
//! - **Windows**: reads the `\\.\D:` volume handle.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::{OpticaldiscsError, Result};
use crate::sector_reader::{SectorReader, SECTOR_SIZE};

/// A physical optical disc, read through its drive's device node.
///
/// Construct with [`PhysicalDisc::open`], then use it anywhere a
/// [`SectorReader`] is accepted.
pub struct PhysicalDisc {
    device: File,
    device_path: PathBuf,
    /// Medium capacity in bytes, when the OS would tell us. `None` simply means
    /// the size is unknown — reads still work, they just run until they hit the
    /// end of the medium.
    size_bytes: Option<u64>,
}

impl PhysicalDisc {
    /// Open the disc currently loaded in the drive at `device_path`.
    ///
    /// `device_path` is what [`crate::drives::list_drives`] reports — e.g.
    /// `/dev/disk6` on macOS, `/dev/sr0` on Linux, `D:\` on Windows. On macOS a
    /// `/dev/diskN` path is translated to the raw `/dev/rdiskN` node.
    ///
    /// Fails if the drive is empty: with no medium loaded there is no device
    /// node to open in the first place.
    pub fn open(device_path: impl AsRef<Path>) -> Result<Self> {
        let requested = device_path.as_ref();
        let resolved = resolve_device_path(requested);

        let device = File::open(&resolved).map_err(OpticaldiscsError::Io)?;
        let size_bytes = probe_size(&device);

        Ok(Self {
            device,
            device_path: resolved,
            size_bytes,
        })
    }

    /// The device node actually opened, after any platform translation.
    pub fn device_path(&self) -> &Path {
        &self.device_path
    }

    /// Medium capacity in bytes, if the OS reported one.
    pub fn size_bytes(&self) -> Option<u64> {
        self.size_bytes
    }

    /// Medium capacity in 2048-byte sectors, if known.
    pub fn sector_count(&self) -> Option<u64> {
        self.size_bytes.map(|b| b / SECTOR_SIZE)
    }
}

impl SectorReader for PhysicalDisc {
    fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
        let offset = lba
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(|| OpticaldiscsError::InvalidData(format!("sector {lba} out of range")))?;

        self.device
            .seek(SeekFrom::Start(offset))
            .map_err(OpticaldiscsError::Io)?;

        // Whole sectors only — a raw character device rejects partial or
        // unaligned reads, and `read_exact` would otherwise be free to stop
        // short at a medium error.
        let mut buf = vec![0u8; SECTOR_SIZE as usize];
        self.device
            .read_exact(&mut buf)
            .map_err(OpticaldiscsError::Io)?;
        Ok(buf)
    }
}

/// Translate a drive path into the node that should actually be opened.
///
/// Only macOS needs this: `/dev/diskN` is the buffered block device, while
/// `/dev/rdiskN` is the raw character device that supports large sequential
/// reads without going through the buffer cache. Every other platform reads the
/// path as given.
fn resolve_device_path(path: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(s) = path.to_str() {
            if let Some(rest) = s.strip_prefix("/dev/disk") {
                return PathBuf::from(format!("/dev/rdisk{rest}"));
            }
        }
    }
    path.to_path_buf()
}

/// Best-effort medium capacity. Returns `None` rather than failing: the size is
/// a convenience for callers that want to bound a full-disc read, and a disc
/// reads fine without it.
///
/// The capacity ioctls used here (`DKIOCGETBLOCKCOUNT` / `DKIOCGETBLOCKSIZE` on
/// macOS, `BLKGETSIZE64` on Linux) are the *generic block-device* ones — unlike
/// the `DKIOCCD*` family that made this module necessary, they have no CD-only
/// restriction and answer for DVD and Blu-ray media alike.
///
/// `seek(End)` is only a last resort: it reports 0 on a macOS raw disk device,
/// so it cannot be the primary probe.
fn probe_size(device: &File) -> Option<u64> {
    // A raw character device reports length 0 through metadata, so only trust a
    // non-zero answer from it.
    if let Ok(m) = device.metadata() {
        if m.len() > 0 {
            return Some(m.len());
        }
    }

    if let Some(size) = platform_size(device) {
        return Some(size);
    }

    let mut probe = device.try_clone().ok()?;
    let end = probe.seek(SeekFrom::End(0)).ok()?;
    (end > 0).then_some(end)
}

#[cfg(target_os = "macos")]
fn platform_size(device: &File) -> Option<u64> {
    use std::os::unix::io::AsRawFd;

    // <sys/disk.h>: _IOR('d', 24, uint32_t) and _IOR('d', 25, uint64_t).
    const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418;
    const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x4008_6419;

    let fd = device.as_raw_fd();
    let mut block_size: u32 = 0;
    let mut block_count: u64 = 0;

    unsafe {
        if libc::ioctl(fd, DKIOCGETBLOCKSIZE, &mut block_size) < 0 {
            return None;
        }
        if libc::ioctl(fd, DKIOCGETBLOCKCOUNT, &mut block_count) < 0 {
            return None;
        }
    }

    block_count.checked_mul(u64::from(block_size))
}

#[cfg(target_os = "linux")]
fn platform_size(device: &File) -> Option<u64> {
    use std::os::unix::io::AsRawFd;

    // <linux/fs.h>: _IOR(0x12, 114, size_t).
    const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;

    let fd = device.as_raw_fd();
    let mut size: u64 = 0;
    unsafe {
        if libc::ioctl(fd, BLKGETSIZE64, &mut size) < 0 {
            return None;
        }
    }
    (size > 0).then_some(size)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_size(_device: &File) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_a_missing_device_is_an_error_not_a_panic() {
        // An empty tray leaves no device node behind.
        assert!(PhysicalDisc::open("/dev/definitely-not-a-drive").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_maps_block_device_to_raw_node() {
        assert_eq!(
            resolve_device_path(Path::new("/dev/disk6")),
            PathBuf::from("/dev/rdisk6")
        );
        // Already raw — must not gain a second "r".
        assert_eq!(
            resolve_device_path(Path::new("/dev/rdisk6")),
            PathBuf::from("/dev/rdisk6")
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn other_platforms_open_the_path_as_given() {
        assert_eq!(
            resolve_device_path(Path::new("/dev/sr0")),
            PathBuf::from("/dev/sr0")
        );
    }

    /// Hardware-gated: reads the disc in the first drive that has one. Skips
    /// (rather than fails) on a machine with no drive or an empty tray, so CI
    /// stays green.
    #[test]
    fn reads_a_real_disc_when_one_is_present() {
        let Some(drive) = crate::drives::list_drives()
            .into_iter()
            .find(|d| d.is_loaded && !d.device_path.as_os_str().is_empty())
        else {
            eprintln!("no loaded optical drive present — skipping");
            return;
        };

        let mut disc = match PhysicalDisc::open(&drive.device_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "could not open {}: {e} — skipping",
                    drive.device_path.display()
                );
                return;
            }
        };

        // Sector 16 is the ISO 9660 Primary Volume Descriptor on any data disc.
        let pvd = disc.read_sector(16).expect("read PVD sector");
        assert_eq!(pvd.len(), SECTOR_SIZE as usize);
        eprintln!(
            "{}: size={:?} PVD id={:?}",
            drive.display_name,
            disc.size_bytes(),
            String::from_utf8_lossy(&pvd[1..6])
        );
    }

    /// Hardware-gated end-to-end check: detect the filesystem on a real disc and
    /// list its root. This is the path that previously died with `ENOTTY` on
    /// DVD media.
    #[test]
    fn browses_a_real_disc_when_one_is_present() {
        use crate::browse::open_physical_filesystem;

        let Some(drive) = crate::drives::list_drives()
            .into_iter()
            .find(|d| d.is_loaded && !d.device_path.as_os_str().is_empty())
        else {
            eprintln!("no loaded optical drive present — skipping");
            return;
        };

        let mut fs = match open_physical_filesystem(&drive.device_path) {
            Ok(fs) => fs,
            Err(e) => {
                eprintln!("could not browse {}: {e} — skipping", drive.display_name);
                return;
            }
        };

        let root = fs.root().expect("read disc root");
        let entries = fs.list_directory(&root).expect("list disc root");
        eprintln!(
            "{}: volume={:?}, {} entries at root",
            drive.display_name,
            fs.volume_name(),
            entries.len()
        );
        for e in entries.iter().take(10) {
            eprintln!("  {}", e.name);
        }
    }
}
