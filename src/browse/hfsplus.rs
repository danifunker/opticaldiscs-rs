//! HFS+ (Mac OS Extended) filesystem browser.
//!
//! Provides [`HfsPlusFilesystem`], which implements the [`Filesystem`] trait
//! for browsing directories and reading files on HFS+-formatted disc images.
//!
//! The implementation walks the HFS+ catalog B-tree by following leaf-node
//! linked-list pointers, decoding HFS+ catalog key / record pairs.
//!
//! See PLAN.md Phase 8.5 for implementation details.

use super::entry::{EntryType, FileEntry};
use super::filesystem::{Filesystem, FilesystemError};
use crate::hfsplus::{extract_volume_name_from_catalog, HfsPlusVolumeHeader};
use crate::sector_reader::SectorReader;

// ── HFS+ catalog record types ─────────────────────────────────────────────────

const HFSPLUS_FOLDER_RECORD: i16 = 1;
const HFSPLUS_FILE_RECORD: i16 = 2;

/// Root folder CNID (always 2 in HFS+).
const HFSPLUS_ROOT_FOLDER_ID: u32 = 2;

// ── Public type ───────────────────────────────────────────────────────────────

/// HFS+ filesystem browser.
///
/// Created by [`open_disc_filesystem`][crate::browse::open_disc_filesystem]
/// when the detected filesystem type is [`FilesystemType::HfsPlus`][crate::formats::FilesystemType::HfsPlus].
pub struct HfsPlusFilesystem {
    reader: Box<dyn SectorReader>,
    /// Byte offset of the HFS+ partition from the disc start.
    partition_offset: u64,
    /// Allocation block size in bytes.
    block_size: u32,
    /// Start block of the catalog file (in allocation blocks).
    catalog_start_block: u32,
    /// B-tree node size in bytes.
    node_size: u16,
    /// Node number of the first leaf in the catalog B-tree.
    first_leaf_node: u32,
    /// Volume name (from the B-tree folder thread record).
    volume_name: String,
}

impl HfsPlusFilesystem {
    /// Open an HFS+ filesystem.
    ///
    /// Reads the volume header at `partition_offset + 1024`, reads the
    /// catalog B-tree header to locate the first leaf node, and extracts
    /// the volume name from the B-tree thread record.
    ///
    /// # Errors
    ///
    /// Returns [`FilesystemError::Parse`] if the volume header or B-tree
    /// header is malformed, or [`FilesystemError::Io`] on read failure.
    pub fn new(
        mut reader: Box<dyn SectorReader>,
        partition_offset: u64,
    ) -> Result<Self, FilesystemError> {
        let header = HfsPlusVolumeHeader::read_from(reader.as_mut(), partition_offset)
            .map_err(hfsplus_err)?;

        let catalog_offset =
            partition_offset + header.catalog_start_block as u64 * header.block_size as u64;

        // B-tree header node (first 256 bytes of the catalog file).
        let btree_hdr = reader
            .read_bytes(catalog_offset, 256)
            .map_err(hfsplus_err)?;

        let node_kind = btree_hdr[8] as i8;
        if node_kind != 1 {
            return Err(FilesystemError::Parse(format!(
                "Expected B-tree header node (kind 1), got {node_kind}"
            )));
        }

        // first_leaf_node at bytes 24–27, node_size at bytes 32–33.
        let first_leaf_node =
            u32::from_be_bytes([btree_hdr[24], btree_hdr[25], btree_hdr[26], btree_hdr[27]]);
        let node_size = u16::from_be_bytes([btree_hdr[32], btree_hdr[33]]);

        // Extract the volume name from the B-tree folder thread record.
        let volume_name = extract_volume_name_from_catalog(reader.as_mut(), partition_offset)
            .unwrap_or(None)
            .unwrap_or_else(|| "HFS+ Volume".to_string());

        Ok(Self {
            reader,
            partition_offset,
            block_size: header.block_size,
            catalog_start_block: header.catalog_start_block,
            node_size,
            first_leaf_node,
            volume_name,
        })
    }

    // ── B-tree helpers ────────────────────────────────────────────────────────

    /// Byte offset of the start of the catalog file.
    fn catalog_offset(&self) -> u64 {
        self.partition_offset + self.catalog_start_block as u64 * self.block_size as u64
    }

    /// Read a single B-tree node by node number.
    fn read_node(&mut self, node_num: u32) -> Result<Vec<u8>, FilesystemError> {
        let offset = self.catalog_offset() + node_num as u64 * self.node_size as u64;
        self.reader
            .read_bytes(offset, self.node_size as usize)
            .map_err(hfsplus_err)
    }

    // ── Directory listing ─────────────────────────────────────────────────────

    /// Walk all leaf nodes and collect entries whose key `parent_id` matches
    /// `parent_cnid`.
    fn list_by_cnid(
        &mut self,
        parent_cnid: u32,
        parent_path: &str,
    ) -> Result<Vec<FileEntry>, FilesystemError> {
        let mut entries = Vec::new();
        let mut current = self.first_leaf_node;
        let mut attempts = 0u32;
        const MAX: u32 = 10_000;

        while current != 0 && attempts < MAX {
            attempts += 1;
            let node = self.read_node(current)?;

            let next = u32::from_be_bytes([node[0], node[1], node[2], node[3]]);
            let kind = node[8] as i8;
            let num_rec = u16::from_be_bytes([node[10], node[11]]);

            if kind != -1 {
                current = next;
                continue;
            }

            process_leaf_node(
                &node,
                self.node_size as usize,
                num_rec,
                parent_cnid,
                parent_path,
                &mut entries,
            );
            current = next;
        }

        // Directories first, then alphabetical (case-insensitive).
        entries.sort_by(|a, b| match (a.entry_type, b.entry_type) {
            (EntryType::Directory, EntryType::File) => std::cmp::Ordering::Less,
            (EntryType::File, EntryType::Directory) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });

        Ok(entries)
    }

    // ── File reading ──────────────────────────────────────────────────────────

    /// Find the catalog file record for a given CNID and return its fork data.
    fn find_file_fork(&mut self, cnid: u32) -> Result<HfsPlusForkData, FilesystemError> {
        let mut current = self.first_leaf_node;
        let mut attempts = 0u32;
        const MAX: u32 = 10_000;

        while current != 0 && attempts < MAX {
            attempts += 1;
            let node = self.read_node(current)?;

            let next = u32::from_be_bytes([node[0], node[1], node[2], node[3]]);
            let kind = node[8] as i8;
            let num_rec = u16::from_be_bytes([node[10], node[11]]);

            if kind != -1 {
                current = next;
                continue;
            }

            if let Some(fork) = search_node_for_file(&node, self.node_size as usize, num_rec, cnid)
            {
                return Ok(fork);
            }

            current = next;
        }

        Err(FilesystemError::NotFound(format!(
            "File CNID {cnid} not found"
        )))
    }

    /// Read a byte range from an HFS+ fork.
    fn read_fork_range(
        &mut self,
        fork: &HfsPlusForkData,
        range_offset: u64,
        range_length: usize,
    ) -> Result<Vec<u8>, FilesystemError> {
        let end = (range_offset + range_length as u64).min(fork.logical_size);
        if range_offset >= end {
            return Ok(Vec::new());
        }

        let mut result = Vec::with_capacity((end - range_offset) as usize);
        let mut logical_pos: u64 = 0;

        for ext in &fork.extents {
            if ext.block_count == 0 {
                break;
            }
            let ext_size = ext.block_count as u64 * self.block_size as u64;
            let ext_end = logical_pos + ext_size;

            if ext_end <= range_offset {
                logical_pos = ext_end;
                continue;
            }
            if logical_pos >= end {
                break;
            }

            let read_start = range_offset.max(logical_pos);
            let read_end = end.min(ext_end);
            let read_len = (read_end - read_start) as usize;
            let offset_in_ext = read_start - logical_pos;

            let phys_off = self.partition_offset
                + ext.start_block as u64 * self.block_size as u64
                + offset_in_ext;

            let chunk = self
                .reader
                .read_bytes(phys_off, read_len)
                .map_err(hfsplus_err)?;
            result.extend_from_slice(&chunk);

            logical_pos = ext_end;
        }

        Ok(result)
    }
}

impl Filesystem for HfsPlusFilesystem {
    fn root(&mut self) -> Result<FileEntry, FilesystemError> {
        Ok(FileEntry::root(HFSPLUS_ROOT_FOLDER_ID as u64))
    }

    fn list_directory(&mut self, entry: &FileEntry) -> Result<Vec<FileEntry>, FilesystemError> {
        if entry.entry_type != EntryType::Directory {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        let cnid = entry.location as u32;
        self.list_by_cnid(cnid, &entry.path)
    }

    fn read_file(&mut self, entry: &FileEntry) -> Result<Vec<u8>, FilesystemError> {
        if entry.entry_type != EntryType::File {
            return Err(FilesystemError::NotADirectory(format!(
                "{} is not a file",
                entry.path
            )));
        }
        let fork = self.find_file_fork(entry.location as u32)?;
        if fork.extents.is_empty() {
            return Ok(Vec::new());
        }
        self.read_fork_range(&fork, 0, fork.logical_size as usize)
    }

    fn read_file_range(
        &mut self,
        entry: &FileEntry,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, FilesystemError> {
        if entry.entry_type != EntryType::File {
            return Err(FilesystemError::NotADirectory(format!(
                "{} is not a file",
                entry.path
            )));
        }
        let fork = self.find_file_fork(entry.location as u32)?;
        if fork.extents.is_empty() {
            return Ok(Vec::new());
        }
        let actual_len = length.min(fork.logical_size.saturating_sub(offset) as usize);
        if actual_len == 0 {
            return Ok(Vec::new());
        }
        self.read_fork_range(&fork, offset, actual_len)
    }

    fn volume_name(&self) -> Option<&str> {
        if self.volume_name.is_empty() {
            None
        } else {
            Some(&self.volume_name)
        }
    }
}

// ── Internal B-tree parsing helpers ──────────────────────────────────────────

/// An HFS+ extent descriptor (start block and block count in allocation blocks).
#[derive(Debug, Clone, Copy)]
struct HfsPlusExtent {
    start_block: u32,
    block_count: u32,
}

/// Data fork information for an HFS+ file.
#[derive(Debug, Clone)]
struct HfsPlusForkData {
    logical_size: u64,
    extents: Vec<HfsPlusExtent>,
}

/// Process all records in a leaf node, collecting entries whose key
/// `parent_id` matches `parent_cnid`.
fn process_leaf_node(
    node: &[u8],
    node_size: usize,
    num_rec: u16,
    parent_cnid: u32,
    parent_path: &str,
    entries: &mut Vec<FileEntry>,
) {
    let offsets_base = node_size - 2;

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
        //                   name_length (u16 BE), name (UTF-16 BE)
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
        if parent_id != parent_cnid {
            continue;
        }

        let name_len = u16::from_be_bytes([node[rec_off + 6], node[rec_off + 7]]) as usize;
        if name_len == 0 {
            // Thread record — skip.
            continue;
        }

        let name_start = rec_off + 8;
        let name_end = name_start + name_len * 2;
        if name_end > node.len() {
            continue;
        }
        let name = decode_utf16_be(&node[name_start..name_end]);
        if name.is_empty() {
            continue;
        }

        // Data starts immediately after the key (the 2-byte key_length field
        // is not included in key_len itself).
        let data_off = rec_off + 2 + key_len;
        if data_off + 4 > node.len() {
            continue;
        }

        let rec_type = i16::from_be_bytes([node[data_off], node[data_off + 1]]);

        let path = if parent_path == "/" {
            format!("/{name}")
        } else {
            format!("{parent_path}/{name}")
        };

        match rec_type {
            HFSPLUS_FOLDER_RECORD => {
                // Folder CNID at data_off + 8 (u32 BE).
                if data_off + 12 > node.len() {
                    continue;
                }
                let cnid = u32::from_be_bytes([
                    node[data_off + 8],
                    node[data_off + 9],
                    node[data_off + 10],
                    node[data_off + 11],
                ]);
                entries.push(FileEntry::new_directory(name, path, cnid as u64));
            }
            HFSPLUS_FILE_RECORD => {
                // CNID at data_off + 8, data fork logical size at data_off + 88.
                if data_off + 96 > node.len() {
                    continue;
                }
                let cnid = u32::from_be_bytes([
                    node[data_off + 8],
                    node[data_off + 9],
                    node[data_off + 10],
                    node[data_off + 11],
                ]);
                let data_size = u64::from_be_bytes([
                    node[data_off + 88],
                    node[data_off + 89],
                    node[data_off + 90],
                    node[data_off + 91],
                    node[data_off + 92],
                    node[data_off + 93],
                    node[data_off + 94],
                    node[data_off + 95],
                ]);
                entries.push(FileEntry::new_file(name, path, data_size, cnid as u64));
            }
            _ => {}
        }
    }
}

/// Search a leaf node for a file record with a matching CNID, returning its
/// data fork information.
fn search_node_for_file(
    node: &[u8],
    node_size: usize,
    num_rec: u16,
    target_cnid: u32,
) -> Option<HfsPlusForkData> {
    let offsets_base = node_size - 2;

    for i in 0..num_rec {
        let off_pos = offsets_base - i as usize * 2;
        if off_pos + 2 > node.len() {
            continue;
        }
        let rec_off = u16::from_be_bytes([node[off_pos], node[off_pos + 1]]) as usize;
        if rec_off + 10 > node.len() {
            continue;
        }

        let key_len = u16::from_be_bytes([node[rec_off], node[rec_off + 1]]) as usize;
        if key_len < 6 {
            continue;
        }

        let data_off = rec_off + 2 + key_len;
        // Minimum: type(2) + flags(2) + valence(4) + cnid(4) + ... + fork(80) = ≥104 bytes
        if data_off + 104 > node.len() {
            continue;
        }

        let rec_type = i16::from_be_bytes([node[data_off], node[data_off + 1]]);
        if rec_type != HFSPLUS_FILE_RECORD {
            continue;
        }

        let cnid = u32::from_be_bytes([
            node[data_off + 8],
            node[data_off + 9],
            node[data_off + 10],
            node[data_off + 11],
        ]);
        if cnid != target_cnid {
            continue;
        }

        // HFSPlusForkData at data_off + 88 (data fork):
        //   [0..8]   logical_size (u64 BE)
        //   [8..12]  clump_size (u32 BE)
        //   [12..16] total_blocks (u32 BE)
        //   [16..]   8 × HFSPlusExtentDescriptor (each 8 bytes: start_block u32 + block_count u32)
        let fork_off = data_off + 88;
        let logical_size = u64::from_be_bytes([
            node[fork_off],
            node[fork_off + 1],
            node[fork_off + 2],
            node[fork_off + 3],
            node[fork_off + 4],
            node[fork_off + 5],
            node[fork_off + 6],
            node[fork_off + 7],
        ]);

        let mut extents = Vec::new();
        for j in 0..8usize {
            let ext_off = fork_off + 16 + j * 8;
            if ext_off + 8 > node.len() {
                break;
            }
            let start_block = u32::from_be_bytes([
                node[ext_off],
                node[ext_off + 1],
                node[ext_off + 2],
                node[ext_off + 3],
            ]);
            let block_count = u32::from_be_bytes([
                node[ext_off + 4],
                node[ext_off + 5],
                node[ext_off + 6],
                node[ext_off + 7],
            ]);
            if block_count == 0 {
                break;
            }
            extents.push(HfsPlusExtent {
                start_block,
                block_count,
            });
        }

        return Some(HfsPlusForkData {
            logical_size,
            extents,
        });
    }

    None
}

/// Decode a UTF-16 BE byte slice into a `String`.
fn decode_utf16_be(bytes: &[u8]) -> String {
    let utf16: Vec<u16> = bytes
        .chunks(2)
        .filter_map(|ch| {
            if ch.len() == 2 {
                Some(u16::from_be_bytes([ch[0], ch[1]]))
            } else {
                None
            }
        })
        .collect();
    String::from_utf16(&utf16).unwrap_or_default()
}

// ── Error conversion ──────────────────────────────────────────────────────────

fn hfsplus_err(e: crate::error::OpticaldiscsError) -> FilesystemError {
    match e {
        crate::error::OpticaldiscsError::Io(io) => FilesystemError::Io(io),
        e => FilesystemError::Parse(e.to_string()),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_utf16_be_ascii() {
        // "AB" as UTF-16 BE
        let bytes = [0x00, 0x41, 0x00, 0x42];
        assert_eq!(decode_utf16_be(&bytes), "AB");
    }

    #[test]
    fn decode_utf16_be_empty() {
        assert_eq!(decode_utf16_be(&[]), "");
    }

    #[test]
    fn process_leaf_node_empty() {
        let node = vec![0u8; 512];
        let mut entries = Vec::new();
        process_leaf_node(&node, 512, 0, 2, "/", &mut entries);
        assert!(entries.is_empty());
    }

    #[test]
    fn process_leaf_node_folder_record() {
        let mut node = vec![0u8; 512];
        let rec_off: u16 = 14;
        // Record offset at end of node
        node[510] = (rec_off >> 8) as u8;
        node[511] = (rec_off & 0xFF) as u8;

        // HFS+ key: key_length (u16), parent_id (u32), name_length (u16), name (UTF-16 BE)
        // key_length = parent_id(4) + name_length(2) + name_bytes(name_len*2)
        // For name_len=1: key_length = 4 + 2 + 2 = 8
        let key_len: u16 = 8;
        node[14..16].copy_from_slice(&key_len.to_be_bytes()); // key_length
        node[16..20].copy_from_slice(&2u32.to_be_bytes()); // parent_id = 2
        node[20..22].copy_from_slice(&1u16.to_be_bytes()); // name_length = 1
        node[22..24].copy_from_slice(&[0x00, 0x41]); // name = "A" (UTF-16 BE)
                                                     // data starts at rec_off + 2 + key_len = 14 + 2 + 8 = 24
        let data_off = 24usize;
        // Folder record type = 1
        node[data_off..data_off + 2].copy_from_slice(&1i16.to_be_bytes());
        // Folder CNID at data_off + 8
        node[data_off + 8..data_off + 12].copy_from_slice(&55u32.to_be_bytes());

        let mut entries = Vec::new();
        process_leaf_node(&node, 512, 1, 2, "/", &mut entries);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "A");
        assert!(entries[0].is_directory());
        assert_eq!(entries[0].location, 55);
    }
}
