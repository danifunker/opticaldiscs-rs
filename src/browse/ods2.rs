//! VMS ODS-2 / Files-11 On-Disk Structure Level 2 browser — read-only.
//!
//! ODS-2 is the OpenVMS volume format (VAX and Alpha). Layout: a **home block**
//! at LBN 1 names the index file `INDEXF.SYS`, whose per-file **file headers**
//! (one 512-byte block each) describe every file via **retrieval pointers**
//! (extent lists). Directories are ordinary files of directory records that map
//! a name + version to a **File-ID**. The master directory `[000000]` is file 4.
//!
//! All fields are little-endian; the block size is 512 bytes (LBN addressing).
//! Scope: browse + extract. Validated against OpenVMS 5.5-2 (VAX) install media.

use super::entry::{EntryType, FileEntry};
use super::filesystem::{Filesystem, FilesystemError};
use crate::sector_reader::SectorReader;

/// ODS-2 logical block size.
const BLK: u64 = 512;
/// The `[000000]` master file directory is file number 4.
const ROOT_FID: u64 = 4;
/// Home block sits at LBN 1.
const HOME_LBN: u64 = 1;
/// `hm2$t_format` string identifying an ODS-2 volume, at byte 496 of the home block.
const FORMAT_OFF: usize = 496;
/// Guard against a corrupt header claiming an absurd file size / extent list.
const MAX_FILE_BYTES: u64 = 4u64 * 1024 * 1024 * 1024;

// ── Ods2Filesystem ───────────────────────────────────────────────────────────────

/// Read-only VMS ODS-2 browser over a [`SectorReader`].
pub struct Ods2Filesystem {
    reader: Box<dyn SectorReader>,
    /// LBN of the start of the index-file bitmap; file headers follow it.
    ibmaplbn: u64,
    /// Size of the index-file bitmap in blocks.
    ibmapsize: u64,
    volume_id: Option<String>,
}

/// A parsed ODS-2 file header (`fh2$`).
struct FileHeader {
    /// File size in bytes, from the record-attributes area.
    size: u64,
    /// Data extents as `(lbn, block_count)` retrieval pointers.
    extents: Vec<(u64, u64)>,
}

impl Ods2Filesystem {
    /// Open an ODS-2 volume, reading the home block.
    pub fn new(mut reader: Box<dyn SectorReader>) -> Result<Self, FilesystemError> {
        let hb = reader
            .read_bytes(HOME_LBN * BLK, BLK as usize)
            .map_err(sector_to_fs_err)?;
        if hb.len() < BLK as usize || &hb[FORMAT_OFF..FORMAT_OFF + 9] != b"DECFILE11" {
            return Err(FilesystemError::Parse("not an ODS-2 volume".into()));
        }
        let ibmaplbn = u32le(&hb, 24) as u64;
        let ibmapsize = u16le(&hb, 32) as u64;
        if ibmaplbn == 0 || ibmapsize == 0 {
            return Err(FilesystemError::InvalidData(
                "ODS-2 home block has no index bitmap".into(),
            ));
        }
        // hm2$t_volname (12 chars, space-padded) at byte 60.
        let volume_id = {
            let raw = &hb[60..60 + 12];
            let s: String = raw
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as char)
                .collect();
            let s = s.trim().to_string();
            (!s.is_empty() && s.chars().all(|c| c.is_ascii_graphic() || c == ' ')).then_some(s)
        };

        Ok(Self {
            reader,
            ibmaplbn,
            ibmapsize,
            volume_id,
        })
    }

    /// Byte offset of the header block for file number `fid` (index file assumed
    /// unfragmented — true for install media and all discs we target).
    fn header_lbn(&self, fid: u64) -> u64 {
        self.ibmaplbn + self.ibmapsize + (fid - 1)
    }

    fn read_header(&mut self, fid: u64) -> Result<FileHeader, FilesystemError> {
        let h = self
            .reader
            .read_bytes(self.header_lbn(fid) * BLK, BLK as usize)
            .map_err(sector_to_fs_err)?;
        if h.len() < BLK as usize {
            return Err(FilesystemError::InvalidData(format!(
                "short file header for fid {fid}"
            )));
        }
        // Area offsets are in 16-bit words.
        let mpoffset = h[1] as usize * 2;
        let acoffset = h[2] as usize * 2;

        // Record-attributes area at byte 16: fat$l_efblk is a longword stored
        // high-word-first, fat$w_ffbyte follows. size = (efblk-1)*512 + ffbyte.
        let efblk = ((u16le(&h, 24) as u64) << 16) | u16le(&h, 26) as u64;
        let ffbyte = u16le(&h, 28) as u64;
        let size = if efblk > 0 {
            (efblk - 1) * BLK + ffbyte
        } else {
            0
        };

        // Retrieval pointers: [mpoffset, acoffset). The top two bits of the first
        // word of each pointer select the format.
        let mut extents = Vec::new();
        let mut o = mpoffset;
        let end = acoffset.min(h.len());
        while o + 2 <= end {
            let w0 = u16le(&h, o);
            let fmt = (w0 >> 14) & 3;
            let (lbn, count, adv) = match fmt {
                0 => {
                    o += 2;
                    continue;
                }
                1 => {
                    if o + 4 > end {
                        break;
                    }
                    let count = (w0 & 0xFF) as u64 + 1;
                    let lbn = (((w0 >> 8) & 0x3F) as u64) << 16 | u16le(&h, o + 2) as u64;
                    (lbn, count, 4)
                }
                2 => {
                    if o + 6 > end {
                        break;
                    }
                    let count = (w0 & 0x3FFF) as u64 + 1;
                    let lbn = u32le(&h, o + 2) as u64;
                    (lbn, count, 6)
                }
                _ => {
                    if o + 8 > end {
                        break;
                    }
                    let count = (((w0 & 0x3FFF) as u64) << 16 | u16le(&h, o + 2) as u64) + 1;
                    let lbn = u32le(&h, o + 4) as u64;
                    (lbn, count, 8)
                }
            };
            o += adv;
            if lbn == 0 {
                break;
            }
            extents.push((lbn, count));
        }

        Ok(FileHeader { size, extents })
    }

    /// Read every allocated block of a file's extents (no size truncation).
    fn read_extents(&mut self, hdr: &FileHeader) -> Result<Vec<u8>, FilesystemError> {
        let mut out = Vec::new();
        for &(lbn, count) in &hdr.extents {
            if out.len() as u64 > MAX_FILE_BYTES {
                break;
            }
            let bytes = self
                .reader
                .read_bytes(lbn * BLK, (count * BLK) as usize)
                .map_err(sector_to_fs_err)?;
            out.extend_from_slice(&bytes);
        }
        Ok(out)
    }

    /// Read a file's content, truncated to its exact byte size.
    fn read_file_by_fid(&mut self, fid: u64) -> Result<Vec<u8>, FilesystemError> {
        let hdr = self.read_header(fid)?;
        let mut data = self.read_extents(&hdr)?;
        if hdr.size > 0 && (hdr.size as usize) < data.len() {
            data.truncate(hdr.size as usize);
        }
        Ok(data)
    }

    /// Parse a directory file's records into `(display_name, is_dir, fid)`,
    /// skipping the self-reference (`fid == dir_fid`) that the MFD carries.
    fn parse_directory(&self, data: &[u8], dir_fid: u64) -> Vec<(String, bool, u64)> {
        let mut out = Vec::new();
        let mut o = 0usize;
        while o + 2 <= data.len() {
            let size = u16le(data, o) as usize;
            // 0xFFFF marks the rest of the block as unused — jump to the next block.
            if size == 0xFFFF {
                o = (o / BLK as usize + 1) * BLK as usize;
                continue;
            }
            // record occupies size+2 bytes (the size field counts the rest).
            let rec_end = o + size + 2;
            if size == 0 || rec_end > data.len() {
                break;
            }
            let namecount = data[o + 5] as usize;
            let name_start = o + 6;
            if name_start + namecount <= rec_end {
                let name =
                    String::from_utf8_lossy(&data[name_start..name_start + namecount]).into_owned();
                let is_dir = name
                    .rsplit(';')
                    .next_back()
                    .unwrap_or(&name)
                    .to_ascii_uppercase()
                    .ends_with(".DIR");
                // Value area (word-aligned after the name): repeated
                // {version u16, fid_num u16, fid_seq u16, fid_rvn u16}.
                let mut vo = (name_start + namecount + 1) & !1;
                while vo + 8 <= rec_end {
                    let version = u16le(data, vo);
                    let fid_num = u16le(data, vo + 2) as u64;
                    vo += 8;
                    if fid_num == 0 || fid_num == dir_fid {
                        continue; // ignore self / empty
                    }
                    let display = if is_dir {
                        // Show a directory as its bare name (strip ".DIR").
                        strip_type(&name, ".DIR")
                    } else {
                        format!("{};{}", name, version)
                    };
                    out.push((display, is_dir, fid_num));
                }
            }
            o = rec_end;
        }
        out
    }

    fn list_fid(
        &mut self,
        dir_fid: u64,
        parent_path: &str,
    ) -> Result<Vec<FileEntry>, FilesystemError> {
        let hdr = self.read_header(dir_fid)?;
        let data = self.read_extents(&hdr)?;
        let mut entries = Vec::new();
        for (name, is_dir, fid) in self.parse_directory(&data, dir_fid) {
            let path = if parent_path == "/" {
                format!("/{name}")
            } else {
                format!("{parent_path}/{name}")
            };
            let entry = if is_dir {
                FileEntry::new_directory(name, path, fid)
            } else {
                // Read the child header for its size (dir records carry none).
                let size = self.read_header(fid).map(|h| h.size).unwrap_or(0);
                FileEntry::new_file(name, path, size, fid)
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

impl Filesystem for Ods2Filesystem {
    fn root(&mut self) -> Result<FileEntry, FilesystemError> {
        Ok(FileEntry::root(ROOT_FID))
    }

    fn list_directory(&mut self, entry: &FileEntry) -> Result<Vec<FileEntry>, FilesystemError> {
        if !entry.is_directory() {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        self.list_fid(entry.location, &entry.path)
    }

    fn read_file(&mut self, entry: &FileEntry) -> Result<Vec<u8>, FilesystemError> {
        if entry.is_directory() {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }
        self.read_file_by_fid(entry.location)
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
        let all = self.read_file_by_fid(entry.location)?;
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

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Strip a trailing type such as `.DIR` (case-insensitive), ignoring any
/// `;version` suffix. `SYS0.DIR` -> `SYS0`.
fn strip_type(name: &str, ty: &str) -> String {
    let base = name.split(';').next().unwrap_or(name);
    if base
        .to_ascii_uppercase()
        .ends_with(&ty.to_ascii_uppercase())
    {
        base[..base.len() - ty.len()].to_string()
    } else {
        base.to_string()
    }
}

fn sector_to_fs_err(e: crate::error::OpticaldiscsError) -> FilesystemError {
    match e {
        crate::error::OpticaldiscsError::Io(io_err) => FilesystemError::Io(io_err),
        e => FilesystemError::InvalidData(e.to_string()),
    }
}

/// True if the image is an ODS-2 volume (`DECFILE11` home block at LBN 1).
pub(crate) fn detect_ods2(reader: &mut dyn SectorReader) -> bool {
    match reader.read_bytes(HOME_LBN * BLK, BLK as usize) {
        Ok(b) if b.len() >= BLK as usize => &b[FORMAT_OFF..FORMAT_OFF + 9] == b"DECFILE11",
        _ => false,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_type_removes_dir() {
        assert_eq!(strip_type("SYS0.DIR", ".DIR"), "SYS0");
        assert_eq!(strip_type("SYS0.DIR;1", ".DIR"), "SYS0");
        assert_eq!(strip_type("FOO.EXE;2", ".DIR"), "FOO.EXE");
    }

    /// Directory-record parser: an MFD-style record for `SYS0.DIR` -> fid 100,
    /// plus a self-reference (`000000.DIR` -> the dir's own fid) that must be
    /// dropped, and a data file with a version suffix.
    #[test]
    fn parse_directory_records() {
        fn rec(name: &str, version: u16, fid: u16) -> Vec<u8> {
            let nc = name.len();
            let name_end = 6 + nc;
            let val_start = (name_end + 1) & !1;
            let total = val_start + 8; // one value entry
            let size = total - 2; // dir$w_size counts the rest
            let mut r = vec![0u8; total];
            r[0..2].copy_from_slice(&(size as u16).to_le_bytes());
            r[4] = 0; // flags
            r[5] = nc as u8;
            r[6..6 + nc].copy_from_slice(name.as_bytes());
            r[val_start..val_start + 2].copy_from_slice(&version.to_le_bytes());
            r[val_start + 2..val_start + 4].copy_from_slice(&fid.to_le_bytes());
            r
        }
        let dir_fid = 4u64;
        let mut data = Vec::new();
        data.extend(rec("000000.DIR", 1, 4)); // self-reference → dropped
        data.extend(rec("SYS0.DIR", 1, 100));
        data.extend(rec("STARTUP.COM", 3, 250));

        // A minimal Ods2Filesystem just to reach parse_directory (no I/O).
        struct Null;
        impl SectorReader for Null {
            fn read_sector(&mut self, _l: u64) -> crate::error::Result<Vec<u8>> {
                Ok(vec![0u8; 2048])
            }
        }
        let fs = Ods2Filesystem {
            reader: Box::new(Null),
            ibmaplbn: 1,
            ibmapsize: 1,
            volume_id: None,
        };
        let ents = fs.parse_directory(&data, dir_fid);
        assert_eq!(
            ents,
            vec![
                ("SYS0".to_string(), true, 100),
                ("STARTUP.COM;3".to_string(), false, 250),
            ]
        );
    }
}
