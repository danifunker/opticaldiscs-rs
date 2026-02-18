//! Physical optical drive enumeration.
//!
//! Requires feature `drives`. Provides `OpticalDrive` and `list_drives()`.
//!
//! See PLAN.md Phase 9 for implementation details.

// TODO: implement OpticalDrive, list_drives() with platform-specific
//       backends (Linux /sys/block/sr*, macOS ioreg, Windows DRIVE_CDROM)
//       in Phase 9.
