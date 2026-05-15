//! SGI Volume Header parsing.
//!
//! SGI disks (IRIX systems) use a 512-byte volume header at sector 0 instead
//! of an MBR or APM partition table. This module parses that header and
//! exposes the partition entries so callers can locate the EFS partition on
//! IRIX install/distribution CDs.
//!
//! On-disk layout (all big-endian, sector 0):
//!
//! - `0x000` magic `0x0BE5A941` (u32)
//! - `0x004` `root_part_num` (u16), `swap_part_num` (u16)
//! - `0x008` `bootfile[16]` — null-terminated path of standalone boot program
//! - `0x048` 15 × 16-byte volume directory entries (`name[8]`, `block_num`, `bytes`)
//! - `0x138` 16 × 12-byte partition entries (`blocks`, `first`, `type`)
//! - `0x1F8` checksum (u32) — verified but not enforced
//!
//! Partition `first`/`blocks` are always in 512-byte units, independent of
//! the underlying device's physical sector size.

use crate::error::{OpticaldiscsError, Result};
use crate::sector_reader::SectorReader;

/// SGI Volume Header magic at byte 0 of sector 0. Big-endian.
pub const SGI_VOLHDR_MAGIC: u32 = 0x0BE5_A941;

/// Volume header is one 512-byte sector.
pub const SGI_VOLHDR_SIZE: usize = 512;

/// Number of partition entries in the volume header.
pub const SGI_NUM_PARTITIONS: usize = 16;

/// Number of volume directory entries.
pub const SGI_NUM_VOL_DIR: usize = 15;

/// SGI partition type codes (big-endian u32 at offset 8 of each entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SgiPartitionType {
    VolHdr,
    TrkRepl,
    SecRepl,
    Raw,
    Bsd,
    SysV,
    Volume,
    Efs,
    LVol,
    RLVol,
    Xfs,
    XfsLog,
    Xlv,
    Xvm,
    Unknown(u32),
}

impl SgiPartitionType {
    pub fn from_raw(v: u32) -> Self {
        match v {
            0 => Self::VolHdr,
            1 => Self::TrkRepl,
            2 => Self::SecRepl,
            3 => Self::Raw,
            4 => Self::Bsd,
            5 => Self::SysV,
            6 => Self::Volume,
            7 => Self::Efs,
            8 => Self::LVol,
            9 => Self::RLVol,
            10 => Self::Xfs,
            11 => Self::XfsLog,
            12 => Self::Xlv,
            13 => Self::Xvm,
            other => Self::Unknown(other),
        }
    }

    pub fn as_u32(self) -> u32 {
        match self {
            Self::VolHdr => 0,
            Self::TrkRepl => 1,
            Self::SecRepl => 2,
            Self::Raw => 3,
            Self::Bsd => 4,
            Self::SysV => 5,
            Self::Volume => 6,
            Self::Efs => 7,
            Self::LVol => 8,
            Self::RLVol => 9,
            Self::Xfs => 10,
            Self::XfsLog => 11,
            Self::Xlv => 12,
            Self::Xvm => 13,
            Self::Unknown(v) => v,
        }
    }

    /// Plain-ASCII display name.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::VolHdr => "VOLHDR",
            Self::TrkRepl => "TRKREPL",
            Self::SecRepl => "SECREPL",
            Self::Raw => "RAW",
            Self::Bsd => "BSD",
            Self::SysV => "SYSV",
            Self::Volume => "VOLUME",
            Self::Efs => "EFS",
            Self::LVol => "LVOL",
            Self::RLVol => "RLVOL",
            Self::Xfs => "XFS",
            Self::XfsLog => "XFSLOG",
            Self::Xlv => "XLV",
            Self::Xvm => "XVM",
            Self::Unknown(_) => "Unknown",
        }
    }

    /// True for partition types that wrap the whole disk or aren't filesystems.
    pub fn is_disk_wide_wrapper(self) -> bool {
        matches!(self, Self::VolHdr | Self::Volume)
    }
}

/// 16-byte volume directory entry: `name[8]` + `block_num` + `bytes`.
///
/// Used by the SGI PROM to locate standalone executables (`sash`, `ide`,
/// `/unix`). Preserved for completeness; not surfaced to filesystem browsing.
#[derive(Debug, Clone)]
pub struct SgiVolumeDirEntry {
    pub name: String,
    pub block_num: u32,
    pub bytes: u32,
}

impl SgiVolumeDirEntry {
    fn parse(buf: &[u8; 16]) -> Self {
        SgiVolumeDirEntry {
            name: parse_fixed_ascii(&buf[0..8]),
            block_num: u32::from_be_bytes(buf[8..12].try_into().unwrap()),
            bytes: u32::from_be_bytes(buf[12..16].try_into().unwrap()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.name.is_empty() && self.block_num == 0 && self.bytes == 0
    }
}

/// 12-byte partition entry: `blocks` + `first` + `type`.
///
/// Sectors are always 512 bytes regardless of physical sector size.
#[derive(Debug, Clone)]
pub struct SgiPartitionEntry {
    /// Sector count (512-byte sectors).
    pub blocks: u32,
    /// First sector (512-byte sectors).
    pub first: u32,
    /// Raw partition type code (kept for round-trip fidelity).
    pub partition_type_raw: u32,
}

impl SgiPartitionEntry {
    fn parse(buf: &[u8; 12]) -> Self {
        SgiPartitionEntry {
            blocks: u32::from_be_bytes(buf[0..4].try_into().unwrap()),
            first: u32::from_be_bytes(buf[4..8].try_into().unwrap()),
            partition_type_raw: u32::from_be_bytes(buf[8..12].try_into().unwrap()),
        }
    }

    pub fn partition_type(&self) -> SgiPartitionType {
        SgiPartitionType::from_raw(self.partition_type_raw)
    }

    pub fn is_empty(&self) -> bool {
        self.blocks == 0 && self.first == 0 && self.partition_type_raw == 0
    }

    /// Partition size in bytes (sectors are always 512 bytes on SGI).
    pub fn size_bytes(&self) -> u64 {
        self.blocks as u64 * 512
    }

    /// Start byte offset from the beginning of the disk.
    pub fn start_offset(&self) -> u64 {
        self.first as u64 * 512
    }
}

/// Parsed SGI Volume Header (sector 0 of an SGI disk).
#[derive(Debug, Clone)]
pub struct SgiVolumeHeader {
    pub magic: u32,
    pub root_part_num: u16,
    pub swap_part_num: u16,
    /// Null-terminated bootfile path (e.g. `/unix`).
    pub bootfile: String,
    pub volume_directory: Vec<SgiVolumeDirEntry>,
    pub partitions: Vec<SgiPartitionEntry>,
    pub checksum: u32,
    /// True if the on-disk checksum verified. Stale checksums after image-tool
    /// round-trips are common, so we log and continue regardless.
    pub checksum_valid: bool,
}

impl SgiVolumeHeader {
    /// Parse from a 512-byte sector buffer.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < SGI_VOLHDR_SIZE {
            return Err(OpticaldiscsError::InvalidData(format!(
                "SGI volume header buffer too small: {} bytes, need {}",
                buf.len(),
                SGI_VOLHDR_SIZE
            )));
        }
        let buf = &buf[..SGI_VOLHDR_SIZE];

        let magic = u32::from_be_bytes(buf[0..4].try_into().unwrap());
        if magic != SGI_VOLHDR_MAGIC {
            return Err(OpticaldiscsError::InvalidData(format!(
                "bad SGI volume header magic: 0x{magic:08X} (expected 0x{SGI_VOLHDR_MAGIC:08X})"
            )));
        }

        let root_part_num = u16::from_be_bytes(buf[4..6].try_into().unwrap());
        let swap_part_num = u16::from_be_bytes(buf[6..8].try_into().unwrap());
        let bootfile = parse_fixed_ascii(&buf[8..24]);

        let mut volume_directory = Vec::with_capacity(SGI_NUM_VOL_DIR);
        for i in 0..SGI_NUM_VOL_DIR {
            let off = 0x048 + i * 16;
            volume_directory.push(SgiVolumeDirEntry::parse(
                buf[off..off + 16].try_into().unwrap(),
            ));
        }

        let mut partitions = Vec::with_capacity(SGI_NUM_PARTITIONS);
        for i in 0..SGI_NUM_PARTITIONS {
            let off = 0x138 + i * 12;
            let entry = SgiPartitionEntry::parse(buf[off..off + 12].try_into().unwrap());
            if !entry.is_empty() && entry.first.checked_add(entry.blocks).is_none() {
                return Err(OpticaldiscsError::InvalidData(format!(
                    "SGI partition[{i}] first={} blocks={} overflows u32",
                    entry.first, entry.blocks
                )));
            }
            partitions.push(entry);
        }

        let checksum = u32::from_be_bytes(buf[0x1F8..0x1FC].try_into().unwrap());
        let checksum_valid = volume_checksum_zero(buf);
        if !checksum_valid {
            log::warn!("SGI volume header checksum mismatch (continuing)");
        }

        Ok(SgiVolumeHeader {
            magic,
            root_part_num,
            swap_part_num,
            bootfile,
            volume_directory,
            partitions,
            checksum,
            checksum_valid,
        })
    }

    /// Read and parse a volume header from sector 0 of a `SectorReader`.
    pub fn read_from(reader: &mut dyn SectorReader) -> Result<Self> {
        let buf = reader.read_bytes(0, SGI_VOLHDR_SIZE)?;
        Self::parse(&buf)
    }
}

/// Verify the SGI volume header checksum: treat the sector as 128 BE u32s;
/// their two's-complement sum must be zero.
fn volume_checksum_zero(buf: &[u8]) -> bool {
    let mut sum: u32 = 0;
    for chunk in buf[..SGI_VOLHDR_SIZE].chunks_exact(4) {
        sum = sum.wrapping_add(u32::from_be_bytes(chunk.try_into().unwrap()));
    }
    sum == 0
}

fn parse_fixed_ascii(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end])
        .trim_end_matches(' ')
        .to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sector_reader::SECTOR_SIZE;
    use std::io::{Cursor, Read, Seek, SeekFrom};

    /// Build a synthetic 512-byte SGI volume header buffer with the given
    /// partition entries.
    pub(crate) fn build_test_volhdr(parts: &[(u32, u32, u32)]) -> [u8; SGI_VOLHDR_SIZE] {
        let mut buf = [0u8; SGI_VOLHDR_SIZE];
        buf[0..4].copy_from_slice(&SGI_VOLHDR_MAGIC.to_be_bytes());
        buf[4..6].copy_from_slice(&0u16.to_be_bytes()); // root_part_num
        buf[6..8].copy_from_slice(&1u16.to_be_bytes()); // swap_part_num
        // bootfile "/unix"
        let boot = b"/unix";
        buf[8..8 + boot.len()].copy_from_slice(boot);

        for (i, (blocks, first, ptype)) in parts.iter().enumerate() {
            if i >= SGI_NUM_PARTITIONS {
                break;
            }
            let off = 0x138 + i * 12;
            buf[off..off + 4].copy_from_slice(&blocks.to_be_bytes());
            buf[off + 4..off + 8].copy_from_slice(&first.to_be_bytes());
            buf[off + 8..off + 12].copy_from_slice(&ptype.to_be_bytes());
        }
        // Fix checksum so the u32-sum is zero.
        let mut sum: u32 = 0;
        for chunk in buf.chunks_exact(4) {
            sum = sum.wrapping_add(u32::from_be_bytes(chunk.try_into().unwrap()));
        }
        let cksum = 0u32.wrapping_sub(sum);
        buf[0x1F8..0x1FC].copy_from_slice(&cksum.to_be_bytes());
        buf
    }

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

    #[test]
    fn rejects_short_buffer() {
        let err = SgiVolumeHeader::parse(&[0u8; 256]).unwrap_err();
        assert!(matches!(err, OpticaldiscsError::InvalidData(_)));
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut buf = vec![0u8; SGI_VOLHDR_SIZE];
        buf[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        let err = SgiVolumeHeader::parse(&buf).unwrap_err();
        assert!(format!("{err}").contains("magic"));
    }

    #[test]
    fn rejects_partition_overflow() {
        let mut buf = vec![0u8; SGI_VOLHDR_SIZE];
        buf[0..4].copy_from_slice(&SGI_VOLHDR_MAGIC.to_be_bytes());
        let off = 0x138;
        buf[off..off + 4].copy_from_slice(&1u32.to_be_bytes());
        buf[off + 4..off + 8].copy_from_slice(&u32::MAX.to_be_bytes());
        buf[off + 8..off + 12].copy_from_slice(&SgiPartitionType::Efs.as_u32().to_be_bytes());
        let err = SgiVolumeHeader::parse(&buf).unwrap_err();
        assert!(format!("{err}").contains("overflow"));
    }

    #[test]
    fn parses_synthetic_header_with_efs_partition() {
        // Mirrors the layout seen on the IRIX install CDs: data partition is
        // type 5 (SYSV), wrappers in slots 8 (VOLHDR) and 10 (VOLUME).
        let mut parts = vec![(0u32, 0u32, 0u32); SGI_NUM_PARTITIONS];
        parts[7] = (877_448, 49_248, SgiPartitionType::SysV.as_u32());
        parts[8] = (49_248, 0, SgiPartitionType::VolHdr.as_u32());
        parts[10] = (926_720, 0, SgiPartitionType::Volume.as_u32());

        let buf = build_test_volhdr(&parts);
        let vh = SgiVolumeHeader::parse(&buf).expect("valid header");
        assert_eq!(vh.magic, SGI_VOLHDR_MAGIC);
        assert_eq!(vh.bootfile, "/unix");
        assert_eq!(vh.partitions[7].first, 49_248);
        assert_eq!(vh.partitions[7].partition_type(), SgiPartitionType::SysV);
        assert_eq!(vh.partitions[8].partition_type(), SgiPartitionType::VolHdr);
        assert_eq!(vh.partitions[10].partition_type(), SgiPartitionType::Volume);
        assert!(vh.checksum_valid);
    }

    #[test]
    fn read_from_sector_reader_round_trip() {
        let mut parts = vec![(0u32, 0u32, 0u32); SGI_NUM_PARTITIONS];
        parts[7] = (1000, 100, SgiPartitionType::Efs.as_u32());
        let vh_bytes = build_test_volhdr(&parts);

        // Embed into a 2048-byte sector at LBA 0.
        let mut img = vec![0u8; SECTOR_SIZE as usize];
        img[..SGI_VOLHDR_SIZE].copy_from_slice(&vh_bytes);
        let mut reader = CursorReader(Cursor::new(img));

        let vh = SgiVolumeHeader::read_from(&mut reader).unwrap();
        assert_eq!(vh.partitions[7].partition_type(), SgiPartitionType::Efs);
        assert_eq!(vh.partitions[7].first, 100);
    }

    #[test]
    fn partition_type_round_trip() {
        for raw in [0u32, 5, 7, 10, 99] {
            assert_eq!(SgiPartitionType::from_raw(raw).as_u32(), raw);
        }
    }

    #[test]
    fn display_names_are_ascii_only() {
        for raw in 0..=13u32 {
            let name = SgiPartitionType::from_raw(raw).display_name();
            assert!(name.is_ascii(), "{name} is not ASCII");
        }
    }
}
