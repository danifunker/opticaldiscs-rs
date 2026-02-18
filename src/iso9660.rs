//! ISO 9660 Primary Volume Descriptor (PVD) parsing.
//!
//! The PVD lives at sector 16 (byte offset 32 768) and is the entry point for
//! everything else: volume label, root directory location, and block geometry.
//!
//! Reference: ECMA-119 / ISO 9660, section 8.4.

use crate::error::{OpticaldiscsError, Result};
use crate::sector_reader::SectorReader;

/// Sector number of the Primary Volume Descriptor.
pub const PVD_SECTOR: u64 = 16;

/// Byte offset of the PVD from the start of the disc.
pub const PVD_OFFSET: u64 = PVD_SECTOR * crate::sector_reader::SECTOR_SIZE;

const PVD_TYPE: u8 = 0x01;
const VD_SET_TERMINATOR_TYPE: u8 = 0xFF;
const ISO9660_ID: &[u8; 5] = b"CD001";

/// Information extracted from an ISO 9660 Primary Volume Descriptor.
#[derive(Debug, Clone)]
pub struct PrimaryVolumeDescriptor {
    /// Volume identifier (label), up to 32 characters.
    pub volume_id: String,
    /// System identifier, up to 32 characters.
    pub system_id: String,
    /// Volume set identifier, up to 128 characters.
    pub volume_set_id: String,
    /// Publisher identifier, up to 128 characters.
    pub publisher_id: String,
    /// Application identifier, up to 128 characters.
    pub application_id: String,
    /// Total number of logical blocks on the volume.
    pub volume_space_size: u32,
    /// Logical block size in bytes (almost always 2048).
    pub logical_block_size: u16,
    /// LBA of the root directory extent.
    pub root_directory_lba: u32,
    /// Size of the root directory extent in bytes.
    pub root_directory_size: u32,
}

impl PrimaryVolumeDescriptor {
    /// Read and parse the PVD via a `SectorReader`.
    pub fn read_from(reader: &mut dyn SectorReader) -> Result<Self> {
        let sector = reader.read_sector(PVD_SECTOR)?;
        Self::parse(&sector)
    }

    /// Parse a PVD from a raw 2048-byte sector slice.
    pub fn parse(sector: &[u8]) -> Result<Self> {
        if sector.len() < crate::sector_reader::SECTOR_SIZE as usize {
            return Err(OpticaldiscsError::Parse(format!(
                "sector too small: {} bytes",
                sector.len()
            )));
        }

        match sector[0] {
            PVD_TYPE => {}
            VD_SET_TERMINATOR_TYPE => {
                return Err(OpticaldiscsError::Parse(
                    "reached Volume Descriptor Set Terminator before PVD".into(),
                ))
            }
            t => {
                return Err(OpticaldiscsError::Parse(format!(
                    "unexpected volume descriptor type 0x{t:02X} (expected 0x01)"
                )))
            }
        }

        if &sector[1..6] != ISO9660_ID {
            return Err(OpticaldiscsError::Parse(
                "missing ISO 9660 identifier 'CD001'".into(),
            ));
        }

        if sector[6] != 1 {
            return Err(OpticaldiscsError::Parse(format!(
                "unsupported PVD version {}",
                sector[6]
            )));
        }

        // ECMA-119 §8.4 field offsets
        let system_id = Self::extract_str(&sector[8..40]);
        let volume_id = Self::extract_str(&sector[40..72]);
        let volume_space_size = u32::from_le_bytes(sector[80..84].try_into().unwrap());
        let logical_block_size = u16::from_le_bytes(sector[128..130].try_into().unwrap());
        let volume_set_id = Self::extract_str(&sector[190..318]);
        let publisher_id = Self::extract_str(&sector[318..446]);
        let application_id = Self::extract_str(&sector[574..702]);

        // Root Directory Record is embedded at offset 156 (34 bytes)
        let rdr = &sector[156..190];
        let root_directory_lba = u32::from_le_bytes(rdr[2..6].try_into().unwrap());
        let root_directory_size = u32::from_le_bytes(rdr[10..14].try_into().unwrap());

        Ok(Self {
            volume_id,
            system_id,
            volume_set_id,
            publisher_id,
            application_id,
            volume_space_size,
            logical_block_size,
            root_directory_lba,
            root_directory_size,
        })
    }

    /// Trim trailing spaces and NUL bytes from an ISO 9660 fixed-width string.
    fn extract_str(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes)
            .trim_end_matches([' ', '\0'])
            .to_string()
    }
}

// ── Helpers for tests and detect.rs ──────────────────────────────────────────

/// Build a minimal but structurally valid 2048-byte PVD sector for testing.
///
/// Produces a byte vector that `PrimaryVolumeDescriptor::parse()` will accept.
/// Intended for unit and integration tests in this crate and downstream crates.
#[doc(hidden)]
pub fn build_test_pvd_sector(volume_id: &str, root_lba: u32, root_size: u32) -> Vec<u8> {
    let mut s = vec![0u8; 2048];
    s[0] = PVD_TYPE;
    s[1..6].copy_from_slice(ISO9660_ID);
    s[6] = 1; // version

    // System identifier (offset 8, 32 bytes)
    let sys = b"CDROM                           ";
    s[8..40].copy_from_slice(sys);

    // Volume identifier (offset 40, 32 bytes)
    let mut vol = [b' '; 32];
    let src = volume_id.as_bytes();
    let len = src.len().min(32);
    vol[..len].copy_from_slice(&src[..len]);
    s[40..72].copy_from_slice(&vol);

    // Volume space size LE+BE (offsets 80..84, 84..88)
    let total = 100u32;
    s[80..84].copy_from_slice(&total.to_le_bytes());
    s[84..88].copy_from_slice(&total.to_be_bytes());

    // Logical block size LE+BE (offsets 128..130, 130..132)
    s[128..130].copy_from_slice(&2048u16.to_le_bytes());
    s[130..132].copy_from_slice(&2048u16.to_be_bytes());

    // Root Directory Record at offset 156
    // [0]   record length = 34
    // [1]   extended attr length = 0
    // [2..6]  LBA LE
    // [6..10] LBA BE
    // [10..14] data length LE
    // [14..18] data length BE
    // [25]  file flags = 0x02 (directory)
    // [32]  file identifier length = 1
    // [33]  file identifier = 0x00 (current)
    let rdr = &mut s[156..190];
    rdr[0] = 34;
    rdr[2..6].copy_from_slice(&root_lba.to_le_bytes());
    rdr[6..10].copy_from_slice(&root_lba.to_be_bytes());
    rdr[10..14].copy_from_slice(&root_size.to_le_bytes());
    rdr[14..18].copy_from_slice(&root_size.to_be_bytes());
    rdr[25] = 0x02;
    rdr[32] = 1;
    rdr[33] = 0x00;

    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_pvd() {
        let sector = build_test_pvd_sector("MY_DISC", 20, 2048);
        let pvd = PrimaryVolumeDescriptor::parse(&sector).unwrap();
        assert_eq!(pvd.volume_id, "MY_DISC");
        assert_eq!(pvd.system_id, "CDROM");
        assert_eq!(pvd.logical_block_size, 2048);
        assert_eq!(pvd.root_directory_lba, 20);
        assert_eq!(pvd.root_directory_size, 2048);
    }

    #[test]
    fn parse_rejects_wrong_magic() {
        let mut sector = build_test_pvd_sector("X", 20, 2048);
        sector[1..6].copy_from_slice(b"XXXXX");
        assert!(PrimaryVolumeDescriptor::parse(&sector).is_err());
    }

    #[test]
    fn parse_rejects_wrong_type() {
        let mut sector = build_test_pvd_sector("X", 20, 2048);
        sector[0] = 0x02; // Supplementary VD, not Primary
        assert!(PrimaryVolumeDescriptor::parse(&sector).is_err());
    }

    #[test]
    fn parse_rejects_terminator() {
        let mut sector = build_test_pvd_sector("X", 20, 2048);
        sector[0] = 0xFF;
        assert!(PrimaryVolumeDescriptor::parse(&sector).is_err());
    }

    #[test]
    fn extract_str_trims_spaces_and_nulls() {
        let bytes = b"HELLO   \0\0";
        assert_eq!(PrimaryVolumeDescriptor::extract_str(bytes), "HELLO");
    }

    #[test]
    fn read_from_sector_reader() {
        use crate::sector_reader::SECTOR_SIZE;
        use std::io::Cursor;

        // Build a minimal disc image: 17 empty sectors + PVD at sector 16
        let total = (PVD_SECTOR + 1) * SECTOR_SIZE;
        let mut img = vec![0u8; total as usize];
        let pvd_bytes = build_test_pvd_sector("READER_TEST", 18, 2048);
        let start = (PVD_SECTOR * SECTOR_SIZE) as usize;
        img[start..start + 2048].copy_from_slice(&pvd_bytes);

        let mut reader = CursorSectorReader(Cursor::new(img));
        let pvd = PrimaryVolumeDescriptor::read_from(&mut reader).unwrap();
        assert_eq!(pvd.volume_id, "READER_TEST");
    }

    /// Minimal SectorReader wrapper around a Cursor for testing.
    struct CursorSectorReader(std::io::Cursor<Vec<u8>>);
    impl SectorReader for CursorSectorReader {
        fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
            use std::io::{Read, Seek, SeekFrom};
            self.0
                .seek(SeekFrom::Start(lba * crate::sector_reader::SECTOR_SIZE))
                .map_err(OpticaldiscsError::Io)?;
            let mut buf = vec![0u8; crate::sector_reader::SECTOR_SIZE as usize];
            self.0.read_exact(&mut buf).map_err(OpticaldiscsError::Io)?;
            Ok(buf)
        }
    }
}
