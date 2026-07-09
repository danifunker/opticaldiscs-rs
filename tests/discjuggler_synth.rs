//! End-to-end test: a synthetic DiscJuggler `.cdi` wrapping a minimal ISO 9660
//! volume is detected as the DiscJuggler container and browses as ISO 9660 —
//! including the Dreamcast-style case where the volume's directory extents use
//! absolute disc LBAs and must be rebased.

use opticaldiscs::browse;
use opticaldiscs::detect::DiscImageInfo;
use opticaldiscs::formats::{DiscFormat, FilesystemType};
use opticaldiscs::iso9660::build_test_pvd_sector;

const RAW: usize = 2352; // raw Mode-1 sector (read_mode 2, data_offset 16)
const PREGAP: u32 = 150;
const VOL_SECTORS: u32 = 19; // PVD at 16, empty root at 18

/// Append a track descriptor mirroring the field walk libmirage performs.
fn push_track(desc: &mut Vec<u8>, track_mode: u32, start_address: u32, track_length: u32) {
    desc.extend_from_slice(&[0u8; 16]); // header fixed
    desc.push(0); // filename length
    desc.extend_from_slice(&[0u8; 29]);
    desc.extend_from_slice(&[0u8; 2]); // medium type
    desc.extend_from_slice(&2u16.to_le_bytes()); // num_indices
    desc.extend_from_slice(&PREGAP.to_le_bytes()); // index 0 = pregap
    desc.extend_from_slice(&(track_length - PREGAP).to_le_bytes()); // index 1
    desc.extend_from_slice(&0u32.to_le_bytes()); // num_cdtext
    desc.extend_from_slice(&[0u8; 2]);
    desc.extend_from_slice(&track_mode.to_le_bytes());
    desc.extend_from_slice(&[0u8; 4]);
    desc.extend_from_slice(&0u32.to_le_bytes()); // session_idx
    desc.extend_from_slice(&0u32.to_le_bytes()); // track_idx
    desc.extend_from_slice(&start_address.to_le_bytes());
    desc.extend_from_slice(&track_length.to_le_bytes());
    desc.extend_from_slice(&[0u8; 16]);
    desc.extend_from_slice(&2u32.to_le_bytes()); // read_mode 2 = 2352 raw
    desc.extend_from_slice(&0u32.to_le_bytes()); // track_ctl
    desc.extend_from_slice(&[0u8; 9]);
    desc.extend_from_slice(&[0u8; 12]); // ISRC
    desc.extend_from_slice(&0u32.to_le_bytes()); // isrc_valid
    desc.extend_from_slice(&[0u8; 99]);
}

/// Build a `.cdi` with a single Mode-1 data track. `base_lba` is the track's
/// absolute `start_address`; the PVD's root extent is authored at
/// `base_lba + 18` so browsing exercises the rebasing reader.
fn build_cdi(volume_label: &str, base_lba: u32) -> Vec<u8> {
    let track_length = PREGAP + VOL_SECTORS;

    // Data region: pregap sectors, then the volume's raw Mode-1 sectors.
    let mut file = vec![0u8; track_length as usize * RAW];
    let pvd = build_test_pvd_sector(volume_label, base_lba + 18, 2048);
    let s16 = (PREGAP + 16) as usize * RAW; // volume LBA 16, past the pregap
    file[s16 + 16..s16 + 16 + 2048].copy_from_slice(&pvd);

    // Descriptor: num_sessions=1, one session with one track, trailing empty.
    let mut desc = Vec::new();
    desc.push(1); // num_sessions
    desc.push(0);
    desc.push(1); // track count
    desc.extend_from_slice(&[0u8; 13]);
    push_track(&mut desc, 1, base_lba, track_length);
    // Trailing empty session.
    desc.push(0);
    desc.push(0);
    desc.extend_from_slice(&[0u8; 13]);

    let dlen = (desc.len() + 4) as u32;
    file.extend_from_slice(&desc);
    file.extend_from_slice(&dlen.to_le_bytes());
    file
}

#[test]
fn cdi_detects_and_browses_relative_volume() {
    let dir = tempfile::tempdir().unwrap();
    let cdi_path = dir.path().join("game.cdi");
    std::fs::write(&cdi_path, build_cdi("CDI_TEST", 0)).unwrap();

    let info = DiscImageInfo::open(&cdi_path).unwrap();
    assert_eq!(info.format, DiscFormat::DiscJuggler);
    assert_eq!(info.filesystem, FilesystemType::Iso9660);
    assert_eq!(info.volume_label.as_deref(), Some("CDI_TEST"));

    let mut fs = browse::open_disc_filesystem(&info).unwrap();
    let root = fs.root().unwrap();
    assert!(fs.list_directory(&root).unwrap().is_empty());
}

#[test]
fn cdi_browses_absolute_lba_volume() {
    // Dreamcast-style: directory extents are absolute (base 45000). Browsing
    // must rebase them onto the track-relative data to reach the root.
    let dir = tempfile::tempdir().unwrap();
    let cdi_path = dir.path().join("dc.cdi");
    std::fs::write(&cdi_path, build_cdi("DC_TEST", 45000)).unwrap();

    let info = DiscImageInfo::open(&cdi_path).unwrap();
    assert_eq!(info.format, DiscFormat::DiscJuggler);
    assert_eq!(info.filesystem, FilesystemType::Iso9660);
    assert_eq!(info.volume_label.as_deref(), Some("DC_TEST"));

    let mut fs = browse::open_disc_filesystem(&info).unwrap();
    let root = fs.root().unwrap();
    // Root extent (absolute 45018) rebases to relative 18, an empty directory.
    assert!(fs.list_directory(&root).unwrap().is_empty());
}
