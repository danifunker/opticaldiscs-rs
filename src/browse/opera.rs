//! 3DO Opera filesystem browser.
//!
//! The 3DO "Opera" filesystem is not ISO 9660. Everything is big-endian. Layout
//! (verified against real 3DO discs):
//!
//! - **Volume header** at block 0: record type `1`, five `0x5A` sync bytes, a
//!   32-byte volume label at offset 40, the block size at 0x4C, and the root
//!   directory's block location at 0x64 (all big-endian). Byte offset of a block
//!   is `block * block_size`.
//! - **Directory block**: a 20-byte header (`next`, `prev`, `flags`,
//!   `content_end`, `first_entry`) followed by variable-length entries in
//!   `[first_entry, content_end)`.
//! - **Directory entry**: 68-byte fixed part (`flags`, id, 4-char `type` — `*dir`
//!   marks a directory — block size, `byte_count`, length-in-blocks, burst, gap,
//!   32-byte filename, then a `copies` count) followed by `copies + 1` 4-byte
//!   avatar (block-pointer) entries; the first avatar is the data's start block.
//!
//! Reference: `docs/GameDiscs_Implementation.md` (Phase G5); barbeque/3dodump
//! `OperaFS-Format.md`.

use super::entry::{EntryType, FileEntry};
use super::filesystem::{Filesystem, FilesystemError};
use crate::sector_reader::SectorReader;

/// Fixed portion of a directory entry, before the avatar (block-pointer) array.
const ENTRY_FIXED: usize = 68;

/// Return `true` if `reader` presents a 3DO Opera volume (block 0 header).
pub fn detect_opera(reader: &mut dyn SectorReader) -> bool {
    match reader.read_sector(0) {
        Ok(b) => b.len() >= 6 && b[0] == 0x01 && b[1..6] == [0x5A; 5],
        Err(_) => false,
    }
}

/// Read the 3DO Opera volume label (block 0, offset 40), if present.
pub fn read_label(reader: &mut dyn SectorReader) -> Option<String> {
    let vh = reader.read_sector(0).ok()?;
    if vh.len() < 72 || vh[0] != 0x01 || vh[1..6] != [0x5A; 5] {
        return None;
    }
    let s = ascii_field(&vh[40..72]);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 3DO Opera filesystem browser.
pub struct OperaFilesystem {
    reader: Box<dyn SectorReader>,
    block_size: u32,
    root_block: u32,
    root_blocks: u32,
    label: String,
}

impl OperaFilesystem {
    /// Create a browser by reading the Opera volume header from `reader`.
    ///
    /// # Errors
    ///
    /// Returns [`FilesystemError::Parse`] if the volume header is missing or
    /// malformed.
    pub fn new(mut reader: Box<dyn SectorReader>) -> Result<Self, FilesystemError> {
        let vh = reader
            .read_sector(0)
            .map_err(|e| FilesystemError::Io(io_of(e)))?;
        if vh.len() < 0x6C || vh[0] != 0x01 || vh[1..6] != [0x5A; 5] {
            return Err(FilesystemError::Parse("not a 3DO Opera volume".into()));
        }
        let label = ascii_field(&vh[40..72]);
        let block_size = be32(&vh, 0x4C);
        let root_blocks = be32(&vh, 0x58);
        let root_block = be32(&vh, 0x64);
        if block_size == 0 {
            return Err(FilesystemError::Parse("Opera block size is zero".into()));
        }
        Ok(Self {
            reader,
            block_size,
            root_block,
            root_blocks,
            label,
        })
    }

    /// Read `count` blocks starting at `start_block` into one buffer.
    fn read_blocks(&mut self, start_block: u32, count: u32) -> Result<Vec<u8>, FilesystemError> {
        let byte_off = start_block as u64 * self.block_size as u64;
        let len = count.max(1) as usize * self.block_size as usize;
        self.reader
            .read_bytes(byte_off, len)
            .map_err(|e| FilesystemError::Io(io_of(e)))
    }

    /// Parse all entries of a directory occupying `blocks` blocks at `start`.
    fn read_dir(
        &mut self,
        start: u32,
        blocks: u32,
        base_path: &str,
    ) -> Result<Vec<FileEntry>, FilesystemError> {
        let bs = self.block_size as usize;
        let data = self.read_blocks(start, blocks)?;
        let mut out = Vec::new();

        // Each block self-describes its used range via its 20-byte header.
        let nblocks = (data.len() / bs).max(1);
        for b in 0..nblocks {
            let block = &data[b * bs..(b * bs + bs).min(data.len())];
            if block.len() < 20 {
                break;
            }
            let content_end = (be32(block, 12) as usize).min(block.len());
            let first_entry = be32(block, 16) as usize;
            let mut pos = first_entry;
            while pos + ENTRY_FIXED <= content_end {
                let flags = be32(block, pos);
                let type_tag = &block[pos + 8..pos + 12];
                let byte_count = be32(block, pos + 16);
                let name = ascii_field(&block[pos + 32..pos + 64]);
                let copies = be32(block, pos + 64);
                // First avatar (block pointer) follows the fixed part.
                let avatar0 = be32(block, pos + ENTRY_FIXED);
                let entry_len = ENTRY_FIXED + (copies as usize + 1) * 4;

                let is_dir = type_tag == b"*dir";
                // Skip the disc-label / special metadata pseudo-files but keep
                // ordinary files and directories.
                if !name.is_empty() {
                    let path = format!("{}/{}", base_path.trim_end_matches('/'), name);
                    out.push(make_entry(name, path, avatar0, byte_count, is_dir));
                }

                pos += entry_len;
                // Last entry in this directory — stop entirely.
                if flags & 0x8000_0000 != 0 {
                    return Ok(out);
                }
                // Last entry in this block — move to the next block.
                if flags & 0x4000_0000 != 0 {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Number of blocks a directory of `byte_size` bytes occupies.
    fn blocks_for(&self, byte_size: u32) -> u32 {
        byte_size.div_ceil(self.block_size).max(1)
    }
}

impl Filesystem for OperaFilesystem {
    fn root(&mut self) -> Result<FileEntry, FilesystemError> {
        Ok(make_entry(
            String::new(),
            "/".into(),
            self.root_block,
            self.root_blocks * self.block_size,
            true,
        ))
    }

    fn list_directory(&mut self, entry: &FileEntry) -> Result<Vec<FileEntry>, FilesystemError> {
        if entry.entry_type != EntryType::Directory {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        let blocks = self.blocks_for(entry.size as u32);
        let path = entry.path.clone();
        self.read_dir(entry.location as u32, blocks, &path)
    }

    fn read_file(&mut self, entry: &FileEntry) -> Result<Vec<u8>, FilesystemError> {
        let byte_off = entry.location * self.block_size as u64;
        self.reader
            .read_bytes(byte_off, entry.size as usize)
            .map_err(|e| FilesystemError::Io(io_of(e)))
    }

    fn read_file_range(
        &mut self,
        entry: &FileEntry,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, FilesystemError> {
        let clamped = length.min(entry.size.saturating_sub(offset) as usize);
        let byte_off = entry.location * self.block_size as u64 + offset;
        self.reader
            .read_bytes(byte_off, clamped)
            .map_err(|e| FilesystemError::Io(io_of(e)))
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
        if self.label.is_empty() {
            None
        } else {
            Some(&self.label)
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_entry(name: String, path: String, block: u32, size: u32, is_dir: bool) -> FileEntry {
    FileEntry {
        name,
        path,
        entry_type: if is_dir {
            EntryType::Directory
        } else {
            EntryType::File
        },
        size: size as u64,
        location: block as u64,
        children: None,
        resource_fork_size: None,
        type_code: None,
        creator_code: None,
        finder_flags: None,
        symlink_target: None,
        timestamps: None,
        posix: None,
    }
}

fn ascii_field(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn io_of(e: crate::error::OpticaldiscsError) -> std::io::Error {
    match e {
        crate::error::OpticaldiscsError::Io(io) => io,
        other => std::io::Error::other(other.to_string()),
    }
}
