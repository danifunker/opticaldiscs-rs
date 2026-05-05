//! BIN/CUE disc image reading and single-BIN CUE generation.
//!
//! A CUE sheet (`.cue`) describes the track layout of a raw binary disc image
//! (`.bin`).  Tracks can be stored in a single BIN file (common for ripped
//! disc images) or in separate per-track BIN files (common for discs with
//! mixed audio and data tracks).
//!
//! This module provides:
//! - [`parse_cue_tracks`] — parse a CUE file into a [`Vec<BinTrack>`]
//! - [`BinCueSectorReader`](crate::sector_reader::BinCueSectorReader) — in
//!   `sector_reader.rs`
//! - [`write_single_bin_cue`] — merge multi-file BINs into one BIN + CUE

use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use cue_sheet::parser::{parse_cue, Command, TrackType as CueTrackType};

use crate::error::{OpticaldiscsError, Result};

// ── Track type ────────────────────────────────────────────────────────────────

/// The data format of a single CD track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackType {
    /// Audio track — 2352 raw bytes per sector, no data header.
    Audio,
    /// Data Mode 1, raw — 2352 bytes/sector, user data at offset 16.
    Mode1Raw,
    /// Data Mode 1, cooked — 2048 bytes/sector, user data at offset 0.
    Mode1Cooked,
    /// Data Mode 2 Form 1, raw — 2352 bytes/sector, user data at offset 24.
    Mode2Form1,
    /// Data Mode 2 Form 2 / XA — 2336 bytes/sector, user data at offset 8.
    Mode2Form2,
}

impl TrackType {
    /// Physical bytes per sector in the BIN file.
    pub fn sector_size(self) -> u64 {
        match self {
            Self::Audio | Self::Mode1Raw | Self::Mode2Form1 => 2352,
            Self::Mode1Cooked => 2048,
            Self::Mode2Form2 => 2336,
        }
    }

    /// Byte offset within a physical sector to the start of user data.
    pub fn data_offset(self) -> u64 {
        match self {
            Self::Audio | Self::Mode1Cooked => 0,
            Self::Mode1Raw => 16,
            // Mode 2 Form 1 and Form 2: 24-byte header in 2352-byte raw sectors
            Self::Mode2Form1 => 24,
            // Mode 2 Form 2 in 2336-byte BIN sectors: 8-byte subheader before data
            Self::Mode2Form2 => 8,
        }
    }

    /// True for data tracks (ISO 9660 / HFS etc.), false for audio.
    pub fn is_data(self) -> bool {
        !matches!(self, Self::Audio)
    }

    /// CUE sheet format string for this track type (e.g. `"MODE1/2352"`).
    pub fn cue_label(self) -> &'static str {
        match self {
            Self::Audio => "AUDIO",
            Self::Mode1Raw => "MODE1/2352",
            Self::Mode1Cooked => "MODE1/2048",
            Self::Mode2Form1 => "MODE2/2352",
            Self::Mode2Form2 => "MODE2/2336",
        }
    }

    /// Map from `cue_sheet` crate's `TrackType` to ours.
    fn from_cue(ct: &CueTrackType) -> Self {
        match ct {
            CueTrackType::Audio | CueTrackType::Cdg => Self::Audio,
            CueTrackType::Mode(1, 2352) => Self::Mode1Raw,
            CueTrackType::Mode(1, _) => Self::Mode1Cooked,
            CueTrackType::Mode(2, 2336) => Self::Mode2Form2,
            CueTrackType::Mode(2, _) => Self::Mode2Form1,
            CueTrackType::Mode(_, _) => Self::Mode1Cooked, // fallback
            CueTrackType::Cdi(_) => Self::Mode2Form1,
        }
    }
}

// ── BinTrack ──────────────────────────────────────────────────────────────────

/// A single track parsed from a CUE sheet, with resolved BIN path and
/// byte-level addressing information ready for the sector reader.
#[derive(Debug, Clone)]
pub struct BinTrack {
    /// 1-based track number.
    pub track_no: u32,
    /// Track format.
    pub track_type: TrackType,
    /// Path to the BIN file that holds this track's data.
    pub bin_path: PathBuf,
    /// Byte offset in the BIN file where this track's sectors start
    /// (= INDEX 01 frame number × `track_type.sector_size()`).
    pub file_byte_offset: u64,
    /// Number of frames (sectors) in this track; 0 means unknown.
    pub frame_count: u64,
}

impl BinTrack {
    /// Physical bytes per sector.
    pub fn sector_size(&self) -> u64 {
        self.track_type.sector_size()
    }
    /// Offset within each physical sector to user data.
    pub fn data_offset(&self) -> u64 {
        self.track_type.data_offset()
    }
    /// True for data tracks.
    pub fn is_data(&self) -> bool {
        self.track_type.is_data()
    }
}

// ── CUE parsing ───────────────────────────────────────────────────────────────

/// Parse a CUE file and return all tracks with resolved BIN paths.
///
/// The returned `Vec` preserves track order. Call
/// `tracks.iter().find(|t| t.is_data())` to get the first data track.
pub fn parse_cue_tracks(cue_path: &Path) -> Result<Vec<BinTrack>> {
    let content = fs::read_to_string(cue_path).map_err(OpticaldiscsError::Io)?;
    let normalized = normalize_cue_keywords(&content);

    let commands = parse_cue(&normalized).map_err(|e| OpticaldiscsError::Cue(format!("{e:?}")))?;

    let cue_dir = cue_path.parent().unwrap_or(Path::new("."));

    // ── First pass: collect raw data from CUE commands ────────────────────
    struct RawTrack {
        track_no: u32,
        track_type: TrackType,
        bin_filename: String,
        index_01_frames: u64, // MSF → frames
    }

    let mut current_bin: Option<String> = None;
    let mut raw: Vec<RawTrack> = Vec::new();

    for cmd in &commands {
        match cmd {
            Command::File(name, _fmt) => {
                current_bin = Some(name.clone());
            }
            Command::Track(no, ct) => {
                raw.push(RawTrack {
                    track_no: *no,
                    track_type: TrackType::from_cue(ct),
                    bin_filename: current_bin.clone().unwrap_or_else(|| "unknown.bin".into()),
                    index_01_frames: 0,
                });
            }
            Command::Index(idx_no, msf) if *idx_no == 1 => {
                if let Some(t) = raw.last_mut() {
                    t.index_01_frames = msf_to_frames(
                        msf.minutes() as u8,
                        msf.seconds() as u8,
                        msf.frames() as u8,
                    );
                }
            }
            _ => {}
        }
    }

    if raw.is_empty() {
        return Err(OpticaldiscsError::Cue(
            "no TRACK entries found in CUE sheet".into(),
        ));
    }

    // ── Second pass: resolve BIN paths and compute frame counts ───────────
    let mut tracks: Vec<BinTrack> = Vec::with_capacity(raw.len());

    for (i, rt) in raw.iter().enumerate() {
        let bin_path = resolve_bin_path(cue_dir, &rt.bin_filename, cue_path)?;
        let file_byte_offset = rt.index_01_frames * rt.track_type.sector_size();

        // Frame count = (next track's INDEX 01 - this track's INDEX 01)
        // when both tracks share the same BIN file; otherwise 0 (unknown).
        let frame_count = raw.get(i + 1).map_or(0, |next| {
            if next.bin_filename == rt.bin_filename && next.index_01_frames > rt.index_01_frames {
                next.index_01_frames - rt.index_01_frames
            } else {
                0
            }
        });

        tracks.push(BinTrack {
            track_no: rt.track_no,
            track_type: rt.track_type,
            bin_path,
            file_byte_offset,
            frame_count,
        });
    }

    Ok(tracks)
}

// ── Single-BIN writer ─────────────────────────────────────────────────────────

/// Merge one or more per-track BIN files into a single BIN and generate a
/// matching CUE sheet.
///
/// All tracks are copied in order.  The output CUE references a single FILE
/// entry and recalculates each track's INDEX 01 position from the running
/// byte offset so they are correct for the new concatenated file.
///
/// `out_bin_name` is the bare filename (no directory) used in the CUE FILE
/// directive — it must match the actual filename of `out_bin`.
pub fn write_single_bin_cue(
    tracks: &[BinTrack],
    out_bin: &Path,
    out_cue: &Path,
    out_bin_name: &str,
) -> Result<()> {
    if tracks.is_empty() {
        return Err(OpticaldiscsError::Cue("no tracks to write".into()));
    }

    // ── Write concatenated BIN ─────────────────────────────────────────────
    let mut writer = BufWriter::new(File::create(out_bin).map_err(OpticaldiscsError::Io)?);

    // Track the running frame offset as we append each track.
    let mut running_frames: Vec<u64> = Vec::with_capacity(tracks.len());
    let mut current_frame: u64 = 0;

    for track in tracks {
        running_frames.push(current_frame);

        // Compute the number of bytes to copy for this track.
        let bin_len = track
            .bin_path
            .metadata()
            .map_err(OpticaldiscsError::Io)?
            .len();

        // If the track has a known file_byte_offset (non-zero, single-BIN
        // source), we only copy from that offset onward.
        let copy_start = track.file_byte_offset;
        let copy_len = bin_len.saturating_sub(copy_start);

        if copy_len == 0 {
            running_frames.push(current_frame); // nothing written
            continue;
        }

        let mut src = File::open(&track.bin_path).map_err(OpticaldiscsError::Io)?;
        if copy_start > 0 {
            use std::io::Seek;
            src.seek(std::io::SeekFrom::Start(copy_start))
                .map_err(OpticaldiscsError::Io)?;
        }

        let mut buf = [0u8; 65536];
        let mut remaining = copy_len;
        while remaining > 0 {
            let to_read = (remaining as usize).min(buf.len());
            let n = src
                .read(&mut buf[..to_read])
                .map_err(OpticaldiscsError::Io)?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n]).map_err(OpticaldiscsError::Io)?;
            remaining -= n as u64;
        }

        let frames_written = copy_len / track.sector_size();
        current_frame += frames_written;
    }

    writer.flush().map_err(OpticaldiscsError::Io)?;
    drop(writer);

    // ── Write CUE sheet ────────────────────────────────────────────────────
    let mut cue = BufWriter::new(File::create(out_cue).map_err(OpticaldiscsError::Io)?);
    writeln!(cue, "FILE \"{}\" BINARY", out_bin_name).map_err(OpticaldiscsError::Io)?;

    for (track, &frame_offset) in tracks.iter().zip(running_frames.iter()) {
        writeln!(
            cue,
            "  TRACK {:02} {}",
            track.track_no,
            track.track_type.cue_label()
        )
        .map_err(OpticaldiscsError::Io)?;
        let (mm, ss, ff) = frames_to_msf(frame_offset);
        writeln!(cue, "    INDEX 01 {:02}:{:02}:{:02}", mm, ss, ff)
            .map_err(OpticaldiscsError::Io)?;
    }

    cue.flush().map_err(OpticaldiscsError::Io)?;
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Convert an MM:SS:FF MSF triple to a frame count.
/// `frames = MM * 60 * 75 + SS * 75 + FF`
pub fn msf_to_frames(mm: u8, ss: u8, ff: u8) -> u64 {
    mm as u64 * 60 * 75 + ss as u64 * 75 + ff as u64
}

/// Convert a frame count to `(MM, SS, FF)`.
pub fn frames_to_msf(frames: u64) -> (u8, u8, u8) {
    let ff = (frames % 75) as u8;
    let total_secs = frames / 75;
    let ss = (total_secs % 60) as u8;
    let mm = (total_secs / 60) as u8;
    (mm, ss, ff)
}

/// Fix case-sensitivity and compatibility issues before handing to cue_sheet.
///
/// The `cue_sheet` crate requires `Binary` (not `BINARY`) and `Wave` (not
/// `WAVE`).  It also chokes on `CATALOG` lines with leading zeros so we strip
/// those.
pub(crate) fn normalize_cue_keywords(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        let trimmed = line.trim();
        // CATALOG lines cause parser issues; they are optional
        if trimmed.starts_with("CATALOG") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.replace("BINARY", "Binary")
        .replace("MOTOROLA", "Motorola")
        .replace(" WAVE", " Wave")
        .replace(" MP3", " Mp3")
        .replace(" AIFF", " Aiff")
}

/// Resolve the BIN file path referenced in a CUE sheet.
///
/// Tries (in order):
/// 1. The filename as-is relative to the CUE directory
/// 2. Just the base filename in the CUE directory
/// 3. Common extensions (`bin`, `BIN`, `img`, `IMG`) with the same stem
/// 4. The CUE file's own stem with the same extensions
fn resolve_bin_path(cue_dir: &Path, bin_filename: &str, cue_path: &Path) -> Result<PathBuf> {
    let candidates: Vec<PathBuf> = {
        let mut v = vec![
            cue_dir.join(bin_filename),
            cue_dir.join(Path::new(bin_filename).file_name().unwrap_or_default()),
        ];

        // Same stem, different extension
        let stem = Path::new(bin_filename)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        for ext in ["bin", "BIN", "img", "IMG"] {
            v.push(cue_dir.join(format!("{stem}.{ext}")));
        }

        // CUE's own stem
        if let Some(cue_stem) = cue_path.file_stem() {
            let s = cue_stem.to_string_lossy();
            for ext in ["bin", "BIN", "img", "IMG"] {
                v.push(cue_dir.join(format!("{s}.{ext}")));
            }
        }
        v
    };

    candidates.into_iter().find(|p| p.exists()).ok_or_else(|| {
        OpticaldiscsError::Cue(format!(
            "BIN file not found: '{}' (relative to {})",
            bin_filename,
            cue_dir.display()
        ))
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_type_params() {
        assert_eq!(TrackType::Audio.sector_size(), 2352);
        assert_eq!(TrackType::Audio.data_offset(), 0);
        assert!(!TrackType::Audio.is_data());

        assert_eq!(TrackType::Mode1Raw.sector_size(), 2352);
        assert_eq!(TrackType::Mode1Raw.data_offset(), 16);
        assert!(TrackType::Mode1Raw.is_data());

        assert_eq!(TrackType::Mode1Cooked.sector_size(), 2048);
        assert_eq!(TrackType::Mode1Cooked.data_offset(), 0);
        assert!(TrackType::Mode1Cooked.is_data());

        assert_eq!(TrackType::Mode2Form1.sector_size(), 2352);
        assert_eq!(TrackType::Mode2Form1.data_offset(), 24);

        assert_eq!(TrackType::Mode2Form2.sector_size(), 2336);
        assert_eq!(TrackType::Mode2Form2.data_offset(), 8);
    }

    #[test]
    fn track_type_from_cue() {
        assert_eq!(TrackType::from_cue(&CueTrackType::Audio), TrackType::Audio);
        assert_eq!(
            TrackType::from_cue(&CueTrackType::Mode(1, 2352)),
            TrackType::Mode1Raw
        );
        assert_eq!(
            TrackType::from_cue(&CueTrackType::Mode(1, 2048)),
            TrackType::Mode1Cooked
        );
        assert_eq!(
            TrackType::from_cue(&CueTrackType::Mode(2, 2352)),
            TrackType::Mode2Form1
        );
        assert_eq!(
            TrackType::from_cue(&CueTrackType::Mode(2, 2336)),
            TrackType::Mode2Form2
        );
    }

    #[test]
    fn msf_roundtrip() {
        // 01:23:45 → frames → back to MSF
        let frames = msf_to_frames(1, 23, 45);
        assert_eq!(frames, 1 * 60 * 75 + 23 * 75 + 45);
        let (mm, ss, ff) = frames_to_msf(frames);
        assert_eq!((mm, ss, ff), (1, 23, 45));
    }

    #[test]
    fn msf_zero() {
        assert_eq!(msf_to_frames(0, 0, 0), 0);
        assert_eq!(frames_to_msf(0), (0, 0, 0));
    }

    #[test]
    fn normalize_removes_catalog() {
        let cue = "CATALOG 0000000000000\nFILE \"a.bin\" BINARY\n";
        let n = normalize_cue_keywords(cue);
        assert!(!n.contains("CATALOG"));
        assert!(n.contains("FILE"));
    }

    #[test]
    fn normalize_fixes_binary_case() {
        let n = normalize_cue_keywords("FILE \"a.bin\" BINARY\n");
        assert!(n.contains("Binary"));
        assert!(!n.contains("BINARY"));
    }

    #[test]
    fn parse_single_bin_cue() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        // Create a dummy BIN file (just needs to exist)
        let bin_path = dir.path().join("disc.bin");
        let cue_path = dir.path().join("disc.cue");
        std::fs::write(&bin_path, vec![0u8; 2352 * 20]).unwrap();

        let cue_content = format!(
            "FILE \"disc.bin\" BINARY\n\
             TRACK 01 MODE1/2352\n\
               INDEX 01 00:00:00\n"
        );
        let mut f = File::create(&cue_path).unwrap();
        f.write_all(cue_content.as_bytes()).unwrap();

        let tracks = parse_cue_tracks(&cue_path).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].track_no, 1);
        assert_eq!(tracks[0].track_type, TrackType::Mode1Raw);
        assert_eq!(tracks[0].file_byte_offset, 0);
        assert!(tracks[0].is_data());
    }

    #[test]
    fn parse_mixed_mode_cue() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        // Two-track disc: data + audio in one BIN
        let bin_size = 2352 * 100;
        std::fs::write(dir.path().join("disc.bin"), vec![0u8; bin_size]).unwrap();

        let cue_path = dir.path().join("disc.cue");
        let cue_content = "FILE \"disc.bin\" BINARY\n\
             TRACK 01 MODE1/2352\n\
               INDEX 01 00:00:00\n\
             TRACK 02 AUDIO\n\
               INDEX 01 01:00:00\n";
        let mut f = File::create(&cue_path).unwrap();
        f.write_all(cue_content.as_bytes()).unwrap();

        let tracks = parse_cue_tracks(&cue_path).unwrap();
        assert_eq!(tracks.len(), 2);
        assert!(tracks[0].is_data());
        assert!(!tracks[1].is_data());
        // Track 2 INDEX 01 = 01:00:00 = 75*60 = 4500 frames → 4500 * 2352 bytes
        assert_eq!(tracks[1].file_byte_offset, 4500 * 2352);
    }

    #[test]
    fn write_single_bin_cue_roundtrip() {
        let dir = tempfile::tempdir().unwrap();

        // Two small per-track BIN files
        let bin1 = dir.path().join("track01.bin");
        let bin2 = dir.path().join("track02.bin");
        std::fs::write(&bin1, vec![0xAAu8; 2352 * 10]).unwrap();
        std::fs::write(&bin2, vec![0xBBu8; 2352 * 5]).unwrap();

        let tracks = vec![
            BinTrack {
                track_no: 1,
                track_type: TrackType::Mode1Raw,
                bin_path: bin1,
                file_byte_offset: 0,
                frame_count: 10,
            },
            BinTrack {
                track_no: 2,
                track_type: TrackType::Audio,
                bin_path: bin2,
                file_byte_offset: 0,
                frame_count: 5,
            },
        ];

        let out_bin = dir.path().join("merged.bin");
        let out_cue = dir.path().join("merged.cue");
        write_single_bin_cue(&tracks, &out_bin, &out_cue, "merged.bin").unwrap();

        // Merged BIN should be 15 sectors * 2352 bytes
        assert_eq!(out_bin.metadata().unwrap().len(), 15 * 2352);

        // CUE should reference correct INDEX positions
        let cue_text = std::fs::read_to_string(&out_cue).unwrap();
        assert!(cue_text.contains("FILE \"merged.bin\" BINARY"));
        assert!(cue_text.contains("TRACK 01 MODE1/2352"));
        assert!(cue_text.contains("INDEX 01 00:00:00"));
        assert!(cue_text.contains("TRACK 02 AUDIO"));
        // Track 2 starts at frame 10 = 00:00:10
        assert!(cue_text.contains("INDEX 01 00:00:10"));
    }
}
