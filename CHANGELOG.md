# Changelog

All notable changes to this crate are documented here. This project follows
[Semantic Versioning](https://semver.org/).

## 0.4.5 — 2026-06-26

### Changed

- **Bumped `libchdman-rs` from `0.287.0-l7` to `0.288.5`.** No source changes —
  the CHD API surface this crate uses (`Chd::open`, `cd::CdCookedReader::open` /
  `open_track`, `cd::list_tracks`, `cd::TrackType`) is identical across the two
  versions. The motivation is cross-compilation: `0.287.0-l7` ships no
  `armv7-unknown-linux-gnueabihf` prebuilt, whereas `0.288.5` does
  (`...-glibc2.31.a`), so downstreams that target MiSTer FPGA / Cortex-A9
  (e.g. rusty-backup's `rb-cli-mini`) can now build the optical stack. Bumping
  also lets a downstream that already uses `libchdman-rs 0.288.x` for CHD
  *creation* dedupe onto a single libchdman copy.

## 0.4.3 — 2026-05-20

### Fixed

- **Multi-FILE BIN/CUE TOC offsets are now absolute disc positions.**
  `build_bincue_toc()` previously used each track's local `file_byte_offset`
  (the per-file `INDEX 01` pregap) as the disc offset, which is correct for
  single-BIN CUEs but produced non-monotonic offsets — and a lead-out that only
  reflected the last BIN — for multi-FILE CUEs (one `.bin` per track, common in
  redump-style dumps). The resulting `DiscTOC::to_toc_string()` /
  `musicbrainz_id()` values were rejected by MusicBrainz with `400 Bad Request —
  "Invalid TOC"`.

  The builder now accumulates a running frame total across BIN files so each
  track's offset is `running_frames + local_index_01_frames`, and the lead-out
  is the summed sector count of every BIN. Single-BIN CUEs (where every track
  shares one file) keep identical output.

  Downstream consumers (e.g. `ODE-artwork-downloader`) should bump their
  `opticaldiscs` dependency to `0.4.3` to fix MusicBrainz lookups on per-track
  BIN dumps.
