//! End-to-end test of GameCube detection + browsing (Phase G2).
//!
//! The image is a **fully synthetic, invented** GameCube-format disc — no bytes,
//! game IDs, or titles are taken from any real (Nintendo or otherwise) game. Its
//! *structure* mirrors what real GameCube discs use (verified against real dumps
//! during development): a `DiscHeader` (0x400) + `PartitionHeader` (0x40) boot
//! block, a zeroed bi2.bin (0x2000), a zeroed apploader header, a minimal DOL
//! header, and a small big-endian FST describing `/hi.txt`, `/sub/`, and
//! `/sub/deep.bin`. This is exactly the layout `nod` consumes, so it exercises
//! the real `nod`-backed browse path without shipping copyrighted data.

use opticaldiscs::browse::open_disc_filesystem;
use opticaldiscs::detect::DiscImageInfo;
use opticaldiscs::formats::FilesystemType;
use opticaldiscs::gameid::{Console, Region};

fn put_u32_be(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

/// FST node: kind(1) + name_offset(u24 BE) + offset(u32 BE) + length(u32 BE).
fn node(kind: u8, name_off: u32, offset: u32, length: u32) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[0] = kind;
    n[1] = (name_off >> 16) as u8;
    n[2] = (name_off >> 8) as u8;
    n[3] = name_off as u8;
    n[4..8].copy_from_slice(&offset.to_be_bytes());
    n[8..12].copy_from_slice(&length.to_be_bytes());
    n
}

fn build_gamecube_image() -> Vec<u8> {
    const DOL_OFF: u32 = 0x3000;
    const FST_OFF: u32 = 0x4000;
    const HI_DATA: u32 = 0x5000;
    const DEEP_DATA: u32 = 0x5100;

    let mut img = vec![0u8; 0x6000];

    // ── DiscHeader (0x000) ── invented game id + title, NOT a real game.
    img[0..6].copy_from_slice(b"GZZE99"); // game_id; [3]='E' → NTSC-U
                                          // wii_magic @ 0x18 stays 0 (this is GameCube); gcn_magic @ 0x1C:
    put_u32_be(&mut img, 0x1C, 0xC233_9F3D);
    img[0x20..0x33].copy_from_slice(b"OPTICALDISCS TESTGC");

    // ── PartitionHeader (0x400) ──
    put_u32_be(&mut img, 0x420, DOL_OFF); // dol_offset
    put_u32_be(&mut img, 0x424, FST_OFF); // fst_offset

    // ── DOL header (0x3000): make dol_size == header size so nod reads nothing
    //    extra. text_offs[0] = 0x100 (= size_of::<DolHeader>()). ──
    put_u32_be(&mut img, DOL_OFF as usize, 0x100);

    // ── FST (0x4000): 4 nodes then the string table ──
    // Layout: /hi.txt (file), /sub (dir), /sub/deep.bin (file).
    let nodes = [
        node(1, 0, 0, 4),          // 0: root dir, child-end index = 4
        node(0, 1, HI_DATA, 5),    // 1: /hi.txt, "hello"
        node(1, 8, 0, 4),          // 2: /sub dir, child-end index = 4
        node(0, 12, DEEP_DATA, 3), // 3: /sub/deep.bin, "abc"
    ];
    let fst = FST_OFF as usize;
    for (i, n) in nodes.iter().enumerate() {
        img[fst + i * 12..fst + i * 12 + 12].copy_from_slice(n);
    }
    // String table right after the 4 nodes.
    let st = fst + 4 * 12;
    // [0]=nul (root), "hi.txt"@1, "sub"@8, "deep.bin"@12
    img[st + 1..st + 7].copy_from_slice(b"hi.txt");
    img[st + 8..st + 11].copy_from_slice(b"sub");
    img[st + 12..st + 20].copy_from_slice(b"deep.bin");
    let fst_size = (4 * 12 + 21) as u32;
    put_u32_be(&mut img, 0x428, fst_size); // fst_size
    put_u32_be(&mut img, 0x42C, fst_size); // fst_max_size

    // ── File data ──
    img[HI_DATA as usize..HI_DATA as usize + 5].copy_from_slice(b"hello");
    img[DEEP_DATA as usize..DEEP_DATA as usize + 3].copy_from_slice(b"abc");

    img
}

fn write_temp(img: &[u8], ext: &str) -> tempfile::NamedTempFile {
    use std::io::Write;
    let mut f = tempfile::Builder::new()
        .suffix(&format!(".{ext}"))
        .tempfile()
        .unwrap();
    f.write_all(img).unwrap();
    f.flush().unwrap();
    f
}

#[test]
fn gamecube_detect_and_browse() {
    let img = build_gamecube_image();
    // Use a plain .iso extension to also exercise the Nintendo-first probe in
    // DiscImageInfo::open's ISO branch (a raw GameCube dump is often `.iso`).
    let f = write_temp(&img, "iso");

    let info = DiscImageInfo::open(f.path()).expect("open");
    assert_eq!(info.filesystem, FilesystemType::GameCube);
    let game = info.game.as_ref().expect("game info");
    assert_eq!(game.console, Console::GameCube);
    assert_eq!(game.serial.as_deref(), Some("GZZE99"));
    assert_eq!(game.region, Some(Region::NtscU));
    assert_eq!(info.volume_label.as_deref(), Some("OPTICALDISCS TESTGC"));

    let mut fs = open_disc_filesystem(&info).expect("open fs");
    assert_eq!(fs.volume_name(), Some("OPTICALDISCS TESTGC"));

    let root = fs.root().expect("root");
    let mut top = fs.list_directory(&root).expect("list root");
    top.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<&str> = top.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["hi.txt", "sub"]);

    // Read the top-level file, whole and partial.
    let hi = top.iter().find(|e| e.name == "hi.txt").unwrap();
    assert_eq!(fs.read_file(hi).unwrap(), b"hello");
    assert_eq!(fs.read_file_range(hi, 1, 3).unwrap(), b"ell");

    // Descend into the subdirectory and read the nested file.
    let sub = top.iter().find(|e| e.name == "sub").unwrap();
    let children = fs.list_directory(sub).expect("list sub");
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].name, "deep.bin");
    assert_eq!(fs.read_file(&children[0]).unwrap(), b"abc");
}
