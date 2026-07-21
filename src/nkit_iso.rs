//! Standalone **NKit ISO** (`*.nkit.iso`) reconstruction for GameCube discs.
//!
//! NKit (by Nanook) shrinks a GameCube/Wii disc image by stripping the
//! deterministic "junk" padding the console masters onto every disc, plus the
//! gaps between files, while preserving the disc header so the image still
//! *identifies* as a normal GC/Wii disc. The stripped image cannot be read as a
//! plain ISO — the file-system table (FST) offsets point at the *original* disc
//! layout, but the file data sits at compacted offsets with the junk removed.
//!
//! [`NkitIsoReader`] rebuilds the original disc image on the fly and presents it
//! as a plain [`Read`] + [`Seek`] stream, which [`nod`] then browses as an
//! ordinary disc via [`nod::Disc::new_stream`]. Reconstruction is a faithful port
//! of NKit's own recover-to-ISO path (`NkitReaderGc` in Nanook's reference
//! implementation): the junk is regenerated with the exact same lagged-Fibonacci
//! PRNG nod already ships as [`nod::LaggedFibonacci`].
//!
//! ## Format (NKit v1, standalone GameCube ISO)
//!
//! The 0x440-byte GameCube disc header is preserved verbatim, except the padding
//! region at `0x200` carries the NKit metadata:
//!
//! | Offset | Field |
//! |--------|-------|
//! | `0x200` | magic + version, ASCII `"NKIT v01"` |
//! | `0x208` | CRC32 of the reconstructed image (u32 BE) |
//! | `0x210` | reconstructed image size (u32 BE) |
//! | `0x214` | junk-seed ID override, 4 bytes (zero = use the game ID) |
//!
//! After the header the file stores, at their original disc offsets, the system
//! area (`bi2.bin`, apploader, `main.dol`) and the FST. The FST is followed by
//! each file's data (compacted, gaps removed) interleaved with 4-byte **gap
//! descriptors** that say how to refill each gap — all-junk, all-zero, or a mix
//! of junk / preserved-bytes / byte-fill runs. See `build_layout` for the
//! decode.
//!
//! Only the **v1** standalone ISO (`"NKIT v01"`) is reconstructed here — every
//! `.nkit.iso` observed in the wild is `NKIT v01`. "NKit v2" is *not* a
//! standalone-ISO format: it is NKit-lossless data embedded in a CISO/WBFS/RVZ
//! block map, which [`nod`] reads natively (its `NKitHeader` handles the v2
//! header there), so those files go straight to `nod` and never reach this
//! module. A standalone `"NKIT v02"` marker is rejected with a clear error.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use nod::LaggedFibonacci;

/// Size of the preserved GameCube disc header.
const GC_HEADER_SIZE: u64 = 0x440;
/// Byte offset of the NKit marker within the disc header.
const NKIT_OFFSET: u64 = 0x200;
/// Bytes NKit v1 overwrites at `0x200` and restores to zero on recovery
/// (7 little rewrites of `0x200`..`0x21C` in the reference reader).
const V1_HEADER_ZERO_LEN: u64 = 0x1C;
/// Reject an implausibly large reconstructed size early (a bit over a
/// dual-layer Wii disc); the real gate is `dst == image_size`.
const MAX_IMAGE_SIZE: u64 = 9 * 1024 * 1024 * 1024;
/// Gap run granularity used by the NKit gap encoder.
const GAP_BLOCK: u64 = 0x100;

// ── Region map ────────────────────────────────────────────────────────────────

/// One contiguous run of the reconstructed disc address space.
#[derive(Clone)]
struct Segment {
    /// Start offset within the reconstructed disc.
    dst: u64,
    /// Length in bytes.
    len: u64,
    kind: SegKind,
}

#[derive(Clone)]
enum SegKind {
    /// Copy verbatim from the `.nkit.iso` file starting at `src`.
    File { src: u64 },
    /// Regenerate junk with the lagged-Fibonacci PRNG seeded at this disc offset.
    Junk,
    /// Zero fill.
    Zero,
    /// Fill with a single repeated byte.
    Byte(u8),
    /// Serve from the small in-memory overlay (patched header / FST) at `off`.
    Mem { off: usize },
}

/// Immutable, shared reconstruction plan for one `.nkit.iso`.
struct Layout {
    /// Segments in ascending `dst`, contiguous, covering `[0, image_size)`.
    segments: Vec<Segment>,
    /// Patched bytes the reconstruction overlays on the file: the (possibly
    /// rewritten) `main.dol` offset at index 0, then the offset/length-rewritten
    /// FST from index 4.
    overlay: Vec<u8>,
    /// Reconstructed disc size in bytes.
    image_size: u64,
    /// Seed ID for junk generation (game ID, or the `0x214` override).
    junk_id: [u8; 4],
    /// Disc number (junk seed component).
    disc_num: u8,
}

// ── Public reader ─────────────────────────────────────────────────────────────

/// A cloneable [`Read`] + [`Seek`] view of the reconstructed disc image.
///
/// Cheap to clone: clones share the parsed `Layout` and the underlying file
/// handle (reads are positional, so clones do not share a cursor) and carry
/// their own seek position. This satisfies `nod`'s `DiscStream` bound
/// (`Read + Seek + Clone + Send + Sync`).
#[derive(Clone)]
pub struct NkitIsoReader {
    layout: Arc<Layout>,
    file: Arc<File>,
    pos: u64,
}

impl NkitIsoReader {
    /// Open `path` and build the reconstruction plan.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if the file is not a recognised standalone NKit ISO,
    /// if the header/FST are malformed, or if the reconstruction does not add up
    /// to the header's stated image size (a wrong-format or corrupt image). It
    /// never blocks: all reads are bounded by the file length and the segment
    /// walk is bounded by the file size.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let layout = build_layout(&file, file_len)?;
        Ok(Self {
            layout: Arc::new(layout),
            file: Arc::new(file),
            pos: 0,
        })
    }
}

/// Return the NKit standalone-ISO version (1 or 2) if `path` begins with an NKit
/// header (`"NKIT v0<n>"` at `0x200`), else `None`.
///
/// Used to route only genuine NKit images through reconstruction.
pub fn nkit_version(path: &Path) -> Option<u8> {
    let mut f = File::open(path).ok()?;
    f.seek(SeekFrom::Start(NKIT_OFFSET)).ok()?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).ok()?;
    parse_version(&magic)
}

/// `"NKIT v0<n>"` → `n`. Note the single space, which distinguishes the
/// standalone-ISO marker from nod's embedded `"NKIT  v"` (two spaces).
fn parse_version(magic: &[u8; 8]) -> Option<u8> {
    if &magic[..6] == b"NKIT v" && magic[6] == b'0' && magic[7].is_ascii_digit() {
        Some(magic[7] - b'0')
    } else {
        None
    }
}

// ── Header ────────────────────────────────────────────────────────────────────

/// Parsed NKit header fields needed to drive reconstruction.
struct Header {
    image_size: u64,
    junk_id: [u8; 4],
}

/// Parse the NKit marker + fixed fields at `0x200`.
///
/// Only the standalone **v1** ISO (`"NKIT v01"`) is a reconstructable format
/// here. "NKit v2" is *not* a standalone-ISO variant: it is NKit-lossless data
/// embedded in a CISO/WBFS/RVZ block map, which `nod` reads natively — so those
/// files never reach this reader. A standalone `"NKIT v02"` marker (not known to
/// exist in the wild) is therefore rejected with a clear, actionable error
/// rather than guessed at.
fn parse_header(hdr: &[u8], game_id: [u8; 4]) -> io::Result<Header> {
    let magic: [u8; 8] = hdr[0x200..0x208].try_into().unwrap();
    match parse_version(&magic) {
        Some(1) => {
            // Fixed layout: crc@0x208, image_size@0x210, junk-id override@0x214.
            let image_size = be32(hdr, 0x210) as u64;
            let junk_override: [u8; 4] = hdr[0x214..0x218].try_into().unwrap();
            let junk_id = if junk_override == [0, 0, 0, 0] {
                game_id
            } else {
                junk_override
            };
            Ok(Header { image_size, junk_id })
        }
        Some(v) => Err(bad(&format!(
            "standalone NKit v{v} is not supported (only v01); NKit v2 lossless is \
             embedded in CISO/WBFS/RVZ and read by nod directly"
        ))),
        None => Err(bad("not an NKit standalone ISO")),
    }
}

// ── Layout builder (port of NkitReaderGc.Read) ────────────────────────────────

/// A file entry parsed from the GameCube FST.
struct FstEntry {
    /// Offset of the file's data within the (compacted) NKit stream.
    data_offset: u64,
    /// File length in bytes.
    length: u64,
    /// Byte offset of this entry's data-offset field within the FST, so the
    /// reconstruction can rewrite it to the recovered disc offset.
    fst_field: usize,
}

/// Parse the header + FST and walk the file/gap stream into a [`Layout`].
fn build_layout(file: &File, file_len: u64) -> io::Result<Layout> {
    if file_len < GC_HEADER_SIZE {
        return Err(bad("file too small for a GameCube header"));
    }
    let mut hdr = vec![0u8; GC_HEADER_SIZE as usize];
    read_exact_at(file, &mut hdr, 0)?;

    // This reconstructor targets GameCube (magic at 0x1C). A Wii NKit ISO uses a
    // different layout — partitions, per-partition junk, encryption — that we do
    // not rebuild; reject it clearly rather than mis-reconstructing.
    if hdr[0x1C..0x20] != nod::GCN_MAGIC {
        if hdr[0x18..0x1C] == nod::WII_MAGIC {
            return Err(bad("Wii NKit ISO reconstruction is not supported"));
        }
        return Err(bad("not a GameCube disc header"));
    }

    let game_id: [u8; 4] = hdr[0..4].try_into().unwrap();
    let disc_num = hdr[6];
    let dol_off = be32(&hdr, 0x420) as u64;
    let fst_offset = be32(&hdr, 0x424) as u64;
    let fst_size = be32(&hdr, 0x428) as u64;

    let header = parse_header(&hdr, game_id)?;
    let image_size = header.image_size;
    if !(GC_HEADER_SIZE..=MAX_IMAGE_SIZE).contains(&image_size) {
        return Err(bad("implausible NKit image size"));
    }

    let fst_size_aligned = align4(fst_size);
    let fst_end = fst_offset
        .checked_add(fst_size_aligned)
        .ok_or_else(|| bad("FST size overflow"))?;
    // The system area + FST are stored verbatim at their original offsets.
    if fst_offset < GC_HEADER_SIZE || fst_end > file_len || fst_end > image_size {
        return Err(bad("FST location out of range"));
    }
    // The v1 marker occupies a fixed span at 0x200 that reconstruction restores
    // to zero; it always sits below the boot offsets at 0x420.
    let zero_end = NKIT_OFFSET + V1_HEADER_ZERO_LEN;

    // Read the FST verbatim; it becomes the base for the patched overlay copy.
    let mut fst = vec![0u8; fst_size_aligned as usize];
    read_exact_at(file, &mut fst, fst_offset)?;
    let files = parse_fst(&fst, fst_size)?;

    // Overlay: [dol offset (4)] then the FST (patched in place below).
    let mut overlay = Vec::with_capacity(4 + fst.len());
    overlay.extend_from_slice(&(dol_off as u32).to_be_bytes());
    overlay.extend_from_slice(&fst);

    // Header segments: verbatim disc header with the NKit marker zeroed and the
    // DOL offset served from the overlay (so it can be repatched if the DOL moves).
    let mut segs: Vec<Segment> = Vec::new();
    let mut dst = 0u64;
    push(&mut segs, &mut dst, 0x200, SegKind::File { src: 0 });
    push(&mut segs, &mut dst, V1_HEADER_ZERO_LEN, SegKind::Zero);
    push(
        &mut segs,
        &mut dst,
        0x420 - zero_end,
        SegKind::File { src: zero_end },
    );
    push(&mut segs, &mut dst, 4, SegKind::Mem { off: 0 });
    push(
        &mut segs,
        &mut dst,
        fst_offset - 0x424,
        SegKind::File { src: 0x424 },
    );
    push(&mut segs, &mut dst, fst_size_aligned, SegKind::Mem { off: 4 });
    debug_assert_eq!(dst, fst_end);

    // Walk the compacted file/gap stream, expanding it back to the full disc.
    let mut walk = Walk {
        file,
        file_len,
        image_size,
        dol_addr: dol_off,
        src: fst_end,
        dst: fst_end,
        nulls_pos: fst_end as i64 + 0x1C,
        segs,
        overlay,
        dol_patch: dol_off as u32,
    };
    walk.run(fst_offset, fst_size_aligned, &files)?;

    if walk.dst != image_size {
        return Err(bad("NKit reconstruction did not fill the disc image"));
    }
    walk.overlay[0..4].copy_from_slice(&walk.dol_patch.to_be_bytes());

    Ok(Layout {
        segments: walk.segs,
        overlay: walk.overlay,
        image_size,
        junk_id: header.junk_id,
        disc_num,
    })
}

/// GameCube FST: a flat array of 12-byte entries (`[type:1][name_off:3]
/// [data_off:4][size:4]`, big-endian). Entry 0 is the root; its `size` is the
/// total entry count. Files are `type == 0`; directories (`type == 1`) carry a
/// subtree-end index we can ignore because a linear scan already visits every
/// entry. Returns the files sorted by `(data_offset, length)`, matching NKit's
/// gap accounting.
fn parse_fst(fst: &[u8], fst_size: u64) -> io::Result<Vec<FstEntry>> {
    if fst.len() < 12 {
        return Err(bad("FST too small"));
    }
    let n_entries = be32(fst, 8) as usize;
    let table_len = n_entries
        .checked_mul(12)
        .ok_or_else(|| bad("FST entry count overflow"))?;
    if table_len > fst_size as usize || table_len > fst.len() {
        return Err(bad("FST entry count exceeds FST size"));
    }
    let mut files = Vec::new();
    for i in 1..n_entries {
        let o = 12 * i;
        let typ = fst[o];
        if typ == 0 {
            files.push(FstEntry {
                data_offset: be32(fst, o + 4) as u64,
                length: be32(fst, o + 8) as u64,
                fst_field: o + 4,
            });
        }
    }
    files.sort_by_key(|f| (f.data_offset, f.length));
    Ok(files)
}

/// Mutable state for the file/gap walk (a port of `NkitReaderGc.Read` +
/// `writeGap` + `copyFile`).
struct Walk<'a> {
    file: &'a File,
    file_len: u64,
    image_size: u64,
    dol_addr: u64,
    /// Read cursor within the `.nkit.iso` file.
    src: u64,
    /// Write cursor within the reconstructed disc.
    dst: u64,
    /// Running "leading nulls" boundary NKit uses to zero the first bytes of a
    /// junk run (can go negative, per the reference).
    nulls_pos: i64,
    segs: Vec<Segment>,
    overlay: Vec<u8>,
    dol_patch: u32,
}

impl Walk<'_> {
    fn run(
        &mut self,
        fst_offset: u64,
        fst_size_aligned: u64,
        files: &[FstEntry],
    ) -> io::Result<()> {
        // conFiles = [FST placeholder, files sorted by (offset,length)]; each
        // carries the gap *after* its own file — i.e. between this conFile's end
        // and the next file's start (or the end of the NKit stream for the last).
        let count = files.len() + 1;
        for i in 0..count {
            // The i-th conFile wraps the (i-1)-th file (or the FST placeholder).
            let (data_offset, length, fst_field) = if i == 0 {
                (fst_offset, fst_size_aligned, None)
            } else {
                let f = &files[i - 1];
                (f.data_offset, f.length, Some(f.fst_field))
            };

            let gap_len = self.gap_len(i, files, data_offset, length)?;

            if i != 0 {
                // Skip any 32K-alignment padding ahead of the file in the stream.
                if self.src < data_offset {
                    self.src = data_offset;
                }
                if data_offset == self.dol_addr {
                    self.dol_patch = self.dst as u32;
                }
                if let Some(field) = fst_field {
                    self.patch_fst(field, self.dst as u32);
                }
                self.copy_file(length)?;
            }

            if self.dst < self.image_size {
                let first_or_last = i == 0 || i == count - 1;
                let restored_len = self.write_gap(gap_len, length, first_or_last)?;
                if let Some(field) = fst_field {
                    // Length may have been restored for an all-junk file.
                    self.patch_fst(field + 4, restored_len.unwrap_or(length) as u32);
                }
            }
        }
        Ok(())
    }

    /// Compacted-stream gap length after conFile `i`, whose file spans
    /// `[cur_offset, cur_offset + cur_len)`. Only its zero/non-zero-ness matters
    /// to the decoder; the real disc gap size comes from the descriptor itself.
    fn gap_len(
        &self,
        i: usize,
        files: &[FstEntry],
        cur_offset: u64,
        cur_len: u64,
    ) -> io::Result<u64> {
        let end = align4(cur_offset + cur_len);
        if i < files.len() {
            let next = files[i].data_offset;
            next.checked_sub(end)
                .ok_or_else(|| bad("overlapping files in FST"))
        } else {
            // Last conFile: gap to the end of the NKit stream.
            Ok(self.file_len.saturating_sub(end))
        }
    }

    /// Copy a file's data verbatim from the compacted stream (aligned to 4,
    /// clamped to the image end).
    fn copy_file(&mut self, length: u64) -> io::Result<()> {
        if length == 0 {
            return Ok(());
        }
        let mut size = align4(length);
        // Clamp so a file never runs past the image end (rare unaligned final
        // file); `saturating_sub` also keeps malformed input from underflowing.
        size = size.min(self.image_size.saturating_sub(self.dst));
        let src = self.src;
        self.advance_src(size)?;
        self.emit(size, SegKind::File { src });
        self.nulls_pos = self.dst as i64 + 0x1C;
        Ok(())
    }

    /// Decode and expand the gap descriptor at the current stream position.
    /// Returns the restored file length when the gap encoded an all-junk file.
    fn write_gap(
        &mut self,
        gap_len: u64,
        file_len: u64,
        first_or_last: bool,
    ) -> io::Result<Option<u64>> {
        if gap_len == 0 {
            if file_len == 0 {
                self.nulls_pos = self.dst as i64 + 0x1C;
            }
            return Ok(None);
        }

        let mut size = self.read_u32()? as u64;
        let mut gt = size & 0b11;
        size &= 0xFFFF_FFFC;
        if size == 0xFFFF_FFFC {
            // 64-bit gap continuation (dual-layer Wii); not used by GameCube.
            size = 0xFFFF_FFFC + self.read_u32()? as u64;
        }

        let mut restored: Option<u64> = None;
        let mut nulls;

        if gt == 0b11 {
            // JunkFile: an FST file whose entire content is regenerable junk.
            self.nulls_pos = (self.nulls_pos - self.dst as i64).min(0);
            let file_nulls = (size & 0xFC) >> 2;
            let jf = self.read_u32()? as u64;
            restored = Some(jf);
            let jf_aligned = align4(jf);
            self.emit(file_nulls.min(jf_aligned), SegKind::Zero);
            self.emit(jf_aligned.saturating_sub(file_nulls), SegKind::Junk);
            if gap_len <= 8 {
                return Ok(restored);
            }
            size = self.read_u32()? as u64;
            gt = size & 0b11;
            size &= 0xFFFF_FFFC;
        } else if file_len == 0 {
            self.nulls_pos = self.dst as i64 + 0x1C;
        }

        let max_nulls = (self.nulls_pos - self.dst as i64).max(0) as u64;
        nulls = if size < max_nulls {
            size
        } else if size >= 0x40000 && !first_or_last {
            0
        } else {
            max_nulls
        };

        match gt {
            0b00 => {
                // AllJunk: leading zeros then regenerated junk.
                let nulls = nulls.min(size);
                self.emit(nulls, SegKind::Zero);
                self.emit(size - nulls, SegKind::Junk);
            }
            0b01 => {
                // AllScrubbed: the whole gap was zeros on the source disc.
                self.emit(size, SegKind::Zero);
            }
            _ => {
                // Mixed: a run-length stream of junk / preserved / byte-fill blocks.
                let mut prg = size;
                let mut bt = 0u64;
                let mut bt_byte = 0u8;
                let mut guard = 0u64;
                while prg > 0 {
                    let blk = self.read_u32()? as u64;
                    let bt_type = blk >> 30;
                    let repeat = bt_type == 0b11;
                    if !repeat {
                        bt = bt_type;
                    }
                    let mut cnt = blk & 0x3FFF_FFFF;
                    let bytes;
                    match bt {
                        0b01 => {
                            // NonJunk: preserved bytes copied verbatim.
                            bytes = (cnt * GAP_BLOCK).min(prg);
                            let src = self.src;
                            self.advance_src(bytes)?;
                            self.emit(bytes, SegKind::File { src });
                        }
                        0b10 => {
                            // ByteFill: a run of one repeated byte.
                            if !repeat {
                                bt_byte = (cnt & 0xFF) as u8;
                                cnt >>= 8;
                            }
                            bytes = (cnt * GAP_BLOCK).min(prg);
                            self.emit(bytes, SegKind::Byte(bt_byte));
                        }
                        _ => {
                            // Junk (with the leading-nulls rule re-applied).
                            bytes = (cnt * GAP_BLOCK).min(prg);
                            let mn = (self.nulls_pos - self.dst as i64).max(0) as u64;
                            nulls = if prg < mn {
                                bytes
                            } else if bytes >= 0x40000 && !first_or_last {
                                0
                            } else {
                                mn
                            };
                            let nulls = nulls.min(bytes);
                            self.emit(nulls, SegKind::Zero);
                            self.emit(bytes - nulls, SegKind::Junk);
                        }
                    }
                    if bytes == 0 {
                        return Err(bad("malformed NKit gap block (no progress)"));
                    }
                    prg -= bytes;
                    guard += 1;
                    if guard > (size / GAP_BLOCK) + 8 {
                        return Err(bad("runaway NKit gap decode"));
                    }
                }
            }
        }
        Ok(restored)
    }

    /// Append a segment of `len` bytes and advance `dst`.
    fn emit(&mut self, len: u64, kind: SegKind) {
        if len == 0 {
            return;
        }
        self.segs.push(Segment {
            dst: self.dst,
            len,
            kind,
        });
        self.dst += len;
    }

    fn advance_src(&mut self, n: u64) -> io::Result<()> {
        let end = self
            .src
            .checked_add(n)
            .ok_or_else(|| bad("stream position overflow"))?;
        if end > self.file_len {
            return Err(bad("NKit stream ended mid-data"));
        }
        self.src = end;
        Ok(())
    }

    fn read_u32(&mut self) -> io::Result<u32> {
        let v = read_u32_be_at(self.file, self.src, self.file_len)?;
        self.src += 4;
        Ok(v)
    }

    fn patch_fst(&mut self, field: usize, value: u32) {
        // overlay = [dol(4)] ++ [fst]; FST fields are relative to index 4.
        let at = 4 + field;
        if at + 4 <= self.overlay.len() {
            self.overlay[at..at + 4].copy_from_slice(&value.to_be_bytes());
        }
    }
}

// ── Read / Seek ───────────────────────────────────────────────────────────────

impl Read for NkitIsoReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let image_size = self.layout.image_size;
        if self.pos >= image_size || buf.is_empty() {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(image_size - self.pos) as usize;
        let buf = &mut buf[..want];

        let segs = &self.layout.segments;
        let mut idx = seg_index(segs, self.pos);
        let mut filled = 0usize;
        while filled < want {
            let seg = &segs[idx];
            let within = self.pos - seg.dst;
            let n = ((seg.len - within) as usize).min(want - filled);
            let out = &mut buf[filled..filled + n];
            match &seg.kind {
                SegKind::File { src } => {
                    read_exact_at(&self.file, out, src + within)?;
                }
                SegKind::Mem { off } => {
                    let start = off + within as usize;
                    out.copy_from_slice(&self.layout.overlay[start..start + n]);
                }
                SegKind::Zero => out.fill(0),
                SegKind::Byte(b) => out.fill(*b),
                SegKind::Junk => {
                    let mut lfg = LaggedFibonacci::default();
                    lfg.fill_sector_chunked(
                        out,
                        self.layout.junk_id,
                        self.layout.disc_num,
                        self.pos,
                    );
                }
            }
            self.pos += n as u64;
            filled += n;
            idx += 1;
        }
        Ok(filled)
    }
}

impl Seek for NkitIsoReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let image_size = self.layout.image_size;
        let new = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::End(n) => (image_size as i64 + n) as u64,
            SeekFrom::Current(n) => (self.pos as i64 + n) as u64,
        };
        self.pos = new;
        Ok(self.pos)
    }
}

/// Index of the segment containing `pos` (segments are sorted and contiguous).
fn seg_index(segs: &[Segment], pos: u64) -> usize {
    let i = segs.partition_point(|s| s.dst <= pos);
    i.saturating_sub(1)
}

// ── Small helpers ─────────────────────────────────────────────────────────────

fn push(segs: &mut Vec<Segment>, dst: &mut u64, len: u64, kind: SegKind) {
    if len == 0 {
        return;
    }
    segs.push(Segment { dst: *dst, len, kind });
    *dst += len;
}

fn align4(x: u64) -> u64 {
    (x + 3) & !3
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("NKit: {msg}"))
}

fn read_u32_be_at(file: &File, offset: u64, file_len: u64) -> io::Result<u32> {
    if offset + 4 > file_len {
        return Err(bad("read past end of NKit stream"));
    }
    let mut b = [0u8; 4];
    read_exact_at(file, &mut b, offset)?;
    Ok(u32::from_be_bytes(b))
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "failed to fill buffer",
                ))
            }
            Ok(n) => {
                let tmp = buf;
                buf = &mut tmp[n..];
                offset += n as u64;
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const IMAGE_SIZE: u64 = 0x8_0000;
    // Past bi2.bin (0x440..0x2440) and the apploader header (0x2440..0x2460) so
    // nod's `meta()` reads sane sections rather than overflowing on filler.
    const DOL_OFF: u64 = 0x2460;
    const FST_OFF: u64 = 0x2600;
    const GAME_ID: [u8; 4] = *b"GTES";
    const GCN_MAGIC: u32 = 0xC233_9F3D;

    fn put_be32(b: &mut [u8], o: usize, v: u32) {
        b[o..o + 4].copy_from_slice(&v.to_be_bytes());
    }

    fn push_entry(fst: &mut Vec<u8>, typ: u8, name_off: u32, data_off: u32, size: u32) {
        fst.extend_from_slice(&((typ as u32) << 24 | name_off).to_be_bytes());
        fst.extend_from_slice(&data_off.to_be_bytes());
        fst.extend_from_slice(&size.to_be_bytes());
    }

    /// Write the NKit marker for `version`, returning the header's zeroed length.
    fn write_nkit_header(nkit: &mut [u8], version: u8) -> u64 {
        match version {
            1 => {
                nkit[0x200..0x208].copy_from_slice(b"NKIT v01");
                put_be32(nkit, 0x208, 0); // crc (unused by the reader)
                put_be32(nkit, 0x210, IMAGE_SIZE as u32);
                // 0x214 junk-id override left zero → use the game ID
                V1_HEADER_ZERO_LEN
            }
            2 => {
                nkit[0x200..0x208].copy_from_slice(b"NKIT v02");
                let header_size: u16 = 0x14;
                nkit[0x208..0x20A].copy_from_slice(&header_size.to_be_bytes());
                nkit[0x20A..0x20C].copy_from_slice(&1u16.to_be_bytes()); // FLAG_SIZE
                nkit[0x20C..0x214].copy_from_slice(&IMAGE_SIZE.to_be_bytes());
                header_size as u64
            }
            _ => unreachable!(),
        }
    }

    /// Build a minimal but genuine standalone NKit ISO (two files packed after the
    /// FST, one trailing all-junk gap) and the disc image it must reconstruct to.
    fn build_synth(version: u8) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
        // FST: root + F0 + F1, then the name table.
        let fst_size = 12u64 * 3 + 6; // three entries + "F0\0F1\0"
        let fst_end = FST_OFF + align4(fst_size);
        let off0 = fst_end;
        let size0: u64 = 0x1000;
        let off1 = align4(off0 + size0);
        let size1: u64 = 0x1800;
        let file1_end = align4(off1 + size1);
        let trailing = IMAGE_SIZE - file1_end;

        let mut fst = Vec::new();
        push_entry(&mut fst, 1, 0, 0, 3); // root: size = entry count
        push_entry(&mut fst, 0, 0, off0 as u32, size0 as u32); // F0
        push_entry(&mut fst, 0, 3, off1 as u32, size1 as u32); // F1
        fst.extend_from_slice(b"F0\0F1\0");
        while fst.len() % 4 != 0 {
            fst.push(0);
        }
        assert_eq!(fst.len() as u64, align4(fst_size));

        // File payloads (distinct, verbatim-preserved bytes).
        let file0: Vec<u8> = (0..size0).map(|i| (0xA0u64 ^ i) as u8).collect();
        let file1: Vec<u8> = (0..size1).map(|i| (0x5Cu64 ^ i.wrapping_mul(7)) as u8).collect();

        // ── The .nkit.iso ──
        let mut nkit = vec![0u8; FST_OFF as usize];
        nkit[0..4].copy_from_slice(&GAME_ID);
        put_be32(&mut nkit, 0x1C, GCN_MAGIC);
        put_be32(&mut nkit, 0x420, DOL_OFF as u32);
        put_be32(&mut nkit, 0x424, FST_OFF as u32);
        put_be32(&mut nkit, 0x428, fst_size as u32);
        let zero_len = write_nkit_header(&mut nkit, version);
        // bi2.bin: distinctive filler to exercise the verbatim copy (opaque to nod).
        for (i, b) in nkit.iter_mut().enumerate().take(0x2440).skip(0x440) {
            *b = (i * 3) as u8;
        }
        // apploader header at 0x2440: size = trailer_size = 0 (already zero), so
        // nod reads only its 0x20-byte header. main.dol header at DOL_OFF: a single
        // zero-length text section gives nod a DOL size of 0x100 (just the header)
        // with no overflow.
        put_be32(&mut nkit, DOL_OFF as usize, 0x100); // DolHeader.text_offs[0]
        nkit.extend_from_slice(&fst); // [FST_OFF, fst_end)
        nkit.extend_from_slice(&file0); // sizes are 4-aligned, no padding
        nkit.extend_from_slice(&file1);
        // Trailing all-junk gap descriptor: (disc gap length) | GapType::AllJunk(0).
        nkit.extend_from_slice(&((trailing as u32) & 0xFFFF_FFFC).to_be_bytes());

        // ── The disc it must reconstruct to ──
        let mut disc = nkit[0..fst_end as usize].to_vec();
        for b in &mut disc[0x200..0x200 + zero_len as usize] {
            *b = 0;
        }
        disc.resize(IMAGE_SIZE as usize, 0);
        disc[off0 as usize..(off0 + size0) as usize].copy_from_slice(&file0);
        disc[off1 as usize..(off1 + size1) as usize].copy_from_slice(&file1);
        // Trailing gap: 0x1c leading zeros (the reference "nulls") then junk.
        let junk_start = file1_end + 0x1C;
        let mut lfg = LaggedFibonacci::default();
        lfg.fill_sector_chunked(
            &mut disc[junk_start as usize..],
            GAME_ID,
            0,
            junk_start,
        );

        (nkit, disc, file0, file1)
    }

    fn write_temp(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    fn reconstruct_all(reader: &mut NkitIsoReader) -> Vec<u8> {
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn version_detection() {
        for (v, magic) in [(1u8, &b"NKIT v01"[..]), (2, b"NKIT v02")] {
            let mut m = [0u8; 8];
            m.copy_from_slice(magic);
            assert_eq!(parse_version(&m), Some(v));
        }
        assert_eq!(parse_version(b"NKIT  v1"), None); // embedded (two-space) marker
        assert_eq!(parse_version(b"GAFE0100"), None);
    }

    #[test]
    fn reconstructs_v1_byte_exact() {
        let (nkit, expected, _, _) = build_synth(1);
        let tmp = write_temp(&nkit);
        let mut reader = NkitIsoReader::open(tmp.path()).unwrap();
        assert_eq!(reader.seek(SeekFrom::End(0)).unwrap(), IMAGE_SIZE);
        let got = reconstruct_all(&mut reader);
        assert_eq!(got.len(), expected.len());
        assert!(got == expected, "v1 reconstruction differs from expected disc");
    }

    #[test]
    fn rejects_v2_standalone() {
        // "NKit v2" is not a standalone-ISO format (it lives inside CISO/WBFS/RVZ,
        // handled by nod). A standalone `NKIT v02` marker must be refused cleanly,
        // never reconstructed or hung on.
        let (nkit, ..) = build_synth(2);
        let err = NkitIsoReader::open(write_temp(&nkit).path())
            .err()
            .expect("v2 standalone must be rejected");
        assert!(
            err.to_string().contains("v2"),
            "expected a v2-specific error, got: {err}"
        );
    }

    #[test]
    fn nod_browses_reconstructed_disc() {
        // The reconstructed stream must read back as an ordinary GameCube disc.
        let (nkit, _, file0, file1) = build_synth(1);
        let tmp = write_temp(&nkit);
        let reader = NkitIsoReader::open(tmp.path()).unwrap();
        let disc = nod::Disc::new_stream(Box::new(reader)).expect("nod opens reconstruction");
        assert!(disc.header().is_gamecube());
        let mut part = disc
            .open_partition_kind(nod::PartitionKind::Data)
            .expect("data partition");
        let meta = part.meta().expect("meta");
        let fst = meta.fst().expect("fst");
        let (_, node0) = fst.find("/F0").expect("F0 present");
        let (_, node1) = fst.find("/F1").expect("F1 present");
        assert_eq!(node0.length() as usize, file0.len());
        assert_eq!(node1.length() as usize, file1.len());
        let mut buf = Vec::new();
        part.open_file(node0)
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();
        assert_eq!(buf, file0, "extracted F0 must be byte-exact");
    }

    #[test]
    fn seek_and_partial_reads_are_consistent() {
        let (nkit, expected, _, _) = build_synth(1);
        let tmp = write_temp(&nkit);
        let mut reader = NkitIsoReader::open(tmp.path()).unwrap();
        // Spot-check reads at boundaries and inside the junk region.
        for &(off, len) in &[(0u64, 0x40usize), (0x1FE, 0x40), (FST_OFF, 8), (0x4820, 0x40)] {
            reader.seek(SeekFrom::Start(off)).unwrap();
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).unwrap();
            assert_eq!(
                buf,
                &expected[off as usize..off as usize + len],
                "mismatch at {off:#x}"
            );
        }
    }

    #[test]
    fn malformed_inputs_error_cleanly() {
        // Truncated below the header size.
        assert!(NkitIsoReader::open(write_temp(&[0u8; 0x100]).path()).is_err());

        // Header present but not an NKit marker.
        let mut plain = vec![0u8; FST_OFF as usize];
        put_be32(&mut plain, 0x1C, GCN_MAGIC);
        assert!(NkitIsoReader::open(write_temp(&plain).path()).is_err());

        // Valid marker, absurd FST entry count → rejected, not a hang.
        let (mut nkit, _, _, _) = build_synth(1);
        put_be32(&mut nkit, FST_OFF as usize + 8, 0x00FF_FFFF); // root size
        assert!(NkitIsoReader::open(write_temp(&nkit).path()).is_err());

        // Valid marker, image size that the gap stream cannot fill.
        let (mut nkit, _, _, _) = build_synth(1);
        put_be32(&mut nkit, 0x210, IMAGE_SIZE as u32 * 2);
        assert!(NkitIsoReader::open(write_temp(&nkit).path()).is_err());

        // Truncated mid-stream: drop the trailing gap descriptor.
        let (mut nkit, _, _, _) = build_synth(1);
        nkit.truncate(nkit.len() - 4);
        assert!(NkitIsoReader::open(write_temp(&nkit).path()).is_err());
    }
}
