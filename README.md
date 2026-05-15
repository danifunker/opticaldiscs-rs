# opticaldiscs

[![CI](https://github.com/dani/opticaldiscs-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/dani/opticaldiscs-rs/actions)
[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)

Format-agnostic optical disc image reading and filesystem browsing for Rust.

Provides a unified `SectorReader` abstraction that handles the cooked/raw sector
translation across three container formats — **ISO**, **BIN/CUE**, and **CHD** — with
filesystem browsers for **ISO 9660**, **HFS**, **HFS+**, and **SGI EFS** on top.

> **Status:** Early development. See [PLAN.md](PLAN.md) for the implementation roadmap.

## Features

| Capability | Status |
|---|---|
| ISO sector reader | Phase 2 |
| BIN/CUE sector reader (raw 2352-byte) | Phase 3 |
| CHD sector reader | Phase 4 |
| TOC + MusicBrainz/FreeDB DiscID | Phase 5 (`toc` feature) |
| Disc format + filesystem auto-detection | Phase 6 |
| ISO 9660 filesystem browser | Phase 7 |
| HFS / HFS+ filesystem browser | Phase 8 |
| SGI Volume Header + EFS filesystem browser (IRIX CDs) | 0.3.0 |
| Physical optical drive enumeration | Phase 9 (`drives` feature) |

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
# git dependency while API stabilises
opticaldiscs = { git = "https://github.com/dani/opticaldiscs-rs" }

# with optional features
opticaldiscs = { git = "...", features = ["toc", "drives"] }
```

## Feature Flags

| Flag | Enables | Extra deps |
|---|---|---|
| `toc` | `DiscTOC`, MusicBrainz DiscID, FreeDB ID | `sha1`, `base64` |
| `drives` | `list_drives()` — enumerate physical optical drives | — |

## Used By

- [rusty-backup](https://github.com/dani/rusty-backup) — vintage disc backup/restore tool
- [ODE-artwork-downloader](https://github.com/dani/ODE-artwork-downloader) — USBODE cover art downloader

## License

GPL-3.0 — see [LICENSE](LICENSE).
