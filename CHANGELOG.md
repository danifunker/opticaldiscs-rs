# Changelog

All notable changes to this crate are documented here. This project follows
[Semantic Versioning](https://semver.org/).

## 0.8.0

> **⚠️ Breaking change.** `DiscFormat` gained `Gdi` and `Nintendo`; `FilesystemType`
> gained `GameCube`, `Wii`, `Cdi`, and `Opera`. Code that exhaustively matches
> either enum must handle them (or add a wildcard arm). `DiscImageInfo` also gained
> a `game: Option<GameDiscInfo>` field.

### Added — video-game optical disc support

- **Game-disc identification** (`gameid.rs`): a new `GameDiscInfo { console, serial,
  title, region, maker, version }` on `DiscImageInfo`, populated by signature probes
  that work across ISO/BIN·CUE/CHD. Recognizes PlayStation 1 & 2 (`SYSTEM.CNF`
  BOOT/BOOT2, incl. PS2 UDF-bridge DVDs), Sega Saturn / Mega-CD / Dreamcast (IP.BIN
  headers), NEC PC-FX & PC Engine CD, Amiga CD32/CDTV, SNK Neo Geo CD, 3DO, CD-i,
  and Nintendo GameCube / Wii, with serial normalization and region tables.
- **GameCube & Wii browsing** (`browse/nod_fs.rs`) via the [`nod`](https://github.com/encounter/nod)
  crate (MIT/Apache-2.0): ISO/GCM, RVZ/WIA, WBFS, CISO, GCZ, TGC, and NFS, including
  Wii AES partition decryption. `nod` bundles the Wii common keys, so **no
  user-supplied key file is required** (it does transitively include Nintendo's
  common key). `DiscFormat::Nintendo`, `FilesystemType::{GameCube, Wii}`.
- **Dreamcast GD-ROM** browsing of the high-density game area, for both CHD and the
  new `.gdi` container (`gdi.rs`). New `GdromSectorReader` rebases the absolute
  HD-area LBAs (≥ 45000) so the ISO 9660 browser reads the game filesystem;
  `ChdInfo::is_gdrom()` / `find_gdrom_hd_track()`. `DiscFormat::Gdi`.
- **Philips CD-i** (Green Book) browser (`browse/cdi.rs`, `FilesystemType::Cdi`):
  `"CD-I "` detection, big-endian directory records, M-type path-table root lookup,
  and System-Use attribute decoding (directory bit `0x8000`).
- **3DO Opera** filesystem browser (`browse/opera.rs`, `FilesystemType::Opera`):
  block-0 volume header, block-based directory tree, and avatar-list file reads.
- `examples/inspect_disc.rs`: a general disc inspector (container, filesystem,
  game-console identity, and a directory listing).

See [`docs/GameDiscs_Implementation.md`](docs/GameDiscs_Implementation.md) for the
full design and per-console format details. Parsers were reverse-engineered from and
validated against real discs; committed tests use synthetic fixtures (no game data —
and, for Nintendo, never derived from a real game).

## 0.7.0

> **⚠️ Breaking change.** `FilesystemType` gained new variants (`HighSierra`,
> `Ufs`, `Ods2`). Code that exhaustively matches `FilesystemType` must handle
> them (or add a wildcard arm).

### Added

- **High Sierra Format** (the pre-ISO 9660 1986 CD-ROM filesystem, `CDROM`
  identifier). `PrimaryVolumeDescriptor::parse` auto-detects it and reads the
  High-Sierra field offsets; the ISO 9660 browser handles its directory records
  (file flags at offset 24). New `FilesystemType::HighSierra`.
- **Raw 2352-byte-sector auto-detection** in a bare `.iso`. `IsoSectorReader`
  recognises the `00 FF…FF 00` sync header and transparently strips the raw
  sync/header (Mode 1 and Mode 2 Form 1), so raw dumps saved with an `.iso`
  extension browse through the normal ISO/Joliet/Rock Ridge reader.
- **UFS1 / FFS browser** (`browse/ufs.rs`, `FilesystemType::Ufs`) for BSD Fast
  File System discs: Digital UNIX / Tru64 (little-endian) and SunOS/Solaris
  (big-endian), endianness auto-detected from the superblock. Cylinder-group
  inode location, direct + single/double/triple-indirect blocks, the pre-4.4
  (OFSFMT) directory format, symlink targets, and special-inode handling. Also
  reads **NeXTSTEP / OpenStep / Rhapsody** FFS wrapped in a `dlV` disk label
  (partition base auto-located; NeXT keeps big-endian FFS even on Intel).
- **UDF browser** (`browse/udf.rs`, `FilesystemType::Udf`) for DVD / data-disc
  Universal Disk Format (physical-partition layout, UDF 1.02–2.01): AVDP →
  Volume Descriptor Sequence → File Set Descriptor → File Entry / Extended File
  Entry → File Identifier Descriptors. Short/long/inline allocation descriptors,
  OSTA CS0 (8- and 16-bit) filename decoding, UDF/ISO **bridge** discs present
  the UDF tree. Blu-ray / UDF 2.50+ metadata-partition discs are detected and
  reported (not yet browsable).
- **VMS ODS-2 / Files-11 browser** (`browse/ods2.rs`, `FilesystemType::Ods2`)
  for OpenVMS (VAX/Alpha) discs: home block → index-file headers → retrieval
  pointers (formats 1/2/3) → file data; directory records resolve name+version
  to File-IDs. VMS `;version` suffixes preserved.

## 0.6.0 — Unreleased

> **⚠️ Breaking change — action required.** `FileEntry` gained two new public
> fields, `timestamps: Option<FileTimestamps>` and `posix: Option<PosixMetadata>`.
> Code that constructs `FileEntry` with a struct literal, or that exhaustively
> matches it, must account for them. (The `new_*` constructors are unchanged and
> default both to `None`.)

### Added

- **Per-file timestamps on `FileEntry`** via the new `timestamps:
  Option<FileTimestamps>` field. Dates are exposed **raw and untranslated**,
  tagged by filesystem, so consumers can re-emit or convert them losslessly:
  - `FileTimestamps::Hfs { created, modified, backup }` — secs since 1904 (local).
  - `FileTimestamps::HfsPlus { created, content_modified, attribute_modified,
    accessed, backup }` — secs since 1904 (GMT).
  - `FileTimestamps::Iso9660 { recorded, created, modified, accessed }` — the
    directory-record recording time plus optional Rock Ridge `TF` times.
  - `FileTimestamps::Unix { atime, mtime, ctime }` — EFS inode times (secs since
    1970).
  - Helper `MAC_EPOCH_UNIX_OFFSET` and `Iso9660DateTime::to_iso8601()` for display.
- **POSIX ownership/permissions on `FileEntry`** via `posix: Option<PosixMetadata>`
  (`mode`, `uid`, `gid`), populated from HFS+ `BSDInfo`, EFS inodes, and ISO 9660
  Rock Ridge `PX`. Helpers `permission_bits()` and `is_symlink()`.
- **Volume-level dates completed.** `MasterDirectoryBlock` gained
  `modification_date` and `backup_date`; `HfsPlusVolumeHeader` gained
  `create_date`, `modify_date`, `backup_date`, and `checked_date` (all secs since
  1904). Reachable via `DiscImageInfo::hfs_mdb` / `hfsplus_header`.
- **Joliet support.** The ISO 9660 browser now scans the volume-descriptor set
  for a Joliet Supplementary Volume Descriptor and, when present, browses the
  Joliet tree with UTF-16BE (Unicode) names. New public `JolietVolumeDescriptor`.
- **Rock Ridge / SUSP support.** POSIX metadata (`PX`), long/alternate names
  (`NM`), symlink targets (`SL`), and timestamps (`TF`) are read from the System
  Use area, following `CE` continuation areas. When a disc has both Rock Ridge
  and Joliet, the Rock Ridge (primary) tree is preferred for metadata fidelity.
- New public `Iso9660DateTime` (the 7-byte binary recording-date form) and
  re-exports of `PrimaryVolumeDescriptor` / `PvdDateTime`.

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

- **Raised the `libchdman-rs` floor from `0.288.5` to `0.288.8`.** No source
  changes — the CHD API this crate uses is identical. `0.288.8` matters for two
  reasons:
  - **License compatibility.** `libchdman-rs` ≤ 0.288.7 declared `GPL-2.0-only`,
    which is incompatible with this crate's `GPL-3.0` (a latent conflict since
    `0.4.0`). `0.288.8` is relicensed `BSD-3-Clause` — matching the MAME chd
    source it wraps — which is compatible with GPL-3.0 (and the AGPL-3.0
    downstream). opticaldiscs stays GPL-3.0.
  - **New targets.** `0.288.7`+ ships prebuilt static archives for
    `aarch64-pc-windows-msvc` (Windows on ARM) and `riscv64gc-unknown-linux-gnu`
    (RISC-V 64), so those build without compiling MAME from source.

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
