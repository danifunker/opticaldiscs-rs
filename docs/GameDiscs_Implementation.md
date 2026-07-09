# Game Disc Support — Implementation Plan

> **Status: G1–G5 shipped.** Identification (PS1/PS2, Sega, PC-FX, PC Engine, CD32,
> Neo Geo), GameCube + Wii browsing (via `nod`), Dreamcast GD-ROM (CHD + `.gdi`),
> Philips CD-i, and 3DO Opera are all implemented, tested, and validated against real
> discs. Remaining: Phase G6 (Xbox XDVDFS, PS3 UDF 2.50, Jaguar). Design/format
> details below; tick the `- [x]` boxes as further work lands.

Read-only support for identifying and browsing console game discs, layered onto the
crate's existing container readers (ISO / BIN·CUE / CHD) and filesystem browsers
(ISO 9660 / UDF / HFS / …). The design principle: **reuse the ISO 9660 browser wherever
the console actually uses ISO 9660** (most CD consoles do), add a thin **identification
layer** for console/game metadata, and only write bespoke parsers for the genuinely
non-ISO formats (GameCube, Wii, 3DO, Xbox).

---

## 1. Key architectural insight — three layers, three kinds of work

Game-disc support lands at three distinct layers of the existing design:

| Layer | Trait / type (today) | What game discs add |
|---|---|---|
| **Container** (physical sectors) | `SectorReader` / `DiscFormat` | new containers: `.gdi`, `.cdi`, and Nintendo formats (raw `.gcm`, RVZ, WBFS, CISO) |
| **Filesystem** (files / dirs) | `Filesystem` / `FilesystemType` | new browsers: GameCube FST, Wii (crypto + FST), 3DO Opera, CD-i-aware ISO, XDVDFS |
| **Identification** (*what disc is this?*) | — *(does not exist yet)* | new: `GameDiscInfo { console, serial, title, region, maker }` |

The single most important finding: **most CD-based consoles use plain ISO 9660 and are
already browsable by the existing `Iso9660Filesystem`.** For PS1, PS2 (CD), Saturn,
Mega-CD (when ISO), CD32, Neo Geo CD, and PC-FX, the gap is *identification*, not
parsing. The genuinely new *parsing* work is concentrated in GameCube, Wii, Dreamcast
(container only — the filesystem is ISO 9660), 3DO, and Xbox.

---

## 2. Scope & tiers

Ordered by the user's stated priority: **"Wii/GameCube and lower"** first; PS3/PS4/Xbox
360 deferred.

### Tier A — browsable today; needs only the identification layer
Plain ISO 9660 discs the existing browser already reads. Add a signature probe that
extracts console + serial + title + region.

| Console | Container | Identifying signature | FS |
|---|---|---|---|
| Sony PlayStation (PS1) | BIN·CUE / CHD (Mode 2/2352) | `SYSTEM.CNF` → `BOOT = cdrom:\SLUS_005.55;1` + sector-4 license string | ISO 9660 ✓ |
| Sony PlayStation 2 | ISO / BIN·CUE / CHD; DVD = UDF-bridge | `SYSTEM.CNF` → `BOOT2 = cdrom0:\SLUS_216.29;1` | ISO 9660 ✓ (find via ISO even on DVD) |
| Sega Saturn | BIN·CUE / CHD | `"SEGA SEGASATURN "` @ LBA 0 of data track | ISO 9660 ✓ |
| Sega Mega-CD / Sega CD | BIN·CUE / CHD | `"SEGADISCSYSTEM  "` @ LBA 0 | often **not** ISO — see §7.4 |
| Amiga CD32 / CDTV | ISO / BIN·CUE / CHD | PVD System-Id `"CDTV"` @ 0x8008; `CD32.TM` vs `CDTV.TM` | ISO 9660 ✓ |
| SNK Neo Geo CD | BIN·CUE / CHD | `IPL.TXT` in ISO root | ISO 9660 ✓ |
| NEC PC-FX | BIN·CUE / CHD | `"PC-FX:Hu_CD-ROM "` @ LBA 0 | ISO 9660 ✓ |
| NEC PC Engine CD | BIN·CUE / CHD | `"PC Engine CD-ROM SYSTEM"` @ sector 1 + 0x20 | usually **no** ISO — low value |

### Tier B — new container, then the existing ISO 9660 browser
| Console | New work |
|---|---|
| Sega Dreamcast (GD-ROM) | new `.gdi`/`.cdi` container readers (+ CHD already works); IP.BIN parse; ISO 9660 in the **high-density area at LBA 45000** |

### Tier C — new filesystem / format parser
| Console | New work | Note |
|---|---|---|
| Nintendo GameCube | GCM header + FST browser | **via `nod`** (unencrypted) |
| Nintendo Wii | partition table + ticket/TMD + AES decrypt + FST | **via `nod`** (keys bundled) |
| Panasonic 3DO | Opera filesystem browser (not ISO 9660) | hand-rolled, big-endian |
| Philips CD-i | CD-i-aware ISO 9660 variant | big-endian records, `"CD-I "` magic |

### Deferred (out of the initial batch)
Microsoft Xbox / Xbox 360 (XDVDFS), Sony PS3 (UDF 2.50 + encryption), PS4, Atari Jaguar
CD (encrypted/custom — indefinite), Wii U.

---

## 3. The Nintendo key question — resolved: depend on `nod`

**Decision: GameCube and Wii are handled by the [`nod`](https://github.com/encounter/nod)
crate (dual MIT/Apache-2.0, GPL-3.0-compatible as a dependency).**

**Answer to "does the user need a key file?" → No.** The only secret required for Wii is
the 16-byte **common key** (plus Korean + vWii variants). The per-disc *title keys* and
*tickets* live on the disc itself; only the common key is external, and it is a single
fixed value. `nod` **compiles the common keys in** and selects the right one automatically
from the ticket's signature issuer + `common_key_idx` — there is **no `KeyBag`, no
`keys.bin`/`keys.txt` to load, and nothing for the downstream app to supply.** GameCube is
unencrypted and needs nothing regardless.

What `nod` gives us for free:
- Formats: raw ISO/GCM, **WBFS, CISO, GCZ, RVZ/WIA, TGC, NFS** (RVZ/WIA are otherwise very
  hard to implement).
- Wii partition decryption (AES-128-CBC), hash handling, and FST browsing.
- A filesystem API (`Fst`, `Node`, `PartitionBase::open_file`) that maps cleanly onto our
  `Filesystem` trait.

**One caveat to record:** `nod` transitively bundles Nintendo's copyrighted common key.
That is the same posture `nod` itself ships with on crates.io, so we inherit its risk
rather than creating new exposure — but note it in the README/CHANGELOG so downstreams are
aware. If this ever becomes a problem, the fallback (documented in git history of this
decision) is a caller-supplied `KeyBag` + hand-rolled AES; all offsets needed for that are
in §6 below, so the escape hatch stays cheap.

### `nod` 2.x API surface we'll wrap
```rust
use nod::{common::PartitionKind,
          read::{DiscOptions, DiscReader, PartitionOptions}};

let disc = DiscReader::new(path, &DiscOptions::default())?;
let mut part = disc.open_partition_kind(PartitionKind::Data, &PartitionOptions::default())?;
let meta = part.meta()?;              // raw ticket / TMD / boot header
let fst  = meta.fst()?;               // nod::disc::fst::Fst  (12-byte Node entries)
if let Some((_i, node)) = fst.find("/opening.bnr") { /* … */ }
let mut file = part.open_file(node)?; // impl Read + Seek
```
`DiscReader::new` also has `new_stream` / `new_from_cloneable_read` variants if we want to
feed it our own byte source instead of a path.

---

## 4. Public API changes

### 4.1 `DiscFormat` (container) — `src/formats.rs`
Add variants + extension mappings:

```rust
pub enum DiscFormat {
    Iso, BinCue, Chd, MdsMdf,   // existing
    Gdi,                        // Dreamcast GD-ROM track descriptor (.gdi)
    Cdi,                        // DiscJuggler (.cdi) — read-only, deferred within Tier B
    Nintendo,                   // .gcm/.rvz/.wbfs/.ciso/.gcz/.wia/.tgc/.nfs → delegated to nod
}
```
- `from_path`: `"gdi" → Gdi`, `"cdi" → Cdi`, `"gcm"|"rvz"|"wbfs"|"ciso"|"gcz"|"wia"|"tgc"|"nfs" → Nintendo`.
- Note: a raw GameCube/Wii dump is frequently just `.iso`. Detection must therefore also
  magic-sniff `.iso` files for the GC (`0xC2339F3D` @ 0x1C) / Wii (`0x5D1C9EA3` @ 0x18)
  magics and route them to the `Nintendo` path (see §5).

### 4.2 `FilesystemType` — `src/formats.rs`
```rust
pub enum FilesystemType {
    /* … existing … */
    GameCube,   // GCM/FST (nod)
    Wii,        // encrypted partition + FST (nod)
    Opera,      // 3DO
    Cdi,        // CD-i Green Book (ISO 9660 variant)
    Xdvdfs,     // Xbox (deferred)
}
```
Update `display_name` and `is_browsable`.

### 4.3 New identification layer — `src/gameid.rs`
This is the highest-value, lowest-cost addition and unlocks all of Tier A.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    Ps1, Ps2, SegaSaturn, SegaMegaCd, SegaDreamcast,
    AmigaCd32, NeoGeoCd, PcFx, PcEngineCd,
    GameCube, Wii, ThreeDo, CdI, Xbox, /* … */
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Region { NtscU, NtscJ, Pal, Asia, Korea, Brazil, World, Unknown }

#[derive(Debug, Clone)]
pub struct GameDiscInfo {
    pub console: Console,
    pub serial:  Option<String>,   // normalized: "SLUS-00555", "GALE01", …
    pub title:   Option<String>,
    pub region:  Option<Region>,
    pub maker:   Option<String>,
    pub version: Option<String>,
}
```

- `fn detect_game_disc(reader, fs, info) -> Option<GameDiscInfo>` runs a chain of signature
  probes. It is **additive** — hangs off `DiscImageInfo` as a new
  `pub game: Option<GameDiscInfo>` field, populated in `DiscImageInfo::build`, and never
  breaks existing filesystem detection.
- Probe order (cheap → specific): sector-0 hardware-ID strings (Sega/PC-FX) → block-0 3DO
  magic → ISO-9660 present? then read `SYSTEM.CNF` (PS1/PS2), System-Id `"CDTV"` (CD32),
  `IPL.TXT` (Neo Geo) → GC/Wii magics → Xbox magic.

Because this reads through the existing `SectorReader`/`Iso9660Filesystem`, it works across
**all** container formats (BIN·CUE, CHD, ISO) automatically.

### 4.4 `open_disc_filesystem` — `src/browse/mod.rs`
Route the new filesystem types: `GameCube | Wii → NodFilesystem`, `Opera → OperaFilesystem`,
`Cdi → CdiFilesystem`, Dreamcast → existing `Iso9660Filesystem` seeded at the HD-area
offset.

---

## 5. Container-layer details

### 5.1 GDI (Dreamcast) — `src/gdi.rs` + a `GdiSectorReader`
`.gdi` is a plain-text track descriptor, structurally analogous to `.cue` (reuse the
`bincue` pattern):
```
3                                   ← track count (line 1)
1 0     4 2352 track01.bin 0        ← [track#] [start LBA] [type 4=data/0=audio] [sector_size] [file] [offset]
2 600   0 2352 track02.raw 0
3 45000 4 2352 track03.bin 0        ← high-density area always starts at LBA 45000
```
- LBA → file byte: `offset + (L - track.start_lba) * sector_size`; for 2352-byte sectors add
  **16** to reach Mode-1 cooked user data.
- The GD data track (track 3) begins at **LBA 45000**; its ISO 9660 PVD is at LBA 45016.
  **Directory records store absolute LBAs in the 45000+ range**, so the browser must be told
  the partition base is 45000 and subtract it: `byte_in_track = (L - 45000) * 2048`.
  Forgetting the `-45000` base is the classic GD-ROM parsing bug.

### 5.2 CDI (DiscJuggler) — deferred within Tier B
Trailer-based proprietary format (descriptor block at EOF; the last 4 bytes are a LE size
used to seek backward to it). Variable-length records + per-version quirks (v2/v3/v3.5/v4).
Support read-only track/session enumeration later; **prefer GDI and CHD first** — both are
far simpler and cover the bulk of Dreamcast dumps.

### 5.3 CHD for GD-ROM — already works
`libchdman-rs` (MAME) reads GD-ROM CHDs, stored as the full **multi-track** layout with
GD-ROM-specific metadata (`CHGD` tag, adds a `PAD` field for 4-frame alignment). Track 3 at
frame 45000 carries the ISO 9660 filesystem. The existing `ChdSectorReader` +
`find_first_data_track` needs a Dreamcast-aware tweak to select the **HD-area** data track
(track 3), not track 1.

### 5.4 Nintendo containers — delegated to `nod`
`DiscFormat::Nintendo` does not get a hand-written `SectorReader`; instead
`open_disc_filesystem` constructs a `NodFilesystem` that owns a `nod::DiscReader` and
implements our `Filesystem` trait over `nod`'s `Fst`/`open_file`. This sidesteps
implementing WBFS/CISO/GCZ/RVZ/WIA ourselves.

---

## 6. Nintendo reference (for the `nod` wrapper — and the escape hatch)

All fields **big-endian**. Kept here so the `nod` wrapper can cross-check, and so a future
hand-rolled path is unblocked. Sources: YAGCD ch.13, WiiBrew *Wii disc* / *Ticket*, and the
`nod` source (verified against code, not just prose).

### 6.1 GameCube boot header (`boot.bin`, first 0x440 bytes)
| Offset | Size | Field |
|---|---|---|
| 0x000 | 1 | Console ID (`'G'`) |
| 0x001 | 2 | Game code (ASCII) |
| 0x003 | 1 | Region code |
| 0x004 | 2 | Maker code (ASCII) |
| 0x006 | 1 | Disc number |
| 0x007 | 1 | Version |
| **0x01C** | 4 | **Magic `0xC2339F3D`** |
| 0x020 | ≤0x3E0 | Game title (ASCII, null-padded) |
| 0x420 | 4 | DOL (main executable) offset |
| 0x424 | 4 | **FST offset** |
| 0x428 | 4 | **FST size** |
| 0x42C | 4 | FST max size |

### 6.2 GameCube FST
One blob: an array of **12-byte entries** followed by a string table.

| Bytes | Field | File | Directory |
|---|---|---|---|
| 0x0 | flags | 0 | 1 |
| 0x1..0x4 | name_offset (24-bit BE into string table) | — | — |
| 0x4 | offset | data offset | parent index |
| 0x8 | length | file size | one-past-last child index |

Root = entry 0 (`flags=1`, `length` = total entry count). String table starts at
`FST_offset + total_entries*12`. Walk with a stack of directory-end indices.
**Wii stores file `offset` as `value/4`** — decode with `<<2`; directory index fields are
never shifted.

### 6.3 Wii disc header + partition table
- Header: **Magic `0x5D1C9EA3` @ 0x18**; title (ASCII, 64 B) @ 0x020; hash-verify disable
  @ 0x060; encryption disable @ 0x061.
- Partition info @ **0x40000**: 4 groups × (count u32, table-offset u32). **Offset fields are
  stored `<<2`** (real byte offset = value << 2).
- Each partition entry (8 B): offset (`<<2`), type (**0 = Data, 1 = Update, 2 = Channel**).

### 6.4 Wii partition header + ticket + decryption
- Partition begins with the **ticket (0x2A4 B)**, then TMD offset/size, cert-chain, H3 table
  (`0x18000`), and **data offset (`<<2`)**.
- Ticket: encrypted title key @ **0x1BF** (16 B); title ID @ **0x1DC** (8 B); common-key
  index @ **0x1F1** (0 std / 1 Korean / 2 vWii).
- Title-key IV = `title_id(8) ‖ zeros(8)`; `title_key = AES-128-CBC-dec(common_key[idx], iv,
  enc_title_key)`.
- Cluster = 0x8000 B = 0x400 hash block + 0x7C00 data. Decrypt hash block with **IV=0**; the
  **data IV is bytes 0x3D0..0x3E0 of the still-encrypted hash block**; decrypt data with it.
  Logical offset `L` → `cluster = L/0x7C00`, `in = L%0x7C00`; read `0x400+in` of the
  decrypted cluster. The concatenated data regions form a GameCube-style header+FST.
- Retail common keys (from `nod`): std `ebe42a225e8593e448d9c5457381aaf7`, Korean
  `63b82bb4f4614e2e13f2fefbba4c9b7e`, vWii `30bfc76e7c19afbb23163330ced7c28d`.

---

## 7. Sega reference

Sources: mc.pp.se/dc, dreamcast.wiki, SaturnSDK IPMaker `SystemID.h`, retrodev.com, rcheevos
`hash_disc.c`. Signature strings are **space-padded to 16 bytes**; numeric fields BE
(SH/68000). Match signatures leniently (14–15 chars) to tolerate padding variants.

### 7.1 Dreamcast IP.BIN (first 256 B of the data track; full IP = 0x8000)
| Off | Len | Field |
|---|---|---|
| 0x000 | 16 | HW ID `"SEGA SEGAKATANA "` |
| 0x010 | 16 | Maker ID `"SEGA ENTERPRISES"` |
| 0x020 | 16 | Device info (`… GD-ROM1/1`) |
| 0x030 | 8 | Area / region symbols (`J`/`U`/`E`) |
| 0x038 | 8 | Peripherals |
| 0x040 | 10 | Product number |
| 0x04A | 6 | Version |
| 0x050 | 16 | Release date (`YYYYMMDD`) |
| 0x060 | 16 | Boot filename (`1ST_READ.BIN`) |
| 0x070 | 16 | Software maker |
| 0x080 | 128 | Game title |

### 7.2 Saturn IP header (256 B @ LBA 0 of the data track)
| Off | Len | Field |
|---|---|---|
| 0x000 | 16 | HW ID `"SEGA SEGASATURN "` |
| 0x010 | 16 | Maker ID |
| 0x020 | 10 | Product number |
| 0x02A | 6 | Version |
| 0x030 | 8 | Release date |
| 0x038 | 8 | Device info |
| 0x040 | 10 | Area symbols (`JTUBKAEL`) |
| 0x050 | 16 | Peripherals |
| 0x060 | 112 | Game title |
| 0x0E0 | 4 | IP size (BE u32) |
FS = standard ISO 9660.

### 7.3 Mega-CD / Sega CD (sector 0)
- ID @ 0x000 (16 B): `"SEGADISCSYSTEM  "` / `"SEGABOOTDISC    "`.
- Genesis-style ROM header @ **0x100**: system name (0x100/16), copyright+date (0x110/16),
  domestic title (0x120/48), overseas title (0x150/48), serial (0x180/14), region (0x1F0/3).
- **Not ISO 9660 at the front** — the identifying data is this raw header. An ISO tree may
  or may not exist afterward; identification does not depend on one.

### 7.4 PC-FX / PC Engine CD
- PC-FX: `"PC-FX:Hu_CD-ROM "` @ 0x000; title @ 0x800 (32 B); **numeric fields little-endian**
  (V810). FS = ISO 9660.
- PC Engine CD: `"PC Engine CD-ROM SYSTEM"` @ **sector 1 + 0x20**; title @ sector 1 + 0x6A
  (~22 B). Usually no ISO filesystem — low value, detect-only.

---

## 8. Other-console reference

### 8.1 PlayStation 1 / 2 — `src/gameid.rs` probes (little-endian)
- Read `/SYSTEM.CNF` via the ISO 9660 browser (ASCII `KEY = value`, CRLF).
  **`BOOT` ⇒ PS1, `BOOT2` ⇒ PS2** (authoritative discriminator per PCSX2). Neither ⇒ unknown.
- Serial from the boot EXE name `XXXX_NNN.NN` → normalize to `XXXX-NNNNN` (strip `.`, `_`→`-`,
  uppercase). Prefixes: `SLUS/SCUS`=NA, `SLES/SCES`=EU, `SLPS/SLPM/SCPS`=JP, `SLKA/SCKA`=Korea.
- PS1 region string is at **sector 4 (byte 0x2000)**: `"…Licensed by…Sony Computer
  Entertainment (America|Europe|Inc.)"`.
- PS2 DVDs are ISO 9660 + UDF-bridge; `SYSTEM.CNF` is reachable through the **ISO** volume, so
  no UDF reader is needed for identification.

### 8.2 Amiga CD32/CDTV & Neo Geo CD (ISO 9660 — detection only)
- CD32/CDTV: PVD System Identifier `"CDTV"` @ **0x8008**; `CD32.TM` (2048 B) vs `CDTV.TM`
  (22152 B) distinguishes the machine; `TM` app-use tag = bootable.
- Neo Geo CD: `IPL.TXT` in ISO root (ASCII `FILENAME.EXT,Bank,Offset` lines, ≤32 entries,
  ends with 0x1A).

### 8.3 Philips CD-i — `src/browse/cdi.rs` (big-endian, Green Book)
Needs a **CD-i-aware ISO 9660 variant**, not the stock reader:
- Standard Identifier @ **0x8001** is `"CD-I "` (not `"CD001"`). System Identifier @ 0x8008 is
  `"CD-RTOS"`.
- Directory records keep **only the big-endian** extent/size copies (LE half zeroed): extent
  `start_lbn` BE @ 0x06, `size` BE @ 0x0E, 6-byte High-Sierra date @ 0x12, flags @ 0x19.
- A structured 10-byte **System-Use area** follows the name: group/owner/attributes (BE),
  `file_no` (matches the Mode-2 subheader). Attribute bit 0x8000 = Directory.
- **Path table is unused — walk the tree from root.** Files may be Form 2 (2324 B).
- Reference implementation: Aaru `Aaru.Filesystems/ISO9660` (CDi structs).

### 8.4 3DO Opera — `src/browse/opera.rs` (big-endian, not ISO 9660)
Volume header @ block 0: record type `01` + **five `0x5A` sync bytes** @ 0x01; block size
@ 0x4C (u32, usually 2048); root dir block location @ 0x64; copies @ 0x60.
- Directory header (start of a dir's data): next/prev block, flags, content-end offset, first
  entry offset (usually 20).
- Directory entry (72-byte fixed part + avatar array): flags @ 0, id @ 4, 4-char type @ 8
  (`*dir`), block_size @ 12, byte_count @ 16, block_count @ 20, filename @ 32 (≤32 B),
  copies `n` @ 64, then `(n+1)` × 4-byte block pointers @ 68. Follow avatar[0] × block_size to
  the data. Flag bit 0x80000000 = last entry in dir.
- Reference: barbeque/3dodump `OperaFS-Format.md`, 3dodev.com.

### 8.5 Xbox / Xbox 360 XDVDFS — **implemented** in 0.9.0, `src/browse/xdvdfs.rs` (little-endian)
20-byte magic `MICROSOFT*XBOX*MEDIA` at **base + 0x10000** (and again at +0x7EC). Candidate
game-partition bases to probe in order: `0`, `0x0FD90000` (XGD2), `0x02080000` (XGD3),
`0x18300000` (XGD1 full dump). Volume descriptor: root dir sector (u32) @ 0x14, root size @
0x18. Directory entries form a **binary tree**: left/right subtree offsets @ 0x00/0x02 (in
4-byte words), start sector @ 0x04, size @ 0x08, attributes @ 0x0C (Directory 0x10), name-len
@ 0x0D, name @ 0x0E. Child offset `0xFFFF` (or `0`) = leaf. Reference: extract-xiso,
xboxdevwiki.net/XDVDFS.

---

## 9. Dependencies

```toml
[dependencies]
nod = "2"          # GameCube/Wii (+ RVZ/WBFS/CISO/GCZ/WIA/TGC/NFS); MIT OR Apache-2.0

# already present: thiserror, log, cue_sheet, libchdman-rs
```
- `nod` is compatible with the crate's GPL-3.0 license (permissive dep in a GPL project).
- Consider gating `nod` behind a default-on `nintendo` feature so downstreams that only care
  about CD consoles can opt out of the extra build weight — decide during Phase G2.

---

## 10. Phasing

Each phase is independently shippable and ends with passing tests. Ordered for
earliest user-visible payoff.

### Phase G1 — Identification layer (Tier A) `[x]`
`src/gameid.rs` + `GameDiscInfo` on `DiscImageInfo`. Signature probes for PS1, PS2, Saturn,
Mega-CD, CD32, Neo Geo CD, PC-FX. **No new parsing** — rides the existing ISO 9660 browser and
sector reader, so it works on BIN·CUE / CHD / ISO immediately. Highest value / lowest cost.
- [x] `Console`, `Region`, `GameDiscInfo` types
- [x] sector-0 Sega/PC-FX header probes
- [x] `SYSTEM.CNF` (BOOT/BOOT2) PS1/PS2 probe + serial normalization + region tables
- [x] CD32 `"CDTV"` / Neo Geo `IPL.TXT` probes
- [x] wire into `DiscImageInfo::build`; unit tests with synthetic sector fixtures

### Phase G2 — GameCube + Wii via `nod` (Tier C) `[x]`
- [x] add `nod`; `DiscFormat::Nintendo` + extension/magic detection (incl. `.iso` GC/Wii sniff)
- [x] `NodFilesystem` implementing `Filesystem` over `nod`'s `Fst`/`open_file`
- [x] GC/Wii `GameDiscInfo` (game code, title, region, maker) from the boot header / nod meta
- [x] Wii: enumerate partitions; browse the Data partition by default
- [x] tests against small synthetic/redump-style fixtures (env-var gated for large ones)

### Phase G3 — Dreamcast (Tier B) `[x]`
- [x] `src/gdi.rs` + `GdiSectorReader` (`.gdi` parse, LBA→file mapping, 45000 base)
- [x] IP.BIN parse → `GameDiscInfo`
- [x] ISO 9660 browse of the HD area (seed the browser at the 45000 base)
- [x] Dreamcast-aware CHD track selection (HD data track, not track 1)
- [x] `.cdi` deferred — track/session enumeration only, later

### Phase G4 — CD-i (Tier C) `[x]`
- [x] `src/browse/cdi.rs` — CD-i-aware ISO variant (BE records, `"CD-I "`, tree walk, system-use)
- [x] detection + `GameDiscInfo`

### Phase G5 — 3DO Opera (Tier C) `[x]`
- [x] `src/opera.rs` (structs) + `src/browse/opera.rs` (browser)
- [x] volume/dir/entry parse, avatar-pointer file reads; detection + `GameDiscInfo`

### Phase G6 — deferred `[ ]`
Xbox XDVDFS `[x]` (0.9.0). PS3 `[x]` (0.9.0): disc ISOs browse via the plaintext ISO 9660
bridge and are identified from `PS3_DISC.SFB` + `PS3_GAME/PARAM.SFO` — decrypted ISOs also read
file data; encrypted redump ISOs browse/identify but their data regions stay ciphertext (no
dkey support). PS4 (encrypted `.pkg`); Jaguar CD.
Wii U (WUD/WUX, encrypted); Switch (NSP/XCI, encrypted); 3DS (CCI/CIA, encrypted) all require
title keys and are out of scope for the disc-image browser. NKit-processed GC/Wii images are
detected and refused (they need reconstruction `nod` does not perform).

---

## 11. Testing & fixtures

Mirror the EFS approach: **synthetic fixtures committed for CI; real game ISOs stay out of the
repo** and are exercised by env-var-gated tests (game dumps are large and copyrighted).
- Build minimal synthetic images per format (a hand-crafted GameCube header+FST; a 3DO volume
  with one dir; a Saturn/Dreamcast IP header; a `SYSTEM.CNF` in a tiny ISO) — no external tools.
- `nod` ships its own tests; our tests only cover the wrapper mapping (`nod::Node` → `FileEntry`).
- Keep every committed fixture ≤ 2 MB (matches the publish-tarball size gate).

---

## 12. Open questions / decisions to make during implementation
- Gate `nod` behind a `nintendo` feature (default-on) vs unconditional dep? (build weight)
- Surface Wii multi-partition structure in the `Filesystem` API (update/channel partitions) or
  only browse Data? (start with Data; revisit)
- Whether to expose raw IP.BIN / boot-header bytes on `GameDiscInfo` (like `pvd`/`hfs_mdb` on
  `DiscImageInfo`) for downstream consumers that want more than the parsed fields.

---

*Sources: YAGCD (hitmen.c02.at/files/yagcd), WiiBrew (wiibrew.org/wiki/Wii_disc, /Ticket),
`nod` (github.com/encounter/nod), dreamcast.wiki/GDI_format, mc.pp.se/dc, SaturnSDK IPMaker,
nocash psx-spx (problemkaputt.de/psxspx), PCSX2 source, Aaru (aaru-dps/Aaru), barbeque/3dodump,
extract-xiso (XboxDev), Redump.*
*Last updated: 2026-07-07.*
