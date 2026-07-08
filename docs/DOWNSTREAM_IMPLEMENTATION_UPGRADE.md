# Downstream Upgrade — Game-Disc Support (0.8)

This guide is for projects that depend on `opticaldiscs` and want to pick up the
video-game optical-disc support added in **0.8**: console/game identification plus
browsing for Nintendo GameCube & Wii, Sega Dreamcast (GD-ROM), Philips CD-i, and 3DO.

If you maintain a consuming project (a CLI, GUI, ripper, artwork tool, …), hand the
[**pickup prompt**](#pickup-prompt-paste-into-the-downstream-repo) at the bottom to an
agent in that repo, or follow the checklist yourself.

---

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
- Browsers: `browse::NodeFilesystem` (GameCube/Wii), `browse::CdiFilesystem`,
  `browse::OperaFilesystem`
- `opticaldiscs::gdi` — `.gdi` parsing (`parse_gdi`, `open_gdi_hd_reader`)
- `sector_reader::GdromSectorReader` + `GDROM_HD_START_LBA`
- `chd::ChdInfo::is_gdrom()` / `find_gdrom_hd_track()`

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
