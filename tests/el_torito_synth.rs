//! End-to-end El Torito test: a synthetic ISO 9660 image carrying a Boot Record
//! VD at sector 17, a boot catalog, and two boot images (an x86 default entry
//! plus a UEFI section entry) is opened through `DiscImageInfo::open` and its
//! `el_torito` catalog is asserted — including `read_boot_image` byte fidelity.

use std::io::Write;

use opticaldiscs::detect::DiscImageInfo;
use opticaldiscs::el_torito::{self, BootMediaType, Platform};
use opticaldiscs::el_torito_edit::{make_bootable, ElToritoEditor, NewBootEntry};
use opticaldiscs::formats::DiscFormat;
use opticaldiscs::iso9660::build_test_pvd_sector;
use opticaldiscs::sector_reader::IsoSectorReader;

const SECTOR: usize = 2048;

// On-disc layout (cooked LBAs):
//   16  Primary Volume Descriptor
//   17  Boot Record Volume Descriptor  → catalog at LBA 19
//   18  root directory (referenced by the PVD)
//   19  boot catalog
//   20  x86 boot image (no-emulation)
//   22  UEFI boot image (no-emulation)
const CATALOG_LBA: u32 = 19;
const X86_IMAGE_LBA: u32 = 20;
const EFI_IMAGE_LBA: u32 = 22;

const X86_SECTOR_COUNT: u16 = 4; // → 2048 bytes
const EFI_SECTOR_COUNT: u16 = 8; // → 4096 bytes

const EL_TORITO_ID: &[u8] = b"EL TORITO SPECIFICATION";

fn write_at(img: &mut [u8], lba: usize, bytes: &[u8]) {
    img[lba * SECTOR..lba * SECTOR + bytes.len()].copy_from_slice(bytes);
}

/// A 32-byte validation entry with a correct 16-bit-word checksum.
fn validation_entry(platform: u8) -> [u8; 32] {
    let mut v = [0u8; 32];
    v[0] = 0x01;
    v[1] = platform;
    v[30] = 0x55;
    v[31] = 0xAA;
    let mut sum: u16 = 0;
    for w in v.chunks_exact(2) {
        sum = sum.wrapping_add(u16::from_le_bytes([w[0], w[1]]));
    }
    v[28..30].copy_from_slice(&(0u16).wrapping_sub(sum).to_le_bytes());
    v
}

/// A 32-byte initial/section boot entry.
fn boot_entry(media: u8, sector_count: u16, load_rba: u32) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[0] = 0x88; // bootable
    e[1] = media;
    e[6..8].copy_from_slice(&sector_count.to_le_bytes());
    e[8..12].copy_from_slice(&load_rba.to_le_bytes());
    e
}

/// Assemble the full disc image and return it plus the two images' raw bytes.
fn build_image() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut img = vec![0u8; 32 * SECTOR];

    // Primary Volume Descriptor (root dir at LBA 18).
    write_at(
        &mut img,
        16,
        &build_test_pvd_sector("BOOTABLE", 18, SECTOR as u32),
    );

    // Boot Record Volume Descriptor at sector 17.
    let mut vd = vec![0u8; SECTOR];
    vd[0] = 0x00;
    vd[1..6].copy_from_slice(b"CD001");
    vd[6] = 0x01;
    vd[7..7 + EL_TORITO_ID.len()].copy_from_slice(EL_TORITO_ID);
    vd[71..75].copy_from_slice(&CATALOG_LBA.to_le_bytes());
    write_at(&mut img, 17, &vd);

    // Boot catalog: validation + x86 default + final EFI section header + entry.
    let mut cat = Vec::new();
    cat.extend_from_slice(&validation_entry(0x00)); // x86
    cat.extend_from_slice(&boot_entry(0, X86_SECTOR_COUNT, X86_IMAGE_LBA));
    let mut header = [0u8; 32];
    header[0] = 0x91; // final section header
    header[1] = 0xEF; // EFI
    header[2..4].copy_from_slice(&1u16.to_le_bytes());
    header[4..7].copy_from_slice(b"EFI");
    cat.extend_from_slice(&header);
    cat.extend_from_slice(&boot_entry(0, EFI_SECTOR_COUNT, EFI_IMAGE_LBA));
    write_at(&mut img, CATALOG_LBA as usize, &cat);

    // Distinctive boot-image payloads.
    let x86_image: Vec<u8> = (0..(X86_SECTOR_COUNT as usize * 512))
        .map(|i| (i % 251) as u8)
        .collect();
    let efi_image: Vec<u8> = (0..(EFI_SECTOR_COUNT as usize * 512))
        .map(|i| (i % 249 + 1) as u8)
        .collect();
    write_at(&mut img, X86_IMAGE_LBA as usize, &x86_image);
    write_at(&mut img, EFI_IMAGE_LBA as usize, &efi_image);

    (img, x86_image, efi_image)
}

#[test]
fn opens_bootable_iso_and_parses_catalog() {
    let (img, x86_image, efi_image) = build_image();

    let mut f = tempfile::Builder::new().suffix(".iso").tempfile().unwrap();
    f.write_all(&img).unwrap();
    f.flush().unwrap();

    let info = DiscImageInfo::open(f.path()).expect("open");
    assert_eq!(info.format, DiscFormat::Iso);

    let et = info.el_torito.as_ref().expect("el_torito present");
    assert_eq!(et.entries.len(), 2, "default + one section entry");

    // Default x86 no-emulation entry.
    let x86 = &et.entries[0];
    assert_eq!(x86.platform, Platform::X86);
    assert!(x86.bootable);
    assert_eq!(x86.media_type, BootMediaType::NoEmulation);
    assert_eq!(x86.load_rba, X86_IMAGE_LBA);
    assert_eq!(x86.image_size, X86_SECTOR_COUNT as u64 * 512);
    assert_eq!(
        x86.image_extent(),
        (X86_IMAGE_LBA as u64 * 2048, X86_SECTOR_COUNT as u64 * 512)
    );

    // UEFI section entry.
    let efi = &et.entries[1];
    assert_eq!(efi.platform, Platform::Efi);
    assert_eq!(efi.media_type, BootMediaType::NoEmulation);
    assert_eq!(efi.load_rba, EFI_IMAGE_LBA);
    assert_eq!(efi.image_size, EFI_SECTOR_COUNT as u64 * 512);
    assert_eq!(efi.id.as_deref(), Some("EFI"));

    // read_boot_image returns exactly the boot-image bytes for each entry.
    let mut reader = IsoSectorReader::new(f.path()).unwrap();
    assert_eq!(
        el_torito::read_boot_image(&mut reader, x86).unwrap(),
        x86_image
    );
    assert_eq!(
        el_torito::read_boot_image(&mut reader, efi).unwrap(),
        efi_image
    );
}

#[test]
fn non_bootable_iso_has_no_el_torito() {
    // Same layout but without the Boot Record VD at sector 17.
    let mut img = vec![0u8; 20 * SECTOR];
    write_at(
        &mut img,
        16,
        &build_test_pvd_sector("PLAIN", 18, SECTOR as u32),
    );

    let mut f = tempfile::Builder::new().suffix(".iso").tempfile().unwrap();
    f.write_all(&img).unwrap();
    f.flush().unwrap();

    let info = DiscImageInfo::open(f.path()).expect("open");
    assert!(info.el_torito.is_none());
}

// ── Editing (write path) ────────────────────────────────────────────────────

/// Write the two-entry bootable fixture to a `.iso` tempfile.
fn fixture_file() -> (tempfile::NamedTempFile, Vec<u8>, Vec<u8>) {
    let (img, x86_image, efi_image) = build_image();
    let mut f = tempfile::Builder::new().suffix(".iso").tempfile().unwrap();
    f.write_all(&img).unwrap();
    f.flush().unwrap();
    (f, x86_image, efi_image)
}

fn reopen(path: &std::path::Path) -> opticaldiscs::el_torito::ElTorito {
    DiscImageInfo::open(path)
        .expect("reopen")
        .el_torito
        .expect("el_torito present after edit")
}

fn read_range(path: &std::path::Path, offset: usize, len: usize) -> Vec<u8> {
    std::fs::read(path).unwrap()[offset..offset + len].to_vec()
}

#[test]
fn set_bootable_toggles_and_persists() {
    let (f, _x86, _efi) = fixture_file();
    let mut ed = ElToritoEditor::open_path(f.path()).unwrap();
    assert!(ed.entries()[0].bootable);
    ed.set_bootable(0, false).unwrap();
    ed.commit().unwrap();

    let et = reopen(f.path());
    assert!(!et.entries[0].bootable);
    assert_eq!(et.entries.len(), 2);
}

#[test]
fn set_platform_persists_and_keeps_valid_checksum() {
    let (f, _x86, _efi) = fixture_file();
    let mut ed = ElToritoEditor::open_path(f.path()).unwrap();
    ed.set_platform(0, Platform::PowerPc).unwrap();
    ed.commit().unwrap();

    let et = reopen(f.path());
    assert_eq!(et.entries[0].platform, Platform::PowerPc);

    // The validation entry's sixteen LE u16 words must still sum to 0.
    let val = read_range(f.path(), CATALOG_LBA as usize * SECTOR, 32);
    let sum = val.chunks_exact(2).fold(0u16, |acc, w| {
        acc.wrapping_add(u16::from_le_bytes([w[0], w[1]]))
    });
    assert_eq!(sum, 0, "validation checksum must balance to zero");
}

#[test]
fn set_media_type_recomputes_image_size() {
    let (f, _x86, _efi) = fixture_file();
    let mut ed = ElToritoEditor::open_path(f.path()).unwrap();
    ed.set_media_type(0, BootMediaType::Floppy1_44M).unwrap();
    ed.commit().unwrap();

    let et = reopen(f.path());
    assert_eq!(et.entries[0].media_type, BootMediaType::Floppy1_44M);
    assert_eq!(et.entries[0].image_size, 1_474_560);
}

#[test]
fn replace_image_same_size_is_in_place() {
    let (f, _x86, _efi) = fixture_file();
    let new_image = vec![0x5Au8; X86_SECTOR_COUNT as usize * 512]; // same 2048 bytes

    let mut ed = ElToritoEditor::open_path(f.path()).unwrap();
    ed.replace_image(0, &new_image, BootMediaType::NoEmulation)
        .unwrap();
    ed.commit().unwrap();

    let et = reopen(f.path());
    assert_eq!(et.entries[0].load_rba, X86_IMAGE_LBA, "load_rba unchanged");
    let mut reader = IsoSectorReader::new(f.path()).unwrap();
    assert_eq!(
        el_torito::read_boot_image(&mut reader, &et.entries[0]).unwrap(),
        new_image
    );
    // The EFI image is untouched.
    assert_eq!(et.entries[1].load_rba, EFI_IMAGE_LBA);
}

#[test]
fn replace_image_larger_relocates_and_bumps_pvd() {
    let (f, _x86, efi_image) = fixture_file();
    // 3 sectors — larger than the original 1-sector x86 image.
    let big: Vec<u8> = (0..6144).map(|i| (i % 253 + 1) as u8).collect();
    let original_total = std::fs::read(f.path()).unwrap().len() / SECTOR; // 32 sectors

    let mut ed = ElToritoEditor::open_path(f.path()).unwrap();
    ed.replace_image(0, &big, BootMediaType::NoEmulation)
        .unwrap();
    ed.commit().unwrap();

    let et = reopen(f.path());
    // Relocated to the old end; load_rba + sector_count + image_size updated.
    assert_eq!(et.entries[0].load_rba, original_total as u32);
    assert_eq!(et.entries[0].sector_count, 12);
    assert_eq!(et.entries[0].image_size, 6144);

    let mut reader = IsoSectorReader::new(f.path()).unwrap();
    assert_eq!(
        el_torito::read_boot_image(&mut reader, &et.entries[0]).unwrap(),
        big
    );
    // Other entry's image is intact.
    assert_eq!(
        el_torito::read_boot_image(&mut reader, &et.entries[1]).unwrap(),
        efi_image
    );

    // PVD volume_space_size (LE at 80..84, BE at 84..88) bumped to the new total.
    let new_total = (original_total + 3) as u32;
    let le = read_range(f.path(), 16 * SECTOR + 80, 4);
    let be = read_range(f.path(), 16 * SECTOR + 84, 4);
    assert_eq!(u32::from_le_bytes([le[0], le[1], le[2], le[3]]), new_total);
    assert_eq!(u32::from_be_bytes([be[0], be[1], be[2], be[3]]), new_total);
}

#[test]
fn add_then_remove_entry_round_trips() {
    let (f, x86_image, efi_image) = fixture_file();
    let mac_image = vec![0x11u8; 1024];

    // Add a third (Mac) entry.
    let mut ed = ElToritoEditor::open_path(f.path()).unwrap();
    let idx = ed
        .add_entry(
            NewBootEntry {
                platform: Platform::Mac,
                bootable: true,
                media_type: BootMediaType::NoEmulation,
                system_type: 0,
                id: Some("MAC".to_string()),
            },
            &mac_image,
        )
        .unwrap();
    assert_eq!(idx, 2);
    ed.commit().unwrap();

    let et = reopen(f.path());
    assert_eq!(et.entries.len(), 3);
    assert_eq!(et.entries[2].platform, Platform::Mac);
    assert_eq!(et.entries[2].id.as_deref(), Some("MAC"));

    let mut reader = IsoSectorReader::new(f.path()).unwrap();
    assert_eq!(
        el_torito::read_boot_image(&mut reader, &et.entries[2]).unwrap(),
        mac_image
    );
    // The pre-existing images are untouched.
    assert_eq!(
        el_torito::read_boot_image(&mut reader, &et.entries[0]).unwrap(),
        x86_image
    );
    assert_eq!(
        el_torito::read_boot_image(&mut reader, &et.entries[1]).unwrap(),
        efi_image
    );

    // Now remove it; the other two entries and their images survive.
    let mut ed = ElToritoEditor::open_path(f.path()).unwrap();
    ed.remove_entry(2).unwrap();
    ed.commit().unwrap();

    let et = reopen(f.path());
    assert_eq!(et.entries.len(), 2);
    let mut reader = IsoSectorReader::new(f.path()).unwrap();
    assert_eq!(
        el_torito::read_boot_image(&mut reader, &et.entries[0]).unwrap(),
        x86_image
    );
    assert_eq!(
        el_torito::read_boot_image(&mut reader, &et.entries[1]).unwrap(),
        efi_image
    );
}

#[test]
fn rejects_non_iso_containers() {
    // BIN/CUE: rejected by extension before any bytes are touched.
    let cue = tempfile::Builder::new().suffix(".cue").tempfile().unwrap();
    assert!(ElToritoEditor::open_path(cue.path()).is_err());

    // CHD: rejected by extension.
    let chd = tempfile::Builder::new().suffix(".chd").tempfile().unwrap();
    assert!(ElToritoEditor::open_path(chd.path()).is_err());

    // CHD magic through the raw `open` path (Cursor) is also rejected.
    let mut buf = vec![0u8; 32 * SECTOR];
    buf[..8].copy_from_slice(b"MComprHD");
    assert!(ElToritoEditor::open(std::io::Cursor::new(buf)).is_err());
}

#[test]
fn make_bootable_adds_el_torito_when_slot_is_free() {
    // Minimal non-bootable ISO: PVD at 16, terminator at 17, sector 18 free,
    // root/data at 20+.
    let mut img = vec![0u8; 24 * SECTOR];
    write_at(
        &mut img,
        16,
        &build_test_pvd_sector("PLAIN", 20, SECTOR as u32),
    );
    // Volume Descriptor Set Terminator at sector 17.
    let mut term = vec![0u8; SECTOR];
    term[0] = 0xFF;
    term[1..6].copy_from_slice(b"CD001");
    term[6] = 0x01;
    write_at(&mut img, 17, &term);

    let mut f = tempfile::Builder::new().suffix(".iso").tempfile().unwrap();
    f.write_all(&img).unwrap();
    f.flush().unwrap();

    let boot_image = vec![0x77u8; 2048];
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.path())
        .unwrap();
    make_bootable(
        file,
        NewBootEntry {
            platform: Platform::X86,
            bootable: true,
            media_type: BootMediaType::NoEmulation,
            system_type: 0,
            id: None,
        },
        &boot_image,
    )
    .unwrap();

    let et = reopen(f.path());
    assert_eq!(et.entries.len(), 1);
    assert_eq!(et.entries[0].platform, Platform::X86);
    assert!(et.entries[0].bootable);
    let mut reader = IsoSectorReader::new(f.path()).unwrap();
    assert_eq!(
        el_torito::read_boot_image(&mut reader, &et.entries[0]).unwrap(),
        boot_image
    );
}

#[test]
fn make_bootable_errors_without_a_free_slot() {
    // Terminator at 17 but sector 18 is occupied → no room for the moved
    // terminator, so make_bootable must refuse.
    let mut img = vec![0u8; 24 * SECTOR];
    write_at(
        &mut img,
        16,
        &build_test_pvd_sector("PLAIN", 20, SECTOR as u32),
    );
    let mut term = vec![0u8; SECTOR];
    term[0] = 0xFF;
    term[1..6].copy_from_slice(b"CD001");
    term[6] = 0x01;
    write_at(&mut img, 17, &term);
    img[18 * SECTOR] = 0x42; // sector 18 occupied

    let mut f = tempfile::Builder::new().suffix(".iso").tempfile().unwrap();
    f.write_all(&img).unwrap();
    f.flush().unwrap();

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.path())
        .unwrap();
    assert!(make_bootable(
        file,
        NewBootEntry {
            platform: Platform::X86,
            bootable: true,
            media_type: BootMediaType::NoEmulation,
            system_type: 0,
            id: None,
        },
        &[0u8; 2048],
    )
    .is_err());
}
