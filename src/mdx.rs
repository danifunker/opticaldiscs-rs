//! DAEMON Tools MDX (`.mdx`) container support (feature = `mdx`).
//!
//! An MDX file is DAEMON Tools' "MDSv2" media descriptor with the disc data
//! embedded in the same file. Unlike the older Alcohol `.mds` (see
//! [`crate::mds`]), an MDX **always** stores its descriptor AES-256-encrypted
//! and zlib-compressed, and the track data is stored as **zlib-compressed sector
//! groups** — so reading it requires both a crypto step (just to enumerate
//! tracks) and an on-the-fly decompressing [`SectorReader`].
//!
//! ## File layout (`.mdx`)
//!
//! - **File header** (48 bytes): `MEDIA DESCRIPTOR` + `version_major`(=2) +
//!   `version_minor` + copyright + `encryption_header_offset` at 0x2C. For an
//!   `.mdx`, that field is `0xFFFFFFFF` (data-in-file); the 48-byte header is
//!   then followed by `footer_offset`(u64) + `footer_length`(u64).
//! - **Track data**: zlib-compressed sector groups near the start of the file.
//! - **Footer** (at `footer_offset`): the encrypted+compressed descriptor
//!   (`footer_length − 64` bytes), followed by a 512-byte **encryption header**
//!   whose leading 64 bytes are the (plaintext) PKCS#5 salt.
//!
//! ## Descriptor decryption
//!
//! The scheme is TrueCrypt-derived (reverse-engineered by the `mdsx` project and
//! ported by cdemu's `libmirage` `image-mdx`, from which this is ported):
//!
//! 1. A password is derived from the 64-byte salt by an "unshuffle" (a reflected
//!    CRC32 with polynomial `0xD8018001` seeds a per-word LCG).
//! 2. **PBKDF2 / RIPE-MD-160**, 2000 iterations, yields a 136-byte master key.
//! 3. **AES-256** (used as an ECB block primitive) in a custom **CBC with
//!    de-whitening** deciphers the 512-byte encryption header; its magic
//!    (`"TRUE"`) and a CRC32 over the key data are verified.
//! 4. The same AES + CBC-de-whitening (keyed from the header's `key_data`)
//!    deciphers the descriptor, which is then zlib-inflated.
//!
//! Encrypted **track data** (MDSv2 AES-XTS, `descriptor.encryption_header_offset
//! != 0`) is detected and rejected — this module handles only unencrypted
//! (but compressed) track data.
//!
//! Reference: cdemu `libmirage` `image-mdx` (`parser.c`, `crypto.c`,
//! `fragment.c`, `image-mdx.h`).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::{OpticaldiscsError, Result};
use crate::sector_reader::{SectorReader, SECTOR_SIZE};

mod crypto;

const FILE_HEADER_SIZE: usize = 48;
const SALT_SIZE: usize = 64;
const KEYDATA_SIZE: usize = 256;
const IV_SIZE: usize = 32;
const ENC_HEADER_SIZE: usize = 512;
const MAGIC: u32 = 0x5452_5545; // "TRUE" ('T','R','U','E'), little-endian
/// 18-byte prefix (signature + version) that descriptor offsets account for.
const DESC_PREFIX: usize = 18;

/// One resolved MDX track, carrying the geometry and compression metadata an
/// [`MdxSectorReader`] needs.
#[derive(Debug, Clone)]
pub struct MdxTrack {
    /// Track number (the `point` field, 1–99).
    pub track_no: u8,
    /// True for a data track (Mode 1 / Mode 2); false for audio.
    pub is_data: bool,
    /// Bytes of main-channel data stored per sector (e.g. 2048, 2352).
    pub main_size: u64,
    /// Bytes of appended subchannel data per sector (0, 16, or 96).
    pub subchannel_size: u64,
    /// Byte offset within the main portion to the 2048-byte user data.
    pub user_offset: u64,
    /// Byte offset in the file where this track's (compressed) data begins.
    pub data_offset: u64,
    /// Number of sectors in the track (from the footer's `track_data_length`).
    pub sector_count: u64,
    /// True if the track data is zlib-compressed in sector groups.
    pub compressed: bool,
    /// Sectors per compression group (valid when `compressed`).
    pub blocks_in_group: u64,
    /// Byte offset of the compression table, relative to `data_offset`.
    pub compression_table_offset: u64,
    /// Path to the `.mdx` file (data lives in the same file).
    pub data_path: PathBuf,
}

/// Parse an MDX image into its track list.
///
/// # Errors
///
/// Returns [`OpticaldiscsError::Parse`] if the file is not a v2 `.mdx`, the
/// descriptor fails to decrypt/verify, the track data is encrypted, or the
/// descriptor is malformed; [`OpticaldiscsError::Io`] on read failure.
pub fn parse_mdx(path: &Path) -> Result<Vec<MdxTrack>> {
    let data = std::fs::read(path).map_err(OpticaldiscsError::Io)?;
    if data.len() < FILE_HEADER_SIZE + 16 {
        return Err(parse("file too small for MDX"));
    }
    if &data[0..16] != b"MEDIA DESCRIPTOR" || data[0x10] != 2 {
        return Err(parse("not a v2 MEDIA DESCRIPTOR (.mdx) file"));
    }
    let enc_off = le32(&data, 0x2C)?;
    if enc_off != 0xFFFF_FFFF {
        return Err(parse(
            "MDSv2 external-.mdf form is not supported (only data-in-file .mdx)",
        ));
    }

    // .mdx: 48-byte header followed by footer_offset(u64) + footer_length(u64).
    let footer_offset = le64(&data, FILE_HEADER_SIZE)? as usize;
    let footer_length = le64(&data, FILE_HEADER_SIZE + 8)? as usize;
    if footer_length < SALT_SIZE || footer_offset + footer_length > data.len() {
        return Err(parse("implausible MDX footer offset/length"));
    }
    let enc_header_offset = footer_offset + footer_length - SALT_SIZE;
    let enc_header = data
        .get(enc_header_offset..enc_header_offset + ENC_HEADER_SIZE)
        .ok_or_else(|| parse("MDX encryption header out of range"))?;

    // Decrypt the encryption header, then the descriptor.
    let hdr = crypto::decipher_encryption_header(enc_header)?;
    if u32::from_le_bytes(hdr[SALT_SIZE + 4..SALT_SIZE + 8].try_into().unwrap()) != MAGIC {
        return Err(parse("MDX encryption header magic mismatch"));
    }
    let key_data = &hdr[SALT_SIZE + 16..SALT_SIZE + 16 + KEYDATA_SIZE];
    let kd_tail = SALT_SIZE + 16 + KEYDATA_SIZE;
    let stored_crc = u32::from_le_bytes(hdr[SALT_SIZE..SALT_SIZE + 4].try_into().unwrap());
    if crypto::crc32(key_data) != stored_crc {
        return Err(parse("MDX key-data checksum mismatch"));
    }
    let compressed_size =
        u32::from_le_bytes(hdr[kd_tail..kd_tail + 4].try_into().unwrap()) as usize;
    let decompressed_size =
        u32::from_le_bytes(hdr[kd_tail + 4..kd_tail + 8].try_into().unwrap()) as usize;

    let desc_raw = data
        .get(footer_offset..footer_offset + (footer_length - SALT_SIZE))
        .ok_or_else(|| parse("MDX descriptor out of range"))?;
    let descriptor = crypto::decipher_descriptor(desc_raw, key_data, compressed_size)?;
    if descriptor.len() != decompressed_size {
        return Err(parse("MDX descriptor decompressed to unexpected size"));
    }
    // Descriptor offsets account for an 18-byte (signature + version) prefix.
    let mut d = vec![0u8; DESC_PREFIX];
    d.extend_from_slice(&descriptor);

    parse_descriptor(&d, path)
}

/// Parse the decrypted+inflated descriptor (with its 18-byte prefix) into tracks.
fn parse_descriptor(d: &[u8], path: &Path) -> Result<Vec<MdxTrack>> {
    let num_sessions = le16(d, 0x14)? as usize;
    let sessions_off = le32(d, 0x50)? as usize;
    if le32(d, 0x58)? != 0 {
        return Err(parse(
            "encrypted MDX track data (MDSv2 AES-XTS) is not supported",
        ));
    }

    let mut tracks = Vec::new();
    for s in 0..num_sessions {
        let sb = sessions_off + s * 32;
        let num_all = *d.get(sb + 0x0A).ok_or_else(|| parse("truncated session"))? as usize;
        let tracks_off = le32(d, sb + 0x14)? as usize;

        for t in 0..num_all {
            let o = tracks_off + t * 80;
            if o + 80 > d.len() {
                return Err(parse("truncated track block"));
            }
            let mode_byte = d[o];
            let point = d[o + 4];
            if !(1..=99).contains(&point) {
                continue; // lead-in/out metadata
            }
            let sector_mode = mode_byte & 0x07;
            let has_sync = (mode_byte >> 5) & 1;
            let has_header = (mode_byte >> 3) & 1;
            let has_subheader = (mode_byte >> 6) & 1;
            let sector_size = le16(d, o + 0x10)? as u64;
            let start_offset = le64(d, o + 0x28)?;
            let footer_count = le32(d, o + 0x30)?;
            let footer_off = le32(d, o + 0x34)? as usize;

            if footer_count == 0 || footer_off == 0 {
                return Err(parse("MDX track has no footer block"));
            }
            // Only the first footer block matters (single-file .mdx never splits).
            let f = footer_off;
            if f + 32 > d.len() {
                return Err(parse("truncated footer block"));
            }
            let flags = d[f + 4];
            let blocks_in_group = le32(d, f + 0x0C)? as u64;
            let track_data_length = le64(d, f + 0x10)?;
            let compression_table_offset = le64(d, f + 0x18)?;

            let (is_data, base_main) = decode_sector_mode(sector_mode)?;
            let user_offset =
                u64::from(has_sync) * 12 + u64::from(has_header) * 4 + u64::from(has_subheader) * 8;
            // Main-channel size = declared sector size minus any subchannel (the
            // difference from the mode's base main size, in v2.0).
            let subchannel_size = sector_size.saturating_sub(base_main);
            let main_size = sector_size - subchannel_size;

            tracks.push(MdxTrack {
                track_no: point,
                is_data,
                main_size,
                subchannel_size,
                user_offset,
                data_offset: start_offset,
                sector_count: track_data_length,
                compressed: flags & 0x01 != 0,
                blocks_in_group,
                compression_table_offset,
                data_path: path.to_path_buf(),
            });
        }
    }

    if tracks.is_empty() {
        return Err(parse("no tracks in MDX descriptor"));
    }
    Ok(tracks)
}

/// Map an MDX `sector_mode` to `(is_data, base_main_size)` — the main-channel
/// size the mode implies when *all* extra-data bits are set (i.e. full raw).
fn decode_sector_mode(sector_mode: u8) -> Result<(bool, u64)> {
    // base_main here is the mode's *declared* main size with subchannel excluded;
    // we compute the subchannel by comparing against the declared sector size.
    // Using the "raw with everything" sizes lets `sector_size - base` isolate the
    // subchannel bytes for v2.0 images (which don't set the subchannel field).
    match sector_mode {
        1 => Ok((false, 2352)), // audio
        2 => Ok((true, 2048)),  // Mode 1 — base user size; +extras via bits
        3 => Ok((true, 2336)),  // Mode 2 formless
        4 => Ok((true, 2048)),  // Mode 2 Form 1
        5 => Ok((true, 2324)),  // Mode 2 Form 2
        other => Err(parse(&format!("unsupported MDX sector mode {other}"))),
    }
}

/// A compression-table entry resolved to an absolute-in-track read plan.
#[derive(Debug, Clone)]
enum GroupEntry {
    /// Stored uncompressed; `data_offset` is relative to the track data start.
    None { data_offset: u64 },
    /// Run-length: the whole group is a single repeated byte.
    Rle { value: u8 },
    /// Zlib (raw deflate) compressed; `compressed_size` bytes at `data_offset`.
    Zlib {
        data_offset: u64,
        compressed_size: usize,
    },
}

/// A [`SectorReader`] over an MDX track: decompresses one sector group at a time
/// and serves 2048-byte cooked sectors from it.
pub struct MdxSectorReader {
    file: File,
    full_size: u64, // main + subchannel
    user_offset: u64,
    data_offset: u64,
    sectors_in_group: u64,
    sector_count: u64,
    /// Per-group entries; empty when the track is uncompressed.
    table: Vec<GroupEntry>,
    /// Cached decompressed group and its index.
    cache: Vec<u8>,
    cached_group: Option<u64>,
}

impl MdxSectorReader {
    /// Open a reader over `track`, reading and inflating its compression table.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the compression table
    /// fails to inflate.
    pub fn open(track: &MdxTrack) -> Result<Self> {
        let mut file = File::open(&track.data_path).map_err(OpticaldiscsError::Io)?;
        let full_size = track.main_size + track.subchannel_size;

        let table = if track.compressed {
            if track.blocks_in_group == 0 {
                return Err(parse("MDX compression group size is zero"));
            }
            read_compression_table(&mut file, track, full_size)?
        } else {
            Vec::new()
        };

        Ok(Self {
            file,
            full_size,
            user_offset: track.user_offset,
            data_offset: track.data_offset,
            sectors_in_group: track.blocks_in_group.max(1),
            sector_count: track.sector_count,
            table,
            cache: Vec::new(),
            cached_group: None,
        })
    }

    /// Ensure the group holding `lba` is decompressed into `self.cache`.
    fn load_group(&mut self, group: u64) -> Result<()> {
        if self.cached_group == Some(group) {
            return Ok(());
        }
        let full = self.full_size as usize;

        if self.table.is_empty() {
            // Uncompressed: read the single sector straight from the file.
            let off = self.data_offset + group * self.full_size;
            self.cache = read_at(&mut self.file, off, full)?;
        } else {
            let entry = self
                .table
                .get(group as usize)
                .ok_or_else(|| parse("MDX sector group out of range"))?
                .clone();
            // Last group may hold fewer sectors than a full group.
            let mut num = self.sectors_in_group;
            if group + 1 == self.table.len() as u64 {
                let rem = self.sector_count % self.sectors_in_group;
                if rem != 0 {
                    num = rem;
                }
            }
            let out_len = (num * self.full_size) as usize;
            self.cache = match entry {
                GroupEntry::Rle { value } => vec![value; out_len],
                GroupEntry::None { data_offset } => {
                    read_at(&mut self.file, self.data_offset + data_offset, out_len)?
                }
                GroupEntry::Zlib {
                    data_offset,
                    compressed_size,
                } => {
                    let comp = read_at(
                        &mut self.file,
                        self.data_offset + data_offset,
                        compressed_size,
                    )?;
                    inflate_raw(&comp, out_len)?
                }
            };
        }
        self.cached_group = Some(group);
        Ok(())
    }
}

impl SectorReader for MdxSectorReader {
    fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
        let group = if self.table.is_empty() {
            lba
        } else {
            lba / self.sectors_in_group
        };
        self.load_group(group)?;

        let index_in_group = if self.table.is_empty() {
            0
        } else {
            lba % self.sectors_in_group
        };
        let base = (index_in_group * self.full_size + self.user_offset) as usize;
        let end = base + SECTOR_SIZE as usize;
        let slice = self
            .cache
            .get(base..end)
            .ok_or_else(|| parse("MDX sector data out of range in group"))?;
        Ok(slice.to_vec())
    }
}

/// Read and inflate the compression table into resolved per-group entries.
fn read_compression_table(
    file: &mut File,
    track: &MdxTrack,
    full_size: u64,
) -> Result<Vec<GroupEntry>> {
    let num_entries = track.sector_count.div_ceil(track.blocks_in_group) as usize;
    // The compressed table size is not stored; read generously and let zlib stop.
    let to_read = (num_entries + 0x800) * 2;
    let off = track.data_offset + track.compression_table_offset;
    let comp = read_at_upto(file, off, to_read)?;
    let raw = inflate_zlib(&comp, num_entries * 2)?;
    if raw.len() != num_entries * 2 {
        return Err(parse("MDX compression table size mismatch"));
    }

    let mut entries = Vec::with_capacity(num_entries);
    let mut cursor = 0u64;
    for chunk in raw.chunks_exact(2) {
        let value = u16::from_le_bytes([chunk[0], chunk[1]]);
        if value == 0 {
            entries.push(GroupEntry::None {
                data_offset: cursor,
            });
            cursor += track.blocks_in_group * full_size;
        } else if value & 0x8000 != 0 {
            entries.push(GroupEntry::Rle {
                value: (value & 0xFF) as u8,
            });
        } else {
            entries.push(GroupEntry::Zlib {
                data_offset: cursor,
                compressed_size: value as usize,
            });
            cursor += u64::from(value);
        }
    }
    Ok(entries)
}

// ── small IO / inflate helpers ────────────────────────────────────────────────

fn read_at(file: &mut File, offset: u64, len: usize) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))
        .map_err(OpticaldiscsError::Io)?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf).map_err(OpticaldiscsError::Io)?;
    Ok(buf)
}

/// Like [`read_at`] but tolerates a short read at EOF (returns what it got).
fn read_at_upto(file: &mut File, offset: u64, len: usize) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))
        .map_err(OpticaldiscsError::Io)?;
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(OpticaldiscsError::Io(e)),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Inflate a zlib stream (with header) expecting `expected` output bytes.
fn inflate_zlib(input: &[u8], expected: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expected);
    flate2::read::ZlibDecoder::new(input)
        .read_to_end(&mut out)
        .map_err(|e| parse(&format!("MDX zlib inflate failed: {e}")))?;
    Ok(out)
}

/// Inflate a raw DEFLATE stream (no zlib header), expecting `expected` bytes.
fn inflate_raw(input: &[u8], expected: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expected);
    flate2::read::DeflateDecoder::new(input)
        .read_to_end(&mut out)
        .map_err(|e| parse(&format!("MDX deflate inflate failed: {e}")))?;
    Ok(out)
}

fn parse(msg: &str) -> OpticaldiscsError {
    OpticaldiscsError::Parse(msg.to_string())
}

fn le16(d: &[u8], o: usize) -> Result<u16> {
    d.get(o..o + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| parse("truncated MDX descriptor"))
}
fn le32(d: &[u8], o: usize) -> Result<u32> {
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| parse("truncated MDX descriptor"))
}
fn le64(d: &[u8], o: usize) -> Result<u64> {
    d.get(o..o + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| parse("truncated MDX descriptor"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iso9660::build_test_pvd_sector;
    use std::io::Write;

    const SS: usize = 2048; // Mode 1 cooked sector
    const GROUP: usize = 16;
    const VOL_SECTORS: usize = 19; // PVD at 16, empty root at 18

    fn deflate_raw(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }
    fn deflate_zlib(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// Build a synthetic, correctly-encrypted `.mdx` wrapping a Mode-1 ISO 9660
    /// volume with `label`, using zlib-compressed 16-sector groups.
    fn build_synthetic_mdx(label: &str) -> Vec<u8> {
        // Volume: cooked 2048-byte sectors, PVD at 16, empty root at 18.
        let mut vol = vec![0u8; VOL_SECTORS * SS];
        let pvd = build_test_pvd_sector(label, 18, 2048);
        vol[16 * SS..16 * SS + 2048].copy_from_slice(&pvd);

        // Compress each 16-sector group with raw deflate.
        let mut data_region = Vec::new();
        let mut table_vals: Vec<u16> = Vec::new();
        let mut g = 0;
        while g * GROUP < VOL_SECTORS {
            let start = g * GROUP * SS;
            let end = ((g + 1) * GROUP * SS).min(vol.len());
            let comp = deflate_raw(&vol[start..end]);
            table_vals.push(comp.len() as u16);
            data_region.extend_from_slice(&comp);
            g += 1;
        }
        let ctab_rel = data_region.len() as u64; // relative to data_offset
        let mut table_bytes = Vec::new();
        for v in &table_vals {
            table_bytes.extend_from_slice(&v.to_le_bytes());
        }
        data_region.extend_from_slice(&deflate_zlib(&table_bytes));

        const DATA_OFFSET: u64 = 64; // track data starts right after the header ptrs

        // Descriptor with an 18-byte prefix, fields at absolute offsets.
        let mut da = vec![0u8; 0x100];
        da[0..16].copy_from_slice(b"MEDIA DESCRIPTOR");
        da[0x10] = 2; // version_major
        da[0x12..0x14].copy_from_slice(&0u16.to_le_bytes()); // medium_type = CD
        da[0x14..0x16].copy_from_slice(&1u16.to_le_bytes()); // num_sessions
        da[0x50..0x54].copy_from_slice(&0x60u32.to_le_bytes()); // sessions_blocks_offset
        da[0x58..0x5C].copy_from_slice(&0u32.to_le_bytes()); // track data enc off = none
                                                             // Session block @0x60.
        da[0x60 + 0x0A] = 1; // num_all_blocks
        da[0x60 + 0x14..0x60 + 0x18].copy_from_slice(&0x80u32.to_le_bytes()); // tracks_off
                                                                              // Track block @0x80.
        let tb = 0x80;
        da[tb] = 0x02; // sector_mode = 2 (Mode 1), no extra bits
        da[tb + 4] = 1; // point = track 1
        da[tb + 0x0C..tb + 0x10].copy_from_slice(&0u32.to_le_bytes()); // extra_offset = 0
        da[tb + 0x10..tb + 0x12].copy_from_slice(&(SS as u16).to_le_bytes()); // sector_size
        da[tb + 0x28..tb + 0x30].copy_from_slice(&DATA_OFFSET.to_le_bytes()); // start_offset
        da[tb + 0x30..tb + 0x34].copy_from_slice(&1u32.to_le_bytes()); // footer_count
        da[tb + 0x34..tb + 0x38].copy_from_slice(&0xE0u32.to_le_bytes()); // footer_offset
        da[tb + 0x40..tb + 0x48].copy_from_slice(&(VOL_SECTORS as u64).to_le_bytes()); // len64
                                                                                       // Footer block @0xE0.
        let fb = 0xE0;
        da[fb + 4] = 0x01; // flags: compression enabled
        da[fb + 0x0C..fb + 0x10].copy_from_slice(&(GROUP as u32).to_le_bytes()); // group size
        da[fb + 0x10..fb + 0x18].copy_from_slice(&(VOL_SECTORS as u64).to_le_bytes()); // data len
        da[fb + 0x18..fb + 0x20].copy_from_slice(&ctab_rel.to_le_bytes()); // ctab offset

        // Payload = descriptor without the 18-byte prefix; compress it.
        let payload = &da[DESC_PREFIX..];
        let compressed = deflate_zlib(payload);

        let salt = [0x5Au8; SALT_SIZE];
        let mut key_data = [0u8; KEYDATA_SIZE];
        for (i, b) in key_data.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(1);
        }
        let desc_enc = crypto::encrypt_test_descriptor(&compressed, &key_data);
        let enc_header = crypto::build_test_encryption_header(
            &salt,
            &key_data,
            compressed.len() as u32,
            payload.len() as u32,
        );

        let footer_offset = DATA_OFFSET as usize + data_region.len();
        let footer_length = desc_enc.len() + SALT_SIZE;

        // Assemble file: 48-byte header + footer ptrs + data + descriptor + enc hdr.
        let mut file = vec![0u8; DATA_OFFSET as usize];
        file[0..16].copy_from_slice(b"MEDIA DESCRIPTOR");
        file[0x10] = 2; // version_major
        file[0x2C..0x30].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // .mdx marker
        file[48..56].copy_from_slice(&(footer_offset as u64).to_le_bytes());
        file[56..64].copy_from_slice(&(footer_length as u64).to_le_bytes());
        file.extend_from_slice(&data_region);
        file.extend_from_slice(&desc_enc);
        file.extend_from_slice(&enc_header);
        file
    }

    fn write(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(".mdx").tempfile().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn rejects_non_mdx() {
        let f = write(&vec![0u8; 128]);
        assert!(parse_mdx(f.path()).is_err());
    }

    #[test]
    fn sector_mode_table() {
        assert_eq!(decode_sector_mode(1).unwrap(), (false, 2352));
        assert_eq!(decode_sector_mode(2).unwrap(), (true, 2048));
        assert!(decode_sector_mode(7).is_err());
    }

    #[test]
    fn parses_and_reads_synthetic_mdx() {
        let f = write(&build_synthetic_mdx("MDX_TEST"));
        let tracks = parse_mdx(f.path()).unwrap();
        assert_eq!(tracks.len(), 1);
        let t = &tracks[0];
        assert!(t.is_data);
        assert_eq!(t.main_size, 2048);
        assert_eq!(t.subchannel_size, 0);
        assert_eq!(t.sector_count, VOL_SECTORS as u64);
        assert!(t.compressed);

        let mut reader = MdxSectorReader::open(t).unwrap();
        // Sector 16 (first group) carries the ISO 9660 PVD.
        let s16 = reader.read_sector(16).unwrap();
        assert_eq!(&s16[1..6], b"CD001");
        // Sector 18 (second group — exercises the last, short group) is the
        // empty root directory: all zero.
        let s18 = reader.read_sector(18).unwrap();
        assert!(s18.iter().all(|&b| b == 0));
    }

    #[test]
    fn synthetic_mdx_browses_as_iso9660() {
        use crate::detect::DiscImageInfo;
        use crate::formats::{DiscFormat, FilesystemType};

        let f = write(&build_synthetic_mdx("MDX_BROWSE"));
        let info = DiscImageInfo::open(f.path()).unwrap();
        assert_eq!(info.format, DiscFormat::Mdx);
        assert_eq!(info.filesystem, FilesystemType::Iso9660);
        assert_eq!(info.volume_label.as_deref(), Some("MDX_BROWSE"));

        let mut fs = crate::browse::open_disc_filesystem(&info).unwrap();
        let root = fs.root().unwrap();
        assert!(fs.list_directory(&root).unwrap().is_empty());
    }
}
