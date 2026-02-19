//! ISO 9660 filesystem browser.
//!
//! Provides [`Iso9660Filesystem`], which implements the [`Filesystem`] trait
//! for ISO 9660 disc images regardless of container format (plain ISO,
//! BIN/CUE, or CHD).
//!
//! Create one via [`crate::browse::open_disc_filesystem`] or directly with
//! [`Iso9660Filesystem::new`] when you already have a `Box<dyn SectorReader>`.

use super::entry::{EntryType, FileEntry};
use super::filesystem::{Filesystem, FilesystemError};
use crate::iso9660::PrimaryVolumeDescriptor;
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
}

impl Iso9660Filesystem {
    /// Create an `Iso9660Filesystem` by reading the PVD from `reader`.
    ///
    /// Reads sector 16, validates the ISO 9660 header, and extracts the root
    /// directory location and size.
    ///
    /// # Errors
    ///
    /// Returns [`FilesystemError::Parse`] if the PVD is missing or malformed.
    pub fn new(mut reader: Box<dyn SectorReader>) -> Result<Self, FilesystemError> {
        let pvd = PrimaryVolumeDescriptor::read_from(&mut *reader)
            .map_err(|e| FilesystemError::Parse(e.to_string()))?;

        Ok(Self {
            reader,
            root_location: pvd.root_directory_lba,
            root_size: pvd.root_directory_size,
            volume_id: pvd.volume_id,
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
    fn parse_directory(&self, data: &[u8], parent_path: &str) -> Vec<FileEntry> {
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

            if let Some(rec) = DirectoryRecord::parse(&data[offset..offset + record_length]) {
                if !rec.is_self() && !rec.is_parent() {
                    let name = rec.clean_name();
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
    /// Raw file identifier (may contain version suffix such as `;1`).
    file_identifier: String,
}

impl DirectoryRecord {
    /// Parse a directory record from a raw byte slice.
    ///
    /// Returns `None` if the slice is shorter than 33 bytes or the record
    /// length field is zero.
    fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 33 || data[0] == 0 {
            return None;
        }

        let extent_location = u32::from_le_bytes(data[2..6].try_into().ok()?);
        let data_length = u32::from_le_bytes(data[10..14].try_into().ok()?);
        let file_flags = data[25];
        let id_len = data[32] as usize;

        if data.len() < 33 + id_len {
            return None;
        }

        let file_identifier = String::from_utf8_lossy(&data[33..33 + id_len]).to_string();

        Some(Self {
            extent_location,
            data_length,
            file_flags,
            file_identifier,
        })
    }

    fn is_directory(&self) -> bool {
        (self.file_flags & 0x02) != 0
    }

    /// True for the `.` (current directory) entry (identifier `\x00`).
    fn is_self(&self) -> bool {
        self.file_identifier == "\0" || self.file_identifier.is_empty()
    }

    /// True for the `..` (parent directory) entry (identifier `\x01`).
    fn is_parent(&self) -> bool {
        self.file_identifier == "\x01"
    }

    /// Return the display name, stripping the `;1` version suffix from files
    /// and trailing dots from directory names.
    fn clean_name(&self) -> String {
        if self.is_self() {
            return ".".to_string();
        }
        if self.is_parent() {
            return "..".to_string();
        }

        // Strip version suffix (";1", ";2", …).
        let name = match self.file_identifier.rfind(';') {
            Some(idx) => &self.file_identifier[..idx],
            None => &self.file_identifier,
        };

        // Strip trailing dot — ISO 9660 directory identifiers end with `.`.
        let name = if self.is_directory() {
            name.trim_end_matches('.')
        } else {
            name
        };

        name.to_string()
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
        let mut append_rec = |buf: &mut Vec<u8>,
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

        let rec = DirectoryRecord::parse(&data).unwrap();
        assert_eq!(rec.extent_location, 42);
        assert_eq!(rec.data_length, 1024);
        assert!(!rec.is_directory());
        assert!(!rec.is_self());
        assert!(!rec.is_parent());
        assert_eq!(rec.clean_name(), "TEST.TXT");
    }

    #[test]
    fn directory_record_parse_self_entry() {
        let mut data = vec![0u8; 34];
        data[0] = 34;
        data[25] = 0x02;
        data[32] = 1;
        data[33] = 0x00; // self identifier
        let rec = DirectoryRecord::parse(&data).unwrap();
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
        let rec = DirectoryRecord::parse(&data).unwrap();
        assert!(rec.is_parent());
    }

    #[test]
    fn directory_record_too_short_returns_none() {
        assert!(DirectoryRecord::parse(&[0u8; 32]).is_none());
    }

    #[test]
    fn clean_name_strips_version_suffix() {
        let rec = DirectoryRecord {
            extent_location: 0,
            data_length: 0,
            file_flags: 0,
            file_identifier: "ARCHIVE.TAR;1".to_string(),
        };
        assert_eq!(rec.clean_name(), "ARCHIVE.TAR");
    }

    #[test]
    fn clean_name_strips_directory_trailing_dot() {
        let rec = DirectoryRecord {
            extent_location: 0,
            data_length: 0,
            file_flags: 0x02,
            file_identifier: "SYSTEM.".to_string(),
        };
        assert_eq!(rec.clean_name(), "SYSTEM");
    }
}
