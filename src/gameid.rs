//! Video-game console disc identification.
//!
//! Most CD-based consoles use a plain ISO 9660 filesystem that the existing
//! [`crate::browse::iso9660`] browser already reads. What they lack is an
//! *identity*: which console pressed the disc, the game's serial, title, and
//! region. This module fills that gap with a set of cheap signature probes that
//! run through the same [`SectorReader`] used for filesystem detection, so they
//! work uniformly across ISO, BIN/CUE, and CHD containers.
//!
//! The probes are ordered cheap → specific:
//! 1. Sector-0 hardware-ID strings (Sega Saturn / Dreamcast / Mega-CD, PC-FX).
//! 2. 3DO Opera volume-header magic (block 0).
//! 3. ISO 9660 content: `SYSTEM.CNF` (PlayStation 1/2), the PVD system
//!    identifier (`CDTV` → Amiga CD32/CDTV), and `IPL.TXT` (Neo Geo CD).
//!
//! Reference: `docs/GameDiscs_Implementation.md`.

use crate::iso9660::PrimaryVolumeDescriptor;
use crate::sector_reader::{SectorReader, SECTOR_SIZE};

/// A video-game console that pressed an optical disc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    /// Sony PlayStation (PS1 / PSX).
    Ps1,
    /// Sony PlayStation 2.
    Ps2,
    /// Sega Saturn.
    SegaSaturn,
    /// Sega Mega-CD / Sega CD.
    SegaMegaCd,
    /// Sega Dreamcast (GD-ROM).
    SegaDreamcast,
    /// Commodore Amiga CD32 / CDTV.
    AmigaCd32,
    /// SNK Neo Geo CD.
    NeoGeoCd,
    /// NEC PC-FX.
    PcFx,
    /// NEC PC Engine CD / TurboGrafx-CD.
    PcEngineCd,
    /// Nintendo GameCube.
    GameCube,
    /// Nintendo Wii.
    Wii,
    /// Panasonic / 3DO (Opera).
    ThreeDo,
    /// Philips CD-i.
    CdI,
    /// Sony PlayStation Portable (UMD).
    Psp,
    /// Sony PlayStation 3 (Blu-ray).
    Ps3,
    /// Microsoft Xbox (XDVDFS).
    Xbox,
}

impl Console {
    /// Human-readable console name.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Ps1 => "Sony PlayStation",
            Self::Ps2 => "Sony PlayStation 2",
            Self::SegaSaturn => "Sega Saturn",
            Self::SegaMegaCd => "Sega Mega-CD / Sega CD",
            Self::SegaDreamcast => "Sega Dreamcast",
            Self::AmigaCd32 => "Amiga CD32 / CDTV",
            Self::NeoGeoCd => "SNK Neo Geo CD",
            Self::PcFx => "NEC PC-FX",
            Self::PcEngineCd => "NEC PC Engine CD",
            Self::GameCube => "Nintendo GameCube",
            Self::Wii => "Nintendo Wii",
            Self::ThreeDo => "3DO",
            Self::CdI => "Philips CD-i",
            Self::Psp => "Sony PlayStation Portable",
            Self::Ps3 => "Sony PlayStation 3",
            Self::Xbox => "Microsoft Xbox",
        }
    }
}

/// Disc region, derived from serial prefixes or on-disc region symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// North America (NTSC-U/C).
    NtscU,
    /// Japan (NTSC-J).
    NtscJ,
    /// Europe / PAL.
    Pal,
    /// Asia (non-Japan NTSC).
    Asia,
    /// Korea.
    Korea,
    /// Multi-region / world.
    World,
    /// Could not be determined.
    Unknown,
}

impl Region {
    /// Human-readable region name.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::NtscU => "North America (NTSC-U)",
            Self::NtscJ => "Japan (NTSC-J)",
            Self::Pal => "Europe (PAL)",
            Self::Asia => "Asia",
            Self::Korea => "Korea",
            Self::World => "World",
            Self::Unknown => "Unknown",
        }
    }
}

/// Identity of a game disc: which console, and the game's metadata.
///
/// Fields other than `console` are best-effort — a value of `None` means the
/// disc did not carry that datum in a place this module knows how to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameDiscInfo {
    /// The console that pressed the disc.
    pub console: Console,
    /// Normalized game serial (e.g. `"SLUS-00555"`, `"T-1234N"`), if found.
    pub serial: Option<String>,
    /// Game title, if the disc carries one in a header this module reads.
    pub title: Option<String>,
    /// Disc region, if derivable.
    pub region: Option<Region>,
    /// Maker / publisher string, if present.
    pub maker: Option<String>,
    /// Version / revision string (e.g. `"V1.000"`), if present.
    pub version: Option<String>,
}

impl GameDiscInfo {
    fn just(console: Console) -> Self {
        Self {
            console,
            serial: None,
            title: None,
            region: None,
            maker: None,
            version: None,
        }
    }
}

// ── Public probe entry point ──────────────────────────────────────────────────

/// Probe `reader` for a recognizable game console disc.
///
/// `pvd` is the already-parsed ISO 9660 Primary Volume Descriptor when the disc
/// has one (used for the ISO-content probes); pass `None` for non-ISO discs.
/// Returns `None` when no console signature matches.
pub fn detect_game_disc(
    reader: &mut dyn SectorReader,
    pvd: Option<&PrimaryVolumeDescriptor>,
) -> Option<GameDiscInfo> {
    detect_game_disc_opts(reader, pvd, true)
}

/// Highest ISO 9660 LBA whose *content* is considered cheap to read when
/// `deep_reads` is disabled. On a sequential-access container (a gzip stream),
/// reading a file's data costs time proportional to its offset; boot files that
/// sit near the end of a multi-gigabyte image would force a full decompression,
/// so those probes fall back to metadata that lives near the start.
const CHEAP_ISO_SECTOR_LIMIT: u32 = 1 << 14; // 16384 sectors ≈ 32 MiB

/// Like [`detect_game_disc`], but when `deep_reads` is `false` the probe avoids
/// reading file *contents* that live deep in the image (see
/// [`CHEAP_ISO_SECTOR_LIMIT`]), relying on near-the-start metadata instead. Set
/// `deep_reads = false` for gzip-compressed images.
pub fn detect_game_disc_opts(
    reader: &mut dyn SectorReader,
    pvd: Option<&PrimaryVolumeDescriptor>,
    deep_reads: bool,
) -> Option<GameDiscInfo> {
    // 1. Sector-0 hardware-ID headers (Sega, PC-FX). These sit at the very
    //    start of the data track and do not depend on a filesystem.
    if let Ok(sector0) = reader.read_sector(0) {
        if let Some(info) = probe_sector0_header(&sector0) {
            return Some(info);
        }
        // 3DO Opera volume header: record type 0x01 followed by five 0x5A sync
        // bytes at block 0.
        if sector0.len() >= 6 && sector0[0] == 0x01 && sector0[1..6] == [0x5A; 5] {
            return Some(probe_3do(&sector0));
        }
    }

    // 2. PC Engine CD carries its signature in sector 1, not sector 0.
    if let Ok(sector1) = reader.read_sector(1) {
        if sector1.len() >= 0x37 + 23 && &sector1[0x20..0x20 + 23] == b"PC Engine CD-ROM SYSTEM" {
            // The title (offset 0x6A, 22 bytes) is Shift-JIS on Japanese discs.
            let title = sjis_field(&sector1, 0x6A, 22);
            return Some(GameDiscInfo {
                title,
                ..GameDiscInfo::just(Console::PcEngineCd)
            });
        }
    }

    // 3. Philips CD-i (Green Book): "CD-I " identifier at sector 16.
    if crate::browse::cdi::detect_cdi(reader) {
        return Some(GameDiscInfo {
            title: crate::browse::cdi::read_volume_id(reader),
            ..GameDiscInfo::just(Console::CdI)
        });
    }

    // 3b. Microsoft Xbox: XDVDFS volume + `default.xbe` certificate.
    if let Some(base) = crate::browse::xdvdfs::detect(reader) {
        return Some(probe_xbox(reader, base));
    }

    // 4. ISO 9660 content probes (need a PVD). PlayStation 2 DVDs are a
    //    UDF + ISO 9660 bridge, so the caller may hand us `None` (UDF was
    //    detected first) even though an ISO PVD is present — parse it directly
    //    in that case so SYSTEM.CNF is still reachable.
    let parsed_pvd;
    let pvd = match pvd {
        Some(p) => Some(p),
        None => {
            parsed_pvd = PrimaryVolumeDescriptor::read_from(reader).ok();
            parsed_pvd.as_ref()
        }
    };
    if let Some(pvd) = pvd {
        // Amiga CD32 / CDTV: PVD system identifier begins with "CDTV".
        if pvd.system_id.trim_start().starts_with("CDTV") {
            return Some(GameDiscInfo {
                title: non_empty(pvd.volume_id.trim()),
                ..GameDiscInfo::just(Console::AmigaCd32)
            });
        }

        // Sony PS3: PS3_DISC.SFB in the root, with title/serial in
        // PS3_GAME/PARAM.SFO. The filesystem metadata is plaintext even on
        // encrypted (redump) dumps, so this works without the disc key.
        if find_root_entry(reader, pvd, "PS3_DISC.SFB").is_some() {
            let sfo = read_subdir_file(reader, pvd, "PS3_GAME", "PARAM.SFO");
            let (title, title_id) = sfo
                .as_deref()
                .map(parse_sfo_title_and_id)
                .unwrap_or((None, None));
            let region = title_id.as_deref().and_then(ps3_region_from_serial);
            return Some(GameDiscInfo {
                console: Console::Ps3,
                serial: title_id,
                title: title.or_else(|| non_empty(pvd.volume_id.trim())),
                region,
                maker: None,
                version: None,
            });
        }

        // Sony PSP (UMD): UMD_DATA.BIN in the root directory. Its first
        // `|`-delimited field is the disc ID (e.g. `ULUS-10241`). It lives near
        // the start, so it is always cheap to read.
        if let Some(umd) = read_root_file(reader, pvd, "UMD_DATA.BIN") {
            let text = String::from_utf8_lossy(&umd);
            let serial = text.split('|').next().map(str::trim).and_then(non_empty);
            return Some(GameDiscInfo {
                serial,
                title: non_empty(pvd.volume_id.trim()),
                ..GameDiscInfo::just(Console::Psp)
            });
        }

        // PlayStation 1 / 2: SYSTEM.CNF (BOOT vs BOOT2) is the authoritative
        // PS1/PS2 discriminator. Its data often sits near the *end* of a PS2
        // DVD, so only read it when deep reads are allowed or it is close to the
        // start; otherwise fall back to cheap near-the-start metadata below.
        if let Some((cnf_lba, _)) = find_root_entry(reader, pvd, "SYSTEM.CNF") {
            if deep_reads || cnf_lba < CHEAP_ISO_SECTOR_LIMIT {
                if let Some(cnf) = read_root_file(reader, pvd, "SYSTEM.CNF") {
                    if let Some(info) = probe_playstation(&cnf, pvd) {
                        return Some(info);
                    }
                }
            } else if let Some(info) = probe_playstation_cheap(reader, pvd) {
                // SYSTEM.CNF present but deep: identify from the PVD + root dir.
                return Some(info);
            }
        }

        // Neo Geo CD: IPL.TXT in the root directory.
        if read_root_file(reader, pvd, "IPL.TXT").is_some() {
            return Some(GameDiscInfo {
                title: non_empty(pvd.volume_id.trim()),
                ..GameDiscInfo::just(Console::NeoGeoCd)
            });
        }
    }

    None
}

// ── Sector-0 header probes (Sega, PC-FX) ─────────────────────────────────────

/// Match the 16-byte hardware-ID strings that Sega and NEC place at the start of
/// the data track, and parse the surrounding metadata header.
fn probe_sector0_header(s: &[u8]) -> Option<GameDiscInfo> {
    if s.len() < 0x100 {
        return None;
    }

    // Sega Saturn — "SEGA SEGASATURN " at 0x00.
    if s.starts_with(b"SEGA SEGASATURN") {
        return Some(GameDiscInfo {
            maker: ascii_field(s, 0x10, 16),
            serial: ascii_field(s, 0x20, 10),
            version: ascii_field(s, 0x2A, 6),
            region: sega_area_region(&s[0x40..0x4A]),
            title: ascii_field(s, 0x60, 112),
            console: Console::SegaSaturn,
        });
    }

    // Sega Dreamcast — "SEGA SEGAKATANA " at 0x00. (Usually seen only once the
    // GD high-density track is selected; harmless to match here too.)
    if s.starts_with(b"SEGA SEGAKATANA") {
        return Some(GameDiscInfo {
            maker: ascii_field(s, 0x10, 16),
            region: sega_area_region(&s[0x30..0x38]),
            serial: ascii_field(s, 0x40, 10),
            version: ascii_field(s, 0x4A, 6),
            title: ascii_field(s, 0x80, 128),
            console: Console::SegaDreamcast,
        });
    }

    // Sega Mega-CD / Sega CD — one of several boot identifiers at 0x00.
    if s.starts_with(b"SEGADISCSYSTEM")
        || s.starts_with(b"SEGABOOTDISC")
        || s.starts_with(b"SEGADATADISC")
    {
        // Genesis-style ROM header at 0x100 carries title/serial/region.
        let region = s.get(0x1F0..0x1F3).and_then(sega_area_region);
        return Some(GameDiscInfo {
            title: ascii_field(s, 0x150, 48).or_else(|| ascii_field(s, 0x120, 48)),
            serial: ascii_field(s, 0x180, 14),
            region,
            ..GameDiscInfo::just(Console::SegaMegaCd)
        });
    }

    // NEC PC-FX — "PC-FX:Hu_CD-ROM " at 0x00.
    if s.starts_with(b"PC-FX:Hu_CD-ROM") {
        return Some(GameDiscInfo {
            maker: ascii_field(s, 0x10, 0xE0).map(|c| first_line(&c)),
            ..GameDiscInfo::just(Console::PcFx)
        });
    }

    None
}

/// Build a 3DO `GameDiscInfo` from an Opera volume header (block 0). The volume
/// label lives at offset 0x28 (40 decimal), 32 bytes, big-endian-neutral ASCII.
fn probe_3do(s: &[u8]) -> GameDiscInfo {
    GameDiscInfo {
        title: ascii_field(s, 0x28, 32),
        ..GameDiscInfo::just(Console::ThreeDo)
    }
}

/// Identify an Xbox disc from its XDVDFS volume at `base`.
///
/// Reads `default.xbe`'s header + certificate (title name, title ID, region).
/// Always returns at least `Console::Xbox`; metadata is best-effort.
fn probe_xbox(reader: &mut dyn SectorReader, base: u64) -> GameDiscInfo {
    let cert = read_xbe_certificate(reader, base);
    match cert {
        Some(c) => GameDiscInfo {
            console: Console::Xbox,
            title: c.title,
            serial: c.serial,
            region: c.region,
            maker: None,
            version: None,
        },
        None => GameDiscInfo::just(Console::Xbox),
    }
}

/// Parsed fields of interest from an Xbox `default.xbe` certificate.
struct XbeCertificate {
    title: Option<String>,
    serial: Option<String>,
    region: Option<Region>,
}

/// Read and parse the `default.xbe` certificate from the XDVDFS root at `base`.
///
/// The XBE header (magic `XBEH`) gives the certificate's virtual address, which
/// is converted to a file offset via the image base. The certificate holds the
/// title name (40 UTF-16LE chars @ 0x0C), the title ID (@ 0x08), and the region
/// flags (@ 0xE0). Returns `None` if the file or header is missing/invalid.
fn read_xbe_certificate(reader: &mut dyn SectorReader, base: u64) -> Option<XbeCertificate> {
    let (sector, _size) = crate::browse::xdvdfs::root_file_extent(reader, base, "default.xbe")?;
    let xbe_start = base + sector as u64 * SECTOR_SIZE;

    let head = reader.read_bytes(xbe_start, 0x120).ok()?;
    if head.len() < 0x11C || &head[0..4] != b"XBEH" {
        return None;
    }
    let base_addr = u32::from_le_bytes(head[0x104..0x108].try_into().ok()?);
    let cert_addr = u32::from_le_bytes(head[0x118..0x11C].try_into().ok()?);
    let cert_off = cert_addr.checked_sub(base_addr)? as u64;

    let cert = reader.read_bytes(xbe_start + cert_off, 0xE4).ok()?;
    if cert.len() < 0xE4 {
        return None;
    }

    let title_id = u32::from_le_bytes(cert[0x08..0x0C].try_into().ok()?);
    let title = decode_utf16le(&cert[0x0C..0x0C + 80]);
    // Certificate layout: title name (0x0C, 80 bytes) → alt title IDs (0x5C,
    // 64 bytes) → allowed media (0x9C) → game region (0xA0).
    let region_flags = u32::from_le_bytes(cert[0xA0..0xA4].try_into().ok()?);

    Some(XbeCertificate {
        title: non_empty(title.trim()),
        serial: xbe_title_id_serial(title_id),
        region: xbe_region(region_flags),
    })
}

/// Format an Xbox title ID as a serial: two ASCII publisher letters, a dash, and
/// the zero-padded title number (e.g. `0x4D53_0003` → `"MS-003"`). Falls back to
/// hex when the publisher bytes are not printable ASCII.
fn xbe_title_id_serial(title_id: u32) -> Option<String> {
    if title_id == 0 {
        return None;
    }
    let p1 = (title_id >> 24) as u8;
    let p2 = (title_id >> 16) as u8;
    let num = title_id & 0xFFFF;
    if p1.is_ascii_uppercase() && p2.is_ascii_uppercase() {
        Some(format!("{}{}-{:03}", p1 as char, p2 as char, num))
    } else {
        Some(format!("{title_id:08X}"))
    }
}

/// Map Xbox certificate region flags to a coarse [`Region`].
/// bit0 = North America, bit1 = Japan, bit2 = Rest of World (PAL); more than one
/// set → `World`.
fn xbe_region(flags: u32) -> Option<Region> {
    let na = flags & 0x1 != 0;
    let jp = flags & 0x2 != 0;
    let row = flags & 0x4 != 0;
    match (na, jp, row) {
        (true, false, false) => Some(Region::NtscU),
        (false, true, false) => Some(Region::NtscJ),
        (false, false, true) => Some(Region::Pal),
        (false, false, false) => None,
        _ => Some(Region::World),
    }
}

/// Decode a UTF-16LE fixed field to a `String`, stopping at the first NUL.
fn decode_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Map a Sega area-symbol byte string (characters like `J`, `U`, `E`) to a
/// coarse [`Region`]. Multiple symbols → `World`.
fn sega_area_region(area: &[u8]) -> Option<Region> {
    let mut j = false;
    let mut u = false;
    let mut e = false;
    for &b in area {
        match b {
            b'J' => j = true,
            b'U' | b'T' | b'B' => u = true,
            b'E' | b'A' | b'L' => e = true,
            _ => {}
        }
    }
    match (j, u, e) {
        (true, false, false) => Some(Region::NtscJ),
        (false, true, false) => Some(Region::NtscU),
        (false, false, true) => Some(Region::Pal),
        (false, false, false) => None,
        _ => Some(Region::World),
    }
}

// ── PlayStation SYSTEM.CNF probe ─────────────────────────────────────────────

/// Parse a PlayStation `SYSTEM.CNF` file. `BOOT` ⇒ PS1, `BOOT2` ⇒ PS2 (the
/// authoritative discriminator). Returns `None` if neither key is present.
fn probe_playstation(cnf: &[u8], pvd: &PrimaryVolumeDescriptor) -> Option<GameDiscInfo> {
    let text = String::from_utf8_lossy(cnf);
    let mut boot2 = None;
    let mut boot = None;
    for line in text.lines() {
        let mut parts = line.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let val = parts.next().unwrap_or("").trim();
        if key.eq_ignore_ascii_case("BOOT2") {
            boot2 = Some(val.to_string());
        } else if key.eq_ignore_ascii_case("BOOT") {
            boot = Some(val.to_string());
        }
    }

    let (console, boot_val) = match (boot2, boot) {
        (Some(v), _) => (Console::Ps2, v),
        (None, Some(v)) => (Console::Ps1, v),
        (None, None) => return None,
    };

    let serial = normalize_ps_serial(&boot_val);
    let region = serial.as_deref().and_then(ps_region_from_serial);
    Some(GameDiscInfo {
        console,
        title: non_empty(pvd.volume_id.trim()),
        region,
        serial,
        maker: None,
        version: None,
    })
}

/// Identify a PlayStation disc without reading SYSTEM.CNF (whose data may sit
/// near the end of the image). Uses only near-the-start metadata:
///
/// - the PVD system/application identifier `PLAYSTATION` confirms the platform;
/// - the boot-executable filename in the root directory (e.g. `SLUS_201.52`)
///   yields the serial and region;
/// - the presence of a UDF bridge (DVD) distinguishes PS2 from a PS1 CD.
fn probe_playstation_cheap(
    reader: &mut dyn SectorReader,
    pvd: &PrimaryVolumeDescriptor,
) -> Option<GameDiscInfo> {
    let is_playstation =
        pvd.system_id.trim() == "PLAYSTATION" || pvd.application_id.trim() == "PLAYSTATION";
    if !is_playstation {
        return None;
    }

    // The boot ELF's filename encodes the serial (SLUS_201.52 → SLUS-20152).
    let serial = root_file_names(reader, pvd)
        .into_iter()
        .find_map(|name| normalize_ps_serial(&name));
    let region = serial.as_deref().and_then(ps_region_from_serial);

    // A UDF/ISO bridge means a DVD ⇒ PS2; a plain ISO 9660 CD ⇒ PS1. (The UDF
    // NSR descriptor sits at sectors 16–20, so this stays cheap.)
    let console = if crate::browse::udf::detect_udf(reader) {
        Console::Ps2
    } else {
        Console::Ps1
    };

    Some(GameDiscInfo {
        console,
        title: non_empty(pvd.volume_id.trim()),
        region,
        serial,
        maker: None,
        version: None,
    })
}

/// Collect the file (non-directory) identifiers in the ISO 9660 root directory,
/// with the `;version` suffix stripped. Reads only the root extent (near the
/// start of the image), so it is cheap even on a sequential-access container.
fn root_file_names(reader: &mut dyn SectorReader, pvd: &PrimaryVolumeDescriptor) -> Vec<String> {
    let flags_off = if pvd.high_sierra { 24 } else { 25 };
    let data = match reader.read_bytes(
        pvd.root_directory_lba as u64 * SECTOR_SIZE,
        pvd.root_directory_size as usize,
    ) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };

    let mut names = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let len = data[pos] as usize;
        if len == 0 {
            let next = (pos / SECTOR_SIZE as usize + 1) * SECTOR_SIZE as usize;
            if next <= pos || next >= data.len() {
                break;
            }
            pos = next;
            continue;
        }
        if len < 34 || pos + len > data.len() {
            break;
        }
        let rec = &data[pos..pos + len];
        let id_len = rec[32] as usize;
        let is_dir = rec.get(flags_off).map(|&f| f & 0x02 != 0).unwrap_or(false);
        if !is_dir && 33 + id_len <= rec.len() {
            let id = &rec[33..33 + id_len];
            let stem = id.split(|&b| b == b';').next().unwrap_or(id);
            names.push(String::from_utf8_lossy(stem).into_owned());
        }
        pos += len;
    }
    names
}

/// Normalize a PlayStation boot path (`cdrom:\SLUS_005.55;1`) to a canonical
/// serial (`SLUS-00555`): take the filename, drop the `;version`, remove `.`,
/// map `_` → `-`, uppercase.
fn normalize_ps_serial(boot: &str) -> Option<String> {
    // Filename = substring after the last '\', '/', or ':'.
    let file = boot.rsplit(['\\', '/', ':']).next().unwrap_or(boot).trim();
    let file = file.split(';').next().unwrap_or(file);
    let normalized: String = file
        .chars()
        .filter(|&c| c != '.')
        .map(|c| {
            if c == '_' {
                '-'
            } else {
                c.to_ascii_uppercase()
            }
        })
        .collect();
    // Sanity: expect something like XXXX-NNNNN.
    if normalized.len() >= 8 && normalized.contains('-') {
        Some(normalized)
    } else {
        None
    }
}

/// Derive a coarse region from a PlayStation serial prefix.
fn ps_region_from_serial(serial: &str) -> Option<Region> {
    let prefix: String = serial.chars().take(4).collect::<String>().to_uppercase();
    match prefix.as_str() {
        "SLUS" | "SCUS" | "LSP-" => Some(Region::NtscU),
        "SLES" | "SCES" | "SLED" | "SCED" => Some(Region::Pal),
        "SLPS" | "SLPM" | "SCPS" | "SIPS" => Some(Region::NtscJ),
        "SLKA" | "SCKA" => Some(Region::Korea),
        "SLAJ" | "SCAJ" => Some(Region::Asia),
        _ => None,
    }
}

/// Derive a coarse region from a PS3 title ID (e.g. `BCUS98147`), keyed on the
/// third/fourth letters of the prefix.
fn ps3_region_from_serial(serial: &str) -> Option<Region> {
    let up = serial.to_uppercase();
    match up.get(2..4)? {
        "US" => Some(Region::NtscU),
        "ES" => Some(Region::Pal),
        "JS" | "JM" => Some(Region::NtscJ),
        "AS" => Some(Region::Asia),
        "KS" => Some(Region::Korea),
        _ => None,
    }
}

// ── PlayStation SFO parsing ───────────────────────────────────────────────────

/// Parse a PS3/PSP/PS Vita `PARAM.SFO` and return `(TITLE, TITLE_ID)`.
///
/// SFO layout (little-endian): header at 0x00 (magic `\0PSF`, key-table start at
/// 0x08, data-table start at 0x0C, entry count at 0x10), then a 16-byte index
/// entry per key: key offset (u16) into the key table, data format (u16), data
/// length (u32), max length (u32), data offset (u32) into the data table. String
/// values (format `0x0204`) are UTF-8, NUL-terminated.
fn parse_sfo_title_and_id(data: &[u8]) -> (Option<String>, Option<String>) {
    let none = (None, None);
    if data.len() < 0x14 || &data[0..4] != b"\0PSF" {
        return none;
    }
    let key_start = u32::from_le_bytes(data[0x08..0x0C].try_into().unwrap()) as usize;
    let data_start = u32::from_le_bytes(data[0x0C..0x10].try_into().unwrap()) as usize;
    let count = u32::from_le_bytes(data[0x10..0x14].try_into().unwrap()) as usize;

    let (mut title, mut title_id) = (None, None);
    for i in 0..count.min(1024) {
        let e = 0x14 + i * 0x10;
        if e + 0x10 > data.len() {
            break;
        }
        let key_off = u16::from_le_bytes(data[e..e + 2].try_into().unwrap()) as usize;
        let data_len = u32::from_le_bytes(data[e + 4..e + 8].try_into().unwrap()) as usize;
        let data_off = u32::from_le_bytes(data[e + 0x0C..e + 0x10].try_into().unwrap()) as usize;

        let key = sfo_cstr(data, key_start + key_off);
        let val_start = data_start + data_off;
        let val = data
            .get(val_start..val_start + data_len)
            .map(|b| {
                let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                String::from_utf8_lossy(&b[..end]).trim().to_string()
            })
            .filter(|s| !s.is_empty());
        match key.as_str() {
            "TITLE" => title = val,
            "TITLE_ID" => title_id = val,
            _ => {}
        }
    }
    (title, title_id)
}

/// Read a NUL-terminated ASCII string from `data` at `off`.
fn sfo_cstr(data: &[u8], off: usize) -> String {
    let Some(slice) = data.get(off..) else {
        return String::new();
    };
    let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..end]).into_owned()
}

// ── ISO 9660 root-directory helpers ──────────────────────────────────────────

/// Read a named file from the ISO 9660 root directory, returning its bytes.
///
/// Walks the root directory extent (given by the PVD) looking for a directory
/// record whose identifier matches `name` (case-insensitive, ignoring the
/// `;version` suffix), then reads that file's extent. Returns `None` if the file
/// is absent or a read fails. Only the primary (non-Joliet) tree is searched,
/// which is where PlayStation and Neo Geo place their boot files.
fn read_root_file(
    reader: &mut dyn SectorReader,
    pvd: &PrimaryVolumeDescriptor,
    name: &str,
) -> Option<Vec<u8>> {
    let (lba, size) = find_root_entry(reader, pvd, name)?;
    // Guard against absurd sizes (corrupt records).
    if size == 0 || size > 8 * 1024 * 1024 {
        return None;
    }
    reader
        .read_bytes(lba as u64 * SECTOR_SIZE, size as usize)
        .ok()
}

/// Locate a **file** record for `name` in the root directory extent.
/// Returns `(extent_lba, data_length)` on a match.
fn find_root_entry(
    reader: &mut dyn SectorReader,
    pvd: &PrimaryVolumeDescriptor,
    name: &str,
) -> Option<(u32, u32)> {
    find_entry_in_extent(
        reader,
        pvd.root_directory_lba,
        pvd.root_directory_size,
        name,
        pvd.high_sierra,
        false,
    )
}

/// Read a file from a first-level subdirectory of the root (e.g.
/// `PS3_GAME/PARAM.SFO`). Returns `None` if either component is missing or a
/// read fails; sizes above 8 MiB are rejected as implausible.
fn read_subdir_file(
    reader: &mut dyn SectorReader,
    pvd: &PrimaryVolumeDescriptor,
    dir_name: &str,
    file_name: &str,
) -> Option<Vec<u8>> {
    let (dir_lba, dir_size) = find_entry_in_extent(
        reader,
        pvd.root_directory_lba,
        pvd.root_directory_size,
        dir_name,
        pvd.high_sierra,
        true,
    )?;
    let (lba, size) =
        find_entry_in_extent(reader, dir_lba, dir_size, file_name, pvd.high_sierra, false)?;
    if size == 0 || size > 8 * 1024 * 1024 {
        return None;
    }
    reader
        .read_bytes(lba as u64 * SECTOR_SIZE, size as usize)
        .ok()
}

/// Scan an ISO 9660 directory extent at `(lba, size)` for an entry named `name`
/// (case-insensitive, ignoring the `;version` suffix), matching a directory when
/// `want_dir` is set or a file otherwise. Returns the match's
/// `(extent_lba, data_length)`.
fn find_entry_in_extent(
    reader: &mut dyn SectorReader,
    lba: u32,
    size: u32,
    name: &str,
    high_sierra: bool,
    want_dir: bool,
) -> Option<(u32, u32)> {
    let flags_off = if high_sierra { 24 } else { 25 };
    let data = reader
        .read_bytes(lba as u64 * SECTOR_SIZE, size as usize)
        .ok()?;

    let mut pos = 0usize;
    while pos < data.len() {
        let len = data[pos] as usize;
        if len == 0 {
            // Records never span a sector boundary; padding runs to the next.
            let next = (pos / SECTOR_SIZE as usize + 1) * SECTOR_SIZE as usize;
            if next <= pos || next >= data.len() {
                break;
            }
            pos = next;
            continue;
        }
        if len < 34 || pos + len > data.len() {
            break;
        }
        let rec = &data[pos..pos + len];
        let id_len = rec[32] as usize;
        let is_dir = rec.get(flags_off).map(|&f| f & 0x02 != 0).unwrap_or(false);
        if is_dir == want_dir && 33 + id_len <= rec.len() {
            let id = &rec[33..33 + id_len];
            let stem = id.split(|&b| b == b';').next().unwrap_or(id);
            if stem.eq_ignore_ascii_case(name.as_bytes()) {
                let elba = u32::from_le_bytes(rec[2..6].try_into().ok()?);
                let dsize = u32::from_le_bytes(rec[10..14].try_into().ok()?);
                return Some((elba, dsize));
            }
        }
        pos += len;
    }
    None
}

// ── Small field helpers ──────────────────────────────────────────────────────

/// Extract a fixed-length, space-padded ASCII field, trimmed. Returns `None`
/// when the field is out of range or empty after trimming.
fn ascii_field(buf: &[u8], off: usize, len: usize) -> Option<String> {
    let slice = buf.get(off..off + len)?;
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    let s: String = slice[..end]
        .iter()
        .map(|&b| {
            if (0x20..0x7F).contains(&b) {
                b as char
            } else {
                ' '
            }
        })
        .collect();
    non_empty(s.trim())
}

/// Extract a Shift-JIS (code page 932) text field, terminated at the first NUL,
/// decoded to UTF-8 and trimmed.
///
/// Japanese console headers — such as the PC Engine CD-ROM² title — store their
/// title in Shift-JIS, which [`ascii_field`] would mangle by replacing every
/// non-ASCII byte with a space. Pure-ASCII fields decode identically, so this is
/// a safe superset. Returns `None` when the field is out of range, or empty
/// after trimming.
fn sjis_field(buf: &[u8], off: usize, len: usize) -> Option<String> {
    let slice = buf.get(off..off + len)?;
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    let (decoded, _, _) = encoding_rs::SHIFT_JIS.decode(&slice[..end]);
    non_empty(decoded.trim())
}

/// The first non-empty line of a string (for multi-line copyright fields).
fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// `Some(owned)` if `s` is non-empty, else `None`.
fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{OpticaldiscsError, Result};
    use std::io::{Cursor, Read, Seek, SeekFrom};

    struct CursorReader(Cursor<Vec<u8>>);
    impl SectorReader for CursorReader {
        fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
            self.0
                .seek(SeekFrom::Start(lba * SECTOR_SIZE))
                .map_err(OpticaldiscsError::Io)?;
            let mut buf = vec![0u8; SECTOR_SIZE as usize];
            self.0.read_exact(&mut buf).map_err(OpticaldiscsError::Io)?;
            Ok(buf)
        }
    }

    fn reader_with_sector0(data: &[u8]) -> CursorReader {
        let mut img = vec![0u8; 4 * SECTOR_SIZE as usize];
        img[..data.len()].copy_from_slice(data);
        CursorReader(Cursor::new(img))
    }

    #[test]
    fn detects_saturn() {
        let mut s = vec![0u8; SECTOR_SIZE as usize];
        s[..16].copy_from_slice(b"SEGA SEGASATURN ");
        s[0x20..0x2A].copy_from_slice(b"T-1234G   ");
        s[0x40..0x4A].copy_from_slice(b"J         ");
        s[0x60..0x66].copy_from_slice(b"MYGAME");
        let mut r = reader_with_sector0(&s);
        let info = detect_game_disc(&mut r, None).unwrap();
        assert_eq!(info.console, Console::SegaSaturn);
        assert_eq!(info.serial.as_deref(), Some("T-1234G"));
        assert_eq!(info.title.as_deref(), Some("MYGAME"));
        assert_eq!(info.region, Some(Region::NtscJ));
    }

    #[test]
    fn detects_dreamcast() {
        let mut s = vec![0u8; SECTOR_SIZE as usize];
        s[..16].copy_from_slice(b"SEGA SEGAKATANA ");
        s[0x40..0x4A].copy_from_slice(b"HDR-0000  ");
        s[0x80..0x86].copy_from_slice(b"SONICS");
        let mut r = reader_with_sector0(&s);
        let info = detect_game_disc(&mut r, None).unwrap();
        assert_eq!(info.console, Console::SegaDreamcast);
        assert_eq!(info.serial.as_deref(), Some("HDR-0000"));
        assert_eq!(info.title.as_deref(), Some("SONICS"));
    }

    #[test]
    fn detects_megacd() {
        let mut s = vec![0u8; SECTOR_SIZE as usize];
        s[..14].copy_from_slice(b"SEGADISCSYSTEM");
        s[0x180..0x18E].copy_from_slice(b"GM MK-4501-00 ");
        let mut r = reader_with_sector0(&s);
        let info = detect_game_disc(&mut r, None).unwrap();
        assert_eq!(info.console, Console::SegaMegaCd);
        assert_eq!(info.serial.as_deref(), Some("GM MK-4501-00"));
    }

    #[test]
    fn detects_pcfx() {
        let mut s = vec![0u8; SECTOR_SIZE as usize];
        s[..16].copy_from_slice(b"PC-FX:Hu_CD-ROM ");
        let mut r = reader_with_sector0(&s);
        let info = detect_game_disc(&mut r, None).unwrap();
        assert_eq!(info.console, Console::PcFx);
    }

    #[test]
    fn detects_3do() {
        let mut s = vec![0u8; SECTOR_SIZE as usize];
        s[0] = 0x01;
        s[1..6].copy_from_slice(&[0x5A; 5]);
        s[0x28..0x2E].copy_from_slice(b"3DOFS!");
        let mut r = reader_with_sector0(&s);
        let info = detect_game_disc(&mut r, None).unwrap();
        assert_eq!(info.console, Console::ThreeDo);
        assert_eq!(info.title.as_deref(), Some("3DOFS!"));
    }

    #[test]
    fn detects_pcengine_sector1() {
        let mut img = vec![0u8; 4 * SECTOR_SIZE as usize];
        let s1 = SECTOR_SIZE as usize;
        img[s1 + 0x20..s1 + 0x20 + 23].copy_from_slice(b"PC Engine CD-ROM SYSTEM");
        img[s1 + 0x6A..s1 + 0x6A + 6].copy_from_slice(b"HUGAME");
        let mut r = CursorReader(Cursor::new(img));
        let info = detect_game_disc(&mut r, None).unwrap();
        assert_eq!(info.console, Console::PcEngineCd);
        assert_eq!(info.title.as_deref(), Some("HUGAME"));
    }

    #[test]
    fn detects_pcengine_shift_jis_title() {
        // A Japanese PC Engine CD title stored in Shift-JIS at 0x6A: the bytes
        // for 夢幻戦士ヴァリスⅢ (Mugen Senshi Valis III), space-padded.
        let title_sjis: &[u8] = &[
            0x96, 0xb2, 0x8c, 0xb6, 0x90, 0xed, 0x8e, 0x6d, 0x83, 0x94, 0x83, 0x40, 0x83, 0x8a,
            0x83, 0x58, 0x87, 0x56, 0x20, 0x20, 0x20, 0x20,
        ];
        let mut img = vec![0u8; 4 * SECTOR_SIZE as usize];
        let s1 = SECTOR_SIZE as usize;
        img[s1 + 0x20..s1 + 0x20 + 23].copy_from_slice(b"PC Engine CD-ROM SYSTEM");
        img[s1 + 0x6A..s1 + 0x6A + title_sjis.len()].copy_from_slice(title_sjis);
        let mut r = CursorReader(Cursor::new(img));
        let info = detect_game_disc(&mut r, None).unwrap();
        assert_eq!(info.console, Console::PcEngineCd);
        assert_eq!(info.title.as_deref(), Some("夢幻戦士ヴァリスⅢ"));
    }

    #[test]
    fn sjis_field_decodes_and_falls_back_to_ascii() {
        // ASCII decodes unchanged; a Shift-JIS run decodes to Japanese; an
        // all-space / empty field yields None.
        assert_eq!(sjis_field(b"HELLO\0junk", 0, 10).as_deref(), Some("HELLO"));
        assert_eq!(
            sjis_field(&[0x83, 0x8a, 0x83, 0x58, 0x00], 0, 5).as_deref(),
            Some("リス")
        );
        assert_eq!(sjis_field(b"      ", 0, 6), None);
        assert_eq!(sjis_field(b"AB", 0, 8), None); // out of range
    }

    #[test]
    fn normalize_ps_serial_works() {
        assert_eq!(
            normalize_ps_serial("cdrom:\\SLUS_005.55;1").as_deref(),
            Some("SLUS-00555")
        );
        assert_eq!(
            normalize_ps_serial("cdrom0:\\SLUS_216.29;1").as_deref(),
            Some("SLUS-21629")
        );
    }

    #[test]
    fn ps_region_table() {
        assert_eq!(ps_region_from_serial("SLUS-00555"), Some(Region::NtscU));
        assert_eq!(ps_region_from_serial("SCES-12345"), Some(Region::Pal));
        assert_eq!(ps_region_from_serial("SLPS-01234"), Some(Region::NtscJ));
    }

    /// Build a synthetic ISO-ish image: a root directory at `root_lba`
    /// containing one file record `name` → `file_lba`, and the file's bytes at
    /// `file_lba`. Returns the image plus a hand-built PVD describing the root.
    fn build_iso_with_file(name: &str, contents: &[u8]) -> (Vec<u8>, PrimaryVolumeDescriptor) {
        let root_lba = 20u32;
        let file_lba = 22u32;
        let mut img = vec![0u8; 24 * SECTOR_SIZE as usize];

        // One directory record for `name` at the start of the root extent.
        let id = format!("{name};1");
        let mut rec = vec![0u8; 33 + id.len()];
        rec[2..6].copy_from_slice(&file_lba.to_le_bytes());
        rec[6..10].copy_from_slice(&file_lba.to_be_bytes());
        rec[10..14].copy_from_slice(&(contents.len() as u32).to_le_bytes());
        rec[14..18].copy_from_slice(&(contents.len() as u32).to_be_bytes());
        rec[25] = 0; // flags: a file
        rec[32] = id.len() as u8;
        rec[33..].copy_from_slice(id.as_bytes());
        if rec.len() % 2 == 1 {
            rec.push(0);
        }
        rec[0] = rec.len() as u8;
        let root_off = root_lba as usize * SECTOR_SIZE as usize;
        img[root_off..root_off + rec.len()].copy_from_slice(&rec);

        let file_off = file_lba as usize * SECTOR_SIZE as usize;
        img[file_off..file_off + contents.len()].copy_from_slice(contents);

        let pvd = PrimaryVolumeDescriptor {
            volume_id: "GAME".into(),
            system_id: "PLAYSTATION".into(),
            volume_set_id: String::new(),
            publisher_id: String::new(),
            application_id: String::new(),
            volume_space_size: 24,
            logical_block_size: 2048,
            root_directory_lba: root_lba,
            root_directory_size: SECTOR_SIZE as u32,
            creation_date: None,
            modification_date: None,
            expiration_date: None,
            effective_date: None,
            high_sierra: false,
        };
        (img, pvd)
    }

    #[test]
    fn detects_ps1_via_system_cnf() {
        let (img, pvd) = build_iso_with_file("SYSTEM.CNF", b"BOOT = cdrom:\\SLUS_005.55;1\r\n");
        let mut r = CursorReader(Cursor::new(img));
        let info = detect_game_disc(&mut r, Some(&pvd)).unwrap();
        assert_eq!(info.console, Console::Ps1);
        assert_eq!(info.serial.as_deref(), Some("SLUS-00555"));
        assert_eq!(info.region, Some(Region::NtscU));
    }

    #[test]
    fn detects_ps2_via_boot2() {
        let (img, pvd) = build_iso_with_file(
            "SYSTEM.CNF",
            b"BOOT2 = cdrom0:\\SLES_216.29;1\r\nVER = 1.00\r\n",
        );
        let mut r = CursorReader(Cursor::new(img));
        let info = detect_game_disc(&mut r, Some(&pvd)).unwrap();
        assert_eq!(info.console, Console::Ps2);
        assert_eq!(info.serial.as_deref(), Some("SLES-21629"));
        assert_eq!(info.region, Some(Region::Pal));
    }

    #[test]
    fn detects_ps2_dvd_without_pvd() {
        // PS2 DVDs are a UDF+ISO bridge; the caller passes pvd=None (UDF was
        // detected first). gameid must still parse the ISO PVD to find BOOT2.
        let (mut img, pvd) =
            build_iso_with_file("SYSTEM.CNF", b"BOOT2 = cdrom0:\\SLPM_651.40;1\r\n");
        // Write a real PVD at sector 16 so the fallback parse succeeds.
        let pvd_sector = crate::iso9660::build_test_pvd_sector(
            "PS2GAME",
            pvd.root_directory_lba,
            pvd.root_directory_size,
        );
        let off = 16 * SECTOR_SIZE as usize;
        img[off..off + pvd_sector.len()].copy_from_slice(&pvd_sector);
        let mut r = CursorReader(Cursor::new(img));

        let info = detect_game_disc(&mut r, None).unwrap();
        assert_eq!(info.console, Console::Ps2);
        assert_eq!(info.serial.as_deref(), Some("SLPM-65140"));
    }

    #[test]
    fn detects_neogeo_via_ipl() {
        let (img, pvd) = build_iso_with_file("IPL.TXT", b"PROG.PRG,0,0\r\n");
        let mut r = CursorReader(Cursor::new(img));
        let info = detect_game_disc(&mut r, Some(&pvd)).unwrap();
        assert_eq!(info.console, Console::NeoGeoCd);
    }

    #[test]
    fn detects_psp_via_umd_data() {
        let (img, pvd) = build_iso_with_file("UMD_DATA.BIN", b"ULUS-10241|0001|G|");
        let mut r = CursorReader(Cursor::new(img));
        let info = detect_game_disc(&mut r, Some(&pvd)).unwrap();
        assert_eq!(info.console, Console::Psp);
        assert_eq!(info.serial.as_deref(), Some("ULUS-10241"));
    }

    #[test]
    fn playstation_cheap_identifies_from_root_elf() {
        // No SYSTEM.CNF content read: identify from the PVD `PLAYSTATION` marker
        // and the boot-ELF filename in the root directory. No UDF ⇒ PS1.
        let (img, pvd) = build_iso_with_file("SLUS_201.52", b"\x7fELF");
        let mut r = CursorReader(Cursor::new(img));
        let info = probe_playstation_cheap(&mut r, &pvd).unwrap();
        assert_eq!(info.console, Console::Ps1);
        assert_eq!(info.serial.as_deref(), Some("SLUS-20152"));
        assert_eq!(info.region, Some(Region::NtscU));
    }

    #[test]
    fn xbe_title_id_and_region_helpers() {
        // "MS" publisher + title 3 → MS-003.
        assert_eq!(xbe_title_id_serial(0x4D53_0003).as_deref(), Some("MS-003"));
        assert_eq!(xbe_title_id_serial(0).as_deref(), None);
        // Non-printable publisher → hex fallback.
        assert_eq!(
            xbe_title_id_serial(0x0000_0001).as_deref(),
            Some("00000001")
        );
        assert_eq!(xbe_region(0x1), Some(Region::NtscU));
        assert_eq!(xbe_region(0x2), Some(Region::NtscJ));
        assert_eq!(xbe_region(0x4), Some(Region::Pal));
        assert_eq!(xbe_region(0x7), Some(Region::World));
        assert_eq!(xbe_region(0), None);
    }

    #[test]
    fn decode_utf16le_stops_at_nul() {
        // "Halo\0" in UTF-16LE plus trailing garbage.
        let mut b = Vec::new();
        for c in "Halo".encode_utf16() {
            b.extend_from_slice(&c.to_le_bytes());
        }
        b.extend_from_slice(&[0, 0, b'X', 0]);
        assert_eq!(decode_utf16le(&b), "Halo");
    }

    #[test]
    fn detects_ps3_via_sfb_and_parses_sfo() {
        // Root with PS3_DISC.SFB → identified as PS3; title falls back to the
        // volume id when PS3_GAME/PARAM.SFO is absent.
        let (img, pvd) = build_iso_with_file("PS3_DISC.SFB", b".SFB\x00\x01");
        let mut r = CursorReader(Cursor::new(img));
        let info = detect_game_disc(&mut r, Some(&pvd)).unwrap();
        assert_eq!(info.console, Console::Ps3);
        assert_eq!(info.title.as_deref(), Some("GAME")); // volume_id fallback
    }

    #[test]
    fn parses_param_sfo_title_and_id() {
        // Build a minimal SFO with TITLE and TITLE_ID string entries.
        let keys = b"TITLE\0TITLE_ID\0";
        let title = b"Test Game\0";
        let title_id = b"BLUS30782\0";
        let key_start = 0x14 + 2 * 0x10; // header + 2 index entries
        let data_start = key_start + keys.len();

        let mut sfo = Vec::new();
        sfo.extend_from_slice(b"\0PSF");
        sfo.extend_from_slice(&0x0101u32.to_le_bytes()); // version
        sfo.extend_from_slice(&(key_start as u32).to_le_bytes());
        sfo.extend_from_slice(&(data_start as u32).to_le_bytes());
        sfo.extend_from_slice(&2u32.to_le_bytes()); // entry count
                                                    // Entry 0: TITLE
        sfo.extend_from_slice(&0u16.to_le_bytes()); // key offset
        sfo.extend_from_slice(&0x0204u16.to_le_bytes()); // utf-8
        sfo.extend_from_slice(&(title.len() as u32).to_le_bytes());
        sfo.extend_from_slice(&(title.len() as u32).to_le_bytes());
        sfo.extend_from_slice(&0u32.to_le_bytes()); // data offset
                                                    // Entry 1: TITLE_ID
        sfo.extend_from_slice(&6u16.to_le_bytes()); // "TITLE_ID" starts at key+6
        sfo.extend_from_slice(&0x0204u16.to_le_bytes());
        sfo.extend_from_slice(&(title_id.len() as u32).to_le_bytes());
        sfo.extend_from_slice(&(title_id.len() as u32).to_le_bytes());
        sfo.extend_from_slice(&(title.len() as u32).to_le_bytes()); // data offset
        sfo.extend_from_slice(keys);
        sfo.extend_from_slice(title);
        sfo.extend_from_slice(title_id);

        let (t, id) = parse_sfo_title_and_id(&sfo);
        assert_eq!(t.as_deref(), Some("Test Game"));
        assert_eq!(id.as_deref(), Some("BLUS30782"));
        assert_eq!(ps3_region_from_serial("BLUS30782"), Some(Region::NtscU));
        assert_eq!(ps3_region_from_serial("BLES01234"), Some(Region::Pal));
        assert_eq!(ps3_region_from_serial("BLJM60001"), Some(Region::NtscJ));
    }

    #[test]
    fn no_match_returns_none() {
        let mut r = reader_with_sector0(&[0u8; 16]);
        assert!(detect_game_disc(&mut r, None).is_none());
    }
}
