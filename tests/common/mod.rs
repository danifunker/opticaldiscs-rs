//! Shared helpers for integration tests.

use opticaldiscs::efs::{EFS_DIRBLK_MAGIC, EFS_MAGIC_OLD};
use opticaldiscs::sgi::{SgiPartitionType, SGI_NUM_PARTITIONS, SGI_VOLHDR_MAGIC, SGI_VOLHDR_SIZE};

/// Synthetic EFS partition starts at sector 64 of the disc.
pub const EFS_PART_FIRST_512: u32 = 64;

/// Build a deterministic synthetic IRIX-style disc image containing:
///
/// - SGI volume header at sector 0 (magic `0x0BE5A941`)
/// - SYSV-typed data partition starting at 512-byte block 64
/// - EFS filesystem inside that partition with:
///   - Root (inode 2) containing three entries: `data`, `link`, `sub`
///   - `data` (inode 4): 512-byte regular file filled with `0xAA`
///   - `link` (inode 5): symlink to `/usr/sbin/init`
///   - `sub`  (inode 6): subdirectory containing `nested` (inode 7) →
///     512-byte regular file filled with `0xBB`
///
/// Total image size is 96 sectors of 2048 bytes (192 KiB). The image is
/// returned as a `Vec<u8>`; callers typically write it to a `.iso` tempfile
/// and feed it to `DiscImageInfo::open`.
pub fn build_synth_irix_disc() -> Vec<u8> {
    // Work in 512-byte block units internally; the partition's `first` is
    // in those units, and EFS structures use them too. The outer disc image
    // is padded to a multiple of 2048 (the cooked sector size) at the end.
    const N_BLOCKS_512: usize = 384; // 384 * 512 = 196608 = 96 * 2048
    let mut img = vec![0u8; N_BLOCKS_512 * 512];

    // ── SGI volume header at sector 0 ──────────────────────────────────────
    img[0..4].copy_from_slice(&SGI_VOLHDR_MAGIC.to_be_bytes());
    img[4..6].copy_from_slice(&0u16.to_be_bytes()); // root_part_num
    img[6..8].copy_from_slice(&1u16.to_be_bytes()); // swap_part_num
    img[8..14].copy_from_slice(b"/unix\0");

    // Partition entries at 0x138. We mirror what real IRIX install CDs do:
    // the data partition is type SYSV (5), not EFS (7).
    let mut parts = [(0u32, 0u32, 0u32); SGI_NUM_PARTITIONS];
    parts[7] = (
        (N_BLOCKS_512 as u32) - EFS_PART_FIRST_512,
        EFS_PART_FIRST_512,
        SgiPartitionType::SysV.as_u32(),
    );
    parts[8] = (EFS_PART_FIRST_512, 0, SgiPartitionType::VolHdr.as_u32());
    parts[10] = (N_BLOCKS_512 as u32, 0, SgiPartitionType::Volume.as_u32());
    for (i, (blocks, first, ptype)) in parts.iter().enumerate() {
        let off = 0x138 + i * 12;
        img[off..off + 4].copy_from_slice(&blocks.to_be_bytes());
        img[off + 4..off + 8].copy_from_slice(&first.to_be_bytes());
        img[off + 8..off + 12].copy_from_slice(&ptype.to_be_bytes());
    }
    // SGI volume-header checksum: u32-sum of the 512-byte sector must be 0.
    {
        let mut sum: u32 = 0;
        for chunk in img[..SGI_VOLHDR_SIZE].chunks_exact(4) {
            sum = sum.wrapping_add(u32::from_be_bytes(chunk.try_into().unwrap()));
        }
        let cksum = 0u32.wrapping_sub(sum);
        img[0x1F8..0x1FC].copy_from_slice(&cksum.to_be_bytes());
    }

    // ── EFS layout, relative to start of partition (block EFS_PART_FIRST_512)
    //
    // Within-partition block layout (each block is 512 bytes):
    //   0  : pad
    //   1  : superblock
    //   2..18 : pad to firstcg
    //   18,19 : inode area (cgisize=2 → 8 inodes; we use 2,4,5,6,7)
    //   20 : root directory block
    //   21 : "data" file content (0xAA fill)
    //   22 : "link" symlink target text
    //   23 : "sub" subdirectory block
    //   24 : "nested" content (0xBB fill)
    //
    // All offsets below are absolute byte offsets into `img`.
    let part_byte = EFS_PART_FIRST_512 as usize * 512;
    let block = |b: usize| part_byte + b * 512;

    // Superblock at block 1 of the partition.
    let sb = block(1);
    let part_blocks = (N_BLOCKS_512 - EFS_PART_FIRST_512 as usize) as u32;
    img[sb..sb + 4].copy_from_slice(&part_blocks.to_be_bytes()); // fs_size
    img[sb + 4..sb + 8].copy_from_slice(&18u32.to_be_bytes()); // firstcg
    img[sb + 8..sb + 12].copy_from_slice(&(part_blocks - 18).to_be_bytes()); // cgfsize
    img[sb + 12..sb + 14].copy_from_slice(&2u16.to_be_bytes()); // cgisize
    img[sb + 14..sb + 16].copy_from_slice(&63u16.to_be_bytes()); // sectors
    img[sb + 16..sb + 18].copy_from_slice(&1u16.to_be_bytes()); // heads
    img[sb + 18..sb + 20].copy_from_slice(&1u16.to_be_bytes()); // ncg
    img[sb + 28..sb + 32].copy_from_slice(&EFS_MAGIC_OLD.to_be_bytes()); // magic
    img[sb + 32..sb + 38].copy_from_slice(b"synth\0");
    img[sb + 38..sb + 44].copy_from_slice(b"pack\0\0");

    // Inode-byte offset within partition for inode `inum`.
    // inodes_per_cg = cgisize * 4 = 8 → all inodes ≤ 7 live in cg 0.
    let ino_off = |inum: u32| -> usize {
        let b = 18u32 + inum / 4;
        block(b as usize) + ((inum % 4) as usize) * 128
    };

    // Inode 2: root directory → block 20 (1 block, size 512).
    let off = ino_off(2);
    img[off..off + 2].copy_from_slice(&0o040755u16.to_be_bytes());
    img[off + 8..off + 12].copy_from_slice(&512u32.to_be_bytes());
    img[off + 28..off + 30].copy_from_slice(&1u16.to_be_bytes());
    img[off + 32..off + 36].copy_from_slice(&20u32.to_be_bytes());
    img[off + 36..off + 40].copy_from_slice(&(1u32 << 24).to_be_bytes());

    // Inode 4: regular file, size 512 → block 21.
    // Also carries ownership + timestamps so metadata plumbing can be asserted.
    let off = ino_off(4);
    img[off..off + 2].copy_from_slice(&0o100644u16.to_be_bytes());
    img[off + 4..off + 6].copy_from_slice(&501u16.to_be_bytes()); // uid
    img[off + 6..off + 8].copy_from_slice(&20u16.to_be_bytes()); // gid
    img[off + 8..off + 12].copy_from_slice(&512u32.to_be_bytes());
    img[off + 12..off + 16].copy_from_slice(&0x1000_0000u32.to_be_bytes()); // atime
    img[off + 16..off + 20].copy_from_slice(&0x2000_0000u32.to_be_bytes()); // mtime
    img[off + 20..off + 24].copy_from_slice(&0x3000_0000u32.to_be_bytes()); // ctime
    img[off + 28..off + 30].copy_from_slice(&1u16.to_be_bytes());
    img[off + 32..off + 36].copy_from_slice(&21u32.to_be_bytes());
    img[off + 36..off + 40].copy_from_slice(&(1u32 << 24).to_be_bytes());

    // Inode 5: symlink with target "/usr/sbin/init" → block 22.
    let target = b"/usr/sbin/init";
    let off = ino_off(5);
    img[off..off + 2].copy_from_slice(&0o120777u16.to_be_bytes());
    img[off + 8..off + 12].copy_from_slice(&(target.len() as u32).to_be_bytes());
    img[off + 28..off + 30].copy_from_slice(&1u16.to_be_bytes());
    img[off + 32..off + 36].copy_from_slice(&22u32.to_be_bytes());
    img[off + 36..off + 40].copy_from_slice(&(1u32 << 24).to_be_bytes());

    // Inode 6: subdirectory → block 23.
    let off = ino_off(6);
    img[off..off + 2].copy_from_slice(&0o040755u16.to_be_bytes());
    img[off + 8..off + 12].copy_from_slice(&512u32.to_be_bytes());
    img[off + 28..off + 30].copy_from_slice(&1u16.to_be_bytes());
    img[off + 32..off + 36].copy_from_slice(&23u32.to_be_bytes());
    img[off + 36..off + 40].copy_from_slice(&(1u32 << 24).to_be_bytes());

    // Inode 7: nested file → block 24.
    let off = ino_off(7);
    img[off..off + 2].copy_from_slice(&0o100644u16.to_be_bytes());
    img[off + 8..off + 12].copy_from_slice(&512u32.to_be_bytes());
    img[off + 28..off + 30].copy_from_slice(&1u16.to_be_bytes());
    img[off + 32..off + 36].copy_from_slice(&24u32.to_be_bytes());
    img[off + 36..off + 40].copy_from_slice(&(1u32 << 24).to_be_bytes());

    // Root directory block @ block 20: 3 entries.
    let d = block(20);
    img[d..d + 2].copy_from_slice(&EFS_DIRBLK_MAGIC.to_be_bytes());
    img[d + 3] = 3;
    // "data" -> inum 4 at block byte 500 (slot 250 = 0xFA)
    img[d + 4] = 0xFA;
    let de = d + 500;
    img[de..de + 4].copy_from_slice(&4u32.to_be_bytes());
    img[de + 4] = 4;
    img[de + 5..de + 9].copy_from_slice(b"data");
    // "link" -> inum 5 at block byte 490 (slot 245 = 0xF5)
    img[d + 5] = 0xF5;
    let de = d + 490;
    img[de..de + 4].copy_from_slice(&5u32.to_be_bytes());
    img[de + 4] = 4;
    img[de + 5..de + 9].copy_from_slice(b"link");
    // "sub" -> inum 6 at block byte 480 (slot 240 = 0xF0)
    img[d + 6] = 0xF0;
    let de = d + 480;
    img[de..de + 4].copy_from_slice(&6u32.to_be_bytes());
    img[de + 4] = 3;
    img[de + 5..de + 8].copy_from_slice(b"sub");

    // "sub" directory block @ block 23: one entry "nested" -> inum 7.
    let d = block(23);
    img[d..d + 2].copy_from_slice(&EFS_DIRBLK_MAGIC.to_be_bytes());
    img[d + 3] = 1;
    img[d + 4] = 0xFA;
    let de = d + 500;
    img[de..de + 4].copy_from_slice(&7u32.to_be_bytes());
    img[de + 4] = 6;
    img[de + 5..de + 11].copy_from_slice(b"nested");

    // File contents.
    let b21 = block(21);
    img[b21..b21 + 512].fill(0xAA);
    let b22 = block(22);
    img[b22..b22 + target.len()].copy_from_slice(target);
    let b24 = block(24);
    img[b24..b24 + 512].fill(0xBB);

    img
}
