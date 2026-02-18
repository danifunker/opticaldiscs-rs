//! Placeholder integration tests.
//! Filled in progressively as each Phase is implemented (see PLAN.md).

/// Smoke test — the library compiles and basic types work.
#[test]
fn formats_compile() {
    use opticaldiscs::formats::{DiscFormat, FilesystemType};

    assert_eq!(DiscFormat::from_path("disc.iso"),  Some(DiscFormat::Iso));
    assert_eq!(DiscFormat::from_path("disc.cue"),  Some(DiscFormat::BinCue));
    assert_eq!(DiscFormat::from_path("disc.chd"),  Some(DiscFormat::Chd));
    assert_eq!(DiscFormat::from_path("disc.txt"),  None);

    assert!(FilesystemType::Iso9660.is_browsable());
    assert!(FilesystemType::Hfs.is_browsable());
    assert!(!FilesystemType::Udf.is_browsable());
}

// Phase 2 — uncomment when IsoSectorReader + PVD parsing are implemented.
// #[test]
// fn read_pvd_from_iso() {
//     use opticaldiscs::sector_reader::IsoSectorReader;
//     use opticaldiscs::iso9660::PrimaryVolumeDescriptor;
//     let mut reader = IsoSectorReader::new("tests/fixtures/data.iso").unwrap();
//     let pvd = PrimaryVolumeDescriptor::read_from(&mut reader).unwrap();
//     assert!(!pvd.volume_id.is_empty());
// }
