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
    /// CloneCD image (`.ccd` descriptor + `.img` raw data + optional `.sub`).
    CloneCd,
    /// Nero image (`.nrg`) — data plus a footer-anchored chunk descriptor.
    Nrg,
    /// DiscJuggler image (`.cdi`) — data plus an end-anchored track descriptor.
    ///
    /// Named for the container (not the Philips CD-i filesystem in
    /// [`FilesystemType::Cdi`], which is orthogonal).
    DiscJuggler,
    /// DAEMON Tools image (`.mdx`) — encrypted+compressed descriptor with
    /// zlib-compressed track data. Browsable only when the crate is built with
    /// the `mdx` feature (it pulls in a crypto stack).
    Mdx,
}

impl DiscFormat {
    /// Every container format this crate knows about, in declaration order.
    ///
    /// Includes formats that the current build cannot open — pair with
    /// [`is_supported`](Self::is_supported) to get only the usable ones:
    ///
    /// ```
    /// use opticaldiscs::DiscFormat;
    ///
    /// let usable: Vec<_> = DiscFormat::ALL
    ///     .iter()
    ///     .copied()
    ///     .filter(|f| f.is_supported())
    ///     .collect();
    /// assert!(usable.contains(&DiscFormat::Iso));
    /// ```
    pub const ALL: &'static [DiscFormat] = &[
        Self::Iso,
        Self::BinCue,
        Self::Chd,
        Self::MdsMdf,
        Self::Gdi,
        Self::Nintendo,
        Self::Cso,
        Self::Gz,
        Self::CloneCd,
        Self::Nrg,
        Self::DiscJuggler,
        Self::Mdx,
    ];

    /// Detect format from file extension (case-insensitive).
    pub fn from_path(path: impl AsRef<Path>) -> Option<Self> {
        let ext = path.as_ref().extension()?.to_str()?.to_lowercase();
        Self::from_extension(&ext)
    }

    /// Map a lowercase extension (no leading dot) to its format.
    ///
    /// Only *entry-point* extensions map to a format: a CloneCD image is opened
    /// through its `.ccd` descriptor, so the `img`/`sub` sidecars listed by
    /// [`extensions`](Self::extensions) resolve to `None` here — they are not
    /// files a caller opens directly.
    fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "iso" | "toast" => Some(Self::Iso),
            "bin" | "cue" => Some(Self::BinCue),
            "chd" => Some(Self::Chd),
            "mds" | "mdf" => Some(Self::MdsMdf),
            "gdi" => Some(Self::Gdi),
            "gcm" | "rvz" | "wbfs" | "ciso" | "gcz" | "wia" | "tgc" | "nfs" => Some(Self::Nintendo),
            "cso" => Some(Self::Cso),
            "gz" => Some(Self::Gz),
            "ccd" => Some(Self::CloneCd),
            "nrg" => Some(Self::Nrg),
            "cdi" => Some(Self::DiscJuggler),
            "mdx" => Some(Self::Mdx),
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
            Self::CloneCd => "CloneCD (CCD/IMG)",
            Self::Nrg => "Nero (NRG)",
            Self::DiscJuggler => "DiscJuggler (CDI)",
            Self::Mdx => "DAEMON Tools (MDX)",
        }
    }

    /// Whether *this build* can actually open images of this format.
    ///
    /// Every format is recognised by [`from_path`](Self::from_path) and by magic
    /// bytes regardless of how the crate was compiled — recognition is what lets
    /// a caller report "this is a CHD, but CHD support wasn't built in" instead
    /// of "unknown file". This method reports whether the read path exists:
    ///
    /// - [`Chd`](Self::Chd) requires the `chd` feature (on by default).
    /// - [`Mdx`](Self::Mdx) requires the `mdx` feature (off by default).
    /// - Every other format is unconditional.
    ///
    /// Opening an unsupported format returns
    /// [`OpticaldiscsError::UnsupportedFormat`](crate::OpticaldiscsError::UnsupportedFormat);
    /// use this to filter a file-open dialog or hide a UI affordance up front
    /// rather than surfacing the error later.
    ///
    /// ```
    /// use opticaldiscs::DiscFormat;
    ///
    /// assert!(DiscFormat::Iso.is_supported());
    /// assert_eq!(DiscFormat::Chd.is_supported(), cfg!(feature = "chd"));
    /// ```
    pub const fn is_supported(self) -> bool {
        match self {
            Self::Chd => cfg!(feature = "chd"),
            Self::Mdx => cfg!(feature = "mdx"),
            Self::Iso
            | Self::BinCue
            | Self::MdsMdf
            | Self::Gdi
            | Self::Nintendo
            | Self::Cso
            | Self::Gz
            | Self::CloneCd
            | Self::Nrg
            | Self::DiscJuggler => true,
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
            Self::CloneCd => &["ccd", "img", "sub"],
            Self::Nrg => &["nrg"],
            Self::DiscJuggler => &["cdi"],
            Self::Mdx => &["mdx"],
        }
    }
}

/// All file extensions recognised by the library, for use in file-open dialogs.
///
/// This is the full set the crate can *identify*, independent of feature flags —
/// it does not shrink when a format's read path is compiled out. For only the
/// extensions this build can actually open, use [`enabled_extensions`].
pub fn supported_extensions() -> &'static [&'static str] {
    &[
        "iso", "toast", "bin", "cue", "chd", "mds", "mdf", "gdi", "gcm", "rvz", "wbfs", "ciso",
        "gcz", "wia", "tgc", "nfs", "cso", "gz", "ccd", "nrg", "cdi", "mdx",
    ]
}

/// File extensions this build can actually open, for use in file-open dialogs.
///
/// [`supported_extensions`] lists everything the crate can *identify*;
/// this is that set minus the formats whose feature is off (see
/// [`DiscFormat::is_supported`]). With default features that means `.mdx` is
/// absent; in a build without `chd`, `.chd` is absent too.
///
/// Returns an owned `Vec` because the result depends on the enabled feature
/// combination; `supported_extensions` stays a `&'static [_]`.
///
/// Derived by filtering [`supported_extensions`], so it is always a subset of it
/// and keeps the same "extensions a caller can open directly" semantics — the
/// `img`/`sub` CloneCD sidecars are excluded from both.
pub fn enabled_extensions() -> Vec<&'static str> {
    supported_extensions()
        .iter()
        .copied()
        .filter(|ext| DiscFormat::from_extension(ext).is_some_and(|f| f.is_supported()))
        .collect()
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
    /// Xbox / Xbox 360 XDVDFS (little-endian, binary-tree directories).
    Xdvdfs,
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
            Self::Xdvdfs => "Xbox XDVDFS",
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
                | Self::Xdvdfs
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

    /// Extension recognition must be feature-independent: a `.chd` is a CHD even
    /// in a build that can't open one.
    #[test]
    fn recognition_is_independent_of_features() {
        assert_eq!(DiscFormat::from_path("game.chd"), Some(DiscFormat::Chd));
        assert_eq!(DiscFormat::from_path("game.mdx"), Some(DiscFormat::Mdx));
        assert!(supported_extensions().contains(&"chd"));
        assert!(supported_extensions().contains(&"mdx"));
    }

    #[test]
    fn is_supported_reflects_features() {
        assert_eq!(DiscFormat::Chd.is_supported(), cfg!(feature = "chd"));
        assert_eq!(DiscFormat::Mdx.is_supported(), cfg!(feature = "mdx"));

        // Every other format is unconditional.
        for f in DiscFormat::ALL {
            if !matches!(f, DiscFormat::Chd | DiscFormat::Mdx) {
                assert!(
                    f.is_supported(),
                    "{f:?} should be unconditionally supported"
                );
            }
        }
    }

    #[test]
    fn all_covers_every_extension() {
        // `ALL` must stay exhaustive, or `enabled_extensions` silently drops
        // formats. Cross-check it against the static extension list.
        for ext in supported_extensions() {
            let fmt = DiscFormat::from_path(format!("x.{ext}"))
                .unwrap_or_else(|| panic!("`{ext}` maps to no DiscFormat"));
            assert!(
                DiscFormat::ALL.contains(&fmt),
                "{fmt:?} (from `{ext}`) missing from DiscFormat::ALL"
            );
        }
    }

    #[test]
    fn enabled_extensions_filters_by_feature() {
        let enabled = enabled_extensions();

        assert!(enabled.contains(&"iso"));
        assert_eq!(enabled.contains(&"chd"), cfg!(feature = "chd"));
        assert_eq!(enabled.contains(&"mdx"), cfg!(feature = "mdx"));

        // Never wider than the recognised set.
        for ext in &enabled {
            assert!(
                supported_extensions().contains(ext),
                "`{ext}` enabled but not in supported_extensions()"
            );
        }
    }
}
