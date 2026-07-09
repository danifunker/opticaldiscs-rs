# Downstream Upgrade Guide

This guide is for projects that depend on `opticaldiscs`. It covers two upgrade waves:

- [**0.9 — more disc-image containers**](#09--more-disc-image-containers): CloneCD,
  Nero (NRG), Alcohol 120% (MDS/MDF), DiscJuggler (CDI), DAEMON Tools (MDX), PSP CSO,
  and gzip-compressed images.
- [**0.8 — game-disc support**](#08--game-disc-support): console/game identification
  plus browsing for Nintendo GameCube & Wii, Sega Dreamcast (GD-ROM), Philips CD-i, 3DO.

If you maintain a consuming project (a CLI, GUI, ripper, artwork tool, …), hand the
relevant **pickup prompt** to an agent in that repo, or follow the checklist yourself.

---

# 0.9 — More disc-image containers

**0.9** adds several new container formats. Every one browses through the existing
`open_disc_filesystem(&info)?` path and returns the usual `Box<dyn Filesystem>` — the
only source-breaking change is, again, new `DiscFormat` enum variants.

## Will it "just work"? (0.9)

| Aspect | Automatic? | Notes |
|---|---|---|
| Opening/browsing the new containers | ✅ | `DiscImageInfo::open()` + `open_disc_filesystem()` detect and browse them; listing/reading are unchanged |
| New file extensions in open dialogs | ✅ | already in `formats::supported_extensions()` (`ccd`, `img`, `sub`, `nrg`, `mds`, `mdf`, `cdi`, `cso`, `gz`) |
| Exhaustive `match` on `DiscFormat` | ❌ **breaks** | new variants → add arms or a `_ =>` wildcard (compile error until you do) |
| Browsing `.mdx` | ⚠️ opt-in | requires the new `mdx` Cargo feature (a crypto stack); without it, `.mdx` is recognised but reported unbrowsable |

### New `DiscFormat` variants (source-breaking for exhaustive matches)
- `DiscFormat::Cso` — PSP `.cso` (CISOv1) compressed ISO
- `DiscFormat::Gz` — gzip-compressed disc image (`.gz`, typically a PS2 ISO)
- `DiscFormat::CloneCd` — CloneCD (`.ccd` + `.img` + optional `.sub`)
- `DiscFormat::Nrg` — Nero (`.nrg`)
- `DiscFormat::MdsMdf` — Alcohol 120% (`.mds` / `.mdf`) — previously a stub, now implemented
- `DiscFormat::DiscJuggler` — DiscJuggler (`.cdi`); handles Dreamcast GD-ROM rips whose
  ISO 9660 volume uses absolute directory LBAs
- `DiscFormat::Mdx` — DAEMON Tools (`.mdx`); browsable only with the `mdx` feature (see below)

No new `FilesystemType` variants — these containers wrap the filesystems you already
handle (ISO 9660, UDF, etc.).

### The `mdx` feature (optional crypto stack)
An MDX descriptor is **always** AES-256-encrypted and zlib-compressed, so support
pulls in `aes`, `pbkdf2`, and `ripemd`. It is **off by default**:

```toml
opticaldiscs = { version = "0.9", features = ["mdx"] }
```

- **With** `mdx`: `.mdx` images decrypt, decompress, and browse like any other container.
  Encrypted *track data* (MDSv2 AES-XTS) is detected and rejected with a clear error.
- **Without** `mdx`: `.mdx` files are still detected as `DiscFormat::Mdx`, but
  `DiscImageInfo::open()` / `open_disc_filesystem()` return an "unsupported" error
  instead of pulling in the crypto crates.

`DiscFormat::Mdx` exists in the enum regardless of the feature, so your `match`
arms compile either way.

### Upgrade checklist (0.9)
1. Bump the dependency to `0.9` (keep existing feature flags; add `"mdx"` if you want
   DAEMON Tools browsing).
2. `cargo build` and fix every non-exhaustive-match error on `DiscFormat` (add arms
   for the variants above or a `_ =>` wildcard, matching how you treat unknown formats).
3. File dialogs / format filters: keep using `formats::supported_extensions()` so the
   new extensions appear automatically.
4. Browsing needs no changes — `open_disc_filesystem(&info)?` then `root()` /
   `list_directory()` / `read_file()` work identically for the new containers.
5. Run the test suite to confirm nothing else broke from the enum changes.

### Pickup prompt (0.9 — paste into the downstream repo)

```
The `opticaldiscs` crate this project depends on gained new disc-image container
formats in 0.9. Integrate it here.

WHAT'S NEW:
- New `DiscFormat` variants: `Cso`, `Gz`, `CloneCd`, `Nrg`, `MdsMdf`, `DiscJuggler`, `Mdx`.
  These are container formats; there are NO new `FilesystemType` variants.
- `browse::open_disc_filesystem(&info)` transparently browses all of them and still
  returns `Box<dyn Filesystem>`; listing/reading are unchanged.
- `.mdx` (DAEMON Tools) browsing is behind an optional `mdx` Cargo feature (it pulls in
  aes/pbkdf2/ripemd). Without the feature, `.mdx` is detected but reported unbrowsable.
- New recognized extensions (already in `formats::supported_extensions()`):
  ccd, img, sub, nrg, mds, mdf, cdi, cso, gz.

TASKS:
1. Set `opticaldiscs = "0.9"` in Cargo.toml (keep existing feature flags). Add the
   "mdx" feature only if you want DAEMON Tools `.mdx` browsing.
2. `cargo build` and fix every non-exhaustive-match error on `DiscFormat` (add arms or
   `_ =>`, matching how this project handles unknown/unsupported formats).
3. Ensure any file-open filter uses `formats::supported_extensions()`.
4. Run the tests and confirm nothing else broke.

Report which files changed and any matches you had to extend.
```

---

# 0.8 — Game-disc support

Console/game identification plus browsing for Nintendo GameCube & Wii, Sega Dreamcast
(GD-ROM), Philips CD-i, and 3DO.

## Will it "just work"?

Mostly yes — with one source-breaking change to handle.

| Aspect | Automatic? | Notes |
|---|---|---|
| `browse::open_disc_filesystem()` browsing the new consoles | ✅ | dispatch is internal; a GameCube/Wii/Dreamcast/CD-i/3DO disc returns the usual `Box<dyn Filesystem>` — directory listing and file reads are unchanged |
| Reading `info.game` for console / serial / title / region | ✅ | new additive field on `DiscImageInfo` |
| New file extensions in open dialogs | ✅ | already included in `formats::supported_extensions()` |
| Wii decryption | ✅ | handled internally — **no key file required from the user** |
| Exhaustive `match` on `DiscFormat` / `FilesystemType` | ❌ **breaks** | new enum variants → add arms or a `_ =>` wildcard (compile error until you do) |
| Build time / binary size | ⚠️ larger | pulls in the `nod` crate and its zstd/bzip2/lzma codecs |

---

## What changed in the public API (all additive except the enum variants)

### New: game identity on `DiscImageInfo`
```rust
pub struct DiscImageInfo {
    // … existing fields …
    pub game: Option<opticaldiscs::gameid::GameDiscInfo>,
}

pub struct GameDiscInfo {
    pub console: Console,           // e.g. Console::Ps1, ::GameCube, ::SegaDreamcast
    pub serial:  Option<String>,    // normalized, e.g. "SLUS-00555", "T-1234N", "GALE01"
    pub title:   Option<String>,
    pub region:  Option<Region>,    // NtscU / NtscJ / Pal / Asia / Korea / World
    pub maker:   Option<String>,
    pub version: Option<String>,
}
```
`Console` and `Region` are re-exported at the crate root
(`opticaldiscs::{Console, Region, GameDiscInfo}`) and both expose
`.display_name() -> &'static str`. `game` is `None` for non-game discs.

### New enum variants (source-breaking for exhaustive matches)
- `DiscFormat::Gdi` — Dreamcast `.gdi` track descriptor
- `DiscFormat::Nintendo` — GameCube/Wii containers (`.gcm .rvz .wbfs .ciso .gcz .wia .tgc .nfs`)
- `FilesystemType::GameCube`, `FilesystemType::Wii`, `FilesystemType::Cdi`, `FilesystemType::Opera`

`FilesystemType::is_browsable()` returns `true` for all four, and
`open_disc_filesystem()` handles them. Raw GameCube/Wii dumps that carry a plain
`.iso` extension are auto-detected by magic and routed correctly.

### New public items you can use directly (optional)
Most callers only need `open_disc_filesystem`, but these are all re-exported at
the **crate root** for direct use:
- Every filesystem browser: `NodeFilesystem` (GameCube/Wii), `CdiFilesystem`,
  `OperaFilesystem`, `UdfFilesystem`, `Iso9660Filesystem`, `HfsFilesystem`,
  `HfsPlusFilesystem`, `UfsFilesystem`, `Ods2Filesystem`, `EfsFilesystem` — each
  constructible from a `Box<dyn SectorReader>` (or a path, for `NodeFilesystem`).
- `detect_game_disc(reader, pvd)` — identify a console/game from any `SectorReader`.
- `GdromSectorReader` + `GDROM_HD_START_LBA` — Dreamcast HD-area sector reader
  (alongside `BinCueSectorReader` / `ChdSectorReader`).

Also available under their modules:
- `opticaldiscs::gdi` — `.gdi` parsing (`GdiTrack`, `parse_gdi`, `open_gdi_hd_reader`)
- `opticaldiscs::chd::ChdInfo::is_gdrom()` / `find_gdrom_hd_track()`
- Standalone filesystem detectors: `browse::{udf::detect_udf, cdi::detect_cdi,
  opera::detect_opera, nod_fs::probe_nintendo}`

### Dependency
Adds `nod` (dual MIT/Apache-2.0), which provides GameCube/Wii reading including
the compressed container formats and Wii AES decryption. It bundles the Wii common
keys, so nothing needs to be supplied at runtime. Note it links compression codecs,
so build time and binary size grow.

---

## Upgrade checklist

1. **Bump the dependency** to `0.8`:
   ```toml
   opticaldiscs = "0.8"
   # keep any feature flags you already use, e.g.:
   # opticaldiscs = { version = "0.8", features = ["toc", "drives"] }
   ```
2. **`cargo build`** and fix every non-exhaustive-match error on `DiscFormat` and
   `FilesystemType`. Add arms for the new variants, or a `_ =>` wildcard consistent
   with how the project already treats unknown/unsupported formats.
3. **Surface the game identity** wherever the project shows disc metadata:
   ```rust
   if let Some(g) = &info.game {
       println!("{} — {}{}",
           g.console.display_name(),
           g.title.as_deref().unwrap_or("(untitled)"),
           g.serial.as_deref().map(|s| format!(" [{s}]")).unwrap_or_default());
   }
   ```
   Always handle `None` gracefully — non-game discs still return `None`.
4. **File dialogs / format filters**: use `formats::supported_extensions()` so the new
   extensions (`gdi`, `gcm`, `rvz`, `wbfs`, `ciso`, `gcz`, `wia`, `tgc`, `nfs`) appear.
5. **Browsing** needs no changes — `open_disc_filesystem(&info)?` then
   `root()` / `list_directory()` / `read_file()` work identically for the new consoles.
6. Run the test suite to confirm nothing else broke from the enum changes.

Do **not** vendor or hardcode any console keys — decryption is handled inside the crate.

---

## Minimal end-to-end example

```rust
use opticaldiscs::detect::DiscImageInfo;
use opticaldiscs::browse;

let info = DiscImageInfo::open("game.rvz")?;         // or .chd, .gdi, .iso, .wbfs, …
println!("Filesystem: {}", info.filesystem.display_name());
if let Some(g) = &info.game {
    println!("Console: {}  Serial: {:?}", g.console.display_name(), g.serial);
}

let mut fs = browse::open_disc_filesystem(&info)?;
let root = fs.root()?;
for entry in fs.list_directory(&root)? {
    println!("{:<40} {}", entry.name, entry.size_string());
}
```

---

## Pickup prompt (paste into the downstream repo)

```
The `opticaldiscs` crate this project depends on gained video-game optical-disc
support in 0.8. Integrate it here.

WHAT'S NEW:
- `DiscImageInfo` has a new field `game: Option<opticaldiscs::gameid::GameDiscInfo>`
  with `{ console: Console, serial, title, region: Option<Region>, maker, version }`.
  `Console` and `Region` are re-exported at the crate root and have `.display_name()`.
- New `DiscFormat` variants: `Gdi`, `Nintendo`.
- New `FilesystemType` variants: `GameCube`, `Wii`, `Cdi`, `Opera`.
- `browse::open_disc_filesystem(&info)` transparently browses GameCube/Wii (Wii
  decrypted internally — no key file needed), Dreamcast GD-ROM (CHD + .gdi), CD-i,
  and 3DO. It still returns `Box<dyn Filesystem>`; listing/reading are unchanged.
- New recognized extensions: gdi, gcm, rvz, wbfs, ciso, gcz, wia, tgc, nfs (already
  in `formats::supported_extensions()`).
- Adds a transitive `nod` dependency (compression codecs → larger build).

TASKS:
1. Set `opticaldiscs = "0.8"` in Cargo.toml (keep existing feature flags).
2. `cargo build` and fix every non-exhaustive-match error on `DiscFormat` and
   `FilesystemType` (add arms or `_ =>`, matching how this project handles
   unknown/unsupported formats).
3. Surface `info.game` (console / serial / title / region) wherever disc metadata is
   shown; handle `None` gracefully.
4. Ensure any file-open filter uses `formats::supported_extensions()`.
5. Run the tests and confirm nothing else broke.

Do NOT vendor or hardcode any console keys. Report which files changed and any matches
you had to extend.
```
