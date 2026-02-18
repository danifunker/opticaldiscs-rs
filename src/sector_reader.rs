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
// TODO: implement in Phase 3

// ── Phase 4: ChdSectorReader ─────────────────────────────────────────────────
// TODO: implement in Phase 4
