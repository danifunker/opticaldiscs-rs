//! Disc image format and filesystem auto-detection.
//!
//! The main entry point is [`DiscImageInfo::open`], which determines both the
//! container format and the on-disc filesystem from a file path, returning a
//! populated [`DiscImageInfo`] without the caller needing to know the internals.
//!
//! Detection is implemented progressively per PLAN.md:
//! - Phase 2: ISO container + ISO 9660 / HFS filesystem probe
//! - Phase 3: BIN/CUE container support added
//! - Phase 4: CHD container support added

use std::path::{Path, PathBuf};

use crate::bincue::parse_cue_tracks;
use crate::error::{OpticaldiscsError, Result};
use crate::formats::{DiscFormat, FilesystemType};
use crate::iso9660::PrimaryVolumeDescriptor;
use crate::sector_reader::{BinCueSectorReader, IsoSectorReader, SectorReader, SECTOR_SIZE};

/// All available information about a disc image, obtained without full parsing.
///
/// Use [`DiscImageInfo::open`] to create one from a file path.
#[derive(Debug)]
pub struct DiscImageInfo {
    /// Path to the disc image file (or to the `.cue` file for BIN/CUE).
    pub path: PathBuf,
    /// Detected container format.
    pub format: DiscFormat,
    /// Detected on-disc filesystem.
    pub filesystem: FilesystemType,
    /// Volume label extracted from the filesystem, if available.
    pub volume_label: Option<String>,
    /// Parsed ISO 9660 Primary Volume Descriptor, if present.
    pub pvd: Option<PrimaryVolumeDescriptor>,
    // Phase 5: pub toc: Option<crate::toc::DiscTOC>,
}

impl DiscImageInfo {
    /// Open and probe a disc image at `path`.
    ///
    /// Determines the container format from the file extension, opens the
    /// appropriate sector reader, and probes the disc for filesystem type and
    /// volume label.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened, the format is not
    /// recognised, or the disc contains no identifiable filesystem.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        let format = DiscFormat::from_path(path).ok_or_else(|| {
            OpticaldiscsError::UnsupportedFormat(format!(
                "unrecognised extension: {}",
                path.display()
            ))
        })?;

        match format {
            DiscFormat::Iso => Self::probe_iso(path),
            DiscFormat::BinCue => Self::probe_bincue(path),
            DiscFormat::Chd => Err(OpticaldiscsError::UnsupportedFormat(
                "CHD support coming in Phase 4".into(),
            )),
            DiscFormat::MdsMdf => Err(OpticaldiscsError::UnsupportedFormat(
                "MDS/MDF is not supported".into(),
            )),
        }
    }

    /// Probe a `.cue` (or `.bin`) BIN/CUE image.
    ///
    /// When `path` points to a `.bin`, looks for a matching `.cue` in the same
    /// directory.  The CUE is parsed, the first data track is located, and the
    /// filesystem is probed through `BinCueSectorReader`.
    fn probe_bincue(path: &Path) -> Result<Self> {
        // Resolve the CUE path: accept either .cue or .bin as the entry point.
        let cue_path = if path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            == Some("bin".into())
        {
            // Try <stem>.cue next to the BIN
            let stem = path.file_stem().unwrap_or_default();
            let cue = path.with_file_name(format!("{}.cue", stem.to_string_lossy()));
            if cue.exists() {
                cue
            } else {
                return Err(OpticaldiscsError::NotFound(format!(
                    "no matching .cue found for {}",
                    path.display()
                )));
            }
        } else {
            path.to_path_buf()
        };

        let tracks = parse_cue_tracks(&cue_path)?;
        let data_track = tracks
            .into_iter()
            .find(|t| t.is_data())
            .ok_or(OpticaldiscsError::NoDataTrack)?;

        let mut reader = BinCueSectorReader::open(&data_track)?;
        let (filesystem, pvd) = probe_filesystem(&mut reader)?;
        let volume_label = pvd.as_ref().map(|p| p.volume_id.clone());

        Ok(Self {
            path: path.to_path_buf(),
            format: DiscFormat::BinCue,
            filesystem,
            volume_label,
            pvd,
        })
    }

    /// Probe a plain `.iso` file.
    fn probe_iso(path: &Path) -> Result<Self> {
        let mut reader = IsoSectorReader::new(path)?;
        let (filesystem, pvd) = probe_filesystem(&mut reader)?;
        let volume_label = pvd.as_ref().map(|p| p.volume_id.clone());

        Ok(Self {
            path: path.to_path_buf(),
            format: DiscFormat::Iso,
            filesystem,
            volume_label,
            pvd,
        })
    }
}

/// Probe the filesystem type and extract a PVD if present.
///
/// Checks (in order):
/// 1. ISO 9660 PVD at sector 16
/// 2. HFS/HFS+ signature at byte offset 1024 (sector 0, offset 1024)
pub(crate) fn probe_filesystem(
    reader: &mut dyn SectorReader,
) -> Result<(FilesystemType, Option<PrimaryVolumeDescriptor>)> {
    // ── Try ISO 9660 ────────────────────────────────────────────────────────
    if let Ok(pvd) = PrimaryVolumeDescriptor::read_from(reader) {
        return Ok((FilesystemType::Iso9660, Some(pvd)));
    }

    // ── Try HFS / HFS+ signature at byte 1024 ───────────────────────────────
    // HFS MDB and HFS+ Volume Header both sit at byte 1024.
    // We read 2 bytes and check the signature without fully parsing the header.
    if let Ok(sig_bytes) = reader.read_bytes(1024, 2) {
        let sig = u16::from_be_bytes([sig_bytes[0], sig_bytes[1]]);
        match sig {
            0x4244 => return Ok((FilesystemType::Hfs, None)), // "BD"
            0x482B | 0x4858 => return Ok((FilesystemType::HfsPlus, None)), // "H+" / "HX"
            _ => {}
        }
    }

    // ── Check for Apple Partition Map (DDM signature "ER" = 0x4552) ─────────
    // An APM disc may have HFS inside; flag it as HFS for now.
    // Full APM parsing is done in Phase 8.
    if let Ok(ddm) = reader.read_bytes(0, 2) {
        if u16::from_be_bytes([ddm[0], ddm[1]]) == 0x4552 {
            // Peek at byte 1024 through sector math
            let sector0 = reader.read_sector(0)?;
            if sector0.len() >= SECTOR_SIZE as usize {
                // HFS MDB / HFS+ VH are at physical byte 1024
                let hfs_bytes = reader.read_bytes(1024, 2).unwrap_or_default();
                if hfs_bytes.len() == 2 {
                    let s = u16::from_be_bytes([hfs_bytes[0], hfs_bytes[1]]);
                    if s == 0x4244 {
                        return Ok((FilesystemType::Hfs, None));
                    } else if s == 0x482B || s == 0x4858 {
                        return Ok((FilesystemType::HfsPlus, None));
                    }
                }
            }
        }
    }

    Ok((FilesystemType::Unknown, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iso9660::{build_test_pvd_sector, PVD_SECTOR};
    use crate::sector_reader::SECTOR_SIZE;
    use std::io::{Cursor, Read, Seek, SeekFrom};

    struct CursorReader(Cursor<Vec<u8>>);
    impl SectorReader for CursorReader {
        fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
            self.0
                .seek(SeekFrom::Start(lba * SECTOR_SIZE))
                .map_err(OpticaldiscsError::Io)?;
            let mut buf = vec![0u8; SECTOR_SIZE as usize];
            self.0.read_exact(&mut buf).map_err(OpticaldiscsError::Io)?;
            Ok(buf)
        }
    }

    fn iso_image_with_label(label: &str) -> Vec<u8> {
        let sectors = PVD_SECTOR as usize + 2;
        let mut img = vec![0u8; sectors * SECTOR_SIZE as usize];
        let pvd = build_test_pvd_sector(label, 18, 2048);
        let off = PVD_SECTOR as usize * SECTOR_SIZE as usize;
        img[off..off + 2048].copy_from_slice(&pvd);
        img
    }

    #[test]
    fn detects_iso9660() {
        let img = iso_image_with_label("PROBE_TEST");
        let mut reader = CursorReader(Cursor::new(img));
        let (fs, pvd) = probe_filesystem(&mut reader).unwrap();
        assert_eq!(fs, FilesystemType::Iso9660);
        assert_eq!(pvd.unwrap().volume_id, "PROBE_TEST");
    }

    #[test]
    fn detects_hfs_signature() {
        // Minimal image: empty sector 0..sector 16, HFS MDB signature at byte 1024
        let mut img = vec![0u8; 17 * SECTOR_SIZE as usize];
        img[1024] = 0x42; // 'B'
        img[1025] = 0x44; // 'D'
        let mut reader = CursorReader(Cursor::new(img));
        let (fs, pvd) = probe_filesystem(&mut reader).unwrap();
        assert_eq!(fs, FilesystemType::Hfs);
        assert!(pvd.is_none());
    }

    #[test]
    fn detects_hfsplus_signature() {
        let mut img = vec![0u8; 17 * SECTOR_SIZE as usize];
        img[1024] = 0x48; // 'H'
        img[1025] = 0x2B; // '+'
        let mut reader = CursorReader(Cursor::new(img));
        let (fs, _) = probe_filesystem(&mut reader).unwrap();
        assert_eq!(fs, FilesystemType::HfsPlus);
    }

    #[test]
    fn unknown_for_empty_image() {
        let img = vec![0u8; 17 * SECTOR_SIZE as usize];
        let mut reader = CursorReader(Cursor::new(img));
        let (fs, pvd) = probe_filesystem(&mut reader).unwrap();
        assert_eq!(fs, FilesystemType::Unknown);
        assert!(pvd.is_none());
    }
}
