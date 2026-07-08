//! PSP "CISO" (`.cso`) compressed-ISO container support.
//!
//! A CSO file is a UMD/ISO 9660 image split into fixed-size blocks, each stored
//! either raw or **raw-DEFLATE**-compressed, with an index table giving every
//! block's file offset. This is the classic **CISOv1** format used by PSP
//! loaders (a distinct format from the GameCube "CISO" handled via `nod`).
//!
//! Header (little-endian):
//!
//! ```text
//! 0x00  u32  magic  "CISO"
//! 0x04  u32  header size (informational; ignored)
//! 0x08  u64  total uncompressed size (bytes of the underlying ISO)
//! 0x10  u32  block size (2048 for PSP UMDs)
//! 0x14  u8   version (1)
//! 0x15  u8   index align shift
//! 0x16  u16  reserved
//! 0x18  ...  index: (num_blocks + 1) × u32
//! ```
//!
//! Each index entry's low 31 bits are `file_offset >> align`; bit 31 set means
//! the block is stored **uncompressed**. A block's compressed length is the gap
//! to the next entry's offset. [`CsoSectorReader`] decompresses blocks on demand
//! and presents a plain 2048-byte cooked view, so the standard ISO 9660 browser
//! reads a `.cso` with no PSP-specific knowledge.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::{OpticaldiscsError, Result};
use crate::sector_reader::{SectorReader, SECTOR_SIZE};

/// CISO magic at offset 0.
pub const CSO_MAGIC: &[u8; 4] = b"CISO";

/// Flag bit in an index entry marking an uncompressed (stored) block.
const CSO_NOT_COMPRESSED: u32 = 0x8000_0000;

/// A [`SectorReader`] over a PSP CISOv1 (`.cso`) image.
///
/// The underlying volume is ISO 9660; this reader only undoes the CSO block
/// compression, exposing the same 2048-byte cooked sectors the browser expects.
pub struct CsoSectorReader {
    file: BufReader<File>,
    /// Block size in bytes — must equal [`SECTOR_SIZE`] (2048) for PSP UMDs.
    block_size: u32,
    /// Index-offset left-shift (bytes per index unit = `1 << align`).
    align: u8,
    /// Index table: `num_blocks + 1` entries. Entry `i`'s block spans file bytes
    /// `off(i)..off(i+1)`, where `off = (entry & !CSO_NOT_COMPRESSED) << align`.
    index: Vec<u32>,
    /// Total uncompressed size of the underlying ISO, in bytes.
    total_size: u64,
    /// Number of blocks (`index.len() - 1`).
    num_blocks: u64,
}

impl CsoSectorReader {
    /// Open and parse the CSO header + index at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`OpticaldiscsError::Parse`] if the magic, version, or block size
    /// is not a supported PSP CISOv1 image, or [`OpticaldiscsError::Io`] on read
    /// failure.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut file = BufReader::new(File::open(path.as_ref()).map_err(OpticaldiscsError::Io)?);

        let mut head = [0u8; 24];
        file.read_exact(&mut head).map_err(OpticaldiscsError::Io)?;
        if &head[0..4] != CSO_MAGIC {
            return Err(OpticaldiscsError::Parse("not a CISO (.cso) image".into()));
        }
        let total_size = u64::from_le_bytes(head[8..16].try_into().unwrap());
        let block_size = u32::from_le_bytes(head[16..20].try_into().unwrap());
        let version = head[20];
        let align = head[21];

        // Only CISOv1 with 2048-byte blocks (raw-DEFLATE) is handled; v2/ZSO/DAX
        // use different block codecs.
        if version != 1 {
            return Err(OpticaldiscsError::Parse(format!(
                "unsupported CSO version {version} (only CISOv1 is supported)"
            )));
        }
        if block_size as u64 != SECTOR_SIZE {
            return Err(OpticaldiscsError::Parse(format!(
                "unsupported CSO block size {block_size} (expected {SECTOR_SIZE})"
            )));
        }
        if total_size == 0 || total_size % block_size as u64 != 0 {
            return Err(OpticaldiscsError::Parse(
                "invalid CSO total size (not a whole number of blocks)".into(),
            ));
        }

        let num_blocks = total_size / block_size as u64;
        // Index has num_blocks + 1 entries (the extra gives the last block's end).
        let index_entries = (num_blocks + 1) as usize;
        let mut raw = vec![0u8; index_entries * 4];
        file.seek(SeekFrom::Start(24))
            .map_err(OpticaldiscsError::Io)?;
        file.read_exact(&mut raw).map_err(OpticaldiscsError::Io)?;
        let index: Vec<u32> = raw
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();

        Ok(Self {
            file,
            block_size,
            align,
            index,
            total_size,
            num_blocks,
        })
    }

    /// Total uncompressed size of the underlying ISO, in bytes.
    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    /// Decompress block `i` into a `block_size`-byte buffer.
    fn read_block(&mut self, i: u64) -> Result<Vec<u8>> {
        let bs = self.block_size as usize;
        if i >= self.num_blocks {
            // Past the end of the volume — return zeros so over-reads are benign.
            return Ok(vec![0u8; bs]);
        }
        let entry = self.index[i as usize];
        let next = self.index[i as usize + 1];
        let plain = entry & CSO_NOT_COMPRESSED != 0;
        let off = ((entry & !CSO_NOT_COMPRESSED) as u64) << self.align;
        let next_off = ((next & !CSO_NOT_COMPRESSED) as u64) << self.align;
        let comp_len = next_off.saturating_sub(off) as usize;
        if comp_len == 0 {
            return Ok(vec![0u8; bs]);
        }

        self.file
            .seek(SeekFrom::Start(off))
            .map_err(OpticaldiscsError::Io)?;
        let mut buf = vec![0u8; comp_len];
        self.file
            .read_exact(&mut buf)
            .map_err(OpticaldiscsError::Io)?;

        if plain {
            // Stored block: the first block_size bytes are the sector (any extra
            // is alignment padding).
            buf.truncate(bs);
            if buf.len() < bs {
                buf.resize(bs, 0);
            }
            return Ok(buf);
        }

        // Raw DEFLATE (no zlib/gzip header) → exactly block_size bytes.
        let mut out = vec![0u8; bs];
        let mut dec = flate2::Decompress::new(false);
        dec.decompress(&buf, &mut out, flate2::FlushDecompress::Finish)
            .map_err(|e| OpticaldiscsError::Parse(format!("CSO block {i} inflate: {e}")))?;
        Ok(out)
    }
}

impl SectorReader for CsoSectorReader {
    fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
        // block_size == SECTOR_SIZE, so one block == one cooked sector.
        self.read_block(lba)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use std::io::Write;

    /// Build a minimal CISOv1 image from cooked sectors, compressing every
    /// other block (and storing the rest plain) to exercise both code paths.
    fn build_cso(sectors: &[[u8; 2048]]) -> Vec<u8> {
        let bs = 2048u32;
        let n = sectors.len() as u64;
        let total = n * bs as u64;

        let mut header = Vec::new();
        header.extend_from_slice(CSO_MAGIC);
        header.extend_from_slice(&0u32.to_le_bytes()); // header size
        header.extend_from_slice(&total.to_le_bytes()); // total size
        header.extend_from_slice(&bs.to_le_bytes()); // block size
        header.push(1); // version
        header.push(0); // align
        header.extend_from_slice(&0u16.to_le_bytes()); // reserved

        // Body starts right after the index.
        let index_bytes = ((n + 1) * 4) as u64;
        let body_start = 24 + index_bytes;

        let mut body = Vec::new();
        let mut index: Vec<u32> = Vec::new();
        for (i, s) in sectors.iter().enumerate() {
            let off = body_start + body.len() as u64;
            if i % 2 == 0 {
                // Compressed (raw DEFLATE).
                let mut enc = DeflateEncoder::new(Vec::new(), Compression::best());
                enc.write_all(s).unwrap();
                let comp = enc.finish().unwrap();
                index.push(off as u32); // compressed flag clear
                body.extend_from_slice(&comp);
            } else {
                // Stored plain.
                index.push(off as u32 | CSO_NOT_COMPRESSED);
                body.extend_from_slice(s);
            }
        }
        // Final sentinel entry: end of the last block.
        index.push((body_start + body.len() as u64) as u32);

        let mut out = header;
        for e in &index {
            out.extend_from_slice(&e.to_le_bytes());
        }
        out.extend_from_slice(&body);
        out
    }

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(".cso").tempfile().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn roundtrips_compressed_and_plain_blocks() {
        let mut s0 = [0u8; 2048];
        let mut s1 = [0u8; 2048];
        let mut s2 = [0u8; 2048];
        for (i, b) in s0.iter_mut().enumerate() {
            *b = (i % 251) as u8; // compressible pattern
        }
        s1.fill(0xAB); // stored plain
        s2[..5].copy_from_slice(b"CD001");

        let cso = build_cso(&[s0, s1, s2]);
        let f = write_tmp(&cso);
        let mut r = CsoSectorReader::open(f.path()).unwrap();
        assert_eq!(r.total_size(), 3 * 2048);
        assert_eq!(r.read_sector(0).unwrap(), s0);
        assert_eq!(r.read_sector(1).unwrap(), s1);
        assert_eq!(&r.read_sector(2).unwrap()[..5], b"CD001");
        // Over-read past the end is benign (zeros).
        assert_eq!(r.read_sector(99).unwrap(), vec![0u8; 2048]);
    }

    #[test]
    fn rejects_non_cso() {
        let f = write_tmp(b"not a cso file at all, padding padding padding padding");
        assert!(CsoSectorReader::open(f.path()).is_err());
    }
}
