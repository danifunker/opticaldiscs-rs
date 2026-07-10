//! El Torito boot-catalog parsing.
//!
//! El Torito is the bootable-CD extension to ISO 9660. A Boot Record Volume
//! Descriptor at cooked sector 17 points to a **boot catalog**, a table of
//! 32-byte entries describing one or more bootable images (a floppy image, a
//! hard-disk image, or a "no-emulation" raw payload) that a BIOS/UEFI firmware
//! loads to boot from the disc.
//!
//! This module owns the **disc side** only: it detects El Torito, parses the
//! whole catalog across every platform/section, and hands back each boot image's
//! on-disc extent (and, as a convenience, its bytes). It deliberately does **not**
//! interpret a boot image as a filesystem — a boot image is just a nested disk
//! image (floppy/HDD), and parsing FAT/NTFS/ext inside it is a consumer's job.
//! Reading the emulated MBR *partition table* for sizing (below) is geometry, not
//! a filesystem parse.
//!
//! ## On-disc layout
//!
//! - **Boot Record VD** — cooked sector 17: type `0x00`, `"CD001"`, version `1`,
//!   boot system id `"EL TORITO SPECIFICATION"`, and a u32 LE pointer (bytes
//!   71..75) to the boot catalog LBA.
//! - **Boot catalog** — a series of 32-byte entries: a *validation entry*, an
//!   *initial/default entry*, then zero or more *section headers* (`0x90`/`0x91`)
//!   each followed by its *section entries*.

use crate::error::Result;
use crate::sector_reader::{SectorReader, SECTOR_SIZE};

/// Cooked sector holding the Boot Record Volume Descriptor.
pub const BOOT_RECORD_VD_SECTOR: u64 = 17;

/// Boot system identifier in the Boot Record VD (bytes 7..30).
pub(crate) const EL_TORITO_ID: &[u8] = b"EL TORITO SPECIFICATION";

/// Upper bound on how many cooked sectors of boot catalog we read. Real catalogs
/// occupy a single sector; this keeps a malformed pointer from reading forever.
const MAX_CATALOG_SECTORS: u64 = 16;

/// Fixed floppy-emulation image sizes, by media type (1.2M / 1.44M / 2.88M).
const FLOPPY_1_2M_SIZE: u64 = 1_228_800;
const FLOPPY_1_44M_SIZE: u64 = 1_474_560;
const FLOPPY_2_88M_SIZE: u64 = 2_949_120;

/// Parsed El Torito boot catalog: every boot entry across the default entry and
/// all section headers.
#[derive(Debug, Clone)]
pub struct ElTorito {
    /// Every boot entry across the default entry and all section headers.
    pub entries: Vec<BootEntry>,
}

/// A single bootable image referenced by the catalog.
#[derive(Debug, Clone)]
pub struct BootEntry {
    /// Platform of the owning validation/section header.
    pub platform: Platform,
    /// Boot indicator was `0x88` (bootable) rather than `0x00`.
    pub bootable: bool,
    /// Emulation/media type of the boot image.
    pub media_type: BootMediaType,
    /// Absolute LBA (2048-byte cooked sectors) where the boot image begins.
    pub load_rba: u32,
    /// "Sector count" field — number of virtual 512-byte sectors to load.
    pub sector_count: u16,
    /// System type byte (the emulated image's MBR partition type, for HDD).
    pub system_type: u8,
    /// Computed size of the boot image in bytes (see the module's size rules).
    pub image_size: u64,
    /// Human id / selection criteria captured from the owning section header,
    /// if any (the default entry has none).
    pub id: Option<String>,
}

impl BootEntry {
    /// `(byte offset within the cooked disc, byte length)` of the boot image.
    ///
    /// `offset = load_rba * 2048`; `length = image_size`.
    pub fn image_extent(&self) -> (u64, u64) {
        (self.load_rba as u64 * SECTOR_SIZE, self.image_size)
    }
}

/// Platform id from a validation/section header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// `0x00` — 80x86.
    X86,
    /// `0x01` — PowerPC.
    PowerPc,
    /// `0x02` — Mac.
    Mac,
    /// `0xEF` — EFI / UEFI.
    Efi,
    /// Any other platform id.
    Other(u8),
}

impl Platform {
    pub(crate) fn from_byte(b: u8) -> Self {
        match b {
            0x00 => Platform::X86,
            0x01 => Platform::PowerPc,
            0x02 => Platform::Mac,
            0xEF => Platform::Efi,
            other => Platform::Other(other),
        }
    }

    /// The platform-id byte for a validation/section header.
    pub(crate) fn to_byte(self) -> u8 {
        match self {
            Platform::X86 => 0x00,
            Platform::PowerPc => 0x01,
            Platform::Mac => 0x02,
            Platform::Efi => 0xEF,
            Platform::Other(b) => b,
        }
    }
}

/// Boot media / emulation type of a boot entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootMediaType {
    /// `0` — no emulation (raw payload; size is `sector_count * 512`).
    NoEmulation,
    /// `1` — 1.2 MB floppy emulation.
    Floppy1_2M,
    /// `2` — 1.44 MB floppy emulation.
    Floppy1_44M,
    /// `3` — 2.88 MB floppy emulation.
    Floppy2_88M,
    /// `4` — hard-disk emulation.
    HardDisk,
    /// Any other media-type value.
    Other(u8),
}

impl BootMediaType {
    /// The media type lives in the low nibble of the entry's media byte; the
    /// upper bits carry unrelated flags (e.g. continuation), so mask them off.
    pub(crate) fn from_byte(b: u8) -> Self {
        match b & 0x0F {
            0 => BootMediaType::NoEmulation,
            1 => BootMediaType::Floppy1_2M,
            2 => BootMediaType::Floppy1_44M,
            3 => BootMediaType::Floppy2_88M,
            4 => BootMediaType::HardDisk,
            _ => BootMediaType::Other(b),
        }
    }

    /// The media-type byte for a boot entry (inverse of [`Self::from_byte`]).
    pub(crate) fn to_byte(self) -> u8 {
        match self {
            BootMediaType::NoEmulation => 0,
            BootMediaType::Floppy1_2M => 1,
            BootMediaType::Floppy1_44M => 2,
            BootMediaType::Floppy2_88M => 3,
            BootMediaType::HardDisk => 4,
            BootMediaType::Other(b) => b,
        }
    }
}

/// Detect and parse an El Torito boot catalog from a disc.
///
/// Returns `None` — never an error — when the disc is not El Torito or the boot
/// catalog is malformed, so a lenient probe never fails an otherwise-good disc.
pub fn detect(reader: &mut dyn SectorReader) -> Option<ElTorito> {
    let vd = reader.read_sector(BOOT_RECORD_VD_SECTOR).ok()?;
    if vd.len() < 75 {
        return None;
    }
    // Boot Record VD: type 0x00, "CD001", version 1, El Torito system id.
    if vd[0] != 0x00 || &vd[1..6] != b"CD001" {
        return None;
    }
    if &vd[7..7 + EL_TORITO_ID.len()] != EL_TORITO_ID {
        return None;
    }

    // Absolute pointer to the boot catalog (u32 LE, in 2048-byte sectors).
    let catalog_lba = u32::from_le_bytes([vd[71], vd[72], vd[73], vd[74]]) as u64;

    let catalog = read_catalog(reader, catalog_lba);
    parse_catalog(&catalog, reader)
}

/// Read the whole boot image into memory (container-agnostic).
///
/// A thin convenience over `reader.read_bytes` across [`BootEntry::image_extent`]
/// so a consumer never has to know the 2048-byte cooked-sector math. Large HDD
/// images can instead be streamed by the caller via `image_extent()`.
pub fn read_boot_image(reader: &mut dyn SectorReader, entry: &BootEntry) -> Result<Vec<u8>> {
    let (offset, length) = entry.image_extent();
    reader.read_bytes(offset, length as usize)
}

/// Read up to [`MAX_CATALOG_SECTORS`] cooked sectors of boot catalog, stopping at
/// the first unreadable sector (a short read just yields a shorter buffer).
fn read_catalog(reader: &mut dyn SectorReader, base_lba: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    for i in 0..MAX_CATALOG_SECTORS {
        match reader.read_sector(base_lba + i) {
            Ok(sector) => buf.extend_from_slice(&sector),
            Err(_) => break,
        }
    }
    buf
}

/// Parse a boot catalog buffer into its boot entries. `None` on a
/// missing/garbage validation entry or when no entries survive.
fn parse_catalog(buf: &[u8], reader: &mut dyn SectorReader) -> Option<ElTorito> {
    // Need at least the validation entry (0..32) and the default entry (32..64).
    if buf.len() < 64 {
        return None;
    }

    // Validation entry: header id 0x01 and key 0x55 0xAA reject garbage. The
    // platform here is inherited by the default entry.
    let val = &buf[0..32];
    if val[0] != 0x01 || val[30] != 0x55 || val[31] != 0xAA {
        return None;
    }
    let default_platform = Platform::from_byte(val[1]);

    let mut entries = Vec::new();

    // Initial/default entry. A wholly-unused default slot (not bootable, no load
    // RBA) is a placeholder, not a real image — skip it.
    let default = parse_entry(&buf[32..64], default_platform, None, reader);
    if default.bootable || default.load_rba != 0 {
        entries.push(default);
    }

    // Section headers (0x90 = header, 0x91 = final header), each followed by its
    // declared count of section entries.
    let mut off = 64;
    while off + 32 <= buf.len() {
        let header = &buf[off..off + 32];
        let header_id = header[0];
        if header_id != 0x90 && header_id != 0x91 {
            break;
        }
        let platform = Platform::from_byte(header[1]);
        let count = u16::from_le_bytes([header[2], header[3]]) as usize;
        let section_id = trim_id(&header[4..32]);
        off += 32;

        for _ in 0..count {
            if off + 32 > buf.len() {
                break;
            }
            let entry = parse_entry(&buf[off..off + 32], platform, section_id.clone(), reader);
            entries.push(entry);
            off += 32;
        }

        // 0x91 is the final section header.
        if header_id == 0x91 {
            break;
        }
    }

    if entries.is_empty() {
        None
    } else {
        Some(ElTorito { entries })
    }
}

/// Parse one 32-byte initial/section entry into a [`BootEntry`], computing its
/// image size (which for HDD emulation reads the boot image's MBR via `reader`).
fn parse_entry(
    e: &[u8],
    platform: Platform,
    id: Option<String>,
    reader: &mut dyn SectorReader,
) -> BootEntry {
    let bootable = e[0] == 0x88;
    let media_type = BootMediaType::from_byte(e[1]);
    let system_type = e[4];
    let sector_count = u16::from_le_bytes([e[6], e[7]]);
    let load_rba = u32::from_le_bytes([e[8], e[9], e[10], e[11]]);
    let image_size = compute_image_size(media_type, sector_count, load_rba, reader);

    BootEntry {
        platform,
        bootable,
        media_type,
        load_rba,
        sector_count,
        system_type,
        image_size,
        id,
    }
}

/// Compute a boot image's byte size per the El Torito media-type rules, reading
/// the on-disc MBR through `reader` for hard-disk emulation.
fn compute_image_size(
    media_type: BootMediaType,
    sector_count: u16,
    load_rba: u32,
    reader: &mut dyn SectorReader,
) -> u64 {
    // Only hard-disk emulation needs the MBR; read it lazily.
    let mbr = if media_type == BootMediaType::HardDisk {
        reader.read_bytes(load_rba as u64 * SECTOR_SIZE, 512).ok()
    } else {
        None
    };
    image_size_from_mbr(media_type, sector_count, mbr.as_deref())
}

/// Compute a boot image's byte size from its media type and (for hard-disk
/// emulation) the first 512 bytes of the image (its MBR), if available.
///
/// Shared by the read path ([`compute_image_size`]) and the write path
/// ([`crate::el_torito_edit`]), so both size images identically.
pub(crate) fn image_size_from_mbr(
    media_type: BootMediaType,
    sector_count: u16,
    mbr: Option<&[u8]>,
) -> u64 {
    let fallback = sector_count as u64 * 512;
    match media_type {
        BootMediaType::Floppy1_2M => FLOPPY_1_2M_SIZE,
        BootMediaType::Floppy1_44M => FLOPPY_1_44M_SIZE,
        BootMediaType::Floppy2_88M => FLOPPY_2_88M_SIZE,
        BootMediaType::NoEmulation | BootMediaType::Other(_) => fallback,
        BootMediaType::HardDisk => mbr.and_then(mbr_last_partition_end).unwrap_or(fallback),
    }
}

/// Size a hard-disk-emulation image from its MBR partition table: the byte offset
/// of the end of the last populated partition. `None` if the MBR is too short,
/// unsigned, or empty (the caller then falls back to `sector_count * 512`).
fn mbr_last_partition_end(mbr: &[u8]) -> Option<u64> {
    if mbr.len() < 512 || mbr[510] != 0x55 || mbr[511] != 0xAA {
        return None;
    }

    // Four 16-byte partition entries at offset 446: type at +4, LBA start at +8
    // (u32 LE), sector count at +12 (u32 LE). Take the furthest partition end.
    let mut max_end: u64 = 0;
    for i in 0..4 {
        let p = 446 + i * 16;
        let ptype = mbr[p + 4];
        if ptype == 0 {
            continue;
        }
        let start = u32::from_le_bytes([mbr[p + 8], mbr[p + 9], mbr[p + 10], mbr[p + 11]]) as u64;
        let count = u32::from_le_bytes([mbr[p + 12], mbr[p + 13], mbr[p + 14], mbr[p + 15]]) as u64;
        max_end = max_end.max(start + count);
    }

    if max_end == 0 {
        None
    } else {
        Some(max_end * 512)
    }
}

/// Trim an id field (NUL/space padded) into an owned string, or `None` if empty.
pub(crate) fn trim_id(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let s = String::from_utf8_lossy(&bytes[..end]);
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::OpticaldiscsError;
    use std::io::{Cursor, Read, Seek, SeekFrom};

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

    /// Build a 32-byte validation entry for `platform`, with a correct checksum.
    fn validation_entry(platform: u8) -> [u8; 32] {
        let mut v = [0u8; 32];
        v[0] = 0x01;
        v[1] = platform;
        v[30] = 0x55;
        v[31] = 0xAA;
        // 16-bit word sum of the whole entry must be zero.
        let mut sum: u16 = 0;
        for w in v.chunks_exact(2) {
            sum = sum.wrapping_add(u16::from_le_bytes([w[0], w[1]]));
        }
        let checksum = (0u16).wrapping_sub(sum);
        v[28..30].copy_from_slice(&checksum.to_le_bytes());
        v
    }

    /// Build a 32-byte initial/section boot entry.
    fn boot_entry(bootable: bool, media: u8, sector_count: u16, load_rba: u32) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[0] = if bootable { 0x88 } else { 0x00 };
        e[1] = media;
        e[6..8].copy_from_slice(&sector_count.to_le_bytes());
        e[8..12].copy_from_slice(&load_rba.to_le_bytes());
        e
    }

    /// Wrap a catalog byte buffer in a full disc image (Boot Record VD at 17,
    /// catalog at LBA 19) and return a reader over it.
    fn reader_with_catalog(catalog: &[u8]) -> CursorReader {
        let mut img = vec![0u8; 32 * SECTOR_SIZE as usize];
        // Boot Record VD at sector 17.
        let vd = 17 * SECTOR_SIZE as usize;
        img[vd] = 0x00;
        img[vd + 1..vd + 6].copy_from_slice(b"CD001");
        img[vd + 6] = 0x01;
        img[vd + 7..vd + 7 + EL_TORITO_ID.len()].copy_from_slice(EL_TORITO_ID);
        img[vd + 71..vd + 75].copy_from_slice(&19u32.to_le_bytes());
        // Catalog at sector 19.
        let cat = 19 * SECTOR_SIZE as usize;
        img[cat..cat + catalog.len()].copy_from_slice(catalog);
        CursorReader(Cursor::new(img))
    }

    #[test]
    fn no_emulation_size_is_sector_count_times_512() {
        let mut cat = Vec::new();
        cat.extend_from_slice(&validation_entry(0x00));
        cat.extend_from_slice(&boot_entry(true, 0, 7, 20));
        let mut reader = reader_with_catalog(&cat);

        let et = detect(&mut reader).expect("el torito");
        assert_eq!(et.entries.len(), 1);
        let e = &et.entries[0];
        assert_eq!(e.media_type, BootMediaType::NoEmulation);
        assert_eq!(e.image_size, 7 * 512);
        assert_eq!(e.image_extent(), (20 * 2048, 7 * 512));
    }

    #[test]
    fn floppy_1_44m_size_is_fixed_geometry() {
        let mut cat = Vec::new();
        cat.extend_from_slice(&validation_entry(0x00));
        cat.extend_from_slice(&boot_entry(true, 2, 1, 20));
        let mut reader = reader_with_catalog(&cat);

        let et = detect(&mut reader).expect("el torito");
        assert_eq!(et.entries[0].media_type, BootMediaType::Floppy1_44M);
        assert_eq!(et.entries[0].image_size, 1_474_560);
    }

    #[test]
    fn hard_disk_size_from_mbr_partition_table() {
        // HDD entry whose image lives at LBA 20; its MBR has one partition
        // ending at sector 101 → 101 * 512 bytes.
        let mut cat = Vec::new();
        cat.extend_from_slice(&validation_entry(0x00));
        cat.extend_from_slice(&boot_entry(true, 4, 1, 20));

        let mut img = vec![0u8; 32 * SECTOR_SIZE as usize];
        let vd = 17 * SECTOR_SIZE as usize;
        img[vd] = 0x00;
        img[vd + 1..vd + 6].copy_from_slice(b"CD001");
        img[vd + 6] = 0x01;
        img[vd + 7..vd + 7 + EL_TORITO_ID.len()].copy_from_slice(EL_TORITO_ID);
        img[vd + 71..vd + 75].copy_from_slice(&19u32.to_le_bytes());
        let cat_off = 19 * SECTOR_SIZE as usize;
        img[cat_off..cat_off + cat.len()].copy_from_slice(&cat);

        // MBR at the boot image (LBA 20): one partition, start 1, 100 sectors.
        let mbr = 20 * SECTOR_SIZE as usize;
        let p = mbr + 446;
        img[p + 4] = 0x0C; // FAT32 LBA
        img[p + 8..p + 12].copy_from_slice(&1u32.to_le_bytes());
        img[p + 12..p + 16].copy_from_slice(&100u32.to_le_bytes());
        img[mbr + 510] = 0x55;
        img[mbr + 511] = 0xAA;

        let mut reader = CursorReader(Cursor::new(img));
        let et = detect(&mut reader).expect("el torito");
        assert_eq!(et.entries[0].media_type, BootMediaType::HardDisk);
        assert_eq!(et.entries[0].image_size, 101 * 512);
    }

    #[test]
    fn hard_disk_falls_back_without_mbr_signature() {
        // No MBR signature at the boot image → fall back to sector_count * 512.
        let mut cat = Vec::new();
        cat.extend_from_slice(&validation_entry(0x00));
        cat.extend_from_slice(&boot_entry(true, 4, 9, 20));
        let mut reader = reader_with_catalog(&cat);

        let et = detect(&mut reader).expect("el torito");
        assert_eq!(et.entries[0].image_size, 9 * 512);
    }

    #[test]
    fn multi_boot_surfaces_all_entries_with_per_entry_platform() {
        let mut cat = Vec::new();
        cat.extend_from_slice(&validation_entry(0x00)); // x86
        cat.extend_from_slice(&boot_entry(true, 0, 4, 20)); // default: x86 no-emu
                                                            // Final section header for EFI, one section entry.
        let mut header = [0u8; 32];
        header[0] = 0x91;
        header[1] = 0xEF;
        header[2..4].copy_from_slice(&1u16.to_le_bytes());
        header[4..7].copy_from_slice(b"EFI");
        cat.extend_from_slice(&header);
        cat.extend_from_slice(&boot_entry(true, 0, 8, 22)); // EFI no-emu

        let mut reader = reader_with_catalog(&cat);
        let et = detect(&mut reader).expect("el torito");
        assert_eq!(et.entries.len(), 2);
        assert_eq!(et.entries[0].platform, Platform::X86);
        assert_eq!(et.entries[1].platform, Platform::Efi);
        assert_eq!(et.entries[1].id.as_deref(), Some("EFI"));
        assert_eq!(et.entries[1].load_rba, 22);
    }

    #[test]
    fn no_boot_record_vd_yields_none() {
        let img = vec![0u8; 32 * SECTOR_SIZE as usize];
        let mut reader = CursorReader(Cursor::new(img));
        assert!(detect(&mut reader).is_none());
    }

    #[test]
    fn garbage_catalog_yields_none() {
        // Valid Boot Record VD but the catalog validation entry is garbage.
        let cat = vec![0xFFu8; 64];
        let mut reader = reader_with_catalog(&cat);
        assert!(detect(&mut reader).is_none());
    }

    #[test]
    fn unused_default_entry_is_skipped() {
        // Default entry all-zero (not bootable, load_rba 0) → no entries.
        let mut cat = Vec::new();
        cat.extend_from_slice(&validation_entry(0x00));
        cat.extend_from_slice(&[0u8; 32]);
        let mut reader = reader_with_catalog(&cat);
        assert!(detect(&mut reader).is_none());
    }
}
