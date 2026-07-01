//! `opticaldiscs` — format-agnostic optical disc image reading and browsing.
//!
//! # Overview
//!
//! This library provides a unified `SectorReader` abstraction that handles the
//! cooked/raw sector translation across three container formats:
//!
//! - **ISO** (`.iso`, `.toast`) — plain 2048-byte cooked sectors
//! - **BIN/CUE** — raw 2352-byte sectors with per-track header stripping
//! - **CHD** — MAME Compressed Hunks of Data, optical variant
//!
//! On top of `SectorReader`, filesystem browsers are provided for:
//! - ISO 9660 (data CDs and DVDs)
//! - HFS (classic Mac CDs)
//! - HFS+ (Mac OS X CDs/DVDs)
//! - SGI EFS (IRIX install/distribution CDs, via SGI Volume Header)
//!
//! # Feature Flags
//!
//! - `toc` — enables `DiscTOC`, `TrackInfo`, MusicBrainz DiscID, and FreeDB ID
//!   calculation (requires `sha1` and `base64`).
//! - `drives` — enables `list_drives()` for enumerating physical optical drives
//!   on Linux, macOS, and Windows.
//!
//! # Quick Example
//!
//! ```ignore
//! use opticaldiscs::detect::DiscImageInfo;
//! use opticaldiscs::browse;
//!
//! let info = DiscImageInfo::open("disc.iso").unwrap();
//! println!("Volume: {:?}", info.volume_label);
//!
//! let mut fs = browse::open_disc_filesystem(&info).unwrap();
//! let root = fs.root().unwrap();
//! for entry in fs.list_directory(&root).unwrap() {
//!     println!("{} ({})", entry.name, entry.size_string());
//! }
//! ```

// ── Modules (implemented progressively per PLAN.md) ──────────────────────────

// Phase 1
pub mod browse;
pub mod error;
pub mod formats;

// Phase 2-4
pub mod bincue;
pub mod chd;
pub mod iso9660;
pub mod sector_reader;

// Phase 5
#[cfg(feature = "toc")]
pub mod toc;

// Phase 6
pub mod detect;

// Phase 7 (HFS metadata — not browsing, see browse/)
pub mod apm;
pub mod hfs;
pub mod hfsplus;

// SGI Volume Header (IRIX install/distribution CDs — see docs/EFS_Implementation.md)
pub mod sgi;

// EFS filesystem on-disk structures.
pub mod efs;

// Phase 9
#[cfg(feature = "drives")]
pub mod drives;

// ── Top-level re-exports ──────────────────────────────────────────────────────

// Phase 1
pub use browse::entry::{
    EntryType, FileEntry, FileTimestamps, PosixMetadata, MAC_EPOCH_UNIX_OFFSET,
};
pub use browse::filesystem::{Filesystem, FilesystemError};
pub use error::OpticaldiscsError;
pub use formats::{supported_extensions, DiscFormat, FilesystemType};

// ISO 9660 metadata types
pub use iso9660::{Iso9660DateTime, JolietVolumeDescriptor, PrimaryVolumeDescriptor, PvdDateTime};

// Phase 2
pub use sector_reader::SectorReader;

// Phase 3
pub use sector_reader::BinCueSectorReader;

// Phase 4
pub use sector_reader::ChdSectorReader;

// Phase 8
pub use hfs::MasterDirectoryBlock;
pub use hfsplus::HfsPlusVolumeHeader;

// SGI
pub use sgi::{SgiPartitionEntry, SgiPartitionType, SgiVolumeHeader};

// EFS
pub use browse::EfsFilesystem;
pub use efs::{EfsExtent, EfsInode, EfsSuperblock};
