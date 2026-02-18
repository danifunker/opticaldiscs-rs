# opticaldiscs — Implementation Plan

A format-agnostic Rust library for reading, browsing, and ripping optical disc images
(ISO, BIN/CUE, CHD) and physical CD/DVD/Blu-ray drives.

---

## License Note

This library is licensed **GPL-3.0**.

- `rusty-backup` (AGPL-3.0) can depend on GPL-3.0 code ✓
- `ODE-artwork-downloader` (GPL-3.0 per LICENSE file) can depend on GPL-3.0 code ✓

**Action needed in ODE:** `Cargo.toml` currently states `license = "MIT"` but the
actual `LICENSE` file is GPL-3.0. Update `Cargo.toml` to `license = "GPL-3.0"` for
consistency.

---

## Goals

1. A single `SectorReader` abstraction that handles the cooked/raw sector translation
   across `.iso`, `.bin`+`.cue`, and `.chd` containers — this is the core value that
   no existing crate provides.
2. ISO 9660, HFS, and HFS+ filesystem browsing on top of that abstraction.
3. Physical optical drive enumeration (feature-flagged, platform-specific).
4. All code is GUI-free; `rusty-backup` and `ODE` add their own application layers on top.

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
    drives.rs           — OpticalDrive struct, list_drives()  [feature = "drives"]
    browse/
      mod.rs            — open_disc_filesystem() dispatcher
      filesystem.rs     — Filesystem trait, FilesystemError
      entry.rs          — FileEntry, EntryType
      iso9660.rs        — ISO9660 directory listing + file reading
      hfs.rs            — HFS filesystem browser
      hfsplus.rs        — HFS+ filesystem browser
```

---

## Dependencies

```toml
[dependencies]
thiserror  = "2"
log        = "0.4"
cue_sheet  = "0.1"           # CUE sheet text parsing
chd        = "0.2"           # CHD reading (optical hunk/track metadata)

# Optional — only with feature "toc"
sha1       = { version = "0.10", optional = true }
base64     = { version = "0.22", optional = true }

[features]
default = []
toc    = ["dep:sha1", "dep:base64"]   # MusicBrainz + FreeDB DiscID calculation
drives = []                            # physical drive enumeration (platform-specific)
```

---

## Integration Architecture

### rusty-backup (`src/optical/`)
A thin application layer on top of this library — not part of opticaldiscs itself.

```
src/optical/
  mod.rs          — re-exports + integration glue
  rip.rs          — stream rip physical disc → ISO or BIN/CUE, with progress callback
  convert.rs      — ISO ↔ BIN/CUE sector conversion; any → CHD via chdman subprocess
  browse_view.rs  — OpticalDiscBrowseView (egui widget, mirrors existing BrowseView)
```

```
src/gui/
  optical_tab.rs  — new "Optical" tab: source picker, disc info, rip/convert UI
```

### ODE-artwork-downloader (`src/disc/`)
Replace the lower-level files with opticaldiscs; keep the high-level ODE-specific layer.

```
KEEP:   src/disc/reader.rs      — DiscInfo, DiscReader (ODE-specific artwork fields)
KEEP:   src/disc/identifier.rs  — game title parsing from filename
REMOVE: src/disc/iso9660.rs     → replaced by opticaldiscs::iso9660
REMOVE: src/disc/bincue.rs      → replaced by opticaldiscs::bincue + sector_reader
REMOVE: src/disc/chd.rs         → replaced by opticaldiscs::chd + sector_reader
REMOVE: src/disc/toc.rs         → replaced by opticaldiscs::toc
REMOVE: src/disc/apm.rs         → replaced by opticaldiscs::apm
REMOVE: src/disc/hfs.rs         → replaced by opticaldiscs::hfs
REMOVE: src/disc/hfsplus.rs     → replaced by opticaldiscs::hfsplus
REMOVE: src/disc/browse/*       → replaced by opticaldiscs::browse
```

---

## Stages

Each stage is independently buildable and testable. Check off items as they are
completed. Stages within a phase can be done in any order; phases should be done
in sequence because later phases build on earlier ones.

Progress key: `[ ]` pending · `[x]` done · `[-]` skipped/deferred

---

### Phase 0 — Repository Scaffold ✅
**Goal:** Working Cargo library that compiles with `cargo build` and `cargo test`.

- [x] **0.1** Initialize `Cargo.toml` as a library crate (`opticaldiscs`, GPL-3.0)
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
Ported from `ODE/src/disc/formats.rs`, `browse/entry.rs`, `browse/filesystem.rs`.

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
Primary Volume Descriptor. Ported from `ODE/src/disc/browse/reader.rs` (IsoSectorReader)
and `ODE/src/disc/iso9660.rs`.

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

### Phase 3 — BIN/CUE Sector Reader
**Goal:** Read sectors from a `.bin` file using a `.cue` sheet; support both raw
2352-byte and cooked 2048-byte tracks; generate a single-BIN CUE output.
Ported from `ODE/src/disc/bincue.rs` and `ODE/src/disc/browse/reader.rs`
(BinCueSectorReader).

- [ ] **3.1** `src/bincue.rs` — CUE parsing
  - Use `cue_sheet` crate to parse the CUE text
  - Extract per-track info: track number, track type, sector size, data offset,
    BIN filename, INDEX 01 position
  - `normalize_cue_keywords()` — fix case/compatibility quirks in the cue_sheet crate
  - `parse_tracks()` — returns `Vec<BinTrack>`

- [ ] **3.2** `src/bincue.rs` — `BinTrack` struct
  - `track_no`, `track_type` (`TrackType` enum: `Audio`, `Mode1Raw`, `Mode1Cooked`,
    `Mode2Form1`, `Mode2Form2`)
  - `sector_size` (2352 or 2048), `data_offset` (16 for Mode1Raw, 0 for cooked)
  - `bin_path`, `frame_start` (from INDEX 01 MSF)

- [ ] **3.3** `src/bincue.rs` — single-BIN writer
  - `write_single_bin_cue(tracks: &[BinTrack], output_bin: &Path, output_cue: &Path)`
  - Concatenates multiple BIN files into one, rewrites CUE FILE references
  - Output CUE uses `MODE1/2352` for raw data tracks

- [ ] **3.4** `src/sector_reader.rs` — `BinCueSectorReader`
  - Takes `bin_path` + parsed `BinTrack` for the data track
  - Translates logical sector LBA → physical byte offset in BIN
    - Raw: `physical = frame_start * 2352 + data_offset + (lba * 2352)`
    - Cooked: `physical = frame_start * 2048 + (lba * 2048)`
  - Strips header bytes transparently; caller always sees 2048-byte cooked sectors

- [ ] **3.5** Tests: read PVD from a known `.bin`/`.cue` fixture
  - If ODE has BIN/CUE test fixtures, copy the smallest one

**Deliverable:** `BinCueSectorReader` presents a `.bin` file as a transparent cooked
sector stream, identical in interface to `IsoSectorReader`.

---

### Phase 4 — CHD Sector Reader
**Goal:** Read sectors from a `.chd` optical disc image via the `chd` crate.
Ported from `ODE/src/disc/chd.rs` and `ODE/src/disc/browse/reader.rs` (ChdSectorReader).

- [ ] **4.1** `src/chd.rs` — CHD metadata parsing
  - Open CHD via `chd::Chd::open()`
  - Parse `CHT2` metadata tag (0x43485432) for track list
  - Build `Vec<ChdTrack>`: track_no, track_type, frames, frame_offset, frame_size,
    data_offset
  - `find_first_data_track()` — returns the track to use for ISO9660/HFS reading

- [ ] **4.2** `src/chd.rs` — `ChdInfo` struct
  - volume_label (Option), filesystem type hint, hunk_size, logical_size, tracks,
    toc (Option, gated behind `toc` feature)

- [ ] **4.3** `src/sector_reader.rs` — `ChdSectorReader`
  - Holds `chd::Chd`, hunk_size, track frame_offset, frame_size, data_offset
  - Translates LBA → hunk index + offset within hunk
  - Decompresses hunks on demand (via `chd` crate), caches last hunk

- [ ] **4.4** Update `src/detect.rs` to probe CHD files
  - Open CHD, find first data track, read PVD/HFS header

- [ ] **4.5** Tests: read PVD from a `.chd` optical test fixture

**Deliverable:** All three container formats present a uniform `SectorReader` interface.
The same ISO9660 / HFS code works regardless of container.

---

### Phase 5 — TOC & Disc Metadata  *(feature = "toc")*
**Goal:** Represent a disc's Table of Contents; calculate MusicBrainz and FreeDB IDs.
Ported from `ODE/src/disc/toc.rs`.

- [ ] **5.1** `src/toc.rs` — `DiscTOC`
  - `first_track`, `last_track`, `lead_out` (frames), `track_offsets: Vec<u32>`

- [ ] **5.2** `src/toc.rs` — `TrackInfo`
  - `number`, `offset` (frames), `track_type`

- [ ] **5.3** `src/toc.rs` — helpers
  - `parse_msf(s: &str) -> u32` — `"MM:SS:FF"` → frames
  - `frames_to_msf(frames: u32) -> (u8, u8, u8)` — inverse
  - `DiscTOC::from_tracks(tracks: &[TrackInfo], lead_out: u32)`
  - `DiscTOC::total_seconds()`, `total_time_string()`, `track_count()`

- [ ] **5.4** `src/toc.rs` — MusicBrainz DiscID *(requires `toc` feature)*
  - SHA-1 over binary TOC data → base64url (RFC 4648)
  - `DiscTOC::musicbrainz_id() -> String`

- [ ] **5.5** `src/toc.rs` — FreeDB DiscID *(requires `toc` feature)*
  - Legacy checksum algorithm
  - `DiscTOC::freedb_id() -> String`

- [ ] **5.6** Tests for MSF parsing and both DiscID algorithms
  - Use a known disc TOC with a published MB ID to validate

**Deliverable:** Audio CD metadata fully supported; ODE can replace its `toc.rs` with
this module.

---

### Phase 6 — Disc Detection
**Goal:** Given a file path, determine both `DiscFormat` and `FilesystemType` without
the caller needing to know anything about the internals.

- [ ] **6.1** `src/detect.rs` — `DiscFormat::detect_from_path(path: &Path)`
  - Extension-based fast path
  - Magic bytes fallback (CHD header, ISO `CD001` at offset 32769)

- [ ] **6.2** `src/detect.rs` — `detect_filesystem(reader: &mut dyn SectorReader)`
  - Check ISO9660 PVD at sector 16
  - Check HFS MDB signature at byte 1024
  - Check HFS+ volume header at byte 1024
  - Check APM DDM signature at byte 0
  - Returns `FilesystemType`

- [ ] **6.3** `src/detect.rs` — `DiscImageInfo` struct (public)
  - `format`, `filesystem`, `volume_label: Option<String>`,
    `pvd: Option<PrimaryVolumeDescriptor>`, `toc: Option<DiscTOC>`
  - `DiscImageInfo::open(path: &Path) -> Result<Self>`  — the one-stop entry point

- [ ] **6.4** Tests: `DiscImageInfo::open()` on each fixture type (ISO, BIN/CUE, CHD)

**Deliverable:** Users of the library only need to call `DiscImageInfo::open(path)` to
get everything they need about a disc image.

---

### Phase 7 — ISO 9660 Filesystem Browser
**Goal:** Browse directories and read files from any ISO 9660 disc image regardless of
container. Ported from `ODE/src/disc/browse/iso9660_fs.rs`.

- [ ] **7.1** `src/browse/iso9660.rs` — `Iso9660Filesystem`
  - Constructor: `new(reader: Box<dyn SectorReader>) -> Result<Self, FilesystemError>`
  - Reads PVD at sector 16, extracts root directory LBA + size

- [ ] **7.2** `src/browse/iso9660.rs` — `DirectoryRecord`
  - length, extent_location (LBA), data_length, file_flags, file_identifier
  - `is_directory()`, `is_self()`, `is_parent()`, `clean_name()` (strip `;1` version)

- [ ] **7.3** `src/browse/iso9660.rs` — implement `Filesystem` trait
  - `root()` — read root from PVD
  - `list_directory(entry)` — seek to entry's LBA, parse all directory records
  - `read_file(entry)` — read entry's sectors into Vec<u8>
  - `read_file_range(entry, offset, length)` — partial read

- [ ] **7.4** `src/browse/mod.rs` — `open_disc_filesystem()`
  - Routes by `DiscFormat` to the right sector reader
  - Routes by `FilesystemType` to the right `Filesystem` impl
  - Returns `Box<dyn Filesystem>`

- [ ] **7.5** Tests: list root of an ISO9660 fixture, verify expected files present

**Deliverable:** `open_disc_filesystem(&disc_info)` returns a browsable filesystem
for ISO images.

---

### Phase 8 — HFS and HFS+ Browsers
**Goal:** Browse directories on Apple HFS and HFS+ disc images (classic Mac CDs).
Ported from `ODE/src/disc/browse/hfs_fs.rs` and `hfsplus_fs.rs`.

- [ ] **8.1** `src/apm.rs`
  - `PartitionEntry` struct: name, partition_type, start_block, block_count
  - `parse_partition_map(reader)` — reads DDM + all PM entries
  - `find_hfs_partition_offset(reader)` — locates first HFS/HFS+ partition byte offset
  - `PartitionEntry::is_hfs()` — checks `Apple_HFS`, `Apple_HFSX`, `Apple_HFS+` type strings

- [ ] **8.2** `src/hfs.rs` — `MasterDirectoryBlock`
  - Reads from byte offset 1024
  - Validates signature 0x4244 (`BD`)
  - Extracts: volume_name, file_count, folder_count, creation_date
  - `MasterDirectoryBlock::read_from(reader, partition_offset)`

- [ ] **8.3** `src/hfsplus.rs` — `HfsPlusVolumeHeader`
  - Reads from byte offset 1024
  - Validates signature 0x482B (`H+`) or 0x4858 (`HX`)
  - Extracts: version, block_size, total_blocks, free_blocks, catalog_file extents
  - `extract_volume_name_from_catalog(reader, partition_offset)` — parses B-tree thread
    record for root CNID (2)

- [ ] **8.4** `src/browse/hfs.rs` — `HfsFilesystem`
  - Constructor: `new(reader, partition_offset, disc_info)`
  - Implements `Filesystem` trait: `root()`, `list_directory()`, `read_file()`

- [ ] **8.5** `src/browse/hfsplus.rs` — `HfsPlusFilesystem`
  - Constructor: `new(reader, partition_offset, disc_info)`
  - Implements `Filesystem` trait

- [ ] **8.6** Update `src/browse/mod.rs` — `open_disc_filesystem()` routes to HFS/HFS+
  - Calls `find_hfs_partition_offset()` before creating HFS filesystem

- [ ] **8.7** Update `src/detect.rs` — APM detection feeds into `open_disc_filesystem()`

- [ ] **8.8** Tests: list root of an HFS and/or HFS+ test fixture

**Deliverable:** Classic Mac CDs fully browsable.

---

### Phase 9 — Physical Drive Enumeration  *(feature = "drives")*
**Goal:** List available optical drives on the current system.

- [ ] **9.1** `src/drives.rs` — `OpticalDrive` struct
  - `device_path: PathBuf` (e.g. `/dev/sr0`, `/dev/disk2`, `D:\`)
  - `display_name: String` (human-readable, e.g. `"SAMSUNG SH-224FB"`)
  - `is_loaded: bool` (disc present)

- [ ] **9.2** `src/drives.rs` — Linux implementation (`#[cfg(target_os = "linux")]`)
  - Scan `/sys/block/sr*` for optical devices
  - Read vendor/model from `/sys/block/srN/device/vendor` + `model`
  - Check media presence via `/sys/block/srN/size > 0`

- [ ] **9.3** `src/drives.rs` — macOS implementation (`#[cfg(target_os = "macos")]`)
  - Parse `diskutil list` or use `ioreg -r -c IODVDDriveNub`
  - Map `/dev/diskN` entries with DVD/CD type

- [ ] **9.4** `src/drives.rs` — Windows implementation (`#[cfg(target_os = "windows")]`)
  - Enumerate drive letters A-Z
  - `GetDriveTypeW()` == `DRIVE_CDROM` (4)
  - `GetVolumeInformationW()` for display name

- [ ] **9.5** `src/drives.rs` — `list_drives() -> Vec<OpticalDrive>`
  - Public entry point, dispatches to platform impl

- [ ] **9.6** Manual / integration test: run on real hardware, log found drives

**Deliverable:** `opticaldiscs::drives::list_drives()` returns optical drives on all
three platforms.

---

### Phase 10 — Migrate ODE to opticaldiscs
**Goal:** ODE-artwork-downloader stops maintaining its own disc layer and depends on
this library instead.

- [ ] **10.1** In ODE `Cargo.toml`: add `opticaldiscs = { git = "...", features = ["toc"] }`
- [ ] **10.2** Fix ODE `Cargo.toml` license field: change `"MIT"` → `"GPL-3.0"`
- [ ] **10.3** Rewrite `ODE/src/disc/reader.rs`:
  - Replace internal `read_iso()` / `read_bin_cue()` / `read_chd()` calls with
    `opticaldiscs::detect::DiscImageInfo::open()`
  - `DiscInfo` gains fields from `DiscImageInfo`; ODE-specific fields (title,
    confidence, parsed_filename, cover_art_path) remain in ODE
- [ ] **10.4** Delete from ODE: `iso9660.rs`, `bincue.rs`, `chd.rs`, `toc.rs`,
              `apm.rs`, `hfs.rs`, `hfsplus.rs`, `browse/` (all replaced)
- [ ] **10.5** Update `browse_view.rs` in ODE to call `opticaldiscs::browse::open_disc_filesystem()`
- [ ] **10.6** Run ODE test suite; fix any regressions
- [ ] **10.7** Verify ODE disc browsing still works on ISO, BIN/CUE, and CHD fixtures

**Deliverable:** ODE is slimmer, and any improvements to opticaldiscs immediately
benefit ODE.

---

### Phase 11 — Disc Ripping in rusty-backup
**Goal:** rusty-backup can rip a physical disc to ISO or BIN/CUE.

- [ ] **11.1** In rusty-backup `Cargo.toml`: add
              `opticaldiscs = { git = "...", features = ["drives"] }`
- [ ] **11.2** `src/optical/mod.rs` — new module, thin rusty-backup layer
- [ ] **11.3** `src/optical/rip.rs` — `RipConfig` + `RipProgress`
  - `RipConfig`: source (device_path), output_path, format (`RipFormat::Iso` or
    `RipFormat::BinCue`), eject_after
  - `RipProgress`: current_bytes, total_bytes, current_sector, total_sectors,
    operation, finished, error, cancel_requested
  - `run_rip(config: RipConfig, progress: Arc<Mutex<RipProgress>>, log_cb)` — runs
    on a background thread
  - ISO rip: open device, read 2048-byte sectors sequentially, write to `.iso`
  - BIN/CUE rip: read raw 2352-byte sectors, write single `.bin`, generate `.cue` with
    MODE1/2352 (or AUDIO for audio tracks); detect track boundaries from TOC

- [ ] **11.4** Platform raw device read:
  - Linux/macOS: `File::open(device_path)`, `read_exact` into 2352-byte or 2048-byte
    buffers; on macOS use `dd` subprocess if direct open is restricted
  - Windows: `CreateFile(\\.\D:)` with `GENERIC_READ` + `FILE_SHARE_READ`,
    `DeviceIoControl(IOCTL_CDROM_READ_TOC)` for track info

- [ ] **11.5** `src/optical/rip.rs` — TOC-aware track splitting for BIN/CUE
  - Read TOC via `IOCTL_CDROM_READ_TOC` (Windows) or `ioctl(CDROMREADTOCHDR)` (Linux)
    or `ioreg` (macOS)
  - Mark track boundaries in the `.cue` file (`TRACK N AUDIO` vs `TRACK N MODE1/2352`)

- [ ] **11.6** Tests: rip a known CD using a disc image loopback (or skip if no drive)

**Deliverable:** rusty-backup can rip a physical disc to a local file.

---

### Phase 12 — Format Conversion + GUI Tab in rusty-backup
**Goal:** Convert between ISO, BIN/CUE, CHD; browse disc images in a new GUI tab.

- [ ] **12.1** `src/optical/convert.rs` — ISO ↔ BIN/CUE
  - `iso_to_bincue(iso_path, bin_path, cue_path)`:
    - Reads 2048-byte cooked sectors
    - Wraps each in a 2352-byte Mode 1 raw frame (12-byte sync + 4-byte header +
      8-byte subheader + user data + 288-byte ECC/EDC)
    - Writes single `.bin`, generates `MODE1/2352` CUE
  - `bincue_to_iso(cue_path, iso_path)`:
    - Uses `BinCueSectorReader`, strips headers, writes cooked sectors

- [ ] **12.2** `src/optical/convert.rs` — any → CHD via chdman
  - BIN/CUE → CHD: call `chdman createcd -i input.cue -o output.chd`
  - ISO → CHD: synthesize a minimal CUE, then call chdman
  - CHD → ISO: call `chdman extractcd`, then strip to ISO
  - Use existing chdman detection from `src/rbformats/chd.rs`

- [ ] **12.3** `src/optical/browse_view.rs` — `OpticalDiscBrowseView`
  - Mirrors structure of existing `BrowseView` (no shared base class needed;
    share patterns, not code)
  - State: `disc_path`, loaded `Box<dyn Filesystem>`, current directory stack,
    selected entry, scroll state
  - `show()` method: renders directory tree, file list, file size, breadcrumb nav
  - Lazy-loads directory contents (call `list_directory()` on expand)

- [ ] **12.4** `src/gui/optical_tab.rs` — `OpticalTab` struct
  - Source: `○ Physical drive [dropdown] [Refresh]` / `○ Existing file [path] [Browse…]`
  - Disc info panel (auto-populated): format, filesystem, volume label, total size
  - Action: `○ Rip/Save as [ISO] [BIN/CUE] [CHD]` / `○ Convert to [ISO] [BIN/CUE] [CHD]`
  - Output file field + Browse button
  - `[Browse Contents]` button → opens `OpticalDiscBrowseView` in a popup/panel
  - Progress bar + cancel button
  - Disables controls during active rip/convert

- [ ] **12.5** Wire `optical_tab.rs` into `src/gui/mod.rs`
  - Add `Tab::Optical` to `enum Tab`
  - Add tab selector button `"Optical"` in the tab bar
  - Add `optical_tab: OpticalTab` to `RustyBackupApp`
  - Dispatch `Tab::Optical` in the update loop

- [ ] **12.6** End-to-end test: open an ISO file in the Optical tab, browse its contents

**Deliverable:** Full optical disc UI in rusty-backup: rip, convert, and browse.

---

## Publishing to crates.io

Publish after Phase 10 (ODE migration) validates the public API in production.

- [ ] Ensure all public items have `///` doc comments
- [ ] Run `cargo doc --open`, review generated docs
- [ ] Add examples to `examples/` directory: `read_pvd.rs`, `browse_iso.rs`
- [ ] Set `repository`, `documentation`, `keywords`, `categories` in `Cargo.toml`
- [ ] Tag `v0.1.0`, run `cargo publish --dry-run`, then `cargo publish`
- [ ] After publish, update both projects to use `opticaldiscs = "0.1"` from crates.io

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

Phase 10 (migrate ODE)      requires Phases 1-8 complete
Phase 11 (ripping)          requires Phase 9 + Phase 10
Phase 12 (GUI + convert)    requires Phase 11
```

---

## Test Fixtures Needed

| Fixture | Used in | Source |
|---------|---------|--------|
| `tests/fixtures/data.iso` | Phases 2, 6, 7 | Create with `mkisofs` or copy from ODE |
| `tests/fixtures/data.bin` + `.cue` | Phases 3, 6 | Copy from ODE test assets |
| `tests/fixtures/data.chd` | Phases 4, 6 | Create with `chdman createcd`, or copy from ODE |
| `tests/fixtures/mac_hfs.iso` | Phase 8 | Classic Mac disc image |
| `tests/fixtures/mac_hfsplus.iso` | Phase 8 | Mac OS X disc image |

Keep test fixtures small (≤ 2MB each). Add them to a `.gitattributes` LFS entry
if the repo will be public, or keep them in a separate `test-fixtures` branch.

---

*Last updated: 2026-02-17*
*Plan covers opticaldiscs v0.1.0 scope. Future versions may add UDF support,
MDS/MDF format, and Blu-ray metadata.*
