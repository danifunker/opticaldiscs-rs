//! CHD (Compressed Hunks of Data) optical disc reading.
//!
//! Thin wrapper around [`libchdman_rs`] — MAME's `chd_file` core via Rust
//! bindings — that exposes the track metadata needed by the rest of this
//! crate without leaking the upstream types into `opticaldiscs`'s public API.
//!
//! Actual sector decompression is handled by
//! [`crate::sector_reader::ChdSectorReader`].

use std::path::Path;

use libchdman_rs::cd::{list_tracks, TrackType as LibTrackType};
use libchdman_rs::Chd;

use crate::error::{OpticaldiscsError, Result};

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
/// Created by [`open_chd`]; does not perform sector decompression.
/// Use [`crate::sector_reader::ChdSectorReader`] to read actual sector data.
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
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Open a CHD file and parse its track metadata.
///
/// Reads the CHD header and track list via [`libchdman_rs`]; no sector data
/// is decompressed.
///
/// # Errors
///
/// Returns an [`OpticaldiscsError::Io`] if the path does not exist or cannot
/// be read; [`OpticaldiscsError::Chd`] for any libchdman-rs failure (invalid
/// header, unsupported format, etc.).
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

    let lib_tracks =
        list_tracks(&chd).map_err(|e| OpticaldiscsError::Chd(format!("list CHD tracks: {e:?}")))?;

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

    #[test]
    fn open_chd_nonexistent_returns_io_error() {
        let err = open_chd("nonexistent_file_that_does_not_exist.chd").unwrap_err();
        assert!(matches!(err, OpticaldiscsError::Io(_)));
    }
}
