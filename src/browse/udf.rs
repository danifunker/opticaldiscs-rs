//! UDF (Universal Disk Format, OSTA / ECMA-167) browser — read-only.
//!
//! Handles the common **physical-partition** UDF layout used by DVDs, DVD-Video,
//! and most data discs (UDF 1.02–2.01, partition-map type 1). The parse chain is:
//! Anchor Volume Descriptor Pointer (sector 256) → Volume Descriptor Sequence
//! (Partition Descriptor for the partition start, Logical Volume Descriptor for
//! the block size and the File Set Descriptor location) → File Set Descriptor
//! (root directory ICB) → File Entry / Extended File Entry → File Identifier
//! Descriptors (directory contents).
//!
//! Not yet handled: the **metadata partition** (type-2 partition map) used by
//! Blu-ray / UDF 2.50+, sparable/virtual partitions, and named streams. Such a
//! disc yields a clear error rather than wrong data.

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

// ── UdfFilesystem ────────────────────────────────────────────────────────────────

/// Read-only UDF browser over a [`SectorReader`].
pub struct UdfFilesystem {
    reader: Box<dyn SectorReader>,
    block_size: u64,
    /// Absolute LSN where the (physical) partition starts.
    partition_start: u64,
    /// Partition-relative logical block number of the root directory's ICB.
    root_lbn: u64,
    volume_id: Option<String>,
}

/// A parsed File Entry / Extended File Entry.
struct Icb {
    file_type: u8,
    size: u64,
    /// Data extents as `(partition-relative lbn, byte length, extent type)`.
    extents: Vec<(u64, u64, u8)>,
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
        let mut partition_start: Option<u64> = None;
        let mut block_size = SECTOR;
        let mut fsd_lbn: Option<u64> = None;
        let mut volume_id = None;
        let nblocks = vds_len.div_ceil(SECTOR);
        for i in 0..nblocks {
            let d = match reader.read_bytes((vds_loc + i) * SECTOR, SECTOR as usize) {
                Ok(b) if b.len() >= SECTOR as usize => b,
                _ => break,
            };
            match tag_id(&d) {
                TAG_PARTITION => partition_start = Some(u32(&d, 188) as u64),
                TAG_LOGICAL_VOLUME => {
                    block_size = u32(&d, 212) as u64;
                    // LogicalVolumeContentsUse (offset 248) is a long_ad → FSD.
                    fsd_lbn = Some(u32(&d, 252) as u64);
                    volume_id = decode_dstring(&d[84..84 + 128]);
                }
                TAG_TERMINATING => break,
                _ => {}
            }
        }
        let partition_start =
            partition_start.ok_or_else(|| FilesystemError::Parse("UDF: no partition".into()))?;
        let fsd_lbn = fsd_lbn
            .ok_or_else(|| FilesystemError::Parse("UDF: no logical volume descriptor".into()))?;
        if block_size == 0 {
            return Err(FilesystemError::InvalidData("UDF: zero block size".into()));
        }

        // ── File Set Descriptor → root directory ICB ───────────────────────
        let fsd_byte = (partition_start + fsd_lbn) * block_size;
        let fsd = read_tagged(reader.as_mut(), fsd_byte, TAG_FILE_SET).map_err(|_| {
            FilesystemError::Parse(
                "UDF: file set descriptor not found (metadata-partition discs \
                 such as Blu-ray/UDF 2.50 are not yet supported)"
                    .into(),
            )
        })?;
        // RootDirectoryICB is a long_ad at offset 400.
        let root_lbn = u32(&fsd, 404) as u64;

        Ok(Self {
            reader,
            block_size,
            partition_start,
            root_lbn,
            volume_id,
        })
    }

    fn byte_of(&self, lbn: u64) -> u64 {
        (self.partition_start + lbn) * self.block_size
    }

    /// Read and parse a File Entry / Extended File Entry at partition-relative `lbn`.
    fn read_icb(&mut self, lbn: u64) -> Result<Icb, FilesystemError> {
        let fe = self
            .reader
            .read_bytes(self.byte_of(lbn), self.block_size as usize)
            .map_err(sector_to_fs_err)?;
        if fe.len() < 176 {
            return Err(FilesystemError::InvalidData("UDF: short file entry".into()));
        }
        let t = tag_id(&fe);
        // ICB tag is at offset 16; file type at +11, flags at +18.
        let file_type = fe[16 + 11];
        let ad_type = (u16(&fe, 16 + 18)) & 7;
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
            let (len_raw, lbn_e, adv) = match ad_type {
                0 => {
                    // short_ad: ExtentLength(u32), ExtentPosition(u32)
                    if o + 8 > ad_end {
                        break;
                    }
                    (u32(&fe, o), u32(&fe, o + 4) as u64, 8)
                }
                1 => {
                    // long_ad: ExtentLength(u32), lb_addr (lbn u32 + partref u16), impuse(6)
                    if o + 16 > ad_end {
                        break;
                    }
                    (u32(&fe, o), u32(&fe, o + 4) as u64, 16)
                }
                _ => break,
            };
            o += adv;
            let etype = (len_raw >> 30) as u8;
            let length = (len_raw & 0x3FFF_FFFF) as u64;
            if length == 0 {
                break;
            }
            extents.push((lbn_e, length, etype));
        }

        Ok(Icb {
            file_type,
            size,
            extents,
            inline: None,
        })
    }

    /// Read a file's full content (truncated to its exact size).
    fn read_body(&mut self, icb: &Icb) -> Result<Vec<u8>, FilesystemError> {
        if let Some(inline) = &icb.inline {
            let n = (icb.size as usize).min(inline.len());
            return Ok(inline[..n].to_vec());
        }
        let mut out = Vec::new();
        for &(lbn, length, etype) in &icb.extents {
            if out.len() as u64 > MAX_BYTES {
                break;
            }
            if etype != 0 {
                // Not-recorded extent → sparse zeros.
                out.resize(out.len() + length as usize, 0);
            } else {
                let bytes = self
                    .reader
                    .read_bytes(self.byte_of(lbn), length as usize)
                    .map_err(sector_to_fs_err)?;
                out.extend_from_slice(&bytes);
            }
        }
        if icb.size > 0 && (icb.size as usize) < out.len() {
            out.truncate(icb.size as usize);
        }
        Ok(out)
    }

    /// Parse a directory's File Identifier Descriptors into `(name, is_dir, icb_lbn)`.
    fn parse_dir(&self, data: &[u8]) -> Vec<(String, bool, u64)> {
        let mut out = Vec::new();
        let mut o = 0usize;
        while o + 38 <= data.len() {
            if tag_id(&data[o..]) != TAG_FILE_ID {
                break;
            }
            let characteristics = data[o + 18];
            let l_fi = data[o + 19] as usize;
            let icb_lbn = u32(data, o + 20 + 4) as u64; // long_ad → lb_addr lbn
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
                    out.push((name, is_dir, icb_lbn));
                }
            }
            if total == 0 {
                break;
            }
            o += total;
        }
        out
    }

    fn list_lbn(&mut self, lbn: u64, parent_path: &str) -> Result<Vec<FileEntry>, FilesystemError> {
        let icb = self.read_icb(lbn)?;
        let data = self.read_body(&icb)?;
        let mut entries = Vec::new();
        for (name, is_dir, child_lbn) in self.parse_dir(&data) {
            let path = if parent_path == "/" {
                format!("/{name}")
            } else {
                format!("{parent_path}/{name}")
            };
            let entry = if is_dir {
                FileEntry::new_directory(name, path, child_lbn)
            } else {
                // Read the child ICB for its size / symlink flag.
                match self.read_icb(child_lbn) {
                    Ok(c) => {
                        let mut fe = FileEntry::new_file(name, path, c.size, child_lbn);
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
        Ok(FileEntry::root(self.root_lbn))
    }

    fn list_directory(&mut self, entry: &FileEntry) -> Result<Vec<FileEntry>, FilesystemError> {
        if !entry.is_directory() {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        self.list_lbn(entry.location, &entry.path)
    }

    fn read_file(&mut self, entry: &FileEntry) -> Result<Vec<u8>, FilesystemError> {
        if entry.is_directory() {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        let icb = self.read_icb(entry.location)?;
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
        let all = self.read_file(entry)?;
        let start = (offset as usize).min(all.len());
        let end = start.saturating_add(length).min(all.len());
        Ok(all[start..end].to_vec())
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
        self.volume_id.as_deref()
    }
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
pub(crate) fn detect_udf(reader: &mut dyn SectorReader) -> bool {
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
