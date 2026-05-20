//! `SectorReader` trait and format-specific implementations.
//!
//! All implementations present a uniform view of 2048-byte cooked sectors,
//! stripping container-specific headers transparently. Callers never need to
//! know whether the underlying file is a plain ISO, a raw BIN, or a CHD.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::{OpticaldiscsError, Result};

/// Cooked sector size in bytes (ISO 9660 logical sector).
pub const SECTOR_SIZE: u64 = 2048;

/// Raw CD sector size in bytes (including sync, header, ECC/EDC).
pub const RAW_SECTOR_SIZE: u64 = 2352;

/// Byte offset to user data within a raw Mode 1 sector.
pub const MODE1_DATA_OFFSET: u64 = 16;

/// Abstraction over different disc image containers.
///
/// Implementations always return 2048-byte cooked sectors — the raw sector
/// headers present in BIN/CUE and CHD files are stripped internally.
pub trait SectorReader: Send {
    /// Read a single 2048-byte cooked sector at the given Logical Block Address.
    fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>>;

    /// Read `length` bytes starting at `byte_offset` (cooked address space).
    ///
    /// The default implementation composes calls to `read_sector`; override
    /// for formats (e.g. plain ISO) where a direct seek+read is cheaper.
    fn read_bytes(&mut self, byte_offset: u64, length: usize) -> Result<Vec<u8>> {
        let sector_lba = byte_offset / SECTOR_SIZE;
        let sector_off = (byte_offset % SECTOR_SIZE) as usize;
        let mut out = Vec::with_capacity(length);
        let mut remaining = length;
        let mut lba = sector_lba;
        let mut offset = sector_off;

        while remaining > 0 {
            let sector = self.read_sector(lba)?;
            let available = sector.len().saturating_sub(offset);
            let take = remaining.min(available);
            out.extend_from_slice(&sector[offset..offset + take]);
            remaining -= take;
            lba += 1;
            offset = 0;
        }
        Ok(out)
    }
}

// ── Phase 2: IsoSectorReader ──────────────────────────────────────────────────

/// `SectorReader` for plain `.iso` / `.toast` files.
///
/// Sectors are stored consecutively as 2048-byte cooked data with no headers,
/// so `read_bytes` is overridden to use a single seek+read.
pub struct IsoSectorReader {
    file: BufReader<File>,
}

impl IsoSectorReader {
    /// Open an ISO image file for reading.
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::open(path.as_ref()).map_err(OpticaldiscsError::Io)?;
        Ok(Self {
            file: BufReader::new(file),
        })
    }
}

impl SectorReader for IsoSectorReader {
    fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
        let offset = lba * SECTOR_SIZE;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(OpticaldiscsError::Io)?;
        let mut buf = vec![0u8; SECTOR_SIZE as usize];
        self.file
            .read_exact(&mut buf)
            .map_err(OpticaldiscsError::Io)?;
        Ok(buf)
    }

    /// Optimised override: direct seek+read avoids sector-at-a-time looping.
    fn read_bytes(&mut self, byte_offset: u64, length: usize) -> Result<Vec<u8>> {
        self.file
            .seek(SeekFrom::Start(byte_offset))
            .map_err(OpticaldiscsError::Io)?;
        let mut buf = vec![0u8; length];
        self.file
            .read_exact(&mut buf)
            .map_err(OpticaldiscsError::Io)?;
        Ok(buf)
    }
}

// ── Phase 3: BinCueSectorReader ──────────────────────────────────────────────

/// `SectorReader` for BIN/CUE disc images.
///
/// Reads a single data track from a raw `.bin` file, translating logical
/// 2048-byte sector addresses to physical byte offsets in the file, stripping
/// raw sector headers (sync + header bytes) transparently.
///
/// Create one via [`BinCueSectorReader::open`] by passing the data track from
/// [`crate::bincue::parse_cue_tracks`].
pub struct BinCueSectorReader {
    file: BufReader<File>,
    /// Byte offset in the BIN file where this track's sectors start.
    file_byte_offset: u64,
    /// Physical bytes per sector in the BIN file (2352, 2048, or 2336).
    physical_sector_size: u64,
    /// Byte offset within each physical sector to the start of user data.
    data_offset: u64,
}

impl BinCueSectorReader {
    /// Open a BIN/CUE data track for reading.
    ///
    /// `track` should be a data track. Use
    /// `parse_cue_tracks(cue_path)?.into_iter().find(|t| t.is_data())`
    /// to obtain one.
    pub fn open(track: &crate::bincue::BinTrack) -> Result<Self> {
        let file = File::open(&track.bin_path).map_err(OpticaldiscsError::Io)?;
        Ok(Self {
            file: BufReader::new(file),
            file_byte_offset: track.file_byte_offset,
            physical_sector_size: track.sector_size(),
            data_offset: track.data_offset(),
        })
    }
}

impl SectorReader for BinCueSectorReader {
    /// Read a 2048-byte cooked sector at `lba`.
    ///
    /// Physical layout per sector:
    /// `file_byte_offset + lba * physical_sector_size + data_offset`
    fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
        let physical_offset =
            self.file_byte_offset + lba * self.physical_sector_size + self.data_offset;
        self.file
            .seek(SeekFrom::Start(physical_offset))
            .map_err(OpticaldiscsError::Io)?;
        let mut buf = vec![0u8; SECTOR_SIZE as usize];
        self.file
            .read_exact(&mut buf)
            .map_err(OpticaldiscsError::Io)?;
        Ok(buf)
    }
}

// ── Phase 4: ChdSectorReader ─────────────────────────────────────────────────

/// `SectorReader` for CHD optical disc images.
///
/// Backed by [`libchdman_rs::cd::CdCookedReader`], which wraps MAME's
/// `chd_file` core and yields a 2048-byte cooked stream for the selected
/// track. Multi-track CHDs are supported: the 1-based track number from
/// [`crate::chd::ChdTrack`] is translated to libchdman-rs's 0-based index.
///
/// Create one via [`ChdSectorReader::open`] by passing the path to the `.chd`
/// file and the data track obtained from [`crate::chd::open_chd`].
pub struct ChdSectorReader {
    inner: libchdman_rs::cd::CdCookedReader,
}

impl ChdSectorReader {
    /// Open a CHD file and prepare to read sectors from `track`.
    ///
    /// Opens a fresh CHD handle and selects the requested track. Audio tracks
    /// are rejected by libchdman-rs — pass a data track (use
    /// [`crate::chd::ChdInfo::find_first_data_track`]).
    pub fn open(path: impl AsRef<Path>, track: &crate::chd::ChdTrack) -> Result<Self> {
        let path = path.as_ref();

        // Surface missing/unreadable files as Io rather than Chd.
        std::fs::metadata(path).map_err(OpticaldiscsError::Io)?;

        let path_str = path.to_str().ok_or_else(|| {
            OpticaldiscsError::Chd(format!("non-UTF-8 CHD path: {}", path.display()))
        })?;

        let chd = libchdman_rs::Chd::open(path_str, false, None)
            .map_err(|e| OpticaldiscsError::Chd(format!("failed to open CHD: {e:?}")))?;

        let track_index = track.track_no.checked_sub(1).ok_or_else(|| {
            OpticaldiscsError::Chd(format!(
                "invalid track_no {} (must be >= 1)",
                track.track_no
            ))
        })?;

        let inner =
            libchdman_rs::cd::CdCookedReader::open_track(chd, track_index).map_err(|e| {
                OpticaldiscsError::Chd(format!("open CHD track {}: {e:?}", track.track_no))
            })?;

        Ok(Self { inner })
    }
}

impl SectorReader for ChdSectorReader {
    /// Read a 2048-byte cooked sector at `lba` (track-relative).
    fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
        self.inner
            .seek(SeekFrom::Start(lba * SECTOR_SIZE))
            .map_err(OpticaldiscsError::Io)?;
        let mut buf = vec![0u8; SECTOR_SIZE as usize];
        self.inner
            .read_exact(&mut buf)
            .map_err(OpticaldiscsError::Io)?;
        Ok(buf)
    }
}
