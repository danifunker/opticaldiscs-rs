//! Dreamcast GD-ROM `.gdi` track-descriptor parsing and reading.
//!
//! A `.gdi` file is a small plain-text descriptor listing the disc's tracks and
//! the per-track binary files that hold their sectors — structurally similar to
//! a `.cue` sheet. The first line is the track count; each subsequent line is:
//!
//! ```text
//! <track#> <start_lba> <type> <sector_size> <filename> <offset>
//! ```
//!
//! where `type` is `4` for a data track or `0` for audio. A GD-ROM's
//! high-density game area begins at absolute LBA [`GDROM_HD_START_LBA`] (45000,
//! track 3) and carries the main ISO 9660 filesystem — but it can span several
//! data tracks (separated by audio tracks), so file data may live in track 3,
//! track 5, and beyond.
//!
//! [`open_gdi_hd_reader`] parses the descriptor, opens every high-density data
//! track's file, and returns a [`SectorReader`] wrapped in [`GdromSectorReader`]
//! so the standard ISO 9660 browser reads the whole HD-area filesystem directly.
//!
//! Reference: `docs/GameDiscs_Implementation.md` (Phase G3), dreamcast.wiki.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::{OpticaldiscsError, Result};
use crate::sector_reader::{
    GdromSectorReader, SectorReader, GDROM_HD_START_LBA, MODE1_DATA_OFFSET, RAW_SECTOR_SIZE,
    SECTOR_SIZE,
};

/// One track described by a `.gdi` file.
#[derive(Debug, Clone)]
pub struct GdiTrack {
    /// 1-based track number.
    pub number: u32,
    /// Absolute Logical Block Address where this track begins.
    pub start_lba: u64,
    /// GDI track type: `4` = data, `0` = audio.
    pub track_type: u32,
    /// Physical bytes per sector in the track file (2352 raw, or 2048 cooked).
    pub sector_size: u32,
    /// Track binary file name, relative to the `.gdi` file's directory.
    pub filename: String,
    /// Byte offset within the file where the track data starts (usually 0).
    pub offset: u64,
}

impl GdiTrack {
    /// Returns `true` for a data track (type 4).
    pub fn is_data(&self) -> bool {
        self.track_type == 4
    }
}

/// Parse a `.gdi` descriptor into its track list.
///
/// # Errors
///
/// Returns [`OpticaldiscsError::Parse`] if the file is empty or a track line is
/// malformed, or [`OpticaldiscsError::Io`] if it cannot be read.
pub fn parse_gdi(path: &Path) -> Result<Vec<GdiTrack>> {
    let text = std::fs::read_to_string(path).map_err(OpticaldiscsError::Io)?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());

    let count: usize = lines
        .next()
        .and_then(|l| l.trim().parse().ok())
        .ok_or_else(|| OpticaldiscsError::Parse("empty or invalid .gdi header".into()))?;

    let mut tracks = Vec::with_capacity(count);
    for line in lines {
        // Fields: num start_lba type sector_size filename [offset]
        // The filename may be quoted (it can contain spaces).
        if let Some(track) = parse_gdi_line(line) {
            tracks.push(track);
        }
    }

    if tracks.is_empty() {
        return Err(OpticaldiscsError::Parse("no tracks in .gdi".into()));
    }
    Ok(tracks)
}

/// Parse one track line. Returns `None` if the line does not have the required
/// leading numeric fields.
fn parse_gdi_line(line: &str) -> Option<GdiTrack> {
    let line = line.trim();
    // The first four fields are numeric; the filename can be quoted and may
    // contain spaces; a trailing numeric offset follows it.
    let (number, rest) = take_uint(line)?;
    let (start_lba, rest) = take_uint(rest)?;
    let (track_type, rest) = take_uint(rest)?;
    let (sector_size, rest) = take_uint(rest)?;
    let rest = rest.trim_start();

    let (filename, after) = if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        (stripped[..end].to_string(), &stripped[end + 1..])
    } else {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        (rest[..end].to_string(), &rest[end..])
    };

    let offset = after.trim().parse::<u64>().unwrap_or(0);

    Some(GdiTrack {
        number: number as u32,
        start_lba,
        track_type: track_type as u32,
        sector_size: sector_size as u32,
        filename,
        offset,
    })
}

/// Consume a leading unsigned integer and return it plus the remaining string.
fn take_uint(s: &str) -> Option<(u64, &str)> {
    let s = s.trim_start();
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    let value = s[..end].parse().ok()?;
    Some((value, &s[end..]))
}

/// Open the high-density (game) area of a `.gdi` disc as a browsable
/// [`SectorReader`].
///
/// Selects **every** data track at or beyond [`GDROM_HD_START_LBA`], opens each
/// track file (resolving names relative to the `.gdi`'s directory), and wraps
/// them in a [`GdromSectorReader`] that routes each absolute HD-area LBA to the
/// track physically holding it. Each track's coverage runs up to the next
/// track's start LBA (the last track's from its file length).
///
/// # Errors
///
/// Returns an error if the descriptor cannot be parsed, no high-density data
/// track exists, or a track file cannot be opened.
pub fn open_gdi_hd_reader(gdi_path: &Path) -> Result<Box<dyn SectorReader>> {
    let mut tracks = parse_gdi(gdi_path)?;
    // Sort by start LBA so each track's coverage runs up to the next track's
    // start; the HD area can span several data tracks separated by audio tracks.
    tracks.sort_by_key(|t| t.start_lba);

    let dir = gdi_path.parent().unwrap_or_else(|| Path::new("."));
    let mut gd_tracks = Vec::new();
    for (i, track) in tracks.iter().enumerate() {
        if !(track.is_data() && track.start_lba >= GDROM_HD_START_LBA) {
            continue;
        }
        // Coverage extends to the next track's start; for the final track fall
        // back to the track file's own length so its extent is bounded.
        let frame_count = match tracks.get(i + 1) {
            Some(next) => next.start_lba.saturating_sub(track.start_lba),
            None => gdi_track_frame_count(dir, track)?,
        };
        let reader = GdiTrackReader::open(dir, track)?;
        gd_tracks.push(crate::sector_reader::GdromHdTrack {
            start_lba: track.start_lba,
            frame_count,
            reader: Box::new(reader),
        });
    }

    if gd_tracks.is_empty() {
        return Err(OpticaldiscsError::Parse(
            "no high-density data track in .gdi".into(),
        ));
    }
    Ok(Box::new(GdromSectorReader::from_tracks(gd_tracks)))
}

/// Number of sectors in `track`'s backing file, from its byte length.
fn gdi_track_frame_count(dir: &Path, track: &GdiTrack) -> Result<u64> {
    let path: PathBuf = dir.join(&track.filename);
    let len = std::fs::metadata(&path)
        .map_err(OpticaldiscsError::Io)?
        .len();
    let phys = if track.sector_size == 0 {
        RAW_SECTOR_SIZE
    } else {
        track.sector_size as u64
    };
    Ok(len.saturating_sub(track.offset) / phys)
}

/// A [`SectorReader`] over a single GD-ROM track file, presenting 2048-byte
/// cooked sectors track-relative (sector 0 = the first sector of the track).
struct GdiTrackReader {
    file: BufReader<File>,
    /// Physical bytes per sector in the file (2352 or 2048).
    physical_sector_size: u64,
    /// Byte offset within each physical sector to user data (16 raw, 0 cooked).
    data_offset: u64,
    /// Byte offset in the file where the track's sectors begin.
    file_byte_offset: u64,
}

impl GdiTrackReader {
    fn open(dir: &Path, track: &GdiTrack) -> Result<Self> {
        let path: PathBuf = dir.join(&track.filename);
        let file = File::open(&path).map_err(OpticaldiscsError::Io)?;
        let (physical_sector_size, data_offset) = match track.sector_size as u64 {
            RAW_SECTOR_SIZE => (RAW_SECTOR_SIZE, MODE1_DATA_OFFSET),
            SECTOR_SIZE => (SECTOR_SIZE, 0),
            other => (
                other,
                if other >= RAW_SECTOR_SIZE {
                    MODE1_DATA_OFFSET
                } else {
                    0
                },
            ),
        };
        Ok(Self {
            file: BufReader::new(file),
            physical_sector_size,
            data_offset,
            file_byte_offset: track.offset,
        })
    }
}

impl SectorReader for GdiTrackReader {
    fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
        let pos = self.file_byte_offset + lba * self.physical_sector_size + self.data_offset;
        self.file
            .seek(SeekFrom::Start(pos))
            .map_err(OpticaldiscsError::Io)?;
        let mut buf = vec![0u8; SECTOR_SIZE as usize];
        self.file
            .read_exact(&mut buf)
            .map_err(OpticaldiscsError::Io)?;
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_gdi() {
        let text = "3\n\
            1 0 4 2352 track01.bin 0\n\
            2 600 0 2352 track02.raw 0\n\
            3 45000 4 2352 track03.bin 0\n";
        let dir = tempfile::tempdir().unwrap();
        let gdi = dir.path().join("game.gdi");
        std::fs::write(&gdi, text).unwrap();

        let tracks = parse_gdi(&gdi).unwrap();
        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[2].number, 3);
        assert_eq!(tracks[2].start_lba, 45000);
        assert!(tracks[2].is_data());
        assert_eq!(tracks[2].sector_size, 2352);
        assert_eq!(tracks[2].filename, "track03.bin");
    }

    #[test]
    fn parses_quoted_filename() {
        let t = parse_gdi_line("3 45000 4 2352 \"my track 03.bin\" 0").unwrap();
        assert_eq!(t.filename, "my track 03.bin");
        assert_eq!(t.start_lba, 45000);
    }
}
