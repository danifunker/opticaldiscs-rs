//! Apple Partition Map detection and partition offset lookup.
//!
//! Reads the Driver Descriptor Map (DDM) and Partition Map (PM) entries from
//! the first few 512-byte blocks of a disc image to locate the first HFS or
//! HFS+ partition.
//!
//! See PLAN.md Phase 8.1 for implementation details.

use crate::error::{OpticaldiscsError, Result};
use crate::sector_reader::SectorReader;

/// Size of an APM block (always 512 bytes, independent of disc sector size).
const APM_BLOCK_SIZE: u64 = 512;

/// Partition Map entry signature: "PM".
const PM_SIGNATURE: u16 = 0x504D;

/// Driver Descriptor Map signature: "ER".
const DDM_SIGNATURE: u16 = 0x4552;

/// Old (pre-1986) partition map signature: "TS" (`pdSigWord` in Apple's SCSI.h).
const TS_SIGNATURE: u16 = 0x5453;

/// Bytes per old-map entry: `pdStart`, `pdSize`, `pdFSID`, each a big-endian u32.
const TS_ENTRY_SIZE: usize = 12;

/// File-system ID Apple's SCSI equates give HFS in the old map (`hfsID`).
const TS_HFS_ID: &str = "TFS1";

// ── Public types ──────────────────────────────────────────────────────────────

/// A single Apple Partition Map (APM) partition entry.
///
/// Each APM disc has a sequence of these entries starting at block 1 (the
/// block immediately after the Driver Descriptor Map at block 0).
#[derive(Debug, Clone)]
pub struct PartitionEntry {
    /// Human-readable partition name (e.g. `"MacOS"`, `"Apple"`, `"Driver"`).
    pub name: String,
    /// Partition type string (e.g. `"Apple_HFS"`, `"Apple_partition_map"`).
    pub partition_type: String,
    /// Start of this partition in 512-byte APM blocks.
    pub start_block: u32,
    /// Size of this partition in 512-byte APM blocks.
    pub block_count: u32,
    /// Total number of partition map entries (taken from the first PM block).
    pub(crate) map_entries: u32,
}

impl PartitionEntry {
    /// Returns `true` if this partition contains an HFS or HFS+ filesystem.
    ///
    /// Matches any `partition_type` that begins with `"Apple_HFS"`, which
    /// covers `"Apple_HFS"`, `"Apple_HFSX"`, and `"Apple_HFS+"`.
    /// An old `TS` map names HFS by its file-system ID, `TFS1`.
    pub fn is_hfs(&self) -> bool {
        self.partition_type.starts_with("Apple_HFS") || self.partition_type == TS_HFS_ID
    }

    /// Parse a single 512-byte APM partition block.
    ///
    /// Returns `None` if the PM signature is absent or the data is too short.
    fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 80 {
            return None;
        }
        let signature = u16::from_be_bytes([data[0], data[1]]);
        if signature != PM_SIGNATURE {
            return None;
        }

        // Bytes 4-7: total number of PM entries (u32 BE)
        let map_entries = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        // Bytes 8-11: start block of this partition (u32 BE)
        let start_block = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        // Bytes 12-15: block count of this partition (u32 BE)
        let block_count = u32::from_be_bytes([data[12], data[13], data[14], data[15]]);
        // Bytes 16-47: null-terminated name string (32 bytes)
        let name = c_str(&data[16..48]);
        // Bytes 48-79: null-terminated partition type string (32 bytes)
        let partition_type = c_str(&data[48..80]);

        Some(Self {
            name,
            partition_type,
            start_block,
            block_count,
            map_entries,
        })
    }
}

// ── Public functions ──────────────────────────────────────────────────────────

/// Parse the full Apple Partition Map from `reader`.
///
/// Reads the optional Driver Descriptor Map at block 0, then reads all PM
/// entries starting at block 1.  The total number of entries is taken from
/// the first PM block's `map_entries` field.
///
/// HFS-on-CD masters commonly omit the DDM (it's only meaningful for
/// bootable Apple HDDs), leaving block 0 zero-filled.  The DDM is therefore
/// treated as advisory: we accept either a valid `"ER"` signature or any
/// non-`"PM"` block 0, and rely on the block-1 `"PM"` magic to gate parsing.
///
/// # Errors
///
/// The pre-1986 map (block 1 signature `TS`, behind an `ER` block 0) is read
/// too; see [`parse_old_partition_map`].
///
/// Returns [`OpticaldiscsError::Parse`] if block 1 does not contain a valid
/// PM entry, or [`OpticaldiscsError::Io`] on read failures.
pub fn parse_partition_map(reader: &mut dyn SectorReader) -> Result<Vec<PartitionEntry>> {
    // Block 0: Driver Descriptor Map — advisory only, see fn doc.
    let ddm = reader.read_bytes(0, APM_BLOCK_SIZE as usize)?;

    // Block 1: first PM entry — tells us the total entry count.
    let first_pm = reader.read_bytes(APM_BLOCK_SIZE, APM_BLOCK_SIZE as usize)?;
    if let Some(entries) = parse_old_partition_map(&ddm, &first_pm) {
        return Ok(entries);
    }
    let first = PartitionEntry::parse(&first_pm)
        .ok_or_else(|| OpticaldiscsError::Parse("APM block 1 is not a valid PM entry".into()))?;
    let total = first.map_entries;
    let mut entries = vec![first];

    // Blocks 2..=total: remaining PM entries.
    for block in 2..=(total as u64) {
        let pm_data = reader.read_bytes(block * APM_BLOCK_SIZE, APM_BLOCK_SIZE as usize)?;
        match PartitionEntry::parse(&pm_data) {
            Some(e) => entries.push(e),
            None => break,
        }
    }

    Ok(entries)
}

/// Parse the old Apple SCSI partition map (Inside Macintosh IV, SCSI Manager).
///
/// Block 1 holds the signature `TS` followed by 12-byte entries (`pdStart`,
/// `pdSize`, `pdFSID`; starts and sizes in 512-byte blocks), ended by an
/// all-zero entry or the end of the block. HD SC Setup wrote it until the `PM`
/// map replaced it in 1986, and some early CD-ROMs carry it too. An `ER` block 0
/// is required, since a bare two-byte `TS` is too weak a signature on its own.
/// Entries come back with an empty `name` and the four-character ID (`TFS1`
/// for HFS) as `partition_type`. Returns `None` when this is not an old map.
pub fn parse_old_partition_map(block0: &[u8], block1: &[u8]) -> Option<Vec<PartitionEntry>> {
    if block0.len() < 2 || block1.len() < 2 {
        return None;
    }
    if u16::from_be_bytes([block0[0], block0[1]]) != DDM_SIGNATURE
        || u16::from_be_bytes([block1[0], block1[1]]) != TS_SIGNATURE
    {
        return None;
    }
    let mut entries = Vec::new();
    for raw in block1[2..].as_chunks::<TS_ENTRY_SIZE>().0 {
        let start_block = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let block_count = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]);
        let fs_id = &raw[8..12];
        if start_block == 0 && block_count == 0 && fs_id == [0u8; 4] {
            break;
        }
        // A real entry sits past blocks 0 and 1 and has a size.
        if start_block < 2 || block_count == 0 {
            return None;
        }
        entries.push(PartitionEntry {
            name: String::new(),
            partition_type: c_str(fs_id),
            start_block,
            block_count,
            map_entries: 0,
        });
    }
    if entries.is_empty() {
        return None;
    }
    let n = entries.len() as u32;
    for e in &mut entries {
        e.map_entries = n;
    }
    Some(entries)
}

/// Return the byte offset of the first HFS or HFS+ partition in `reader`.
///
/// Calls [`parse_partition_map`] and multiplies the matching entry's
/// `start_block` by 512 to get the byte offset from the disc start.
///
/// # Errors
///
/// Returns [`OpticaldiscsError::NotFound`] if no HFS partition is present,
/// or propagates any error from [`parse_partition_map`].
pub fn find_hfs_partition_offset(reader: &mut dyn SectorReader) -> Result<u64> {
    let entries = parse_partition_map(reader)?;
    entries
        .iter()
        .find(|e| e.is_hfs())
        .map(|e| e.start_block as u64 * APM_BLOCK_SIZE)
        .ok_or_else(|| OpticaldiscsError::NotFound("No HFS partition found in APM".into()))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Decode a null-terminated C string from a byte slice (Latin-1 / ASCII).
fn c_str(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::OpticaldiscsError;
    use crate::sector_reader::{SectorReader, SECTOR_SIZE};
    use std::io::{Cursor, Read, Seek, SeekFrom};

    struct CursorReader(Cursor<Vec<u8>>);
    impl SectorReader for CursorReader {
        fn read_sector(&mut self, lba: u64) -> crate::error::Result<Vec<u8>> {
            self.0
                .seek(SeekFrom::Start(lba * SECTOR_SIZE))
                .map_err(OpticaldiscsError::Io)?;
            let mut buf = vec![0u8; SECTOR_SIZE as usize];
            self.0.read_exact(&mut buf).map_err(OpticaldiscsError::Io)?;
            Ok(buf)
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_ddm() -> [u8; 512] {
        let mut block = [0u8; 512];
        block[0] = 0x45; // 'E'
        block[1] = 0x52; // 'R'
        block
    }

    fn make_pm_block(
        map_entries: u32,
        start_block: u32,
        block_count: u32,
        name: &str,
        partition_type: &str,
    ) -> [u8; 512] {
        let mut block = [0u8; 512];
        block[0] = 0x50; // 'P'
        block[1] = 0x4D; // 'M'
        block[4..8].copy_from_slice(&map_entries.to_be_bytes());
        block[8..12].copy_from_slice(&start_block.to_be_bytes());
        block[12..16].copy_from_slice(&block_count.to_be_bytes());
        let nb = name.as_bytes();
        block[16..16 + nb.len().min(31)].copy_from_slice(&nb[..nb.len().min(31)]);
        let tb = partition_type.as_bytes();
        block[48..48 + tb.len().min(31)].copy_from_slice(&tb[..tb.len().min(31)]);
        block
    }

    fn make_apm_image(partitions: &[(&str, &str, u32, u32)]) -> Vec<u8> {
        // Enough sectors to hold the APM blocks plus some data
        let mut img = vec![0u8; 4 * SECTOR_SIZE as usize];
        // Block 0: DDM
        img[..512].copy_from_slice(&make_ddm());
        // Blocks 1+: PM entries
        for (i, &(name, ptype, start, count)) in partitions.iter().enumerate() {
            let block = make_pm_block(partitions.len() as u32, start, count, name, ptype);
            let offset = (i + 1) * 512;
            img[offset..offset + 512].copy_from_slice(&block);
        }
        img
    }

    // ── Unit tests ───────────────────────────────────────────────────────────

    #[test]
    fn parse_valid_pm_entry() {
        let block = make_pm_block(3, 10, 100, "MacOS", "Apple_HFS");
        let entry = PartitionEntry::parse(&block).unwrap();
        assert_eq!(entry.map_entries, 3);
        assert_eq!(entry.start_block, 10);
        assert_eq!(entry.block_count, 100);
        assert_eq!(entry.name, "MacOS");
        assert_eq!(entry.partition_type, "Apple_HFS");
    }

    #[test]
    fn parse_wrong_signature_returns_none() {
        let mut block = [0u8; 512];
        block[0] = 0x12;
        block[1] = 0x34;
        assert!(PartitionEntry::parse(&block).is_none());
    }

    #[test]
    fn parse_short_data_returns_none() {
        assert!(PartitionEntry::parse(&[0u8; 16]).is_none());
    }

    #[test]
    fn is_hfs_various_types() {
        let make = |pt: &str| PartitionEntry {
            name: String::new(),
            partition_type: pt.to_string(),
            start_block: 0,
            block_count: 0,
            map_entries: 0,
        };
        assert!(make("Apple_HFS").is_hfs());
        assert!(make("Apple_HFSX").is_hfs());
        assert!(make("Apple_HFS+").is_hfs());
        assert!(!make("Apple_partition_map").is_hfs());
        assert!(!make("Apple_Free").is_hfs());
        assert!(!make("Linux_Ext2").is_hfs());
    }

    #[test]
    fn c_str_trims_at_null() {
        assert_eq!(c_str(b"Hello\0World"), "Hello");
    }

    #[test]
    fn c_str_no_null() {
        assert_eq!(c_str(b"NoNull"), "NoNull");
    }

    #[test]
    fn parse_partition_map_basic() {
        let img = make_apm_image(&[
            ("Partition_Map", "Apple_partition_map", 1, 2),
            ("MacOS", "Apple_HFS", 3, 200),
        ]);
        let mut reader = CursorReader(Cursor::new(img));
        let entries = parse_partition_map(&mut reader).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].partition_type, "Apple_partition_map");
        assert_eq!(entries[1].partition_type, "Apple_HFS");
        assert_eq!(entries[1].start_block, 3);
    }

    #[test]
    fn parse_partition_map_empty_fails() {
        // No DDM and no PM at block 1 — block 1 PM check must reject.
        let img = vec![0u8; 4 * SECTOR_SIZE as usize];
        let mut reader = CursorReader(Cursor::new(img));
        assert!(matches!(
            parse_partition_map(&mut reader),
            Err(OpticaldiscsError::Parse(_))
        ));
    }

    #[test]
    fn parse_partition_map_zero_ddm_with_valid_pm() {
        // HFS-on-CD masters often leave block 0 zero-filled (no DDM).
        // Parsing must still succeed when block 1 has a valid PM entry.
        let mut img = vec![0u8; 4 * SECTOR_SIZE as usize];
        // Block 0 stays zero.
        img[512..1024].copy_from_slice(&make_pm_block(
            2,
            1,
            2,
            "Partition_Map",
            "Apple_partition_map",
        ));
        img[1024..1536].copy_from_slice(&make_pm_block(2, 8, 200, "MacOS", "Apple_HFS"));
        let mut reader = CursorReader(Cursor::new(img));
        let entries = parse_partition_map(&mut reader).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].partition_type, "Apple_HFS");
        assert_eq!(entries[1].start_block, 8);
    }

    #[test]
    fn find_hfs_partition_offset_returns_byte_offset() {
        let img = make_apm_image(&[
            ("Partition_Map", "Apple_partition_map", 1, 2),
            ("MacOS", "Apple_HFS", 4, 100),
        ]);
        let mut reader = CursorReader(Cursor::new(img));
        let offset = find_hfs_partition_offset(&mut reader).unwrap();
        // start_block 4 × 512 = 2048
        assert_eq!(offset, 4 * 512);
    }

    /// Block 0 `ER`, block 1 the old `TS` map: the layout of The Manhole CD.
    fn make_ts_image(entries: &[(u32, u32, &[u8; 4])]) -> Vec<u8> {
        let mut img = vec![0u8; 4 * SECTOR_SIZE as usize];
        img[..512].copy_from_slice(&make_ddm());
        img[512] = 0x54; // 'T'
        img[513] = 0x53; // 'S'
        for (i, &(start, size, id)) in entries.iter().enumerate() {
            let o = 514 + i * TS_ENTRY_SIZE;
            img[o..o + 4].copy_from_slice(&start.to_be_bytes());
            img[o + 4..o + 8].copy_from_slice(&size.to_be_bytes());
            img[o + 8..o + 12].copy_from_slice(id);
        }
        img
    }

    #[test]
    fn old_ts_map_lists_its_entries() {
        let img = make_ts_image(&[(6, 0x1ef3a, b"TFS1"), (0x1ef40, 100, b"XFS9")]);
        let mut reader = CursorReader(Cursor::new(img));
        let entries = parse_partition_map(&mut reader).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].partition_type, "TFS1");
        assert_eq!(entries[0].start_block, 6);
        assert_eq!(entries[0].block_count, 0x1ef3a);
        assert!(entries[0].is_hfs());
        assert!(!entries[1].is_hfs());
        assert_eq!(entries[1].map_entries, 2);
    }

    #[test]
    fn old_ts_map_hfs_offset_is_start_times_512() {
        let img = make_ts_image(&[(6, 0x1ef3a, b"TFS1")]);
        let mut reader = CursorReader(Cursor::new(img));
        assert_eq!(find_hfs_partition_offset(&mut reader).unwrap(), 6 * 512);
    }

    #[test]
    fn old_ts_map_needs_er_block0_and_sane_entries() {
        let mut no_ddm = make_ts_image(&[(6, 100, b"TFS1")]);
        no_ddm[..2].copy_from_slice(&[0, 0]);
        assert!(parse_partition_map(&mut CursorReader(Cursor::new(no_ddm))).is_err());
        let empty = make_ts_image(&[]);
        assert!(parse_partition_map(&mut CursorReader(Cursor::new(empty))).is_err());
        let overlaps_map = make_ts_image(&[(1, 100, b"TFS1")]);
        assert!(parse_partition_map(&mut CursorReader(Cursor::new(overlaps_map))).is_err());
    }

    #[test]
    fn find_hfs_partition_offset_no_hfs_returns_not_found() {
        let img = make_apm_image(&[("Partition_Map", "Apple_partition_map", 1, 2)]);
        let mut reader = CursorReader(Cursor::new(img));
        assert!(matches!(
            find_hfs_partition_offset(&mut reader),
            Err(OpticaldiscsError::NotFound(_))
        ));
    }
}
