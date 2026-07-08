//! Synthetic UDF 2.50 **metadata-partition** browse test (Blu-ray-style).
//!
//! Models the layout decoded from a real Blu-ray: a type-1 physical partition
//! plus a type-2 `*UDF Metadata Partition` map. The File Set Descriptor, the
//! root directory, and file entries all live in the metadata partition (partref
//! 1), addressed through the metadata file's extent, while the file *data* is a
//! `long_ad` pointing into the physical partition (partref 0). This exercises
//! the partition-reference resolver end to end. No real disc data is used.

use opticaldiscs::browse::open_disc_filesystem;
use opticaldiscs::detect::DiscImageInfo;
use opticaldiscs::formats::FilesystemType;

const SECTOR: usize = 2048;

// Layout, by Logical Sector Number.
const NSR_LSN: usize = 17; // UDF volume-recognition descriptor
const VDS_LSN: usize = 32; // PD, LVD, terminator
const PHYS_START: u32 = 100; // physical partition start LSN
const META_FILE_PHYS_LBN: u32 = 0; // metadata file ICB at physical lbn 0
const META_AREA_PHYS_LBN: u32 = 10; // metadata partition contents at physical lbn 10
const FILE_DATA_PHYS_LBN: u32 = 20; // HELLO.TXT data at physical lbn 20
const AVDP_LSN: usize = 256;

fn p16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn p32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn tag(b: &mut [u8], id: u16) {
    p16(b, 0, id);
}

/// A short_ad (8 bytes): ExtentLength (with type in high 2 bits) + position.
fn short_ad(b: &mut [u8], o: usize, len_bytes: u32, lbn: u32) {
    p32(b, o, len_bytes); // etype 0 (recorded) in the top bits
    p32(b, o + 4, lbn);
}
/// A long_ad (16 bytes): ExtentLength + lb_addr(lbn u32 + partref u16) + impuse.
fn long_ad(b: &mut [u8], o: usize, len_bytes: u32, lbn: u32, partref: u16) {
    p32(b, o, len_bytes);
    p32(b, o + 4, lbn);
    p16(b, o + 8, partref);
}

/// Build a File Entry (tag 261). `adtype` 0 = short_ad, 1 = long_ad.
fn file_entry(file_type: u8, adtype: u16, size: u64, l_ad: u32) -> Vec<u8> {
    let mut fe = vec![0u8; SECTOR];
    tag(&mut fe, 261);
    fe[16 + 11] = file_type; // ICB tag: file type at +11
    p16(&mut fe, 16 + 18, adtype); // ICB tag flags at +18 (adtype in low 3 bits)
    fe[56..64].copy_from_slice(&size.to_le_bytes()); // InformationLength
    p32(&mut fe, 168, 0); // L_EA
    p32(&mut fe, 172, l_ad); // L_AD
    fe
}

/// A File Identifier Descriptor (tag 257).
fn fid(name: &[u8], is_dir: bool, is_parent: bool, icb_lbn: u32, icb_partref: u16) -> Vec<u8> {
    let l_fi = if is_parent { 0 } else { name.len() };
    let total = (38 + l_fi + 3) & !3;
    let mut f = vec![0u8; total];
    tag(&mut f, 257);
    let mut ch = 0u8;
    if is_dir {
        ch |= 0x02;
    }
    if is_parent {
        ch |= 0x08;
    }
    f[18] = ch;
    f[19] = l_fi as u8;
    long_ad(&mut f, 20, SECTOR as u32, icb_lbn, icb_partref);
    p16(&mut f, 36, 0); // L_IU
    if !is_parent {
        f[38..38 + name.len()].copy_from_slice(name);
    }
    f
}

fn write_lsn(img: &mut [u8], lsn: usize, bytes: &[u8]) {
    img[lsn * SECTOR..lsn * SECTOR + bytes.len()].copy_from_slice(bytes);
}

fn build_image() -> Vec<u8> {
    let mut img = vec![0u8; 260 * SECTOR];

    // Volume-recognition descriptor so detection sees a UDF disc.
    let mut nsr = vec![0u8; SECTOR];
    nsr[1..6].copy_from_slice(b"NSR03");
    write_lsn(&mut img, NSR_LSN, &nsr);

    // ── AVDP (LSN 256) → VDS at LSN 32 ──
    let mut avdp = vec![0u8; SECTOR];
    tag(&mut avdp, 2);
    p32(&mut avdp, 16, (3 * SECTOR) as u32); // VDS length
    p32(&mut avdp, 20, VDS_LSN as u32); // VDS location
    write_lsn(&mut img, AVDP_LSN, &avdp);

    // ── Partition Descriptor (LSN 32) ──
    let mut pd = vec![0u8; SECTOR];
    tag(&mut pd, 5);
    p16(&mut pd, 22, 0); // partition number 0
    p32(&mut pd, 188, PHYS_START); // partition start LSN
    p32(&mut pd, 192, 160); // partition length (blocks)
    write_lsn(&mut img, VDS_LSN, &pd);

    // ── Logical Volume Descriptor (LSN 33) ──
    let mut lvd = vec![0u8; SECTOR];
    tag(&mut lvd, 6);
    // volume id dstring @84 (128): compID 8, "UDF25", last byte = used length.
    lvd[84] = 8;
    lvd[85..90].copy_from_slice(b"UDF25");
    lvd[84 + 127] = 6; // used length (compID + 5 chars)
    p32(&mut lvd, 212, SECTOR as u32); // logical block size
                                       // LogicalVolumeContentsUse (long_ad @248) → FSD at metadata lbn 0, partref 1.
    long_ad(&mut lvd, 248, SECTOR as u32, 0, 1);
    p32(&mut lvd, 268, 2); // number of partition maps
                           // Partition map 0: type-1 physical, 6 bytes, at offset 440.
    lvd[440] = 1;
    lvd[441] = 6;
    p16(&mut lvd, 442, 1); // volume sequence number
    p16(&mut lvd, 444, 0); // partition number 0
                           // Partition map 1: type-2 metadata, 64 bytes, at offset 446.
    let m = 446;
    lvd[m] = 2;
    lvd[m + 1] = 64;
    // EntityID @ +4: flags byte then "*UDF Metadata Partition".
    lvd[m + 5..m + 5 + 23].copy_from_slice(b"*UDF Metadata Partition");
    p16(&mut lvd, m + 38, 0); // physical partition number
    p32(&mut lvd, m + 40, META_FILE_PHYS_LBN); // metadata file location
    write_lsn(&mut img, VDS_LSN + 1, &lvd);

    // ── Terminating descriptor (LSN 34) ──
    let mut term = vec![0u8; SECTOR];
    tag(&mut term, 8);
    write_lsn(&mut img, VDS_LSN + 2, &term);

    // ── Metadata file ICB (physical lbn 0 = LSN 100) ──
    // One extent: the metadata contents live at physical lbn 10, 8 blocks.
    let mut meta_fe = file_entry(250, 0, (8 * SECTOR) as u64, 8);
    short_ad(&mut meta_fe, 176, (8 * SECTOR) as u32, META_AREA_PHYS_LBN);
    write_lsn(
        &mut img,
        (PHYS_START + META_FILE_PHYS_LBN) as usize,
        &meta_fe,
    );

    // Metadata partition block N → physical LSN (PHYS_START + META_AREA_PHYS_LBN + N).
    let meta_lsn = |n: u32| (PHYS_START + META_AREA_PHYS_LBN + n) as usize;

    // ── File Set Descriptor (metadata block 0) ──
    let mut fsd = vec![0u8; SECTOR];
    tag(&mut fsd, 256);
    // RootDirectoryICB (long_ad @400) → metadata block 1, partref 1.
    long_ad(&mut fsd, 400, SECTOR as u32, 1, 1);
    write_lsn(&mut img, meta_lsn(0), &fsd);

    // ── Root directory FE (metadata block 1), data in metadata block 2 ──
    let mut parent = fid(&[], true, true, 1, 1);
    let hello_fid = fid(&[8, b'H', b'E', b'L', b'L', b'O'], false, false, 3, 1);
    let mut root_data = Vec::new();
    root_data.append(&mut parent);
    root_data.extend_from_slice(&hello_fid);
    let mut root_fe = file_entry(4, 0, root_data.len() as u64, 8);
    short_ad(&mut root_fe, 176, root_data.len() as u32, 2); // metadata lbn 2
    write_lsn(&mut img, meta_lsn(1), &root_fe);
    write_lsn(&mut img, meta_lsn(2), &root_data);

    // ── HELLO file FE (metadata block 3): data is a long_ad into the PHYSICAL
    //    partition (partref 0), like real Blu-ray stream data. ──
    let body = b"hello udf25";
    let mut hello_fe = file_entry(5, 1, body.len() as u64, 16);
    long_ad(&mut hello_fe, 176, body.len() as u32, FILE_DATA_PHYS_LBN, 0);
    write_lsn(&mut img, meta_lsn(3), &hello_fe);
    write_lsn(&mut img, (PHYS_START + FILE_DATA_PHYS_LBN) as usize, body);

    img
}

#[test]
fn udf_metadata_partition_browse() {
    let img = build_image();
    use std::io::Write;
    let mut f = tempfile::Builder::new().suffix(".iso").tempfile().unwrap();
    f.write_all(&img).unwrap();
    f.flush().unwrap();

    let info = DiscImageInfo::open(f.path()).expect("open");
    assert_eq!(info.filesystem, FilesystemType::Udf);

    let mut fs = open_disc_filesystem(&info).expect("open fs");
    assert_eq!(fs.volume_name(), Some("UDF25"));

    let root = fs.root().unwrap();
    let entries = fs.list_directory(&root).unwrap();
    let hello = entries
        .iter()
        .find(|e| e.name == "HELLO")
        .expect("HELLO present");
    assert!(!hello.is_directory());
    assert_eq!(hello.size, 11);

    // Full read (data resolves through the physical partition via long_ad).
    assert_eq!(fs.read_file(hello).unwrap(), b"hello udf25");
    // Extent-aware partial read.
    assert_eq!(fs.read_file_range(hello, 6, 5).unwrap(), b"udf25");
}
