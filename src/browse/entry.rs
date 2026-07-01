//! File and directory entry types for disc filesystem browsing.

use crate::iso9660::Iso9660DateTime;

/// Seconds between the classic Mac epoch (1904-01-01) and the Unix epoch
/// (1970-01-01). Add/subtract to convert the raw HFS/HFS+ timestamps carried by
/// [`FileTimestamps::Hfs`] / [`FileTimestamps::HfsPlus`] to/from Unix time.
pub const MAC_EPOCH_UNIX_OFFSET: i64 = 2_082_844_800;

/// Raw, untranslated per-file timestamps, tagged by the filesystem they came
/// from. Each variant stores exactly the fields that filesystem records, in its
/// native on-disk encoding — no epoch normalization is performed, so consumers
/// can re-emit them faithfully or convert as they see fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTimestamps {
    /// Classic HFS. Seconds since the Mac epoch (1904-01-01), **local time**
    /// (HFS stores wall-clock time with no zone).
    Hfs {
        created: u32,
        modified: u32,
        backup: u32,
    },
    /// HFS+. Seconds since the Mac epoch (1904-01-01), **GMT**.
    HfsPlus {
        created: u32,
        content_modified: u32,
        attribute_modified: u32,
        accessed: u32,
        backup: u32,
    },
    /// ISO 9660. `recorded` is the directory record's recording time (always
    /// present); the optional fields are filled from Rock Ridge `TF` entries
    /// when the disc carries them.
    Iso9660 {
        recorded: Iso9660DateTime,
        created: Option<Iso9660DateTime>,
        modified: Option<Iso9660DateTime>,
        accessed: Option<Iso9660DateTime>,
    },
    /// Unix-epoch seconds (EFS inode times; also Rock Ridge where applicable).
    Unix { atime: i64, mtime: i64, ctime: i64 },
}

/// POSIX ownership and permission bits, surfaced where the filesystem records
/// them: HFS+ `BSDInfo`, EFS inodes, and ISO 9660 Rock Ridge `PX` entries.
/// Values are raw on-disk bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PosixMetadata {
    /// Full mode word: file-type bits plus permission bits (e.g. `0o100644`).
    pub mode: u32,
    /// Owner user ID.
    pub uid: u32,
    /// Owner group ID.
    pub gid: u32,
}

impl PosixMetadata {
    /// Permission bits only (`mode & 0o7777`).
    pub fn permission_bits(&self) -> u32 {
        self.mode & 0o7777
    }

    /// True if the mode's type field marks a symbolic link (`S_IFLNK`).
    pub fn is_symlink(&self) -> bool {
        self.mode & 0o170000 == 0o120000
    }
}

/// A single file or directory entry within a disc filesystem.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// File or directory name (no path separator).
    pub name: String,
    /// Full absolute path from root (e.g. `"/System/Library/file.txt"`).
    pub path: String,
    /// Whether this entry is a file or directory.
    pub entry_type: EntryType,
    /// Data fork size in bytes; `0` for directories.
    pub size: u64,
    /// Filesystem-specific location hint.
    /// - ISO 9660: LBA (Logical Block Address)
    /// - HFS/HFS+: extent start block or CNID
    pub location: u64,
    /// Child entries — populated only when this directory has been expanded.
    pub children: Option<Vec<FileEntry>>,
    /// Resource fork size in bytes. `None` for filesystems without resource
    /// forks (ISO 9660), `Some(0)` for HFS/HFS+ files with no resource fork.
    pub resource_fork_size: Option<u64>,
    /// Raw 4-byte Mac Finder type code, exactly as stored on disk (e.g.
    /// `*b"TEXT"`). `None` for non-HFS filesystems. Storing the raw bytes —
    /// rather than a lossy display string — preserves high-bit/non-printable
    /// codes verbatim, which is required to re-emit a file in MacBinary /
    /// AppleDouble / BinHex form without corrupting its type. Use
    /// [`Self::type_code_string`] for a human-readable rendering.
    pub type_code: Option<[u8; 4]>,
    /// Raw 4-byte Mac Finder creator code, exactly as stored on disk (e.g.
    /// `*b"ttxt"`). `None` for non-HFS filesystems. See [`Self::type_code`].
    /// Use [`Self::creator_code_string`] for a human-readable rendering.
    pub creator_code: Option<[u8; 4]>,
    /// HFS/HFS+ Finder flags (`FInfo.fdFlags`): a 16-bit field carrying bits such
    /// as `isAlias` (`0x8000`), `isInvisible` (`0x4000`), `hasBundle` (`0x2000`),
    /// and `hasCustomIcon` (`0x0400`). `None` for non-HFS filesystems.
    pub finder_flags: Option<u16>,
    /// If this entry is an alias or symlink, the resolved target string for
    /// display. `None` for regular files.
    pub symlink_target: Option<String>,
    /// Raw on-disk timestamps for this entry, tagged by filesystem. `None` if
    /// the filesystem records none (or they were not read).
    pub timestamps: Option<FileTimestamps>,
    /// POSIX ownership/permission bits, where the filesystem records them
    /// (HFS+, EFS, ISO 9660 Rock Ridge). `None` otherwise.
    pub posix: Option<PosixMetadata>,
}

/// Whether a `FileEntry` represents a file or a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryType {
    File,
    Directory,
}

/// Format a 4-byte Mac Finder type or creator code.
///
/// If every byte is printable ASCII (0x20..=0x7E), returns the bytes as-is as
/// a string (e.g. `"TEXT"`). Otherwise returns a hex representation such as
/// `"0x00000000"` so callers can still display unusual codes.
pub fn format_mac_code(code: [u8; 4]) -> String {
    if code.iter().all(|&b| (0x20..=0x7E).contains(&b)) {
        // Safe: all bytes are printable ASCII, so the slice is valid UTF-8.
        String::from_utf8(code.to_vec()).unwrap_or_default()
    } else {
        format!(
            "0x{:02X}{:02X}{:02X}{:02X}",
            code[0], code[1], code[2], code[3]
        )
    }
}

impl FileEntry {
    pub fn new_file(name: String, path: String, size: u64, location: u64) -> Self {
        Self {
            name,
            path,
            entry_type: EntryType::File,
            size,
            location,
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

    /// Build a file entry with HFS/HFS+ metadata populated. `type_code` and
    /// `creator_code` are the raw 4-byte Finder fields (stored verbatim);
    /// `finder_flags` is the `FInfo.fdFlags` field.
    #[allow(clippy::too_many_arguments)] // metadata-rich HFS constructor
    pub fn new_hfs_file(
        name: String,
        path: String,
        size: u64,
        location: u64,
        resource_fork_size: u64,
        type_code: [u8; 4],
        creator_code: [u8; 4],
        finder_flags: u16,
    ) -> Self {
        Self {
            name,
            path,
            entry_type: EntryType::File,
            size,
            location,
            children: None,
            resource_fork_size: Some(resource_fork_size),
            type_code: Some(type_code),
            creator_code: Some(creator_code),
            finder_flags: Some(finder_flags),
            symlink_target: None,
            timestamps: None,
            posix: None,
        }
    }

    pub fn new_directory(name: String, path: String, location: u64) -> Self {
        Self {
            name,
            path,
            entry_type: EntryType::Directory,
            size: 0,
            location,
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

    pub fn root(location: u64) -> Self {
        Self {
            name: String::new(),
            path: "/".to_string(),
            entry_type: EntryType::Directory,
            size: 0,
            location,
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

    /// Human-readable rendering of [`Self::type_code`] via [`format_mac_code`]
    /// (e.g. `"TEXT"`, or `"0x12345678"` for non-printable codes). `None` for
    /// non-HFS entries.
    pub fn type_code_string(&self) -> Option<String> {
        self.type_code.map(format_mac_code)
    }

    /// Human-readable rendering of [`Self::creator_code`]. See
    /// [`Self::type_code_string`].
    pub fn creator_code_string(&self) -> Option<String> {
        self.creator_code.map(format_mac_code)
    }

    pub fn is_directory(&self) -> bool {
        self.entry_type == EntryType::Directory
    }
    pub fn is_file(&self) -> bool {
        self.entry_type == EntryType::File
    }

    /// Total size of both data and resource forks. For non-HFS entries or
    /// entries with no resource fork this equals [`Self::size`].
    pub fn total_size(&self) -> u64 {
        self.size + self.resource_fork_size.unwrap_or(0)
    }

    /// Human-friendly size string (e.g. `"1.4 MB"`). Empty for directories.
    pub fn size_string(&self) -> String {
        if self.is_directory() {
            return String::new();
        }
        match self.size {
            s if s < 1_024 => format!("{} B", s),
            s if s < 1_024 * 1_024 => format!("{:.1} KB", s as f64 / 1_024.0),
            s if s < 1_024 * 1_024 * 1_024 => format!("{:.1} MB", s as f64 / (1_024.0 * 1_024.0)),
            s => format!("{:.2} GB", s as f64 / (1_024.0 * 1_024.0 * 1_024.0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_file_fields() {
        let e = FileEntry::new_file("readme.txt".into(), "/readme.txt".into(), 1234, 42);
        assert_eq!(e.name, "readme.txt");
        assert_eq!(e.path, "/readme.txt");
        assert_eq!(e.size, 1234);
        assert_eq!(e.location, 42);
        assert!(e.is_file());
        assert!(!e.is_directory());
        assert!(e.children.is_none());
    }

    #[test]
    fn new_directory_fields() {
        let e = FileEntry::new_directory("System".into(), "/System".into(), 17);
        assert_eq!(e.name, "System");
        assert_eq!(e.size, 0);
        assert!(e.is_directory());
        assert!(!e.is_file());
    }

    #[test]
    fn root_entry() {
        let e = FileEntry::root(16);
        assert_eq!(e.path, "/");
        assert!(e.name.is_empty());
        assert!(e.is_directory());
        assert_eq!(e.location, 16);
    }

    #[test]
    fn size_string_bytes() {
        let e = FileEntry::new_file("f".into(), "/f".into(), 512, 0);
        assert_eq!(e.size_string(), "512 B");
    }

    #[test]
    fn size_string_kb() {
        let e = FileEntry::new_file("f".into(), "/f".into(), 2048, 0);
        assert_eq!(e.size_string(), "2.0 KB");
    }

    #[test]
    fn size_string_mb() {
        let e = FileEntry::new_file("f".into(), "/f".into(), 1_572_864, 0);
        assert_eq!(e.size_string(), "1.5 MB");
    }

    #[test]
    fn size_string_gb() {
        let e = FileEntry::new_file("f".into(), "/f".into(), 2_147_483_648, 0);
        assert_eq!(e.size_string(), "2.00 GB");
    }

    #[test]
    fn size_string_empty_for_directory() {
        let e = FileEntry::new_directory("dir".into(), "/dir".into(), 0);
        assert_eq!(e.size_string(), "");
    }

    #[test]
    fn new_file_has_no_hfs_metadata() {
        let e = FileEntry::new_file("a".into(), "/a".into(), 1, 1);
        assert!(e.resource_fork_size.is_none());
        assert!(e.type_code.is_none());
        assert!(e.creator_code.is_none());
        assert!(e.finder_flags.is_none());
        assert!(e.type_code_string().is_none());
    }

    #[test]
    fn new_hfs_file_populates_metadata() {
        let e = FileEntry::new_hfs_file(
            "note".into(),
            "/note".into(),
            100,
            77,
            50,
            *b"TEXT",
            *b"ttxt",
            0x4000, // isInvisible
        );
        assert_eq!(e.size, 100);
        assert_eq!(e.resource_fork_size, Some(50));
        // Raw bytes stored verbatim …
        assert_eq!(e.type_code, Some(*b"TEXT"));
        assert_eq!(e.creator_code, Some(*b"ttxt"));
        assert_eq!(e.finder_flags, Some(0x4000));
        // … and the display helper renders them.
        assert_eq!(e.type_code_string().as_deref(), Some("TEXT"));
        assert_eq!(e.creator_code_string().as_deref(), Some("ttxt"));
        assert_eq!(e.total_size(), 150);
    }

    #[test]
    fn high_bit_type_code_round_trips_raw() {
        // Prince of Persia-style creator with a high-bit byte (0xC4): the raw
        // bytes survive, while the display helper falls back to hex.
        let e = FileEntry::new_hfs_file(
            "x".into(),
            "/x".into(),
            1,
            1,
            0,
            *b"APPL",
            [0x50, 0x6F, 0xC4, 0x50],
            0,
        );
        assert_eq!(e.creator_code, Some([0x50, 0x6F, 0xC4, 0x50]));
        assert_eq!(e.creator_code_string().as_deref(), Some("0x506FC450"));
    }

    #[test]
    fn total_size_without_resource_fork() {
        let e = FileEntry::new_file("f".into(), "/f".into(), 42, 1);
        assert_eq!(e.total_size(), 42);
    }

    #[test]
    fn format_mac_code_printable_ascii() {
        assert_eq!(format_mac_code(*b"TEXT"), "TEXT");
        assert_eq!(format_mac_code(*b"ttxt"), "ttxt");
    }

    #[test]
    fn format_mac_code_non_printable_falls_back_to_hex() {
        assert_eq!(format_mac_code([0, 0, 0, 0]), "0x00000000");
        assert_eq!(format_mac_code([0xDE, 0xAD, 0xBE, 0xEF]), "0xDEADBEEF");
    }
}
