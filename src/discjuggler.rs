//! DiscJuggler (`.cdi`) container support.
//!
//! A CDI file stores the disc data at the **start** of the file and a
//! **descriptor** at the very end. The last 4 bytes are the descriptor length
//! (`u32`, little-endian); the descriptor is that many bytes at the end of the
//! file. All multi-byte values in the descriptor are little-endian.
//!
//! The descriptor is heuristic / reverse-engineered (there is no version-stable
//! public spec). Its structure, per cdemu's `libmirage` `image-cdi` parser:
//!
//! - `num_sessions` (`u8`) at the start;
//! - then, for each session (and one trailing empty session), a 15-byte session
//!   descriptor whose 2nd byte is the session's track count, followed by that
//!   many **variable-length** track descriptors;
//! - each track descriptor is a fixed "bare" body wrapped around a variable
//!   filename, a variable index list, and optional CD-Text blocks — so the
//!   cursor must be walked field by field, exactly like libmirage;
//! - a disc descriptor closes the file (not needed here).
//!
//! The image data is a plain concatenation of every track's raw sectors, in
//! order. A running byte cursor (`cur_offset`) advances by
//! `sector_size × track_length` per track, giving each track's byte offset.
//!
//! ## Dreamcast GD-ROM rips
//!
//! Most `.cdi` in the wild are Dreamcast GD-ROM rips. Their high-density game
//! session carries an ISO 9660 volume whose **directory extents use absolute
//! disc LBAs** (the session's `start_address`), while the Primary Volume
//! Descriptor is still addressed volume-relative at LBA 16. The [`start_address`]
//! field records that absolute base; browsing rebases reads through
//! [`crate::sector_reader::RebaseSectorReader`] so the standard ISO 9660 browser
//! resolves both. The 150-sector start pregap is physically present in the file,
//! so a track's user data begins `pregap` sectors past its raw start.
//!
//! [`start_address`]: DiscJugglerTrack::base_lba
//!
//! Reference: cdemu `libmirage` `image-cdi/parser.c` (`load_disc` / `load_session`
//! / `load_track` / `parse_header`).

use std::path::{Path, PathBuf};

use crate::error::{OpticaldiscsError, Result};

/// A single track resolved from a DiscJuggler image, with explicit sector
/// geometry ready for [`crate::sector_reader::BinCueSectorReader::with_layout`].
#[derive(Debug, Clone)]
pub struct DiscJugglerTrack {
    /// 1-based track number within its session.
    pub track_no: u32,
    /// True for a data track (Mode 1 / Mode 2); false for audio.
    pub is_data: bool,
    /// Physical bytes per sector in the `.cdi` (main portion + any subchannel).
    pub physical_sector_size: u64,
    /// Byte offset within each physical sector to the 2048-byte user data.
    pub data_offset: u64,
    /// Byte offset in the `.cdi` of the track's **user data** (past the pregap).
    pub file_byte_offset: u64,
    /// Absolute disc LBA at which the track's volume begins (`start_address`).
    ///
    /// Non-zero for Dreamcast high-density sessions, whose ISO 9660 directory
    /// extents are stored as absolute LBAs; 0 for a normal volume-relative disc.
    pub base_lba: u64,
    /// Track length in sectors (includes the pregap).
    pub frame_count: u64,
    /// Path to the `.cdi` file (data lives in the same file).
    pub data_path: PathBuf,
}

/// Parse a DiscJuggler image into its track list.
///
/// # Errors
///
/// Returns [`OpticaldiscsError::Parse`] if the descriptor length is implausible
/// or the descriptor is malformed/truncated, or [`OpticaldiscsError::Io`] on
/// read failure.
pub fn parse_discjuggler(cdi_path: &Path) -> Result<Vec<DiscJugglerTrack>> {
    let data = std::fs::read(cdi_path).map_err(OpticaldiscsError::Io)?;
    if data.len() < 8 {
        return Err(OpticaldiscsError::Parse("file too small for CDI".into()));
    }

    // Last 4 bytes: descriptor length; the descriptor is that many bytes at EOF.
    let dlen = le32(&data, data.len() - 4)? as usize;
    if dlen < 4 || dlen > data.len() {
        return Err(OpticaldiscsError::Parse(
            "implausible CDI descriptor length".into(),
        ));
    }
    let desc = &data[data.len() - dlen..];

    let mut cur = Cursor { d: desc, p: 0 };
    let num_sessions = cur.u8()?;

    let mut cur_offset: u64 = 0;
    let mut tracks = Vec::new();
    // One extra trailing session descriptor (with 0 tracks) closes the list.
    for _ in 0..=num_sessions {
        load_session(&mut cur, &mut cur_offset, cdi_path, &mut tracks)?;
    }

    if tracks.is_empty() {
        return Err(OpticaldiscsError::Parse(
            "no tracks in CDI descriptor".into(),
        ));
    }
    Ok(tracks)
}

/// Parse one session descriptor (15 bytes) and its tracks.
fn load_session(
    cur: &mut Cursor,
    cur_offset: &mut u64,
    cdi_path: &Path,
    out: &mut Vec<DiscJugglerTrack>,
) -> Result<()> {
    // 2nd byte of the session descriptor is the track count.
    let num_tracks = *cur.peek(1)?;
    cur.skip(15)?;
    for i in 0..num_tracks {
        load_track(cur, cur_offset, u32::from(i) + 1, cdi_path, out)?;
    }
    Ok(())
}

/// Parse one variable-length track descriptor, appending the resolved track and
/// advancing the running image byte offset.
fn load_track(
    cur: &mut Cursor,
    cur_offset: &mut u64,
    track_no: u32,
    cdi_path: &Path,
    out: &mut Vec<DiscJugglerTrack>,
) -> Result<()> {
    parse_header(cur)?;

    // Index list.
    let num_indices = cur.u16()? as usize;
    let mut first_index = 0u32;
    for i in 0..num_indices {
        let idx = cur.u32()?;
        if i == 0 {
            first_index = idx; // start pregap length, in sectors
        }
    }

    // Optional CD-Text blocks.
    let num_cdtext = cur.u32()? as usize;
    for _ in 0..num_cdtext {
        parse_cdtext(cur)?;
    }

    cur.skip(2)?; // 2 undeciphered bytes
    let track_mode = cur.u32()?;
    cur.skip(4)?; // 4 undeciphered bytes
    let _session_idx = cur.u32()?;
    let _track_idx = cur.u32()?;
    let start_address = cur.u32()?;
    let track_length = cur.u32()?;
    cur.skip(16)?; // 16 undeciphered bytes
    let read_mode = cur.u32()?;
    let _track_ctl = cur.u32()?;
    cur.skip(9)?; // 9 undeciphered bytes (repeated track length + zeros)
    cur.skip(12)?; // ISRC
    let _isrc_valid = cur.u32()?;
    cur.skip(99)?; // 99 undeciphered trailing bytes

    let (main_size, subchannel_size) = decode_read_mode(read_mode)?;
    let physical_sector_size = u64::from(main_size) + u64::from(subchannel_size);
    let is_data = matches!(track_mode, 1 | 2);
    let pregap = u64::from(first_index);
    let track_start = *cur_offset;

    out.push(DiscJugglerTrack {
        track_no,
        is_data,
        physical_sector_size,
        data_offset: user_data_offset(track_mode, main_size),
        // The pregap sectors are physically present; user data starts past them.
        file_byte_offset: track_start + pregap * physical_sector_size,
        base_lba: u64::from(start_address),
        frame_count: u64::from(track_length),
        data_path: cdi_path.to_path_buf(),
    });

    *cur_offset += physical_sector_size * u64::from(track_length);
    Ok(())
}

/// Skip the per-track/-disc header: 16 fixed bytes, a length-prefixed filename,
/// 29 undeciphered bytes, and a 2-byte medium type.
fn parse_header(cur: &mut Cursor) -> Result<()> {
    cur.skip(16)?;
    let filename_length = cur.u8()? as usize;
    cur.skip(filename_length)?;
    cur.skip(29)?;
    cur.skip(2)?; // medium type
    Ok(())
}

/// Skip a CD-Text block: 18 fields, each a length byte plus that many bytes.
fn parse_cdtext(cur: &mut Cursor) -> Result<()> {
    for _ in 0..18 {
        let length = cur.u8()? as usize;
        if length != 0 {
            cur.skip(length)?;
        }
    }
    Ok(())
}

/// Map a CDI `read_mode` to `(main_size, subchannel_size)` in bytes.
fn decode_read_mode(read_mode: u32) -> Result<(u32, u32)> {
    match read_mode {
        0 => Ok((2048, 0)),  // Mode 1 cooked
        1 => Ok((2336, 0)),  // Mode 2 (no sync/header)
        2 => Ok((2352, 0)),  // raw / audio
        3 => Ok((2352, 16)), // raw + Q subchannel
        4 => Ok((2352, 96)), // raw + PW subchannel
        other => Err(OpticaldiscsError::Parse(format!(
            "invalid CDI read mode: {other}"
        ))),
    }
}

/// Byte offset to the 2048-byte user data within a track's main sector portion,
/// from its `track_mode` (0 audio / 1 Mode 1 / 2 Mode 2) and main sector size.
fn user_data_offset(track_mode: u32, main_size: u32) -> u64 {
    match track_mode {
        1 => {
            if main_size >= 2352 {
                16 // raw Mode 1: 12 sync + 4 header
            } else {
                0 // cooked (2048)
            }
        }
        2 => match main_size {
            s if s >= 2352 => 24, // raw Mode 2: 16 sync/header + 8 subheader
            2336 => 8,            // Mode 2 without sync+header: 8-byte subheader
            _ => 0,
        },
        _ => 0, // audio
    }
}

/// Little-endian byte cursor over the descriptor, with bounds checking so a
/// truncated or foreign file yields a [`OpticaldiscsError::Parse`], not a panic.
struct Cursor<'a> {
    d: &'a [u8],
    p: usize,
}

impl Cursor<'_> {
    fn peek(&self, off: usize) -> Result<&u8> {
        self.d.get(self.p + off).ok_or_else(trunc)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        let end = self.p.checked_add(n).ok_or_else(trunc)?;
        if end > self.d.len() {
            return Err(trunc());
        }
        self.p = end;
        Ok(())
    }
    fn u8(&mut self) -> Result<u8> {
        let v = *self.d.get(self.p).ok_or_else(trunc)?;
        self.p += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16> {
        let v = le16(self.d, self.p)?;
        self.p += 2;
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32> {
        let v = le32(self.d, self.p)?;
        self.p += 4;
        Ok(v)
    }
}

fn trunc() -> OpticaldiscsError {
    OpticaldiscsError::Parse("truncated CDI descriptor".into())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Append a track descriptor to `desc` for the given fields. Uses the
    /// minimal shape libmirage walks: empty filename, two indices (pregap +
    /// index 1), no CD-Text.
    fn push_track(
        desc: &mut Vec<u8>,
        track_mode: u32,
        read_mode: u32,
        start_address: u32,
        track_length: u32,
        pregap: u32,
    ) {
        // Header: 16 fixed + filename_length(0) + 29 + 2 medium type.
        desc.extend_from_slice(&[0u8; 16]);
        desc.push(0); // filename length
        desc.extend_from_slice(&[0u8; 29]);
        desc.extend_from_slice(&[0u8; 2]); // medium type
                                           // Indices: pregap length, then index 1.
        desc.extend_from_slice(&2u16.to_le_bytes()); // num_indices (u16)
        desc.extend_from_slice(&pregap.to_le_bytes());
        desc.extend_from_slice(&(track_length - pregap).to_le_bytes());
        desc.extend_from_slice(&0u32.to_le_bytes()); // num_cdtext_blocks
        desc.extend_from_slice(&[0u8; 2]); // 2 bytes
        desc.extend_from_slice(&track_mode.to_le_bytes());
        desc.extend_from_slice(&[0u8; 4]);
        desc.extend_from_slice(&0u32.to_le_bytes()); // session_idx
        desc.extend_from_slice(&0u32.to_le_bytes()); // track_idx
        desc.extend_from_slice(&start_address.to_le_bytes());
        desc.extend_from_slice(&track_length.to_le_bytes());
        desc.extend_from_slice(&[0u8; 16]);
        desc.extend_from_slice(&read_mode.to_le_bytes());
        desc.extend_from_slice(&0u32.to_le_bytes()); // track_ctl
        desc.extend_from_slice(&[0u8; 9]);
        desc.extend_from_slice(&[0u8; 12]); // ISRC
        desc.extend_from_slice(&0u32.to_le_bytes()); // isrc_valid
        desc.extend_from_slice(&[0u8; 99]);
    }

    /// Build a minimal CDI file: `sessions` is a list of track specs grouped by
    /// session, each `(track_mode, read_mode, start_address, track_length,
    /// pregap)`. Data is zero-filled to cover the tracks.
    fn build_cdi(sessions: &[Vec<(u32, u32, u32, u32, u32)>]) -> Vec<u8> {
        // Compute total image length.
        let mut total_sectors = 0u64;
        for s in sessions {
            for &(_, rm, _, len, _) in s {
                let (main, sub) = decode_read_mode(rm).unwrap();
                total_sectors += (u64::from(main) + u64::from(sub)) * u64::from(len);
            }
        }
        // Descriptor.
        let mut desc = Vec::new();
        desc.push(sessions.len() as u8); // num_sessions
        for s in sessions {
            desc.push(0); // byte 0
            desc.push(s.len() as u8); // track count
            desc.extend_from_slice(&[0u8; 13]); // rest of 15-byte session desc
            for &(tm, rm, sa, len, pg) in s {
                push_track(&mut desc, tm, rm, sa, len, pg);
            }
        }
        // Trailing empty session (0 tracks).
        desc.push(0);
        desc.push(0);
        desc.extend_from_slice(&[0u8; 13]);

        // A minimal disc descriptor is not parsed, but the length field must
        // still cover the sessions we wrote. Append the length word.
        let dlen = (desc.len() + 4) as u32;

        let mut file = vec![0u8; (total_sectors * 2352) as usize];
        // Ensure the file is at least as large as the data region we imply.
        if file.is_empty() {
            file = vec![0u8; 2352];
        }
        file.extend_from_slice(&desc);
        file.extend_from_slice(&dlen.to_le_bytes());
        file
    }

    fn write(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::Builder::new().suffix(".cdi").tempfile().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn parses_single_mode2_track() {
        // One session, one Mode 2 (2336) data track, 100 sectors, pregap 150.
        let img = build_cdi(&[vec![(2, 1, 0, 250, 150)]]);
        let f = write(&img);
        let tracks = parse_discjuggler(f.path()).unwrap();
        assert_eq!(tracks.len(), 1);
        let t = &tracks[0];
        assert!(t.is_data);
        assert_eq!(t.physical_sector_size, 2336);
        assert_eq!(t.data_offset, 8);
        assert_eq!(t.base_lba, 0);
        // User data starts past the 150-sector pregap.
        assert_eq!(t.file_byte_offset, 150 * 2336);
        assert_eq!(t.frame_count, 250);
    }

    #[test]
    fn accumulates_offsets_across_sessions() {
        // Session 0: audio track (raw 2352), 452 sectors.
        // Session 1: Mode 2 data track at absolute LBA 11702, pregap 150.
        let img = build_cdi(&[vec![(0, 2, 0, 452, 150)], vec![(2, 1, 11702, 500, 150)]]);
        let f = write(&img);
        let tracks = parse_discjuggler(f.path()).unwrap();
        assert_eq!(tracks.len(), 2);
        assert!(!tracks[0].is_data);
        let data = &tracks[1];
        assert!(data.is_data);
        assert_eq!(data.base_lba, 11702);
        // Data track starts after the audio track's 452 raw sectors, plus its
        // own 150-sector pregap.
        assert_eq!(data.file_byte_offset, 452 * 2352 + 150 * 2336);
    }

    #[test]
    fn read_mode_and_data_offset_tables() {
        assert_eq!(decode_read_mode(0).unwrap(), (2048, 0));
        assert_eq!(decode_read_mode(1).unwrap(), (2336, 0));
        assert_eq!(decode_read_mode(3).unwrap(), (2352, 16));
        assert!(decode_read_mode(9).is_err());

        assert_eq!(user_data_offset(1, 2048), 0);
        assert_eq!(user_data_offset(1, 2352), 16);
        assert_eq!(user_data_offset(2, 2336), 8);
        assert_eq!(user_data_offset(2, 2352), 24);
        assert_eq!(user_data_offset(0, 2352), 0); // audio
    }

    #[test]
    fn rejects_non_cdi() {
        let f = write(b"not a disc juggler image at all, no descriptor");
        assert!(parse_discjuggler(f.path()).is_err());
    }
}
