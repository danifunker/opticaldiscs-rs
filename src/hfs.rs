//! HFS (Hierarchical File System) Master Directory Block parsing.
//!
//! The Master Directory Block (MDB) is the root metadata structure for an
//! HFS volume.  It is located at byte offset 1024 from the start of the
//! HFS partition (or from byte 0 for images without an Apple Partition Map).
//!
//! See PLAN.md Phase 8.2 for implementation details.

use crate::error::{OpticaldiscsError, Result};
use crate::sector_reader::SectorReader;

/// HFS Master Directory Block signature: "BD" (big-endian `0x4244`).
const HFS_MDB_SIGNATURE: u16 = 0x4244;

/// Byte offset of the MDB from the start of the HFS partition.
const MDB_OFFSET: u64 = 1024;

// ── Public types ──────────────────────────────────────────────────────────────

/// HFS Master Directory Block — the root metadata structure of an HFS volume.
///
/// Parsed from the 512-byte block at `partition_offset + 1024`.
/// Field offsets follow the HFS Volume Format specification (Inside Macintosh:
/// Files, chapter 2).
#[derive(Debug, Clone)]
pub struct MasterDirectoryBlock {
    /// Volume creation timestamp (seconds since the Mac OS epoch: 1904-01-01).
    pub creation_date: u32,
    /// Number of files in the root directory.
    pub file_count: u16,
    /// Size of each allocation block in bytes.
    pub alloc_block_size: u32,
    /// First allocation block sector number (in 512-byte sectors from partition start).
    pub alloc_block_start: u16,
    /// Volume name (Pascal string, up to 27 Mac Roman characters).
    pub volume_name: String,
    /// Start block of the catalog file's first extent (in allocation blocks).
    pub catalog_start_block: u16,
    /// Number of allocation blocks in the catalog file's first extent.
    pub catalog_block_count: u16,
    /// Catalog file logical size in bytes (`drCTFlSize`).
    pub catalog_file_size: u32,
    /// All three catalog file extents (`drCTExtRec`).
    ///
    /// Each tuple is `(start_block, block_count)` in allocation blocks.
    /// Unused extents are `(0, 0)`.
    pub catalog_extents: [(u16, u16); 3],
}

impl MasterDirectoryBlock {
    /// Read and parse the HFS Master Directory Block.
    ///
    /// Reads 256 bytes at `partition_offset + 1024` and validates the
    /// `0x4244` ("BD") signature.
    ///
    /// # Errors
    ///
    /// Returns [`OpticaldiscsError::Parse`] if the signature is wrong.
    /// Returns [`OpticaldiscsError::Io`] if the disc cannot be read.
    pub fn read_from(reader: &mut dyn SectorReader, partition_offset: u64) -> Result<Self> {
        let data = reader.read_bytes(partition_offset + MDB_OFFSET, 256)?;

        let signature = u16::from_be_bytes([data[0], data[1]]);
        if signature != HFS_MDB_SIGNATURE {
            return Err(OpticaldiscsError::Parse(format!(
                "Invalid HFS MDB signature: expected 0x{HFS_MDB_SIGNATURE:04X}, \
                 got 0x{signature:04X}"
            )));
        }

        // Bytes 2–5: drCrDate — volume creation date (u32 BE, Mac epoch).
        let creation_date = u32::from_be_bytes([data[2], data[3], data[4], data[5]]);

        // Bytes 10–11: drNmFls — number of files in the root directory (u16 BE).
        let file_count = u16::from_be_bytes([data[10], data[11]]);

        // Bytes 20–23: drAlBlkSiz — allocation block size in bytes (u32 BE).
        let alloc_block_size = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);

        // Bytes 28–29: drAlBlSt — first allocation block sector number (u16 BE).
        let alloc_block_start = u16::from_be_bytes([data[28], data[29]]);

        // Bytes 36+: drVN — Pascal string volume name.
        //   Byte 36: length (u8), bytes 37–63: up to 27 Mac Roman characters.
        let name_len = data[36] as usize;
        let name_end = (37 + name_len).min(64);
        let volume_name = mac_roman_to_string(&data[37..name_end]);

        // Bytes 146–149: drCTFlSize — catalog file logical size (u32 BE).
        let catalog_file_size = u32::from_be_bytes([data[146], data[147], data[148], data[149]]);

        // Bytes 150–161: drCTExtRec — three catalog extents (start, count) × 3.
        let mut catalog_extents = [(0u16, 0u16); 3];
        for (i, ext) in catalog_extents.iter_mut().enumerate() {
            let base = 150 + i * 4;
            ext.0 = u16::from_be_bytes([data[base], data[base + 1]]);
            ext.1 = u16::from_be_bytes([data[base + 2], data[base + 3]]);
        }
        let (catalog_start_block, catalog_block_count) = catalog_extents[0];

        Ok(Self {
            creation_date,
            file_count,
            alloc_block_size,
            alloc_block_start,
            volume_name,
            catalog_start_block,
            catalog_block_count,
            catalog_file_size,
            catalog_extents,
        })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Decode a byte slice that may contain Mac Roman characters into a `String`.
///
/// Bytes in the ASCII range (< 0x80) are passed through as-is.  Bytes ≥ 0x80
/// are looked up in the Mac OS Roman → Unicode table.
pub(crate) fn mac_roman_to_string(data: &[u8]) -> String {
    data.iter().map(|&b| mac_roman_char(b)).collect()
}

/// Map a single Mac Roman byte to its Unicode `char`.
fn mac_roman_char(b: u8) -> char {
    if b < 0x80 {
        return b as char;
    }
    // Mac OS Roman upper half (0x80–0xFF) → Unicode.
    MAC_ROMAN_TABLE[(b - 0x80) as usize]
}

/// Mac OS Roman (upper half) → Unicode character table.
///
/// Index 0 corresponds to byte value `0x80`, index 127 to `0xFF`.
static MAC_ROMAN_TABLE: [char; 128] = [
    // 0x80–0x87
    '\u{00C4}', '\u{00C5}', '\u{00C7}', '\u{00C9}', '\u{00D1}', '\u{00D6}', '\u{00DC}', '\u{00E1}',
    // 0x88–0x8F
    '\u{00E0}', '\u{00E2}', '\u{00E4}', '\u{00E3}', '\u{00E5}', '\u{00E7}', '\u{00E9}', '\u{00E8}',
    // 0x90–0x97
    '\u{00EA}', '\u{00EB}', '\u{00ED}', '\u{00EC}', '\u{00EE}', '\u{00EF}', '\u{00F1}', '\u{00F3}',
    // 0x98–0x9F
    '\u{00F2}', '\u{00F4}', '\u{00F6}', '\u{00FA}', '\u{00F9}', '\u{00FB}', '\u{00FC}', '\u{2020}',
    // 0xA0–0xA7
    '\u{00B0}', '\u{00A2}', '\u{00A3}', '\u{00A7}', '\u{2022}', '\u{00B6}', '\u{00DF}', '\u{00AE}',
    // 0xA8–0xAF
    '\u{00A9}', '\u{2122}', '\u{00B4}', '\u{00A8}', '\u{2260}', '\u{00C6}', '\u{00D8}', '\u{221E}',
    // 0xB0–0xB7
    '\u{00B1}', '\u{2264}', '\u{2265}', '\u{00A5}', '\u{00B5}', '\u{2202}', '\u{2211}', '\u{220F}',
    // 0xB8–0xBF
    '\u{03C0}', '\u{222B}', '\u{00AA}', '\u{00BA}', '\u{03A9}', '\u{00E6}', '\u{00F8}', '\u{00BF}',
    // 0xC0–0xC7
    '\u{00A1}', '\u{00AC}', '\u{221A}', '\u{0192}', '\u{2248}', '\u{2206}', '\u{00AB}', '\u{00BB}',
    // 0xC8–0xCF
    '\u{2026}', '\u{00A0}', '\u{00C0}', '\u{00C3}', '\u{00D5}', '\u{0152}', '\u{0153}', '\u{2013}',
    // 0xD0–0xD7
    '\u{2014}', '\u{201C}', '\u{201D}', '\u{2018}', '\u{2019}', '\u{00F7}', '\u{25CA}', '\u{00FF}',
    // 0xD8–0xDF
    '\u{0178}', '\u{2044}', '\u{20AC}', '\u{2039}', '\u{203A}', '\u{FB01}', '\u{FB02}', '\u{2021}',
    // 0xE0–0xE7
    '\u{00B7}', '\u{201A}', '\u{201E}', '\u{2030}', '\u{00C2}', '\u{00CA}', '\u{00C1}', '\u{00CB}',
    // 0xE8–0xEF
    '\u{00C8}', '\u{00CD}', '\u{00CE}', '\u{00CF}', '\u{00CC}', '\u{00D3}', '\u{00D4}', '\u{F8FF}',
    // 0xF0–0xF7
    '\u{00D2}', '\u{00DA}', '\u{00DB}', '\u{00D9}', '\u{0131}', '\u{02C6}', '\u{02DC}', '\u{00AF}',
    // 0xF8–0xFF
    '\u{02D8}', '\u{02D9}', '\u{02DA}', '\u{00B8}', '\u{02DD}', '\u{02DB}', '\u{02C7}', '\u{FFFD}',
];

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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

    fn make_mdb_image(
        name: &str,
        alloc_block_size: u32,
        alloc_block_start: u16,
        catalog_start: u16,
        catalog_count: u16,
    ) -> Vec<u8> {
        // Two 2048-byte sectors is enough to contain byte 1024 + 256 bytes.
        let mut img = vec![0u8; 2 * SECTOR_SIZE as usize];
        let off = 1024usize;
        // Signature: "BD"
        img[off] = 0x42;
        img[off + 1] = 0x44;
        // Creation date (bytes 2–5)
        img[off + 2..off + 6].copy_from_slice(&0x8000_0000u32.to_be_bytes());
        // file_count (bytes 10–11)
        img[off + 10..off + 12].copy_from_slice(&5u16.to_be_bytes());
        // alloc_block_size (bytes 20–23)
        img[off + 20..off + 24].copy_from_slice(&alloc_block_size.to_be_bytes());
        // alloc_block_start (bytes 28–29)
        img[off + 28..off + 30].copy_from_slice(&alloc_block_start.to_be_bytes());
        // Pascal string volume name (byte 36 = length, bytes 37+ = name)
        let nb = name.as_bytes();
        img[off + 36] = nb.len() as u8;
        img[off + 37..off + 37 + nb.len()].copy_from_slice(nb);
        // Catalog file size (bytes 146–149) — caller supplies via `catalog_count` × block_size
        let cat_size = catalog_count as u32 * alloc_block_size;
        img[off + 146..off + 150].copy_from_slice(&cat_size.to_be_bytes());
        // Catalog extent (bytes 150–153)
        img[off + 150..off + 152].copy_from_slice(&catalog_start.to_be_bytes());
        img[off + 152..off + 154].copy_from_slice(&catalog_count.to_be_bytes());
        img
    }

    #[test]
    fn read_mdb_all_fields() {
        let img = make_mdb_image("TestVol", 4096, 3, 5, 10);
        let mut reader = CursorReader(Cursor::new(img));
        let mdb = MasterDirectoryBlock::read_from(&mut reader, 0).unwrap();
        assert_eq!(mdb.volume_name, "TestVol");
        assert_eq!(mdb.alloc_block_size, 4096);
        assert_eq!(mdb.alloc_block_start, 3);
        assert_eq!(mdb.catalog_start_block, 5);
        assert_eq!(mdb.catalog_block_count, 10);
        assert_eq!(mdb.catalog_extents[0], (5, 10));
        assert_eq!(mdb.catalog_extents[1], (0, 0));
        assert_eq!(mdb.catalog_file_size, 10 * 4096);
        assert_eq!(mdb.file_count, 5);
        assert_eq!(mdb.creation_date, 0x8000_0000);
    }

    #[test]
    fn wrong_signature_is_parse_error() {
        let mut img = make_mdb_image("X", 512, 0, 0, 0);
        img[1024] = 0xFF; // corrupt signature
        let mut reader = CursorReader(Cursor::new(img));
        assert!(matches!(
            MasterDirectoryBlock::read_from(&mut reader, 0),
            Err(OpticaldiscsError::Parse(_))
        ));
    }

    #[test]
    fn mac_roman_ascii_passthrough() {
        assert_eq!(mac_roman_to_string(b"Hello"), "Hello");
    }

    #[test]
    fn mac_roman_upper_range() {
        // 0x80 → Ä (U+00C4)
        assert_eq!(mac_roman_char(0x80), 'Ä');
        // 0xA9 → ™ (U+2122)
        assert_eq!(mac_roman_char(0xA9), '™');
    }
}
