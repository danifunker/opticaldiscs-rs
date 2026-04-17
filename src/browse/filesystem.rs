//! `Filesystem` trait — the common interface for all disc filesystem browsers.

use super::entry::FileEntry;
use thiserror::Error;

/// Errors returned by filesystem operations.
#[derive(Debug, Error)]
pub enum FilesystemError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Not a directory: {0}")]
    NotADirectory(String),

    #[error("Entry not found: {0}")]
    NotFound(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Unsupported filesystem")]
    Unsupported,

    #[error("Invalid data: {0}")]
    InvalidData(String),
}

/// Uniform interface for browsing files on a disc image, regardless of
/// container format (ISO, BIN/CUE, CHD) or on-disc filesystem (ISO9660, HFS, HFS+).
pub trait Filesystem: Send {
    /// Return the root directory entry.
    fn root(&mut self) -> Result<FileEntry, FilesystemError>;

    /// List the direct children of a directory entry.
    fn list_directory(&mut self, entry: &FileEntry) -> Result<Vec<FileEntry>, FilesystemError>;

    /// Read the entire contents of a file entry into memory.
    fn read_file(&mut self, entry: &FileEntry) -> Result<Vec<u8>, FilesystemError>;

    /// Read a byte range from a file (for large files that should not be fully loaded).
    fn read_file_range(
        &mut self,
        entry: &FileEntry,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, FilesystemError>;

    /// Read the entire resource fork of a file.
    ///
    /// Returns `Ok(None)` when the filesystem does not support resource forks
    /// (ISO 9660) or when the file has no resource fork. HFS/HFS+ return
    /// `Ok(Some(vec![]))` for files whose resource fork exists but has zero
    /// length — callers can use `resource_fork_size` on [`FileEntry`] to
    /// decide whether calling this is worthwhile.
    fn read_resource_fork(&mut self, entry: &FileEntry)
        -> Result<Option<Vec<u8>>, FilesystemError>;

    /// Read a byte range from the resource fork. Returns `Ok(None)` under
    /// the same conditions as [`Self::read_resource_fork`].
    fn read_resource_fork_range(
        &mut self,
        entry: &FileEntry,
        offset: u64,
        length: usize,
    ) -> Result<Option<Vec<u8>>, FilesystemError>;

    /// The volume label, if the filesystem provides one.
    fn volume_name(&self) -> Option<&str>;
}
