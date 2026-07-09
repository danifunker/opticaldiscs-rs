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
use crate::efs::{EfsSuperblock, EFS_BLOCKSIZE, EFS_MAGIC_NEW, EFS_MAGIC_OLD};
use crate::error::{OpticaldiscsError, Result};
use crate::formats::{DiscFormat, FilesystemType};
use crate::hfs::MasterDirectoryBlock;
use crate::hfsplus::{extract_volume_name_from_catalog, HfsPlusVolumeHeader};
use crate::iso9660::PrimaryVolumeDescriptor;
use crate::sector_reader::{BinCueSectorReader, ChdSectorReader, IsoSectorReader, SectorReader};
use crate::sgi::{SgiVolumeHeader, SGI_VOLHDR_MAGIC};

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
    /// Video-game console identity, if the disc is a recognized game disc.
    ///
    /// Populated by [`crate::gameid::detect_game_disc`]; `None` for
    /// non-game discs (or game consoles this crate does not yet recognize).
    pub game: Option<crate::gameid::GameDiscInfo>,
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
            // A raw GameCube/Wii dump often carries a plain `.iso` extension, so
            // try the Nintendo path first and fall back to ISO 9660.
            DiscFormat::Iso => match Self::probe_nintendo(path) {
                Some(info) => Ok(info),
                None => Self::probe_iso(path),
            },
            DiscFormat::BinCue => Self::probe_bincue(path),
            DiscFormat::Chd => Self::probe_chd(path),
            DiscFormat::Nintendo => Self::probe_nintendo(path).ok_or_else(|| {
                OpticaldiscsError::UnsupportedFormat(format!(
                    "not a recognisable GameCube/Wii image: {}",
                    path.display()
                ))
            }),
            DiscFormat::Gdi => Self::probe_gdi(path),
            DiscFormat::Cso => Self::probe_cso(path),
            DiscFormat::Gz => Self::probe_gz(path),
            DiscFormat::CloneCd => Self::probe_ccd(path),
            DiscFormat::MdsMdf => Self::probe_mds(path),
        }
    }

    /// Probe a GameCube or Wii disc image via `nod`. Returns `None` when `path`
    /// is not a recognizable Nintendo disc (e.g. a plain data ISO).
    fn probe_nintendo(path: &Path) -> Option<Self> {
        let (filesystem, game) = crate::browse::nod_fs::probe_nintendo(path)?;
        Some(Self {
            path: path.to_path_buf(),
            format: DiscFormat::Nintendo,
            filesystem,
            volume_label: game.title.clone(),
            pvd: None,
            hfs_mdb: None,
            hfsplus_header: None,
            sgi_header: None,
            efs_partition_offset: None,
            game: Some(game),
            #[cfg(feature = "toc")]
            toc: None,
        })
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
        // gzip is a sequential-access container: prefer the ISO 9660 bridge tree
        // so browsing never reads the UDF anchor at the end of the disc, and
        // avoid deep file-content reads during game identification (a PS2 boot
        // file can sit near the end of the image). Both would otherwise force a
        // full decompression.
        let prefer_iso = matches!(format, DiscFormat::Gz);
        let (mut filesystem, pvd) = probe_filesystem_opts(reader, prefer_iso)?;
        let (hfs_mdb, hfsplus_header, hfs_volume_label) = probe_hfs_detail(reader, filesystem);
        let sgi = probe_sgi_detail(reader);
        if filesystem == FilesystemType::Unknown {
            if let Some(fs) = sgi.filesystem {
                filesystem = fs;
            }
        }
        let mut volume_label = pvd
            .as_ref()
            .map(|p| p.volume_id.clone())
            .or(hfs_volume_label)
            .or(sgi.volume_label);
        if volume_label.is_none() && filesystem == FilesystemType::Cdi {
            volume_label = crate::browse::cdi::read_volume_id(reader);
        }
        if volume_label.is_none() && filesystem == FilesystemType::Opera {
            volume_label = crate::browse::opera::read_label(reader);
        }

        let game = crate::gameid::detect_game_disc_opts(reader, pvd.as_ref(), !prefer_iso);

        Ok(Self {
            path: path.to_path_buf(),
            format,
            filesystem,
            volume_label,
            pvd,
            hfs_mdb,
            hfsplus_header,
            sgi_header: sgi.header,
            efs_partition_offset: sgi.efs_partition_offset,
            game,
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

    /// Probe a CloneCD (`.ccd`/`.img`) image via its first data track.
    fn probe_ccd(path: &Path) -> Result<Self> {
        let tracks = crate::ccd::parse_ccd(path)?;
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
            DiscFormat::CloneCd,
            &mut reader,
            #[cfg(feature = "toc")]
            toc,
        )
    }

    /// Probe an Alcohol 120% (`.mds`/`.mdf`) image via its first data track.
    ///
    /// Accepts either the `.mds` descriptor or the `.mdf` data file (resolving
    /// the sibling `.mds` in the latter case).
    fn probe_mds(path: &Path) -> Result<Self> {
        let mds_path = crate::mds::resolve_mds_path(path)?;
        let tracks = crate::mds::parse_mds(&mds_path)?;
        let data = tracks
            .iter()
            .find(|t| t.is_data)
            .ok_or(OpticaldiscsError::NoDataTrack)?;
        let mut reader = BinCueSectorReader::with_layout(
            &data.data_path,
            data.file_byte_offset,
            data.physical_sector_size,
            data.data_offset,
        )?;
        Self::build(
            path,
            DiscFormat::MdsMdf,
            &mut reader,
            #[cfg(feature = "toc")]
            None,
        )
    }

    /// Probe a Dreamcast `.gdi` disc, browsing its high-density game track.
    fn probe_gdi(path: &Path) -> Result<Self> {
        let mut reader = crate::gdi::open_gdi_hd_reader(path)?;
        Self::build(
            path,
            DiscFormat::Gdi,
            reader.as_mut(),
            #[cfg(feature = "toc")]
            None,
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

    /// Probe a PSP `.cso` (CISOv1) image: decompress on the fly and probe the
    /// underlying ISO 9660 volume.
    fn probe_cso(path: &Path) -> Result<Self> {
        let mut reader = crate::cso::CsoSectorReader::open(path)?;
        Self::build(
            path,
            DiscFormat::Cso,
            &mut reader,
            #[cfg(feature = "toc")]
            None,
        )
    }

    /// Probe a `.gz` image: decompress on the fly and probe the underlying
    /// filesystem (typically a PS2 ISO 9660 volume).
    fn probe_gz(path: &Path) -> Result<Self> {
        let mut reader = crate::gz::GzSectorReader::open(path)?;
        Self::build(
            path,
            DiscFormat::Gz,
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
                    game: None,
                    #[cfg(feature = "toc")]
                    toc,
                });
            }
        };

        // Dreamcast GD-ROM: browse the high-density game track (track 3+ at
        // frame 45000) through a GdromSectorReader that rebases absolute LBAs.
        if chd_info.is_gdrom() {
            if let Some(hd) = chd_info.find_gdrom_hd_track() {
                let inner = ChdSectorReader::open(path, hd)?;
                let mut reader = crate::sector_reader::GdromSectorReader::new(Box::new(inner));
                return Self::build(
                    path,
                    DiscFormat::Chd,
                    &mut reader,
                    #[cfg(feature = "toc")]
                    toc,
                );
            }
        }

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
/// A track's `file_byte_offset` is its INDEX 01 *relative to its own BIN file*.
/// For a single-BIN CUE every track lives in the same file, so those offsets
/// are already absolute disc positions.  For a multi-FILE CUE (one BIN per
/// track, common in redump-style dumps) each offset is only the local pregap of
/// that file, so we must accumulate a running frame total across files to
/// recover absolute, strictly-increasing offsets.
///
/// We handle both layouts uniformly by tracking the cumulative frame count of
/// all *previous* files and adding each track's local offset on top.  The
/// lead-out is the total frame count across every BIN file.  Returns `None` if
/// the track list is empty or a BIN file size cannot be determined.
#[cfg(feature = "toc")]
fn build_bincue_toc(tracks: &[crate::bincue::BinTrack]) -> Option<crate::toc::DiscTOC> {
    use crate::toc::{DiscTOC, TrackInfo};
    use std::path::Path;

    if tracks.is_empty() {
        return None;
    }

    let mut track_infos: Vec<TrackInfo> = Vec::with_capacity(tracks.len());

    // Frames in all files seen *before* the current one.
    let mut running_frames: u64 = 0;
    // Frames in the file the current track belongs to.
    let mut cur_file_frames: u64 = 0;
    let mut prev_bin: Option<&Path> = None;

    for t in tracks {
        let sector_size = t.sector_size();

        // When the BIN file changes, fold the previous file's full length into
        // the running total and measure the new file.  Single-BIN CUEs take
        // this branch exactly once (running_frames stays 0).
        if prev_bin != Some(t.bin_path.as_path()) {
            if prev_bin.is_some() {
                running_frames += cur_file_frames;
            }
            let file_len = std::fs::metadata(&t.bin_path).ok()?.len();
            cur_file_frames = file_len / sector_size;
            prev_bin = Some(t.bin_path.as_path());
        }

        let local_frames = t.file_byte_offset / sector_size;
        track_infos.push(TrackInfo {
            number: t.track_no as u8,
            offset: (running_frames + local_frames) as u32,
            track_type: t.track_type.cue_label().to_string(),
        });
    }

    // Lead-out = total frames across every file.
    let lead_out_raw = (running_frames + cur_file_frames) as u32;

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
struct SgiProbe {
    header: Option<SgiVolumeHeader>,
    efs_partition_offset: Option<u64>,
    filesystem: Option<FilesystemType>,
    volume_label: Option<String>,
}

fn probe_sgi_detail(reader: &mut dyn SectorReader) -> SgiProbe {
    let empty = SgiProbe {
        header: None,
        efs_partition_offset: None,
        filesystem: None,
        volume_label: None,
    };
    let magic_bytes = match reader.read_bytes(0, 4) {
        Ok(b) => b,
        Err(_) => return empty,
    };
    let magic = u32::from_be_bytes([
        magic_bytes[0],
        magic_bytes[1],
        magic_bytes[2],
        magic_bytes[3],
    ]);
    if magic != SGI_VOLHDR_MAGIC {
        return empty;
    }

    let header = match SgiVolumeHeader::read_from(reader) {
        Ok(h) => h,
        Err(_) => return empty,
    };

    for entry in &header.partitions {
        if entry.is_empty() {
            continue;
        }
        if entry.partition_type().is_disk_wide_wrapper() {
            continue;
        }
        let byte_off = entry.start_offset();
        let sb_byte = byte_off + EFS_BLOCKSIZE;
        let sb = match reader.read_bytes(sb_byte, EFS_BLOCKSIZE as usize) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let sb_magic = u32::from_be_bytes([sb[28], sb[29], sb[30], sb[31]]);
        if sb_magic == EFS_MAGIC_OLD || sb_magic == EFS_MAGIC_NEW {
            // Extract the EFS volume label for DiscImageInfo.
            let label = EfsSuperblock::parse(&sb)
                .ok()
                .map(|s| s.label())
                .filter(|l| !l.is_empty());
            return SgiProbe {
                header: Some(header),
                efs_partition_offset: Some(byte_off),
                filesystem: Some(FilesystemType::Efs),
                volume_label: label,
            };
        }
    }

    SgiProbe {
        header: Some(header),
        efs_partition_offset: None,
        filesystem: None,
        volume_label: None,
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
    probe_filesystem_opts(reader, false)
}

/// Like [`probe_filesystem`], but when `prefer_iso` is set the ISO 9660 tree is
/// chosen ahead of UDF whenever a PVD is present.
///
/// UDF/ISO **bridge** discs (e.g. PlayStation 2) carry equivalent trees, but
/// UDF's Anchor Volume Descriptor lives at the *end* of the disc. On a
/// sequential-access container (a gzip stream), touching the end forces a full
/// decompression, so for those we browse the ISO 9660 bridge — whose structures
/// sit near the start — instead.
pub(crate) fn probe_filesystem_opts(
    reader: &mut dyn SectorReader,
    prefer_iso: bool,
) -> Result<(FilesystemType, Option<PrimaryVolumeDescriptor>)> {
    if prefer_iso {
        if let Ok(pvd) = PrimaryVolumeDescriptor::read_from(reader) {
            let fs = if pvd.high_sierra {
                FilesystemType::HighSierra
            } else {
                FilesystemType::Iso9660
            };
            return Ok((fs, Some(pvd)));
        }
    }

    // ── Try UDF first (a UDF/ISO bridge disc should present its UDF tree) ────
    if crate::browse::udf::detect_udf(reader) {
        return Ok((FilesystemType::Udf, None));
    }

    // ── Try CD-i (Green Book): "CD-I " identifier at sector 16, byte 1 ───────
    if crate::browse::cdi::detect_cdi(reader) {
        return Ok((FilesystemType::Cdi, None));
    }

    // ── Try 3DO Opera: volume header (rec type 1 + five 0x5A) at block 0 ─────
    if crate::browse::opera::detect_opera(reader) {
        return Ok((FilesystemType::Opera, None));
    }

    // ── Try Xbox / Xbox 360 XDVDFS (magic at a game-partition base + 0x10000) ─
    if crate::browse::xdvdfs::detect(reader).is_some() {
        return Ok((FilesystemType::Xdvdfs, None));
    }

    // ── Try ISO 9660 / High Sierra ──────────────────────────────────────────
    if let Ok(pvd) = PrimaryVolumeDescriptor::read_from(reader) {
        let fs = if pvd.high_sierra {
            FilesystemType::HighSierra
        } else {
            FilesystemType::Iso9660
        };
        return Ok((fs, Some(pvd)));
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

    // ── Try UFS1 (BSD FFS) — Digital UNIX / Tru64, SunOS/Solaris CDs ─────────
    if crate::browse::ufs::detect_ufs(reader) {
        return Ok((FilesystemType::Ufs, None));
    }

    // ── Try VMS ODS-2 / Files-11 — OpenVMS discs ────────────────────────────
    if crate::browse::ods2::detect_ods2(reader) {
        return Ok((FilesystemType::Ods2, None));
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
        let probe = probe_sgi_detail(&mut reader);
        assert!(probe.header.is_some());
        assert_eq!(probe.filesystem, Some(FilesystemType::Efs));
        assert_eq!(probe.efs_partition_offset, Some((FIRST as u64) * 512));
    }

    #[test]
    fn probe_sgi_detail_returns_none_without_sgi_magic() {
        let img = vec![0u8; 4 * SECTOR_SIZE as usize];
        let mut reader = CursorReader(Cursor::new(img));
        let probe = probe_sgi_detail(&mut reader);
        assert!(probe.header.is_none());
        assert!(probe.efs_partition_offset.is_none());
        assert!(probe.filesystem.is_none());
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

    // ── build_bincue_toc tests (feature = "toc") ──────────────────────────
    //
    // Regression coverage for multi-FILE BIN/CUE TOC offsets — see commit log
    // for v0.4.3 "fix multi-FILE BIN/CUE TOC offsets".

    #[cfg(feature = "toc")]
    fn write_bin(dir: &std::path::Path, name: &str, frames: u64, sector_size: u64) -> PathBuf {
        use std::io::Write;
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        let buf = vec![0u8; (frames * sector_size) as usize];
        f.write_all(&buf).unwrap();
        path
    }

    #[cfg(feature = "toc")]
    #[test]
    fn build_bincue_toc_multi_file_offsets_are_absolute() {
        use crate::bincue::{BinTrack, TrackType};

        let tmp = tempfile::tempdir().unwrap();
        // 3 audio tracks, each in its own BIN, each with non-zero local pregap.
        // File lengths (in frames): 20000, 15000, 10000.
        // Local INDEX 01 frames: 0, 224, 148.  (224 = 00:02:74, 148 = 00:01:73)
        let ss = TrackType::Audio.sector_size();
        let p1 = write_bin(tmp.path(), "t1.bin", 20_000, ss);
        let p2 = write_bin(tmp.path(), "t2.bin", 15_000, ss);
        let p3 = write_bin(tmp.path(), "t3.bin", 10_000, ss);

        let tracks = vec![
            BinTrack {
                track_no: 1,
                track_type: TrackType::Audio,
                bin_path: p1,
                file_byte_offset: 0,
                frame_count: 0,
            },
            BinTrack {
                track_no: 2,
                track_type: TrackType::Audio,
                bin_path: p2,
                file_byte_offset: 224 * ss,
                frame_count: 0,
            },
            BinTrack {
                track_no: 3,
                track_type: TrackType::Audio,
                bin_path: p3,
                file_byte_offset: 148 * ss,
                frame_count: 0,
            },
        ];

        let toc = build_bincue_toc(&tracks).expect("toc");

        // DiscTOC::from_tracks adds a 150-frame pregap to every offset and
        // to the lead-out.  Raw offsets we fed: 0, 20000+224, 35000+148.
        let offsets: Vec<u32> = toc.track_offsets.to_vec();
        assert_eq!(offsets, vec![150, 20_224 + 150, 35_148 + 150]);

        // Strictly increasing offsets (MusicBrainz requirement).
        assert!(
            offsets.windows(2).all(|w| w[0] < w[1]),
            "offsets must increase: {offsets:?}"
        );

        // Lead-out = sum of all file frames + 150 pregap.
        assert_eq!(toc.lead_out, 20_000 + 15_000 + 10_000 + 150);

        // to_toc_string is also monotonic across track offsets.
        // Format: "first_track+track_count+leadout+offset1+offset2+…"
        let s = toc.to_toc_string();
        let nums: Vec<u32> = s.split('+').skip(3).map(|n| n.parse().unwrap()).collect();
        assert_eq!(nums, offsets);
    }

    #[cfg(feature = "toc")]
    #[test]
    fn build_bincue_toc_single_bin_unchanged() {
        use crate::bincue::{BinTrack, TrackType};

        // Single shared BIN: every track's file_byte_offset is already an
        // absolute disc position, so behavior must match the pre-fix code.
        let tmp = tempfile::tempdir().unwrap();
        let ss = TrackType::Audio.sector_size();
        let total_frames = 50_000u64;
        let bin = write_bin(tmp.path(), "all.bin", total_frames, ss);

        let tracks = vec![
            BinTrack {
                track_no: 1,
                track_type: TrackType::Audio,
                bin_path: bin.clone(),
                file_byte_offset: 0,
                frame_count: 20_000,
            },
            BinTrack {
                track_no: 2,
                track_type: TrackType::Audio,
                bin_path: bin.clone(),
                file_byte_offset: 20_000 * ss,
                frame_count: 15_000,
            },
            BinTrack {
                track_no: 3,
                track_type: TrackType::Audio,
                bin_path: bin,
                file_byte_offset: 35_000 * ss,
                frame_count: 15_000,
            },
        ];

        let toc = build_bincue_toc(&tracks).expect("toc");
        let offsets: Vec<u32> = toc.track_offsets.to_vec();
        assert_eq!(offsets, vec![150, 20_000 + 150, 35_000 + 150]);
        assert_eq!(toc.lead_out, total_frames as u32 + 150);
    }
}
