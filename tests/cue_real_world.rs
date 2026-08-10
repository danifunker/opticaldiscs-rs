//! CUE sheets in the shapes real media ships them in.
//!
//! Each fixture in `tests/fixtures/*.cue` pins one shape that a stricter parser
//! rejects. They were all found by opening actual discs:
//!
//! - `unpadded-track-number.cue` — a retail CD-ROM (Microsoft Bookshelf) whose
//!   mastering tool wrote `TRACK 1` rather than the spec's `TRACK 01`.
//! - `cdda-no-data-track.cue` — a plain audio CD, which has no data track by
//!   definition and so must open without one.
//! - `eac-audio-cd.cue` — an Exact Audio Copy rip, carrying `CATALOG` and
//!   multi-word `REM` values.
//!
//! A fixture only holds the sheet; the data file it names is synthesized into a
//! temp directory next to a copy of the sheet, so the repository stays small.
//!
//! `tests/fixtures/` is excluded from the published crate (see `package.exclude`
//! — the release pipeline enforces a slim tarball), so the fixture-backed tests
//! below skip when it is absent, as the CHD fixture tests in
//! `integration_test.rs` do. The behaviour they assert is also covered by tests
//! that build their sheet inline, which run everywhere.

use std::path::{Path, PathBuf};

use opticaldiscs::detect::DiscImageInfo;
use opticaldiscs::{DiscFormat, FilesystemType};

const RAW_SECTOR: usize = 2352;

/// Copy `fixture` into a fresh temp dir alongside a synthesized data file.
///
/// `None` when the fixture is not present — see the module note.
fn stage(fixture: &str, data_name: &str, data: &[u8]) -> Option<(tempfile::TempDir, PathBuf)> {
    let cue_src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    if !cue_src.exists() {
        return None;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let cue_path = dir.path().join(fixture);
    std::fs::copy(&cue_src, &cue_path).unwrap_or_else(|e| panic!("copy {fixture}: {e}"));
    std::fs::write(dir.path().join(data_name), data).expect("write data file");
    Some((dir, cue_path))
}

/// Stage a fixture, or report a skip and return from the test.
macro_rules! stage_or_skip {
    ($test:literal, $fixture:expr, $data_name:expr, $data:expr) => {
        match stage($fixture, $data_name, $data) {
            Some(staged) => staged,
            None => {
                eprintln!("SKIP {}: tests/fixtures/{} not found", $test, $fixture);
                return;
            }
        }
    };
}

/// `sectors` raw MODE1/2352 sectors carrying an ISO 9660 PVD at sector 16.
fn mode1_raw_image(volume_label: &str, sectors: usize) -> Vec<u8> {
    let mut bin = vec![0u8; sectors * RAW_SECTOR];
    let pvd = opticaldiscs::iso9660::build_test_pvd_sector(volume_label, 18, 2048);
    // A raw sector is a 16-byte sync/header followed by 2048 bytes of user data.
    let user = 16 * RAW_SECTOR + 16;
    bin[user..user + 2048].copy_from_slice(&pvd);
    bin
}

/// `sectors` sectors of silence — enough to give an audio track a length.
fn silence(sectors: usize) -> Vec<u8> {
    vec![0u8; sectors * RAW_SECTOR]
}

// ── Issue 1: unpadded track and index numbers ─────────────────────────────────

#[test]
fn unpadded_track_number_opens() {
    let (_dir, cue) = stage_or_skip!(
        "unpadded_track_number_opens",
        "unpadded-track-number.cue",
        "BOOKSHELF.img",
        &mode1_raw_image("BOOKSHELF", 20)
    );

    let info = DiscImageInfo::open(&cue).expect("`TRACK 1` is as valid as `TRACK 01`");
    assert_eq!(info.format, DiscFormat::BinCue);
    assert_eq!(info.filesystem, FilesystemType::Iso9660);
    assert_eq!(info.volume_label.as_deref(), Some("BOOKSHELF"));
}

#[test]
fn padding_is_the_only_difference() {
    // The same disc, one byte apart: zero-padding the numbers must not change
    // anything the caller sees.
    let (dir, unpadded) = stage_or_skip!(
        "padding_is_the_only_difference",
        "unpadded-track-number.cue",
        "BOOKSHELF.img",
        &mode1_raw_image("BOOKSHELF", 20)
    );
    let padded = dir.path().join("padded.cue");
    let text = std::fs::read_to_string(&unpadded).unwrap();
    std::fs::write(
        &padded,
        text.replace("TRACK 1 ", "TRACK 01 ")
            .replace("INDEX 1 ", "INDEX 01 "),
    )
    .unwrap();

    let a = DiscImageInfo::open(&unpadded).unwrap();
    let b = DiscImageInfo::open(&padded).unwrap();
    assert_eq!(a.filesystem, b.filesystem);
    assert_eq!(a.volume_label, b.volume_label);
    assert_eq!(a.tracks, b.tracks);
}

// ── Issue 2: audio-only discs ─────────────────────────────────────────────────

#[test]
fn audio_only_disc_opens() {
    let (_dir, cue) = stage_or_skip!(
        "audio_only_disc_opens",
        "cdda-no-data-track.cue",
        "cdda-noaudiodata.bin",
        &silence(1000)
    );

    let info = DiscImageInfo::open(&cue).expect("audio-only is a valid disc");

    assert!(info.tracks.iter().all(|t| t.is_audio()));
    assert!(info.filesystem.is_none());
    assert!(info.is_audio_only());
    assert_eq!(info.format, DiscFormat::BinCue);
    // Nothing a filesystem would have filled in.
    assert!(info.volume_label.is_none());
    assert!(info.pvd.is_none());
}

#[test]
fn audio_only_disc_lists_its_tracks() {
    let (_dir, cue) = stage_or_skip!(
        "audio_only_disc_lists_its_tracks",
        "cdda-no-data-track.cue",
        "cdda-noaudiodata.bin",
        &silence(1000)
    );
    let info = DiscImageInfo::open(&cue).unwrap();

    assert_eq!(info.tracks.len(), 2);
    assert_eq!(info.tracks[0].number, 1);
    assert_eq!(info.tracks[0].start_lba, 0);
    assert_eq!(info.tracks[0].cue_label(), "AUDIO");

    // Track 2 at INDEX 01 00:05:25 = 5*75 + 25 = 400 frames, running to the
    // end of the 1000-sector file.
    assert_eq!(info.tracks[1].number, 2);
    assert_eq!(info.tracks[1].start_lba, 400);
    assert_eq!(info.tracks[1].start_msf(), (0, 5, 25));
    assert_eq!(info.tracks[1].length_sectors, 600);
    assert_eq!(info.tracks[1].duration_msf(), Some((0, 8, 0)));
}

#[test]
fn audio_only_disc_cannot_be_browsed() {
    // Opening describes the disc; there is still no filesystem on it.
    let (_dir, cue) = stage_or_skip!(
        "audio_only_disc_cannot_be_browsed",
        "cdda-no-data-track.cue",
        "cdda-noaudiodata.bin",
        &silence(1000)
    );
    let info = DiscImageInfo::open(&cue).unwrap();
    let err = opticaldiscs::browse::open_disc_filesystem(&info)
        .err()
        .expect("an audio CD has nothing to browse");
    assert!(err.to_string().contains("audio-only"), "{err}");
}

#[cfg(feature = "toc")]
#[test]
fn audio_only_disc_still_has_a_toc() {
    // The TOC is what an audio CD is *for* — MusicBrainz lookups need it.
    let (_dir, cue) = stage_or_skip!(
        "audio_only_disc_still_has_a_toc",
        "cdda-no-data-track.cue",
        "cdda-noaudiodata.bin",
        &silence(1000)
    );
    let info = DiscImageInfo::open(&cue).unwrap();
    let toc = info.toc.expect("audio CD TOC");
    assert_eq!(toc.first_track, 1);
    assert_eq!(toc.last_track, 2);
    // DiscTOC offsets are absolute Red Book addresses: LBA + the 150-frame lead-in.
    assert_eq!(toc.track_offsets, vec![150, 550]);
    assert_eq!(toc.lead_out, 1150);
}

// ── Mixed mode: the audio tracks were previously invisible ───────────────────

#[test]
fn mixed_mode_disc_surfaces_its_audio_tracks() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("mixed.bin");
    std::fs::write(&bin, mode1_raw_image("MIXED_MODE", 10_000)).unwrap();
    let cue = dir.path().join("mixed.cue");
    std::fs::write(
        &cue,
        "FILE \"mixed.bin\" BINARY\n\
         \x20 TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n\
         \x20 TRACK 02 AUDIO\n    INDEX 01 01:00:00\n\
         \x20 TRACK 03 AUDIO\n    INDEX 01 02:00:00\n",
    )
    .unwrap();

    let info = DiscImageInfo::open(&cue).unwrap();
    assert_eq!(info.filesystem, FilesystemType::Iso9660);
    assert!(!info.is_audio_only());

    let data = info.tracks.iter().filter(|t| t.is_data()).count();
    let audio = info.tracks.iter().filter(|t| t.is_audio()).count();
    assert_eq!((data, audio), (1, 2));
    assert_eq!(info.tracks[1].start_lba, 4500);
    assert_eq!(info.tracks[2].start_lba, 9000);
}

// ── Exact Audio Copy sheets ───────────────────────────────────────────────────

#[test]
fn eac_sheet_with_catalog_and_multiword_remarks_opens() {
    // `REM GENRE Alternative Rock` used to be read as a remark followed by a
    // stray `Rock` command, which failed the whole sheet.
    let (_dir, cue) = stage_or_skip!(
        "eac_sheet_with_catalog_and_multiword_remarks_opens",
        "eac-audio-cd.cue",
        "eac-audio-cd.bin",
        &silence(1000)
    );

    let info = DiscImageInfo::open(&cue).expect("EAC sheets are ordinary sheets");
    assert!(info.is_audio_only());
    assert_eq!(info.tracks.len(), 2);
    // Track 2's INDEX 00 is its pregap; INDEX 01 at 00:07:00 = 525 frames is
    // where the track proper starts.
    assert_eq!(info.tracks[1].start_lba, 525);
}

// ── Acceptance, without fixtures ──────────────────────────────────────────────
//
// The tests above pin the exact bytes of real-world sheets, which means they
// skip wherever `tests/fixtures/` is not shipped. These build their sheet inline
// so the two fixed behaviours are verified everywhere, including from a
// published crate.

/// Write `body` as a `.cue` beside a data file, and open it.
fn open_inline(body: &str, data_name: &str, data: &[u8]) -> (tempfile::TempDir, DiscImageInfo) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(data_name), data).expect("write data file");
    let cue = dir.path().join("disc.cue");
    std::fs::write(&cue, body).expect("write cue");
    let info = DiscImageInfo::open(&cue).expect("open");
    (dir, info)
}

#[test]
fn acceptance_unpadded_numbers_open() {
    let (_dir, info) = open_inline(
        "FILE \"disc.img\" BINARY\n   TRACK 1 MODE1/2352\n   INDEX 1 00:00:00\n",
        "disc.img",
        &mode1_raw_image("UNPADDED", 20),
    );
    assert_eq!(info.filesystem, FilesystemType::Iso9660);
    assert_eq!(info.volume_label.as_deref(), Some("UNPADDED"));
}

#[test]
fn acceptance_audio_only_opens_with_no_filesystem() {
    let (_dir, info) = open_inline(
        "FILE \"disc.bin\" BINARY\n\
         \x20 TRACK 01 AUDIO\n    INDEX 01 00:00:00\n\
         \x20 TRACK 02 AUDIO\n    INDEX 01 00:05:25\n",
        "disc.bin",
        &silence(1000),
    );
    assert!(info.tracks.iter().all(|t| t.is_audio()));
    assert!(info.filesystem.is_none());
    assert!(info.is_audio_only());
}

#[test]
fn acceptance_multiword_rem_does_not_break_the_sheet() {
    let (_dir, info) = open_inline(
        "REM GENRE Alternative Rock\n\
         CATALOG 0000000000000\n\
         FILE \"disc.bin\" BINARY\n\
         \x20 TRACK 01 AUDIO\n    INDEX 01 00:00:00\n",
        "disc.bin",
        &silence(1000),
    );
    assert!(info.is_audio_only());
}
