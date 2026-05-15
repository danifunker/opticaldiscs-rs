//! Integration test for the synthetic IRIX-style EFS disc image.
//!
//! Builds a deterministic 192 KiB disc image (SGI volume header + EFS
//! partition) via [`common::build_synth_irix_disc`], writes it to a `.iso`
//! tempfile, opens it through `DiscImageInfo::open`, and exercises the
//! full browse → read pipeline.

use std::io::Write;

use opticaldiscs::browse;
use opticaldiscs::detect::DiscImageInfo;
use opticaldiscs::formats::FilesystemType;

mod common;
use common::build_synth_irix_disc;

fn write_fixture_to_tempfile() -> tempfile::NamedTempFile {
    let bytes = build_synth_irix_disc();
    let mut f = tempfile::Builder::new()
        .suffix(".iso")
        .tempfile()
        .expect("tempfile");
    f.write_all(&bytes).expect("write fixture");
    f.flush().expect("flush");
    f
}

#[test]
fn detects_efs_via_disc_image_info() {
    let f = write_fixture_to_tempfile();
    let info = DiscImageInfo::open(f.path()).expect("open synth disc");
    assert_eq!(info.filesystem, FilesystemType::Efs);
    assert!(info.sgi_header.is_some(), "SGI volhdr should be populated");
    assert_eq!(
        info.efs_partition_offset,
        Some(common::EFS_PART_FIRST_512 as u64 * 512)
    );
    assert_eq!(info.volume_label.as_deref(), Some("synth:pack"));
}

#[test]
fn browse_lists_root_and_descends() {
    let f = write_fixture_to_tempfile();
    let info = DiscImageInfo::open(f.path()).expect("open synth disc");
    let mut fs = browse::open_disc_filesystem(&info).expect("open filesystem");
    let root = fs.root().expect("root");

    let entries = fs.list_directory(&root).expect("list root");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    for expected in ["data", "link", "sub"] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }

    let sub = entries.iter().find(|e| e.name == "sub").unwrap();
    let children = fs.list_directory(sub).expect("list sub");
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].name, "nested");
    assert_eq!(children[0].path, "/sub/nested");
}

#[test]
fn reads_file_contents_byte_for_byte() {
    let f = write_fixture_to_tempfile();
    let info = DiscImageInfo::open(f.path()).expect("open synth disc");
    let mut fs = browse::open_disc_filesystem(&info).expect("open filesystem");
    let root = fs.root().unwrap();
    let entries = fs.list_directory(&root).unwrap();

    let data = entries.iter().find(|e| e.name == "data").unwrap();
    let bytes = fs.read_file(data).expect("read data");
    assert_eq!(bytes.len(), 512);
    assert!(bytes.iter().all(|&b| b == 0xAA));

    // Window read
    let mid = fs.read_file_range(data, 100, 32).expect("range");
    assert_eq!(mid.len(), 32);
    assert!(mid.iter().all(|&b| b == 0xAA));
}

#[test]
fn symlink_target_is_resolved() {
    let f = write_fixture_to_tempfile();
    let info = DiscImageInfo::open(f.path()).expect("open synth disc");
    let mut fs = browse::open_disc_filesystem(&info).expect("open filesystem");
    let root = fs.root().unwrap();
    let entries = fs.list_directory(&root).unwrap();
    let link = entries.iter().find(|e| e.name == "link").unwrap();
    assert_eq!(link.symlink_target.as_deref(), Some("/usr/sbin/init"));
}

#[test]
fn nested_file_reads_correctly() {
    let f = write_fixture_to_tempfile();
    let info = DiscImageInfo::open(f.path()).expect("open synth disc");
    let mut fs = browse::open_disc_filesystem(&info).expect("open filesystem");
    let root = fs.root().unwrap();
    let entries = fs.list_directory(&root).unwrap();
    let sub = entries.iter().find(|e| e.name == "sub").unwrap();
    let children = fs.list_directory(sub).unwrap();
    let nested = children.iter().find(|e| e.name == "nested").unwrap();
    let bytes = fs.read_file(nested).expect("read nested");
    assert_eq!(bytes.len(), 512);
    assert!(bytes.iter().all(|&b| b == 0xBB));
}

#[test]
fn resource_fork_returns_none_for_efs() {
    let f = write_fixture_to_tempfile();
    let info = DiscImageInfo::open(f.path()).expect("open synth disc");
    let mut fs = browse::open_disc_filesystem(&info).expect("open filesystem");
    let root = fs.root().unwrap();
    let entries = fs.list_directory(&root).unwrap();
    let data = entries.iter().find(|e| e.name == "data").unwrap();
    assert!(fs.read_resource_fork(data).unwrap().is_none());
}
