//! Disc Table of Contents (TOC) and disc ID calculation.
//!
//! This module is gated behind the `toc` feature flag.  It provides:
//!
//! - [`DiscTOC`] — the track layout of a disc (absolute frame positions)
//! - [`TrackInfo`] — per-track data used to build a [`DiscTOC`]
//! - [`parse_msf`] / [`frames_to_msf`] — MSF ↔ frame conversion helpers
//! - [`DiscTOC::musicbrainz_id`] — MusicBrainz Disc ID (SHA-1 + MB base64)
//! - [`DiscTOC::freedb_id`] — FreeDB disc ID (legacy CDDB checksum)
//!
//! # Feature flag
//!
//! Add `features = ["toc"]` to the `opticaldiscs` dependency in your
//! `Cargo.toml` to enable this module.

use base64::Engine as _;
use sha1::{Digest, Sha1};

use crate::error::{OpticaldiscsError, Result};

/// CD frames per second (Red Book standard: 75 frames = 1 second).
pub const FRAMES_PER_SECOND: u32 = 75;

/// Absolute frame offset of the physical start of user data on any CD.
///
/// The first 2 seconds (150 frames) of every disc are reserved for the pregap.
/// Track 1 INDEX 01 at CUE position `00:00:00` therefore lives at absolute
/// frame 150.
pub const PREGAP_FRAMES: u32 = 150;

// ── TrackInfo ─────────────────────────────────────────────────────────────────

/// Information about a single disc track.
///
/// `offset` is the *raw* frame index as read from a CUE sheet or CHD CHT2
/// metadata entry — i.e. the value returned by [`parse_msf`] for the INDEX 01
/// timestamp.  [`DiscTOC::from_tracks`] adds the 150-frame pregap to produce
/// the absolute offset stored inside [`DiscTOC`].
#[derive(Debug, Clone)]
pub struct TrackInfo {
    /// 1-based track number.
    pub number: u8,
    /// Frame offset of INDEX 01 relative to the disc data area start
    /// (i.e. *without* the 150-frame pregap).
    pub offset: u32,
    /// Track type string: `"AUDIO"`, `"MODE1"`, `"MODE2"`, etc.
    pub track_type: String,
}

// ── DiscTOC ───────────────────────────────────────────────────────────────────

/// Disc Table of Contents.
///
/// All frame positions stored here are *absolute* — they include the 150-frame
/// pregap that the Red Book specification mandates before track 1.
#[derive(Debug, Clone)]
pub struct DiscTOC {
    /// Number of the first track (usually 1).
    pub first_track: u8,
    /// Number of the last track.
    pub last_track: u8,
    /// Absolute frame offset of the lead-out (end of disc).
    pub lead_out: u32,
    /// Absolute frame offsets of each track's INDEX 01 position.
    pub track_offsets: Vec<u32>,
}

impl DiscTOC {
    /// Build a [`DiscTOC`] from a slice of [`TrackInfo`] and the raw lead-out
    /// frame count (without the 150-frame pregap).
    ///
    /// Returns `None` if `tracks` is empty.
    ///
    /// The 150-frame pregap is added to every track offset and to the lead-out
    /// so all values stored in the returned struct are absolute frame addresses.
    pub fn from_tracks(tracks: &[TrackInfo], lead_out_frames: u32) -> Option<Self> {
        let first = tracks.first()?;
        let last = tracks.last()?;
        Some(Self {
            first_track: first.number,
            last_track: last.number,
            lead_out: lead_out_frames + PREGAP_FRAMES,
            track_offsets: tracks.iter().map(|t| t.offset + PREGAP_FRAMES).collect(),
        })
    }

    /// Total disc playing time in whole seconds.
    pub fn total_seconds(&self) -> u32 {
        self.lead_out / FRAMES_PER_SECOND
    }

    /// Total disc playing time formatted as `"MM:SS"`.
    pub fn total_time_string(&self) -> String {
        let s = self.total_seconds();
        format!("{:02}:{:02}", s / 60, s % 60)
    }

    /// Number of tracks on the disc.
    pub fn track_count(&self) -> u8 {
        self.last_track.saturating_sub(self.first_track) + 1
    }

    /// Generate the MusicBrainz Disc ID string.
    ///
    /// The ID is computed by SHA-1 hashing a 402-byte binary record:
    ///
    /// ```text
    /// [first_track: u8] [last_track: u8] [lead_out: u32 BE]
    /// [offset_0: u32 BE] … [offset_98: u32 BE]   (always 99 slots, padded with 0)
    /// ```
    ///
    /// The resulting 20-byte hash is encoded with MusicBrainz's non-standard
    /// Base64 variant: standard Base64 (with `=` padding), then:
    /// `+` → `.`, `/` → `_`, `=` → `-`
    ///
    /// This produces a 28-character string, e.g. `"88mmRzPF.QgexLjFmYaEiNMUN44-"`.
    ///
    /// > **Note:** this differs from RFC 4648 base64url, which would produce a
    /// > 27-character string and use `-` (not `.`) for the position-62 character.
    ///
    /// Reference: <https://musicbrainz.org/doc/Disc_ID_Calculation>
    pub fn musicbrainz_id(&self) -> String {
        let mut data = Vec::with_capacity(2 + 4 + 99 * 4);
        data.push(self.first_track);
        data.push(self.last_track);
        data.extend_from_slice(&self.lead_out.to_be_bytes());
        for i in 0..99usize {
            let offset = self.track_offsets.get(i).copied().unwrap_or(0);
            data.extend_from_slice(&offset.to_be_bytes());
        }

        let hash = Sha1::digest(&data);

        // Standard Base64 WITH padding, then remap special chars to MB convention
        base64::engine::general_purpose::STANDARD
            .encode(hash.as_slice())
            .chars()
            .map(|c| match c {
                '+' => '.',
                '/' => '_',
                '=' => '-',
                other => other,
            })
            .collect()
    }

    /// Generate the FreeDB disc ID string.
    ///
    /// Returns an 8-character lowercase hexadecimal string computed via the
    /// legacy CDDB checksum algorithm.
    ///
    /// Reference: <http://ftp.freedb.org/pub/freedb/latest/CDDBPROTO>
    pub fn freedb_id(&self) -> String {
        let checksum: u32 = self
            .track_offsets
            .iter()
            .map(|&off| digit_sum(off / FRAMES_PER_SECOND))
            .sum();

        let total_seconds = self.lead_out / FRAMES_PER_SECOND;
        let first_seconds = self.track_offsets.first().copied().unwrap_or(0) / FRAMES_PER_SECOND;
        let length = total_seconds.saturating_sub(first_seconds);

        let disc_id = ((checksum % 0xFF) << 24) | (length << 8) | self.track_offsets.len() as u32;

        format!("{disc_id:08x}")
    }

    /// Generate a TOC string for MusicBrainz fuzzy lookup.
    ///
    /// Format: `first_track+track_count+leadout+offset1+offset2+…`
    ///
    /// Example: `"1+12+267257+150+22767+41887+…"`
    pub fn to_toc_string(&self) -> String {
        let mut parts = vec![
            self.first_track.to_string(),
            self.track_count().to_string(),
            self.lead_out.to_string(),
        ];
        for offset in &self.track_offsets {
            parts.push(offset.to_string());
        }
        parts.join("+")
    }
}

// ── MSF conversion helpers ────────────────────────────────────────────────────

/// Parse an `"MM:SS:FF"` MSF timestamp into a total frame count.
///
/// Returns an error if the string is malformed or contains non-numeric fields.
///
/// # Examples
///
/// ```
/// # use opticaldiscs::toc::parse_msf;
/// assert_eq!(parse_msf("00:00:00").unwrap(), 0);
/// assert_eq!(parse_msf("00:02:00").unwrap(), 150);   // 150-frame pregap
/// assert_eq!(parse_msf("01:00:00").unwrap(), 4500);
/// ```
pub fn parse_msf(s: &str) -> Result<u32> {
    let err = || OpticaldiscsError::Parse(format!("invalid MSF timestamp: {s:?}"));
    let mut parts = s.splitn(3, ':');
    let mm: u32 = parts.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    let ss: u32 = parts.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    let ff: u32 = parts.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    Ok((mm * 60 + ss) * FRAMES_PER_SECOND + ff)
}

/// Convert a total frame count into `(minutes, seconds, frames)`.
///
/// This is the inverse of [`parse_msf`].
///
/// # Examples
///
/// ```
/// # use opticaldiscs::toc::frames_to_msf;
/// assert_eq!(frames_to_msf(0),    (0, 0, 0));
/// assert_eq!(frames_to_msf(150),  (0, 2, 0));
/// assert_eq!(frames_to_msf(4500), (1, 0, 0));
/// ```
pub fn frames_to_msf(frames: u32) -> (u8, u8, u8) {
    let ff = (frames % FRAMES_PER_SECOND) as u8;
    let total_seconds = frames / FRAMES_PER_SECOND;
    let ss = (total_seconds % 60) as u8;
    let mm = (total_seconds / 60) as u8;
    (mm, ss, ff)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Sum of the decimal digits of `n` (used in the FreeDB checksum algorithm).
fn digit_sum(n: u32) -> u32 {
    let mut sum = 0;
    let mut remaining = n;
    loop {
        sum += remaining % 10;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    sum
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_msf ──────────────────────────────────────────────────────────────

    #[test]
    fn parse_msf_zero() {
        assert_eq!(parse_msf("00:00:00").unwrap(), 0);
    }

    #[test]
    fn parse_msf_pregap() {
        assert_eq!(parse_msf("00:02:00").unwrap(), 150);
    }

    #[test]
    fn parse_msf_one_minute() {
        assert_eq!(parse_msf("01:00:00").unwrap(), 4500);
    }

    #[test]
    fn parse_msf_maximum() {
        // 74:59:74 — the maximum addressable MSF on a CD
        assert_eq!(parse_msf("74:59:74").unwrap(), 337_499);
    }

    #[test]
    fn parse_msf_invalid_format() {
        assert!(parse_msf("00:00").is_err());
        assert!(parse_msf("notmsf").is_err());
        assert!(parse_msf("00:xx:00").is_err());
        assert!(parse_msf("").is_err());
    }

    // ── frames_to_msf ─────────────────────────────────────────────────────────

    #[test]
    fn frames_to_msf_zero() {
        assert_eq!(frames_to_msf(0), (0, 0, 0));
    }

    #[test]
    fn frames_to_msf_pregap() {
        assert_eq!(frames_to_msf(150), (0, 2, 0));
    }

    #[test]
    fn frames_to_msf_one_minute() {
        assert_eq!(frames_to_msf(4500), (1, 0, 0));
    }

    #[test]
    fn frames_to_msf_roundtrip() {
        for &msf in &["00:00:00", "00:02:00", "01:00:00", "74:59:74", "32:17:42"] {
            let frames = parse_msf(msf).unwrap();
            let (mm, ss, ff) = frames_to_msf(frames);
            let back = format!("{mm:02}:{ss:02}:{ff:02}");
            assert_eq!(back, msf, "roundtrip failed for {msf}");
        }
    }

    // ── DiscTOC construction ───────────────────────────────────────────────────

    #[test]
    fn from_tracks_basic() {
        let tracks = vec![
            TrackInfo {
                number: 1,
                offset: 0,
                track_type: "AUDIO".into(),
            },
            TrackInfo {
                number: 2,
                offset: 18_901,
                track_type: "AUDIO".into(),
            },
        ];
        let toc = DiscTOC::from_tracks(&tracks, 41_000).unwrap();

        assert_eq!(toc.first_track, 1);
        assert_eq!(toc.last_track, 2);
        assert_eq!(toc.track_count(), 2);
        // Pregap (150) is added to every offset and to the lead-out
        assert_eq!(toc.track_offsets, vec![150, 19_051]);
        assert_eq!(toc.lead_out, 41_150);
    }

    #[test]
    fn from_tracks_empty_returns_none() {
        assert!(DiscTOC::from_tracks(&[], 0).is_none());
    }

    #[test]
    fn from_tracks_single_track() {
        let tracks = vec![TrackInfo {
            number: 1,
            offset: 0,
            track_type: "MODE1".into(),
        }];
        let toc = DiscTOC::from_tracks(&tracks, 9_000).unwrap();
        assert_eq!(toc.track_count(), 1);
        assert_eq!(toc.track_offsets, vec![150]);
        assert_eq!(toc.lead_out, 9_150);
    }

    // ── Total time helpers ────────────────────────────────────────────────────

    #[test]
    fn total_seconds_and_time_string() {
        let tracks = vec![TrackInfo {
            number: 1,
            offset: 0,
            track_type: "AUDIO".into(),
        }];
        // 4500 frames = 60 seconds = 1 minute
        let toc = DiscTOC::from_tracks(&tracks, 4_350).unwrap(); // 4350 + 150 = 4500
        assert_eq!(toc.total_seconds(), 60);
        assert_eq!(toc.total_time_string(), "01:00");
    }

    // ── FreeDB ID ─────────────────────────────────────────────────────────────

    #[test]
    fn freedb_id_two_track_disc() {
        // Manually verified test vector:
        //   track_offsets = [150, 19051]  (after +150 pregap)
        //   lead_out = 41150
        //   digit_sum(150/75=2) = 2
        //   digit_sum(19051/75=254) = 2+5+4 = 11
        //   checksum = 13
        //   total_sec = 41150/75 = 548, first_sec = 150/75 = 2, length = 546
        //   disc_id = (13 << 24) | (546 << 8) | 2 = 0x0D022202
        let tracks = vec![
            TrackInfo {
                number: 1,
                offset: 0,
                track_type: "AUDIO".into(),
            },
            TrackInfo {
                number: 2,
                offset: 18_901,
                track_type: "AUDIO".into(),
            },
        ];
        let toc = DiscTOC::from_tracks(&tracks, 41_000).unwrap();
        assert_eq!(toc.freedb_id(), "0d022202");
    }

    #[test]
    fn freedb_id_single_data_track() {
        // Minimal data CD: 1 track, ~4 minutes of content
        let tracks = vec![TrackInfo {
            number: 1,
            offset: 0,
            track_type: "MODE1".into(),
        }];
        // 18000 frames = 240 seconds = 4 minutes, lead_out = 18150
        let toc = DiscTOC::from_tracks(&tracks, 18_000).unwrap();
        let id = toc.freedb_id();
        // Must be exactly 8 lowercase hex characters
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── MusicBrainz DiscID ────────────────────────────────────────────────────

    #[test]
    fn musicbrainz_id_two_track_disc() {
        // Same 2-track disc as the FreeDB test.
        // Expected value computed with Python:
        //   sha1(01 02 00 00 A0 9E 00 00 00 96 00 00 4A AB 00...00)
        //   = f3c9a64733c5f9081ec4b8c599868488d314378e
        //   → standard base64 → MB char replacement
        //   = "88mmRzPF.QgexLjFmYaEiNMUN44-"
        //
        // Note: the '.' in position 8 is the MB encoding of base64 char '+'.
        // URL_SAFE_NO_PAD (used by some other libs) would produce '-' there,
        // and would be 27 characters long — both are wrong for MusicBrainz.
        let tracks = vec![
            TrackInfo {
                number: 1,
                offset: 0,
                track_type: "AUDIO".into(),
            },
            TrackInfo {
                number: 2,
                offset: 18_901,
                track_type: "AUDIO".into(),
            },
        ];
        let toc = DiscTOC::from_tracks(&tracks, 41_000).unwrap();
        let id = toc.musicbrainz_id();
        assert_eq!(id, "88mmRzPF.QgexLjFmYaEiNMUN44-");
    }

    #[test]
    fn musicbrainz_id_is_28_chars() {
        // SHA-1 → 20 bytes → standard base64 = 28 chars (incl. padding)
        // After '=' → '-' replacement: always 28 chars.
        let tracks = vec![TrackInfo {
            number: 1,
            offset: 0,
            track_type: "AUDIO".into(),
        }];
        let toc = DiscTOC::from_tracks(&tracks, 9_000).unwrap();
        let id = toc.musicbrainz_id();
        assert_eq!(id.len(), 28, "MB DiscID must be exactly 28 characters");
    }

    #[test]
    fn musicbrainz_id_valid_char_set() {
        // MB DiscID uses A-Z a-z 0-9 . _ - (position-62 is '.' not '+')
        let tracks = vec![
            TrackInfo {
                number: 1,
                offset: 0,
                track_type: "AUDIO".into(),
            },
            TrackInfo {
                number: 2,
                offset: 18_901,
                track_type: "AUDIO".into(),
            },
        ];
        let toc = DiscTOC::from_tracks(&tracks, 41_000).unwrap();
        let id = toc.musicbrainz_id();
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'),
            "invalid char in MB DiscID: {id}"
        );
    }

    #[test]
    fn musicbrainz_id_deterministic() {
        let tracks = vec![TrackInfo {
            number: 1,
            offset: 0,
            track_type: "AUDIO".into(),
        }];
        let toc = DiscTOC::from_tracks(&tracks, 9_000).unwrap();
        assert_eq!(toc.musicbrainz_id(), toc.musicbrainz_id());
    }

    // ── digit_sum helper ──────────────────────────────────────────────────────

    #[test]
    fn digit_sum_values() {
        assert_eq!(digit_sum(0), 0);
        assert_eq!(digit_sum(1), 1);
        assert_eq!(digit_sum(9), 9);
        assert_eq!(digit_sum(10), 1);
        assert_eq!(digit_sum(123), 6);
        assert_eq!(digit_sum(999), 27);
        assert_eq!(digit_sum(254), 11); // used in FreeDB test above
    }
}
