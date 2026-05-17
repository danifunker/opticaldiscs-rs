//! SGI EFS filesystem browser.
//!
//! Implements the [`Filesystem`] trait on top of an [`EfsSuperblock`] and a
//! [`SectorReader`]. Provides read-only directory listing, file reads, and
//! symlink target resolution.
//!
//! EFS stores up to 12 extents inline in the inode. Files needing more than
//! 12 extents switch to **indirect** mode: the inline slots describe runs of
//! disk blocks containing arrays of `EfsExtent` records (64 per 512-byte
//! block); `numextents` is the total data-extent count and
//! `extents[0].offset` holds `direxts`, the number of inline slots actually
//! used. See Linux `fs/efs/inode.c::efs_map_block`.

use super::entry::{EntryType, FileEntry};
use super::filesystem::{Filesystem, FilesystemError};
use crate::efs::{
    inode_byte_offset, EfsExtent, EfsInode, EfsSuperblock, EFS_BLOCKSIZE, EFS_DIRBLK_HEADERSIZE,
    EFS_DIRBLK_MAGIC, EFS_DIRECTEXTENTS, EFS_EXTENTS_PER_BLOCK, EFS_ROOT_INODE,
};
use crate::error::OpticaldiscsError;
use crate::sector_reader::SectorReader;

/// EFS filesystem browser.
///
/// Created via [`open_disc_filesystem`][crate::browse::open_disc_filesystem]
/// when the detected filesystem type is
/// [`FilesystemType::Efs`][crate::formats::FilesystemType::Efs].
pub struct EfsFilesystem {
    reader: Box<dyn SectorReader>,
    /// Byte offset of the EFS partition from the disc start.
    partition_offset: u64,
    sb: EfsSuperblock,
    label: String,
}

impl EfsFilesystem {
    /// Open an EFS filesystem at the given byte offset within `reader`.
    ///
    /// Reads the superblock from `partition_offset + 512`.
    pub fn new(
        mut reader: Box<dyn SectorReader>,
        partition_offset: u64,
    ) -> Result<Self, FilesystemError> {
        let sb_bytes = reader
            .read_bytes(partition_offset + EFS_BLOCKSIZE, EFS_BLOCKSIZE as usize)
            .map_err(disc_err)?;
        let sb = EfsSuperblock::parse(&sb_bytes).map_err(disc_err)?;
        let label = sb.label();
        Ok(EfsFilesystem {
            reader,
            partition_offset,
            sb,
            label,
        })
    }

    /// Read one EFS inode by number.
    fn read_inode(&mut self, inum: u32) -> Result<EfsInode, FilesystemError> {
        if inum == 0 {
            return Err(FilesystemError::InvalidData(
                "EFS inode 0 is reserved".into(),
            ));
        }
        let off = inode_byte_offset(&self.sb, inum);
        let block_byte = (off / EFS_BLOCKSIZE) * EFS_BLOCKSIZE;
        let block = self
            .reader
            .read_bytes(self.partition_offset + block_byte, EFS_BLOCKSIZE as usize)
            .map_err(disc_err)?;
        let in_block = (off - block_byte) as usize;
        let mut ino_buf = [0u8; 128];
        ino_buf.copy_from_slice(&block[in_block..in_block + 128]);
        Ok(EfsInode::parse(inum, &ino_buf))
    }

    /// Read one 512-byte directory block at disk block `bn`.
    ///
    /// Returns `Ok(Some(buf))` on a valid block, `Ok(None)` when the block is
    /// truncated or carries no `EFS_DIRBLK_MAGIC` (damaged / past EOF).
    fn read_dir_block(
        &mut self,
        bn: u32,
    ) -> Result<Option<[u8; EFS_BLOCKSIZE as usize]>, FilesystemError> {
        let byte = bn as u64 * EFS_BLOCKSIZE;
        let res = self
            .reader
            .read_bytes(self.partition_offset + byte, EFS_BLOCKSIZE as usize);
        let v = match res {
            Ok(v) => v,
            // Truncated images return EOF — surface as "no block", not an error.
            Err(OpticaldiscsError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(None);
            }
            Err(e) => return Err(disc_err(e)),
        };
        if v.len() < EFS_BLOCKSIZE as usize {
            return Ok(None);
        }
        let magic = u16::from_be_bytes([v[0], v[1]]);
        if magic != EFS_DIRBLK_MAGIC {
            return Ok(None);
        }
        let mut buf = [0u8; EFS_BLOCKSIZE as usize];
        buf.copy_from_slice(&v);
        Ok(Some(buf))
    }

    /// Resolve an inode's data extents in file (logical) order, handling both
    /// direct mode (`numextents <= 12`) and indirect mode (`numextents > 12`,
    /// where the inline slots point at runs of indirect-extent blocks and
    /// `extents[0].offset` holds the number of inline slots used).
    fn resolve_data_extents(
        &mut self,
        inode: &EfsInode,
    ) -> Result<Vec<EfsExtent>, FilesystemError> {
        let total = inode.numextents as usize;
        if total <= EFS_DIRECTEXTENTS {
            let mut out: Vec<EfsExtent> = inode.extents.iter().take(total).copied().collect();
            out.sort_by_key(|e| e.offset);
            return Ok(out);
        }
        let direxts = inode.extents[0].offset as usize;
        if direxts == 0 || direxts > EFS_DIRECTEXTENTS {
            return Err(FilesystemError::InvalidData(format!(
                "EFS inode {} indirect mode: bad direxts={direxts} (numextents={total})",
                inode.inum
            )));
        }
        let mut out: Vec<EfsExtent> = Vec::with_capacity(total);
        'collect: for dirslot in 0..direxts {
            let ind = inode.extents[dirslot];
            for blk in 0..ind.length as u32 {
                let bn = ind.bn + blk;
                let byte = bn as u64 * EFS_BLOCKSIZE;
                let block = self
                    .reader
                    .read_bytes(self.partition_offset + byte, EFS_BLOCKSIZE as usize)
                    .map_err(disc_err)?;
                if block.len() < EFS_BLOCKSIZE as usize {
                    return Err(FilesystemError::InvalidData(format!(
                        "EFS inode {} indirect block {bn}: short read ({} bytes)",
                        inode.inum,
                        block.len()
                    )));
                }
                for slot in 0..EFS_EXTENTS_PER_BLOCK {
                    let off = slot * 8;
                    let raw: &[u8; 8] = (&block[off..off + 8]).try_into().unwrap();
                    let ext = EfsExtent::parse(raw);
                    if ext.magic != 0 {
                        return Err(FilesystemError::InvalidData(format!(
                            "EFS inode {} indirect extent slot {slot} in block {bn}: bad magic 0x{:02X}",
                            inode.inum, ext.magic
                        )));
                    }
                    out.push(ext);
                    if out.len() == total {
                        break 'collect;
                    }
                }
            }
        }
        if out.len() != total {
            return Err(FilesystemError::InvalidData(format!(
                "EFS inode {} indirect: collected {} of {total} extents",
                inode.inum,
                out.len()
            )));
        }
        out.sort_by_key(|e| e.offset);
        Ok(out)
    }

    /// Walk an inode's data extents (direct or indirect) and copy `length`
    /// bytes starting at `start_off`. Returns the bytes copied (may be less
    /// than `length` if the requested range extends past file end).
    fn read_extent_range(
        &mut self,
        inode: &EfsInode,
        start_off: u64,
        length: usize,
    ) -> Result<Vec<u8>, FilesystemError> {
        let file_size = inode.size as u64;
        if start_off >= file_size {
            return Ok(Vec::new());
        }
        let end = (start_off + length as u64).min(file_size);
        let want = (end - start_off) as usize;
        let mut out = Vec::with_capacity(want);

        let extents = self.resolve_data_extents(inode)?;

        for ext in &extents {
            // Logical byte range this extent covers in the file.
            let ext_start = ext.offset as u64 * EFS_BLOCKSIZE;
            let ext_end = ext_start + ext.length as u64 * EFS_BLOCKSIZE;
            if ext_end <= start_off || ext_start >= end {
                continue;
            }
            // Translate to disk and to the (start_off, end) window.
            let win_start = start_off.max(ext_start);
            let win_end = end.min(ext_end);
            let disk_byte = ext.bn as u64 * EFS_BLOCKSIZE + (win_start - ext_start);
            let len = (win_end - win_start) as usize;
            let bytes = self
                .reader
                .read_bytes(self.partition_offset + disk_byte, len)
                .map_err(disc_err)?;
            out.extend_from_slice(&bytes);
            if out.len() >= want {
                break;
            }
        }

        Ok(out)
    }

    fn read_symlink_target(&mut self, inode: &EfsInode) -> Option<String> {
        let bytes = self.read_extent_range(inode, 0, inode.size as usize).ok()?;
        Some(
            String::from_utf8_lossy(&bytes)
                .trim_end_matches('\0')
                .to_string(),
        )
    }
}

/// Decode one EFS directory block and emit `(inum, name)` pairs.
///
/// Header: `magic:u16, firstused:u8, slots:u8`. Each non-zero slot byte is
/// shifted left by 1 to produce the dirent offset within the block. Each
/// dirent is `inumber:u32, namelen:u8, name[namelen]`.
fn iter_dir_block<F>(buf: &[u8; EFS_BLOCKSIZE as usize], mut emit: F)
where
    F: FnMut(u32, &str),
{
    let slots = buf[3] as usize;
    let max_slots = (EFS_BLOCKSIZE as usize - EFS_DIRBLK_HEADERSIZE).min(slots);
    for slot in 0..max_slots {
        let raw = buf[EFS_DIRBLK_HEADERSIZE + slot];
        if raw == 0 {
            continue;
        }
        let off = (raw as usize) << 1;
        if off + 5 > EFS_BLOCKSIZE as usize {
            continue;
        }
        let inum = u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        let namelen = buf[off + 4] as usize;
        if namelen == 0 || off + 5 + namelen > EFS_BLOCKSIZE as usize {
            continue;
        }
        let name_bytes = &buf[off + 5..off + 5 + namelen];
        let name = String::from_utf8_lossy(name_bytes);
        emit(inum, &name);
    }
}

impl Filesystem for EfsFilesystem {
    fn root(&mut self) -> Result<FileEntry, FilesystemError> {
        let ino = self.read_inode(EFS_ROOT_INODE)?;
        if !ino.is_dir() {
            return Err(FilesystemError::InvalidData(format!(
                "EFS root inode 2 is not a directory (mode=0o{:o})",
                ino.mode
            )));
        }
        Ok(FileEntry::root(EFS_ROOT_INODE as u64))
    }

    fn list_directory(&mut self, entry: &FileEntry) -> Result<Vec<FileEntry>, FilesystemError> {
        if entry.entry_type != EntryType::Directory {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        let inum = entry.location as u32;
        let dir_ino = self.read_inode(inum)?;
        if !dir_ino.is_dir() {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }

        let extents = self.resolve_data_extents(&dir_ino)?;

        let mut entries: Vec<FileEntry> = Vec::new();
        for ext in &extents {
            for i in 0..ext.length as u32 {
                let bn = ext.bn + i;
                let block = match self.read_dir_block(bn)? {
                    Some(b) => b,
                    None => continue,
                };
                let mut pending: Vec<(u32, String)> = Vec::new();
                iter_dir_block(&block, |inum, name| {
                    pending.push((inum, name.to_string()));
                });
                for (child_inum, name) in pending {
                    if name == "." || name == ".." || child_inum == 0 {
                        continue;
                    }
                    let path = if entry.path == "/" {
                        format!("/{name}")
                    } else {
                        format!("{}/{}", entry.path, name)
                    };
                    match self.read_inode(child_inum) {
                        Ok(child) => {
                            let symlink_target = if child.is_symlink() {
                                self.read_symlink_target(&child)
                            } else {
                                None
                            };
                            let mut e = if child.is_dir() {
                                FileEntry::new_directory(name, path, child_inum as u64)
                            } else {
                                FileEntry::new_file(
                                    name,
                                    path,
                                    child.size as u64,
                                    child_inum as u64,
                                )
                            };
                            e.symlink_target = symlink_target;
                            entries.push(e);
                        }
                        Err(_) => {
                            // Damaged inode block — surface the name as a
                            // zero-size placeholder so listings stay usable.
                            entries.push(FileEntry::new_file(name, path, 0, child_inum as u64));
                        }
                    }
                }
            }
        }
        Ok(entries)
    }

    fn read_file(&mut self, entry: &FileEntry) -> Result<Vec<u8>, FilesystemError> {
        if entry.entry_type != EntryType::File {
            return Err(FilesystemError::NotADirectory(format!(
                "{} is not a file",
                entry.path
            )));
        }
        let inum = entry.location as u32;
        let inode = self.read_inode(inum)?;
        self.read_extent_range(&inode, 0, inode.size as usize)
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
        let inum = entry.location as u32;
        let inode = self.read_inode(inum)?;
        self.read_extent_range(&inode, offset, length)
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

fn disc_err(e: OpticaldiscsError) -> FilesystemError {
    match e {
        OpticaldiscsError::Io(io_err) => FilesystemError::Io(io_err),
        e => FilesystemError::InvalidData(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::efs::EFS_MAGIC_OLD;
    use crate::error::Result as OResult;
    use crate::sector_reader::SECTOR_SIZE;
    use std::io::{Cursor, Read, Seek, SeekFrom};

    struct CursorReader(Cursor<Vec<u8>>);
    impl SectorReader for CursorReader {
        fn read_sector(&mut self, lba: u64) -> OResult<Vec<u8>> {
            self.0
                .seek(SeekFrom::Start(lba * SECTOR_SIZE))
                .map_err(OpticaldiscsError::Io)?;
            let mut buf = vec![0u8; SECTOR_SIZE as usize];
            self.0.read_exact(&mut buf).map_err(OpticaldiscsError::Io)?;
            Ok(buf)
        }
    }

    /// Build a tiny synthetic EFS image with one regular file "data" (inode 4),
    /// one symlink "link" -> "/usr/sbin/init" (inode 5), and one subdirectory
    /// "sub" (inode 6) containing one file "nested" (inode 7).
    ///
    /// Layout (512-byte blocks):
    ///   0  : pad
    ///   1  : superblock
    ///   2..18 : pad to firstcg
    ///   18,19 : inode area (cgisize=2 -> 8 inodes; we use 2,4,5,6,7)
    ///   20 : root directory block
    ///   21 : "data" file content (one block, 0xAA fill)
    ///   22 : "link" symlink target text
    ///   23 : "sub" subdirectory block
    ///   24 : "nested" content (one block, 0xBB fill)
    fn build_synth_image() -> Vec<u8> {
        let mut img = vec![0u8; 64 * 512];

        // Superblock at sector 1.
        let sb_off = 512;
        let total_blocks = (img.len() / 512) as u32;
        img[sb_off..sb_off + 4].copy_from_slice(&total_blocks.to_be_bytes()); // fs_size
        img[sb_off + 4..sb_off + 8].copy_from_slice(&18u32.to_be_bytes()); // firstcg
        img[sb_off + 8..sb_off + 12].copy_from_slice(&14u32.to_be_bytes()); // cgfsize
        img[sb_off + 12..sb_off + 14].copy_from_slice(&2u16.to_be_bytes()); // cgisize
        img[sb_off + 18..sb_off + 20].copy_from_slice(&1u16.to_be_bytes()); // ncg
        img[sb_off + 28..sb_off + 32].copy_from_slice(&EFS_MAGIC_OLD.to_be_bytes());
        img[sb_off + 32..sb_off + 38].copy_from_slice(b"synth\0");
        img[sb_off + 38..sb_off + 44].copy_from_slice(b"pack\0\0");

        // Helper to write an inode at (inum) within the inode area.
        // inodes_per_cg = 2*4 = 8, so inum<=7 lives in cg 0, block (inum/4)
        // from start of inode area, byte (inum%4)*128 within that block.
        let inode_off = |inum: u32| -> usize {
            let block = 18u32 + inum / 4;
            (block as usize) * 512 + ((inum % 4) as usize) * 128
        };

        // Inode 2: root directory, one extent -> block 20 (length=1).
        let off = inode_off(2);
        img[off..off + 2].copy_from_slice(&0o040755u16.to_be_bytes());
        img[off + 8..off + 12].copy_from_slice(&512u32.to_be_bytes());
        img[off + 28..off + 30].copy_from_slice(&1u16.to_be_bytes());
        img[off + 32..off + 36].copy_from_slice(&20u32.to_be_bytes()); // bn=20
        img[off + 36..off + 40].copy_from_slice(&(1u32 << 24).to_be_bytes()); // len=1, off=0

        // Inode 4: regular file, size 512, extent at block 21.
        let off = inode_off(4);
        img[off..off + 2].copy_from_slice(&0o100644u16.to_be_bytes());
        img[off + 8..off + 12].copy_from_slice(&512u32.to_be_bytes());
        img[off + 28..off + 30].copy_from_slice(&1u16.to_be_bytes());
        img[off + 32..off + 36].copy_from_slice(&21u32.to_be_bytes());
        img[off + 36..off + 40].copy_from_slice(&(1u32 << 24).to_be_bytes());

        // Inode 5: symlink with target "/usr/sbin/init".
        let target = b"/usr/sbin/init";
        let off = inode_off(5);
        img[off..off + 2].copy_from_slice(&0o120777u16.to_be_bytes());
        img[off + 8..off + 12].copy_from_slice(&(target.len() as u32).to_be_bytes());
        img[off + 28..off + 30].copy_from_slice(&1u16.to_be_bytes());
        img[off + 32..off + 36].copy_from_slice(&22u32.to_be_bytes());
        img[off + 36..off + 40].copy_from_slice(&(1u32 << 24).to_be_bytes());

        // Inode 6: subdirectory, one extent -> block 23.
        let off = inode_off(6);
        img[off..off + 2].copy_from_slice(&0o040755u16.to_be_bytes());
        img[off + 8..off + 12].copy_from_slice(&512u32.to_be_bytes());
        img[off + 28..off + 30].copy_from_slice(&1u16.to_be_bytes());
        img[off + 32..off + 36].copy_from_slice(&23u32.to_be_bytes());
        img[off + 36..off + 40].copy_from_slice(&(1u32 << 24).to_be_bytes());

        // Inode 7: nested regular file, size 512, extent at block 24.
        let off = inode_off(7);
        img[off..off + 2].copy_from_slice(&0o100644u16.to_be_bytes());
        img[off + 8..off + 12].copy_from_slice(&512u32.to_be_bytes());
        img[off + 28..off + 30].copy_from_slice(&1u16.to_be_bytes());
        img[off + 32..off + 36].copy_from_slice(&24u32.to_be_bytes());
        img[off + 36..off + 40].copy_from_slice(&(1u32 << 24).to_be_bytes());

        // Root directory block at block 20: 3 entries "data"->4, "link"->5,
        // "sub"->6. Dirents written tail-first; slot bytes are dirent_offset/2.
        let dir_off = 20 * 512;
        img[dir_off..dir_off + 2].copy_from_slice(&EFS_DIRBLK_MAGIC.to_be_bytes());
        img[dir_off + 3] = 3; // slot count

        // dirent "data" (9 bytes: inum + namelen + 4 chars) at block byte 500
        // -> slot value 500/2 = 250 = 0xFA
        img[dir_off + 4] = 0xFA;
        let de = dir_off + 500;
        img[de..de + 4].copy_from_slice(&4u32.to_be_bytes());
        img[de + 4] = 4;
        img[de + 5..de + 9].copy_from_slice(b"data");

        // dirent "link" at block byte 490 -> 245 = 0xF5
        img[dir_off + 5] = 0xF5;
        let de = dir_off + 490;
        img[de..de + 4].copy_from_slice(&5u32.to_be_bytes());
        img[de + 4] = 4;
        img[de + 5..de + 9].copy_from_slice(b"link");

        // dirent "sub" (3 chars, 8 bytes total) at block byte 480 -> 240 = 0xF0
        img[dir_off + 6] = 0xF0;
        let de = dir_off + 480;
        img[de..de + 4].copy_from_slice(&6u32.to_be_bytes());
        img[de + 4] = 3;
        img[de + 5..de + 8].copy_from_slice(b"sub");

        // Sub-directory block at block 23: one entry "nested"->7.
        let dir_off = 23 * 512;
        img[dir_off..dir_off + 2].copy_from_slice(&EFS_DIRBLK_MAGIC.to_be_bytes());
        img[dir_off + 3] = 1;
        // "nested" is 6 chars, dirent length 11. Place at byte 500 -> 250.
        img[dir_off + 4] = 0xFA;
        let de = dir_off + 500;
        img[de..de + 4].copy_from_slice(&7u32.to_be_bytes());
        img[de + 4] = 6;
        img[de + 5..de + 11].copy_from_slice(b"nested");

        // File data
        img[21 * 512..22 * 512].fill(0xAA);
        img[22 * 512..22 * 512 + target.len()].copy_from_slice(target);
        img[24 * 512..25 * 512].fill(0xBB);

        // Pad to a multiple of SECTOR_SIZE (2048) so the cursor reader can
        // service whole-sector requests.
        let pad = SECTOR_SIZE as usize - (img.len() % SECTOR_SIZE as usize);
        if pad != SECTOR_SIZE as usize {
            img.extend(std::iter::repeat(0u8).take(pad));
        }
        img
    }

    fn open_synth() -> EfsFilesystem {
        let img = build_synth_image();
        EfsFilesystem::new(Box::new(CursorReader(Cursor::new(img))), 0).expect("open EFS")
    }

    #[test]
    fn opens_synthetic_image_with_label() {
        let fs = open_synth();
        assert_eq!(fs.volume_name(), Some("synth:pack"));
    }

    #[test]
    fn lists_root_directory() {
        let mut fs = open_synth();
        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"data"));
        assert!(names.contains(&"link"));
        assert!(names.contains(&"sub"));
        assert!(!names.contains(&"."));
        assert!(!names.contains(&".."));
    }

    #[test]
    fn reads_regular_file() {
        let mut fs = open_synth();
        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        let entry = entries.iter().find(|e| e.name == "data").unwrap();
        let bytes = fs.read_file(entry).unwrap();
        assert_eq!(bytes.len(), 512);
        assert!(bytes.iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn read_file_range_returns_window() {
        let mut fs = open_synth();
        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        let entry = entries.iter().find(|e| e.name == "data").unwrap();
        let head = fs.read_file_range(entry, 0, 16).unwrap();
        assert_eq!(head.len(), 16);
        let mid = fs.read_file_range(entry, 100, 32).unwrap();
        assert_eq!(mid.len(), 32);
        let past_end = fs.read_file_range(entry, 600, 32).unwrap();
        assert_eq!(past_end.len(), 0);
    }

    #[test]
    fn symlink_target_is_populated() {
        let mut fs = open_synth();
        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        let link = entries.iter().find(|e| e.name == "link").unwrap();
        assert_eq!(link.symlink_target.as_deref(), Some("/usr/sbin/init"));
    }

    #[test]
    fn descends_into_subdirectory() {
        let mut fs = open_synth();
        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        let sub = entries.iter().find(|e| e.name == "sub").unwrap();
        assert!(sub.is_directory());
        let children = fs.list_directory(sub).unwrap();
        let names: Vec<&str> = children.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["nested"]);
        assert_eq!(children[0].path, "/sub/nested");
        let bytes = fs.read_file(&children[0]).unwrap();
        assert!(bytes.iter().all(|&b| b == 0xBB));
    }

    #[test]
    fn resource_fork_returns_none() {
        let mut fs = open_synth();
        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        let entry = entries.iter().find(|e| e.name == "data").unwrap();
        assert!(fs.read_resource_fork(entry).unwrap().is_none());
        assert!(fs.read_resource_fork_range(entry, 0, 16).unwrap().is_none());
    }
}
