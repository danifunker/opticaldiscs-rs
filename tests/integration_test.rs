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
    assert!(!FilesystemType::Udf.is_browsable());
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
// Uncomment when BinCueSectorReader is implemented.

// #[test]
// fn read_pvd_from_bincue() { ... }

// ── Phase 4: CHD Sector Reader ────────────────────────────────────────────────
// Uncomment when ChdSectorReader is implemented.

// #[test]
// fn read_pvd_from_chd() { ... }

// ── Phase 7: ISO9660 Browser ──────────────────────────────────────────────────
// Uncomment when Iso9660Filesystem is implemented.

// #[test]
// fn browse_iso_root() {
//     use opticaldiscs::detect::DiscImageInfo;
//     let (_f, path) = write_test_iso("BROWSE_TEST");
//     let info = DiscImageInfo::open(&path).unwrap();
//     let mut fs = opticaldiscs::browse::open_disc_filesystem(&info).unwrap();
//     let root = fs.root().unwrap();
//     let entries = fs.list_directory(&root).unwrap();
//     // Empty root dir in our minimal image is fine — just check it doesn't panic
//     let _ = entries;
// }
