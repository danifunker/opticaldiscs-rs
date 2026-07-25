//! Integration tests — one section per PLAN.md phase.
//! Uncomment / expand each section as the corresponding phase is implemented.

// ── Phase 1: Core Types ───────────────────────────────────────────────────────

#[test]
fn disc_format_from_extension() {
    use opticaldiscs::DiscFormat;
    assert_eq!(DiscFormat::from_path("disc.iso"), Some(DiscFormat::Iso));
    assert_eq!(DiscFormat::from_path("disc.ISO"), Some(DiscFormat::Iso));
    assert_eq!(DiscFormat::from_path("disc.toast"), Some(DiscFormat::Iso));
    assert_eq!(DiscFormat::from_path("disc.bin"), Some(DiscFormat::BinCue));
    assert_eq!(DiscFormat::from_path("disc.cue"), Some(DiscFormat::BinCue));
    assert_eq!(DiscFormat::from_path("disc.chd"), Some(DiscFormat::Chd));
    assert_eq!(DiscFormat::from_path("disc.mds"), Some(DiscFormat::MdsMdf));
    assert_eq!(DiscFormat::from_path("disc.txt"), None);
}

#[test]
fn disc_format_display_names() {
    use opticaldiscs::DiscFormat;
    assert_eq!(DiscFormat::Iso.display_name(), "ISO image");
    assert_eq!(DiscFormat::BinCue.display_name(), "BIN/CUE");
    assert_eq!(
        DiscFormat::Chd.display_name(),
        "CHD (Compressed Hunks of Data)"
    );
}

#[test]
fn filesystem_type_browsable() {
    use opticaldiscs::FilesystemType;
    assert!(FilesystemType::Iso9660.is_browsable());
    assert!(FilesystemType::Hfs.is_browsable());
    assert!(FilesystemType::HfsPlus.is_browsable());
    assert!(FilesystemType::Efs.is_browsable());
    assert!(FilesystemType::Ufs.is_browsable());
    assert!(FilesystemType::Ods2.is_browsable());
    assert!(FilesystemType::HighSierra.is_browsable());
    assert!(FilesystemType::Udf.is_browsable());
    assert!(!FilesystemType::Unknown.is_browsable());
}

#[test]
fn supported_extensions_list() {
    use opticaldiscs::supported_extensions;
    let exts = supported_extensions();
    assert!(exts.contains(&"iso"));
    assert!(exts.contains(&"cue"));
    assert!(exts.contains(&"chd"));
}

#[test]
fn file_entry_helpers() {
    use opticaldiscs::FileEntry;

    let file = FileEntry::new_file("readme.txt".into(), "/readme.txt".into(), 4096, 22);
    assert!(file.is_file());
    assert!(!file.is_directory());
    assert_eq!(file.size_string(), "4.0 KB");

    let dir = FileEntry::new_directory("System".into(), "/System".into(), 30);
    assert!(dir.is_directory());
    assert_eq!(dir.size_string(), "");

    let root = FileEntry::root(16);
    assert_eq!(root.path, "/");
    assert!(root.is_directory());
}

// ── Phase 2: ISO Sector Reader + PVD ─────────────────────────────────────────

/// Build a minimal but valid ISO 9660 image in a temp file and return its path.
///
/// Layout:
///   sectors  0-15 : System Area (zeros)
///   sector  16    : Primary Volume Descriptor
///   sector  17    : Volume Descriptor Set Terminator
///   sector  18    : Root directory (one sector of zeros)
#[cfg(test)]
fn write_test_iso(volume_label: &str) -> (tempfile::NamedTempFile, std::path::PathBuf) {
    use opticaldiscs::iso9660::build_test_pvd_sector;
    use std::io::Write;

    let mut f = tempfile::Builder::new()
        .suffix(".iso")
        .tempfile()
        .expect("tempfile");

    const SECTOR: usize = 2048;
    const SECTORS: usize = 19; // system area + PVD + terminator + root dir
    let mut img = vec![0u8; SECTORS * SECTOR];

    // PVD at sector 16
    let pvd = build_test_pvd_sector(volume_label, 18, SECTOR as u32);
    img[16 * SECTOR..17 * SECTOR].copy_from_slice(&pvd);

    // Volume Descriptor Set Terminator at sector 17
    img[17 * SECTOR] = 0xFF;
    img[17 * SECTOR + 1..17 * SECTOR + 6].copy_from_slice(b"CD001");
    img[17 * SECTOR + 6] = 1;

    f.write_all(&img).expect("write test ISO");
    f.flush().expect("flush");

    let path = f.path().to_path_buf();
    (f, path)
}

#[test]
fn iso_sector_reader_reads_sector() {
    use opticaldiscs::sector_reader::{IsoSectorReader, SectorReader, SECTOR_SIZE};

    let (_f, path) = write_test_iso("SECTOR_TEST");
    let mut reader = IsoSectorReader::new(&path).unwrap();

    // Sector 0 should be all zeros (system area)
    let s0 = reader.read_sector(0).unwrap();
    assert_eq!(s0.len(), SECTOR_SIZE as usize);
    assert!(s0.iter().all(|&b| b == 0));
}

#[test]
fn iso_sector_reader_read_bytes() {
    use opticaldiscs::iso9660::PVD_OFFSET;
    use opticaldiscs::sector_reader::{IsoSectorReader, SectorReader};

    let (_f, path) = write_test_iso("BYTES_TEST");
    let mut reader = IsoSectorReader::new(&path).unwrap();

    // Bytes 1..6 of the PVD should be "CD001"
    let id = reader.read_bytes(PVD_OFFSET + 1, 5).unwrap();
    assert_eq!(&id, b"CD001");
}

#[test]
fn pvd_read_from_iso_file() {
    use opticaldiscs::iso9660::PrimaryVolumeDescriptor;
    use opticaldiscs::sector_reader::IsoSectorReader;

    let (_f, path) = write_test_iso("MY_TEST_DISC");
    let mut reader = IsoSectorReader::new(&path).unwrap();
    let pvd = PrimaryVolumeDescriptor::read_from(&mut reader).unwrap();

    assert_eq!(pvd.volume_id, "MY_TEST_DISC");
    assert_eq!(pvd.logical_block_size, 2048);
    assert_eq!(pvd.root_directory_lba, 18);
}

#[test]
fn disc_image_info_open_iso() {
    use opticaldiscs::detect::DiscImageInfo;
    use opticaldiscs::FilesystemType;

    let (_f, path) = write_test_iso("DETECT_ME");
    let info = DiscImageInfo::open(&path).unwrap();

    assert_eq!(info.filesystem, FilesystemType::Iso9660);
    assert_eq!(info.volume_label.as_deref(), Some("DETECT_ME"));
    assert!(info.pvd.is_some());
}

// ── Phase 3: BIN/CUE Sector Reader ───────────────────────────────────────────

/// Write a minimal Mode1/2352 BIN + CUE pair containing a valid ISO 9660 PVD.
///
/// Layout (raw 2352-byte sectors):
///   sectors  0-15 : zeros (system area)
///   sector  16    : Mode1 header (16 bytes) + PVD (2048 bytes) + EDC/ECC zeros
///   sector  17    : zeros (Volume Descriptor Set Terminator)
///   sector  18    : zeros (root directory — empty)
#[cfg(test)]
fn write_test_bincue(
    volume_label: &str,
) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    use opticaldiscs::iso9660::build_test_pvd_sector;
    use std::io::Write;

    const RAW: usize = 2352;
    const SECTORS: usize = 19; // system area + PVD + terminator + root dir

    let dir = tempfile::tempdir().expect("tempdir");

    // Build raw BIN: 19 sectors × 2352 bytes
    let mut bin = vec![0u8; SECTORS * RAW];

    // Embed the PVD at sector 16: 16-byte Mode1 header + 2048-byte user data
    let pvd_data = build_test_pvd_sector(volume_label, 18, 2048);
    let sector16_start = 16 * RAW;
    // Bytes 0-15: sync/header (zeros OK for testing — reader skips them)
    bin[sector16_start + 16..sector16_start + 16 + 2048].copy_from_slice(&pvd_data);

    let bin_path = dir.path().join("disc.bin");
    let cue_path = dir.path().join("disc.cue");

    std::fs::write(&bin_path, &bin).expect("write BIN");

    let cue_content = "FILE \"disc.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n";
    let mut f = std::fs::File::create(&cue_path).expect("create CUE");
    f.write_all(cue_content.as_bytes()).expect("write CUE");

    (dir, bin_path, cue_path)
}

#[test]
fn bincue_sector_reader_reads_pvd() {
    use opticaldiscs::bincue::parse_cue_tracks;
    use opticaldiscs::iso9660::PrimaryVolumeDescriptor;
    use opticaldiscs::sector_reader::BinCueSectorReader;

    let (_dir, _bin, cue_path) = write_test_bincue("BINCUE_TEST");

    let tracks = parse_cue_tracks(&cue_path).unwrap();
    let data_track = tracks.into_iter().find(|t| t.is_data()).unwrap();
    let mut reader = BinCueSectorReader::open(&data_track).unwrap();

    let pvd = PrimaryVolumeDescriptor::read_from(&mut reader).unwrap();
    assert_eq!(pvd.volume_id, "BINCUE_TEST");
    assert_eq!(pvd.logical_block_size, 2048);
}

#[test]
fn disc_image_info_open_bincue() {
    use opticaldiscs::detect::DiscImageInfo;
    use opticaldiscs::{DiscFormat, FilesystemType};

    let (_dir, _bin, cue_path) = write_test_bincue("DETECT_BINCUE");
    let info = DiscImageInfo::open(&cue_path).unwrap();

    assert_eq!(info.format, DiscFormat::BinCue);
    assert_eq!(info.filesystem, FilesystemType::Iso9660);
    assert_eq!(info.volume_label.as_deref(), Some("DETECT_BINCUE"));
    assert!(info.pvd.is_some());
}

// ── Phase 4: CHD Sector Reader ────────────────────────────────────────────────
//
// These tests require a real CHD test fixture.  Create one with chdman:
//
//   mkdir -p tests/fixtures
//   chdman createcd -i tests/fixtures/data.cue -o tests/fixtures/data.chd
//
// (Use the BIN/CUE pair already present from Phase 3 testing, or supply your
// own.)  Tests are silently skipped when the fixture is absent so that CI
// stays green without requiring chdman.

/// Return the path to the CHD test fixture, or `None` if it is not present.
#[cfg(all(test, feature = "chd"))]
fn fixture_chd_path() -> Option<std::path::PathBuf> {
    let path = std::path::Path::new("tests/fixtures/data.chd");
    if path.exists() {
        Some(path.to_path_buf())
    } else {
        None
    }
}

#[cfg(feature = "chd")]
#[test]
fn open_chd_missing_file_returns_io_error() {
    use opticaldiscs::chd::open_chd;
    use opticaldiscs::OpticaldiscsError;
    let err = open_chd("no_such_file.chd").unwrap_err();
    assert!(matches!(err, OpticaldiscsError::Io(_)));
}

#[cfg(feature = "chd")]
#[test]
fn chd_sector_reader_reads_pvd() {
    use opticaldiscs::chd::open_chd;
    use opticaldiscs::iso9660::PrimaryVolumeDescriptor;
    use opticaldiscs::sector_reader::ChdSectorReader;

    let path = match fixture_chd_path() {
        Some(p) => p,
        None => {
            eprintln!("SKIP chd_sector_reader_reads_pvd: tests/fixtures/data.chd not found");
            return;
        }
    };

    let info = open_chd(&path).unwrap();
    let track = info
        .find_first_data_track()
        .expect("no data track in fixture CHD");
    let mut reader = ChdSectorReader::open(&path, track).unwrap();
    let pvd = PrimaryVolumeDescriptor::read_from(&mut reader).unwrap();
    assert!(!pvd.volume_id.is_empty(), "volume_id should be non-empty");
}

#[cfg(feature = "chd")]
#[test]
fn disc_image_info_open_chd() {
    use opticaldiscs::detect::DiscImageInfo;
    use opticaldiscs::{DiscFormat, FilesystemType};

    let path = match fixture_chd_path() {
        Some(p) => p,
        None => {
            eprintln!("SKIP disc_image_info_open_chd: tests/fixtures/data.chd not found");
            return;
        }
    };

    let info = DiscImageInfo::open(&path).unwrap();
    assert_eq!(info.format, DiscFormat::Chd);
    assert_eq!(info.filesystem, FilesystemType::Iso9660);
    assert!(info.volume_label.is_some());
}

// ── Feature `chd` off: recognised, but not openable ───────────────────────────

/// Without `chd`, a real CHD is still *identified* — only the read path is gone.
///
/// This is the contract the decoupling rests on: a consumer can name the format
/// in an error message or a UI listing even in a build that cannot open it.
#[cfg(not(feature = "chd"))]
#[test]
fn chd_recognised_but_open_fails_without_feature() {
    use opticaldiscs::detect::{detect_format, DiscImageInfo};
    use opticaldiscs::{DiscFormat, OpticaldiscsError};
    use std::io::Write;

    let mut f = tempfile::Builder::new().suffix(".chd").tempfile().unwrap();
    f.write_all(b"MComprHD").unwrap();
    f.write_all(&[0u8; 512]).unwrap();
    f.flush().unwrap();

    // Detection still works — by extension and by magic.
    assert_eq!(detect_format(f.path()).unwrap(), DiscFormat::Chd);

    // Opening reports the missing feature rather than an unknown format.
    let err = DiscImageInfo::open(f.path()).unwrap_err();
    match err {
        OpticaldiscsError::UnsupportedFormat(msg) => {
            assert!(
                msg.contains("CHD support was not compiled in"),
                "got: {msg}"
            );
        }
        other => panic!("expected UnsupportedFormat, got {other:?}"),
    }
}

/// The runtime query must agree with the compiled feature set.
#[test]
fn chd_runtime_support_query_matches_build() {
    use opticaldiscs::DiscFormat;

    assert_eq!(opticaldiscs::chd::is_supported(), cfg!(feature = "chd"));
    assert_eq!(DiscFormat::Chd.is_supported(), cfg!(feature = "chd"));

    // Unconditional formats are always supported; `.chd` is always *recognised*.
    assert!(DiscFormat::Iso.is_supported());
    assert!(opticaldiscs::supported_extensions().contains(&"chd"));
    assert_eq!(
        opticaldiscs::enabled_extensions().contains(&"chd"),
        cfg!(feature = "chd")
    );
}

// ── Phase 6: Disc Detection ───────────────────────────────────────────────────

#[test]
fn detect_format_from_extension() {
    use opticaldiscs::detect::detect_format;
    use opticaldiscs::DiscFormat;
    use std::path::Path;

    assert_eq!(
        detect_format(Path::new("disc.iso")).unwrap(),
        DiscFormat::Iso
    );
    assert_eq!(
        detect_format(Path::new("disc.cue")).unwrap(),
        DiscFormat::BinCue
    );
    assert_eq!(
        detect_format(Path::new("disc.chd")).unwrap(),
        DiscFormat::Chd
    );
}

#[test]
fn detect_filesystem_on_iso() {
    use opticaldiscs::detect::detect_filesystem;
    use opticaldiscs::sector_reader::IsoSectorReader;
    use opticaldiscs::FilesystemType;

    let (_f, path) = write_test_iso("DETECT_FS");
    let mut reader = IsoSectorReader::new(&path).unwrap();
    let fs = detect_filesystem(&mut reader).unwrap();
    assert_eq!(fs, FilesystemType::Iso9660);
}

#[cfg(feature = "toc")]
#[test]
fn disc_image_info_bincue_has_toc() {
    use opticaldiscs::detect::DiscImageInfo;

    let (_dir, _bin, cue_path) = write_test_bincue("TOC_BINCUE");
    let info = DiscImageInfo::open(&cue_path).unwrap();

    let toc = info.toc.as_ref().expect("BIN/CUE should have a TOC");
    assert_eq!(toc.first_track, 1);
    assert_eq!(toc.track_count(), 1);
    // Lead-out should be > 150 (pregap) + some frames
    assert!(toc.lead_out > 150);
}

#[cfg(feature = "toc")]
#[test]
fn disc_image_info_iso_has_no_toc() {
    use opticaldiscs::detect::DiscImageInfo;

    let (_f, path) = write_test_iso("NO_TOC_ISO");
    let info = DiscImageInfo::open(&path).unwrap();
    assert!(info.toc.is_none(), "plain ISO should have no TOC");
}

// ── Phase 7: ISO9660 Browser ─────────────────────────────────────────────────

/// Build a minimal ISO 9660 image with files in the root directory.
///
/// Each entry in `files` is a `(identifier_with_version, content)` pair.
/// Files are placed at consecutive sectors starting at sector 19;
/// the root directory lives at sector 18.
#[cfg(test)]
fn write_test_iso_with_files(
    volume_label: &str,
    files: &[(&str, &[u8])],
) -> (tempfile::NamedTempFile, std::path::PathBuf) {
    use opticaldiscs::iso9660::build_test_pvd_sector;
    use std::io::Write;

    const SECTOR: usize = 2048;
    let total_sectors = 19 + files.len();
    let mut img = vec![0u8; total_sectors * SECTOR];

    // PVD at sector 16, root dir at sector 18, size = one sector.
    let pvd = build_test_pvd_sector(volume_label, 18, SECTOR as u32);
    img[16 * SECTOR..17 * SECTOR].copy_from_slice(&pvd);

    // Volume Descriptor Set Terminator at sector 17.
    img[17 * SECTOR] = 0xFF;
    img[17 * SECTOR + 1..17 * SECTOR + 6].copy_from_slice(b"CD001");
    img[17 * SECTOR + 6] = 1;

    // Root directory at sector 18.
    let mut dir = vec![0u8; SECTOR];
    let mut dir_off = 0usize;

    let append_rec =
        |buf: &mut Vec<u8>, off: &mut usize, lba: u32, len: u32, flags: u8, id: &[u8]| {
            let id_len = id.len();
            let rec_len = 33 + id_len;
            let rec_len = if rec_len % 2 == 0 {
                rec_len
            } else {
                rec_len + 1
            };
            buf[*off] = rec_len as u8;
            buf[*off + 2..*off + 6].copy_from_slice(&lba.to_le_bytes());
            buf[*off + 6..*off + 10].copy_from_slice(&lba.to_be_bytes());
            buf[*off + 10..*off + 14].copy_from_slice(&len.to_le_bytes());
            buf[*off + 14..*off + 18].copy_from_slice(&len.to_be_bytes());
            buf[*off + 25] = flags;
            buf[*off + 32] = id_len as u8;
            buf[*off + 33..*off + 33 + id_len].copy_from_slice(id);
            *off += rec_len;
        };

    // `.` and `..` entries (both pointing to root sector 18).
    append_rec(&mut dir, &mut dir_off, 18, SECTOR as u32, 0x02, b"\x00");
    append_rec(&mut dir, &mut dir_off, 18, SECTOR as u32, 0x02, b"\x01");

    for (i, (name, content)) in files.iter().enumerate() {
        let file_lba = 19u32 + i as u32;
        append_rec(
            &mut dir,
            &mut dir_off,
            file_lba,
            content.len() as u32,
            0x00,
            name.as_bytes(),
        );
        let sector_start = (19 + i) * SECTOR;
        let copy_len = content.len().min(SECTOR);
        img[sector_start..sector_start + copy_len].copy_from_slice(&content[..copy_len]);
    }

    img[18 * SECTOR..19 * SECTOR].copy_from_slice(&dir);

    let mut f = tempfile::Builder::new()
        .suffix(".iso")
        .tempfile()
        .expect("tempfile");
    f.write_all(&img).expect("write");
    f.flush().expect("flush");
    let path = f.path().to_path_buf();
    (f, path)
}

#[test]
fn browse_iso_root() {
    use opticaldiscs::detect::DiscImageInfo;

    let (_f, path) = write_test_iso("BROWSE_TEST");
    let info = DiscImageInfo::open(&path).unwrap();
    let mut fs = opticaldiscs::browse::open_disc_filesystem(&info).unwrap();
    let root = fs.root().unwrap();
    let entries = fs.list_directory(&root).unwrap();
    // Empty root dir in our minimal image is fine — just check it doesn't panic.
    assert!(entries.is_empty());
}

#[test]
fn browse_iso_file_listing() {
    use opticaldiscs::detect::DiscImageInfo;

    let (_f, path) = write_test_iso_with_files(
        "FILES_TEST",
        &[("README.TXT;1", b"hello"), ("DATA.BIN;1", b"binary")],
    );
    let info = DiscImageInfo::open(&path).unwrap();
    let mut fs = opticaldiscs::browse::open_disc_filesystem(&info).unwrap();

    let root = fs.root().unwrap();
    let entries = fs.list_directory(&root).unwrap();

    assert_eq!(entries.len(), 2);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"README.TXT"));
    assert!(names.contains(&"DATA.BIN"));
}

#[test]
fn browse_iso_read_file() {
    use opticaldiscs::detect::DiscImageInfo;

    let content = b"the quick brown fox";
    let (_f, path) = write_test_iso_with_files("READ_TEST", &[("FOX.TXT;1", content)]);
    let info = DiscImageInfo::open(&path).unwrap();
    let mut fs = opticaldiscs::browse::open_disc_filesystem(&info).unwrap();

    let root = fs.root().unwrap();
    let entries = fs.list_directory(&root).unwrap();
    let file = entries.iter().find(|e| e.name == "FOX.TXT").unwrap();

    let data = fs.read_file(file).unwrap();
    assert_eq!(data, content);
}

#[test]
fn browse_iso_volume_name() {
    use opticaldiscs::detect::DiscImageInfo;

    let (_f, path) = write_test_iso_with_files("MY_VOLUME", &[]);
    let info = DiscImageInfo::open(&path).unwrap();
    let fs = opticaldiscs::browse::open_disc_filesystem(&info).unwrap();
    assert_eq!(fs.volume_name(), Some("MY_VOLUME"));
}

#[test]
fn browse_bincue_iso9660() {
    use opticaldiscs::detect::DiscImageInfo;

    // BIN/CUE image with ISO 9660 filesystem should also be browsable.
    let (_dir, _bin, cue_path) = write_test_bincue("BINCUE_BROWSE");
    let info = DiscImageInfo::open(&cue_path).unwrap();
    let mut fs = opticaldiscs::browse::open_disc_filesystem(&info).unwrap();
    let root = fs.root().unwrap();
    let _ = fs.list_directory(&root).unwrap();
}
