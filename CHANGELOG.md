# Changelog

All notable changes to this crate are documented here. This project follows
[Semantic Versioning](https://semver.org/).

## 0.5.0 — Unreleased

> **⚠️ Breaking change — action required.** `FileEntry::type_code` and
> `FileEntry::creator_code` changed type from `Option<String>` to
> `Option<[u8; 4]>` (the raw Finder bytes, stored verbatim). **Downstream code
> that reads these fields must be updated.** Migration:
>
> - For display, replace `entry.type_code` (was a `String`) with
>   `entry.type_code_string()` (and `creator_code_string()`), which returns the
>   same `Option<String>` rendering as before (`"TEXT"`, or `"0x12345678"` for
>   non-printable codes).
> - For byte-exact use (e.g. re-emitting MacBinary/AppleDouble), use the raw
>   `entry.type_code` / `entry.creator_code` arrays directly.
> - `FileEntry::new_hfs_file` gained a trailing `finder_flags: u16` parameter.

### Changed (breaking)

- **`FileEntry::type_code` / `creator_code` are now `Option<[u8; 4]>`** instead
  of `Option<String>`. The previous design collapsed the Finder type/creator to
  a display string at parse time, discarding the raw bytes — so a downstream
  that re-emitted a forked Mac file (MacBinary / AppleDouble / BinHex) could not
  reproduce high-bit codes byte-for-byte. The raw bytes are now preserved; use
  the new `type_code_string()` / `creator_code_string()` helpers for display.
  (Synced from rusty-backup, which made the same fix.)

### Changed

- **Raised the `libchdman-rs` floor from `0.288.5` to `0.288.7`.** No source
  changes — the CHD API this crate uses is identical — but `0.288.7` adds
  prebuilt static archives for `aarch64-pc-windows-msvc` (Windows on ARM) and
  `riscv64gc-unknown-linux-gnu` (RISC-V 64), so those targets build without
  compiling MAME from source. Pinning the minimum guarantees the new prebuilts
  rather than relying on the resolver picking the latest `0.288.x`.

### Added

- **`FileEntry::finder_flags: Option<u16>`** — the HFS/HFS+ `FInfo.fdFlags`
  field (bits such as `isAlias` `0x8000`, `isInvisible` `0x4000`, `hasBundle`
  `0x2000`, `hasCustomIcon` `0x0400`). Previously parsed internally for alias
  detection but not exposed; now surfaced on every HFS/HFS+ file entry.
- `FileEntry::type_code_string()` / `creator_code_string()` — human-readable
  rendering of the raw type/creator codes (the previous `type_code` behaviour).

### Fixed

- **Mac OS Roman decoding table was shifted for bytes ≥ 0x9B.** The
  `MAC_ROMAN_TABLE` used to decode HFS volume and catalog (file/directory) names
  was missing `õ` (U+00F5) at byte `0x9B`, which shifted every mapping from
  `0x9B`–`0xFE` down one slot and left `0xFF` decoding to `U+FFFD`. As a result,
  HFS names containing common accented characters mis-decoded — e.g. `ü` (`0x9F`)
  rendered as `†`, `ú` (`0x9C`) as `ù`, and `©` (`0xA9`) as `™`. The table now
  matches the canonical Apple Mac OS Roman set. (Synced from rusty-backup.)
- **HFS+ names with a malformed UTF-16 unit no longer vanish.** UTF-16 BE name
  and volume-name decoding now uses a lossy conversion (`String::from_utf16_lossy`),
  so a single invalid/unpaired code unit becomes `U+FFFD` instead of discarding the
  entire name — which previously dropped the affected entry from directory listings
  and reset the volume name to the generic placeholder.

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

## 0.4.4 — 2026-05-20

### Added

- **ISO 9660 PVD date/time metadata.** New `PvdDateTime` struct (year, month, day,
  hour, minute, second, hundredths, `gmt_offset_quarter_hours`) with
  `PvdDateTime::parse(&[u8])` and `PvdDateTime::to_iso8601()`. `parse` handles the
  "not specified" encodings (all-`0x00` and all-ASCII-`'0'`), validates each digit
  group, and returns `None` on corrupt data. `to_iso8601()` renders e.g.
  `1997-03-18T16:45:47.00+00:00`, with the GMT offset shown as ±HH:MM from
  `gmt_offset_quarter_hours × 15`.
- Four `Option<PvdDateTime>` fields on `PrimaryVolumeDescriptor` —
  `creation_date`, `modification_date`, `expiration_date`, `effective_date` —
  parsed from PVD offsets 813 / 830 / 847 / 864.

  Additive only: no new dependencies, and existing fields/signatures are unchanged.

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

## 0.4.2 — 2026-05-19

First release published to [crates.io](https://crates.io/crates/opticaldiscs).

### Changed

- Bumped the optional `sha1` dependency (used by the `toc` feature) to `0.11`.

### Fixed

- CI publishing workflow fixes so the crate publishes cleanly to crates.io.
