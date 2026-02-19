//! CHD (Compressed Hunks of Data) optical disc reading.
//!
//! Parses CHT2 track metadata from a CHD file to determine the track layout,
//! and exposes [`open_chd`] for extracting a [`ChdInfo`] (hunk size, logical
//! size, and parsed track list) without needing to decompress any sector data.
//!
//! Actual sector decompression is handled by
//! [`crate::sector_reader::ChdSectorReader`].

use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

use chd::metadata::MetadataTag;
use chd::Chd;

use crate::error::{OpticaldiscsError, Result};

/// CHT2 metadata tag: CD-ROM Track v2 (`b"CHT2"` = 0x43485432).
const CHT2_TAG: u32 = 0x43485432;

/// Byte size of one CHD CD-ROM frame: 2352-byte raw sector + 96-byte subcode.
pub const CHD_CD_FRAME_SIZE: u64 = 2448;

/// Byte offset to user data within a raw Mode 1 CD frame (skip sync + header).
pub const CHD_MODE1_DATA_OFFSET: u64 = 16;

// ── ChdTrackType ──────────────────────────────────────────────────────────────

/// Track type as encoded in a CHT2 metadata entry.
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
    fn from_str(s: &str) -> Self {
        match s {
            "MODE1_RAW" => ChdTrackType::Mode1Raw,
            "MODE1" => ChdTrackType::Mode1Cooked,
            "MODE2_RAW" => ChdTrackType::Mode2Raw,
            "MODE2_FORM1" => ChdTrackType::Mode2Form1,
            "MODE2_FORM2" => ChdTrackType::Mode2Form2,
            "AUDIO" => ChdTrackType::Audio,
            other => ChdTrackType::Unknown(other.to_string()),
        }
    }

    /// Byte offset to user data within a 2448-byte CHD frame for this track type.
    ///
    /// Raw modes store a full raw sector (sync + 4-byte header + data + ECC), so
    /// user data starts at byte 16.  All other types store data at byte 0.
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
        )
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
    /// Track list parsed from CHT2 metadata, sorted by track number.
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
/// Reads the CHT2 metadata tag from the CHD header to build a [`ChdInfo`]
/// containing the hunk size, logical size, and sorted track list.  No sector
/// data is decompressed.
///
/// # Errors
///
/// Returns an error if the file cannot be opened, the CHD header is invalid,
/// or the CHD format is not supported by the `chd` crate.
pub fn open_chd(path: impl AsRef<Path>) -> Result<ChdInfo> {
    let path = path.as_ref();
    let file = File::open(path).map_err(OpticaldiscsError::Io)?;
    let mut buf = BufReader::new(file);

    let mut chd = Chd::open(&mut buf, None)
        .map_err(|e| OpticaldiscsError::Chd(format!("failed to open CHD: {e:?}")))?;

    let hunk_size = chd.header().hunk_size();
    let logical_size = chd.header().logical_bytes();

    let tracks = parse_chd_tracks(&mut chd)?;
    log::debug!(
        "CHD opened: hunk_size={}, logical_size={}, tracks={}",
        hunk_size,
        logical_size,
        tracks.len()
    );

    Ok(ChdInfo {
        hunk_size,
        logical_size,
        tracks,
    })
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Parse CHT2 track metadata from an open CHD.
///
/// Collects all metadata refs matching the CHT2 tag in a first pass (to
/// release the mutable borrow on `chd`), then reads their content in a second
/// pass using [`Chd::inner`].  Results are sorted by track number and assigned
/// cumulative frame offsets.
fn parse_chd_tracks<F: Read + Seek>(chd: &mut Chd<F>) -> Result<Vec<ChdTrack>> {
    // First pass: collect MetadataRefs (they are owned values, no borrow held after collect)
    let meta_refs: Vec<_> = chd
        .metadata_refs()
        .filter(|r| r.metatag() == CHT2_TAG)
        .collect();

    let mut tracks: Vec<ChdTrack> = Vec::new();

    // Second pass: read each metadata entry through chd.inner()
    for meta_ref in meta_refs {
        match meta_ref.read(chd.inner()) {
            Ok(metadata) => {
                if let Ok(content) = String::from_utf8(metadata.value) {
                    log::trace!("CHT2 entry: {}", content.trim());
                    if let Some(track) = parse_cht2_entry(&content) {
                        tracks.push(track);
                    }
                }
            }
            Err(e) => {
                log::warn!("Failed to read CHT2 metadata entry: {e:?}");
            }
        }
    }

    // Sort by track number, then assign cumulative frame offsets
    tracks.sort_by_key(|t| t.track_no);
    let mut offset = 0u64;
    for track in &mut tracks {
        track.frame_offset = offset;
        offset += track.frames as u64;
    }

    Ok(tracks)
}

/// Parse a single CHT2 metadata entry string into a [`ChdTrack`].
///
/// CHT2 format (space-separated key:value pairs):
/// ```text
/// TRACK:1 TYPE:MODE1_RAW SUBTYPE:NONE FRAMES:27166 PREGAP:0 PGTYPE:MODE1_RAW PGSUB:NONE POSTGAP:0
/// ```
///
/// Returns `None` if `TRACK` is 0 or the string is unparseable.
fn parse_cht2_entry(content: &str) -> Option<ChdTrack> {
    let mut track_no = 0u32;
    let mut type_str = String::new();
    let mut frames = 0u32;

    for part in content.split_whitespace() {
        if let Some((key, value)) = part.split_once(':') {
            match key {
                "TRACK" => track_no = value.parse().unwrap_or(0),
                "TYPE" => type_str = value.to_string(),
                "FRAMES" => frames = value.parse().unwrap_or(0),
                _ => {} // SUBTYPE, PREGAP, PGTYPE, PGSUB, POSTGAP ignored for now
            }
        }
    }

    if track_no == 0 {
        return None;
    }

    Some(ChdTrack {
        track_no,
        track_type: ChdTrackType::from_str(&type_str),
        frames,
        frame_offset: 0, // recalculated in parse_chd_tracks
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cht2_mode1_raw() {
        let entry = "TRACK:1 TYPE:MODE1_RAW SUBTYPE:NONE FRAMES:27166 PREGAP:0 PGTYPE:MODE1_RAW PGSUB:NONE POSTGAP:0";
        let track = parse_cht2_entry(entry).unwrap();
        assert_eq!(track.track_no, 1);
        assert_eq!(track.track_type, ChdTrackType::Mode1Raw);
        assert_eq!(track.frames, 27166);
        assert!(track.is_data());
        assert!(!track.is_audio());
        assert_eq!(track.data_offset(), 16);
    }

    #[test]
    fn parse_cht2_audio() {
        let entry = "TRACK:2 TYPE:AUDIO SUBTYPE:NONE FRAMES:5000 PREGAP:150";
        let track = parse_cht2_entry(entry).unwrap();
        assert_eq!(track.track_no, 2);
        assert_eq!(track.track_type, ChdTrackType::Audio);
        assert_eq!(track.frames, 5000);
        assert!(track.is_audio());
        assert!(!track.is_data());
        assert_eq!(track.data_offset(), 0);
    }

    #[test]
    fn parse_cht2_mode1_cooked() {
        let entry = "TRACK:1 TYPE:MODE1 SUBTYPE:NONE FRAMES:10000";
        let track = parse_cht2_entry(entry).unwrap();
        assert_eq!(track.track_type, ChdTrackType::Mode1Cooked);
        assert_eq!(track.data_offset(), 0);
        assert!(track.is_data());
    }

    #[test]
    fn parse_cht2_mode2_form1() {
        let entry = "TRACK:1 TYPE:MODE2_FORM1 SUBTYPE:NONE FRAMES:30000";
        let track = parse_cht2_entry(entry).unwrap();
        assert_eq!(track.track_type, ChdTrackType::Mode2Form1);
        assert_eq!(track.data_offset(), 0);
        assert!(track.is_data());
    }

    #[test]
    fn parse_cht2_invalid_track_zero() {
        assert!(parse_cht2_entry("TRACK:0 TYPE:AUDIO FRAMES:1000").is_none());
    }

    #[test]
    fn parse_cht2_empty_string() {
        assert!(parse_cht2_entry("").is_none());
    }

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
    fn open_chd_nonexistent_returns_error() {
        let err = open_chd("nonexistent_file_that_does_not_exist.chd").unwrap_err();
        // Should be an IO error (file not found)
        assert!(matches!(err, OpticaldiscsError::Io(_)));
    }
}
