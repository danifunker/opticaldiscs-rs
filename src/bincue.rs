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

use crate::cue::{parse_cue, CueCommand};
use crate::error::{OpticaldiscsError, Result};
use crate::track::DiscTrack;

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

    /// True for Red Book audio tracks — the inverse of [`is_data`](Self::is_data).
    pub fn is_audio(self) -> bool {
        matches!(self, Self::Audio)
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

    /// Parse a CUE sheet `TRACK` format string, case-insensitively.
    ///
    /// The inverse of [`cue_label`](Self::cue_label) for the formats that
    /// round-trip exactly:
    ///
    /// ```
    /// use opticaldiscs::bincue::TrackType;
    ///
    /// assert_eq!(TrackType::from_cue_label("mode1/2352"), Some(TrackType::Mode1Raw));
    /// assert_eq!(TrackType::from_cue_label("AUDIO"), Some(TrackType::Audio));
    /// assert_eq!(TrackType::from_cue_label("nonsense"), None);
    /// ```
    pub fn from_cue_label(label: &str) -> Option<Self> {
        match label.to_ascii_uppercase().as_str() {
            // CD+G hides its graphics in the subchannel; the main channel is
            // ordinary 2352-byte audio, which is all this crate reads.
            "AUDIO" | "CDG" => Some(Self::Audio),
            "MODE1/2048" => Some(Self::Mode1Cooked),
            "MODE1/2352" => Some(Self::Mode1Raw),
            // Mode 2 stored in 2048-byte sectors is already cooked: the
            // subheader is gone and user data starts at offset 0.
            "MODE2/2048" => Some(Self::Mode1Cooked),
            "MODE2/2336" | "CDI/2336" => Some(Self::Mode2Form2),
            "MODE2/2352" | "CDI/2352" => Some(Self::Mode2Form1),
            // Any other MODEn/size — MODE2/2324 is the only one seen in the
            // wild — reads as cooked rather than failing the whole sheet.
            other if other.starts_with("MODE") => Some(Self::Mode1Cooked),
            _ => None,
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
    let commands = parse_cue(&content)?;

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
            CueCommand::File { name, .. } => {
                current_bin = Some(name.clone());
            }
            CueCommand::Track { number, format } => {
                let track_type = TrackType::from_cue_label(format).ok_or_else(|| {
                    OpticaldiscsError::Cue(format!(
                        "TRACK {number}: unknown track format {format:?}"
                    ))
                })?;
                raw.push(RawTrack {
                    track_no: *number,
                    track_type,
                    bin_filename: current_bin.clone().unwrap_or_else(|| "unknown.bin".into()),
                    index_01_frames: 0,
                });
            }
            CueCommand::Index { number: 1, time } => {
                if let Some(t) = raw.last_mut() {
                    t.index_01_frames = time.to_frames();
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

// ── Absolute disc geometry ────────────────────────────────────────────────────

/// Where one track sits on the disc, in absolute sectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackSpan {
    /// Absolute start sector of the track's `INDEX 01`, counted from the start
    /// of the disc (so track 1 of a normal disc is 0).
    pub start_lba: u64,
    /// Length of the track in sectors, measured `INDEX 01` to `INDEX 01` (or to
    /// the end of the file for the last track in one).
    ///
    /// Where the next track has an in-file pregap — an `INDEX 00` before its
    /// `INDEX 01` — that pregap counts toward this track rather than the next,
    /// which is the same convention the TOC's track offsets use.
    pub length_frames: u64,
}

/// Resolve each track's absolute position and length from a CUE/CCD track list.
///
/// A [`BinTrack`]'s `file_byte_offset` is its `INDEX 01` *relative to its own
/// BIN file*. For a single-BIN sheet every track shares one file, so those
/// offsets are already absolute disc positions. For a multi-FILE sheet (one BIN
/// per track, common in redump-style dumps) each offset is only the local pregap
/// of that file, so a running frame total has to be carried across files to
/// recover absolute, strictly-increasing positions.
///
/// Both layouts are handled uniformly by tracking the cumulative frame count of
/// all *previous* files and adding each track's local offset on top. A track's
/// length runs to the next track in the same file, or to the end of the file for
/// the last one.
///
/// Returns the spans and the lead-out position (total frames across every file),
/// or `None` if `tracks` is empty or a BIN file's size cannot be read.
pub fn absolute_track_spans(tracks: &[BinTrack]) -> Option<(Vec<TrackSpan>, u64)> {
    if tracks.is_empty() {
        return None;
    }

    // Absolute start of each track, and the absolute end of the file it lives in.
    let mut starts: Vec<u64> = Vec::with_capacity(tracks.len());
    let mut file_ends: Vec<u64> = Vec::with_capacity(tracks.len());

    // Frames in all files seen *before* the current one.
    let mut running_frames: u64 = 0;
    // Frames in the file the current track belongs to.
    let mut cur_file_frames: u64 = 0;
    let mut prev_bin: Option<&Path> = None;

    for t in tracks {
        let sector_size = t.sector_size();

        // When the BIN file changes, fold the previous file's full length into
        // the running total and measure the new file. Single-BIN sheets take
        // this branch exactly once (running_frames stays 0).
        if prev_bin != Some(t.bin_path.as_path()) {
            if prev_bin.is_some() {
                running_frames += cur_file_frames;
            }
            let file_len = std::fs::metadata(&t.bin_path).ok()?.len();
            cur_file_frames = file_len / sector_size;
            prev_bin = Some(t.bin_path.as_path());
        }

        starts.push(running_frames + t.file_byte_offset / sector_size);
        file_ends.push(running_frames + cur_file_frames);
    }

    let spans = starts
        .iter()
        .enumerate()
        .map(|(i, &start)| {
            // The next track bounds this one only when it is in the same file;
            // across a file boundary the file's own end is the bound.
            let end = match starts.get(i + 1) {
                Some(&next) if tracks[i + 1].bin_path == tracks[i].bin_path => next,
                _ => file_ends[i],
            };
            TrackSpan {
                start_lba: start,
                length_frames: end.saturating_sub(start),
            }
        })
        .collect();

    Some((spans, running_frames + cur_file_frames))
}

/// Describe a CUE/CCD track list as [`DiscTrack`]s for
/// [`DiscImageInfo::tracks`](crate::detect::DiscImageInfo::tracks).
///
/// Falls back to per-file offsets with unknown lengths when a BIN file's size
/// cannot be read, so a listing is still produced for a partially-present set of
/// track files.
pub fn disc_tracks(tracks: &[BinTrack]) -> Vec<DiscTrack> {
    let spans = absolute_track_spans(tracks).map(|(spans, _lead_out)| spans);

    tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let span = spans.as_ref().map(|s| s[i]);
            DiscTrack {
                number: t.track_no,
                track_type: t.track_type,
                start_lba: span.map_or(t.file_byte_offset / t.sector_size(), |s| s.start_lba),
                length_sectors: span.map_or(0, |s| s.length_frames),
            }
        })
        .collect()
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
    fn track_type_from_cue_label() {
        let t = TrackType::from_cue_label;
        assert_eq!(t("AUDIO"), Some(TrackType::Audio));
        assert_eq!(t("CDG"), Some(TrackType::Audio));
        assert_eq!(t("MODE1/2352"), Some(TrackType::Mode1Raw));
        assert_eq!(t("MODE1/2048"), Some(TrackType::Mode1Cooked));
        assert_eq!(t("MODE2/2352"), Some(TrackType::Mode2Form1));
        assert_eq!(t("MODE2/2336"), Some(TrackType::Mode2Form2));
        assert_eq!(t("MODE2/2048"), Some(TrackType::Mode1Cooked));
        // A CD-i track's sector size decides its shape; both are Mode 2.
        assert_eq!(t("CDI/2352"), Some(TrackType::Mode2Form1));
        assert_eq!(t("CDI/2336"), Some(TrackType::Mode2Form2));
        // Unlisted MODEn/size falls back rather than failing the sheet.
        assert_eq!(t("MODE2/2324"), Some(TrackType::Mode1Cooked));
        assert_eq!(t("nonsense"), None);
    }

    #[test]
    fn every_cue_label_round_trips() {
        for tt in [
            TrackType::Audio,
            TrackType::Mode1Raw,
            TrackType::Mode1Cooked,
            TrackType::Mode2Form1,
            TrackType::Mode2Form2,
        ] {
            assert_eq!(TrackType::from_cue_label(tt.cue_label()), Some(tt));
        }
    }

    #[test]
    fn audio_and_data_are_complementary() {
        for tt in [
            TrackType::Audio,
            TrackType::Mode1Raw,
            TrackType::Mode1Cooked,
            TrackType::Mode2Form1,
            TrackType::Mode2Form2,
        ] {
            assert_ne!(tt.is_audio(), tt.is_data(), "{tt:?}");
        }
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

    /// Write `cue_body` and a `sectors`-long dummy BIN into a fresh temp dir.
    fn cue_with_bin(
        bin_name: &str,
        sectors: usize,
        cue_body: &str,
    ) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(bin_name), vec![0u8; 2352 * sectors]).unwrap();
        let cue_path = dir.path().join("disc.cue");
        std::fs::write(&cue_path, cue_body).unwrap();
        (dir, cue_path)
    }

    #[test]
    fn parse_single_bin_cue() {
        let (_d, cue_path) = cue_with_bin(
            "disc.bin",
            20,
            "FILE \"disc.bin\" BINARY\n\
             TRACK 01 MODE1/2352\n\
               INDEX 01 00:00:00\n",
        );

        let tracks = parse_cue_tracks(&cue_path).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].track_no, 1);
        assert_eq!(tracks[0].track_type, TrackType::Mode1Raw);
        assert_eq!(tracks[0].file_byte_offset, 0);
        assert!(tracks[0].is_data());
    }

    #[test]
    fn unpadded_numbers_parse_identically() {
        // Retail pressings ship sheets written this way (Microsoft Bookshelf).
        let body = |track: &str, index: &str| {
            format!("FILE \"disc.bin\" BINARY\n   TRACK {track} MODE1/2352\n   INDEX {index} 00:00:00\n")
        };
        let (_d1, padded) = cue_with_bin("disc.bin", 20, &body("01", "01"));
        let (_d2, unpadded) = cue_with_bin("disc.bin", 20, &body("1", "1"));

        let a = parse_cue_tracks(&padded).unwrap();
        let b = parse_cue_tracks(&unpadded).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].track_no, b[0].track_no);
        assert_eq!(a[0].track_type, b[0].track_type);
        assert_eq!(a[0].file_byte_offset, b[0].file_byte_offset);
    }

    #[test]
    fn unpadded_msf_parses() {
        let (_d, cue_path) = cue_with_bin(
            "disc.bin",
            5000,
            "FILE \"disc.bin\" BINARY\nTRACK 1 AUDIO\nINDEX 1 1:0:0\n",
        );
        let tracks = parse_cue_tracks(&cue_path).unwrap();
        assert_eq!(tracks[0].file_byte_offset, 4500 * 2352);
    }

    #[test]
    fn catalog_and_cd_text_do_not_break_the_sheet() {
        let (_d, cue_path) = cue_with_bin(
            "disc.bin",
            20,
            "CATALOG 0000000000000\n\
             REM GENRE Alternative Rock\n\
             PERFORMER \"An Artist\"\n\
             FILE \"disc.bin\" BINARY\n\
             TRACK 01 MODE1/2352\n\
               INDEX 01 00:00:00\n",
        );
        let tracks = parse_cue_tracks(&cue_path).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].track_type, TrackType::Mode1Raw);
    }

    #[test]
    fn unknown_track_format_is_reported() {
        let (_d, cue_path) = cue_with_bin(
            "disc.bin",
            20,
            "FILE \"disc.bin\" BINARY\nTRACK 01 SOMETHING\nINDEX 01 00:00:00\n",
        );
        let err = parse_cue_tracks(&cue_path).unwrap_err().to_string();
        assert!(err.contains("SOMETHING"), "{err}");
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
    fn spans_of_a_single_bin_chain_end_to_end() {
        let (_d, cue_path) = cue_with_bin(
            "disc.bin",
            10_000,
            "FILE \"disc.bin\" BINARY\n\
             TRACK 01 MODE1/2352\n  INDEX 01 00:00:00\n\
             TRACK 02 AUDIO\n  INDEX 01 01:00:00\n",
        );
        let tracks = parse_cue_tracks(&cue_path).unwrap();
        let (spans, lead_out) = absolute_track_spans(&tracks).unwrap();

        assert_eq!(spans[0].start_lba, 0);
        assert_eq!(spans[0].length_frames, 4500);
        assert_eq!(spans[1].start_lba, 4500);
        // Track 2 runs to the end of the file.
        assert_eq!(spans[1].length_frames, 10_000 - 4500);
        assert_eq!(lead_out, 10_000);
    }

    #[test]
    fn spans_accumulate_across_per_track_bins() {
        let dir = tempfile::tempdir().unwrap();
        let ss = TrackType::Audio.sector_size();
        for (name, frames) in [("t1.bin", 200u64), ("t2.bin", 150), ("t3.bin", 100)] {
            std::fs::write(dir.path().join(name), vec![0u8; (frames * ss) as usize]).unwrap();
        }
        let cue_path = dir.path().join("disc.cue");
        std::fs::write(
            &cue_path,
            "FILE \"t1.bin\" BINARY\nTRACK 01 AUDIO\n  INDEX 01 00:00:00\n\
             FILE \"t2.bin\" BINARY\nTRACK 02 AUDIO\n  INDEX 01 00:00:00\n\
             FILE \"t3.bin\" BINARY\nTRACK 03 AUDIO\n  INDEX 01 00:00:00\n",
        )
        .unwrap();

        let tracks = parse_cue_tracks(&cue_path).unwrap();
        let (spans, lead_out) = absolute_track_spans(&tracks).unwrap();

        // Each file's INDEX 01 restarts at zero, so positions must accumulate.
        assert_eq!(
            spans.iter().map(|s| s.start_lba).collect::<Vec<_>>(),
            vec![0, 200, 350]
        );
        assert_eq!(
            spans.iter().map(|s| s.length_frames).collect::<Vec<_>>(),
            vec![200, 150, 100]
        );
        assert_eq!(lead_out, 450);
    }

    #[test]
    fn disc_tracks_describe_a_mixed_mode_disc() {
        let (_d, cue_path) = cue_with_bin(
            "disc.bin",
            10_000,
            "FILE \"disc.bin\" BINARY\n\
             TRACK 01 MODE1/2352\n  INDEX 01 00:00:00\n\
             TRACK 02 AUDIO\n  INDEX 01 01:00:00\n",
        );
        let listed = disc_tracks(&parse_cue_tracks(&cue_path).unwrap());

        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].number, 1);
        assert!(listed[0].is_data());
        assert_eq!(listed[0].cue_label(), "MODE1/2352");
        assert_eq!(listed[1].number, 2);
        assert!(listed[1].is_audio());
        assert_eq!(listed[1].start_msf(), (1, 0, 0));
        assert_eq!(listed[1].duration_msf(), Some((1, 13, 25)));
    }

    #[test]
    fn disc_tracks_survive_a_missing_bin_measurement() {
        // A track list assembled by hand (no file on disk) still lists, with
        // per-file offsets and unknown lengths rather than nothing at all.
        let listed = disc_tracks(&[BinTrack {
            track_no: 1,
            track_type: TrackType::Audio,
            bin_path: PathBuf::from("/nonexistent/audio.bin"),
            file_byte_offset: 4500 * 2352,
            frame_count: 0,
        }]);
        assert_eq!(listed[0].start_lba, 4500);
        assert_eq!(listed[0].length_sectors, 0);
        assert_eq!(listed[0].duration_msf(), None);
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
