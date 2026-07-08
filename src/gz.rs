//! gzip-compressed disc image (`.gz`) support.
//!
//! Some collections store a disc image (commonly a PS2 ISO for PCSX2) as a plain
//! **gzip** stream — the whole ISO 9660 volume compressed as one `.gz` file,
//! optionally alongside a `*.gz.pindex` PCSX2 seek index (which this reader does
//! not require).
//!
//! gzip has no native random access, so [`GzSectorReader`] decompresses the
//! stream forward, tracking the current uncompressed offset. A forward read just
//! inflates a little more; a *backward* seek restarts inflation from the start of
//! the file and skips forward. Directory extents and the volume descriptors of a
//! well-formed ISO live near the beginning, so browsing stays responsive; reading
//! the *contents* of a file deep in a multi-gigabyte image is proportionally
//! slower (it must inflate up to that point). Sequential reads — how the ISO
//! browser reads a file — never trigger a restart.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;

use crate::error::{OpticaldiscsError, Result};
use crate::sector_reader::{SectorReader, SECTOR_SIZE};

/// gzip magic (`1f 8b`) at offset 0.
pub const GZ_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// Size of the throwaway buffer used when skipping decompressed bytes forward.
const SKIP_CHUNK: usize = 64 * 1024;

/// A [`SectorReader`] over a gzip-compressed disc image.
///
/// Presents the decompressed stream as 2048-byte cooked sectors. Random access
/// is emulated on top of gzip's inherently sequential decompression (see the
/// module docs for the performance characteristics).
pub struct GzSectorReader {
    path: PathBuf,
    decoder: MultiGzDecoder<BufReader<File>>,
    /// Number of uncompressed bytes already consumed from `decoder`.
    pos: u64,
    /// Reusable skip buffer.
    scratch: Vec<u8>,
}

impl GzSectorReader {
    /// Open a `.gz` image, validating the gzip magic.
    ///
    /// # Errors
    ///
    /// Returns [`OpticaldiscsError::Parse`] if the file is not gzip, or
    /// [`OpticaldiscsError::Io`] if it cannot be opened.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut magic = [0u8; 2];
        {
            let mut f = File::open(&path).map_err(OpticaldiscsError::Io)?;
            f.read_exact(&mut magic).map_err(OpticaldiscsError::Io)?;
        }
        if magic != GZ_MAGIC {
            return Err(OpticaldiscsError::Parse("not a gzip (.gz) image".into()));
        }
        let decoder = Self::fresh_decoder(&path)?;
        Ok(Self {
            path,
            decoder,
            pos: 0,
            scratch: vec![0u8; SKIP_CHUNK],
        })
    }

    /// Open a fresh gzip decoder positioned at the start of the stream.
    fn fresh_decoder(path: &Path) -> Result<MultiGzDecoder<BufReader<File>>> {
        let file = File::open(path).map_err(OpticaldiscsError::Io)?;
        Ok(MultiGzDecoder::new(BufReader::new(file)))
    }

    /// Restart decompression from the beginning of the file.
    fn reset(&mut self) -> Result<()> {
        self.decoder = Self::fresh_decoder(&self.path)?;
        self.pos = 0;
        Ok(())
    }

    /// Advance the decompressor by `n` bytes, discarding them.
    fn skip(&mut self, mut n: u64) -> Result<()> {
        while n > 0 {
            let want = n.min(self.scratch.len() as u64) as usize;
            let got = read_fill(&mut self.decoder, &mut self.scratch[..want])
                .map_err(OpticaldiscsError::Io)?;
            if got == 0 {
                // Reached EOF before the target — treat the rest as consumed so
                // callers see zero-filled tails rather than an error.
                self.pos += n;
                return Ok(());
            }
            self.pos += got as u64;
            n -= got as u64;
        }
        Ok(())
    }

    /// Read `len` uncompressed bytes starting at absolute offset `offset`.
    ///
    /// Reads past end-of-stream are zero-filled so over-reads (e.g. probing the
    /// last sector) stay benign.
    fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        if offset < self.pos {
            self.reset()?;
        }
        if offset > self.pos {
            let skip = offset - self.pos;
            self.skip(skip)?;
        }
        let mut buf = vec![0u8; len];
        let got = read_fill(&mut self.decoder, &mut buf).map_err(OpticaldiscsError::Io)?;
        self.pos += got as u64;
        // Anything not filled (short read at EOF) stays zero.
        Ok(buf)
    }
}

impl SectorReader for GzSectorReader {
    fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
        self.read_at(lba * SECTOR_SIZE, SECTOR_SIZE as usize)
    }
}

/// Read repeatedly until `buf` is full or EOF; returns the number of bytes read.
/// (`Read::read` may return short reads mid-stream, especially from a decoder.)
fn read_fill(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn write_gz(payload: &[u8]) -> tempfile::NamedTempFile {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(payload).unwrap();
        let gz = enc.finish().unwrap();
        let mut f = tempfile::Builder::new().suffix(".gz").tempfile().unwrap();
        f.write_all(&gz).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn reads_sectors_forward_and_backward() {
        // Three distinct sectors.
        let mut payload = vec![0u8; 3 * 2048];
        payload[..2048].fill(0x11);
        payload[2048..4096].fill(0x22);
        payload[4096..6144].fill(0x33);
        payload[4096..4101].copy_from_slice(b"CD001");

        let f = write_gz(&payload);
        let mut r = GzSectorReader::open(f.path()).unwrap();

        // Forward.
        assert_eq!(r.read_sector(0).unwrap()[0], 0x11);
        assert_eq!(r.read_sector(1).unwrap()[0], 0x22);
        assert_eq!(&r.read_sector(2).unwrap()[..5], b"CD001");
        // Backward seek → restart under the hood, still correct.
        assert_eq!(r.read_sector(0).unwrap()[0], 0x11);
        // Over-read past EOF is zero-filled.
        assert_eq!(r.read_sector(50).unwrap(), vec![0u8; 2048]);
    }

    #[test]
    fn read_bytes_spanning_sectors() {
        let mut payload = vec![0u8; 2 * 2048];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }
        let f = write_gz(&payload);
        let mut r = GzSectorReader::open(f.path()).unwrap();
        // Cross the sector boundary at 2048.
        let out = r.read_bytes(2046, 4).unwrap();
        assert_eq!(out, [(2046 % 256) as u8, 255, 0, 1]);
    }

    #[test]
    fn rejects_non_gzip() {
        let mut f = tempfile::Builder::new().suffix(".gz").tempfile().unwrap();
        f.write_all(b"PK\x03\x04 not gzip").unwrap();
        f.flush().unwrap();
        assert!(GzSectorReader::open(f.path()).is_err());
    }
}
