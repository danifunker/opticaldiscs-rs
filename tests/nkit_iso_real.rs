//! Optional integration test against a real GameCube **NKit v1 ISO** library.
//!
//! Skipped unless a GameCube NKit directory is present. It defaults to
//! `/Volumes/ConsoleGames/ROMs/GCN` (the maintainer's library) and can be
//! overridden with `OPTICALDISCS_GCN_NKIT=/path/to/dir`.
//!
//! For every `*.nkit.iso` it finds (capped, smallest first, so CI-ish runs stay
//! quick) it asserts:
//!
//! 1. `DiscImageInfo::open` reports `filesystem == GameCube` (detection intact).
//! 2. `open_disc_filesystem` yields a browsable FST with a non-empty root.
//! 3. Full-disc reconstruction hashes to the CRC32 stored in the NKit header —
//!    the definitive proof that junk/gap regeneration lines up bit-for-bit.

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use opticaldiscs::browse::{self, EntryType, FileEntry, Filesystem};
use opticaldiscs::detect::DiscImageInfo;
use opticaldiscs::formats::FilesystemType;
use opticaldiscs::nkit_iso::NkitIsoReader;

/// Find the smallest non-empty file within `depth` directory levels of `entries`.
fn find_file(fs: &mut dyn Filesystem, entries: &[FileEntry], depth: usize) -> Option<FileEntry> {
    let mut best: Option<FileEntry> = None;
    for e in entries {
        match e.entry_type {
            EntryType::File if e.size > 0 => {
                if best.as_ref().is_none_or(|b| e.size < b.size) {
                    best = Some(e.clone());
                }
            }
            EntryType::Directory if depth > 0 => {
                if let Ok(children) = fs.list_directory(e) {
                    if let Some(f) = find_file(fs, &children, depth - 1) {
                        if best.as_ref().is_none_or(|b| f.size < b.size) {
                            best = Some(f);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    best
}

fn gcn_dir() -> PathBuf {
    std::env::var_os("OPTICALDISCS_GCN_NKIT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/Volumes/ConsoleGames/ROMs/GCN"))
}

/// Standard IEEE (zlib/PNG/redump) CRC32, computed with a runtime table.
fn crc32_stream(mut r: impl Read) -> std::io::Result<u32> {
    let mut table = [0u32; 256];
    for (n, slot) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
        }
    }
    Ok(crc ^ 0xFFFF_FFFF)
}

fn stored_crc_and_size(path: &std::path::Path) -> (u32, u64) {
    let mut f = std::fs::File::open(path).unwrap();
    let mut hdr = [0u8; 0x218];
    f.seek(SeekFrom::Start(0)).unwrap();
    f.read_exact(&mut hdr).unwrap();
    let crc = u32::from_be_bytes(hdr[0x208..0x20C].try_into().unwrap());
    let size = u32::from_be_bytes(hdr[0x210..0x214].try_into().unwrap()) as u64;
    (crc, size)
}

#[test]
fn browses_and_reconstructs_real_gamecube_nkit_iso() {
    let dir = gcn_dir();
    if !dir.is_dir() {
        eprintln!("{} not present — skipping", dir.display());
        return;
    }

    // Collect *.nkit.iso, smallest first (AnimalCrossing ~27 MB leads).
    let mut samples: Vec<(u64, PathBuf)> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.to_ascii_lowercase().ends_with(".nkit.iso"))
        })
        .map(|p| {
            (
                std::fs::metadata(&p).map(|m| m.len()).unwrap_or(u64::MAX),
                p,
            )
        })
        .collect();
    samples.sort();

    // Cap how many we fully hash — reconstructing 1.46 GB each is not free.
    let full_hash_budget: usize = std::env::var("OPTICALDISCS_GCN_NKIT_FULL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    // Cap how many we browse — a full library can be 1000+ images.
    let browse_budget: usize = std::env::var("OPTICALDISCS_GCN_NKIT_BROWSE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);

    assert!(
        !samples.is_empty(),
        "no *.nkit.iso found in {}",
        dir.display()
    );
    samples.truncate(browse_budget.max(full_hash_budget));

    for (idx, (_, path)) in samples.iter().enumerate() {
        // 1. Detection unchanged.
        let info = DiscImageInfo::open(path)
            .unwrap_or_else(|e| panic!("open {} failed: {e}", path.display()));
        assert_eq!(
            info.filesystem,
            FilesystemType::GameCube,
            "expected GameCube for {}",
            path.display()
        );

        // 2. Browsable FST with a non-empty root.
        let mut fs = browse::open_disc_filesystem(&info)
            .unwrap_or_else(|e| panic!("browse {} failed: {e}", path.display()));
        let root = fs.root().unwrap();
        let entries = fs
            .list_directory(&root)
            .unwrap_or_else(|e| panic!("list root of {} failed: {e}", path.display()));
        assert!(!entries.is_empty(), "empty root FST for {}", path.display());

        // 2b. Extract one real file through nod and confirm it reads at its full
        //     FST-declared length (exercises the byte-for-byte extraction path).
        if let Some(file) = find_file(fs.as_mut(), &entries, 3) {
            let bytes = fs
                .read_file(&file)
                .unwrap_or_else(|e| panic!("read {} in {}: {e}", file.path, path.display()));
            assert_eq!(
                bytes.len() as u64,
                file.size,
                "short read of {} in {}",
                file.path,
                path.display()
            );
        }

        // 3. Full-disc reconstruction hashes to the stored CRC32.
        if idx < full_hash_budget {
            let (want_crc, want_size) = stored_crc_and_size(path);
            let reader = NkitIsoReader::open(path)
                .unwrap_or_else(|e| panic!("NkitIsoReader::open {} failed: {e}", path.display()));
            let got_size = {
                let mut r = reader.clone();
                r.seek(SeekFrom::End(0)).unwrap()
            };
            assert_eq!(got_size, want_size, "image size for {}", path.display());
            let got_crc = crc32_stream(reader).unwrap();
            assert_eq!(
                got_crc,
                want_crc,
                "reconstructed CRC32 mismatch for {} (got {:08X}, want {:08X})",
                path.display(),
                got_crc,
                want_crc
            );
            eprintln!("verified full reconstruction: {}", path.display());
        }
    }
}
