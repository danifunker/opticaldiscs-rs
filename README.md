# opticaldiscs

[![crates.io](https://img.shields.io/crates/v/opticaldiscs.svg)](https://crates.io/crates/opticaldiscs)
[![CI](https://github.com/danifunker/opticaldiscs-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/danifunker/opticaldiscs-rs/actions)
[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)

Format-agnostic optical disc image reading and filesystem browsing for Rust.

Provides a unified `SectorReader` abstraction that handles the cooked/raw sector
translation across three container formats — **ISO**, **BIN/CUE**, and **CHD** — with
filesystem browsers for **ISO 9660**, **HFS**, **HFS+**, and **SGI EFS** on top.

> **Status:** Published on [crates.io](https://crates.io/crates/opticaldiscs). Disc
> reading, detection, and filesystem browsing are complete and in production use;
> physical-disc ripping is the next milestone. See [PLAN.md](PLAN.md) for the roadmap.

## CHD support

opticaldiscs-rs reads CHD optical images via [libchdman-rs](https://github.com/danifunker/libchdman-rs),
which wraps MAME's official `chd_file` core. This provides byte-for-byte
parity with the `chdman` tool — including subcode handling, audio
byte-swapping, and per-track frame semantics — at the cost of needing
to link MAME's C++ code.

By default, opticaldiscs-rs enables libchdman-rs's `prebuilt` feature,
which downloads a pre-built static archive matching the build target
from libchdman-rs's GitHub Releases instead of compiling MAME from
source. This keeps CI build times in seconds, not minutes.

### When you might need to override prebuilt behavior

| Situation | Set env var |
|---|---|
| Target triple isn't covered by libchdman-rs's prebuilt matrix | `LIBCHDMAN_PREBUILT_FALLBACK=1` |
| Local development without network access | `LIBCHDMAN_FORCE_SOURCE=1` |
| Linux: pick a specific glibc floor for the prebuilt archive | `LIBCHDMAN_GLIBC=2.31` (or `2.35`, `2.39`) |

See [libchdman-rs's README](https://github.com/danifunker/libchdman-rs#pre-built-static-archives-faster-ci-builds)
for the full list of supported targets, glibc floors, and escape hatches.

## Features

| Capability | Status |
|---|---|
| ISO sector reader | ✓ |
| BIN/CUE sector reader (raw 2352-byte) | ✓ |
| CHD sector reader (via libchdman-rs) | ✓ |
| Disc format + filesystem auto-detection | ✓ |
| ISO 9660 filesystem browser | ✓ |
| ISO 9660 PVD date/time metadata (creation/modification/expiration/effective) | ✓ (since 0.4.4) |
| Joliet (Unicode long names) | ✓ (since 0.6.0) |
| Rock Ridge / SUSP (POSIX metadata, long names, symlinks, timestamps) | ✓ (since 0.6.0) |
| Per-file timestamps + POSIX ownership on `FileEntry` (all filesystems) | ✓ (since 0.6.0) |
| High Sierra Format browser (pre-ISO 9660 `CDROM` discs) | ✓ (since 0.7.0) |
| Raw 2352-byte-sector auto-detect in a bare `.iso` | ✓ (since 0.7.0) |
| HFS / HFS+ filesystem browser | ✓ |
| SGI Volume Header + EFS filesystem browser (IRIX CDs) | ✓ (since 0.3.0) |
| UFS1 / FFS browser — Digital UNIX/Tru64, SunOS; NeXT/OpenStep/Rhapsody | ✓ (since 0.7.0) |
| VMS ODS-2 / Files-11 browser (OpenVMS VAX/Alpha) | ✓ (since 0.7.0) |
| UDF browser — DVD / data discs (physical partition, UDF 1.02–2.01) | ✓ (since 0.7.0) |
| UDF 2.50+ metadata-partition browser (Blu-ray / metadata discs) | ◻ detected only — planned for a future release |
| Game-disc identification — console + serial/title/region (PS1/PS2, Saturn, Mega-CD, Dreamcast, PC-FX, PC Engine CD, CD32, Neo Geo CD, 3DO, CD-i, GameCube, Wii) | ✓ (since 0.8.0) |
| GameCube & Wii filesystem browser (ISO/GCM, RVZ/WIA, WBFS, CISO, GCZ, TGC; Wii decryption — via [`nod`](https://github.com/encounter/nod)) | ✓ (since 0.8.0) |
| Dreamcast GD-ROM browser — high-density area (CHD + `.gdi`) | ✓ (since 0.8.0) |
| Philips CD-i (Green Book) filesystem browser | ✓ (since 0.8.0) |
| 3DO Opera filesystem browser | ✓ (since 0.8.0) |
| TOC + MusicBrainz/FreeDB DiscID | ✓ (`toc` feature) |
| Physical optical drive enumeration | ✓ (`drives` feature) |

## Quick Example

```rust
use opticaldiscs::detect::DiscImageInfo;
use opticaldiscs::browse;

let info = DiscImageInfo::open("disc.iso")?;
println!("Volume: {:?}", info.volume_label);
println!("Filesystem: {}", info.filesystem.display_name());

let mut fs = browse::open_disc_filesystem(&info)?;
let root = fs.root()?;
for entry in fs.list_directory(&root)? {
    println!("{:<40} {}", entry.name, entry.size_string());
}
```

## Cargo.toml

```toml
opticaldiscs = "0.4"

# with optional features
opticaldiscs = { version = "0.4", features = ["toc", "drives"] }
```

To track unreleased changes, depend on the git repository instead:

```toml
opticaldiscs = { git = "https://github.com/danifunker/opticaldiscs-rs" }
```

## Feature Flags

| Flag | Enables | Extra deps |
|---|---|---|
| `toc` | `DiscTOC`, MusicBrainz DiscID, FreeDB ID | `sha1`, `base64` |
| `drives` | `list_drives()` — enumerate physical optical drives | — |

## Used By

- [rusty-backup](https://github.com/danifunker/rusty-backup) — vintage disc backup/restore tool
- [ODE-artwork-downloader](https://github.com/danifunker/ODE-artwork-downloader) — USBODE cover art downloader

## License

GPL-3.0 — see [LICENSE](LICENSE).
