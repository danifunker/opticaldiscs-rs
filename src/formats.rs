//! Disc image format and filesystem type definitions.

use std::path::Path;

/// Supported disc image container formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscFormat {
    /// Plain ISO 9660 image (`.iso`, `.toast`) — 2048-byte cooked sectors.
    Iso,
    /// Raw binary with CUE sheet (`.bin` + `.cue`) — 2352-byte raw sectors.
    BinCue,
    /// MAME Compressed Hunks of Data, optical variant (`.chd`).
    Chd,
    /// Media Descriptor Sidecar (`.mds` + `.mdf`) — not yet implemented.
    MdsMdf,
}

impl DiscFormat {
    /// Detect format from file extension (case-insensitive).
    pub fn from_path(path: impl AsRef<Path>) -> Option<Self> {
        let ext = path.as_ref().extension()?.to_str()?.to_lowercase();
        match ext.as_str() {
            "iso" | "toast" => Some(Self::Iso),
            "bin" | "cue" => Some(Self::BinCue),
            "chd" => Some(Self::Chd),
            "mds" | "mdf" => Some(Self::MdsMdf),
            _ => None,
        }
    }

    /// Human-readable name for this format.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Iso => "ISO image",
            Self::BinCue => "BIN/CUE",
            Self::Chd => "CHD (Compressed Hunks of Data)",
            Self::MdsMdf => "MDS/MDF",
        }
    }

    /// File extensions associated with this format.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Iso => &["iso", "toast"],
            Self::BinCue => &["bin", "cue"],
            Self::Chd => &["chd"],
            Self::MdsMdf => &["mds", "mdf"],
        }
    }
}

/// All file extensions recognised by the library, for use in file-open dialogs.
pub fn supported_extensions() -> &'static [&'static str] {
    &["iso", "toast", "bin", "cue", "chd", "mds", "mdf"]
}

/// Filesystem type found on the data track of a disc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemType {
    /// ISO 9660 standard filesystem (data CDs, DVDs).
    Iso9660,
    /// High Sierra Format — the pre-ISO 9660 CD-ROM filesystem (1986 standard,
    /// `CDROM` identifier). Early Microsoft/IBM titles (Bookshelf, Programmer's
    /// Library). Browsed by the ISO 9660 reader with High-Sierra field offsets.
    HighSierra,
    /// Joliet extensions (Unicode long filenames on top of ISO 9660).
    Joliet,
    /// Universal Disk Format (DVDs, Blu-ray).
    Udf,
    /// Classic HFS (Macintosh CDs, pre-Mac OS X).
    Hfs,
    /// HFS+ / Mac OS Extended (Mac OS X CDs and DVDs).
    HfsPlus,
    /// SGI EFS (Extent File System) — IRIX install/distribution CDs.
    Efs,
    /// UFS / FFS (BSD Fast File System) — Digital UNIX / Tru64, SunOS/Solaris CDs.
    Ufs,
    /// Could not be determined.
    Unknown,
}

impl FilesystemType {
    /// Human-readable name.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Iso9660 => "ISO 9660",
            Self::HighSierra => "High Sierra",
            Self::Joliet => "Joliet",
            Self::Udf => "UDF",
            Self::Hfs => "HFS",
            Self::HfsPlus => "HFS+",
            Self::Efs => "EFS",
            Self::Ufs => "UFS",
            Self::Unknown => "Unknown",
        }
    }

    /// Returns true if this filesystem can be browsed by the library.
    pub fn is_browsable(self) -> bool {
        matches!(
            self,
            Self::Iso9660 | Self::HighSierra | Self::Hfs | Self::HfsPlus | Self::Efs | Self::Ufs
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_from_extension() {
        assert_eq!(DiscFormat::from_path("game.iso"), Some(DiscFormat::Iso));
        assert_eq!(DiscFormat::from_path("game.ISO"), Some(DiscFormat::Iso));
        assert_eq!(DiscFormat::from_path("game.toast"), Some(DiscFormat::Iso));
        assert_eq!(DiscFormat::from_path("game.bin"), Some(DiscFormat::BinCue));
        assert_eq!(DiscFormat::from_path("game.cue"), Some(DiscFormat::BinCue));
        assert_eq!(DiscFormat::from_path("game.chd"), Some(DiscFormat::Chd));
        assert_eq!(DiscFormat::from_path("game.txt"), None);
    }
}
