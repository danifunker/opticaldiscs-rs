//! Synthetic Philips CD-i browse test (Phase G4).
//!
//! Models the real CD-i (Green Book) layout decoded from actual discs: the
//! `"CD-I "` identifier at sector 16 byte 1, an M-type path table (pointer at VD
//! offset 148, big-endian) locating the root, big-endian directory records
//! (extent @ 6, size @ 14), and the directory bit `0x8000` living in the
//! System-Use area after the name. No real game data is used.

use opticaldiscs::browse::open_disc_filesystem;
use opticaldiscs::detect::DiscImageInfo;
use opticaldiscs::formats::FilesystemType;
use opticaldiscs::gameid::Console;

const SECTOR: usize = 2048;
const ATTR_DIR: u16 = 0x8555;
const ATTR_FILE: u16 = 0x0555;

/// Build a CD-i directory record (big-endian extent/size; attributes in the
/// System-Use area after the name).
fn record(name: &[u8], extent: u32, size: u32, attr: u16) -> Vec<u8> {
    let id_len = name.len();
    let pad = (id_len + 1) & 1;
    // fixed 33 + name + pad + 10-byte system-use area
    let len = 33 + id_len + pad + 10;
    let len = len + (len & 1); // whole record even
    let mut rec = vec![0u8; len];
    rec[0] = len as u8;
    rec[6..10].copy_from_slice(&extent.to_be_bytes());
    rec[14..18].copy_from_slice(&size.to_be_bytes());
    rec[32] = id_len as u8;
    rec[33..33 + id_len].copy_from_slice(name);
    let attr_off = 33 + id_len + pad + 4;
    rec[attr_off..attr_off + 2].copy_from_slice(&attr.to_be_bytes());
    rec
}

fn write_at(img: &mut [u8], lba: usize, bytes: &[u8]) {
    img[lba * SECTOR..lba * SECTOR + bytes.len()].copy_from_slice(bytes);
}

fn build_cdi_image() -> Vec<u8> {
    let mut img = vec![0u8; 24 * SECTOR];

    // Root directory (LBA 20): ".", "..", HELLO.TXT (file), SUB (dir).
    let mut root = Vec::new();
    // Placeholder size for "." / ".."; patched after we know the total.
    root.extend(record(&[0x00], 20, 0, ATTR_DIR));
    root.extend(record(&[0x01], 20, 0, ATTR_DIR));
    root.extend(record(b"HELLO.TXT", 21, 11, ATTR_FILE));
    root.extend(record(b"SUB", 22, 0, ATTR_DIR));
    let root_size = root.len() as u32;
    // Patch the "." and ".." size fields (offset 14) to the real directory size.
    root[14..18].copy_from_slice(&root_size.to_be_bytes());
    let dotdot = record(&[0x00], 20, 0, ATTR_DIR).len();
    root[dotdot + 14..dotdot + 18].copy_from_slice(&root_size.to_be_bytes());

    // SUB directory (LBA 22): ".", "..", INNER.DAT.
    let mut sub = Vec::new();
    sub.extend(record(&[0x00], 22, 0, ATTR_DIR));
    sub.extend(record(&[0x01], 20, root_size, ATTR_DIR));
    sub.extend(record(b"INNER.DAT", 23, 5, ATTR_FILE));
    let sub_size = sub.len() as u32;
    sub[14..18].copy_from_slice(&sub_size.to_be_bytes());
    // Patch SUB's size in the root record too.
    // (root's SUB record is the 4th; re-find by name is overkill — recompute.)
    let off =
        record(&[0x00], 20, 0, ATTR_DIR).len() * 2 + record(b"HELLO.TXT", 21, 11, ATTR_FILE).len();
    root[off + 14..off + 18].copy_from_slice(&sub_size.to_be_bytes());

    // ── Volume descriptor (sector 16) ──
    let mut vd = vec![0u8; SECTOR];
    vd[0] = 1;
    vd[1..6].copy_from_slice(b"CD-I ");
    let label = b"SYNTH CDI";
    vd[40..40 + label.len()].copy_from_slice(label);
    vd[136..140].copy_from_slice(&8u32.to_be_bytes()); // path table size (BE)
    vd[148..152].copy_from_slice(&18u32.to_be_bytes()); // M-path-table LBA (BE)
    write_at(&mut img, 16, &vd);

    // ── M-type path table (sector 18): one entry for the root ──
    let mut pt = vec![0u8; SECTOR];
    pt[0] = 1; // id_len
    pt[2..6].copy_from_slice(&20u32.to_be_bytes()); // root extent (BE)
    pt[6..8].copy_from_slice(&1u16.to_be_bytes()); // parent
    write_at(&mut img, 18, &pt);

    write_at(&mut img, 20, &root);
    write_at(&mut img, 21, b"hello cd-i!");
    write_at(&mut img, 22, &sub);
    write_at(&mut img, 23, b"inner");

    img
}

#[test]
fn cdi_detect_and_browse() {
    let img = build_cdi_image();
    // CD-i discs are Mode 2; a bare .iso here just feeds the cooked sectors.
    use std::io::Write;
    let mut f = tempfile::Builder::new().suffix(".iso").tempfile().unwrap();
    f.write_all(&img).unwrap();
    f.flush().unwrap();

    let info = DiscImageInfo::open(f.path()).expect("open");
    assert_eq!(info.filesystem, FilesystemType::Cdi);
    assert_eq!(info.volume_label.as_deref(), Some("SYNTH CDI"));
    let game = info.game.as_ref().expect("game");
    assert_eq!(game.console, Console::CdI);
    assert_eq!(game.title.as_deref(), Some("SYNTH CDI"));

    let mut fs = open_disc_filesystem(&info).expect("open fs");
    let root = fs.root().unwrap();
    let mut top = fs.list_directory(&root).unwrap();
    top.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<&str> = top.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["HELLO.TXT", "SUB"]);

    let hello = top.iter().find(|e| e.name == "HELLO.TXT").unwrap();
    assert!(!hello.is_directory());
    assert_eq!(fs.read_file(hello).unwrap(), b"hello cd-i!");

    let sub = top.iter().find(|e| e.name == "SUB").unwrap();
    assert!(sub.is_directory());
    let inner = fs.list_directory(sub).unwrap();
    assert_eq!(inner.len(), 1);
    assert_eq!(inner[0].name, "INNER.DAT");
    assert_eq!(fs.read_file(&inner[0]).unwrap(), b"inner");
}
