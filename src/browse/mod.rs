//! Filesystem browsing for disc images.
//!
//! The main entry point is [`open_disc_filesystem`], which opens the
//! appropriate sector reader for the container format and wraps it in the
//! right `Filesystem` implementation for the on-disc filesystem type.
//!
//! See PLAN.md Phases 7–8 for implementation details.

pub mod entry;
pub mod filesystem;

// Phase 7
pub mod iso9660;
pub mod rockridge;

// Phase 8
pub mod hfs;
pub mod hfsplus;
pub mod mac_alias;

// SGI EFS
pub mod efs;

// BSD UFS / FFS
pub mod ufs;

// VMS ODS-2 / Files-11
pub mod ods2;

// UDF (DVD/BD)
pub mod udf;

// Nintendo GameCube / Wii (via the nod crate)
pub mod nod_fs;

// Philips CD-i (Green Book)
pub mod cdi;

// 3DO Opera
pub mod opera;

// Xbox / Xbox 360 XDVDFS
pub mod xdvdfs;

pub use cdi::CdiFilesystem;
pub use efs::EfsFilesystem;
pub use entry::{EntryType, FileEntry};
pub use filesystem::{Filesystem, FilesystemError};
pub use hfs::HfsFilesystem;
pub use hfsplus::HfsPlusFilesystem;
pub use iso9660::Iso9660Filesystem;
pub use nod_fs::NodeFilesystem;
pub use ods2::Ods2Filesystem;
pub use opera::OperaFilesystem;
pub use udf::UdfFilesystem;
pub use ufs::UfsFilesystem;
pub use xdvdfs::XdvdfsFilesystem;

use crate::detect::DiscImageInfo;
use crate::error::OpticaldiscsError;
use crate::formats::{DiscFormat, FilesystemType};
use crate::sector_reader::SectorReader;

// ── open_disc_filesystem ──────────────────────────────────────────────────────

/// Open a browsable filesystem for `info`.
///
/// Creates the appropriate [`SectorReader`] for the container format (ISO,
/// BIN/CUE, or CHD), then wraps it in the right [`Filesystem`] implementation
/// for the on-disc filesystem type.
///
/// Supported filesystem types:
/// - [`FilesystemType::Iso9660`] — all three container formats
/// - [`FilesystemType::Hfs`] — HFS (classic Mac CDs)
/// - [`FilesystemType::HfsPlus`] — HFS+ (Mac OS X CDs/DVDs)
///
/// # Errors
///
/// Returns [`FilesystemError::Unsupported`] when the container format or
/// filesystem type is not yet supported.  Returns [`FilesystemError::Io`]
/// or [`FilesystemError::Parse`] if the disc cannot be opened or the
/// filesystem header is malformed.
pub fn open_disc_filesystem(info: &DiscImageInfo) -> Result<Box<dyn Filesystem>, FilesystemError> {
    // Nintendo discs are browsed directly from the file path via `nod`, which
    // owns its own reader (and, for Wii, decrypts partitions) — they do not use
    // the SectorReader abstraction.
    if matches!(
        info.filesystem,
        FilesystemType::GameCube | FilesystemType::Wii
    ) {
        return Ok(Box::new(NodeFilesystem::new(&info.path)?));
    }

    let mut reader = open_sector_reader(info)?;

    match info.filesystem {
        FilesystemType::Iso9660 | FilesystemType::HighSierra => {
            Ok(Box::new(Iso9660Filesystem::new(reader)?))
        }

        FilesystemType::Efs => {
            let partition_offset = info.efs_partition_offset.ok_or_else(|| {
                FilesystemError::InvalidData(
                    "EFS detected but partition offset not recorded".into(),
                )
            })?;
            Ok(Box::new(EfsFilesystem::new(reader, partition_offset)?))
        }

        FilesystemType::Ufs => Ok(Box::new(UfsFilesystem::new(reader)?)),

        FilesystemType::Ods2 => Ok(Box::new(Ods2Filesystem::new(reader)?)),

        FilesystemType::Udf => Ok(Box::new(UdfFilesystem::new(reader)?)),

        FilesystemType::Cdi => Ok(Box::new(CdiFilesystem::new(reader)?)),

        FilesystemType::Opera => Ok(Box::new(OperaFilesystem::new(reader)?)),

        FilesystemType::Xdvdfs => Ok(Box::new(XdvdfsFilesystem::new(reader)?)),

        FilesystemType::Hfs | FilesystemType::HfsPlus => {
            // Use resolve_apple_hfs to get the correct offset — this handles
            // native HFS+, embedded HFS+ (HFS wrapper), and pure HFS.
            let raw_offset = crate::apm::find_hfs_partition_offset(reader.as_mut()).unwrap_or(0);
            let (resolved_fs, resolved_offset) =
                crate::detect::resolve_apple_hfs(reader.as_mut(), raw_offset);

            match resolved_fs {
                FilesystemType::Hfs => Ok(Box::new(HfsFilesystem::new(reader, raw_offset)?)),
                FilesystemType::HfsPlus => {
                    Ok(Box::new(HfsPlusFilesystem::new(reader, resolved_offset)?))
                }
                _ => Err(FilesystemError::Unsupported),
            }
        }

        _ => Err(FilesystemError::Unsupported),
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Build a boxed [`SectorReader`] appropriate for `info`'s container format.
fn open_sector_reader(info: &DiscImageInfo) -> Result<Box<dyn SectorReader>, FilesystemError> {
    let path = &info.path;

    match info.format {
        DiscFormat::Iso => {
            let reader = crate::sector_reader::IsoSectorReader::new(path).map_err(disc_err)?;
            Ok(Box::new(reader))
        }

        DiscFormat::BinCue => {
            let cue_path = resolve_cue_path(path)?;
            let tracks = crate::bincue::parse_cue_tracks(&cue_path)
                .map_err(|e| FilesystemError::InvalidData(e.to_string()))?;
            let data_track = tracks
                .iter()
                .find(|t| t.is_data())
                .ok_or_else(|| FilesystemError::InvalidData("no data track in CUE sheet".into()))?
                .clone();
            let reader =
                crate::sector_reader::BinCueSectorReader::open(&data_track).map_err(disc_err)?;
            Ok(Box::new(reader))
        }

        DiscFormat::Chd => {
            let chd_info = crate::chd::open_chd(path).map_err(disc_err)?;
            // Dreamcast GD-ROM: the high-density area can span several data tracks
            // (separated by audio tracks), so wrap every HD data track and let the
            // GdromSectorReader route absolute LBAs to the track that holds them
            // (see GdromSectorReader) — the ISO 9660 browser then sees one volume.
            if chd_info.is_gdrom() {
                let hd_tracks = chd_info.find_gdrom_hd_tracks();
                if !hd_tracks.is_empty() {
                    let mut gd_tracks = Vec::with_capacity(hd_tracks.len());
                    for hd in hd_tracks {
                        let reader = crate::sector_reader::ChdSectorReader::open(path, hd)
                            .map_err(disc_err)?;
                        gd_tracks.push(crate::sector_reader::GdromHdTrack {
                            start_lba: hd.frame_offset,
                            frame_count: hd.frames as u64,
                            reader: Box::new(reader),
                        });
                    }
                    return Ok(Box::new(
                        crate::sector_reader::GdromSectorReader::from_tracks(gd_tracks),
                    ));
                }
            }
            let track = chd_info
                .find_first_data_track()
                .ok_or(FilesystemError::Unsupported)?
                .clone();
            let reader =
                crate::sector_reader::ChdSectorReader::open(path, &track).map_err(disc_err)?;
            Ok(Box::new(reader))
        }

        DiscFormat::Gdi => crate::gdi::open_gdi_hd_reader(path).map_err(disc_err),

        DiscFormat::Cso => {
            let reader = crate::cso::CsoSectorReader::open(path).map_err(disc_err)?;
            Ok(Box::new(reader))
        }

        DiscFormat::Gz => {
            let reader = crate::gz::GzSectorReader::open(path).map_err(disc_err)?;
            Ok(Box::new(reader))
        }

        DiscFormat::CloneCd => {
            let tracks = crate::ccd::parse_ccd(path).map_err(disc_err)?;
            let data_track = tracks
                .iter()
                .find(|t| t.is_data())
                .ok_or_else(|| {
                    FilesystemError::InvalidData("no data track in .ccd descriptor".into())
                })?
                .clone();
            let reader =
                crate::sector_reader::BinCueSectorReader::open(&data_track).map_err(disc_err)?;
            Ok(Box::new(reader))
        }

        DiscFormat::MdsMdf => {
            let mds_path = crate::mds::resolve_mds_path(path).map_err(disc_err)?;
            let tracks = crate::mds::parse_mds(&mds_path).map_err(disc_err)?;
            let data = tracks.iter().find(|t| t.is_data).ok_or_else(|| {
                FilesystemError::InvalidData("no data track in .mds descriptor".into())
            })?;
            let reader = crate::sector_reader::BinCueSectorReader::with_layout(
                &data.data_path,
                data.file_byte_offset,
                data.physical_sector_size,
                data.data_offset,
            )
            .map_err(disc_err)?;
            Ok(Box::new(reader))
        }

        DiscFormat::Nrg => {
            let tracks = crate::nrg::parse_nrg(path).map_err(disc_err)?;
            let data = tracks.iter().find(|t| t.is_data).ok_or_else(|| {
                FilesystemError::InvalidData("no data track in .nrg image".into())
            })?;
            let reader = crate::sector_reader::BinCueSectorReader::with_layout(
                &data.data_path,
                data.file_byte_offset,
                data.physical_sector_size,
                data.data_offset,
            )
            .map_err(disc_err)?;
            Ok(Box::new(reader))
        }

        DiscFormat::DiscJuggler => {
            let tracks = crate::discjuggler::parse_discjuggler(path).map_err(disc_err)?;
            // Browse the last data track (the Dreamcast HD game session; session
            // 0 is a low-density stub), rebasing absolute directory LBAs.
            let data = tracks.iter().rev().find(|t| t.is_data).ok_or_else(|| {
                FilesystemError::InvalidData("no data track in .cdi image".into())
            })?;
            let inner = crate::sector_reader::BinCueSectorReader::with_layout(
                &data.data_path,
                data.file_byte_offset,
                data.physical_sector_size,
                data.data_offset,
            )
            .map_err(disc_err)?;
            Ok(Box::new(crate::sector_reader::RebaseSectorReader::new(
                Box::new(inner),
                data.base_lba,
            )))
        }

        #[cfg(feature = "mdx")]
        DiscFormat::Mdx => {
            let tracks = crate::mdx::parse_mdx(path).map_err(disc_err)?;
            let data = tracks.iter().find(|t| t.is_data).ok_or_else(|| {
                FilesystemError::InvalidData("no data track in .mdx image".into())
            })?;
            let reader = crate::mdx::MdxSectorReader::open(data).map_err(disc_err)?;
            Ok(Box::new(reader))
        }

        #[cfg(not(feature = "mdx"))]
        DiscFormat::Mdx => Err(FilesystemError::Unsupported),

        DiscFormat::Nintendo => Err(FilesystemError::Unsupported),
    }
}

/// Resolve the CUE path from a `.bin` or `.cue` file path.
///
/// If `path` has a `.bin` extension, looks for a matching `.cue` in the same
/// directory.  Otherwise returns `path` unchanged.
fn resolve_cue_path(path: &std::path::Path) -> Result<std::path::PathBuf, FilesystemError> {
    let is_bin = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
        == Some("bin");

    if is_bin {
        let stem = path.file_stem().unwrap_or_default();
        let cue = path.with_file_name(format!("{}.cue", stem.to_string_lossy()));
        if cue.exists() {
            Ok(cue)
        } else {
            Err(FilesystemError::NotFound(format!(
                "no matching .cue found for {}",
                path.display()
            )))
        }
    } else {
        Ok(path.to_path_buf())
    }
}

/// Convert an [`OpticaldiscsError`] to a [`FilesystemError`].
fn disc_err(e: OpticaldiscsError) -> FilesystemError {
    match e {
        OpticaldiscsError::Io(io_err) => FilesystemError::Io(io_err),
        e => FilesystemError::InvalidData(e.to_string()),
    }
}
