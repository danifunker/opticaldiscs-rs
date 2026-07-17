//! ISO 9660 filesystem browser.
//!
//! Provides [`Iso9660Filesystem`], which implements the [`Filesystem`] trait
//! for ISO 9660 disc images regardless of container format (plain ISO,
//! BIN/CUE, or CHD).
//!
//! Create one via [`crate::browse::open_disc_filesystem`] or directly with
//! [`Iso9660Filesystem::new`] when you already have a `Box<dyn SectorReader>`.

use super::entry::{EntryType, FileEntry, FileTimestamps};
use super::filesystem::{Filesystem, FilesystemError};
use crate::iso9660::{Iso9660DateTime, PrimaryVolumeDescriptor};
use crate::sector_reader::{SectorReader, SECTOR_SIZE};

// ── Iso9660Filesystem ─────────────────────────────────────────────────────────

/// ISO 9660 filesystem browser.
///
/// Reads directory records and file data from a [`SectorReader`]-backed disc
/// image. Any container format (plain ISO, BIN/CUE, CHD) is supported as long
/// as the sector reader provides 2048-byte cooked sectors.
pub struct Iso9660Filesystem {
    reader: Box<dyn SectorReader>,
    root_location: u32,
    root_size: u32,
    volume_id: String,
    /// When set, the browsed tree is a Joliet tree, so directory-record
    /// identifiers are decoded as UTF-16BE.
    joliet: bool,
    /// When set, the volume is High Sierra Format, whose directory records place
    /// the file-flags byte at offset 24 (ISO 9660 uses 25).
    high_sierra: bool,
}

impl Iso9660Filesystem {
    /// Create an `Iso9660Filesystem` by reading the PVD from `reader`.
    ///
    /// Reads sector 16, validates the ISO 9660 header, and extracts the root
    /// directory location and size. Then selects which directory tree to browse:
    /// the primary tree is preferred when it carries **Rock Ridge** (so POSIX
    /// metadata, long names, and symlinks are available); otherwise a **Joliet**
    /// tree is used when present (Unicode names). A plain ISO 9660 disc falls
    /// back to the primary tree with 8.3-style names.
    ///
    /// # Errors
    ///
    /// Returns [`FilesystemError::Parse`] if the PVD is missing or malformed.
    pub fn new(mut reader: Box<dyn SectorReader>) -> Result<Self, FilesystemError> {
        let pvd = PrimaryVolumeDescriptor::read_from(&mut *reader)
            .map_err(|e| FilesystemError::Parse(e.to_string()))?;
        let high_sierra = pvd.high_sierra;

        // Prefer Rock Ridge on the primary tree; only fall back to Joliet when
        // the primary tree has no Rock Ridge extensions. (High Sierra predates
        // both, so neither probe will match — it browses its primary tree.)
        let rock_ridge = detect_rock_ridge_root(
            reader.as_mut(),
            pvd.root_directory_lba,
            pvd.root_directory_size,
            high_sierra,
        );
        let joliet_svd = if rock_ridge {
            None
        } else {
            crate::iso9660::JolietVolumeDescriptor::find(reader.as_mut())
                .ok()
                .flatten()
        };

        let (root_location, root_size, volume_id, joliet) = match joliet_svd {
            Some(j) => (
                j.root_directory_lba,
                j.root_directory_size,
                j.volume_id,
                true,
            ),
            None => (
                pvd.root_directory_lba,
                pvd.root_directory_size,
                pvd.volume_id,
                false,
            ),
        };

        Ok(Self {
            reader,
            root_location,
            root_size,
            volume_id,
            joliet,
            high_sierra,
        })
    }

    /// Read directory data: `size` bytes starting at `location` (LBA).
    fn read_directory_data(
        &mut self,
        location: u32,
        size: u32,
    ) -> Result<Vec<u8>, FilesystemError> {
        if size == 0 {
            return Ok(Vec::new());
        }
        let byte_offset = location as u64 * SECTOR_SIZE;
        self.reader
            .read_bytes(byte_offset, size as usize)
            .map_err(sector_to_fs_err)
    }

    /// Parse all directory records from raw directory extent bytes.
    ///
    /// Skips the `.` (self) and `..` (parent) entries.
    /// Sets `entry.size` to `data_length` for directories so that
    /// [`Self::list_directory`] can read them without re-parsing the extent
    /// size from the disc.
    fn parse_directory(&mut self, data: &[u8], parent_path: &str) -> Vec<FileEntry> {
        let mut entries = Vec::new();
        let mut offset = 0usize;

        while offset < data.len() {
            let record_length = data[offset] as usize;

            // A record length of 0 signals the end of the current sector.
            // Advance to the next 2048-byte boundary and continue.
            if record_length == 0 {
                let next_sector = (offset / SECTOR_SIZE as usize + 1) * SECTOR_SIZE as usize;
                if next_sector >= data.len() {
                    break;
                }
                offset = next_sector;
                continue;
            }

            if offset + record_length > data.len() {
                break;
            }

            if let Some(rec) =
                DirectoryRecord::parse(&data[offset..offset + record_length], self.high_sierra)
            {
                if !rec.is_self() && !rec.is_parent() {
                    let mut name = rec.clean_name(self.joliet);
                    // Rock Ridge overrides (name, POSIX, timestamps, symlink) are
                    // applied from the record's System Use area, if present.
                    let rr = super::rockridge::parse(&rec.system_use, self.reader.as_mut());
                    if let Some(alt) = &rr.name {
                        name = alt.clone();
                    }
                    let path = if parent_path == "/" {
                        format!("/{}", name)
                    } else {
                        format!("{}/{}", parent_path, name)
                    };

                    let mut entry = if rec.is_directory() {
                        FileEntry::new_directory(name, path, rec.extent_location as u64)
                    } else {
                        FileEntry::new_file(
                            name,
                            path,
                            rec.data_length as u64,
                            rec.extent_location as u64,
                        )
                    };

                    // Store data_length in size for directories so
                    // list_directory knows how many bytes to read.
                    if entry.is_directory() {
                        entry.size = rec.data_length as u64;
                    }

                    if let Some(recorded) = rec.recorded {
                        entry.timestamps = Some(FileTimestamps::Iso9660 {
                            recorded,
                            created: rr.created,
                            modified: rr.modified,
                            accessed: rr.accessed,
                        });
                    }
                    if let Some(posix) = rr.posix {
                        entry.posix = Some(posix);
                    }
                    if let Some(target) = rr.symlink_target {
                        entry.symlink_target = Some(target);
                    }

                    entries.push(entry);
                }
            }

            offset += record_length;
        }

        // Sort: directories first, then files; each group sorted by lowercase name.
        entries.sort_by(|a, b| match (a.entry_type, b.entry_type) {
            (EntryType::Directory, EntryType::File) => std::cmp::Ordering::Less,
            (EntryType::File, EntryType::Directory) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });

        entries
    }
}

impl Filesystem for Iso9660Filesystem {
    fn root(&mut self) -> Result<FileEntry, FilesystemError> {
        let mut root = FileEntry::root(self.root_location as u64);
        root.size = self.root_size as u64;
        Ok(root)
    }

    fn list_directory(&mut self, entry: &FileEntry) -> Result<Vec<FileEntry>, FilesystemError> {
        if !entry.is_directory() {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }

        let (location, size) = if entry.path == "/" {
            // For root, use the sizes stored during construction.
            (self.root_location, self.root_size)
        } else {
            // For non-root directories, size was stored in entry.size by
            // parse_directory when the entry was first listed.
            (entry.location as u32, entry.size as u32)
        };

        let data = self.read_directory_data(location, size)?;
        Ok(self.parse_directory(&data, &entry.path))
    }

    fn read_file(&mut self, entry: &FileEntry) -> Result<Vec<u8>, FilesystemError> {
        if entry.is_directory() {
            return Err(FilesystemError::NotADirectory(format!(
                "{} is a directory",
                entry.path
            )));
        }

        let byte_offset = entry.location * SECTOR_SIZE;
        self.reader
            .read_bytes(byte_offset, entry.size as usize)
            .map_err(sector_to_fs_err)
    }

    fn read_file_range(
        &mut self,
        entry: &FileEntry,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, FilesystemError> {
        if entry.is_directory() {
            return Err(FilesystemError::NotADirectory(format!(
                "{} is a directory",
                entry.path
            )));
        }

        let actual_length = length.min(entry.size.saturating_sub(offset) as usize);
        if actual_length == 0 {
            return Ok(Vec::new());
        }

        let file_byte_start = entry.location * SECTOR_SIZE;
        self.reader
            .read_bytes(file_byte_start + offset, actual_length)
            .map_err(sector_to_fs_err)
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
        // ISO 9660 allocates file data in whole 2048-byte logical blocks; the
        // browser addresses the volume entirely at this cooked-sector size.
        Some(SECTOR_SIZE)
    }

    fn volume_name(&self) -> Option<&str> {
        if self.volume_id.is_empty() {
            None
        } else {
            Some(&self.volume_id)
        }
    }
}

// ── DirectoryRecord ───────────────────────────────────────────────────────────

/// A parsed ISO 9660 directory record (ECMA-119 §9.1).
#[derive(Debug, Clone)]
struct DirectoryRecord {
    /// Location of the extent (LBA).
    extent_location: u32,
    /// Size of the extent in bytes (file size or directory size).
    data_length: u32,
    /// ISO 9660 file flags byte (bit 1 = directory).
    file_flags: u8,
    /// Raw file identifier bytes (may contain version suffix such as `;1`, and,
    /// for Joliet, UTF-16BE code units).
    file_identifier: Vec<u8>,
    /// Recording date/time (ECMA-119 §9.1.5, 7-byte binary form). `None` if the
    /// field is unset.
    recorded: Option<Iso9660DateTime>,
    /// Rock Ridge / SUSP "System Use" bytes that follow the identifier (plus its
    /// padding byte), up to the end of the record. Empty when the disc carries
    /// no System Use area.
    system_use: Vec<u8>,
}

impl DirectoryRecord {
    /// Parse a directory record from a raw byte slice.
    ///
    /// Returns `None` if the slice is shorter than 33 bytes or the record
    /// length field is zero. When `high_sierra` is set, the file-flags byte is
    /// read from offset 24 and the recording date/time is the 6-byte High Sierra
    /// form (no GMT-offset byte); ISO 9660 uses offset 25 and a 7-byte date.
    fn parse(data: &[u8], high_sierra: bool) -> Option<Self> {
        if data.len() < 33 || data[0] == 0 {
            return None;
        }

        let extent_location = u32::from_le_bytes(data[2..6].try_into().ok()?);
        let data_length = u32::from_le_bytes(data[10..14].try_into().ok()?);
        let (recorded, file_flags) = if high_sierra {
            // High Sierra: 6-byte date at 18..24, flags at 24. Pad to the 7-byte
            // form (GMT offset 0) so the shared date parser can read it.
            let mut dt = [0u8; 7];
            dt[..6].copy_from_slice(&data[18..24]);
            (Iso9660DateTime::parse(&dt), data[24])
        } else {
            (Iso9660DateTime::parse(&data[18..25]), data[25])
        };
        let id_len = data[32] as usize;

        if data.len() < 33 + id_len {
            return None;
        }

        let file_identifier = data[33..33 + id_len].to_vec();

        // The System Use area follows the identifier, after a padding byte that
        // is present only when `id_len` is even (so the identifier + optional pad
        // ends on an even boundary). Everything from there to `record_length`
        // (the whole slice) is available for Rock Ridge / SUSP.
        let su_start = 33 + id_len + (1 - id_len % 2);
        let system_use = data.get(su_start..).map(|s| s.to_vec()).unwrap_or_default();

        Some(Self {
            extent_location,
            data_length,
            file_flags,
            file_identifier,
            recorded,
            system_use,
        })
    }

    fn is_directory(&self) -> bool {
        (self.file_flags & 0x02) != 0
    }

    /// True for the `.` (current directory) entry (identifier `\x00`).
    fn is_self(&self) -> bool {
        self.file_identifier.is_empty() || self.file_identifier == [0x00]
    }

    /// True for the `..` (parent directory) entry (identifier `\x01`).
    fn is_parent(&self) -> bool {
        self.file_identifier == [0x01]
    }

    /// Return the display name. When `joliet` is set the identifier bytes are
    /// decoded as UTF-16BE (UCS-2); otherwise as (lossy) UTF-8. The `;1` version
    /// suffix is stripped from files and trailing dots from directory names.
    fn clean_name(&self, joliet: bool) -> String {
        if self.is_self() {
            return ".".to_string();
        }
        if self.is_parent() {
            return "..".to_string();
        }

        let decoded = if joliet {
            crate::iso9660::decode_utf16be(&self.file_identifier)
        } else {
            String::from_utf8_lossy(&self.file_identifier).into_owned()
        };

        // Strip version suffix (";1", ";2", …).
        let name = match decoded.rfind(';') {
            Some(idx) => &decoded[..idx],
            None => &decoded[..],
        };

        // Strip trailing dot — ISO 9660 directory identifiers end with `.`.
        if self.is_directory() {
            name.trim_end_matches('.').to_string()
        } else {
            name.to_string()
        }
    }
}

/// Detect Rock Ridge by inspecting the root directory's `.` (self) record: its
/// System Use area carries the SUSP `SP` indicator and Rock Ridge entries when
/// the disc uses the extension. Returns `false` on any read/parse failure.
fn detect_rock_ridge_root(
    reader: &mut dyn SectorReader,
    root_lba: u32,
    root_size: u32,
    high_sierra: bool,
) -> bool {
    if root_size == 0 {
        return false;
    }
    let want = (root_size as usize).min(SECTOR_SIZE as usize);
    let data = match reader.read_bytes(root_lba as u64 * SECTOR_SIZE, want) {
        Ok(d) => d,
        Err(_) => return false,
    };
    if data.is_empty() {
        return false;
    }
    let rec_len = data[0] as usize;
    if rec_len == 0 || rec_len > data.len() {
        return false;
    }
    match DirectoryRecord::parse(&data[..rec_len], high_sierra) {
        Some(rec) => super::rockridge::detect(&rec.system_use),
        None => false,
    }
}

// ── Error conversion ──────────────────────────────────────────────────────────

fn sector_to_fs_err(e: crate::error::OpticaldiscsError) -> FilesystemError {
    match e {
        crate::error::OpticaldiscsError::Io(io_err) => FilesystemError::Io(io_err),
        e => FilesystemError::InvalidData(e.to_string()),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iso9660::build_test_pvd_sector;
    use crate::sector_reader::SECTOR_SIZE;
    use std::io::{Cursor, Read, Seek, SeekFrom};

    // ── Minimal SectorReader over a Cursor ─────────────────────────────────

    struct CursorReader(Cursor<Vec<u8>>);

    impl SectorReader for CursorReader {
        fn read_sector(&mut self, lba: u64) -> crate::error::Result<Vec<u8>> {
            self.0
                .seek(SeekFrom::Start(lba * SECTOR_SIZE))
                .map_err(crate::error::OpticaldiscsError::Io)?;
            let mut buf = vec![0u8; SECTOR_SIZE as usize];
            self.0
                .read_exact(&mut buf)
                .map_err(crate::error::OpticaldiscsError::Io)?;
            Ok(buf)
        }
    }

    // ── Test image builder ─────────────────────────────────────────────────

    /// Build a raw ISO 9660 image with zero or more file entries in the root.
    ///
    /// `files` is a list of `(name_with_version, content)` pairs.
    /// Each file is placed in consecutive sectors starting at sector 19.
    /// The root directory is at sector 18 and includes `.`, `..`, and the
    /// supplied file entries.
    fn build_iso_image(label: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        const SECTOR: usize = SECTOR_SIZE as usize;
        let total_sectors = 19 + files.len();
        let mut img = vec![0u8; total_sectors * SECTOR];

        // PVD at sector 16 — root dir at sector 18, size = one sector.
        let pvd = build_test_pvd_sector(label, 18, SECTOR as u32);
        img[16 * SECTOR..17 * SECTOR].copy_from_slice(&pvd);

        // Volume Descriptor Set Terminator at sector 17.
        img[17 * SECTOR] = 0xFF;
        img[17 * SECTOR + 1..17 * SECTOR + 6].copy_from_slice(b"CD001");
        img[17 * SECTOR + 6] = 1;

        // Build root directory at sector 18.
        let mut dir = vec![0u8; SECTOR];
        let mut dir_off = 0usize;

        // Helper: append a directory record.
        let append_rec = |buf: &mut Vec<u8>,
                          off: &mut usize,
                          extent_lba: u32,
                          data_len: u32,
                          flags: u8,
                          id: &[u8]| {
            let id_len = id.len();
            let rec_len = 33 + id_len;
            let rec_len = if rec_len % 2 == 0 {
                rec_len
            } else {
                rec_len + 1
            };

            buf[*off] = rec_len as u8;
            buf[*off + 2..*off + 6].copy_from_slice(&extent_lba.to_le_bytes());
            buf[*off + 6..*off + 10].copy_from_slice(&extent_lba.to_be_bytes());
            buf[*off + 10..*off + 14].copy_from_slice(&data_len.to_le_bytes());
            buf[*off + 14..*off + 18].copy_from_slice(&data_len.to_be_bytes());
            buf[*off + 25] = flags;
            buf[*off + 32] = id_len as u8;
            buf[*off + 33..*off + 33 + id_len].copy_from_slice(id);

            *off += rec_len;
        };

        // `.` entry (self-reference, sector 18, size = one directory sector).
        append_rec(&mut dir, &mut dir_off, 18, SECTOR as u32, 0x02, b"\x00");
        // `..` entry (parent = root for root dir).
        append_rec(&mut dir, &mut dir_off, 18, SECTOR as u32, 0x02, b"\x01");

        // File entries.
        for (i, (name, content)) in files.iter().enumerate() {
            let file_lba = 19 + i as u32;
            append_rec(
                &mut dir,
                &mut dir_off,
                file_lba,
                content.len() as u32,
                0x00, // file
                name.as_bytes(),
            );
            // Write file content at its sector.
            let sector_off = (19 + i) * SECTOR;
            let copy_len = content.len().min(SECTOR);
            img[sector_off..sector_off + copy_len].copy_from_slice(&content[..copy_len]);
        }

        img[18 * SECTOR..19 * SECTOR].copy_from_slice(&dir);
        img
    }

    // ── Unit tests ─────────────────────────────────────────────────────────

    #[test]
    fn empty_root_directory_returns_no_entries() {
        let img = build_iso_image("EMPTY_ROOT", &[]);
        let reader = Box::new(CursorReader(Cursor::new(img)));
        let mut fs = Iso9660Filesystem::new(reader).unwrap();
        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn root_directory_lists_files() {
        let content = b"hello world";
        let img = build_iso_image("BROWSE_TEST", &[("README.TXT;1", content)]);
        let reader = Box::new(CursorReader(Cursor::new(img)));
        let mut fs = Iso9660Filesystem::new(reader).unwrap();

        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "README.TXT"); // version suffix stripped
        assert!(entries[0].is_file());
        assert_eq!(entries[0].size, content.len() as u64);
    }

    #[test]
    fn read_file_returns_correct_content() {
        let content = b"ISO 9660 file content";
        let img = build_iso_image("READ_TEST", &[("DATA.BIN;1", content)]);
        let reader = Box::new(CursorReader(Cursor::new(img)));
        let mut fs = Iso9660Filesystem::new(reader).unwrap();

        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        let file = entries.iter().find(|e| e.name == "DATA.BIN").unwrap();

        let data = fs.read_file(file).unwrap();
        assert_eq!(data, content);
    }

    #[test]
    fn read_file_range_returns_slice() {
        let content = b"abcdefghij";
        let img = build_iso_image("RANGE_TEST", &[("RANGE.TXT;1", content)]);
        let reader = Box::new(CursorReader(Cursor::new(img)));
        let mut fs = Iso9660Filesystem::new(reader).unwrap();

        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        let file = entries.iter().find(|e| e.name == "RANGE.TXT").unwrap();

        // Read bytes 3..7 = "defg"
        let slice = fs.read_file_range(file, 3, 4).unwrap();
        assert_eq!(slice, b"defg");
    }

    #[test]
    fn volume_name_returned() {
        let img = build_iso_image("MY_DISC", &[]);
        let reader = Box::new(CursorReader(Cursor::new(img)));
        let fs = Iso9660Filesystem::new(reader).unwrap();
        assert_eq!(fs.volume_name(), Some("MY_DISC"));
    }

    #[test]
    fn allocation_unit_is_logical_block() {
        let img = build_iso_image("ALLOC_TEST", &[]);
        let reader = Box::new(CursorReader(Cursor::new(img)));
        let fs = Iso9660Filesystem::new(reader).unwrap();
        assert_eq!(fs.allocation_unit(), Some(2048));
    }

    #[test]
    fn list_directory_on_file_returns_error() {
        let content = b"data";
        let img = build_iso_image("ERR_TEST", &[("FILE.TXT;1", content)]);
        let reader = Box::new(CursorReader(Cursor::new(img)));
        let mut fs = Iso9660Filesystem::new(reader).unwrap();

        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        let file = entries.iter().find(|e| e.is_file()).unwrap();
        assert!(fs.list_directory(file).is_err());
    }

    #[test]
    fn read_resource_fork_returns_none() {
        let content = b"data";
        let img = build_iso_image("RSRC_TEST", &[("FILE.TXT;1", content)]);
        let reader = Box::new(CursorReader(Cursor::new(img)));
        let mut fs = Iso9660Filesystem::new(reader).unwrap();

        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        let file = entries.iter().find(|e| e.is_file()).unwrap();

        assert!(fs.read_resource_fork(file).unwrap().is_none());
        assert!(fs.read_resource_fork_range(file, 0, 4).unwrap().is_none());
        assert!(file.resource_fork_size.is_none());
        assert!(file.type_code.is_none());
        assert!(file.creator_code.is_none());
    }

    #[test]
    fn read_file_on_directory_returns_error() {
        let img = build_iso_image("ERR_TEST2", &[]);
        let reader = Box::new(CursorReader(Cursor::new(img)));
        let mut fs = Iso9660Filesystem::new(reader).unwrap();

        let root = fs.root().unwrap();
        assert!(fs.read_file(&root).is_err());
    }

    #[test]
    fn multiple_files_sorted_alphabetically() {
        let img = build_iso_image(
            "SORT_TEST",
            &[
                ("ZEBRA.TXT;1", b"z"),
                ("APPLE.TXT;1", b"a"),
                ("MANGO.TXT;1", b"m"),
            ],
        );
        let reader = Box::new(CursorReader(Cursor::new(img)));
        let mut fs = Iso9660Filesystem::new(reader).unwrap();

        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["APPLE.TXT", "MANGO.TXT", "ZEBRA.TXT"]);
    }

    // ── DirectoryRecord unit tests ─────────────────────────────────────────

    #[test]
    fn directory_record_parse_file() {
        let name = b"TEST.TXT;1";
        let id_len = name.len();
        let rec_len = 33 + id_len;
        let mut data = vec![0u8; rec_len];
        data[0] = rec_len as u8;
        data[2..6].copy_from_slice(&42u32.to_le_bytes()); // extent LBA
        data[10..14].copy_from_slice(&1024u32.to_le_bytes()); // data length
        data[25] = 0; // file
        data[32] = id_len as u8;
        data[33..33 + id_len].copy_from_slice(name);

        let rec = DirectoryRecord::parse(&data, false).unwrap();
        assert_eq!(rec.extent_location, 42);
        assert_eq!(rec.data_length, 1024);
        assert!(!rec.is_directory());
        assert!(!rec.is_self());
        assert!(!rec.is_parent());
        assert_eq!(rec.clean_name(false), "TEST.TXT");
    }

    #[test]
    fn directory_record_parse_self_entry() {
        let mut data = vec![0u8; 34];
        data[0] = 34;
        data[25] = 0x02;
        data[32] = 1;
        data[33] = 0x00; // self identifier
        let rec = DirectoryRecord::parse(&data, false).unwrap();
        assert!(rec.is_self());
        assert!(rec.is_directory());
    }

    #[test]
    fn directory_record_parse_parent_entry() {
        let mut data = vec![0u8; 34];
        data[0] = 34;
        data[25] = 0x02;
        data[32] = 1;
        data[33] = 0x01; // parent identifier
        let rec = DirectoryRecord::parse(&data, false).unwrap();
        assert!(rec.is_parent());
    }

    #[test]
    fn directory_record_too_short_returns_none() {
        assert!(DirectoryRecord::parse(&[0u8; 32], false).is_none());
    }

    #[test]
    fn clean_name_strips_version_suffix() {
        let rec = DirectoryRecord {
            extent_location: 0,
            data_length: 0,
            file_flags: 0,
            file_identifier: b"ARCHIVE.TAR;1".to_vec(),
            recorded: None,
            system_use: Vec::new(),
        };
        assert_eq!(rec.clean_name(false), "ARCHIVE.TAR");
    }

    #[test]
    fn clean_name_strips_directory_trailing_dot() {
        let rec = DirectoryRecord {
            extent_location: 0,
            data_length: 0,
            file_flags: 0x02,
            file_identifier: b"SYSTEM.".to_vec(),
            recorded: None,
            system_use: Vec::new(),
        };
        assert_eq!(rec.clean_name(false), "SYSTEM");
    }

    #[test]
    fn clean_name_decodes_joliet_utf16be() {
        // "Café" in UTF-16BE, with a ";1" version suffix (ASCII, still UTF-16BE).
        let mut id: Vec<u8> = Vec::new();
        for u in "Café;1".encode_utf16() {
            id.extend_from_slice(&u.to_be_bytes());
        }
        let rec = DirectoryRecord {
            extent_location: 0,
            data_length: 0,
            file_flags: 0,
            file_identifier: id,
            recorded: None,
            system_use: Vec::new(),
        };
        assert_eq!(rec.clean_name(true), "Café");
    }

    // ── End-to-end: recording dates, Rock Ridge, Joliet ─────────────────────

    use crate::browse::entry::FileTimestamps;

    /// Build a SUSP entry: signature(2) + length(1) + version(1) + data.
    fn susp(sig: &[u8; 2], data: &[u8]) -> Vec<u8> {
        let mut v = vec![sig[0], sig[1], (4 + data.len()) as u8, 1];
        v.extend_from_slice(data);
        v
    }

    /// Encode an ISO 9660 "both-endian" u32.
    fn both(n: u32) -> Vec<u8> {
        let mut v = n.to_le_bytes().to_vec();
        v.extend_from_slice(&n.to_be_bytes());
        v
    }

    /// Build a directory record with an optional recording date and System Use.
    fn dir_record(
        extent: u32,
        size: u32,
        flags: u8,
        id: &[u8],
        recorded: Option<[u8; 7]>,
        system_use: &[u8],
    ) -> Vec<u8> {
        let id_len = id.len();
        let su_start = 33 + id_len + (1 - id_len % 2);
        let mut rec_len = su_start + system_use.len();
        if rec_len % 2 != 0 {
            rec_len += 1; // directory records are even-length
        }
        let mut r = vec![0u8; rec_len];
        r[0] = rec_len as u8;
        r[2..6].copy_from_slice(&extent.to_le_bytes());
        r[6..10].copy_from_slice(&extent.to_be_bytes());
        r[10..14].copy_from_slice(&size.to_le_bytes());
        r[14..18].copy_from_slice(&size.to_be_bytes());
        if let Some(d) = recorded {
            r[18..25].copy_from_slice(&d);
        }
        r[25] = flags;
        r[32] = id_len as u8;
        r[33..33 + id_len].copy_from_slice(id);
        r[su_start..su_start + system_use.len()].copy_from_slice(system_use);
        r
    }

    #[test]
    fn rock_ridge_end_to_end() {
        const SEC: usize = SECTOR_SIZE as usize;
        let mut img = vec![0u8; 21 * SEC];

        // PVD (sector 16): primary root at sector 18.
        img[16 * SEC..17 * SEC].copy_from_slice(&build_test_pvd_sector("RR_DISC", 18, SEC as u32));
        // Terminator (sector 17).
        img[17 * SEC] = 0xFF;
        img[17 * SEC + 1..17 * SEC + 6].copy_from_slice(b"CD001");

        // Rock Ridge System Use for the file: PX + TF + NM.
        let mut px = Vec::new();
        px.extend(both(0o100_644)); // mode
        px.extend(both(1)); // nlink
        px.extend(both(501)); // uid
        px.extend(both(20)); // gid
        let mut tf = vec![0b0000_0010u8]; // modify only, short form
        tf.extend_from_slice(&[98, 6, 1, 8, 30, 0, 0]);
        let mut nm = vec![0u8];
        nm.extend_from_slice(b"real name.txt");
        let mut su = Vec::new();
        su.extend(susp(b"PX", &px));
        su.extend(susp(b"TF", &tf));
        su.extend(susp(b"NM", &nm));

        // Root directory (sector 18): `.` (with SP), `..`, and the file record.
        let mut dir = Vec::new();
        dir.extend(dir_record(
            18,
            SEC as u32,
            0x02,
            &[0x00],
            None,
            &susp(b"SP", &[0xBE, 0xEF, 0]),
        ));
        dir.extend(dir_record(18, SEC as u32, 0x02, &[0x01], None, &[]));
        dir.extend(dir_record(
            20,
            5,
            0x00,
            b"FILE.TXT;1",
            Some([97, 3, 18, 16, 45, 47, 0]),
            &su,
        ));
        img[18 * SEC..18 * SEC + dir.len()].copy_from_slice(&dir);
        // File content (sector 20).
        img[20 * SEC..20 * SEC + 5].copy_from_slice(b"hello");

        let mut fs = Iso9660Filesystem::new(Box::new(CursorReader(Cursor::new(img)))).unwrap();
        assert!(!fs.joliet, "Rock Ridge disc should browse the primary tree");
        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        assert_eq!(entries.len(), 1);
        let f = &entries[0];
        // NM overrode the 8.3 identifier.
        assert_eq!(f.name, "real name.txt");
        // PX populated POSIX metadata.
        let px = f.posix.expect("posix present");
        assert_eq!(px.uid, 501);
        assert_eq!(px.gid, 20);
        assert_eq!(px.permission_bits(), 0o644);
        // Recording date + Rock Ridge TF modify time.
        match f.timestamps {
            Some(FileTimestamps::Iso9660 {
                recorded, modified, ..
            }) => {
                assert_eq!(recorded.year(), 1997);
                assert_eq!(modified.expect("TF modify").year(), 1998);
            }
            _ => panic!("expected ISO 9660 timestamps"),
        }
    }

    #[test]
    fn joliet_end_to_end_prefers_unicode_names() {
        const SEC: usize = SECTOR_SIZE as usize;
        let mut img = vec![0u8; 21 * SEC];

        // PVD (16): primary root at 18. Joliet SVD (17): joliet root at 19.
        img[16 * SEC..17 * SEC].copy_from_slice(&build_test_pvd_sector("PRIMARY", 18, SEC as u32));
        let mut svd = build_test_pvd_sector("PRIMARY", 19, SEC as u32);
        svd[0] = 0x02; // Supplementary
        svd[88..91].copy_from_slice(b"%/E"); // Joliet escape
        img[17 * SEC..18 * SEC].copy_from_slice(&svd);

        // Primary root (18): 8.3 name.
        let mut pdir = Vec::new();
        pdir.extend(dir_record(18, SEC as u32, 0x02, &[0x00], None, &[]));
        pdir.extend(dir_record(18, SEC as u32, 0x02, &[0x01], None, &[]));
        pdir.extend(dir_record(20, 5, 0x00, b"FILE.TXT;1", None, &[]));
        img[18 * SEC..18 * SEC + pdir.len()].copy_from_slice(&pdir);

        // Joliet root (19): UTF-16BE Unicode name.
        let uni: Vec<u8> = "Café.txt;1"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        let mut jdir = Vec::new();
        jdir.extend(dir_record(19, SEC as u32, 0x02, &[0x00], None, &[]));
        jdir.extend(dir_record(19, SEC as u32, 0x02, &[0x01], None, &[]));
        jdir.extend(dir_record(20, 5, 0x00, &uni, None, &[]));
        img[19 * SEC..19 * SEC + jdir.len()].copy_from_slice(&jdir);
        img[20 * SEC..20 * SEC + 5].copy_from_slice(b"hello");

        let mut fs = Iso9660Filesystem::new(Box::new(CursorReader(Cursor::new(img)))).unwrap();
        assert!(fs.joliet, "should select the Joliet tree");
        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Café.txt");
    }

    // ── High Sierra Format ──────────────────────────────────────────────────

    /// Build a High Sierra volume descriptor sector: `CDROM` id at byte 9,
    /// descriptor type at byte 8, volume id at 48, root directory record at 180.
    fn build_hsf_vd_sector(label: &str, root_lba: u32, root_size: u32) -> Vec<u8> {
        let mut s = vec![0u8; SECTOR_SIZE as usize];
        s[8] = 1; // Standard File Structure Volume Descriptor
        s[9..14].copy_from_slice(b"CDROM");
        s[14] = 1; // version
        let lab = label.as_bytes();
        s[48..48 + lab.len()].copy_from_slice(lab);
        s[136..138].copy_from_slice(&2048u16.to_le_bytes()); // logical block size
                                                             // Root directory record at offset 180 (flags at HSF offset 24).
        let r = &mut s[180..214];
        r[0] = 34;
        r[2..6].copy_from_slice(&root_lba.to_le_bytes());
        r[6..10].copy_from_slice(&root_lba.to_be_bytes());
        r[10..14].copy_from_slice(&root_size.to_le_bytes());
        r[14..18].copy_from_slice(&root_size.to_be_bytes());
        r[24] = 0x02; // directory flag
        r[32] = 1;
        r[33] = 0x00;
        s
    }

    /// Build a High Sierra directory record (file flags at offset 24, no
    /// System Use area).
    fn hsf_dir_record(extent: u32, size: u32, flags: u8, id: &[u8]) -> Vec<u8> {
        let id_len = id.len();
        let mut rec_len = 33 + id_len + (1 - id_len % 2);
        if rec_len % 2 != 0 {
            rec_len += 1;
        }
        let mut r = vec![0u8; rec_len];
        r[0] = rec_len as u8;
        r[2..6].copy_from_slice(&extent.to_le_bytes());
        r[6..10].copy_from_slice(&extent.to_be_bytes());
        r[10..14].copy_from_slice(&size.to_le_bytes());
        r[14..18].copy_from_slice(&size.to_be_bytes());
        r[24] = flags; // High Sierra flags offset
        r[32] = id_len as u8;
        r[33..33 + id_len].copy_from_slice(id);
        r
    }

    #[test]
    fn high_sierra_end_to_end() {
        const SEC: usize = SECTOR_SIZE as usize;
        let mut img = vec![0u8; 21 * SEC];

        // HSF volume descriptor at sector 16, root directory at sector 18.
        img[16 * SEC..17 * SEC].copy_from_slice(&build_hsf_vd_sector("HSFDISC", 18, SEC as u32));

        // Root directory (18): '.', '..', a subdirectory, and a file. The
        // directory-vs-file distinction rides on the flags byte at offset 24 —
        // if the parser used ISO 9660's offset 25 these would misclassify.
        let mut dir = Vec::new();
        dir.extend(hsf_dir_record(18, SEC as u32, 0x02, &[0x00]));
        dir.extend(hsf_dir_record(18, SEC as u32, 0x02, &[0x01]));
        dir.extend(hsf_dir_record(19, SEC as u32, 0x02, b"SUBDIR"));
        dir.extend(hsf_dir_record(20, 5, 0x00, b"FILE.TXT;1"));
        img[18 * SEC..18 * SEC + dir.len()].copy_from_slice(&dir);
        // File content at sector 20.
        img[20 * SEC..20 * SEC + 5].copy_from_slice(b"hello");

        let mut fs = Iso9660Filesystem::new(Box::new(CursorReader(Cursor::new(img)))).unwrap();
        assert!(fs.high_sierra, "should detect High Sierra");
        assert!(!fs.joliet);
        assert_eq!(fs.volume_name(), Some("HSFDISC"));

        let root = fs.root().unwrap();
        let entries = fs.list_directory(&root).unwrap();
        assert_eq!(entries.len(), 2);
        // Directories sort first.
        assert_eq!(entries[0].name, "SUBDIR");
        assert!(entries[0].is_directory());
        assert_eq!(entries[1].name, "FILE.TXT");
        assert!(entries[1].is_file());
        assert_eq!(entries[1].size, 5);
        // File read exercises the shared read path.
        assert_eq!(fs.read_file(&entries[1]).unwrap(), b"hello");
    }
}
