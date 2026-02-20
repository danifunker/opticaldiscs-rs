//! Disc image format and filesystem auto-detection.
//!
//! The main entry point is [`DiscImageInfo::open`], which determines both the
//! container format and the on-disc filesystem from a file path, returning a
//! populated [`DiscImageInfo`] without the caller needing to know the internals.
//!
//! ## Detection strategy
//!
//! [`detect_format`] first checks the file extension (fast path), then reads a
//! few bytes of the file for magic-byte confirmation when the extension is absent
//! or ambiguous:
//!
//! - **CHD**: first 8 bytes are `MComprHD`
//! - **ISO 9660**: bytes 32769–32773 are `CD001`
//!
//! [`detect_filesystem`] probes the disc content and returns the filesystem type:
//!
//! - Phase 2: ISO container + ISO 9660 / HFS filesystem probe
//! - Phase 3: BIN/CUE container support added
//! - Phase 4: CHD container support added
//! - Phase 6: magic-byte format detection; `toc` field (feature = `"toc"`)

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::bincue::parse_cue_tracks;
use crate::chd::open_chd;
use crate::error::{OpticaldiscsError, Result};
use crate::formats::{DiscFormat, FilesystemType};
use crate::iso9660::PrimaryVolumeDescriptor;
use crate::sector_reader::{BinCueSectorReader, ChdSectorReader, IsoSectorReader, SectorReader};

/// CHD magic bytes at offset 0.
const CHD_MAGIC: &[u8; 8] = b"MComprHD";

/// Byte offset of the ISO 9660 identifier within the PVD sector.
///
/// = sector 16 × 2048 + 1 (the `CD001` identifier starts at byte 1 of the PVD).
const ISO_MAGIC_OFFSET: u64 = 16 * 2048 + 1;

// ── Public format-detection API ───────────────────────────────────────────────

/// Detect the disc image format for `path`.
///
/// Tries the file extension first (fast path, no I/O).  If the extension is
/// absent or unrecognised the file is opened and magic bytes are checked:
///
/// - **CHD**: first 8 bytes are `MComprHD`
/// - **ISO 9660**: bytes 32769–32773 are `CD001`
///
/// # Errors
///
/// Returns [`OpticaldiscsError::UnsupportedFormat`] if neither the extension
/// nor any magic-byte signature matches, or [`OpticaldiscsError::Io`] if the
/// file cannot be opened.
pub fn detect_format(path: &Path) -> Result<DiscFormat> {
    // Fast path: extension-based detection (no file I/O required).
    if let Some(fmt) = DiscFormat::from_path(path) {
        return Ok(fmt);
    }

    // Slow path: magic bytes.
    let mut file = std::fs::File::open(path).map_err(OpticaldiscsError::Io)?;

    // CHD: 8-byte magic "MComprHD" at offset 0.
    let mut magic = [0u8; 8];
    if file.read_exact(&mut magic).is_ok() && &magic == CHD_MAGIC {
        return Ok(DiscFormat::Chd);
    }

    // ISO 9660: "CD001" at byte 32769 (offset 1 in the PVD at sector 16).
    if file.seek(SeekFrom::Start(ISO_MAGIC_OFFSET)).is_ok() {
        let mut id = [0u8; 5];
        if file.read_exact(&mut id).is_ok() && &id == b"CD001" {
            return Ok(DiscFormat::Iso);
        }
    }

    Err(OpticaldiscsError::UnsupportedFormat(format!(
        "unrecognised format: {}",
        path.display()
    )))
}

// ── Public filesystem-detection API ──────────────────────────────────────────

/// Determine the filesystem type from a disc sector reader.
///
/// Checks (in order):
/// 1. ISO 9660 PVD at sector 16
/// 2. HFS MDB signature (`0x4244` = `"BD"`) at byte 1024
/// 3. HFS+ volume header signature (`0x482B` = `"H+"` / `0x4858` = `"HX"`) at byte 1024
/// 4. Apple Partition Map DDM signature (`0x4552` = `"ER"`) at byte 0
///
/// Returns [`FilesystemType::Unknown`] if no recognisable filesystem is found.
pub fn detect_filesystem(reader: &mut dyn SectorReader) -> Result<FilesystemType> {
    probe_filesystem(reader).map(|(fs, _pvd)| fs)
}

// ── DiscImageInfo ─────────────────────────────────────────────────────────────

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
    /// Disc Table of Contents, if the format provides track metadata.
    ///
    /// Present for BIN/CUE and CHD images; `None` for plain ISO files.
    /// Requires the `toc` feature.
    #[cfg(feature = "toc")]
    pub toc: Option<crate::toc::DiscTOC>,
}

impl DiscImageInfo {
    /// Open and probe a disc image at `path`.
    ///
    /// Determines the container format using [`detect_format`] (extension +
    /// magic bytes), opens the appropriate sector reader, and probes the disc
    /// for filesystem type and volume label.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened, the format is not
    /// recognised, or the disc contains no identifiable filesystem.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let format = detect_format(path)?;

        match format {
            DiscFormat::Iso => Self::probe_iso(path),
            DiscFormat::BinCue => Self::probe_bincue(path),
            DiscFormat::Chd => Self::probe_chd(path),
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

        // Clone the data track to release the borrow on `tracks` before use below.
        let data_track = tracks
            .iter()
            .find(|t| t.is_data())
            .ok_or(OpticaldiscsError::NoDataTrack)?
            .clone();

        let mut reader = BinCueSectorReader::open(&data_track)?;
        let (filesystem, pvd) = probe_filesystem(&mut reader)?;
        let volume_label = pvd.as_ref().map(|p| p.volume_id.clone());

        #[cfg(feature = "toc")]
        let toc = build_bincue_toc(&tracks);

        Ok(Self {
            path: path.to_path_buf(),
            format: DiscFormat::BinCue,
            filesystem,
            volume_label,
            pvd,
            #[cfg(feature = "toc")]
            toc,
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
            // Plain ISO files carry no track metadata.
            #[cfg(feature = "toc")]
            toc: None,
        })
    }

    /// Probe a `.chd` file.
    ///
    /// Parses CHT2 track metadata, locates the first data track, and probes
    /// the filesystem through a [`ChdSectorReader`].  Audio-only discs (no
    /// data track) are returned with `FilesystemType::Unknown`.
    fn probe_chd(path: &Path) -> Result<Self> {
        let chd_info = open_chd(path)?;

        #[cfg(feature = "toc")]
        let toc = build_chd_toc(&chd_info.tracks);

        let data_track = match chd_info.find_first_data_track() {
            Some(track) => track.clone(),
            None => {
                // Audio-only disc — valid CHD, but no filesystem to probe
                return Ok(Self {
                    path: path.to_path_buf(),
                    format: DiscFormat::Chd,
                    filesystem: FilesystemType::Unknown,
                    volume_label: None,
                    pvd: None,
                    #[cfg(feature = "toc")]
                    toc,
                });
            }
        };

        let mut reader = ChdSectorReader::open(path, &data_track)?;
        let (filesystem, pvd) = probe_filesystem(&mut reader)?;
        let volume_label = pvd.as_ref().map(|p| p.volume_id.clone());

        Ok(Self {
            path: path.to_path_buf(),
            format: DiscFormat::Chd,
            filesystem,
            volume_label,
            pvd,
            #[cfg(feature = "toc")]
            toc,
        })
    }
}

// ── TOC helpers (feature = "toc") ─────────────────────────────────────────────

/// Build a [`DiscTOC`] from a BIN/CUE track list.
///
/// Recovers per-track frame offsets from the stored `file_byte_offset`, and
/// computes the lead-out from the last track's BIN file size.  Returns `None`
/// if the track list is empty or the BIN file size cannot be determined.
#[cfg(feature = "toc")]
fn build_bincue_toc(tracks: &[crate::bincue::BinTrack]) -> Option<crate::toc::DiscTOC> {
    use crate::toc::{DiscTOC, TrackInfo};

    if tracks.is_empty() {
        return None;
    }

    let track_infos: Vec<TrackInfo> = tracks
        .iter()
        .map(|t| {
            let offset_frames = (t.file_byte_offset / t.sector_size()) as u32;
            TrackInfo {
                number: t.track_no as u8,
                offset: offset_frames,
                track_type: t.track_type.cue_label().to_string(),
            }
        })
        .collect();

    // Lead-out: last track start + frames in that track.
    // For the last track in a single-BIN or per-track BIN, compute frames from
    // the remaining bytes in the BIN file (handles both layouts).
    let last = tracks.last()?;
    let file_len = std::fs::metadata(&last.bin_path).ok()?.len();
    let last_start_frames = last.file_byte_offset / last.sector_size();
    let last_frames = file_len.saturating_sub(last.file_byte_offset) / last.sector_size();
    let lead_out_raw = (last_start_frames + last_frames) as u32;

    DiscTOC::from_tracks(&track_infos, lead_out_raw)
}

/// Build a [`DiscTOC`] from CHD track metadata.
///
/// Uses each track's `frame_offset` directly (already a raw frame index from
/// the start of the disc data area).  The lead-out is the last track's offset
/// plus its frame count.  Returns `None` if the track list is empty.
#[cfg(feature = "toc")]
fn build_chd_toc(tracks: &[crate::chd::ChdTrack]) -> Option<crate::toc::DiscTOC> {
    use crate::toc::{DiscTOC, TrackInfo};

    if tracks.is_empty() {
        return None;
    }

    let track_infos: Vec<TrackInfo> = tracks
        .iter()
        .map(|t| TrackInfo {
            number: t.track_no as u8,
            offset: t.frame_offset as u32,
            track_type: format!("{:?}", t.track_type),
        })
        .collect();

    let last = tracks.last()?;
    let lead_out_raw = last.frame_offset as u32 + last.frames;

    DiscTOC::from_tracks(&track_infos, lead_out_raw)
}

// ── Internal filesystem probe ─────────────────────────────────────────────────

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

    // ── Try Apple Partition Map (DDM signature "ER" = 0x4552) ───────────────
    // Parse the partition map to find the HFS partition byte offset, then
    // check the HFS MDB / HFS+ VH signature at partition_offset + 1024.
    if let Ok(entries) = crate::apm::parse_partition_map(reader) {
        if let Some(partition) = entries.iter().find(|e| e.is_hfs()) {
            let offset = partition.start_block as u64 * 512;
            if let Ok(sig_bytes) = reader.read_bytes(offset + 1024, 2) {
                if sig_bytes.len() == 2 {
                    match u16::from_be_bytes([sig_bytes[0], sig_bytes[1]]) {
                        0x4244 => return Ok((FilesystemType::Hfs, None)),
                        0x482B | 0x4858 => return Ok((FilesystemType::HfsPlus, None)),
                        _ => {}
                    }
                }
            }
        }
    }

    Ok((FilesystemType::Unknown, None))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

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

    // ── detect_format tests ────────────────────────────────────────────────

    #[test]
    fn detect_format_by_extension() {
        // These paths don't exist on disk — extension detection requires no I/O.
        assert_eq!(
            detect_format(Path::new("disc.iso")).unwrap(),
            DiscFormat::Iso
        );
        assert_eq!(
            detect_format(Path::new("disc.cue")).unwrap(),
            DiscFormat::BinCue
        );
        assert_eq!(
            detect_format(Path::new("disc.chd")).unwrap(),
            DiscFormat::Chd
        );
    }

    #[test]
    fn detect_format_no_extension_unknown() {
        // File doesn't exist, so Io error is expected.
        let err = detect_format(Path::new("disc_no_ext")).unwrap_err();
        // Either UnsupportedFormat (no ext) or Io (file not found) is acceptable.
        assert!(matches!(
            err,
            OpticaldiscsError::UnsupportedFormat(_) | OpticaldiscsError::Io(_)
        ));
    }

    #[test]
    fn detect_format_magic_bytes_iso() {
        use std::io::Write;

        let mut f = tempfile::Builder::new()
            .suffix(".img") // non-standard extension
            .tempfile()
            .unwrap();

        // Write 17 sectors + 5 bytes of ISO 9660 magic at offset 32769.
        let size = 17 * 2048 + 6;
        let mut buf = vec![0u8; size];
        // "CD001" at offset 32769
        buf[32769..32774].copy_from_slice(b"CD001");
        f.write_all(&buf).unwrap();
        f.flush().unwrap();

        let fmt = detect_format(f.path()).unwrap();
        assert_eq!(fmt, DiscFormat::Iso);
    }

    #[test]
    fn detect_format_magic_bytes_chd() {
        use std::io::Write;

        let mut f = tempfile::Builder::new()
            .suffix(".img") // non-standard extension
            .tempfile()
            .unwrap();

        // CHD magic "MComprHD" at offset 0
        let mut buf = vec![0u8; 256];
        buf[..8].copy_from_slice(b"MComprHD");
        f.write_all(&buf).unwrap();
        f.flush().unwrap();

        let fmt = detect_format(f.path()).unwrap();
        assert_eq!(fmt, DiscFormat::Chd);
    }

    // ── detect_filesystem tests ────────────────────────────────────────────

    #[test]
    fn detect_filesystem_iso9660() {
        let img = iso_image_with_label("FS_TEST");
        let mut reader = CursorReader(Cursor::new(img));
        assert_eq!(
            detect_filesystem(&mut reader).unwrap(),
            FilesystemType::Iso9660
        );
    }

    #[test]
    fn detect_filesystem_unknown() {
        let img = vec![0u8; 17 * SECTOR_SIZE as usize];
        let mut reader = CursorReader(Cursor::new(img));
        assert_eq!(
            detect_filesystem(&mut reader).unwrap(),
            FilesystemType::Unknown
        );
    }
}
