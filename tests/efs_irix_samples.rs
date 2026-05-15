//! Optional integration test against real IRIX install CDs.
//!
//! Skipped unless `OPTICALDISCS_IRIX_CDS` is set to a directory containing
//! the bundled sample ISOs (e.g. `~/irixCDs/`). The five samples in that
//! directory are all SGI-Volume-Header + EFS images:
//!
//! | File                                          | Data partition |
//! |-----------------------------------------------|----------------|
//! | IRIX 5.3 XFS.iso                              | SYSV @ 60768   |
//! | IRIX 6.4.1 (Origin, Octane).iso               | SYSV @ 62272   |
//! | IRIX-53.iso                                   | SYSV @ 56864   |
//! | sgi-mips-irix4.0.4.bin                        | SYSV @ 41408   |
//! | Irix 6.5.29 Installation & Overlays (1-3).iso | SYSV @ 49248   |
//!
//! The test asserts each opens cleanly, reports filesystem=Efs, and lists
//! a non-empty root directory.

use std::path::PathBuf;

use opticaldiscs::browse;
use opticaldiscs::detect::DiscImageInfo;
use opticaldiscs::formats::{DiscFormat, FilesystemType};

const SAMPLES: &[&str] = &[
    "IRIX 5.3 XFS.iso",
    "IRIX 6.4.1 (Origin, Octane).iso",
    "IRIX-53.iso",
    "sgi-mips-irix4.0.4.bin",
    "Irix 6.5.29 Installation & Overlays (1-3).iso",
];

fn samples_dir() -> Option<PathBuf> {
    std::env::var_os("OPTICALDISCS_IRIX_CDS").map(PathBuf::from)
}

#[test]
fn opens_all_bundled_irix_samples() {
    let Some(dir) = samples_dir() else {
        eprintln!("OPTICALDISCS_IRIX_CDS not set — skipping");
        return;
    };
    let mut checked = 0usize;
    for name in SAMPLES {
        let path = dir.join(name);
        if !path.exists() {
            eprintln!("skipping (not present): {}", path.display());
            continue;
        }
        let info = DiscImageInfo::open(&path)
            .unwrap_or_else(|e| panic!("open {} failed: {e}", path.display()));
        assert!(matches!(info.format, DiscFormat::Iso | DiscFormat::BinCue));
        assert_eq!(
            info.filesystem,
            FilesystemType::Efs,
            "expected EFS for {}",
            path.display()
        );
        assert!(
            info.sgi_header.is_some(),
            "SGI volume header missing for {}",
            path.display()
        );
        assert!(
            info.efs_partition_offset.is_some(),
            "EFS partition offset missing for {}",
            path.display()
        );

        let mut fs = browse::open_disc_filesystem(&info)
            .unwrap_or_else(|e| panic!("open filesystem for {}: {e}", path.display()));
        let root = fs.root().expect("root inode");
        let entries = fs
            .list_directory(&root)
            .unwrap_or_else(|e| panic!("list root of {}: {e}", path.display()));
        assert!(
            !entries.is_empty(),
            "root listing empty for {}",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "OPTICALDISCS_IRIX_CDS set but no sample files found"
    );
}
