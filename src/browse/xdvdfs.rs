//! Xbox / Xbox 360 **XDVDFS** filesystem browser.
//!
//! Xbox game discs do not use ISO 9660; they use Microsoft's XDVDFS. Everything
//! is little-endian and addressed in 2048-byte sectors relative to the start of
//! the *game partition* (`base`).
//!
//! - **Volume descriptor** at `base + 0x10000`: the 20-byte magic
//!   `MICROSOFT*XBOX*MEDIA`, then the root directory's start sector (u32) at
//!   offset `0x14` and its size in bytes (u32) at `0x18`.
//! - **Directory**: a self-balancing **binary tree** of entries packed into the
//!   directory extent. Each entry: left-subtree word offset (u16) at `0x00`,
//!   right-subtree word offset (u16) at `0x02`, start sector (u32) at `0x04`,
//!   size (u32) at `0x08`, attributes (u8) at `0x0C` (`0x10` = directory),
//!   name length (u8) at `0x0D`, then the name. Subtree offsets are counted in
//!   4-byte words from the start of the extent; `0xFFFF` (or `0`) marks a leaf.
//!
//! The game partition starts at one of several byte offsets depending on how the
//! disc was dumped ([`XDVDFS_BASES`]); [`detect`] probes them in order.
//!
//! Reference: `docs/GameDiscs_Implementation.md` §8.5; extract-xiso;
//! xboxdevwiki.net/XDVDFS.

use std::collections::HashSet;

use super::entry::{EntryType, FileEntry};
use super::filesystem::{Filesystem, FilesystemError};
use crate::sector_reader::{SectorReader, SECTOR_SIZE};

/// 20-byte XDVDFS volume magic, located at `base + 0x10000`.
pub const XDVDFS_MAGIC: &[u8; 20] = b"MICROSOFT*XBOX*MEDIA";

/// Byte offset of the volume descriptor within the game partition.
const VD_OFFSET: u64 = 0x1_0000;

/// Candidate game-partition base byte offsets, probed in order:
/// stripped/`.xiso` dumps (0), XGD2 (Xbox 360), XGD3, and XGD1 full dumps.
pub const XDVDFS_BASES: [u64; 4] = [0, 0x0FD9_0000, 0x0208_0000, 0x1830_0000];

/// Directory-attribute bit marking a subdirectory.
const ATTR_DIRECTORY: u8 = 0x10;

/// Upper bound on directory entries traversed, guarding against corrupt trees.
const MAX_DIR_ENTRIES: usize = 100_000;

/// Detect an XDVDFS volume, returning the game-partition **base** byte offset.
///
/// Probes each candidate base in [`XDVDFS_BASES`] for the volume magic. Returns
/// `None` if no base matches (the disc is not XDVDFS).
pub fn detect(reader: &mut dyn SectorReader) -> Option<u64> {
    for &base in &XDVDFS_BASES {
        if let Ok(d) = reader.read_bytes(base + VD_OFFSET, XDVDFS_MAGIC.len()) {
            if d == XDVDFS_MAGIC {
                return Some(base);
            }
        }
    }
    None
}

/// Locate a file directly in the root directory by name (case-insensitive),
/// returning its `(start_sector, size)`. Used for cheap identity probes (e.g.
/// reading `default.xbe`) without materialising the whole tree.
pub fn root_file_extent(
    reader: &mut dyn SectorReader,
    base: u64,
    name: &str,
) -> Option<(u32, u32)> {
    let vd = reader.read_bytes(base + VD_OFFSET, 0x20).ok()?;
    if vd.len() < 0x20 || &vd[..20] != XDVDFS_MAGIC {
        return None;
    }
    let root_sector = le32(&vd, 0x14);
    let root_size = le32(&vd, 0x18);
    if root_size == 0 {
        return None;
    }
    let data = reader
        .read_bytes(base + root_sector as u64 * SECTOR_SIZE, root_size as usize)
        .ok()?;
    for e in walk_dir_tree(&data) {
        if !e.is_dir && e.name.eq_ignore_ascii_case(name) {
            return Some((e.start_sector, e.size));
        }
    }
    None
}

/// XDVDFS filesystem browser.
pub struct XdvdfsFilesystem {
    reader: Box<dyn SectorReader>,
    /// Game-partition base byte offset; every sector is relative to this.
    base: u64,
    root_sector: u32,
    root_size: u32,
}

impl XdvdfsFilesystem {
    /// Create a browser by locating the XDVDFS volume in `reader`.
    ///
    /// # Errors
    ///
    /// Returns [`FilesystemError::Parse`] if no XDVDFS volume descriptor is found.
    pub fn new(mut reader: Box<dyn SectorReader>) -> Result<Self, FilesystemError> {
        let base = detect(reader.as_mut())
            .ok_or_else(|| FilesystemError::Parse("no XDVDFS volume descriptor".into()))?;
        let vd = reader
            .read_bytes(base + VD_OFFSET, 0x20)
            .map_err(sector_to_fs_err)?;
        let root_sector = le32(&vd, 0x14);
        let root_size = le32(&vd, 0x18);
        Ok(Self {
            reader,
            base,
            root_sector,
            root_size,
        })
    }

    /// Read `size` bytes of a directory/file extent starting at `sector`
    /// (relative to the game-partition base).
    fn read_extent(&mut self, sector: u32, size: u32) -> Result<Vec<u8>, FilesystemError> {
        if size == 0 {
            return Ok(Vec::new());
        }
        self.reader
            .read_bytes(self.base + sector as u64 * SECTOR_SIZE, size as usize)
            .map_err(sector_to_fs_err)
    }
}

impl Filesystem for XdvdfsFilesystem {
    fn root(&mut self) -> Result<FileEntry, FilesystemError> {
        let mut root = FileEntry::root(self.root_sector as u64);
        root.size = self.root_size as u64;
        Ok(root)
    }

    fn list_directory(&mut self, entry: &FileEntry) -> Result<Vec<FileEntry>, FilesystemError> {
        if !entry.is_directory() {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        let (sector, size) = if entry.path == "/" {
            (self.root_sector, self.root_size)
        } else {
            (entry.location as u32, entry.size as u32)
        };

        let data = self.read_extent(sector, size)?;
        let parent = entry.path.trim_end_matches('/');
        let mut entries: Vec<FileEntry> = walk_dir_tree(&data)
            .into_iter()
            .map(|e| {
                let path = format!("{parent}/{}", e.name);
                let mut fe = if e.is_dir {
                    FileEntry::new_directory(e.name, path, e.start_sector as u64)
                } else {
                    FileEntry::new_file(e.name, path, e.size as u64, e.start_sector as u64)
                };
                // Directories carry their extent size in `size` so a later
                // list_directory can read them without re-parsing the parent.
                if e.is_dir {
                    fe.size = e.size as u64;
                }
                fe
            })
            .collect();

        // Directories first, then files; each group sorted case-insensitively.
        entries.sort_by(|a, b| match (a.entry_type, b.entry_type) {
            (EntryType::Directory, EntryType::File) => std::cmp::Ordering::Less,
            (EntryType::File, EntryType::Directory) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });
        Ok(entries)
    }

    fn read_file(&mut self, entry: &FileEntry) -> Result<Vec<u8>, FilesystemError> {
        if entry.is_directory() {
            return Err(FilesystemError::NotADirectory(format!(
                "{} is a directory",
                entry.path
            )));
        }
        self.read_extent(entry.location as u32, entry.size as u32)
    }

    fn read_file_range(
        &mut self,
        entry: &FileEntry,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, FilesystemError> {
        if entry.is_directory() {
            return Err(FilesystemError::NotADirectory(format!(
                "{} is a directory",
                entry.path
            )));
        }
        let actual = length.min(entry.size.saturating_sub(offset) as usize);
        if actual == 0 {
            return Ok(Vec::new());
        }
        let byte = self.base + entry.location * SECTOR_SIZE + offset;
        self.reader
            .read_bytes(byte, actual)
            .map_err(sector_to_fs_err)
    }

    fn read_resource_fork(
        &mut self,
        _entry: &FileEntry,
    ) -> Result<Option<Vec<u8>>, FilesystemError> {
        Ok(None)
    }

    fn read_resource_fork_range(
        &mut self,
        _entry: &FileEntry,
        _offset: u64,
        _length: usize,
    ) -> Result<Option<Vec<u8>>, FilesystemError> {
        Ok(None)
    }

    fn volume_name(&self) -> Option<&str> {
        None
    }
}

// ── Directory-tree traversal ──────────────────────────────────────────────────

/// A raw XDVDFS directory entry (before path/`FileEntry` construction).
struct RawEntry {
    name: String,
    start_sector: u32,
    size: u32,
    is_dir: bool,
}

/// Traverse the binary tree packed into a directory extent, returning every
/// entry. Cycles and out-of-bounds offsets are skipped; traversal is capped at
/// [`MAX_DIR_ENTRIES`].
fn walk_dir_tree(data: &[u8]) -> Vec<RawEntry> {
    let mut out = Vec::new();
    if data.len() < 14 {
        return out;
    }
    let mut stack: Vec<u16> = vec![0];
    let mut visited: HashSet<u16> = HashSet::new();

    while let Some(woff) = stack.pop() {
        // 0xFFFF is the canonical "no child" sentinel; a 0 child offset is also
        // treated as empty (only the root legitimately lives at offset 0, and it
        // is seeded directly rather than pointed to).
        if woff == 0xFFFF {
            continue;
        }
        if !visited.insert(woff) || visited.len() > MAX_DIR_ENTRIES {
            continue;
        }
        let boff = woff as usize * 4;
        if boff + 14 > data.len() {
            continue;
        }
        let left = le16(data, boff);
        let right = le16(data, boff + 2);
        let start_sector = le32(data, boff + 4);
        let size = le32(data, boff + 8);
        let attrs = data[boff + 0x0C];
        let nlen = data[boff + 0x0D] as usize;
        if boff + 14 + nlen > data.len() {
            continue;
        }
        let name = latin1(&data[boff + 14..boff + 14 + nlen]);
        if !name.is_empty() && name != "." && name != ".." {
            out.push(RawEntry {
                name,
                start_sector,
                size,
                is_dir: attrs & ATTR_DIRECTORY != 0,
            });
        }
        if left != 0 {
            stack.push(left);
        }
        if right != 0 {
            stack.push(right);
        }
    }
    out
}

// ── Small helpers ─────────────────────────────────────────────────────────────

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Decode a Latin-1 filename field to a `String` (Xbox names are 8-bit).
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

fn sector_to_fs_err(e: crate::error::OpticaldiscsError) -> FilesystemError {
    match e {
        crate::error::OpticaldiscsError::Io(io_err) => FilesystemError::Io(io_err),
        e => FilesystemError::InvalidData(e.to_string()),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Seek, SeekFrom};

    struct CursorReader(Cursor<Vec<u8>>);
    impl SectorReader for CursorReader {
        fn read_sector(&mut self, lba: u64) -> crate::error::Result<Vec<u8>> {
            self.0
                .seek(SeekFrom::Start(lba * SECTOR_SIZE))
                .map_err(crate::error::OpticaldiscsError::Io)?;
            let mut buf = vec![0u8; SECTOR_SIZE as usize];
            // Zero-fill past EOF rather than erroring.
            let _ = self.0.read(&mut buf);
            Ok(buf)
        }
    }

    /// Encode one directory entry at 4-byte-aligned `word_off` into `buf`.
    #[allow(clippy::too_many_arguments)]
    fn put_entry(
        buf: &mut [u8],
        word_off: u16,
        left: u16,
        right: u16,
        start: u32,
        size: u32,
        is_dir: bool,
        name: &str,
    ) {
        let o = word_off as usize * 4;
        buf[o..o + 2].copy_from_slice(&left.to_le_bytes());
        buf[o + 2..o + 4].copy_from_slice(&right.to_le_bytes());
        buf[o + 4..o + 8].copy_from_slice(&start.to_le_bytes());
        buf[o + 8..o + 12].copy_from_slice(&size.to_le_bytes());
        buf[o + 0x0C] = if is_dir { ATTR_DIRECTORY } else { 0 };
        buf[o + 0x0D] = name.len() as u8;
        buf[o + 14..o + 14 + name.len()].copy_from_slice(name.as_bytes());
    }

    /// Build a minimal XDVDFS image (base 0) with a root directory holding a
    /// file and a subdirectory; the subdirectory holds one file.
    fn build_image() -> Vec<u8> {
        const SEC: usize = SECTOR_SIZE as usize;
        // Sectors: 32 = VD, 33 = root dir, 34 = subdir, 35 = file data,
        // 36 = subdir file data. Size the image to 40 sectors.
        let mut img = vec![0u8; 40 * SEC];

        // Volume descriptor at 0x10000 (sector 32): magic, root sector 33, size.
        let vd = 0x10000usize;
        img[vd..vd + 20].copy_from_slice(XDVDFS_MAGIC);
        img[vd + 0x14..vd + 0x18].copy_from_slice(&33u32.to_le_bytes());
        img[vd + 0x18..vd + 0x1C].copy_from_slice(&(SEC as u32).to_le_bytes());

        // Root dir (sector 33): tree root = "SUBDIR" (dir), right child "GAME.XBE".
        // "SUBDIR" needs 14 + 6 = 20 bytes = 5 words, so the right child starts at
        // word offset 5 (byte 20) to avoid overlapping the root entry's name.
        let mut root = vec![0u8; SEC];
        put_entry(&mut root, 0, 0xFFFF, 5, 34, SEC as u32, true, "SUBDIR");
        put_entry(&mut root, 5, 0xFFFF, 0xFFFF, 35, 11, false, "GAME.XBE");
        img[33 * SEC..34 * SEC].copy_from_slice(&root);

        // Subdir (sector 34): one file "INNER.BIN".
        let mut sub = vec![0u8; SEC];
        put_entry(&mut sub, 0, 0xFFFF, 0xFFFF, 36, 5, false, "INNER.BIN");
        img[34 * SEC..35 * SEC].copy_from_slice(&sub);

        // File data.
        img[35 * SEC..35 * SEC + 11].copy_from_slice(b"hello world");
        img[36 * SEC..36 * SEC + 5].copy_from_slice(b"inner");
        img
    }

    #[test]
    fn browses_tree_and_reads_files() {
        let img = build_image();
        let mut fs = XdvdfsFilesystem::new(Box::new(CursorReader(Cursor::new(img)))).unwrap();

        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        // Directory first, then file.
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "SUBDIR");
        assert!(entries[0].is_directory());
        assert_eq!(entries[1].name, "GAME.XBE");
        assert!(entries[1].is_file());
        assert_eq!(entries[1].size, 11);

        // Read the root file.
        assert_eq!(fs.read_file(&entries[1]).unwrap(), b"hello world");
        // Ranged read.
        assert_eq!(fs.read_file_range(&entries[1], 6, 5).unwrap(), b"world");

        // Descend into the subdirectory.
        let sub = fs.list_directory(&entries[0]).unwrap();
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].name, "INNER.BIN");
        assert_eq!(sub[0].path, "/SUBDIR/INNER.BIN");
        assert_eq!(fs.read_file(&sub[0]).unwrap(), b"inner");
    }

    #[test]
    fn detects_base_and_finds_root_file() {
        let img = build_image();
        let mut r = CursorReader(Cursor::new(img));
        assert_eq!(detect(&mut r), Some(0));
        assert_eq!(root_file_extent(&mut r, 0, "game.xbe"), Some((35, 11)));
        assert_eq!(root_file_extent(&mut r, 0, "missing"), None);
    }

    #[test]
    fn rejects_non_xdvdfs() {
        let img = vec![0u8; 40 * SECTOR_SIZE as usize];
        assert!(XdvdfsFilesystem::new(Box::new(CursorReader(Cursor::new(img)))).is_err());
    }
}
