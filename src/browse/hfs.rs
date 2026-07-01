//! HFS (classic Mac) filesystem browser.
//!
//! Provides [`HfsFilesystem`], which implements the [`Filesystem`] trait for
//! browsing directories and reading files on HFS-formatted disc images.
//!
//! The implementation walks the HFS catalog B-tree by following leaf-node
//! linked-list pointers, parsing HFS catalog key / record pairs.
//!
//! See PLAN.md Phase 8.4 for implementation details.

use super::entry::{EntryType, FileEntry, FileTimestamps};
use super::filesystem::{Filesystem, FilesystemError};
use crate::hfs::{mac_roman_to_string, MasterDirectoryBlock};
use crate::sector_reader::SectorReader;

/// Read a big-endian `u32` from `node` at byte offset `off`. Callers must have
/// already bounds-checked that `off + 4 <= node.len()`.
fn be32(node: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([node[off], node[off + 1], node[off + 2], node[off + 3]])
}

// ── HFS B-tree / catalog constants ───────────────────────────────────────────

/// Root directory CNID (always 2 in HFS).
const HFS_ROOT_DIR_ID: u32 = 2;

/// Catalog record type: folder.
const HFS_FOLDER_RECORD: i8 = 1;
/// Catalog record type: file.
const HFS_FILE_RECORD: i8 = 2;

// ── Public type ───────────────────────────────────────────────────────────────

/// HFS filesystem browser.
///
/// Created by [`open_disc_filesystem`][crate::browse::open_disc_filesystem]
/// when the detected filesystem type is [`FilesystemType::Hfs`][crate::formats::FilesystemType::Hfs].
pub struct HfsFilesystem {
    reader: Box<dyn SectorReader>,
    /// Byte offset of the HFS partition from the disc start (0 for non-APM images).
    partition_offset: u64,
    /// Allocation block size in bytes.
    alloc_block_size: u32,
    /// First allocation block sector number (in 512-byte sectors from partition start).
    alloc_block_start: u32,
    /// Full catalog file contents, loaded at open time across all 3 extents.
    catalog_data: Vec<u8>,
    /// B-tree node size in bytes.
    node_size: u16,
    /// Node number of the first leaf node in the catalog B-tree.
    first_leaf_node: u32,
    /// Volume name decoded from the MDB.
    volume_name: String,
}

impl HfsFilesystem {
    /// Open an HFS filesystem.
    ///
    /// Reads the MDB at `partition_offset + 1024`, then reads the catalog
    /// B-tree header to locate the first leaf node.
    ///
    /// # Errors
    ///
    /// Returns [`FilesystemError::Parse`] if the MDB or B-tree header is
    /// malformed, or [`FilesystemError::Io`] on read failure.
    pub fn new(
        mut reader: Box<dyn SectorReader>,
        partition_offset: u64,
    ) -> Result<Self, FilesystemError> {
        let mdb = MasterDirectoryBlock::read_from(reader.as_mut(), partition_offset)
            .map_err(hfs_disc_err)?;

        // Load the entire catalog file across all 3 extents into memory.
        // CD/DVD HFS catalogs are typically a few MB; keeping it cached
        // simplifies B-tree traversal and handles fragmented catalogs.
        let catalog_extents: Vec<HfsExtent> = mdb
            .catalog_extents
            .iter()
            .filter(|(_, count)| *count != 0)
            .map(|&(start, count)| HfsExtent {
                start_block: start,
                block_count: count,
            })
            .collect();

        let catalog_data = read_extents_into_vec(
            reader.as_mut(),
            partition_offset,
            mdb.alloc_block_start as u32,
            mdb.alloc_block_size,
            &catalog_extents,
            mdb.catalog_file_size as usize,
        )?;

        // B-tree header node layout:
        //   [0..14]  node descriptor (14 bytes)
        //   [14..] B-tree header record
        //
        // Relevant fields (offsets from node start, matching ODE hfs_fs.rs):
        //   [24..28] first_leaf_node (u32 BE)
        //   [32..34] node_size (u16 BE)
        if catalog_data.len() < 34 {
            return Err(FilesystemError::Parse(
                "HFS catalog too small to contain B-tree header".into(),
            ));
        }
        let first_leaf_node = u32::from_be_bytes([
            catalog_data[24],
            catalog_data[25],
            catalog_data[26],
            catalog_data[27],
        ]);
        let node_size = u16::from_be_bytes([catalog_data[32], catalog_data[33]]);

        Ok(Self {
            reader,
            partition_offset,
            alloc_block_size: mdb.alloc_block_size,
            alloc_block_start: mdb.alloc_block_start as u32,
            catalog_data,
            node_size,
            first_leaf_node,
            volume_name: mdb.volume_name,
        })
    }

    // ── B-tree helpers ────────────────────────────────────────────────────────

    /// Borrow a single B-tree node by node number from the cached catalog.
    fn read_node(&self, node_num: u32) -> Result<&[u8], FilesystemError> {
        let node_size = self.node_size as usize;
        let start = node_num as usize * node_size;
        let end = start + node_size;
        self.catalog_data.get(start..end).ok_or_else(|| {
            FilesystemError::Parse(format!(
                "HFS B-tree node {node_num} out of bounds (catalog has {} bytes)",
                self.catalog_data.len()
            ))
        })
    }

    // ── Directory listing ─────────────────────────────────────────────────────

    /// Walk all leaf nodes and collect entries whose key `parent_id` matches
    /// `parent_cnid`.
    fn list_by_id(
        &mut self,
        parent_cnid: u32,
        parent_path: &str,
    ) -> Result<Vec<FileEntry>, FilesystemError> {
        let mut entries: Vec<FileEntry> = Vec::new();
        let mut metas: Vec<Option<HfsFileMeta>> = Vec::new();
        let mut current = self.first_leaf_node;
        let mut attempts = 0u32;
        const MAX: u32 = 10_000;

        while current != 0 && attempts < MAX {
            attempts += 1;
            let node = self.read_node(current)?;

            // Node descriptor
            let next = u32::from_be_bytes([node[0], node[1], node[2], node[3]]);
            let kind = node[8] as i8;
            let num_rec = u16::from_be_bytes([node[10], node[11]]);

            if kind != -1 {
                current = next;
                continue;
            }

            process_leaf_node(
                node,
                self.node_size as usize,
                num_rec,
                parent_cnid,
                parent_path,
                &mut entries,
                &mut metas,
            );
            current = next;
        }

        // Resolve classic Mac aliases: files with the alias flag set have
        // an `alis` resource in their resource fork pointing to the target.
        for (i, meta) in metas.iter().enumerate() {
            if let Some(meta) = meta {
                if meta.finder_flags & super::mac_alias::IS_ALIAS_FLAG != 0 && meta.rsrc_size > 0 {
                    if let Ok(rsrc) =
                        self.read_extents_range(&meta.rsrc_extents, 0, meta.rsrc_size as usize)
                    {
                        if let Some(target) = super::mac_alias::resolve_alias_target(&rsrc) {
                            entries[i].symlink_target = Some(target);
                        }
                    }
                }
            }
        }

        // Directories first, then sort by name (case-insensitive).
        entries.sort_by(|a, b| match (a.entry_type, b.entry_type) {
            (EntryType::Directory, EntryType::File) => std::cmp::Ordering::Less,
            (EntryType::File, EntryType::Directory) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });

        Ok(entries)
    }

    // ── File reading ──────────────────────────────────────────────────────────

    /// Find the catalog file record for a given CNID and return its fork data.
    fn find_file_record(&mut self, cnid: u32) -> Result<HfsFileRecord, FilesystemError> {
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

            if let Some(rec) = search_node_for_file(node, self.node_size as usize, num_rec, cnid) {
                return Ok(rec);
            }

            current = next;
        }

        Err(FilesystemError::NotFound(format!(
            "File CNID {cnid} not found in catalog"
        )))
    }

    /// Read a byte range from a list of HFS extents.
    fn read_extents_range(
        &mut self,
        extents: &[HfsExtent],
        range_offset: u64,
        range_length: usize,
    ) -> Result<Vec<u8>, FilesystemError> {
        let first_alloc_offset = self.partition_offset + self.alloc_block_start as u64 * 512;

        let mut result = Vec::with_capacity(range_length);
        let mut logical_pos: u64 = 0;
        let end = range_offset + range_length as u64;

        for ext in extents {
            if ext.block_count == 0 {
                break;
            }
            let ext_size = ext.block_count as u64 * self.alloc_block_size as u64;
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

            let phys_off = first_alloc_offset
                + ext.start_block as u64 * self.alloc_block_size as u64
                + offset_in_ext;

            let chunk = self
                .reader
                .read_bytes(phys_off, read_len)
                .map_err(hfs_disc_err)?;
            result.extend_from_slice(&chunk);

            logical_pos = ext_end;
        }

        Ok(result)
    }
}

impl Filesystem for HfsFilesystem {
    fn root(&mut self) -> Result<FileEntry, FilesystemError> {
        Ok(FileEntry::root(HFS_ROOT_DIR_ID as u64))
    }

    fn list_directory(&mut self, entry: &FileEntry) -> Result<Vec<FileEntry>, FilesystemError> {
        if entry.entry_type != EntryType::Directory {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        let cnid = entry.location as u32;
        self.list_by_id(cnid, &entry.path)
    }

    fn read_file(&mut self, entry: &FileEntry) -> Result<Vec<u8>, FilesystemError> {
        if entry.entry_type != EntryType::File {
            return Err(FilesystemError::NotADirectory(format!(
                "{} is not a file",
                entry.path
            )));
        }
        let rec = self.find_file_record(entry.location as u32)?;
        self.read_extents_range(&rec.data_extents, 0, entry.size as usize)
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
        let actual_len = length.min(entry.size.saturating_sub(offset) as usize);
        if actual_len == 0 {
            return Ok(Vec::new());
        }
        let rec = self.find_file_record(entry.location as u32)?;
        self.read_extents_range(&rec.data_extents, offset, actual_len)
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
        if rec.resource_size == 0 {
            return Ok(None);
        }
        let bytes =
            self.read_extents_range(&rec.resource_extents, 0, rec.resource_size as usize)?;
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
        if rec.resource_size == 0 {
            return Ok(None);
        }
        let actual_len = length.min(rec.resource_size.saturating_sub(offset) as usize);
        if actual_len == 0 {
            return Ok(Some(Vec::new()));
        }
        let bytes = self.read_extents_range(&rec.resource_extents, offset, actual_len)?;
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

/// An HFS extent descriptor: start block and block count (both in allocation blocks).
#[derive(Debug, Clone, Copy)]
struct HfsExtent {
    start_block: u16,
    block_count: u16,
}

/// Read a fork (catalog or other file) across its extent list into a `Vec`.
///
/// Walks `extents` in order, reading the corresponding physical bytes from
/// `reader` until `total_size` bytes have been collected.  Stops early when
/// it hits an empty extent (`block_count == 0`).
fn read_extents_into_vec(
    reader: &mut dyn SectorReader,
    partition_offset: u64,
    alloc_block_start: u32,
    alloc_block_size: u32,
    extents: &[HfsExtent],
    total_size: usize,
) -> Result<Vec<u8>, FilesystemError> {
    let first_alloc_offset = partition_offset + alloc_block_start as u64 * 512;
    let mut data = Vec::with_capacity(total_size);

    for ext in extents {
        if ext.block_count == 0 || data.len() >= total_size {
            break;
        }
        let phys_off = first_alloc_offset + ext.start_block as u64 * alloc_block_size as u64;
        let ext_len = ext.block_count as usize * alloc_block_size as usize;
        let want = ext_len.min(total_size - data.len());
        let chunk = reader.read_bytes(phys_off, want).map_err(hfs_disc_err)?;
        data.extend_from_slice(&chunk);
    }

    data.truncate(total_size);
    Ok(data)
}

/// The fork data for an HFS file: data fork extents and resource fork
/// extents (plus the resource fork's logical size).
///
/// Note: HFS stores only the first three extents per fork in the catalog
/// record. Files that overflow into the extents-overflow B-tree are not
/// handled here — on CD/DVD images files are overwhelmingly contiguous, so
/// this is a known but rarely-hit limitation.
#[derive(Debug, Clone)]
struct HfsFileRecord {
    data_extents: Vec<HfsExtent>,
    resource_extents: Vec<HfsExtent>,
    resource_size: u64,
}

/// Per-file metadata gathered during leaf traversal so that `list_by_id`
/// can resolve classic Mac aliases without re-walking the B-tree.
#[derive(Debug, Clone)]
struct HfsFileMeta {
    finder_flags: u16,
    rsrc_extents: Vec<HfsExtent>,
    rsrc_size: u64,
}

/// Process all records in a leaf node, collecting entries with matching parent.
///
/// `metas` is pushed in parallel with `entries`: `None` for folders, `Some`
/// for files, so that aliases can later be resolved without re-walking the
/// B-tree.
fn process_leaf_node(
    node: &[u8],
    node_size: usize,
    num_rec: u16,
    parent_cnid: u32,
    parent_path: &str,
    entries: &mut Vec<FileEntry>,
    metas: &mut Vec<Option<HfsFileMeta>>,
) {
    let offsets_base = node_size - 2;

    for i in 0..num_rec {
        let off_pos = offsets_base - i as usize * 2;
        if off_pos + 2 > node.len() {
            continue;
        }
        let rec_off = u16::from_be_bytes([node[off_pos], node[off_pos + 1]]) as usize;
        if rec_off + 8 > node.len() {
            continue;
        }

        // HFS catalog key layout:
        //   [0]      key_len (u8): length of key data (bytes 1..key_len inclusive)
        //   [1]      reserved (u8)
        //   [2..5]   parent_id (u32 BE)
        //   [6]      name_len (u8): number of Mac Roman bytes
        //   [7..]    name bytes (Mac Roman)
        let key_len = node[rec_off] as usize;
        if key_len < 6 || rec_off + 1 + key_len > node.len() {
            continue;
        }
        let pid = u32::from_be_bytes([
            node[rec_off + 2],
            node[rec_off + 3],
            node[rec_off + 4],
            node[rec_off + 5],
        ]);
        if pid != parent_cnid {
            continue;
        }
        let name_len = node[rec_off + 6] as usize;
        if name_len == 0 {
            // Thread record — skip.
            continue;
        }
        let name_start = rec_off + 7;
        let name_end = name_start + name_len;
        if name_end > node.len() {
            continue;
        }
        let name = mac_roman_to_string(&node[name_start..name_end]);

        // Record data starts at the first even byte after the key length byte + key data.
        // data_off (relative to rec_off) = round_up_to_even(1 + key_len)
        //   = (key_len + 2) & !1usize
        let data_off = rec_off + ((key_len + 2) & !1usize);
        if data_off + 2 > node.len() {
            continue;
        }

        let rec_type = node[data_off] as i8;

        let path = if parent_path == "/" {
            format!("/{name}")
        } else {
            format!("{parent_path}/{name}")
        };

        match rec_type {
            HFS_FOLDER_RECORD => {
                // Folder record (offsets relative to data_off, HFS `CatDataDir`):
                //   +6   : dir_id            (u32 BE)
                //   +10  : creation date     (u32 BE, secs since 1904, local)
                //   +14  : modification date (u32 BE)
                //   +18  : backup date       (u32 BE)
                if data_off + 22 > node.len() {
                    continue;
                }
                let dir_id = be32(node, data_off + 6);
                let mut entry = FileEntry::new_directory(name, path, dir_id as u64);
                entry.timestamps = Some(FileTimestamps::Hfs {
                    created: be32(node, data_off + 10),
                    modified: be32(node, data_off + 14),
                    backup: be32(node, data_off + 18),
                });
                entries.push(entry);
                metas.push(None);
            }
            HFS_FILE_RECORD => {
                // File record (offsets relative to data_off):
                //   +4   : fdType    (4 bytes, FInfo)
                //   +8   : fdCreator (4 bytes, FInfo)
                //   +12  : fdFlags                    (u16 BE, FInfo)
                //   +20  : file_id                    (u32 BE)
                //   +26  : data fork logical size     (u32 BE)
                //   +36  : resource fork logical size (u32 BE)
                //   +44  : creation date              (u32 BE, secs since 1904, local)
                //   +48  : modification date          (u32 BE)
                //   +52  : backup date                (u32 BE)
                //   +86..+98: resource fork extents (3 × HfsExtent)
                if data_off + 98 > node.len() {
                    continue;
                }
                let type_code = [
                    node[data_off + 4],
                    node[data_off + 5],
                    node[data_off + 6],
                    node[data_off + 7],
                ];
                let creator_code = [
                    node[data_off + 8],
                    node[data_off + 9],
                    node[data_off + 10],
                    node[data_off + 11],
                ];
                let file_id = u32::from_be_bytes([
                    node[data_off + 20],
                    node[data_off + 21],
                    node[data_off + 22],
                    node[data_off + 23],
                ]);
                let data_logical_size = u32::from_be_bytes([
                    node[data_off + 26],
                    node[data_off + 27],
                    node[data_off + 28],
                    node[data_off + 29],
                ]);
                let rsrc_logical_size = u32::from_be_bytes([
                    node[data_off + 36],
                    node[data_off + 37],
                    node[data_off + 38],
                    node[data_off + 39],
                ]);
                let finder_flags = u16::from_be_bytes([node[data_off + 12], node[data_off + 13]]);
                let mut entry = FileEntry::new_hfs_file(
                    name,
                    path,
                    data_logical_size as u64,
                    file_id as u64,
                    rsrc_logical_size as u64,
                    type_code,
                    creator_code,
                    finder_flags,
                );
                entry.timestamps = Some(FileTimestamps::Hfs {
                    created: be32(node, data_off + 44),
                    modified: be32(node, data_off + 48),
                    backup: be32(node, data_off + 52),
                });
                entries.push(entry);
                let mut rsrc_extents = Vec::with_capacity(3);
                for j in 0..3 {
                    let base = data_off + 86 + j * 4;
                    rsrc_extents.push(HfsExtent {
                        start_block: u16::from_be_bytes([node[base], node[base + 1]]),
                        block_count: u16::from_be_bytes([node[base + 2], node[base + 3]]),
                    });
                }
                metas.push(Some(HfsFileMeta {
                    finder_flags,
                    rsrc_extents,
                    rsrc_size: rsrc_logical_size as u64,
                }));
            }
            _ => {}
        }
    }
}

/// Search a single B-tree leaf node for a file record whose `file_id` matches
/// `target_cnid`.  Returns the file's data-fork and resource-fork extents
/// along with the resource-fork logical size.
fn search_node_for_file(
    node: &[u8],
    node_size: usize,
    num_rec: u16,
    target_cnid: u32,
) -> Option<HfsFileRecord> {
    let offsets_base = node_size - 2;

    for i in 0..num_rec {
        let off_pos = offsets_base - i as usize * 2;
        if off_pos + 2 > node.len() {
            continue;
        }
        let rec_off = u16::from_be_bytes([node[off_pos], node[off_pos + 1]]) as usize;
        if rec_off + 8 > node.len() {
            continue;
        }

        let key_len = node[rec_off] as usize;
        if key_len < 6 {
            continue;
        }

        let data_off = rec_off + ((key_len + 2) & !1usize);
        if data_off + 2 > node.len() {
            continue;
        }

        let rec_type = node[data_off] as i8;
        if rec_type != HFS_FILE_RECORD {
            continue;
        }

        // file_id at data_off + 20 (u32 BE); resource-fork extents end at
        // data_off + 98 so require at least that many bytes.
        if data_off + 98 > node.len() {
            continue;
        }
        let file_id = u32::from_be_bytes([
            node[data_off + 20],
            node[data_off + 21],
            node[data_off + 22],
            node[data_off + 23],
        ]);
        if file_id != target_cnid {
            continue;
        }

        let resource_size = u32::from_be_bytes([
            node[data_off + 36],
            node[data_off + 37],
            node[data_off + 38],
            node[data_off + 39],
        ]) as u64;

        let read_three_extents = |base_off: usize| -> Vec<HfsExtent> {
            (0..3)
                .map(|j| {
                    let base = base_off + j * 4;
                    HfsExtent {
                        start_block: u16::from_be_bytes([node[base], node[base + 1]]),
                        block_count: u16::from_be_bytes([node[base + 2], node[base + 3]]),
                    }
                })
                .collect()
        };

        // Data-fork extents at data_off + 74; resource-fork extents at +86.
        let data_extents = read_three_extents(data_off + 74);
        let resource_extents = read_three_extents(data_off + 86);

        return Some(HfsFileRecord {
            data_extents,
            resource_extents,
            resource_size,
        });
    }

    None
}

// ── Error conversion ──────────────────────────────────────────────────────────

fn hfs_disc_err(e: crate::error::OpticaldiscsError) -> FilesystemError {
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
    fn hfs_extent_fields() {
        let ext = HfsExtent {
            start_block: 10,
            block_count: 5,
        };
        assert_eq!(ext.start_block, 10);
        assert_eq!(ext.block_count, 5);
    }

    #[test]
    fn process_leaf_node_empty_returns_nothing() {
        let node = vec![0u8; 512];
        let mut entries = Vec::new();
        let mut metas = Vec::new();
        // num_rec = 0 → no work done
        process_leaf_node(&node, 512, 0, 2, "/", &mut entries, &mut metas);
        assert!(entries.is_empty());
    }

    #[test]
    fn process_leaf_node_wrong_parent_skips() {
        // Build a minimal node with one folder record for parent 99, not 2.
        let mut node = vec![0u8; 512];
        // Record offset table at end: one record at byte 14
        let rec_off: u16 = 14;
        node[510] = (rec_off >> 8) as u8;
        node[511] = (rec_off & 0xFF) as u8;

        // key_len = 7: reserved(1) + parent_id(4) + name_len(1) + name(1)
        let key_len: u8 = 7;
        node[14] = key_len;
        node[15] = 0; // reserved
        node[16..20].copy_from_slice(&99u32.to_be_bytes()); // parent_id = 99 (not 2)
        node[20] = 1; // name_len = 1
        node[21] = b'A'; // name = "A"

        let mut entries = Vec::new();
        let mut metas = Vec::new();
        process_leaf_node(&node, 512, 1, 2, "/", &mut entries, &mut metas);
        // parent_id mismatch → no entries
        assert!(entries.is_empty());
    }

    #[test]
    fn process_leaf_node_folder_record() {
        let mut node = vec![0u8; 512];
        // Record at byte 14
        let rec_off: u16 = 14;
        node[510] = (rec_off >> 8) as u8;
        node[511] = (rec_off & 0xFF) as u8;

        // key: key_len=7, reserved, parent_id=2, name_len=1, name="A"
        node[14] = 7; // key_len
        node[15] = 0; // reserved
        node[16..20].copy_from_slice(&2u32.to_be_bytes()); // parent_id = 2
        node[20] = 1; // name_len
        node[21] = b'A'; // name = "A"
                         // key_len=7 → data_off = 14 + ((7 + 2) & !1) = 14 + 8 = 22
        let data_off = 22usize;
        // Folder record: type=1 at data_off, dir_id at data_off+6
        node[data_off] = 1; // HFS_FOLDER_RECORD
        node[data_off + 6..data_off + 10].copy_from_slice(&42u32.to_be_bytes()); // dir_id = 42
                                                                                 // creation/modification/backup dates at +10/+14/+18 (secs since 1904).
        node[data_off + 10..data_off + 14].copy_from_slice(&0x1000_0000u32.to_be_bytes());
        node[data_off + 14..data_off + 18].copy_from_slice(&0x2000_0000u32.to_be_bytes());
        node[data_off + 18..data_off + 22].copy_from_slice(&0x3000_0000u32.to_be_bytes());

        let mut entries = Vec::new();
        let mut metas = Vec::new();
        process_leaf_node(&node, 512, 1, 2, "/", &mut entries, &mut metas);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "A");
        assert_eq!(entries[0].path, "/A");
        assert!(entries[0].is_directory());
        assert_eq!(entries[0].location, 42);
        assert_eq!(
            entries[0].timestamps,
            Some(crate::browse::entry::FileTimestamps::Hfs {
                created: 0x1000_0000,
                modified: 0x2000_0000,
                backup: 0x3000_0000,
            })
        );
    }

    #[test]
    fn process_leaf_node_file_record() {
        let mut node = vec![0u8; 512];
        let rec_off: u16 = 14;
        node[510] = (rec_off >> 8) as u8;
        node[511] = (rec_off & 0xFF) as u8;

        node[14] = 9; // key_len: reserved(1)+parent_id(4)+name_len(1)+name(3)
        node[15] = 0;
        node[16..20].copy_from_slice(&2u32.to_be_bytes()); // parent_id = 2
        node[20] = 3; // name_len = 3
        node[21..24].copy_from_slice(b"foo"); // name = "foo"
                                              // key_len=9 → data_off = 14 + ((9 + 2) & !1) = 14 + 10 = 24
        let data_off = 24usize;
        node[data_off] = 2; // HFS_FILE_RECORD
                            // fdType at +4, fdCreator at +8
        node[data_off + 4..data_off + 8].copy_from_slice(b"TEXT");
        node[data_off + 8..data_off + 12].copy_from_slice(b"ttxt");
        // file_id at data_off + 20
        node[data_off + 20..data_off + 24].copy_from_slice(&77u32.to_be_bytes());
        // data-fork logical_size at data_off + 26
        node[data_off + 26..data_off + 30].copy_from_slice(&1024u32.to_be_bytes());
        // resource-fork logical_size at data_off + 36
        node[data_off + 36..data_off + 40].copy_from_slice(&256u32.to_be_bytes());
        // creation/modification/backup dates at +44/+48/+52 (secs since 1904).
        node[data_off + 44..data_off + 48].copy_from_slice(&0x1111_1111u32.to_be_bytes());
        node[data_off + 48..data_off + 52].copy_from_slice(&0x2222_2222u32.to_be_bytes());
        node[data_off + 52..data_off + 56].copy_from_slice(&0x3333_3333u32.to_be_bytes());

        let mut entries = Vec::new();
        let mut metas = Vec::new();
        process_leaf_node(&node, 512, 1, 2, "/", &mut entries, &mut metas);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "foo");
        assert!(entries[0].is_file());
        assert_eq!(entries[0].size, 1024);
        assert_eq!(entries[0].location, 77);
        assert_eq!(entries[0].resource_fork_size, Some(256));
        assert_eq!(entries[0].type_code, Some(*b"TEXT"));
        assert_eq!(entries[0].creator_code, Some(*b"ttxt"));
        assert_eq!(
            entries[0].timestamps,
            Some(crate::browse::entry::FileTimestamps::Hfs {
                created: 0x1111_1111,
                modified: 0x2222_2222,
                backup: 0x3333_3333,
            })
        );
    }

    #[test]
    fn search_node_for_file_returns_both_forks() {
        let mut node = vec![0u8; 512];
        let rec_off: u16 = 14;
        node[510] = (rec_off >> 8) as u8;
        node[511] = (rec_off & 0xFF) as u8;

        node[14] = 9; // key_len
        node[15] = 0;
        node[16..20].copy_from_slice(&2u32.to_be_bytes());
        node[20] = 3;
        node[21..24].copy_from_slice(b"bar");
        let data_off = 24usize;
        node[data_off] = 2; // HFS_FILE_RECORD
        node[data_off + 20..data_off + 24].copy_from_slice(&42u32.to_be_bytes()); // file_id=42
        node[data_off + 36..data_off + 40].copy_from_slice(&100u32.to_be_bytes()); // rsrc size
                                                                                   // Data-fork extents at +74: one extent (start=10, count=2)
        node[data_off + 74..data_off + 76].copy_from_slice(&10u16.to_be_bytes());
        node[data_off + 76..data_off + 78].copy_from_slice(&2u16.to_be_bytes());
        // Resource-fork extents at +86: one extent (start=30, count=1)
        node[data_off + 86..data_off + 88].copy_from_slice(&30u16.to_be_bytes());
        node[data_off + 88..data_off + 90].copy_from_slice(&1u16.to_be_bytes());

        let rec = search_node_for_file(&node, 512, 1, 42).expect("record found");
        assert_eq!(rec.resource_size, 100);
        assert_eq!(rec.data_extents[0].start_block, 10);
        assert_eq!(rec.data_extents[0].block_count, 2);
        assert_eq!(rec.resource_extents[0].start_block, 30);
        assert_eq!(rec.resource_extents[0].block_count, 1);
    }
}
