//! Nero (`.nrg`) container support.
//!
//! An NRG file stores the disc data followed by a **footer-anchored** chunk
//! collection describing the tracks. All multi-byte values are big-endian. Two
//! versions exist:
//!
//! - **v2 (`NER5`)** — the last 12 bytes are `NER5` + a `u64` offset to the
//!   chunk list; DAO track offsets are 64-bit; the DAO chunk id is `DAOX`.
//! - **v1 (`NERO`)** — the last 8 bytes are `NERO` + a `u32` offset; DAO track
//!   offsets are 32-bit; the DAO chunk id is `DAOI`.
//!
//! Each chunk is `id[4]` + `length` (`u32`, big-endian) + `length` bytes of
//! data, ending at an `END!` chunk. The DAO chunk holds a 22-byte header then
//! one block per track: `sector_size` (u16) at 0x0C, `mode_code` (u8) at 0x0E,
//! and — at 0x12 — `pregap_offset`, `start_offset`, `end_offset` (the track's
//! **byte offsets in the `.nrg`**). `start_offset` marks the track's user data.
//!
//! Reference: cdemu `libmirage` `image-nrg/image-nrg.h` + `parser.c`.

use std::path::{Path, PathBuf};

use crate::error::{OpticaldiscsError, Result};

/// A single track resolved from an NRG image, with explicit sector geometry
/// ready for [`crate::sector_reader::BinCueSectorReader::with_layout`].
#[derive(Debug, Clone)]
pub struct NrgTrack {
    /// 1-based track number.
    pub track_no: u32,
    /// True for a data track (Mode 1 / Mode 2); false for audio.
    pub is_data: bool,
    /// Physical bytes per sector in the `.nrg`.
    pub physical_sector_size: u64,
    /// Byte offset within each physical sector to the 2048-byte user data.
    pub data_offset: u64,
    /// Byte offset of the track's user data in the `.nrg`.
    pub file_byte_offset: u64,
    /// Track length in sectors (0 if unknown).
    pub frame_count: u64,
    /// Path to the `.nrg` file (data lives in the same file).
    pub data_path: PathBuf,
}

/// Parse an NRG image into its track list.
///
/// # Errors
///
/// Returns [`OpticaldiscsError::Parse`] if the footer magic is missing or the
/// chunks are malformed, or [`OpticaldiscsError::Io`] on read failure.
pub fn parse_nrg(nrg_path: &Path) -> Result<Vec<NrgTrack>> {
    let data = std::fs::read(nrg_path).map_err(OpticaldiscsError::Io)?;
    let len = data.len();
    if len < 12 {
        return Err(OpticaldiscsError::Parse("file too small for NRG".into()));
    }

    // Footer: prefer the 12-byte NER5 (v2); fall back to the 8-byte NERO (v1).
    let (chunk_off, v2) = if &data[len - 12..len - 8] == b"NER5" {
        (be64(&data, len - 8)? as usize, true)
    } else if &data[len - 8..len - 4] == b"NERO" {
        (be32(&data, len - 4)? as usize, false)
    } else {
        return Err(OpticaldiscsError::Parse(
            "no NER5/NERO footer magic in .nrg".into(),
        ));
    };
    let dao_id: &[u8; 4] = if v2 { b"DAOX" } else { b"DAOI" };
    let block_size = if v2 { 42 } else { 30 };
    let offset_width = if v2 { 8 } else { 4 };

    let mut tracks = Vec::new();
    let mut p = chunk_off;
    while p + 8 <= len {
        let id = &data[p..p + 4];
        let clen = be32(&data, p + 4)? as usize;
        let body_start = p + 8;
        if id == b"END!" {
            break;
        }
        if body_start + clen > len {
            break;
        }
        if id == dao_id {
            parse_dao(
                &data[body_start..body_start + clen],
                block_size,
                offset_width,
                nrg_path,
                &mut tracks,
            )?;
        }
        p = body_start + clen;
    }

    if tracks.is_empty() {
        return Err(OpticaldiscsError::Parse(
            "no DAO track information in .nrg".into(),
        ));
    }
    Ok(tracks)
}

/// Parse a DAO chunk body (22-byte header + one block per track).
fn parse_dao(
    body: &[u8],
    block_size: usize,
    offset_width: usize,
    nrg_path: &Path,
    out: &mut Vec<NrgTrack>,
) -> Result<()> {
    const HEADER: usize = 22;
    if body.len() < HEADER {
        return Err(OpticaldiscsError::Parse("truncated DAO chunk".into()));
    }
    let num_blocks = (body.len() - HEADER) / block_size;
    for i in 0..num_blocks {
        let o = HEADER + i * block_size;
        let sector_size = be16(body, o + 0x0C)? as u64;
        let mode_code = body[o + 0x0E];
        // pregap, start, end offsets follow at 0x12 (u64 for v2, u32 for v1).
        let start_offset = read_off(body, o + 0x12 + offset_width, offset_width)?;
        let end_offset = read_off(body, o + 0x12 + 2 * offset_width, offset_width)?;

        let (data_offset, is_data) = decode_mode(mode_code);
        let frame_count = end_offset
            .saturating_sub(start_offset)
            .checked_div(sector_size)
            .unwrap_or(0);

        out.push(NrgTrack {
            track_no: (i as u32) + 1,
            is_data,
            physical_sector_size: sector_size,
            data_offset,
            file_byte_offset: start_offset,
            frame_count,
            data_path: nrg_path.to_path_buf(),
        });
    }
    Ok(())
}

/// Map a Nero `mode_code` to `(user_data_offset, is_data)`.
fn decode_mode(code: u8) -> (u64, bool) {
    match code {
        0x00 | 0x02 => (0, true),  // Mode 1 / Mode 2 Form 1, cooked (2048)
        0x03 => (8, true),         // Mode 2 Form 2 (2336), 8-byte subheader
        0x05 | 0x0F => (16, true), // Mode 1, raw (2352)
        0x06 | 0x11 => (24, true), // Mode 2 mixed, raw (2352)
        0x07 | 0x10 => (0, false), // audio
        _ => (0, false),           // unknown → treat as audio
    }
}

fn read_off(d: &[u8], o: usize, width: usize) -> Result<u64> {
    if width == 8 {
        be64(d, o)
    } else {
        Ok(be32(d, o)? as u64)
    }
}

fn be16(d: &[u8], o: usize) -> Result<u16> {
    d.get(o..o + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .ok_or_else(|| OpticaldiscsError::Parse("truncated NRG chunk".into()))
}
fn be32(d: &[u8], o: usize) -> Result<u32> {
    d.get(o..o + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| OpticaldiscsError::Parse("truncated NRG chunk".into()))
}
fn be64(d: &[u8], o: usize) -> Result<u64> {
    d.get(o..o + 8)
        .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
        .ok_or_else(|| OpticaldiscsError::Parse("truncated NRG chunk".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal NER5 image: a data region, then CUEX/DAOX/END! chunks and
    /// the NER5 footer. `mode`/`ssize` describe the single track at `start`.
    fn build_ner5(mode: u8, ssize: u16, start: u64, sectors: u64) -> Vec<u8> {
        let end = start + sectors * ssize as u64;
        let mut data = vec![0u8; end as usize];

        let chunk_off = data.len() as u64;
        // DAOX chunk: 22-byte header + one 42-byte block.
        let mut dao = vec![0u8; 22 + 42];
        dao[0x14] = 1; // first_track
        dao[0x15] = 1; // last_track
        let b = 22;
        dao[b + 0x0C..b + 0x0E].copy_from_slice(&ssize.to_be_bytes());
        dao[b + 0x0E] = mode;
        dao[b + 0x12..b + 0x1A].copy_from_slice(&start.to_be_bytes()); // pregap
        dao[b + 0x1A..b + 0x22].copy_from_slice(&start.to_be_bytes()); // start
        dao[b + 0x22..b + 0x2A].copy_from_slice(&end.to_be_bytes()); // end

        let push_chunk = |v: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]| {
            v.extend_from_slice(id);
            v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            v.extend_from_slice(payload);
        };
        push_chunk(&mut data, b"CUEX", &[0u8; 16]);
        push_chunk(&mut data, b"DAOX", &dao);
        push_chunk(&mut data, b"END!", &[]);

        // NER5 footer.
        data.extend_from_slice(b"NER5");
        data.extend_from_slice(&chunk_off.to_be_bytes());
        data
    }

    fn write(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::Builder::new().suffix(".nrg").tempfile().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn parses_ner5_mode1_cooked() {
        // Data track at byte 307200, 2048-byte cooked sectors, 100 sectors.
        let img = build_ner5(0x00, 2048, 307200, 100);
        let f = write(&img);
        let tracks = parse_nrg(f.path()).unwrap();
        assert_eq!(tracks.len(), 1);
        let t = &tracks[0];
        assert!(t.is_data);
        assert_eq!(t.physical_sector_size, 2048);
        assert_eq!(t.data_offset, 0);
        assert_eq!(t.file_byte_offset, 307200);
        assert_eq!(t.frame_count, 100);
    }

    #[test]
    fn parses_ner5_mode1_raw() {
        let img = build_ner5(0x05, 2352, 0, 50);
        let f = write(&img);
        let t = &parse_nrg(f.path()).unwrap()[0];
        assert_eq!(t.physical_sector_size, 2352);
        assert_eq!(t.data_offset, 16); // raw Mode 1
        assert_eq!(t.frame_count, 50);
    }

    #[test]
    fn decode_mode_table() {
        assert_eq!(decode_mode(0x00), (0, true));
        assert_eq!(decode_mode(0x03), (8, true));
        assert_eq!(decode_mode(0x05), (16, true));
        assert_eq!(decode_mode(0x06), (24, true));
        assert_eq!(decode_mode(0x07), (0, false)); // audio
    }

    #[test]
    fn rejects_non_nrg() {
        let f = write(b"not a nero image, no footer magic at all here...");
        assert!(parse_nrg(f.path()).is_err());
    }
}
