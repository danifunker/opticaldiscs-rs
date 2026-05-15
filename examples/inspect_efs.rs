//! Quick CLI to inspect an EFS / IRIX disc image.
//!
//! Usage: `cargo run --example inspect_efs -- <path-to-iso>`

use opticaldiscs::browse;
use opticaldiscs::detect::DiscImageInfo;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: inspect_efs <path-to-iso>")?;
    let info = DiscImageInfo::open(&path)?;
    println!("Path        : {}", info.path.display());
    println!("Format      : {}", info.format.display_name());
    println!("Filesystem  : {}", info.filesystem.display_name());
    println!("Volume label: {:?}", info.volume_label);
    println!("EFS off     : {:?}", info.efs_partition_offset);
    if let Some(h) = &info.sgi_header {
        println!("SGI bootfile: {}", h.bootfile);
        for (i, p) in h.partitions.iter().enumerate() {
            if !p.is_empty() {
                println!(
                    "  part[{i:>2}]: type={:<7} first={:<10} blocks={}",
                    p.partition_type().display_name(),
                    p.first,
                    p.blocks
                );
            }
        }
    }

    let mut fs = browse::open_disc_filesystem(&info)?;
    let root = fs.root()?;
    println!("\n/ ({}):", fs.volume_name().unwrap_or("(no label)"));
    for e in fs.list_directory(&root)? {
        let kind = if e.is_directory() { "d" } else { "-" };
        let link = e
            .symlink_target
            .as_deref()
            .map(|t| format!(" -> {t}"))
            .unwrap_or_default();
        println!("  {kind} {:>10}  {}{link}", e.size, e.name);
    }
    Ok(())
}
