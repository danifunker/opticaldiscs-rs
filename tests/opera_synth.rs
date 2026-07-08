//! Synthetic 3DO Opera browse test (Phase G5).
//!
//! Models the real Opera layout decoded from actual 3DO discs: a big-endian
//! volume header at block 0 (record type 1, five 0x5A sync bytes, label at 40,
//! block size at 0x4C, root block at 0x64), a directory block with a 20-byte
//! header, and variable-length entries (68-byte fixed part + avatar list, with
//! `*dir` marking directories and the 0x80000000 flag ending the directory).
//! No real game data is used.

use opticaldiscs::browse::open_disc_filesystem;
use opticaldiscs::detect::DiscImageInfo;
use opticaldiscs::formats::FilesystemType;
use opticaldiscs::gameid::Console;

const BLOCK: usize = 2048;

fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

/// Build an Opera directory entry: 68-byte fixed part + one avatar (block ptr).
fn entry(type_tag: &[u8; 4], name: &str, block: u32, byte_count: u32, last: bool) -> Vec<u8> {
    let mut e = vec![0u8; 68 + 4];
    let flags: u32 = if last { 0x8000_0000 } else { 0 } | 0x02;
    e[0..4].copy_from_slice(&be32(flags));
    e[8..12].copy_from_slice(type_tag);
    e[12..16].copy_from_slice(&be32(BLOCK as u32)); // entry block size
    e[16..20].copy_from_slice(&be32(byte_count));
    e[20..24].copy_from_slice(&be32(byte_count.div_ceil(BLOCK as u32).max(1)));
    let nb = name.as_bytes();
    e[32..32 + nb.len()].copy_from_slice(nb);
    e[64..68].copy_from_slice(&be32(0)); // copies = 0 → one avatar
    e[68..72].copy_from_slice(&be32(block)); // avatar[0] = start block
    e
}

/// Build a directory block: 20-byte header + entries.
fn dir_block(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut blk = vec![0u8; BLOCK];
    blk[0..4].copy_from_slice(&be32(u32::MAX)); // next = -1
    blk[4..8].copy_from_slice(&be32(u32::MAX)); // prev = -1
    let first_entry = 20u32;
    let mut pos = first_entry as usize;
    for e in entries {
        blk[pos..pos + e.len()].copy_from_slice(e);
        pos += e.len();
    }
    blk[12..16].copy_from_slice(&be32(pos as u32)); // content_end
    blk[16..20].copy_from_slice(&be32(first_entry));
    blk
}

fn build_opera_image() -> Vec<u8> {
    // Blocks: 0 = volume header, 20 = root dir, 21 = file data, 22 = sub dir,
    // 23 = nested file data.
    let mut img = vec![0u8; 24 * BLOCK];

    // Volume header (block 0).
    let vh = &mut img[0..BLOCK];
    vh[0] = 0x01;
    vh[1..6].copy_from_slice(&[0x5A; 5]);
    vh[6] = 0x01; // version
    vh[40..49].copy_from_slice(b"SYNTH 3DO"); // volume label
    vh[0x4C..0x50].copy_from_slice(&be32(BLOCK as u32)); // block size
    vh[0x58..0x5C].copy_from_slice(&be32(1)); // root dir blocks
    vh[0x64..0x68].copy_from_slice(&be32(20)); // root dir block location

    // Root directory (block 20): a file and a subdirectory.
    let root = dir_block(&[
        entry(b"    ", "HELLO", 21, 11, false),
        entry(b"*dir", "SUB", 22, BLOCK as u32, true),
    ]);
    img[20 * BLOCK..21 * BLOCK].copy_from_slice(&root);

    // File data (block 21).
    img[21 * BLOCK..21 * BLOCK + 11].copy_from_slice(b"hello 3do!!");

    // Subdirectory (block 22): one nested file.
    let sub = dir_block(&[entry(b"    ", "INNER", 23, 5, true)]);
    img[22 * BLOCK..23 * BLOCK].copy_from_slice(&sub);

    // Nested file data (block 23).
    img[23 * BLOCK..23 * BLOCK + 5].copy_from_slice(b"inner");

    img
}

#[test]
fn opera_detect_and_browse() {
    let img = build_opera_image();
    use std::io::Write;
    let mut f = tempfile::Builder::new().suffix(".iso").tempfile().unwrap();
    f.write_all(&img).unwrap();
    f.flush().unwrap();

    let info = DiscImageInfo::open(f.path()).expect("open");
    assert_eq!(info.filesystem, FilesystemType::Opera);
    assert_eq!(info.volume_label.as_deref(), Some("SYNTH 3DO"));
    assert_eq!(info.game.as_ref().unwrap().console, Console::ThreeDo);

    let mut fs = open_disc_filesystem(&info).expect("open fs");
    assert_eq!(fs.volume_name(), Some("SYNTH 3DO"));
    let root = fs.root().unwrap();
    let mut top = fs.list_directory(&root).unwrap();
    top.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<&str> = top.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["HELLO", "SUB"]);

    let hello = top.iter().find(|e| e.name == "HELLO").unwrap();
    assert!(!hello.is_directory());
    assert_eq!(fs.read_file(hello).unwrap(), b"hello 3do!!");

    let sub = top.iter().find(|e| e.name == "SUB").unwrap();
    assert!(sub.is_directory());
    let inner = fs.list_directory(sub).unwrap();
    assert_eq!(inner.len(), 1);
    assert_eq!(inner[0].name, "INNER");
    assert_eq!(fs.read_file(&inner[0]).unwrap(), b"inner");
}
