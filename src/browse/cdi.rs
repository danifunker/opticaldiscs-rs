//! Philips CD-i (Green Book) filesystem browser.
//!
//! CD-i discs use an ISO 9660-derived structure with several twists that break a
//! stock ISO 9660 reader (verified against real CD-i discs):
//!
//! - The volume descriptor's Standard Identifier at byte 1 is `"CD-I "`, not
//!   `"CD001"`.
//! - Numeric fields are stored **big-endian only** (the ISO both-endian layout's
//!   little-endian half is zeroed), because the target CPU is a 68000.
//! - The volume descriptor carries no embedded root-directory record; the root
//!   is located through the **M-type path table** (pointer at VD offset 148,
//!   big-endian; size at offset 136).
//! - Per-entry POSIX-ish attributes (including the directory bit `0x8000`) live
//!   in a structured System-Use area after the file name.
//!
//! Reference: `docs/GameDiscs_Implementation.md` (Phase G4); Aaru
//! `Aaru.Filesystems/ISO9660` CD-i structs.

use super::entry::{EntryType, FileEntry};
use super::filesystem::{Filesystem, FilesystemError};
use crate::sector_reader::{SectorReader, SECTOR_SIZE};

/// Sector holding the CD-i volume descriptor (same as ISO 9660).
const VD_SECTOR: u64 = 16;

/// CD-i attribute bit marking a directory (System-Use area, big-endian).
const ATTR_DIRECTORY: u16 = 0x8000;

/// Return `true` if `reader` presents a CD-i (Green Book) volume.
pub fn detect_cdi(reader: &mut dyn SectorReader) -> bool {
    match reader.read_sector(VD_SECTOR) {
        Ok(vd) => vd.len() >= 6 && &vd[1..6] == b"CD-I ",
        Err(_) => false,
    }
}

/// Read the CD-i volume identifier (the on-disc volume label), if present.
pub fn read_volume_id(reader: &mut dyn SectorReader) -> Option<String> {
    let vd = reader.read_sector(VD_SECTOR).ok()?;
    if vd.len() < 72 || &vd[1..6] != b"CD-I " {
        return None;
    }
    let s = ascii_field(&vd[40..72]);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// CD-i filesystem browser.
pub struct CdiFilesystem {
    reader: Box<dyn SectorReader>,
    root_lba: u32,
    root_size: u32,
    volume_id: String,
}

impl CdiFilesystem {
    /// Create a browser by reading the CD-i volume descriptor from `reader`.
    ///
    /// # Errors
    ///
    /// Returns [`FilesystemError::Parse`] if the disc is not a CD-i volume or its
    /// path table / root directory cannot be located.
    pub fn new(mut reader: Box<dyn SectorReader>) -> Result<Self, FilesystemError> {
        let vd = reader
            .read_sector(VD_SECTOR)
            .map_err(|e| FilesystemError::Io(io_of(e)))?;
        if vd.len() < 152 || &vd[1..6] != b"CD-I " {
            return Err(FilesystemError::Parse("not a CD-i volume".into()));
        }
        let volume_id = ascii_field(&vd[40..72]);

        // Root directory is the first entry of the M-type path table.
        let m_path_lba = be32(&vd[148..152]);
        let pt = reader
            .read_sector(m_path_lba as u64)
            .map_err(|e| FilesystemError::Io(io_of(e)))?;
        // Path-table entry: id_len(1) ext_attr(1) extent(BE u32) parent(BE u16) name
        if pt.len() < 8 || pt[0] == 0 {
            return Err(FilesystemError::Parse("empty CD-i path table".into()));
        }
        let root_lba = be32(&pt[2..6]);

        // The directory's own size comes from its first ("." self) record.
        let root0 = reader
            .read_sector(root_lba as u64)
            .map_err(|e| FilesystemError::Io(io_of(e)))?;
        let root_size = if root0.first().copied().unwrap_or(0) as usize >= 18 {
            be32(&root0[14..18])
        } else {
            SECTOR_SIZE as u32
        };

        Ok(Self {
            reader,
            root_lba,
            root_size,
            volume_id,
        })
    }

    /// Read a directory extent and parse its CD-i records into entries.
    fn read_dir(
        &mut self,
        lba: u32,
        size: u32,
        base_path: &str,
    ) -> Result<Vec<FileEntry>, FilesystemError> {
        let bytes = self
            .reader
            .read_bytes(lba as u64 * SECTOR_SIZE, size as usize)
            .map_err(|e| FilesystemError::Io(io_of(e)))?;

        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos < bytes.len() {
            let len = bytes[pos] as usize;
            if len == 0 {
                // Records never span a logical sector; skip padding to the next.
                let next = (pos / SECTOR_SIZE as usize + 1) * SECTOR_SIZE as usize;
                if next <= pos || next >= bytes.len() {
                    break;
                }
                pos = next;
                continue;
            }
            if len < 34 || pos + len > bytes.len() {
                break;
            }
            let rec = &bytes[pos..pos + len];
            let id_len = rec[32] as usize;
            if 33 + id_len > rec.len() {
                break;
            }
            let id = &rec[33..33 + id_len];

            // Skip "." (0x00) and ".." (0x01) self/parent records.
            let is_special = id_len == 1 && (id[0] == 0x00 || id[0] == 0x01);
            if !is_special {
                let extent = be32(&rec[6..10]);
                let data_len = be32(&rec[14..18]);
                let attr = cdi_attributes(rec, id_len);
                let is_dir = attr & ATTR_DIRECTORY != 0;
                let name = clean_name(id);
                let path = format!("{}/{}", base_path.trim_end_matches('/'), name);
                out.push(make_entry(name, path, extent, data_len, is_dir));
            }
            pos += len;
        }
        Ok(out)
    }
}

impl Filesystem for CdiFilesystem {
    fn root(&mut self) -> Result<FileEntry, FilesystemError> {
        Ok(make_entry(
            String::new(),
            "/".into(),
            self.root_lba,
            self.root_size,
            true,
        ))
    }

    fn list_directory(&mut self, entry: &FileEntry) -> Result<Vec<FileEntry>, FilesystemError> {
        if entry.entry_type != EntryType::Directory {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        let path = entry.path.clone();
        self.read_dir(entry.location as u32, entry.size as u32, &path)
    }

    fn read_file(&mut self, entry: &FileEntry) -> Result<Vec<u8>, FilesystemError> {
        self.reader
            .read_bytes(entry.location * SECTOR_SIZE, entry.size as usize)
            .map_err(|e| FilesystemError::Io(io_of(e)))
    }

    fn read_file_range(
        &mut self,
        entry: &FileEntry,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, FilesystemError> {
        let clamped = length.min((entry.size.saturating_sub(offset)) as usize);
        self.reader
            .read_bytes(entry.location * SECTOR_SIZE + offset, clamped)
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
        if self.volume_id.is_empty() {
            None
        } else {
            Some(&self.volume_id)
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Read the CD-i attribute word from a directory record's System-Use area.
///
/// The System-Use area follows the file name, padded so it starts on an even
/// boundary (33 + id_len is odd ⟺ id_len even → 1 pad byte). Its structure is
/// `group(u16) owner(u16) attributes(u16) …`, so the attribute word is 4 bytes
/// into the area. All big-endian.
fn cdi_attributes(rec: &[u8], id_len: usize) -> u16 {
    let pad = (id_len + 1) & 1;
    let attr_off = 33 + id_len + pad + 4;
    if attr_off + 2 <= rec.len() {
        be16(&rec[attr_off..attr_off + 2])
    } else {
        0
    }
}

fn make_entry(name: String, path: String, lba: u32, size: u32, is_dir: bool) -> FileEntry {
    FileEntry {
        name,
        path,
        entry_type: if is_dir {
            EntryType::Directory
        } else {
            EntryType::File
        },
        size: size as u64,
        location: lba as u64,
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

/// Strip a trailing `;version` and decode the identifier as ASCII.
fn clean_name(id: &[u8]) -> String {
    let stem = id.split(|&b| b == b';').next().unwrap_or(id);
    ascii_field(stem)
}

fn ascii_field(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn io_of(e: crate::error::OpticaldiscsError) -> std::io::Error {
    match e {
        crate::error::OpticaldiscsError::Io(io) => io,
        other => std::io::Error::other(other.to_string()),
    }
}
