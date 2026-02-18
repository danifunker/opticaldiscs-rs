//! Filesystem browsing for disc images.
//!
//! Entry point: `open_disc_filesystem()` — returns a `Box<dyn Filesystem>`
//! for any supported disc image and filesystem combination.
//!
//! See PLAN.md Phases 7–8 for implementation details.

pub mod entry;
pub mod filesystem;

// Phase 7
pub mod iso9660;

// Phase 8
pub mod hfs;
pub mod hfsplus;

pub use entry::{EntryType, FileEntry};
pub use filesystem::{Filesystem, FilesystemError};

// TODO: implement open_disc_filesystem() in Phase 7
