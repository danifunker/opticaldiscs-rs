//! Container-independent description of a disc's tracks.
//!
//! A CD is a sequence of tracks, only some of which need be data. Every
//! container that records a track table — BIN/CUE, CloneCD, Nero, Alcohol,
//! DiscJuggler, DAEMON Tools, CHD — is reduced to a `Vec<DiscTrack>` on
//! [`DiscImageInfo::tracks`](crate::detect::DiscImageInfo::tracks), so a caller
//! can render "1 data + 12 audio" without knowing which container it opened.

use crate::bincue::{frames_to_msf, TrackType};

/// One track of a disc, as described by its container's track table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscTrack {
    /// Track number as written in the track table (1-based).
    pub number: u32,
    /// Sector format of the track.
    pub track_type: TrackType,
    /// Absolute start sector of the track's `INDEX 01`, counted from the start
    /// of the disc — so track 1 of an ordinary disc starts at 0.
    ///
    /// This is a logical block address, *not* a Red Book absolute address: add
    /// the 150-frame lead-in for the latter, as [`crate::toc::DiscTOC`] does.
    pub start_lba: u64,
    /// Track length in sectors, or `0` when the container does not record it and
    /// it cannot be derived (a truncated or partially-present image).
    ///
    /// Measured from this track's start to the next track's, so an in-file
    /// pregap (`INDEX 00`) counts toward the track before it — the same
    /// convention [`crate::toc::DiscTOC`] uses for its offsets.
    pub length_sectors: u64,
}

impl DiscTrack {
    /// True for Red Book audio tracks.
    pub fn is_audio(&self) -> bool {
        self.track_type.is_audio()
    }

    /// True for data tracks — the ones that can carry a filesystem.
    pub fn is_data(&self) -> bool {
        self.track_type.is_data()
    }

    /// CUE sheet format string for this track (`"AUDIO"`, `"MODE1/2352"`, …).
    pub fn cue_label(&self) -> &'static str {
        self.track_type.cue_label()
    }

    /// Start position as `(minutes, seconds, frames)`.
    ///
    /// This is the MSF you would write in a CUE `INDEX 01` line for a
    /// single-file image; a CD player's display adds the 150-frame lead-in.
    /// Minutes are a `u32` because a DVD-sized image runs past the 255 minutes
    /// a byte would hold.
    pub fn start_msf(&self) -> (u32, u8, u8) {
        msf(self.start_lba)
    }

    /// Playing time as `(minutes, seconds, frames)`, or `None` if the length is
    /// unknown.
    pub fn duration_msf(&self) -> Option<(u32, u8, u8)> {
        (self.length_sectors > 0).then(|| msf(self.length_sectors))
    }

    /// Playing time in seconds, or `None` if the length is unknown.
    pub fn duration_seconds(&self) -> Option<f64> {
        (self.length_sectors > 0).then(|| self.length_sectors as f64 / 75.0)
    }
}

/// `frames_to_msf` with minutes widened past the 255 a `u8` holds.
fn msf(frames: u64) -> (u32, u8, u8) {
    let (_, ss, ff) = frames_to_msf(frames);
    ((frames / 75 / 60) as u32, ss, ff)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(track_type: TrackType, start_lba: u64, length_sectors: u64) -> DiscTrack {
        DiscTrack {
            number: 1,
            track_type,
            start_lba,
            length_sectors,
        }
    }

    #[test]
    fn audio_and_data_are_complementary() {
        let audio = track(TrackType::Audio, 0, 100);
        assert!(audio.is_audio() && !audio.is_data());
        assert_eq!(audio.cue_label(), "AUDIO");

        let data = track(TrackType::Mode1Raw, 0, 100);
        assert!(data.is_data() && !data.is_audio());
        assert_eq!(data.cue_label(), "MODE1/2352");
    }

    #[test]
    fn msf_matches_the_cue_index_position() {
        // 01:23:45 = 1*60*75 + 23*75 + 45
        let t = track(TrackType::Audio, 60 * 75 + 23 * 75 + 45, 0);
        assert_eq!(t.start_msf(), (1, 23, 45));
    }

    #[test]
    fn msf_minutes_run_past_a_byte() {
        // 300 minutes would wrap a u8 (300 - 256 = 44).
        let t = track(TrackType::Mode1Cooked, 300 * 60 * 75, 0);
        assert_eq!(t.start_msf(), (300, 0, 0));
    }

    #[test]
    fn unknown_length_reports_no_duration() {
        let t = track(TrackType::Audio, 0, 0);
        assert_eq!(t.duration_msf(), None);
        assert_eq!(t.duration_seconds(), None);
    }

    #[test]
    fn duration_from_length() {
        let t = track(TrackType::Audio, 150, 75 * 90);
        assert_eq!(t.duration_msf(), Some((1, 30, 0)));
        assert_eq!(t.duration_seconds(), Some(90.0));
    }
}
