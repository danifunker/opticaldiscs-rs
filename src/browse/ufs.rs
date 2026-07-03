//! UFS / FFS (BSD Fast File System) browser — read-only.
//!
//! Handles the UFS1 on-disc format used by 4.x-BSD-derived Unixes on optical
//! media, most notably **Digital UNIX / Tru64** install CDs (little-endian, DEC
//! Alpha) and **SunOS / Solaris** media (big-endian, SPARC). The endianness is
//! detected from the superblock magic at open time; every multi-byte field is
//! then read in that orientation.
//!
//! Scope: browse + extract (list directories, read files, resolve symlinks).
//! Only UFS1 (128-byte inodes, 32-bit block pointers, superblock at byte 8192)
//! is implemented — the only variant seen on the target discs. UFS2 (256-byte
//! inodes, 64-bit pointers, superblock at 65536) is detected but rejected with a
//! clear error rather than mis-parsed.

use super::entry::{EntryType, FileEntry};
use super::filesystem::{Filesystem, FilesystemError};
use crate::sector_reader::SectorReader;

// ── On-disc constants (FreeBSD sys/ufs/ffs/fs.h, sys/ufs/ufs/dinode.h) ──────────

const SB_OFFSET_UFS1: u64 = 8192;
const SB_OFFSET_UFS2: u64 = 65536;
const SB_READ_SIZE: usize = 2048;
const MAGIC_UFS1: u32 = 0x0001_1954;
const MAGIC_UFS2: u32 = 0x1954_0119;
const MAGIC_OFF: usize = 1372;

// struct fs field offsets.
const OFF_IBLKNO: usize = 0x010; // fs_iblkno   i32 — inode-block addr in frags
const OFF_CGOFFSET: usize = 0x018; // fs_old_cgoffset i32
const OFF_CGMASK: usize = 0x01C; // fs_old_cgmask   i32
const OFF_BSIZE: usize = 0x030; // fs_bsize    i32
const OFF_FSIZE: usize = 0x034; // fs_fsize    i32
const OFF_FRAG: usize = 0x038; // fs_frag     i32 — frags per block
const OFF_IPG: usize = 0x0B8; // fs_ipg      u32 — inodes per cylinder group
const OFF_FPG: usize = 0x0BC; // fs_fpg      i32 — frags per cylinder group
const OFF_VOLNAME: usize = 680; // fs_volname[32]
const OFF_MAXSYMLINKLEN: usize = 0x528; // fs_maxsymlinklen i32

const ROOT_INODE: u64 = 2;
const DINODE1_SIZE: u64 = 128;
const UFS_NDADDR: usize = 12; // direct block pointers
const UFS_NIADDR: usize = 3; // indirect block pointers

// ufs1_dinode field offsets.
const D1_OFF_MODE: usize = 0; // di_mode  u16
const D1_OFF_SIZE: usize = 8; // di_size  u64
const D1_OFF_DB: usize = 40; // di_db[12] i32
const D1_OFF_IB: usize = 88; // di_ib[3]  i32

// File-mode type bits (S_IFMT).
const S_IFMT: u16 = 0o170000;
const S_IFDIR: u16 = 0o040000;
const S_IFLNK: u16 = 0o120000;

/// Cap on how many bytes of a directory we will read (protects against a corrupt
/// di_size). 64 MiB is far beyond any real directory.
const MAX_DIR_BYTES: u64 = 64 * 1024 * 1024;

// ── UfsFilesystem ───────────────────────────────────────────────────────────────

/// Read-only UFS1 filesystem browser over a [`SectorReader`].
pub struct UfsFilesystem {
    reader: Box<dyn SectorReader>,
    big_endian: bool,
    bsize: u64,
    fsize: u64,
    frag: u64,
    ipg: u64,
    fpg: u64,
    iblkno: i64,
    cgoffset: i64,
    cgmask: i64,
    inopb: u64,
    nindir: u64,
    /// True for the pre-4.4 directory format (`fs_maxsymlinklen <= 0`): the
    /// two bytes after `d_reclen` are a 16-bit name length, with no `d_type`.
    old_dirfmt: bool,
    /// `fs_maxsymlinklen`: symlink targets shorter than this are stored inline in
    /// the block-pointer area rather than in a data block.
    max_symlink_len: i64,
    volume_id: Option<String>,
}

/// A minimally-parsed UFS1 inode.
struct Dinode {
    mode: u16,
    size: u64,
    db: [u64; UFS_NDADDR],
    ib: [u64; UFS_NIADDR],
    /// The raw 128-byte inode, kept so inline symlink targets (stored in the
    /// block-pointer area) can be read without a second fetch.
    raw: Vec<u8>,
}

impl Dinode {
    fn is_dir(&self) -> bool {
        (self.mode & S_IFMT) == S_IFDIR
    }
    fn is_symlink(&self) -> bool {
        (self.mode & S_IFMT) == S_IFLNK
    }
}

impl UfsFilesystem {
    /// Open a UFS filesystem, auto-detecting the superblock location and
    /// endianness. Fails on UFS2 or when no UFS superblock is found.
    pub fn new(mut reader: Box<dyn SectorReader>) -> Result<Self, FilesystemError> {
        let (sb, big_endian) = Self::find_superblock(reader.as_mut())?;

        let ri = |o: usize| -> i64 {
            let b = [sb[o], sb[o + 1], sb[o + 2], sb[o + 3]];
            if big_endian {
                i32::from_be_bytes(b) as i64
            } else {
                i32::from_le_bytes(b) as i64
            }
        };
        let ru = |o: usize| -> u64 {
            let b = [sb[o], sb[o + 1], sb[o + 2], sb[o + 3]];
            if big_endian {
                u32::from_be_bytes(b) as u64
            } else {
                u32::from_le_bytes(b) as u64
            }
        };

        let bsize = ri(OFF_BSIZE) as u64;
        let fsize = ri(OFF_FSIZE) as u64;
        let frag = ri(OFF_FRAG) as u64;
        let ipg = ru(OFF_IPG);
        let fpg = ri(OFF_FPG) as u64;
        if bsize == 0 || fsize == 0 || frag == 0 || ipg == 0 || fpg == 0 {
            return Err(FilesystemError::InvalidData(
                "UFS superblock has zero geometry field".into(),
            ));
        }
        let inopb = bsize / DINODE1_SIZE;
        let nindir = bsize / 4; // UFS1: 32-bit pointers
        let max_symlink_len = ri(OFF_MAXSYMLINKLEN);

        let volname = &sb[OFF_VOLNAME..OFF_VOLNAME + 32];
        let volume_id = {
            let s: String = volname
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as char)
                .collect();
            let s = s.trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };

        Ok(Self {
            reader,
            big_endian,
            bsize,
            fsize,
            frag,
            ipg,
            fpg,
            iblkno: ri(OFF_IBLKNO),
            cgoffset: ri(OFF_CGOFFSET),
            cgmask: ri(OFF_CGMASK),
            inopb,
            nindir,
            old_dirfmt: max_symlink_len <= 0,
            max_symlink_len,
            volume_id,
        })
    }

    /// Locate the superblock and determine byte order. Probes the UFS1 offset
    /// (8192) then the UFS2 offset (65536); UFS2 is detected but not supported.
    fn find_superblock(
        reader: &mut dyn SectorReader,
    ) -> Result<(Vec<u8>, bool), FilesystemError> {
        for &off in &[SB_OFFSET_UFS1, SB_OFFSET_UFS2] {
            let sb = match reader.read_bytes(off, SB_READ_SIZE) {
                Ok(b) if b.len() >= SB_READ_SIZE => b,
                _ => continue,
            };
            let m = &sb[MAGIC_OFF..MAGIC_OFF + 4];
            let le = u32::from_le_bytes([m[0], m[1], m[2], m[3]]);
            let be = u32::from_be_bytes([m[0], m[1], m[2], m[3]]);
            let big_endian = if le == MAGIC_UFS1 || le == MAGIC_UFS2 {
                false
            } else if be == MAGIC_UFS1 || be == MAGIC_UFS2 {
                true
            } else {
                continue;
            };
            let magic = if big_endian { be } else { le };
            if magic == MAGIC_UFS2 {
                return Err(FilesystemError::Unsupported);
            }
            return Ok((sb, big_endian));
        }
        Err(FilesystemError::Parse("no UFS superblock found".into()))
    }

    // ── low-level readers ──────────────────────────────────────────────────

    fn u16_at(&self, b: &[u8], o: usize) -> u16 {
        let v = [b[o], b[o + 1]];
        if self.big_endian {
            u16::from_be_bytes(v)
        } else {
            u16::from_le_bytes(v)
        }
    }
    fn u32_at(&self, b: &[u8], o: usize) -> u64 {
        let v = [b[o], b[o + 1], b[o + 2], b[o + 3]];
        if self.big_endian {
            u32::from_be_bytes(v) as u64
        } else {
            u32::from_le_bytes(v) as u64
        }
    }
    fn u64_at(&self, b: &[u8], o: usize) -> u64 {
        let mut v = [0u8; 8];
        v.copy_from_slice(&b[o..o + 8]);
        if self.big_endian {
            u64::from_be_bytes(v)
        } else {
            u64::from_le_bytes(v)
        }
    }

    /// Cylinder-group inode-area start, in fragments (FFS `cgimin`).
    fn cgimin(&self, cg: i64) -> i64 {
        let cgstart = self.fpg as i64 * cg + self.cgoffset * (cg & !self.cgmask);
        cgstart + self.iblkno
    }

    fn read_dinode(&mut self, ino: u64) -> Result<Dinode, FilesystemError> {
        let cg = (ino / self.ipg) as i64;
        let iic = ino % self.ipg;
        let fragaddr = self.cgimin(cg) as u64 + (iic / self.inopb) * self.frag;
        let byte = fragaddr * self.fsize + (iic % self.inopb) * DINODE1_SIZE;
        let raw = self
            .reader
            .read_bytes(byte, DINODE1_SIZE as usize)
            .map_err(sector_to_fs_err)?;
        if raw.len() < DINODE1_SIZE as usize {
            return Err(FilesystemError::InvalidData(format!(
                "short inode read for ino {ino}"
            )));
        }
        let mode = self.u16_at(&raw, D1_OFF_MODE);
        let size = self.u64_at(&raw, D1_OFF_SIZE);
        let mut db = [0u64; UFS_NDADDR];
        for (i, slot) in db.iter_mut().enumerate() {
            *slot = self.u32_at(&raw, D1_OFF_DB + i * 4);
        }
        let mut ib = [0u64; UFS_NIADDR];
        for (i, slot) in ib.iter_mut().enumerate() {
            *slot = self.u32_at(&raw, D1_OFF_IB + i * 4);
        }
        Ok(Dinode {
            mode,
            size,
            db,
            ib,
            raw,
        })
    }

    /// Read one indirect block (an array of `nindir` fragment pointers). A zero
    /// fragment address denotes a hole and yields all-zero pointers.
    fn read_indirect(&mut self, fragaddr: u64) -> Result<Vec<u64>, FilesystemError> {
        if fragaddr == 0 {
            return Ok(vec![0u64; self.nindir as usize]);
        }
        let block = self
            .reader
            .read_bytes(fragaddr * self.fsize, self.bsize as usize)
            .map_err(sector_to_fs_err)?;
        let mut out = Vec::with_capacity(self.nindir as usize);
        for i in 0..self.nindir as usize {
            out.push(self.u32_at(&block, i * 4));
        }
        Ok(out)
    }

    /// Map a logical block number within a file to its fragment address (0 =
    /// sparse hole). Handles direct, single-, double-, and triple-indirect blocks.
    fn block_addr(&mut self, dn: &Dinode, lbn: u64) -> Result<u64, FilesystemError> {
        if lbn < UFS_NDADDR as u64 {
            return Ok(dn.db[lbn as usize]);
        }
        let mut l = lbn - UFS_NDADDR as u64;
        let n = self.nindir;
        if l < n {
            let ind = self.read_indirect(dn.ib[0])?;
            return Ok(ind[l as usize]);
        }
        l -= n;
        if l < n * n {
            let l1 = self.read_indirect(dn.ib[1])?;
            let ind = self.read_indirect(l1[(l / n) as usize])?;
            return Ok(ind[(l % n) as usize]);
        }
        l -= n * n;
        // Triple indirect.
        let l1 = self.read_indirect(dn.ib[2])?;
        let l2 = self.read_indirect(l1[(l / (n * n)) as usize])?;
        let ind = self.read_indirect(l2[((l / n) % n) as usize])?;
        Ok(ind[(l % n) as usize])
    }

    /// Read `[start, start+len)` of the file/directory at inode `dn`, honouring
    /// sparse holes and the true `di_size`.
    fn read_range(&mut self, dn: &Dinode, start: u64, len: u64) -> Result<Vec<u8>, FilesystemError> {
        let end = (start + len).min(dn.size);
        if start >= end {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity((end - start) as usize);
        let mut pos = start;
        while pos < end {
            let lbn = pos / self.bsize;
            let within = pos % self.bsize;
            let take = (self.bsize - within).min(end - pos);
            let frag = self.block_addr(dn, lbn)?;
            if frag == 0 {
                out.resize(out.len() + take as usize, 0);
            } else {
                let chunk = self
                    .reader
                    .read_bytes(frag * self.fsize + within, take as usize)
                    .map_err(sector_to_fs_err)?;
                out.extend_from_slice(&chunk);
            }
            pos += take;
        }
        Ok(out)
    }

    fn read_whole(&mut self, dn: &Dinode) -> Result<Vec<u8>, FilesystemError> {
        self.read_range(dn, 0, dn.size)
    }

    /// Resolve a symlink target: inline (stored in the block-pointer area when
    /// `di_size <= fs_maxsymlinklen`) or from the first data block.
    fn symlink_target(&mut self, dn: &Dinode) -> Option<String> {
        let bytes = if self.max_symlink_len > 0 && dn.size <= self.max_symlink_len as u64 {
            dn.raw
                .get(D1_OFF_DB..D1_OFF_DB + dn.size as usize)
                .map(|s| s.to_vec())?
        } else {
            self.read_whole(dn).ok()?
        };
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Parse a directory extent into `(inode, name)` pairs, skipping `.`/`..`.
    fn parse_dir(&self, data: &[u8]) -> Vec<(u64, String)> {
        let mut out = Vec::new();
        let mut off = 0usize;
        while off + 8 <= data.len() {
            let ino = self.u32_at(data, off);
            let reclen = self.u16_at(data, off + 4) as usize;
            if reclen < 8 || off + reclen > data.len() {
                break;
            }
            if ino != 0 {
                let namlen = if self.old_dirfmt {
                    self.u16_at(data, off + 6) as usize
                } else {
                    data[off + 7] as usize
                };
                if off + 8 + namlen <= data.len() {
                    let name = String::from_utf8_lossy(&data[off + 8..off + 8 + namlen]).into_owned();
                    if name != "." && name != ".." && !name.is_empty() {
                        out.push((ino, name));
                    }
                }
            }
            off += reclen;
        }
        out
    }
}

impl Filesystem for UfsFilesystem {
    fn root(&mut self) -> Result<FileEntry, FilesystemError> {
        let dn = self.read_dinode(ROOT_INODE)?;
        let mut root = FileEntry::root(ROOT_INODE);
        root.size = dn.size;
        Ok(root)
    }

    fn list_directory(&mut self, entry: &FileEntry) -> Result<Vec<FileEntry>, FilesystemError> {
        if !entry.is_directory() {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        let dn = self.read_dinode(entry.location)?;
        if !dn.is_dir() {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        let want = dn.size.min(MAX_DIR_BYTES);
        let data = self.read_range(&dn, 0, want)?;

        let mut entries = Vec::new();
        for (ino, name) in self.parse_dir(&data) {
            let path = if entry.path == "/" {
                format!("/{name}")
            } else {
                format!("{}/{}", entry.path, name)
            };
            // UFS directory entries carry no size/type (old format), so read the
            // child inode to classify it and get its size.
            let child = match self.read_dinode(ino) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let mut fe = if child.is_dir() {
                let mut d = FileEntry::new_directory(name, path, ino);
                d.size = child.size;
                d
            } else {
                let mut f = FileEntry::new_file(name, path, child.size, ino);
                if child.is_symlink() {
                    f.symlink_target = self.symlink_target(&child);
                }
                f
            };
            fe.location = ino;
            entries.push(fe);
        }

        entries.sort_by(|a, b| match (a.entry_type, b.entry_type) {
            (EntryType::Directory, EntryType::File) => std::cmp::Ordering::Less,
            (EntryType::File, EntryType::Directory) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });
        Ok(entries)
    }

    fn read_file(&mut self, entry: &FileEntry) -> Result<Vec<u8>, FilesystemError> {
        if entry.is_directory() {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        let dn = self.read_dinode(entry.location)?;
        self.read_whole(&dn)
    }

    fn read_file_range(
        &mut self,
        entry: &FileEntry,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, FilesystemError> {
        if entry.is_directory() {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        let dn = self.read_dinode(entry.location)?;
        self.read_range(&dn, offset, length as u64)
    }

    fn read_resource_fork(&mut self, _entry: &FileEntry) -> Result<Option<Vec<u8>>, FilesystemError> {
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
        self.volume_id.as_deref()
    }
}

fn sector_to_fs_err(e: crate::error::OpticaldiscsError) -> FilesystemError {
    match e {
        crate::error::OpticaldiscsError::Io(io_err) => FilesystemError::Io(io_err),
        e => FilesystemError::InvalidData(e.to_string()),
    }
}

// ── Detection helper ────────────────────────────────────────────────────────────

/// Probe for a UFS1 superblock at the standard offset. Returns `true` if a UFS1
/// magic is present in either byte order. (UFS2 is intentionally not reported
/// here — it is unsupported by the browser.)
pub(crate) fn detect_ufs(reader: &mut dyn SectorReader) -> bool {
    let sb = match reader.read_bytes(SB_OFFSET_UFS1, MAGIC_OFF + 4) {
        Ok(b) if b.len() >= MAGIC_OFF + 4 => b,
        _ => return false,
    };
    let m = &sb[MAGIC_OFF..MAGIC_OFF + 4];
    let le = u32::from_le_bytes([m[0], m[1], m[2], m[3]]);
    let be = u32::from_be_bytes([m[0], m[1], m[2], m[3]]);
    le == MAGIC_UFS1 || be == MAGIC_UFS1
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny helper filesystem with only the fields `parse_dir` needs.
    fn dir_parser(big_endian: bool, old_dirfmt: bool) -> UfsFilesystem {
        UfsFilesystem {
            reader: Box::new(NullReader),
            big_endian,
            bsize: 8192,
            fsize: 1024,
            frag: 8,
            ipg: 64,
            fpg: 6480,
            iblkno: 32,
            cgoffset: 32,
            cgmask: -16,
            inopb: 64,
            nindir: 2048,
            old_dirfmt,
            max_symlink_len: 0,
            volume_id: None,
        }
    }

    struct NullReader;
    impl SectorReader for NullReader {
        fn read_sector(&mut self, _lba: u64) -> crate::error::Result<Vec<u8>> {
            Ok(vec![0u8; 2048])
        }
    }

    /// Append an old-format (u16 namlen, no d_type) directory entry.
    fn push_old(buf: &mut Vec<u8>, big_endian: bool, ino: u32, name: &str) {
        let namlen = name.len() as u16;
        let reclen = (8 + name.len()).next_multiple_of(4) as u16;
        let mut rec = vec![0u8; reclen as usize];
        if big_endian {
            rec[0..4].copy_from_slice(&ino.to_be_bytes());
            rec[4..6].copy_from_slice(&reclen.to_be_bytes());
            rec[6..8].copy_from_slice(&namlen.to_be_bytes());
        } else {
            rec[0..4].copy_from_slice(&ino.to_le_bytes());
            rec[4..6].copy_from_slice(&reclen.to_le_bytes());
            rec[6..8].copy_from_slice(&namlen.to_le_bytes());
        }
        rec[8..8 + name.len()].copy_from_slice(name.as_bytes());
        buf.extend_from_slice(&rec);
    }

    #[test]
    fn parse_dir_old_format_le() {
        let mut data = Vec::new();
        push_old(&mut data, false, 2, ".");
        push_old(&mut data, false, 2, "..");
        push_old(&mut data, false, 64, "DOCUMENTATION");
        push_old(&mut data, false, 100, "vmunix");
        let fs = dir_parser(false, true);
        let entries = fs.parse_dir(&data);
        assert_eq!(
            entries,
            vec![(64, "DOCUMENTATION".to_string()), (100, "vmunix".to_string())]
        );
    }

    #[test]
    fn parse_dir_old_format_be() {
        let mut data = Vec::new();
        push_old(&mut data, true, 2, ".");
        push_old(&mut data, true, 2, "..");
        push_old(&mut data, true, 55, "kernel");
        let fs = dir_parser(true, true);
        assert_eq!(fs.parse_dir(&data), vec![(55, "kernel".to_string())]);
    }

    /// New-format entries use d_type(1) + d_namlen(1); the parser must read
    /// namlen from offset 7.
    #[test]
    fn parse_dir_new_format() {
        let name = "bin";
        let reclen = (8 + name.len()).next_multiple_of(4) as u16;
        let mut rec = vec![0u8; reclen as usize];
        rec[0..4].copy_from_slice(&7u32.to_le_bytes()); // ino
        rec[4..6].copy_from_slice(&reclen.to_le_bytes());
        rec[6] = 4; // d_type = DT_DIR
        rec[7] = name.len() as u8; // d_namlen
        rec[8..8 + name.len()].copy_from_slice(name.as_bytes());
        let fs = dir_parser(false, false);
        assert_eq!(fs.parse_dir(&rec), vec![(7, "bin".to_string())]);
    }

    #[test]
    fn cgimin_matches_ffs_formula() {
        let fs = dir_parser(false, true);
        // cg 0: cgstart 0 + iblkno 32.
        assert_eq!(fs.cgimin(0), 32);
        // cg 1: fpg*1 + cgoffset*(1 & ~cgmask) + iblkno.
        assert_eq!(fs.cgimin(1), 6480 + 32 * (1 & !(-16i64)) + 32);
    }
}
