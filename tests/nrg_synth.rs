//! End-to-end test: a synthetic Nero `.nrg` wrapping a minimal ISO 9660 volume
//! is detected as the NRG container and browses as ISO 9660.

use opticaldiscs::browse;
use opticaldiscs::detect::DiscImageInfo;
use opticaldiscs::formats::{DiscFormat, FilesystemType};
use opticaldiscs::iso9660::build_test_pvd_sector;

const RAW: usize = 2352;

/// Append an NRG chunk (`id` + big-endian u32 length + payload).
fn push_chunk(v: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]) {
    v.extend_from_slice(id);
    v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    v.extend_from_slice(payload);
}

/// Build a NER5 `.nrg`: 19 raw Mode-1 sectors (PVD at sector 16, empty root at
/// 18), then CUEX/DAOX/END! chunks describing a single 2352-byte data track,
/// then the NER5 footer.
fn build_nrg(volume_label: &str) -> Vec<u8> {
    const SECTORS: usize = 19;
    let mut data = vec![0u8; SECTORS * RAW];

    // PVD at sector 16: 16-byte Mode-1 header + 2048-byte user data.
    let pvd = build_test_pvd_sector(volume_label, 18, 2048);
    let s16 = 16 * RAW;
    data[s16 + 16..s16 + 16 + 2048].copy_from_slice(&pvd);

    let chunk_off = data.len() as u64;
    let end_offset = (SECTORS * RAW) as u64;

    // DAOX: 22-byte header + one 42-byte block (mode 0x05 = raw Mode 1, 2352).
    let mut dao = vec![0u8; 22 + 42];
    dao[0x14] = 1; // first_track
    dao[0x15] = 1; // last_track
    let b = 22;
    dao[b + 0x0C..b + 0x0E].copy_from_slice(&2352u16.to_be_bytes()); // sector_size
    dao[b + 0x0E] = 0x05; // mode_code: Mode 1 raw
    dao[b + 0x12..b + 0x1A].copy_from_slice(&0u64.to_be_bytes()); // pregap_offset
    dao[b + 0x1A..b + 0x22].copy_from_slice(&0u64.to_be_bytes()); // start_offset
    dao[b + 0x22..b + 0x2A].copy_from_slice(&end_offset.to_be_bytes()); // end_offset

    push_chunk(&mut data, b"CUEX", &[0u8; 16]);
    push_chunk(&mut data, b"DAOX", &dao);
    push_chunk(&mut data, b"END!", &[]);

    data.extend_from_slice(b"NER5");
    data.extend_from_slice(&chunk_off.to_be_bytes());
    data
}

#[test]
fn nrg_detects_and_browses_iso9660() {
    let dir = tempfile::tempdir().unwrap();
    let nrg_path = dir.path().join("game.nrg");
    std::fs::write(&nrg_path, build_nrg("NRG_TEST")).unwrap();

    let info = DiscImageInfo::open(&nrg_path).unwrap();
    assert_eq!(info.format, DiscFormat::Nrg);
    assert_eq!(info.filesystem, FilesystemType::Iso9660);
    assert_eq!(info.volume_label.as_deref(), Some("NRG_TEST"));

    // The empty root directory browses to zero entries without error.
    let mut fs = browse::open_disc_filesystem(&info).unwrap();
    let root = fs.root().unwrap();
    let entries = fs.list_directory(&root).unwrap();
    assert!(entries.is_empty());
}
