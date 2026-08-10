# opticaldiscs — Implementation Plan

A format-agnostic Rust library for reading and browsing optical disc images
(ISO, BIN/CUE, CHD), with physical CD/DVD/Blu-ray drive enumeration.

---

## License

This library is licensed **MIT** (since 0.15.0; 0.14.0 and earlier were GPL-3.0).

---

## Goals

1. A single `SectorReader` abstraction that handles the cooked/raw sector translation
   across `.iso`, `.bin`+`.cue`, and `.chd` containers — this is the core value that
   no existing crate provides.
2. ISO 9660, HFS, and HFS+ filesystem browsing on top of that abstraction.
3. Physical optical drive enumeration (feature-flagged, platform-specific).
4. All code is GUI-free and application-agnostic; consumers add their own
   application layers on top.

---

## Final Module Layout

```
opticaldiscs/
  src/
    lib.rs              — public API, feature flags, re-exports
    error.rs            — unified OpticaldiscsError (thiserror)
    formats.rs          — DiscFormat enum, FilesystemType enum
    detect.rs           — probe format + filesystem type from a file path
    sector_reader.rs    — SectorReader trait + IsoSectorReader, BinCueSectorReader,
                          ChdSectorReader
    iso9660.rs          — PVD parsing, ISO9660 directory record types
    bincue.rs           — CUE sheet parsing, BIN track layout, single-BIN writer
    chd.rs              — CHD metadata (CHT2 tag), track extraction
    toc.rs              — DiscTOC, TrackInfo, MSF/frame conversion,
                          MusicBrainz DiscID, FreeDB ID  [feature = "toc"]
    apm.rs              — Apple Partition Map detection and partition offset
    hfs.rs              — HFS Master Directory Block
    hfsplus.rs          — HFS+ volume header, catalog B-tree root lookup
    sgi.rs              — SGI Volume Header (IRIX disc partition table)
    efs.rs              — SGI EFS on-disk structures (superblock, inode, extent)
    drives.rs           — OpticalDrive struct, list_drives()  [feature = "drives"]
    browse/
      mod.rs            — open_disc_filesystem() dispatcher
      filesystem.rs     — Filesystem trait, FilesystemError
      entry.rs          — FileEntry, EntryType
      iso9660.rs        — ISO9660 directory listing + file reading
      hfs.rs            — HFS filesystem browser
      hfsplus.rs        — HFS+ filesystem browser
      efs.rs            — SGI EFS filesystem browser
      mac_alias.rs      — HFS/HFS+ alias-record (symlink target) resolution
```

> EFS / SGI support (the `sgi.rs`, `efs.rs`, and `browse/efs.rs` modules) was added
> after the original 12-phase plan as the `0.3.0` release. Its own implementation
> plan lives in [docs/EFS_Implementation.md](docs/EFS_Implementation.md).

---

## Dependencies

```toml
[dependencies]
thiserror    = "2"
log          = "0.4"
cue_sheet    = "0.3"                              # CUE sheet text parsing
libchdman-rs = { version = "0.288.8", features = ["prebuilt"] }  # CHD reading via MAME chd_file

# Optional — only with feature "toc"
sha1       = { version = "0.11", optional = true }
base64     = { version = "0.22", optional = true }

[features]
default = []
toc    = ["dep:sha1", "dep:base64"]   # MusicBrainz + FreeDB DiscID calculation
drives = []                            # physical drive enumeration (platform-specific)
```

> **Note:** CHD reading went through one swap. The original plan used the pure-Rust
> `chd` crate; from `0.4.0` onward the crate reads CHDs via
> [`libchdman-rs`](https://github.com/danifunker/libchdman-rs) (a binding over MAME's
> official `chd_file` core) for byte-for-byte parity with `chdman`. See the README's
> "CHD support" section and CHANGELOG for the prebuilt-archive details.

---

## Consumers

This library is application- and GUI-agnostic; downstream projects build their own
layers on top. Known consumers:

- **[ODE-artwork-downloader](https://github.com/danifunker/ODE-artwork-downloader)**
  — disc detection, ISO/HFS browsing, and TOC/DiscID for cover-art lookup.
- **[rusty-backup](https://github.com/danifunker/rusty-backup)** — physical-drive
  enumeration and the sector readers, for disc ripping/conversion and an
  optical-disc browse view.

How each consumer integrates with (or migrates onto) this crate is tracked in that
project's own repository, not in this plan.

---

## Stages

Each stage is independently buildable and testable. Check off items as they are
completed. Stages within a phase can be done in any order; phases should be done
in sequence because later phases build on earlier ones.

Progress key: `[ ]` pending · `[x]` done · `[-]` skipped/deferred

---

### Phase 0 — Repository Scaffold ✅
**Goal:** Working Cargo library that compiles with `cargo build` and `cargo test`.

- [x] **0.1** Initialize `Cargo.toml` as a library crate (`opticaldiscs`; GPL-3.0 at
              the time, MIT since 0.15.0)
- [x] **0.2** Create `src/lib.rs` with module stubs and feature-flag gates
- [x] **0.3** Add `.gitignore` entries for `target/`, `*.swp`, `Cargo.lock` (lib convention)
- [x] **0.4** Add `README.md` (crate description, quick example, license badge)
- [x] **0.5** Set up GitHub Actions CI: `cargo build`, `cargo test`, `cargo clippy`,
              `cargo fmt --check`
- [x] **0.6** Add `tests/` directory with a placeholder integration test

**Deliverable:** `cargo build` passes, CI green, repo is ready for code.

---

### Phase 1 — Core Types ✅
**Goal:** The shared enums, error type, and entry type that everything else depends on.

- [x] **1.1** `src/formats.rs`
  - `DiscFormat` enum: `Iso`, `BinCue`, `Chd`, `MdsMdf`
  - `FilesystemType` enum: `Iso9660`, `Joliet`, `Udf`, `Hfs`, `HfsPlus`, `Unknown`
  - `DiscFormat::from_path()` extension detection
  - `supported_extensions()` helper

- [x] **1.2** `src/error.rs`
  - `OpticaldiscsError` (thiserror) covering IO, parse, format, unsupported, not-found

- [x] **1.3** `src/browse/entry.rs`
  - `FileEntry` struct: name, path, entry_type, size, location, children
  - `EntryType` enum: `File`, `Directory`
  - `new_file()`, `new_directory()`, `root()`, `size_string()` helpers

- [x] **1.4** `src/browse/filesystem.rs`
  - `Filesystem` trait: `root()`, `list_directory()`, `read_file()`,
    `read_file_range()`, `volume_name()`
  - `FilesystemError` (thiserror)

- [x] **1.5** Wire up `src/lib.rs` public re-exports for all Phase 1 types

- [x] **1.6** Unit tests for `DiscFormat::from_path()`, `FileEntry` helpers
  - 9 unit tests in `browse/entry.rs`, 1 unit test in `formats.rs`
  - 5 integration tests in `tests/integration_test.rs`

**Deliverable:** Types compile; downstream code can depend on `opticaldiscs::formats`,
`opticaldiscs::browse::entry`, etc.

---

### Phase 2 — SectorReader + ISO File Reader ✅
**Goal:** Read cooked 2048-byte sectors from a plain `.iso` file; parse the ISO 9660
Primary Volume Descriptor.

- [x] **2.1** `src/sector_reader.rs` — `SectorReader` trait
  - `read_sector(&mut self, lba: u64) -> Result<Vec<u8>, OpticaldiscsError>`
  - `read_bytes(&mut self, byte_offset: u64, length: usize) -> Result<Vec<u8>, ...>`
    (default impl + optimised override in IsoSectorReader)
  - Constants: `SECTOR_SIZE`, `RAW_SECTOR_SIZE`, `MODE1_DATA_OFFSET`

- [x] **2.2** `src/sector_reader.rs` — `IsoSectorReader`
  - Wraps `BufReader<File>`, opens via `IsoSectorReader::new(path)`
  - `read_bytes` overridden with a direct seek+read (avoids sector-at-a-time loop)

- [x] **2.3** `src/iso9660.rs` — `PrimaryVolumeDescriptor`
  - Reads from sector 16 via `SectorReader`; also accepts raw `&[u8]` via `parse()`
  - Fields: volume_id, system_id, volume_set_id, publisher_id, application_id,
    volume_space_size, logical_block_size, root_directory_lba, root_directory_size
  - Validates type byte, `CD001` identifier, version, terminator type
  - `build_test_pvd_sector()` helper (`#[doc(hidden)]`) for tests

- [x] **2.4** `src/detect.rs` — `DiscImageInfo::open()` + `probe_filesystem()`
  - `DiscImageInfo` struct: path, format, filesystem, volume_label, pvd
  - `probe_filesystem()` probes ISO9660, HFS, HFS+, APM
  - ISO support only (BIN/CUE + CHD return a clear "coming in Phase N" error)

- [x] **2.5** Tests: programmatic ISO fixture (no external tools needed)
  - `write_test_iso()` helper in integration tests builds a valid minimal ISO
  - 6 unit tests in `iso9660.rs`, 4 in `detect.rs`, 4 new integration tests
  - 28 total tests passing

**Deliverable:** `DiscImageInfo::open("disc.iso")` returns filesystem type and volume
label; `PrimaryVolumeDescriptor::read_from(&mut reader)` works on any ISO file.

---

### Phase 3 — BIN/CUE Sector Reader ✅
**Goal:** Read sectors from a `.bin` file using a `.cue` sheet; support both raw
2352-byte and cooked 2048-byte tracks; generate a single-BIN CUE output.

- [x] **3.1** `src/bincue.rs` — CUE parsing
  - Use `cue_sheet` crate to parse the CUE text
  - Extract per-track info: track number, track type, sector size, data offset,
    BIN filename, INDEX 01 position
  - `normalize_cue_keywords()` — fix case/compatibility quirks in the cue_sheet crate
  - `parse_cue_tracks()` — returns `Vec<BinTrack>`

- [x] **3.2** `src/bincue.rs` — `BinTrack` struct
  - `track_no`, `track_type` (`TrackType` enum: `Audio`, `Mode1Raw`, `Mode1Cooked`,
    `Mode2Form1`, `Mode2Form2`)
  - `sector_size()`, `data_offset()` methods on `TrackType`
  - `bin_path`, `file_byte_offset` (INDEX 01 frames × sector_size)

- [x] **3.3** `src/bincue.rs` — single-BIN writer
  - `write_single_bin_cue(tracks, out_bin, out_cue, out_bin_name)`
  - Concatenates multiple BIN files into one, rewrites CUE FILE references
  - Recalculates INDEX 01 positions from running frame count

- [x] **3.4** `src/sector_reader.rs` — `BinCueSectorReader`
  - Takes a `&BinTrack` for the data track
  - `physical = file_byte_offset + lba * physical_sector_size + data_offset`
  - Strips header bytes transparently; caller always sees 2048-byte cooked sectors

- [x] **3.5** Tests: programmatic BIN/CUE fixture (no external tools needed)
  - `write_test_bincue()` helper in integration tests builds a minimal Mode1/2352 image
  - 8 unit tests in `bincue.rs` (MSF, TrackType, parse, write roundtrip)
  - 2 new integration tests (`bincue_sector_reader_reads_pvd`, `disc_image_info_open_bincue`)
  - 39 total tests passing; `cargo clippy` and `cargo fmt --check` clean

**Deliverable:** `BinCueSectorReader` presents a `.bin` file as a transparent cooked
sector stream, identical in interface to `IsoSectorReader`. `DiscImageInfo::open()`
now works for `.cue` files.

---

### Phase 4 — CHD Sector Reader ✅
**Goal:** Read sectors from a `.chd` optical disc image via the `chd` crate.

> **Superseded (0.4.0):** the bullets below describe the original pure-Rust `chd`
> crate implementation. The CHD path was later reimplemented on
> [`libchdman-rs`](https://github.com/danifunker/libchdman-rs): `src/chd.rs` opens
> via `libchdman_rs::Chd::open` and lists tracks with `libchdman_rs::cd::list_tracks`,
> and `ChdSectorReader` now wraps `libchdman_rs::cd::CdCookedReader` (MAME's
> `chd_file` core) instead of decompressing hunks itself.

- [x] **4.1** `src/chd.rs` — CHD metadata parsing
  - Open CHD via `chd::Chd::open()`
  - Parse `CHT2` metadata tag (0x43485432) for track list
  - Build `Vec<ChdTrack>`: track_no, track_type, frames, frame_offset (frame_size
    is always 2448; data_offset derived from track_type)
  - `find_first_data_track()` — returns the track to use for ISO9660/HFS reading

- [x] **4.2** `src/chd.rs` — `ChdInfo` struct
  - hunk_size, logical_size, tracks; toc deferred to Phase 5

- [x] **4.3** `src/sector_reader.rs` — `ChdSectorReader`
  - Holds `chd::Chd<BufReader<File>>`, hunk_size, track_byte_offset, data_offset
  - Translates LBA → hunk index + offset within hunk
  - Decompresses hunks on demand (via `chd` crate), caches last hunk in place
    (no separate Vec clone — hunk_buf IS the cache)

- [x] **4.4** Update `src/detect.rs` to probe CHD files
  - `probe_chd()` opens CHD, finds first data track, reads PVD/HFS header
  - Audio-only discs return `FilesystemType::Unknown` cleanly

- [x] **4.5** Tests: 9 unit tests in `chd.rs` (CHT2 parsing, track types,
  data offsets, find_first_data_track, error on missing file)
  - 3 integration tests: `open_chd_missing_file_returns_io_error` (always runs),
    `chd_sector_reader_reads_pvd` and `disc_image_info_open_chd` (skip gracefully
    if `tests/fixtures/data.chd` is absent — create with `chdman createcd`)
  - 52 total tests passing; `cargo clippy` and `cargo fmt --check` clean

**Deliverable:** All three container formats present a uniform `SectorReader` interface.
The same ISO9660 / HFS code works regardless of container.

---

### Phase 5 — TOC & Disc Metadata  *(feature = "toc")* ✅
**Goal:** Represent a disc's Table of Contents; calculate MusicBrainz and FreeDB IDs.

- [x] **5.1** `src/toc.rs` — `DiscTOC`
  - `first_track`, `last_track`, `lead_out` (frames), `track_offsets: Vec<u32>`
  - All stored as absolute frames (150-frame pregap already added)

- [x] **5.2** `src/toc.rs` — `TrackInfo`
  - `number`, `offset` (raw frames, without pregap), `track_type`

- [x] **5.3** `src/toc.rs` — helpers
  - `parse_msf(s: &str) -> Result<u32>` — `"MM:SS:FF"` → frames
  - `frames_to_msf(frames: u32) -> (u8, u8, u8)` — inverse, always succeeds
  - `DiscTOC::from_tracks(tracks: &[TrackInfo], lead_out: u32)` — adds 150-frame pregap
  - `DiscTOC::total_seconds()`, `total_time_string()`, `track_count()`
  - `FRAMES_PER_SECOND = 75`, `PREGAP_FRAMES = 150` exported as public constants

- [x] **5.4** `src/toc.rs` — MusicBrainz DiscID *(requires `toc` feature)*
  - SHA-1 over 402-byte binary record (per MB spec)
  - **Correct MB base64 variant**: standard Base64 with `+`→`.`, `/`→`_`, `=`→`-`
    (NOT RFC 4648 base64url — that encoding is wrong for MusicBrainz)
  - Produces 28-character strings (e.g. `"88mmRzPF.QgexLjFmYaEiNMUN44-"`)
  - `DiscTOC::musicbrainz_id() -> String`

- [x] **5.5** `src/toc.rs` — FreeDB DiscID *(requires `toc` feature)*
  - Legacy CDDB checksum algorithm
  - `DiscTOC::freedb_id() -> String`

- [x] **5.6** 20 unit tests: MSF parse/roundtrip, frames_to_msf, from_tracks, helpers,
  FreeDB ID (manually verified: `"0d022202"` for the 2-track test vector),
  MB DiscID verified against Python-computed value `"88mmRzPF.QgexLjFmYaEiNMUN44-"`,
  plus length=28 and char-set assertions
  - 58 unit + 14 integration + 2 doc-tests = 74 total passing
  - `cargo clippy --features toc` and `cargo fmt --check` clean

**Deliverable:** Audio CD metadata fully supported. Note the MB base64 encoding is the
correct variant (28 chars, `.` not `-` for position-62, trailing `-` for padding).

---

### Phase 6 — Disc Detection ✅
**Goal:** Given a file path, determine both `DiscFormat` and `FilesystemType` without
the caller needing to know anything about the internals.

- [x] **6.1** `src/detect.rs` — `detect_format(path: &Path) -> Result<DiscFormat>`
  - Extension-based fast path (no I/O)
  - Magic bytes fallback: CHD (`MComprHD` at offset 0), ISO 9660 (`CD001` at offset 32769)

- [x] **6.2** `src/detect.rs` — `detect_filesystem(reader: &mut dyn SectorReader)`
  - Thin public wrapper over internal `probe_filesystem`
  - Returns `FilesystemType` (ISO 9660, HFS, HFS+, APM, Unknown)

- [x] **6.3** `src/detect.rs` — `DiscImageInfo` struct (public)
  - `format`, `filesystem`, `volume_label: Option<String>`,
    `pvd: Option<PrimaryVolumeDescriptor>`,
    `#[cfg(feature = "toc")] toc: Option<DiscTOC>`
  - `DiscImageInfo::open()` updated to use `detect_format` (magic-byte aware)
  - TOC populated for BIN/CUE (from track `file_byte_offset` + BIN file size) and
    CHD (from `ChdTrack.frame_offset` + `.frames`); `None` for plain ISO

- [x] **6.4** Tests: `detect_format_*` (extension + magic bytes), `detect_filesystem_*`,
  `disc_image_info_bincue_has_toc`, `disc_image_info_iso_has_no_toc`
  - 60 unit + 16 integration = 76 total passing (no `toc` feature)
  - 64 unit + 18 integration + 2 doc-tests = 84 total passing (`--features toc`)
  - `cargo clippy --features toc` and `cargo fmt --check` clean

**Deliverable:** Users of the library only need to call `DiscImageInfo::open(path)` to
get everything they need about a disc image.

---

### Phase 7 — ISO 9660 Filesystem Browser ✅
**Goal:** Browse directories and read files from any ISO 9660 disc image regardless of
container.

- [x] **7.1** `src/browse/iso9660.rs` — `Iso9660Filesystem`
  - Constructor: `new(reader: Box<dyn SectorReader>) -> Result<Self, FilesystemError>`
  - Re-uses existing `PrimaryVolumeDescriptor::read_from()` (avoids duplicating PVD parsing)

- [x] **7.2** `src/browse/iso9660.rs` — `DirectoryRecord` (private)
  - extent_location, data_length, file_flags, file_identifier
  - `is_directory()`, `is_self()`, `is_parent()`, `clean_name()` (strip `;1` version)

- [x] **7.3** `src/browse/iso9660.rs` — implement `Filesystem` trait
  - `root()` — returns root from PVD, sets `size` field for `list_directory` to use
  - `list_directory(entry)` — reads directory extent via `read_bytes`, parses records;
    stores `data_length` in `entry.size` for directories (no re-read needed)
  - `read_file(entry)` — reads file extent via `read_bytes`
  - `read_file_range(entry, offset, length)` — partial read via `read_bytes`

- [x] **7.4** `src/browse/mod.rs` — `open_disc_filesystem(info: &DiscImageInfo)`
  - Creates the right `SectorReader` for the container format (ISO, BIN/CUE, CHD)
  - Creates `Iso9660Filesystem`; `FilesystemType::Hfs/HfsPlus` return `Unsupported`

- [x] **7.5** Tests: 12 unit tests in `browse/iso9660.rs` (parse_directory, read_file,
  read_file_range, volume_name, error cases, sorting, DirectoryRecord parsing);
  5 integration tests: `browse_iso_root`, `browse_iso_file_listing`,
  `browse_iso_read_file`, `browse_iso_volume_name`, `browse_bincue_iso9660`
  - 58 unit + 23 integration = 81 total passing (no feature)
  - 78 unit + 23 integration + 2 doc-tests = 103 total passing (`--features toc`)
  - `cargo clippy --features toc` and `cargo fmt --check` clean

**Deliverable:** `open_disc_filesystem(&disc_info)` returns a browsable filesystem
for ISO images (plain ISO, BIN/CUE, and CHD containers).

---

### Phase 8 — HFS and HFS+ Browsers ✅
**Goal:** Browse directories on Apple HFS and HFS+ disc images (classic Mac CDs).

- [x] **8.1** `src/apm.rs`
  - `PartitionEntry` struct: name, partition_type, start_block, block_count
  - `parse_partition_map(reader)` — reads DDM + all PM entries
  - `find_hfs_partition_offset(reader)` — locates first HFS/HFS+ partition byte offset
  - `PartitionEntry::is_hfs()` — checks `Apple_HFS*` type strings

- [x] **8.2** `src/hfs.rs` — `MasterDirectoryBlock`
  - Reads from byte offset 1024
  - Validates signature 0x4244 (`BD`)
  - Extracts: volume_name, file_count, creation_date, alloc_block_size/start, catalog extents
  - `MasterDirectoryBlock::read_from(reader, partition_offset)`
  - `mac_roman_to_string` helper + `MAC_ROMAN_TABLE` (128-char lookup)

- [x] **8.3** `src/hfsplus.rs` — `HfsPlusVolumeHeader`
  - Reads from byte offset 1024
  - Validates signature 0x482B (`H+`) or 0x4858 (`HX`)
  - Extracts: version, block_size, total_blocks, free_blocks, catalog_file extents
  - `extract_volume_name_from_catalog(reader, partition_offset)` — parses B-tree thread
    record for root CNID (2), returns UTF-16 BE volume name

- [x] **8.4** `src/browse/hfs.rs` — `HfsFilesystem`
  - Constructor: `new(reader, partition_offset)` — no disc_info needed; reads name from MDB
  - Implements `Filesystem` trait: `root()`, `list_directory()`, `read_file()`, `read_file_range()`
  - MacRoman catalog key/record parsing; HFS extent reading

- [x] **8.5** `src/browse/hfsplus.rs` — `HfsPlusFilesystem`
  - Constructor: `new(reader, partition_offset)` — volume name from B-tree thread record
  - Implements `Filesystem` trait
  - HFS+ catalog key (UTF-16 BE) and fork data (8 extents × u32) parsing

- [x] **8.6** Update `src/browse/mod.rs` — `open_disc_filesystem()` routes to HFS/HFS+
  - Calls `find_hfs_partition_offset()` (falls back to 0 for non-APM images)

- [x] **8.7** Update `src/detect.rs` — APM detection uses `parse_partition_map()`
  - Replaces naive byte-1024 check with proper partition map traversal

- [x] **8.8** Tests: 84 unit + 21 integration passing; APM/MDB/VH/B-tree parsing covered
  - No HFS/HFS+ fixture files available; parsing logic tested synthetically per module

**Deliverable:** Classic Mac CDs fully browsable.

---

### Phase 9 — Physical Drive Enumeration  *(feature = "drives")* ✅
**Goal:** List available optical drives on the current system.

- [x] **9.1** `src/drives.rs` — `OpticalDrive` struct
  - `device_path: PathBuf` (e.g. `/dev/sr0`, `/dev/disk2`, `D:\`)
  - `display_name: String` (human-readable, e.g. `"SAMSUNG SH-224FB"`)
  - `is_loaded: bool` (disc present)

- [x] **9.2** `src/drives.rs` — Linux implementation (`#[cfg(target_os = "linux")]`)
  - Scan `/sys/block/sr*` for optical devices
  - Read vendor/model from `/sys/block/srN/device/vendor` + `model`
  - Check media presence via `/sys/block/srN/size > 0`

- [x] **9.3** `src/drives.rs` — macOS implementation (`#[cfg(target_os = "macos")]`)
  - Runs `ioreg -r -c IODVDDriveNub -l`; parses `Vendor Name`, `Product Name`,
    `BSD Name`, `Media Present`; device_path = `/dev/{bsd_name}`

- [x] **9.4** `src/drives.rs` — Windows implementation (`#[cfg(target_os = "windows")]`)
  - Enumerate drive letters A-Z
  - `GetDriveTypeW()` == `DRIVE_CDROM` (5, not 4 — PLAN had a typo)
  - `GetVolumeInformationW()` for display name; success → is_loaded

- [x] **9.5** `src/drives.rs` — `list_drives() -> Vec<OpticalDrive>`
  - Public entry point, dispatches to platform impl

- [x] **9.6** Manual / integration test: `list_drives_smoke_test` logs found drives
  without asserting a count (machine may have no drive)

**Deliverable:** `opticaldiscs::drives::list_drives()` returns optical drives on all
three platforms.

---

## Publishing to crates.io ✅

Published on [crates.io](https://crates.io/crates/opticaldiscs) — first release
`0.4.2` (2026-05-19), current `0.4.5`. Releases are cut by the
`.github/workflows/publish-crates-io.yml` workflow (`workflow_dispatch`), which
derives the tag from `Cargo.toml`, verifies a slim (≤ 2 MB, no binary fixtures)
tarball, creates the GitHub Release, and runs `cargo publish`.

- [x] Ensure all public items have `///` doc comments
- [x] Run `cargo doc`, review generated docs
- [x] Add an example to `examples/` (`inspect_efs.rs`)
- [x] Set `repository`, `keywords`, `categories` in `Cargo.toml`
- [x] Publish to crates.io via the `publish-crates-io.yml` workflow
- [x] Downstream consumers depend on `opticaldiscs = "0.4"` from crates.io

---

## Stage Dependency Summary

```
Phase 0  (scaffold)
  └── Phase 1  (core types)
        ├── Phase 2  (ISO sector reader)    ─┐
        ├── Phase 3  (BIN/CUE reader)        ├── Phase 6  (detection)
        └── Phase 4  (CHD reader)           ─┘      │
                                                     ├── Phase 7  (ISO9660 browser)
              Phase 5  (TOC) ────────────────────────│
                                                     └── Phase 8  (HFS/HFS+ browsers)
Phase 9 is independent of 7+8 (feature = "drives")
```

---

## Test Fixtures Needed

| Fixture | Used in | Source |
|---------|---------|--------|
| `tests/fixtures/data.iso` | Phases 2, 6, 7 | Create with `mkisofs`, or build programmatically |
| `tests/fixtures/data.bin` + `.cue` | Phases 3, 6 | Build programmatically (minimal Mode1/2352) |
| `tests/fixtures/data.chd` | Phases 4, 6 | Create with `chdman createcd` |
| `tests/fixtures/mac_hfs.iso` | Phase 8 | Classic Mac disc image |
| `tests/fixtures/mac_hfsplus.iso` | Phase 8 | Mac OS X disc image |

Keep test fixtures small (≤ 2MB each). Add them to a `.gitattributes` LFS entry
if the repo will be public, or keep them in a separate `test-fixtures` branch.

---

*Last updated: 2026-07-07.*
*All phases (0–9) are complete and shipped (current release `0.7.0`);
EFS / SGI support landed as `0.3.0` (see [docs/EFS_Implementation.md](docs/EFS_Implementation.md)).
UDF support (physical-partition layout, UDF 1.02–2.01: DVDs, DVD-Video, data discs)
landed as `0.7.0`; UDF 2.50+ metadata-partition support (Blu-ray / BDMV discs)
landed as `0.8.0`. Future versions may add UDF sparable/virtual partitions and
the MDS/MDF format.*

*Video-game optical-disc support (PlayStation, Sega, Nintendo GameCube/Wii, 3DO,
CD-i, and more) is planned — see [docs/GameDiscs_Implementation.md](docs/GameDiscs_Implementation.md)
for the design, per-console format details, and phased roadmap (Phases G1–G6).
GameCube/Wii decryption is delegated to the [`nod`](https://github.com/encounter/nod)
crate, which bundles the Wii common keys, so no user-supplied key file is required.*
