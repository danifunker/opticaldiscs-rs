//! Alcohol 120% (`.mds` / `.mdf`) container support.
//!
//! An MDS file is a binary "Media Descriptor" sidecar for a raw data image
//! (`.mdf`). It begins with the 16-byte signature `MEDIA DESCRIPTOR`, followed
//! by fixed-size, little-endian structures:
//!
//! - **Header** (88 bytes) — `num_sessions` at 0x14, `sessions_blocks_offset`
//!   at 0x50.
//! - **Session block** (24 bytes) — `num_all_blocks` at 0x0A and
//!   `tracks_blocks_offset` at 0x14.
//! - **Track block** (80 bytes) — `mode` at 0x00, `subchannel` at 0x01, `point`
//!   at 0x04 (`0xA0`/`0xA1`/`0xA2` are lead-in/out metadata, not real tracks),
//!   `extra_offset` at 0x0C, `sector_size` at 0x10, `start_sector` at 0x24, and
//!   `start_offset` (the track's **byte offset in the `.mdf`**) at 0x28.
//! - **Track extra block** (8 bytes) — `pregap`, then `length` (sector count).
//!
//! Note `sector_size` is the *physical* stride and often includes 96 bytes of
//! appended subchannel data (e.g. 2448 = 2352 + 96); the 2048-byte user data
//! still starts at the mode's usual offset within the 2352-byte main portion.
//!
//! Reference: cdemu `libmirage` `image-mds/image-mds.h` + `parser.c`.

use std::path::{Path, PathBuf};

use crate::error::{OpticaldiscsError, Result};

/// 16-byte MDS signature at offset 0.
pub const MDS_SIGNATURE: &[u8; 16] = b"MEDIA DESCRIPTOR";

// Track `mode` values (MDS_TrackMode).
const MODE_AUDIO: u8 = 0xA9;
const MODE_MODE1: u8 = 0xAA;
const MODE_MODE2: u8 = 0xAB;
const MODE_MODE2_FORM1: u8 = 0xAC;
const MODE_MODE2_FORM2: u8 = 0xAD;

/// A single real track resolved from an MDS descriptor, with explicit sector
/// geometry ready for [`crate::sector_reader::BinCueSectorReader::with_layout`].
#[derive(Debug, Clone)]
pub struct MdsTrack {
    /// Track number (the `point` field, 1–99).
    pub track_no: u8,
    /// True for a data track (Mode 1 / Mode 2); false for audio.
    pub is_data: bool,
    /// Physical bytes per sector in the `.mdf` (may include 96 subchannel bytes).
    pub physical_sector_size: u64,
    /// Byte offset within each physical sector to the 2048-byte user data.
    pub data_offset: u64,
    /// Byte offset of the track's first sector in the data file.
    pub file_byte_offset: u64,
    /// Track length in sectors (0 if unknown).
    pub frame_count: u64,
    /// Path to the `.mdf` data file holding this track.
    pub data_path: PathBuf,
}

/// Parse an MDS descriptor into its real tracks, resolving the `.mdf` data file.
///
/// Lead-in/lead-out metadata blocks (`point` `0xA0`–`0xA2`) are skipped.
///
/// # Errors
///
/// Returns [`OpticaldiscsError::Parse`] if the signature is missing or the
/// structures are truncated, or [`OpticaldiscsError::NotFound`] if no `.mdf` can
/// be located.
pub fn parse_mds(mds_path: &Path) -> Result<Vec<MdsTrack>> {
    let d = std::fs::read(mds_path).map_err(OpticaldiscsError::Io)?;
    if d.len() < 0x58 || &d[..16] != MDS_SIGNATURE {
        return Err(OpticaldiscsError::Parse(
            "not an Alcohol .mds descriptor".into(),
        ));
    }
    let data_path = resolve_mdf_path(mds_path)?;

    let num_sessions = le16(&d, 0x14)? as usize;
    let sessions_off = le32(&d, 0x50)? as usize;

    let mut tracks = Vec::new();
    for s in 0..num_sessions {
        let sb = sessions_off + s * 24;
        let num_all = *d.get(sb + 0x0A).ok_or_else(trunc)? as usize;
        let tracks_off = le32(&d, sb + 0x14)? as usize;

        for t in 0..num_all {
            let o = tracks_off + t * 80;
            if o + 80 > d.len() {
                return Err(trunc());
            }
            let mode = d[o];
            let subchannel = d[o + 1];
            let point = d[o + 4];
            // Skip lead-in/first/last/lead-out metadata blocks.
            if !(1..=99).contains(&point) {
                continue;
            }
            let sector_size = le16(&d, o + 0x10)? as u64;
            let start_offset = le64(&d, o + 0x28)?;
            let extra_offset = le32(&d, o + 0x0C)? as usize;

            let frame_count = if extra_offset != 0 && extra_offset + 8 <= d.len() {
                le32(&d, extra_offset + 4)? as u64 // length field (pregap is at +0)
            } else {
                0
            };

            let subchannel_bytes = if subchannel == 0x08 { 96 } else { 0 };
            let main_size = sector_size.saturating_sub(subchannel_bytes);
            let is_data = matches!(
                mode,
                MODE_MODE1 | MODE_MODE2 | MODE_MODE2_FORM1 | MODE_MODE2_FORM2
            );

            tracks.push(MdsTrack {
                track_no: point,
                is_data,
                physical_sector_size: sector_size,
                data_offset: user_data_offset(mode, main_size),
                file_byte_offset: start_offset,
                frame_count,
                data_path: data_path.clone(),
            });
        }
    }

    if tracks.is_empty() {
        return Err(OpticaldiscsError::Parse(
            "no tracks in .mds descriptor".into(),
        ));
    }
    Ok(tracks)
}

/// Byte offset to the 2048-byte user data within a track's main sector portion,
/// from its `mode` and main (subchannel-excluded) sector size.
fn user_data_offset(mode: u8, main_size: u64) -> u64 {
    match mode {
        MODE_AUDIO => 0,
        MODE_MODE1 => {
            if main_size >= 2352 {
                16 // raw Mode 1: 12 sync + 4 header
            } else {
                0 // cooked
            }
        }
        MODE_MODE2 | MODE_MODE2_FORM1 | MODE_MODE2_FORM2 => match main_size {
            s if s >= 2352 => 24, // raw Mode 2: 16 sync/header + 8 subheader
            2336 => 8,            // Mode 2 without sync+header: 8-byte subheader
            _ => 0,               // cooked
        },
        _ => 0,
    }
}

/// Given either a `.mds` descriptor or a `.mdf` data file, return the `.mds`
/// path. A `.mds` is returned unchanged; a `.mdf` resolves to its sibling `.mds`.
pub fn resolve_mds_path(path: &Path) -> Result<PathBuf> {
    let is_mdf = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mdf"));
    if !is_mdf {
        return Ok(path.to_path_buf());
    }
    for ext in ["mds", "MDS", "Mds"] {
        let cand = path.with_extension(ext);
        if cand.exists() {
            return Ok(cand);
        }
    }
    Err(OpticaldiscsError::NotFound(format!(
        "no matching .mds found for {}",
        path.display()
    )))
}

/// Resolve the `.mdf` data file for `mds_path` (sibling with the same stem).
fn resolve_mdf_path(mds_path: &Path) -> Result<PathBuf> {
    for ext in ["mdf", "MDF", "Mdf"] {
        let cand = mds_path.with_extension(ext);
        if cand.exists() {
            return Ok(cand);
        }
    }
    let stem = mds_path.file_stem().unwrap_or_default();
    if let Some(dir) = mds_path.parent() {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                let ext_ok = p
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("mdf"));
                if ext_ok && p.file_stem() == Some(stem) {
                    return Ok(p);
                }
            }
        }
    }
    Err(OpticaldiscsError::NotFound(format!(
        "no matching .mdf found for {}",
        mds_path.display()
    )))
}

fn trunc() -> OpticaldiscsError {
    OpticaldiscsError::Parse("truncated .mds descriptor".into())
}

fn le16(d: &[u8], o: usize) -> Result<u16> {
    d.get(o..o + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(trunc)
}
fn le32(d: &[u8], o: usize) -> Result<u32> {
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(trunc)
}
fn le64(d: &[u8], o: usize) -> Result<u64> {
    d.get(o..o + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(trunc)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal MDS: header + one session + 3 metadata blocks + 1 data
    /// track (Mode 1, 2448-byte sectors with subchannel), plus an extra block.
    fn build_mds(dir: &Path, stem: &str) -> PathBuf {
        let mut d = vec![0u8; 0x58]; // header
        d[..16].copy_from_slice(MDS_SIGNATURE);
        d[0x14..0x16].copy_from_slice(&1u16.to_le_bytes()); // num_sessions
        let sessions_off = 0x58u32;
        d[0x50..0x54].copy_from_slice(&sessions_off.to_le_bytes());

        // Session block (24 bytes) at 0x58.
        let tracks_off = sessions_off + 24;
        let mut sb = vec![0u8; 24];
        sb[0x0A] = 4; // num_all_blocks: 3 metadata + 1 track
        sb[0x14..0x18].copy_from_slice(&tracks_off.to_le_bytes());
        d.extend_from_slice(&sb);

        // Track blocks (80 bytes each): 3 metadata (point A0/A1/A2) + 1 real.
        let extra_off = tracks_off + 4 * 80;
        for point in [0xA0u8, 0xA1, 0xA2] {
            let mut tb = vec![0u8; 80];
            tb[4] = point;
            d.extend_from_slice(&tb);
        }
        let mut tb = vec![0u8; 80];
        tb[0] = MODE_MODE1;
        tb[1] = 0x08; // PW subchannel
        tb[4] = 0x01; // point = track 1
        tb[0x0C..0x10].copy_from_slice(&extra_off.to_le_bytes());
        tb[0x10..0x12].copy_from_slice(&2448u16.to_le_bytes()); // sector size
        tb[0x28..0x30].copy_from_slice(&0u64.to_le_bytes()); // start_offset
        d.extend_from_slice(&tb);

        // Extra block: pregap=150, length=100.
        d.extend_from_slice(&150u32.to_le_bytes());
        d.extend_from_slice(&100u32.to_le_bytes());

        let mds_path = dir.join(format!("{stem}.mds"));
        std::fs::write(&mds_path, &d).unwrap();
        // Matching .mdf (content irrelevant to the parser).
        std::fs::write(dir.join(format!("{stem}.mdf")), vec![0u8; 2448 * 100]).unwrap();
        mds_path
    }

    #[test]
    fn parses_mode1_with_subchannel() {
        let dir = tempfile::tempdir().unwrap();
        let mds = build_mds(dir.path(), "GAME");
        let tracks = parse_mds(&mds).unwrap();

        assert_eq!(tracks.len(), 1); // metadata blocks skipped
        let t = &tracks[0];
        assert_eq!(t.track_no, 1);
        assert!(t.is_data);
        assert_eq!(t.physical_sector_size, 2448); // 2352 + 96 subchannel
        assert_eq!(t.data_offset, 16); // raw Mode 1
        assert_eq!(t.file_byte_offset, 0);
        assert_eq!(t.frame_count, 100);
        assert!(t.data_path.ends_with("GAME.mdf"));
    }

    #[test]
    fn user_data_offsets() {
        assert_eq!(user_data_offset(MODE_AUDIO, 2352), 0);
        assert_eq!(user_data_offset(MODE_MODE1, 2352), 16);
        assert_eq!(user_data_offset(MODE_MODE1, 2048), 0);
        assert_eq!(user_data_offset(MODE_MODE2_FORM1, 2352), 24);
        assert_eq!(user_data_offset(MODE_MODE2, 2336), 8);
        assert_eq!(user_data_offset(MODE_MODE2_FORM1, 2048), 0);
    }

    #[test]
    fn rejects_non_mds() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.mds");
        std::fs::write(&p, b"not a media descriptor file......").unwrap();
        assert!(parse_mds(&p).is_err());
    }
}
