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
    /// File size in bytes; `0` for directories.
    pub size: u64,
    /// Filesystem-specific location hint.
    /// - ISO 9660: LBA (Logical Block Address)
    /// - HFS/HFS+: extent start block or CNID
    pub location: u64,
    /// Child entries — populated only when this directory has been expanded.
    pub children: Option<Vec<FileEntry>>,
}

/// Whether a `FileEntry` represents a file or a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryType {
    File,
    Directory,
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
        }
    }

    pub fn is_directory(&self) -> bool {
        self.entry_type == EntryType::Directory
    }
    pub fn is_file(&self) -> bool {
        self.entry_type == EntryType::File
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
}
