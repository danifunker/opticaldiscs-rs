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
        Self { name, path, entry_type: EntryType::File, size, location, children: None }
    }

    pub fn new_directory(name: String, path: String, location: u64) -> Self {
        Self { name, path, entry_type: EntryType::Directory, size: 0, location, children: None }
    }

    pub fn root(location: u64) -> Self {
        Self {
            name:       String::new(),
            path:       "/".to_string(),
            entry_type: EntryType::Directory,
            size:       0,
            location,
            children:   None,
        }
    }

    pub fn is_directory(&self) -> bool { self.entry_type == EntryType::Directory }
    pub fn is_file(&self)      -> bool { self.entry_type == EntryType::File }

    /// Human-friendly size string (e.g. `"1.4 MB"`). Empty for directories.
    pub fn size_string(&self) -> String {
        if self.is_directory() { return String::new(); }
        match self.size {
            s if s < 1_024               => format!("{} B", s),
            s if s < 1_024 * 1_024       => format!("{:.1} KB", s as f64 / 1_024.0),
            s if s < 1_024 * 1_024 * 1_024 => format!("{:.1} MB", s as f64 / (1_024.0 * 1_024.0)),
            s                            => format!("{:.2} GB", s as f64 / (1_024.0 * 1_024.0 * 1_024.0)),
        }
    }
}
