//! UDF (Universal Disk Format, OSTA / ECMA-167) browser — read-only.
//!
//! Handles the **physical-partition** layout used by DVDs, DVD-Video, and most
//! data discs (UDF 1.02–2.01, partition-map type 1) **and** the **metadata
//! partition** (type-2 partition map) used by Blu-ray / UDF 2.50+. The parse
//! chain is: Anchor Volume Descriptor Pointer (sector 256) → Volume Descriptor
//! Sequence (Partition Descriptors for partition starts, Logical Volume
//! Descriptor for the block size, partition maps, and File Set Descriptor
//! location) → File Set Descriptor (root directory ICB) → File Entry / Extended
//! File Entry → File Identifier Descriptors (directory contents).
//!
//! Every on-disc address is a `(logical block number, partition reference)`
//! pair. A physical partition reference resolves directly; a metadata partition
//! reference is translated through the **metadata file**'s extents into the
//! underlying physical partition. On a Blu-ray this means file *entries* and
//! directories live in the metadata partition while the actual file *data*
//! extents (carried by `long_ad`s with an explicit partition reference) resolve
//! to the physical partition.
//!
//! Not yet handled: sparable/virtual partitions and named streams.

use super::entry::{EntryType, FileEntry};
use super::filesystem::{Filesystem, FilesystemError};
use crate::sector_reader::SectorReader;

const SECTOR: u64 = 2048;
/// The first Anchor Volume Descriptor Pointer is mandated at LSN 256.
const AVDP_LSN: u64 = 256;

// Descriptor tag identifiers (ECMA-167 §3/4).
const TAG_AVDP: u16 = 2;
const TAG_PARTITION: u16 = 5;
const TAG_LOGICAL_VOLUME: u16 = 6;
const TAG_TERMINATING: u16 = 8;
const TAG_FILE_SET: u16 = 256;
const TAG_FILE_ID: u16 = 257;
const TAG_FILE_ENTRY: u16 = 261;
const TAG_EXTENDED_FILE_ENTRY: u16 = 266;

// ICB file types.
const FT_DIRECTORY: u8 = 4;
const FT_SYMLINK: u8 = 12;

/// Cap on assembled file / directory size (guards a corrupt descriptor).
const MAX_BYTES: u64 = 8u64 * 1024 * 1024 * 1024;

/// A UDF partition, addressed by its partition-reference index.
enum Partition {
    /// A physical (type-1) partition: block `lbn` is at LSN `start + lbn`.
    Physical { start: u64 },
    /// A metadata (type-2) partition: block `lbn` maps into the physical
    /// partition starting at `phys_start` via the metadata file's `extents`
    /// (each `(physical-partition-relative lbn, byte length)`).
    Metadata {
        phys_start: u64,
        extents: Vec<(u64, u64)>,
    },
    /// A partition map this browser does not support (virtual / sparable). Kept
    /// so partition-reference indices stay aligned; errors only if referenced.
    Unsupported,
}

/// A partition map parsed from the LVD, before Partition Descriptors are joined.
enum RawMap {
    Physical {
        number: u16,
        start_override: Option<u64>,
    },
    Metadata {
        phys_partition: u16,
        meta_file_lbn: u64,
    },
    Unsupported,
}

// ── UdfFilesystem ────────────────────────────────────────────────────────────────

/// Read-only UDF browser over a [`SectorReader`].
pub struct UdfFilesystem {
    reader: Box<dyn SectorReader>,
    block_size: u64,
    /// Partitions indexed by partition-reference number.
    partitions: Vec<Partition>,
    /// Root directory ICB, as `(lbn, partition reference)`.
    root_lbn: u64,
    root_partref: usize,
    volume_id: Option<String>,
}

/// A parsed File Entry / Extended File Entry.
struct Icb {
    file_type: u8,
    size: u64,
    /// Data extents as `(lbn, partition reference, byte length, extent type)`.
    extents: Vec<(u64, usize, u64, u8)>,
    /// Inline data (`adtype == 3`), if the file body is embedded in the ICB.
    inline: Option<Vec<u8>>,
}

impl Icb {
    fn is_dir(&self) -> bool {
        self.file_type == FT_DIRECTORY
    }
    fn is_symlink(&self) -> bool {
        self.file_type == FT_SYMLINK
    }
}

/// Pack a `(lbn, partref)` address into a single [`FileEntry::location`] value.
/// `partref` occupies the high 16 bits; `lbn` the low 48. A physical-only disc
/// uses `partref == 0`, so this is backward compatible with a bare LBN.
fn pack_loc(lbn: u64, partref: usize) -> u64 {
    ((partref as u64) << 48) | (lbn & 0x0000_FFFF_FFFF_FFFF)
}

fn unpack_loc(loc: u64) -> (u64, usize) {
    (loc & 0x0000_FFFF_FFFF_FFFF, (loc >> 48) as usize)
}

/// Read just the UDF volume identifier, without building a whole filesystem.
///
/// [`crate::detect::DiscImageInfo`] needs the label during probing, where it has
/// only a `&mut dyn SectorReader` and cannot hand the owned `Box` that
/// [`UdfFilesystem::new`] requires. Mirrors `cdi::read_volume_id` /
/// `opera::read_label`.
///
/// Walks AVDP → Volume Descriptor Sequence and decodes the Logical Volume
/// Descriptor's identifier dstring. Returns `None` for anything that is not a
/// readable UDF volume.
pub(crate) fn read_label(reader: &mut dyn SectorReader) -> Option<String> {
    let avdp = read_tagged(reader, AVDP_LSN * SECTOR, TAG_AVDP)
        .or_else(|_| read_tagged(reader, 512 * SECTOR, TAG_AVDP))
        .ok()?;
    let vds_len = u32(&avdp, 16) as u64;
    let vds_loc = u32(&avdp, 20) as u64;

    for i in 0..vds_len.div_ceil(SECTOR) {
        let d = match reader.read_bytes((vds_loc + i) * SECTOR, SECTOR as usize) {
            Ok(b) if b.len() >= SECTOR as usize => b,
            _ => break,
        };
        match tag_id(&d) {
            TAG_LOGICAL_VOLUME => return decode_dstring(&d[84..84 + 128]),
            TAG_TERMINATING => break,
            _ => {}
        }
    }
    None
}

impl UdfFilesystem {
    /// Open a UDF volume, walking AVDP → VDS → FSD to locate the root directory.
    pub fn new(mut reader: Box<dyn SectorReader>) -> Result<Self, FilesystemError> {
        // ── Anchor Volume Descriptor Pointer (try 256, then 512) ───────────
        let avdp = read_tagged(reader.as_mut(), AVDP_LSN * SECTOR, TAG_AVDP)
            .or_else(|_| read_tagged(reader.as_mut(), 512 * SECTOR, TAG_AVDP))
            .map_err(|_| FilesystemError::Parse("no UDF anchor descriptor".into()))?;
        let vds_len = u32(&avdp, 16) as u64;
        let vds_loc = u32(&avdp, 20) as u64;

        // ── Volume Descriptor Sequence ─────────────────────────────────────
        // Partition Descriptors: partition number → start LSN.
        let mut pd_start: std::collections::HashMap<u16, u64> = std::collections::HashMap::new();
        let mut block_size = SECTOR;
        let mut fsd_lbn: Option<u64> = None;
        let mut fsd_partref: usize = 0;
        let mut volume_id = None;
        // Raw partition-map descriptions parsed from the LVD.
        let mut raw_maps: Vec<RawMap> = Vec::new();
        let nblocks = vds_len.div_ceil(SECTOR);
        for i in 0..nblocks {
            let d = match reader.read_bytes((vds_loc + i) * SECTOR, SECTOR as usize) {
                Ok(b) if b.len() >= SECTOR as usize => b,
                _ => break,
            };
            match tag_id(&d) {
                TAG_PARTITION => {
                    let number = u16(&d, 22);
                    pd_start.insert(number, u32(&d, 188) as u64);
                }
                TAG_LOGICAL_VOLUME => {
                    block_size = u32(&d, 212) as u64;
                    // LogicalVolumeContentsUse (offset 248) is a long_ad → FSD.
                    fsd_lbn = Some(u32(&d, 252) as u64);
                    fsd_partref = u16(&d, 256) as usize;
                    volume_id = decode_dstring(&d[84..84 + 128]);
                    raw_maps = parse_partition_maps(&d);
                }
                TAG_TERMINATING => break,
                _ => {}
            }
        }
        let fsd_lbn = fsd_lbn
            .ok_or_else(|| FilesystemError::Parse("UDF: no logical volume descriptor".into()))?;
        if block_size == 0 {
            return Err(FilesystemError::InvalidData("UDF: zero block size".into()));
        }

        // ── Build the partition table (physical first, then metadata) ──────
        // Fall back to a single physical partition when the LVD carried no maps
        // (older/minimal images), using the first Partition Descriptor.
        if raw_maps.is_empty() {
            if let Some((_, &start)) = pd_start.iter().next() {
                raw_maps.push(RawMap::Physical {
                    number: 0,
                    start_override: Some(start),
                });
            }
        }
        let mut partitions: Vec<Partition> = Vec::with_capacity(raw_maps.len());
        for m in &raw_maps {
            match *m {
                RawMap::Physical {
                    number,
                    start_override,
                } => {
                    let start = start_override
                        .or_else(|| pd_start.get(&number).copied())
                        .ok_or_else(|| FilesystemError::Parse("UDF: no partition".into()))?;
                    partitions.push(Partition::Physical { start });
                }
                RawMap::Metadata {
                    phys_partition,
                    meta_file_lbn,
                } => {
                    let phys_start = *pd_start.get(&phys_partition).ok_or_else(|| {
                        FilesystemError::Parse(
                            "UDF: metadata partition references unknown partition".into(),
                        )
                    })?;
                    // The metadata file's own ICB lives in the physical partition.
                    let byte = (phys_start + meta_file_lbn) * block_size;
                    let icb = read_fe(reader.as_mut(), byte, block_size, 0)?;
                    // Its extents are physical-partition-relative (lbn, byte length).
                    let extents = icb
                        .extents
                        .iter()
                        .map(|&(lbn, _pr, len, _et)| (lbn, len))
                        .collect();
                    partitions.push(Partition::Metadata {
                        phys_start,
                        extents,
                    });
                }
                RawMap::Unsupported => partitions.push(Partition::Unsupported),
            }
        }
        if partitions.is_empty() {
            return Err(FilesystemError::Parse("UDF: no partitions".into()));
        }

        // ── File Set Descriptor → root directory ICB ───────────────────────
        let fsd_byte = resolve(&partitions, block_size, fsd_lbn, fsd_partref)?;
        let fsd = read_tagged(reader.as_mut(), fsd_byte, TAG_FILE_SET)
            .map_err(|_| FilesystemError::Parse("UDF: file set descriptor not found".into()))?;
        // RootDirectoryICB is a long_ad at offset 400 (lbn @ 404, partref @ 408).
        let root_lbn = u32(&fsd, 404) as u64;
        let root_partref = u16(&fsd, 408) as usize;

        Ok(Self {
            reader,
            block_size,
            partitions,
            root_lbn,
            root_partref,
            volume_id,
        })
    }

    fn byte_of(&self, lbn: u64, partref: usize) -> Result<u64, FilesystemError> {
        resolve(&self.partitions, self.block_size, lbn, partref)
    }

    /// Read and parse a File Entry / Extended File Entry at `(lbn, partref)`.
    fn read_icb(&mut self, lbn: u64, partref: usize) -> Result<Icb, FilesystemError> {
        let byte = self.byte_of(lbn, partref)?;
        read_fe(self.reader.as_mut(), byte, self.block_size, partref)
    }

    /// Read a file's full content (truncated to its exact size).
    fn read_body(&mut self, icb: &Icb) -> Result<Vec<u8>, FilesystemError> {
        if let Some(inline) = &icb.inline {
            let n = (icb.size as usize).min(inline.len());
            return Ok(inline[..n].to_vec());
        }
        let mut out = Vec::new();
        for &(lbn, partref, length, etype) in &icb.extents {
            if out.len() as u64 > MAX_BYTES {
                break;
            }
            if etype != 0 {
                // Not-recorded extent → sparse zeros.
                out.resize(out.len() + length as usize, 0);
            } else {
                let byte = self.byte_of(lbn, partref)?;
                let bytes = self
                    .reader
                    .read_bytes(byte, length as usize)
                    .map_err(sector_to_fs_err)?;
                out.extend_from_slice(&bytes);
            }
        }
        if icb.size > 0 && (icb.size as usize) < out.len() {
            out.truncate(icb.size as usize);
        }
        Ok(out)
    }

    /// Read only the `[offset, offset+length)` byte range of a file, touching
    /// just the extents that overlap it. This is what makes partial reads of a
    /// multi-gigabyte Blu-ray stream cheap instead of loading the whole file.
    fn read_body_range(
        &mut self,
        icb: &Icb,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, FilesystemError> {
        if let Some(inline) = &icb.inline {
            let start = (offset as usize).min(inline.len());
            let end = start.saturating_add(length).min(inline.len());
            return Ok(inline[start..end].to_vec());
        }
        // Clamp the request to the file's real size.
        let file_end = if icb.size > 0 { icb.size } else { u64::MAX };
        let want_start = offset.min(file_end);
        let want_end = want_start.saturating_add(length as u64).min(file_end);

        let mut out = Vec::new();
        let mut pos = 0u64; // running byte position within the file
        for &(lbn, partref, ext_len, etype) in &icb.extents {
            if pos >= want_end {
                break;
            }
            let ext_start = pos;
            let ext_end = pos + ext_len;
            pos = ext_end;
            if ext_end <= want_start {
                continue; // entirely before the requested range
            }
            let read_from = want_start.max(ext_start);
            let read_to = want_end.min(ext_end);
            let within = read_from - ext_start; // offset into this extent
            let n = (read_to - read_from) as usize;
            if n == 0 {
                continue;
            }
            if etype != 0 {
                out.resize(out.len() + n, 0);
            } else {
                let byte = self.byte_of(lbn, partref)? + within;
                let bytes = self.reader.read_bytes(byte, n).map_err(sector_to_fs_err)?;
                out.extend_from_slice(&bytes);
            }
        }
        Ok(out)
    }

    /// Parse a directory's File Identifier Descriptors into
    /// `(name, is_dir, icb_lbn, icb_partref)`.
    fn parse_dir(&self, data: &[u8]) -> Vec<(String, bool, u64, usize)> {
        let mut out = Vec::new();
        let mut o = 0usize;
        while o + 38 <= data.len() {
            if tag_id(&data[o..]) != TAG_FILE_ID {
                break;
            }
            let characteristics = data[o + 18];
            let l_fi = data[o + 19] as usize;
            // ICB is a long_ad at offset 20: lbn @ +24, partref @ +28.
            let icb_lbn = u32(data, o + 24) as u64;
            let icb_partref = u16(data, o + 28) as usize;
            let l_iu = u16(data, o + 36) as usize;
            let name_off = o + 38 + l_iu;
            let total = ((38 + l_iu + l_fi) + 3) & !3; // 4-byte aligned
            if name_off + l_fi > data.len() {
                break;
            }
            let is_parent = characteristics & 0x08 != 0;
            let is_dir = characteristics & 0x02 != 0;
            let deleted = characteristics & 0x10 != 0;
            if !is_parent && !deleted && l_fi > 0 {
                let name = decode_dchars(&data[name_off..name_off + l_fi]);
                if !name.is_empty() {
                    out.push((name, is_dir, icb_lbn, icb_partref));
                }
            }
            if total == 0 {
                break;
            }
            o += total;
        }
        out
    }

    fn list_lbn(
        &mut self,
        lbn: u64,
        partref: usize,
        parent_path: &str,
    ) -> Result<Vec<FileEntry>, FilesystemError> {
        let icb = self.read_icb(lbn, partref)?;
        let data = self.read_body(&icb)?;
        let mut entries = Vec::new();
        for (name, is_dir, child_lbn, child_partref) in self.parse_dir(&data) {
            let path = if parent_path == "/" {
                format!("/{name}")
            } else {
                format!("{parent_path}/{name}")
            };
            let loc = pack_loc(child_lbn, child_partref);
            let entry = if is_dir {
                FileEntry::new_directory(name, path, loc)
            } else {
                // Read the child ICB for its size / symlink flag.
                match self.read_icb(child_lbn, child_partref) {
                    Ok(c) => {
                        let mut fe = FileEntry::new_file(name, path, c.size, loc);
                        if c.is_symlink() {
                            // UDF path-component symlinks aren't decoded yet; mark it.
                            fe.symlink_target = Some(String::new());
                        }
                        fe
                    }
                    Err(_) => continue,
                }
            };
            entries.push(entry);
        }
        entries.sort_by(|a, b| match (a.entry_type, b.entry_type) {
            (EntryType::Directory, EntryType::File) => std::cmp::Ordering::Less,
            (EntryType::File, EntryType::Directory) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });
        Ok(entries)
    }
}

impl Filesystem for UdfFilesystem {
    fn root(&mut self) -> Result<FileEntry, FilesystemError> {
        Ok(FileEntry::root(pack_loc(self.root_lbn, self.root_partref)))
    }

    fn list_directory(&mut self, entry: &FileEntry) -> Result<Vec<FileEntry>, FilesystemError> {
        if !entry.is_directory() {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        let (lbn, partref) = unpack_loc(entry.location);
        self.list_lbn(lbn, partref, &entry.path)
    }

    fn read_file(&mut self, entry: &FileEntry) -> Result<Vec<u8>, FilesystemError> {
        if entry.is_directory() {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        let (lbn, partref) = unpack_loc(entry.location);
        let icb = self.read_icb(lbn, partref)?;
        if icb.is_dir() {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        self.read_body(&icb)
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
        let (lbn, partref) = unpack_loc(entry.location);
        let icb = self.read_icb(lbn, partref)?;
        self.read_body_range(&icb, offset, length)
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

    fn allocation_unit(&self) -> Option<u64> {
        // UDF allocates in whole logical blocks; `block_size` is guaranteed
        // non-zero at construction.
        Some(self.block_size)
    }

    fn volume_name(&self) -> Option<&str> {
        self.volume_id.as_deref()
    }
}

// ── partition resolution ─────────────────────────────────────────────────────

/// Parse the partition maps from a Logical Volume Descriptor.
///
/// `NumberofPartitionMaps` is at offset 268; the map table begins at offset 440.
/// A type-1 map (physical) is 6 bytes with the partition number at +4; a type-2
/// map (64 bytes) carries an EntityID at +4 identifying it — only the
/// `*UDF Metadata Partition` variant is understood here.
fn parse_partition_maps(lvd: &[u8]) -> Vec<RawMap> {
    let nmaps = u32(lvd, 268) as usize;
    let mut out = Vec::new();
    let mut o = 440usize;
    for _ in 0..nmaps {
        if o + 2 > lvd.len() {
            break;
        }
        let mtype = lvd[o];
        let mlen = lvd[o + 1] as usize;
        if mlen == 0 || o + mlen > lvd.len() {
            break;
        }
        match mtype {
            1 => out.push(RawMap::Physical {
                number: u16(lvd, o + 4),
                start_override: None,
            }),
            2 if lvd[o + 4..o + 36]
                .windows(23)
                .any(|w| w == b"*UDF Metadata Partition") =>
            {
                out.push(RawMap::Metadata {
                    phys_partition: u16(lvd, o + 38),
                    meta_file_lbn: u32(lvd, o + 40) as u64,
                });
            }
            _ => out.push(RawMap::Unsupported),
        }
        o += mlen;
    }
    out
}

/// Resolve a `(lbn, partref)` address to an absolute byte offset.
fn resolve(
    partitions: &[Partition],
    block_size: u64,
    lbn: u64,
    partref: usize,
) -> Result<u64, FilesystemError> {
    let part = partitions.get(partref).ok_or_else(|| {
        FilesystemError::Parse(format!("UDF: partition ref {partref} out of range"))
    })?;
    match part {
        Partition::Physical { start } => Ok((start + lbn) * block_size),
        Partition::Metadata {
            phys_start,
            extents,
        } => {
            // Walk the metadata file's extents to map the metadata-partition
            // block `lbn` onto a physical-partition-relative block.
            let mut acc = 0u64;
            for &(ext_lbn, ext_len) in extents {
                let ext_blocks = ext_len.div_ceil(block_size);
                if lbn < acc + ext_blocks {
                    let phys_rel = ext_lbn + (lbn - acc);
                    return Ok((phys_start + phys_rel) * block_size);
                }
                acc += ext_blocks;
            }
            Err(FilesystemError::Parse(
                "UDF: metadata block out of range".into(),
            ))
        }
        Partition::Unsupported => Err(FilesystemError::Parse(
            "UDF: unsupported partition map (virtual/sparable)".into(),
        )),
    }
}

/// Read and parse a File Entry / Extended File Entry at absolute byte offset
/// `byte`. `self_partref` is the partition the ICB itself lives in; `short_ad`
/// extents (which carry no partition reference) inherit it, while `long_ad`
/// extents use their own explicit reference.
fn read_fe(
    reader: &mut dyn SectorReader,
    byte: u64,
    block_size: u64,
    self_partref: usize,
) -> Result<Icb, FilesystemError> {
    let fe = reader
        .read_bytes(byte, block_size as usize)
        .map_err(sector_to_fs_err)?;
    if fe.len() < 176 {
        return Err(FilesystemError::InvalidData("UDF: short file entry".into()));
    }
    let t = tag_id(&fe);
    // ICB tag is at offset 16; file type at +11, flags at +18.
    let file_type = fe[16 + 11];
    let ad_type = u16(&fe, 16 + 18) & 7;
    let size = u64(&fe, 56);
    let (l_ea, l_ad, ad_base) = match t {
        TAG_FILE_ENTRY => (u32(&fe, 168) as usize, u32(&fe, 172) as usize, 176usize),
        TAG_EXTENDED_FILE_ENTRY => (u32(&fe, 208) as usize, u32(&fe, 212) as usize, 216usize),
        _ => {
            return Err(FilesystemError::InvalidData(format!(
                "UDF: expected file entry, got tag {t}"
            )))
        }
    };
    let ad_off = ad_base + l_ea;
    let ad_end = (ad_off + l_ad).min(fe.len());

    if ad_type == 3 {
        // Inline: the file body is the allocation-descriptor area itself.
        let end = ad_off.saturating_add(l_ad).min(fe.len());
        let inline = fe.get(ad_off..end).map(|s| s.to_vec());
        return Ok(Icb {
            file_type,
            size,
            extents: Vec::new(),
            inline,
        });
    }

    let mut extents = Vec::new();
    let mut o = ad_off;
    while o < ad_end {
        let (len_raw, lbn_e, partref, adv) = match ad_type {
            0 => {
                // short_ad: ExtentLength(u32), ExtentPosition(u32). No partref →
                // inherits the ICB's own partition.
                if o + 8 > ad_end {
                    break;
                }
                (u32(&fe, o), u32(&fe, o + 4) as u64, self_partref, 8)
            }
            1 => {
                // long_ad: ExtentLength(u32), lb_addr(lbn u32 + partref u16), impuse(6)
                if o + 16 > ad_end {
                    break;
                }
                (
                    u32(&fe, o),
                    u32(&fe, o + 4) as u64,
                    u16(&fe, o + 8) as usize,
                    16,
                )
            }
            _ => break,
        };
        o += adv;
        let etype = (len_raw >> 30) as u8;
        let length = (len_raw & 0x3FFF_FFFF) as u64;
        if length == 0 {
            break;
        }
        extents.push((lbn_e, partref, length, etype));
    }

    Ok(Icb {
        file_type,
        size,
        extents,
        inline: None,
    })
}

// ── helpers ─────────────────────────────────────────────────────────────────────

fn u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64(b: &[u8], o: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(v)
}
fn tag_id(b: &[u8]) -> u16 {
    if b.len() >= 2 {
        u16(b, 0)
    } else {
        0
    }
}

/// Read a `SECTOR`-sized descriptor at `byte_off` and confirm its tag identifier.
fn read_tagged(
    reader: &mut dyn SectorReader,
    byte_off: u64,
    want: u16,
) -> Result<Vec<u8>, FilesystemError> {
    let d = reader
        .read_bytes(byte_off, SECTOR as usize)
        .map_err(sector_to_fs_err)?;
    if d.len() >= SECTOR as usize && tag_id(&d) == want {
        Ok(d)
    } else {
        Err(FilesystemError::Parse(format!(
            "UDF: expected tag {want} at {byte_off}"
        )))
    }
}

/// Decode an OSTA CS0 d-characters field (used for File Identifier names): the
/// first byte is a compression ID (8 = 8-bit, 16 = 16-bit UTF-16BE); the rest is
/// the name.
fn decode_dchars(b: &[u8]) -> String {
    if b.is_empty() {
        return String::new();
    }
    match b[0] {
        8 => b[1..].iter().map(|&c| c as char).collect(),
        16 => {
            let units: Vec<u16> = b[1..]
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8_lossy(&b[1..]).into_owned(),
    }
}

/// Decode a fixed-length OSTA CS0 dstring: the last byte holds the used length.
fn decode_dstring(field: &[u8]) -> Option<String> {
    if field.len() < 2 {
        return None;
    }
    let used = *field.last().unwrap() as usize;
    if used == 0 || used > field.len() {
        return None;
    }
    let s = decode_dchars(&field[..used]);
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn sector_to_fs_err(e: crate::error::OpticaldiscsError) -> FilesystemError {
    match e {
        crate::error::OpticaldiscsError::Io(io_err) => FilesystemError::Io(io_err),
        e => FilesystemError::InvalidData(e.to_string()),
    }
}

/// True if the image carries a UDF volume-recognition sequence (an `NSR02` or
/// `NSR03` descriptor in the recognition area at sectors 16..).
pub fn detect_udf(reader: &mut dyn SectorReader) -> bool {
    for lsn in 16..=20u64 {
        if let Ok(d) = reader.read_bytes(lsn * SECTOR, 8) {
            if d.len() >= 6 && (&d[1..6] == b"NSR02" || &d[1..6] == b"NSR03") {
                return true;
            }
        }
    }
    false
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dchars_8bit_and_16bit() {
        assert_eq!(decode_dchars(&[8, b'h', b'i']), "hi");
        // 16-bit "AB"
        assert_eq!(decode_dchars(&[16, 0, b'A', 0, b'B']), "AB");
        assert_eq!(decode_dchars(&[]), "");
    }

    #[test]
    fn dstring_uses_last_byte_length() {
        // 8-byte field: compID 8, "OK", zero padding, last byte = used length (3).
        let field = [8u8, b'O', b'K', 0, 0, 0, 0, 3];
        assert_eq!(decode_dstring(&field).as_deref(), Some("OK"));
    }
}
