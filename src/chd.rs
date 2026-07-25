//! CHD (Compressed Hunks of Data) optical disc reading.
//!
//! Thin wrapper around `libchdman-rs` — MAME's `chd_file` core via Rust
//! bindings — that exposes the track metadata needed by the rest of this
//! crate without leaking the upstream types into `opticaldiscs`'s public API.
//!
//! Actual sector decompression is handled by `ChdSectorReader` in
//! [`crate::sector_reader`].
//!
//! # Feature `chd`
//!
//! Reading a CHD requires the `chd` feature (**on by default**), which pulls in
//! `libchdman-rs`. This module is compiled either way: the descriptive types
//! ([`ChdTrack`], [`ChdTrackType`], [`ChdInfo`] and their helpers) are plain
//! Rust and always available, so downstream code that names them keeps
//! compiling in a lean build. Only `open_chd` — the one function that touches
//! the C++ core — is feature-gated, along with `ChdSectorReader`.
//!
//! Ask at runtime with [`is_supported`] (or
//! [`DiscFormat::Chd.is_supported()`](crate::DiscFormat::is_supported)) rather
//! than duplicating `cfg!(feature = "chd")` at every call site.
//!
//! (Items gated behind `chd` are referred to in plain backticks above, not as
//! doc links: in a build without the feature they do not exist, and a link to
//! them would be an unresolved-link warning for every downstream `cargo doc`.)

#[cfg(feature = "chd")]
use std::path::Path;

#[cfg(feature = "chd")]
use libchdman_rs::cd::{list_tracks, TrackType as LibTrackType};
#[cfg(feature = "chd")]
use libchdman_rs::Chd;

#[cfg(feature = "chd")]
use crate::error::{OpticaldiscsError, Result};

/// Name the media kind a CHD's info record declares, for error messages.
#[cfg(feature = "chd")]
fn describe_media(info: &libchdman_rs::ChdInfo) -> &'static str {
    if info.is_hd {
        "hard-disk"
    } else if info.is_dvd {
        "DVD"
    } else if info.is_av {
        "A/V"
    } else {
        "non-CD"
    }
}

/// Map a `cd::*` failure, translating "not CD media" to
/// [`OpticaldiscsError::UnsupportedFormat`].
///
/// A hard-disk or DVD CHD is a perfectly valid file that simply isn't an optical
/// disc image, so it is an unsupported *format* rather than a CHD malfunction —
/// which lets a caller route it elsewhere instead of reporting corruption.
///
/// Requires libchdman-rs >= 0.288.10. Before that release these calls aborted
/// the process (MAME's `cdrom_file` constructor throws a bare `nullptr`, which
/// Rust frames cannot unwind), so there was no error to map; that is why
/// `Cargo.toml` sets 0.288.10 as the floor.
#[cfg(feature = "chd")]
fn map_cd_err(path: &Path, e: libchdman_rs::ChdError) -> OpticaldiscsError {
    match e {
        libchdman_rs::ChdError::NotCdMedia => OpticaldiscsError::UnsupportedFormat(format!(
            "{} is a CHD without CD/GD-ROM geometry, not a disc image",
            path.display()
        )),
        other => OpticaldiscsError::Chd(format!("read CHD tracks: {other:?}")),
    }
}

/// Whether this build can open CHD images — i.e. whether the `chd` feature is
/// enabled.
///
/// `.chd` files are recognised by [`crate::detect::detect_format`] either way;
/// this reports whether `open_chd` and `ChdSectorReader` exist and whether
/// [`DiscImageInfo::open`](crate::detect::DiscImageInfo::open) can get past
/// identifying the container. Use it to hide or disable CHD affordances instead
/// of letting the open fail:
///
/// ```
/// if opticaldiscs::chd::is_supported() {
///     // `.chd` can be opened and browsed in this build.
/// } else {
///     // Recognised, but opening returns `UnsupportedFormat`.
/// }
/// ```
///
/// Equivalent to [`crate::DiscFormat::Chd.is_supported()`](crate::DiscFormat::is_supported),
/// which is the general form covering every conditional format.
pub const fn is_supported() -> bool {
    cfg!(feature = "chd")
}

/// Byte size of one CHD CD-ROM frame: 2352-byte raw sector + 96-byte subcode.
///
/// Retained as a public constant for downstream consumers; the
/// libchdman-rs-backed reader no longer uses it internally.
pub const CHD_CD_FRAME_SIZE: u64 = 2448;

/// Byte offset to user data within a raw Mode 1 CD frame (skip sync + header).
///
/// Retained as a public constant for downstream consumers; the
/// libchdman-rs-backed reader no longer uses it internally.
pub const CHD_MODE1_DATA_OFFSET: u64 = 16;

// ── ChdTrackType ──────────────────────────────────────────────────────────────

/// Track type as reported by the CHD's CHT2 metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChdTrackType {
    /// `"MODE1_RAW"` — Mode 1, 2352-byte raw sectors (sync + header + data + ECC).
    Mode1Raw,
    /// `"MODE1"` — Mode 1, 2048-byte cooked user data.
    Mode1Cooked,
    /// `"MODE2_RAW"` — Mode 2, 2352-byte raw sectors.
    Mode2Raw,
    /// `"MODE2_FORM1"` — Mode 2 Form 1 (PlayStation/Saturn data).
    Mode2Form1,
    /// `"MODE2_FORM2"` — Mode 2 Form 2 (PlayStation/Saturn mixed).
    Mode2Form2,
    /// `"AUDIO"` — Red Book audio track.
    Audio,
    /// Any other type string not recognised above.
    Unknown(String),
}

impl ChdTrackType {
    #[cfg(feature = "chd")]
    fn from_lib(t: LibTrackType) -> Self {
        match t {
            LibTrackType::Mode1Raw => ChdTrackType::Mode1Raw,
            LibTrackType::Mode1 => ChdTrackType::Mode1Cooked,
            LibTrackType::Mode2Raw => ChdTrackType::Mode2Raw,
            LibTrackType::Mode2Form1 => ChdTrackType::Mode2Form1,
            LibTrackType::Mode2Form2 => ChdTrackType::Mode2Form2,
            LibTrackType::Audio => ChdTrackType::Audio,
            // Mode2 / Mode2FormMix have no named variant in this public enum;
            // surface them as Unknown rather than panicking so the public type
            // doesn't lose information.
            LibTrackType::Mode2 => ChdTrackType::Unknown("MODE2".into()),
            LibTrackType::Mode2FormMix => ChdTrackType::Unknown("MODE2_FORM_MIX".into()),
        }
    }

    /// Byte offset to user data within a 2448-byte CHD frame for this track type.
    ///
    /// Raw modes store a full raw sector (sync + 4-byte header + data + ECC), so
    /// user data starts at byte 16.  All other types store data at byte 0.
    ///
    /// Retained for downstream consumers that compute their own offsets; the
    /// libchdman-rs-backed reader uses MAME's per-mode extraction instead.
    pub fn data_offset(&self) -> u64 {
        match self {
            ChdTrackType::Mode1Raw | ChdTrackType::Mode2Raw => CHD_MODE1_DATA_OFFSET,
            _ => 0,
        }
    }

    /// Returns `true` for data track types (Mode 1 / Mode 2 variants).
    pub fn is_data(&self) -> bool {
        matches!(
            self,
            ChdTrackType::Mode1Raw
                | ChdTrackType::Mode1Cooked
                | ChdTrackType::Mode2Raw
                | ChdTrackType::Mode2Form1
                | ChdTrackType::Mode2Form2
        ) || matches!(self, ChdTrackType::Unknown(s) if s.starts_with("MODE2"))
    }

    /// Returns `true` for Red Book audio tracks.
    pub fn is_audio(&self) -> bool {
        *self == ChdTrackType::Audio
    }
}

// ── ChdTrack ──────────────────────────────────────────────────────────────────

/// A single track parsed from CHD CHT2 metadata.
#[derive(Debug, Clone)]
pub struct ChdTrack {
    /// 1-based track number.
    pub track_no: u32,
    /// Track type (Mode 1 raw, Mode 1 cooked, audio, etc.).
    pub track_type: ChdTrackType,
    /// Number of frames (raw CD sectors) in this track, including any pregap.
    pub frames: u32,
    /// Cumulative frame index within the CHD data stream where this track starts.
    ///
    /// Multiply by [`CHD_CD_FRAME_SIZE`] to get the byte offset within the CHD.
    pub frame_offset: u64,
}

impl ChdTrack {
    /// Returns `true` if this is a data track (can contain a filesystem).
    pub fn is_data(&self) -> bool {
        self.track_type.is_data()
    }

    /// Returns `true` if this is a Red Book audio track.
    pub fn is_audio(&self) -> bool {
        self.track_type.is_audio()
    }

    /// Byte offset to user data within each 2448-byte CHD frame for this track.
    pub fn data_offset(&self) -> u64 {
        self.track_type.data_offset()
    }
}

// ── ChdInfo ───────────────────────────────────────────────────────────────────

/// Metadata extracted from a CHD optical disc image.
///
/// Created by `open_chd` (feature `chd`); does not perform sector
/// decompression. Use `ChdSectorReader` to read actual sector data.
#[derive(Debug)]
pub struct ChdInfo {
    /// Compressed hunk size in bytes (typically 8–64 KiB for CD-ROM).
    pub hunk_size: u32,
    /// Total uncompressed data size in bytes.
    pub logical_size: u64,
    /// Track list, sorted by track number.
    pub tracks: Vec<ChdTrack>,
}

impl ChdInfo {
    /// Return the first data track, suitable for ISO 9660 / HFS reading.
    ///
    /// Returns `None` for audio-only discs.
    pub fn find_first_data_track(&self) -> Option<&ChdTrack> {
        self.tracks.iter().find(|t| t.is_data())
    }

    /// Returns `true` if this CHD is a Dreamcast GD-ROM.
    ///
    /// A GD-ROM stores its high-density (game) area beginning at frame 45000
    /// ([`crate::sector_reader::GDROM_HD_START_LBA`]); a data track starting at
    /// or beyond that offset is the tell-tale sign, distinguishing a GD-ROM from
    /// an ordinary CD image whose tracks never reach that frame count so early.
    pub fn is_gdrom(&self) -> bool {
        self.tracks
            .iter()
            .any(|t| t.is_data() && t.frame_offset >= crate::sector_reader::GDROM_HD_START_LBA)
    }

    /// Return the Dreamcast high-density game track (the first data track at or
    /// beyond frame 45000), which carries the main ISO 9660 filesystem.
    ///
    /// Returns `None` if the CHD is not a GD-ROM.
    pub fn find_gdrom_hd_track(&self) -> Option<&ChdTrack> {
        self.tracks
            .iter()
            .find(|t| t.is_data() && t.frame_offset >= crate::sector_reader::GDROM_HD_START_LBA)
    }

    /// Return **all** high-density data tracks (every data track at or beyond
    /// frame 45000), in track order.
    ///
    /// A GD-ROM's high-density area routinely spans multiple data tracks
    /// separated by audio tracks; file and directory extents may live in any of
    /// them. Reading the whole HD-area filesystem therefore requires every such
    /// track, not just the first. Returns an empty vector if the CHD is not a
    /// GD-ROM.
    pub fn find_gdrom_hd_tracks(&self) -> Vec<&ChdTrack> {
        self.tracks
            .iter()
            .filter(|t| t.is_data() && t.frame_offset >= crate::sector_reader::GDROM_HD_START_LBA)
            .collect()
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Open a CHD file and parse its track metadata.
///
/// Reads the CHD header and track list via `libchdman-rs`; no sector data
/// is decompressed.
///
/// Requires the `chd` feature (on by default) — see [`is_supported`].
///
/// # Errors
///
/// Returns an [`OpticaldiscsError::Io`] if the path does not exist or cannot
/// be read; [`OpticaldiscsError::Chd`] for any libchdman-rs failure (invalid
/// header, unsupported format, etc.).
#[cfg(feature = "chd")]
pub fn open_chd(path: impl AsRef<Path>) -> Result<ChdInfo> {
    let path = path.as_ref();

    // Surface a missing/unreadable file as Io rather than Chd to preserve
    // the prior crate's error semantics.
    std::fs::metadata(path).map_err(OpticaldiscsError::Io)?;

    let path_str = path
        .to_str()
        .ok_or_else(|| OpticaldiscsError::Chd(format!("non-UTF-8 path: {}", path.display())))?;

    let chd = Chd::open(path_str, false, None)
        .map_err(|e| OpticaldiscsError::Chd(format!("failed to open CHD: {e:?}")))?;

    let info = chd
        .info()
        .map_err(|e| OpticaldiscsError::Chd(format!("CHD info: {e:?}")))?;

    // A CHD is a generic container: hard-disk, DVD and A/V images share the
    // `MComprHD` magic that `detect_format` keys on, so a non-optical CHD reaches
    // this function routinely. Screen it out from the info record, which is
    // already in hand — no `cdrom_file` construction, and the message names the
    // actual media kind instead of a generic parse failure.
    if !(info.is_cd || info.is_gd) {
        return Err(OpticaldiscsError::UnsupportedFormat(format!(
            "{} is a {} CHD, not a CD/GD-ROM disc image",
            path.display(),
            describe_media(&info),
        )));
    }

    let lib_tracks = list_tracks(&chd).map_err(|e| map_cd_err(path, e))?;

    let mut tracks: Vec<ChdTrack> = lib_tracks
        .into_iter()
        .map(|t| ChdTrack {
            track_no: t.track_num,
            track_type: ChdTrackType::from_lib(t.track_type),
            frames: t.frames,
            frame_offset: 0,
        })
        .collect();

    tracks.sort_by_key(|t| t.track_no);
    let mut offset = 0u64;
    for track in &mut tracks {
        track.frame_offset = offset;
        offset += track.frames as u64;
    }

    log::debug!(
        "CHD opened: hunk_bytes={}, logical_bytes={}, tracks={}",
        info.hunk_bytes,
        info.logical_bytes,
        tracks.len()
    );

    Ok(ChdInfo {
        hunk_size: info.hunk_bytes,
        logical_size: info.logical_bytes,
        tracks,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_data_offsets() {
        assert_eq!(ChdTrackType::Mode1Raw.data_offset(), 16);
        assert_eq!(ChdTrackType::Mode2Raw.data_offset(), 16);
        assert_eq!(ChdTrackType::Mode1Cooked.data_offset(), 0);
        assert_eq!(ChdTrackType::Mode2Form1.data_offset(), 0);
        assert_eq!(ChdTrackType::Mode2Form2.data_offset(), 0);
        assert_eq!(ChdTrackType::Audio.data_offset(), 0);
        assert_eq!(ChdTrackType::Unknown("OTHER".into()).data_offset(), 0);
    }

    #[test]
    fn is_data_classifications() {
        assert!(ChdTrackType::Mode1Raw.is_data());
        assert!(ChdTrackType::Mode1Cooked.is_data());
        assert!(ChdTrackType::Mode2Raw.is_data());
        assert!(ChdTrackType::Mode2Form1.is_data());
        assert!(ChdTrackType::Mode2Form2.is_data());
        assert!(!ChdTrackType::Audio.is_data());
        assert!(ChdTrackType::Audio.is_audio());
        assert!(ChdTrackType::Unknown("MODE2".into()).is_data());
        assert!(!ChdTrackType::Unknown("OTHER".into()).is_data());
    }

    #[test]
    fn find_first_data_track_returns_first_data() {
        let info = ChdInfo {
            hunk_size: 16384,
            logical_size: 1024 * 1024,
            tracks: vec![
                ChdTrack {
                    track_no: 1,
                    track_type: ChdTrackType::Audio,
                    frames: 1000,
                    frame_offset: 0,
                },
                ChdTrack {
                    track_no: 2,
                    track_type: ChdTrackType::Mode1Raw,
                    frames: 5000,
                    frame_offset: 1000,
                },
            ],
        };
        let track = info.find_first_data_track().unwrap();
        assert_eq!(track.track_no, 2);
    }

    #[test]
    fn find_first_data_track_audio_only() {
        let info = ChdInfo {
            hunk_size: 16384,
            logical_size: 0,
            tracks: vec![ChdTrack {
                track_no: 1,
                track_type: ChdTrackType::Audio,
                frames: 1000,
                frame_offset: 0,
            }],
        };
        assert!(info.find_first_data_track().is_none());
    }

    #[cfg(feature = "chd")]
    #[test]
    fn open_chd_nonexistent_returns_io_error() {
        let err = open_chd("nonexistent_file_that_does_not_exist.chd").unwrap_err();
        assert!(matches!(err, OpticaldiscsError::Io(_)));
    }

    /// A hard-disk CHD must come back as an error, not take the process with it.
    ///
    /// `detect_format` keys CHD off the `MComprHD` magic, which every CHD carries
    /// regardless of media, so a hard-disk image reaches the CD code paths through
    /// the ordinary front door. Up to libchdman-rs 0.288.9 that **aborted the
    /// process**: MAME's `cdrom_file` constructor reports bad geometry by throwing
    /// a bare `nullptr`, and Rust frames cannot unwind a foreign exception. This
    /// test is the floor guard — against an older libchdman-rs it does not fail,
    /// it kills the test binary.
    #[cfg(feature = "chd")]
    #[test]
    fn hard_disk_chd_is_unsupported_not_an_abort() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("disk.img");
        // 64 KiB of zeros: a whole number of both 512-byte units and 4 KiB hunks.
        let mut f = std::fs::File::create(&raw).unwrap();
        f.write_all(&vec![0u8; 64 * 1024]).unwrap();
        f.flush().unwrap();
        drop(f);

        let chd_path = dir.path().join("disk.chd");
        libchdman_rs::hd::create_from_path(
            &raw,
            &chd_path,
            libchdman_rs::hd::HdCreateOptions::default(),
            &mut |_| {},
            &|| false,
        )
        .expect("could not build the hard-disk CHD fixture");

        // Confirm the fixture is what this test is about before asserting on it.
        let info = Chd::open(chd_path.to_str().unwrap(), false, None)
            .unwrap()
            .info()
            .unwrap();
        assert!(info.is_hd, "fixture should be hard-disk media");
        assert!(
            !info.is_cd && !info.is_gd,
            "fixture should not be CD/GD-ROM"
        );

        // Screened from the info record, so no `cdrom_file` is ever constructed.
        match open_chd(&chd_path) {
            Err(OpticaldiscsError::UnsupportedFormat(msg)) => {
                assert!(msg.contains("hard-disk"), "got: {msg}");
            }
            other => panic!("expected UnsupportedFormat, got {other:?}"),
        }

        // Same through the front door, which is how a consumer actually hits it.
        let err = crate::detect::DiscImageInfo::open(&chd_path).unwrap_err();
        assert!(
            matches!(err, OpticaldiscsError::UnsupportedFormat(_)),
            "expected UnsupportedFormat, got {err:?}"
        );
    }

    #[test]
    fn is_supported_tracks_the_feature() {
        assert_eq!(is_supported(), cfg!(feature = "chd"));
        assert_eq!(is_supported(), crate::DiscFormat::Chd.is_supported());
    }
}
