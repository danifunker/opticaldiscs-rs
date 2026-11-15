//! El Torito boot-catalog **editing** (write path) for raw `.iso` images.
//!
//! [`ElToritoEditor`] opens a raw ISO over a `Read + Write + Seek` handle, parses
//! its existing El Torito boot catalog (via the shared types in
//! [`crate::el_torito`]), and lets a consumer edit it:
//!
//! - flip an entry bootable/off, change its platform / media type / system type,
//! - replace a boot image with a new opaque byte blob (in place when the size is
//!   unchanged, otherwise relocated to appended free space),
//! - add and remove boot entries.
//!
//! All edits are staged in memory and written by [`ElToritoEditor::commit`],
//! which orders its writes so an interrupted commit leaves the original bootable:
//! appended images and any relocated catalog are written first, then the Boot
//! Record VD pointer and the PVD size are updated last.
//!
//! ## Boundary
//!
//! This module owns only the disc/container side: it rewrites the boot catalog
//! and places boot-image blobs, keeping the ISO self-consistent (`load_rba` /
//! `sector_count`, the Boot Record VD pointer, and the PVD `volume_space_size`).
//! It **never** interprets a boot image as a filesystem — the consumer edits the
//! FAT/NTFS/… inside the image with its own engine and hands over finished bytes.
//!
//! ## Scope & limitations
//!
//! - **Raw `.iso` only.** BIN/CUE (2352 framing) and CHD (compressed hunks) are
//!   rejected — convert to `.iso` first. Raw 2352-byte "`.iso`" dumps are also
//!   rejected, since the write math assumes 2048-byte cooked sectors.
//! - **Dead space is leaked.** A relocated (grown) image or a removed entry's
//!   image leaves its old sectors unused; reclaiming them is a full remaster and
//!   is out of scope.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::el_torito::{
    image_size_from_mbr, trim_id, BootEntry, BootMediaType, Platform, BOOT_RECORD_VD_SECTOR,
    EL_TORITO_ID,
};
use crate::error::{OpticaldiscsError, Result};
use crate::formats::DiscFormat;
use crate::sector_reader::SECTOR_SIZE;

/// 12-byte sync pattern that begins every raw (2352-byte) CD sector.
const RAW_SYNC: [u8; 12] = [
    0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00,
];

/// CHD magic bytes at offset 0.
const CHD_MAGIC: &[u8; 8] = b"MComprHD";

/// The Primary Volume Descriptor lives at cooked sector 16.
const PVD_SECTOR: u64 = 16;

/// Specification for a boot entry added via [`ElToritoEditor::add_entry`].
#[derive(Debug, Clone)]
pub struct NewBootEntry {
    /// Platform of the new entry.
    pub platform: Platform,
    /// Whether the entry is bootable (boot indicator `0x88`).
    pub bootable: bool,
    /// Emulation/media type of the boot image.
    pub media_type: BootMediaType,
    /// System type byte (the emulated image's MBR partition type, for HDD).
    pub system_type: u8,
    /// Optional id string carried in the entry's section header.
    pub id: Option<String>,
}

/// One boot entry plus the private fields needed to rewrite it, and any staged
/// replacement image bytes.
struct Slot {
    entry: BootEntry,
    /// Load segment (bytes 2..4 of the entry); `0` means the default `0x07C0`.
    load_segment: u16,
    /// Selection-criteria byte (byte 12 of a section entry).
    selection: u8,
    /// Staged image bytes to write on commit, and where.
    pending: Option<PendingImage>,
}

struct PendingImage {
    bytes: Vec<u8>,
    /// Overwrite the existing extent at `entry.load_rba` (size unchanged) rather
    /// than appending to free space.
    in_place: bool,
}

/// Editor over a raw ISO's El Torito boot catalog. See the module docs.
pub struct ElToritoEditor<RW: Read + Write + Seek> {
    rw: RW,
    /// Total cooked sectors currently in the image (file length / 2048).
    total_sectors: u32,
    /// LBA of the boot catalog (from the Boot Record VD pointer).
    catalog_lba: u32,
    /// Cooked sectors the current on-disc catalog occupies (its allocated room
    /// before the following data); the catalog may be rewritten within this span.
    catalog_span: u32,
    /// Manufacturer id from the validation entry (bytes 4..28), preserved verbatim.
    validation_id: [u8; 24],
    slots: Vec<Slot>,
    /// A catalog rewrite is needed (metadata/structure changed).
    dirty_catalog: bool,
}

impl ElToritoEditor<std::fs::File> {
    /// Open the raw ISO at `path` read-write for editing.
    ///
    /// Rejects non-ISO containers (BIN/CUE, CHD, …) by format before touching the
    /// bytes; see [`ElToritoEditor::open`] for the content-level checks.
    pub fn open_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match crate::detect::detect_format(path) {
            Ok(DiscFormat::Iso) => {}
            Ok(other) => {
                return Err(OpticaldiscsError::UnsupportedFormat(format!(
                    "El Torito editing supports raw .iso only, not {other:?}; convert first"
                )))
            }
            Err(e) => return Err(e),
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(OpticaldiscsError::Io)?;
        Self::open(file)
    }
}

impl<RW: Read + Write + Seek> ElToritoEditor<RW> {
    /// Parse the existing catalog of a raw ISO for editing.
    ///
    /// # Errors
    ///
    /// - [`OpticaldiscsError::UnsupportedFormat`] if the image is a CHD, a raw
    ///   2352-byte dump, not an ISO 9660, or not already El Torito.
    pub fn open(mut rw: RW) -> Result<Self> {
        let file_len = rw.seek(SeekFrom::End(0)).map_err(OpticaldiscsError::Io)?;
        if file_len % SECTOR_SIZE != 0 {
            return Err(OpticaldiscsError::UnsupportedFormat(
                "not a cooked 2048-byte ISO (length is not a sector multiple)".into(),
            ));
        }
        let total_sectors = u32::try_from(file_len / SECTOR_SIZE)
            .map_err(|_| OpticaldiscsError::UnsupportedFormat("image too large to edit".into()))?;

        // Reject containers we cannot write to.
        let head = read_at(&mut rw, 0, 16)?;
        if head.len() >= 8 && &head[0..8] == CHD_MAGIC {
            return Err(OpticaldiscsError::UnsupportedFormat(
                "CHD is not writable for El Torito editing; convert to .iso first".into(),
            ));
        }
        if head.len() >= 12 && head[0..12] == RAW_SYNC {
            return Err(OpticaldiscsError::UnsupportedFormat(
                "raw 2352-byte .iso is not supported for editing; convert to a cooked .iso".into(),
            ));
        }

        // Require an ISO 9660 PVD ("CD001" at 16*2048 + 1).
        let iso_id = read_at(&mut rw, PVD_SECTOR * SECTOR_SIZE + 1, 5)?;
        if iso_id != b"CD001" {
            return Err(OpticaldiscsError::UnsupportedFormat(
                "not an ISO 9660 image (no CD001 at sector 16)".into(),
            ));
        }

        // Boot Record VD at sector 17.
        let vd = read_at(
            &mut rw,
            BOOT_RECORD_VD_SECTOR * SECTOR_SIZE,
            SECTOR_SIZE as usize,
        )?;
        if vd[0] != 0x00 || &vd[1..6] != b"CD001" || &vd[7..7 + EL_TORITO_ID.len()] != EL_TORITO_ID
        {
            return Err(OpticaldiscsError::UnsupportedFormat(
                "image is not El Torito (no Boot Record VD at sector 17)".into(),
            ));
        }
        let catalog_lba = u32::from_le_bytes([vd[71], vd[72], vd[73], vd[74]]);

        // Read and parse the boot catalog.
        let catalog = read_catalog(&mut rw, catalog_lba, total_sectors)?;
        let (validation_id, slots, catalog_bytes) = parse_catalog(&mut rw, &catalog)?;
        let catalog_span = sectors_for(catalog_bytes as u64).max(1);

        Ok(Self {
            rw,
            total_sectors,
            catalog_lba,
            catalog_span,
            validation_id,
            slots,
            dirty_catalog: false,
        })
    }

    /// The boot entries currently staged, in catalog order (index 0 is the
    /// initial/default entry). Reflects pending, uncommitted edits.
    pub fn entries(&self) -> Vec<BootEntry> {
        self.slots.iter().map(|s| s.entry.clone()).collect()
    }

    // ── metadata edits (in place) ──────────────────────────────────────────────

    /// Set entry `index` bootable (`0x88`) or not (`0x00`).
    pub fn set_bootable(&mut self, index: usize, bootable: bool) -> Result<()> {
        self.slot_mut(index)?.entry.bootable = bootable;
        self.dirty_catalog = true;
        Ok(())
    }

    /// Set the platform of entry `index`.
    pub fn set_platform(&mut self, index: usize, platform: Platform) -> Result<()> {
        self.slot_mut(index)?.entry.platform = platform;
        self.dirty_catalog = true;
        Ok(())
    }

    /// Set the media type of entry `index`. Recomputes `image_size` (the on-disc
    /// bytes are unchanged; the consumer should also `replace_image` if the new
    /// media type implies a different geometry).
    pub fn set_media_type(&mut self, index: usize, media: BootMediaType) -> Result<()> {
        let slot = self.slot_mut(index)?;
        slot.entry.media_type = media;
        slot.entry.image_size = image_size_from_mbr(media, slot.entry.sector_count, None);
        self.dirty_catalog = true;
        Ok(())
    }

    /// Set the system-type byte of entry `index`.
    pub fn set_system_type(&mut self, index: usize, system_type: u8) -> Result<()> {
        self.slot_mut(index)?.entry.system_type = system_type;
        self.dirty_catalog = true;
        Ok(())
    }

    // ── boot-image replacement ─────────────────────────────────────────────────

    /// Replace entry `index`'s boot image with `new_image` (an opaque blob).
    ///
    /// Same byte length → overwritten in place at the existing `load_rba`.
    /// Different length → relocated to appended free space on commit, with
    /// `load_rba` / `sector_count` / `image_size` and the catalog updated.
    pub fn replace_image(
        &mut self,
        index: usize,
        new_image: &[u8],
        media: BootMediaType,
    ) -> Result<()> {
        let sector_count = sector_count_for(new_image.len());
        let mbr = if media == BootMediaType::HardDisk {
            new_image.get(0..512)
        } else {
            None
        };
        let image_size = image_size_from_mbr(media, sector_count, mbr);

        let slot = self.slot_mut(index)?;
        let same_size = new_image.len() as u64 == slot.entry.image_size
            && slot.entry.load_rba != 0
            && media == slot.entry.media_type;

        slot.entry.media_type = media;
        slot.entry.sector_count = sector_count;
        slot.entry.image_size = image_size;
        slot.pending = Some(PendingImage {
            bytes: new_image.to_vec(),
            in_place: same_size,
        });

        // A same-size, same-media replace leaves every catalog field unchanged.
        if !same_size {
            self.dirty_catalog = true;
        }
        Ok(())
    }

    // ── structural edits ───────────────────────────────────────────────────────

    /// Append a new boot entry carrying `image`. Returns its index.
    pub fn add_entry(&mut self, spec: NewBootEntry, image: &[u8]) -> Result<usize> {
        let sector_count = sector_count_for(image.len());
        let mbr = if spec.media_type == BootMediaType::HardDisk {
            image.get(0..512)
        } else {
            None
        };
        let image_size = image_size_from_mbr(spec.media_type, sector_count, mbr);

        self.slots.push(Slot {
            entry: BootEntry {
                platform: spec.platform,
                bootable: spec.bootable,
                media_type: spec.media_type,
                load_rba: 0, // assigned on commit (appended)
                sector_count,
                system_type: spec.system_type,
                image_size,
                id: spec.id,
            },
            load_segment: 0,
            selection: 0,
            pending: Some(PendingImage {
                bytes: image.to_vec(),
                in_place: false,
            }),
        });
        self.dirty_catalog = true;
        Ok(self.slots.len() - 1)
    }

    /// Remove boot entry `index`. Its image bytes become dead space (leaked).
    /// Rejects removing the last remaining entry (a catalog needs a default).
    pub fn remove_entry(&mut self, index: usize) -> Result<()> {
        if index >= self.slots.len() {
            return Err(OpticaldiscsError::NotFound(format!(
                "boot entry index {index} out of range"
            )));
        }
        if self.slots.len() == 1 {
            return Err(OpticaldiscsError::InvalidData(
                "cannot remove the last boot entry".into(),
            ));
        }
        self.slots.remove(index);
        self.dirty_catalog = true;
        Ok(())
    }

    // ── commit ─────────────────────────────────────────────────────────────────

    /// Write all staged edits to the image.
    ///
    /// Order (for crash-safety): staged images are written first (in place, or
    /// appended and their `load_rba` assigned), then the catalog (rewritten in
    /// its existing span or relocated to the end), then — only if the catalog
    /// moved or the image grew — the Boot Record VD pointer and the PVD size.
    pub fn commit(mut self) -> Result<()> {
        if self.slots.is_empty() {
            return Err(OpticaldiscsError::InvalidData(
                "cannot commit a catalog with no boot entries".into(),
            ));
        }

        // 1. Write staged images: in-place overwrites, then appends.
        for i in 0..self.slots.len() {
            let Some(pending) = self.slots[i].pending.take() else {
                continue;
            };
            if pending.in_place {
                let off = self.slots[i].entry.load_rba as u64 * SECTOR_SIZE;
                write_at(&mut self.rw, off, &pending.bytes)?;
            } else {
                let load_rba = self.total_sectors;
                let off = load_rba as u64 * SECTOR_SIZE;
                write_sectors(&mut self.rw, off, &pending.bytes)?;
                self.total_sectors += sectors_for(pending.bytes.len() as u64);
                self.slots[i].entry.load_rba = load_rba;
                self.dirty_catalog = true;
            }
        }

        // 2. Rewrite the catalog if anything changed.
        let mut grew = false;
        if self.dirty_catalog {
            let catalog = serialize_catalog(&self.slots, &self.validation_id);
            let needed = sectors_for(catalog.len() as u64).max(1);
            if needed <= self.catalog_span {
                // Fits its existing room: zero the span, then write in place.
                let off = self.catalog_lba as u64 * SECTOR_SIZE;
                let mut buf = vec![0u8; (self.catalog_span as u64 * SECTOR_SIZE) as usize];
                buf[..catalog.len()].copy_from_slice(&catalog);
                write_at(&mut self.rw, off, &buf)?;
            } else {
                // Relocate to appended free space, then repoint the VD (below).
                let new_lba = self.total_sectors;
                let off = new_lba as u64 * SECTOR_SIZE;
                write_sectors(&mut self.rw, off, &catalog)?;
                self.total_sectors += needed;
                self.catalog_lba = new_lba;
                self.catalog_span = needed;
                grew = true;
            }
        }

        // 3. Repoint the Boot Record VD if the catalog was relocated.
        if grew {
            let off = BOOT_RECORD_VD_SECTOR * SECTOR_SIZE + 71;
            write_at(&mut self.rw, off, &self.catalog_lba.to_le_bytes())?;
        }

        // 4. Keep the PVD self-consistent: bump volume_space_size (LE and BE).
        bump_pvd_size(&mut self.rw, self.total_sectors)?;

        self.rw.flush().map_err(OpticaldiscsError::Io)?;
        Ok(())
    }

    fn slot_mut(&mut self, index: usize) -> Result<&mut Slot> {
        self.slots.get_mut(index).ok_or_else(|| {
            OpticaldiscsError::NotFound(format!("boot entry index {index} out of range"))
        })
    }
}

/// Add a Boot Record VD + boot catalog + image to a non-bootable raw ISO
/// (**stretch**).
///
/// Requires a free volume-descriptor slot: the terminator must sit at sector 17
/// with sector 18 free (all-zero) and unreferenced by the volume, so the Boot
/// Record VD can be inserted at 17 and the terminator moved to 18. The catalog
/// and image are appended to free space. Returns a clear error when there is no
/// room (a full remaster would be required).
pub fn make_bootable<RW: Read + Write + Seek>(
    mut rw: RW,
    spec: NewBootEntry,
    image: &[u8],
) -> Result<()> {
    let file_len = rw.seek(SeekFrom::End(0)).map_err(OpticaldiscsError::Io)?;
    if file_len % SECTOR_SIZE != 0 {
        return Err(OpticaldiscsError::UnsupportedFormat(
            "not a cooked 2048-byte ISO (length is not a sector multiple)".into(),
        ));
    }
    let mut total_sectors = u32::try_from(file_len / SECTOR_SIZE)
        .map_err(|_| OpticaldiscsError::UnsupportedFormat("image too large to edit".into()))?;

    let head = read_at(&mut rw, 0, 16)?;
    if head.len() >= 8 && &head[0..8] == CHD_MAGIC {
        return Err(OpticaldiscsError::UnsupportedFormat(
            "CHD is not writable; convert to .iso first".into(),
        ));
    }
    if head.len() >= 12 && head[0..12] == RAW_SYNC {
        return Err(OpticaldiscsError::UnsupportedFormat(
            "raw 2352-byte .iso is not supported; convert to a cooked .iso".into(),
        ));
    }
    let iso_id = read_at(&mut rw, PVD_SECTOR * SECTOR_SIZE + 1, 5)?;
    if iso_id != b"CD001" {
        return Err(OpticaldiscsError::UnsupportedFormat(
            "not an ISO 9660 image".into(),
        ));
    }

    // Must not already be El Torito.
    let s17 = read_at(
        &mut rw,
        BOOT_RECORD_VD_SECTOR * SECTOR_SIZE,
        SECTOR_SIZE as usize,
    )?;
    if s17[0] == 0x00 && &s17[1..6] == b"CD001" && &s17[7..7 + EL_TORITO_ID.len()] == EL_TORITO_ID {
        return Err(OpticaldiscsError::UnsupportedFormat(
            "image is already El Torito".into(),
        ));
    }

    // Need the terminator at sector 17 (type 0xFF, "CD001") and sector 18 free.
    if s17[0] != 0xFF || &s17[1..6] != b"CD001" {
        return Err(OpticaldiscsError::UnsupportedFormat(
            "no free volume-descriptor slot for a Boot Record VD (needs remaster)".into(),
        ));
    }
    let s18 = read_at(&mut rw, 18 * SECTOR_SIZE, SECTOR_SIZE as usize)?;
    if s18.iter().any(|&b| b != 0) {
        return Err(OpticaldiscsError::UnsupportedFormat(
            "sector 18 is occupied; no room for the moved terminator (needs remaster)".into(),
        ));
    }

    // Append the boot image, then the single-entry catalog.
    let image_lba = total_sectors;
    write_sectors(&mut rw, image_lba as u64 * SECTOR_SIZE, image)?;
    total_sectors += sectors_for(image.len() as u64);

    let sector_count = sector_count_for(image.len());
    let mbr = if spec.media_type == BootMediaType::HardDisk {
        image.get(0..512)
    } else {
        None
    };
    let image_size = image_size_from_mbr(spec.media_type, sector_count, mbr);
    let slot = Slot {
        entry: BootEntry {
            platform: spec.platform,
            bootable: spec.bootable,
            media_type: spec.media_type,
            load_rba: image_lba,
            sector_count,
            system_type: spec.system_type,
            image_size,
            id: spec.id,
        },
        load_segment: 0,
        selection: 0,
        pending: None,
    };
    let catalog = serialize_catalog(std::slice::from_ref(&slot), &[0u8; 24]);
    let catalog_lba = total_sectors;
    write_sectors(&mut rw, catalog_lba as u64 * SECTOR_SIZE, &catalog)?;
    total_sectors += sectors_for(catalog.len() as u64).max(1);

    // Insert the Boot Record VD at 17 (pointing at the catalog), move the
    // terminator to 18, then bump the PVD size.
    let mut vd = vec![0u8; SECTOR_SIZE as usize];
    vd[0] = 0x00;
    vd[1..6].copy_from_slice(b"CD001");
    vd[6] = 0x01;
    vd[7..7 + EL_TORITO_ID.len()].copy_from_slice(EL_TORITO_ID);
    vd[71..75].copy_from_slice(&catalog_lba.to_le_bytes());
    write_at(&mut rw, BOOT_RECORD_VD_SECTOR * SECTOR_SIZE, &vd)?;

    let mut term = vec![0u8; SECTOR_SIZE as usize];
    term[0] = 0xFF;
    term[1..6].copy_from_slice(b"CD001");
    term[6] = 0x01;
    write_at(&mut rw, 18 * SECTOR_SIZE, &term)?;

    bump_pvd_size(&mut rw, total_sectors)?;
    rw.flush().map_err(OpticaldiscsError::Io)?;
    Ok(())
}

// ── catalog (de)serialization ──────────────────────────────────────────────────

/// Read up to 16 cooked sectors of boot catalog, bounded by the image length.
fn read_catalog<RW: Read + Seek>(
    rw: &mut RW,
    base_lba: u32,
    total_sectors: u32,
) -> Result<Vec<u8>> {
    let avail = total_sectors.saturating_sub(base_lba).min(16);
    if avail == 0 {
        return Err(OpticaldiscsError::InvalidData(
            "boot catalog pointer is past the end of the image".into(),
        ));
    }
    read_at(
        rw,
        base_lba as u64 * SECTOR_SIZE,
        (avail as u64 * SECTOR_SIZE) as usize,
    )
}

/// Parse a boot catalog into `(validation_id, slots, bytes_consumed)`, reading
/// each HDD entry's MBR through `rw` for sizing.
#[allow(clippy::type_complexity)]
fn parse_catalog<RW: Read + Seek>(rw: &mut RW, buf: &[u8]) -> Result<([u8; 24], Vec<Slot>, usize)> {
    if buf.len() < 64 {
        return Err(OpticaldiscsError::InvalidData(
            "boot catalog is too short".into(),
        ));
    }
    let val = &buf[0..32];
    if val[0] != 0x01 || val[30] != 0x55 || val[31] != 0xAA {
        return Err(OpticaldiscsError::InvalidData(
            "invalid El Torito validation entry".into(),
        ));
    }
    let mut validation_id = [0u8; 24];
    validation_id.copy_from_slice(&val[4..28]);
    let default_platform = Platform::from_byte(val[1]);

    let mut slots = Vec::new();
    let default = parse_slot(rw, &buf[32..64], default_platform, None, false);
    if default.entry.bootable || default.entry.load_rba != 0 {
        slots.push(default);
    }

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
            slots.push(parse_slot(
                rw,
                &buf[off..off + 32],
                platform,
                section_id.clone(),
                true,
            ));
            off += 32;
        }
        if header_id == 0x91 {
            break;
        }
    }

    if slots.is_empty() {
        return Err(OpticaldiscsError::InvalidData(
            "boot catalog has no boot entries".into(),
        ));
    }
    Ok((validation_id, slots, off))
}

/// Parse a single 32-byte entry into a [`Slot`], sizing HDD images via `rw`.
fn parse_slot<RW: Read + Seek>(
    rw: &mut RW,
    e: &[u8],
    platform: Platform,
    id: Option<String>,
    is_section: bool,
) -> Slot {
    let bootable = e[0] == 0x88;
    let media_type = BootMediaType::from_byte(e[1]);
    let load_segment = u16::from_le_bytes([e[2], e[3]]);
    let system_type = e[4];
    let sector_count = u16::from_le_bytes([e[6], e[7]]);
    let load_rba = u32::from_le_bytes([e[8], e[9], e[10], e[11]]);
    let selection = if is_section { e[12] } else { 0 };

    let mbr = if media_type == BootMediaType::HardDisk {
        read_at(rw, load_rba as u64 * SECTOR_SIZE, 512).ok()
    } else {
        None
    };
    let image_size = image_size_from_mbr(media_type, sector_count, mbr.as_deref());

    Slot {
        entry: BootEntry {
            platform,
            bootable,
            media_type,
            load_rba,
            sector_count,
            system_type,
            image_size,
            id,
        },
        load_segment,
        selection,
        pending: None,
    }
}

/// Serialize a catalog: validation entry, the default (initial) entry from
/// `slots[0]`, then one section header + section entry per remaining slot.
fn serialize_catalog(slots: &[Slot], validation_id: &[u8; 24]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32 * (1 + 2 * slots.len()));

    // Validation entry — platform is the default entry's platform.
    let mut v = [0u8; 32];
    v[0] = 0x01;
    v[1] = slots[0].entry.platform.to_byte();
    v[4..28].copy_from_slice(validation_id);
    v[30] = 0x55;
    v[31] = 0xAA;
    let checksum = validation_checksum(&v);
    v[28..30].copy_from_slice(&checksum.to_le_bytes());
    buf.extend_from_slice(&v);

    // Initial/default entry.
    buf.extend_from_slice(&serialize_entry(&slots[0], false));

    // Section entries, each under its own single-entry section header.
    let section_count = slots.len() - 1;
    for (i, slot) in slots[1..].iter().enumerate() {
        let mut h = [0u8; 32];
        h[0] = if i + 1 == section_count { 0x91 } else { 0x90 };
        h[1] = slot.entry.platform.to_byte();
        h[2..4].copy_from_slice(&1u16.to_le_bytes());
        if let Some(id) = &slot.entry.id {
            let bytes = id.as_bytes();
            let n = bytes.len().min(28);
            h[4..4 + n].copy_from_slice(&bytes[..n]);
        }
        buf.extend_from_slice(&h);
        buf.extend_from_slice(&serialize_entry(slot, true));
    }

    buf
}

/// Serialize one 32-byte initial/section boot entry.
fn serialize_entry(slot: &Slot, is_section: bool) -> [u8; 32] {
    let e = &slot.entry;
    let mut b = [0u8; 32];
    b[0] = if e.bootable { 0x88 } else { 0x00 };
    b[1] = e.media_type.to_byte();
    b[2..4].copy_from_slice(&slot.load_segment.to_le_bytes());
    b[4] = e.system_type;
    b[6..8].copy_from_slice(&e.sector_count.to_le_bytes());
    b[8..12].copy_from_slice(&e.load_rba.to_le_bytes());
    if is_section {
        b[12] = slot.selection;
    }
    b
}

/// The balancing value for bytes 28..30 so the validation entry's sixteen
/// little-endian u16 words sum to zero. `v[28..30]` must be zero on entry.
fn validation_checksum(v: &[u8; 32]) -> u16 {
    let mut sum: u16 = 0;
    for w in v.chunks_exact(2) {
        sum = sum.wrapping_add(u16::from_le_bytes([w[0], w[1]]));
    }
    (0u16).wrapping_sub(sum)
}

// ── low-level I/O helpers ──────────────────────────────────────────────────────

/// Bump the PVD `volume_space_size` (both the LE copy at 80..84 and the BE copy
/// at 84..88) to `total_sectors`.
fn bump_pvd_size<RW: Write + Seek>(rw: &mut RW, total_sectors: u32) -> Result<()> {
    let base = PVD_SECTOR * SECTOR_SIZE;
    write_at(rw, base + 80, &total_sectors.to_le_bytes())?;
    write_at(rw, base + 84, &total_sectors.to_be_bytes())?;
    Ok(())
}

fn read_at<RW: Read + Seek>(rw: &mut RW, offset: u64, len: usize) -> Result<Vec<u8>> {
    rw.seek(SeekFrom::Start(offset))
        .map_err(OpticaldiscsError::Io)?;
    let mut buf = vec![0u8; len];
    rw.read_exact(&mut buf).map_err(OpticaldiscsError::Io)?;
    Ok(buf)
}

fn write_at<RW: Write + Seek>(rw: &mut RW, offset: u64, data: &[u8]) -> Result<()> {
    rw.seek(SeekFrom::Start(offset))
        .map_err(OpticaldiscsError::Io)?;
    rw.write_all(data).map_err(OpticaldiscsError::Io)?;
    Ok(())
}

/// Write `data` at `offset`, zero-padded up to a whole number of 2048-byte
/// sectors so appended blobs never leave a partial trailing sector.
fn write_sectors<RW: Write + Seek>(rw: &mut RW, offset: u64, data: &[u8]) -> Result<()> {
    let padded = sectors_for(data.len() as u64) as usize * SECTOR_SIZE as usize;
    let mut buf = vec![0u8; padded];
    buf[..data.len()].copy_from_slice(data);
    write_at(rw, offset, &buf)
}

/// Number of 2048-byte cooked sectors needed to hold `bytes` (rounded up).
fn sectors_for(bytes: u64) -> u32 {
    bytes.div_ceil(SECTOR_SIZE) as u32
}

/// Number of virtual 512-byte sectors for a boot image of `len` bytes (the
/// entry's `sector_count` field), clamped to `u16::MAX`.
fn sector_count_for(len: usize) -> u16 {
    let sectors = (len as u64).div_ceil(512);
    sectors.min(u16::MAX as u64) as u16
}
