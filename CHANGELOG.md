# Changelog

All notable changes to this crate are documented here. This project follows
[Semantic Versioning](https://semver.org/).

## 0.14.0

### Fixed — a non-CD CHD no longer aborts the process

**libchdman-rs floor raised to 0.288.10** (was 0.288.8). Below that version,
handing a CHD without CD track metadata — an ordinary hard-disk, DVD, or A/V
image — to any `cd::*` call **killed the host process outright**:

```
libc++abi: terminating due to uncaught exception of type std::nullptr_t
fatal runtime error: Rust cannot catch foreign exceptions, aborting
```

MAME's `cdrom_file` constructor signals bad input by throwing a bare `nullptr`,
and Rust frames cannot unwind a foreign exception, so no caller ever got the
chance to handle it.

**This was reachable straight through this crate's front door.** `detect_format`
identifies CHD by the `MComprHD` magic, which *every* CHD carries regardless of
media, so `DiscImageInfo::open("some-harddisk.chd")` took the CD path and
aborted. Nothing a consumer could write would prevent it — hence a hard floor
rather than a compatible-range bump.

Two changes on top of the dependency bump:

- **`open_chd` now screens media kind from the CHD's info record**, which it
  already reads — so no `cdrom_file` is constructed at all for a non-CD CHD, and
  the error names the actual media: `"… is a hard-disk CHD, not a CD/GD-ROM disc
  image"`.
- **`ChdError::NotCdMedia` maps to `OpticaldiscsError::UnsupportedFormat`**, not
  `OpticaldiscsError::Chd`, in both `open_chd` and `ChdSectorReader::open`. A
  hard-disk CHD is a valid file that simply isn't an optical image, so callers can
  route it elsewhere instead of reporting corruption. This is not a compatibility
  break: before 0.288.10 the case aborted, so no consumer could have been
  matching on an error for it.

Covered by `chd::tests::hard_disk_chd_is_unsupported_not_an_abort`, which builds
a real hard-disk CHD fixture and asserts both entry points refuse it. Against an
older libchdman-rs that test does not fail — it kills the test binary, which is
the point.

**Known gap, unchanged by this release:** a **DVD** CHD is still rejected. It is
legitimately readable in principle (a flat run of 2048-byte sectors, and this
crate browses UDF fine), but there is no reader for it yet. Previously it aborted;
now it reports `UnsupportedFormat`.

### Changed — CHD support is now an (on-by-default) feature

`libchdman-rs` is no longer a hard dependency. CHD reading moved behind a new
**`chd` feature, which is in `default`**, so nothing changes for existing
consumers — but it can now be turned off:

```toml
opticaldiscs = { version = "0.14", default-features = false }
```

With `chd` off, `libchdman-rs` leaves the dependency graph entirely: no MAME C++
core to link, and no build-time download of a prebuilt static archive. That makes
the crate usable on targets libchdman-rs can't serve — i486/i586 and PowerPC
Linux, vintage macOS/Windows toolchains, and fully offline builds — while every
other container and filesystem keeps working.

**Not a breaking change.** No API was removed, `DiscFormat::Chd` still exists,
and `default = ["chd"]` means a plain dependency bump behaves exactly as before.

- **`.chd` files are still recognised without the feature.** `detect_format`
  identifies them by extension and by `MComprHD` magic as always;
  `supported_extensions()` is unchanged. Only the read path is absent, so
  `DiscImageInfo::open` returns `UnsupportedFormat("CHD support was not compiled
  in (rebuild with --features chd)")` and `browse` reports
  `FilesystemError::Unsupported`. A caller can therefore still name the format
  precisely instead of falling back to "unknown file" — the same contract the
  `mdx` feature has always had.
- **The `chd` module still compiles without the feature.** `ChdTrack`,
  `ChdTrackType`, `ChdInfo` and their helpers are plain Rust and remain
  available, so downstream code naming those types builds either way. Only
  `open_chd` and `ChdSectorReader` — the parts that touch the C++ core — are
  gated. `OpticaldiscsError::Chd` is unconditional, so error matching is
  unaffected.

### Added — runtime queries for what this build supports

Feature flags are compile-time, but a UI needs to decide at runtime whether to
offer CHD at all. Four additions, so consumers don't have to mirror
`cfg!(feature = ...)` in their own code:

- **`chd::is_supported() -> bool`** — the direct "is CHD enabled in here?"
  answer. `const fn`, so it also works in const contexts.
- **`DiscFormat::is_supported(self) -> bool`** — the general form, covering every
  conditionally-compiled format (`Chd` needs `chd`, `Mdx` needs `mdx`, everything
  else is unconditional). Use it to grey out an entry up front rather than
  provoking an error on open.
- **`DiscFormat::ALL`** — every known format as a `&'static [DiscFormat]`, for
  enumerating and filtering by `is_supported()`.
- **`enabled_extensions() -> Vec<&'static str>`** — the file-open-dialog
  companion to `supported_extensions()`: the extensions *this* build can actually
  open. Always a subset of `supported_extensions()`, which keeps its full
  recognised set and its `&'static [_]` return type.

### Internal

- `DiscFormat::from_path` now delegates to a private `from_extension(&str)`,
  which is also what `enabled_extensions` filters through. Behaviour is
  unchanged, including that the CloneCD `img`/`sub` sidecars map to no format —
  a `.ccd` is the entry point.
- CI gained a `--no-default-features` build/test/clippy leg (plus a
  `toc,drives`-without-`chd` combination), so feature-gating mistakes that only
  appear when CHD is compiled out get caught.

## 0.13.0

### Added — reading DVD and Blu-ray discs from a physical drive

- **`physical` module** with **`PhysicalDisc`** (`src/physical.rs`, feature
  `drives`): a `SectorReader` backed by a drive's device node instead of an image
  file. Because it implements the same trait every image format does, the entire
  existing stack — detection, browse, extraction — works against a disc in the
  tray with no format-specific code.
- **This is what makes DVD and Blu-ray work.** Physical-disc reading is
  conventionally done with MMC/SCSI pass-through (`READ CD`, `READ TOC`), which
  is unavoidable for CD because a CD's sectors are 2352 bytes on the wire. Those
  commands are **CD-only**: on macOS `DKIOCCDREADTOC` / `DKIOCCDREAD` against DVD
  or Blu-ray media fail outright with `ENOTTY`, so a pass-through reader cannot
  read a DVD at all — not even its table of contents.
- A **data** DVD or Blu-ray needs none of that. The drive already presents the
  medium as a flat run of 2048-byte cooked sectors, which is exactly the shape
  `SectorReader` is defined in terms of, so an ordinary `seek` + `read` is both
  sufficient and portable. DVD/BD support therefore costs **zero** ioctls.
- **`browse::open_physical_filesystem(device_path)`** browses the loaded disc
  directly: detect the filesystem off the device, then hand the reader to the
  normal filesystem stack. Verified end-to-end against a DVD-Video disc in a USB
  HL-DT-ST DVDRAM GP65NW60 — volume `HANCOCK`, root listing `AUDIO_TS` /
  `VIDEO_TS`.
- **`browse::open_filesystem_from_reader(reader, filesystem)`** is the new
  source-agnostic half of `open_disc_filesystem`, so image-backed and
  device-backed discs share one dispatch. `open_disc_filesystem` keeps its exact
  previous behaviour; EFS still takes the path that supplies its partition
  offset, and GameCube/Wii still go to `nod` by file path.
- Medium capacity comes from the **generic** block-device ioctls
  (`DKIOCGETBLOCKCOUNT`/`DKIOCGETBLOCKSIZE` on macOS, `BLKGETSIZE64` on Linux),
  which — unlike the `DKIOCCD*` family — answer for DVD and Blu-ray too.
  `lseek(SEEK_END)` is not usable as the primary probe: it reports 0 on a macOS
  raw disk device. Adds a unix-only `libc` dependency for those two calls.
- On macOS the raw node (`/dev/rdiskN`) is used deliberately: the buffered
  `/dev/diskN` returns `EBUSY` while the disc is mounted, whereas the raw node
  reads fine.
- **Audio CDs remain out of scope** here — their sectors have no cooked form, so
  they genuinely do need MMC pass-through.
- **`PhysicalDisc::read_sectors(lba, count)`** reads a run of sectors in a single
  seek + read, and `read_bytes` is overridden to do the same. This is not a
  micro-optimisation: an optical drive charges seek and rotational latency *per
  I/O*, so reading one 2048-byte sector per request measured **~10 KB/s** against
  a DVD-R — days for a full disc. Reading the whole run per request measured
  **4.15 MB/s** on the same disc and drive.
- **`DiscImageInfo::open_physical(device)`** probes a loaded disc directly.
  [`DiscImageInfo::open`] is file-oriented — it sniffs a container format from the
  path — and on macOS it cannot even open a drive whose disc is mounted, because
  the buffered `/dev/diskN` node returns `EBUSY`. The physical probe reads the raw
  node as flat cooked sectors instead.
- **`DiscImageInfo::media_size_bytes`** reports the medium's capacity (from the
  drive's block count), so callers can show what a full rip will cost before
  starting one. `None` for image files, whose size is just the file's.

### Fixed

- **UDF volumes now report a volume label.** `volume_label` was populated from the
  ISO 9660 PVD, HFS, SGI, CD-i and Opera, but never UDF — which keeps its name in
  the Logical Volume Descriptor, not a PVD. Every DVD-Video and Blu-ray disc
  therefore probed with no label at all, even though browsing showed the name.
  Adds `udf::read_label`, mirroring the existing CD-i / Opera helpers.
- **The root directory entry now names itself `"/"`** instead of an empty string.
  An unnamed root gave browsers a tree node they could neither label nor click, so
  each one had to substitute `"/"` for itself. Fixed centrally in
  `FileEntry::root`, so every filesystem benefits rather than one at a time.

## 0.12.1

### Fixed — macOS optical drives (USB drives were invisible)

- **`drives::list_drives` found no drives at all on modern macOS.** The
  enumerator ran `ioreg -r -c IODVDDriveNub`, but that class is not what macOS
  publishes for USB-attached optical drives — they register as
  `IODVDServices` (and `IOCDServices` / `IOBDServices` for CD- and BD-class
  drives). On a machine whose only optical drive is external, `IODVDDriveNub`
  matches **zero** registry objects, so the call returned an empty `Vec` and
  callers reported "no optical drives connected". Verified against a USB
  HL-DT-ST DVDRAM GP65NW60, which now enumerates as `/dev/disk6`.
- All six drive classes are now queried (`IO{CD,DVD,BD}Services` plus the legacy
  `IO{CD,DVD,BD}DriveNub`, kept for internal ATAPI drives on older systems), and
  results are de-duplicated on the IOKit registry object id — one drive normally
  answers to more than one class.
- **Drive identity is parsed from the `Device Characteristics` dict.** Modern
  ioreg output packs `Vendor Name` / `Product Name` into a single-line dict
  written `"Vendor Name"="HL-DT-ST"` — no spaces around the `=`. The old parser
  only accepted the spaced `"Key" = "value"` spelling, so even a matched drive
  would have come out unnamed. `parse_str_value` now accepts either spelling.
- **`BSD Name` is read from the nested `IOMedia` node.** The disc's device node
  is published on a descendant of the drive, not on the drive object itself; the
  old parser split the output on every `+-o` occurrence, which put children in
  separate chunks and made the nested value unreachable. Parsing is now
  indentation-aware: each top-level `+-o` subtree is scanned as one unit.
- **Partition nodes are no longer mistaken for the disc.** A hybrid
  ISO9660/HFS disc publishes `disk6s1`, `disk6s1s2`, … alongside the whole-disc
  `disk6`; only the whole-disc node (`disk<digits>`) is now accepted as the
  device path.
- **A drive with an empty tray is listed instead of hidden.** macOS creates no
  `/dev` node until media is loaded, and the old code skipped any entry without
  a `BSD Name` — so a connected but empty drive vanished from the list entirely.
  Such a drive is now reported with an empty `device_path` and
  `is_loaded == false`, matching the Linux behaviour of listing an empty `sr0`.

## 0.12.0

### Added — native browsing of standalone NKit ISO images (GameCube)

- **`nkit_iso` module** with **`NkitIsoReader`** (`src/nkit_iso.rs`): standalone
  NKit ISO (`*.nkit.iso`) images are now browsed and extracted natively, instead
  of being refused. NKit shrinks a GameCube disc by stripping the console's
  deterministic "junk" padding and compacting the gaps between files, so the
  stripped image cannot be read as a plain ISO — the FST offsets point at the
  original disc layout while the file data sits at compacted offsets. Previously
  `browse` returned `Unsupported` for these and callers had to convert to
  ISO/RVZ first.
- `NkitIsoReader` reconstructs the original disc image on the fly (a faithful
  port of NKit's own recover-to-ISO path) and presents it as a cloneable
  `Read + Seek` stream, which `nod` then browses as an ordinary disc via
  `Disc::new_stream`. The junk padding is regenerated with the exact
  lagged-Fibonacci PRNG `nod` already ships (`nod::LaggedFibonacci`), so the
  rebuild is byte-for-byte identical to the source disc — verified against the
  CRC32 stored in the NKit header.
- Reconstruction parses the header at `0x200`, reads the system area + FST
  verbatim, walks the compacted file/gap stream (gap kinds: all-junk,
  all-scrubbed, and mixed junk / preserved-bytes / byte-fill runs), regenerates
  junk regions, and rewrites the FST/DOL offsets to the recovered layout — all as
  a lazy region map, so browsing touches only the sectors it needs rather than
  materialising the full 1.46 GB disc.
- Scope is the **v1** standalone ISO (`NKIT v01`), the format the NKit tool
  writes for `.nkit.iso` — every standalone image observed in the wild is v01.
  "NKit v2" is *not* a standalone-ISO format: it is NKit-lossless data embedded
  in a CISO/WBFS/RVZ block map, which `nod` already reads natively, so those
  images go straight to `nod` and never reach this reader. A standalone
  `NKIT v02` marker (or a Wii NKit ISO, or any malformed image) is rejected with
  a clear `FilesystemError` rather than guessed at or hung on.
- `browse::open_disc_filesystem` / `NodeFilesystem::new` route NKit images
  through the reconstructor transparently; detection (`DiscImageInfo::open`
  reporting `nintendo` / `gamecube`) is unchanged. Malformed or unknown-variant
  images return a clear `FilesystemError` rather than blocking.

## 0.11.1

### Added — per-volume allocation-unit accessor on the browse `Filesystem` trait

- **`Filesystem::allocation_unit(&self) -> Option<u64>`** (`browse/filesystem.rs`):
  a new trait method reporting the volume's fixed allocation/logical block size in
  bytes, when it has one — the unit a fork's length is rounded up to for its real
  on-disk footprint. It has a `None` default, so the addition is additive and
  backward-compatible (no existing impl breaks). This lets a downstream consumer
  (e.g. `rb-cli du`) model both-fork allocated bytes for optical HFS/HFS+ discs,
  mirroring the same accessor already on rusty-backup's block-image `Filesystem`.
- Implemented on every backend with a single fixed unit: **HFS** (`drAlBlkSiz`),
  **HFS+** (`blockSize`), **ISO 9660** and **CD-i** (2048-byte logical sector),
  **UDF** and **3DO Opera** (their logical `block_size`), **SGI EFS** (512-byte
  basic block), and **Xbox XDVDFS** (2048-byte sector). HFS/HFS+ guard a zero
  block size to `None`.
- Left at the `None` default where the format has no single fixed unit: **UFS**
  (block + fragment allocation), **ODS-2** (allocates in volume clusters; the
  cluster factor is not parsed), and **GameCube/Wii** (storage abstracted by the
  `nod` crate).

## 0.11.0

### Added — hybrid Mac/PC disc detection

- **Hybrid filesystems** (`detect.rs`): a disc can carry two filesystems over one
  data track — an ISO 9660 volume (the PC side) plus an `Apple_HFS` /
  `Apple_HFS+` partition (the Mac side), separated by an Apple Partition Map
  rather than by disc sessions. Only one filesystem can be the primary
  `DiscImageInfo::filesystem`, so previously the HFS side of these common 90s
  Mac/PC game discs was invisible — ISO 9660 won the probe and the Apple
  partition was never surfaced.
- `DiscImageInfo` gains a `hybrid_filesystems: Vec<HybridFilesystem>` field,
  populated during `open` by probing the APM regardless of the primary
  filesystem. Each `HybridFilesystem` carries its `FilesystemType`, volume label
  (read from the MDB / volume header), and `partition_offset`. A pure-HFS disc —
  where the APM's HFS partition *is* the primary — yields an empty list, so the
  same partition is never reported twice.
- `browse::open_hybrid_filesystem(&info, index)` opens one of those co-resident
  partitions, mirroring `open_disc_filesystem`'s HFS handling (raw offset for
  HFS, resolved offset for embedded HFS+). Everything reads through the same
  cooked `SectorReader`, so the APM's `start_block * 512` offsets resolve
  correctly regardless of the container's physical sector size. (Verified against
  real Toast-mastered hybrid CDs — *The Incredible Machine 3* and *Age of
  Empires II Gold*.)

## 0.10.0

### Added — El Torito boot-catalog support

- **El Torito** (`el_torito.rs`): a new module parses the boot catalog of a
  bootable CD and exposes it as `DiscImageInfo::el_torito: Option<ElTorito>`,
  populated during `DiscImageInfo::open` across every container (`.iso`, BIN/CUE,
  CHD, …) via the existing `SectorReader`. It walks the whole catalog — the
  validation entry, the initial/default entry, and every section header
  (`0x90`/`0x91`) with its section entries — surfacing all boot images (e.g. an
  x86 floppy plus a UEFI image) as `BootEntry` values with platform, bootable
  flag, media type, load RBA, sector count, system type, and a computed image
  size (fixed floppy geometry, `sector_count * 512` for no-emulation, or the last
  populated MBR partition end for hard-disk emulation). `BootEntry::image_extent`
  gives the on-disc `(offset, length)` and `el_torito::read_boot_image` reads the
  raw image bytes container-agnostically. Detection is lenient: a missing or
  malformed catalog leaves `el_torito == None` and never fails an otherwise-good
  open. The crate deliberately does **not** interpret a boot image as a
  filesystem — that stays with consumers. (Byte-verified against a real Plop Boot
  Manager El Torito CD.)

### Added — El Torito boot-catalog editing (write path)

- **El Torito editing** (`el_torito_edit.rs`): `ElToritoEditor` edits the boot
  catalog of a **raw `.iso`** over a `Read + Write + Seek` handle (or
  `open_path`). It can flip an entry bootable/off, change its platform / media
  type / system type, `replace_image` with a new opaque blob, and `add_entry` /
  `remove_entry`. Edits are staged and written by `commit`, which orders writes
  for crash-safety (appended images and any relocated catalog first, then the
  Boot Record VD pointer and PVD size). Same-size image replacement is in place;
  a grown image is relocated to appended free space with `load_rba` /
  `sector_count` / the catalog updated and the PVD `volume_space_size` bumped
  (LE **and** BE). A metadata edit rewrites the catalog in its existing sector and
  recomputes the validation-entry checksum. BIN/CUE and CHD are rejected (convert
  to `.iso` first); leaked dead space from relocations/removals is documented and
  out of scope to reclaim. A stretch `make_bootable` adds El Torito to a
  non-bootable ISO when a free volume-descriptor slot exists, else returns a clear
  "needs remaster" error. This crate still never interprets a boot image as a
  filesystem — consumers edit the FAT/NTFS/… inside and hand over finished bytes.
  (Byte-verified: a same-size round-trip of a real Plop Boot Manager CD is
  byte-identical.)

## 0.9.0

### Added — more disc-image containers

- **DAEMON Tools (`.mdx`)** (`mdx.rs`, opt-in `mdx` feature): a new
  `DiscFormat::Mdx` browses DAEMON Tools "MDSv2" images. Unlike Alcohol `.mds`,
  an MDX **always** stores its descriptor AES-256-encrypted + zlib-compressed and
  its track data as zlib-compressed sector groups, so support pulls in a small
  crypto stack (`aes`, `pbkdf2`, `ripemd`) and is gated behind the off-by-default
  `mdx` feature — the base build stays dependency-light, and `.mdx` files are
  recognised but reported unbrowsable without it. The descriptor decrypt is a
  TrueCrypt-derived scheme (salt-unshuffle → PBKDF2/RIPE-MD-160 → AES-256 in a
  custom CBC-with-de-whitening), and an `MdxSectorReader` inflates sector groups
  on the fly (none/RLE/deflate). Encrypted *track data* (MDSv2 AES-XTS) is
  detected and rejected cleanly. Ported from cdemu/libmirage `image-mdx`.
  (Descriptor decrypt verified against real AKAI sampler `.mdx` images — magic,
  key CRC, and inflate sizes all match — which detect as the MDX container but
  carry a proprietary non-ISO filesystem; the full browse path plus the
  none/RLE/deflate reader are covered by an end-to-end synthetic test that builds
  a correctly-encrypted, compressed MDX wrapping an ISO 9660 volume.)
- **DiscJuggler (`.cdi`)** (`discjuggler.rs`): a new `DiscFormat::DiscJuggler`
  parses the end-anchored, variable-length track descriptor (last 4 bytes are its
  length) and browses the `.cdi` data through per-track sector geometry. Named
  for the container, distinct from the Philips `FilesystemType::Cdi`. The
  150-sector start pregap is physically present, so a track's user data begins
  past it. Dreamcast GD-ROM rips — the common case — author their high-density
  game session's ISO 9660 volume with **absolute** directory LBAs (the session's
  `start_address`) while keeping the PVD volume-relative at LBA 16; browsing the
  last (game) data track through a new `sector_reader::RebaseSectorReader`
  resolves both. Format referenced from cdemu/libmirage. (Verified against real
  Dreamcast rips — *Half-Life [DCRES]* and *Capcom vs. SNK 2* — browsing and
  reading every file with zero failures; ISO 9660 browse + absolute-LBA rebase
  also covered by end-to-end synthetic tests.)
- **Nero (`.nrg`)** (`nrg.rs`): a new `DiscFormat::Nrg` parses the footer-anchored
  chunk collection (both `NER5`/v2 64-bit and `NERO`/v1 32-bit) and browses the
  `.nrg` data via the DAO track offsets and per-track sector geometry. Format
  referenced from cdemu/libmirage. (Container verified against real Nero images;
  ISO 9660 browsing covered by an end-to-end synthetic test. Nero images of
  non-ISO discs — e.g. AKAI sampler CDs — parse as the NRG container but report
  an unknown filesystem, as expected.)
- **Alcohol 120% (`.mds` / `.mdf`)** (`mds.rs`): the previously-stubbed
  `DiscFormat::MdsMdf` now parses the binary `MEDIA DESCRIPTOR` header/session/
  track blocks and browses the `.mdf`. Handles the common case of 2448-byte
  sectors (2352 data + 96 appended subchannel) via a new
  `BinCueSectorReader::with_layout` for explicit sector geometry; lead-in/out
  metadata blocks are skipped. Either the `.mds` or the `.mdf` may be opened.
  Format referenced from cdemu/libmirage. (Verified against real Alcohol images.)
  Note: DAEMON Tools `.mdx` (MDSv2, which adds optional encryption/compression)
  is **not** covered by this and remains unsupported.
- **CloneCD (`.ccd` / `.img` / `.sub`)** (`ccd.rs`): a new `DiscFormat::CloneCd`
  parses the INI `.ccd` descriptor into tracks and browses the flat 2352-byte
  `.img` through the shared `BinCueSectorReader`. Mode 1 and Mode 2 data tracks
  and mixed audio+data discs are handled; the `.sub` subchannel file is ignored.
  Format layout referenced from cdemu/libmirage. (Verified against real Die Hard
  Trilogy 2 and Tyrian 2000 images.)

### Added — PlayStation 3 identification

- **PS3 disc ISOs are identified** (`Console::Ps3`). PS3 discs already *browsed*
  through the ISO 9660 bridge — its directory metadata is plaintext even on
  encrypted (redump) dumps, so no disc key is needed — but were unidentified.
  Detection now keys on `PS3_DISC.SFB` in the root and parses
  `PS3_GAME/PARAM.SFO` for the display title and title ID (serial), deriving the
  region from the serial prefix. A new `PARAM.SFO` parser and a
  subdirectory-file reader support this. Fully **decrypted** ISOs also read file
  contents; encrypted redump ISOs still browse and identify (only file *data*
  regions are ciphertext).

### Added — Xbox XDVDFS browsing + identification

- **Xbox / Xbox 360 XDVDFS support** (`browse/xdvdfs.rs`): a new
  `FilesystemType::Xdvdfs` and `XdvdfsFilesystem` browse Microsoft Xbox game
  discs (little-endian, binary-tree directories). Detection probes the
  `MICROSOFT*XBOX*MEDIA` magic at each candidate game-partition base
  (`0`, XGD2, XGD3, XGD1-full), so `.iso` dumps of original Xbox discs — and
  Xbox 360 XDVDFS ISOs — now browse. Xbox discs are identified (`Console::Xbox`)
  by parsing `default.xbe`'s certificate for the title, title ID (serial), and
  region.

### Fixed — NKit-processed GC/Wii images no longer hang

- **NKit images are refused cleanly instead of hanging.** An `.nkit.iso`
  preserves the disc header (so it still *identifies* as the correct GC/Wii
  game) but scrubs and rearranges the partition data, which `nod` cannot
  reconstruct — previously browsing one would hang indefinitely. `NodeFilesystem`
  now detects the `NKIT` marker at byte `0x200` and returns
  `FilesystemError::Unsupported` immediately; convert such images to a full
  ISO/RVZ to browse them.

### Added — PSP CSO, gzip-compressed images, and PSP identification

- **PSP `.cso` (CISOv1) support** (`cso.rs`): a new [`DiscFormat::Cso`] and
  `CsoSectorReader` decompress a PSP UMD's raw-DEFLATE blocks on demand, exposing
  the underlying ISO 9660 volume to the standard browser. PSP discs are
  identified (`Console::Psp`) via `UMD_DATA.BIN`, with the serial taken from its
  first field.
- **gzip-compressed image (`.gz`) support** (`gz.rs`): a new [`DiscFormat::Gz`]
  and `GzSectorReader` present a gzip-compressed disc image (typically a PS2 ISO,
  as used by PCSX2) as cooked sectors. Because gzip has no random access, the
  reader decompresses forward and restarts on backward seeks — **directory
  listing is fast** (structures sit near the start), while reading the *contents*
  of a file deep in a multi-gigabyte image is proportionally slower.
  - To keep opening fast, gz images prefer the **ISO 9660 bridge tree** over UDF
    (whose anchor sits at the end of the disc) and, during game identification,
    avoid reading boot files that live deep in the image: PS2 discs are
    identified from the PVD `PLAYSTATION` marker plus the boot-ELF filename in the
    root directory (serial) and the presence of a UDF bridge (PS1 vs PS2) — all
    near-the-start metadata. New `detect_game_disc_opts` /
    `probe_filesystem_opts` carry the `deep_reads` / `prefer_iso` options.

### Fixed — Dreamcast GD-ROM multi-track high-density areas

- **GD-ROM high-density areas that span multiple data tracks now browse and read
  fully.** A GD-ROM's HD area is often split across several data tracks (e.g.
  track 3 and track 5, separated by audio tracks); the reader previously wrapped
  only the first HD track and rebased every absolute LBA by a fixed 45000, so any
  file or subdirectory whose extent lived in a later data track was unreadable
  (`failed to fill whole buffer`). `GdromSectorReader` now holds every HD data
  track and routes each absolute LBA to the track that physically contains it,
  reading it track-relative. Applies to both CHD (`browse::open_disc_filesystem`)
  and `.gdi` (`gdi::open_gdi_hd_reader`) sources. New
  `ChdInfo::find_gdrom_hd_tracks` and `GdromSectorReader::from_tracks` /
  `GdromHdTrack` support this; the single-track `GdromSectorReader::new` is
  retained. Verified against *NFL Blitz 2001* and *4x4 Evo* (files in later
  tracks, including subfolder contents, now read with correct size).

### Fixed — PC Engine CD Shift-JIS titles

- **Japanese PC Engine CD-ROM² titles now decode correctly.** The title field
  (sector 1, offset `0x6A`) is Shift-JIS, but was read with the ASCII-only field
  helper, which replaced every multi-byte character with a space (e.g.
  `m   @   X V`). A new Shift-JIS field decoder (via the added `encoding_rs`
  dependency) now yields the real title — e.g. `夢幻戦士ヴァリスⅢ`. Pure-ASCII
  titles are unaffected.

## 0.8.0

> **⚠️ Breaking change.** `DiscFormat` gained `Gdi` and `Nintendo`; `FilesystemType`
> gained `GameCube`, `Wii`, `Cdi`, and `Opera`. Code that exhaustively matches
> either enum must handle them (or add a wildcard arm). `DiscImageInfo` also gained
> a `game: Option<GameDiscInfo>` field.

### Added — UDF 2.50 (Blu-ray) metadata partitions

- **UDF metadata-partition support** (`browse/udf.rs`): the UDF browser now
  resolves every on-disc address as a `(logical block number, partition
  reference)` pair through the Logical Volume Descriptor's partition maps. Type-2
  `*UDF Metadata Partition` maps (used by Blu-ray / UDF 2.50+) are translated
  through the metadata file's extents into the underlying physical partition, so
  BDMV discs — whose file entries and directories live in the metadata partition
  while file data lives in the physical partition — now browse. Unknown type-2
  maps (virtual/sparable) are kept index-aligned and error only if referenced.
- **Extent-aware `read_file_range`** for UDF: a partial read now touches only the
  overlapping extents instead of loading the whole file, so seeking into a
  multi-gigabyte Blu-ray `.m2ts` stream is cheap.

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
