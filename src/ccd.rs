//! CloneCD (`.ccd` / `.img` / `.sub`) container support.
//!
//! A CloneCD image is a raw 2352-byte-sector dump (`.img`) described by a small
//! INI-style text sidecar (`.ccd`); an optional `.sub` holds 96-byte subchannel
//! data (not needed for filesystem browsing). The `.img` stores every sector
//! contiguously by absolute LBA — sector *L* lives at byte `L × 2352` — so a
//! track's data begins at `INDEX 1 × 2352`.
//!
//! The `.ccd` describes the disc with sections:
//! - `[Disc]` — `TocEntries`, `Sessions`
//! - `[Entry N]` — a TOC entry: `Point` (`0x01`–`0x63` = track start, `0xA2` =
//!   lead-out) and `PLBA` (the absolute LBA)
//! - `[TRACK N]` — `MODE` (0 = audio, 1 = Mode 1, 2 = Mode 2) and `INDEX 1`
//!   (the track's user-data start LBA)
//!
//! [`parse_ccd`] turns this into a [`Vec<BinTrack>`] pointing at the `.img`, so
//! the shared [`crate::sector_reader::BinCueSectorReader`] and filesystem
//! browsers read a CloneCD image exactly like a BIN/CUE one.
//!
//! Reference: cdemu `libmirage` `image-ccd/parser.c`.

use std::path::{Path, PathBuf};

use crate::bincue::{BinTrack, TrackType};
use crate::error::{OpticaldiscsError, Result};

/// Bytes per raw CloneCD `.img` sector.
const RAW_SECTOR: u64 = 2352;

/// Parse a `.ccd` descriptor into its track list, resolving the sibling `.img`.
///
/// # Errors
///
/// Returns [`OpticaldiscsError::Parse`] if the descriptor has no tracks, or
/// [`OpticaldiscsError::Io`]/[`OpticaldiscsError::NotFound`] if the `.ccd` or its
/// `.img` cannot be read.
pub fn parse_ccd(ccd_path: &Path) -> Result<Vec<BinTrack>> {
    let text = std::fs::read_to_string(ccd_path).map_err(OpticaldiscsError::Io)?;
    let img_path = resolve_img_path(ccd_path)?;
    let sections = parse_ini_sections(&text);

    // First pass: gather raw tracks (number, mode, index1) and the lead-out LBA.
    let mut raw: Vec<(u32, u32, u64)> = Vec::new();
    let mut leadout: Option<u64> = None;

    for (header, kv) in &sections {
        let mut parts = header.split_whitespace();
        match parts.next() {
            Some("TRACK") => {
                let Some(num) = parts.next().and_then(|n| n.parse::<u32>().ok()) else {
                    continue;
                };
                let mode = get(kv, "MODE")
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(1);
                let index1 = get(kv, "INDEX 1")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                raw.push((num, mode, index1));
            }
            Some("Entry") => {
                // The lead-out entry (Point 0xA2) bounds the last track.
                if let Some(point) = get(kv, "Point").and_then(parse_int) {
                    if point == 0xA2 {
                        if let Some(plba) = get(kv, "PLBA").and_then(|v| v.parse::<u64>().ok()) {
                            leadout = Some(plba);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if raw.is_empty() {
        return Err(OpticaldiscsError::Parse(
            "no tracks in .ccd descriptor".into(),
        ));
    }

    raw.sort_by_key(|t| t.0);

    // Second pass: build BinTracks. A track's data starts at `index1 × 2352` in
    // the .img; its length runs to the next track's start (or the lead-out).
    let mut tracks = Vec::with_capacity(raw.len());
    for (i, &(num, mode, index1)) in raw.iter().enumerate() {
        let next_start = raw.get(i + 1).map(|t| t.2).or(leadout);
        let frame_count = next_start.map(|n| n.saturating_sub(index1)).unwrap_or(0);
        tracks.push(BinTrack {
            track_no: num,
            track_type: mode_to_track_type(mode),
            bin_path: img_path.clone(),
            file_byte_offset: index1 * RAW_SECTOR,
            frame_count,
        });
    }
    Ok(tracks)
}

/// Map a CloneCD `MODE` value to a [`TrackType`]. CloneCD `.img` sectors are
/// always 2352-byte raw; mode 1 → Mode 1, mode 2 → Mode 2 Form 1, else audio.
fn mode_to_track_type(mode: u32) -> TrackType {
    match mode {
        1 => TrackType::Mode1Raw,
        2 => TrackType::Mode2Form1,
        _ => TrackType::Audio,
    }
}

/// Resolve the `.img` data file that accompanies `ccd_path`.
fn resolve_img_path(ccd_path: &Path) -> Result<PathBuf> {
    for ext in ["img", "IMG", "Img"] {
        let cand = ccd_path.with_extension(ext);
        if cand.exists() {
            return Ok(cand);
        }
    }
    // Fall back to any sibling file with the same stem and an .img extension.
    let stem = ccd_path.file_stem().unwrap_or_default();
    if let Some(dir) = ccd_path.parent() {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                let ext_ok = p
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("img"));
                if ext_ok && p.file_stem() == Some(stem) {
                    return Ok(p);
                }
            }
        }
    }
    Err(OpticaldiscsError::NotFound(format!(
        "no matching .img found for {}",
        ccd_path.display()
    )))
}

/// Parse an integer that may be hex (`0x…`) or decimal.
fn parse_int(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        s.parse().ok()
    }
}

/// Case-insensitive lookup of `key` in a section's key/value list.
fn get<'a>(kv: &'a [(String, String)], key: &str) -> Option<&'a str> {
    kv.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

/// Split INI text into `(section-header, [(key, value)])` pairs. Keys may
/// contain spaces (e.g. `INDEX 1`), so only the first `=` splits a line.
fn parse_ini_sections(text: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut out: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            out.push((header.trim().to_string(), Vec::new()));
        } else if let Some((k, v)) = line.split_once('=') {
            if let Some((_, kv)) = out.last_mut() {
                kv.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_pair(dir: &Path, stem: &str, ccd: &str, img_sectors: usize) -> PathBuf {
        let ccd_path = dir.join(format!("{stem}.ccd"));
        std::fs::write(&ccd_path, ccd).unwrap();
        std::fs::write(
            dir.join(format!("{stem}.img")),
            vec![0u8; img_sectors * RAW_SECTOR as usize],
        )
        .unwrap();
        ccd_path
    }

    #[test]
    fn parses_single_data_track() {
        let ccd = "[CloneCD]\nVersion=3\n[Disc]\nTocEntries=4\nSessions=1\n\
                   [Entry 2]\nPoint=0xa2\nPLBA=309313\n\
                   [TRACK 1]\nMODE=1\nINDEX 1=0\n";
        let dir = tempfile::tempdir().unwrap();
        let ccd_path = write_pair(dir.path(), "GAME", ccd, 4);

        let tracks = parse_ccd(&ccd_path).unwrap();
        assert_eq!(tracks.len(), 1);
        let t = &tracks[0];
        assert_eq!(t.track_no, 1);
        assert_eq!(t.track_type, TrackType::Mode1Raw);
        assert_eq!(t.file_byte_offset, 0);
        assert_eq!(t.frame_count, 309313);
        assert!(t.is_data());
        assert!(t.bin_path.ends_with("GAME.img"));
    }

    #[test]
    fn mixed_mode_data_then_audio() {
        // Track 1 = Mode 2 data at LBA 0; track 2 = audio at LBA 7624.
        let ccd = "[Entry 3]\nPoint=0xa2\nPLBA=10000\n\
                   [TRACK 1]\nMODE=2\nINDEX 1=0\n\
                   [TRACK 2]\nMODE=0\nINDEX 0=7613\nINDEX 1=7624\n";
        let dir = tempfile::tempdir().unwrap();
        let ccd_path = write_pair(dir.path(), "MIX", ccd, 10000);

        let tracks = parse_ccd(&ccd_path).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].track_type, TrackType::Mode2Form1);
        assert_eq!(tracks[0].file_byte_offset, 0);
        assert_eq!(tracks[0].frame_count, 7624);
        assert!(!tracks[1].is_data());
        assert_eq!(tracks[1].file_byte_offset, 7624 * RAW_SECTOR);
        assert_eq!(tracks[1].frame_count, 10000 - 7624);

        // First data track is track 1.
        let data = tracks.iter().find(|t| t.is_data()).unwrap();
        assert_eq!(data.track_no, 1);
    }

    #[test]
    fn missing_img_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ccd_path = dir.path().join("noimg.ccd");
        std::fs::write(&ccd_path, "[TRACK 1]\nMODE=1\nINDEX 1=0\n").unwrap();
        assert!(parse_ccd(&ccd_path).is_err());
    }
}
