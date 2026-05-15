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

use crate::apm::find_hfs_partition_offset;
use crate::bincue::parse_cue_tracks;
use crate::chd::open_chd;
use crate::error::{OpticaldiscsError, Result};
use crate::formats::{DiscFormat, FilesystemType};
use crate::hfs::MasterDirectoryBlock;
use crate::hfsplus::{extract_volume_name_from_catalog, HfsPlusVolumeHeader};
use crate::iso9660::PrimaryVolumeDescriptor;
use crate::sector_reader::{BinCueSectorReader, ChdSectorReader, IsoSectorReader, SectorReader};
use crate::sgi::{SgiVolumeHeader, SGI_VOLHDR_MAGIC};

/// EFS superblock magic values (old / new). Located at sb+28 due to natural-
/// alignment padding in the C `struct efs_super` (see docs/EFS_Implementation.md).
const EFS_MAGIC_OLD: u32 = 0x0007_2959;
const EFS_MAGIC_NEW: u32 = 0x0007_295A;

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
    /// HFS Master Directory Block, if the disc is an HFS volume.
    pub hfs_mdb: Option<MasterDirectoryBlock>,
    /// HFS+ Volume Header, if the disc is an HFS+ volume.
    pub hfsplus_header: Option<HfsPlusVolumeHeader>,
    /// SGI Volume Header, if present (IRIX install/distribution CDs).
    pub sgi_header: Option<SgiVolumeHeader>,
    /// Byte offset of the EFS partition within the disc, if the filesystem
    /// is EFS. Used by browsers to locate the superblock and inodes.
    pub efs_partition_offset: Option<u64>,
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
    fn build(
        path: &Path,
        format: DiscFormat,
        reader: &mut dyn SectorReader,
        #[cfg(feature = "toc")] toc: Option<crate::toc::DiscTOC>,
    ) -> Result<Self> {
        let (mut filesystem, pvd) = probe_filesystem(reader)?;
        let (hfs_mdb, hfsplus_header, hfs_volume_label) = probe_hfs_detail(reader, filesystem);
        let (sgi_header, efs_partition_offset, sgi_fs) = probe_sgi_detail(reader);
        if filesystem == FilesystemType::Unknown {
            if let Some(fs) = sgi_fs {
                filesystem = fs;
            }
        }
        let volume_label = pvd
            .as_ref()
            .map(|p| p.volume_id.clone())
            .or(hfs_volume_label);

        Ok(Self {
            path: path.to_path_buf(),
            format,
            filesystem,
            volume_label,
            pvd,
            hfs_mdb,
            hfsplus_header,
            sgi_header,
            efs_partition_offset,
            #[cfg(feature = "toc")]
            toc,
        })
    }

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

        #[cfg(feature = "toc")]
        let toc = build_bincue_toc(&tracks);

        Self::build(
            path,
            DiscFormat::BinCue,
            &mut reader,
            #[cfg(feature = "toc")]
            toc,
        )
    }

    /// Probe a plain `.iso` file.
    fn probe_iso(path: &Path) -> Result<Self> {
        let mut reader = IsoSectorReader::new(path)?;
        Self::build(
            path,
            DiscFormat::Iso,
            &mut reader,
            #[cfg(feature = "toc")]
            None,
        )
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
                    hfs_mdb: None,
                    hfsplus_header: None,
                    sgi_header: None,
                    efs_partition_offset: None,
                    #[cfg(feature = "toc")]
                    toc,
                });
            }
        };

        let mut reader = ChdSectorReader::open(path, &data_track)?;
        Self::build(
            path,
            DiscFormat::Chd,
            &mut reader,
            #[cfg(feature = "toc")]
            toc,
        )
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

/// Resolve the actual filesystem variant at a given offset.
///
/// Given a byte offset (either 0 for non-APM images or the APM partition
/// offset), reads the signature at `offset + 1024` and determines the real
/// filesystem type and the byte offset to use for reading its header.
///
/// Three cases are handled:
/// - **Native HFS+** (`0x482B`/`0x4858` at offset+1024): returns
///   `(HfsPlus, offset)`.
/// - **Embedded HFS+** (HFS MDB `0x4244` wrapper with `drEmbedSigWord ==
///   0x482B`): computes the embedded volume's start from the MDB's
///   `drAlBlSt`, `drAlBlkSiz`, and `drEmbedExtent.startBlock` fields.
/// - **Pure HFS** (`0x4244`, no embedded HFS+): returns `(Hfs, offset)`.
pub(crate) fn resolve_apple_hfs(
    reader: &mut dyn SectorReader,
    partition_offset: u64,
) -> (FilesystemType, u64) {
    // Read 162 bytes: enough for the MDB signature, drAlBlkSiz (20),
    // drAlBlSt (28), drEmbedSigWord (124), and drEmbedExtent (126–127).
    let buf = match reader.read_bytes(partition_offset + 1024, 162) {
        Ok(b) => b,
        Err(_) => return (FilesystemType::Unknown, partition_offset),
    };
    let sig = u16::from_be_bytes([buf[0], buf[1]]);
    match sig {
        0x4244 => {
            // HFS MDB — check for embedded HFS+ (drEmbedSigWord at MDB offset 124)
            let embedded_sig = u16::from_be_bytes([buf[124], buf[125]]);
            if embedded_sig == 0x482B {
                // drAlBlkSiz  (allocation block size, bytes) at MDB offset 20 (u32 BE)
                // drAlBlSt    (first alloc block in 512-byte sectors) at MDB offset 28 (u16 BE)
                // drEmbedExtent.startBlock at MDB offset 126 (u16 BE)
                let block_size = u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]) as u64;
                let first_alloc_block = u16::from_be_bytes([buf[28], buf[29]]) as u64;
                let embedded_start = u16::from_be_bytes([buf[126], buf[127]]) as u64;
                let hfsplus_offset =
                    partition_offset + first_alloc_block * 512 + embedded_start * block_size;
                (FilesystemType::HfsPlus, hfsplus_offset)
            } else {
                (FilesystemType::Hfs, partition_offset)
            }
        }
        // Native HFS+ or HFSX — volume header is directly at partition_offset + 1024
        0x482B | 0x4858 => (FilesystemType::HfsPlus, partition_offset),
        _ => (FilesystemType::Unknown, partition_offset),
    }
}

/// Parse HFS or HFS+ metadata structs and extract the volume label.
///
/// Called after `probe_filesystem` to fully read the MDB / volume header when
/// an HFS or HFS+ filesystem has been detected.  Returns
/// `(hfs_mdb, hfsplus_header, volume_label)`.
fn probe_hfs_detail(
    reader: &mut dyn SectorReader,
    filesystem: FilesystemType,
) -> (
    Option<MasterDirectoryBlock>,
    Option<HfsPlusVolumeHeader>,
    Option<String>,
) {
    // Use resolve_apple_hfs to get the correct offset — this handles both
    // native HFS+, embedded HFS+ (wrapper MDB), and pure HFS.
    let raw_offset = find_hfs_partition_offset(reader).unwrap_or(0);
    let (_resolved_fs, resolved_offset) = resolve_apple_hfs(reader, raw_offset);

    match filesystem {
        FilesystemType::Hfs => match MasterDirectoryBlock::read_from(reader, raw_offset) {
            Ok(mdb) => {
                let label = mdb.volume_name.clone();
                (Some(mdb), None, Some(label))
            }
            Err(_) => (None, None, None),
        },
        FilesystemType::HfsPlus => match HfsPlusVolumeHeader::read_from(reader, resolved_offset) {
            Ok(vh) => {
                let label = extract_volume_name_from_catalog(reader, resolved_offset)
                    .ok()
                    .flatten();
                (None, Some(vh), label)
            }
            Err(_) => (None, None, None),
        },
        _ => (None, None, None),
    }
}

/// Probe for an SGI Volume Header and, if present, find the byte offset of
/// an EFS partition within the disc.
///
/// Returns `(sgi_header, efs_partition_offset, fs_type)`. The IRIX install
/// CDs label their data partition with type byte 5 (SYSV) rather than 7
/// (EFS), so we probe any non-wrapper partition for the EFS superblock magic
/// at `first*512 + 512` (sb+28 = `0x00072959` / `0x0007295A`) and accept the
/// first one that matches.
fn probe_sgi_detail(
    reader: &mut dyn SectorReader,
) -> (Option<SgiVolumeHeader>, Option<u64>, Option<FilesystemType>) {
    // Cheap magic check before parsing.
    let magic_bytes = match reader.read_bytes(0, 4) {
        Ok(b) => b,
        Err(_) => return (None, None, None),
    };
    let magic = u32::from_be_bytes([
        magic_bytes[0],
        magic_bytes[1],
        magic_bytes[2],
        magic_bytes[3],
    ]);
    if magic != SGI_VOLHDR_MAGIC {
        return (None, None, None);
    }

    let header = match SgiVolumeHeader::read_from(reader) {
        Ok(h) => h,
        Err(_) => return (None, None, None),
    };

    for entry in &header.partitions {
        if entry.is_empty() {
            continue;
        }
        let pt = entry.partition_type();
        if pt.is_disk_wide_wrapper() {
            continue;
        }
        // Probe partitions whose type is plausibly a filesystem. We probe
        // EFS (7) and SYSV (5, used by IRIX install CDs) explicitly; any
        // other type with a valid EFS superblock will also be accepted.
        let byte_off = entry.start_offset();
        let sb_off = byte_off + 512;
        let sb = match reader.read_bytes(sb_off + 28, 4) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let sb_magic = u32::from_be_bytes([sb[0], sb[1], sb[2], sb[3]]);
        if sb_magic == EFS_MAGIC_OLD || sb_magic == EFS_MAGIC_NEW {
            return (Some(header), Some(byte_off), Some(FilesystemType::Efs));
        }
    }

    (Some(header), None, None)
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
    // Use resolve_apple_hfs to handle embedded HFS+ (HFS wrapper) correctly.
    if let Ok(sig_bytes) = reader.read_bytes(1024, 2) {
        let sig = u16::from_be_bytes([sig_bytes[0], sig_bytes[1]]);
        if sig == 0x4244 || sig == 0x482B || sig == 0x4858 {
            let (fs, _offset) = resolve_apple_hfs(reader, 0);
            if fs != FilesystemType::Unknown {
                return Ok((fs, None));
            }
        }
    }

    // ── Try Apple Partition Map (DDM signature "ER" = 0x4552) ───────────────
    // Parse the partition map to find the HFS partition byte offset, then
    // use resolve_apple_hfs to correctly identify native vs embedded HFS+.
    if let Ok(entries) = crate::apm::parse_partition_map(reader) {
        if let Some(partition) = entries.iter().find(|e| e.is_hfs()) {
            let offset = partition.start_block as u64 * 512;
            let (fs, _resolved_offset) = resolve_apple_hfs(reader, offset);
            if fs != FilesystemType::Unknown {
                return Ok((fs, None));
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

    // ── resolve_apple_hfs tests ───────────────────────────────────────────

    #[test]
    fn resolve_native_hfsplus() {
        // HFS+ signature "H+" at byte 1024
        let mut img = vec![0u8; 4096];
        img[1024] = 0x48; // 'H'
        img[1025] = 0x2B; // '+'
        let mut reader = CursorReader(Cursor::new(img));
        let (fs, offset) = resolve_apple_hfs(&mut reader, 0);
        assert_eq!(fs, FilesystemType::HfsPlus);
        assert_eq!(offset, 0);
    }

    #[test]
    fn resolve_native_hfsx() {
        // HFSX signature "HX" at byte 1024
        let mut img = vec![0u8; 4096];
        img[1024] = 0x48; // 'H'
        img[1025] = 0x58; // 'X'
        let mut reader = CursorReader(Cursor::new(img));
        let (fs, offset) = resolve_apple_hfs(&mut reader, 0);
        assert_eq!(fs, FilesystemType::HfsPlus);
        assert_eq!(offset, 0);
    }

    #[test]
    fn resolve_pure_hfs() {
        // HFS MDB signature "BD" at byte 1024, no embedded HFS+
        let mut img = vec![0u8; 4096];
        img[1024] = 0x42; // 'B'
        img[1025] = 0x44; // 'D'
        let mut reader = CursorReader(Cursor::new(img));
        let (fs, offset) = resolve_apple_hfs(&mut reader, 0);
        assert_eq!(fs, FilesystemType::Hfs);
        assert_eq!(offset, 0);
    }

    #[test]
    fn resolve_embedded_hfsplus() {
        // HFS MDB wrapper with embedded HFS+ volume
        let mut img = vec![0u8; 1024 * 1024]; // 1 MB
                                              // MDB signature at byte 1024
        img[1024] = 0x42; // 'B'
        img[1025] = 0x44; // 'D'
                          // drAlBlkSiz at MDB offset 20 (byte 1044): 4096 bytes
        img[1044..1048].copy_from_slice(&4096u32.to_be_bytes());
        // drAlBlSt at MDB offset 28 (byte 1052): sector 4 (= 2048 bytes)
        img[1052..1054].copy_from_slice(&4u16.to_be_bytes());
        // drEmbedSigWord at MDB offset 124 (byte 1148): "H+"
        img[1148] = 0x48;
        img[1149] = 0x2B;
        // drEmbedExtent.startBlock at MDB offset 126 (byte 1150): block 2
        img[1150..1152].copy_from_slice(&2u16.to_be_bytes());

        let mut reader = CursorReader(Cursor::new(img));
        let (fs, offset) = resolve_apple_hfs(&mut reader, 0);
        assert_eq!(fs, FilesystemType::HfsPlus);
        // Expected: 0 + 4*512 + 2*4096 = 2048 + 8192 = 10240
        assert_eq!(offset, 10240);
    }

    #[test]
    fn resolve_embedded_hfsplus_with_partition_offset() {
        // Same as above but with a non-zero APM partition offset
        let partition_offset: u64 = 32768; // 64 * 512
        let mut img = vec![0u8; 1024 * 1024];
        let base = partition_offset as usize + 1024;
        img[base] = 0x42; // 'B'
        img[base + 1] = 0x44; // 'D'
        img[base + 20..base + 24].copy_from_slice(&4096u32.to_be_bytes());
        img[base + 28..base + 30].copy_from_slice(&4u16.to_be_bytes());
        img[base + 124] = 0x48;
        img[base + 125] = 0x2B;
        img[base + 126..base + 128].copy_from_slice(&2u16.to_be_bytes());

        let mut reader = CursorReader(Cursor::new(img));
        let (fs, offset) = resolve_apple_hfs(&mut reader, partition_offset);
        assert_eq!(fs, FilesystemType::HfsPlus);
        // Expected: 32768 + 4*512 + 2*4096 = 32768 + 2048 + 8192 = 43008
        assert_eq!(offset, 43008);
    }

    #[test]
    fn resolve_unknown_signature() {
        let img = vec![0u8; 4096];
        let mut reader = CursorReader(Cursor::new(img));
        let (fs, offset) = resolve_apple_hfs(&mut reader, 0);
        assert_eq!(fs, FilesystemType::Unknown);
        assert_eq!(offset, 0);
    }

    #[test]
    fn probe_sgi_detail_finds_efs_partition() {
        use crate::sgi::tests::build_test_volhdr;
        use crate::sgi::SgiPartitionType;

        // Build a small disc image: SGI volhdr in sector 0, EFS superblock
        // at partition first*512 + 512. Partition uses SYSV type byte 5 to
        // mirror what the IRIX install CDs do.
        const FIRST: u32 = 32;
        let mut parts = vec![(0u32, 0u32, 0u32); 16];
        parts[7] = (1000, FIRST, SgiPartitionType::SysV.as_u32());
        let vh = build_test_volhdr(&parts);

        // Allocate enough sectors to cover partition start + a few sb sectors.
        let total_sectors = (FIRST as u64) + 8;
        let mut img = vec![0u8; (total_sectors * SECTOR_SIZE) as usize];
        img[..vh.len()].copy_from_slice(&vh);

        // Write EFS magic at byte (FIRST*512 + 512 + 28).
        let sb_magic_off = (FIRST as usize) * 512 + 512 + 28;
        img[sb_magic_off..sb_magic_off + 4].copy_from_slice(&0x0007_2959u32.to_be_bytes());

        let mut reader = CursorReader(Cursor::new(img));
        let (hdr, off, fs) = probe_sgi_detail(&mut reader);
        assert!(hdr.is_some());
        assert_eq!(fs, Some(FilesystemType::Efs));
        assert_eq!(off, Some((FIRST as u64) * 512));
    }

    #[test]
    fn probe_sgi_detail_returns_none_without_sgi_magic() {
        let img = vec![0u8; 4 * SECTOR_SIZE as usize];
        let mut reader = CursorReader(Cursor::new(img));
        let (hdr, off, fs) = probe_sgi_detail(&mut reader);
        assert!(hdr.is_none());
        assert!(off.is_none());
        assert!(fs.is_none());
    }

    #[test]
    fn probe_detects_embedded_hfsplus_without_apm() {
        // Image with HFS MDB at byte 1024 wrapping an embedded HFS+ volume.
        // probe_filesystem should detect this as HfsPlus, not Hfs.
        let mut img = vec![0u8; 17 * SECTOR_SIZE as usize];
        img[1024] = 0x42; // 'B'
        img[1025] = 0x44; // 'D'
                          // drAlBlkSiz = 2048
        img[1044..1048].copy_from_slice(&2048u32.to_be_bytes());
        // drAlBlSt = 0
        img[1052..1054].copy_from_slice(&0u16.to_be_bytes());
        // drEmbedSigWord = 0x482B
        img[1148] = 0x48;
        img[1149] = 0x2B;
        // drEmbedExtent.startBlock = 0
        img[1150..1152].copy_from_slice(&0u16.to_be_bytes());

        let mut reader = CursorReader(Cursor::new(img));
        let (fs, pvd) = probe_filesystem(&mut reader).unwrap();
        assert_eq!(fs, FilesystemType::HfsPlus);
        assert!(pvd.is_none());
    }
}
