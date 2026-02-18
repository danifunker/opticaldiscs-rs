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

// ── Phase 2: ISO Sector Reader ────────────────────────────────────────────────
// Uncomment when IsoSectorReader + PVD parsing are implemented.

// #[test]
// fn read_pvd_from_iso() {
//     use opticaldiscs::sector_reader::IsoSectorReader;
//     use opticaldiscs::iso9660::PrimaryVolumeDescriptor;
//     let mut reader = IsoSectorReader::new("tests/fixtures/data.iso").unwrap();
//     let pvd = PrimaryVolumeDescriptor::read_from(&mut reader).unwrap();
//     assert!(!pvd.volume_id.is_empty());
// }

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
//     let info = DiscImageInfo::open("tests/fixtures/data.iso").unwrap();
//     let mut fs = opticaldiscs::browse::open_disc_filesystem(&info).unwrap();
//     let root = fs.root().unwrap();
//     let entries = fs.list_directory(&root).unwrap();
//     assert!(!entries.is_empty());
// }
