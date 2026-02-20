//! HFS+ (Mac OS Extended) volume header parsing and catalog volume name lookup.
//!
//! The HFS+ Volume Header is located at byte offset 1024 from the start of
//! the HFS+ partition.  The volume name is not stored directly in the header;
//! it lives in the catalog B-tree as a folder thread record for the root
//! folder (CNID 2).
//!
//! See PLAN.md Phase 8.3 for implementation details.

use crate::error::{OpticaldiscsError, Result};
use crate::sector_reader::SectorReader;

/// HFS+ volume header signature "H+" (big-endian).
pub(crate) const HFSPLUS_SIGNATURE: u16 = 0x482B;
/// HFSX volume header signature "HX" (big-endian).
pub(crate) const HFSX_SIGNATURE: u16 = 0x4858;

/// Byte offset of the volume header from the start of the HFS+ partition.
const HEADER_OFFSET: u64 = 1024;

/// HFS+ catalog folder thread record type.
const HFSPLUS_FOLDER_THREAD_RECORD: i16 = 3;

/// CNID of the root parent — the virtual parent of the root folder.
const HFSPLUS_ROOT_PARENT_ID: u32 = 1;

// ── Public types ──────────────────────────────────────────────────────────────

/// HFS+ Volume Header — the root metadata structure of an HFS+ volume.
///
/// Parsed from 512 bytes at `partition_offset + 1024`.
/// Field offsets follow the HFS+ Volume Format specification.
#[derive(Debug, Clone)]
pub struct HfsPlusVolumeHeader {
    /// Signature: `0x482B` ("H+") for HFS+, `0x4858` ("HX") for HFSX.
    pub signature: u16,
    /// Format version: 4 for HFS+, 5 for HFSX.
    pub version: u16,
    /// Size of each allocation block in bytes.
    pub block_size: u32,
    /// Total number of allocation blocks on the volume.
    pub total_blocks: u32,
    /// Number of free allocation blocks.
    pub free_blocks: u32,
    /// Start block of the catalog file's first extent (in allocation blocks).
    pub catalog_start_block: u32,
    /// Number of allocation blocks in the catalog file's first extent.
    pub catalog_block_count: u32,
}

impl HfsPlusVolumeHeader {
    /// Read and parse the HFS+ volume header.
    ///
    /// Reads 512 bytes at `partition_offset + 1024` and validates the
    /// signature (`0x482B` or `0x4858`).
    ///
    /// # Errors
    ///
    /// Returns [`OpticaldiscsError::Parse`] for an invalid signature.
    /// Returns [`OpticaldiscsError::Io`] on read failure.
    pub fn read_from(reader: &mut dyn SectorReader, partition_offset: u64) -> Result<Self> {
        let hdr = reader.read_bytes(partition_offset + HEADER_OFFSET, 512)?;

        let signature = u16::from_be_bytes([hdr[0], hdr[1]]);
        if signature != HFSPLUS_SIGNATURE && signature != HFSX_SIGNATURE {
            return Err(OpticaldiscsError::Parse(format!(
                "Invalid HFS+ volume header signature: 0x{signature:04X}"
            )));
        }

        // Bytes 2–3: version (u16 BE)
        let version = u16::from_be_bytes([hdr[2], hdr[3]]);
        // Bytes 40–43: block_size (u32 BE)
        let block_size = u32::from_be_bytes([hdr[40], hdr[41], hdr[42], hdr[43]]);
        // Bytes 44–47: total_blocks (u32 BE)
        let total_blocks = u32::from_be_bytes([hdr[44], hdr[45], hdr[46], hdr[47]]);
        // Bytes 48–51: free_blocks (u32 BE)
        let free_blocks = u32::from_be_bytes([hdr[48], hdr[49], hdr[50], hdr[51]]);
        // Bytes 128–131: catalog file first extent start block (u32 BE)
        let catalog_start_block = u32::from_be_bytes([hdr[128], hdr[129], hdr[130], hdr[131]]);
        // Bytes 132–135: catalog file first extent block count (u32 BE)
        let catalog_block_count = u32::from_be_bytes([hdr[132], hdr[133], hdr[134], hdr[135]]);

        Ok(Self {
            signature,
            version,
            block_size,
            total_blocks,
            free_blocks,
            catalog_start_block,
            catalog_block_count,
        })
    }
}

// ── Public functions ──────────────────────────────────────────────────────────

/// Extract the volume name from the HFS+ catalog B-tree.
///
/// Walks the catalog B-tree leaf nodes looking for the folder thread record
/// (type 3) whose key has `parent_id == 1` (the root parent CNID).  The
/// thread record's `nodeName` field contains the volume name as UTF-16 BE.
///
/// Returns `None` if the thread record is not found (e.g. empty catalog).
///
/// # Errors
///
/// Returns [`OpticaldiscsError::Parse`] if the volume header is malformed.
/// Returns [`OpticaldiscsError::Io`] on read failure.
pub fn extract_volume_name_from_catalog(
    reader: &mut dyn SectorReader,
    partition_offset: u64,
) -> Result<Option<String>> {
    let header = HfsPlusVolumeHeader::read_from(reader, partition_offset)?;
    let catalog_offset =
        partition_offset + header.catalog_start_block as u64 * header.block_size as u64;

    // Read the B-tree header node (first 256 bytes of the catalog file).
    // The header node descriptor is 14 bytes; the B-tree header record follows.
    let btree_hdr = reader.read_bytes(catalog_offset, 256)?;

    // B-tree header node kind must be 1 (header node).
    let node_kind = btree_hdr[8] as i8;
    if node_kind != 1 {
        return Err(OpticaldiscsError::Parse(format!(
            "Expected B-tree header node (kind 1), got kind {node_kind}"
        )));
    }

    // first_leaf_node at bytes 24–27, node_size at bytes 32–33 (from node start).
    let first_leaf =
        u32::from_be_bytes([btree_hdr[24], btree_hdr[25], btree_hdr[26], btree_hdr[27]]);
    let node_size = u16::from_be_bytes([btree_hdr[32], btree_hdr[33]]) as u64;

    // Walk all leaf nodes.
    let mut current = first_leaf;
    let mut attempts = 0u32;
    const MAX: u32 = 10_000;

    while current != 0 && attempts < MAX {
        attempts += 1;
        let node_off = catalog_offset + current as u64 * node_size;
        let node = reader.read_bytes(node_off, node_size as usize)?;

        // Node descriptor: next_node at [0..4], kind at [8], num_records at [10..12]
        let next = u32::from_be_bytes([node[0], node[1], node[2], node[3]]);
        let kind = node[8] as i8;
        let num_rec = u16::from_be_bytes([node[10], node[11]]);

        if kind != -1 {
            // Not a leaf node — skip.
            current = next;
            continue;
        }

        // Record offsets are at the end of the node, in reverse order.
        // Offset for record i is at: node[node_size - 2 - i*2 .. node_size - i*2].
        let offsets_base = node_size as usize - 2;
        for i in 0..num_rec {
            let off_pos = offsets_base - i as usize * 2;
            if off_pos + 2 > node.len() {
                continue;
            }
            let rec_off = u16::from_be_bytes([node[off_pos], node[off_pos + 1]]) as usize;
            if rec_off + 10 > node.len() {
                continue;
            }

            // HFS+ catalog key: key_length (u16 BE), parent_id (u32 BE),
            //                   name_length (u16 BE), name (UTF-16 BE chars)
            let key_len = u16::from_be_bytes([node[rec_off], node[rec_off + 1]]) as usize;
            if key_len < 6 {
                continue;
            }
            let parent_id = u32::from_be_bytes([
                node[rec_off + 2],
                node[rec_off + 3],
                node[rec_off + 4],
                node[rec_off + 5],
            ]);
            let name_len = u16::from_be_bytes([node[rec_off + 6], node[rec_off + 7]]) as usize;

            // Thread records: parent_id == root parent (1), name_length == 0.
            if parent_id != HFSPLUS_ROOT_PARENT_ID || name_len != 0 {
                continue;
            }

            // Data starts immediately after the key (key_length does NOT count
            // the 2-byte key_length field itself).
            let data_off = rec_off + 2 + key_len;
            if data_off + 14 > node.len() {
                continue;
            }

            let rec_type = i16::from_be_bytes([node[data_off], node[data_off + 1]]);
            if rec_type != HFSPLUS_FOLDER_THREAD_RECORD {
                continue;
            }

            // Thread data layout:
            //   [0..2]   record type (i16)
            //   [2..8]   reserved (3 × i16)
            //   [8..12]  parent CNID (u32)
            //   [12..14] nodeName count (u16, number of UTF-16 code units)
            //   [14..]   nodeName data (UTF-16 BE)
            let vol_name_len =
                u16::from_be_bytes([node[data_off + 12], node[data_off + 13]]) as usize;
            let name_start = data_off + 14;
            let name_end = name_start + vol_name_len * 2;
            if name_end > node.len() {
                continue;
            }

            let utf16: Vec<u16> = node[name_start..name_end]
                .chunks(2)
                .filter_map(|ch| {
                    if ch.len() == 2 {
                        Some(u16::from_be_bytes([ch[0], ch[1]]))
                    } else {
                        None
                    }
                })
                .collect();

            return Ok(String::from_utf16(&utf16).ok());
        }

        current = next;
    }

    Ok(None)
}

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

    fn make_header_image(block_size: u32, catalog_start: u32, catalog_count: u32) -> Vec<u8> {
        let mut img = vec![0u8; 3 * SECTOR_SIZE as usize];
        let off = 1024usize;
        img[off] = 0x48;
        img[off + 1] = 0x2B; // "H+"
        img[off + 2..off + 4].copy_from_slice(&4u16.to_be_bytes()); // version
        img[off + 40..off + 44].copy_from_slice(&block_size.to_be_bytes());
        img[off + 44..off + 48].copy_from_slice(&1000u32.to_be_bytes()); // total_blocks
        img[off + 48..off + 52].copy_from_slice(&500u32.to_be_bytes()); // free_blocks
        img[off + 128..off + 132].copy_from_slice(&catalog_start.to_be_bytes());
        img[off + 132..off + 136].copy_from_slice(&catalog_count.to_be_bytes());
        img
    }

    #[test]
    fn read_volume_header_fields() {
        let img = make_header_image(4096, 10, 20);
        let mut reader = CursorReader(Cursor::new(img));
        let vh = HfsPlusVolumeHeader::read_from(&mut reader, 0).unwrap();
        assert_eq!(vh.signature, HFSPLUS_SIGNATURE);
        assert_eq!(vh.version, 4);
        assert_eq!(vh.block_size, 4096);
        assert_eq!(vh.total_blocks, 1000);
        assert_eq!(vh.free_blocks, 500);
        assert_eq!(vh.catalog_start_block, 10);
        assert_eq!(vh.catalog_block_count, 20);
    }

    #[test]
    fn hfsx_signature_accepted() {
        let mut img = make_header_image(4096, 0, 0);
        img[1024] = 0x48;
        img[1025] = 0x58; // "HX"
        let mut reader = CursorReader(Cursor::new(img));
        let vh = HfsPlusVolumeHeader::read_from(&mut reader, 0).unwrap();
        assert_eq!(vh.signature, HFSX_SIGNATURE);
    }

    #[test]
    fn wrong_signature_is_parse_error() {
        let mut img = make_header_image(4096, 0, 0);
        img[1024] = 0xFF; // corrupt
        let mut reader = CursorReader(Cursor::new(img));
        assert!(matches!(
            HfsPlusVolumeHeader::read_from(&mut reader, 0),
            Err(OpticaldiscsError::Parse(_))
        ));
    }
}
