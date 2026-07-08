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

/// Byte offset to user data within a raw Mode 2 (Form 1, XA) sector.
///
/// Mode 2 sectors carry an 8-byte subheader after the 16-byte sync+header,
/// so the 2048-byte user area starts at offset 24.
pub const MODE2_DATA_OFFSET: u64 = 24;

/// 12-byte sync pattern that begins every raw (2352-byte) CD sector.
const RAW_SYNC: [u8; 12] = [
    0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00,
];

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

/// `SectorReader` for plain `.iso` / `.toast` files, **and** for bare `.iso`
/// files that are actually raw 2352-byte-sector dumps.
///
/// Most `.iso` files store sectors consecutively as 2048-byte cooked data with
/// no headers, so `read_bytes` uses a single seek+read. Some dumps (often from
/// CD-burning tools that saved the full raw sector) instead store 2352-byte raw
/// sectors — recognisable by the 12-byte sync pattern `00 FF×10 00` at offset 0.
/// [`IsoSectorReader::new`] auto-detects this and transparently strips the raw
/// sync+header (+ Mode 2 subheader) so callers still see a 2048-byte cooked view.
pub struct IsoSectorReader {
    file: BufReader<File>,
    /// Physical bytes per sector in the file: 2048 (cooked) or 2352 (raw).
    physical_sector_size: u64,
    /// Byte offset within each physical sector to the start of user data:
    /// 0 (cooked), 16 (raw Mode 1), or 24 (raw Mode 2 Form 1).
    data_offset: u64,
}

impl IsoSectorReader {
    /// Open an ISO image file for reading, auto-detecting raw 2352-byte layout.
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let mut file = BufReader::new(File::open(path.as_ref()).map_err(OpticaldiscsError::Io)?);

        // Sniff the first 16 bytes for the raw-sector sync pattern.
        let mut head = [0u8; 16];
        let (physical_sector_size, data_offset) = match file.read_exact(&mut head) {
            Ok(()) if head[..12] == RAW_SYNC => {
                // Byte 15 is the mode byte (after the 3-byte MSF address).
                let data_offset = match head[15] {
                    2 => MODE2_DATA_OFFSET,
                    _ => MODE1_DATA_OFFSET, // Mode 1 (and unknown → treat as Mode 1)
                };
                (RAW_SECTOR_SIZE, data_offset)
            }
            _ => (SECTOR_SIZE, 0),
        };
        file.seek(SeekFrom::Start(0))
            .map_err(OpticaldiscsError::Io)?;

        Ok(Self {
            file,
            physical_sector_size,
            data_offset,
        })
    }

    /// True if this image is stored as raw 2352-byte sectors.
    fn is_raw(&self) -> bool {
        self.physical_sector_size != SECTOR_SIZE
    }
}

impl SectorReader for IsoSectorReader {
    fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
        let offset = lba * self.physical_sector_size + self.data_offset;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(OpticaldiscsError::Io)?;
        let mut buf = vec![0u8; SECTOR_SIZE as usize];
        self.file
            .read_exact(&mut buf)
            .map_err(OpticaldiscsError::Io)?;
        Ok(buf)
    }

    /// Cooked images use an optimised direct seek+read; raw images fall back to
    /// sector-at-a-time composition (the default impl) so the per-sector
    /// sync+header gaps are stripped correctly.
    fn read_bytes(&mut self, byte_offset: u64, length: usize) -> Result<Vec<u8>> {
        if self.is_raw() {
            // Sector-composed read (mirrors the trait default) to skip gaps.
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
            return Ok(out);
        }
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

// ── GD-ROM high-density area reader ──────────────────────────────────────────

/// The Logical Block Address at which a Dreamcast GD-ROM's high-density area
/// (the game data) begins. Track 3 of a GD-ROM always starts here.
pub const GDROM_HD_START_LBA: u64 = 45000;

/// A `SectorReader` that presents a Dreamcast GD-ROM high-density track using
/// the disc's *absolute* LBAs.
///
/// The high-density area's ISO 9660 volume is physically stored starting at
/// absolute LBA [`GDROM_HD_START_LBA`] (track 3), but its directory records
/// reference **absolute** disc LBAs (e.g. the root directory at 45020) while the
/// Primary Volume Descriptor is still addressed volume-relative at LBA 16. This
/// wrapper reconciles the two: any requested LBA at or beyond
/// [`GDROM_HD_START_LBA`] is rebased into the track (`lba - 45000`), while lower
/// LBAs (the PVD at 16, the path table) pass through to the track directly. The
/// net effect is that the standard [`crate::browse::iso9660`] browser reads the
/// HD-area filesystem correctly with no Dreamcast-specific knowledge.
pub struct GdromSectorReader {
    inner: Box<dyn SectorReader>,
}

impl GdromSectorReader {
    /// Wrap a reader already scoped to the GD-ROM high-density track (track 3).
    pub fn new(inner: Box<dyn SectorReader>) -> Self {
        Self { inner }
    }
}

impl SectorReader for GdromSectorReader {
    fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
        let mapped = if lba >= GDROM_HD_START_LBA {
            lba - GDROM_HD_START_LBA
        } else {
            lba
        };
        self.inner.read_sector(mapped)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build one raw 2352-byte sector carrying `data` as its cooked payload.
    fn raw_sector(mode: u8, data: &[u8]) -> Vec<u8> {
        let mut s = vec![0u8; RAW_SECTOR_SIZE as usize];
        s[..12].copy_from_slice(&RAW_SYNC);
        s[15] = mode;
        let off = if mode == 2 {
            MODE2_DATA_OFFSET
        } else {
            MODE1_DATA_OFFSET
        } as usize;
        let n = data.len().min(SECTOR_SIZE as usize);
        s[off..off + n].copy_from_slice(&data[..n]);
        s
    }

    fn write_tmp_iso(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(".iso").tempfile().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    /// A cooked `.iso` is read at 2048-byte stride with no offset.
    #[test]
    fn cooked_iso_reads_directly() {
        let mut img = vec![0u8; 18 * SECTOR_SIZE as usize];
        // Marker at start of cooked sector 16.
        let off = 16 * SECTOR_SIZE as usize;
        img[off..off + 5].copy_from_slice(b"CD001");
        let f = write_tmp_iso(&img);
        let mut r = IsoSectorReader::new(f.path()).unwrap();
        assert!(!r.is_raw());
        assert_eq!(&r.read_sector(16).unwrap()[..5], b"CD001");
        assert_eq!(&r.read_bytes(16 * SECTOR_SIZE, 5).unwrap(), b"CD001");
    }

    /// A raw 2352-byte Mode 1 `.iso` is auto-detected and the sync/header
    /// stripped so callers still see cooked 2048-byte sectors.
    #[test]
    fn raw_mode1_iso_is_detected_and_stripped() {
        let mut sectors: Vec<u8> = Vec::new();
        for lba in 0..18u8 {
            let mut data = vec![0u8; SECTOR_SIZE as usize];
            if lba == 16 {
                data[..5].copy_from_slice(b"CD001");
                data[100..105].copy_from_slice(b"HELLO");
            }
            sectors.extend_from_slice(&raw_sector(1, &data));
        }
        let f = write_tmp_iso(&sectors);
        let mut r = IsoSectorReader::new(f.path()).unwrap();
        assert!(r.is_raw());
        assert_eq!(r.physical_sector_size, RAW_SECTOR_SIZE);
        assert_eq!(r.data_offset, MODE1_DATA_OFFSET);
        assert_eq!(&r.read_sector(16).unwrap()[..5], b"CD001");
        // read_bytes must compose across the raw sector gaps correctly.
        assert_eq!(&r.read_bytes(16 * SECTOR_SIZE + 100, 5).unwrap(), b"HELLO");
    }

    /// The GD-ROM reader rebases absolute HD-area LBAs into the track while
    /// passing volume-relative low LBAs (the PVD at 16) straight through.
    #[test]
    fn gdrom_reader_rebases_absolute_lbas() {
        struct Track(Vec<u8>);
        impl SectorReader for Track {
            fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
                let off = (lba * SECTOR_SIZE) as usize;
                Ok(self.0[off..off + SECTOR_SIZE as usize].to_vec())
            }
        }
        // 25-sector track: mark sector 16 (PVD slot) and sector 20 (abs 45020).
        let mut buf = vec![0u8; 25 * SECTOR_SIZE as usize];
        buf[16 * SECTOR_SIZE as usize] = 0xAA;
        buf[20 * SECTOR_SIZE as usize] = 0xBB;
        let mut r = GdromSectorReader::new(Box::new(Track(buf)));

        // LBA 16 < 45000 → track-relative 16.
        assert_eq!(r.read_sector(16).unwrap()[0], 0xAA);
        // Absolute LBA 45020 → track-relative 20.
        assert_eq!(r.read_sector(GDROM_HD_START_LBA + 20).unwrap()[0], 0xBB);
    }

    /// Mode 2 raw sectors put user data at offset 24.
    #[test]
    fn raw_mode2_iso_uses_offset_24() {
        let mut sectors: Vec<u8> = Vec::new();
        for lba in 0..18u8 {
            let mut data = vec![0u8; SECTOR_SIZE as usize];
            if lba == 16 {
                data[..5].copy_from_slice(b"CD001");
            }
            sectors.extend_from_slice(&raw_sector(2, &data));
        }
        let f = write_tmp_iso(&sectors);
        let mut r = IsoSectorReader::new(f.path()).unwrap();
        assert!(r.is_raw());
        assert_eq!(r.data_offset, MODE2_DATA_OFFSET);
        assert_eq!(&r.read_sector(16).unwrap()[..5], b"CD001");
    }
}
