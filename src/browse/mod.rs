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

// Phase 8
pub mod hfs;
pub mod hfsplus;

pub use entry::{EntryType, FileEntry};
pub use filesystem::{Filesystem, FilesystemError};
pub use hfs::HfsFilesystem;
pub use hfsplus::HfsPlusFilesystem;
pub use iso9660::Iso9660Filesystem;

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
    let mut reader = open_sector_reader(info)?;

    match info.filesystem {
        FilesystemType::Iso9660 => Ok(Box::new(Iso9660Filesystem::new(reader)?)),

        FilesystemType::Hfs => {
            // Try APM to find the HFS partition offset; fall back to 0 for
            // non-APM images where the HFS filesystem starts at the disc root.
            let partition_offset =
                crate::apm::find_hfs_partition_offset(reader.as_mut()).unwrap_or(0);
            Ok(Box::new(HfsFilesystem::new(reader, partition_offset)?))
        }

        FilesystemType::HfsPlus => {
            let partition_offset =
                crate::apm::find_hfs_partition_offset(reader.as_mut()).unwrap_or(0);
            Ok(Box::new(HfsPlusFilesystem::new(reader, partition_offset)?))
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
            let track = chd_info
                .find_first_data_track()
                .ok_or(FilesystemError::Unsupported)?
                .clone();
            let reader =
                crate::sector_reader::ChdSectorReader::open(path, &track).map_err(disc_err)?;
            Ok(Box::new(reader))
        }

        DiscFormat::MdsMdf => Err(FilesystemError::Unsupported),
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
