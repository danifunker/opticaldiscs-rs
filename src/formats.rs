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
    /// Dreamcast GD-ROM track descriptor (`.gdi`) with sidecar track files.
    Gdi,
    /// Nintendo GameCube / Wii disc image (`.gcm`, `.rvz`, `.wbfs`, `.ciso`,
    /// `.gcz`, `.wia`, `.tgc`, `.nfs`) — read via the `nod` crate. A raw GameCube
    /// or Wii dump may also carry a plain `.iso` extension; those are detected by
    /// magic and browsed through the same path.
    Nintendo,
    /// PSP "CISO" compressed ISO (`.cso`) — ISO 9660 split into raw-DEFLATE
    /// blocks. Distinct from the GameCube `.ciso` handled by [`Self::Nintendo`].
    Cso,
    /// gzip-compressed disc image (`.gz`) — typically a PS2 ISO stored as plain
    /// gzip (e.g. for PCSX2). Decompressed on the fly to a cooked view.
    Gz,
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
            "gdi" => Some(Self::Gdi),
            "gcm" | "rvz" | "wbfs" | "ciso" | "gcz" | "wia" | "tgc" | "nfs" => Some(Self::Nintendo),
            "cso" => Some(Self::Cso),
            "gz" => Some(Self::Gz),
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
            Self::Gdi => "GDI (Dreamcast GD-ROM)",
            Self::Nintendo => "Nintendo GameCube/Wii",
            Self::Cso => "CSO (PSP compressed ISO)",
            Self::Gz => "GZ (gzip-compressed image)",
        }
    }

    /// File extensions associated with this format.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Iso => &["iso", "toast"],
            Self::BinCue => &["bin", "cue"],
            Self::Chd => &["chd"],
            Self::MdsMdf => &["mds", "mdf"],
            Self::Gdi => &["gdi"],
            Self::Nintendo => &["gcm", "rvz", "wbfs", "ciso", "gcz", "wia", "tgc", "nfs"],
            Self::Cso => &["cso"],
            Self::Gz => &["gz"],
        }
    }
}

/// All file extensions recognised by the library, for use in file-open dialogs.
pub fn supported_extensions() -> &'static [&'static str] {
    &[
        "iso", "toast", "bin", "cue", "chd", "mds", "mdf", "gdi", "gcm", "rvz", "wbfs", "ciso",
        "gcz", "wia", "tgc", "nfs", "cso", "gz",
    ]
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
    /// VMS ODS-2 / Files-11 — OpenVMS (VAX / Alpha) discs.
    Ods2,
    /// Nintendo GameCube filesystem (GCM/FST), browsed via the `nod` crate.
    GameCube,
    /// Nintendo Wii filesystem (encrypted partitions + FST), via the `nod` crate.
    Wii,
    /// Philips CD-i (Green Book) — ISO 9660-derived, big-endian records.
    Cdi,
    /// 3DO Opera filesystem (big-endian, block-based directory tree).
    Opera,
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
            Self::Ods2 => "ODS-2",
            Self::GameCube => "GameCube",
            Self::Wii => "Wii",
            Self::Cdi => "CD-i",
            Self::Opera => "3DO Opera",
            Self::Unknown => "Unknown",
        }
    }

    /// Returns true if this filesystem can be browsed by the library.
    pub fn is_browsable(self) -> bool {
        matches!(
            self,
            Self::Iso9660
                | Self::HighSierra
                | Self::Hfs
                | Self::HfsPlus
                | Self::Efs
                | Self::Ufs
                | Self::Ods2
                | Self::Udf
                | Self::GameCube
                | Self::Wii
                | Self::Cdi
                | Self::Opera
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
