//! SGI EFS (Extent File System) on-disk structures.
//!
//! Read-only parsing of the EFS superblock, inodes, extents, and directory
//! blocks. This module is filesystem-agnostic with respect to I/O — it only
//! decodes byte buffers. The companion browser in [`crate::browse::efs`]
//! glues it onto a `SectorReader`.
//!
//! References:
//! - Linux kernel `fs/efs/` (last present in v5.17, March 2022).
//! - `docs/EFS_Implementation.md` — implementation plan and on-disk notes.

use crate::error::{OpticaldiscsError, Result};

/// EFS uses a fixed 512-byte block size, independent of physical sector size.
pub const EFS_BLOCKSIZE: u64 = 512;

/// On-disk inode size in bytes.
pub const EFS_INODESIZE: u64 = 128;

/// Inodes packed into one 512-byte block.
pub const EFS_INODES_PER_BLOCK: u64 = EFS_BLOCKSIZE / EFS_INODESIZE; // 4

/// Number of inline direct extents per inode. EFS has no indirect blocks, so
/// this caps the addressable file size at 12 × 16 MiB = 192 MiB.
pub const EFS_DIRECTEXTENTS: usize = 12;

/// Old EFS magic ("efs1" era).
pub const EFS_MAGIC_OLD: u32 = 0x0007_2959;
/// New EFS magic ("efs2" era).
pub const EFS_MAGIC_NEW: u32 = 0x0007_295A;

/// Directory block magic (`0xBEEF`) at byte 0 of every dir block.
pub const EFS_DIRBLK_MAGIC: u16 = 0xBEEF;
/// Directory block header size: magic (u16) + firstused (u8) + slots (u8).
pub const EFS_DIRBLK_HEADERSIZE: usize = 4;

/// Inode number of the root directory.
pub const EFS_ROOT_INODE: u32 = 2;

// ── EfsSuperblock ─────────────────────────────────────────────────────────────

/// Parsed EFS superblock. Lives at byte 512 of the partition.
///
/// The C `struct efs_super` is **not** `__attribute__((packed))`, so the
/// compiler inserts 2 bytes of natural-alignment padding between
/// `fs_dirty` (be16 at +20) and `fs_time` (be32 at +24). The magic therefore
/// sits at sb+28, not sb+26.
#[derive(Debug, Clone)]
pub struct EfsSuperblock {
    /// Total size of the filesystem in 512-byte blocks.
    pub fs_size: u32,
    /// Block number of the first cylinder group.
    pub firstcg: u32,
    /// Blocks per cylinder group.
    pub cgfsize: u32,
    /// Inode blocks per cylinder group.
    pub cgisize: u16,
    /// Sectors per track (geometry — informational).
    pub sectors: u16,
    /// Heads per cylinder (geometry — informational).
    pub heads: u16,
    /// Number of cylinder groups.
    pub ncg: u16,
    /// Dirty flag (non-zero = filesystem was not cleanly unmounted).
    pub dirty: u16,
    /// Last-mount time (seconds since Unix epoch).
    pub fs_time: u32,
    /// EFS magic (either [`EFS_MAGIC_OLD`] or [`EFS_MAGIC_NEW`]).
    pub magic: u32,
    /// Volume name (6-byte fixed field, null/space padded).
    pub fname: [u8; 6],
    /// Pack name (6-byte fixed field, null/space padded).
    pub fpack: [u8; 6],
}

impl EfsSuperblock {
    /// Parse the documented prefix of an EFS superblock from a 44+ byte buffer.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 44 {
            return Err(OpticaldiscsError::InvalidData(format!(
                "EFS superblock buffer too small: {} bytes",
                buf.len()
            )));
        }
        let magic = u32::from_be_bytes(buf[28..32].try_into().unwrap());
        if magic != EFS_MAGIC_OLD && magic != EFS_MAGIC_NEW {
            return Err(OpticaldiscsError::InvalidData(format!(
                "bad EFS magic: 0x{magic:08X} (expected 0x{EFS_MAGIC_OLD:08X} or 0x{EFS_MAGIC_NEW:08X})"
            )));
        }
        let mut fname = [0u8; 6];
        fname.copy_from_slice(&buf[32..38]);
        let mut fpack = [0u8; 6];
        fpack.copy_from_slice(&buf[38..44]);
        Ok(EfsSuperblock {
            fs_size: u32::from_be_bytes(buf[0..4].try_into().unwrap()),
            firstcg: u32::from_be_bytes(buf[4..8].try_into().unwrap()),
            cgfsize: u32::from_be_bytes(buf[8..12].try_into().unwrap()),
            cgisize: u16::from_be_bytes(buf[12..14].try_into().unwrap()),
            sectors: u16::from_be_bytes(buf[14..16].try_into().unwrap()),
            heads: u16::from_be_bytes(buf[16..18].try_into().unwrap()),
            ncg: u16::from_be_bytes(buf[18..20].try_into().unwrap()),
            dirty: u16::from_be_bytes(buf[20..22].try_into().unwrap()),
            fs_time: u32::from_be_bytes(buf[24..28].try_into().unwrap()),
            magic,
            fname,
            fpack,
        })
    }

    /// Combined "fname:fpack" volume label. Either part may be empty.
    pub fn label(&self) -> String {
        let n = trim_ascii(&self.fname);
        let p = trim_ascii(&self.fpack);
        match (n.is_empty(), p.is_empty()) {
            (true, true) => String::new(),
            (false, true) => n,
            (true, false) => p,
            (false, false) => format!("{n}:{p}"),
        }
    }
}

// ── EfsExtent ─────────────────────────────────────────────────────────────────

/// Single on-disk extent (8 bytes, big-endian).
///
/// Word 0: `magic:8 | bn:24` (`bn` = start block on disk).
/// Word 1: `length:8 | offset:24` (length in 512-byte blocks; offset in file).
#[derive(Debug, Clone, Copy)]
pub struct EfsExtent {
    pub magic: u8,
    pub bn: u32,
    pub length: u8,
    pub offset: u32,
}

impl EfsExtent {
    pub fn parse(buf: &[u8; 8]) -> Self {
        let w0 = u32::from_be_bytes(buf[0..4].try_into().unwrap());
        let w1 = u32::from_be_bytes(buf[4..8].try_into().unwrap());
        EfsExtent {
            magic: ((w0 >> 24) & 0xFF) as u8,
            bn: w0 & 0x00FF_FFFF,
            length: ((w1 >> 24) & 0xFF) as u8,
            offset: w1 & 0x00FF_FFFF,
        }
    }
}

// ── EfsInode ──────────────────────────────────────────────────────────────────

/// Parsed EFS inode (128 bytes).
#[derive(Debug, Clone)]
pub struct EfsInode {
    pub inum: u32,
    pub mode: u16,
    pub nlink: u16,
    pub uid: u16,
    pub gid: u16,
    pub size: u32,
    pub atime: u32,
    pub mtime: u32,
    pub ctime: u32,
    pub gen: u32,
    pub numextents: u16,
    pub version: u8,
    pub extents: [EfsExtent; EFS_DIRECTEXTENTS],
}

impl EfsInode {
    pub fn parse(inum: u32, buf: &[u8; 128]) -> Self {
        let mut extents = [EfsExtent {
            magic: 0,
            bn: 0,
            length: 0,
            offset: 0,
        }; EFS_DIRECTEXTENTS];
        for (i, ext) in extents.iter_mut().enumerate() {
            let off = 32 + i * 8;
            let chunk: &[u8; 8] = buf[off..off + 8].try_into().unwrap();
            *ext = EfsExtent::parse(chunk);
        }
        EfsInode {
            inum,
            mode: u16::from_be_bytes(buf[0..2].try_into().unwrap()),
            nlink: u16::from_be_bytes(buf[2..4].try_into().unwrap()),
            uid: u16::from_be_bytes(buf[4..6].try_into().unwrap()),
            gid: u16::from_be_bytes(buf[6..8].try_into().unwrap()),
            size: u32::from_be_bytes(buf[8..12].try_into().unwrap()),
            atime: u32::from_be_bytes(buf[12..16].try_into().unwrap()),
            mtime: u32::from_be_bytes(buf[16..20].try_into().unwrap()),
            ctime: u32::from_be_bytes(buf[20..24].try_into().unwrap()),
            gen: u32::from_be_bytes(buf[24..28].try_into().unwrap()),
            numextents: u16::from_be_bytes(buf[28..30].try_into().unwrap()),
            version: buf[30],
            extents,
        }
    }

    pub fn is_dir(&self) -> bool {
        (self.mode & 0o170000) == 0o040000
    }

    pub fn is_symlink(&self) -> bool {
        (self.mode & 0o170000) == 0o120000
    }

    pub fn is_regular(&self) -> bool {
        (self.mode & 0o170000) == 0o100000
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Byte offset (from start of partition) of inode `inum`, given the superblock.
///
/// Mirrors `efs_iget` in Linux v5.15:
/// - `inodes_per_cg = cgisize * (BLOCKSIZE / INODESIZE)`
/// - `cg            = inum / inodes_per_cg`
/// - `inblock       = (inum % inodes_per_cg) / (BLOCKSIZE / INODESIZE)`
/// - `block         = firstcg + cg * cgfsize + inblock`
/// - `byte_in_block = (inum % (BLOCKSIZE / INODESIZE)) * INODESIZE`
pub fn inode_byte_offset(sb: &EfsSuperblock, inum: u32) -> u64 {
    let inodes_per_cg = sb.cgisize as u64 * EFS_INODES_PER_BLOCK;
    let cg = inum as u64 / inodes_per_cg;
    let off_in_cg = inum as u64 % inodes_per_cg;
    let inblock = off_in_cg / EFS_INODES_PER_BLOCK;
    let block = sb.firstcg as u64 + cg * sb.cgfsize as u64 + inblock;
    let byte_in_block = (inum as u64 % EFS_INODES_PER_BLOCK) * EFS_INODESIZE;
    block * EFS_BLOCKSIZE + byte_in_block
}

/// Trim a fixed-length ASCII field: stops at first `\0`, then strips
/// surrounding whitespace.
pub(crate) fn trim_ascii(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end])
        .trim_matches(|c: char| c == ' ' || c == '\0')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_sb(magic: u32) -> Vec<u8> {
        let mut buf = vec![0u8; 64];
        buf[0..4].copy_from_slice(&100u32.to_be_bytes()); // fs_size
        buf[4..8].copy_from_slice(&8u32.to_be_bytes()); // firstcg
        buf[8..12].copy_from_slice(&50u32.to_be_bytes()); // cgfsize
        buf[12..14].copy_from_slice(&2u16.to_be_bytes()); // cgisize
        buf[14..16].copy_from_slice(&63u16.to_be_bytes()); // sectors
        buf[16..18].copy_from_slice(&10u16.to_be_bytes()); // heads
        buf[18..20].copy_from_slice(&2u16.to_be_bytes()); // ncg
        buf[28..32].copy_from_slice(&magic.to_be_bytes());
        buf[32..38].copy_from_slice(b"vol\0\0\0");
        buf[38..44].copy_from_slice(b"pack\0\0");
        buf
    }

    #[test]
    fn parses_minimal_superblock() {
        let buf = build_sb(EFS_MAGIC_OLD);
        let sb = EfsSuperblock::parse(&buf).unwrap();
        assert_eq!(sb.magic, EFS_MAGIC_OLD);
        assert_eq!(sb.fs_size, 100);
        assert_eq!(sb.firstcg, 8);
        assert_eq!(sb.label(), "vol:pack");
    }

    #[test]
    fn accepts_new_magic() {
        let buf = build_sb(EFS_MAGIC_NEW);
        assert_eq!(EfsSuperblock::parse(&buf).unwrap().magic, EFS_MAGIC_NEW);
    }

    #[test]
    fn rejects_bad_magic() {
        let buf = build_sb(0xDEAD_BEEF);
        let err = EfsSuperblock::parse(&buf).unwrap_err();
        assert!(format!("{err}").contains("magic"));
    }

    #[test]
    fn rejects_short_buffer() {
        let err = EfsSuperblock::parse(&[0u8; 8]).unwrap_err();
        assert!(format!("{err}").contains("too small"));
    }

    #[test]
    fn label_empty_when_blank() {
        let mut buf = build_sb(EFS_MAGIC_OLD);
        buf[32..44].fill(0);
        let sb = EfsSuperblock::parse(&buf).unwrap();
        assert_eq!(sb.label(), "");
    }

    #[test]
    fn inode_byte_offset_matches_hand_computation() {
        // From docs/EFS_Implementation.md: IRIX 5.3 fixture has firstcg=1830,
        // cgisize=2460, so inodes_per_cg = 2460*4 = 9840. Root inode 2 lives
        // in cg=0, inblock=0, byte_in_block=2*128=256 → byte 1830*512+256
        // = 937472? Wait: 1830*512 = 936960; +256 = 937216 = 0xE4D00.
        let mut sb = EfsSuperblock::parse(&build_sb(EFS_MAGIC_OLD)).unwrap();
        sb.firstcg = 1830;
        sb.cgfsize = 95954;
        sb.cgisize = 2460;
        assert_eq!(inode_byte_offset(&sb, 2), 0xE4D00);
        assert_eq!(inode_byte_offset(&sb, 4), 0xE4E00);
    }

    #[test]
    fn extent_decode() {
        // bn = 0x123456, length = 0x07, offset = 0x000001, magic = 0x00
        let mut buf = [0u8; 8];
        buf[0..4].copy_from_slice(&0x0012_3456u32.to_be_bytes());
        buf[4..8].copy_from_slice(&((0x07u32 << 24) | 0x000001).to_be_bytes());
        let ext = EfsExtent::parse(&buf);
        assert_eq!(ext.magic, 0);
        assert_eq!(ext.bn, 0x123456);
        assert_eq!(ext.length, 7);
        assert_eq!(ext.offset, 1);
    }

    #[test]
    fn inode_type_predicates() {
        let mut buf = [0u8; 128];
        // mode 0o040755 → directory
        buf[0..2].copy_from_slice(&0o040755u16.to_be_bytes());
        let ino = EfsInode::parse(2, &buf);
        assert!(ino.is_dir());
        assert!(!ino.is_regular());
        assert!(!ino.is_symlink());

        buf[0..2].copy_from_slice(&0o100644u16.to_be_bytes());
        let ino = EfsInode::parse(3, &buf);
        assert!(ino.is_regular());

        buf[0..2].copy_from_slice(&0o120777u16.to_be_bytes());
        let ino = EfsInode::parse(4, &buf);
        assert!(ino.is_symlink());
    }
}
