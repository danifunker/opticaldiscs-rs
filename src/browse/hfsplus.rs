//! HFS+ (Mac OS Extended) filesystem browser.
//!
//! Provides [`HfsPlusFilesystem`], which implements the [`Filesystem`] trait
//! for browsing directories and reading files on HFS+-formatted disc images.
//!
//! The implementation walks the HFS+ catalog B-tree by following leaf-node
//! linked-list pointers, decoding HFS+ catalog key / record pairs.
//!
//! See PLAN.md Phase 8.5 for implementation details.

use super::entry::{EntryType, FileEntry, FileTimestamps, PosixMetadata};
use super::filesystem::{Filesystem, FilesystemError};
use crate::hfsplus::{extract_volume_name_from_catalog, HfsPlusVolumeHeader};
use crate::sector_reader::SectorReader;

/// Read a big-endian `u32` from `node` at byte offset `off`. Callers must have
/// already bounds-checked that `off + 4 <= node.len()`.
fn be32(node: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([node[off], node[off + 1], node[off + 2], node[off + 3]])
}

/// Read the five HFS+ catalog dates (createDate, contentModDate,
/// attributeModDate, accessDate, backupDate) that sit at `data_off + 12..32` in
/// both file and folder records. Callers must have bounds-checked to `+32`.
fn hfsplus_timestamps(node: &[u8], data_off: usize) -> FileTimestamps {
    FileTimestamps::HfsPlus {
        created: be32(node, data_off + 12),
        content_modified: be32(node, data_off + 16),
        attribute_modified: be32(node, data_off + 20),
        accessed: be32(node, data_off + 24),
        backup: be32(node, data_off + 28),
    }
}

/// Read the HFS+ `BSDInfo` at `data_off + 32` (ownerID +32, groupID +36,
/// fileMode +42). Callers must have bounds-checked to `+44`.
fn hfsplus_posix(node: &[u8], data_off: usize) -> PosixMetadata {
    PosixMetadata {
        uid: be32(node, data_off + 32),
        gid: be32(node, data_off + 36),
        mode: u16::from_be_bytes([node[data_off + 42], node[data_off + 43]]) as u32,
    }
}

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
        let mut entries: Vec<FileEntry> = Vec::new();
        let mut metas: Vec<Option<HfsPlusFileMeta>> = Vec::new();
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
                &mut metas,
            );
            current = next;
        }

        // Resolve HFS+ UNIX symlinks (slnk/rhap target in the data fork) and
        // classic Mac aliases (alis resource in the resource fork).
        for (i, meta) in metas.iter().enumerate() {
            let meta = match meta {
                Some(m) => m,
                None => continue,
            };
            let entry = &mut entries[i];

            let is_slnk = entry.type_code == Some(super::mac_alias::SLNK_TYPE)
                && entry.creator_code == Some(super::mac_alias::RHAP_CREATOR);
            if is_slnk && meta.data_fork.logical_size > 0 && meta.data_fork.logical_size <= 4096 {
                let len = meta.data_fork.logical_size as usize;
                if let Ok(data) = self.read_fork_range(&meta.data_fork, 0, len) {
                    if let Ok(s) = std::str::from_utf8(&data) {
                        let trimmed = s.trim_end_matches('\0').trim();
                        if !trimmed.is_empty() {
                            entry.symlink_target = Some(trimmed.to_string());
                        }
                    }
                }
            }

            if entry.symlink_target.is_none()
                && meta.finder_flags & super::mac_alias::IS_ALIAS_FLAG != 0
                && meta.resource_fork.logical_size > 0
            {
                let len = meta.resource_fork.logical_size as usize;
                if let Ok(rsrc) = self.read_fork_range(&meta.resource_fork, 0, len) {
                    if let Some(target) = super::mac_alias::resolve_alias_target(&rsrc) {
                        entry.symlink_target = Some(target);
                    }
                }
            }
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

    /// Find the catalog file record for a given CNID and return its fork data
    /// for both data and resource forks.
    fn find_file_record(&mut self, cnid: u32) -> Result<HfsPlusFileRecord, FilesystemError> {
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

            if let Some(rec) = search_node_for_file(&node, self.node_size as usize, num_rec, cnid) {
                return Ok(rec);
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
        let rec = self.find_file_record(entry.location as u32)?;
        if rec.data_fork.extents.is_empty() {
            return Ok(Vec::new());
        }
        self.read_fork_range(&rec.data_fork, 0, rec.data_fork.logical_size as usize)
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
        let rec = self.find_file_record(entry.location as u32)?;
        if rec.data_fork.extents.is_empty() {
            return Ok(Vec::new());
        }
        let actual_len = length.min(rec.data_fork.logical_size.saturating_sub(offset) as usize);
        if actual_len == 0 {
            return Ok(Vec::new());
        }
        self.read_fork_range(&rec.data_fork, offset, actual_len)
    }

    fn read_resource_fork(
        &mut self,
        entry: &FileEntry,
    ) -> Result<Option<Vec<u8>>, FilesystemError> {
        if entry.entry_type != EntryType::File {
            return Err(FilesystemError::NotADirectory(format!(
                "{} is not a file",
                entry.path
            )));
        }
        let rec = self.find_file_record(entry.location as u32)?;
        if rec.resource_fork.logical_size == 0 || rec.resource_fork.extents.is_empty() {
            return Ok(None);
        }
        let bytes = self.read_fork_range(
            &rec.resource_fork,
            0,
            rec.resource_fork.logical_size as usize,
        )?;
        Ok(Some(bytes))
    }

    fn read_resource_fork_range(
        &mut self,
        entry: &FileEntry,
        offset: u64,
        length: usize,
    ) -> Result<Option<Vec<u8>>, FilesystemError> {
        if entry.entry_type != EntryType::File {
            return Err(FilesystemError::NotADirectory(format!(
                "{} is not a file",
                entry.path
            )));
        }
        let rec = self.find_file_record(entry.location as u32)?;
        if rec.resource_fork.logical_size == 0 || rec.resource_fork.extents.is_empty() {
            return Ok(None);
        }
        let actual_len = length.min(rec.resource_fork.logical_size.saturating_sub(offset) as usize);
        if actual_len == 0 {
            return Ok(Some(Vec::new()));
        }
        let bytes = self.read_fork_range(&rec.resource_fork, offset, actual_len)?;
        Ok(Some(bytes))
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

/// One fork (data or resource) of an HFS+ file: its logical size and the
/// first 8 extents from the catalog record.
///
/// HFS+ stores 8 extents per fork inline. Files that spill over into the
/// extents-overflow B-tree are not handled here — typical CD/DVD content
/// uses few contiguous extents so this is a known but rarely-hit limitation.
#[derive(Debug, Clone)]
struct HfsPlusForkData {
    logical_size: u64,
    extents: Vec<HfsPlusExtent>,
}

/// The parsed file record returned by [`search_node_for_file`]: both forks.
#[derive(Debug, Clone)]
struct HfsPlusFileRecord {
    data_fork: HfsPlusForkData,
    resource_fork: HfsPlusForkData,
}

/// Per-file metadata collected during leaf traversal so that `list_by_cnid`
/// can resolve HFS+ UNIX symlinks and classic Mac aliases without re-walking
/// the B-tree.
#[derive(Debug, Clone)]
struct HfsPlusFileMeta {
    finder_flags: u16,
    data_fork: HfsPlusForkData,
    resource_fork: HfsPlusForkData,
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
    metas: &mut Vec<Option<HfsPlusFileMeta>>,
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
                // Folder record (offsets relative to data_off):
                //   +8    : CNID                       (u32 BE)
                //   +12..+32 : create/contentMod/attrMod/access/backup dates
                //   +32   : BSDInfo (ownerID/groupID/…/fileMode at +42)
                if data_off + 44 > node.len() {
                    continue;
                }
                let cnid = be32(node, data_off + 8);
                let mut entry = FileEntry::new_directory(name, path, cnid as u64);
                entry.timestamps = Some(hfsplus_timestamps(node, data_off));
                entry.posix = Some(hfsplus_posix(node, data_off));
                entries.push(entry);
                metas.push(None);
            }
            HFSPLUS_FILE_RECORD => {
                // File record offsets (relative to data_off):
                //   +8    : CNID                   (u32 BE)
                //   +12..+32 : create/contentMod/attrMod/access/backup dates
                //   +32   : BSDInfo (ownerID/groupID/…/fileMode at +42)
                //   +48   : fdType                 (4 bytes, FileInfo)
                //   +52   : fdCreator              (4 bytes, FileInfo)
                //   +56   : fdFlags                (u16 BE,  FileInfo)
                //   +88   : data fork logical size (u64 BE, first field of the 80-byte data fork)
                //   +168  : resource fork logical size (u64 BE, first field of the 80-byte resource fork)
                // Need the full 80-byte resource fork to capture its extents.
                if data_off + 248 > node.len() {
                    continue;
                }
                let cnid = u32::from_be_bytes([
                    node[data_off + 8],
                    node[data_off + 9],
                    node[data_off + 10],
                    node[data_off + 11],
                ]);
                let type_code = [
                    node[data_off + 48],
                    node[data_off + 49],
                    node[data_off + 50],
                    node[data_off + 51],
                ];
                let creator_code = [
                    node[data_off + 52],
                    node[data_off + 53],
                    node[data_off + 54],
                    node[data_off + 55],
                ];
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
                let rsrc_size = u64::from_be_bytes([
                    node[data_off + 168],
                    node[data_off + 169],
                    node[data_off + 170],
                    node[data_off + 171],
                    node[data_off + 172],
                    node[data_off + 173],
                    node[data_off + 174],
                    node[data_off + 175],
                ]);
                let finder_flags = u16::from_be_bytes([node[data_off + 56], node[data_off + 57]]);
                let mut entry = FileEntry::new_hfs_file(
                    name,
                    path,
                    data_size,
                    cnid as u64,
                    rsrc_size,
                    type_code,
                    creator_code,
                    finder_flags,
                );
                entry.timestamps = Some(hfsplus_timestamps(node, data_off));
                entry.posix = Some(hfsplus_posix(node, data_off));
                entries.push(entry);
                let data_fork = parse_fork(node, data_off + 88);
                let resource_fork = parse_fork(node, data_off + 168);
                metas.push(Some(HfsPlusFileMeta {
                    finder_flags,
                    data_fork,
                    resource_fork,
                }));
            }
            _ => {}
        }
    }
}

/// Search a leaf node for a file record with a matching CNID, returning its
/// data and resource fork information.
fn search_node_for_file(
    node: &[u8],
    node_size: usize,
    num_rec: u16,
    target_cnid: u32,
) -> Option<HfsPlusFileRecord> {
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
        // Need the full file record: data fork ends at +168 and resource fork
        // at +248.
        if data_off + 248 > node.len() {
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

        let data_fork = parse_fork(node, data_off + 88);
        let resource_fork = parse_fork(node, data_off + 168);

        return Some(HfsPlusFileRecord {
            data_fork,
            resource_fork,
        });
    }

    None
}

/// Parse one 80-byte `HFSPlusForkData` structure starting at `fork_off`.
///
/// Layout:
/// - `[0..8]`   `logical_size` (u64 BE)
/// - `[8..12]`  `clump_size`   (u32 BE)
/// - `[12..16]` `total_blocks` (u32 BE)
/// - `[16..80]` 8 × `HFSPlusExtentDescriptor` (each `start_block` u32 BE +
///   `block_count` u32 BE)
fn parse_fork(node: &[u8], fork_off: usize) -> HfsPlusForkData {
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

    HfsPlusForkData {
        logical_size,
        extents,
    }
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
    // Lossy: a single malformed UTF-16 unit becomes U+FFFD rather than
    // discarding the whole name (which would drop the entry from listings).
    String::from_utf16_lossy(&utf16)
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
        let mut metas = Vec::new();
        process_leaf_node(&node, 512, 0, 2, "/", &mut entries, &mut metas);
        assert!(entries.is_empty());
    }

    struct FileRecordSpec<'a> {
        node_size: usize,
        parent_id: u32,
        name: &'a str,
        cnid: u32,
        data_size: u64,
        rsrc_size: u64,
        type_code: [u8; 4],
        creator_code: [u8; 4],
    }

    /// Helper to write an HFS+ file record into a node buffer and return the
    /// data offset so the caller can assert on parsed fields.
    fn build_file_record_node(spec: &FileRecordSpec) -> (Vec<u8>, usize) {
        let mut node = vec![0u8; spec.node_size];
        let rec_off: u16 = 14;
        // Record offset table entry at end of node.
        node[spec.node_size - 2] = (rec_off >> 8) as u8;
        node[spec.node_size - 1] = (rec_off & 0xFF) as u8;

        // Key: key_length + parent_id + name_length + name (UTF-16 BE).
        let name_chars: Vec<u16> = spec.name.encode_utf16().collect();
        let name_len = name_chars.len();
        let key_len: u16 = 4 + 2 + (name_len as u16) * 2;
        node[14..16].copy_from_slice(&key_len.to_be_bytes());
        node[16..20].copy_from_slice(&spec.parent_id.to_be_bytes());
        node[20..22].copy_from_slice(&(name_len as u16).to_be_bytes());
        for (i, &ch) in name_chars.iter().enumerate() {
            let off = 22 + i * 2;
            node[off..off + 2].copy_from_slice(&ch.to_be_bytes());
        }

        let data_off = 14 + 2 + key_len as usize;

        // File record.
        node[data_off..data_off + 2].copy_from_slice(&2i16.to_be_bytes());
        node[data_off + 8..data_off + 12].copy_from_slice(&spec.cnid.to_be_bytes());
        node[data_off + 48..data_off + 52].copy_from_slice(&spec.type_code);
        node[data_off + 52..data_off + 56].copy_from_slice(&spec.creator_code);
        node[data_off + 88..data_off + 96].copy_from_slice(&spec.data_size.to_be_bytes());
        node[data_off + 168..data_off + 176].copy_from_slice(&spec.rsrc_size.to_be_bytes());

        (node, data_off)
    }

    #[test]
    fn process_leaf_node_file_record_populates_hfs_metadata() {
        let (mut node, data_off) = build_file_record_node(&FileRecordSpec {
            node_size: 2048,
            parent_id: 2,
            name: "note",
            cnid: 77,
            data_size: 1024,
            rsrc_size: 512,
            type_code: *b"TEXT",
            creator_code: *b"ttxt",
        });
        // Dates at +12..+32; BSDInfo (ownerID +32, groupID +36, fileMode +42).
        node[data_off + 12..data_off + 16].copy_from_slice(&0x0A00_0000u32.to_be_bytes());
        node[data_off + 16..data_off + 20].copy_from_slice(&0x0B00_0000u32.to_be_bytes());
        node[data_off + 20..data_off + 24].copy_from_slice(&0x0C00_0000u32.to_be_bytes());
        node[data_off + 24..data_off + 28].copy_from_slice(&0x0D00_0000u32.to_be_bytes());
        node[data_off + 28..data_off + 32].copy_from_slice(&0x0E00_0000u32.to_be_bytes());
        node[data_off + 32..data_off + 36].copy_from_slice(&501u32.to_be_bytes());
        node[data_off + 36..data_off + 40].copy_from_slice(&20u32.to_be_bytes());
        node[data_off + 42..data_off + 44].copy_from_slice(&0o100_644u16.to_be_bytes());

        let mut entries = Vec::new();
        let mut metas = Vec::new();
        process_leaf_node(&node, 2048, 1, 2, "/", &mut entries, &mut metas);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.name, "note");
        assert!(e.is_file());
        assert_eq!(e.size, 1024);
        assert_eq!(e.location, 77);
        assert_eq!(e.resource_fork_size, Some(512));
        assert_eq!(e.type_code, Some(*b"TEXT"));
        assert_eq!(e.creator_code, Some(*b"ttxt"));
        assert_eq!(
            e.timestamps,
            Some(crate::browse::entry::FileTimestamps::HfsPlus {
                created: 0x0A00_0000,
                content_modified: 0x0B00_0000,
                attribute_modified: 0x0C00_0000,
                accessed: 0x0D00_0000,
                backup: 0x0E00_0000,
            })
        );
        let px = e.posix.expect("posix present");
        assert_eq!(px.uid, 501);
        assert_eq!(px.gid, 20);
        assert_eq!(px.mode, 0o100_644);
    }

    #[test]
    fn search_node_for_file_returns_both_forks() {
        let (mut node, data_off) = build_file_record_node(&FileRecordSpec {
            node_size: 2048,
            parent_id: 2,
            name: "bar",
            cnid: 42,
            data_size: 1024,
            rsrc_size: 256,
            type_code: *b"TEXT",
            creator_code: *b"ttxt",
        });
        // Add one data-fork extent (start=10, count=2) at data_off+88+16.
        let dfe = data_off + 88 + 16;
        node[dfe..dfe + 4].copy_from_slice(&10u32.to_be_bytes());
        node[dfe + 4..dfe + 8].copy_from_slice(&2u32.to_be_bytes());
        // Add one resource-fork extent (start=30, count=1) at data_off+168+16.
        let rfe = data_off + 168 + 16;
        node[rfe..rfe + 4].copy_from_slice(&30u32.to_be_bytes());
        node[rfe + 4..rfe + 8].copy_from_slice(&1u32.to_be_bytes());

        let rec = search_node_for_file(&node, 2048, 1, 42).expect("record found");
        assert_eq!(rec.data_fork.logical_size, 1024);
        assert_eq!(rec.data_fork.extents[0].start_block, 10);
        assert_eq!(rec.data_fork.extents[0].block_count, 2);
        assert_eq!(rec.resource_fork.logical_size, 256);
        assert_eq!(rec.resource_fork.extents[0].start_block, 30);
        assert_eq!(rec.resource_fork.extents[0].block_count, 1);
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
        // Dates at +12..+32; BSDInfo groupID +36, fileMode +42.
        node[data_off + 12..data_off + 16].copy_from_slice(&0x0100_0000u32.to_be_bytes());
        node[data_off + 16..data_off + 20].copy_from_slice(&0x0200_0000u32.to_be_bytes());
        node[data_off + 20..data_off + 24].copy_from_slice(&0x0300_0000u32.to_be_bytes());
        node[data_off + 24..data_off + 28].copy_from_slice(&0x0400_0000u32.to_be_bytes());
        node[data_off + 28..data_off + 32].copy_from_slice(&0x0500_0000u32.to_be_bytes());
        node[data_off + 36..data_off + 40].copy_from_slice(&80u32.to_be_bytes());
        node[data_off + 42..data_off + 44].copy_from_slice(&0o040_755u16.to_be_bytes());

        let mut entries = Vec::new();
        let mut metas = Vec::new();
        process_leaf_node(&node, 512, 1, 2, "/", &mut entries, &mut metas);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "A");
        assert!(entries[0].is_directory());
        assert_eq!(entries[0].location, 55);
        assert_eq!(
            entries[0].timestamps,
            Some(crate::browse::entry::FileTimestamps::HfsPlus {
                created: 0x0100_0000,
                content_modified: 0x0200_0000,
                attribute_modified: 0x0300_0000,
                accessed: 0x0400_0000,
                backup: 0x0500_0000,
            })
        );
        let px = entries[0].posix.expect("posix present");
        assert_eq!(px.gid, 80);
        assert_eq!(px.mode, 0o040_755);
    }
}
