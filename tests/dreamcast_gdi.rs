//! Synthetic Dreamcast `.gdi` browse test (Phase G3).
//!
//! Builds a minimal but structurally real GD-ROM high-density track: an IP.BIN
//! boot header (`SEGA SEGAKATANA`) at track sector 0, an ISO 9660 PVD at track
//! sector 16, and — crucially — directory records that use **absolute** disc
//! LBAs (root at 45020, a file at 45021), exactly as real Dreamcast discs do.
//! This exercises the LBA-rebasing `GdromSectorReader` end to end. No real game
//! data is used.

use opticaldiscs::browse::open_disc_filesystem;
use opticaldiscs::detect::DiscImageInfo;
use opticaldiscs::formats::{DiscFormat, FilesystemType};
use opticaldiscs::gameid::{Console, Region};
use opticaldiscs::iso9660::build_test_pvd_sector;

const SECTOR: usize = 2048;
const HD_START: u32 = 45000;

/// Build one ISO 9660 directory record.
fn dir_record(id: &[u8], lba: u32, size: u32, is_dir: bool) -> Vec<u8> {
    let mut rec = vec![0u8; 33 + id.len()];
    rec[2..6].copy_from_slice(&lba.to_le_bytes());
    rec[6..10].copy_from_slice(&lba.to_be_bytes());
    rec[10..14].copy_from_slice(&size.to_le_bytes());
    rec[14..18].copy_from_slice(&size.to_be_bytes());
    rec[25] = if is_dir { 0x02 } else { 0x00 };
    rec[32] = id.len() as u8;
    rec[33..].copy_from_slice(id);
    if rec.len() % 2 == 1 {
        rec.push(0);
    }
    rec[0] = rec.len() as u8;
    rec
}

/// Build the HD-area track file (cooked 2048-byte sectors, track-relative).
fn build_hd_track() -> Vec<u8> {
    // Track sectors 0..=21 (abs LBA 45000..=45021).
    let mut img = vec![0u8; 22 * SECTOR];

    // IP.BIN at track sector 0 so the game-id probe recognizes Dreamcast.
    img[0..16].copy_from_slice(b"SEGA SEGAKATANA ");
    img[0x10..0x20].copy_from_slice(b"SEGA ENTERPRISES");
    img[0x30..0x38].copy_from_slice(b"U       "); // area symbols → NTSC-U
    img[0x40..0x4A].copy_from_slice(b"T-00001N  "); // product number
    img[0x80..0x8A].copy_from_slice(b"SYNTH GAME");

    // PVD at track sector 16; its root directory is at ABSOLUTE LBA 45020.
    let pvd = build_test_pvd_sector("DCSYNTH", HD_START + 20, SECTOR as u32);
    img[16 * SECTOR..16 * SECTOR + SECTOR].copy_from_slice(&pvd);

    // Root directory at track sector 20 (abs 45020): ".", "..", "READ.TXT".
    let mut root = Vec::new();
    root.extend(dir_record(&[0x00], HD_START + 20, SECTOR as u32, true));
    root.extend(dir_record(&[0x01], HD_START + 20, SECTOR as u32, true));
    root.extend(dir_record(b"READ.TXT;1", HD_START + 21, 10, false));
    img[20 * SECTOR..20 * SECTOR + root.len()].copy_from_slice(&root);

    // File data at track sector 21 (abs 45021).
    img[21 * SECTOR..21 * SECTOR + 10].copy_from_slice(b"dreamcast!");

    img
}

#[test]
fn gdi_high_density_browse() {
    let dir = tempfile::tempdir().unwrap();
    // Only the HD track file needs to exist; tracks 1/2 are never opened.
    std::fs::write(dir.path().join("track03.bin"), build_hd_track()).unwrap();
    let gdi = dir.path().join("game.gdi");
    std::fs::write(
        &gdi,
        "3\n\
         1 0 4 2048 track01.bin 0\n\
         2 600 0 2048 track02.raw 0\n\
         3 45000 4 2048 track03.bin 0\n",
    )
    .unwrap();

    let info = DiscImageInfo::open(&gdi).expect("open gdi");
    assert_eq!(info.format, DiscFormat::Gdi);
    assert_eq!(info.filesystem, FilesystemType::Iso9660);
    assert_eq!(info.volume_label.as_deref(), Some("DCSYNTH"));

    let game = info.game.as_ref().expect("game info");
    assert_eq!(game.console, Console::SegaDreamcast);
    assert_eq!(game.serial.as_deref(), Some("T-00001N"));
    assert_eq!(game.title.as_deref(), Some("SYNTH GAME"));
    assert_eq!(game.region, Some(Region::NtscU));

    // Browse the HD-area filesystem: the reader must rebase absolute LBAs.
    let mut fs = open_disc_filesystem(&info).expect("open fs");
    let root = fs.root().expect("root");
    let entries = fs.list_directory(&root).expect("list");
    let read = entries
        .iter()
        .find(|e| e.name == "READ.TXT")
        .expect("READ.TXT present");
    assert_eq!(fs.read_file(read).unwrap(), b"dreamcast!");
}
