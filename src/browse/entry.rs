//! File and directory entry types for disc filesystem browsing.

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
    /// 4-byte Mac type code (e.g. `"TEXT"`). `None` for non-HFS filesystems.
    pub type_code: Option<String>,
    /// 4-byte Mac creator code (e.g. `"ttxt"`). `None` for non-HFS filesystems.
    pub creator_code: Option<String>,
    /// If this entry is an alias or symlink, the resolved target string for
    /// display. `None` for regular files.
    pub symlink_target: Option<String>,
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
            symlink_target: None,
        }
    }

    /// Build a file entry with HFS/HFS+ metadata populated. `type_code` and
    /// `creator_code` are the raw 4-byte Finder fields, formatted via
    /// [`format_mac_code`].
    pub fn new_hfs_file(
        name: String,
        path: String,
        size: u64,
        location: u64,
        resource_fork_size: u64,
        type_code: [u8; 4],
        creator_code: [u8; 4],
    ) -> Self {
        Self {
            name,
            path,
            entry_type: EntryType::File,
            size,
            location,
            children: None,
            resource_fork_size: Some(resource_fork_size),
            type_code: Some(format_mac_code(type_code)),
            creator_code: Some(format_mac_code(creator_code)),
            symlink_target: None,
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
            symlink_target: None,
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
            symlink_target: None,
        }
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
        );
        assert_eq!(e.size, 100);
        assert_eq!(e.resource_fork_size, Some(50));
        assert_eq!(e.type_code.as_deref(), Some("TEXT"));
        assert_eq!(e.creator_code.as_deref(), Some("ttxt"));
        assert_eq!(e.total_size(), 150);
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
